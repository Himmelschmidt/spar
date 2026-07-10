mod api;
mod bus;
mod cli;
mod config;
mod doctor;
mod events;
mod executor;
mod exit_codes;
mod liveness;
mod mailbox;
mod markers;
mod model_select;
mod paths;
mod process;
mod provider_ref;
mod providers;
mod quota;
mod registry;
mod runlock;
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
            select,
            urgency,
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
                select,
                urgency,
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
            select,
            urgency,
            big,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task: task.clone(),
                providers,
                select,
                urgency,
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
            select,
            urgency,
            big,
        } => {
            let (paths, cfg) = project_ctx()?;
            let opts = CommonOpts {
                task,
                providers,
                select,
                urgency,
                detach,
                json,
                backend,
                dry_run,
                big,
            };
            workflow::run_named(workflow, opts, &paths, &cfg)
        }
        Command::Status { run_id, json, all } => status_cmd(run_id, json, all),
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
        Command::Model { action } => {
            let cfg = match project_ctx() {
                Ok((_, c)) => c,
                Err(_) => Config::default(),
            };
            model_select::run_cmd(action, &cfg)
        }
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
        Command::Stop { run_id, json } => stop_cmd(&run_id, json),
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

fn status_cmd(run_id: Option<String>, json: bool, all: bool) -> Result<ExitCode> {
    let local_root = paths::find_project_root().ok();

    // Observe-only: process exit is always 0 when the command succeeds.
    if let Some(id) = run_id {
        let (swarm, cfg, state) = load_run_anywhere(&id, local_root.as_deref())?;
        if json {
            let v = run_status_json(&swarm, &cfg, &state)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("run: {}", state.id);
            println!("project: {}", swarm.project_root.display());
            println!("phase: {:?}", state.phase);
            println!("workflow: {:?}", state.workflow);
            if let Some(task) = &state.task {
                println!("task: {task}");
            }
            if state.dry_run {
                println!("dry_run: true");
            }
            if let Some(c) = state.status_exit_code() {
                println!("run_exit_code: {c}  (process exit always 0 for status)");
            }
            match runlock::RunLock::owner(&swarm, &state.id) {
                Some(p) => {
                    let alive = process::pid_alive(p);
                    println!("orchestrator: pid={p} alive={alive}");
                }
                None => println!("orchestrator: none"),
            }
            println!("slots: {}", state.slots.len());
            for slot in &state.slots {
                let act = liveness::SlotActivity::observe(slot, cfg.timeouts.stall_warn_secs);
                let silent = act.human_silent();
                let stall = if act.stalled { " STALL" } else { "" };
                let pid = slot
                    .pid
                    .or_else(|| markers::read_pid(&swarm, &state.id, &slot.id));
                let alive = pid.map(process::pid_alive).unwrap_or(false);
                let pid_s = pid.map(|p| format!(" pid={p}")).unwrap_or_default();
                let zombie = if slot.status == state::SlotStatus::Done && alive {
                    " DONE-BUT-ALIVE"
                } else {
                    ""
                };
                println!(
                    "  - {} provider={} role={:?} status={:?}{pid_s} silent={silent}{stall}{zombie}",
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
        return Ok(ExitCode::Success);
    }

    let use_all = all || local_root.is_none();
    let runs = if use_all {
        registry::list_all_runs()?
    } else {
        let root = local_root.as_ref().unwrap();
        let _ = registry::ensure_known(Some(root));
        registry::list_project_runs(root)?
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
    } else if runs.is_empty() {
        if use_all {
            println!(
                "no runs in global registry ({})",
                registry::spar_home().display()
            );
            println!(
                "hint: run spar inside a project once, or spar status --all after work starts"
            );
        } else {
            println!(
                "no runs in {}",
                local_root.as_ref().unwrap().join(".spar").display()
            );
        }
    } else {
        if use_all {
            println!(
                "all projects (registry {}):",
                registry::spar_home().display()
            );
        } else {
            println!("runs in {}:", local_root.as_ref().unwrap().display());
        }
        let mut last_proj = String::new();
        for summary in runs {
            let proj = summary.project_name.clone().unwrap_or_else(|| "·".into());
            if use_all && proj != last_proj {
                println!("  [{proj}]");
                last_proj = proj;
            }
            let dry = if summary.dry_run { " dry" } else { "" };
            let task = summary
                .task
                .as_deref()
                .map(|t| format!("  {}", truncate_cli(t, 40)))
                .unwrap_or_default();
            println!("    {}  {:?}{}{task}", summary.id, summary.phase, dry);
        }
    }
    Ok(ExitCode::Success)
}

/// Status/stop JSON: the persisted run plus run_id, project_root, exit_code,
/// orchestrator liveness, and per-slot liveness enrichment.
fn run_status_json(
    swarm: &paths::SparPaths,
    cfg: &Config,
    state: &state::RunState,
) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(state)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("run_id".into(), serde_json::Value::String(state.id.clone()));
        obj.insert(
            "project_root".into(),
            serde_json::Value::String(swarm.project_root.display().to_string()),
        );
        obj.insert(
            "exit_code".into(),
            match state.status_exit_code() {
                Some(c) => serde_json::json!(c),
                None => serde_json::Value::Null,
            },
        );
        let orch_pid = runlock::RunLock::owner(swarm, &state.id);
        obj.insert(
            "orchestrator_pid".into(),
            match orch_pid {
                Some(p) => serde_json::json!(p),
                None => serde_json::Value::Null,
            },
        );
        obj.insert(
            "orchestrator_alive".into(),
            serde_json::Value::Bool(orch_pid.map(process::pid_alive).unwrap_or(false)),
        );
    }
    liveness::enrich_status_json(&mut v, &state.slots, cfg, swarm, &state.id);
    Ok(v)
}

fn stop_cmd(run_id: &str, json: bool) -> Result<ExitCode> {
    let (paths, cfg) = project_ctx()?;
    let mut state = state::RunState::load(&paths, run_id)?;

    // A finished or gated run is already at rest: never downgrade it to Stopped or
    // drop a resumable marker that would make a later `implement --run` redo work.
    // PlanApproved is `is_terminal` only for the plan sub-workflow; it is the normal
    // resumable plan→implement handoff, so stop still applies there.
    let finished = state.phase.is_terminal() && state.phase != state::Phase::PlanApproved;
    if finished || state.phase.is_gate() {
        if json {
            let v = run_status_json(&paths, &cfg, &state)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("run {run_id} already at {:?}; nothing to stop", state.phase);
        }
        return Ok(ExitCode::Success);
    }

    // 1. Marker first: an orchestrator that survives the signal stops at its next
    //    dispatch boundary instead of resurrecting a killed slot.
    markers::write_marker(&paths, run_id, "stopped", "stopped by operator\n")?;

    // 2. Orchestrator before slots: signalling slots first lets the orchestrator
    //    re-dispatch them. The orchestrator is not a group leader — bare pid.
    if let Some(owner) = runlock::RunLock::owner(&paths, run_id) {
        if process::pid_alive(owner) {
            process::terminate_tree(owner, false);
        }
    }

    // 3. Slot process groups: reaps nested cargo test / pnpm build children too.
    for slot in &state.slots {
        let pid = slot
            .pid
            .or_else(|| markers::read_pid(&paths, run_id, &slot.id));
        if let Some(pid) = pid {
            if process::pid_alive(pid) {
                process::terminate_tree(pid, true);
            }
        }
    }

    // 4. The orchestrator may have died before recording it; write phase ourselves.
    state.set_phase(state::Phase::Stopped);
    state.save(&paths)?;

    if json {
        let v = run_status_json(&paths, &cfg, &state)?;
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("stopped run {run_id}; branch and worktree kept");
        println!("resume: spar implement --run {run_id} --providers <…>");
    }
    Ok(ExitCode::Success)
}

fn load_run_anywhere(
    run_id: &str,
    local_root: Option<&std::path::Path>,
) -> Result<(paths::SparPaths, Config, state::RunState)> {
    if let Some(root) = local_root {
        let swarm = paths::SparPaths::new(root);
        if let Ok(state) = state::RunState::load(&swarm, run_id) {
            let cfg = Config::load(root).unwrap_or_default();
            return Ok((swarm, cfg, state));
        }
    }
    for summary in registry::list_all_runs()? {
        if summary.id != run_id {
            continue;
        }
        let Some(root) = summary.project_root else {
            continue;
        };
        let swarm = paths::SparPaths::new(&root);
        if let Ok(state) = state::RunState::load(&swarm, run_id) {
            let cfg = Config::load(&root).unwrap_or_default();
            return Ok((swarm, cfg, state));
        }
    }
    anyhow::bail!("run {run_id} not found in current project or global registry");
}

fn truncate_cli(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
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

fn logs_follow(paths: &paths::SparPaths, run_id: &str, slot: Option<&str>) -> Result<ExitCode> {
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
                println!(
                    "===== {} =====",
                    path.file_name().unwrap().to_string_lossy()
                );
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
        worktree::prune_empty_spar_parents(&paths)?;
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
            if !paths.runs_dir().is_dir() {
                println!("removed empty .spar/runs");
            }
            if !paths.root.is_dir() {
                println!("removed empty .spar");
            }
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
