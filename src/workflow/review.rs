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
        // No explicit pool: honour a pinned `[roles].reviewer` panel's exact size
        // (Bug A) instead of always asking for two — a one-entry pin must not exhaust
        // `[providers].order` trying to fill a second seat nobody asked for.
        crate::workflow::roles_resolve::panel_size(cfg)
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
    let requested = opts.resolve_pool(n, &roles, paths, cfg, &state.id)?;
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

    // The independent-review workflow bypasses `build_implement_seats` (no panel
    // pinning), but each reviewer's source is still resolved per position so a `Synth`
    // pool mixing `[roles]` and `[providers].order` seats reports each honestly.
    let pool_origin = crate::workflow::roles_resolve::pool_origin_for(&opts);
    state.pool_origin = pool_origin;
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let has_backup = !cfg.backups.reviewer.is_empty();
    let available: Option<std::collections::HashSet<String>> = if dry || !has_backup {
        None
    } else {
        let detected: std::collections::HashSet<String> = crate::providers::detect_all()
            .into_iter()
            .filter(|r| r.available)
            .map(|r| format!("cli:{}", r.name))
            .collect();
        if detected.is_empty() {
            None
        } else {
            Some(detected)
        }
    };
    let sources = crate::workflow::roles_resolve::resolve_seat_sources(
        SlotRole::Reviewer,
        state.providers.len(),
        &requested,
        pool_origin,
        cfg,
    );
    for (i, prov) in state.providers.iter().enumerate() {
        let mut provider = prov.clone();
        let mut source = sources.get(i).copied();
        let mut model = crate::runspec::Pin::parse(&provider)
            .ok()
            .and_then(|p| p.model);
        let backup_pin =
            if !crate::backup::is_provider_eligible(&provider, &store, available.as_ref()) {
                if let Some(backup_raw) = crate::backup::backup_for_role(SlotRole::Reviewer, i, cfg)
                {
                    if crate::backup::is_provider_eligible(&backup_raw, &store, available.as_ref())
                    {
                        if let Ok(bpin) = crate::runspec::Pin::parse(&backup_raw) {
                            provider = bpin.display();
                            source = Some(crate::state::SeatSource::Backup);
                            model = bpin.model.clone();
                            Some(bpin)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
        let id = if source == Some(crate::state::SeatSource::Backup) {
            if let Some(bpin) = backup_pin {
                format!("review-{}-{}", i, sanitize_slot(&bpin.provider))
            } else {
                format!("review-{}-{}", i, sanitize_slot(&provider))
            }
        } else {
            format!("review-{}-{}", i, sanitize_slot(&provider))
        };
        let mut slot = executor::init_slot_model(&id, &provider, SlotRole::Reviewer, model);
        slot.source = source;
        state.slots.push(slot);
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
        return super::detach_and_wait(&state, paths, opts.json);
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

    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let has_backup = !cfg.backups.reviewer.is_empty();
    let available: Option<std::collections::HashSet<String>> = if state.dry_run || !has_backup {
        None
    } else {
        let detected: std::collections::HashSet<String> = crate::providers::detect_all()
            .into_iter()
            .filter(|r| r.available)
            .map(|r| format!("cli:{}", r.name))
            .collect();
        if detected.is_empty() {
            None
        } else {
            Some(detected)
        }
    };
    for (idx, job) in jobs.iter().enumerate() {
        let slot_state = match state.slots.iter().find(|s| s.id == job.slot_id).cloned() {
            Some(s) => s,
            None => continue,
        };
        if slot_state.status != SlotStatus::Failed {
            continue;
        }
        let cause = crate::backup::dispatch_stop_cause(
            &slot_state,
            &job.provider,
            &store,
            available.as_ref(),
        );
        if cause != crate::backup::StopCause::Environmental
            || slot_state.source == Some(crate::state::SeatSource::Backup)
        {
            continue;
        }
        let backup_raw = match crate::backup::backup_for_role(SlotRole::Reviewer, idx, cfg) {
            Some(b) => b,
            None => continue,
        };
        if !crate::backup::is_provider_eligible(&backup_raw, &store, available.as_ref()) {
            continue;
        }
        let cur_key = crate::provider_ref::ProviderRef::parse(&job.provider)
            .map(|r| r.storage_key())
            .unwrap_or_else(|_| job.provider.clone());
        let backup_key = crate::provider_ref::ProviderRef::parse(&backup_raw)
            .map(|r| r.storage_key())
            .unwrap_or_else(|_| backup_raw.clone());
        if cur_key == backup_key {
            continue;
        }
        let pin = match crate::runspec::Pin::parse(&backup_raw) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let new_id = format!(
            "review-{}-{}",
            idx,
            crate::util::sanitize_slot(&pin.provider)
        );
        let old_id = job.slot_id.clone();
        let mut new_slot_id = new_id.clone();
        let new_artifact = format!("review-{new_id}.md");
        if let Some(s) = state.slot_mut(&old_id) {
            s.id = new_id.clone();
            new_slot_id = s.id.clone();
            s.provider = pin.provider.clone();
            s.model = pin.model.clone();
            s.source = Some(crate::state::SeatSource::Backup);
            s.status = SlotStatus::Pending;
            s.error = None;
            s.quota_hit = false;
        } else if let Some(s) = state.slot_mut(&job.slot_id) {
            s.provider = pin.provider.clone();
            s.model = pin.model.clone();
            s.source = Some(crate::state::SeatSource::Backup);
            s.status = SlotStatus::Pending;
            s.error = None;
            s.quota_hit = false;
        }
        state.save(paths)?;
        let retry_job = SlotJob {
            slot_id: new_slot_id,
            provider: pin.display(),
            role: job.role,
            template: job.template.clone(),
            extra_vars: job.extra_vars.clone(),
            expected_artifact: Some(new_artifact),
            model: pin.model.clone(),
        };
        let _ = executor::run_slot(state, paths, cfg, &retry_job);
    }

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
    let any_quota_failed = failed.iter().any(|s| s.quota_hit);
    let any_plain_failed = failed.iter().any(|s| !s.quota_hit);
    // Checked ahead of the approve/request_changes tally: `salvage_expected_artifact`
    // (executor.rs) writes a synthetic `request_changes` verdict for every interrupted
    // reviewer, so `changes == 0` never holds once any reviewer fails and the tally
    // alone can't tell a real review panel from one where every slot died on a rate
    // limit — a `changes == 0` guard on the `Failed` branch below would be dead code
    // for every all-failed panel, quota or not. Branch on `all_failed` directly instead:
    // every slot quota-hit parks at `Phase::Quota`; every slot failed for any other (or
    // mixed) reason still fails the run at `Phase::Failed` rather than falling through
    // to `Done` on the strength of two fabricated `request_changes` votes.
    //
    // A panel that is not all-failed can still have a quota-hit slot among successful
    // siblings (e.g. one of two reviewers rate-limited, the other completed): that
    // slot's absence from the vote is a resource block, not a vote, so it must still
    // park the run rather than silently tallying a partial review as `Done`. Only
    // fires when *every* failure in the panel is quota-detected — a mix of a quota hit
    // and a genuine defect still falls through to `Done` on its live tally, the same
    // pre-existing gap named above for an all-genuine-failure panel with a survivor.
    if all_failed && !any_plain_failed {
        state.set_phase(Phase::Quota);
        state.error = Some("all review slots failed: rate limit".into());
    } else if all_failed {
        state.set_phase(Phase::Failed);
        state.error = Some("all review slots failed".into());
    } else if any_quota_failed && !any_plain_failed {
        state.set_phase(Phase::Quota);
        state.error = Some("a review slot failed: rate limit".into());
    } else {
        state.set_phase(Phase::Done);
    }
    state.save(paths)?;
    if cfg.auto_cleanup && state.phase == Phase::Done {
        let _ = worktree::cleanup_run(state, false);
    }
    Ok(())
}
