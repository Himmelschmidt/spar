mod api;
mod bus;
mod cli;
mod config;
mod doctor;
mod events;
mod executor;
mod exit_codes;
mod mailbox;
mod markers;
mod paths;
mod process;
mod provider_ref;
mod providers;
mod quota;
mod sandbox;
mod ship;
mod skills;
mod state;
mod tasks;
mod templates;
mod tmux;
mod tui;
mod util;
mod workflow;
mod worktree;

use anyhow::Result;
use clap::Parser;
use cli::{BusCmd, Cli, Command, SkillsCmd};
use config::Config;
use exit_codes::ExitCode;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::ExitCode as StdExitCode;
use std::time::Duration;
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

    if let Some(cwd) = &cli.cwd {
        if cli.command.is_some() {
            std::env::set_current_dir(cwd)?;
        }
    }

    let Some(command) = cli.command else {
        return tui::run_with(tui::TuiOpts {
            task_seed: cli.task.clone(),
            cwd: cli.cwd.clone(),
        });
    };

    match command {
        Command::Doctor { json } => doctor::run(json),
        Command::Plan {
            task,
            providers,
            detach,
            json,
            backend,
            dry_run,
            big,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task: Some(task.clone()),
                providers,
                detach,
                json,
                backend,
                dry_run,
                big,
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
            big,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task: task.clone(),
                providers,
                detach,
                json,
                backend,
                dry_run,
                big,
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
            big,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task,
                providers,
                detach,
                json,
                backend,
                dry_run,
                big,
            };
            workflow::run_named(workflow, opts, &paths, &cfg)
        }
        Command::Status { run_id, json } => status_cmd(run_id, json),
        Command::Wait {
            run_id,
            timeout,
            json,
            follow,
        } => {
            let (paths, _) = project_ctx()?;
            let dur = util::parse_duration(&timeout)?;
            executor::wait_run(&paths, &run_id, dur, json, follow)
        }
        Command::Logs {
            run_id,
            slot,
            follow,
        } => logs_cmd(&run_id, slot.as_deref(), follow),
        Command::Attach { run_id } => attach_cmd(&run_id),
        Command::Dashboard => tui::run_with(tui::TuiOpts {
            task_seed: cli.task.clone(),
            cwd: cli.cwd.clone(),
        }),
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
        Command::Reconcile { run_id, json } => {
            let (paths, cfg) = project_ctx()?;
            workflow::arena::reconcile(&paths, &cfg, &run_id, json)
        }
        Command::Bus { action } => bus_cmd(action),
        Command::Cleanup {
            run_id,
            json,
            purge,
        } => cleanup_cmd(&run_id, json, purge),
        Command::Skills { action } => match action {
            SkillsCmd::List { json } => skills::run(skills::SkillsAction::List { json }),
            SkillsCmd::Get { name } => skills::run(skills::SkillsAction::Get { name }),
        },
        Command::InternalContinue { run_id } => {
            let (paths, cfg) = project_ctx()?;
            workflow::implement::continue_run(&paths, &cfg, &run_id)
        }
    }
}

fn bus_cmd(action: BusCmd) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    match action {
        BusCmd::Send {
            run_id,
            from,
            to,
            message,
            json,
        } => {
            let msg = bus::chat(
                &paths,
                &run_id,
                &from,
                &to,
                message,
                bus::MessageBudget::Normal,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msg)?);
            } else {
                println!("sent {} → {}", msg.from, msg.to);
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Log { run_id, json } => {
            let events = bus::list_events(&paths, &run_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                for e in events {
                    println!(
                        "{} {} → {} ({:?}) {}",
                        e.ts.format("%H:%M:%S"),
                        e.from,
                        e.to,
                        e.kind,
                        e.body.chars().take(100).collect::<String>()
                    );
                }
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Presence { run_id, json } => {
            let p = bus::list_presence(&paths, &run_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                for a in p {
                    println!("{:<20} {:<12} {:?}", a.agent, a.status, a.provider);
                }
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Reserve {
            run_id,
            path,
            holder,
        } => {
            bus::reserve(&paths, &run_id, &path, &holder)?;
            println!("reserved {path} by {holder}");
            Ok(ExitCode::Success)
        }
        BusCmd::Release {
            run_id,
            path,
            holder,
        } => {
            bus::release(&paths, &run_id, &path, &holder)?;
            println!("released {path}");
            Ok(ExitCode::Success)
        }
    }
}

fn project_ctx() -> Result<(paths::SparPaths, Config)> {
    let root = paths::find_project_root()?;
    let paths = paths::SparPaths::new(&root);
    let cfg = Config::load(&root)?;
    Ok((paths, cfg))
}

fn status_cmd(run_id: Option<String>, json: bool) -> Result<ExitCode> {
    let root = paths::find_project_root()?;
    let swarm = paths::SparPaths::new(&root);

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

fn logs_cmd(run_id: &str, slot: Option<&str>, follow: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let logs_dir = paths.logs_dir(run_id);
    if !logs_dir.is_dir() {
        anyhow::bail!("no logs for run {run_id}");
    }

    if follow {
        return logs_follow(&paths, run_id, slot);
    }

    if let Some(slot) = slot {
        let p = resolve_log_path(&paths, run_id, slot)?;
        print!("{}", std::fs::read_to_string(p)?);
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

fn resolve_log_path(
    paths: &paths::SparPaths,
    run_id: &str,
    slot: &str,
) -> Result<std::path::PathBuf> {
    let p = paths.log_file(run_id, slot);
    if p.is_file() {
        return Ok(p);
    }
    let logs_dir = paths.logs_dir(run_id);
    for e in std::fs::read_dir(&logs_dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(slot) {
            return Ok(e.path());
        }
    }
    anyhow::bail!("no log for slot {slot}")
}

fn logs_follow(
    paths: &paths::SparPaths,
    run_id: &str,
    slot: Option<&str>,
) -> Result<ExitCode> {
    let targets: Vec<std::path::PathBuf> = if let Some(slot) = slot {
        vec![resolve_log_path(paths, run_id, slot)?]
    } else {
        let logs_dir = paths.logs_dir(run_id);
        let mut files = Vec::new();
        for e in std::fs::read_dir(&logs_dir)? {
            let e = e?;
            if e.path().extension().and_then(|x| x.to_str()) == Some("log") {
                files.push(e.path());
            }
        }
        files.sort();
        if files.is_empty() {
            anyhow::bail!("no log files for run {run_id}");
        }
        files
    };

    let multi = targets.len() > 1;
    let mut offsets: Vec<u64> = vec![0; targets.len()];

    // First dump existing
    for (i, path) in targets.iter().enumerate() {
        if path.is_file() {
            let data = std::fs::read(path)?;
            if multi {
                println!("===== {} =====", path.file_name().unwrap().to_string_lossy());
            }
            let _ = std::io::stdout().write_all(&data);
            offsets[i] = data.len() as u64;
        }
    }
    let _ = std::io::stdout().flush();

    loop {
        // stop if run reached terminal-ish? keep following until ctrl-c; check phase
        if let Ok(st) = state::RunState::load(paths, run_id) {
            if st.phase.is_waitable_stop() {
                // one more read then exit
                for (i, path) in targets.iter().enumerate() {
                    if let Ok(mut f) = std::fs::File::open(path) {
                        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                        if len > offsets[i] {
                            f.seek(SeekFrom::Start(offsets[i]))?;
                            let mut buf = Vec::new();
                            f.read_to_end(&mut buf)?;
                            if multi && !buf.is_empty() {
                                println!(
                                    "===== {} =====",
                                    path.file_name().unwrap().to_string_lossy()
                                );
                            }
                            let _ = std::io::stdout().write_all(&buf);
                            offsets[i] = len;
                        }
                    }
                }
                let _ = std::io::stdout().flush();
                return Ok(st.exit_code());
            }
        }

        for (i, path) in targets.iter().enumerate() {
            if let Ok(mut f) = std::fs::File::open(path) {
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                if len > offsets[i] {
                    f.seek(SeekFrom::Start(offsets[i]))?;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf)?;
                    if multi && !buf.is_empty() {
                        println!(
                            "===== {} =====",
                            path.file_name().unwrap().to_string_lossy()
                        );
                    }
                    let _ = std::io::stdout().write_all(&buf);
                    offsets[i] = len;
                }
            }
        }
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(250));
    }
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
                    let p = paths::SparPaths::new(r);
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
