mod api;
mod brief;
mod bus;
mod cli;
mod config;
mod daemon;
mod doctor;
mod events;
mod executor;
mod exit_codes;
mod liveness;
mod mailbox;
mod markers;
mod model_select;
mod motion;
mod notify;
mod nudge;
mod orchestrator;
mod paths;
mod process;
mod provider_ref;
mod providers;
mod quota;
mod record;
mod registry;
mod runlock;
mod sandbox;
mod ship;
mod skills;
mod state;
mod suite;
mod tasks;
mod templates;
mod terminal;
mod theme;
mod tmux;
mod tui;
mod util;
mod workflow;
mod workspace;
mod worktree;

#[cfg(test)]
mod structured_views_contract;

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
    let code = match run() {
        Ok(code) => code.into(),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::Failure.into()
        }
    };
    // A one-shot CLI invocation (the common case: `spar plan`, `spar implement --run`
    // without `--detach`) often has nothing left to do after the phase transition that
    // just fired a lifecycle or human alert — main() returns and the process exits
    // within microseconds, which a bare `thread::spawn` cannot outrace. Fire-and-forget
    // must still mean *delivered*, so give in-flight notify threads a bounded window to
    // finish before the process actually dies; a broken sink still cannot fail this
    // exit code, since notify errors are only ever logged to stderr, never propagated.
    notify::drain_before_exit();
    code
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
            full_mouse: cli.full_mouse,
        });
    };

    // Orchestrating commands reap their slots on SIGINT/SIGTERM. Never the TUI: there
    // Ctrl+C is the agent's, not spar's.
    if matches!(
        command,
        Command::Plan { .. }
            | Command::Implement { .. }
            | Command::Run { .. }
            | Command::InternalContinue { .. }
    ) {
        process::install_shutdown_handler();
        // Launch is the one moment a mutation like this belongs: read commands must stay
        // observe-only, so `status` can never be the thing that hid a run from you.
        auto_archive_at_launch();
    }

    match command {
        Command::Doctor { json } => doctor::run(json),
        Command::Plan {
            task,
            spec,
            brief,
            run_id,
            providers,
            select,
            urgency,
            role,
            fleet,
            without,
            base,
            detach,
            json,
            backend,
            dry_run,
            big,
        } => {
            let count = [task.is_some(), spec.is_some(), brief.is_some()]
                .iter()
                .filter(|v| **v)
                .count();
            if count != 1 {
                anyhow::bail!("pass exactly one of -t / --spec / --brief (got {count})");
            }
            if brief.is_some() && task.is_some() {
                anyhow::bail!("--brief is exclusive with -t");
            }
            if brief.is_some() && spec.is_some() {
                anyhow::bail!("--brief is exclusive with --spec");
            }
            // A replan reads the run's own frozen config (O27), never the live file,
            // and re-dispatches the run's existing planner and critic slots. Flags that
            // could only apply to a NEW run are refused, not silently dropped. `--spec`
            // is accepted here too: it is the natural way to hand a replan a long
            // directive, same as `-t`.
            if let Some(id) = run_id {
                let ignored = [
                    (!providers.is_empty(), "--providers"),
                    (!select.is_empty(), "--select"),
                    (!role.is_empty(), "--role"),
                    (fleet.is_some(), "--fleet"),
                    (!without.is_empty(), "--without"),
                    (base.is_some(), "--base"),
                    (big, "--big"),
                    (detach, "--detach"),
                    (dry_run, "--dry-run"),
                ]
                .into_iter()
                .filter(|(set, _)| *set)
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
                if !ignored.is_empty() {
                    anyhow::bail!(
                        "`plan --run` replans an existing run and inherits its pool, base and config; \
                         {} cannot apply. Drop them, or start new work with `spar plan -t \"…\"`.",
                        ignored.join(", ")
                    );
                }
                let directive = match (&task, &spec, &brief) {
                    (Some(t), None, None) => t.clone(),
                    (None, Some(path), None) => brief::read_spec_text(path)?,
                    (None, None, Some(path)) => brief::read_spec_text(path)?,
                    _ => anyhow::bail!("plan --run requires exactly one of -t / --spec / --brief"),
                };
                let (paths, _) = project_ctx()?;
                let cfg = Config::for_run(&paths, &id)?;
                return workflow::plan::replan(&paths, &cfg, &id, directive, json);
            }
            let (paths, mut cfg) = project_ctx()?;
            if let Some(preset) = &fleet {
                cfg.apply_fleet_preset(config::FleetPreset::parse(preset)?);
            }
            cfg.apply_without(&without)?;
            cfg.apply_role_overrides(&role)?;
            let (task_text, brief_path) = match (spec, brief, task) {
                (Some(path), None, None) => {
                    let b = brief::intake(&paths, &path)?;
                    if !json {
                        eprintln!("brief {} written to {}", b.slug, b.path.display());
                    }
                    (b.body, Some(b.path))
                }
                (None, Some(path), None) => {
                    let body = brief::read_spec_text(&path)?;
                    let rel = path
                        .strip_prefix(&paths.project_root)
                        .map(|p| p.to_path_buf())
                        .unwrap_or(path.clone());
                    if !path.exists() {
                        anyhow::bail!("brief file not found: {}", path.display());
                    }
                    (body, Some(rel))
                }
                (None, None, Some(t)) => (t, None),
                _ => unreachable!("exactly one of task/spec/brief"),
            };
            let opts = CommonOpts {
                task: Some(task_text.clone()),
                providers,
                select,
                urgency,
                base,
                detach,
                json,
                backend,
                dry_run,
                big,
                max_rounds: None,
                accept_contract: false,
            };
            workflow::plan::run(task_text, brief_path, opts, &paths, &cfg)
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
        Command::Brief { run_id, full, json } => brief_cmd(&run_id, full, json),
        Command::Resume {
            run_id,
            detach,
            json,
        } => {
            let (paths, _) = project_ctx()?;
            let cfg = Config::for_run(&paths, &run_id)?;
            workflow::implement::resume(&paths, &cfg, &run_id, detach, json)
        }
        Command::Implement {
            run_id,
            new,
            plan,
            task,
            role,
            fleet,
            without,
            reload_config,
            max_rounds,
            accept_contract,
            base,
            detach,
            json,
            backend,
            dry_run,
            providers,
            select,
            urgency,
            big,
        } => {
            let (paths, cfg) = implement_ctx(
                run_id.as_deref(),
                &role,
                &without,
                fleet.as_deref(),
                reload_config,
            )?;
            let opts = CommonOpts {
                task: task.clone(),
                providers,
                select,
                urgency,
                base,
                detach,
                json,
                backend,
                dry_run,
                big,
                max_rounds,
                accept_contract,
            };
            workflow::implement::run_from_cli(run_id, plan, task, new, opts, &paths, &cfg)
        }
        Command::Run {
            workflow,
            task,
            role,
            fleet,
            without,
            base,
            detach,
            json,
            backend,
            dry_run,
            providers,
            select,
            urgency,
            big,
        } => {
            let (paths, mut cfg) = project_ctx()?;
            if let Some(preset) = &fleet {
                cfg.apply_fleet_preset(config::FleetPreset::parse(preset)?);
            }
            cfg.apply_without(&without)?;
            cfg.apply_role_overrides(&role)?;
            let opts = CommonOpts {
                task,
                providers,
                select,
                urgency,
                base,
                detach,
                json,
                backend,
                dry_run,
                big,
                max_rounds: None,
                accept_contract: false,
            };
            workflow::run_named(workflow, opts, &paths, &cfg)
        }
        Command::Status {
            run_id,
            json,
            all,
            archived,
        } => status_cmd(run_id, json, all, archived),
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
            full_mouse: cli.full_mouse,
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
            base,
            confirm,
            confirm_only,
        } => {
            let (paths, _) = project_ctx()?;
            let cfg = Config::for_run(&paths, &run_id)?;
            if confirm || confirm_only {
                ship::confirm_ship(&paths, &run_id, json)?;
                if confirm_only {
                    return Ok(ExitCode::Success);
                }
            }
            ship::ship(&paths, &cfg, &run_id, json, base.as_deref())
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
            let (paths, _) = project_ctx()?;
            let cfg = Config::for_run(&paths, &run_id)?;
            workflow::arena::reconcile(&paths, &cfg, &run_id, json)
        }
        Command::Bus { action } => bus_cmd(action),
        Command::Stop {
            run_id,
            abandoned,
            json,
        } => stop_cmd(run_id.as_deref(), abandoned, json),
        Command::ReconcileState {
            run_id,
            all,
            apply,
            dry_run: _,
            json,
        } => reconcile_state_cmd(run_id.as_deref(), all, apply, json),
        Command::Cleanup {
            run_id,
            all,
            older_than,
            merged,
            json,
            purge,
            force,
        } => cleanup_cmd(
            run_id.as_deref(),
            all,
            older_than.as_deref(),
            merged,
            json,
            purge,
            force,
        ),
        Command::Reclaim { run_id, all, json } => reclaim_cmd(run_id.as_deref(), all, json),
        Command::Archive {
            run_id,
            all,
            older_than,
            gates,
            undo,
            json,
        } => archive_cmd(
            run_id.as_deref(),
            all,
            older_than.as_deref(),
            gates,
            undo,
            json,
        ),
        Command::Link {
            run_id,
            parent,
            undo,
            json,
        } => link_cmd(&run_id, parent.as_deref(), undo, json),
        Command::Skills { action } => match action {
            SkillsCmd::List { json } => skills::run(skills::SkillsAction::List { json }),
            SkillsCmd::Get { name } => skills::run(skills::SkillsAction::Get { name }),
        },
        Command::Daemon { cmd } => {
            let (paths, _) = project_ctx()?;
            match cmd {
                cli::DaemonCmd::Start { foreground } => daemon::start(&paths, foreground),
                cli::DaemonCmd::Stop => daemon::stop(&paths),
                cli::DaemonCmd::Status { json } => daemon::status(&paths, json),
            }
        }
        Command::InternalContinue { run_id } => {
            let (paths, _) = project_ctx()?;
            let cfg = Config::for_run(&paths, &run_id)?;
            workflow::implement::continue_run(&paths, &cfg, &run_id)
        }
    }
}

fn bus_cmd(action: BusCmd) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    match action {
        BusCmd::Send {
            run,
            from,
            to,
            message,
            json,
            surface,
            conversation,
            turn,
        } => {
            let mut meta = std::collections::HashMap::new();
            if let Some(s) = surface {
                meta.insert("surface".to_string(), s);
            }
            if let Some(c) = conversation {
                meta.insert("conversation".to_string(), c);
            }
            if let Some(t) = turn {
                meta.insert("turn".to_string(), t);
            }
            let msg = bus::BusMessage {
                id: bus::new_id(),
                ts: chrono::Utc::now(),
                from: from.clone(),
                to: to.clone(),
                kind: bus::MsgKind::Chat,
                body: message.clone(),
                run: run.clone(),
                subject: None,
                refs: bus::MsgRefs::default(),
                requires_ack: false,
                meta,
            };
            let msg = bus::send(&paths, msg, bus::MessageBudget::Normal)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msg)?);
            } else {
                println!("sent {} → {}", msg.from, msg.to);
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Log { run, json } => {
            let events = bus::list_events(&paths, run.as_deref())?;
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
        BusCmd::Inbox {
            agent,
            claim,
            run,
            json,
        } => {
            // Accept either the short slot id (+ `--run`) or the already-unique id
            // (`$SPAR_AGENT_ID`); resolve to the unique inbox key either way.
            let unique = bus::resolve_addr(run.as_deref(), &agent);
            let msgs = if claim {
                bus::inbox_claim(&paths, &unique)?
            } else {
                bus::inbox(&paths, &unique)?
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&msgs)?);
            } else {
                for m in &msgs {
                    println!(
                        "{} {} → {} ({:?}) {}",
                        m.ts.format("%H:%M:%S"),
                        m.from,
                        m.to,
                        m.kind,
                        m.body.chars().take(100).collect::<String>()
                    );
                }
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Presence { run, json } => {
            let p = bus::list_presence(&paths, run.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                for a in p {
                    println!("{:<20} {:<12} {:?}", a.agent, a.status, a.provider);
                }
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Heartbeat { agent, status, run } => {
            bus::heartbeat(&paths, run.as_deref(), &agent, &status)?;
            Ok(ExitCode::Success)
        }
        BusCmd::Deliver { agent, run, json } => bus_deliver(&paths, run.as_deref(), &agent, json),
        BusCmd::Ack {
            msg_id,
            from,
            run,
            json,
        } => {
            let msg = bus::ack(&paths, run.as_deref(), &from, &msg_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msg)?);
            } else {
                println!("acked {msg_id}");
            }
            Ok(ExitCode::Success)
        }
        BusCmd::Reserve { path, holder, run } => {
            bus::reserve(&paths, run.as_deref(), &path, &holder)?;
            println!("reserved {path} by {holder}");
            Ok(ExitCode::Success)
        }
        BusCmd::Release { path, holder, run } => {
            bus::release(&paths, run.as_deref(), &path, &holder)?;
            println!("released {path}");
            Ok(ExitCode::Success)
        }
    }
}

/// Resolve `agent`'s delivery strategy from its run slot's provider adapter. An agent
/// with no slot or no CLI adapter (bare agent / api slot) has no injection channel.
fn agent_delivery_strategy(state: &state::RunState, agent: &str) -> providers::DeliveryStrategy {
    state
        .slots
        .iter()
        .find(|s| s.id == agent)
        .and_then(|s| providers::adapter_named(&s.provider))
        .map(|a| a.delivery_strategy())
        .unwrap_or(providers::DeliveryStrategy::None)
}

fn bus_deliver(
    paths: &paths::SparPaths,
    run_id: Option<&str>,
    agent: &str,
    json: bool,
) -> Result<ExitCode> {
    // The hook / `$SPAR_AGENT_ID` may hand us the short slot id (+ `--run`) or the
    // already-unique `run:slot` id. The inbox drain keys on the unique id; the delivery
    // strategy is looked up by the short slot id (`state.slots[].id`).
    let unique = bus::resolve_addr(run_id, agent);
    let short = run_id
        .and_then(|r| agent.strip_prefix(&format!("{r}:")))
        .unwrap_or(agent);
    // A bare agent has no run slot/state, so it has no injection channel — it drains its
    // own workspace inbox on its next turn (strategy `None`).
    let (strategy, session_id, dry_run) = match run_id {
        Some(run) => {
            let state = state::RunState::load(paths, run)?;
            let strat = agent_delivery_strategy(&state, short);
            // The slot's own live sidecar, not `state.usage` (only set once a dispatch
            // finishes): a mid-run codex thread id must be found while it is still
            // running, the exact moment a turn-boundary push needs it.
            let sid = state
                .slots
                .iter()
                .find(|s| s.id == short)
                .and_then(|s| s.log_path.as_deref())
                .and_then(process::StreamStats::load)
                .and_then(|s| s.session_id);
            (
                strat,
                sid,
                state.dry_run || util::env_truthy("SPAR_DRY_RUN"),
            )
        }
        None => (
            providers::DeliveryStrategy::None,
            None,
            util::env_truthy("SPAR_DRY_RUN"),
        ),
    };
    // A turn boundary is one of the swarm's delivery pulses: advance any unacked-message
    // redeliveries first so a due redelivery lands in this same drain. This is not the
    // only pulse — the wait loop and TUI refresh also tick acks, so redelivery/escalation
    // advances in runs with no Claude slot (whose Stop hook is the only pulse here).
    bus::tick_acks(paths, &bus::AckPolicy::default(), chrono::Utc::now())?;
    let d = providers::delivery::deliver(
        paths,
        run_id,
        &unique,
        strategy,
        session_id.as_deref(),
        dry_run,
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&d)?);
    } else if let Some(payload) = &d.payload {
        // Hook mode: stdout carries only the raw injection payload for the hook runner.
        println!("{payload}");
    }
    Ok(ExitCode::Success)
}

/// Config for `spar implement`.
///
/// A new run (`--task` / `--plan`) starts from the project's live config; `--run <id>`
/// binds to the config that run was created with, so an agent editing `spar.toml` for
/// its own run cannot change the fleet, timeouts or ship gate of one already in flight.
/// `--reload-config` is the deliberate way out: it re-reads the file and re-freezes the
/// run against it.
fn implement_ctx(
    run_id: Option<&str>,
    role: &[String],
    without: &[String],
    fleet: Option<&str>,
    reload_config: bool,
) -> Result<(paths::SparPaths, Config)> {
    let (paths, mut cfg) = project_ctx()?;
    let Some(run_id) = run_id else {
        if let Some(preset) = fleet {
            cfg.apply_fleet_preset(config::FleetPreset::parse(preset)?);
        }
        cfg.apply_without(without)?;
        cfg.apply_role_overrides(role)?;
        return Ok((paths, cfg));
    };
    if !reload_config {
        if !role.is_empty() {
            anyhow::bail!(
                "run {run_id} is bound to the config it was created with; \
                 pass --reload-config to apply --role to it"
            );
        }
        if !without.is_empty() {
            anyhow::bail!(
                "run {run_id} is bound to the config it was created with; \
                 pass --reload-config to apply --without to it"
            );
        }
        if let Some(preset) = fleet {
            // Validate before refusing: an unknown preset name is its own error, not
            // one the `--reload-config` refusal should swallow. `standard` is a
            // documented no-op over the file (never re-enables a channel the file
            // already disabled), so it never needs to change a bound run's frozen
            // config and must not be refused just for being spelled out.
            if config::FleetPreset::parse(preset)? != config::FleetPreset::Standard {
                anyhow::bail!(
                    "run {run_id} is bound to the config it was created with; \
                     pass --reload-config to apply --fleet to it"
                );
            }
        }
        let cfg = Config::for_run(&paths, run_id)?;
        return Ok((paths, cfg));
    }
    if let Some(preset) = fleet {
        cfg.apply_fleet_preset(config::FleetPreset::parse(preset)?);
    }
    cfg.apply_without(without)?;
    cfg.apply_role_overrides(role)?;
    cfg.save_snapshot(&paths, run_id)?;
    eprintln!("config: re-read spar.toml for run {run_id}");
    Ok((paths, cfg))
}

fn project_ctx() -> Result<(paths::SparPaths, Config)> {
    let root = paths::find_project_root()?;
    let paths = paths::SparPaths::new(&root);
    let cfg = Config::load(&root)?;
    Ok((paths, cfg))
}

/// Archive finished runs that have gone quiet, once per launch. Best-effort and silent on
/// failure: housekeeping must never be why a run did not start.
fn auto_archive_at_launch() {
    let Ok((paths, cfg)) = project_ctx() else {
        return;
    };
    let Some(idle) = cfg.auto_archive_idle() else {
        return;
    };
    match state::auto_archive(&paths, idle, chrono::Utc::now()) {
        Ok(ids) if !ids.is_empty() => eprintln!(
            "archived {} finished run(s) idle over {} (spar status --archived)",
            ids.len(),
            cfg.auto_archive_after
        ),
        _ => {}
    }
}

fn status_cmd(run_id: Option<String>, json: bool, all: bool, archived: bool) -> Result<ExitCode> {
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
            if let (Some(r), Some(c)) = (&state.base_ref, &state.base_commit) {
                println!("base: {r} ({})", c.chars().take(8).collect::<String>());
            }
            println!("phase: {:?}", state.phase);
            // Looking a run up by id is the documented way to reach an archived one, so it
            // has to say that it is hidden — otherwise the output is indistinguishable
            // from a run that simply is not in the listing for some other reason.
            if let Some(at) = state.archived_at {
                println!(
                    "archived: {}  (hidden from listings; spar archive {} --undo)",
                    at.to_rfc3339(),
                    state.id
                );
            }
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
                Some(t) => {
                    let alive = t.alive();
                    println!("orchestrator: pid={} alive={alive}", t.pid);
                }
                None => println!("orchestrator: none"),
            }
            if state.abandoned(&swarm) {
                println!(
                    "abandoned: no live orchestrator (resume with `spar resume {}` or `spar stop {}`)",
                    state.id, state.id
                );
            }
            println!("slots: {}", state.slots.len());
            let hb_map = bus::heartbeat_map(&swarm, Some(&state.id));
            for slot in &state.slots {
                let hb = hb_map
                    .get(&bus::resolve_addr(Some(&state.id), &slot.id))
                    .copied();
                let soft = executor::timeout_for_role(&cfg, slot.role).as_secs();
                let act =
                    liveness::SlotActivity::observe(slot, cfg.timeouts.stall_warn_secs, soft, hb);
                let silent = act.human_silent();
                let stall = if act.stalled { " STALL" } else { "" };
                let token = markers::read_pid(&swarm, &state.id, &slot.id)
                    .or_else(|| slot.pid.map(process::PidToken::from_pid));
                let pid = token.map(|t| t.pid);
                let alive = token.map(|t| t.alive()).unwrap_or(false);
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
    let mut runs = if use_all {
        registry::list_all_runs()?
    } else {
        let root = local_root.as_ref().unwrap();
        let _ = registry::ensure_known(Some(root));
        registry::list_project_runs(root)?
    };

    // Filtered here, not in the listing functions: `load_run_anywhere` resolves a run id
    // through the same registry walk, and an archived run has to stay addressable by id.
    let hidden = if archived {
        0
    } else {
        let before = runs.len();
        runs.retain(|r| !r.archived);
        before - runs.len()
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
        if hidden > 0 {
            println!("({hidden} archived — spar status --archived)");
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
            let abandoned = if summary.abandoned { " ABANDONED" } else { "" };
            let arch = if summary.archived { " archived" } else { "" };
            let task = summary
                .task
                .as_deref()
                .map(|t| format!("  {}", truncate_cli(t, 40)))
                .unwrap_or_default();
            println!(
                "    {}  {:?}{}{abandoned}{arch}{task}",
                summary.id, summary.phase, dry
            );
        }
        if hidden > 0 {
            println!("  ({hidden} archived hidden — spar status --archived)");
        }
    }
    Ok(ExitCode::Success)
}

/// `spar brief <id>`: re-hydrate a fresh session on a run's own state, in one call.
/// Read-only — uses `load_for_display`, same as `status`, and never writes.
fn brief_cmd(run_id: &str, full: bool, json: bool) -> Result<ExitCode> {
    let local_root = paths::find_project_root().ok();
    let (paths, _cfg, state) = load_run_anywhere(run_id, local_root.as_deref())?;

    let brief_body: Option<String> = match &state.brief {
        // `state.brief` is project-root-relative; `Path::join` passes an already-
        // absolute path through unchanged, so this needs no separate branch for one.
        Some(path) => std::fs::read_to_string(paths.project_root.join(path)).ok(),
        None => None,
    };
    let brief_text = brief_body.as_deref().or(state.task.as_deref());

    let artifact_names = [
        "plan.md",
        "test-contract.md",
        "suite.md",
        "ranking.md",
        "ship.md",
    ];
    let mut artifacts: Vec<(String, std::path::PathBuf)> = artifact_names
        .iter()
        .map(|n| (n.to_string(), paths.artifact(&state.id, n)))
        .filter(|(_, p)| p.is_file())
        .collect();
    for slot in &state.slots {
        let p = paths.artifact(&state.id, &format!("review-{}.md", slot.id));
        if p.is_file() {
            artifacts.push((format!("review-{}.md", slot.id), p));
        }
    }

    let ac_ids: Vec<String> = {
        let contract_path = paths.artifact(&state.id, "test-contract.md");
        let mut ids: Vec<String> = std::fs::read_to_string(&contract_path)
            .unwrap_or_default()
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .filter(|tok| tok.starts_with("AC-") && tok.len() > 3)
            .map(|tok| tok.to_string())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };

    let events: Vec<crate::events::Event> = events::read_all(&paths, &state.id)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .take(15)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let next = notify::next_command(&state);

    if json {
        let v = serde_json::json!({
            "run": state.id,
            "workflow": state.workflow,
            "phase": state.phase,
            "run_exit_code": state.status_exit_code(),
            "round": state.round,
            "max_rounds": state.max_rounds,
            "abandoned": state.abandoned(&paths),
            "archived": state.archived_at,
            "brief": brief_text,
            "base_ref": state.base_ref,
            "base_commit": state.base_commit,
            "worktrees": state.worktrees,
            "fleet": executor::role_assignments(&state),
            "gates": state.gates,
            "contract_modified": state.contract_modified,
            "suite_outcome": state.suite_outcome,
            "error": state.error,
            "artifacts": artifacts.iter().map(|(n, p)| serde_json::json!({"name": n, "path": p})).collect::<Vec<_>>(),
            "ac_ids": ac_ids,
            "recent_events": events,
            "next_command": next,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(ExitCode::Success);
    }

    println!("# run {} ({:?})", state.id, state.workflow);
    println!(
        "phase: {:?}  round: {}/{}",
        state.phase, state.round, state.max_rounds
    );
    println!("abandoned: {}", state.abandoned(&paths));
    if let Some(at) = state.archived_at {
        println!("archived: {}", at.to_rfc3339());
    }
    println!();
    println!("## brief");
    match brief_text {
        // The brief is the one thing a fresh session cannot cheaply re-derive, so it
        // is never truncated here — `--full` only controls whether `plan.md` /
        // `test-contract.md` are inlined below.
        Some(t) => println!("{t}"),
        None => println!("(none)"),
    }
    println!();
    if let (Some(r), Some(c)) = (&state.base_ref, &state.base_commit) {
        println!("base: {r} ({})", c.chars().take(8).collect::<String>());
    }
    println!("worktrees:");
    for w in &state.worktrees {
        println!("  - {} {} ({})", w.slot_id, w.path.display(), w.branch);
    }
    println!("fleet:");
    for r in executor::role_assignments(&state) {
        println!("  - {r}");
    }
    println!(
        "gates: plan_approved={} ship_confirmed={}",
        state.gates.plan_approved, state.gates.ship_confirmed
    );
    if state.contract_modified {
        println!("contract_modified: true");
    }
    if let Some(o) = state.suite_outcome {
        println!("suite_outcome: {o:?}");
    }
    if let Some(err) = &state.error {
        println!("error: {err}");
    }
    println!("artifacts:");
    for (name, path) in &artifacts {
        if full && (name == "plan.md" || name == "test-contract.md") {
            println!("  - {name}:");
            if let Ok(body) = std::fs::read_to_string(path) {
                println!("{body}");
            }
        } else {
            println!("  - {} ({})", name, path.display());
        }
    }
    if !ac_ids.is_empty() {
        println!("acceptance criteria: {}", ac_ids.join(", "));
    }
    println!("recent events:");
    for e in &events {
        println!("  - {} {:?}", e.ts.to_rfc3339(), e.kind);
    }
    println!();
    println!("next: {next}");
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
        let orch = runlock::RunLock::owner(swarm, &state.id);
        obj.insert(
            "orchestrator_pid".into(),
            match &orch {
                Some(t) => serde_json::json!(t.pid),
                None => serde_json::Value::Null,
            },
        );
        obj.insert(
            "orchestrator_alive".into(),
            serde_json::Value::Bool(orch.map(|t| t.alive()).unwrap_or(false)),
        );
        let abandoned = state.abandoned(swarm);
        obj.insert("abandoned".into(), serde_json::Value::Bool(abandoned));
        // Only meaningful when nobody owns the run: these are slot processes still
        // burning tokens with no orchestrator to collect their work.
        let orphans = if abandoned {
            state::live_slot_pids(swarm, state)
        } else {
            Vec::new()
        };
        obj.insert("orphan_pids".into(), serde_json::json!(orphans));
    }
    liveness::enrich_status_json(&mut v, &state.slots, cfg, swarm, &state.id);
    Ok(v)
}

fn stop_cmd(run_id: Option<&str>, abandoned: bool, json: bool) -> Result<ExitCode> {
    match (run_id, abandoned) {
        (Some(id), false) => stop_one(id, json),
        (None, true) => stop_abandoned(json),
        (Some(_), true) => {
            anyhow::bail!("pass a run id or --abandoned, not both")
        }
        (None, false) => anyhow::bail!("usage: spar stop <run_id> | spar stop --abandoned"),
    }
}

/// Reap every run that is in flight with no live orchestrator. Explicit by design: a
/// read command must never kill processes, and a human driving a slot through the TUI
/// holds no run lock, so their run reads as abandoned too.
fn stop_abandoned(json: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let mut swept = Vec::new();
    for summary in state::list_runs(&paths)? {
        let Ok(state) = state::RunState::load(&paths, &summary.id) else {
            continue;
        };
        if !state.abandoned(&paths) {
            continue;
        }
        let orphans = state::live_slot_pids(&paths, &state);
        let outcome = stop_one_quiet(&paths, &summary.id);
        swept.push(serde_json::json!({
            "run_id": summary.id,
            "phase": format!("{:?}", state.phase),
            "orphan_pids": orphans,
            "stopped": outcome.is_ok(),
        }));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "swept": swept }))?
        );
    } else if swept.is_empty() {
        println!("no abandoned runs");
    } else {
        for r in &swept {
            println!(
                "stopped {} (was {}), reaped {} slot process(es)",
                r["run_id"].as_str().unwrap_or_default(),
                r["phase"].as_str().unwrap_or_default(),
                r["orphan_pids"].as_array().map(|a| a.len()).unwrap_or(0)
            );
        }
    }
    Ok(ExitCode::Success)
}

fn stop_one_quiet(paths: &paths::SparPaths, run_id: &str) -> Result<()> {
    let reason = halt_reason(paths, run_id);
    reap_run(paths, run_id)?;
    let mut state = state::RunState::load(paths, run_id)?;
    // After the reap, before the phase: the orchestrator was killed inside its dispatch,
    // so its slots are frozen at `running` and only this settles them (O49). `Nobody` is
    // what the reap just established, not something to re-observe: a just-SIGKILLed
    // orchestrator whose parent has not reaped it is a zombie, and a zombie answers
    // `kill(pid, 0)` and keeps its `/proc` start-time, so observing here reads the
    // process spar itself just killed as a live owner and skips the demotion.
    state.reconcile_and_save(paths, state::RunOwner::Nobody, reason)?;
    if !phase_at_rest(state.phase) {
        state.set_phase(state::Phase::Stopped);
        state.save(paths)?;
    }
    Ok(())
}

/// Marker, then orchestrator, then slot process groups — the order matters, see below.
fn reap_run(paths: &paths::SparPaths, run_id: &str) -> Result<()> {
    let state = state::RunState::load(paths, run_id)?;
    // 1. Marker first: an orchestrator that survives the signal stops at its next
    //    dispatch boundary instead of resurrecting a killed slot.
    markers::write_marker(paths, run_id, "stopped", "stopped by operator\n")?;

    // 2. Orchestrator before slots: signalling slots first lets the orchestrator
    //    re-dispatch them. The orchestrator is not a group leader — bare pid.
    if let Some(owner) = runlock::RunLock::owner(paths, run_id) {
        if owner.alive() {
            process::terminate_tree(owner.pid, false);
        }
    }

    // 3. Slot process groups: reaps nested cargo test / pnpm build children too.
    //    Start-time checked wherever the token carries one, which every `.pid` marker
    //    now does; a legacy record with only `slot.pid` degrades to bare liveness,
    //    deliberately, because missing an orphan leaves it burning tokens. The reconcile
    //    path trades the other way; see `state::slot_process_alive`.
    for pid in state::live_slot_pids(paths, &state) {
        process::terminate_tree(pid, true);
    }
    Ok(())
}

/// What to record on a slot this stop is about to settle. Read **before** the reap,
/// which kills the orchestrator and would make every stop look like a crash: a run that
/// had a live orchestrator when the operator typed `spar stop` was halted, not orphaned.
fn halt_reason(paths: &paths::SparPaths, run_id: &str) -> &'static str {
    match state::RunOwner::observe(paths, run_id) {
        state::RunOwner::Live => state::STOPPED_SLOT,
        state::RunOwner::Nobody => state::ORPHANED_SLOT,
    }
}

fn stop_one(run_id: &str, json: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let cfg = Config::for_run(&paths, run_id)?;
    let state = state::RunState::load_for_display(&paths, run_id)?;

    // A finished or gated run is already at rest: never downgrade it to Stopped or
    // drop a resumable marker that would make a later `implement --run` redo work.
    if phase_at_rest(state.phase) {
        if json {
            let v = run_status_json(&paths, &cfg, &state)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("run {run_id} already at {:?}; nothing to stop", state.phase);
        }
        return Ok(ExitCode::Success);
    }

    // A queued run has no orchestrator to reap, just a spool file naming it as wanted.
    // Clear it first so a capacity-freeing tick can never admit a run the operator just
    // told to stop.
    let _ = std::fs::remove_file(paths.queue_file(run_id));

    let reason = halt_reason(&paths, run_id);
    reap_run(&paths, run_id)?;

    // 4. The kill window above spans seconds; the orchestrator may have finished
    //    naturally and persisted a terminal/gate phase while dying. Reload,
    //    settle the slots the reap orphaned (`Nobody` is established by the reap, see
    //    `stop_one_quiet`), and
    //    re-check: never downgrade a run that reached rest on its own, and drop the
    //    stopped marker so a later `implement --run` does not redo finished work.
    let mut state = state::RunState::load(&paths, run_id)?;
    state.reconcile_and_save(&paths, state::RunOwner::Nobody, reason)?;
    if phase_at_rest(state.phase) {
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
        if json {
            let v = run_status_json(&paths, &cfg, &state)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!(
                "run {run_id} finished at {:?} before stop took effect; left as-is",
                state.phase
            );
        }
        return Ok(ExitCode::Success);
    }
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

/// A run at a terminal (non-`PlanApproved`) or gate phase is already at rest and must
/// not be downgraded to `Stopped`. `PlanApproved` is `is_terminal` only for the plan
/// sub-workflow; it is the normal resumable plan→implement handoff, so stop applies there.
fn phase_at_rest(phase: state::Phase) -> bool {
    let finished = phase.is_terminal() && phase != state::Phase::PlanApproved;
    finished || phase.is_gate()
}

/// Settle slots an orchestrator left at `running` when it died, on runs that already
/// exist. The backfill counterpart of the reconciliation `stop` and resume now do.
///
/// Deliberately narrow, and deliberately **not** part of `spar cleanup`: the only writes
/// are `slots[].status` and `slots[].error`, patched into the existing `state.json`
/// document. It never resolves, reads or removes a worktree path, never touches
/// `logs/` or `artifacts/`, and never signals a process. A worktree is often the only
/// copy of a slot's work, and this command wears a housekeeping name.
fn reconcile_state_cmd(
    run_id: Option<&str>,
    all: bool,
    apply: bool,
    json: bool,
) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let ids: Vec<String> = match (run_id, all) {
        (Some(_), true) => anyhow::bail!("pass a run id or --all, not both"),
        (None, false) => {
            anyhow::bail!("usage: spar reconcile-state <run_id> | spar reconcile-state --all")
        }
        (Some(id), false) => vec![id.to_string()],
        (None, true) => state::list_runs(&paths)?
            .into_iter()
            .map(|s| s.id)
            .collect(),
    };

    let mut changes = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        let Ok(state) = state::RunState::load(&paths, &id) else {
            continue;
        };
        // A live orchestrator is the authority on its own run's slots. This command
        // decides from a snapshot and writes some time later, so applying to a run in
        // flight can stamp a verdict over one the orchestrator recorded in between.
        // The realistic trigger is the intended use: `--all --apply` while work is running.
        let owner = state::RunOwner::observe(&paths, &id);
        if owner == state::RunOwner::Live {
            skipped.push(serde_json::json!({ "run_id": id, "reason": "orchestrator is live" }));
            continue;
        }
        let before: Vec<state::SlotStatus> = state.slots.iter().map(|s| s.status).collect();
        let mut settled = state;
        if !settled.reconcile_dead_slots(&paths, owner, state::ORPHANED_SLOT) {
            continue;
        }
        let mut patch = Vec::new();
        for (slot, was) in settled.slots.iter().zip(before) {
            if slot.status == was {
                continue;
            }
            patch.push((
                slot.id.clone(),
                serde_json::to_value(slot.status)?,
                slot.error.clone(),
            ));
            changes.push(serde_json::json!({
                "run_id": id,
                "slot_id": slot.id,
                "from": serde_json::to_value(was)?,
                "to": serde_json::to_value(slot.status)?,
                "error": slot.error,
            }));
        }
        if apply {
            patch_slot_status(&paths, &id, &patch)?;
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "applied": apply,
                "changes": changes,
                "skipped": skipped,
            }))?
        );
        return Ok(ExitCode::Success);
    }
    for sk in &skipped {
        println!(
            "{}: skipped, {}",
            sk["run_id"].as_str().unwrap_or_default(),
            sk["reason"].as_str().unwrap_or_default()
        );
    }
    if changes.is_empty() {
        println!("nothing to reconcile");
        return Ok(ExitCode::Success);
    }
    for c in &changes {
        let reason = c["error"]
            .as_str()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        println!(
            "{} {}: {} -> {}{reason}",
            c["run_id"].as_str().unwrap_or_default(),
            c["slot_id"].as_str().unwrap_or_default(),
            c["from"].as_str().unwrap_or_default(),
            c["to"].as_str().unwrap_or_default(),
        );
    }
    if apply {
        println!("updated {} slot(s)", changes.len());
    } else {
        println!(
            "{} slot(s) would change; re-run with --apply",
            changes.len()
        );
    }
    Ok(ExitCode::Success)
}

/// Patch `status` (and `error`, where one was derived) into the run's existing
/// `state.json` document.
///
/// Value-level rather than typed, so a field spar does not model survives the write; the
/// document is re-read here rather than re-serialized from the caller's snapshot. Only
/// those two fields change *in content*; the round trip does re-sort keys alphabetically
/// until the next `RunState::save` writes struct order back.
fn patch_slot_status(
    paths: &paths::SparPaths,
    run_id: &str,
    patch: &[(String, serde_json::Value, Option<String>)],
) -> Result<()> {
    let file = paths.state_file(run_id);
    let mut doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
    let Some(slots) = doc.get_mut("slots").and_then(|s| s.as_array_mut()) else {
        anyhow::bail!("run {run_id}: state.json has no slots array");
    };
    for (slot_id, status, error) in patch {
        let Some(slot) = slots.iter_mut().find(|s| s["id"] == slot_id.as_str()) else {
            continue;
        };
        slot["status"] = status.clone();
        if let Some(e) = error {
            slot["error"] = serde_json::Value::String(e.clone());
        }
    }
    std::fs::write(&file, serde_json::to_string_pretty(&doc)?)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", file.display()))?;
    Ok(())
}

/// Observe-only: the returned state is reconciled against on-disk markers, never written back.
fn load_run_anywhere(
    run_id: &str,
    local_root: Option<&std::path::Path>,
) -> Result<(paths::SparPaths, Config, state::RunState)> {
    if let Some(root) = local_root {
        let swarm = paths::SparPaths::new(root);
        if let Ok(state) = state::RunState::load_for_display(&swarm, run_id) {
            let cfg = Config::for_run(&swarm, run_id).unwrap_or_default();
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
        if let Ok(state) = state::RunState::load_for_display(&swarm, run_id) {
            let cfg = Config::for_run(&swarm, run_id).unwrap_or_default();
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

/// Delete build output inside finished runs' worktrees.
///
/// Kept apart from `cleanup` in the CLI on purpose: cleanup removes worktrees and carries
/// the questions that come with that, this removes only bytes a build regenerates. Making
/// it a `cleanup` flag would give an operator with a standing rule against autonomous
/// sweeps a back door into one.
fn reclaim_cmd(run_id: Option<&str>, all: bool, json: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let reaps = match (run_id, all) {
        (Some(_), true) => anyhow::bail!("pass a run id or --all, not both"),
        (None, false) => anyhow::bail!("usage: spar reclaim <run_id> | spar reclaim --all"),
        (Some(id), false) => {
            let state = state::RunState::load(&paths, id)?;
            if !(state.phase.is_terminal() || state.phase == state::Phase::Stopped) {
                anyhow::bail!(
                    "run {id} is still running ({:?}) — its build cache is in use",
                    state.phase
                );
            }
            vec![worktree::reap_build_cache(
                &state,
                &worktree::LiveCwds::snapshot(),
            )]
        }
        (None, true) => worktree::reap_finished_caches(&paths, None)?,
    };
    let freed: u64 = reaps.iter().map(|r| r.freed_bytes).sum();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "freed_bytes": freed, "runs": reaps })
            )?
        );
        return Ok(ExitCode::Success);
    }
    for r in &reaps {
        if r.freed_bytes > 0 {
            println!("{}: {}", r.run_id, human_bytes(r.freed_bytes));
        }
        for s in &r.skipped {
            println!("  skipped {s}");
        }
    }
    println!("reclaimed {}", human_bytes(freed));
    Ok(ExitCode::Success)
}

fn human_bytes(n: u64) -> String {
    const UNIT: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNIT.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNIT[i])
}

/// Fold a run into another as a leg of the same unit of work (O46). spar never infers
/// this: pairing legs by task text would silently merge two unrelated issues.
fn link_cmd(run_id: &str, parent: Option<&str>, undo: bool, json: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    // Whole-file read-modify-write, so it needs the lock a live orchestrator holds:
    // without it the orchestrator's next save silently drops the link.
    let _lock = crate::runlock::RunLock::acquire(&paths, run_id)?;
    let mut state = state::RunState::load(&paths, run_id)?;
    if undo {
        state.parent_run = None;
    } else {
        let parent =
            parent.ok_or_else(|| anyhow::anyhow!("usage: spar link <run> --to <run> | --undo"))?;
        if parent == run_id {
            anyhow::bail!("a run cannot be a leg of itself");
        }
        // Resolve through the parent's own link so chains flatten to one unit of work.
        let mut root = state::RunState::load(&paths, parent)?;
        let mut hops = 0;
        while let Some(next) = root.parent_run.clone() {
            if next == run_id {
                anyhow::bail!("that would make a cycle: {parent} is already a leg of {run_id}");
            }
            let Ok(up) = state::RunState::load(&paths, &next) else {
                break;
            };
            root = up;
            hops += 1;
            if hops > 16 {
                break;
            }
        }
        state.parent_run = Some(root.id.clone());
    }
    state.save(&paths)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "run": state.id,
                "parent_run": state.parent_run,
            }))?
        );
    } else {
        match &state.parent_run {
            Some(p) => println!("{} is now a leg of {p}", state.id),
            None => println!("{} stands on its own", state.id),
        }
    }
    Ok(ExitCode::Success)
}

/// Hide finished runs from listings, or bring them back.
///
/// Archiving is presentation, not lifecycle: nothing is deleted, the run stays addressable
/// by id (`spar status <id>` works archived), and resuming it clears the flag. That is what
/// separates it from `cleanup` (reclaims worktrees, keeps the record) and `--purge`
/// (deletes the record and its artifacts).
fn archive_cmd(
    run_id: Option<&str>,
    all: bool,
    older_than: Option<&str>,
    gates: bool,
    undo: bool,
    json: bool,
) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let now = chrono::Utc::now();
    // Flags that cannot apply are refused, not ignored: a silently dropped `--older-than`
    // reads as "nothing qualified" and hides the fact that the filter never ran.
    if gates && (run_id.is_some() || undo || !all) {
        anyhow::bail!("--gates only applies to `spar archive --all`");
    }
    if older_than.is_some() && (run_id.is_some() || undo) {
        anyhow::bail!("--older-than only applies to `spar archive --all`");
    }
    let changed: Vec<String> = match (run_id, all) {
        (Some(_), true) => anyhow::bail!("pass a run id or --all, not both"),
        (None, false) => anyhow::bail!("usage: spar archive <run_id> | spar archive --all"),
        (Some(id), false) => {
            let mut state = state::RunState::load(&paths, id)?;
            if undo {
                state.archived_at = None;
            } else {
                if !state::archivable_by_hand(state.phase) {
                    anyhow::bail!(
                        "run {id} is in flight ({:?}) — stop it before archiving",
                        state.phase
                    );
                }
                state.archived_at = Some(now);
            }
            state.save(&paths)?;
            vec![id.to_string()]
        }
        (None, true) if undo => {
            let mut out = Vec::new();
            for summary in state::list_runs(&paths)? {
                if !summary.archived {
                    continue;
                }
                let Ok(mut s) = state::RunState::load(&paths, &summary.id) else {
                    continue;
                };
                s.archived_at = None;
                if s.save(&paths).is_ok() {
                    out.push(summary.id);
                }
            }
            out
        }
        (None, true) => {
            let min_idle = older_than
                .map(util::parse_duration)
                .transpose()?
                .unwrap_or_default();
            state::archive_sweep(&paths, min_idle, now, true, gates)?
        }
    };
    let verb = if undo { "unarchived" } else { "archived" };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ verb: changed }))?
        );
    } else if changed.is_empty() {
        println!("nothing to {}", if undo { "unarchive" } else { "archive" });
    } else {
        println!("{verb} {} run(s): {}", changed.len(), changed.join(", "));
    }
    Ok(ExitCode::Success)
}

fn cleanup_cmd(
    run_id: Option<&str>,
    all: bool,
    older_than: Option<&str>,
    merged: bool,
    json: bool,
    purge: bool,
    force: bool,
) -> Result<ExitCode> {
    // No project-wide force. A sweep decides *for you* which runs it touches, so pairing
    // it with an override that ignores unsaved work is how one command destroys work
    // across a project — the failure this whole change exists to prevent.
    if force && all {
        anyhow::bail!("--force applies to an explicit run id, not --all");
    }
    match (run_id, all) {
        (Some(id), false) => cleanup_one(id, json, purge, force),
        (None, true) => cleanup_sweep(older_than, merged, json, purge),
        (Some(_), true) => anyhow::bail!("pass a run id or --all, not both"),
        (None, false) => anyhow::bail!("usage: spar cleanup <run_id> | spar cleanup --all"),
    }
}

/// Reap the worktrees of every finished run in the project. Never touches a run that is
/// in flight: `spar stop` (or `stop --abandoned`) parks those first, and only then does
/// their work become garbage.
fn cleanup_sweep(
    older_than: Option<&str>,
    merged: bool,
    json: bool,
    purge: bool,
) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let min_idle = older_than.map(util::parse_duration).transpose()?;
    let now = chrono::Utc::now();
    let mut swept = Vec::new();
    let mut spared = Vec::new();
    for summary in state::list_runs(&paths)? {
        let Ok(state) = state::RunState::load(&paths, &summary.id) else {
            continue;
        };
        let idle = (now - state.updated_at).to_std().unwrap_or_default();
        // Reported, not silent: a sweep that prints "nothing to sweep" while gigabytes of
        // finished work sit on disk reads as a refusal rather than a policy.
        let mut skip = state::sweep_skip_reason(state.phase, idle, min_idle);
        // `--merged` is evidence in its own right, and stronger than age: the work is in
        // the base branch. It still cannot reach a run in flight.
        if let (Some(why), true, true) = (&skip, merged, state::at_rest(state.phase)) {
            skip = match worktree::merged_into_base(&state) {
                Some(true) => None,
                Some(false) => Some(format!(
                    "{why}, and not merged into {}",
                    state.base_ref.as_deref().unwrap_or("its base")
                )),
                // `None` is "no verdict", which covers a missing base ref *and* a run
                // whose branches no longer resolve — naming only the first misreports
                // the second, which is the state every already-reaped run is left in.
                None => Some(format!("{why}, and has nothing left to judge as merged")),
            };
        }
        if let Some(reason) = skip {
            spared.push(serde_json::json!({
                "run_id": summary.id,
                "phase": format!("{:?}", state.phase),
                "idle_secs": idle.as_secs(),
                "reason": reason,
            }));
            continue;
        }
        let cleaned = worktree::cleanup_run(&state, /* force */ false)?;
        if let Some(session) = &state.tmux_session {
            let _ = tmux::kill_session(session);
        }
        // Never purge the record of a run whose worktree was spared. The record is how
        // `cleanup <id> --force` reaches it later, and it carries base_ref/base_commit and
        // plan.md; deleting it strands the very work the veto just saved.
        let kept_any = cleaned.iter().any(|c| c.skipped.is_some());
        if purge && !kept_any {
            let dir = paths.run_dir(&summary.id);
            if dir.is_dir() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
        swept.push(serde_json::json!({
            "run_id": summary.id,
            "phase": format!("{:?}", state.phase),
            "idle_secs": idle.as_secs(),
            "worktrees": cleaned,
            "purged": purge,
        }));
    }
    if purge {
        worktree::prune_empty_spar_parents(&paths)?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "swept": swept,
                "spared": spared,
                // Aggregates, because a driver reading `swept.len()` alone counts a run
                // whose every worktree was refused as reclaimed.
                "worktrees_removed": swept
                    .iter()
                    .map(|r| r["removed"].as_u64().unwrap_or(0))
                    .sum::<u64>(),
                "worktrees_kept": swept
                    .iter()
                    .map(|r| r["kept"].as_u64().unwrap_or(0))
                    .sum::<u64>(),
            }))?
        );
        return Ok(ExitCode::Success);
    }
    if swept.is_empty() {
        println!("nothing to sweep");
    } else {
        let mut trees = 0;
        let mut kept = 0;
        for r in &swept {
            let entries = r["worktrees"].as_array().cloned().unwrap_or_default();
            // Count what was actually removed. Counting every entry reports a worktree the
            // veto *refused* as swept, which is the one number an operator would read as
            // "the work is gone".
            let n = entries.iter().filter(|w| w["removed"] == true).count();
            trees += n;
            println!(
                "{} ({}): {n} worktree(s)",
                r["run_id"].as_str().unwrap_or_default(),
                r["phase"].as_str().unwrap_or_default()
            );
            for w in entries.iter() {
                if let Some(why) = w["skipped"].as_str() {
                    kept += 1;
                    println!("    kept {}: {why}", w["path"].as_str().unwrap_or_default());
                }
            }
        }
        println!("swept {} run(s), {trees} worktree(s)", swept.len());
        if kept > 0 {
            println!("kept {kept} worktree(s) still holding work");
        }
    }
    // Capped: a project that has accumulated hundreds of runs would otherwise bury the
    // swept lines under a wall of spared ones. `--json` carries all of them.
    const SPARED_SHOWN: usize = 10;
    for r in spared.iter().take(SPARED_SHOWN) {
        println!(
            "spared {}: {}",
            r["run_id"].as_str().unwrap_or_default(),
            r["reason"].as_str().unwrap_or_default()
        );
    }
    if spared.len() > SPARED_SHOWN {
        println!(
            "... and {} more spared (--json for all)",
            spared.len() - SPARED_SHOWN
        );
    }
    Ok(ExitCode::Success)
}

fn cleanup_one(run_id: &str, json: bool, purge: bool, force: bool) -> Result<ExitCode> {
    let (paths, _) = project_ctx()?;
    let state = state::RunState::load(&paths, run_id)?;
    let cleaned = worktree::cleanup_run(&state, force)?;
    let kept_any = cleaned.iter().any(|c| c.skipped.is_some());
    if let Some(session) = &state.tmux_session {
        let _ = tmux::kill_session(session);
    }
    // Same rule as the sweep: the record is the only route back to a spared worktree.
    let purged = purge && !kept_any;
    if purged {
        let dir = paths.run_dir(run_id);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
        }
        worktree::prune_empty_spar_parents(&paths)?;
    }
    let removed = cleaned.iter().filter(|c| c.removed).count();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "run_id": run_id,
                // `cleaned: true` regardless of outcome told an outer agent the removal
                // happened when every worktree had been refused.
                "cleaned": removed > 0,
                "removed": removed,
                "kept": cleaned.iter().filter(|c| c.skipped.is_some()).count(),
                "purged": purged,
                "worktrees": cleaned,
            })
        );
    } else {
        for w in &cleaned {
            if let Some(reason) = &w.skipped {
                println!("skipped {}: {reason}", w.path.display());
                continue;
            }
            if !w.killed.is_empty() {
                let pids: Vec<String> = w.killed.iter().map(|p| p.to_string()).collect();
                println!(
                    "killed {} process(es) in {} (pid {})",
                    w.killed.len(),
                    w.path.display(),
                    pids.join(", ")
                );
            }
            if w.removed {
                println!("removed worktree {}", w.path.display());
            } else {
                println!("could not remove {}", w.path.display());
            }
        }
        println!("cleaned worktrees for {run_id}");
        if purge && kept_any {
            println!("kept the run record: a worktree still holds work");
        }
        if purged {
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
                        let key = quota::normalize_key(&p.name);
                        let q = quota.get(&key);
                        // Effective status: a lapsed pause reads Available, so the
                        // listing matches what a run will actually do (auto-recover).
                        let status = quota.effective_status(&key);
                        let hint = (status != quota::ProviderStatus::Available)
                            .then_some(q.hint)
                            .flatten();
                        serde_json::json!({
                            "name": p.name,
                            "available": p.available,
                            "path": p.path,
                            "version": p.version,
                            "capabilities": p.capabilities,
                            "quota_status": status,
                            "quota_hint": hint,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                for p in &report {
                    let mark = if p.available { "ok" } else { "missing" };
                    let status = quota.effective_status(&quota::normalize_key(&p.name));
                    println!(
                        "{:<8} {mark:<8} {:<12} {}",
                        p.name,
                        format!("{status:?}"),
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
            let key = quota::normalize_key(&name);
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
            store.pause_manual(&key, until_dt);
            store.save(&paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&store.get(&key))?);
            } else {
                println!("paused provider {key}");
            }
            Ok(ExitCode::Success)
        }
        cli::ProviderAction::Resume { name, json } => {
            let (paths, _) = project_ctx()?;
            let key = quota::normalize_key(&name);
            let mut store = quota::QuotaStore::load(&paths)?;
            store.resume(&key);
            store.save(&paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&store.get(&key))?);
            } else {
                println!("resumed provider {key}");
            }
            Ok(ExitCode::Success)
        }
        cli::ProviderAction::AgyStatuslineUninstall => {
            match providers::agy_telemetry::root() {
                Some(root) => {
                    providers::agy_telemetry::uninstall_statusline_hook(&root)?;
                    println!("removed spar agy statusline wrapper; restored original");
                }
                None => println!("agy config dir not found; nothing to uninstall"),
            }
            Ok(ExitCode::Success)
        }
    }
}
