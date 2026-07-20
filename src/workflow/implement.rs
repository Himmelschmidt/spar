use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole, SlotStatus, SuiteOutcome};
use crate::util;
use crate::worktree;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn run_from_cli(
    run_id: Option<String>,
    plan: Option<PathBuf>,
    task: Option<String>,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    if let Some(id) = run_id {
        return run_from_approved(&id, task, opts, paths, cfg);
    }
    if let Some(plan_path) = plan {
        let body = std::fs::read_to_string(&plan_path)?;
        let task =
            task.unwrap_or_else(|| format!("Implement approved plan from {}", plan_path.display()));
        return run_with_task(task, Some(body), opts, paths, cfg, None);
    }
    let task =
        task.ok_or_else(|| anyhow::anyhow!("implement requires --run, --plan, or --task"))?;
    run_with_task(task, None, opts, paths, cfg, None)
}

pub fn run_loop(opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let task = opts
        .task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--task required for loop workflow"))?;
    run_with_task(task, None, opts, paths, cfg, None)
}

fn run_from_approved(
    run_id: &str,
    amendment: Option<String>,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    let resumable = state.gates.plan_approved
        || state.phase == Phase::PlanApproved
        || state.phase == Phase::Stopped;
    if !resumable {
        bail!(
            "run {run_id} plan is not approved (phase={:?})",
            state.phase
        );
    }
    // Resuming a stopped run: drop the marker so execute_loop dispatches instead
    // of halting again at its first boundary.
    if state.phase == Phase::Stopped {
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
    }
    // `-t` on an approved run is a directive for THIS round only. It never rewrites
    // the run's task; absent `-t`, any prior amendment is cleared so it never silently
    // re-applies to a later round.
    state.amendment = amendment;
    if !opts.json {
        match &state.amendment {
            Some(a) => println!("amendment applied for this round: {a}"),
            None => println!("no amendment (running the original task)"),
        }
    }
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = opts.resolve_dry_run();
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    if state.dry_run {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let n = cfg.max_agents.max(3) as usize;
    let roles: Vec<&str> = std::iter::once("implementer")
        .chain(std::iter::repeat("reviewer"))
        .take(n)
        .collect();
    let requested = opts.resolve_fleet(n, &roles, paths, cfg, &state.id)?;
    // Quota filter before slot assignment so paused providers never get slots.
    if !state.dry_run {
        match crate::quota::apply_quota_filter(paths, &requested) {
            Ok(p) => state.providers = p,
            Err(e) => {
                state.error = Some(e.to_string());
                state.set_phase(Phase::Quota);
                state.save(paths)?;
                if opts.json {
                    executor::emit_run_json(&state)?;
                } else {
                    eprintln!("error: {e}");
                }
                return Ok(ExitCode::Quota);
            }
        }
    } else {
        state.providers = requested.clone();
    }
    let dry = state.dry_run;
    prepare_implement_slots(&mut state, Some(&requested), dry, cfg, paths)?;
    if state.slots.iter().all(|s| s.role != SlotRole::Implementer) {
        bail!("no implementer slot after provider pick");
    }
    if opts.detach {
        state.save(paths)?;
        return detach_implement(&state, paths, opts.json);
    }
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    state.save(paths)?;
    execute_loop(&mut state, paths, cfg)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

fn prepare_implement_slots(
    state: &mut RunState,
    requested: Option<&[String]>,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
) -> Result<()> {
    state.workflow = crate::cli::WorkflowKind::Loop;
    state.max_fix_rounds = 3;
    state.child_run = None;
    state.fix_rounds = 0;
    state.rotated_implementer = false;
    state.widened_reviewers = false;

    // Keep planner slots as historical; add impl/review if missing.
    let has_impl = state.slots.iter().any(|s| s.role == SlotRole::Implementer);
    if has_impl {
        ensure_suite_slot(state, dry, cfg, paths)?;
        return Ok(());
    }

    let Some(req) = requested.filter(|r| !r.is_empty()) else {
        bail!("--providers is required");
    };
    let n = cfg.max_agents.max(3) as usize;
    state.providers = providers::pick_providers(req, n, Some(req), dry);
    if state.providers.is_empty() {
        bail!("no usable providers from --providers {:?}", req);
    }
    // Cycle the explicit list to fill impl + reviewers (same provider may repeat).
    let mut provs = state.providers.clone();
    while provs.len() < 3 {
        provs.push(provs[0].clone());
    }

    // Apply model-select choices onto slots when artifact exists.
    let art = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten();
    let model_for = |idx: usize| -> Option<String> {
        art.as_ref().and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.slot == idx)
                .and_then(|c| c.model.clone())
        })
    };

    state.slots.push(executor::init_slot_model(
        "impl",
        &provs[0],
        SlotRole::Implementer,
        model_for(0),
    ));
    ensure_suite_slot(state, dry, cfg, paths)?;
    state.slots.push(executor::init_slot_model(
        format!("review-{}-a", sanitize_slot(&provs[1])),
        &provs[1],
        SlotRole::Reviewer,
        model_for(1),
    ));
    state.slots.push(executor::init_slot_model(
        format!("review-{}-b", sanitize_slot(&provs[2])),
        &provs[2],
        SlotRole::Reviewer,
        model_for(2),
    ));
    Ok(())
}

/// Ensure a tester slot exists when suite is enabled. Fail closed if no provider.
fn ensure_suite_slot(
    state: &mut RunState,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
) -> Result<()> {
    if !cfg.suite.enabled {
        return Ok(());
    }
    if state.slots.iter().any(|s| s.role == SlotRole::Tester) {
        return Ok(());
    }
    let (suite_prov, suite_model) =
        resolve_suite_provider(cfg, dry, &state.providers, Some(paths), Some(&state.id))?;
    state.slots.push(executor::init_slot_model(
        format!("suite-{}", sanitize_slot(&suite_prov)),
        &suite_prov,
        SlotRole::Tester,
        suite_model,
    ));
    Ok(())
}

/// Cheap suite-channel provider: config override, model-select (tester/fast), prefs, fleet.
fn resolve_suite_provider(
    cfg: &Config,
    dry: bool,
    fleet: &[String],
    paths: Option<&SparPaths>,
    run_id: Option<&str>,
) -> Result<(String, Option<String>)> {
    if let Some(p) = &cfg.suite.provider {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid suite.provider {p:?}: {e}"))?;
        return Ok((p.clone(), None));
    }
    // Prefer model-select artifact / fresh pick with tester role (fast profile).
    if let (Some(paths), Some(run_id)) = (paths, run_id) {
        if let Ok(Some(art)) = crate::model_select::load_select_artifact(paths, run_id) {
            if let Some(c) = art
                .choices
                .iter()
                .find(|c| c.role.as_deref() == Some("tester"))
            {
                return Ok((c.provider.clone(), c.model.clone()));
            }
            let exclude: Vec<String> = art.choices.iter().map(|c| c.vals_id.clone()).collect();
            let urgency = crate::model_select::Urgency::parse(&art.urgency)
                .unwrap_or(crate::model_select::Urgency::Normal);
            if let Ok(c) =
                crate::model_select::pick_one_for_role("tester", urgency, cfg, dry, &exclude)
            {
                // Append to artifact for audit trail.
                let mut art = art;
                let mut c = c;
                c.slot = art.choices.len();
                art.choices.push(c.clone());
                let _ = crate::model_select::write_select_artifact(paths, run_id, &art);
                return Ok((c.provider, c.model));
            }
        }
    }
    const PREFS: &[&str] = &["cli:claude", "cli:grok", "cli:agy", "api:xai", "api:openai"];
    if dry {
        return Ok((PREFS[0].into(), None));
    }
    if let Some(p) = PREFS
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .map(|s| (*s).to_string())
    {
        return Ok((p, None));
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok((p, None));
    }
    bail!("suite.enabled but no usable suite provider (set [suite].provider or install a CLI)")
}

enum SuiteResult {
    Pass,
    Fail,
}

/// Parse the `## Result` line. `None` means the file has no parsable verdict — the
/// agent wrote garbage, which is a runner problem, not a code failure.
fn parse_suite_result(body: &str) -> Option<SuiteResult> {
    let lower = body.to_ascii_lowercase();
    let idx = lower.find("## result")?;
    let after = &lower[idx..];
    let line = after
        .lines()
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_start_matches(['*', '`', '_', '-', ' ']);
    if line.starts_with("pass") || line.starts_with("skipped") {
        return Some(SuiteResult::Pass);
    }
    if line.starts_with("fail") {
        return Some(SuiteResult::Fail);
    }
    None
}

/// Tri-state suite verdict. `Fail` requires a clean tester exit AND a `## Result: fail`;
/// anything else uncertain (signal death, timeout, missing/garbled report) is `Inconclusive`.
fn derive_suite_outcome(slot_ok: bool, exit_code: Option<i32>, body: Option<&str>) -> SuiteOutcome {
    if !slot_ok || exit_code.is_none() {
        return SuiteOutcome::Inconclusive;
    }
    let Some(body) = body else {
        return SuiteOutcome::Inconclusive;
    };
    match parse_suite_result(body) {
        Some(SuiteResult::Pass) => SuiteOutcome::Pass,
        Some(SuiteResult::Fail) => SuiteOutcome::Fail,
        None => SuiteOutcome::Inconclusive,
    }
}

/// Both `Fail` and `Inconclusive` gate the ship (fail closed).
fn suite_blocks_ship(outcome: SuiteOutcome) -> bool {
    matches!(outcome, SuiteOutcome::Fail | SuiteOutcome::Inconclusive)
}

/// Why the suite was `Inconclusive`, for the bus broadcast and the reviewer prompt.
fn suite_inconclusive_reason(
    slot_ok: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
    body: Option<&str>,
) -> String {
    if let Some(sig) = signal {
        return format!("suite runner killed by signal {sig} before a clean report");
    }
    if exit_code.is_none() {
        return "suite runner exited without a status (timed out or killed)".into();
    }
    if !slot_ok {
        return "suite runner did not complete cleanly".into();
    }
    match body {
        None => "no suite.md written".into(),
        Some(_) => "suite.md has no parsable ## Result".into(),
    }
}

fn suite_guidance(outcome: SuiteOutcome) -> String {
    let header = "## Suite channel (do not re-run full suites)\n\
         A dedicated cheap tester slot runs the full suite; its output is the `## Suite report` section above.\n\n";
    match outcome {
        SuiteOutcome::Pass => format!(
            "{header}\
             - Do **not** kick off full multi-minute/hour test suites.\n\
             - At most: static/diff review, plus optional 1–2 targeted tests on suspect files.\n\
             - Use the suite report above for pass/fail evidence.\n"
        ),
        SuiteOutcome::Fail => format!(
            "{header}\
             - Do **not** kick off full multi-minute/hour test suites.\n\
             - At most: static/diff review, plus optional 1–2 targeted tests on suspect files.\n\
             - Use the suite report above for pass/fail evidence.\n\
             - Orchestrator treats suite **fail** as request_changes even if you approve.\n"
        ),
        SuiteOutcome::Inconclusive => format!(
            "{header}\
             - The suite channel is **inconclusive**: the runner fell over and the suite DID NOT RUN to a clean result. Do **not** cite this as a code or test failure.\n\
             - Do **not** kick off the full multi-minute/hour suite yourself.\n\
             - Instead, run 1–2 targeted tests on the files this change touches for confidence.\n"
        ),
    }
}

fn sanitize_slot(s: &str) -> String {
    s.replace([':', '/'], "-")
}

fn run_with_task(
    task: String,
    plan_body: Option<String>,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
    _parent_run: Option<String>,
) -> Result<ExitCode> {
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Loop,
        paths.project_root.clone(),
    );
    state.task = Some(task.clone());
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    state.big = opts.big;
    state.max_fix_rounds = 3;
    let n = cfg.max_agents.max(3) as usize;
    let roles: Vec<&str> = std::iter::once("implementer")
        .chain(std::iter::repeat("reviewer"))
        .take(n)
        .collect();
    let requested = opts.resolve_fleet(n, &roles, paths, cfg, &state.id)?;

    if !dry {
        match crate::quota::apply_quota_filter(paths, &requested) {
            Ok(p) => state.providers = p,
            Err(e) => {
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
    } else {
        state.providers = requested.clone();
    }
    prepare_implement_slots(&mut state, Some(&requested), dry, cfg, paths)?;

    paths.ensure_run_dirs(&state.id)?;
    let _ = crate::bus::ensure_bus(paths);
    let _ = crate::bus::join(paths, Some(&state.id), "orchestrator", None, None);
    if let Some(body) = &plan_body {
        std::fs::write(paths.artifact(&state.id, "plan.md"), body)?;
        if state.big {
            let _ = crate::tasks::seed_from_plan(paths, &state.id, body);
        }
    }
    state.save(paths)?;

    if opts.detach {
        return detach_implement(&state, paths, opts.json);
    }

    let _lock = crate::runlock::RunLock::acquire(paths, &state.id)?;
    execute_loop(&mut state, paths, cfg)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

fn maybe_auto_ship_or_cleanup(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    if state.phase == Phase::AwaitingShipConfirm && cfg.auto_ship() {
        state.gates.ship_confirmed = true;
        // leave at AwaitingShipConfirm with gate set — ship command still does push
        // unless we call ship; for dry-run mark Done
        if state.dry_run {
            state.set_phase(Phase::Done);
            state.save(paths)?;
        }
    }
    if cfg.auto_cleanup && state.phase.is_terminal() && matches!(state.phase, Phase::Done) {
        let _ = crate::worktree::cleanup_run(state);
    }
    Ok(())
}

/// True once `spar stop` has dropped the `stopped` marker for this run.
pub fn should_stop(paths: &SparPaths, run_id: &str) -> bool {
    crate::markers::marker_exists(paths, run_id, "stopped")
}

/// Halt without dispatching or touching worktrees; the run stays resumable.
fn stop_now(state: &mut RunState, paths: &SparPaths) -> Result<()> {
    state.set_phase(Phase::Stopped);
    state.save(paths)?;
    Ok(())
}

pub fn execute_loop(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    // Only isolate the implementer; reviewers share its cwd.
    let impl_ids: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .map(|s| s.id.clone())
        .collect();
    worktree::prepare_isolation(state, paths, &impl_ids)?;

    let plan_body =
        std::fs::read_to_string(paths.artifact(&state.id, "plan.md")).unwrap_or_default();
    let amendment_section = state
        .amendment
        .as_deref()
        .map(|a| {
            format!(
                "## Amendment (this round)\nThe operator supplied a directive for THIS round. It takes precedence over the original task where they conflict. The original task below is context; the amendment is the work.\n\n{a}\n"
            )
        })
        .unwrap_or_default();
    let test_contract_body = {
        let p = paths.artifact(&state.id, "test-contract.md");
        std::fs::read_to_string(&p).unwrap_or_else(|_| {
            "(no pre-written acceptance contract — implement without frozen tests)".into()
        })
    };

    // Bring pre-coding acceptance tests into implementer cwd (fail closed if author ran).
    if let Some(author) = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::TestAuthor)
        .map(|s| s.id.clone())
    {
        let impl_cwd = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .and_then(|s| s.cwd.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("implementer cwd missing; cannot apply acceptance tests")
            })?;
        if let Err(e) = worktree::apply_spec_tests_to_impl(state, &author, &impl_cwd) {
            return fail(
                state,
                paths,
                anyhow::anyhow!("failed to apply acceptance tests from {author}: {e}"),
            );
        }
    }

    loop {
        // Stop boundary: before the implementer (and every fix-round re-dispatch).
        if should_stop(paths, &state.id) {
            return stop_now(state, paths);
        }
        state.set_phase(Phase::Dispatch);
        state.save(paths)?;

        // Re-resolve implementer each iteration (stable id; provider may have rotated).
        let impl_slot = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no implementer slot"))?;

        if let Some(s) = state.slot_mut(&impl_slot.id) {
            s.status = SlotStatus::Pending;
            s.error = None;
        }

        let mut extra = HashMap::new();
        extra.insert("plan_body".into(), plan_body.clone());
        extra.insert("test_contract_body".into(), test_contract_body.clone());
        extra.insert("amendment_section".into(), amendment_section.clone());
        let impl_model = impl_slot.model.clone();
        let impl_job = SlotJob {
            slot_id: impl_slot.id.clone(),
            provider: impl_slot.provider.clone(),
            role: SlotRole::Implementer,
            template: "implementer".into(),
            extra_vars: extra,
            expected_artifact: Some(format!("summary-{}.md", impl_slot.id)),
            model: impl_model,
        };
        if let Err(e) = executor::run_slot(state, paths, cfg, &impl_job) {
            return fail(state, paths, e);
        }

        // Refresh implementer cwd after run (worktree may have been set at prepare).
        let impl_slot = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .cloned()
            .unwrap();
        let review_cwd = impl_slot
            .cwd
            .clone()
            .or_else(|| {
                state
                    .worktrees
                    .iter()
                    .find(|w| w.slot_id == impl_slot.id)
                    .map(|w| w.path.clone())
            })
            .unwrap_or_else(|| state.project_root.clone());

        // Stop boundary: before the suite job.
        if should_stop(paths, &state.id) {
            return stop_now(state, paths);
        }

        // Suite channel: cheap model runs full suites; reviewers must not re-run them.
        let mut suite_body = String::new();
        let mut suite_outcome = SuiteOutcome::Pass;
        let suite_channel_active = cfg.suite.enabled;
        if cfg.suite.enabled {
            let tester = state
                .slots
                .iter()
                .find(|s| s.role == SlotRole::Tester)
                .cloned();
            if let Some(tester) = tester {
                state.set_phase(Phase::Suite);
                state.save(paths)?;
                if let Some(s) = state.slot_mut(&tester.id) {
                    s.status = SlotStatus::Pending;
                    s.cwd = Some(review_cwd.clone());
                    s.error = None;
                }
                let suite_path = paths.artifact(&state.id, "suite.md");
                let _ = std::fs::remove_file(&suite_path);
                let _ = std::fs::remove_file(
                    paths
                        .markers_dir(&state.id)
                        .join(format!("{}.done", tester.id)),
                );
                let _ = std::fs::remove_file(
                    paths
                        .markers_dir(&state.id)
                        .join(format!("{}.failed", tester.id)),
                );
                let suite_job = SlotJob {
                    slot_id: tester.id.clone(),
                    provider: tester.provider.clone(),
                    role: SlotRole::Tester,
                    template: "tester".into(),
                    extra_vars: HashMap::new(),
                    expected_artifact: Some("suite.md".into()),
                    model: tester.model.clone(),
                };
                let suite_ok = executor::run_slot(state, paths, cfg, &suite_job).is_ok();
                // Absence is meaningful: a missing suite.md is Inconclusive, never a synthesized fail.
                let body_opt = std::fs::read_to_string(&suite_path).ok();
                let (exit_code, signal) = state
                    .slots
                    .iter()
                    .find(|s| s.id == tester.id)
                    .map(|s| (s.exit_code, s.signal))
                    .unwrap_or((None, None));
                suite_outcome = derive_suite_outcome(suite_ok, exit_code, body_opt.as_deref());
                suite_body = body_opt.clone().unwrap_or_default();
                let msg = match suite_outcome {
                    SuiteOutcome::Pass => format!("suite channel green (slot {})", tester.id),
                    SuiteOutcome::Fail => format!("suite channel red (slot {})", tester.id),
                    SuiteOutcome::Inconclusive => {
                        let reason = suite_inconclusive_reason(
                            suite_ok,
                            exit_code,
                            signal,
                            body_opt.as_deref(),
                        );
                        format!("suite channel inconclusive (slot {}): {reason}", tester.id)
                    }
                };
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    "orchestrator",
                    msg,
                    state.message_budget,
                );
            } else {
                suite_outcome = SuiteOutcome::Inconclusive;
                suite_body = "## Summary\nsuite.enabled but no tester slot was prepared\n".into();
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    "orchestrator",
                    "suite channel inconclusive: no tester slot prepared".to_string(),
                    state.message_budget,
                );
            }
            state.suite_outcome = Some(suite_outcome);
        }

        let suite_guidance = if suite_channel_active {
            suite_guidance(suite_outcome)
        } else {
            "## Tests\nYou may run targeted or full suites as needed for confidence. Prefer evidence over claims.\n".to_string()
        };

        state.set_phase(Phase::Review);
        state.save(paths)?;

        let reviewers: Vec<_> = state
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Reviewer)
            .cloned()
            .collect();

        let mut any_request_changes = suite_channel_active && suite_blocks_ship(suite_outcome);
        for rev in &reviewers {
            // Stop boundary: before each reviewer job.
            if should_stop(paths, &state.id) {
                return stop_now(state, paths);
            }
            if let Some(s) = state.slot_mut(&rev.id) {
                s.status = SlotStatus::Pending;
                s.cwd = Some(review_cwd.clone());
            }
            let mut extra = HashMap::new();
            extra.insert("review_cwd".into(), review_cwd.display().to_string());
            if !suite_body.is_empty() {
                extra.insert("suite_body".into(), suite_body.clone());
            }
            extra.insert("suite_guidance".into(), suite_guidance.clone());
            extra.insert("plan_body".into(), plan_body.clone());
            extra.insert("test_contract_body".into(), test_contract_body.clone());
            let mut job = SlotJob {
                slot_id: rev.id.clone(),
                provider: rev.provider.clone(),
                role: SlotRole::Reviewer,
                template: "reviewer".into(),
                extra_vars: extra,
                expected_artifact: Some(format!("review-{}.md", rev.id)),
                model: None,
            };
            let mut review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
            if !review_ok {
                // Stop boundary: don't re-dispatch a killed reviewer as a "failure".
                if should_stop(paths, &state.id) {
                    return stop_now(state, paths);
                }
                // Rotate provider and re-run once before treating as blocking failure.
                if try_rotate_reviewer_provider(state, paths, &rev.id, &review_cwd, cfg)? {
                    if let Some(s) = state.slots.iter().find(|s| s.id == rev.id) {
                        job.provider = s.provider.clone();
                    }
                    if let Some(s) = state.slot_mut(&rev.id) {
                        s.status = SlotStatus::Pending;
                        s.error = None;
                    }
                    review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
                }
            }

            let review_path = paths.artifact(&state.id, &format!("review-{}.md", rev.id));
            let review_text = std::fs::read_to_string(&review_path).ok();
            let missing_or_empty = review_text
                .as_ref()
                .map(|t| t.trim().is_empty())
                .unwrap_or(true);

            // Fail closed: failed slot or missing review artifact ⇒ treat as request_changes.
            // Timeout salvage may have already written a partial review-*.md.
            if !review_ok || missing_or_empty {
                any_request_changes = true;
                if missing_or_empty {
                    let _ = std::fs::write(
                        &review_path,
                        format!(
                            "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — review slot `{}` failed or produced no artifact\n",
                            rev.id
                        ),
                    );
                }
            } else if let Some(text) = review_text {
                // Fail closed: only an anchored `## Verdict` / approve clears the gate.
                if !crate::workflow::review_result::parse_review(&text).approves() {
                    any_request_changes = true;
                }
            }
        }
        if !any_request_changes {
            write_impl_summary(state, paths)?;
            if state.big {
                if let Ok(mut g) = crate::tasks::TaskGraph::load(paths, &state.id) {
                    for t in g
                        .ready_wave()
                        .iter()
                        .map(|t| t.id.clone())
                        .collect::<Vec<_>>()
                    {
                        g.mark_done(&t);
                    }
                    // mark all done for dry/simple path after successful review
                    for t in &mut g.tasks {
                        t.status = crate::tasks::TaskStatus::Done;
                    }
                    let _ = g.save(paths);
                }
            }
            if cfg.auto_ship() && state.dry_run {
                state.gates.ship_confirmed = true;
                state.set_phase(Phase::Done);
            } else {
                state.set_phase(Phase::AwaitingShipConfirm);
            }
            state.save(paths)?;
            return Ok(());
        }

        state.fix_rounds += 1;
        if state.fix_rounds > state.max_fix_rounds {
            // stuck policy: rotate implementer → widen reviewers → escalate
            if !state.rotated_implementer && try_rotate_implementer(state, paths)? {
                state.rotated_implementer = true;
                state.fix_rounds = 0;
                state.save(paths)?;
                continue;
            }
            if !state.widened_reviewers && try_widen_reviewers(state, paths, &review_cwd)? {
                state.widened_reviewers = true;
                state.fix_rounds = 0;
                state.save(paths)?;
                continue;
            }
            state.set_phase(Phase::Stuck);
            state.error = Some("fix rounds exhausted; escalated".into());
            state.save(paths)?;
            write_stuck(paths, &state.id)?;
            return Ok(());
        }
        state.set_phase(Phase::Fix);
        state.save(paths)?;
    }
}

/// Change implementer **provider** only; keep stable slot id and worktree.
fn try_rotate_implementer(state: &mut RunState, paths: &SparPaths) -> Result<bool> {
    let current = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .map(|s| s.provider.clone());
    let Some(cur) = current else {
        return Ok(false);
    };
    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .map(|s| s.provider.clone())
        .collect();
    let defaults = ["cli:claude", "cli:grok", "cli:agy"];
    let next = state
        .providers
        .iter()
        .map(|s| s.as_str())
        .chain(defaults.iter().copied())
        .find(|p| *p != cur.as_str() && !used.iter().any(|u| u == p))
        .map(|s| s.to_string());
    let Some(next) = next else {
        return Ok(false);
    };
    let impl_id = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .map(|s| s.id.clone())
        .unwrap();
    if let Some(s) = state.slot_mut(&impl_id) {
        s.provider = next;
        s.status = SlotStatus::Pending;
        s.error = None;
    }
    state.save(paths)?;
    Ok(true)
}

/// Add an extra adversarial reviewer from a provider not already reviewing.
fn try_widen_reviewers(
    state: &mut RunState,
    paths: &SparPaths,
    review_cwd: &std::path::Path,
) -> Result<bool> {
    let existing: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Reviewer)
        .map(|s| s.provider.clone())
        .collect();
    let candidate = [
        "cli:claude",
        "cli:grok",
        "cli:agy",
        "cli:claude",
        "cli:grok",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .chain(state.providers.iter().cloned())
    .find(|p| !existing.contains(p));
    let Some(prov) = candidate else {
        // still widen with a synthetic extra reviewer on a repeated provider
        let prov = existing
            .first()
            .cloned()
            .unwrap_or_else(|| "cli:claude".into());
        let id = format!("review-{}-wide", state.slots.len());
        let mut slot = executor::init_slot(&id, &prov, SlotRole::Reviewer);
        slot.cwd = Some(review_cwd.to_path_buf());
        state.slots.push(slot);
        state.save(paths)?;
        return Ok(true);
    };
    let id = format!("review-{prov}-wide");
    if state.slots.iter().any(|s| s.id == id) {
        return Ok(false);
    }
    let mut slot = executor::init_slot(&id, &prov, SlotRole::Reviewer);
    slot.cwd = Some(review_cwd.to_path_buf());
    state.slots.push(slot);
    state.save(paths)?;
    Ok(true)
}

/// Returns true if provider was changed.
fn try_rotate_reviewer_provider(
    state: &mut RunState,
    paths: &SparPaths,
    rev_id: &str,
    review_cwd: &std::path::Path,
    cfg: &Config,
) -> Result<bool> {
    let cur = state
        .slots
        .iter()
        .find(|s| s.id == rev_id)
        .map(|s| s.provider.clone());
    let Some(cur) = cur else {
        return Ok(false);
    };
    let next = state
        .providers
        .iter()
        .find(|p| **p != cur)
        .cloned()
        .or_else(|| cfg.providers.order.iter().find(|p| **p != cur).cloned());
    let Some(next) = next else {
        return Ok(false);
    };
    if let Some(s) = state.slot_mut(rev_id) {
        s.provider = next;
        s.cwd = Some(review_cwd.to_path_buf());
        s.status = SlotStatus::Pending;
        s.error = None;
    }
    state.save(paths)?;
    Ok(true)
}

fn fail(state: &mut RunState, paths: &SparPaths, e: anyhow::Error) -> Result<()> {
    state.set_phase(Phase::Failed);
    state.error = Some(e.to_string());
    state.save(paths)?;
    Err(e)
}

fn write_impl_summary(state: &RunState, paths: &SparPaths) -> Result<()> {
    let suite_line = match state.suite_outcome {
        Some(SuiteOutcome::Pass) => "Suite: pass\n",
        Some(SuiteOutcome::Fail) => "Suite: fail\n",
        Some(SuiteOutcome::Inconclusive) => {
            "Suite: inconclusive (runner fell over; tests did not run)\n"
        }
        None => "",
    };
    let mut body = format!(
        "# Implementation summary\n\nRun: {}\nTask: {}\nFix rounds: {}\n{suite_line}\n",
        state.id,
        state.task.as_deref().unwrap_or(""),
        state.fix_rounds
    );
    for s in &state.slots {
        body.push_str(&format!("- {} ({}) {:?}\n", s.id, s.provider, s.status));
    }
    body.push_str("\nShip when ready: `spar ship ");
    body.push_str(&state.id);
    body.push_str("` (requires confirm).\n");
    std::fs::write(paths.artifact(&state.id, "summary.md"), body)?;
    Ok(())
}

fn write_stuck(paths: &SparPaths, run_id: &str) -> Result<()> {
    std::fs::write(
        paths.artifact(run_id, "escalation.md"),
        "# Escalation\n\nStuck policy exhausted. Human intervention required.\n",
    )?;
    Ok(())
}

fn finish_out(state: &RunState, json: bool) -> Result<()> {
    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
    }
    Ok(())
}

fn detach_implement(state: &RunState, paths: &SparPaths, json: bool) -> Result<ExitCode> {
    if let Some(owner) = crate::runlock::RunLock::owner(paths, &state.id) {
        if owner.alive() {
            return Err(crate::runlock::OrchestratorBusy {
                run_id: state.id.clone(),
                owner_pid: owner.pid,
            }
            .into());
        }
    }
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
        println!("detached; wait with: spar wait {}", state.id);
    }
    Ok(ExitCode::Success)
}

pub fn continue_run(paths: &SparPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.workflow == crate::cli::WorkflowKind::Plan {
        return crate::workflow::plan::continue_run(paths, cfg, run_id);
    }
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    match state.workflow {
        crate::cli::WorkflowKind::Loop => {
            execute_loop(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Arena => {
            crate::workflow::arena::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Roles => {
            crate::workflow::roles::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Peer => {
            crate::workflow::peer::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Review => {
            crate::workflow::review::execute(&mut state, paths, cfg)?;
        }
        crate::cli::WorkflowKind::Plan => unreachable!("plan handled above"),
    }
    Ok(state.exit_code())
}

#[cfg(test)]
mod suite_parse_tests {
    use super::{
        derive_suite_outcome, should_stop, suite_blocks_ship, suite_guidance, SuiteOutcome,
    };
    use crate::paths::SparPaths;
    use tempfile::tempdir;

    #[test]
    fn should_stop_tracks_marker() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert!(!should_stop(&paths, "r1"));
        crate::markers::write_marker(&paths, "r1", "stopped", "by operator").unwrap();
        assert!(should_stop(&paths, "r1"));
    }

    #[test]
    fn clean_exit_fail_report_is_fail() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\nfail\n")),
            SuiteOutcome::Fail
        );
    }

    #[test]
    fn clean_exit_pass_or_skipped_is_pass() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\npass\n")),
            SuiteOutcome::Pass
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\nskipped\n")),
            SuiteOutcome::Pass
        );
    }

    #[test]
    fn clean_exit_no_body_is_inconclusive() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), None),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn clean_exit_unparsable_result_is_inconclusive() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n\n")),
            SuiteOutcome::Inconclusive
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("no result header at all")),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn signal_death_with_fail_body_is_inconclusive_not_fail() {
        // Body is not trustworthy when the runner was signal-killed (no exit code captured).
        assert_eq!(
            derive_suite_outcome(false, None, Some("## Result\nfail\n")),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn timeout_is_inconclusive() {
        // Timeout kills the process group: no exit code captured.
        assert_eq!(
            derive_suite_outcome(false, None, None),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn fail_markup_tolerated() {
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n**fail**\n")),
            SuiteOutcome::Fail
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n`fail`\n")),
            SuiteOutcome::Fail
        );
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\n- fail\n")),
            SuiteOutcome::Fail
        );
    }

    #[test]
    fn inconclusive_and_fail_both_block_ship() {
        assert!(suite_blocks_ship(SuiteOutcome::Fail));
        assert!(suite_blocks_ship(SuiteOutcome::Inconclusive));
        assert!(!suite_blocks_ship(SuiteOutcome::Pass));
    }

    #[test]
    fn tester_template_forbids_backgrounding_and_warns_pkill() {
        let tester = include_str!("../../templates/tester.md");
        let lower = tester.to_lowercase();
        assert!(lower.contains("foreground"), "must mandate foreground");
        assert!(lower.contains("background"), "must address backgrounding");
        assert!(
            tester.contains("nohup") && tester.contains("disown") && tester.contains('&'),
            "must forbid the concrete backgrounding mechanisms"
        );
        assert!(
            tester.contains("pkill -f"),
            "must carry the pkill -f warning"
        );
    }

    #[test]
    fn implementer_template_warns_pkill() {
        let implementer = include_str!("../../templates/implementer.md");
        assert!(implementer.contains("pkill -f"));
    }

    #[test]
    fn tester_template_never_routes_budget_exhaustion_to_green() {
        let tester = include_str!("../../templates/tester.md");
        let lower = tester.to_lowercase();
        let budget_rule = lower
            .lines()
            .find(|l| l.contains("cannot complete within the budget"))
            .expect("budget-exhaustion rule must exist");
        assert!(
            budget_rule.contains("inconclusive"),
            "budget-exhaustion must be reported as inconclusive, got: {budget_rule}"
        );
        assert!(
            !budget_rule.contains("= `skipped`"),
            "budget-exhaustion must not be assigned `skipped` (skipped maps to a green Pass): {budget_rule}"
        );
        // `skipped -> Pass` stays reserved strictly for a repo with no test suite.
        assert!(
            lower.contains("skipped` only when no suite could be found")
                || lower.contains("skipped only when no suite could be found"),
            "skipped must remain reserved for 'no suite could be found'"
        );
        // The verdict the template now mandates for budget exhaustion must gate the ship.
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some("## Result\ninconclusive\n")),
            SuiteOutcome::Inconclusive
        );
    }

    #[test]
    fn guidance_distinguishes_inconclusive_from_fail() {
        let inconclusive = suite_guidance(SuiteOutcome::Inconclusive).to_lowercase();
        assert!(inconclusive.contains("did not run"));
        assert!(!inconclusive.contains("treats suite"));

        let fail = suite_guidance(SuiteOutcome::Fail).to_lowercase();
        assert!(fail.contains("request_changes"));
        assert!(fail.contains("treats suite"));
        assert!(!fail.contains("did not run"));
    }

    #[test]
    fn test_author_template_emits_criterion_ids() {
        let test_author = include_str!("../../templates/test_author.md");
        assert!(
            test_author.contains("AC-1:"),
            "contract format must show the AC-n criterion id shape"
        );
        assert!(
            test_author.contains("verify:"),
            "each criterion must carry a verify: hint"
        );
    }

    #[test]
    fn reviewer_template_sees_plan_and_contract() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("{{plan_body}}"),
            "reviewer must receive the plan"
        );
        assert!(
            reviewer.contains("{{test_contract_body}}"),
            "reviewer must receive the acceptance contract"
        );
    }

    #[test]
    fn reviewer_template_uses_suite_body() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("{{suite_body}}"),
            "suite_body is seeded in base_vars and must be referenced"
        );
    }
}
