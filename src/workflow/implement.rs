use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{
    CeilingPartialKind, CeilingPartialRound, FleetSeat, Phase, PoolOrigin, RunState, SeatSource,
    SlotRole, SlotState, SlotStatus, SuiteOutcome,
};
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
    if let Some(t) = &task {
        crate::workflow::validate_task(t).map_err(anyhow::Error::msg)?;
    }
    if let Some(id) = run_id {
        return run_from_approved(&id, task, opts, paths, cfg);
    }
    if let Some(plan_path) = plan {
        // A plan belongs to the run that produced it. Implementing it is that run
        // continuing, not a new one (O45) — this is the bypass that split biddesk's
        // work across two ids 35 times.
        // `--new` says this is separate work, so it wins over the trace.
        if let Some(id) = run_id_for_plan(&plan_path, paths).filter(|_| !new) {
            if opts.key.is_some() {
                bail!(
                    "--key names a new run; this plan belongs to run {id}, which keeps its own key"
                );
            }
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

/// Put a resumed run's interrupted slots back to `Pending`.
///
/// `spar stop` reconciles every slot it interrupts from `Running` to `Failed`. The
/// non-plan workflows rebuild their job list from *every* retained slot
/// (`review.rs`'s `state.slots.iter().map(..)`) and `executor::run_slot` flips each
/// one back to `Running`, so those slots are about to run — but
/// `daemon::run_demand` counts only `Pending`/`Running`, so the run reads as needing
/// nothing and `maybe_enqueue` admits it however full the bucket is. The cap is then
/// overrun the moment dispatch starts.
///
/// This is a rule, not a tidy-up: what the cap reserves has to be what is about to
/// run.
fn restore_interrupted_slots(state: &mut RunState) {
    for slot in &mut state.slots {
        if slot.status == SlotStatus::Failed {
            slot.status = SlotStatus::Pending;
        }
    }
}

/// `spar resume <id>`: dispatch on what the run actually is, rather than aliasing
/// `implement --run`. A gate is refused (a decision is waiting, and resume is not an
/// operator); a live owner is refused naming its pid; an abandoned in-flight run goes
/// to `continue_run` in the foreground, or (with `--detach`) through the same
/// `detach_implement` a fresh `implement --detach` uses, since `continue_run` already
/// dispatches on `state.workflow` regardless of which process calls it. A stopped
/// `Plan` run — the one workflow with a plan-approval gate — either refuses (never
/// approved) or falls through to `run_from_approved` (approved, or `Loop`, which has no
/// approval step at all). A stopped `Arena`/`Roles`/`Peer`/`Review` run has no approval
/// gate to check at all, so it is routed straight to `continue_run` instead: those
/// workflows are not `run_from_approved`'s business, since it only knows approved plans
/// and `Loop`, and `prepare_implement_slots` would otherwise rewrite the run's workflow
/// to `Loop`, silently changing what the run is.
pub fn resume(
    paths: &SparPaths,
    cfg: &Config,
    run_id: &str,
    detach: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if let Some(owner) = crate::runlock::RunLock::owner(paths, run_id) {
        if owner.alive() {
            return Err(crate::runlock::OrchestratorBusy {
                run_id: run_id.to_string(),
                owner_pid: owner.pid,
            }
            .into());
        }
    }
    if state.amend_in_progress.is_some() {
        let lock = crate::runlock::RunLock::acquire(paths, run_id)?;
        state = RunState::load(paths, run_id)?;
        crate::workflow::plan::recover_interrupted_amend(&mut state, paths)?;
        drop(lock);
    }
    if state.phase.is_gate() {
        bail!(
            "run {run_id} is waiting on a human ({:?}); resolve it with: {}",
            state.phase,
            crate::notify::next_command(&state)
        );
    }
    if matches!(state.phase, Phase::Done | Phase::PlanRejected) {
        bail!(
            "run {run_id} is finished ({:?}); nothing to resume. Start a new round with \
             `spar plan --run {run_id} -t \"…\"`.",
            state.phase
        );
    }
    // `spar stop` parks a plan run at `Stopped` before it was ever approved just as
    // readily as it parks an approved run. Resuming that straight into `run_from_approved`
    // would drive an unapproved plan through to ship — the same hazard `Phase::Quota`
    // already guards against there. Refuse here, by name, instead of falling through to
    // `run_from_approved`'s generic "plan is not approved" bail: resume's contract is that
    // it names the human command for every state it refuses.
    //
    // `Plan` is the *only* workflow with an approval gate, so it is the only workflow this
    // check applies to. `Loop` has none and resumes below via `run_from_approved`'s own
    // `Stopped`+`Loop` disjunct. `Arena`/`Roles`/`Peer`/`Review` have none either, and are
    // routed to their own continuation just below — never through `run_from_approved`,
    // which would treat them as unapproved plans and refuse, or worse, rewrite them.
    if state.phase == Phase::Stopped
        && state.workflow == crate::cli::WorkflowKind::Plan
        && !state.gates.plan_approved
    {
        bail!(
            "run {run_id} is a stopped plan run that was never approved; resuming it would \
             drive an unapproved plan through to ship. Replan it with \
             `spar plan --run {run_id} -t \"…\"`, or if a plan is already written and \
             awaiting a decision, `spar approve {run_id}`."
        );
    }
    if state.phase == Phase::Stopped
        && !matches!(
            state.workflow,
            crate::cli::WorkflowKind::Plan | crate::cli::WorkflowKind::Loop
        )
    {
        let lock = crate::runlock::RunLock::acquire(paths, run_id)?;
        let mut state = RunState::load(paths, run_id)?;
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
        restore_interrupted_slots(&mut state);
        state.save(paths)?;
        drop(lock);
        if detach {
            return detach_implement(&state, paths, json);
        }
        return continue_run(paths, cfg, run_id);
    }
    // Shipping is the operator's step, never a supervisor's (O70): a run that died
    // mid-ship goes back to the ship gate instead of re-running its rounds (O117).
    if state.phase == Phase::Shipping {
        return park_interrupted_ship(paths, run_id, json);
    }
    // An approved plan run has finished planning, so an in-flight park continues into
    // implementation through `run_from_approved` rather than into a second plan round
    // that would ask for the approval again.
    let approved_plan =
        state.workflow == crate::cli::WorkflowKind::Plan && state.gates.plan_approved;
    if !state.phase.is_waitable_stop() && !approved_plan {
        // `continue_run` has no detach knob of its own: it always runs to completion
        // in the foreground of whoever calls it. `--detach` here means the same thing
        // it means for `plan`/`implement`: spawn `__internal_continue` into a session
        // of its own and hand back once it holds the run's lock, rather than running
        // it in this process.
        if detach {
            return detach_implement(&state, paths, json);
        }
        return continue_run(paths, cfg, run_id);
    }
    // `run_from_approved` always re-decides `state.dry_run` from `opts.resolve_dry_run()`
    // (it is not sticky across `implement --run` re-entries, by design, so a test can
    // force a live run into stub mode). `resume` offers no `--dry-run` flag of its own —
    // resuming does not get to change what kind of run this is — so it carries the run's
    // own recorded flag through instead of silently defaulting to `false` and dispatching
    // a dry-run's later rounds against real providers and real git merges.
    let opts = CommonOpts {
        detach,
        json,
        dry_run: state.dry_run,
        ..CommonOpts::default()
    };
    run_from_approved(run_id, None, opts, paths, cfg)
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

/// How this dispatch re-entered an existing run. Two independent facts, together
/// because a pair of bare `bool` arguments at a call site says nothing about which
/// is which.
#[derive(Debug, Clone, Copy)]
struct Reentry {
    /// Lifted off the round ceiling (O52): the same escalation continuing, so the
    /// stuck ladder's rotate/widen budget is not reset.
    from_round_gate: bool,
    /// `--reload-config`: seats that already exist are re-resolved from the fresh
    /// config rather than keeping the provider they were minted with.
    reload_config: bool,
}

/// Re-resolve the provider and model of seats that already exist, from a config the
/// caller reloaded (`--reload-config`).
///
/// Matched by seat id, so a seat the new config no longer produces is left alone
/// rather than guessed at: dropping it is `--without`'s job, and this must not become
/// a second path that reaps slots.
///
/// **A `Running` seat is never re-pointed.** Its provider is the one that is mid-flight;
/// rewriting it would make the run's own record disagree with the process it is
/// waiting on, and the slot's pid marker is filed against what actually spawned.
fn repoint_existing_seats(state: &mut RunState, cfg: &Config) -> Result<()> {
    let pool = state.providers.clone();
    let seats = crate::workflow::roles_resolve::build_implement_seats(
        cfg,
        &pool,
        state.pool_origin,
        false,
        &|_| None,
    );
    for seat in seats {
        let Some(slot) = state.slots.iter_mut().find(|s| s.id == seat.seat) else {
            continue;
        };
        // Never touch the seat that is mid-flight: its provider is the one that
        // spawned, and its pid marker is filed against exactly that.
        if slot.status == SlotStatus::Running {
            continue;
        }
        // The operator moved this seat by hand (O136); the config never named its
        // provider, so it has nothing to say about its model either.
        if slot.source == Some(SeatSource::Reassigned) {
            continue;
        }
        // **Model only.** A reviewer seat's id is `review-{r}-{sanitized provider}`
        // (`roles_resolve::build_implement_seats`), so the id *encodes* the provider:
        // a matching id already means a matching provider, and writing
        // `seat.provider` over it can only ever make the id contradict its own
        // contents. The first cut of this did write it, and on a run whose reviewer
        // pins were re-stated it put `cli:claude` behind the seat named
        // `review-1-cli-codex` — silently collapsing a deliberate two-vendor panel to
        // one vendor (the exact thing feature 011 exists to keep exact) and then
        // blaming codex in the rate-limit error when claude's limit was the one hit.
        //
        // A genuinely *different* provider therefore resolves to a seat id that does
        // not exist yet, falls through the `find` above, and is left to the seat
        // machinery rather than smuggled in here. Changing who fills a seat is a
        // different operation from changing which model they use, and only the
        // second one is safe to do to a slot that already carries a worktree, a
        // branch and a round history.
        // Enforced, not asserted: a seat id that names one provider while the slot
        // carries another is how a two-vendor panel becomes one vendor without
        // anybody being told, and how a rate-limit error ends up blaming the wrong
        // provider. If this ever does not hold, the seat machinery and the id
        // formula have diverged, and refusing is the only safe answer — writing
        // either field would bake the disagreement in.
        if slot.provider != seat.provider {
            bail!(
                "seat `{}` resolves to {} but the run carries {}; the seat id encodes \
                 its provider, so these cannot both be right. Refusing to re-point it \
                 rather than leave a seat whose id contradicts what fills it.",
                seat.seat,
                seat.provider,
                slot.provider
            );
        }
        if slot.model == seat.model {
            continue;
        }
        slot.model = seat.model.clone();
        slot.source = Some(seat.source);
    }
    Ok(())
}

/// One `--reassign <role|seat>=<provider[@model]>`, checked before the run is touched.
#[derive(Debug, Clone)]
pub struct Reassignment {
    target: String,
    pin: crate::provider_ref::Pin,
}

/// Parse `--reassign` and refuse a provider the run could not dispatch anyway: unknown,
/// manually paused, or paused on a trusted quota signal (O106, O136). Read before the run
/// lock, so a typo costs nothing. The pause check keys the route the next dispatch will
/// stamp (this process's environment): a model-less codex seat reassigned onto a paused
/// ambient bucket is refused even when the run's stale record would key elsewhere.
pub fn parse_reassignments(
    raw: &[String],
    paths: &SparPaths,
    route: crate::providers::RecordedRoute<'_>,
) -> Result<Vec<Reassignment>> {
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let mut out = Vec::new();
    for r in raw {
        let Some((target, provider)) = r.split_once('=') else {
            bail!("--reassign {r:?}: expected <role|seat>=<provider[@model]>");
        };
        let target = target.trim();
        if target.is_empty() {
            bail!("--reassign {r:?}: name a role (implementer, reviewer, tester) or a seat id");
        }
        let pin = crate::provider_ref::Pin::parse(provider)
            .map_err(|e| anyhow::anyhow!("--reassign {r:?}: {e}"))?;
        let pref = crate::provider_ref::ProviderRef::parse(&pin.provider)?;
        if let Some(cli) = pref.cli_name() {
            if providers::adapter_named(cli).is_none() {
                bail!("--reassign {r:?}: no adapter named {cli}");
            }
        }
        let seat = crate::provider_ref::ProviderRef::for_seat(&pin.provider, pin.model.as_deref());
        let key = seat.quota_key(route);
        if store.blocks_live_dispatch(&key) {
            bail!(
                "--reassign {r:?}: {key} is paused; resume it with `spar provider resume {key}` \
                 or reassign to another provider"
            );
        }
        if out.iter().any(|o: &Reassignment| o.target == target) {
            bail!("--reassign names {target} twice");
        }
        out.push(Reassignment {
            target: target.to_string(),
            pin,
        });
    }
    Ok(out)
}

/// The seat a `--reassign` target names: a role with exactly one seat, or a seat id.
fn reassign_target(state: &RunState, target: &str) -> Result<String> {
    let movable = |s: &&SlotState| {
        matches!(
            s.role,
            SlotRole::Implementer | SlotRole::Reviewer | SlotRole::Tester
        )
    };
    if let Some(s) = state.slots.iter().filter(movable).find(|s| s.id == target) {
        return Ok(s.id.clone());
    }
    let role = match target {
        "implementer" | "impl" => SlotRole::Implementer,
        "reviewer" => SlotRole::Reviewer,
        "tester" => SlotRole::Tester,
        _ => {
            let seats: Vec<&str> = state
                .slots
                .iter()
                .filter(movable)
                .map(|s| s.id.as_str())
                .collect();
            bail!(
                "--reassign {target}: run {} has no such seat (seats: {})",
                state.id,
                if seats.is_empty() {
                    "none yet".to_string()
                } else {
                    seats.join(", ")
                }
            );
        }
    };
    let seats: Vec<&SlotState> = state.slots.iter().filter(|s| s.role == role).collect();
    match seats.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => bail!(
            "--reassign {target}: run {} has no {target} seat yet; `--reassign` moves a seat \
             that exists. For a run that has not dispatched its implement panel, use \
             `--reload-config --role {target}=…`",
            state.id
        ),
        many => bail!(
            "--reassign {target}: run {} has {} {target} seats; name one: {}",
            state.id,
            many.len(),
            many.iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve every `--reassign` against the run before anything moves, so one bad
/// target leaves every seat as it was.
fn resolve_reassignments(
    state: &RunState,
    wanted: &[Reassignment],
) -> Result<Vec<(String, crate::provider_ref::Pin)>> {
    let mut out: Vec<(String, crate::provider_ref::Pin)> = Vec::new();
    for r in wanted {
        let seat = reassign_target(state, &r.target)?;
        if out.iter().any(|(s, _)| *s == seat) {
            bail!("--reassign names seat {seat} twice");
        }
        let slot = state
            .slots
            .iter()
            .find(|s| s.id == seat)
            .ok_or_else(|| anyhow::anyhow!("seat {seat} vanished"))?;
        if slot.provider == r.pin.provider && slot.model == r.pin.model {
            bail!(
                "--reassign {}: seat {seat} is already on {}",
                r.target,
                r.pin.display()
            );
        }
        let new_id = moved_seat_id(slot.role, &seat, &r.pin.provider);
        if new_id != seat && state.slots.iter().any(|s| s.id == new_id) {
            bail!(
                "--reassign {}: seat {seat} would become {new_id}, which this run already has",
                r.target
            );
        }
        out.push((seat, r.pin.clone()));
    }
    Ok(out)
}

/// Retire a seat's provider and put another in it (O136, an operator override of O80).
/// The worktree, branch, carry-forward brief, round count and review history stay; a
/// reviewer seat is renamed through `rename_slot` (O85) and its last review artifact
/// follows it, so the blockers it raised still reach the implementer next round. The new
/// provider starts cold: a session marker left under the new seat id by an earlier
/// rotation belongs to a conversation this seat never had.
fn reassign_seat(
    state: &mut RunState,
    paths: &SparPaths,
    seat: &str,
    pin: &crate::provider_ref::Pin,
) -> Result<crate::state::SeatReassignment> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.id == seat)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown seat {seat}"))?;
    let from_provider = crate::provider_ref::Pin {
        provider: slot.provider.clone(),
        model: slot.model.clone(),
    }
    .display();
    let new_id = move_seat(state, paths, seat, pin, SeatSource::Reassigned)?;
    if new_id != seat {
        let from = paths.artifact(&state.id, &format!("review-{seat}.md"));
        if from.is_file() {
            let _ = std::fs::rename(
                &from,
                paths.artifact(&state.id, &format!("review-{new_id}.md")),
            );
        }
    }
    if let Some(cli) = crate::provider_ref::ProviderRef::parse(&pin.provider)
        .ok()
        .and_then(|p| p.cli_name().map(str::to_string))
    {
        crate::markers::clear_session_id(paths, &state.id, &new_id, &cli);
    }
    let record = crate::state::SeatReassignment {
        at: chrono::Utc::now(),
        round: state.round,
        role: slot.role,
        from_seat: seat.to_string(),
        from_provider,
        to_seat: new_id,
        to_provider: pin.display(),
    };
    state.seat_reassignments.push(record.clone());
    let msg = format!(
        "seat {} reassigned by the operator: {} -> {} (now seat {})",
        record.from_seat, record.from_provider, record.to_provider, record.to_seat
    );
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    Ok(record)
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
                    | Phase::Quota
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

/// Whether any slot dispatch started in the run's current round. Dispatch stamps
/// `dispatched_round` and saves before it spawns, so a round it never reached spent
/// nothing.
pub fn current_round_dispatched(state: &RunState) -> bool {
    state.dispatched_round >= state.round
}

fn run_from_approved(
    run_id: &str,
    amendment: Option<String>,
    mut opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    // Loaded once before the lock only so a mistyped id fails without the lock minting
    // a run directory for it. Everything below mutates the run, so it all happens under
    // the lock: a live orchestrator refuses us before its state.json can be overwritten.
    let unlocked = RunState::load(paths, run_id)?;
    // Key the route this process's dispatch will stamp (continuing entries
    // stamp on resumption), not the previous record: a stale record would
    // accept a bucket the dispatch now bills, or refuse one it no longer does.
    let reassignments = parse_reassignments(
        &opts.reassign,
        paths,
        crate::providers::codex::dispatch_route().as_recorded(),
    )?;
    let mut patient = None;
    if let Some(mr) = opts.max_rounds {
        let target = mr.resolve(unlocked.round);
        check_round_extension(&unlocked, target, !current_round_dispatched(&unlocked))?;
        let extension_only = amendment.is_none()
            && opts.providers.is_empty()
            && opts.select.is_empty()
            && opts.base.is_none()
            && !opts.reload_config
            && !opts.accept_contract
            && opts.reassign.is_empty();
        let live = crate::runlock::RunLock::owner(paths, run_id).filter(|o| o.alive());
        if let (true, Some(owner)) = (extension_only, live) {
            if let Some(code) =
                queue_round_extension(paths, &unlocked, target, owner.pid, opts.json)?
            {
                return Ok(code);
            }
            patient = Some((target, owner.pid));
        }
    }
    let lock = match patient {
        None => crate::runlock::RunLock::acquire(paths, run_id)?,
        Some((target, owner_pid)) => {
            match acquire_for_extension(paths, run_id, target, owner_pid, opts.json)? {
                Ok(lock) => lock,
                Err(code) => return Ok(code),
            }
        }
    };
    // Every way back into a run (`--run`, `resume`, a `--plan` the run wrote) dispatches
    // under the environment the run was created in (O130), from its own snapshot.
    Config::for_run(paths, run_id)?.reapply_captured_env();
    let mut state = RunState::load(paths, run_id)?;
    // Read before anything below resets slot statuses for re-dispatch.
    let round_dispatched = current_round_dispatched(&state);
    let resumable = crate::state::is_resumable(&state);
    if !resumable {
        let hint = if state.phase.is_waitable_stop() {
            String::new()
        } else {
            format!("; an in-flight run nobody is driving is picked up with `spar resume {run_id}`")
        };
        bail!(
            "run {run_id} is not resumable (workflow={:?}, phase={:?}){hint}",
            state.workflow,
            state.phase
        );
    }
    if let Some(mr) = opts.max_rounds {
        check_round_extension(&state, mr.resolve(state.round), !round_dispatched)?;
    }
    let moves = resolve_reassignments(&state, &reassignments)
        .map_err(|e| ReassignRefused(format!("{e:#}")))?;
    // Resuming a stopped run: drop the marker so execute_loop dispatches instead
    // of halting again at its first boundary.
    if state.phase == Phase::Stopped {
        let _ = std::fs::remove_file(paths.marker(run_id, "stopped"));
    }
    // Resuming a terminal run (a failed/stuck/quota-parked attempt): clear the dead
    // verdict so the detach snapshot reads as a fresh re-dispatch, not `failed` with
    // `exit_code: 1` (which reads as a refusal). This is purely for snapshot coherence —
    // `execute_loop` re-dispatches the coding slots regardless of their incoming status;
    // here we just make the persisted state it briefly emits honest. Per-slot terminal
    // markers are cleared at dispatch by the executor. A still-paused provider is caught
    // again by the `ensure_usable` gate below, which re-parks at `Quota` with a fresh
    // error rather than silently retrying into the same wall.
    if matches!(state.phase, Phase::Failed | Phase::Stuck | Phase::Quota) {
        for s in &mut state.slots {
            if matches!(s.status, SlotStatus::Failed | SlotStatus::Stuck) {
                s.status = SlotStatus::Pending;
                s.error = None;
                s.exit_code = None;
                s.signal = None;
                s.pid = None;
                s.quota_hit = false;
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
    for (seat, pin) in &moves {
        let r = reassign_seat(&mut state, paths, seat, pin)?;
        if !opts.json {
            println!(
                "reassigned seat {} ({}) to {}{}",
                r.from_seat,
                r.from_provider,
                r.to_provider,
                if r.to_seat == r.from_seat {
                    String::new()
                } else {
                    format!(", now seat {}", r.to_seat)
                }
            );
        }
    }
    if state.dry_run {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    // A bare `implement --run <id>` (no flags at all) is a continuation, not a fresh
    // request. Reuse the run's frozen pool only when it was a genuine operator override
    // (`--providers`/`--select`) — `pool_intent` when the plan phase captured one wider
    // than its own slot count needed, else `providers`. When the frozen pool was itself
    // synthesized from `[roles]`/`[providers].order` (`PoolOrigin::Synth`), there is
    // nothing of the operator's to preserve, so this resolves fresh from `cfg` instead —
    // the only way a `--reload-config` continuation can pick up an edited `[roles]`.
    // Reusing unconditionally (ignoring `pool_origin`) would silently drop an earlier
    // `--providers`/`--select` the moment any `[roles]` entry (e.g. a CLI-pinned
    // reviewer) also happens to be frozen — the opposite bug.
    let is_explicit_frozen_pool = matches!(
        state.pool_origin,
        PoolOrigin::CliProviders | PoolOrigin::Selected
    );
    let frozen_pool: Vec<String> = if !state.pool_intent.is_empty() {
        state.pool_intent.clone()
    } else {
        state.providers.clone()
    };
    let reuses_frozen_pool = opts.providers.is_empty()
        && opts.select.is_empty()
        && is_explicit_frozen_pool
        && !frozen_pool.is_empty();
    let n = crate::workflow::roles_resolve::pool_width(cfg);
    if reuses_frozen_pool {
        // The frozen pool may be narrower than this round's panel — e.g. a plan phase
        // that ran with `--without critic,spec` only ever needed one provider. Refusing
        // here regressed the documented `plan -> approve -> implement --run` flow itself
        // (any plan pool narrower than the implement panel broke its own continuation);
        // `pick_providers` below already cycles a short explicit pool to fill `n`, the
        // same fallback an unpinned panel already relies on, so let it.
        opts.providers = frozen_pool.clone();
    }
    let roles: Vec<&str> = std::iter::once(SlotRole::Implementer.as_config_key())
        .chain(std::iter::repeat(SlotRole::Reviewer.as_config_key()))
        .take(n)
        .collect();
    let requested = opts.resolve_pool(n, &roles, paths, cfg, &state.id)?;
    // `opts.providers` was just filled from the frozen pool above when reusing a
    // continuation's pool — that is not a fresh CLI `--providers`, so the source it
    // reports has to stay whatever the run already froze (`state.pool_origin`), not get
    // relabelled `cli-providers` for having reached the resolver as a non-empty pool.
    let pool_origin = if reuses_frozen_pool {
        state.pool_origin
    } else {
        crate::workflow::roles_resolve::pool_origin_for(&opts)
    };
    state.pool_origin = pool_origin;
    // A replacement pool this round (an explicit `--providers`/`--select` on the
    // continuation) becomes the new frozen intent for any later round; a reused one
    // needs no change, and a fresh non-explicit resolution has no intent to freeze.
    if !reuses_frozen_pool {
        state.pool_intent =
            if matches!(pool_origin, PoolOrigin::CliProviders | PoolOrigin::Selected) {
                requested.clone()
            } else {
                Vec::new()
            };
    }
    state.providers = providers::pick_providers(&requested, n, Some(&requested), state.dry_run);
    // Quota gate is now backup-aware; it runs after seats are materialized.
    // A bare pool gate before prepare_implement_slots would see the pool entry as paused
    // and park, even though the seat's CLI pin would be covered by its backup.
    let from_round_gate = state.phase == Phase::AwaitingRoundExtension;
    // An extension queued for an orchestrator that exited before its ceiling check
    // still counts; an explicit flag on this re-entry outranks it.
    apply_queued_round_extension(&mut state, paths);
    if let Some(mr) = opts.max_rounds {
        state.max_rounds = mr.resolve(state.round);
    }
    // A round that never dispatched is still the current round, not a spent one: an
    // orchestrator killed between opening it and spawning anything (a launching shell
    // that exited, a quota park before dispatch) cost nothing, so re-entry reuses it,
    // and reusing it is not a step past the ceiling.
    let reuse_round = state.round > 0 && !round_dispatched;
    if !reuse_round && state.round_ceiling_reached() && escalate_round_ceiling(&mut state, paths)? {
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
    let round = if reuse_round {
        state.round
    } else {
        state.begin_round()
    };
    // A re-entered round number is not a reused dispatch: this entry is about to
    // read pauses and spawn, so the pre-existing/this-round line starts now.
    // A fix pass below opens rounds without re-stamping: it continues this
    // dispatch, and its seats must not trip over this dispatch's own pauses.
    state.stamp_dispatch();
    if !opts.json {
        println!("run {run_id} · round {round}");
    }
    let dry = state.dry_run;
    prepare_implement_slots(
        &mut state,
        Some(&requested),
        pool_origin,
        dry,
        cfg,
        paths,
        Reentry {
            from_round_gate,
            reload_config: opts.reload_config,
        },
    )?;
    // Only the implementer must run next at run start, so only it can park the
    // run here: a paused reviewer or tester seat is reseated onto its backup
    // or stood down at its own dispatch.
    if !dry {
        if let Some(msg) = gate_implementer_seat(&mut state, paths, cfg)? {
            state.error = Some(msg.clone());
            state.set_phase(crate::state::Phase::Quota);
            state.save(paths)?;
            // The round already bumped above: refresh so poll files drop what the
            // new round ages out even though no slot dispatches.
            crate::providers::delivery::refresh_run_poll_files(paths, run_id);
            if opts.json {
                crate::executor::emit_run_json(&state)?;
            } else {
                eprintln!("error: {msg}");
            }
            return Ok(crate::exit_codes::ExitCode::Quota);
        }
    }
    if state.slots.iter().all(|s| s.role != SlotRole::Implementer) {
        bail!("no implementer slot after provider pick");
    }
    state.save(paths)?;
    crate::providers::delivery::refresh_run_poll_files(paths, run_id);
    if opts.detach {
        // The detached child takes the lock itself, and flock refuses a second holder.
        drop(lock);
        return detach_implement(&state, paths, opts.json);
    }
    execute_loop(&mut state, paths, cfg, opts.accept_contract)?;
    maybe_auto_ship_or_cleanup(&mut state, paths, cfg)?;
    finish_out(&state, opts.json)?;
    Ok(state.exit_code())
}

fn prepare_implement_slots(
    state: &mut RunState,
    requested: Option<&[String]>,
    pool_origin: PoolOrigin,
    dry: bool,
    cfg: &Config,
    paths: &SparPaths,
    reentry: Reentry,
) -> Result<()> {
    let Reentry {
        from_round_gate: resuming_from_round_gate,
        reload_config,
    } = reentry;
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
        // `--reload-config` resolved a fresh `Config` (O27's one sanctioned escape),
        // and until now that reached `config.json` and stopped there: seats minted by
        // an earlier round kept whatever provider they were created with, so
        // `--role implementer=…` on a run whose slots exist changed the frozen config
        // and nothing that runs. `skills/core.md` documents `--reload-config` as the
        // way to change a running run's fleet, so the contract was real and unhonoured.
        if reload_config {
            repoint_existing_seats(state, cfg)?;
        }
        ensure_suite_slot(state, dry, cfg, paths)?;
        return Ok(());
    }

    let req = requested.unwrap_or_default();
    let n = crate::workflow::roles_resolve::pool_width(cfg);
    state.providers = providers::pick_providers(req, n, Some(req), dry);
    if state.providers.is_empty() {
        bail!("no usable providers (pass --providers, set a [roles] block, or --select)");
    }
    let pool = state.providers.clone();

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

    // One resolver for slot creation and for the plan gate's projection of this same
    // panel (feature 011, item C): the id formula cannot drift between the two.
    let seats = crate::workflow::roles_resolve::build_implement_seats(
        cfg,
        &pool,
        pool_origin,
        false,
        &model_for,
    );
    if seats.iter().all(|s| s.role != SlotRole::Implementer) {
        anyhow::bail!("no provider resolved for implementer");
    }
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let has_backup = cfg.backups.implementer.is_some()
        || !cfg.backups.reviewer.is_empty()
        || cfg.backups.tester.is_some();
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
    for seat in seats {
        let mut provider = seat.provider.clone();
        let mut source = seat.source;
        let mut model = seat.model.clone();
        let mut seat_id = seat.seat.clone();
        let seat_ref =
            crate::provider_ref::ProviderRef::for_seat(&provider, model.as_deref()).display();
        if !crate::backup::is_provider_eligible(
            &seat_ref,
            &store,
            available.as_ref(),
            state.recorded_route(),
        ) {
            let ordinal = if seat.role == SlotRole::Reviewer {
                reviewer_ordinal(&seat.seat).unwrap_or(0)
            } else {
                0
            };
            if let Some(backup_raw) = crate::backup::backup_for_role(seat.role, ordinal, cfg) {
                if crate::backup::backup_eligible(
                    &backup_raw,
                    &store,
                    available.as_ref(),
                    paths,
                    dry,
                    state.recorded_route(),
                ) {
                    if let Ok(pin) = crate::provider_ref::Pin::parse(&backup_raw) {
                        provider = pin.display();
                        source = crate::state::SeatSource::Backup;
                        model = pin.model.clone();
                        if seat.role == SlotRole::Reviewer {
                            seat_id = format!(
                                "review-{}-{}",
                                ordinal,
                                crate::util::sanitize_slot(&pin.provider)
                            );
                        }
                    }
                }
            }
        }
        let mut slot = executor::init_slot_model(&seat_id, &provider, seat.role, model);
        slot.source = Some(source);
        state.slots.push(slot);
    }
    // The projected panel has graduated into real slots; a stale projection would only
    // risk drifting from what was actually just resolved above.
    state.projected_fleet.clear();
    ensure_suite_slot(state, dry, cfg, paths)?;
    Ok(())
}

/// Kill and drop every tester slot: the pid marker is filed under the slot id, so it
/// must be reaped before the slot record disappears or a still-running agent becomes an
/// orphan nothing can find (O28).
fn reap_tester_slots(state: &mut RunState, paths: &SparPaths) {
    for slot in state.slots.iter().filter(|s| s.role == SlotRole::Tester) {
        if let Some(token) = crate::markers::read_pid(paths, &state.id, &slot.id) {
            if token.alive() {
                crate::process::terminate_tree(token.pid, true);
            }
        }
        crate::markers::clear_pid(paths, &state.id, &slot.id);
    }
    state.slots.retain(|s| s.role != SlotRole::Tester);
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
        // `--reload-config`) carries a slot nothing will ever dispatch again. Reap before
        // dropping: the slot id is the only key its pid marker is filed under, so
        // removing the record first is how a still-running agent becomes an orphan
        // nothing can find (O28). Keeping the slot instead is worse — nothing in the
        // built-in round settles it, so it reads `Running` forever, trips the stall
        // check every round, and goes on writing the same `suite.md` spar writes.
        // Killing it is this command's call: it holds the run lock, the slot is its own,
        // and the channel that slot belonged to no longer exists.
        reap_tester_slots(state, paths);
        return Ok(());
    }
    if !cfg.suite.enabled {
        // `--without suite` on a run that already has a live tester slot (a resumed
        // round) must not leave it in place for the next dispatch to pick back up —
        // same reap as the built-in-suite branch above, for the same reason (O28).
        reap_tester_slots(state, paths);
        return Ok(());
    }
    if state.slots.iter().any(|s| s.role == SlotRole::Tester) {
        return Ok(());
    }
    let (suite_prov, suite_model, source) = resolve_suite_provider(
        cfg,
        dry,
        &state.providers,
        state.pool_origin,
        Some(paths),
        Some(&state.id),
    )?;
    let mut slot = executor::init_slot_model(
        format!("suite-{}", sanitize_slot(&suite_prov)),
        &suite_prov,
        SlotRole::Tester,
        suite_model,
    );
    slot.source = Some(source);
    state.slots.push(slot);
    Ok(())
}

/// The tester seat a plan gate has not dispatched yet, projected the same way the
/// implementer/reviewer panel is (feature 011, item C). A built-in `[suite].command` or
/// `--without suite` never spawns an agent seat, so neither projects one — the plan
/// gate's `fleet` array must not promise a paid seat the run will never create.
///
/// `paths`/`run_id`, when given, are read-only: if a `tester`-role choice is already
/// recorded in `model-select.json` (e.g. from an earlier `--select`), the projection
/// reports that exact provider/model/`ModelSelect` source, matching what
/// `ensure_suite_slot`'s own artifact lookup will find at dispatch. Never calls
/// `pick_one_for_role` / `write_select_artifact` here — a projection must not choose a
/// model or mutate the artifact as a side effect of showing a gate, so a run with no
/// prior tester choice still reports the same `SuitePreferences` guess dispatch would
/// fall back to, honestly labelled as not yet decided.
pub fn project_tester_seat(
    cfg: &Config,
    dry: bool,
    pool: &[String],
    pool_origin: PoolOrigin,
    paths: Option<&SparPaths>,
    run_id: Option<&str>,
) -> Option<FleetSeat> {
    if cfg.suite.is_builtin() || !cfg.suite.enabled {
        return None;
    }
    if let (Some(paths), Some(run_id)) = (paths, run_id) {
        if let Ok(Some(art)) = crate::model_select::load_select_artifact(paths, run_id) {
            if let Some(c) = art
                .choices
                .iter()
                .find(|c| c.role.as_deref() == Some("tester"))
            {
                return Some(FleetSeat {
                    seat: format!("suite-{}", sanitize_slot(&c.provider)),
                    role: SlotRole::Tester,
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    source: SeatSource::ModelSelect,
                    projected: true,
                });
            }
        }
    }
    let (provider, model, source) =
        resolve_suite_provider(cfg, dry, pool, pool_origin, None, None).ok()?;
    Some(FleetSeat {
        seat: format!("suite-{}", sanitize_slot(&provider)),
        role: SlotRole::Tester,
        provider,
        model,
        source,
        projected: true,
    })
}

/// Cheap suite-channel provider: config override, model-select (tester/fast), prefs, fleet.
/// Returns the provider, an optional explicit model, and the honest source of the pick —
/// the fleet fallback is `state.providers`, so its source follows `pool_origin` rather
/// than an unconditional `ProvidersOrder`.
fn resolve_suite_provider(
    cfg: &Config,
    dry: bool,
    fleet: &[String],
    pool_origin: PoolOrigin,
    paths: Option<&SparPaths>,
    run_id: Option<&str>,
) -> Result<(String, Option<String>, SeatSource)> {
    if let Some(p) = &cfg.roles.tester {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid [roles].tester {p:?}: {e}"))?;
        if dry || providers::is_provider_usable(p, false) {
            let source = if cfg.cli_role_keys.contains(SlotRole::Tester.as_config_key()) {
                SeatSource::CliRole
            } else {
                SeatSource::RolesFile
            };
            return Ok((p.clone(), None, source));
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
                return Ok((c.provider.clone(), c.model.clone(), SeatSource::ModelSelect));
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
                return Ok((c.provider, c.model, SeatSource::ModelSelect));
            }
        }
    }
    const PREFS: &[&str] = &["cli:claude", "cli:grok", "cli:agy", "api:xai", "api:openai"];
    if dry {
        return Ok((PREFS[0].into(), None, SeatSource::SuitePreferences));
    }
    if let Some(p) = PREFS
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .map(|s| (*s).to_string())
    {
        return Ok((p, None, SeatSource::SuitePreferences));
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok((p, None, pool_origin.as_seat_source()));
    }
    bail!("suite.enabled but no usable suite provider (set [roles].tester or install a CLI)")
}

#[cfg(test)]
mod round_reuse_tests {
    use super::*;

    fn run(workflow: crate::cli::WorkflowKind, phase: Phase) -> RunState {
        let mut st = RunState::new("r1", workflow, std::path::PathBuf::from("/x"));
        st.phase = phase;
        st
    }

    /// #179: only a dispatch that started in the current round charges it. A slot put
    /// back to `Pending` by a backup failover after it really ran still counts.
    #[test]
    fn a_round_counts_as_dispatched_only_once_a_dispatch_started_in_it() {
        let mut st = run(crate::cli::WorkflowKind::Loop, Phase::Stopped);
        st.round = 1;
        st.slots.push(executor::init_slot(
            "impl",
            "cli:claude",
            SlotRole::Implementer,
        ));
        assert!(!current_round_dispatched(&st), "created, never dispatched");
        st.dispatched_round = 1;
        assert!(
            current_round_dispatched(&st),
            "dispatched, then failed over to Pending"
        );
        st.round = 2;
        assert!(
            !current_round_dispatched(&st),
            "round 2 opened, nothing ran in it"
        );
    }
}

#[cfg(test)]
mod suite_reap_tests {
    use super::*;

    fn run_with_impl(provider: &str, model: &str, status: SlotStatus) -> RunState {
        let mut st = RunState::new(
            "repoint1",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        let mut slot = executor::init_slot_model(
            "impl",
            provider,
            SlotRole::Implementer,
            Some(model.to_string()),
        );
        slot.status = status;
        st.slots.push(slot);
        st.providers = vec![provider.to_string()];
        st
    }

    fn cfg_with_implementer(pin: &str) -> Config {
        let mut cfg = Config::default();
        cfg.roles.implementer = Some(pin.to_string());
        cfg
    }

    /// `--reload-config` is documented in `skills/core.md` as the way to change a
    /// running run's fleet. Before this, it reached `config.json` and stopped:
    /// `prepare_implement_slots` early-returned on `has_impl`, so a seat minted by an
    /// earlier round kept its original provider and the re-dispatch used it. That is
    /// how a bad model pin survived three separate corrections and had to be fixed by
    /// editing `state.json` by hand.
    #[test]
    fn reload_config_repoints_a_seat_that_already_exists() {
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Failed);
        let cfg = cfg_with_implementer("cli:muse@muse-spark-1.2");

        repoint_existing_seats(&mut st, &cfg).unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(impl_slot.model.as_deref(), Some("muse-spark-1.2"));
    }

    /// The invariant a re-point must preserve: every reviewer seat's provider still
    /// matches the provider its id names (`review-{r}-{provider}`).
    ///
    /// This pins the invariant, **not** the incident. In a live session a
    /// `--reload-config` left `cli:claude` behind the seat named `review-1-cli-codex`
    /// on two runs — collapsing a deliberate two-vendor panel to one vendor, which is
    /// the exact thing feature 011 exists to keep exact, and then naming codex in the
    /// rate-limit error when claude's limit was the one hit. The config that produced
    /// it could not be reconstructed from the run state afterwards, so this test does
    /// not fail against the old code. What catches that case now is the `bail!` in
    /// `repoint_existing_seats`: a seat whose id and provider disagree is refused
    /// instead of written.
    #[test]
    fn a_repoint_keeps_every_seat_matching_the_provider_its_id_names() {
        let mut st = RunState::new(
            "panel001",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        for (id, prov, model) in [
            ("review-0-cli-claude", "cli:claude", "opus"),
            ("review-1-cli-codex", "cli:codex", "gpt-5.6-terra"),
        ] {
            let mut slot =
                executor::init_slot_model(id, prov, SlotRole::Reviewer, Some(model.into()));
            slot.status = SlotStatus::Done;
            st.slots.push(slot);
        }
        st.providers = vec!["cli:claude@opus".into(), "cli:codex@gpt-5.6-terra".into()];

        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:claude@opus".into(), "cli:codex@gpt-5.6-terra".into()];

        repoint_existing_seats(&mut st, &cfg).unwrap();

        let panel: Vec<(&str, &str)> = st
            .slots
            .iter()
            .filter(|s| s.role == SlotRole::Reviewer)
            .map(|s| (s.id.as_str(), s.provider.as_str()))
            .collect();
        assert_eq!(
            panel,
            vec![
                ("review-0-cli-claude", "cli:claude"),
                ("review-1-cli-codex", "cli:codex"),
            ],
            "every seat's provider must still match the provider its id names"
        );
        for s in st.slots.iter().filter(|s| s.role == SlotRole::Reviewer) {
            assert!(
                s.id.ends_with(&crate::util::sanitize_slot(&s.provider)),
                "seat {} carries {}, so its id is a lie",
                s.id,
                s.provider
            );
        }
    }

    /// The seat that is mid-flight is the one whose provider is load-bearing: its pid
    /// marker is filed against what actually spawned, so rewriting the record would
    /// make the run disagree with the process it is waiting on.
    #[test]
    fn a_running_seat_is_never_repointed() {
        let mut st = run_with_impl("cli:muse", "muse-spark-1.3", SlotStatus::Running);
        let cfg = cfg_with_implementer("cli:muse@muse-spark-1.2");

        repoint_existing_seats(&mut st, &cfg).unwrap();

        let impl_slot = st.slots.iter().find(|s| s.id == "impl").unwrap();
        assert_eq!(
            impl_slot.model.as_deref(),
            Some("muse-spark-1.3"),
            "a live dispatch keeps the provider that spawned it"
        );
    }

    /// The cap must reserve what is about to run.
    ///
    /// `spar stop` leaves every interrupted slot at `Failed`, and
    /// `daemon::run_demand` counts only `Pending`/`Running` — so a stopped
    /// Review/Arena/Roles/Peer run read as needing *nothing*, was admitted past a
    /// full cross-run cap, and then overran it as `executor::run_slot` flipped each
    /// retained slot back to `Running`.
    ///
    /// Asserted against `run_demand` itself, not against slot statuses after a
    /// resume: a dry-run workflow completes its slots to `done`, so an end-state
    /// assertion passes with or without the fix and guarantees nothing. (It did.)
    #[test]
    fn a_stops_interrupted_slots_are_demand_again_before_admission() {
        let mut state = RunState::new(
            "capbypass",
            crate::cli::WorkflowKind::Review,
            std::path::PathBuf::from("/x"),
        );
        for i in 0..3 {
            let mut slot =
                crate::executor::init_slot(format!("review-{i}"), "cli:claude", SlotRole::Reviewer);
            // The state `stop_one` leaves behind: interrupted, reconciled to `Failed`.
            slot.status = SlotStatus::Failed;
            state.slots.push(slot);
        }

        let before: u32 = crate::daemon::run_demand(&state).values().copied().sum();
        assert_eq!(
            before, 0,
            "precondition: this is exactly why the cap could be bypassed"
        );

        restore_interrupted_slots(&mut state);

        let after: u32 = crate::daemon::run_demand(&state).values().copied().sum();
        assert_eq!(
            after, 3,
            "every slot the workflow is about to re-dispatch has to count as demand"
        );
    }

    /// `--without suite --reload-config` on a run whose earlier round already dispatched
    /// an agent tester must reap that slot, not leave it for the next round to
    /// re-dispatch (review `review-0-cli-claude`, finding 5).
    #[test]
    fn disabling_suite_reaps_an_existing_tester_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.slots.push(executor::init_slot(
            "suite-cli-claude",
            "cli:claude",
            SlotRole::Tester,
        ));
        let mut cfg = Config::default();
        cfg.suite.enabled = false;

        ensure_suite_slot(&mut state, true, &cfg, &paths).unwrap();

        assert!(
            state.slots.iter().all(|s| s.role != SlotRole::Tester),
            "a resumed run with --without suite must not keep dispatching its old tester: {:?}",
            state.slots
        );
    }
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
pub(crate) fn acceptance_blocks_ship(
    criteria: &[String],
    res: &ReviewResult,
    cfg: &Config,
) -> bool {
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
pub(crate) fn acceptance_block_reasons(
    criteria: &[String],
    res: &ReviewResult,
    cfg: &Config,
) -> Vec<String> {
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

/// The reviewed-commit gate's decision for one reviewer slot. The panel-dispatch
/// head is the contract: a reviewer that judged an older sha is stale, and one
/// that judged a newer sha is equally not a verdict — the suite ran against the
/// contract head and sibling reviewers judged it, so counting a verdict about a
/// different tree would smuggle an unjudged tree past the panel. Both self-heal:
/// the mismatch feeds `any_request_changes` and the next round re-resolves head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewedCommitDecision {
    /// spar could not resolve the panel head; skip the check loudly, fail open.
    Skipped,
    /// The parsed sha names the panel head.
    Match,
    /// The artifact names no (parsable) commit.
    Absent,
    /// The parsed sha names a different commit.
    Mismatch,
}

/// The recorded verdict a stood-down reviewer carries into the gate: only a seat
/// that never dispatched this round (`stood_down`) and whose last recorded
/// verdict rejects this head. A veto about an older head was superseded by the
/// fix round; a seat that never judged has nothing to carry; a failure stub
/// names no commit, so only a real judgment about this head carries.
fn carried_rejection<'a>(
    slot: &'a SlotState,
    panel_head: Option<&str>,
) -> Option<&'a ReviewResult> {
    slot.stood_down.as_ref()?;
    let head = panel_head?;
    let res = slot.review.as_ref()?;
    if res.verdict != Some(review_result::Verdict::RequestChanges) {
        return None;
    }
    let sha = res.reviewed_commit.as_deref()?;
    review_result::reviewed_commit_matches(head, sha).then_some(res)
}

fn decide_reviewed_commit(panel_head: Option<&str>, res: &ReviewResult) -> ReviewedCommitDecision {
    let Some(head) = panel_head else {
        return ReviewedCommitDecision::Skipped;
    };
    let Some(sha) = res.reviewed_commit.as_deref() else {
        return ReviewedCommitDecision::Absent;
    };
    if review_result::reviewed_commit_matches(head, sha) {
        ReviewedCommitDecision::Match
    } else {
        ReviewedCommitDecision::Mismatch
    }
}

/// Fail a reviewer slot closed on a reviewed-commit gate trip: failed status with
/// a greppable error, a slot-attributed event, and a stderr warning. The
/// artifact bytes are preserved as drift evidence, never synthesized over.
fn fail_review_slot_on_commit(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    error: String,
) {
    if let Some(s) = state.slot_mut(slot_id) {
        s.status = SlotStatus::Failed;
        s.error = Some(error.clone());
    }
    eprintln!("warning: review slot `{slot_id}` failed: {error}");
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot_note(slot_id, error),
    );
    let _ = state.save(paths);
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
    state.key = opts.key.clone();
    state.backend = opts.backend;
    worktree::apply_run_base(&mut state, opts.base.as_deref(), opts.json)?;
    cfg.save_snapshot(paths, &state.id)?;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    state.big = opts.big;
    state.max_fix_rounds = 3;
    state.max_rounds = opts
        .max_rounds
        .map_or(cfg.rounds.max, |mr| mr.resolve(state.round));
    let n = crate::workflow::roles_resolve::pool_width(cfg);
    let roles: Vec<&str> = std::iter::once(SlotRole::Implementer.as_config_key())
        .chain(std::iter::repeat(SlotRole::Reviewer.as_config_key()))
        .take(n)
        .collect();
    let requested = opts.resolve_pool(n, &roles, paths, cfg, &state.id)?;
    let pool_origin = crate::workflow::roles_resolve::pool_origin_for(&opts);
    // A later round resuming this run without `--reload-config` (O27) reads this back
    // via `run_from_approved`'s frozen-pool reuse, so it has to be recorded here too, not
    // just at the plan phase.
    state.pool_origin = pool_origin;
    state.providers = providers::pick_providers(&requested, n, Some(&requested), dry);
    state.stamp_dispatch();
    // Quota gate is now backup-aware and runs after seat resolution: a paused primary
    // with an eligible backup must not park the run.
    prepare_implement_slots(
        &mut state,
        Some(&requested),
        pool_origin,
        dry,
        cfg,
        paths,
        Reentry {
            from_round_gate: false,
            reload_config: opts.reload_config,
        },
    )?;
    // Only the implementer must run next at run start, so only it can park the
    // run here: a paused reviewer or tester seat is reseated onto its backup
    // or stood down at its own dispatch.
    if !dry {
        if let Some(msg) = gate_implementer_seat(&mut state, paths, cfg)? {
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

    paths.ensure_run_dirs(&state.id)?;
    let _ = crate::bus::ensure_bus(paths);
    let _ = crate::bus::join(
        paths,
        Some(&state.id),
        crate::bus::ORCHESTRATOR_SEAT,
        None,
        None,
    );
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

    // Only the implementer gets a branch; reviewers judge from detached checkouts cut
    // at each panel head (O138).
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
    // The frozen text itself, kept apart from the file the implementer can write to,
    // so `spar amend` starts from what was frozen (O137).
    if paths.artifact(&state.id, "test-contract.md").is_file() {
        let _ = std::fs::write(
            paths.artifact(&state.id, crate::workflow::plan::FROZEN_CONTRACT),
            &frozen.body,
        );
    }
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
    if let Some(author) = state.test_author_seat().map(|s| s.id.clone()) {
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
                    false,
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
                    crate::bus::ORCHESTRATOR_SEAT,
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
    let mut blockers: Vec<String> = blockers_from_disk(
        state,
        paths,
        &crate::rules::unwaived(&frozen.criteria, &crate::rules::for_run(paths, &state.id)),
        cfg,
    );

    loop {
        // Set when the implementer is killed at its hard ceiling with work left
        // behind: the leftovers are committed as a partial WIP round and the loop
        // goes on to the suite and the panel instead of failing (O114 follow-up).
        // Per-round state: a later fix round sets it again or leaves it empty.
        let mut ceiling_partial_note: Option<String> = None;
        // Stop boundary: before the implementer (and every fix-round re-dispatch).
        if should_stop(paths, &state.id) {
            return stop_now(state, paths);
        }
        state.set_phase(Phase::Dispatch);
        state.save(paths)?;

        if let Some(id) = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Implementer)
            .map(|s| s.id.clone())
        {
            if let SeatCheck::Park(msg) = recheck_seat(state, paths, cfg, &id, false)? {
                return fail(state, paths, anyhow::anyhow!(msg), true);
            }
        }

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
        let persisted =
            crate::bus::persisted_for(paths, &state.id, &impl_slot.id).unwrap_or_default();
        let unique = crate::bus::agent_ref(Some(&state.id), &impl_slot.id);
        for m in &persisted {
            for event in [
                crate::bus::ReceiptEvent::Delivered,
                crate::bus::ReceiptEvent::Read,
            ] {
                crate::bus::record_receipt(paths, &m.id, &unique, event, "carry_forward");
            }
        }
        let persisted: Vec<String> = persisted
            .iter()
            .map(|m| format!("from {} (message {}): {}", m.from, m.id, m.body))
            .collect();
        let mut extra = HashMap::new();
        extra.insert("plan_body".into(), plan_body.clone());
        extra.insert("test_contract_body".into(), test_contract_body.clone());
        extra.insert("amendment_section".into(), amendment_section.clone());
        extra.insert(
            "carry_forward_section".into(),
            carry_forward_section(
                state.round,
                &persisted,
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
            extra_vars: extra.clone(),
            expected_artifact: Some(format!("summary-{}.md", impl_slot.id)),
            model: impl_model,
            effort: None,
        };
        // Set when the pass below records a ceiling partial for this round, so
        // the normal-finish clear after the dispatch does not erase it.
        let mut partial_recorded = false;
        if let Err(e) = executor::run_slot(state, paths, cfg, &impl_job) {
            // A hard-ceiling kill is reported as a typed cause on the slot, never
            // matched from the error prose. With work left behind the round goes on:
            // the leftovers are committed as WIP below and judged like any round.
            // With no work at all the round fails as before.
            // Quota wins over the ceiling: a dispatch that hit both parks with
            // exit 4 instead of going to review on partial work, which could end
            // the run at 2 or 0. Exit codes are a public contract.
            if record_ceiling_partial_unless_quota(
                state,
                paths,
                &impl_job.slot_id,
                &mut ceiling_partial_note,
            )? {
                partial_recorded = true;
            }
            if !partial_recorded {
                let quota_hit = executor::slot_parks_on_quota(state, &impl_job.slot_id);
                if quota_hit {
                    if let Some(backup_raw) =
                        crate::backup::backup_for_role(SlotRole::Implementer, 0, cfg)
                    {
                        let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                        let has_backup = cfg.backups.implementer.is_some();
                        let available: Option<std::collections::HashSet<String>> =
                            if state.dry_run || !has_backup {
                                None
                            } else {
                                let detected: std::collections::HashSet<String> =
                                    crate::providers::detect_all()
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
                        let slot_state = state
                            .slots
                            .iter()
                            .find(|s| s.id == impl_job.slot_id)
                            .cloned()
                            .unwrap();
                        let cause = crate::backup::dispatch_stop_cause(
                            &slot_state,
                            &store,
                            available.as_ref(),
                            state.recorded_route(),
                        );
                        if cause.engages_backup() && slot_state.source != Some(SeatSource::Backup) {
                            let backup_key = crate::provider_ref::ProviderRef::parse(&backup_raw)
                                .map(|r| r.storage_key())
                                .unwrap_or_else(|_| backup_raw.clone());
                            let cur_key =
                                crate::provider_ref::ProviderRef::parse(&impl_slot.provider)
                                    .map(|r| r.storage_key())
                                    .unwrap_or_else(|_| impl_slot.provider.clone());
                            if backup_key != cur_key
                                && crate::backup::backup_eligible(
                                    &backup_raw,
                                    &store,
                                    available.as_ref(),
                                    paths,
                                    state.dry_run,
                                    state.recorded_route(),
                                )
                            {
                                if let Some(s) = state.slot_mut(&impl_job.slot_id) {
                                    let pin = crate::provider_ref::Pin::parse(&backup_raw)
                                        .map(|p| p.display())
                                        .unwrap_or(backup_raw.clone());
                                    s.point_at(&pin);
                                    s.source = Some(SeatSource::Backup);
                                }
                                state.save(paths)?;
                                // Retry the implementer with backup in the same round
                                let retry_impl_slot = state
                                    .slots
                                    .iter()
                                    .find(|s| s.id == impl_job.slot_id)
                                    .cloned()
                                    .unwrap();
                                let retry_job = SlotJob {
                                    slot_id: retry_impl_slot.id.clone(),
                                    provider: retry_impl_slot.provider.clone(),
                                    role: SlotRole::Implementer,
                                    template: "implementer".into(),
                                    extra_vars: extra.clone(),
                                    expected_artifact: Some(format!(
                                        "summary-{}.md",
                                        retry_impl_slot.id
                                    )),
                                    model: retry_impl_slot.model.clone(),
                                    effort: None,
                                };
                                if let Err(e2) = executor::run_slot(state, paths, cfg, &retry_job) {
                                    if !handle_retry_err(
                                        state,
                                        paths,
                                        &retry_job.slot_id,
                                        e2,
                                        &mut ceiling_partial_note,
                                    )? {
                                        return Ok(());
                                    }
                                    partial_recorded = true;
                                }
                                // Retry succeeded, continue to review phase
                            } else {
                                return fail(state, paths, e, quota_hit);
                            }
                        } else {
                            return fail(state, paths, e, quota_hit);
                        }
                    } else {
                        return fail(state, paths, e, quota_hit);
                    }
                } else {
                    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                    let has_backup = cfg.backups.implementer.is_some();
                    let available: Option<std::collections::HashSet<String>> =
                        if state.dry_run || !has_backup {
                            None
                        } else {
                            let detected: std::collections::HashSet<String> =
                                crate::providers::detect_all()
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
                    let slot_state = state
                        .slots
                        .iter()
                        .find(|s| s.id == impl_job.slot_id)
                        .cloned()
                        .unwrap();
                    let cause = crate::backup::dispatch_stop_cause(
                        &slot_state,
                        &store,
                        available.as_ref(),
                        state.recorded_route(),
                    );
                    if cause.engages_backup() {
                        if let Some(backup_raw) =
                            crate::backup::backup_for_role(SlotRole::Implementer, 0, cfg)
                        {
                            if crate::backup::backup_eligible(
                                &backup_raw,
                                &store,
                                available.as_ref(),
                                paths,
                                state.dry_run,
                                state.recorded_route(),
                            ) && slot_state.source != Some(SeatSource::Backup)
                            {
                                let backup_key =
                                    crate::provider_ref::ProviderRef::parse(&backup_raw)
                                        .map(|r| r.storage_key())
                                        .unwrap_or_else(|_| backup_raw.clone());
                                let cur_key =
                                    crate::provider_ref::ProviderRef::parse(&impl_slot.provider)
                                        .map(|r| r.storage_key())
                                        .unwrap_or_else(|_| impl_slot.provider.clone());
                                if backup_key != cur_key {
                                    if let Some(s) = state.slot_mut(&impl_job.slot_id) {
                                        let pin = crate::provider_ref::Pin::parse(&backup_raw)
                                            .map(|p| p.display())
                                            .unwrap_or(backup_raw);
                                        s.point_at(&pin);
                                        s.source = Some(SeatSource::Backup);
                                    }
                                    state.save(paths)?;
                                    let retry_impl_slot = state
                                        .slots
                                        .iter()
                                        .find(|s| s.id == impl_job.slot_id)
                                        .cloned()
                                        .unwrap();
                                    let retry_job = SlotJob {
                                        slot_id: retry_impl_slot.id.clone(),
                                        provider: retry_impl_slot.provider.clone(),
                                        role: SlotRole::Implementer,
                                        template: "implementer".into(),
                                        extra_vars: extra.clone(),
                                        expected_artifact: Some(format!(
                                            "summary-{}.md",
                                            retry_impl_slot.id
                                        )),
                                        model: retry_impl_slot.model.clone(),
                                        effort: None,
                                    };
                                    if let Err(e2) =
                                        executor::run_slot(state, paths, cfg, &retry_job)
                                    {
                                        if !handle_retry_err(
                                            state,
                                            paths,
                                            &retry_job.slot_id,
                                            e2,
                                            &mut ceiling_partial_note,
                                        )? {
                                            return Ok(());
                                        }
                                        partial_recorded = true;
                                    }
                                } else {
                                    return fail(state, paths, e, quota_hit);
                                }
                            } else {
                                return fail(state, paths, e, quota_hit);
                            }
                        } else {
                            return fail(state, paths, e, quota_hit);
                        }
                    } else {
                        return fail(state, paths, e, quota_hit);
                    }
                }
            }
        }

        // The implementer finished without a ceiling kill, so this round is not
        // cut short even if an earlier same-round attempt was: drop its mark
        // before review. A just-recorded partial sets the flag below instead.
        if !partial_recorded {
            clear_finished_round_partial_mark(state, paths)?;
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
        if let Some(note) = commit_before_review(state, paths, cfg, &impl_slot.id) {
            ceiling_partial_note = Some(note);
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
            // A tester seat with no usable provider parks the run: the suite
            // is a singular gate. `recheck_seat` moves it onto an eligible
            // backup when it has one (O153).
            let tester = match tester {
                Some(t) => match recheck_seat(state, paths, cfg, &t.id, false)? {
                    SeatCheck::Ready(id) => state.slots.iter().find(|s| s.id == id).cloned(),
                    SeatCheck::Park(msg) => return fail(state, paths, anyhow::anyhow!(msg), true),
                },
                None => None,
            };
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
                    crate::bus::ORCHESTRATOR_SEAT,
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
                    effort: None,
                };
                let suite_ok = executor::run_slot(state, paths, cfg, &suite_job).is_ok();
                if !suite_ok && executor::slot_parks_on_quota(state, &tester.id) {
                    // A rate-limited tester must park the run, not read as an
                    // inconclusive suite that burns a fix round against the same
                    // paused provider.
                    return fail(
                        state,
                        paths,
                        anyhow::anyhow!("tester slot `{}` rate-limited", tester.id),
                        true,
                    );
                }
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
                    crate::bus::ORCHESTRATOR_SEAT,
                    msg,
                    state.message_budget,
                );
            } else {
                suite_outcome = SuiteOutcome::Inconclusive;
                suite_body = "## Summary\nsuite.enabled but no tester slot was prepared\n".into();
                let _ = crate::bus::broadcast(
                    paths,
                    Some(&state.id),
                    crate::bus::ORCHESTRATOR_SEAT,
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
                    crate::bus::ORCHESTRATOR_SEAT,
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
                crate::bus::ORCHESTRATOR_SEAT,
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
        // One head per panel, resolved once after the suite ran: the commit the
        // panel was dispatched against is the contract every verdict is judged
        // against. A rotated re-dispatch inside the panel keeps the same head;
        // widened reviewers join a later round's panel with its freshly resolved
        // head. Unresolvable (review_cwd falls back to a possibly non-repo
        // project_root) fails OPEN, loudly: failing closed there would brick such
        // runs permanently with no reviewer fault.
        if let Some(wt) = state.worktrees.iter().find(|w| w.slot_id == impl_slot.id) {
            if let Some(msg) =
                worktree::restore_branch(&wt.path, &wt.branch, state.base_commit.as_deref())
            {
                eprintln!("warning: {msg}");
                let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            }
        }
        let panel_head = worktree::git_out(&review_cwd, &["rev-parse", "HEAD"]);
        if panel_head.is_none() {
            let msg = format!(
                "review panel sha check skipped: could not resolve HEAD in {}",
                review_cwd.display()
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
        }
        let scoped_findings = scoped_review_section(state, cfg, &review_cwd);
        // A ceiling-cut round reaches the panel on a partial WIP commit: the
        // reviewers judge what is here, and the note rides along so a missing
        // surface reads as unfinished work, not a regression.
        let prior_findings = match &ceiling_partial_note {
            Some(note) if scoped_findings.is_empty() => note.clone(),
            Some(note) => format!("{note}\n\n{scoped_findings}"),
            None => scoped_findings.clone(),
        };
        // Read per panel, so a waiver added while the run is live counts from the next
        // gate on. The contract stays frozen (O43); the waiver is the operator's (O121).
        let rulings = crate::rules::for_run(paths, &state.id);
        let criteria = crate::rules::unwaived(&frozen.criteria, &rulings);
        for (id, r) in crate::rules::waived(&rulings) {
            if !frozen.criteria.iter().any(|c| c == id) {
                continue;
            }
            let msg = format!("{id} waived by operator ruling {}: {}", r.id, r.text);
            eprintln!("note: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
        }
        // A reviewer seat with no usable provider is stood down, not parked: the
        // panel keeps running its other seats. Only a panel with no usable
        // seat left parks the run (O153).
        let mut stood_down: Vec<String> = Vec::new();
        for rev in &reviewers {
            // Stop boundary: before each reviewer job.
            if should_stop(paths, &state.id) {
                return stop_now(state, paths);
            }
            let rev_id = match recheck_seat(state, paths, cfg, &rev.id, false)? {
                SeatCheck::Ready(id) => id,
                SeatCheck::Park(msg) => {
                    let note = format!("reviewer seat `{}` stood down: {msg}", rev.id);
                    eprintln!("warning: {note}");
                    let _ =
                        crate::events::append(paths, &state.id, &crate::events::Event::info(note));
                    if let Some(s) = state.slot_mut(&rev.id) {
                        s.stood_down = Some(msg.clone());
                    }
                    state.save(paths)?;
                    stood_down.push(msg);
                    continue;
                }
            };
            let mut rev = state
                .slots
                .iter()
                .find(|s| s.id == rev_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("reviewer `{rev_id}` vanished"))?;
            let rev_cwd = reviewer_cwd(
                state,
                paths,
                cfg,
                &rev.id,
                panel_head.as_deref(),
                &review_cwd,
            );
            if let Some(s) = state.slot_mut(&rev.id) {
                s.status = SlotStatus::Pending;
                s.cwd = Some(rev_cwd.clone());
            }
            let mut extra = HashMap::new();
            extra.insert("review_cwd".into(), rev_cwd.display().to_string());
            if !suite_body.is_empty() {
                extra.insert("suite_body".into(), suite_body.clone());
            }
            extra.insert("suite_guidance".into(), suite_guidance.clone());
            extra.insert("plan_body".into(), plan_body.clone());
            extra.insert("test_contract_body".into(), test_contract_body.clone());
            extra.insert("contract_drift_note".into(), contract_drift_note.clone());
            extra.insert("prior_findings".into(), prior_findings.clone());
            let mut job = SlotJob {
                slot_id: rev.id.clone(),
                provider: rev.provider.clone(),
                role: SlotRole::Reviewer,
                template: "reviewer".into(),
                extra_vars: extra,
                expected_artifact: Some(format!("review-{}.md", rev.id)),
                model: None,
                effort: None,
            };
            let mut review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
            if !review_ok {
                // Stop boundary: don't re-dispatch a killed reviewer as a "failure".
                if should_stop(paths, &state.id) {
                    return stop_now(state, paths);
                }
                let infra = state
                    .slots
                    .iter()
                    .find(|s| s.id == rev.id)
                    .and_then(reviewer_infra_fault);
                if infra.is_some() {
                    drop_salvaged_verdict(paths, &state.id, &rev.id);
                }
                // Re-point and re-run once before treating the failure as anything.
                if let Some(id) = try_rotate_reviewer_provider(
                    state,
                    paths,
                    &rev.id,
                    &review_cwd,
                    cfg,
                    infra.is_some(),
                )? {
                    let rev_cwd =
                        reviewer_cwd(state, paths, cfg, &id, panel_head.as_deref(), &review_cwd);
                    if let Some(s) = state.slot_mut(&id) {
                        s.status = SlotStatus::Pending;
                        s.error = None;
                        s.cwd = Some(rev_cwd.clone());
                    }
                    job.extra_vars
                        .insert("review_cwd".into(), rev_cwd.display().to_string());
                    rev = state
                        .slots
                        .iter()
                        .find(|s| s.id == id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("reviewer `{id}` vanished"))?;
                    job.slot_id = rev.id.clone();
                    job.provider = rev.provider.clone();
                    job.expected_artifact = Some(format!("review-{}.md", rev.id));
                    review_ok = executor::run_slot(state, paths, cfg, &job).is_ok();
                }
            }
            if !review_ok {
                let fault = state
                    .slots
                    .iter()
                    .find(|s| s.id == rev.id)
                    .and_then(reviewer_infra_fault);
                if let Some(reason) = fault {
                    // The panel could not answer, which says nothing about the work: no
                    // verdict is recorded and no fix round is spent (O103).
                    drop_salvaged_verdict(paths, &state.id, &rev.id);
                    let msg = format!(
                        "review slot `{}` could not run on {}: {reason}. the provider could not serve this reviewer; no verdict recorded",
                        rev.id, rev.provider
                    );
                    eprintln!("error: {msg}");
                    let _ = crate::events::append(
                        paths,
                        &state.id,
                        &crate::events::Event::slot_note(&rev.id, msg.clone()),
                    );
                    return fail(state, paths, anyhow::anyhow!(msg), true);
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
                            "{}\n## Verdict\nrequest_changes\n\n{acceptance}## Findings\n- severity: major — review slot `{}` failed or produced no artifact\n",
                            review_result::SYNTHETIC_REVIEW_MARKER,
                            rev.id
                        ),
                    );
                }
                // Whatever sits in a failed slot's artifact (an earlier round's stale
                // verdict, a partial review, even a copied marker) is not what the gate
                // counted: it counted spar's own stand-in, so that is what is recorded.
                let why = state
                    .slots
                    .iter()
                    .find(|s| s.id == rev.id)
                    .and_then(|s| s.error.clone())
                    .unwrap_or_else(|| "failed or produced no review".into());
                let res = review_result::ReviewResult {
                    verdict: Some(review_result::Verdict::RequestChanges),
                    findings: vec![review_result::Finding {
                        severity: review_result::Severity::Major,
                        file: None,
                        line: None,
                        text: format!("review slot `{}`: {why}", rev.id),
                        synthetic: true,
                        follow_up: false,
                    }],
                    synthetic: true,
                    ..Default::default()
                };
                review_result::record_review(state, paths, &rev.id, &res);
            } else if let Some(text) = review_text {
                // Fail closed: only an anchored `## Verdict` / approve clears the gate,
                // and every contract criterion must be reported as passing (O19/O20).
                let res = review_result::ReviewResult {
                    scoped: !scoped_findings.is_empty(),
                    ..review_result::parse_review(&text)
                };
                review_result::record_review(state, paths, &rev.id, &res);
                // The semantic layer above the mtime gate: a verdict that names no
                // commit, or one about a commit that is not the panel head, is not
                // a verdict — treated exactly like a missing review.
                match decide_reviewed_commit(panel_head.as_deref(), &res) {
                    ReviewedCommitDecision::Skipped => {}
                    ReviewedCommitDecision::Match => {
                        if let Some(s) = state.slot_mut(&rev.id) {
                            s.reviewed_commit = res.reviewed_commit.clone();
                        }
                        let _ = crate::events::append(
                            paths,
                            &state.id,
                            &crate::events::Event::slot_note(
                                &rev.id,
                                format!(
                                    "judged commit {}",
                                    res.reviewed_commit.as_deref().unwrap_or("?")
                                ),
                            ),
                        );
                    }
                    ReviewedCommitDecision::Absent => {
                        any_request_changes = true;
                        let head = panel_head.as_deref().unwrap_or("unknown");
                        blockers.push(format!(
                            "review `{}` names no commit (panel head {head})",
                            rev.id
                        ));
                        fail_review_slot_on_commit(
                            state,
                            paths,
                            &rev.id,
                            format!(
                                "review names no commit (panel head {head}); a verdict that names no commit cannot be counted"
                            ),
                        );
                    }
                    ReviewedCommitDecision::Mismatch => {
                        any_request_changes = true;
                        let judged = res.reviewed_commit.as_deref().unwrap_or("?");
                        let head = panel_head.as_deref().unwrap_or("unknown");
                        let mut blocker = format!(
                            "review `{}` judged {judged}, panel dispatched against {head}",
                            rev.id
                        );
                        // A mid-panel commit visible at a glance: current HEAD as a
                        // third value when it moved past the panel head.
                        if let Some(cur) = worktree::git_out(&review_cwd, &["rev-parse", "HEAD"]) {
                            if cur != head {
                                blocker.push_str(&format!(" [current HEAD {cur}]"));
                            }
                        }
                        blockers.push(blocker);
                        fail_review_slot_on_commit(
                            state,
                            paths,
                            &rev.id,
                            format!("review judged {judged}, panel dispatched against {head}"),
                        );
                    }
                }
                if res.blocks(cfg.review.block_on) {
                    any_request_changes = true;
                    blockers.push(format!(
                        "review `{}` requested changes: {}",
                        rev.id,
                        review_path.display()
                    ));
                } else if !res.approves() {
                    let _ = crate::events::append(
                        paths,
                        &state.id,
                        &crate::events::Event::slot_note(
                            &rev.id,
                            format!(
                                "request_changes counted as approve-with-notes: no finding at or above block_on = {}",
                                cfg.review.block_on.as_str()
                            ),
                        ),
                    );
                }
                if acceptance_blocks_ship(&criteria, &res, cfg) {
                    any_request_changes = true;
                    let reasons = acceptance_block_reasons(&criteria, &res, cfg);
                    // Into the next round's prompt, not only onto the bus: without this
                    // the fix round's implementer prompt is byte-identical to round 1's
                    // and never learns which criterion it failed.
                    for r in &reasons {
                        blockers.push(format!("acceptance (review `{}`): {r}", rev.id));
                    }
                    let _ = crate::bus::broadcast(
                        paths,
                        Some(&state.id),
                        crate::bus::ORCHESTRATOR_SEAT,
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
        // Every reviewer stood down: the panel has no usable seat, so the run
        // parks exactly as a lone reviewer with no backup always has.
        if !reviewers.is_empty() && stood_down.len() == reviewers.len() {
            return fail(state, paths, anyhow::anyhow!(stood_down.join("; ")), true);
        }
        // A stood-down seat's recorded veto still blocks when it rejects the
        // head about to ship: judged, unanswered, and unable to re-judge. The
        // same verdict and acceptance predicates judge it as a fresh artifact.
        let mut carried: Vec<(String, review_result::ReviewResult)> = Vec::new();
        for s in state.slots.iter().filter(|s| s.role == SlotRole::Reviewer) {
            if let Some(res) = carried_rejection(s, panel_head.as_deref()) {
                carried.push((s.id.clone(), res.clone()));
            }
        }
        for (id, res) in &carried {
            if res.blocks(cfg.review.block_on) || acceptance_blocks_ship(&criteria, res, cfg) {
                any_request_changes = true;
                blockers.push(format!(
                    "review `{id}` stood down on quota; its recorded request_changes on {} still blocks this head",
                    res.reviewed_commit.as_deref().unwrap_or("unknown")
                ));
            }
        }
        // The next round continues from the partial WIP commit, so its
        // implementer prompt carries the cut-short notice as a blocker: without
        // this the fix round would start from partial work with no idea it is.
        if let Some(note) = ceiling_partial_note.clone() {
            blockers.push(note);
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
        apply_queued_round_extension(state, paths);
        if state.round_ceiling_reached() && escalate_round_ceiling(state, paths)? {
            return Ok(());
        }
        // A fix pass re-dispatches the implementer and the panel, so it is a round of
        // work in its own right (O45) — `fix_rounds` bounds the loop, `round` counts
        // what happened on the run.
        state.begin_round();
        state.set_phase(Phase::Fix);
        state.save(paths)?;
        let run_id = state.id.clone();
        crate::providers::delivery::refresh_run_poll_files(paths, &run_id);
    }
}

/// What committing an implementer worktree's leftovers onto its branch found.
enum LeftoverCommit {
    Committed(String),
    Clean,
    Refused(String),
}

/// Commit what the implementer left uncommitted, overlaid acceptance tests included,
/// on its branch before the suite and the panel run (O138): reviewers judge a detached
/// checkout of `HEAD`, the suite runs on that same commit, and `ship` pushes it. The
/// subject is the PR title, so it reads right as a squash subject. A failure (a hook
/// that refuses) is said out loud and the round goes on: the panel then judges `HEAD`.
/// `None` when there is no worktree to commit on: a dry run, a missing dir, or
/// `.spar` internals. The commit message stays with the caller; the guard set
/// does not drift between the review commit and the ceiling-kill WIP commit.
fn commit_branch_leftovers(
    state: &RunState,
    paths: &SparPaths,
    impl_id: &str,
    subject: &str,
    body: &str,
) -> Option<(String, LeftoverCommit)> {
    let wt = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == impl_id)?
        .clone();
    if state.dry_run || !wt.path.is_dir() || wt.path.starts_with(state.project_root.join(".spar")) {
        return None;
    }
    if let Some(msg) = worktree::restore_branch(&wt.path, &wt.branch, state.base_commit.as_deref())
    {
        eprintln!("warning: {msg}");
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
    }
    let outcome = match worktree::commit_leftovers(&wt.path, subject, body) {
        Ok(Some(sha)) => LeftoverCommit::Committed(sha),
        Ok(None) => LeftoverCommit::Clean,
        Err(e) => LeftoverCommit::Refused(e),
    };
    Some((wt.branch, outcome))
}

/// The refreshed cut-short notice when the commit below banks a ceiling-cut
/// round's leftovers: the reviewers and the next round read this instead of
/// the kill-time note, so a Refused round whose work did land never claims it
/// is uncommitted. `None` when no notice needs refreshing.
fn commit_before_review(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    impl_id: &str,
) -> Option<String> {
    let subject = crate::ship::pr_title(state.task.as_deref(), cfg.ship.title_max);
    let body = format!(
        "spar: uncommitted implementer work before review round {}",
        state.round
    );
    let round = state.round;
    let (branch, outcome) = commit_branch_leftovers(state, paths, impl_id, &subject, &body)?;
    match outcome {
        LeftoverCommit::Committed(sha) => {
            let note = format!(
                "committed uncommitted implementer work on {branch} as {} before review round {round}",
                sha.chars().take(8).collect::<String>(),
            );
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(note));
            // A ceiling-cut round whose WIP commit the hooks refused banks here
            // instead: HEAD now carries the work, so the round reads as WIP
            // rather than Refused. Rounds without a partial mark leave none behind.
            if let Some(entry) = state
                .ceiling_partial_rounds
                .iter_mut()
                .find(|e| e.round == round)
            {
                entry.kind = CeilingPartialKind::Wip;
                entry.commit = Some(sha);
                let _ = state.save(paths);
                return Some(ceiling_partial_notice(round));
            }
            None
        }
        LeftoverCommit::Clean => None,
        LeftoverCommit::Refused(e) => {
            let msg = format!(
                "could not commit uncommitted implementer work before review round {round}: {e}; \
                 the panel judges HEAD without it"
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            None
        }
    }
}

/// What the reviewers and the next round are told when the implementer is cut
/// short at its hard ceiling: the commit under review is partial work, saved so
/// the round can be judged and continued rather than thrown away.
fn ceiling_partial_notice(round: u32) -> String {
    format!(
        "NOTE: the implementer pass for round {round} was cut short at its hard time \
         ceiling before it could finish, so the commit under review is partial work \
         (saved as a WIP commit). Judge what is here and continue it; unmet task scope \
         or acceptance criteria are still request_changes reasons, not an approved round."
    )
}

/// What the reviewers and the next round are told when the implementer is cut
/// short at its hard ceiling after committing its work: the commit under review
/// is partial work, but the tree was clean so no WIP commit was written.
fn ceiling_partial_clean_notice(round: u32) -> String {
    format!(
        "NOTE: the implementer pass for round {round} was cut short at its hard time \
         ceiling after committing its work; the tree was clean, so no WIP commit \
         was written. Judge what is here and continue it; unmet task scope \
         or acceptance criteria are still request_changes reasons, not an approved round."
    )
}

/// What the next round is told when the previous implementer was cut short at
/// its hard ceiling and its leftovers could not be committed: no WIP commit
/// exists, so the notice must not claim one, and the panel judges a checkout of
/// HEAD, so it must never be told to judge an uncommitted tree. The live kill
/// names the commit error; the rebuild has none to name.
fn ceiling_partial_refused_notice(round: u32, detail: Option<&str>) -> String {
    let cause = match detail {
        Some(e) => format!(" ({e})"),
        None => String::new(),
    };
    format!(
        "NOTE: the implementer pass for round {round} was cut short at its hard time \
         ceiling and its partial work could not be committed{cause}; judge the HEAD commit \
         as partial work and continue the task from there."
    )
}

/// Commit an implementer's leftovers after a hard-ceiling kill as a WIP round and
/// report the notice for the reviewers and the next round. `None` when the kill
/// was not at the ceiling, left no work at all, or the round's banked commit no
/// longer sits at HEAD: those rounds fail as before.
///
/// A resume that idles into a second ceiling kill returns the carried bank's
/// notice (a WIP or Clean one) instead of failing: the tree is clean and HEAD
/// is still the banked commit, so the banked partial is still there and the
/// run goes on like the first hit, whether the resume stayed in the bank's
/// round or opened a new one.
///
/// The message names the ceiling kill, so the WIP commit reads as an interrupted
/// round on the branch rather than a normal one. Like `commit_before_review`, a
/// commit the hooks refuse is said out loud and the round still goes on.
fn try_commit_ceiling_partial(
    state: &mut RunState,
    paths: &SparPaths,
    impl_id: &str,
) -> Option<(String, CeilingPartialKind, Option<String>)> {
    if executor::slot_timeout_cause(state, impl_id) != Some(executor::TimeoutCause::HardCeiling) {
        return None;
    }
    let wt_path = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == impl_id)
        .map(|w| w.path.clone())?;
    // The round's start HEAD and tree, never the run base: from round 2 on the base
    // predates earlier rounds' commits, so the base always reads as work. The tree
    // snapshot matters too: round 1 starts dirty (the test author's files land
    // uncommitted) and a Refused round leaves leftovers, so a dirty tree alone
    // cannot mean this round did anything. Without a recorded start only a dirty
    // tree can be judged.
    let (since, since_tree) = state
        .slots
        .iter()
        .find(|s| s.id == impl_id)
        .map(|s| (s.dispatch_head.clone(), s.dispatch_tree.clone()))
        .unwrap_or_default();
    let round = state.round;
    // The latest banked partial at or before this round. A same-round resume
    // carries its own round's bank; a resume that opened a new round (the
    // previous round parked, e.g. on quota) carries the previous round's bank
    // when the new round adds nothing before its own ceiling kill. In both
    // cases HEAD sitting on the bank with a clean tree reads as no new work,
    // and the run goes on with the banked commit instead of failing.
    let carried = state
        .ceiling_partial_rounds
        .iter()
        .rev()
        .find(|e| e.round <= round);
    let banked = carried.and_then(|e| e.commit.clone());
    if !executor::dispatch_left_work(
        &wt_path,
        since.as_deref(),
        since_tree.as_deref(),
        banked.as_deref(),
    ) {
        // A resume that idles into a second ceiling kill: the tree is clean and
        // HEAD is still the carried banked commit, so there is no new work, but
        // the banked partial is still there. Go on like the first hit instead
        // of failing the round. The notice names the bank's round, since that
        // is the round whose work is under review. The HEAD check keeps the
        // notice honest: an agent that reset HEAD away from the bank before the
        // second kill no longer carries that commit, so the bank says nothing.
        let banked_entry = state
            .ceiling_partial_rounds
            .iter()
            .rev()
            .find(|e| e.round <= round)?;
        let bank_round = banked_entry.round;
        let commit = banked_entry.commit.clone()?;
        if worktree::git_out(&wt_path, &["rev-parse", "HEAD"]).as_deref() != Some(commit.as_str()) {
            return None;
        }
        let notice = match banked_entry.kind {
            CeilingPartialKind::Wip => ceiling_partial_notice(bank_round),
            CeilingPartialKind::Clean => ceiling_partial_clean_notice(bank_round),
            CeilingPartialKind::Refused => return None,
        };
        return Some((notice, banked_entry.kind, Some(commit)));
    }
    let subject =
        format!("chore(spar): partial work, implementer cut short at hard ceiling (round {round})");
    let body = format!(
        "spar: the round {round} implementer was killed at its hard time ceiling before it \
         could finish. Its leftovers are committed here so the suite and the review panel \
         judge what is on the branch, and the next round continues from this commit."
    );
    let (branch, outcome) = commit_branch_leftovers(state, paths, impl_id, &subject, &body)?;
    let (note, notice, kind, commit) = match outcome {
        LeftoverCommit::Committed(sha) => (
            format!(
                "committed partial round-{round} implementer work on {branch} as {} (ceiling kill WIP)",
                sha.chars().take(8).collect::<String>(),
            ),
            ceiling_partial_notice(round),
            CeilingPartialKind::Wip,
            Some(sha),
        ),
        LeftoverCommit::Clean => {
            let head = worktree::git_out(&wt_path, &["rev-parse", "HEAD"]);
            if banked
                .as_deref()
                .is_some_and(|sha| head.as_deref() == Some(sha))
            {
                return None;
            }
            (
                format!(
                    "ceiling kill left committed work on {branch} with a clean tree; round {round} goes on"
                ),
                ceiling_partial_clean_notice(round),
                CeilingPartialKind::Clean,
                head,
            )
        }
        LeftoverCommit::Refused(e) => {
            let msg = format!(
                "could not commit partial round-{round} implementer work (ceiling kill WIP): {e}; \
                 the panel judges HEAD without it"
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            return Some((
                ceiling_partial_refused_notice(round, Some(&e)),
                CeilingPartialKind::Refused,
                None,
            ));
        }
    };
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(note));
    Some((notice, kind, commit))
}

/// Bank a ceiling-cut partial for review unless the slot also hit quota, in
/// which case the run parks with exit 4 instead: a review on partial work
/// could end the run at 2 or 0, and exit codes are a public contract. Returns
/// whether the loop goes on to review on a recorded partial.
fn record_ceiling_partial_unless_quota(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    note_out: &mut Option<String>,
) -> Result<bool> {
    if executor::slot_parks_on_quota(state, slot_id) {
        return Ok(false);
    }
    if let Some((note, kind, commit)) = try_commit_ceiling_partial(state, paths, slot_id) {
        record_ceiling_partial(state, paths, slot_id, note, kind, commit, note_out)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// A backup-retry implementer dispatch failed. A ceiling-cut partial round goes on
/// like the primary dispatch's; anything else fails the run with its quota routing.
/// Returns whether the loop falls through to the review phase: a recorded ceiling
/// partial continues, while a park or hard failure already ended the loop inside
/// `fail` and the caller must return from the loop function.
fn handle_retry_err(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    err: anyhow::Error,
    note_out: &mut Option<String>,
) -> Result<bool> {
    if record_ceiling_partial_unless_quota(state, paths, slot_id, note_out)? {
        return Ok(true);
    }
    let quota_hit = executor::slot_parks_on_quota(state, slot_id);
    fail(state, paths, err, quota_hit)?;
    Ok(false)
}

/// Drop this round's ceiling-cut mark when its implementer finishes without a
/// ceiling kill: the round is no longer cut short, so ship judges the real
/// summary and the next round gets no stale notice. Keyed on the dispatch, not
/// the verdict (a finished round the panel rejects still finished). Older
/// rounds keep their marks for the PR's Partial work section.
fn clear_finished_round_partial_mark(state: &mut RunState, paths: &SparPaths) -> Result<()> {
    let before = state.ceiling_partial_rounds.len();
    state
        .ceiling_partial_rounds
        .retain(|e| e.round != state.round);
    if state.ceiling_partial_rounds.len() != before {
        state.save(paths)?;
    }
    Ok(())
}

/// Record a ceiling-cut partial round: the killed slot reads done (the kill stays
/// in the event log, not as a failure string on a done slot) and the round is
/// remembered on the run so a fresh process rebuilds the cut-short notice from
/// disk for the next round's implementer. The kind records how the work was left
/// so the rebuilt notice words the WIP commit honestly.
fn record_ceiling_partial(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    note: String,
    kind: CeilingPartialKind,
    commit: Option<String>,
    out: &mut Option<String>,
) -> Result<()> {
    eprintln!("note: {note}");
    // The slot reads done now, so the executor's failed dispatch verdict for it is
    // stale: verdict markers only steer running slots, and anything else reading
    // them must not see a failed implementer for a round that went on.
    crate::markers::clear_slot(paths, &state.id, slot_id);
    if let Some(s) = state.slot_mut(slot_id) {
        s.status = SlotStatus::Done;
        s.error = None;
        s.timeout_cause = None;
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot(slot_id, SlotStatus::Done),
    );
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot_note(slot_id, note.clone()),
    );
    let _ = crate::bus::broadcast(
        paths,
        Some(&state.id),
        "orchestrator",
        note.clone(),
        state.message_budget,
    );
    if let Some(entry) = state
        .ceiling_partial_rounds
        .iter_mut()
        .find(|e| e.round == state.round)
    {
        entry.kind = kind;
        entry.commit = commit;
    } else {
        state.ceiling_partial_rounds.push(CeilingPartialRound {
            round: state.round,
            kind,
            commit,
        });
    }
    state.save(paths)?;
    *out = Some(note);
    Ok(())
}

/// The tree reviewer `seat` judges from: its own detached checkout at the panel head
/// (O138), so a reviewer that checks out or commits anything moves nothing the
/// implementer or `ship` reads (#158). The implementer's tree when there is no real
/// worktree to detach from (dry run, `isolation = "none"`, a head that did not resolve),
/// and, loudly, when the checkout cannot be made: `restore_branch` stays the net there.
fn reviewer_cwd(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    seat: &str,
    panel_head: Option<&str>,
    review_cwd: &Path,
) -> PathBuf {
    let Some(head) = panel_head else {
        return review_cwd.to_path_buf();
    };
    let real_worktree = state
        .worktrees
        .iter()
        .any(|w| w.path == review_cwd && !w.path.starts_with(state.project_root.join(".spar")));
    if state.dry_run || !real_worktree {
        return review_cwd.to_path_buf();
    }
    match worktree::reviewer_checkout(state, seat, head, &cfg.worktree.copy) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!(
                "reviewer `{seat}` judges from the implementer's worktree: its own checkout \
                 failed ({e:#})"
            );
            eprintln!("warning: {msg}");
            let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
            review_cwd.to_path_buf()
        }
    }
}

/// The panel position a reviewer seat id encodes (`review-1-cli-codex` is 1), which is
/// the index into `[backups].reviewer`. `None` for ids that carry none (`review-cli-grok-wide`).
fn reviewer_ordinal(slot_id: &str) -> Option<usize> {
    slot_id
        .strip_prefix("review-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Put another provider in a seat. A reviewer seat is renamed to name its new vendor
/// (O85); the implementer keeps its id because its worktree and branch are keyed on it.
/// Returns the seat's id after the move. The caller saves.
fn move_seat(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    pin: &crate::provider_ref::Pin,
    source: SeatSource,
) -> Result<String> {
    let role = state
        .slots
        .iter()
        .find(|s| s.id == slot_id)
        .map(|s| s.role)
        .ok_or_else(|| anyhow::anyhow!("unknown slot {slot_id}"))?;
    if let Some(s) = state.slot_mut(slot_id) {
        s.point_at(&pin.display());
        // A level the old CLI took may not exist on the new one; the dispatch then
        // resolves the role's level from config.
        if executor::check_slot_effort(&pin.display(), s.effort).is_err() {
            s.effort = None;
        }
        s.source = Some(source);
    }
    let new_id = moved_seat_id(role, slot_id, &pin.provider);
    if new_id != slot_id && state.slots.iter().all(|s| s.id != new_id) {
        state.rename_slot(paths, slot_id, &new_id);
        return Ok(new_id);
    }
    Ok(slot_id.to_string())
}

/// The id a seat takes when `provider` fills it: reviewer and tester ids name their
/// provider (O80), the implementer keeps `impl`.
fn moved_seat_id(role: SlotRole, slot_id: &str, provider: &str) -> String {
    match (role, reviewer_ordinal(slot_id)) {
        (SlotRole::Reviewer, Some(ordinal)) => {
            format!("review-{ordinal}-{}", sanitize_slot(provider))
        }
        (SlotRole::Tester, _) => format!("suite-{}", sanitize_slot(provider)),
        (SlotRole::Reviewer, None) if slot_id.ends_with("-wide") => {
            format!("review-{}-wide", sanitize_slot(provider))
        }
        _ => slot_id.to_string(),
    }
}

fn move_seat_to_backup(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    backup_raw: &str,
) -> Result<String> {
    let pin = crate::provider_ref::Pin::parse(backup_raw)?;
    let id = move_seat(state, paths, slot_id, &pin, SeatSource::Backup)?;
    state.save(paths)?;
    Ok(id)
}

/// Why a failed reviewer dispatch is an infrastructure failure rather than a review
/// (#155, O103): the provider could not serve it (quota, auth), the CLI never started,
/// or it exited having run no tools, which on a native CLI means it never read the
/// diff. `None` for a reviewer that genuinely ran and still failed: that keeps the
/// fail-closed `request_changes` it always had. A kill (signal) is not infra either:
/// a hung CLI ran into its ceiling, and `stop` is handled before this is asked.
fn reviewer_infra_fault(slot: &SlotState) -> Option<String> {
    if slot.status != SlotStatus::Failed {
        return None;
    }
    if slot.quota_hit {
        return Some("rate-limited".into());
    }
    let err = slot.error.as_deref().unwrap_or("failed");
    if slot.auth_fault {
        return Some(format!("auth: {err}"));
    }
    if slot.exec_backend == Some(crate::provider_ref::ExecBackend::ApiSdk) || slot.signal.is_some()
    {
        return None;
    }
    match &slot.usage {
        None if slot.exit_code.is_none() => Some(format!("could not start: {err}")),
        Some(u) if u.tools == 0 => Some(format!("ran no tools (tools == 0): {err}")),
        _ => None,
    }
}

/// Remove the `request_changes` stub `salvage_expected_artifact` writes for a failed
/// reviewer, when the failure was the provider's: that stub is a verdict nobody gave.
/// Only the stub is removed; a review the slot wrote itself is left alone.
fn drop_salvaged_verdict(paths: &SparPaths, run_id: &str, slot_id: &str) {
    let path = paths.artifact(run_id, &format!("review-{slot_id}.md"));
    let is_stub =
        std::fs::read_to_string(&path).is_ok_and(|t| t.contains(executor::REVIEW_SALVAGE_FINDING));
    if is_stub {
        let _ = std::fs::remove_file(&path);
    }
}

enum SeatCheck {
    Ready(String),
    Park(String),
}

/// Run-start quota gate: only the implementer must run next at run start, so
/// only it can park the run here. `recheck_seat` moves it onto an eligible
/// backup when it has one; a seat with no usable provider parks the run, and
/// the returned message names it (O153).
fn gate_implementer_seat(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<Option<String>> {
    let impl_id: Option<String> = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .map(|s| s.id.clone());
    match impl_id {
        None => Ok(None),
        Some(id) => match recheck_seat(state, paths, cfg, &id, true)? {
            SeatCheck::Ready(id) => {
                // Quota-clean but the CLI may still be missing: the old run-start
                // gate parked then too, once backups exist to detect against.
                let has_backup = cfg.backups.implementer.is_some()
                    || !cfg.backups.reviewer.is_empty()
                    || cfg.backups.tester.is_some();
                if !has_backup {
                    return Ok(None);
                }
                let detected: std::collections::HashSet<String> = crate::providers::detect_all()
                    .into_iter()
                    .filter(|r| r.available)
                    .map(|r| format!("cli:{}", r.name))
                    .collect();
                let available = if detected.is_empty() {
                    None
                } else {
                    Some(detected)
                };
                let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                let route = state.recorded_route();
                let missing = state
                    .slots
                    .iter()
                    .find(|s| s.id == id)
                    .filter(|s| {
                        !crate::backup::is_provider_eligible(
                            &s.provider_ref().display(),
                            &store,
                            available.as_ref(),
                            route,
                        )
                    })
                    .map(|s| s.provider.clone());
                match missing {
                    Some(provider) => Ok(Some(format!(
                        "provider for `{id}` not available ({provider}). \
                         install the CLI or set `--select`/`--providers` to a usable one"
                    ))),
                    None => Ok(None),
                }
            }
            SeatCheck::Park(msg) => Ok(Some(msg)),
        },
    }
}

/// Re-read pause state right before a dispatch (#175, O106). Pauses used to be read
/// only when a run started or resumed, so `provider pause grok --until 2027-01-01` did
/// not stop a live run from dispatching its next grok reviewer. A seat whose provider
/// is no longer eligible moves to its declared backup, or the run parks.
fn recheck_seat(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    slot_id: &str,
    // A start-of-run gate parks on any pause (O106: a weak pause still steers the
    // next run's seat choice); a live re-check trusts only manual and strong pauses.
    strict: bool,
) -> Result<SeatCheck> {
    let Some(slot) = state.slots.iter().find(|s| s.id == slot_id).cloned() else {
        return Ok(SeatCheck::Ready(slot_id.to_string()));
    };
    // Dry runs skip pause gating, as the run-start gate always has.
    if state.dry_run {
        return Ok(SeatCheck::Ready(slot_id.to_string()));
    }
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    let seat = slot.provider_ref();
    let key = seat.quota_key(state.recorded_route());
    let sub = seat.storage_key();
    // A live run trusts only pauses it can trust: manual and strong (O106). A
    // weak heuristic pause ("429" in "429 passed") written by this very dispatch
    // must not park it or reseat it — but a weak pause written before this
    // dispatch started steers the seat onto its backup, so the first dispatch
    // reads it strict while later dispatches stay trust-only. The line is the
    // dispatch start, not the run creation: one id threads plan, implement, and
    // every round (O45), so `created_at` long predates any later round.
    let horizon = state.round_started_at.unwrap_or(state.created_at);
    let blocked = if strict {
        !store.is_usable(&key)
    } else if store.blocks_live_dispatch(&key) {
        true
    } else {
        !store.is_usable(&key) && store.get(&key).updated_at.is_none_or(|at| at < horizon)
    };
    if !blocked {
        return Ok(SeatCheck::Ready(slot_id.to_string()));
    }
    let ordinal = match slot.role {
        SlotRole::Reviewer => reviewer_ordinal(slot_id),
        _ => Some(0),
    };
    let route = state.recorded_route();
    let backup = ordinal
        .and_then(|o| crate::backup::backup_for_role(slot.role, o, cfg))
        .filter(|b| crate::quota::sub_key(b) != sub)
        .filter(|b| crate::backup::backup_eligible(b, &store, None, paths, state.dry_run, route));
    let Some(backup_raw) = backup else {
        return Ok(SeatCheck::Park(format!(
            "provider {key} is paused or on cooldown and `{slot_id}` has no eligible backup. \
             resume with `spar provider resume {key}` or reassign the role"
        )));
    };
    let new_id = move_seat_to_backup(state, paths, slot_id, &backup_raw)?;
    let note = format!("{key} is paused; `{new_id}` moved to its backup {backup_raw}");
    eprintln!("note: {note}");
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot_note(&new_id, note),
    );
    Ok(SeatCheck::Ready(new_id))
}

/// Change implementer **provider** only; keep stable slot id and worktree.
fn try_rotate_implementer(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<bool> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Implementer)
        .cloned();
    let Some(slot_state) = slot else {
        return Ok(false);
    };
    let cur = slot_state.provider.clone();
    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .map(|s| s.provider.clone())
        .collect();
    {
        let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        let has_backup = cfg.backups.implementer.is_some();
        let available_opt: Option<std::collections::HashSet<String>> =
            if state.dry_run || !has_backup {
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
        let cause = crate::backup::dispatch_stop_cause(
            &slot_state,
            &store,
            available_opt.as_ref(),
            state.recorded_route(),
        );
        if has_backup {
            if cause.engages_backup() {
                if let Some(backup_raw) =
                    crate::backup::backup_for_role(SlotRole::Implementer, 0, cfg)
                {
                    let backup_key = crate::provider_ref::ProviderRef::parse(&backup_raw)
                        .map(|r| r.storage_key())
                        .unwrap_or_else(|_| backup_raw.clone());
                    let used_contains_backup = used.iter().any(|u| {
                        crate::provider_ref::ProviderRef::parse(u)
                            .map(|r| r.storage_key())
                            .unwrap_or_else(|_| u.clone())
                            == backup_key
                    });
                    if !used_contains_backup
                        && crate::backup::backup_eligible(
                            &backup_raw,
                            &store,
                            available_opt.as_ref(),
                            paths,
                            state.dry_run,
                            state.recorded_route(),
                        )
                        && slot_state.source != Some(SeatSource::Backup)
                    {
                        let backup_pin = crate::provider_ref::Pin::parse(&backup_raw)
                            .map(|p| p.display())
                            .unwrap_or(backup_raw.clone());
                        let impl_id = slot_state.id.clone();
                        if let Some(s) = state.slot_mut(&impl_id) {
                            s.point_at(&backup_pin);
                            s.source = Some(SeatSource::Backup);
                        }
                        state.save(paths)?;
                        return Ok(true);
                    }
                }
                return Ok(false);
            } else {
                return Ok(false);
            }
        }
    }
    // Candidate order: [roles].implementer, then [providers].order, then the live fleet.
    // Rotation must report the rung it actually drew from (`SlotState::point_at` only
    // touches provider/model), not leave the slot claiming its original source.
    let implementer_role_source = if cfg
        .cli_role_keys
        .contains(SlotRole::Implementer.as_config_key())
    {
        SeatSource::CliRole
    } else {
        SeatSource::RolesFile
    };
    // The live-fleet leg resolves each pool position honestly through `resolve_seat`
    // instead of stamping every position with `state.pool_origin.as_seat_source()`: a
    // `Synth` pool mixes `[roles]` and `[providers].order` seats, and the blanket guess
    // mislabels whichever positions did not actually take that rung.
    let pool_sources = crate::workflow::roles_resolve::resolve_seat_sources(
        SlotRole::Implementer,
        state.providers.len(),
        &state.providers,
        state.pool_origin,
        cfg,
    );
    let next = cfg
        .roles
        .implementer
        .iter()
        .map(|s| (s.as_str(), implementer_role_source))
        .chain(
            cfg.providers
                .order
                .iter()
                .map(|s| (s.as_str(), SeatSource::ProvidersOrder)),
        )
        .chain(
            state
                .providers
                .iter()
                .map(|s| s.as_str())
                .zip(pool_sources.iter().copied()),
        )
        .find(|(p, _)| *p != cur.as_str() && !used.iter().any(|u| u == p))
        .map(|(p, src)| (p.to_string(), src));
    let Some((next, source)) = next else {
        return Ok(false);
    };
    let impl_id = slot_state.id.clone();
    if let Some(s) = state.slot_mut(&impl_id) {
        s.point_at(&next);
        s.source = Some(source);
    }
    state.save(paths)?;
    Ok(true)
}

/// Add an extra adversarial reviewer from a provider not already reviewing.
fn try_widen_reviewers(
    state: &mut RunState,
    paths: &SparPaths,
    review_cwd: &std::path::Path,
    cfg: &Config,
) -> Result<bool> {
    // A pinned panel (CLI `--role reviewer=…` or file `[roles].reviewer`) is an
    // exclusion list, not a floor: widening it must duplicate a pin the run actually
    // dispatched, never import a provider from `[providers].order` the operator
    // deliberately left out (Bug A). Compare by storage key so a `cli:codex@terra` pin
    // recognises the slot it created (`provider=cli:codex, model=terra`) as itself.
    if crate::workflow::roles_resolve::reviewer_panel_pinned(cfg) {
        let pins: Vec<String> = cfg
            .roles
            .reviewer
            .iter()
            .map(|p| {
                crate::provider_ref::ProviderRef::parse(p)
                    .map(|r| r.storage_key())
                    .unwrap_or_else(|_| p.clone())
            })
            .collect();
        let dispatched = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Reviewer && pins.iter().any(|p| p == &s.provider))
            .or_else(|| state.slots.iter().find(|s| s.role == SlotRole::Reviewer));
        let Some(src) = dispatched else {
            return Ok(false);
        };
        let (provider, model, source) = (src.provider.clone(), src.model.clone(), src.source);
        let id = format!("review-{}-wide", state.slots.len());
        let mut slot = executor::init_slot_model(&id, &provider, SlotRole::Reviewer, model);
        slot.cwd = Some(review_cwd.to_path_buf());
        slot.source = source;
        state.slots.push(slot);
        state.save(paths)?;
        return Ok(true);
    }
    let existing: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Reviewer)
        .map(|s| s.provider.clone())
        .collect();
    // Unpinned: draw the next reviewer from the run's own pool first — an explicit
    // --providers/--select still outranks [providers].order (O56) even on the widen
    // path — then fall to [providers].order once the pool is exhausted. Each pool
    // position resolves its honest source through `resolve_seat` rather than a blanket
    // `pool_origin.as_seat_source()` guess, since a `Synth` pool mixes `[roles]` and
    // `[providers].order` seats. Compare by storage key: a pool entry may still carry an
    // `@model` suffix while `existing`/`slot.provider` are always model-free, so a raw
    // string comparison would miss a provider that is already reviewing and add a third
    // seat on it instead of moving on.
    let normalize = |p: &str| {
        crate::provider_ref::ProviderRef::parse(p)
            .map(|r| r.storage_key())
            .unwrap_or_else(|_| p.to_string())
    };
    let pool_sources = crate::workflow::roles_resolve::resolve_seat_sources(
        SlotRole::Reviewer,
        state.providers.len(),
        &state.providers,
        state.pool_origin,
        cfg,
    );
    let candidate = state
        .providers
        .iter()
        .cloned()
        .zip(pool_sources)
        .chain(
            cfg.providers
                .order
                .iter()
                .cloned()
                .map(|p| (p, SeatSource::ProvidersOrder)),
        )
        .find(|(p, _)| !existing.contains(&normalize(p)));
    let Some((prov, source)) = candidate else {
        // still widen with a synthetic extra reviewer on a repeated provider
        let dispatched = state
            .slots
            .iter()
            .find(|s| s.role == SlotRole::Reviewer && existing.first() == Some(&s.provider));
        let prov = existing
            .first()
            .cloned()
            .unwrap_or_else(|| "cli:claude".into());
        let source = dispatched.and_then(|s| s.source);
        let id = format!("review-{}-wide", state.slots.len());
        let mut slot = executor::init_slot(&id, &prov, SlotRole::Reviewer);
        slot.cwd = Some(review_cwd.to_path_buf());
        slot.source = source;
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
    slot.source = Some(source);
    state.slots.push(slot);
    state.save(paths)?;
    Ok(true)
}

/// Re-point a failed reviewer seat for one more dispatch. Returns the seat's id after
/// the move (a backup renames it, O85), or `None` when there is nowhere to go.
///
/// `infra` is set when the failure was the provider's, not the review's (O103): then
/// the declared `--backup` for this panel position is tried first, the way every other
/// role's environmental stop already does. Every candidate must be eligible right now:
/// rotating onto a paused provider only buys the same failure again.
fn try_rotate_reviewer_provider(
    state: &mut RunState,
    paths: &SparPaths,
    rev_id: &str,
    review_cwd: &std::path::Path,
    cfg: &Config,
    infra: bool,
) -> Result<Option<String>> {
    let Some(slot) = state.slots.iter().find(|s| s.id == rev_id).cloned() else {
        return Ok(None);
    };
    let cur = slot.provider.clone();
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    if infra && slot.source != Some(SeatSource::Backup) {
        let route = state.recorded_route();
        let backup = reviewer_ordinal(rev_id)
            .and_then(|o| crate::backup::backup_for_role(SlotRole::Reviewer, o, cfg))
            .filter(|b| crate::quota::sub_key(b) != crate::quota::sub_key(&cur))
            .filter(|b| {
                crate::backup::backup_eligible(b, &store, None, paths, state.dry_run, route)
            });
        if let Some(raw) = backup {
            let id = move_seat_to_backup(state, paths, rev_id, &raw)?;
            if let Some(s) = state.slot_mut(&id) {
                s.cwd = Some(review_cwd.to_path_buf());
            }
            state.save(paths)?;
            return Ok(Some(id));
        }
    }
    let route = state.recorded_route();
    let eligible = |p: &String| -> bool {
        *p != cur && crate::backup::is_provider_eligible(p, &store, None, route)
    };
    let reviewer_role_source = if cfg
        .cli_role_keys
        .contains(SlotRole::Reviewer.as_config_key())
    {
        SeatSource::CliRole
    } else {
        SeatSource::RolesFile
    };
    // A pinned panel is an exclusion list (Bug A): rotation must stay inside
    // `[roles].reviewer` and never reach into the run's pool or `[providers].order` for a
    // replacement, even when the pin list has only one entry and offers none. Only a real
    // pin list counts here (`reviewer_panel_has_pins`, not `reviewer_panel_pinned`): a
    // `--fleet small` narrowing with no underlying pins has nothing to rotate within and
    // must fall through like an unpinned panel. Bound the pin candidates to `panel_size`
    // too, so rotating a `small`-truncated two-pin panel duplicates the dispatched pin
    // rather than restoring the pin the preset dropped.
    let next = if crate::workflow::roles_resolve::reviewer_panel_has_pins(cfg) {
        cfg.roles
            .reviewer
            .iter()
            .take(crate::workflow::roles_resolve::panel_size(cfg))
            .find(|p| eligible(p))
            .cloned()
            .map(|p| (p, reviewer_role_source))
    } else {
        cfg.roles
            .reviewer
            .iter()
            .find(|p| eligible(p))
            .cloned()
            .map(|p| (p, reviewer_role_source))
            .or_else(|| {
                state
                    .providers
                    .iter()
                    .find(|p| eligible(p))
                    .cloned()
                    .map(|p| (p, state.pool_origin.as_seat_source()))
            })
            .or_else(|| {
                cfg.providers
                    .order
                    .iter()
                    .find(|p| eligible(p))
                    .cloned()
                    .map(|p| (p, SeatSource::ProvidersOrder))
            })
    };
    let Some((next, source)) = next else {
        return Ok(None);
    };
    if let Some(s) = state.slot_mut(rev_id) {
        s.point_at(&next);
        s.source = Some(source);
        s.cwd = Some(review_cwd.to_path_buf());
    }
    state.save(paths)?;
    Ok(Some(rev_id.to_string()))
}

#[cfg(test)]
mod moved_seat_tests {
    use super::*;

    #[test]
    fn moved_seat_ids_name_the_new_provider() {
        assert_eq!(
            moved_seat_id(SlotRole::Reviewer, "review-1-cli-codex", "cli:claude"),
            "review-1-cli-claude"
        );
        assert_eq!(
            moved_seat_id(SlotRole::Reviewer, "review-cli-grok-wide", "cli:claude"),
            "review-cli-claude-wide"
        );
        assert_eq!(
            moved_seat_id(SlotRole::Tester, "suite-cli-codex", "cli:pi"),
            "suite-cli-pi"
        );
        assert_eq!(
            moved_seat_id(SlotRole::Implementer, "impl", "cli:pi"),
            "impl"
        );
    }

    #[test]
    fn a_rename_carries_the_artifact_and_remembers_the_old_id() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        let mut slot = executor::init_slot("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        slot.artifact = Some("review-review-0-cli-codex.md".into());
        let refused: crate::effort::EffortLevel = crate::effort::EffortLevel::all()
            .iter()
            .map(|r| r.parse().unwrap())
            .find(|l| executor::check_slot_effort("cli:claude", Some(*l)).is_err())
            .expect("claude refuses some level");
        slot.effort = Some(refused);
        state.slots.push(slot);
        let pin = crate::provider_ref::Pin::parse("cli:claude").unwrap();
        let id = move_seat(
            &mut state,
            &paths,
            "review-0-cli-codex",
            &pin,
            SeatSource::Reassigned,
        )
        .unwrap();
        let s = state.slots.iter().find(|s| s.id == id).unwrap();
        assert_eq!(id, "review-0-cli-claude");
        assert_eq!(s.artifact.as_deref(), Some("review-review-0-cli-claude.md"));
        assert_eq!(s.former_ids, ["review-0-cli-codex"]);
        assert_eq!(s.effort, None, "a level the new CLI refuses is dropped");
    }
}

#[cfg(test)]
mod recheck_seat_tests {
    use super::*;
    use tempfile::tempdir;

    fn state_with(slot_id: &str, provider: &str, role: SlotRole) -> RunState {
        let mut st = RunState::new(
            "r-recheck",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        st.slots.push(executor::init_slot(slot_id, provider, role));
        st
    }

    fn pause(paths: &SparPaths, key: &str) {
        let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        store.pause_manual(key, None);
        store.save(paths).unwrap();
    }

    #[test]
    fn an_eligible_seat_dispatches_as_it_is() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let got = recheck_seat(
            &mut st,
            &paths,
            &Config::default(),
            "review-0-cli-grok",
            false,
        )
        .unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-grok"));
    }

    /// The 2026-10-22 shape: grok paused after the run started. The next grok reviewer
    /// must move to its declared backup, renamed to name its new vendor (O85).
    #[test]
    fn a_seat_paused_mid_run_moves_to_its_backup_before_dispatch() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        pause(&paths, "cli:grok");
        let mut st = state_with("review-1-cli-grok", "cli:grok", SlotRole::Reviewer);
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec![String::new(), "cli:codex@gpt-5.6-terra".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-1-cli-grok", false).unwrap();
        let SeatCheck::Ready(id) = got else {
            panic!("expected a move to the backup");
        };
        assert_eq!(id, "review-1-cli-codex");
        let slot = st.slots.iter().find(|s| s.id == id).unwrap();
        assert_eq!(slot.provider, "cli:codex");
        assert_eq!(slot.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(slot.source, Some(SeatSource::Backup));
    }

    #[test]
    fn a_paused_seat_with_no_eligible_backup_parks() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        pause(&paths, "cli:claude");
        let mut st = state_with("impl", "cli:claude", SlotRole::Implementer);
        let got = recheck_seat(&mut st, &paths, &Config::default(), "impl", true).unwrap();
        assert!(matches!(got, SeatCheck::Park(ref m) if m.contains("cli:claude")));

        pause(&paths, "cli:codex");
        let mut cfg = Config::default();
        cfg.backups.implementer = Some("cli:codex".into());
        let got = recheck_seat(&mut st, &paths, &cfg, "impl", true).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(_)),
            "a paused backup is no way out"
        );
        assert_eq!(st.slots[0].provider, "cli:claude");
    }

    /// One id threads plan, implement, and every round (O45), so `created_at`
    /// long predates any later round: the pre-existing/this-dispatch line is the
    /// dispatch start. A weak pause written after the run was created but before
    /// this dispatch steers onto the backup; one written after the dispatch
    /// started stays trust-only.
    #[test]
    fn a_weak_pause_between_creation_and_dispatch_steers_onto_the_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        st.created_at -= chrono::Duration::minutes(10);
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.providers.get_mut("cli:grok").unwrap().updated_at =
            Some(st.created_at + chrono::Duration::seconds(60));
        store.save(&paths).unwrap();
        st.stamp_dispatch();
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-grok", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-codex"),
            "a weak pause predating the dispatch steers onto the backup"
        );
    }

    #[test]
    fn a_weak_pause_written_after_dispatch_start_stays_trust_only() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        st.stamp_dispatch();
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.save(&paths).unwrap();
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-grok", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-grok"),
            "a weak pause from this dispatch must not move the seat"
        );
    }

    /// A weak pause written before the run started steers the next run onto its
    /// backup (O106): only pauses this run wrote stay trust-only.
    #[test]
    fn a_weak_pause_older_than_the_run_moves_a_reviewer_to_its_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.providers.get_mut("cli:grok").unwrap().updated_at =
            Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        store.save(&paths).unwrap();
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:codex".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-grok", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-codex"),
            "a pre-existing weak pause steers onto the backup"
        );
    }

    #[test]
    fn a_weak_pause_older_than_the_run_parks_a_tester_with_no_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:codex", "possible quota signal: 429");
        store.providers.get_mut("cli:codex").unwrap().updated_at =
            Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        store.save(&paths).unwrap();
        let mut st = state_with("test", "cli:codex", SlotRole::Tester);
        let got = recheck_seat(&mut st, &paths, &Config::default(), "test", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Park(_)),
            "a pre-existing weak pause parks a tester with no backup"
        );
    }

    /// A pre-existing weak pause benches a reviewer with no backup too: the
    /// re-check parks, and the review panel loop stands the seat down instead
    /// of parking the run.
    #[test]
    fn a_weak_pause_older_than_the_run_parks_a_reviewer_with_no_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.providers.get_mut("cli:grok").unwrap().updated_at =
            Some(chrono::Utc::now() - chrono::Duration::minutes(10));
        store.save(&paths).unwrap();
        let mut st = state_with("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        let got = recheck_seat(
            &mut st,
            &paths,
            &Config::default(),
            "review-0-cli-grok",
            false,
        )
        .unwrap();
        assert!(
            matches!(got, SeatCheck::Park(_)),
            "a pre-existing weak pause parks a reviewer with no backup"
        );
    }

    /// The broad scrape paused codex because a test log said "429 passed". That pause is
    /// weak: it must not park this run or move its seat.
    #[test]
    fn a_weak_heuristic_pause_never_moves_or_parks_a_live_seat() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:codex", "possible quota signal: 429");
        store.save(&paths).unwrap();
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into()];
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-codex", false).unwrap();
        assert!(matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-codex"));
        assert_eq!(st.slots[0].provider, "cli:codex");

        let mut store = crate::quota::QuotaStore::load(&paths).unwrap();
        store.mark_strong("cli:codex");
        store.save(&paths).unwrap();
        let got = recheck_seat(&mut st, &paths, &cfg, "review-0-cli-codex", false).unwrap();
        assert!(
            matches!(got, SeatCheck::Ready(id) if id == "review-0-cli-grok"),
            "a strong pause still moves the seat"
        );
    }

    /// The start-of-run gate is strict where a live re-check is lenient (O106): a
    /// weak pause still steers the next run's seat choice, so with no backup the
    /// implementer parks instead of dispatching into it.
    #[test]
    fn a_weak_pause_parks_the_start_gate_with_no_backup() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:claude", "possible quota signal: quota");
        store.save(&paths).unwrap();
        let mut st = state_with("impl", "cli:claude", SlotRole::Implementer);
        let got = recheck_seat(&mut st, &paths, &Config::default(), "impl", true).unwrap();
        assert!(matches!(got, SeatCheck::Park(ref m) if m.contains("cli:claude")));
    }

    #[test]
    fn reviewer_ordinal_reads_the_panel_position() {
        assert_eq!(reviewer_ordinal("review-0-cli-claude"), Some(0));
        assert_eq!(reviewer_ordinal("review-12-cli-codex"), Some(12));
        assert_eq!(reviewer_ordinal("review-cli-grok-wide"), None);
        assert_eq!(reviewer_ordinal("impl"), None);
    }
}

#[cfg(test)]
mod carried_rejection_tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn stood_down(body: &str) -> SlotState {
        let mut s = executor::init_slot("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        s.stood_down = Some("provider cli:grok is paused".into());
        s.review = Some(crate::workflow::review_result::parse_review(body));
        s
    }

    fn verdict_on(verdict: &str, sha: &str) -> String {
        format!("## Verdict\n{verdict}\n\nReviewed-Commit: {sha}\n\n## Findings\n- severity: major — x\n")
    }

    #[test]
    fn request_changes_on_this_head_carries() {
        let s = stood_down(&verdict_on("request_changes", HEAD));
        assert!(carried_rejection(&s, Some(HEAD)).is_some());
    }

    #[test]
    fn request_changes_on_an_older_head_does_not_carry() {
        let s = stood_down(&verdict_on("request_changes", OTHER));
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn approve_on_this_head_does_not_carry() {
        let s = stood_down(&verdict_on("approve", HEAD));
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_dispatched_seat_carries_nothing() {
        let mut s = stood_down(&verdict_on("request_changes", HEAD));
        s.stood_down = None;
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_seat_that_never_judged_carries_nothing() {
        let mut s = executor::init_slot("review-0-cli-grok", "cli:grok", SlotRole::Reviewer);
        s.stood_down = Some("provider cli:grok is paused".into());
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }

    #[test]
    fn a_failure_stub_names_no_commit_so_it_carries_nothing() {
        let s = stood_down("## Verdict\nrequest_changes\n\n## Findings\n- severity: major — x\n");
        assert!(carried_rejection(&s, Some(HEAD)).is_none());
    }
}

#[cfg(test)]
mod reviewer_infra_tests {
    use super::*;
    use tempfile::tempdir;

    fn failed(tools: Option<u32>, exit: Option<i32>, error: &str) -> SlotState {
        let mut s = executor::init_slot("review-0-cli-codex", "cli:codex", SlotRole::Reviewer);
        s.status = SlotStatus::Failed;
        s.exit_code = exit;
        s.error = Some(error.into());
        s.exec_backend = Some(crate::provider_ref::ExecBackend::NativeCli);
        s.usage = tools.map(|t| {
            serde_json::from_value(serde_json::json!({
                "slot_id": "review-0-cli-codex",
                "provider": "cli:codex",
                "input_tokens": 0,
                "output_tokens": 0,
                "tools": t,
            }))
            .unwrap()
        });
        s
    }

    /// The incident shapes from #155: codex unable to spawn its host binary, a logged
    /// out CLI, a revoked token. None of them is a review.
    #[test]
    fn provider_failures_are_infrastructure_not_verdicts() {
        let spawn = failed(
            None,
            None,
            "spawn failed: No such file or directory (os error 2)",
        );
        assert!(reviewer_infra_fault(&spawn)
            .unwrap()
            .starts_with("could not start"));

        let zero = failed(
            Some(0),
            Some(1),
            "Error: failed to spawn codex-code-mode-host: No such file or directory",
        );
        assert!(reviewer_infra_fault(&zero).unwrap().contains("tools == 0"));

        let mut quota = failed(Some(3), Some(1), "exit 1");
        quota.quota_hit = true;
        assert_eq!(
            reviewer_infra_fault(&quota).as_deref(),
            Some("rate-limited")
        );

        let mut auth = failed(Some(4), Some(1), "auth: cli:codex rejected the credential");
        auth.auth_fault = true;
        assert!(reviewer_infra_fault(&auth).unwrap().starts_with("auth:"));
    }

    /// A reviewer that read the diff and then failed to write its artifact keeps the
    /// fail-closed `request_changes` (O89); so does a hung one spar killed.
    #[test]
    fn a_reviewer_that_ran_keeps_todays_fail_closed_path() {
        let ran = failed(
            Some(7),
            Some(1),
            "missing expected artifact: review-review-0-cli-codex.md",
        );
        assert!(reviewer_infra_fault(&ran).is_none());

        let mut killed = failed(Some(0), None, "hard ceiling: slot timed out");
        killed.signal = Some(9);
        assert!(reviewer_infra_fault(&killed).is_none());

        let mut api = failed(Some(0), Some(1), "exit 1");
        api.exec_backend = Some(crate::provider_ref::ExecBackend::ApiSdk);
        assert!(
            reviewer_infra_fault(&api).is_none(),
            "api slots always report tools 0"
        );

        let mut done = failed(Some(0), Some(0), "");
        done.status = SlotStatus::Done;
        assert!(reviewer_infra_fault(&done).is_none());
    }

    #[test]
    fn an_infra_failure_rotates_to_the_declared_backup_first() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = RunState::new(
            "r-infra",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        st.slots.push(failed(Some(0), Some(1), "exit 1"));
        st.providers = vec!["cli:claude".into(), "cli:codex".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into()];

        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0-cli-codex",
            tmp.path(),
            &cfg,
            true,
        )
        .unwrap();
        assert_eq!(id.as_deref(), Some("review-0-cli-grok"));
        assert_eq!(st.slots[0].provider, "cli:grok");
        assert_eq!(st.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn a_work_failure_never_takes_the_backup_and_rotation_skips_paused_providers() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_manual("cli:claude", None);
        store.save(&paths).unwrap();
        let mut st = RunState::new(
            "r-infra",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        st.slots
            .push(failed(Some(5), Some(1), "missing expected artifact"));
        st.providers = vec!["cli:claude".into(), "cli:codex".into(), "cli:agy".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.backups.reviewer = vec!["cli:grok".into()];

        let id = try_rotate_reviewer_provider(
            &mut st,
            &paths,
            "review-0-cli-codex",
            tmp.path(),
            &cfg,
            false,
        )
        .unwrap();
        assert_eq!(id.as_deref(), Some("review-0-cli-codex"));
        assert_eq!(
            st.slots[0].provider, "cli:agy",
            "paused claude is skipped, the backup is not used for a work failure"
        );
    }

    #[test]
    fn only_spars_own_salvage_stub_is_dropped() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let art = paths.artifact("r1", "review-review-0-cli-codex.md");
        std::fs::create_dir_all(art.parent().unwrap()).unwrap();
        std::fs::write(
            &art,
            format!(
                "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — {} (exit 1)\n",
                executor::REVIEW_SALVAGE_FINDING
            ),
        )
        .unwrap();
        drop_salvaged_verdict(&paths, "r1", "review-0-cli-codex");
        assert!(!art.exists());

        std::fs::write(
            &art,
            "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — real bug\n",
        )
        .unwrap();
        drop_salvaged_verdict(&paths, "r1", "review-0-cli-codex");
        assert!(
            art.exists(),
            "a review the slot wrote is evidence and stays"
        );
    }
}

#[cfg(test)]
mod rotate_reviewer_tests {
    use super::*;
    use tempfile::tempdir;

    fn state_with_reviewer(provider: &str) -> RunState {
        let mut st = RunState::new(
            "r-rotate",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        let mut slot = executor::init_slot("review-0", provider, SlotRole::Reviewer);
        slot.source = Some(SeatSource::RolesFile);
        st.slots.push(slot);
        st
    }

    /// A one-entry pinned panel offers no alternate provider: rotation must refuse
    /// rather than reach into `state.providers`/`[providers].order` for a substitute
    /// nobody pinned (Bug A, also reachable through this escalation path).
    #[test]
    fn single_pin_panel_never_falls_through_to_pool_or_order() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:codex");
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        let changed =
            try_rotate_reviewer_provider(&mut st, &paths, "review-0", tmp.path(), &cfg, false)
                .unwrap()
                .is_some();
        assert!(!changed, "single-pin panel has no alternate to rotate to");
        assert_eq!(st.slots[0].provider, "cli:codex");
    }

    /// A two-entry pinned panel rotates within the pins, never past them.
    #[test]
    fn two_pin_panel_rotates_to_the_other_pin_only() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:codex");
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into(), "api:openai".into()];
        let changed =
            try_rotate_reviewer_provider(&mut st, &paths, "review-0", tmp.path(), &cfg, false)
                .unwrap()
                .is_some();
        assert!(changed);
        assert_eq!(st.slots[0].provider, "api:openai");
    }

    /// Run 52588b2c's seat: a claude dispatch hit its weekly limit, then the seat was
    /// rotated to codex. The record must not name codex while still carrying claude's
    /// verdict, or `reviewer_infra_fault` reports codex as rate-limited (O149).
    #[test]
    fn rotation_drops_the_previous_providers_verdict() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:claude");
        {
            let s = &mut st.slots[0];
            s.status = SlotStatus::Failed;
            s.quota_hit = true;
            s.auth_fault = true;
            s.exit_code = Some(1);
            s.pid = Some(42);
            s.error = Some("exit 1".into());
            s.usage = Some(
                serde_json::from_value(serde_json::json!({
                    "slot_id": "review-0",
                    "provider": "cli:claude",
                    "input_tokens": 0,
                    "output_tokens": 15,
                }))
                .unwrap(),
            );
        }
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:claude".into(), "cli:codex@gpt-5.6-terra".into()];
        try_rotate_reviewer_provider(&mut st, &paths, "review-0", tmp.path(), &cfg, false)
            .unwrap()
            .expect("rotates to the other pin");
        let s = &st.slots[0];
        assert_eq!(s.provider, "cli:codex");
        assert_eq!(s.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(s.status, SlotStatus::Pending);
        assert!(!s.quota_hit && !s.auth_fault);
        assert!(s.usage.is_none() && s.exit_code.is_none());
        assert_eq!(
            s.pid,
            Some(42),
            "a live process stays visible to stop --abandoned"
        );
        assert_eq!(reviewer_infra_fault(s), None);
    }

    /// `--fleet small` (a preset-only panel narrowing, no `[roles].reviewer` pin) must not
    /// disable failure-recovery rotation: with nothing actually pinned, rotation falls
    /// through to the run's own pool the same way an ordinary unpinned panel does
    /// (review-0-cli-claude, major finding 1).
    #[test]
    fn preset_only_narrowing_still_rotates_through_the_pool() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:grok");
        st.providers = vec!["cli:claude".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        let cfg = Config {
            fleet_reviewer_override: Some(1),
            ..Config::default()
        };
        let changed =
            try_rotate_reviewer_provider(&mut st, &paths, "review-0", tmp.path(), &cfg, false)
                .unwrap()
                .is_some();
        assert!(changed, "narrowed-but-unpinned panel must still retry");
        assert_eq!(st.slots[0].provider, "cli:claude");
    }

    /// Rotating a `--fleet small`-truncated two-pin panel must not restore the pin
    /// `small` dropped: candidates are bound to `panel_size`, so the truncated panel has
    /// no alternate to rotate to and refuses, exactly like a genuine one-pin panel
    /// (review-0-cli-claude, minor finding 2).
    #[test]
    fn small_truncated_two_pin_panel_does_not_restore_the_dropped_pin() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_reviewer("cli:codex");
        let cfg = Config {
            fleet_reviewer_override: Some(1),
            roles: crate::config::RolesConfig {
                reviewer: vec!["cli:codex".into(), "api:openai".into()],
                ..Default::default()
            },
            ..Config::default()
        };
        let changed =
            try_rotate_reviewer_provider(&mut st, &paths, "review-0", tmp.path(), &cfg, false)
                .unwrap()
                .is_some();
        assert!(!changed, "truncated panel has no alternate within its size");
        assert_eq!(st.slots[0].provider, "cli:codex");
    }
}

#[cfg(test)]
mod try_rotate_implementer_tests {
    use super::*;
    use crate::state::{SeatSource, SlotStatus};
    use tempfile::tempdir;

    fn state_with_impl(provider: &str, source: Option<SeatSource>, quota_hit: bool) -> RunState {
        let mut st = RunState::new(
            "r-rotate-impl",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        );
        let mut slot = executor::init_slot("impl", provider, SlotRole::Implementer);
        slot.source = source;
        slot.status = SlotStatus::Failed;
        slot.quota_hit = quota_hit;
        slot.error = Some("simulated failure".into());
        st.slots.push(slot);
        st.providers = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        st.pool_origin = PoolOrigin::CliProviders;
        st
    }

    #[test]
    fn environmental_activates_backup_over_distinctive_pool_provider() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        // Primary environmental (quota_hit) -> backup, distinctive next is codex but backup is grok@backup
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st.slots[0].error = Some("rate limit seven_day rejected".into());
        // Make primary ineligible via store
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = Some("cli:grok@backup".into());
        // ensure backup provider is available (detect_all would list claude,grok,codex as available if binaries exist? But we use store only, and available None because we didn't set detect. is_provider_eligible will check store and available; with available None, only store matters. backup grok is not paused, so eligible)
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(changed, "environmental should rotate to backup");
        assert_eq!(st.slots[0].provider, "cli:grok");
        assert_eq!(st.slots[0].model.as_deref(), Some("backup"));
        assert_eq!(st.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn work_failure_with_backup_declared_does_not_rotate() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), false);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(
            !changed,
            "work failure with backup declared must not rotate at all (no backup, no pool)"
        );
        assert_eq!(st.slots[0].provider, "cli:claude");
        assert_ne!(st.slots[0].source, Some(SeatSource::Backup));
        // Neutralisation: if work branch were changed to activate backup or pool, this would fail
        let mut st_env = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st_env.slots[0].error = Some("rate limit exceeded".into());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let changed_env = try_rotate_implementer(&mut st_env, &paths, &cfg).unwrap();
        assert!(
            changed_env,
            "environmental with same backup must activate, proving branch not neutralised"
        );
    }

    #[test]
    fn work_failure_without_backup_still_rotates_to_pool() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), false);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = None;
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(
            changed,
            "work failure without backup should rotate via pool"
        );
        assert_eq!(st.slots[0].provider, "cli:codex");
        assert_ne!(st.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn work_failure_with_declared_backup_neutralisation_fails() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), false);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(
            !changed,
            "neutralisation check: work with backup must not activate backup"
        );
        assert_ne!(st.slots[0].source, Some(SeatSource::Backup));
        assert_eq!(st.slots[0].provider, "cli:claude");
        // Environmental should still activate backup – proves branch not neutralised to always false
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let mut st2 = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st2.slots[0].error = Some("rate limit exceeded".into());
        let changed2 = try_rotate_implementer(&mut st2, &paths, &cfg).unwrap();
        assert!(changed2, "environmental with same backup must activate");
        assert_eq!(st2.slots[0].source, Some(SeatSource::Backup));
    }

    #[test]
    fn consumed_backup_is_not_reselected() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        // Slot already from backup: simulate environmental failure after backup already activated
        let mut st = state_with_impl("cli:grok", Some(SeatSource::Backup), true);
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = Some("cli:grok@backup".into());
        st.providers = vec!["cli:grok".into(), "cli:codex".into()];
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(!changed, "consumed backup must not be reselected");
        assert_eq!(st.slots[0].provider, "cli:grok");
    }

    #[test]
    fn backup_not_eligible_does_not_activate() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        // Also pause backup
        let mut store = crate::quota::QuotaStore::load(&paths).unwrap();
        store.pause_quota("cli:grok", "test");
        store.save(&paths).unwrap();
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st.slots[0].error = Some("rate limit exceeded".into());
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into(), "cli:grok".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(!changed, "ineligible backup must not be selected");
    }

    #[test]
    fn neutralization_would_fail_backup_branch() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        {
            let mut store = crate::quota::QuotaStore::default();
            store.pause_quota("cli:claude", "test");
            store.save(&paths).unwrap();
        }
        let mut st = state_with_impl("cli:claude", Some(SeatSource::CliRole), true);
        st.slots[0].error = Some("rate limit exceeded".into());
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:claude".into(), "cli:codex".into()];
        cfg.roles.implementer = Some("cli:claude".into());
        cfg.cli_role_keys.insert("implementer".into());
        cfg.backups.implementer = Some("cli:grok@backup".into());
        let changed = try_rotate_implementer(&mut st, &paths, &cfg).unwrap();
        assert!(changed);
        assert_eq!(st.slots[0].provider, "cli:grok");
        // If Environmental branch were deleted, rotation would fall to codex
        // This test documents that deleting the if cause == Environmental block changes behavior
    }
}

/// `quota_hit` is the discriminator: only a dispatch whose own log matched the quota
/// scrape (`executor::detect_and_pause_quota`) parks here instead of failing outright,
/// so a genuine defect never gets misread as "wait and retry".
///
/// A quota park returns `Ok`, not `Err`: `execute_loop`'s callers propagate an `Err`
/// straight past `finish_out`/`state.exit_code()` to `main`'s catch-all, which always
/// exits `1` — the same shortcut the pre-dispatch quota gate in `run_from_approved`
/// avoids by returning `Ok(ExitCode::Quota)` directly. Ending the round loop with `Ok`
/// here lets the normal tail read the just-set `Phase::Quota` and exit `4`.
fn fail(state: &mut RunState, paths: &SparPaths, e: anyhow::Error, quota_hit: bool) -> Result<()> {
    state.set_phase(if quota_hit {
        Phase::Quota
    } else {
        Phase::Failed
    });
    state.error = Some(e.to_string());
    state.save(paths)?;
    if quota_hit {
        Ok(())
    } else {
        Err(e)
    }
}

#[cfg(test)]
mod fail_quota_routing_tests {
    use super::*;
    use crate::exit_codes::ExitCode as ProcessExitCode;
    use tempfile::tempdir;

    fn state() -> RunState {
        RunState::new(
            "r-quota",
            crate::cli::WorkflowKind::Loop,
            PathBuf::from("/tmp/x"),
        )
    }

    /// A quota-detected dispatch failure must park at `Phase::Quota` and exit `4`, and
    /// must not propagate `Err` — that would skip `finish_out`/`state.exit_code()` and
    /// fall through to `main`'s catch-all, which always exits `1`.
    #[test]
    fn quota_hit_parks_at_quota_and_does_not_propagate_err() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state();
        let res = fail(&mut st, &paths, anyhow::anyhow!("rate limited"), true);
        assert!(res.is_ok(), "quota park must return Ok, not Err");
        assert_eq!(st.phase, Phase::Quota);
        assert_eq!(st.exit_code(), ProcessExitCode::Quota);
    }

    /// An ordinary dispatch failure (no quota signal in the log) must still fail the
    /// run outright at exit `1` — the discriminator must never swallow a real defect.
    #[test]
    fn ordinary_failure_stays_failed_and_propagates_err() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut st = state();
        let res = fail(
            &mut st,
            &paths,
            anyhow::anyhow!("panic: index out of bounds"),
            false,
        );
        assert!(res.is_err(), "an ordinary failure must still propagate Err");
        assert_eq!(st.phase, Phase::Failed);
        assert_eq!(st.exit_code(), ProcessExitCode::Failure);
    }
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
    if let Some(p) = state.partial_round_phrase() {
        body.push_str(&format!(
            "\nPartial work: {} {} cut short at the hard ceiling; the branch carries partial work.\n",
            p.noun, p.verb
        ));
    }
    body.push_str("\nShip when ready: `spar ship ");
    body.push_str(&state.id);
    body.push_str("` (requires confirm).\n");
    std::fs::write(paths.artifact(&state.id, "summary.md"), body)?;
    Ok(())
}

/// Park the run at the round-ceiling gate instead of buying another cold re-dispatch.
///
const QUEUED_MAX_ROUNDS: &str = "max-rounds.queued";

/// A ceiling at or below the round the run is on parks it again the moment it re-enters,
/// which reads as the flag having been ignored. `reusing` is a round opened but never
/// dispatched: re-entry runs that round again, so it may be the ceiling itself.
/// A `--reassign` the run's own seats refuse (no such seat, ambiguous role): found under
/// the lock, after the launch pre-check, and still a refusal of the launch (O126).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReassignRefused(String);

#[derive(Debug, thiserror::Error)]
#[error(
    "--max-rounds {target} is at or below run {run_id}'s current round {round}, so it would \
     park at the ceiling again immediately; pass at least {floor}, or +N for N more rounds"
)]
pub struct RoundExtensionRefused {
    run_id: String,
    target: u32,
    round: u32,
    floor: u32,
}

pub fn check_round_extension(state: &RunState, target: u32, reusing: bool) -> Result<()> {
    let floor = if reusing && state.round > 0 {
        state.round
    } else {
        state.round + 1
    };
    if target >= floor {
        return Ok(());
    }
    Err(RoundExtensionRefused {
        run_id: state.id.clone(),
        target,
        round: state.round,
        floor,
    }
    .into())
}

/// How long an extension waits for a run's lock when its orchestrator is parking or
/// finishing up (the gate is saved before the lock is released, and reclaim can follow).
const EXTENSION_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// `implement --run <id> --max-rounds …` against a run an orchestrator is still driving.
/// In flight, the extension is left as a marker the loop reads before its next ceiling
/// check and once more after saving a gate. Already at a waitable stop, nothing will read
/// it, so `None` sends the caller to wait for the lock instead; the same when the run
/// reached one between our first read and the marker landing (whichever of us looks
/// second sees the other's write).
fn queue_round_extension(
    paths: &SparPaths,
    state: &RunState,
    target: u32,
    owner_pid: u32,
    json: bool,
) -> Result<Option<ExitCode>> {
    if state.phase.is_waitable_stop() {
        return Ok(None);
    }
    crate::markers::write_marker(paths, &state.id, QUEUED_MAX_ROUNDS, &format!("{target}\n"))?;
    if RunState::load(paths, &state.id)?.phase.is_waitable_stop() {
        return Ok(None);
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::info(format!(
            "round ceiling {target} queued for the live orchestrator (pid {owner_pid})"
        )),
    );
    report_queued(&state.id, target, owner_pid, state.round, json)?;
    Ok(Some(ExitCode::Success))
}

fn report_queued(run_id: &str, target: u32, owner_pid: u32, round: u32, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "run_id": run_id,
                "queued_max_rounds": target,
                "owner_pid": owner_pid,
                "round": round,
            }))?
        );
    } else {
        println!(
            "run {run_id} is being driven by pid {owner_pid}; round ceiling {target} queued for its next ceiling check"
        );
    }
    Ok(())
}

/// Poll for the lock of a run whose orchestrator is parking. `Err(code)` when that
/// orchestrator consumed a marker we left and carried on (the extension took effect);
/// `OrchestratorBusy` when the lock never frees.
fn acquire_for_extension(
    paths: &SparPaths,
    run_id: &str,
    target: u32,
    owner_pid: u32,
    json: bool,
) -> Result<std::result::Result<crate::runlock::RunLock, ExitCode>> {
    let deadline = std::time::Instant::now() + EXTENSION_LOCK_WAIT;
    loop {
        let err = match crate::runlock::RunLock::acquire(paths, run_id) {
            Ok(lock) => return Ok(Ok(lock)),
            Err(e) if e.is::<crate::runlock::OrchestratorBusy>() => e,
            Err(e) => return Err(e),
        };
        if !paths.marker(run_id, QUEUED_MAX_ROUNDS).exists() {
            let st = RunState::load(paths, run_id)?;
            if !st.phase.is_waitable_stop() && st.max_rounds >= target {
                report_queued(run_id, target, owner_pid, st.round, json)?;
                return Ok(Err(ExitCode::Success));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(err);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Consume a queued `--max-rounds`. Only ever raises: a queued value the run already
/// passed (a later flag won) is dropped rather than lowering the ceiling under it.
fn apply_queued_round_extension(state: &mut RunState, paths: &SparPaths) {
    let path = paths.marker(&state.id, QUEUED_MAX_ROUNDS);
    let Ok(body) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let Ok(target) = body.trim().parse::<u32>() else {
        return;
    };
    if state.max_rounds == 0 || target <= state.max_rounds {
        return;
    }
    state.max_rounds = target;
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::info(format!("round ceiling raised to {target} (queued)")),
    );
}

/// Same idiom as the fix-round exhaustion path below it — set an error, write
/// `escalation.md`, stop — with one deliberate difference: this is a **gate** (exit 2),
/// not `Stuck` (exit 3). Nothing is broken; the run has simply spent the re-dispatch
/// budget it was allowed to spend on its own, and only a human can authorise more.
///
/// `Ok(false)` when an extension queued while the gate was being saved lifted the
/// ceiling after all: the caller carries on instead of parking (the lock is still ours).
fn escalate_round_ceiling(state: &mut RunState, paths: &SparPaths) -> Result<bool> {
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
    // An extension may have been queued between the last read and this save; a queuer
    // that read the run before the save cannot see the gate, so this read must see it.
    apply_queued_round_extension(state, paths);
    if !state.round_ceiling_reached() {
        state.error = None;
        return Ok(false);
    }
    // `print_run_human` shows the phase but not `error`, so without this an operator
    // running implement interactively sees a phase name and no way out of it.
    eprintln!("{msg}");
    // Findings describe the commit their reviewer read; one whose file moved since may
    // already be fixed, and whoever takes the run over should check it at HEAD (O120).
    let open = review_result::open_findings(state, &review_result::findings_cwd(state));
    let mut findings = String::new();
    if !open.is_empty() {
        findings.push_str("\n## Open findings\n\n");
        for f in &open {
            findings.push_str(&f.line());
            findings.push('\n');
        }
        eprint!("{findings}");
    }
    std::fs::write(
        paths.artifact(&state.id, "escalation.md"),
        format!(
            "# Escalation: round ceiling\n\n{msg}\n\nFix rounds this leg: {}\nSuite: {:?}\n{findings}",
            state.fix_rounds, state.suite_outcome
        ),
    )?;
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg.clone()));
    let _ = crate::bus::broadcast(
        paths,
        Some(&state.id),
        crate::bus::ORCHESTRATOR_SEAT,
        msg,
        state.message_budget,
    );
    Ok(true)
}

/// `{{prior_findings}}` for this round's panel (O119): from `[review]
/// scoped_from_round` on, the previous panel's findings and the diff since the commit it
/// judged, so reviewers check those and look for regressions instead of re-reviewing
/// from scratch. Empty, meaning a full review, when the previous panel cannot anchor
/// one: none recorded, a review spar wrote itself, or reviewers that judged different
/// commits.
fn scoped_review_section(state: &RunState, cfg: &Config, review_cwd: &Path) -> String {
    let from = cfg.review.scoped_from_round;
    if from == 0 || state.round < from {
        return String::new();
    }
    let panel = review_result::panel_before(&state.review_history, state.round);
    if panel.is_empty() || panel.iter().any(|e| e.review.synthetic) {
        return String::new();
    }
    let commit = panel[0].review.reviewed_commit.clone();
    if panel.iter().any(|e| e.review.reviewed_commit != commit) {
        return String::new();
    }
    // A critical filed under Follow-up blocked its round, so it is re-judged too.
    let prior: Vec<_> = review_result::panel_findings(&panel)
        .into_iter()
        .filter(|(_, f)| !f.follow_up || f.severity == review_result::Severity::Critical)
        .collect();
    let stat = commit
        .as_deref()
        .and_then(|c| worktree::git_out(review_cwd, &["diff", "--stat", &format!("{c}..HEAD")]));
    render_scoped_review(
        state.round,
        &prior,
        commit.as_deref(),
        stat.as_deref(),
        review_cwd,
    )
}

fn render_scoped_review(
    round: u32,
    prior: &[(&str, &review_result::Finding)],
    commit: Option<&str>,
    stat: Option<&str>,
    review_cwd: &Path,
) -> String {
    let mut out = format!(
        "## Scoped re-review (round {round})\n\n\
         This is a re-review. Earlier rounds already reviewed this change in full. Judge \
         the previous panel's findings below at HEAD and look for regressions in what \
         changed since; do not re-review the whole change from scratch.\n\n"
    );
    if let Some(c) = commit {
        out.push_str(&format!(
            "The previous panel judged `{c}`. Full diff since: `git -C {} diff {c}..HEAD`.\n",
            review_cwd.display()
        ));
        match stat.map(str::trim).filter(|s| !s.is_empty()) {
            Some(stat) => out.push_str(&format!("\n```\n{stat}\n```\n")),
            None => out.push_str("Nothing changed since, or the diff could not be read.\n"),
        }
        out.push('\n');
    }
    out.push_str("### Prior findings\n");
    if prior.is_empty() {
        out.push_str("- (none recorded: the previous panel blocked on acceptance criteria only)\n");
    }
    for (slot, f) in prior {
        out.push_str(&review_result::finding_line(slot, f));
        out.push('\n');
    }
    out.push_str(
        "\nThis round:\n\
         - Under `## Findings`, list each prior finding still open at HEAD, at its severity, \
         and any regression the change since introduced. Leave resolved ones out and name \
         them under `## Resolved`.\n\
         - Any other new finding that is not critical goes under `## Follow-up`, with a \
         severity (`- severity: major — ...`). Follow-up never blocks the ship; it goes into \
         the PR body as open work. A new critical finding still goes under `## Findings`.\n\
         - `request_changes` means a prior finding is still open, the change since regressed \
         something, or an acceptance criterion fails.\n",
    );
    out
}

fn ceiling_partial_blocker(kind: CeilingPartialKind, round: u32) -> String {
    match kind {
        CeilingPartialKind::Clean => ceiling_partial_clean_notice(round),
        CeilingPartialKind::Refused => ceiling_partial_refused_notice(round, None),
        CeilingPartialKind::Wip => ceiling_partial_notice(round),
    }
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
        // Whether that review answered a scoped prompt is on its recorded entry.
        let scoped = state
            .review_history
            .iter()
            .filter(|e| e.slot_id == rev.id || rev.former_ids.contains(&e.slot_id))
            .max_by_key(|e| e.round)
            .is_some_and(|e| e.review.scoped);
        let res = review_result::ReviewResult {
            scoped,
            ..review_result::parse_review(&text)
        };
        if res.blocks(cfg.review.block_on) {
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
    // A round cut short at its hard ceiling left its work behind as a partial
    // round; the round after it starts from that commit whether this process
    // saw the kill or not, so the notice is rebuilt here rather than carried in
    // memory. Only the immediately previous round qualifies: anything older was
    // either consumed by the round after it or approved as-is. The recorded kind
    // picks the wording: only a saved WIP commit may be claimed as one.
    if let Some(prev) = state.round.checked_sub(1) {
        if let Some(entry) = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == prev)
        {
            out.push(ceiling_partial_blocker(entry.kind, prev));
        }
    }
    // A same-round resume after a ceiling kill (`continue` without a new round):
    // the resumed implementer would otherwise start with no idea its own round
    // was cut short. The round-start snapshot is not re-stamped on resume, so
    // this notice cannot make the round's own banked commit read as new work.
    if let Some(entry) = state
        .ceiling_partial_rounds
        .iter()
        .find(|e| e.round == state.round)
    {
        out.push(ceiling_partial_blocker(entry.kind, state.round));
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
    persisted: &[String],
    blockers: &[String],
    brief: Option<&str>,
    max_chars: usize,
) -> String {
    if persisted.is_empty() && blockers.is_empty() && brief.is_none() {
        return String::new();
    }
    let mut out = format!(
        "## Carry-forward (you already ran on this worktree — this is round {round})\n\
         Context from the previous round, not a verdict. It is here so you do not spend \
         this round re-deriving what the last one already learned. The acceptance gate \
         never reads it.\n\n"
    );
    if !persisted.is_empty() {
        // First, so the cap cannot cut an operator ruling; these stand until the sender
        // withdraws them, which is the point of `--persist`.
        out.push_str(
            "### Operator messages (they stand until withdrawn; follow them over your own notes)\n",
        );
        for m in persisted {
            out.push_str(&format!("- {}\n", clamp_line(m, max_chars / 4)));
        }
        out.push('\n');
    }
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
                    "detached (pid {pid}, session of its own); wait with: spar wait {}",
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

/// A run whose orchestrator (or `spar ship`) died in `shipping` goes back to the ship
/// gate: shipping is the operator's step, and every resume entry, the daemon's included,
/// must route here rather than re-running the rounds that produced the work (O117).
pub fn park_interrupted_ship(paths: &SparPaths, run_id: &str, json: bool) -> Result<ExitCode> {
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    let mut state = RunState::load(paths, run_id)?;
    if state.phase == Phase::Shipping {
        state.set_phase(Phase::AwaitingShipConfirm);
        state.save(paths)?;
    }
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!(
            "run {run_id} died while shipping; back at the ship gate: spar ship {run_id} --confirm"
        );
    }
    Ok(state.exit_code())
}

pub fn continue_run(paths: &SparPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase == Phase::Shipping {
        return park_interrupted_ship(paths, run_id, false);
    }
    // An approved plan has finished planning: continuing it is implementation, never a
    // second plan round asking for the approval again (O117).
    if state.workflow == crate::cli::WorkflowKind::Plan && state.gates.plan_approved {
        let opts = CommonOpts {
            dry_run: state.dry_run,
            ..CommonOpts::default()
        };
        return run_from_approved(run_id, None, opts, paths, cfg);
    }
    if state.workflow == crate::cli::WorkflowKind::Plan {
        return crate::workflow::plan::continue_run(paths, cfg, run_id);
    }
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    // Re-read under the lock: the routing load above can be stale, and the
    // stamp below writes, so stamping the stale copy would clobber a
    // concurrent writer's fields.
    state = RunState::load(paths, run_id)?;
    // A resume is a new dispatch in this process: re-stamp the horizon and the
    // profile so a pause written before this resume steers the seat, and reads
    // key the seats this process actually dispatches. Fix passes inside the
    // loop below never re-stamp; see `stamp_dispatch`.
    state.stamp_dispatch();
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

    #[test]
    fn reviewer_template_requires_the_judged_commit() {
        let reviewer = include_str!("../../templates/reviewer_adversarial.md");
        assert!(
            reviewer.contains("Reviewed-Commit:"),
            "the verdict must name the commit it judged"
        );
        assert!(
            reviewer.contains("git rev-parse HEAD"),
            "the reviewer must be told how to learn HEAD"
        );
        assert!(
            reviewer.contains("cannot be counted"),
            "the template must say plainly why a commit-less verdict is not a verdict"
        );
    }
}

#[cfg(test)]
mod reviewed_commit_gate_tests {
    use super::{decide_reviewed_commit, ReviewedCommitDecision};
    use crate::workflow::review_result::parse_review;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn decide(head: Option<&str>, body: &str) -> ReviewedCommitDecision {
        decide_reviewed_commit(head, &parse_review(body))
    }

    #[test]
    fn full_sha_match_is_a_verdict() {
        let body = format!("## Verdict\napprove\n\nReviewed-Commit: {HEAD}\n");
        assert_eq!(decide(Some(HEAD), &body), ReviewedCommitDecision::Match);
    }

    #[test]
    fn short_prefix_match_is_a_verdict() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: 0123456\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Match);
    }

    #[test]
    fn full_mismatch_is_not_a_verdict() {
        let body =
            "## Verdict\napprove\n\nReviewed-Commit: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Mismatch);
    }

    #[test]
    fn short_non_prefix_is_not_a_verdict() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: deadbee\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Mismatch);
    }

    #[test]
    fn absent_line_is_not_a_verdict() {
        assert_eq!(
            decide(Some(HEAD), "## Verdict\napprove\n"),
            ReviewedCommitDecision::Absent
        );
    }

    #[test]
    fn malformed_line_is_absence_not_mismatch() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: <full-sha-of-HEAD>\n";
        assert_eq!(decide(Some(HEAD), body), ReviewedCommitDecision::Absent);
    }

    #[test]
    fn unresolvable_panel_head_skips_either_way() {
        assert_eq!(
            decide(None, "## Verdict\napprove\n"),
            ReviewedCommitDecision::Skipped
        );
        let body = format!("## Verdict\napprove\n\nReviewed-Commit: {HEAD}\n");
        assert_eq!(decide(None, &body), ReviewedCommitDecision::Skipped);
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
        carry_forward_section(round, &[], blockers, brief, max)
    }

    /// An operator's persisted message leads the section and survives the cap that
    /// eats the slot's own notes: a relaunch must not drop it (O135).
    #[test]
    fn persisted_messages_lead_and_survive_truncation() {
        let persisted = vec!["from human (message abc): merge main first".to_string()];
        let essay = "my own plan, which differs\n".repeat(400);
        let out = carry_forward_section(2, &persisted, &[], Some(&essay), 1200);
        let ruling = out
            .find("merge main first")
            .expect("persisted message kept");
        assert!(
            ruling < out.find("my own plan").unwrap_or(usize::MAX),
            "{out}"
        );
        assert!(out.contains("Operator messages"), "{out}");
        assert!(!carry_forward_section(1, &persisted, &[], None, 4000).is_empty());
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
    /// A minors-only `request_changes` on disk is not a blocker for the next round
    /// either: `blockers_from_disk` uses the same rule as the live gate (O118).
    #[test]
    fn a_minors_only_review_seeds_no_verdict_blocker() {
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
            "## Verdict\nrequest_changes\n\n## Acceptance\nAC-1: pass — done\n\n## Findings\n- severity: minor — stale comment at src/a.rs:3\n",
        )
        .unwrap();
        let ac = ["AC-1".to_string()];
        assert!(blockers_from_disk(&state, &paths, &ac, &Config::default()).is_empty());
        let mut strict = Config::default();
        strict.review.block_on = review_result::Severity::Minor;
        assert_eq!(blockers_from_disk(&state, &paths, &ac, &strict).len(), 1);
    }

    fn scoped_state(round: u32, reviews: &[(&str, &str)]) -> RunState {
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/nonexistent".into());
        state.round = round;
        for (slot, body) in reviews {
            state.review_history.push(crate::state::ReviewHistoryEntry {
                round: round - 1,
                slot_id: slot.to_string(),
                review: review_result::parse_review(body),
            });
        }
        state
    }

    const PRIOR: &str = "## Verdict\nrequest_changes\n\nReviewed-Commit: abcdef1234567\n\n## Findings\n- severity: major — the retry at src/net.rs:40 never stops\n\n## Follow-up\n- severity: minor — split the module later\n";

    #[test]
    fn scoped_review_starts_at_its_round_and_carries_the_prior_findings() {
        let cfg = Config::default();
        let cwd = Path::new("/nonexistent");
        assert!(scoped_review_section(&scoped_state(2, &[("rev-1", PRIOR)]), &cfg, cwd).is_empty());

        let s = scoped_review_section(&scoped_state(3, &[("rev-1", PRIOR)]), &cfg, cwd);
        assert!(s.contains("## Scoped re-review (round 3)"), "{s}");
        assert!(
            s.contains("- major `src/net.rs:40` (rev-1): the retry"),
            "{s}"
        );
        assert!(s.contains("diff abcdef1234567..HEAD"), "{s}");
        assert!(s.contains("`## Follow-up`"), "{s}");
        assert!(
            !s.contains("split the module"),
            "follow-ups are not re-judged: {s}"
        );

        let mut off = Config::default();
        off.review.scoped_from_round = 0;
        assert!(scoped_review_section(&scoped_state(5, &[("rev-1", PRIOR)]), &off, cwd).is_empty());
    }

    /// A panel spar had to stand in for, or one whose reviewers judged different
    /// commits, cannot anchor a scoped round: that round gets a full review.
    #[test]
    fn scoped_review_falls_back_to_a_full_review_without_an_anchor() {
        let cfg = Config::default();
        let cwd = Path::new("/nonexistent");
        let synthetic = format!("{}\n{PRIOR}", review_result::SYNTHETIC_REVIEW_MARKER);
        assert!(scoped_review_section(
            &scoped_state(4, &[("rev-1", PRIOR), ("rev-2", &synthetic)]),
            &cfg,
            cwd
        )
        .is_empty());
        let other = PRIOR.replace("abcdef1234567", "1111111222222");
        assert!(scoped_review_section(
            &scoped_state(4, &[("rev-1", PRIOR), ("rev-2", &other)]),
            &cfg,
            cwd
        )
        .is_empty());
        assert!(scoped_review_section(&scoped_state(4, &[]), &cfg, cwd).is_empty());
    }
    /// A critical filed under Follow-up blocked its own round, so the next scoped
    /// round must still re-judge it rather than drop it with the other follow-ups.
    #[test]
    fn scoped_review_keeps_a_critical_filed_under_follow_up() {
        let body =
            format!("{PRIOR}- severity: critical — the migration drops rows at src/m.rs:5\n");
        let s = scoped_review_section(
            &scoped_state(3, &[("rev-1", &body)]),
            &Config::default(),
            Path::new("/nonexistent"),
        );
        assert!(
            s.contains("- critical follow-up `src/m.rs:5` (rev-1)"),
            "{s}"
        );
        assert!(!s.contains("split the module"), "{s}");
    }
    #[test]
    fn reviewer_template_carries_the_severity_rubric() {
        let t = crate::templates::get("reviewer").unwrap();
        assert!(t.contains("Severity rubric"), "{t}");
        assert!(t.contains("Anything that makes the code do the wrong thing is major or above"));
        assert!(t.contains("Minor never blocks the ship"));
        assert!(t.contains("Be strict."));
    }

    /// O127: an extension queued while the gate was being saved is read once more after
    /// the save, so the orchestrator carries on instead of parking over it.
    #[test]
    fn a_queued_extension_read_after_the_gate_save_unparks() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, tmp.path().into());
        state.round = 3;
        state.max_rounds = 3;
        assert!(state.round_ceiling_reached());
        crate::markers::write_marker(&paths, "r1", QUEUED_MAX_ROUNDS, "5\n").unwrap();
        assert!(!escalate_round_ceiling(&mut state, &paths).unwrap());
        assert_eq!(state.max_rounds, 5);
        assert!(state.error.is_none());
        assert!(!paths.marker("r1", QUEUED_MAX_ROUNDS).exists());

        // Nothing queued: it parks.
        state.round = 5;
        assert!(escalate_round_ceiling(&mut state, &paths).unwrap());
        assert_eq!(state.phase, Phase::AwaitingRoundExtension);
    }
}

#[cfg(test)]
mod ceiling_partial_notice_tests {
    use super::{
        blockers_from_disk, ceiling_partial_clean_notice, ceiling_partial_notice,
        ceiling_partial_refused_notice, clear_finished_round_partial_mark, commit_before_review,
        fail, handle_retry_err, record_ceiling_partial_unless_quota, try_commit_ceiling_partial,
    };
    use crate::config::Config;
    use crate::executor::{self, TimeoutCause};
    use crate::paths::SparPaths;
    use crate::state::{
        CeilingPartialKind, CeilingPartialRound, RunState, SlotRole, SlotState, WorktreeRecord,
    };

    fn blockers(kind: Option<CeilingPartialKind>) -> Vec<String> {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.round = 2;
        if let Some(kind) = kind {
            state.ceiling_partial_rounds = vec![CeilingPartialRound {
                round: 1,
                kind,
                commit: None,
            }];
        }
        blockers_from_disk(&state, &paths, &[], &Config::default())
    }

    #[test]
    fn wip_round_claims_a_saved_wip_commit() {
        let out = blockers(Some(CeilingPartialKind::Wip));
        assert_eq!(out, [ceiling_partial_notice(1)]);
        assert!(out[0].contains("saved as a WIP commit"), "{}", out[0]);
    }

    #[test]
    fn clean_round_must_not_claim_a_wip_commit() {
        let out = blockers(Some(CeilingPartialKind::Clean));
        assert_eq!(out, [ceiling_partial_clean_notice(1)]);
        assert!(!out[0].contains("saved as a WIP commit"), "{}", out[0]);
        assert!(out[0].contains("no WIP commit was written"), "{}", out[0]);
    }

    #[test]
    fn refused_round_must_not_claim_a_wip_commit() {
        let out = blockers(Some(CeilingPartialKind::Refused));
        assert_eq!(out, [ceiling_partial_refused_notice(1, None)]);
        assert!(!out[0].contains("saved as a WIP commit"), "{}", out[0]);
        assert!(out[0].contains("could not be committed"), "{}", out[0]);
    }

    #[test]
    fn live_refused_notice_names_the_commit_error() {
        let note = ceiling_partial_refused_notice(1, Some("hook said no"));
        assert!(note.contains("hook said no"), "{note}");
        assert!(!note.contains("saved as a WIP commit"), "{note}");
    }

    #[test]
    fn no_partial_round_means_no_notice() {
        assert!(blockers(None).is_empty());
    }

    #[test]
    fn refused_notice_judges_head_not_an_uncommitted_tree() {
        for detail in [None, Some("hook said no")] {
            let note = ceiling_partial_refused_notice(1, detail);
            assert!(!note.contains("uncommitted"), "{note}");
            assert!(note.contains("judge the HEAD commit"), "{note}");
        }
    }

    #[test]
    fn same_round_resume_sees_its_own_cut_short_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.round = 1;
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: None,
        }];
        let out = blockers_from_disk(&state, &paths, &[], &Config::default());
        assert_eq!(out, [ceiling_partial_notice(1)]);
    }

    #[test]
    fn finishing_a_round_clears_only_its_own_partial_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.round = 2;
        state.ceiling_partial_rounds = vec![
            CeilingPartialRound {
                round: 1,
                kind: CeilingPartialKind::Wip,
                commit: Some("abc".into()),
            },
            CeilingPartialRound {
                round: 2,
                kind: CeilingPartialKind::Clean,
                commit: None,
            },
        ];
        clear_finished_round_partial_mark(&mut state, &paths).unwrap();
        assert_eq!(state.ceiling_partial_rounds.len(), 1);
        assert_eq!(state.ceiling_partial_rounds[0].round, 1);
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn ceiling_state(
        tmp: &tempfile::TempDir,
        wt: &std::path::Path,
        branch: &str,
        head: Option<String>,
        tree: Option<String>,
    ) -> (SparPaths, RunState) {
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.task = Some("add work".into());
        let mut slot: SlotState = executor::init_slot("impl", "cli:muse", SlotRole::Implementer);
        slot.timeout_cause = Some(TimeoutCause::HardCeiling);
        slot.dispatch_head = head;
        slot.dispatch_tree = tree;
        state.slots = vec![slot];
        state.worktrees = vec![WorktreeRecord {
            slot_id: "impl".into(),
            path: wt.to_path_buf(),
            branch: branch.into(),
        }];
        (paths, state)
    }

    fn fresh_repo(tmp: &tempfile::TempDir) -> (std::path::PathBuf, String) {
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        git(&wt, &["init", "-q"]);
        git(&wt, &["config", "user.email", "t@t"]);
        git(&wt, &["config", "user.name", "t"]);
        std::fs::write(wt.join("f"), "a\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "base"]);
        let branch = git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]);
        (wt, branch)
    }

    #[test]
    fn first_ceiling_kill_with_new_work_banks_a_conventional_wip() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("new work at the ceiling is a partial round");
        assert_eq!(notice, ceiling_partial_notice(1));
        assert!(matches!(kind, CeilingPartialKind::Wip));
        let sha = commit.expect("the wip commit is banked");
        let subject = git(&wt, &["log", "-1", "--format=%s", &sha]);
        assert!(
            subject.starts_with("chore(spar):"),
            "wip subject must pass conventional-commit hooks: {subject}"
        );
    }

    #[test]
    fn second_idle_ceiling_kill_continues_on_the_banked_wip() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked.clone()),
        }];
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("the banked partial is still there, so the round goes on");
        assert_eq!(notice, ceiling_partial_notice(1));
        assert!(matches!(kind, CeilingPartialKind::Wip));
        assert_eq!(commit.as_deref(), Some(banked.as_str()));
    }

    #[test]
    fn resumed_round_idle_ceiling_kill_continues_on_the_previous_round_bank() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        // The resume opened a new round (the previous round parked) and added
        // nothing before its own ceiling kill.
        state.round = 2;
        state.slots[0].dispatch_head = Some(banked.clone());
        state.slots[0].dispatch_tree = executor::worktree_snapshot(&wt);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked.clone()),
        }];
        let (notice, kind, commit) = try_commit_ceiling_partial(&mut state, &paths, "impl")
            .expect("the previous round's banked partial is still there, so the round goes on");
        assert_eq!(notice, ceiling_partial_notice(1));
        assert!(matches!(kind, CeilingPartialKind::Wip));
        assert_eq!(commit.as_deref(), Some(banked.as_str()));
    }

    #[test]
    fn second_ceiling_kill_after_a_reset_away_from_the_bank_is_not_a_partial_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "banked wip"]);
        let banked = git(&wt, &["rev-parse", "HEAD"]);
        git(&wt, &["reset", "--hard", "-q", "HEAD~1"]);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Wip,
            commit: Some(banked),
        }];
        assert!(
            try_commit_ceiling_partial(&mut state, &paths, "impl").is_none(),
            "the banked commit is no longer carried, so the bank says nothing"
        );
    }

    #[test]
    fn ceiling_kill_with_no_work_and_no_banked_partial_is_not_a_partial_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        assert!(
            try_commit_ceiling_partial(&mut state, &paths, "impl").is_none(),
            "a first ceiling kill with nothing left behind still fails the round"
        );
    }

    #[test]
    fn ceiling_kill_that_also_hit_quota_parks_instead_of_banking() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "slot work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        state.slots[0].quota_hit = true;
        let mut note = None;
        assert!(
            !record_ceiling_partial_unless_quota(&mut state, &paths, "impl", &mut note).unwrap(),
            "quota wins over the ceiling: no partial is banked"
        );
        assert!(note.is_none());
        assert!(
            state.ceiling_partial_rounds.is_empty(),
            "no banked partial to send to review"
        );
        fail(
            &mut state,
            &paths,
            anyhow::anyhow!("quota and ceiling"),
            true,
        )
        .unwrap();
        assert!(
            matches!(state.phase, crate::state::Phase::Quota),
            "a quota hit parks the run"
        );
        assert_eq!(
            state.exit_code(),
            crate::exit_codes::ExitCode::Quota,
            "exit codes are a public contract: quota parks at 4"
        );
    }

    #[test]
    fn backup_retry_ceiling_kill_with_new_work_banks_a_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        std::fs::write(wt.join("work.txt"), "backup seat work\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        let mut note = None;
        assert!(
            handle_retry_err(
                &mut state,
                &paths,
                "impl",
                anyhow::anyhow!("backup ceiling kill"),
                &mut note,
            )
            .unwrap(),
            "a ceiling-cut backup retry goes on to review like the primary dispatch"
        );
        let entry = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == 1)
            .expect("the backup retry banks its partial");
        assert!(matches!(entry.kind, CeilingPartialKind::Wip));
        assert!(entry.commit.is_some());
    }

    #[test]
    fn backup_retry_measures_work_against_the_round_start() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        let tree = executor::worktree_snapshot(&wt);
        let (_, mut state) = ceiling_state(&tmp, &wt, &branch, Some(head), tree);
        std::fs::write(wt.join("work.txt"), "primary seat work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-qm", "primary seat work"]);
        let slot = state.slots.iter_mut().find(|s| s.id == "impl").unwrap();
        slot.point_at("cli:grok@backup");
        let slot = state.slots.iter().find(|s| s.id == "impl").unwrap();
        assert!(
            executor::dispatch_left_work(
                &wt,
                slot.dispatch_head.as_deref(),
                slot.dispatch_tree.as_deref(),
                None,
            ),
            "rotation to backup keeps the round-start stamps, so the primary seat's commit reads as work"
        );
        let fresh_head = git(&wt, &["rev-parse", "HEAD"]);
        let fresh_tree = executor::worktree_snapshot(&wt);
        assert!(
            !executor::dispatch_left_work(&wt, Some(&fresh_head), fresh_tree.as_deref(), None),
            "re-taken stamps would hide the primary seat's commit: the contrast this test pins"
        );
    }

    #[test]
    fn commit_before_review_banks_a_refused_round_as_wip() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        std::fs::write(wt.join("leftovers.txt"), "not yet committed\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, None, None);
        state.ceiling_partial_rounds = vec![CeilingPartialRound {
            round: 1,
            kind: CeilingPartialKind::Refused,
            commit: None,
        }];
        let note = commit_before_review(&mut state, &paths, &Config::default(), "impl");
        assert_eq!(note, Some(ceiling_partial_notice(1)));
        let entry = state
            .ceiling_partial_rounds
            .iter()
            .find(|e| e.round == 1)
            .unwrap();
        assert!(matches!(entry.kind, CeilingPartialKind::Wip));
        assert!(entry.commit.is_some(), "head now carries the work");
    }

    #[test]
    fn commit_before_review_leaves_no_mark_on_a_normal_round() {
        let tmp = tempfile::tempdir().unwrap();
        let (wt, branch) = fresh_repo(&tmp);
        std::fs::write(wt.join("leftovers.txt"), "not yet committed\n").unwrap();
        let (paths, mut state) = ceiling_state(&tmp, &wt, &branch, None, None);
        let note = commit_before_review(&mut state, &paths, &Config::default(), "impl");
        assert!(note.is_none());
        assert!(state.ceiling_partial_rounds.is_empty());
    }
}
