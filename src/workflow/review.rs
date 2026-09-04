//! Concurrent multi-provider **independent review** (not split-stack peer).
use super::CommonOpts;
use crate::bus;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole, SlotStatus};
use crate::util::{self, sanitize_slot};
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

/// N independent reviewers in parallel, adversarial review prompt, then summary.
pub fn run(opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let task = opts
        .task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--task required for review"))?;
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let n = if !opts.providers.is_empty() {
        opts.providers.len().max(1)
    } else if opts.select.len() > 1 {
        opts.select.len()
    } else {
        2
    };
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Review,
        paths.project_root.clone(),
    );
    state.task = Some(task);
    state.backend = opts.backend;
    worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
    cfg.save_snapshot(paths, &state.id)?;
    // Reviews of existing trees: still isolate per reviewer so they don't stomp files.
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.message_budget = cfg.message_budget;
    state.autonomy = cfg.autonomy;
    let roles: Vec<&str> = (0..n).map(|_| "reviewer").collect();
    let requested = opts.resolve_fleet(n, &roles, paths, cfg, &state.id)?;
    state.providers = providers::pick_providers(&requested, n, Some(&requested), dry);
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

    for (i, prov) in state.providers.iter().enumerate() {
        let id = format!("review-{}-{}", i, sanitize_slot(prov));
        state
            .slots
            .push(executor::init_slot(&id, prov, SlotRole::Reviewer));
    }

    paths.ensure_run_dirs(&state.id)?;
    bus::ensure_bus(paths)?;
    bus::join(paths, Some(&state.id), "orchestrator", None, None)?;
    for s in &state.slots {
        let _ = bus::join(paths, Some(&state.id), &s.id, Some(&s.provider), None);
    }
    let _ = bus::broadcast(
        paths,
        Some(&state.id),
        "orchestrator",
        format!(
            "independent review: {} concurrent reviewers — no coordination, each votes alone",
            state.slots.len()
        ),
        state.message_budget,
    );
    if !opts.json {
        eprintln!(
            "roles: {} (concurrent independent review)",
            executor::role_assignments(&state).join(", ")
        );
    }
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
    }
    Ok(state.exit_code())
}

pub fn execute(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    if crate::workflow::implement::should_stop(paths, &state.id) {
        state.set_phase(Phase::Stopped);
        state.save(paths)?;
        return Ok(());
    }
    let ids: Vec<String> = state.slots.iter().map(|s| s.id.clone()).collect();
    // Reviewers share the project root view: worktrees still used for write safety,
    // but each gets the same task (independent).
    worktree::prepare_isolation(state, paths, &ids)?;
    state.set_phase(Phase::Review);
    state.save(paths)?;

    let jobs: Vec<SlotJob> = state
        .slots
        .iter()
        .map(|slot| {
            let review_cwd = slot
                .cwd
                .clone()
                .unwrap_or_else(|| state.project_root.clone());
            let mut extra = HashMap::new();
            extra.insert("review_cwd".into(), review_cwd.display().to_string());
            extra.insert(
                "task".into(),
                format!(
                    "{}\n\nYou are an **independent** reviewer among several. \
                     Do not coordinate with other agents. Produce your own verdict.",
                    state.task.as_deref().unwrap_or("")
                ),
            );
            SlotJob {
                slot_id: slot.id.clone(),
                provider: slot.provider.clone(),
                role: SlotRole::Reviewer,
                template: "reviewer".into(),
                extra_vars: extra,
                expected_artifact: Some(format!("review-{}.md", slot.id)),
                model: None,
            }
        })
        .collect();

    executor::run_slots_parallel(state, paths, cfg, &jobs)?;

    // Aggregate
    let mut body = format!(
        "# Independent review summary\n\nRun: {}\nTask: {}\n\n",
        state.id,
        state.task.as_deref().unwrap_or("")
    );
    let mut approve = 0u32;
    let mut changes = 0u32;
    for slot in &state.slots {
        let path = paths.artifact(&state.id, &format!("review-{}.md", slot.id));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "(missing review)".into());
        let lower = text.to_ascii_lowercase();
        let verdict = if lower.contains("request_changes") {
            changes += 1;
            "request_changes"
        } else if lower.contains("approve") {
            approve += 1;
            "approve"
        } else {
            "unknown"
        };
        body.push_str(&format!(
            "## {} ({}) — {verdict} — status={:?}\n\n",
            slot.id, slot.provider, slot.status
        ));
        body.push_str(&text);
        body.push_str("\n\n");
    }
    body.push_str(&format!(
        "\n## Tally\n- approve: {approve}\n- request_changes: {changes}\n- slots: {}\n",
        state.slots.len()
    ));
    std::fs::write(paths.artifact(&state.id, "summary.md"), body)?;

    let failed: Vec<&crate::state::SlotState> = state
        .slots
        .iter()
        .filter(|s| s.status == SlotStatus::Failed)
        .collect();
    let all_failed = !state.slots.is_empty() && failed.len() == state.slots.len();
    // Checked ahead of the approve/request_changes tally: `salvage_expected_artifact`
    // (executor.rs) writes a synthetic `request_changes` verdict for every interrupted
    // reviewer, so `changes == 0` never holds once any reviewer fails and the tally
    // alone can't tell a real review panel from one where every slot died on a rate
    // limit — a `changes == 0` guard on the `Failed` branch below would be dead code
    // for every all-failed panel, quota or not. Branch on `all_failed` directly instead:
    // every slot quota-hit parks at `Phase::Quota`; every slot failed for any other (or
    // mixed) reason still fails the run at `Phase::Failed` rather than falling through
    // to `Done` on the strength of two fabricated `request_changes` votes.
    if all_failed && failed.iter().all(|s| s.quota_hit) {
        state.set_phase(Phase::Quota);
        state.error = Some("all review slots failed: rate limit".into());
    } else if all_failed {
        state.set_phase(Phase::Failed);
        state.error = Some("all review slots failed".into());
    } else {
        state.set_phase(Phase::Done);
    }
    state.save(paths)?;
    if cfg.auto_cleanup && state.phase == Phase::Done {
        let _ = worktree::cleanup_run(state, false);
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
