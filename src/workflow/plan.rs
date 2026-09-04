use super::CommonOpts;
use crate::bus::{self, MessageBudget, MsgKind, MsgRefs};
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::util::{self, sanitize_slot};
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

pub fn run(task: String, opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
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
    let n_slots = if cfg.spec.enabled { 3 } else { 2 };
    let roles: &[&str] = if cfg.spec.enabled {
        &[
            SlotRole::Planner.as_config_key(),
            SlotRole::PlanCritic.as_config_key(),
            SlotRole::TestAuthor.as_config_key(),
        ]
    } else {
        &[
            SlotRole::Planner.as_config_key(),
            SlotRole::PlanCritic.as_config_key(),
        ]
    };
    let requested = opts.resolve_fleet(n_slots, roles, paths, cfg, &state.id)?;
    state.providers = providers::pick_providers(&requested, n_slots, Some(&requested), dry);
    // `state.providers` is positional: `provider_for(role, idx, …)` maps each slot by
    // index, so quota must gate the fleet in place, never compact it — dropping a paused
    // entry would slide another model into a role's slot and silently collapse the fleet
    // onto one model (identical assignment to --dry-run is the contract).
    if !dry {
        if let Err(e) = crate::quota::ensure_usable(paths, &state.providers) {
            state.error = Some(e.to_string());
            state.set_phase(Phase::Quota);
            paths.ensure_run_dirs(&state.id)?;
            state.save(paths)?;
            if opts.json {
                executor::emit_run_json(&state)?;
            } else {
                eprintln!("error: {e}");
            }
            return Ok(ExitCode::Quota);
        }
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
    for (idx, (id, role, template, prov)) in plan_slot_specs(&state, cfg).into_iter().enumerate() {
        let model = art.as_ref().and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.role.as_deref() == Some(role.as_config_key()) || c.slot == idx)
                .and_then(|c| c.model.clone())
        });
        state
            .slots
            .push(executor::init_slot_model(&id, &prov, role, model.clone()));
        jobs.push(SlotJob {
            slot_id: id,
            provider: prov,
            role,
            template: template.into(),
            extra_vars: HashMap::from([(
                "amendment_section".to_string(),
                plan_amendment_section(&state),
            )]),
            expected_artifact: Some("plan.md".into()),
            model,
        });
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
        return detach_self(&state, opts.json);
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

/// The planner + critic slot specs `(id, role, template, provider)`, drawn from the
/// resolved fleet via `provider_for` so both the first-pass and re-plan paths key the
/// two slots identically (explicit `--providers` positional > `[roles]` > order).
fn plan_slot_specs(
    state: &RunState,
    cfg: &Config,
) -> Vec<(String, SlotRole, &'static str, String)> {
    let specs = [
        (SlotRole::Planner, "planner", "planner"),
        (SlotRole::PlanCritic, "critic", "plan_critic"),
    ];
    let mut out = Vec::with_capacity(specs.len());
    for (idx, (role, prefix, template)) in specs.into_iter().enumerate() {
        let Some(prov) =
            crate::workflow::roles_resolve::provider_for(role, idx, &state.providers, cfg)
        else {
            continue;
        };
        let id = format!("{prefix}-{}", sanitize_slot(&prov));
        out.push((id, role, template, prov));
    }
    out
}

pub fn execute_plan(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    jobs: &[SlotJob],
) -> Result<()> {
    let slot_ids: Vec<String> = jobs.iter().map(|j| j.slot_id.clone()).collect();
    worktree::prepare_isolation(state, paths, &slot_ids)?;
    state.set_phase(Phase::SpawnSlots);
    state.save(paths)?;

    state.set_phase(Phase::Dispatch);
    state.save(paths)?;

    for job in jobs {
        if let Err(e) = executor::run_slot(state, paths, cfg, job) {
            // A quota-detected failure parks the run regardless of role: the critic is
            // best-effort feedback for the planner (a genuine critic defect still just
            // marks the slot Failed and the plan proceeds without it, unchanged), but a
            // rate limit is not a defect to shrug off — it must surface on the quota
            // gate the same way the planner's own dispatch already does, not silently
            // finish a plan with the critic's rate limit invisible to the caller.
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
    let critic_slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::PlanCritic)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| "critic".into());

    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| matches!(s.role, SlotRole::Planner | SlotRole::PlanCritic))
        .map(|s| s.provider.clone())
        .collect();
    let provider = resolve_spec_provider(cfg, state.dry_run, &state.providers, &used)?;
    let model = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten()
        .and_then(|a| {
            a.choices
                .iter()
                .find(|c| {
                    c.role.as_deref() == Some(SlotRole::TestAuthor.as_config_key()) || c.slot == 2
                })
                .and_then(|c| c.model.clone())
        });
    let safe = sanitize_slot(&provider);
    let id = format!("test-author-{safe}");

    if state.slots.iter().all(|s| s.id != id) {
        state.slots.push(executor::init_slot_model(
            &id,
            &provider,
            SlotRole::TestAuthor,
            model.clone(),
        ));
    }
    worktree::prepare_isolation(state, paths, std::slice::from_ref(&id))?;
    // After isolation so status/TUI show Spec for the author wall-clock, not PrepareIsolation.
    state.set_phase(Phase::Spec);
    state.save(paths)?;

    let _ = bus::join(paths, Some(&state.id), &id, Some(&provider), None);
    seed_spec_bus(state, paths, &id, &planner_slot, &critic_slot)?;

    let mut extra = HashMap::new();
    extra.insert("planner_slot".into(), planner_slot);
    extra.insert("critic_slot".into(), critic_slot);
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
    critic_slot: &str,
) -> Result<()> {
    let budget = state.message_budget;
    let body = format!(
        "Spec phase: `{author_id}` will freeze acceptance tests from plan.md. \
         Planner `{planner_slot}` and critic `{critic_slot}`: reply on bus if still available; \
         otherwise the author uses plan + critique artifacts."
    );
    let _ = bus::broadcast(paths, Some(&state.id), "orchestrator", &body, budget);

    for (to, note) in [
        (
            author_id,
            format!(
                "You are the test author. Coordinate with `{planner_slot}` and `{critic_slot}` via bus, then write tests + test-contract.md."
            ),
        ),
        (
            planner_slot,
            format!("Test author `{author_id}` is writing acceptance tests. Answer bus questions if you can."),
        ),
        (
            critic_slot,
            format!("Test author `{author_id}` is freezing the test bar. Challenge weak scenarios on the bus if you can."),
        ),
    ] {
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
fn resolve_spec_provider(
    cfg: &Config,
    dry: bool,
    fleet: &[String],
    used: &[String],
) -> Result<String> {
    if let Some(p) = &cfg.roles.test_author {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid [roles].test_author {p:?}: {e}"))?;
        if dry || providers::is_provider_usable(p, false) {
            return Ok(p.clone());
        }
        // Fall through to fleet if override is unusable (missing CLI / paused).
    }
    if dry {
        if let Some(p) = fleet.iter().find(|p| !used.contains(p)) {
            return Ok(p.clone());
        }
        if let Some(p) = fleet.get(2) {
            return Ok(p.clone());
        }
        if let Some(p) = fleet.last() {
            return Ok(p.clone());
        }
        return Ok("cli:claude".into());
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| !used.contains(p) && providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok(p);
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok(p);
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

fn detach_self(state: &RunState, json: bool) -> Result<ExitCode> {
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
    #[cfg(not(unix))]
    {
        anyhow::bail!("detach not supported on this platform yet");
    }

    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
        println!("detached; poll with: spar wait {}", state.id);
    }
    Ok(ExitCode::Success)
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
        jobs.push(SlotJob {
            slot_id: slot.id.clone(),
            provider: slot.provider.clone(),
            role: slot.role,
            template: template.into(),
            extra_vars: HashMap::from([(
                "amendment_section".to_string(),
                amendment_section.clone(),
            )]),
            expected_artifact: Some("plan.md".into()),
            model: None,
        });
    }
    if jobs.is_empty() {
        for (id, role, template, prov) in plan_slot_specs(&state, cfg) {
            if state.slots.iter().all(|s| s.id != id) {
                state.slots.push(executor::init_slot(&id, &prov, role));
            }
            jobs.push(SlotJob {
                slot_id: id,
                provider: prov,
                role,
                template: template.into(),
                extra_vars: HashMap::from([(
                    "amendment_section".to_string(),
                    amendment_section.clone(),
                )]),
                expected_artifact: Some("plan.md".into()),
                model: None,
            });
        }
        state.save(paths)?;
    }
    execute_plan(&mut state, paths, cfg, &jobs)?;
    Ok(state.exit_code())
}
