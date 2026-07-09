use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::mailbox::{self, Message};
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::util;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

/// Peer workflow: two agents with mailbox protocol.
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
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    state.providers =
        providers::pick_providers(&cfg.providers.order, 2, opts.providers.as_deref(), dry);
    if dry && state.providers.len() < 2 {
        state.providers = vec!["claude".into(), "grok".into()];
    }
    while state.providers.len() < 2 {
        state.providers.push(state.providers[0].clone());
    }

    let a = state.providers[0].clone();
    let b = state.providers[1].clone();
    let id_a = format!("peer-a-{a}");
    let id_b = format!("peer-b-{b}");
    state
        .slots
        .push(executor::init_slot(&id_a, &a, SlotRole::Peer));
    state
        .slots
        .push(executor::init_slot(&id_b, &b, SlotRole::Peer));

    paths.ensure_run_dirs(&state.id)?;
    // seed handshake
    mailbox::send(
        paths,
        &state.id,
        &Message::new(
            "orchestrator",
            &id_a,
            "hello",
            "You are peer A. Coordinate with peer B via mailbox.",
        ),
    )?;
    mailbox::send(
        paths,
        &state.id,
        &Message::new(
            "orchestrator",
            &id_b,
            "hello",
            "You are peer B. Coordinate with peer A via mailbox.",
        ),
    )?;
    state.save(paths)?;

    if opts.detach {
        return detach(&state, opts.json);
    }
    execute(&mut state, paths, cfg)?;
    if opts.json {
        executor::emit_run_json(&state)?;
    } else {
        executor::print_run_human(&state);
        let msgs = mailbox::list(paths, &state.id)?;
        println!("mailbox messages: {}", msgs.len());
    }
    Ok(state.exit_code())
}

pub fn execute(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    let ids: Vec<String> = state.slots.iter().map(|s| s.id.clone()).collect();
    worktree::prepare_isolation(state, paths, &ids)?;
    state.set_phase(Phase::PeerRelay);
    state.save(paths)?;

    let slots: Vec<_> = state.slots.clone();
    for slot in &slots {
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
        let job = SlotJob {
            slot_id: slot.id.clone(),
            provider: slot.provider.clone(),
            role: SlotRole::Peer,
            template: "peer_half".into(),
            extra_vars: extra,
            expected_artifact: Some(format!("summary-{}.md", slot.id)),
        };
        let _ = executor::run_slot(state, paths, cfg, &job);
    }

    // relay summary of mailbox
    let msgs = mailbox::list(paths, &state.id)?;
    let mut body = format!("# Peer summary\n\nMessages: {}\n\n", msgs.len());
    for m in msgs {
        body.push_str(&format!("- {} → {}: {}\n", m.from, m.to, m.subject));
    }
    std::fs::write(paths.artifact(&state.id, "summary.md"), body)?;
    state.set_phase(Phase::Done);
    state.save(paths)?;
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
