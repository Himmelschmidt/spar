use super::CommonOpts;
use crate::bus::{self, MessageBudget, MsgKind};
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::util;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

/// Peer workflow: two agents with swarm bus protocol.
pub fn run(opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let task = opts
        .task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--task required for peer"))?;
    let dry = opts.resolve_dry_run();
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Peer,
        paths.project_root.clone(),
    );
    state.task = Some(task);
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.message_budget = cfg.message_budget;
    state.autonomy = cfg.autonomy;
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let requested = opts.resolve_fleet(2, &["implementer", "implementer"], paths, cfg, &state.id)?;
    state.providers = providers::pick_providers(&requested, 2, Some(&requested), dry);
    if state.providers.is_empty() {
        state.error = Some("no usable providers".into());
        state.set_phase(Phase::Failed);
        paths.ensure_run_dirs(&state.id)?;
        state.save(paths)?;
        if opts.json {
            executor::emit_run_json(&state)?;
        } else {
            eprintln!("error: no usable providers");
        }
        return Ok(ExitCode::Failure);
    }
    while state.providers.len() < 2 {
        state.providers.push(state.providers[0].clone());
    }

    let a = state.providers[0].clone();
    let b = state.providers[1].clone();
    let id_a = format!("peer-a-{}", a.replace(':', "-"));
    let id_b = format!("peer-b-{}", b.replace(':', "-"));
    state
        .slots
        .push(executor::init_slot(&id_a, &a, SlotRole::Peer));
    state
        .slots
        .push(executor::init_slot(&id_b, &b, SlotRole::Peer));

    paths.ensure_run_dirs(&state.id)?;
    bus::ensure_bus(paths, &state.id)?;
    bus::join(paths, &state.id, "orchestrator", None, None)?;
    bus::join(paths, &state.id, &id_a, Some(&a), Some("native-cli"))?;
    bus::join(paths, &state.id, &id_b, Some(&b), Some("native-cli"))?;
    bus::send(
        paths,
        &state.id,
        bus::BusMessage {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            ts: chrono::Utc::now(),
            from: "orchestrator".into(),
            to: id_a.clone(),
            kind: MsgKind::Hello,
            body: "You are peer A. Coordinate with peer B via the swarm bus.".into(),
            subject: Some("hello".into()),
            refs: bus::MsgRefs::default(),
            requires_ack: false,
            meta: HashMap::new(),
        },
        MessageBudget::Normal,
    )?;
    bus::send(
        paths,
        &state.id,
        bus::BusMessage {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            ts: chrono::Utc::now(),
            from: "orchestrator".into(),
            to: id_b.clone(),
            kind: MsgKind::Hello,
            body: "You are peer B. Coordinate with peer A via the swarm bus.".into(),
            subject: Some("hello".into()),
            refs: bus::MsgRefs::default(),
            requires_ack: false,
            meta: HashMap::new(),
        },
        MessageBudget::Normal,
    )?;
    let _ = bus::reserve(paths, &state.id, "contracts/", &id_a);
    state.save(paths)?;

    if opts.detach {
        return detach(&state, opts.json);
    }
    let _lock = crate::runlock::RunLock::acquire(paths, &state.id)?;
    execute(&mut state, paths, cfg)?;
    if opts.json {
        executor::emit_run_json(&state)?;
    } else {
        executor::print_run_human(&state);
        let msgs = bus::list_events(paths, &state.id)?;
        println!("bus events: {}", msgs.len());
    }
    Ok(state.exit_code())
}

pub fn execute(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    let ids: Vec<String> = state.slots.iter().map(|s| s.id.clone()).collect();
    worktree::prepare_isolation(state, paths, &ids)?;
    state.set_phase(Phase::PeerRelay);
    state.save(paths)?;

    // Concurrent halves (not A-then-B serial). Split-stack collaboration still
    // uses peer_half templates; for independent multi-review use --workflow review.
    let slots: Vec<_> = state.slots.clone();
    let jobs: Vec<SlotJob> = slots
        .iter()
        .map(|slot| {
            let partner = slots
                .iter()
                .find(|s| s.id != slot.id)
                .map(|s| s.id.clone())
                .unwrap_or_default();
            let mut extra = HashMap::new();
            extra.insert(
                "peer_role".into(),
                if slot.id.contains("peer-a") {
                    "peer-a"
                } else {
                    "peer-b"
                }
                .into(),
            );
            extra.insert("partner_slot".into(), partner);
            SlotJob {
                slot_id: slot.id.clone(),
                provider: slot.provider.clone(),
                role: SlotRole::Peer,
                template: "peer_half".into(),
                extra_vars: extra,
                expected_artifact: Some(format!("summary-{}.md", slot.id)),
            model: None,
            }
        })
        .collect();
    executor::run_slots_parallel(state, paths, cfg, &jobs)?;
    for slot in &slots {
        let partner = slots
            .iter()
            .find(|s| s.id != slot.id)
            .map(|s| s.id.as_str())
            .unwrap_or("broadcast");
        let _ = bus::chat(
            paths,
            &state.id,
            &slot.id,
            partner,
            format!("peer {} finished", slot.id),
            state.message_budget,
        );
    }

    let msgs = bus::list_events(paths, &state.id)?;
    let mut body = format!("# Peer summary\n\nBus events: {}\n\n", msgs.len());
    for m in msgs {
        body.push_str(&format!(
            "- {} → {} ({:?}): {}\n",
            m.from,
            m.to,
            m.kind,
            m.body.chars().take(80).collect::<String>()
        ));
    }
    std::fs::write(paths.artifact(&state.id, "summary.md"), body)?;
    state.set_phase(Phase::Done);
    state.save(paths)?;
    if cfg.auto_cleanup {
        let _ = worktree::cleanup_run(state);
    }
    Ok(())
}

fn detach(state: &RunState, json: bool) -> Result<ExitCode> {
    #[cfg(unix)]
    {
        let mut child_cmd = std::process::Command::new(std::env::current_exe()?);
        child_cmd
            .arg("__internal_continue")
            .arg(&state.id)
            .env("SPAR_INTERNAL", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = child_cmd.spawn()?;
    }
    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
    }
    Ok(ExitCode::Success)
}
