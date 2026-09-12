use super::CommonOpts;
use crate::bus::{self, MessageBudget, MsgKind, MsgRefs};
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, PoolOrigin, RunState, SeatSource, SlotRole};
use crate::util::{self, sanitize_slot};
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

pub fn run(
    task: String,
    brief: Option<PathBuf>,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Plan,
        paths.project_root.clone(),
    );
    state.task = Some(task.clone());
    // Stored project-root-relative, like every other path on `state` — an absolute
    // path would strand `spar brief` the moment the project moves or is restored
    // somewhere else.
    state.brief = brief.map(|p| {
        p.strip_prefix(&paths.project_root)
            .map(PathBuf::from)
            .unwrap_or(p)
    });
    state.backend = opts.backend;
    worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
    cfg.save_snapshot(paths, &state.id)?;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    state.big = opts.big;
    // Frozen here because a plan run is where most units of work are created, and the
    // ceiling has to be the one the project set when the work started (O27/O52).
    state.max_rounds = cfg.rounds.max;
    let mut roles: Vec<&str> = vec![SlotRole::Planner.as_config_key()];
    if cfg.critic.enabled {
        roles.push(SlotRole::PlanCritic.as_config_key());
    }
    if cfg.spec.enabled {
        roles.push(SlotRole::TestAuthor.as_config_key());
    }
    let n_slots = roles.len();
    let requested = opts.resolve_pool(n_slots, &roles, paths, cfg, &state.id)?;
    let pool_origin = crate::workflow::roles_resolve::pool_origin_for(&opts);
    state.pool_origin = pool_origin;
    // The raw, un-narrowed pool: `state.providers` below is cycled/truncated to the plan
    // phase's own slot count, which is narrower than the implement panel whenever
    // `--fleet small` or `--without critic`/`spec` drops a plan-phase seat. The plan gate
    // projection and a later bare `implement --run` continuation both need the width the
    // operator actually asked for, not the plan phase's.
    if matches!(
        pool_origin,
        crate::state::PoolOrigin::CliProviders | crate::state::PoolOrigin::Selected
    ) {
        state.pool_intent = requested.clone();
    }
    state.providers = providers::pick_providers(&requested, n_slots, Some(&requested), dry);
    // `state.providers` is positional, but CLI `--role` pins outrank it (O79). A
    // preflight quota gate that checks the pool before seat resolution sees the
    // *pool* entry as paused and parks, even though the seat's own provider (from
    // `cfg.roles`) would be replaced by its backup. Resolve seats first, then gate
    // only those that remain ineligible with no eligible backup — so a declared backup
    // actually covers its role.
    if !dry {
        // Check after seat resolution below; pool alone is not authoritative when backups
        // can cover individual roles. Save the pool now and defer the quota gate.
    }

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
    paths.ensure_run_dirs(&state.id)?;
    let _ = bus::ensure_bus(paths);
    let _ = bus::join(paths, Some(&state.id), "orchestrator", None, None);
    state.save(paths)?;

    let art = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten();
    let mut jobs = Vec::new();
    for (idx, (id, role, template, prov, source)) in
        plan_slot_specs(&state, cfg).into_iter().enumerate()
    {
        let model = art.as_ref().and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.role.as_deref() == Some(role.as_config_key()) || c.slot == idx)
                .and_then(|c| c.model.clone())
        });
        let mut slot = executor::init_slot_model(&id, &prov, role, model.clone());
        slot.source = Some(source);
        state.slots.push(slot);
        let expected_artifact = "plan.md".to_string();
        jobs.push(SlotJob {
            slot_id: id,
            provider: prov,
            role,
            template: template.into(),
            extra_vars: HashMap::from([(
                "amendment_section".to_string(),
                plan_amendment_section(&state),
            )]),
            expected_artifact: Some(expected_artifact),
            model,
        });
    }

    // Backup-aware quota gate: after seat resolution, a paused primary that has an
    // eligible backup is already covered (plan_slot_specs swapped it). Only those that
    // remain ineligible with no eligible backup should park.
    if !dry {
        let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        let has_backup = cfg.backups.planner.is_some()
            || cfg.backups.plan_critic.is_some()
            || cfg.backups.test_author.is_some();
        let available: Option<std::collections::HashSet<String>> = if !has_backup {
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
        let mut still_paused: Vec<String> = Vec::new();
        for job in &jobs {
            if !crate::backup::is_provider_eligible(&job.provider, &store, available.as_ref()) {
                let key = crate::quota::normalize_key(&job.provider);
                if !still_paused.contains(&key) {
                    still_paused.push(key);
                }
            }
        }
        if !still_paused.is_empty() {
            let msg = format!(
                "provider(s) paused or on cooldown: {}. resume with `spar provider resume <name>` or reassign the role",
                still_paused.join(", ")
            );
            state.error = Some(msg.clone());
            state.set_phase(Phase::Quota);
            paths.ensure_run_dirs(&state.id)?;
            state.save(paths)?;
            if opts.json {
                executor::emit_run_json(&state)?;
            } else {
                eprintln!("error: {msg}");
            }
            return Ok(ExitCode::Quota);
        }
    }

    // Printed after the slots resolve, not off `state.providers`: the pool lists what the
    // run *may* draw from, which is not what any role got.
    if !opts.json {
        eprintln!(
            "roles: {}{}",
            executor::role_assignments(&state).join(", "),
            if dry {
                " (dry-run: no git worktrees; agents stubbed)"
            } else {
                ""
            }
        );
    }

    if opts.detach {
        return detach_self(&state, paths, opts.json);
    }

    execute_plan(&mut state, paths, cfg, &jobs)?;
    if opts.json {
        executor::emit_run_json(&state)?;
    } else {
        executor::print_run_human(&state);
        println!("plan: {}", paths.artifact(&state.id, "plan.md").display());
        let contract = paths.artifact(&state.id, "test-contract.md");
        if contract.is_file() {
            println!("tests: {}", contract.display());
        }
    }
    Ok(state.exit_code())
}

/// The planner + critic slot specs `(id, role, template, provider, source)`, drawn from
/// the resolved pool via `resolve_seat` so both the first-pass and re-plan paths key the
/// slots identically. `plan_critic` is omitted entirely when `[critic].enabled` is false
/// (`--without critic`) — not resolved-then-discarded, so it never touches the bus.
fn plan_slot_specs(
    state: &RunState,
    cfg: &Config,
) -> Vec<(String, SlotRole, &'static str, String, SeatSource)> {
    let mut specs = vec![(SlotRole::Planner, "planner", "planner")];
    if cfg.critic.enabled {
        specs.push((SlotRole::PlanCritic, "critic", "plan_critic"));
    }
    let store = crate::quota::QuotaStore::load(&crate::paths::SparPaths::new(&state.project_root))
        .unwrap_or_default();
    let has_backup = cfg.backups.planner.is_some()
        || cfg.backups.plan_critic.is_some()
        || cfg.backups.test_author.is_some();
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
    let mut out = Vec::with_capacity(specs.len());
    for (idx, (role, prefix, template)) in specs.into_iter().enumerate() {
        let Some((prov, source)) = crate::backup::resolve_with_backup(
            role,
            idx,
            idx,
            &state.providers,
            state.pool_origin,
            cfg,
            &store,
            available.as_ref(),
        )
        .or_else(|| {
            crate::workflow::roles_resolve::resolve_seat(
                role,
                idx,
                idx,
                &state.providers,
                state.pool_origin,
                cfg,
            )
        }) else {
            continue;
        };
        let id = format!("{prefix}-{}", sanitize_slot(&prov));
        out.push((id, role, template, prov, source));
    }
    out
}

pub fn execute_plan(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    jobs: &[SlotJob],
) -> Result<()> {
    // A queued run's admission and an operator's `spar stop` race: the daemon can decide
    // to admit before `stop_one` removes the spool file, and the admitted child can still
    // reach here after the stop already wrote the `stopped` marker. Refuse to dispatch
    // rather than requiring dequeue and cancellation to be mutually exclusive — the same
    // marker `implement::should_stop` already gates every other workflow's dispatch loop
    // on, and only an explicit resume clears it.
    if crate::workflow::implement::should_stop(paths, &state.id) {
        state.set_phase(Phase::Stopped);
        state.save(paths)?;
        return Ok(());
    }
    let slot_ids: Vec<String> = jobs.iter().map(|j| j.slot_id.clone()).collect();
    worktree::prepare_isolation(state, paths, &slot_ids)?;
    state.set_phase(Phase::SpawnSlots);
    state.save(paths)?;

    state.set_phase(Phase::Dispatch);
    state.save(paths)?;

    for job in jobs {
        if let Err(e) = executor::run_slot(state, paths, cfg, job) {
            let slot_state = state.slots.iter().find(|s| s.id == job.slot_id).cloned();
            let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
            let has_backup_global = cfg.backups.planner.is_some()
                || cfg.backups.plan_critic.is_some()
                || cfg.backups.test_author.is_some();
            let available: Option<std::collections::HashSet<String>> = if state.dry_run
                || !has_backup_global
            {
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
            let mut handled_via_backup = false;
            if let Some(slot_state) = slot_state {
                let cause = crate::backup::dispatch_stop_cause(
                    &slot_state,
                    &job.provider,
                    &store,
                    available.as_ref(),
                );
                if cause == crate::backup::StopCause::Environmental
                    && slot_state.source != Some(SeatSource::Backup)
                {
                    if let Some(raw) = crate::backup::backup_for_role(job.role, 0, cfg) {
                        if crate::backup::is_provider_eligible(&raw, &store, available.as_ref()) {
                            let cur_key = crate::provider_ref::ProviderRef::parse(&job.provider)
                                .map(|r| r.storage_key())
                                .unwrap_or_else(|_| job.provider.clone());
                            let backup_key = crate::provider_ref::ProviderRef::parse(&raw)
                                .map(|r| r.storage_key())
                                .unwrap_or_else(|_| raw.clone());
                            if cur_key != backup_key {
                                if let Ok(pin) = crate::runspec::Pin::parse(&raw) {
                                    let prefix = match job.role {
                                        crate::state::SlotRole::Planner => "planner",
                                        crate::state::SlotRole::PlanCritic => "critic",
                                        _ => "planner",
                                    };
                                    let new_id = format!(
                                        "{prefix}-{}",
                                        crate::util::sanitize_slot(&pin.provider)
                                    );
                                    let old_id = job.slot_id.clone();
                                    let mut new_slot_id = new_id.clone();
                                    if let Some(s) = state.slot_mut(&old_id) {
                                        s.id = new_id.clone();
                                        new_slot_id = s.id.clone();
                                        s.provider = pin.provider.clone();
                                        s.model = pin.model.clone();
                                        s.source = Some(SeatSource::Backup);
                                        s.status = crate::state::SlotStatus::Pending;
                                        s.error = None;
                                        s.quota_hit = false;
                                    } else if let Some(s) = state.slot_mut(&job.slot_id) {
                                        s.provider = pin.provider.clone();
                                        s.model = pin.model.clone();
                                        s.source = Some(SeatSource::Backup);
                                        s.status = crate::state::SlotStatus::Pending;
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
                                        expected_artifact: job.expected_artifact.clone(),
                                        model: pin.model.clone(),
                                    };
                                    match executor::run_slot(state, paths, cfg, &retry_job) {
                                        Ok(()) => {
                                            handled_via_backup = true;
                                        }
                                        Err(e2) => {
                                            if executor::slot_quota_hit(state, &retry_job.slot_id) {
                                                state.error = Some(e2.to_string());
                                                state.set_phase(Phase::Quota);
                                                state.save(paths)?;
                                                return Ok(());
                                            }
                                            if retry_job.role == SlotRole::Planner {
                                                state.error = Some(e2.to_string());
                                                state.set_phase(Phase::Failed);
                                                state.save(paths)?;
                                                return Err(e2);
                                            }
                                            if let Some(s) = state.slot_mut(&retry_job.slot_id) {
                                                s.status = crate::state::SlotStatus::Failed;
                                                s.error = Some(e2.to_string());
                                            }
                                            handled_via_backup = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if handled_via_backup {
                continue;
            }
            if executor::slot_quota_hit(state, &job.slot_id) {
                state.error = Some(e.to_string());
                state.set_phase(Phase::Quota);
                state.save(paths)?;
                return Ok(());
            }
            if job.role == SlotRole::Planner {
                state.error = Some(e.to_string());
                state.set_phase(Phase::Failed);
                state.save(paths)?;
                return Err(e);
            }
            if let Some(s) = state.slot_mut(&job.slot_id) {
                s.status = crate::state::SlotStatus::Failed;
                s.error = Some(e.to_string());
            }
        }
    }

    let plan_path = paths.artifact(&state.id, "plan.md");
    if !plan_path.is_file() {
        let mut combined = String::from("# Plan\n\n");
        if let Ok(rd) = std::fs::read_dir(paths.artifacts_dir(&state.id)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("plan-") && name.ends_with(".md") {
                    if let Ok(t) = std::fs::read_to_string(e.path()) {
                        combined.push_str(&t);
                        combined.push_str("\n\n");
                    }
                }
            }
        }
        if combined.trim() == "# Plan" {
            combined.push_str(&format!(
                "## Goal\n{}\n",
                state.task.as_deref().unwrap_or("")
            ));
        }
        std::fs::write(&plan_path, combined)?;
    }

    if state.big {
        if let Ok(body) = std::fs::read_to_string(&plan_path) {
            let _ = crate::tasks::seed_from_plan(paths, &state.id, &body);
        }
    }

    if cfg.spec.enabled {
        if let Err(e) = run_test_author(state, paths, cfg) {
            if state.phase != Phase::Failed {
                state.set_phase(Phase::Failed);
                state.error = Some(e.to_string());
                let _ = state.save(paths);
            }
            return Err(e);
        }
        // `run_test_author` parks quota-detected failures itself (returning `Ok`, not
        // `Err`), so this must be checked separately from the `Err` arm above or the
        // `auto_plan()` branch below would immediately clobber `Phase::Quota`.
        if state.phase == Phase::Quota {
            return Ok(());
        }
    }

    // The implement panel this run will dispatch, projected from the frozen config so
    // the plan gate shows it before it exists (feature 011, item C): the human deciding
    // whether to approve is exactly the one who needs to see what it will cost.
    // `pool_intent` is the un-narrowed operator pool when there is one — the plan
    // phase's own `state.providers` is truncated to the plan's slot count and would drop
    // reviewer positions the implement panel still needs.
    let projection_pool = if state.pool_intent.is_empty() {
        &state.providers
    } else {
        &state.pool_intent
    };
    state.projected_fleet = crate::workflow::roles_resolve::project_implement_fleet(
        cfg,
        projection_pool,
        state.pool_origin,
        state.dry_run,
        Some(paths),
        Some(&state.id),
    );
    if let Some(seat) = crate::workflow::implement::project_tester_seat(
        cfg,
        state.dry_run,
        projection_pool,
        state.pool_origin,
        Some(paths),
        Some(&state.id),
    ) {
        state.projected_fleet.push(seat);
    }

    if cfg.auto_plan() {
        state.gates.plan_approved = true;
        state.set_phase(Phase::PlanApproved);
        let _ = bus::broadcast(
            paths,
            Some(&state.id),
            "orchestrator",
            "plan auto-approved (autonomy)",
            state.message_budget,
        );
    } else {
        state.set_phase(Phase::AwaitingPlanApproval);
    }
    state.save(paths)?;
    Ok(())
}

fn run_test_author(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    let planner_slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Planner)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| "planner".into());
    // `None` when `--without critic` dropped the seat entirely — the spec protocol must
    // not invent a placeholder id to coordinate with (Bug AC-19: nothing on the bus may
    // be addressed to a critic that does not exist).
    let critic_slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::PlanCritic)
        .map(|s| s.id.clone());

    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| matches!(s.role, SlotRole::Planner | SlotRole::PlanCritic))
        .map(|s| s.provider.clone())
        .collect();
    let test_author_idx = 1 + usize::from(cfg.critic.enabled);
    let (mut provider, mut source) = resolve_spec_provider(
        cfg,
        state.dry_run,
        &state.providers,
        state.pool_origin,
        &used,
    )?;
    let mut model = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten()
        .and_then(|a| {
            a.choices
                .iter()
                .find(|c| {
                    c.role.as_deref() == Some(SlotRole::TestAuthor.as_config_key())
                        || c.slot == test_author_idx
                })
                .and_then(|c| c.model.clone())
        });
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let has_backup = cfg.backups.test_author.is_some();
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
    if !crate::backup::is_provider_eligible(&provider, &store, available.as_ref()) {
        if let Some(backup_raw) = crate::backup::backup_for_role(SlotRole::TestAuthor, 0, cfg) {
            if crate::backup::is_provider_eligible(&backup_raw, &store, available.as_ref()) {
                if let Ok(pin) = crate::runspec::Pin::parse(&backup_raw) {
                    provider = pin.display();
                    source = crate::state::SeatSource::Backup;
                    model = pin.model;
                }
            }
        }
    }
    let safe = sanitize_slot(&provider);
    let id = format!("test-author-{safe}");

    if state.slots.iter().all(|s| s.id != id) {
        let mut slot =
            executor::init_slot_model(&id, &provider, SlotRole::TestAuthor, model.clone());
        slot.source = Some(source);
        state.slots.push(slot);
    }
    worktree::prepare_isolation(state, paths, std::slice::from_ref(&id))?;
    // After isolation so status/TUI show Spec for the author wall-clock, not PrepareIsolation.
    state.set_phase(Phase::Spec);
    state.save(paths)?;

    let _ = bus::join(paths, Some(&state.id), &id, Some(&provider), None);
    seed_spec_bus(state, paths, &id, &planner_slot, critic_slot.as_deref())?;

    let mut extra = HashMap::new();
    extra.insert("planner_slot".into(), planner_slot);
    if let Some(c) = &critic_slot {
        extra.insert("critic_slot".into(), c.clone());
    }
    let job = SlotJob {
        slot_id: id.clone(),
        provider,
        role: SlotRole::TestAuthor,
        template: "test_author".into(),
        extra_vars: extra,
        expected_artifact: Some("test-contract.md".into()),
        model,
    };

    if let Err(e) = executor::run_slot(state, paths, cfg, &job) {
        let slot_state = state.slots.iter().find(|s| s.id == id).cloned();
        let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        let has_backup = cfg.backups.test_author.is_some();
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
        let mut handled = false;
        if let Some(slot_state) = slot_state {
            let cause = crate::backup::dispatch_stop_cause(
                &slot_state,
                &job.provider,
                &store,
                available.as_ref(),
            );
            if cause == crate::backup::StopCause::Environmental
                && slot_state.source != Some(crate::state::SeatSource::Backup)
            {
                if let Some(raw) = crate::backup::backup_for_role(SlotRole::TestAuthor, 0, cfg) {
                    if crate::backup::is_provider_eligible(&raw, &store, available.as_ref()) {
                        let cur_key = crate::provider_ref::ProviderRef::parse(&job.provider)
                            .map(|r| r.storage_key())
                            .unwrap_or_else(|_| job.provider.clone());
                        let backup_key = crate::provider_ref::ProviderRef::parse(&raw)
                            .map(|r| r.storage_key())
                            .unwrap_or_else(|_| raw.clone());
                        if cur_key != backup_key {
                            if let Ok(pin) = crate::runspec::Pin::parse(&raw) {
                                if let Some(s) = state.slot_mut(&id) {
                                    s.provider = pin.provider.clone();
                                    s.model = pin.model.clone();
                                    s.source = Some(crate::state::SeatSource::Backup);
                                    s.status = crate::state::SlotStatus::Pending;
                                    s.error = None;
                                    s.quota_hit = false;
                                }
                                state.save(paths)?;
                                let retry_job = SlotJob {
                                    slot_id: id.clone(),
                                    provider: pin.display(),
                                    role: SlotRole::TestAuthor,
                                    template: "test_author".into(),
                                    extra_vars: job.extra_vars.clone(),
                                    expected_artifact: Some("test-contract.md".into()),
                                    model: pin.model.clone(),
                                };
                                match executor::run_slot(state, paths, cfg, &retry_job) {
                                    Ok(()) => {
                                        handled = true;
                                    }
                                    Err(e2) => {
                                        if executor::slot_quota_hit(state, &id) {
                                            state.error = Some(format!("test-author failed: {e2}"));
                                            state.set_phase(Phase::Quota);
                                            state.save(paths)?;
                                            return Ok(());
                                        }
                                        state.error = Some(format!("test-author failed: {e2}"));
                                        state.set_phase(Phase::Failed);
                                        state.save(paths)?;
                                        return Err(e2);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if handled {
            // retry succeeded, continue without error
        } else {
            state.error = Some(format!("test-author failed: {e}"));
            if executor::slot_quota_hit(state, &id) {
                state.set_phase(Phase::Quota);
                state.save(paths)?;
                return Ok(());
            }
            state.set_phase(Phase::Failed);
            state.save(paths)?;
            return Err(e);
        }
    }

    let contract = paths.artifact(&state.id, "test-contract.md");
    if !contract.is_file()
        || std::fs::metadata(&contract)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
    {
        let msg = "test-author finished without test-contract.md";
        state.set_phase(Phase::Failed);
        state.error = Some(msg.into());
        state.save(paths)?;
        anyhow::bail!("{msg}");
    }

    let _ = bus::broadcast(
        paths,
        Some(&state.id),
        "orchestrator",
        "test-author finished; acceptance contract ready for plan approval",
        state.message_budget,
    );
    Ok(())
}

fn seed_spec_bus(
    state: &RunState,
    paths: &SparPaths,
    author_id: &str,
    planner_slot: &str,
    critic_slot: Option<&str>,
) -> Result<()> {
    let budget = state.message_budget;
    let critic_clause = critic_slot
        .map(|c| format!(" and critic `{c}`"))
        .unwrap_or_default();
    let body = format!(
        "Spec phase: `{author_id}` will freeze acceptance tests from plan.md. \
         Planner `{planner_slot}`{critic_clause}: reply on bus if still available; \
         otherwise the author uses plan + critique artifacts."
    );
    let _ = bus::broadcast(paths, Some(&state.id), "orchestrator", &body, budget);

    let coordinate_with = critic_slot
        .map(|c| format!(" and `{c}`"))
        .unwrap_or_default();
    let mut recipients = vec![
        (
            author_id,
            format!(
                "You are the test author. Coordinate with `{planner_slot}`{coordinate_with} via bus, then write tests + test-contract.md."
            ),
        ),
        (
            planner_slot,
            format!("Test author `{author_id}` is writing acceptance tests. Answer bus questions if you can."),
        ),
    ];
    if let Some(c) = critic_slot {
        recipients.push((
            c,
            format!("Test author `{author_id}` is freezing the test bar. Challenge weak scenarios on the bus if you can."),
        ));
    }
    for (to, note) in recipients {
        let _ = bus::send(
            paths,
            bus::BusMessage {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                ts: chrono::Utc::now(),
                from: "orchestrator".into(),
                to: to.into(),
                kind: MsgKind::Hello,
                body: note,
                run: Some(state.id.clone()),
                subject: Some("spec".into()),
                refs: MsgRefs {
                    artifact: Some("plan.md".into()),
                    ..Default::default()
                },
                requires_ack: false,
                meta: HashMap::new(),
            },
            budget,
        );
    }
    Ok(())
}

/// Spec provider: config override, then fleet provider not used by planner/critic, then cycle.
/// Spec provider precedence, matching `roles_resolve::resolve_seat`: CLI `--role
/// test_author=…`, then an explicit pool (`--providers`/`--select`), then
/// `[roles].test_author`, then the plan's own resolved pool as a last resort. Before this
/// fix `[roles].test_author` was checked unconditionally first, so it beat an explicit
/// pool it should have lost to (Bug B for this one staged seat).
fn resolve_spec_provider(
    cfg: &Config,
    dry: bool,
    fleet: &[String],
    pool_origin: PoolOrigin,
    used: &[String],
) -> Result<(String, SeatSource)> {
    let cli_pinned = cfg
        .cli_role_keys
        .contains(SlotRole::TestAuthor.as_config_key());
    if cli_pinned {
        if let Some(p) = &cfg.roles.test_author {
            crate::provider_ref::ProviderRef::parse(p)
                .map_err(|e| anyhow::anyhow!("invalid [roles].test_author {p:?}: {e}"))?;
            if dry || providers::is_provider_usable(p, false) {
                return Ok((p.clone(), SeatSource::CliRole));
            }
        }
    }
    let pool_explicit = matches!(pool_origin, PoolOrigin::CliProviders | PoolOrigin::Selected);
    let pool_source = pool_origin.as_seat_source();
    if pool_explicit {
        if let Some(p) = fleet
            .iter()
            .find(|p| !used.contains(p) && (dry || providers::is_provider_usable(p, false)))
            .cloned()
        {
            return Ok((p, pool_source));
        }
    }
    if !cli_pinned {
        if let Some(p) = &cfg.roles.test_author {
            crate::provider_ref::ProviderRef::parse(p)
                .map_err(|e| anyhow::anyhow!("invalid [roles].test_author {p:?}: {e}"))?;
            if dry || providers::is_provider_usable(p, false) {
                return Ok((p.clone(), SeatSource::RolesFile));
            }
            // Fall through to fleet if override is unusable (missing CLI / paused).
        }
    }
    if dry {
        if let Some(p) = fleet.iter().find(|p| !used.contains(p)) {
            return Ok((p.clone(), pool_source));
        }
        if let Some(p) = fleet.get(2) {
            return Ok((p.clone(), pool_source));
        }
        if let Some(p) = fleet.last() {
            return Ok((p.clone(), pool_source));
        }
        return Ok(("cli:claude".into(), SeatSource::ProvidersOrder));
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| !used.contains(p) && providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok((p, pool_source));
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok((p, pool_source));
    }
    anyhow::bail!(
        "spec.enabled but no usable test-author provider (set [roles].test_author or pass more --providers)"
    )
}

pub fn approve(paths: &SparPaths, run_id: &str, json: bool) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase != Phase::AwaitingPlanApproval && state.phase != Phase::PlanRejected {
        anyhow::bail!(
            "run {run_id} is not awaiting plan approval (phase={:?})",
            state.phase
        );
    }
    state.gates.plan_approved = true;
    state.gates.reject_reason = None;
    state.set_phase(Phase::PlanApproved);
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("approved plan (+ acceptance contract if present) for run {run_id}");
        println!("next: spar implement --run {run_id}  (same run id)");
    }
    let _ = bus::broadcast(
        paths,
        Some(run_id),
        "human",
        "plan approved",
        MessageBudget::Normal,
    );
    Ok(ExitCode::Success)
}

pub fn reject(
    paths: &SparPaths,
    run_id: &str,
    reason: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase != Phase::AwaitingPlanApproval {
        anyhow::bail!(
            "run {run_id} is not awaiting plan approval (phase={:?})",
            state.phase
        );
    }
    state.gates.plan_approved = false;
    state.gates.reject_reason = reason.clone();
    state.set_phase(Phase::PlanRejected);
    state.error = reason;
    state.save(paths)?;
    // Nothing can resume a rejected plan (`implement --run` requires an approved one),
    // so its worktrees are garbage from here. Artifacts under `.spar/runs/<id>` stay:
    // the plan and the critique are why you rejected it.
    let cleaned = crate::worktree::cleanup_run(&state, false)?;
    // Keep the record of anything the veto spared. Clearing unconditionally orphans it:
    // no record means `cleanup <id> --force` iterates an empty list and silently does
    // nothing, so a test-author's uncommitted acceptance tests would be unreachable by
    // any spar command.
    let kept: Vec<&std::path::Path> = cleaned
        .iter()
        .filter(|c| c.skipped.is_some())
        .map(|c| c.path.as_path())
        .collect();
    state.worktrees.retain(|w| kept.contains(&w.path.as_path()));
    state.save(paths)?;
    for c in cleaned.iter().filter(|c| c.skipped.is_some()) {
        eprintln!(
            "kept {}: {}",
            c.path.display(),
            c.skipped.as_deref().unwrap_or("")
        );
    }
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("rejected plan for run {run_id}");
    }
    Ok(ExitCode::Failure)
}

fn detach_self(state: &RunState, paths: &SparPaths, json: bool) -> Result<ExitCode> {
    if let Some(owner) = crate::runlock::RunLock::owner(paths, &state.id) {
        if owner.alive() {
            return Err(crate::runlock::OrchestratorBusy {
                run_id: state.id.clone(),
                owner_pid: owner.pid,
            }
            .into());
        }
    }
    if let Some(msg) = crate::daemon::maybe_enqueue(paths, state)? {
        if json {
            executor::emit_run_json(state)?;
        } else {
            executor::print_run_human(state);
            println!("{msg}");
        }
        return Ok(ExitCode::Success);
    }
    let detached = match crate::process::spawn_detached_orchestrator(paths, &state.id) {
        Ok(d) => d,
        Err(e) => {
            crate::daemon::release_reservation(paths, &state.id);
            return Err(e);
        }
    };
    let outcome = match crate::process::await_detached_start(paths, &state.id, detached) {
        Ok(o) => o,
        Err(e) => {
            crate::daemon::release_reservation(paths, &state.id);
            return Err(e);
        }
    };
    match outcome {
        crate::process::DetachOutcome::Confirmed { pid } => {
            if json {
                executor::emit_run_json(state)?;
            } else {
                executor::print_run_human(state);
                println!(
                    "detached (pid {pid}, session of its own); poll with: spar wait {}",
                    state.id
                );
            }
            Ok(ExitCode::Success)
        }
        crate::process::DetachOutcome::Completed => {
            // The run settled inside the handshake window without ever holding a
            // `Running` slot for `effective_supply` to see, so its admission
            // reservation (if `maybe_enqueue` granted one) would otherwise squat on
            // capacity for the full TTL.
            crate::daemon::release_reservation(paths, &state.id);
            let state = RunState::load_for_display(paths, &state.id)?;
            if json {
                executor::emit_run_json(&state)?;
            } else {
                executor::print_run_human(&state);
            }
            Ok(state.exit_code())
        }
    }
}

/// The directive for this plan round, rendered for the planner and critic prompts.
/// On a replan it also carries why the last plan was rejected — that reason is the
/// whole point of planning again.
fn plan_amendment_section(state: &RunState) -> String {
    let mut out = String::new();
    if let Some(a) = state.amendment.as_deref() {
        out.push_str(&format!(
            "## Directive for this round (round {})\nThe operator asked for this plan to be redone. This directive takes precedence over the original task where they conflict: the task below is context, this is the work.\n\n{a}\n",
            state.round
        ));
    }
    if let Some(r) = state.gates.reject_reason.as_deref() {
        out.push_str(&format!("\n## Why the previous plan was rejected\n{r}\n"));
    }
    out
}

/// Replan an existing run: a new round on the same id (O45), not a second run.
/// The run keeps its brief, base, config and usage ledger — it is the same unit of
/// work, being planned again.
pub fn replan(
    paths: &SparPaths,
    cfg: &Config,
    run_id: &str,
    directive: String,
    json: bool,
) -> Result<ExitCode> {
    // Take the lock BEFORE touching anything. Acquiring it after the save meant a run
    // someone else was driving got its approval cleared and its phase reset to `init`,
    // and only then did the command fail — a torn state.json left behind by a command
    // that reported an error.
    let lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    let mut state = RunState::load(paths, run_id)?;
    if state.workflow == crate::cli::WorkflowKind::Review {
        anyhow::bail!("run {run_id} is a review run; there is no plan to redo");
    }
    if state.slots.iter().all(|s| s.role != SlotRole::Planner) {
        anyhow::bail!(
            "run {run_id} has no planner slot to re-run — start a plan with `spar plan -t \"…\"`"
        );
    }
    // A run nobody can plan again: say so before mutating anything.
    if !matches!(
        state.phase,
        Phase::AwaitingPlanApproval
            | Phase::PlanRejected
            | Phase::PlanApproved
            | Phase::PlanReady
            | Phase::Done
            | Phase::Stopped
            | Phase::Failed
            | Phase::Stuck
            | Phase::Quota
    ) {
        anyhow::bail!(
            "run {run_id} is mid-flight (phase={:?}); stop it before replanning",
            state.phase
        );
    }
    // Resuming a stopped run: drop the marker so execute_plan dispatches instead of
    // immediately re-parking at Stopped (see the guard at the top of execute_plan).
    if state.phase == Phase::Stopped {
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
    }
    let round = state.begin_round();
    state.amendment = Some(directive);
    // The gate reopens: whatever was approved or rejected was about the old plan.
    state.gates.plan_approved = false;
    // Keep the previous round's plan and contract as a record, and — more importantly
    // — get them out of the way: `execute_plan` only notices a planner that wrote
    // nothing by `plan.md` being absent, so leaving them in place lets a no-op round
    // present the OLD plan (and the old frozen contract) at the approval gate.
    archive_round_artifacts(paths, &state, round - 1);
    state.contract_fingerprint = None;
    state.set_phase(Phase::Init);
    state.save(paths)?;
    if !json {
        eprintln!("replanning run {run_id} (round {round})");
    }
    let code = continue_locked(paths, cfg, run_id)?;
    drop(lock);
    if json {
        let state = RunState::load(paths, run_id)?;
        executor::emit_run_json(&state)?;
    }
    Ok(code)
}

/// Move a finished round's plan and contract aside so the next round cannot be
/// mistaken for it. Best-effort: a missing artifact is exactly the state we want.
fn archive_round_artifacts(paths: &SparPaths, state: &RunState, round: u32) {
    for name in ["plan.md", "test-contract.md"] {
        let from = paths.artifact(&state.id, name);
        if !from.is_file() {
            continue;
        }
        let stem = name.trim_end_matches(".md");
        let to = paths.artifact(&state.id, &format!("{stem}-round{round}.md"));
        let _ = std::fs::rename(&from, &to);
    }
}

pub fn continue_run(paths: &SparPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    continue_locked(paths, cfg, run_id)
}

/// `continue_run`'s body, for callers that already hold the run lock.
fn continue_locked(paths: &SparPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    let amendment_section = plan_amendment_section(&state);
    let mut jobs = Vec::new();
    for slot in &state.slots {
        let template = match slot.role {
            SlotRole::Planner => "planner",
            SlotRole::PlanCritic => "plan_critic",
            // Test author is spawned after plan draft inside execute_plan.
            SlotRole::TestAuthor => continue,
            _ => continue,
        };
        let expected_artifact = "plan.md".to_string();
        jobs.push(SlotJob {
            slot_id: slot.id.clone(),
            provider: slot.provider.clone(),
            role: slot.role,
            template: template.into(),
            extra_vars: HashMap::from([(
                "amendment_section".to_string(),
                amendment_section.clone(),
            )]),
            expected_artifact: Some(expected_artifact),
            model: None,
        });
    }
    if jobs.is_empty() {
        for (id, role, template, prov, source) in plan_slot_specs(&state, cfg) {
            if state.slots.iter().all(|s| s.id != id) {
                let mut slot = executor::init_slot(&id, &prov, role);
                slot.source = Some(source);
                state.slots.push(slot);
            }
            let expected_artifact = "plan.md".to_string();
            jobs.push(SlotJob {
                slot_id: id,
                provider: prov,
                role,
                template: template.into(),
                extra_vars: HashMap::from([(
                    "amendment_section".to_string(),
                    amendment_section.clone(),
                )]),
                expected_artifact: Some(expected_artifact),
                model: None,
            });
        }
        state.save(paths)?;
    }
    execute_plan(&mut state, paths, cfg, &jobs)?;
    Ok(state.exit_code())
}
