use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole, SlotState, SlotStatus, SuiteOutcome};
use crate::util::{self, sanitize_slot};
use crate::workflow::review_result::{self, AcStatus, ReviewResult};
use crate::worktree;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

pub fn run_from_cli(
    run_id: Option<String>,
    plan: Option<PathBuf>,
    task: Option<String>,
    new: bool,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    if let Some(id) = run_id {
        return run_from_approved(&id, task, opts, paths, cfg);
    }
    if let Some(plan_path) = plan {
        // A plan belongs to the run that produced it. Implementing it is that run
        // continuing, not a new one (O45) — this is the bypass that split biddesk's
        // work across two ids 35 times.
        // `--new` says this is separate work, so it wins over the trace.
        if let Some(id) = run_id_for_plan(&plan_path, paths).filter(|_| !new) {
            if !opts.json {
                eprintln!("continuing run {id} (it wrote this plan)");
            }
            return run_from_approved(&id, task, opts, paths, cfg);
        }
        if !new {
            bail!("{}", unlinked_plan_error(&plan_path, paths));
        }
        let body = std::fs::read_to_string(&plan_path)?;
        let task =
            task.unwrap_or_else(|| format!("Implement approved plan from {}", plan_path.display()));
        return run_with_task(task, Some(body), opts, paths, cfg, None);
    }
    let task =
        task.ok_or_else(|| anyhow::anyhow!("implement requires --run, --plan, or --task"))?;
    run_with_task(task, None, opts, paths, cfg, None)
}

/// The run that wrote this plan, if spar wrote it: plan artifacts live at
/// `.spar/runs/<id>/artifacts/plan*.md`, so the id is in the path.
fn run_id_for_plan(plan: &std::path::Path, paths: &SparPaths) -> Option<String> {
    let plan = plan.canonicalize().ok()?;
    if plan.extension()?.to_str()? != "md" {
        return None;
    }
    let runs = paths.runs_dir().canonicalize().ok()?;
    let rest = plan.strip_prefix(&runs).ok()?;
    let mut parts = rest.components();
    let id = parts.next()?.as_os_str().to_str()?.to_string();
    // Exactly `<id>/artifacts/<something>.md`. Anything else under the run dir — a
    // log, a marker, a nested project's `.spar` — is not a plan this run wrote, and
    // attaching to it would re-dispatch the whole implement fleet on a typo.
    if parts.next()?.as_os_str() != "artifacts" {
        return None;
    }
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    paths.state_file(&id).is_file().then_some(id)
}

/// What to say when someone hands us a plan we cannot trace to a run. Naming the
/// candidates is the point: the answer is almost always one of them.
fn unlinked_plan_error(plan: &std::path::Path, paths: &SparPaths) -> String {
    let mut msg = format!(
        "--plan {} does not belong to a run, so spar cannot tell which unit of work this continues.\n\
         A run covers a task from brief to draft PR; implementing its plan is that run continuing (O45).\n",
        plan.display()
    );
    let mut candidates: Vec<crate::state::RunSummary> = crate::state::list_runs(paths)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            matches!(
                r.phase,
                Phase::PlanApproved
                    | Phase::AwaitingPlanApproval
                    | Phase::PlanReady
                    | Phase::Stopped
            )
        })
        .collect();
    candidates.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    if candidates.is_empty() {
        msg.push_str(
            "\nNo run here is waiting to be implemented. If this really is new work: --new",
        );
    } else {
        msg.push_str("\nContinue one of these instead:\n");
        for r in candidates.iter().take(5) {
            msg.push_str(&format!(
                "  spar implement --run {}   # {} · {}\n",
                r.id,
                serde_json::to_string(&r.phase)
                    .unwrap_or_default()
                    .trim_matches('"'),
                r.task
                    .as_deref()
                    .unwrap_or("(no task)")
                    .chars()
                    .take(56)
                    .collect::<String>()
            ));
        }
        msg.push_str("\nOr, if this really is a new unit of work: --new");
    }
    msg
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
        || state.phase == Phase::Stopped
        // The round-ceiling gate is lifted by re-entering implement, so a run parked
        // there has to be resumable even when no plan gate ever ran (`--workflow loop`).
        || state.phase == Phase::AwaitingRoundExtension;
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
    // Resuming a terminal run (a failed/stuck attempt): clear the dead verdict so the detach
    // snapshot reads as a fresh re-dispatch, not `failed` with `exit_code: 1` (which reads as
    // a refusal). This is purely for snapshot coherence — `execute_loop` re-dispatches the
    // coding slots regardless of their incoming status; here we just make the persisted state
    // it briefly emits honest. Per-slot terminal markers are cleared at dispatch by the executor.
    if matches!(state.phase, Phase::Failed | Phase::Stuck) {
        for s in &mut state.slots {
            if matches!(s.status, SlotStatus::Failed | SlotStatus::Stuck) {
                s.status = SlotStatus::Pending;
                s.error = None;
                s.exit_code = None;
                s.signal = None;
                s.pid = None;
            }
        }
        state.error = None;
        state.set_phase(Phase::PrepareIsolation);
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
    // A resumed run keeps the base its plan phase was cut from. `--base` may only
    // re-point a run with no worktrees yet: the plan phase's test-author worktree is
    // reused as-is and its whole tree is overlaid onto the implementer, so a run
    // straddling two bases produces a branch parented on the new base carrying the old
    // base's file contents — a diff that reads as reverting the difference.
    if opts.base.is_some() {
        let previous = state.base_commit.clone();
        worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
        if !state.worktrees.is_empty() && previous != state.base_commit {
            bail!(
                "run {run_id} is already based on {}; a run's base is fixed when it is \
                 created — plan a new run with `--base` instead",
                previous.as_deref().unwrap_or("an unrecorded base")
            );
        }
    }
    if state.dry_run {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let n = cfg.max_agents.max(3) as usize;
    let roles: Vec<&str> = std::iter::once(SlotRole::Implementer.as_config_key())
        .chain(std::iter::repeat(SlotRole::Reviewer.as_config_key()))
        .take(n)
        .collect();
    let requested = opts.resolve_fleet(n, &roles, paths, cfg, &state.id)?;
    state.providers = providers::pick_providers(&requested, n, Some(&requested), state.dry_run);
    // Gate the positional fleet in place — never compact it, or a paused provider would
    // slide a different model into a role's slot (silent single-model collapse). Paused
    // providers fail loud so the review panel stays exactly what was specified.
    if !state.dry_run {
        if let Err(e) = crate::quota::ensure_usable(paths, &state.providers) {
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
    // Continuing an approved plan is this unit of work's next round, not a new run
    // (O45): the id, brief, base, config and usage ledger all carry. Counted here,
    // past the quota gate, so an invocation that never dispatched anything does not
    // leave the run claiming a round it did not run — five bounced retries against an
    // exhausted bucket would otherwise read as round 6.
    let from_round_gate = state.phase == Phase::AwaitingRoundExtension;
    if let Some(m) = opts.max_rounds {
        state.max_rounds = m;
    }
    if state.round_ceiling_reached() {
        escalate_round_ceiling(&mut state, paths)?;
        finish_out(&state, opts.json)?;
        return Ok(state.exit_code());
    }
    // The ceiling has been lifted: drop the verdict that parked the run, or a run that
    // goes on to reach the ship gate still reports "round ceiling reached" as its
    // reason, and an outer agent reading `error` at exit 2 re-issues `--max-rounds`
    // instead of shipping.
    if from_round_gate {
        state.error = None;
    }
    let round = state.begin_round();
    if !opts.json {
        println!("run {run_id} · round {round}");
    }
    let dry = state.dry_run;
    prepare_implement_slots(
        &mut state,
        Some(&requested),
        dry,
        cfg,
        paths,
        from_round_gate,
    )?;
    if state.slots.iter().all(|s| s.role != SlotRole::Implementer) {
        bail!("no implementer slot after provider pick");
    }
    if opts.detach {
        state.save(paths)?;
        return detach_implement(&state, paths, opts.json);
    }
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    state.save(paths)?;
    execute_loop(&mut state, paths, cfg, opts.accept_contract)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

/// Reviewer slots spawned alongside the implementer. The fleet width (`max_agents.max(3)`)
/// still bounds provider resolution; this names the review-panel size once instead of the
/// three unrelated coincidences (the old `while` pad, two literal pushes, `.max(3)`).
const DEFAULT_REVIEWERS: usize = 2;

fn prepare_implement_slots(
    state: &mut RunState,
    requested: Option<&[String]>,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
    resuming_from_round_gate: bool,
) -> Result<()> {
    state.workflow = crate::cli::WorkflowKind::Loop;
    state.max_fix_rounds = 3;
    state.child_run = None;
    // A run lifted off the round ceiling is the *same* escalation continuing: the
    // operator bought rounds, not a fresh stuck ladder. Resetting these here made
    // `stuck` unreachable in a lift loop, because every lift handed the run another
    // rotate + widen it had already spent (O52).
    if !resuming_from_round_gate {
        state.fix_rounds = 0;
        state.rotated_implementer = false;
        state.widened_reviewers = false;
    }

    // Keep planner slots as historical; add impl/review if missing.
    let has_impl = state.slots.iter().any(|s| s.role == SlotRole::Implementer);
    if has_impl {
        ensure_suite_slot(state, dry, cfg, paths)?;
        return Ok(());
    }

    let req = requested.unwrap_or_default();
    let n = cfg.max_agents.max(3) as usize;
    state.providers = providers::pick_providers(req, n, Some(req), dry);
    if state.providers.is_empty() {
        bail!("no usable providers (pass --providers, set a [roles] block, or --select)");
    }
    let fleet = state.providers.clone();

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

    // Implementer at fleet index 0; a fixed number of reviewers follow it positionally.
    // `provider_for` keys each slot: explicit --providers wins, else [roles], else order.
    let impl_prov =
        crate::workflow::roles_resolve::provider_for(SlotRole::Implementer, 0, &fleet, cfg)
            .ok_or_else(|| anyhow::anyhow!("no provider resolved for implementer"))?;
    state.slots.push(executor::init_slot_model(
        "impl",
        &impl_prov,
        SlotRole::Implementer,
        model_for(0),
    ));
    ensure_suite_slot(state, dry, cfg, paths)?;
    for r in 0..DEFAULT_REVIEWERS {
        let idx = r + 1;
        let Some(prov) =
            crate::workflow::roles_resolve::provider_for(SlotRole::Reviewer, idx, &fleet, cfg)
        else {
            continue;
        };
        // Id: reviewer index + the model-free provider name (storage_key drops `@model`),
        // so two slots on `cli:codex@a` / `cli:codex@b` still get distinct ids.
        let name = crate::provider_ref::ProviderRef::parse(&prov)
            .map(|p| p.storage_key())
            .unwrap_or_else(|_| prov.clone());
        state.slots.push(executor::init_slot_model(
            format!("review-{r}-{}", sanitize_slot(&name)),
            &prov,
            SlotRole::Reviewer,
            model_for(idx),
        ));
    }
    Ok(())
}

/// Ensure a tester slot exists when suite is enabled. Fail closed if no provider.
fn ensure_suite_slot(
    state: &mut RunState,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
) -> Result<()> {
    if cfg.suite.is_builtin() {
        // A run that had an agent tester before `[suite].command` was added (or before a
        // `--reload-config`) would otherwise carry a slot nothing ever dispatches. A
        // *live* one is kept: its slot id is the only key `live_slot_pids` has for the
        // agent still burning tokens behind it, and dropping the record is how an orphan
        // becomes unreapable (O28).
        state
            .slots
            .retain(|s| s.role != SlotRole::Tester || s.status == SlotStatus::Running);
        return Ok(());
    }
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
    if let Some(p) = &cfg.roles.tester {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid [roles].tester {p:?}: {e}"))?;
        if dry || providers::is_provider_usable(p, false) {
            return Ok((p.clone(), None));
        }
        // Fall through to model-select / prefs / fleet if the override is unusable
        // (missing CLI / paused).
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
    bail!("suite.enabled but no usable suite provider (set [roles].tester or install a CLI)")
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

/// Acceptance gate (DECISIONS O19). An empty `criteria` means there is no contract at
/// all (`[spec].enabled = false`) — acceptance is not evaluated and the verdict alone
/// gates. Otherwise this is fail closed: a criterion the reviewer never mentioned counts
/// against the ship exactly like a reported `fail`.
fn acceptance_blocks_ship(criteria: &[String], res: &ReviewResult, cfg: &Config) -> bool {
    if criteria.is_empty() {
        return false;
    }
    criteria
        .iter()
        .any(|id| match res.acceptance.iter().find(|a| &a.id == id) {
            None => true,
            Some(a) => match a.status {
                AcStatus::Fail => true,
                AcStatus::Unverified => cfg.review.require_all_criteria,
                AcStatus::Pass => false,
            },
        })
}

/// Which criteria blocked and why, one line each.
///
/// Per criterion rather than pre-joined: the carry-forward brief renders these as bullets
/// and truncates on line boundaries, so a single joined line (measured at 4k-10k chars on
/// real reviews) is dropped whole and leaves a heading saying nothing blocked.
fn acceptance_block_reasons(criteria: &[String], res: &ReviewResult, cfg: &Config) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    for id in criteria {
        match res.acceptance.iter().find(|a| &a.id == id) {
            None => parts.push(format!("{id}: not reported by the reviewer")),
            Some(a) => match a.status {
                AcStatus::Fail => {
                    let ev = if a.evidence.is_empty() {
                        "no evidence given".to_string()
                    } else {
                        a.evidence.clone()
                    };
                    parts.push(format!("{id}: fail — {ev}"));
                }
                AcStatus::Unverified if cfg.review.require_all_criteria => {
                    parts.push(format!("{id}: unverified"))
                }
                _ => {}
            },
        }
    }
    parts
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

/// Added test files that the suite's own command list never names.
///
/// The tester writes its command list itself, so an implementer that adds a new
/// integration-test binary can leave the authoritative gate passing green without ever
/// compiling the code written to prove the change. This warns; it never edits the
/// command list, because a harness that rewrites its own gate is not a gate.
///
/// Silent unless the suite is *selective*. A bare `cargo test` / `pytest` compiles
/// everything, so naming no files there means nothing — warning on it would fire on
/// every run and train the operator to ignore it.
fn unreferenced_test_files(cwd: &Path, base: Option<&str>, suite_body: &str) -> Vec<String> {
    let base = match base {
        Some(b) => b,
        None => return Vec::new(),
    };
    let commands = suite_commands(suite_body);
    // *Every* command must be narrow. One command running the project default covers the
    // added files whatever the others do, so `any` here would warn about files that were
    // in fact compiled.
    if commands.is_empty() || !commands.iter().all(|c| command_is_selective(c)) {
        return Vec::new();
    }
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=A", base, "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success());
    let Some(out) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        // Test *targets* only. A `tests/common/mod.rs` helper or a `tests/data/golden.json`
        // fixture is not a target anything can name, so reporting it as uncovered is a
        // false claim in the one place a false claim is most expensive: the reviewers'
        // suite report.
        .filter(|f| !f.is_empty() && is_test_path(f) && is_test_target(f))
        .filter(|f| !commands.iter().any(|c| command_names(c, f)))
        .map(str::to_string)
        .collect()
}

/// Command lines from the report's `## Commands` section (`- \`cmd\` → exit N`).
fn suite_commands(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("##") {
            in_section = t
                .trim_start_matches('#')
                .trim()
                .eq_ignore_ascii_case("commands");
            continue;
        }
        if in_section {
            if let Some(rest) = t.strip_prefix('-') {
                let cmd = rest.trim().trim_start_matches('`');
                let cmd = cmd.split('`').next().unwrap_or(cmd).trim();
                if !cmd.is_empty() {
                    out.push(cmd.to_string());
                }
            }
        }
    }
    out
}

/// True when a command narrows to specific *targets* instead of running the project's
/// default set.
///
/// Explicit target flags plus test-file arguments only. Earlier this also counted any
/// token containing `/`, which made `pytest tests/`, `go test ./...` and
/// `./scripts/test.sh` all read as selective — the canonical full-suite command of three
/// ecosystems, each of which does collect every new test file. Warning on those is the
/// false-positive class this check exists to avoid.
///
/// `-p` / `--package` are deliberately absent: `cargo test -p foo` builds every test
/// target in `foo`, including one added this round, so it covers what it does not name.
fn command_is_selective(cmd: &str) -> bool {
    // `a && b` runs both, so the line is narrow only if *every* stage is. Judging one
    // stage is wrong in both directions: `cd repo && cargo test --test foo` would be read
    // as the `cd`, and `cargo test --lib && cargo test --tests` as the `--lib` while
    // `--tests` builds every integration target. Pipeline tails and redirect targets are
    // dropped rather than judged.
    let stages: Vec<&str> = cmd
        .split("&&")
        .map(|s| s.split(['|', '>', ';']).next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
        // A `cd repo &&` / `source .venv/bin/activate &&` prefix is setup, not a test
        // invocation, and judging it would read the whole line as broad.
        .filter(|s| !is_shell_setup(s))
        .collect();
    !stages.is_empty() && stages.iter().all(|s| stage_is_selective(s))
}

fn is_shell_setup(stage: &str) -> bool {
    const SETUP: [&str; 6] = ["cd", "export", "source", ".", "set", "unset"];
    stage
        .split_whitespace()
        .next()
        .is_some_and(|head| SETUP.contains(&head))
}

fn stage_is_selective(stage: &str) -> bool {
    // Both spellings: `--test foo` and `--test=foo`.
    const NARROWING: [&str; 6] = ["--test ", "--test=", "--bin ", "--bin=", "--lib", "::"];
    if NARROWING.iter().any(|f| stage.contains(f)) {
        return true;
    }
    // A named test *source file* is selective; a directory, a glob, or a path handed to a
    // flag is not. The first token is the runner, not a target.
    let mut prev_is_flag = false;
    for tok in stage.split_whitespace().skip(1) {
        let is_flag = tok.starts_with('-');
        let candidate = !is_flag && !prev_is_flag && !tok.ends_with('/');
        prev_is_flag = is_flag;
        if candidate && is_test_path(tok) && is_test_source(tok) {
            return true;
        }
    }
    false
}

/// A test file that is its own target, i.e. one a suite command could name.
///
/// Cargo makes an integration target of `tests/*.rs` only at the top level: `tests/
/// common/mod.rs` is a helper compiled into the others and can never be named by
/// `--test`. Everything nested elsewhere is judged on the source-extension check alone.
fn is_test_target(path: &str) -> bool {
    if !is_test_source(path) {
        return false;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if file == "mod.rs" {
        return false;
    }
    match (path.find("tests/"), path.ends_with(".rs")) {
        (Some(i), true) => !path[i + "tests/".len()..].contains('/'),
        _ => true,
    }
}

/// A file a test runner could compile or collect. The extension allowlist is what keeps
/// `pytest -c tests/pytest.ini` and an added `tests/data/golden.json` from reading as
/// test targets.
fn is_test_source(tok: &str) -> bool {
    const SOURCE_EXT: [&str; 11] = [
        "rs", "py", "go", "ts", "tsx", "js", "jsx", "rb", "java", "kt", "swift",
    ];
    let file = tok.rsplit('/').next().unwrap_or(tok);
    file.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && SOURCE_EXT.contains(&ext))
}

fn is_test_path(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|c| c == "tests" || c == "test" || c == "__tests__")
        || file.starts_with("test_")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
}

/// A command covers a file when it names the path or the file stem (cargo's `--test foo`
/// for `tests/foo.rs`).
fn command_names(cmd: &str, path: &str) -> bool {
    if cmd.contains(path) {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    !stem.is_empty()
        && cmd
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|t| t == stem)
}

fn suite_guidance(outcome: SuiteOutcome) -> String {
    let header = "## Suite channel (do not re-run full suites)\n\
         A dedicated channel runs the full suite — spar's own configured commands, or a cheap tester slot; its output is the `## Suite report` section above.\n\n";
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
    worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
    cfg.save_snapshot(paths, &state.id)?;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    state.big = opts.big;
    state.max_fix_rounds = 3;
    state.max_rounds = opts.max_rounds.unwrap_or(cfg.rounds.max);
    let n = cfg.max_agents.max(3) as usize;
    let roles: Vec<&str> = std::iter::once(SlotRole::Implementer.as_config_key())
        .chain(std::iter::repeat(SlotRole::Reviewer.as_config_key()))
        .take(n)
        .collect();
    let requested = opts.resolve_fleet(n, &roles, paths, cfg, &state.id)?;
    state.providers = providers::pick_providers(&requested, n, Some(&requested), dry);
    // Gate the positional fleet in place — never compact it (see the sibling entry point).
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
    prepare_implement_slots(&mut state, Some(&requested), dry, cfg, paths, false)?;

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
    execute_loop(&mut state, paths, cfg, opts.accept_contract)?;
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
        let _ = crate::worktree::cleanup_run(state, false);
    }
    Ok(())
}

/// True once `spar stop` has dropped the `stopped` marker for this run.
pub fn should_stop(paths: &SparPaths, run_id: &str) -> bool {
    crate::markers::marker_exists(paths, run_id, "stopped") || crate::process::shutdown_requested()
}

/// Halt without dispatching or touching worktrees; the run stays resumable.
fn stop_now(state: &mut RunState, paths: &SparPaths) -> Result<()> {
    state.set_phase(Phase::Stopped);
    state.save(paths)?;
    Ok(())
}

/// The acceptance contract as it stood when the round loop was entered. `body` is the
/// bytes read from disk, never the prompt copy: the overlay note appended to the
/// prompt below must not be readable as a criterion (defect 2, DECISIONS O43).
pub struct FrozenContract {
    pub body: String,
    pub criteria: Vec<String>,
    pub fingerprint: String,
}

impl FrozenContract {
    pub fn freeze(body: &str) -> Self {
        let criteria = review_result::parse_contract_criteria(body);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        Self {
            body: body.to_string(),
            criteria,
            fingerprint: format!("{:x}", hasher.finish()),
        }
    }

    /// Compares against the frozen bytes, not the prompt copy the round mutates.
    pub fn drifted(&self, on_disk: &str) -> bool {
        self.body != on_disk
    }
}

pub fn execute_loop(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    accept_contract: bool,
) -> Result<()> {
    // A resume inherits whatever the last orchestrator left behind; settle its slots
    // before this round stamps its own over them (O49). `Nobody`, asserted: the run lock
    // is held by this process and nothing has been dispatched yet in this call, so any
    // slot still `running` belongs to an orchestrator that is gone. Observing the lock
    // here would find *us* and skip the demotion.
    state.reconcile_and_save(
        paths,
        crate::state::RunOwner::Nobody,
        crate::state::ORPHANED_SLOT,
    )?;

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
    // Frozen once, from the bytes on disk, before the overlay note is appended below so
    // the note can never be read as a criterion. Every round for the life of this
    // `execute_loop` call judges reviewers against this same list; re-entering
    // `implement --run <id>` is what re-freezes against an amended file (DECISIONS O43).
    let frozen = FrozenContract::freeze(&test_contract_body);
    if frozen.criteria.is_empty() && review_result::mentions_criterion_id(&frozen.body) {
        let msg = "test-contract.md mentions AC-n ids but declares no criteria; \
                    the acceptance gate is vacuous for this run until a declaration is added"
            .to_string();
        eprintln!("warning: {msg}");
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    }
    let previous_fingerprint = state.contract_fingerprint.clone();
    let moved = previous_fingerprint
        .as_ref()
        .is_some_and(|prev| *prev != frozen.fingerprint);
    // O43 made re-freezing a *deliberate* human act — stop, edit, re-run — and the round
    // ceiling (O52) turned it into a routine one that spar itself prompts for. So a
    // re-entry may not adopt a contract spar watched move under the slot the contract
    // bounds: the implementer can write to `artifacts/`, and "delete the criterion you
    // cannot pass, then let the ceiling invite the operator to re-freeze it" is a clean
    // path past the acceptance gate. `contract_modified` is exactly the flag that says
    // spar saw it happen, so it is the flag that closes the door.
    if moved && state.contract_modified && !accept_contract {
        bail!(
            "test-contract.md changed while the previous round was running, and this \
             re-entry would adopt it: {} criteria now ({}), was fingerprint {}.\n\
             The implementer can write to that file, so spar will not re-freeze it for \
             you. Diff it against what you approved, then either revert it or re-run \
             with --accept-contract.",
            frozen.criteria.len(),
            frozen.fingerprint,
            previous_fingerprint.as_deref().unwrap_or("(none)")
        );
    }
    state.contract_fingerprint = Some(frozen.fingerprint.clone());
    state.contract_modified = false;
    if moved {
        let msg = format!(
            "contract re-frozen: {} criteria (fingerprint {}, was {}){}",
            frozen.criteria.len(),
            frozen.fingerprint,
            previous_fingerprint.as_deref().unwrap_or("(none)"),
            if accept_contract {
                " — adopted a drifted contract via --accept-contract"
            } else {
                ""
            }
        );
        // Loud, like the adjacent vacuous-contract warning: a re-freeze changes what the
        // ship gate enforces, and one line in events.jsonl is not somewhere anyone looks.
        eprintln!("warning: {msg}");
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    }
    state.save(paths)?;
    let mut test_contract_body = test_contract_body;

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
        match worktree::apply_spec_tests_to_impl(state, &author, &impl_cwd) {
            Err(e) => {
                return fail(
                    state,
                    paths,
                    anyhow::anyhow!("failed to apply acceptance tests from {author}: {e}"),
                );
            }
            // Said out loud on every channel: the overlay carries git-visible files only,
            // so a fixture the author wrote into an ignored path stays behind. Silent, that
            // surfaces as an acceptance test failing at runtime for no visible reason —
            // and the implementer is told it may weaken a test if it documents why.
            Ok(overlay) if !overlay.ignored.is_empty() => {
                let note = format!(
                    "acceptance tests copied WITHOUT {} git-ignored path(s) ({}); \
                     they remain in {}",
                    overlay.ignored.len(),
                    overlay.ignored.join(", "),
                    overlay.author_path.display()
                );
                eprintln!("note: {note}");
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(note.clone()),
                );
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    "orchestrator",
                    note,
                    state.message_budget,
                );
                test_contract_body.push_str(&format!(
                    "\n## Not copied (git-ignored in the test-author worktree)\n\
                     The acceptance tests above were copied from `{}`, but git ignores these \
                     paths so they did **not** come with them:\n{}\n\n\
                     If a test needs one of them, copy it across yourself from that worktree. \
                     Do **not** copy build output or dependency directories (`target/`, \
                     `node_modules/` and the like) — those are ignored on purpose and \
                     rebuilding is cheaper than copying them.\n",
                    overlay.author_path.display(),
                    overlay
                        .ignored
                        .iter()
                        .map(|p| format!("- `{p}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            Ok(_) => {}
        }
    }

    // What the previous round's review panel rejected. Recovered from the artifacts on
    // disk rather than carried in memory: the round after a gate — the one a human just
    // paid for — is a fresh process, and an in-memory list would leave it running blind
    // on the most expensive round of the run. Nothing in the implementer's prompt tells
    // it to go read `review-*.md` on its own.
    let mut blockers: Vec<String> = blockers_from_disk(state, paths, &frozen.criteria, cfg);

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

        let brief = take_carry_forward(paths, &state.id, &impl_slot.id);
        let mut extra = HashMap::new();
        extra.insert("plan_body".into(), plan_body.clone());
        extra.insert("test_contract_body".into(), test_contract_body.clone());
        extra.insert("amendment_section".into(), amendment_section.clone());
        extra.insert(
            "carry_forward_section".into(),
            carry_forward_section(
                state.round,
                &blockers,
                brief.as_deref(),
                cfg.rounds.carry_forward_chars,
            ),
        );
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
            if cfg.suite.is_builtin() {
                state.set_phase(Phase::Suite);
                state.save(paths)?;
                let report = if state.dry_run {
                    crate::suite::dry(&cfg.suite.command)
                } else {
                    let run_id = state.id.clone();
                    crate::suite::run(&crate::suite::Options {
                        cwd: &review_cwd,
                        commands: &cfg.suite.command,
                        log_path: &paths.artifact(&state.id, "suite.log"),
                        // The ceiling, not the soft number: `suite.timeout_secs` is the
                        // tester slot's nudge threshold and its kill sits at
                        // `hard_ceiling_multiple` past it (O50/O51). There is nothing to
                        // nudge here, so the knob keeps its meaning by running to the
                        // same wall.
                        budget: executor::hard_ceiling_for_role(cfg, SlotRole::Tester),
                        // These commands compile and run code a model just wrote, so they
                        // get the run's confinement like every other spawn in the tree.
                        isolation: state.isolation,
                        on_pid: &|pid| match pid {
                            Some(pid) => {
                                let _ = crate::markers::write_pid(
                                    paths,
                                    &run_id,
                                    crate::state::BUILTIN_SUITE_PID_ID,
                                    crate::process::PidToken::capture(pid),
                                );
                            }
                            None => crate::markers::clear_pid(
                                paths,
                                &run_id,
                                crate::state::BUILTIN_SUITE_PID_ID,
                            ),
                        },
                        stop: &|| should_stop(paths, &run_id),
                    })
                };
                suite_outcome = report.outcome;
                suite_body = report.body;
                std::fs::write(paths.artifact(&state.id, "suite.md"), &suite_body)?;
                let msg = format!(
                    "suite channel {} (built-in, {} command(s))",
                    match suite_outcome {
                        SuiteOutcome::Pass => "green",
                        SuiteOutcome::Fail => "red",
                        SuiteOutcome::Inconclusive => "inconclusive",
                    },
                    report.runs.len()
                );
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(msg.clone()),
                );
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    "orchestrator",
                    msg,
                    state.message_budget,
                );
            } else if let Some(tester) = tester {
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

            let unreferenced =
                unreferenced_test_files(&review_cwd, state.base_commit.as_deref(), &suite_body);
            if !unreferenced.is_empty() {
                let note = format!(
                    "suite gate does not reference {} added test file(s): {}",
                    unreferenced.len(),
                    unreferenced.join(", ")
                );
                let _ = crate::events::append(
                    paths,
                    &state.id,
                    &crate::events::Event::info(note.clone()),
                );
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    "orchestrator",
                    note.clone(),
                    state.message_budget,
                );
                // Into the report the reviewers read: the gate itself cannot be trusted to
                // notice a target it never compiled, but a reviewer can. Written to the
                // artifact too, not just the prompt — `suite.md` is what a human opens.
                let warning = format!(
                    "\n## Coverage warning (orchestrator)\nThe suite commands above select \
                     specific targets and name none of these added test files:\n{}\n\nTreat the \
                     suite result as not covering them.\n",
                    unreferenced
                        .iter()
                        .map(|f| format!("- {f}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                suite_body.push_str(&warning);
                let suite_path = paths.artifact(&state.id, "suite.md");
                if suite_path.is_file() {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&suite_path) {
                        let _ = f.write_all(warning.as_bytes());
                    }
                }
            }
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

        // Detected from disk, against the frozen body, never against `test_contract_body`
        // (which carries the overlay note appended above). Loud, not blocking: the gate
        // still judges the frozen criteria, and spar never rewrites the artifact. A read
        // error (the file removed mid-round) is the loudest possible tamper and must not
        // go silent just because there is nothing to diff against; it is only skipped
        // when the freeze itself had no real criteria (`[spec] enabled = false`), since
        // there is nothing an operator needs to be told about a placeholder contract.
        let mut contract_drift_note = String::new();
        let drift_reason =
            match std::fs::read_to_string(paths.artifact(&state.id, "test-contract.md")) {
                Ok(on_disk) if frozen.drifted(&on_disk) => Some("changed".to_string()),
                Ok(_) => None,
                Err(_) if !frozen.criteria.is_empty() => Some("could not be re-read".to_string()),
                Err(_) => None,
            };
        if let Some(reason) = drift_reason {
            state.contract_modified = true;
            let msg = format!(
                "test-contract.md {reason} after the freeze (write window held by \
                 implementer slot `{}`); the acceptance gate still judges the frozen \
                 contract",
                impl_slot.id
            );
            eprintln!("warning: {msg}");
            let _ =
                crate::events::append(paths, &state.id, &crate::events::Event::info(msg.clone()));
            let _ = crate::bus::broadcast(
                paths,
                Some(&state.id),
                "orchestrator",
                msg.clone(),
                state.message_budget,
            );
            contract_drift_note = format!("\n**Contract drift detected.** {msg}\n");
            state.save(paths)?;
        }

        let mut any_request_changes = suite_channel_active && suite_blocks_ship(suite_outcome);
        blockers.clear();
        if any_request_changes {
            blockers.push(format!(
                "suite channel {}: see {}",
                match suite_outcome {
                    SuiteOutcome::Fail => "red",
                    _ => "inconclusive",
                },
                paths.artifact(&state.id, "suite.md").display()
            ));
        }
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
            extra.insert("contract_drift_note".into(), contract_drift_note.clone());
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
                blockers.push(format!(
                    "review slot `{}` failed or produced no review",
                    rev.id
                ));
                if missing_or_empty {
                    let acceptance = if frozen.criteria.is_empty() {
                        String::new()
                    } else {
                        let lines: Vec<String> = frozen
                            .criteria
                            .iter()
                            .map(|id| format!("{id}: unverified — reviewer produced no review"))
                            .collect();
                        format!("## Acceptance\n{}\n\n", lines.join("\n"))
                    };
                    let _ = std::fs::write(
                        &review_path,
                        format!(
                            "## Verdict\nrequest_changes\n\n{acceptance}## Findings\n- severity: major — review slot `{}` failed or produced no artifact\n",
                            rev.id
                        ),
                    );
                }
            } else if let Some(text) = review_text {
                // Fail closed: only an anchored `## Verdict` / approve clears the gate,
                // and every contract criterion must be reported as passing (O19/O20).
                let res = review_result::parse_review(&text);
                if !res.approves() {
                    any_request_changes = true;
                    blockers.push(format!(
                        "review `{}` requested changes: {}",
                        rev.id,
                        review_path.display()
                    ));
                }
                if acceptance_blocks_ship(&frozen.criteria, &res, cfg) {
                    any_request_changes = true;
                    let reasons = acceptance_block_reasons(&frozen.criteria, &res, cfg);
                    // Into the next round's prompt, not only onto the bus: without this
                    // the fix round's implementer prompt is byte-identical to round 1's
                    // and never learns which criterion it failed.
                    for r in &reasons {
                        blockers.push(format!("acceptance (review `{}`): {r}", rev.id));
                    }
                    let _ = crate::bus::broadcast(
                        paths,
                        Some(&state.id),
                        "orchestrator",
                        format!(
                            "acceptance gate blocked ship (review {}): {}",
                            rev.id,
                            reasons.join("; ")
                        ),
                        state.message_budget,
                    );
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
            // Stuck policy: rotate implementer → widen reviewers → escalate. Resolved
            // *before* the round ceiling, so `stuck` (exit 3, "this cannot be fixed")
            // always beats the ceiling gate (exit 2, "this is costing too much"). The
            // other order let a run whose ladder was already exhausted present as a
            // question for the operator instead of a verdict.
            if !state.rotated_implementer && try_rotate_implementer(state, paths, cfg)? {
                state.rotated_implementer = true;
                state.fix_rounds = 0;
            } else if !state.widened_reviewers
                && try_widen_reviewers(state, paths, &review_cwd, cfg)?
            {
                state.widened_reviewers = true;
                state.fix_rounds = 0;
            } else {
                state.set_phase(Phase::Stuck);
                state.error = Some("fix rounds exhausted; escalated".into());
                state.save(paths)?;
                write_stuck(paths, &state.id)?;
                return Ok(());
            }
        }
        // Every path that reaches here is about to buy another cold re-dispatch, the
        // ladder's own retries included, so the ceiling bounds all of them.
        if state.round_ceiling_reached() {
            return escalate_round_ceiling(state, paths);
        }
        // A fix pass re-dispatches the implementer and the panel, so it is a round of
        // work in its own right (O45) — `fix_rounds` bounds the loop, `round` counts
        // what happened on the run.
        state.begin_round();
        state.set_phase(Phase::Fix);
        state.save(paths)?;
    }
}

/// Change implementer **provider** only; keep stable slot id and worktree.
fn try_rotate_implementer(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<bool> {
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
    // Candidate order: [roles].implementer, then [providers].order, then the live fleet.
    let next = cfg
        .roles
        .implementer
        .iter()
        .map(|s| s.as_str())
        .chain(cfg.providers.order.iter().map(|s| s.as_str()))
        .chain(state.providers.iter().map(|s| s.as_str()))
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
        set_slot_provider(s, next);
        s.status = SlotStatus::Pending;
        s.error = None;
    }
    state.save(paths)?;
    Ok(true)
}

/// Point a slot at a different provider, applying the same `provider` / `model` split
/// `init_slot_model` does: `provider` stays model-free (it keys the quota bucket and slot
/// naming) and any `@model` moves to `model`.
///
/// Assigning the raw ref instead leaves the *previous* provider's model in place, so the
/// new CLI is handed a `--model` belonging to the old one and `status` reports a
/// `provider@model` pair that does not exist.
fn set_slot_provider(slot: &mut SlotState, provider: String) {
    match crate::provider_ref::ProviderRef::parse(&provider) {
        Ok(pref) => {
            slot.provider = pref.storage_key();
            slot.model = pref.model;
        }
        Err(_) => {
            slot.provider = provider;
            slot.model = None;
        }
    }
}

/// Add an extra adversarial reviewer from a provider not already reviewing.
fn try_widen_reviewers(
    state: &mut RunState,
    paths: &SparPaths,
    review_cwd: &std::path::Path,
    cfg: &Config,
) -> Result<bool> {
    let existing: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Reviewer)
        .map(|s| s.provider.clone())
        .collect();
    // Draw the next reviewer from [roles].reviewer, then [providers].order, then the fleet.
    let candidate = cfg
        .roles
        .reviewer
        .iter()
        .cloned()
        .chain(cfg.providers.order.iter().cloned())
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
    let id = format!("review-{}-wide", sanitize_slot(&prov));
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
    let next = cfg
        .roles
        .reviewer
        .iter()
        .find(|p| **p != cur)
        .cloned()
        .or_else(|| state.providers.iter().find(|p| **p != cur).cloned())
        .or_else(|| cfg.providers.order.iter().find(|p| **p != cur).cloned());
    let Some(next) = next else {
        return Ok(false);
    };
    if let Some(s) = state.slot_mut(rev_id) {
        set_slot_provider(s, next);
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

/// Park the run at the round-ceiling gate instead of buying another cold re-dispatch.
///
/// Same idiom as the fix-round exhaustion path below it — set an error, write
/// `escalation.md`, stop — with one deliberate difference: this is a **gate** (exit 2),
/// not `Stuck` (exit 3). Nothing is broken; the run has simply spent the re-dispatch
/// budget it was allowed to spend on its own, and only a human can authorise more.
fn escalate_round_ceiling(state: &mut RunState, paths: &SparPaths) -> Result<()> {
    // Deliberately does not offer `ship`: `confirm_ship` accepts only the ship and
    // winner gates, so from here it exits 1.
    let msg = format!(
        "round ceiling reached: round {} of a maximum {}. Each further round is a cold \
         re-dispatch. Buy more with `spar implement --run {} --max-rounds {}`, or \
         `spar stop {}` and take it over by hand.",
        state.round,
        state.max_rounds,
        state.id,
        state.max_rounds + 2,
        state.id
    );
    state.set_phase(Phase::AwaitingRoundExtension);
    state.error = Some(msg.clone());
    state.save(paths)?;
    // `print_run_human` shows the phase but not `error`, so without this an operator
    // running implement interactively sees a phase name and no way out of it.
    eprintln!("{msg}");
    std::fs::write(
        paths.artifact(&state.id, "escalation.md"),
        format!(
            "# Escalation: round ceiling\n\n{msg}\n\nFix rounds this leg: {}\nSuite: {:?}\n",
            state.fix_rounds, state.suite_outcome
        ),
    )?;
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg.clone()));
    let _ = crate::bus::broadcast(
        paths,
        Some(&state.id),
        "orchestrator",
        msg,
        state.message_budget,
    );
    Ok(())
}

/// Rebuild the previous round's blocker list from the review artifacts on disk.
///
/// Seeds the round loop on entry so a re-dispatch after *any* pause — the round-ceiling
/// gate, a stop, a quota bounce, a crashed orchestrator — starts knowing which `AC-n`
/// failed. Silent when the reviews approved: a stale approval is not a blocker, and the
/// live round overwrites this list at its own review step anyway.
fn blockers_from_disk(
    state: &RunState,
    paths: &SparPaths,
    criteria: &[String],
    cfg: &Config,
) -> Vec<String> {
    let mut out = Vec::new();
    if matches!(state.suite_outcome, Some(SuiteOutcome::Fail)) {
        out.push(format!(
            "suite channel red last round: see {}",
            paths.artifact(&state.id, "suite.md").display()
        ));
    }
    for rev in state.slots.iter().filter(|s| s.role == SlotRole::Reviewer) {
        let path = paths.artifact(&state.id, &format!("review-{}.md", rev.id));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let res = review_result::parse_review(&text);
        if !res.approves() {
            out.push(format!(
                "review `{}` requested changes: {}",
                rev.id,
                path.display()
            ));
        }
        for r in acceptance_block_reasons(criteria, &res, cfg) {
            out.push(format!("acceptance (review `{}`): {r}", rev.id));
        }
    }
    out
}

/// Read and consume the previous round's carry-forward brief.
///
/// **Consumed on read.** A round whose implementer died without writing one gets
/// nothing, rather than a brief describing a worktree two rounds stale — which reads as
/// "what I just tried" and is worse than silence.
///
/// Per **slot**, not per run: `artifacts_dir` is shared and arena spawns N concurrent
/// implementers off one template, so an unscoped name would have them overwriting each
/// other. Rounds re-dispatch the same slot id (rotation changes the provider and keeps
/// the id and the worktree), so a slot always reads back its own last round.
/// The brief's filename. Shared with the nudge watcher, which must name it alongside the
/// summary: a nudge that names only the summary gets a slot that lands it and stops, and
/// the next round then finds no brief in exactly the case the brief exists for.
pub fn carry_forward_name(slot_id: &str) -> String {
    format!("carry-forward-{slot_id}.md")
}

fn take_carry_forward(paths: &SparPaths, run_id: &str, slot_id: &str) -> Option<String> {
    let path = paths.artifact(run_id, &carry_forward_name(slot_id));
    let body = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let body = body.trim().to_string();
    (!body.is_empty()).then_some(body)
}

/// The `{{carry_forward_section}}` seeded into the next round's implementer.
///
/// Blockers first, deliberately: they are the machine-known half (which `AC-n` failed and
/// why, whether the suite went red) and the truncation eats the tail, so a slot that
/// wrote a 40k-character essay cannot squeeze them out. The whole section is capped —
/// carrying more every round is the quadratic problem this exists to avoid.
fn carry_forward_section(
    round: u32,
    blockers: &[String],
    brief: Option<&str>,
    max_chars: usize,
) -> String {
    if blockers.is_empty() && brief.is_none() {
        return String::new();
    }
    let mut out = format!(
        "## Carry-forward (you already ran on this worktree — this is round {round})\n\
         Context from the previous round, not a verdict. It is here so you do not spend \
         this round re-deriving what the last one already learned. The acceptance gate \
         never reads it.\n\n"
    );
    if !blockers.is_empty() {
        out.push_str("### What blocked the ship last round\n");
        for b in blockers {
            // Clamped per bullet, not just in aggregate. A reviewer that pastes a stack
            // trace as its evidence produces one line longer than the whole budget, and
            // the line-boundary truncation below would drop it whole — rendering an
            // empty heading, which reads as "nothing blocked".
            out.push_str(&format!("- {}\n", clamp_line(b, max_chars / 4)));
        }
        out.push('\n');
    }
    if let Some(brief) = brief {
        out.push_str("### The previous round's own notes\n");
        out.push_str(brief);
        out.push('\n');
    }
    truncate_at_line(&out, max_chars)
}

/// Cut to `max` characters on a line boundary, saying so where the cut happened.
/// A silent truncation reads as an agent that stopped mid-sentence for its own reasons.
fn truncate_at_line(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let head = match cut.rfind('\n') {
        Some(i) if i > 0 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{head}\n\n_(carry-forward truncated by spar at {max} characters.)_\n")
}

/// Clamp one line so no single bullet can eat the whole section budget.
fn clamp_line(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let head: String = one_line.chars().take(max.saturating_sub(3)).collect();
    format!("{head}...")
}

fn write_stuck(paths: &SparPaths, run_id: &str) -> Result<()> {
    std::fs::write(
        paths.artifact(run_id, "escalation.md"),
        "# Escalation\n\nStuck policy exhausted. Human intervention required.\n",
    )?;
    Ok(())
}

fn finish_out(state: &RunState, json: bool) -> Result<()> {
    reclaim_own_cache(state, json);
    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
    }
    Ok(())
}

/// Drop this run's build output once its orchestrator is done with it.
///
/// Scoped to the run's **own** worktrees, at the moment its own orchestrator concludes —
/// not a project-wide sweep, which is `spar reclaim --all` and stays explicit. Nothing here
/// is unrecoverable: the worktree, its branch, its commits and any uncommitted changes all
/// survive, so this asks no permission and needs no evidence. It is the largest single
/// source of disk on the box (457 GB of 587 GB measured), and a *stopped* run's target dir
/// was the biggest object on the machine.
/// Phases the *automatic* reclaim may take: finished, and nothing can resume them.
///
/// Narrower than `is_terminal()` on purpose. `Quota`, `Stuck` and `Failed` are terminal
/// yet are exactly what `spar implement --run <id>` exists to pick up — a run that paused
/// because a provider bucket ran dry is meant to be resumed when it refills. Deleting
/// `node_modules` there is not free: regenerating it needs the network and an agent that
/// knows to reinstall, and the resumed implementer just meets a broken suite. Those stay
/// for the explicit `spar reclaim`, which is the operator's call.
fn auto_reclaimable(phase: Phase) -> bool {
    matches!(phase, Phase::Done | Phase::PlanRejected | Phase::Escalated)
}

fn reclaim_own_cache(state: &RunState, json: bool) {
    if state.dry_run
        || !auto_reclaimable(state.phase)
        || !crate::config::Config::for_run(&SparPaths::new(&state.project_root), &state.id)
            .map(|c| c.auto_reclaim)
            .unwrap_or(true)
    {
        return;
    }
    let reap = worktree::reap_build_cache(state, &worktree::LiveCwds::snapshot());
    if reap.freed_bytes > 0 && !json {
        eprintln!(
            "reclaimed {:.1} GB of build cache from this run's worktrees",
            reap.freed_bytes as f64 / 1024.0 / 1024.0 / 1024.0
        );
    }
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
            // A daemon/`continue` resume is nobody's deliberate amendment, so it never
            // adopts a contract that moved under the previous round.
            execute_loop(&mut state, paths, cfg, false)?;
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
        acceptance_block_reasons, acceptance_blocks_ship, auto_reclaimable, command_is_selective,
        command_names, derive_suite_outcome, is_test_path, is_test_target, should_stop,
        suite_blocks_ship, suite_commands, suite_guidance, Phase, SuiteOutcome,
    };
    use crate::config::Config;
    use crate::paths::SparPaths;
    use crate::workflow::review_result::parse_review;
    use tempfile::tempdir;

    fn criteria(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn relaxed() -> Config {
        let mut cfg = Config::default();
        cfg.review.require_all_criteria = false;
        cfg
    }

    #[test]
    fn acceptance_empty_criteria_never_blocks() {
        let res = parse_review("");
        assert!(!acceptance_blocks_ship(&[], &res, &Config::default()));
    }

    #[test]
    fn acceptance_fail_blocks() {
        let res = parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: fail — broken\n");
        assert!(acceptance_blocks_ship(
            &criteria(&["AC-1"]),
            &res,
            &Config::default()
        ));
    }

    #[test]
    fn acceptance_missing_criterion_blocks() {
        let res = parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: pass — ok\n");
        let c = criteria(&["AC-1", "AC-2"]);
        assert!(acceptance_blocks_ship(&c, &res, &Config::default()));
        let reasons = acceptance_block_reasons(&c, &res, &Config::default());
        assert!(reasons.iter().any(|r| r.contains("AC-2")), "{reasons:?}");
        // One entry per criterion: a joined line is dropped whole by the carry-forward's
        // line-boundary truncation, leaving a heading that reads as "nothing blocked".
        assert!(
            reasons.iter().all(|r| !r.contains(';')),
            "reasons must stay per-criterion: {reasons:?}"
        );
        // Unmentioned criteria block even when unverified is tolerated.
        assert!(acceptance_blocks_ship(&c, &res, &relaxed()));
    }

    #[test]
    fn acceptance_unverified_blocks_by_default() {
        let res =
            parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: unverified — no time\n");
        assert!(acceptance_blocks_ship(
            &criteria(&["AC-1"]),
            &res,
            &Config::default()
        ));
    }

    #[test]
    fn acceptance_unverified_allowed_when_relaxed() {
        let res = parse_review(
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass — ok\nAC-2: unverified — no time\n",
        );
        assert!(!acceptance_blocks_ship(
            &criteria(&["AC-1", "AC-2"]),
            &res,
            &relaxed()
        ));
    }

    #[test]
    fn acceptance_all_pass_does_not_block() {
        let res =
            parse_review("## Verdict\napprove\n\n## Acceptance\nAC-1: pass — a\nAC-2: pass — b\n");
        assert!(!acceptance_blocks_ship(
            &criteria(&["AC-1", "AC-2"]),
            &res,
            &Config::default()
        ));
    }

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

    /// `suite.md` is what the reviewers and the operator read; `report.outcome` is what
    /// the gate holds. The built-in path sets the gate from exit codes and renders the
    /// body separately, so nothing but this test stops the two from drifting apart and
    /// telling a reviewer the suite passed on a run that blocked.
    #[test]
    fn a_builtin_report_reads_back_as_the_verdict_the_gate_holds() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("suite.log");
        let cases = [
            (vec!["true".to_string()], std::time::Duration::from_secs(30)),
            (
                vec!["exit 1".to_string()],
                std::time::Duration::from_secs(30),
            ),
            (
                vec!["sleep 30".to_string()],
                std::time::Duration::from_millis(300),
            ),
        ];
        for (cmds, budget) in cases {
            let rep = crate::suite::run(&crate::suite::Options {
                cwd: d.path(),
                commands: &cmds,
                log_path: &log,
                budget,
                isolation: crate::config::IsolationMode::Worktree,
                on_pid: &|_| {},
                stop: &|| false,
            });
            assert_eq!(
                derive_suite_outcome(true, Some(0), Some(&rep.body)),
                rep.outcome,
                "{}",
                rep.body
            );
        }
        let rep = crate::suite::dry(&["cargo test".to_string()]);
        assert_eq!(
            derive_suite_outcome(true, Some(0), Some(&rep.body)),
            rep.outcome
        );
    }

    /// The coverage check (O32) parses `## Commands` back out of `suite.md`. That is a
    /// contract between two modules with no compiler between them: a format change on
    /// either side silently disables the warning rather than breaking anything.
    #[test]
    fn the_coverage_check_can_still_read_a_builtin_command_list() {
        let body = crate::suite::dry(&[
            "cargo test --test foo".to_string(),
            "pytest tests/unit".to_string(),
        ])
        .body;
        assert_eq!(
            suite_commands(&body),
            vec!["cargo test --test foo", "pytest tests/unit"]
        );
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
        // Keyed on the verdict the rule assigns, not on its prose: the wording moved once
        // already when `suite.timeout_secs` stopped being a kill.
        let budget_rule = lower
            .lines()
            .find(|l| l.contains("`## result` = `inconclusive`"))
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
    fn reviewer_template_declares_acceptance_block() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("## Acceptance"),
            "reviewer output contract must declare the ## Acceptance section"
        );
        assert!(
            reviewer.contains("unverified"),
            "reviewer must be told the unverified status exists"
        );
    }

    /// The gate is model-authored, so a new test binary it forgets to name passes green
    /// without ever being compiled. Warn on that — but only when the commands actually
    /// select targets, or the warning fires on every `cargo test` and gets tuned out.
    #[test]
    fn coverage_warning_only_fires_on_a_selective_suite() {
        let selective = "## Commands\n- `cargo test --test existing` → exit 0\n";
        assert!(command_is_selective("cargo test --test existing"));
        assert_eq!(
            suite_commands(selective),
            vec!["cargo test --test existing"]
        );

        assert!(suite_commands("## Commands\n- `cargo test` → exit 0\n")
            .iter()
            .all(|c| !command_is_selective(c)));

        assert!(command_names(
            "cargo test --test existing",
            "tests/existing.rs"
        ));
        assert!(!command_names(
            "cargo test --test existing",
            "tests/brand_new.rs"
        ));
        // A stem that merely appears inside a longer word is not coverage.
        assert!(!command_names(
            "cargo test --test existing_more",
            "tests/existing.rs"
        ));
    }

    /// The canonical full-suite command of each ecosystem compiles or collects every new
    /// test file, so warning on one is a false claim that fires every run. This is the
    /// whole risk of the check, and the earlier "any token with a `/`" rule tripped on
    /// `pytest tests/`, `go test ./...` and `./scripts/test.sh` alike.
    #[test]
    fn full_suite_commands_are_never_selective() {
        for cmd in [
            "cargo test",
            "cargo test --workspace",
            "cargo test -p mycrate",
            "cargo nextest run -p spar",
            "pnpm test",
            "pytest",
            "pytest tests/",
            "go test ./...",
            "./scripts/test.sh",
            "cargo test 2>&1 | tee /tmp/suite.log",
            "cd /repo && cargo test",
            "source .venv/bin/activate && pytest",
        ] {
            assert!(!command_is_selective(cmd), "must not be selective: {cmd}");
        }
    }

    #[test]
    fn naming_specific_targets_is_selective() {
        for cmd in [
            "pytest tests/test_users.py",
            "cargo test --test existing",
            "cargo test --lib",
            "cargo test mymod::case",
            "go test ./pkg/thing_test.go",
            // Every stage narrow ⇒ the line is narrow.
            "cargo test --test a && cargo test --test b",
        ] {
            assert!(command_is_selective(cmd), "must be selective: {cmd}");
        }
    }

    /// `a && b` runs both. Judging the line off one stage was wrong in both directions:
    /// a narrow stage beside a full-suite stage is covered, and a `cd` prefix is not a
    /// verdict on the command behind it.
    #[test]
    fn every_stage_must_be_narrow() {
        // `--tests` builds every integration target, so the line covers new files.
        assert!(!command_is_selective(
            "cargo test --lib && cargo test --tests"
        ));
        // A `cd` prefix is setup, not a verdict on the command behind it.
        assert!(command_is_selective("cd repo && cargo test --test foo"));
        assert!(!command_is_selective("cd repo && cargo test"));
        assert!(!command_is_selective("cargo build && cargo test"));
        // Both spellings of the target flags.
        assert!(command_is_selective("cargo test --test=foo"));
        assert!(command_is_selective("cargo test --bin=cli"));
        // A path handed to a flag is not a target.
        assert!(!command_is_selective("pytest -c tests/pytest.ini"));
    }

    /// Only files a suite command could actually name are reportable. A helper module or
    /// a fixture is neither, and a false "not covered" claim is most expensive in the
    /// reviewers' suite report.
    #[test]
    fn only_real_test_targets_are_reportable() {
        assert!(is_test_target("tests/acceptance.rs"));
        assert!(is_test_target("tests/test_users.py"));
        assert!(is_test_target("pkg/thing_test.go"));
        assert!(!is_test_target("tests/common/mod.rs"));
        assert!(!is_test_target("tests/scenarios/plan.rs"));
        assert!(!is_test_target("tests/data/golden.json"));
    }

    #[test]
    fn test_paths_are_recognized_across_stacks() {
        assert!(is_test_path("tests/scenarios/plan.rs"));
        assert!(is_test_path("src/__tests__/thing.ts"));
        assert!(is_test_path("pkg/thing_test.go"));
        assert!(is_test_path("app/foo.spec.ts"));
        assert!(is_test_path("api/test_users.py"));
        assert!(!is_test_path("src/executor.rs"));
        assert!(!is_test_path("docs/testing.md"));
    }

    /// Automatic reclaim is for runs nothing can resume. A quota pause is meant to be
    /// picked back up when the bucket refills, and node_modules is not free to regenerate
    /// -- it needs the network and an agent that knows to reinstall.
    #[test]
    fn auto_reclaim_spares_runs_that_can_be_resumed() {
        for phase in [Phase::Done, Phase::PlanRejected, Phase::Escalated] {
            assert!(auto_reclaimable(phase), "{phase:?}");
        }
        for phase in [
            Phase::Quota,
            Phase::Stuck,
            Phase::Failed,
            Phase::Stopped,
            Phase::PlanApproved,
            Phase::AwaitingShipConfirm,
            Phase::Review,
        ] {
            assert!(!auto_reclaimable(phase), "{phase:?} is resumable or live");
        }
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

#[cfg(test)]
mod contract_freeze_tests {
    use super::FrozenContract;
    use crate::workflow::review_result::parse_contract_criteria;

    const BODY: &str = "## Scenarios\n- [ ] AC-1: a — verify: `x`\n- [ ] AC-2: b — verify: `x`\n";

    /// AC-8.
    #[test]
    fn freeze_captures_the_declared_criteria_and_the_bytes_on_disk() {
        let frozen = FrozenContract::freeze(BODY);
        assert_eq!(frozen.criteria, ["AC-1", "AC-2"]);
        assert_eq!(frozen.body, BODY);
        assert!(
            !frozen.fingerprint.is_empty(),
            "the fingerprint is what `status --json` reports; it cannot be blank"
        );
    }

    /// AC-8.
    #[test]
    fn fingerprint_is_stable_for_the_same_body_and_moves_with_it() {
        let a = FrozenContract::freeze(BODY);
        let b = FrozenContract::freeze(BODY);
        assert_eq!(a.fingerprint, b.fingerprint);
        let amended = format!("{BODY}- [ ] AC-3: c — verify: `x`\n");
        assert_ne!(a.fingerprint, FrozenContract::freeze(&amended).fingerprint);
    }

    /// AC-9.
    #[test]
    fn drift_is_measured_against_the_body_that_was_frozen() {
        let frozen = FrozenContract::freeze(BODY);
        assert!(!frozen.drifted(BODY));
        // Same length, one byte different: a fingerprint that only counted bytes would
        // miss a criterion being reworded in place.
        let same_len = BODY.replace("AC-2: b —", "AC-2: q —");
        assert_eq!(same_len.len(), BODY.len());
        assert!(frozen.drifted(&same_len));
        assert!(frozen.drifted(&format!("{BODY}- [ ] AC-3: added mid-run\n")));
    }

    /// AC-9, AC-10. The invariant that survived the round loop only by accident of
    /// ordering: the
    /// overlay note is appended to the *prompt copy*, and the note names git-ignored
    /// paths, so a file called `AC-99:fixture.json` reads as a criterion declaration.
    /// The frozen criteria must come from the bytes on disk, never from that copy.
    #[test]
    fn overlay_note_on_the_prompt_copy_cannot_reach_the_gate() {
        let frozen = FrozenContract::freeze(BODY);
        let mut prompt_copy = frozen.body.clone();
        prompt_copy.push_str(
            "\n## Not copied (git-ignored in the test-author worktree)\n- `AC-99:fixture.json`\n",
        );
        assert!(
            parse_contract_criteria(&prompt_copy).contains(&"AC-99".to_string()),
            "guard is vacuous unless the note is declaration-shaped"
        );
        assert_eq!(frozen.criteria, ["AC-1", "AC-2"]);
        assert!(!frozen.drifted(BODY), "the prompt copy is not the contract");
    }
}

#[cfg(test)]
mod carry_forward_tests {
    use super::*;

    fn brief_of(round: u32, blockers: &[String], brief: Option<&str>, max: usize) -> String {
        carry_forward_section(round, blockers, brief, max)
    }

    /// A first round has nothing to carry, and an empty section must render as nothing
    /// at all — an empty "## Carry-forward" heading is a prompt telling the model there
    /// was a previous round when there was not.
    #[test]
    fn nothing_to_carry_renders_nothing() {
        assert_eq!(brief_of(2, &[], None, 4000), "");
    }

    /// The whole point: the failed criterion and its evidence reach the next round's
    /// implementer, which before this only ever went to the bus.
    #[test]
    fn blockers_name_the_failed_criterion() {
        let blockers = vec!["acceptance (review `rev-1`): AC-2: fail — no test exists".into()];
        let out = brief_of(4, &blockers, None, 4000);
        assert!(out.contains("AC-2: fail — no test exists"), "{out}");
        assert!(out.contains("round 4"), "{out}");
    }

    /// The cap is on the whole section, and blockers lead, so a slot that wrote an essay
    /// cannot push the machine-known half out of the prompt.
    #[test]
    fn truncation_eats_the_slot_prose_not_the_blockers() {
        let blockers = vec!["acceptance (review `rev-1`): AC-2: fail — missing guard".into()];
        let essay = "padding line that goes on and on\n".repeat(400);
        assert!(
            essay.len() > 4000,
            "guard is vacuous unless the brief is long"
        );
        let out = brief_of(3, &blockers, Some(&essay), 1200);
        assert!(out.chars().count() <= 1400, "capped, got {}", out.len());
        assert!(out.contains("AC-2: fail — missing guard"), "{out}");
        assert!(
            out.contains("truncated by spar"),
            "a silent cut is a lie:\n{out}"
        );
    }

    /// Two rounds of the same size must produce the same size. A section that appended
    /// would recreate the quadratic context growth it exists to avoid.
    #[test]
    fn the_section_does_not_grow_with_the_round_number() {
        let blockers = vec!["acceptance (review `rev-1`): AC-1: fail — x".into()];
        let brief = "- touched src/a.rs: added the guard\n- rejected: a feature flag\n";
        let round_3 = brief_of(3, &blockers, Some(brief), 4000);
        let round_9 = brief_of(9, &blockers, Some(brief), 4000);
        assert_eq!(round_3.len(), round_9.len());
    }

    /// The measured defect: `acceptance_block_reasons` used to join every failing
    /// criterion into one line, real reviews produced 4k-10k of it, and a line-boundary
    /// truncation drops an over-cap line **whole** — leaving a bare "What blocked the
    /// ship last round" heading, which reads as "nothing blocked".
    #[test]
    fn one_giant_blocker_never_leaves_an_empty_heading() {
        let huge = format!(
            "acceptance (review `rev-1`): AC-2: fail — {}",
            "x".repeat(9000)
        );
        let out = brief_of(4, &[huge], None, 1000);
        assert!(
            out.contains("### What blocked the ship last round"),
            "{out}"
        );
        let after_heading = out
            .split_once("### What blocked the ship last round\n")
            .map(|(_, rest)| rest.trim().to_string())
            .unwrap_or_default();
        assert!(
            after_heading.starts_with("- acceptance"),
            "the heading must still carry its bullet:\n{out}"
        );
        assert!(after_heading.contains("AC-2: fail"), "{out}");
    }

    /// Many failing criteria stay separately visible instead of collapsing into one
    /// line that the cap then eats entirely.
    #[test]
    fn each_criterion_is_its_own_bullet() {
        let blockers: Vec<String> = (1..=6)
            .map(|n| format!("acceptance (review `rev-1`): AC-{n}: fail — evidence {n}"))
            .collect();
        let out = brief_of(3, &blockers, None, 4000);
        for n in 1..=6 {
            assert!(
                out.contains(&format!("AC-{n}: fail")),
                "AC-{n} missing:\n{out}"
            );
        }
    }

    #[test]
    fn clamp_line_collapses_and_caps() {
        assert_eq!(clamp_line("short", 40), "short");
        assert_eq!(clamp_line("a\nb", 40), "a b");
        let out = clamp_line(&"y".repeat(100), 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("..."), "{out}");
    }

    #[test]
    fn truncate_keeps_whole_lines() {
        let out = truncate_at_line("alpha\nbravo\ncharlie\n", 9);
        assert!(out.starts_with("alpha\n"), "{out}");
        assert!(!out.contains("charlie"), "{out}");
    }

    /// Consumed on read: a round whose implementer died writes nothing, and must inherit
    /// nothing, rather than a brief describing a worktree two rounds stale.
    #[test]
    fn carry_forward_is_consumed_on_read() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let f = paths.artifact("r1", "carry-forward-impl-cli-claude.md");
        std::fs::write(&f, "- touched src/a.rs\n").unwrap();

        let first = take_carry_forward(&paths, "r1", "impl-cli-claude");
        assert_eq!(first.as_deref(), Some("- touched src/a.rs"));
        assert!(!f.exists(), "the brief must not survive its own round");
        assert!(take_carry_forward(&paths, "r1", "impl-cli-claude").is_none());
    }

    /// The round bought at a gate is a fresh process with an empty `blockers` list, so
    /// the list is rebuilt from the review artifacts rather than carried in memory.
    #[test]
    fn blockers_are_recovered_from_the_review_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        std::fs::write(
            paths.artifact("r1", "review-rev-1.md"),
            "## Verdict\nrequest_changes\n\n## Acceptance\nAC-1: fail — no guard exists\n",
        )
        .unwrap();

        let out = blockers_from_disk(&state, &paths, &["AC-1".to_string()], &Config::default());
        assert!(
            out.iter()
                .any(|b| b.contains("AC-1: fail — no guard exists")),
            "{out:?}"
        );
        assert!(
            out.iter().any(|b| b.contains("requested changes")),
            "{out:?}"
        );
    }

    /// A stale *approval* is not a blocker; only the live round's review can clear or
    /// re-raise the gate.
    #[test]
    fn an_approving_review_seeds_no_blockers() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(crate::executor::init_slot(
            "rev-1",
            "cli:claude",
            SlotRole::Reviewer,
        ));
        std::fs::write(
            paths.artifact("r1", "review-rev-1.md"),
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass — done\n",
        )
        .unwrap();
        assert!(
            blockers_from_disk(&state, &paths, &["AC-1".to_string()], &Config::default())
                .is_empty()
        );
    }

    /// `artifacts_dir` is shared per run and arena spawns N implementers at once, so the
    /// brief has to be slot-scoped or they clobber each other.
    #[test]
    fn briefs_are_scoped_per_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        std::fs::write(paths.artifact("r1", "carry-forward-a.md"), "from a\n").unwrap();
        std::fs::write(paths.artifact("r1", "carry-forward-b.md"), "from b\n").unwrap();
        assert_eq!(
            take_carry_forward(&paths, "r1", "a").as_deref(),
            Some("from a")
        );
        assert_eq!(
            take_carry_forward(&paths, "r1", "b").as_deref(),
            Some("from b")
        );
    }
}
