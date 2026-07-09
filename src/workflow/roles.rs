use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::templates;
use crate::util;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

/// Role workflow: frontend + backend style split with role templates.
pub fn run(opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let task = opts
        .task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--task required for roles"))?;
    let dry = opts.resolve_dry_run();
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Roles,
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

    let fe = state.providers[0].clone();
    let be = state.providers[1].clone();
    state.slots.push(executor::init_slot(
        format!("role-frontend-{fe}"),
        &fe,
        SlotRole::Peer,
    ));
    state.slots.push(executor::init_slot(
        format!("role-backend-{be}"),
        &be,
        SlotRole::Peer,
    ));

    paths.ensure_run_dirs(&state.id)?;
    // seed role notes
    std::fs::write(
        paths.artifact(&state.id, "roles.md"),
        format!(
            "# Roles\n\n## frontend\n{}\n\n## backend\n{}\n",
            templates::get("role_frontend").unwrap_or(""),
            templates::get("role_backend").unwrap_or("")
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
    }
    Ok(state.exit_code())
}

pub fn execute(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    let ids: Vec<String> = state.slots.iter().map(|s| s.id.clone()).collect();
    worktree::prepare_isolation(state, paths, &ids)?;
    state.set_phase(Phase::Dispatch);
    state.save(paths)?;

    let slots: Vec<_> = state.slots.clone();
    for slot in &slots {
        let peer_role = if slot.id.contains("frontend") {
            "frontend"
        } else {
            "backend"
        };
        let partner = slots
            .iter()
            .find(|s| s.id != slot.id)
            .map(|s| s.id.clone())
            .unwrap_or_default();
        let mut extra = HashMap::new();
        extra.insert("peer_role".into(), peer_role.into());
        extra.insert("partner_slot".into(), partner);
        let role_extra = templates::get(if peer_role == "frontend" {
            "role_frontend"
        } else {
            "role_backend"
        })
        .unwrap_or("");
        extra.insert(
            "task".into(),
            format!(
                "{}\n\nRole notes:\n{role_extra}",
                state.task.as_deref().unwrap_or("")
            ),
        );
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

    std::fs::write(
        paths.artifact(&state.id, "summary.md"),
        format!(
            "# Roles summary\n\nRun {}\nTask: {}\nSlots: {}\n",
            state.id,
            state.task.as_deref().unwrap_or(""),
            state.slots.len()
        ),
    )?;
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
