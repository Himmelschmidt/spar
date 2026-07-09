mod cli;
mod config;
mod doctor;
mod executor;
mod exit_codes;
mod mailbox;
mod markers;
mod paths;
mod process;
mod providers;
mod quota;
mod sandbox;
mod ship;
mod state;
mod templates;
mod tmux;
mod tui;
mod util;
mod workflow;
mod worktree;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use config::Config;
use exit_codes::ExitCode;
use std::process::ExitCode as StdExitCode;
use workflow::CommonOpts;

fn main() -> StdExitCode {
    match run() {
        Ok(code) => code.into(),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::Failure.into()
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor { json } => doctor::run(json),
        Command::Plan {
            task,
            providers,
            detach,
            json,
            backend,
            dry_run,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task: Some(task.clone()),
                providers,
                detach,
                json,
                backend,
                dry_run,
            };
            workflow::plan::run(task, opts, &paths, &cfg)
        }
        Command::Approve { run_id, json } => {
            let (paths, _) = project_ctx()?;
            workflow::plan::approve(&paths, &run_id, json)
        }
        Command::Reject {
            run_id,
            reason,
            json,
        } => {
            let (paths, _) = project_ctx()?;
            workflow::plan::reject(&paths, &run_id, reason, json)
        }
        Command::Implement {
            run_id,
            plan,
            task,
            detach,
            json,
            backend,
            dry_run,
            providers,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task: task.clone(),
                providers,
                detach,
                json,
                backend,
                dry_run,
            };
            workflow::implement::run_from_cli(run_id, plan, task, opts, &paths, &cfg)
        }
        Command::Run {
            workflow,
            task,
            detach,
            json,
            backend,
            dry_run,
            providers,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task,
                providers,
                detach,
                json,
                backend,
                dry_run,
            };
            workflow::run_named(workflow, opts, &paths, &cfg)
        }
        Command::Status { run_id, json } => status_cmd(run_id, json),
        Command::Wait {
            run_id,
            timeout,
            json,
        } => {
            let (paths, _) = project_ctx()?;
            let dur = util::parse_duration(&timeout)?;
            executor::wait_run(&paths, &run_id, dur, json)
        }
        Command::Logs { run_id, slot } => logs_cmd(&run_id, slot.as_deref()),
        Command::Attach { run_id } => attach_cmd(&run_id),
        Command::Dashboard => tui::run(),
        Command::Provider { action } => provider_cmd(action),
        Command::Ship {
            run_id,
            json,
            confirm,
            confirm_only,
        } => {
            let (paths, cfg) = project_ctx()?;
            if confirm || confirm_only {
                ship::confirm_ship(&paths, &run_id, json)?;
                if confirm_only {
                    return Ok(ExitCode::Success);
                }
            }
            ship::ship(&paths, &cfg, &run_id, json)
        }
        Command::Confirm {
            run_id,
            winner,
            json,
        } => {
            let (paths, _) = project_ctx()?;
            workflow::arena::confirm_winner(&paths, &run_id, winner, json)
        }
        Command::Cleanup {
            run_id,
            json,
            purge,
        } => cleanup_cmd(&run_id, json, purge),
        Command::InternalContinue { run_id } => {
            let (paths, cfg) = project_ctx()?;
            workflow::implement::continue_run(&paths, &cfg, &run_id)
        }
    }
}

fn project_ctx() -> Result<(paths::SwarmPaths, Config)> {
    let root = paths::find_project_root()?;
    let paths = paths::SwarmPaths::new(&root);
    let cfg = Config::load(&root)?;
    Ok((paths, cfg))
}

fn status_cmd(run_id: Option<String>, json: bool) -> Result<ExitCode> {
    let root = paths::find_project_root()?;
    let swarm = paths::SwarmPaths::new(&root);

    if let Some(id) = run_id {
        let state = state::RunState::load(&swarm, &id)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&state)?);
        } else {
            println!("run: {}", state.id);
            println!("phase: {:?}", state.phase);
            println!("workflow: {:?}", state.workflow);
            if let Some(task) = &state.task {
                println!("task: {task}");
            }
            if state.dry_run {
                println!("dry_run: true");
            }
            println!("slots: {}", state.slots.len());
            for slot in &state.slots {
                println!(
                    "  - {} provider={} role={:?} status={:?}",
                    slot.id, slot.provider, slot.role, slot.status
                );
            }
            if let Some(w) = &state.winner_slot {
                println!("winner: {w}");
            }
            if let Some(err) = &state.error {
                println!("error: {err}");
            }
        }
        return Ok(state.exit_code());
    }

    let runs = state::list_runs(&swarm)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
    } else if runs.is_empty() {
        println!("no runs in {}", swarm.root.display());
    } else {
        println!("runs in {}:", swarm.root.display());
        for summary in runs {
            println!(
                "  {}  {:?}/{:?}",
                summary.id, summary.workflow, summary.phase
            );
        }
    }
    Ok(ExitCode::Success)
}

fn logs_cmd(run_id: &str, slot: Option<&str>) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let logs_dir = paths.logs_dir(run_id);
    if !logs_dir.is_dir() {
        anyhow::bail!("no logs for run {run_id}");
    }
    if let Some(slot) = slot {
        let p = paths.log_file(run_id, slot);
        if !p.is_file() {
            // try prefix match
            let mut found = None;
            for e in std::fs::read_dir(&logs_dir)? {
                let e = e?;
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(slot) {
                    found = Some(e.path());
                    break;
                }
            }
            match found {
                Some(p) => {
                    print!("{}", std::fs::read_to_string(p)?);
                }
                None => anyhow::bail!("no log for slot {slot}"),
            }
        } else {
            print!("{}", std::fs::read_to_string(p)?);
        }
        return Ok(ExitCode::Success);
    }
    for e in std::fs::read_dir(&logs_dir)? {
        let e = e?;
        if e.path().extension().and_then(|x| x.to_str()) == Some("log") {
            println!("===== {} =====", e.file_name().to_string_lossy());
            print!("{}", std::fs::read_to_string(e.path())?);
            println!();
        }
    }
    Ok(ExitCode::Success)
}

fn attach_cmd(run_id: &str) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let state = state::RunState::load(&paths, run_id)?;
    let session = state
        .tmux_session
        .unwrap_or_else(|| tmux::session_name(run_id));
    tmux::attach_command(&session)?;
    Ok(ExitCode::Success)
}

fn cleanup_cmd(run_id: &str, json: bool, purge: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let state = state::RunState::load(&paths, run_id)?;
    worktree::cleanup_run(&state)?;
    if let Some(session) = &state.tmux_session {
        let _ = tmux::kill_session(session);
    }
    if purge {
        let dir = paths.run_dir(run_id);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
        }
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "run_id": run_id,
                "cleaned": true,
                "purged": purge,
            })
        );
    } else {
        println!("cleaned worktrees for {run_id}");
        if purge {
            println!("purged run dir");
        }
    }
    Ok(ExitCode::Success)
}

fn provider_cmd(action: cli::ProviderAction) -> Result<ExitCode> {
    match action {
        cli::ProviderAction::List { json } => {
            let report = providers::detect_all();
            let quota = paths::find_project_root()
                .ok()
                .and_then(|r| {
                    let p = paths::SwarmPaths::new(r);
                    quota::QuotaStore::load(&p).ok()
                })
                .unwrap_or_default();
            if json {
                let enriched: Vec<serde_json::Value> = report
                    .iter()
                    .map(|p| {
                        let q = quota.get(&p.name);
                        serde_json::json!({
                            "name": p.name,
                            "available": p.available,
                            "path": p.path,
                            "version": p.version,
                            "capabilities": p.capabilities,
                            "quota_status": q.status,
                            "quota_hint": q.hint,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                for p in &report {
                    let mark = if p.available { "ok" } else { "missing" };
                    let q = quota.get(&p.name);
                    println!(
                        "{:<8} {mark:<8} {:<12} {}",
                        p.name,
                        format!("{:?}", q.status),
                        p.path.as_deref().unwrap_or("-")
                    );
                    if p.available {
                        println!(
                            "         headless={} interactive={} version={}",
                            p.capabilities.headless,
                            p.capabilities.interactive,
                            p.version.as_deref().unwrap_or("unknown")
                        );
                    }
                }
            }
            Ok(ExitCode::Success)
        }
        cli::ProviderAction::Pause { name, until, json } => {
            let (paths, _) = project_ctx()?;
            let mut store = quota::QuotaStore::load(&paths)?;
            let until_dt = if let Some(u) = until {
                // accept RFC3339 or duration from now
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&u) {
                    Some(dt.with_timezone(&chrono::Utc))
                } else {
                    let d = util::parse_duration(&u)?;
                    Some(chrono::Utc::now() + chrono::Duration::from_std(d)?)
                }
            } else {
                None
            };
            store.pause_manual(&name, until_dt);
            store.save(&paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&store.get(&name))?);
            } else {
                println!("paused provider {name}");
            }
            Ok(ExitCode::Success)
        }
        cli::ProviderAction::Resume { name, json } => {
            let (paths, _) = project_ctx()?;
            let mut store = quota::QuotaStore::load(&paths)?;
            store.resume(&name);
            store.save(&paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&store.get(&name))?);
            } else {
                println!("resumed provider {name}");
            }
            Ok(ExitCode::Success)
        }
    }
}
