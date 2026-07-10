use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole, SlotStatus};
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
        return run_from_approved(&id, opts, paths, cfg);
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
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if !state.gates.plan_approved && state.phase != Phase::PlanApproved {
        bail!(
            "run {run_id} plan is not approved (phase={:?})",
            state.phase
        );
    }
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = opts.resolve_dry_run();
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    if state.dry_run {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let requested = opts.require_providers()?.to_vec();
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
    prepare_implement_slots(&mut state, Some(&requested), dry, cfg)?;
    if state.slots.iter().all(|s| s.role != SlotRole::Implementer) {
        bail!("no implementer slot after provider pick");
    }
    state.save(paths)?;
    if opts.detach {
        return detach_implement(&state, opts.json);
    }
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
        ensure_suite_slot(state, dry, cfg)?;
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

    state.slots.push(executor::init_slot("impl", &provs[0], SlotRole::Implementer));
    ensure_suite_slot(state, dry, cfg)?;
    state.slots.push(executor::init_slot(
        format!("review-{}-a", sanitize_slot(&provs[1])),
        &provs[1],
        SlotRole::Reviewer,
    ));
    state.slots.push(executor::init_slot(
        format!("review-{}-b", sanitize_slot(&provs[2])),
        &provs[2],
        SlotRole::Reviewer,
    ));
    Ok(())
}

/// Ensure a tester slot exists when suite is enabled. Fail closed if no provider.
fn ensure_suite_slot(state: &mut RunState, dry: bool, cfg: &Config) -> Result<()> {
    if !cfg.suite.enabled {
        return Ok(());
    }
    if state.slots.iter().any(|s| s.role == SlotRole::Tester) {
        return Ok(());
    }
    let suite_prov = resolve_suite_provider(cfg, dry, &state.providers)?;
    state.slots.push(executor::init_slot(
        format!("suite-{}", sanitize_slot(&suite_prov)),
        &suite_prov,
        SlotRole::Tester,
    ));
    Ok(())
}

/// Cheap suite-channel provider: config override, prefs, then fleet providers.
fn resolve_suite_provider(
    cfg: &Config,
    dry: bool,
    fleet: &[String],
) -> Result<String> {
    if let Some(p) = &cfg.suite.provider {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid suite.provider {p:?}: {e}"))?;
        return Ok(p.clone());
    }
    const PREFS: &[&str] = &[
        "cli:claude",
        "cli:grok",
        "cli:agy",
        "api:xai",
        "api:openai",
    ];
    if dry {
        return Ok(PREFS[0].into());
    }
    if let Some(p) = PREFS
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .map(|s| (*s).to_string())
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
    bail!(
        "suite.enabled but no usable suite provider (set [suite].provider or install a CLI)"
    )
}

/// Fail closed: missing/unrecognized Result ⇒ red. Only pass/skipped are green.
fn suite_report_is_red(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if let Some(idx) = lower.find("## result") {
        let after = &lower[idx..];
        let line = after
            .lines()
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_start_matches(['*', '`', '_', '-', ' ']);
        if line.starts_with("pass") || line.starts_with("skipped") {
            return false;
        }
        // fail / failed / blank / unknown after ## Result
        return true;
    }
    // No structured result — fail closed.
    true
}

#[cfg(test)]
mod suite_parse_tests {
    use super::suite_report_is_red;

    #[test]
    fn pass_and_skipped_green() {
        assert!(!suite_report_is_red("## Result\npass\n"));
        assert!(!suite_report_is_red("## Result\nskipped\n"));
    }

    #[test]
    fn fail_and_malformed_red() {
        assert!(suite_report_is_red("## Result\nfail\n"));
        assert!(suite_report_is_red("## Result\nfailed\n"));
        assert!(suite_report_is_red("## Result\n\n"));
        assert!(suite_report_is_red("no result header"));
        assert!(suite_report_is_red("## Result\n**fail**\n") || suite_report_is_red("## Result\nfail\n"));
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
    let requested = opts.require_providers()?.to_vec();

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
    prepare_implement_slots(&mut state, Some(&requested), dry, cfg)?;

    paths.ensure_run_dirs(&state.id)?;
    let _ = crate::bus::ensure_bus(paths, &state.id);
    let _ = crate::bus::join(paths, &state.id, "orchestrator", None, None);
    if let Some(body) = &plan_body {
        std::fs::write(paths.artifact(&state.id, "plan.md"), body)?;
        if state.big {
            let _ = crate::tasks::seed_from_plan(paths, &state.id, body);
        }
    }
    state.save(paths)?;

    if opts.detach {
        return detach_implement(&state, opts.json);
    }

    execute_loop(&mut state, paths, cfg)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

fn maybe_auto_ship_or_cleanup(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<()> {
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
    let test_contract_body = {
        let p = paths.artifact(&state.id, "test-contract.md");
        std::fs::read_to_string(&p).unwrap_or_else(|_| {
            "(no pre-written acceptance contract — implement without frozen tests)".into()
        })
    };

    // Merge pre-coding acceptance tests from test-author worktree once at start.
    if let Some(author) = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::TestAuthor)
        .map(|s| s.id.clone())
    {
        if let Some(impl_slot) = state.slots.iter().find(|s| s.role == SlotRole::Implementer) {
            if let Some(cwd) = impl_slot.cwd.clone() {
                let _ = worktree::apply_spec_tests_to_impl(state, &author, &cwd);
            }
        }
    }

    loop {
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
        let impl_job = SlotJob {
            slot_id: impl_slot.id.clone(),
            provider: impl_slot.provider.clone(),
            role: SlotRole::Implementer,
            template: "implementer".into(),
            extra_vars: extra,
            expected_artifact: Some(format!("summary-{}.md", impl_slot.id)),
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

        // Suite channel: cheap model runs full suites; reviewers must not re-run them.
        let mut suite_body = String::new();
        let mut suite_red = false;
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
                    paths.markers_dir(&state.id).join(format!("{}.done", tester.id)),
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
                };
                let suite_ok = executor::run_slot(state, paths, cfg, &suite_job).is_ok();
                suite_body = std::fs::read_to_string(&suite_path).unwrap_or_else(|_| {
                    format!(
                        "## Result\nfail\n\n## Summary\nmissing suite.md after suite slot `{}`\n",
                        tester.id
                    )
                });
                suite_red = !suite_ok || suite_report_is_red(&suite_body);
                let _ = crate::bus::broadcast(
                    paths,
                    &state.id,
                    "orchestrator",
                    if suite_red {
                        format!("suite channel red (slot {})", tester.id)
                    } else {
                        format!("suite channel green (slot {})", tester.id)
                    },
                    state.message_budget,
                );
            } else {
                suite_red = true;
                suite_body =
                    "## Result\nfail\n\n## Summary\nsuite.enabled but no tester slot was prepared\n"
                        .into();
                let _ = std::fs::write(paths.artifact(&state.id, "suite.md"), &suite_body);
            }
        }

        let contract_note = if paths.artifact(&state.id, "test-contract.md").is_file() {
            "\n- Check pre-written acceptance contract (`test-contract.md`) is still honored; do not accept deleted/weakened acceptance tests without a finding.\n"
        } else {
            ""
        };
        let suite_guidance = if suite_channel_active {
            format!(
                "## Suite channel (do not re-run full suites)\n\
                 A dedicated cheap tester slot runs the full suite. Results:\n\n\
                 {suite_body}\n\n\
                 - Do **not** kick off full multi-minute/hour test suites.\n\
                 - At most: static/diff review, plus optional 1–2 targeted tests on suspect files.\n\
                 - Use the suite report above for pass/fail evidence.\n\
                 - Orchestrator treats suite **fail** as request_changes even if you approve.\n\
                 {contract_note}"
            )
        } else {
            format!(
                "## Tests\nYou may run targeted or full suites as needed for confidence. Prefer evidence over claims.\n{contract_note}"
            )
        };

        state.set_phase(Phase::Review);
        state.save(paths)?;

        let reviewers: Vec<_> = state
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Reviewer)
            .cloned()
            .collect();

        let mut any_request_changes = suite_red;
        for rev in &reviewers {
            if let Some(s) = state.slot_mut(&rev.id) {
                s.status = SlotStatus::Pending;
                s.cwd = Some(review_cwd.clone());
            }
            let mut extra = HashMap::new();
            extra.insert("review_cwd".into(), review_cwd.display().to_string());
            extra.insert("suite_body".into(), suite_body.clone());
            extra.insert("suite_guidance".into(), suite_guidance.clone());
            let mut job = SlotJob {
                slot_id: rev.id.clone(),
                provider: rev.provider.clone(),
                role: SlotRole::Reviewer,
                template: "reviewer".into(),
                extra_vars: extra,
                expected_artifact: Some(format!("review-{}.md", rev.id)),
            };
            let mut review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
            if !review_ok {
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
                if text.to_ascii_lowercase().contains("request_changes") {
                    any_request_changes = true;
                }
            }
        }
        if !any_request_changes {
            write_impl_summary(state, paths)?;
            if state.big {
                if let Ok(mut g) = crate::tasks::TaskGraph::load(paths, &state.id) {
                    for t in g.ready_wave().iter().map(|t| t.id.clone()).collect::<Vec<_>>() {
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
    let candidate = ["cli:claude", "cli:grok", "cli:agy", "cli:claude", "cli:grok"]
        .iter()
        .map(|s| (*s).to_string())
        .chain(state.providers.iter().cloned())
        .find(|p| !existing.contains(p));
    let Some(prov) = candidate else {
        // still widen with a synthetic extra reviewer on a repeated provider
        let prov = existing.first().cloned().unwrap_or_else(|| "cli:claude".into());
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
    let mut body = format!(
        "# Implementation summary\n\nRun: {}\nTask: {}\nFix rounds: {}\n\n",
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

fn detach_implement(state: &RunState, json: bool) -> Result<ExitCode> {
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
        crate::cli::WorkflowKind::Plan => {
            return crate::workflow::plan::continue_run(paths, cfg, run_id);
        }
    }
    Ok(state.exit_code())
}
