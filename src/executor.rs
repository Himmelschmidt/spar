use crate::api;
use crate::cli::Backend;
use crate::config::Config;
use crate::markers;
use crate::paths::SparPaths;
use crate::process::{self, SpawnRequest};
use crate::provider_ref::ProviderRef;
use crate::providers::{self, SpawnOpts, TrustPolicy};
use crate::sandbox;
use crate::state::{FleetSeat, RunState, SeatSource, SlotRole, SlotState, SlotStatus, SlotUsage};
use crate::templates;
use crate::tmux;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Resolve effective backend for a provider under a policy.
pub fn resolve_backend(policy: Backend, provider: &str) -> Backend {
    match policy {
        Backend::Headless => Backend::Headless,
        Backend::Tmux => Backend::Tmux,
        Backend::Auto => {
            if let Some(a) = providers::adapter_named(provider) {
                if a.capabilities().headless {
                    Backend::Headless
                } else if tmux::available() {
                    Backend::Tmux
                } else {
                    Backend::Headless
                }
            } else {
                Backend::Headless
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SlotJob {
    pub slot_id: String,
    pub provider: String,
    pub role: SlotRole,
    pub template: String,
    pub extra_vars: HashMap<String, String>,
    /// Expected primary artifact name under artifacts/
    pub expected_artifact: Option<String>,
    /// Optional model override for CLI `--model` / API body.
    pub model: Option<String>,
}

/// Run multiple slots **concurrently** (live). Dry-run stays sequential for simpler state.
pub fn run_slots_parallel(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    jobs: &[SlotJob],
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    if state.dry_run || jobs.len() == 1 {
        for job in jobs {
            let _ = run_slot(state, paths, cfg, job);
        }
        return Ok(());
    }

    // Prepare prompts + mark running sequentially, then spawn processes in parallel.
    let mut prepared = Vec::new();
    for job in jobs {
        match prepare_slot_execution(state, paths, cfg, job) {
            Ok(p) => prepared.push(p),
            Err(e) => {
                // `prepare_slot_execution`'s own `quota_hit = false` reset sits after its
                // fallible steps (template render, prompt write, provider parse), so a
                // slot that hit quota last round and fails one of those *this* round
                // would otherwise carry a stale `true` into a terminal-phase mapping —
                // routing a template bug to `Phase::Quota` instead of `Phase::Failed`.
                if let Some(s) = state.slot_mut(&job.slot_id) {
                    s.quota_hit = false;
                }
                let _ =
                    mark_slot_failed(state, paths, &job.slot_id, &e.to_string(), None, None, None);
            }
        }
    }
    state.save(paths)?;

    let isolation = state.isolation;
    let backend_policy = state.backend;

    let mut handles = Vec::new();
    for prep in prepared {
        handles.push(std::thread::spawn(move || {
            let outcome = execute_prepared(&prep, isolation, backend_policy);
            (prep.job.slot_id.clone(), outcome, prep)
        }));
    }

    for h in handles {
        match h.join() {
            Ok((slot_id, outcome, prep)) => {
                if let Err(e) = apply_parallel_outcome(state, paths, &slot_id, outcome, &prep) {
                    eprintln!("warning: recording slot {slot_id}'s outcome failed: {e:#}");
                    // Continue, so one slot's failure cannot discard its siblings, but
                    // never leave it claiming to run: that is the permanently-`running`
                    // class arriving by a different door.
                    if let Some(s) = state.slot_mut(&slot_id) {
                        if s.status == SlotStatus::Running {
                            s.status = SlotStatus::Failed;
                            s.error = Some(format!("outcome not recorded: {e}"));
                        }
                    }
                }
            }
            Err(_) => bail!("slot thread panicked"),
        }
    }
    state.save(paths)?;
    Ok(())
}

struct PreparedSlot {
    job: SlotJob,
    cwd: PathBuf,
    log_path: PathBuf,
    prompt_path: PathBuf,
    prompt: String,
    pref: ProviderRef,
    /// Identity + presence env attached to the spawned agent (empty for api slots).
    env: Vec<(String, String)>,
    /// Owned so the supervisor's liveness beat survives the move into a spawn thread.
    paths: SparPaths,
    run_id: String,
    /// True when `cwd` is this slot's *own* recorded worktree. False under
    /// `isolation = "none"`, where every slot runs in the project checkout.
    owns_cwd: bool,
    /// The run's base, for deciding whether a slot missing its artifact left work behind.
    base_commit: Option<String>,
    round: u32,
    /// The native session/thread id an earlier round of this same slot captured
    /// (`markers::read_session_id`), if any. `execute_prepared` passes it to the
    /// adapter's `build_resume`; adapters that don't support resume just ignore it.
    prior_session_id: Option<String>,
    /// The adapter's bare name (e.g. `"codex"`), used to scope the session-id marker so a
    /// provider rotation on this slot id never resumes a different provider's session.
    session_provider: String,
    /// The run's frozen config (O27), carried across the thread boundary so a worker
    /// sizes its budgets and nudge cadence off the same document every other phase reads.
    cfg: Config,
    dry_run: bool,
}

/// Refreshes a live slot's presence heartbeat while its child process runs, throttled
/// to [`crate::bus::LIVENESS_HEARTBEAT_SECS`]. Wired to `run_captured`'s per-poll tick so
/// lease liveness tracks the actual process, not event-driven provider hooks that a whole
/// adapter class (`PresenceSource::None`, e.g. agy) never installs. See the finding at
/// `bus::reserve_at`: without this an alive holder's lease expires and its path is reclaimed.
struct LivenessBeat<'a> {
    paths: &'a SparPaths,
    run_id: &'a str,
    slot_id: &'a str,
    last: std::cell::Cell<std::time::Instant>,
}

impl LivenessBeat<'_> {
    fn tick(&self) {
        if self.last.get().elapsed()
            < Duration::from_secs(crate::bus::LIVENESS_HEARTBEAT_SECS as u64)
        {
            return;
        }
        self.last.set(std::time::Instant::now());
        let _ = crate::bus::heartbeat(self.paths, Some(self.run_id), self.slot_id, "running");
    }
}

/// Wire the adapter's presence source for a CLI slot: install its hook file into the
/// worktree, log any degraded-mode note, and return the identity env every agent
/// carries (`SPAR_AGENT_ID` / `SPAR_RUN_ID` / `SPAR_PROJECT_ROOT`). API slots have no
/// CLI adapter, so they get an empty env. Best-effort — never fails the spawn.
fn wire_slot_presence(
    state: &RunState,
    paths: &SparPaths,
    job: &SlotJob,
    cwd: &Path,
    pref: &ProviderRef,
) -> Vec<(String, String)> {
    if pref.is_api() {
        return Vec::new();
    }
    let cli_name = pref.cli_name().unwrap_or(job.provider.as_str());
    let Some(adapter) = providers::adapter_named(cli_name) else {
        return Vec::new();
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("spar"));
    let identity = providers::presence::SlotIdentity {
        agent_id: &job.slot_id,
        run_id: Some(&state.id),
        project_root: &state.project_root,
        worktree: cwd,
        spar_exe: &exe,
    };
    let wiring = providers::presence::wire(adapter.as_ref(), &identity);
    if let Some(note) = wiring.note {
        let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(note));
    }
    wiring.env
}

fn prepare_slot_execution(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    job: &SlotJob,
) -> Result<PreparedSlot> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.id == job.slot_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown slot {}", job.slot_id))?;
    let cwd = slot
        .cwd
        .clone()
        .unwrap_or_else(|| state.project_root.clone());
    let backend = resolve_backend(state.backend, &job.provider);
    let log_path = paths.log_file(&state.id, &job.slot_id);
    let branch = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == job.slot_id)
        .map(|w| w.branch.clone())
        .unwrap_or_else(|| format!("spar/{}/{}", state.id, job.slot_id));

    let project_root_s = state.project_root.display().to_string();
    let cwd_s = cwd.display().to_string();
    let artifacts_s = paths.artifacts_dir(&state.id).display().to_string();
    let markers_s = paths.markers_dir(&state.id).display().to_string();
    let mailbox_s = paths.mailbox_dir(&state.id).display().to_string();
    let nudge_s = crate::nudge::poll_file(paths, &state.id, &job.slot_id)
        .display()
        .to_string();
    let mut vars = templates::base_vars(&templates::TemplateCtx {
        task: state.task.as_deref().unwrap_or(""),
        project_root: &project_root_s,
        cwd: &cwd_s,
        run_id: &state.id,
        artifacts_dir: &artifacts_s,
        markers_dir: &markers_s,
        mailbox_dir: &mailbox_s,
        slot_id: &job.slot_id,
        provider: &job.provider,
        branch: &branch,
        nudge_file: &nudge_s,
    });
    for (k, v) in &job.extra_vars {
        vars.insert(k.clone(), v.clone());
    }
    let prompt = templates::render(&job.template, &vars)?;
    let prompt_path = paths
        .run_dir(&state.id)
        .join(format!("prompt-{}.md", job.slot_id));
    std::fs::write(&prompt_path, &prompt)?;

    let pref = ProviderRef::parse(&job.provider)?;
    let mut job = job.clone();
    if job.model.is_none() {
        job.model = slot_model_for(Some(state), &job);
    }
    // Drop any prior attempt's terminal/pid markers before this slot goes Running, so a
    // stale `<slot>.failed` doesn't outrank the live process during reconciliation.
    markers::clear_slot(paths, &state.id, &job.slot_id);
    let round = state.round;
    if let Some(s) = state.slot_mut(&job.slot_id) {
        s.status = SlotStatus::Running;
        // A dispatch's outcome fields describe *that* dispatch. Carried into the next
        // one they read as an impossible record (`running` with the prior round's
        // `exit_code: 0`) and, worse, survive onto the next terminal status: a `done`
        // slot still carrying `error: "exit 143"` from the attempt before it.
        s.exit_code = None;
        s.signal = None;
        s.pid = None;
        s.error = None;
        s.usage = None;
        s.quota_hit = false;
        // Stamp the round at dispatch: slot ids are stable across re-dispatch (the
        // implementer keeps its worktree through fix rounds), so this is where a slot
        // joins the round that is running now (O45).
        s.round = round;
        s.exec_backend = Some(pref.backend);
        s.backend = Some(if pref.is_api() {
            "api-sdk".into()
        } else {
            format!("{backend:?}").to_ascii_lowercase()
        });
        s.log_path = Some(log_path.clone());
        s.artifact = job.expected_artifact.clone();
        if s.model.is_none() {
            s.model = job.model.clone();
        }
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot(&job.slot_id, SlotStatus::Running),
    );
    let _ = crate::bus::heartbeat(paths, Some(&state.id), &job.slot_id, "running");
    let env = wire_slot_presence(state, paths, &job, &cwd, &pref);
    let owns_cwd = owns_cwd(state, &job.slot_id, &cwd);
    let session_provider = pref.cli_name().unwrap_or(job.provider.as_str()).to_string();
    let prior_session_id =
        prior_session_for_role(paths, &state.id, &job.slot_id, &session_provider, job.role);

    Ok(PreparedSlot {
        job,
        cwd,
        log_path,
        prompt_path,
        prompt,
        pref,
        env,
        paths: paths.clone(),
        run_id: state.id.clone(),
        base_commit: state.base_commit.clone(),
        owns_cwd,
        round,
        prior_session_id,
        session_provider,
        cfg: cfg.clone(),
        dry_run: state.dry_run,
    })
}

/// Whether `cwd` is the slot's own recorded worktree.
///
/// Recovery's whole premise is "the work in this tree is yours". Under
/// `isolation = "none"` every slot's cwd is `project_root`, so that premise fails for the
/// implementer too and a recovery turn would write up the operator's own WIP as the run's
/// deliverable. Role alone cannot see this — the cwd has to be checked.
fn owns_cwd(state: &RunState, slot_id: &str, cwd: &Path) -> bool {
    state
        .worktrees
        .iter()
        .any(|w| w.slot_id == slot_id && w.path == cwd)
}

/// Read this slot's captured native session id when the role allows a resume.
/// Judging roles (`Reviewer`, `Tester`, `PlanCritic`) always dispatch cold: their
/// verdict judges the current commit, and reopening the session that already
/// delivered it cannot re-judge. The marker file is left alone (the implementer
/// path still needs the mechanism); it is simply not read here. Both native
/// dispatch paths consult this one predicate so they cannot drift apart.
fn prior_session_for_role(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    provider: &str,
    role: SlotRole,
) -> Option<String> {
    if !role.resumes_across_rounds() {
        return None;
    }
    markers::read_session_id(paths, run_id, slot_id, provider)
}

/// Resume a previously captured native session (`prior_session_id`) instead of a cold
/// `build_headless` dispatch, when the adapter supports it (see `ProviderAdapter::build_resume`).
/// Adapters that don't implement `build_resume`, or that decline for this call, fall back
/// to `build_headless` — the only path before this round's codex resume wiring (O63).
///
/// The `bool` reports whether the resume path was taken, so the caller can tell a lost
/// rollout (resume attempted, no session ever established) from an ordinary cold-dispatch
/// failure and retry cold instead of just failing the round — see `resume_lost_its_session`.
fn build_dispatch_command(
    adapter: &dyn providers::ProviderAdapter,
    bin: &Path,
    opts: &SpawnOpts,
    prior_session_id: Option<&str>,
) -> (std::process::Command, bool) {
    if let Some(sid) = prior_session_id {
        if let Some(cmd) = adapter.build_resume(bin, opts, sid) {
            return (cmd, true);
        }
    }
    (adapter.build_headless(bin, opts), false)
}

/// True when a resume dispatch exited without ever establishing a session (no
/// `thread.started` captured). Necessary but not sufficient evidence that the vendor's
/// rollout is gone (pruned, a different `CODEX_HOME`, a moved box) — plenty of other
/// pre-session failures (a bad model override, an expired `auth.json`, a transient
/// network error) also exit with no session id captured. Callers must additionally
/// consult `ProviderAdapter::resume_failure_is_missing_session` on the dispatch's log
/// before treating this as a lost rollout; this predicate alone only narrows to "did no
/// work", not "why".
///
/// Excludes a timeout: a resume that ran the full ceiling without answering is a genuine
/// hang, and retrying cold there would just double the wall-clock cost for the slot's
/// budget instead of recovering anything.
///
/// Does not condition on the exit code. The discriminator that carries the meaning is
/// `session_id.is_none()` — a resume that never emitted `thread.started` did no work
/// regardless of how it exited. Today's codex (0.152.0) always exits non-zero on a
/// missing rollout, but keying on `session_id` alone means a future version that exits
/// 0 on the same non-start still gets the retry instead of leaving a marker that dead-
/// ends every later round of the slot.
fn resume_lost_its_session(used_resume: bool, res: &process::SpawnResult) -> bool {
    used_resume && !res.timed_out && res.stats.session_id.is_none()
}

/// Waits between transient-failure retries. Four attempts total (the initial dispatch
/// plus three retries) at roughly 60s/150s/300s is about 8.5 minutes elapsed, which
/// waits out a ten-minute backend window given the first failure already burned a few
/// minutes doing real work. Named constants, not config — `sleep` is injected so tests
/// advance instantly instead of sleeping this schedule, and
/// `SPAR_TRANSIENT_RETRY_BACKOFF_SECS` (a comma list like `0,0,0`) overrides the waits
/// for the same reason. That env var is a test seam, not a user knob: it exists only
/// so a live end-to-end run never sleeps the real schedule under test.
const TRANSIENT_RETRY_BACKOFF_SECS: [u64; 3] = [60, 150, 300];

fn parse_backoff_list(raw: &str) -> Vec<u64> {
    raw.split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

fn transient_backoff_secs() -> Vec<u64> {
    if let Ok(raw) = std::env::var("SPAR_TRANSIENT_RETRY_BACKOFF_SECS") {
        let parsed = parse_backoff_list(&raw);
        if !parsed.is_empty() {
            // A stray value in an operator's environment silently reshapes
            // production retries (dropped entries mean fewer attempts *and*
            // shorter waits), so say so loudly instead of just shrinking.
            if parsed.len() != raw.split(',').count() {
                eprintln!(
                    "warning: ignoring unparseable entries in SPAR_TRANSIENT_RETRY_BACKOFF_SECS={raw:?}"
                );
            }
            return parsed;
        }
        // All-garbage: fall through to the real schedule rather than retrying
        // zero times (an empty backoff list means no retries at all).
        eprintln!(
            "warning: ignoring SPAR_TRANSIENT_RETRY_BACKOFF_SECS={raw:?}: no parseable entries"
        );
    }
    TRANSIENT_RETRY_BACKOFF_SECS.to_vec()
}

/// The production backoff sleep: waits `total`, but in slices so liveness keeps ticking
/// through it. The longest wait in the schedule is 300s and the slot's path-reserve
/// lease TTL is exactly 300s (`bus::reserve_at`), so a wait that never ticks leaves
/// presence stale for its whole duration and puts the lease on its own reclaim
/// boundary — survived today only by that comparison being inclusive, and only while
/// nothing else contends for the path. Slicing is invisible to the seam's contract:
/// callers still hand over one whole wait, and the injected test seam still records it
/// as one.
fn sleep_ticking(total: Duration, tick: &dyn Fn()) {
    const SLICE: Duration = Duration::from_secs(30);
    let mut left = total;
    while left > SLICE {
        std::thread::sleep(SLICE);
        tick();
        left -= SLICE;
    }
    std::thread::sleep(left);
    tick();
}

/// `<run_dir>/logs/<slot>.transient-retry-N.log` — a sibling of the shared log path,
/// copied just before a transient retry overwrites it (`run_captured` truncates via
/// `File::create`). Same preservation the lost-resume path already does: without it
/// the retry's own output is all that remains and the failure that caused the wait
/// is gone.
fn transient_retry_log_path(log_path: &Path, attempt: usize) -> PathBuf {
    let stem = log_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("slot");
    log_path.with_file_name(format!("{stem}.transient-retry-{attempt}.log"))
}

/// Runs `req`, recovers from a lost-rollout resume (clears the marker, retries cold once
/// — see `resume_lost_its_session` and `ProviderAdapter::resume_failure_is_missing_session`),
/// retries a transient provider failure with backoff (see
/// `ProviderAdapter::dispatch_failure_is_transient`), and persists whatever session id
/// the (possibly retried) dispatch captured. Shared by `execute_prepared` and
/// `run_headless`, which differ only in how they build `req`/`opts` and their
/// pid-capture `sink` — the recovery-and-persist sequence itself must not drift
/// between the two, since a real bug here silently disables resume for the affected path
/// (see `dispatch_records_session_id_and_recovers_from_lost_resume`'s test coverage).
///
/// `resume_attempt` is the session id the initial `req` tried to resume, if any
/// (`None` for a cold dispatch). `sleep` is the backoff wait, injected so tests never
/// sleep the real schedule.
#[allow(clippy::too_many_arguments)]
fn dispatch_with_resume_recovery(
    adapter: &dyn providers::ProviderAdapter,
    bin: &Path,
    opts: &SpawnOpts,
    resume_attempt: Option<&str>,
    isolation: crate::config::IsolationMode,
    req: SpawnRequest,
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    session_provider: &str,
    sink: &dyn Fn(u32),
    tick: &dyn Fn(),
    sleep: &dyn Fn(Duration),
) -> Result<process::SpawnResult> {
    let used_resume = resume_attempt.is_some();
    // Which dispatch ran last matters after retries: each retry re-resolves the
    // marker, so the post-loop lost-session and stale-resume checks must consult
    // the *last* dispatch's resume attempt, not the initial one.
    let mut last_resume_attempt: Option<String> = resume_attempt.map(str::to_string);
    let mut last_used_resume = used_resume;
    let cwd = req.cwd.clone();
    let log_path = req.log_path.clone();
    let env = req.env.clone();
    let timeout = req.timeout;
    let mut res = process::run_captured(&req, Some(sink), Some(tick))?;
    // A usage error is spar's fault (it built a command line the provider rejected),
    // never the session's: it must be reported as such, never retried, and the marker
    // left untouched — a cold retry of the same bad command line would just fail again
    // while destroying a possibly-valid session id.
    if !adapter.is_usage_error(res.exit_code)
        && resume_lost_its_session(used_resume, &res)
        && adapter.resume_failure_is_missing_session(
            &std::fs::read_to_string(&log_path).unwrap_or_default(),
            resume_attempt,
        )
    {
        // The rollout this slot's marker pointed at is gone (pruned, a different
        // CODEX_HOME, a moved box): clear it so the *next* round doesn't repeat the same
        // failure, and retry this round cold, once, rather than losing it outright.
        markers::clear_session_id(paths, run_id, slot_id, session_provider);
        let _ = crate::events::append(
            paths,
            run_id,
            &crate::events::Event::slot_note(
                slot_id,
                "resume lost its session (no thread.started); retrying cold dispatch once",
            ),
        );
        // `run_captured` truncates `log_path`; preserve the failed resume's own log
        // (e.g. codex's "no rollout found") before the cold retry overwrites it.
        let _ = std::fs::copy(&log_path, lost_resume_log_path(&log_path));
        res = cold_redispatch(
            adapter, bin, opts, isolation, &cwd, &env, timeout, &log_path, sink, tick,
        )?;
        last_used_resume = false;
        last_resume_attempt = None;
    }
    // A transient provider failure after real work (muse's backend 404, which reads as
    // a model-access error with exit 1): back off and re-dispatch rather than failing
    // the slot. Each retry re-resolves through `build_dispatch_command`, so whenever a
    // session id was captured the retry is a resume of the same session, not a cold
    // restart — the work the failed attempt did stays in context. Gated on at least
    // one completed tool call: a first-call failure is indistinguishable from a
    // genuinely wrong model name or a dead entitlement and must fail fast instead of
    // waiting out a window that will never clear. Timeouts, signals, and usage errors
    // never retry: a hang, a kill, and spar's own bad command line are not the backend
    // being briefly sick.
    let backoff = transient_backoff_secs();
    let mut retries = 0usize;
    while retries < backoff.len()
        && !res.timed_out
        && res.signal.is_none()
        && res.exit_code != Some(0)
        && !adapter.is_usage_error(res.exit_code)
        && res.stats.tools >= 1
        && adapter
            .dispatch_failure_is_transient(&std::fs::read_to_string(&log_path).unwrap_or_default())
    {
        let wait = Duration::from_secs(backoff[retries]);
        let _ = std::fs::copy(&log_path, transient_retry_log_path(&log_path, retries + 1));
        // Persist before re-resolving: the failed attempt's captured session id is what
        // makes this retry a resume rather than a cold restart.
        if let Some(sid) = res.stats.session_id.clone() {
            let _ = markers::write_session_id(paths, run_id, slot_id, session_provider, &sid);
        }
        let _ = crate::events::append(
            paths,
            run_id,
            &crate::events::Event::slot_note(
                slot_id,
                format!(
                    "transient provider failure (attempt {} of {}): waiting {}s before retrying{}",
                    retries + 1,
                    backoff.len() + 1,
                    wait.as_secs(),
                    res.stats
                        .session_id
                        .as_deref()
                        .map(|sid| format!(" (resuming session {sid})"))
                        .unwrap_or_default(),
                ),
            ),
        );
        sleep(wait);
        let prior = markers::read_session_id(paths, run_id, slot_id, session_provider);
        let (cmd, retry_used_resume) = build_dispatch_command(adapter, bin, opts, prior.as_deref());
        let (program, args) = providers::command_to_parts(&cmd);
        let (program, args) = sandbox::maybe_wrap(isolation, &cwd, &program, &args);
        let req = SpawnRequest {
            program,
            args,
            cwd: cwd.clone(),
            log_path: log_path.clone(),
            env: env.clone(),
            timeout,
        };
        res = process::run_captured(&req, Some(sink), Some(tick))?;
        last_used_resume = retry_used_resume;
        last_resume_attempt = prior;
        retries += 1;
    }
    // A session can die between transient attempts: the loop above only retries the
    // transient signature, so a retry that died pre-session with a missing-session
    // signature would otherwise exit with the dead id still in the marker. One
    // bounded cold retry, same as the initial lost-resume path — not a loop, so a
    // missing session costs one extra dispatch, never a spiral.
    if !adapter.is_usage_error(res.exit_code)
        && resume_lost_its_session(last_used_resume, &res)
        && adapter.resume_failure_is_missing_session(
            &std::fs::read_to_string(&log_path).unwrap_or_default(),
            last_resume_attempt.as_deref(),
        )
    {
        markers::clear_session_id(paths, run_id, slot_id, session_provider);
        let _ = crate::events::append(
            paths,
            run_id,
            &crate::events::Event::slot_note(
                slot_id,
                "resume lost its session on a retry (no session established); retrying cold dispatch once",
            ),
        );
        let _ = std::fs::copy(&log_path, lost_resume_log_path(&log_path));
        res = cold_redispatch(
            adapter, bin, opts, isolation, &cwd, &env, timeout, &log_path, sink, tick,
        )?;
        last_used_resume = false;
        last_resume_attempt = None;
    }
    // Persisted regardless of this dispatch's own outcome: a captured thread id is what
    // lets the *next* round resume instead of a cold dispatch (O63), and that is worth
    // keeping even off a failed attempt — a resumed thread still holds real progress.
    if let Some(sid) = &res.stats.session_id {
        let _ = markers::write_session_id(paths, run_id, slot_id, session_provider, sid);
    }
    // An exit-0 resume against a session the vendor no longer has mints a brand-new
    // session instead of failing (muse: exit 0 on an unknown id, `resume: false` on
    // the fresh id), so `resume_lost_its_session` never fires — the captured id is
    // `Some`, just not the requested one. The dispatch itself succeeded, so there is
    // nothing to retry; note it so the operator knows this round ran with fresh
    // context. The new marker (persisted above) already self-heals.
    let stale_resume = if last_used_resume
        && !res.timed_out
        && res.exit_code == Some(0)
        && !adapter.is_usage_error(res.exit_code)
    {
        match (
            last_resume_attempt.as_deref(),
            res.stats.session_id.as_deref(),
        ) {
            (Some(requested), Some(captured)) if requested != captured => {
                let gone = adapter.resume_failure_is_missing_session(
                    &std::fs::read_to_string(&log_path).unwrap_or_default(),
                    Some(requested),
                );
                gone.then(|| (requested.to_string(), captured.to_string()))
            }
            _ => None,
        }
    } else {
        None
    };
    if let Some((requested, captured)) = stale_resume {
        let _ = crate::events::append(
            paths,
            run_id,
            &crate::events::Event::slot_note(
                slot_id,
                format!(
                    "resumed session {requested} was gone; continued on new session {captured} with fresh context"
                ),
            ),
        );
    }
    Ok(res)
}

/// One cold `build_headless` dispatch through the same sandbox wrapping as the
/// resume path: the shared tail of every lost-session recovery in
/// `dispatch_with_resume_recovery`, factored out so the initial, post-retry, and
/// future recovery sites cannot drift apart.
#[allow(clippy::too_many_arguments)]
fn cold_redispatch(
    adapter: &dyn providers::ProviderAdapter,
    bin: &Path,
    opts: &SpawnOpts,
    isolation: crate::config::IsolationMode,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
    log_path: &Path,
    sink: &dyn Fn(u32),
    tick: &dyn Fn(),
) -> Result<process::SpawnResult> {
    let cold = adapter.build_headless(bin, opts);
    let (program, args) = providers::command_to_parts(&cold);
    let (program, args) = sandbox::maybe_wrap(isolation, cwd, &program, &args);
    let cold_req = SpawnRequest {
        program,
        args,
        cwd: cwd.to_path_buf(),
        log_path: log_path.to_path_buf(),
        env: env.to_vec(),
        timeout,
    };
    process::run_captured(&cold_req, Some(sink), Some(tick))
}

fn execute_prepared(
    prep: &PreparedSlot,
    isolation: crate::config::IsolationMode,
    backend_policy: Backend,
) -> Result<SlotOutcome> {
    let soft = timeout_for_role(&prep.cfg, prep.job.role);
    let timeout = hard_ceiling_for_role(&prep.cfg, prep.job.role);
    if prep.pref.is_api() {
        let expected = prep.job.expected_artifact.as_ref().map(|n| {
            // artifact path reconstructed from log path parent layout
            prep.log_path
                .parent()
                .and_then(|p| p.parent())
                .map(|run| run.join("artifacts").join(n))
                .unwrap_or_else(|| PathBuf::from(n))
        });
        let model = prep.job.model.clone();
        reset_api_slot_log(&prep.log_path);
        let (ok, err, usage) = crate::api::run_api_slot(&crate::api::runtime::ApiSlotRequest {
            provider_name: &prep.pref.name,
            prompt: &prep.prompt,
            cwd: &prep.cwd,
            log_path: &prep.log_path,
            expected_artifact: expected.as_deref(),
            timeout,
            dry_run: false,
            model_override: model.clone(),
        })?;
        let slot_usage = SlotUsage {
            slot_id: prep.job.slot_id.clone(),
            provider: prep.pref.storage_key(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: 0,
            context_tokens: usage.peak_input_tokens,
            billed_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
            tools: 0,
            model: usage.model.or(model),
            cost_usd: None,
            subagent_stats: None,
            model_usage: Default::default(),
        };
        return Ok(if ok {
            SlotOutcome {
                ok: true,
                pid: None,
                exit_code: Some(0),
                signal: None,
                error: None,
                usage: Some(slot_usage),
                agy_quota_hit: false,
                quota_rejected: None,
                quota_resets_at: None,
                quota_recovered: false,
            }
        } else {
            SlotOutcome {
                ok: false,
                pid: None,
                exit_code: Some(1),
                signal: None,
                error: err,
                usage: Some(slot_usage),
                agy_quota_hit: false,
                quota_rejected: None,
                quota_resets_at: None,
                quota_recovered: false,
            }
        });
    }

    let backend = resolve_backend(backend_policy, &prep.job.provider);
    let _ = backend;
    let adapter = providers::adapter_named(&prep.job.provider)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {}", prep.job.provider))?;
    let bin = adapter
        .resolve_binary()
        .ok_or_else(|| anyhow::anyhow!("provider {} not on PATH", prep.job.provider))?;
    if provider_is_agy(&prep.job.provider) {
        if let Some(root) = providers::agy_telemetry::root() {
            let _ = providers::agy_telemetry::ensure_statusline_hook(&root);
        }
    }
    let opts = SpawnOpts {
        prompt: prep.prompt.clone(),
        prompt_file: Some(prep.prompt_path.clone()),
        cwd: prep.cwd.clone(),
        trust: TrustPolicy::FullAuto,
        extra_args: vec![],
        model: prep.job.model.clone(),
        timeout_secs: Some(timeout.as_secs()),
    };
    let (cmd, used_resume) = build_dispatch_command(
        adapter.as_ref(),
        &bin,
        &opts,
        prep.prior_session_id.as_deref(),
    );
    let (program, args) = providers::command_to_parts(&cmd);
    let (program, args) = sandbox::maybe_wrap(isolation, &prep.cwd, &program, &args);
    let cmdline = format!("{} {}", program.display(), args.join(" "));
    // Adapter-derived env (e.g. muse's self-timeout alignment) rides the request:
    // `command_to_parts` drops anything set via `cmd.env`, so merging here is the
    // only path that reaches the child.
    let mut dispatch_env = prep.env.clone();
    dispatch_env.extend(adapter.extra_env(&opts));
    let req = SpawnRequest {
        program,
        args,
        cwd: prep.cwd.clone(),
        log_path: prep.log_path.clone(),
        env: dispatch_env,
        timeout,
    };
    let pid_cell = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let sink_cell = pid_cell.clone();
    let pid_file = pid_marker_from_log(&prep.log_path, &prep.job.slot_id);
    let sink = move |pid: u32| {
        sink_cell.store(pid, std::sync::atomic::Ordering::SeqCst);
        if let Some(f) = &pid_file {
            let _ = std::fs::write(f, process::PidToken::capture(pid).encode());
        }
    };
    let beat = LivenessBeat {
        paths: &prep.paths,
        run_id: &prep.run_id,
        slot_id: &prep.job.slot_id,
        last: std::cell::Cell::new(std::time::Instant::now()),
    };
    let watch = crate::nudge::NudgeWatch::new(
        crate::nudge::WatchSpec {
            paths: &prep.paths,
            run_id: &prep.run_id,
            slot_id: &prep.job.slot_id,
            provider: &prep.job.provider,
            role: prep.job.role,
            log_path: &prep.log_path,
            artifacts: owed_artifacts(
                prep.job.role,
                &prep.job.slot_id,
                prep.job.expected_artifact.as_deref(),
            ),
            soft,
            ceiling: timeout,
            label: timeout_label(prep.job.role),
            dry_run: prep.dry_run,
        },
        &prep.cfg,
    );
    let tick = || {
        beat.tick();
        watch.tick();
    };
    let resume_attempt = if used_resume {
        prep.prior_session_id.as_deref()
    } else {
        None
    };
    // The artifact gate below only counts a write from this dispatch, so the
    // start instant is captured immediately before the child is spawned. A cold
    // retry or transient retry inside `dispatch_with_resume_recovery` is still
    // this dispatch, and its write lands after this capture either way.
    let dispatch_start = SystemTime::now();
    let mut res = dispatch_with_resume_recovery(
        adapter.as_ref(),
        &bin,
        &opts,
        resume_attempt,
        isolation,
        req,
        &prep.paths,
        &prep.run_id,
        &prep.job.slot_id,
        &prep.session_provider,
        &sink,
        &tick,
        &|d| sleep_ticking(d, &tick),
    )?;
    let pid = load_pid(&pid_cell);
    // Before the gates below, and before any state save: markers outlive an orchestrator
    // that dies between here and the save, `state.json` does not (O49).
    let mut verdict = markers::DispatchVerdict {
        ok: !res.timed_out && res.exit_code == Some(0),
        round: prep.round,
        pid,
        exit_code: res.exit_code,
        signal: res.signal,
        reason: None,
    };
    let _ = markers::write_dispatch_verdict(&prep.paths, &prep.run_id, &prep.job.slot_id, &verdict);
    let agy_quota_hit = enrich_agy_stats(
        &mut res.stats,
        &prep.job.provider,
        &prep.cwd,
        &prep.log_path,
        &prep.paths,
    );
    enrich_muse_stats(&mut res.stats, &prep.job.provider, &prep.log_path);
    enrich_opencode_stats(
        &mut res.stats,
        &prep.job.provider,
        &prep.log_path,
        &prep.paths,
        &prep.run_id,
        &prep.job.slot_id,
    );
    let quota_rejected = res.stats.quota_rejected.clone();
    let quota_resets_at = resets_at_from_epoch_secs(res.stats.quota_resets_at);
    let quota_recovered = res.stats.quota_recovered;
    let usage = usage_from_stream(&prep.job.slot_id, &prep.job.provider, &res.stats);
    if res.timed_out {
        let error = crate::nudge::ceiling_error(timeout, soft, timeout_label(prep.job.role));
        let _ = crate::events::append(
            &prep.paths,
            &prep.run_id,
            &crate::events::Event::slot_note(&prep.job.slot_id, &error),
        );
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: res.exit_code,
            signal: res.signal,
            error: Some(error),
            usage: Some(usage),
            agy_quota_hit,
            quota_rejected: quota_rejected.clone(),
            quota_resets_at,
            quota_recovered,
        });
    }
    if res.exit_code != Some(0) {
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: res.exit_code,
            signal: res.signal,
            error: Some(dispatch_error(
                adapter.as_ref(),
                &prep.log_path,
                &cmdline,
                res.exit_code,
                res.signal,
            )),
            usage: Some(usage),
            agy_quota_hit,
            quota_rejected: quota_rejected.clone(),
            quota_resets_at,
            quota_recovered,
        });
    }
    // A clean exit is not success on its own: a slot that produced no artifact (e.g. an
    // adapter that never received its prompt) must fail, not silently pass. Only a
    // write from this dispatch counts — the previous round's file is still on
    // disk, and reading it as this round's verdict is the stale-verdict defect.
    // Mirrors the gate in the sequential `run_headless` path.
    if let Some(name) = &prep.job.expected_artifact {
        let path = prep.paths.artifact(&prep.run_id, name);
        // Short grace for late writers, then the freshness verdict stands.
        let fresh = artifact_fresh_since(&path, dispatch_start)
            || markers::wait_for_artifact(
                &prep.paths,
                &prep.run_id,
                name,
                dispatch_start,
                Duration::from_secs(2),
            )
            .unwrap_or(false);
        if !fresh {
            // No judgment landed this dispatch. Name the precise cause first: a
            // native-CLI judging slot that ran no tools could not have read the
            // diff. A fresh artifact would have vouched for the dispatch
            // regardless of the parsed tool count, so this only fires when the
            // artifact is stale too. Api-backed slots return before this point
            // with a hardcoded `tools: 0`, so they never reach this gate.
            if let Some(error) = judging_no_tool_error(
                prep.job.role,
                &prep.job.slot_id,
                usage.tools,
                prep.pref.is_api(),
            ) {
                note_no_judgment(
                    &prep.paths,
                    &prep.run_id,
                    &prep.job.slot_id,
                    &mut verdict,
                    &error,
                );
                return Ok(SlotOutcome {
                    ok: false,
                    pid,
                    exit_code: Some(0),
                    signal: None,
                    error: Some(error),
                    usage: Some(usage),
                    agy_quota_hit,
                    quota_rejected: quota_rejected.clone(),
                    quota_resets_at,
                    quota_recovered,
                });
            }
            let recovered = recover_artifact(&ArtifactRecovery {
                paths: &prep.paths,
                run_id: &prep.run_id,
                slot_id: &prep.job.slot_id,
                role: prep.job.role,
                owns_cwd: prep.owns_cwd,
                provider: &prep.job.provider,
                model: prep.job.model.clone(),
                cwd: &prep.cwd,
                log_path: &prep.log_path,
                prompt_path: &recovery_prompt_path(&prep.prompt_path, &prep.job.slot_id),
                env: &prep.env,
                isolation,
                base_commit: prep.base_commit.as_deref(),
                artifact: &path,
            });
            if !recovered {
                let error = format!("missing expected artifact {name}");
                verdict.ok = false;
                verdict.reason = Some(error.clone());
                let _ = markers::write_dispatch_verdict(
                    &prep.paths,
                    &prep.run_id,
                    &prep.job.slot_id,
                    &verdict,
                );
                return Ok(SlotOutcome {
                    ok: false,
                    pid,
                    exit_code: Some(0),
                    signal: None,
                    error: Some(error),
                    usage: Some(usage),
                    agy_quota_hit,
                    quota_rejected: quota_rejected.clone(),
                    quota_resets_at,
                    quota_recovered,
                });
            }
        }
    }
    Ok(SlotOutcome {
        ok: true,
        pid,
        exit_code: Some(0),
        signal: None,
        error: None,
        usage: Some(usage),
        agy_quota_hit,
        quota_rejected: quota_rejected.clone(),
        quota_resets_at,
        quota_recovered,
    })
}

/// Budget for the artifact-only recovery turn. Deliberately short: the work is already
/// done and on disk, so this turn writes one file. A slot that spends longer than this
/// is doing something other than what it was asked.
const ARTIFACT_RECOVERY_SECS: u64 = 600;

/// Everything a recovery turn needs. A struct because the call takes ten values and
/// several of them are paths that must not be swapped.
struct ArtifactRecovery<'a> {
    paths: &'a SparPaths,
    run_id: &'a str,
    slot_id: &'a str,
    role: SlotRole,
    /// `cwd` is this slot's own worktree. See [`owns_cwd`].
    owns_cwd: bool,
    provider: &'a str,
    model: Option<String>,
    cwd: &'a Path,
    log_path: &'a Path,
    prompt_path: &'a Path,
    env: &'a [(String, String)],
    isolation: crate::config::IsolationMode,
    base_commit: Option<&'a str>,
    artifact: &'a Path,
}

/// Only the implementer may be recovered.
///
/// Recovery infers an artifact from whatever is in the slot's cwd, and it is the only
/// role for which that inference is sound:
///
/// - `tester` and `reviewer` are pointed at the *implementer's* worktree, so
///   `slot_has_work` is true for them whether or not they did anything. A recovered
///   `suite.md` reading `## Result: pass` sets the authoritative gate green with no suite
///   ever having run.
/// - `test_author` writes the `AC-n` acceptance contract. Prose passes the non-empty
///   check, `parse_contract_criteria` then finds no criteria, and the ship gate goes
///   vacuous — a green run with nothing holding it.
/// - `ranker` runs in `project_root`, whose tree is somebody else's WIP.
///
/// A failed slot in those roles is the correct outcome: the existing salvage path records
/// why, and a human sees it. Only the implementer's deliverable is genuinely on disk with
/// only the write-up missing.
fn role_is_recoverable(role: SlotRole) -> bool {
    matches!(role, SlotRole::Implementer)
}

/// Work the slot left behind: uncommitted changes, or commits past the run's base.
///
/// `base_commit` is `None` for pre-O26 runs and when git couldn't answer; there the
/// dirty check stands alone rather than guessing at HEAD.
fn slot_has_work(cwd: &Path, base_commit: Option<&str>) -> bool {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) {
        return true;
    }
    match (base_commit, git(&["rev-parse", "HEAD"])) {
        (Some(base), Some(head)) => head != base,
        _ => false,
    }
}

/// Re-prompt a clean-exiting slot for its artifact alone.
///
/// A slot that wrote and committed code and then exited without its summary is not a
/// failed slot — the deliverable is on disk and only the write-up is missing. Failing it
/// there throws away a full build and re-dispatches from scratch, which is the most
/// expensive way to recover the cheapest thing to reproduce.
///
/// Only fires for the implementer, and only when the tree actually holds work — a slot
/// that did nothing still fails.
fn recover_artifact(r: &ArtifactRecovery) -> bool {
    if !role_is_recoverable(r.role) || !r.owns_cwd || !slot_has_work(r.cwd, r.base_commit) {
        return false;
    }
    let Some(adapter) = providers::adapter_named(r.provider) else {
        return false;
    };
    let Some(bin) = adapter.resolve_binary() else {
        return false;
    };
    // The recovery turn streams to its own `<slot>.recovery.log`, so the finished
    // turn's recorded session id is taken out for the duration of the recovery spawn
    // below and restored once it returns (see the bottom of this function) — nothing
    // running concurrently can mistake it for the recovery turn's. The value also gets
    // stashed into `session_id_recovery_stash` before the clear, not just held in this
    // local — spar getting killed mid-recovery (`spar stop`, SIGKILL, a panic) never
    // returns here to restore it, and `muse_telemetry::enrich` / `nudge.rs`'s
    // `live_billed` both fall back to the stash field, so a crash loses only the live
    // session link, not the durable usage record.
    let recovered_session_id = process::StreamStats::load(r.log_path).and_then(|mut stats| {
        let id = stats.session_id.take();
        if let Some(id) = &id {
            stats.session_id_recovery_stash = Some(id.clone());
        }
        if id.is_some() {
            let _ = stats.save(r.log_path);
        }
        id
    });
    let prompt = format!(
        "Your previous turn ended without writing `{}`, but your work is still in this \
         worktree ({}).\n\nWrite that file now, and nothing else. Read your own changes \
         (`git status`, `git diff`, `git log`) and summarize what you did, what you \
         verified, and anything you left undone or uncertain.\n\nDo not start new work. \
         Do not run builds, linters or tests. Do not modify any file other than the one \
         named above.\n",
        r.artifact.display(),
        r.cwd.display()
    );
    if std::fs::write(r.prompt_path, &prompt).is_err() {
        return false;
    }
    let timeout = Duration::from_secs(ARTIFACT_RECOVERY_SECS);
    let opts = SpawnOpts {
        prompt,
        prompt_file: Some(r.prompt_path.to_path_buf()),
        cwd: r.cwd.to_path_buf(),
        trust: TrustPolicy::FullAuto,
        extra_args: vec![],
        model: r.model.clone(),
        timeout_secs: Some(timeout.as_secs()),
    };
    let cmd = adapter.build_headless(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let (program, args) = sandbox::maybe_wrap(r.isolation, r.cwd, &program, &args);
    let mut recovery_env = r.env.to_vec();
    recovery_env.extend(adapter.extra_env(&opts));
    let req = SpawnRequest {
        program,
        args,
        cwd: r.cwd.to_path_buf(),
        // Its own log. `run_captured` opens with `File::create`, so reusing the slot's
        // would truncate the transcript of the turn that did all the work — which is both
        // the operator's only diagnosis and what `salvage_expected_artifact` tails when
        // recovery itself fails.
        log_path: recovery_log_path(r.log_path),
        env: recovery_env,
        timeout,
    };
    // Tracked like any other slot spawn: without the pid marker this agent is invisible to
    // `stop --abandoned`, and without the heartbeat a recovery longer than
    // `RESERVE_LEASE_TTL_SECS` lets a live holder's path reserves be reclaimed.
    let pid_file = markers_pid_path(r.paths, r.run_id, r.slot_id);
    let sink = move |pid: u32| {
        let _ = std::fs::write(&pid_file, process::PidToken::capture(pid).encode());
    };
    let beat = LivenessBeat {
        paths: r.paths,
        run_id: r.run_id,
        slot_id: r.slot_id,
        last: std::cell::Cell::new(std::time::Instant::now()),
    };
    let tick = || beat.tick();
    let spawned = process::run_captured(&req, Some(&sink), Some(&tick));
    if let Some(id) = recovered_session_id {
        if let Some(mut stats) = process::StreamStats::load(r.log_path) {
            stats.session_id = Some(id);
            stats.session_id_recovery_stash = None;
            let _ = stats.save(r.log_path);
        }
    }
    if spawned.is_err() {
        return false;
    }
    artifact_written(r.artifact)
}

/// `<run_dir>/logs/<slot>.recovery.log`.
fn recovery_log_path(log_path: &Path) -> PathBuf {
    let stem = log_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("slot");
    log_path.with_file_name(format!("{stem}.recovery.log"))
}

/// `<run_dir>/logs/<slot>.lost-resume.log` — a sibling of the shared log path, copied to
/// just before `resume_lost_its_session`'s cold retry overwrites it (`run_captured`
/// truncates via `File::create`). Preserves the failed resume's own stderr (e.g. codex's
/// `no rollout found for thread id …`), which is otherwise gone by the time anyone reads
/// the log — the retry's own output is all that would remain.
fn lost_resume_log_path(log_path: &Path) -> PathBuf {
    let stem = log_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("slot");
    log_path.with_file_name(format!("{stem}.lost-resume.log"))
}

fn markers_pid_path(paths: &SparPaths, run_id: &str, slot_id: &str) -> PathBuf {
    paths.markers_dir(run_id).join(format!("{slot_id}.pid"))
}

fn artifact_written(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 0
}

/// Whether the expected artifact was written by *this* dispatch: non-empty and
/// its mtime at or after the dispatch start instant (1s grace, see
/// `markers::freshness_floor`). The previous round's file is still on disk
/// when a slot re-dispatches, so a presence-only check reads a dead verdict
/// as this round's. Callers that genuinely mean "does a file exist"
/// (`recover_artifact`'s post-turn check, `salvage_carry_forward`) keep using
/// `artifact_written`; the dispatch gates use this.
fn artifact_fresh_since(path: &Path, since: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() == 0 {
        return false;
    }
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    mtime >= markers::freshness_floor(since)
}

/// Whether a native-CLI judging dispatch produced no judgment at all. A
/// reviewer or tester that ran zero tools did not read the diff: reading and
/// writing are tool calls. Returns the failure text when the gate fires.
/// Only consulted when no fresh artifact exists: a fresh write vouches for the
/// dispatch regardless of the parsed tool count, so a working turn the stream
/// parser undercounts still passes. Api-backed slots never reach this: their
/// `SlotUsage` hardcodes `tools: 0` by construction (`run_api` and
/// `execute_prepared`'s api arm), so `is_api` exempts them.
fn judging_no_tool_error(
    role: SlotRole,
    slot_id: &str,
    tools: u32,
    is_api: bool,
) -> Option<String> {
    if is_api || tools > 0 {
        return None;
    }
    match role {
        SlotRole::Reviewer | SlotRole::Tester => Some(format!(
            "{} slot {slot_id} ran no tools (tools == 0): no judgment this dispatch",
            role.as_config_key(),
        )),
        _ => None,
    }
}

/// Record a no-judgment failure where the operator can grep for it: one
/// `events.jsonl` slot-note line naming the slot and the cause, plus the
/// on-disk dispatch verdict downgraded. The caller builds the `SlotOutcome`.
fn note_no_judgment(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    verdict: &mut markers::DispatchVerdict,
    error: &str,
) {
    let _ = crate::events::append(
        paths,
        run_id,
        &crate::events::Event::slot_note(slot_id, error),
    );
    verdict.ok = false;
    verdict.reason = Some(error.to_string());
    let _ = markers::write_dispatch_verdict(paths, run_id, slot_id, verdict);
}

/// `<run_dir>/prompt-<slot>-artifact.md` — the recovery prompt, kept beside the slot's
/// original so a failed recovery is readable after the fact.
fn recovery_prompt_path(prompt_path: &Path, slot_id: &str) -> PathBuf {
    prompt_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("prompt-{slot_id}-artifact.md"))
}

/// Derive `<run_dir>/markers/<slot>.pid` from a slot log path (`<run_dir>/logs/<slot>.log`).
fn pid_marker_from_log(log_path: &Path, slot_id: &str) -> Option<PathBuf> {
    log_path
        .parent()
        .and_then(|logs| logs.parent())
        .map(|run| run.join("markers").join(format!("{slot_id}.pid")))
}

fn load_pid(cell: &std::sync::atomic::AtomicU32) -> Option<u32> {
    match cell.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        p => Some(p),
    }
}

/// The provider's own final message for a codex dispatch, when it is usable as
/// salvage input: the slot-scoped `--output-last-message` file beside the slot
/// log, non-empty and fresh for this dispatch. Freshness is relative to the slot
/// log, which `run_captured` truncates at spawn: a last-message file older than
/// the log is left over from an earlier round (O89) and must not be mistaken for
/// this dispatch's output. `None` for any other provider and for a missing,
/// empty, stale or unreadable file. Read-only: never writes.
fn codex_last_message(log_path: &Path, provider: &str) -> Option<String> {
    if !provider_is_codex(provider) {
        return None;
    }
    let path = providers::codex::last_message_path(log_path);
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == 0 {
        return None;
    }
    let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
    let log_mtime = std::fs::metadata(log_path).ok()?.modified().ok()?;
    if mtime < markers::freshness_floor(log_mtime) {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(clamp_chars(&text, 6000))
}

/// True when a provider ref resolves to the codex adapter (`cli:codex`, bare
/// `codex`, `codex@model`).
fn provider_is_codex(provider: &str) -> bool {
    ProviderRef::parse(provider)
        .ok()
        .and_then(|p| p.cli_name().map(|n| n == "codex"))
        .unwrap_or(provider == "codex")
}

/// True when a provider ref resolves to the agy adapter (`cli:agy`, bare `agy`, `agy@model`).
fn provider_is_agy(provider: &str) -> bool {
    ProviderRef::parse(provider)
        .ok()
        .and_then(|p| p.cli_name().map(|n| n == "agy"))
        .unwrap_or(provider == "agy")
}

/// muse emits no token counts on stdout at all; usage lives only in its session log.
/// Sum that (including the subagent sessions muse fans out per turn) and rewrite the
/// slot's stats sidecar so `stats.json` and the TUI reflect real spend.
fn enrich_muse_stats(stats: &mut process::StreamStats, provider: &str, log_path: &Path) {
    let is_muse = ProviderRef::parse(provider)
        .ok()
        .and_then(|p| p.cli_name().map(|n| n == "muse"))
        .unwrap_or(provider == "muse");
    if !is_muse {
        return;
    }
    providers::muse_telemetry::enrich(stats);
    let _ = stats.save(log_path);
}

fn is_opencode_provider(provider: &str) -> bool {
    ProviderRef::parse(provider)
        .ok()
        .and_then(|p| p.cli_name().map(|n| n == "opencode"))
        .unwrap_or(provider == "opencode")
}

/// opencode's own stream filters a `task` subagent's usage out before it ever reaches
/// stdout, so a slot that fanned out reports only its own step deltas. Add the missing
/// child spend from opencode's sqlite ledger and rewrite the slot's stats sidecar. When
/// the ledger was found but could not be read, that is a real anomaly indistinguishable
/// from "nothing to recover" in `stats.json` alone, so it goes to the run's event log.
fn enrich_opencode_stats(
    stats: &mut process::StreamStats,
    provider: &str,
    log_path: &Path,
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
) {
    if !is_opencode_provider(provider) {
        return;
    }
    if let Some(note) = providers::opencode_telemetry::enrich(stats) {
        let _ = crate::events::append(
            paths,
            run_id,
            &crate::events::Event::slot_note(slot_id, &note),
        );
    }
    let _ = stats.save(log_path);
}

/// agy's `--output-format stream-json` now feeds tools/tokens straight into `stats` via
/// `StreamCoalescer::handle_agy`. What's left to recover from the statusline sink is what
/// the stream doesn't carry: the context-window snapshot and quota (see
/// `providers::agy_telemetry`). Also drives a real agy quota cooldown from the payload's
/// reset horizon (finding #3), and returns whether it did: agy's statusline is the *only*
/// place that shows up (its own stdout usage is per-step, not a rejection notice), so
/// callers OR this into a failed dispatch's `quota_hit` themselves.
fn enrich_agy_stats(
    stats: &mut process::StreamStats,
    provider: &str,
    cwd: &Path,
    log_path: &Path,
    paths: &SparPaths,
) -> bool {
    if !provider_is_agy(provider) {
        return false;
    }
    let Some(root) = providers::agy_telemetry::root() else {
        return false;
    };
    let Some(t) = providers::agy_telemetry::collect(&root, cwd) else {
        return false;
    };
    if t.context_tokens > 0 {
        stats.context_tokens = t.context_tokens;
        let _ = stats.save(log_path);
    }

    // Finding #3: when the account's binding gemini quota is (near) exhausted, cool the
    // provider down until its real reset instead of the fixed heuristic window.
    if let (Some(frac), Some(reset)) = (t.quota_remaining_fraction, t.quota_reset_secs) {
        if frac < 0.02 && reset > 0 {
            let until = chrono::Utc::now() + chrono::Duration::seconds(reset);
            let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
            store.pause_quota_until(
                "cli:agy",
                Some(until),
                t.quota_hint
                    .clone()
                    .unwrap_or_else(|| "agy quota exhausted".into()),
            );
            let _ = store.save(paths);
            return true;
        }
    }
    false
}

fn usage_from_stream(slot_id: &str, provider: &str, s: &process::StreamStats) -> SlotUsage {
    SlotUsage {
        slot_id: slot_id.into(),
        provider: provider.into(),
        input_tokens: s.input_tokens,
        output_tokens: s.output_tokens,
        cache_read_tokens: s.cache_read_tokens,
        context_tokens: s.context_tokens,
        billed_tokens: s.billed_tokens,
        tools: s.tools,
        model: s.model.clone(),
        cost_usd: s.cost_usd,
        subagent_stats: s.subagent_stats.clone(),
        model_usage: s.model_usage.clone(),
    }
}

fn apply_parallel_outcome(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    outcome: Result<SlotOutcome>,
    prep: &PreparedSlot,
) -> Result<()> {
    match outcome {
        Ok(result) if result.ok => {
            // Best-effort, and ordered before nothing: a marker write that fails must
            // not return early and leave this slot at `running` while its siblings are
            // recorded and the phase advances. That is the class of bug this file is
            // fixing, so it must not be reintroduced by its own error handling.
            let _ = markers::write_dispatch_verdict(
                paths,
                &state.id,
                slot_id,
                &markers::DispatchVerdict {
                    ok: true,
                    round: prep.round,
                    pid: result.pid,
                    exit_code: result.exit_code.or(Some(0)),
                    signal: result.signal,
                    reason: None,
                },
            );
            if let Some(s) = state.slot_mut(slot_id) {
                s.status = SlotStatus::Done;
                s.pid = result.pid;
                s.exit_code = result.exit_code.or(Some(0));
                s.signal = result.signal;
                if let Some(u) = &result.usage {
                    s.usage = Some(u.clone());
                }
            }
            if let Some(u) = result.usage {
                state.usage.push(u);
            }
            // Artifact presence is already enforced in `execute_prepared`; a slot that
            // reaches here with `ok` has its expected artifact.
            let _ = crate::events::append(
                paths,
                &state.id,
                &crate::events::Event::slot(slot_id, SlotStatus::Done),
            );
        }
        Ok(result) => {
            // True-parallel dispatch (`run_slots_parallel`, e.g. `--workflow review`)
            // had no quota detection at all before this: a rate-limited slot here left
            // its provider unpaused. This only pauses/records the hit on the slot; it
            // is `review`/`peer`/`roles`'s own terminal-phase mapping that reads
            // `quota_hit` back off the slot to route the run to `Phase::Quota`. Read
            // through `&result` here, before anything below moves its fields out: see
            // `quota_hit_for_outcome`.
            let quota_hit =
                quota_hit_for_outcome(paths, &prep.job.provider, &prep.log_path, &result);
            if let Some(s) = state.slot_mut(slot_id) {
                s.quota_hit = quota_hit;
            }
            let err = result.error.unwrap_or_else(|| "failed".into());
            salvage_expected_artifact(
                paths,
                &state.id,
                &prep.job,
                &prep.log_path,
                &err,
                &prep.cwd,
                prep.cfg.rounds.carry_forward_chars,
            );
            if let Some(u) = result.usage {
                if let Some(s) = state.slot_mut(slot_id) {
                    s.usage = Some(u.clone());
                }
                state.usage.push(u);
            }
            mark_slot_failed(
                state,
                paths,
                slot_id,
                &err,
                result.pid,
                result.exit_code,
                result.signal,
            )?;
        }
        Err(e) => {
            salvage_expected_artifact(
                paths,
                &state.id,
                &prep.job,
                &prep.log_path,
                &e.to_string(),
                &prep.cwd,
                prep.cfg.rounds.carry_forward_chars,
            );
            // Same api-sdk gap as `run_slot`'s early-err arm: a 429 propagates as this
            // `Err` without ever reaching `prep.log_path`, so the error text itself is
            // scraped alongside the log tail.
            let log_text = process::tail_log(&prep.log_path, 8000);
            // No outcome exists on this path, so no typed verdict: the stream never
            // produced a rate_limit_event. The text fallback is what catches an
            // api-sdk 429 here.
            let quota_hit = detect_and_pause_quota_with_err(
                paths,
                &prep.job.provider,
                &log_text,
                &e.to_string(),
            );
            if let Some(s) = state.slot_mut(slot_id) {
                s.quota_hit = quota_hit;
            }
            mark_slot_failed(state, paths, slot_id, &e.to_string(), None, None, None)?;
        }
    }
    let _ = crate::bus::heartbeat(paths, Some(&state.id), slot_id, "done");
    // Per join, not once after all of them: a batch that loses its orchestrator between
    // joins otherwise discards every verdict already collected.
    state.save(paths)?;
    Ok(())
}

/// The role's **soft** budget: where nudges start, not where the dispatch is killed
/// (O50). Also what `SlotActivity` reads as the point past which continued log silence is
/// a stall, which is unchanged by the soft/hard split — a slot that has said nothing for
/// its whole budget is hung whatever the ceiling says.
pub fn timeout_for_role(cfg: &Config, role: SlotRole) -> Duration {
    let secs = match role {
        SlotRole::Tester => cfg.suite.timeout_secs,
        SlotRole::TestAuthor => cfg.spec.timeout_secs,
        SlotRole::Reviewer => cfg.timeouts.review_secs(),
        _ => cfg.timeouts.slot_secs,
    };
    Duration::from_secs(secs)
}

/// The only wall clock that still kills, and the one handed to `run_captured`. A slot's
/// work is often the sole copy of the round's output, so the ceiling sits far above the
/// soft budget and the recurring nudges do the bounding.
pub fn hard_ceiling_for_role(cfg: &Config, role: SlotRole) -> Duration {
    let soft = timeout_for_role(cfg, role);
    Duration::from_secs((soft.as_secs() as f64 * cfg.timeouts.hard_multiple()).round() as u64)
}

/// On timeout/fail, keep any non-empty expected artifact; else salvage from the slot log.
/// Also synthesizes a missing implementer carry-forward brief (see
/// `salvage_carry_forward`): without it the next round opens cold on a half-edited
/// worktree with no record that the tree is dirty.
pub fn salvage_expected_artifact(
    paths: &SparPaths,
    run_id: &str,
    job: &SlotJob,
    log_path: &Path,
    reason: &str,
    cwd: &Path,
    carry_forward_chars: usize,
) {
    // First, unconditionally: a missing implementer brief is synthesized even when the
    // primary artifact already exists (the agent may have written its summary and died
    // before the brief), and `salvage_carry_forward` itself no-ops for other roles and
    // for briefs the agent already wrote.
    salvage_carry_forward(
        paths,
        run_id,
        job,
        cwd,
        log_path,
        reason,
        carry_forward_chars,
    );
    let Some(name) = &job.expected_artifact else {
        return;
    };
    let path = paths.artifact(run_id, name);
    if path.is_file() && std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0 {
        return;
    }
    // Never synthesize a suite report: an absent suite.md is meaningful (Inconclusive),
    // whereas a fabricated `## Result: fail` blocks the ship on a runner problem.
    if job.role == SlotRole::Tester {
        return;
    }
    // A codex dispatch was passed `--output-last-message` pointing at a slot-scoped
    // file under this run's `logs/` (see `CodexAdapter::build_headless`): when that
    // file is fresh for this dispatch it carries the provider's own final message,
    // which beats the reconstructed log tail. Anything else (another provider, a
    // missing file, a stale file from an earlier round) falls back to today's tail.
    // Either way the content lands inside the role-shaped transcript section below,
    // never as the artifact itself: a reviewer's final chat message is not a review.
    let tail = codex_last_message(log_path, &job.provider)
        .unwrap_or_else(|| process::tail_log(log_path, 6000));
    let body = match job.role {
        SlotRole::Reviewer => format!(
            "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — review slot interrupted ({reason}); partial transcript salvaged below\n\n## Tests\nsee partial transcript\n\n## Partial transcript\n\n```\n{tail}\n```\n"
        ),
        SlotRole::TestAuthor => format!(
            "## Scenarios\n- (interrupted: {reason})\n\n## Non-goals\n- n/a\n\n## How to run\n- unknown\n\n## Expected before implement\nskipped-reason\n\n## Notes\nPartial transcript:\n```\n{tail}\n```\n"
        ),
        _ => format!("# Salvaged artifact ({reason})\n\n```\n{tail}\n```\n"),
    };
    let _ = std::fs::write(path, body);
}

/// Best-effort `git -C <cwd> <args>`, trimmed. `None` when git is missing, `cwd` is
/// not a repo, or the output is empty — the brief keeps its tool tail either way.
fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn clamp_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Write the implementer carry-forward brief the dead slot never finished, from what
/// spar can already see: the slot worktree's `git status`/`diff --stat` (what changed)
/// plus a clamped `git diff` (how), and the tail of the coalesced tool stream (what it
/// was doing). Provider-generic — no adapter knowledge, just the worktree and the log.
///
/// Never overwrites a brief the agent wrote itself. Clamped to `carry_forward_chars`,
/// the same budget the next round's `carry_forward_section` applies, so a synthesized
/// brief cannot smuggle a bigger context climb than a written one. With no worktree
/// changes and no transcript there is nothing to say, and nothing is written.
fn salvage_carry_forward(
    paths: &SparPaths,
    run_id: &str,
    job: &SlotJob,
    cwd: &Path,
    log_path: &Path,
    reason: &str,
    carry_forward_chars: usize,
) {
    if job.role != SlotRole::Implementer || carry_forward_chars == 0 {
        return;
    }
    let path = paths.artifact(
        run_id,
        &crate::workflow::implement::carry_forward_name(&job.slot_id),
    );
    if artifact_written(&path) {
        return;
    }
    let mut sections = vec![format!(
        "# Synthesized carry-forward (slot {} died mid-dispatch: {reason})\n\nMachine-written \
         from the worktree and the partial transcript because the agent never finished \
         its turn. Hints, not verdicts: verify before trusting.\n",
        job.slot_id,
    )];
    if let Some(status) = git_output(cwd, &["status", "--porcelain"]) {
        sections.push(format!(
            "## Worktree status\n\n```\n{}\n```\n",
            clamp_chars(&status, 2000)
        ));
    }
    if let Some(stat) = git_output(cwd, &["diff", "--stat"]) {
        sections.push(format!(
            "## What changed\n\n```\n{}\n```\n",
            clamp_chars(&stat, 2000)
        ));
    }
    if git_output(cwd, &["rev-parse", "--is-inside-work-tree"]).is_some() {
        // Inside a repo (possibly with no diff yet): the clamped diff shows how far the
        // edits got, untracked files included via `--stat` above only when tracked, so
        // `status` carries the new files. `--no-color` keeps the brief plain text.
        if let Some(diff) = git_output(cwd, &["diff", "--no-color"]) {
            sections.push(format!(
                "## Diff (clamped)\n\n```diff\n{}\n```\n",
                clamp_chars(&diff, carry_forward_chars / 2)
            ));
        }
    }
    let tail_budget = carry_forward_chars / 2;
    let tail = process::tail_log(log_path, tail_budget.max(1000));
    if !tail.trim().is_empty() {
        sections.push(format!("## Partial tool transcript\n\n```\n{tail}\n```\n"));
    }
    if sections.len() == 1 {
        return;
    }
    let body = clamp_chars(&sections.join("\n"), carry_forward_chars);
    let _ = std::fs::write(path, body);
}

pub fn run_slot(
    state: &mut RunState,
    paths: &SparPaths,
    cfg: &Config,
    job: &SlotJob,
) -> Result<()> {
    let slot = state
        .slots
        .iter()
        .find(|s| s.id == job.slot_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown slot {}", job.slot_id))?;

    let cwd = slot
        .cwd
        .clone()
        .unwrap_or_else(|| state.project_root.clone());
    let backend = resolve_backend(state.backend, &job.provider);
    let log_path = paths.log_file(&state.id, &job.slot_id);
    let branch = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == job.slot_id)
        .map(|w| w.branch.clone())
        .unwrap_or_else(|| format!("spar/{}/{}", state.id, job.slot_id));

    let project_root_s = state.project_root.display().to_string();
    let cwd_s = cwd.display().to_string();
    let artifacts_s = paths.artifacts_dir(&state.id).display().to_string();
    let markers_s = paths.markers_dir(&state.id).display().to_string();
    let mailbox_s = paths.mailbox_dir(&state.id).display().to_string();
    let nudge_s = crate::nudge::poll_file(paths, &state.id, &job.slot_id)
        .display()
        .to_string();
    let mut vars = templates::base_vars(&templates::TemplateCtx {
        task: state.task.as_deref().unwrap_or(""),
        project_root: &project_root_s,
        cwd: &cwd_s,
        run_id: &state.id,
        artifacts_dir: &artifacts_s,
        markers_dir: &markers_s,
        mailbox_dir: &mailbox_s,
        slot_id: &job.slot_id,
        provider: &job.provider,
        branch: &branch,
        nudge_file: &nudge_s,
    });
    for (k, v) in &job.extra_vars {
        vars.insert(k.clone(), v.clone());
    }
    let prompt = templates::render(&job.template, &vars)?;

    // Write prompt file for providers that prefer files
    let prompt_path = paths
        .run_dir(&state.id)
        .join(format!("prompt-{}.md", job.slot_id));
    std::fs::write(&prompt_path, &prompt)
        .with_context(|| format!("write {}", prompt_path.display()))?;

    let pref = ProviderRef::parse(&job.provider)?;
    // See prepare_slot_execution: clear a prior attempt's markers before going Running.
    markers::clear_slot(paths, &state.id, &job.slot_id);
    let round = state.round;
    if let Some(s) = state.slot_mut(&job.slot_id) {
        s.status = SlotStatus::Running;
        // A dispatch's outcome fields describe *that* dispatch. Carried into the next
        // one they read as an impossible record (`running` with the prior round's
        // `exit_code: 0`) and, worse, survive onto the next terminal status: a `done`
        // slot still carrying `error: "exit 143"` from the attempt before it.
        s.exit_code = None;
        s.signal = None;
        s.pid = None;
        s.error = None;
        s.usage = None;
        s.quota_hit = false;
        // Stamp the round at dispatch: slot ids are stable across re-dispatch (the
        // implementer keeps its worktree through fix rounds), so this is where a slot
        // joins the round that is running now (O45).
        s.round = round;
        s.exec_backend = Some(pref.backend);
        s.backend = Some(if pref.is_api() {
            "api-sdk".into()
        } else {
            format!("{backend:?}").to_ascii_lowercase()
        });
        s.log_path = Some(log_path.clone());
        s.artifact = job.expected_artifact.clone();
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot(&job.slot_id, SlotStatus::Running),
    );
    let _ = crate::bus::heartbeat(paths, Some(&state.id), &job.slot_id, "running");
    state.save(paths)?;

    // The kill, not the budget: `timeouts.slot_secs` is soft since O50 and reaches the
    // dispatch as the nudge threshold instead.
    let timeout = hard_ceiling_for_role(cfg, job.role);

    if state.dry_run {
        return run_dry(state, paths, job, &cwd, &log_path, &prompt);
    }

    let presence_env = wire_slot_presence(state, paths, job, &cwd, &pref);

    // A backend `Err` (spawn/setup failure, not a completed dispatch) still writes a
    // log a rate-limit rejection could land in on some adapters, so it gets the same
    // quota scrape as the `!result.ok` branch below rather than silently skipping
    // detection because this failure surfaced a step earlier. On the api-sdk backend a
    // 429 propagates as this very `Err` and its text never reaches `log_path` at all
    // (`openai_compat::chat_completion`'s error is never appended to the log before the
    // `?` unwinds), so the error's own `to_string()` is scraped alongside the log tail
    // rather than the log alone.
    let quota_on_early_err =
        |state: &mut RunState, log_path: &Path, provider: &str, slot_id: &str, err_text: &str| {
            let log_text = process::tail_log(log_path, 8000);
            // No outcome exists on this path either: the backend never produced a
            // stream to have parsed a `rate_limit_event` out of, so there is no typed
            // reset instant to thread through.
            let quota_hit = detect_and_pause_quota_with_err(paths, provider, &log_text, err_text);
            if let Some(s) = state.slot_mut(slot_id) {
                s.quota_hit = quota_hit;
            }
        };
    let result = if pref.is_api() {
        match run_api(state, paths, job, &pref, &cwd, &log_path, &prompt, timeout) {
            Ok(r) => r,
            Err(e) => {
                salvage_expected_artifact(
                    paths,
                    &state.id,
                    job,
                    &log_path,
                    &e.to_string(),
                    &cwd,
                    cfg.rounds.carry_forward_chars,
                );
                quota_on_early_err(
                    state,
                    &log_path,
                    &job.provider,
                    &job.slot_id,
                    &e.to_string(),
                );
                mark_slot_failed(state, paths, &job.slot_id, &e.to_string(), None, None, None)?;
                return Err(e);
            }
        }
    } else {
        match backend {
            Backend::Tmux => {
                match run_tmux(
                    state,
                    paths,
                    job,
                    &cwd,
                    &log_path,
                    &prompt_path,
                    &prompt,
                    timeout,
                    &presence_env,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        salvage_expected_artifact(
                            paths,
                            &state.id,
                            job,
                            &log_path,
                            &e.to_string(),
                            &cwd,
                            cfg.rounds.carry_forward_chars,
                        );
                        quota_on_early_err(
                            state,
                            &log_path,
                            &job.provider,
                            &job.slot_id,
                            &e.to_string(),
                        );
                        mark_slot_failed(
                            state,
                            paths,
                            &job.slot_id,
                            &e.to_string(),
                            None,
                            None,
                            None,
                        )?;
                        return Err(e);
                    }
                }
            }
            Backend::Headless | Backend::Auto => {
                match run_headless(
                    state,
                    paths,
                    cfg,
                    job,
                    &cwd,
                    &log_path,
                    &prompt_path,
                    &prompt,
                    timeout,
                    &presence_env,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        salvage_expected_artifact(
                            paths,
                            &state.id,
                            job,
                            &log_path,
                            &e.to_string(),
                            &cwd,
                            cfg.rounds.carry_forward_chars,
                        );
                        quota_on_early_err(
                            state,
                            &log_path,
                            &job.provider,
                            &job.slot_id,
                            &e.to_string(),
                        );
                        mark_slot_failed(
                            state,
                            paths,
                            &job.slot_id,
                            &e.to_string(),
                            None,
                            None,
                            None,
                        )?;
                        return Err(e);
                    }
                }
            }
        }
    };

    if result.ok {
        markers::write_dispatch_verdict(
            paths,
            &state.id,
            &job.slot_id,
            &markers::DispatchVerdict {
                ok: true,
                round: state.round,
                pid: result.pid,
                exit_code: result.exit_code.or(Some(0)),
                signal: result.signal,
                reason: None,
            },
        )?;
        if let Some(s) = state.slot_mut(&job.slot_id) {
            s.status = SlotStatus::Done;
            s.pid = result.pid;
            s.exit_code = result.exit_code.or(Some(0));
            s.signal = result.signal;
            if let Some(u) = &result.usage {
                s.usage = Some(u.clone());
            }
        }
        if let Some(u) = result.usage {
            state.usage.push(u);
        }
        let _ = crate::events::append(
            paths,
            &state.id,
            &crate::events::Event::slot(&job.slot_id, SlotStatus::Done),
        );
    } else {
        // Read through `&result` before anything below moves its fields out — see
        // `quota_hit_for_outcome`.
        let quota_hit = quota_hit_for_outcome(paths, &job.provider, &log_path, &result);
        let err = result.error.as_deref().unwrap_or("failed");
        salvage_expected_artifact(
            paths,
            &state.id,
            job,
            &log_path,
            err,
            &cwd,
            cfg.rounds.carry_forward_chars,
        );
        markers::write_dispatch_verdict(
            paths,
            &state.id,
            &job.slot_id,
            &markers::DispatchVerdict {
                ok: false,
                round: state.round,
                pid: result.pid,
                exit_code: result.exit_code,
                signal: result.signal,
                reason: Some(err.to_string()),
            },
        )?;
        if let Some(s) = state.slot_mut(&job.slot_id) {
            s.status = SlotStatus::Failed;
            s.error = result.error.clone();
            s.pid = result.pid;
            s.exit_code = result.exit_code;
            s.signal = result.signal;
            if let Some(u) = &result.usage {
                s.usage = Some(u.clone());
            }
        }
        if let Some(u) = result.usage {
            state.usage.push(u);
        }
        let _ = crate::events::append(
            paths,
            &state.id,
            &crate::events::Event::slot(&job.slot_id, SlotStatus::Failed),
        );
        if let Some(s) = state.slot_mut(&job.slot_id) {
            s.quota_hit = quota_hit;
        }
    }
    let _ = crate::bus::heartbeat(
        paths,
        Some(&state.id),
        &job.slot_id,
        if result.ok { "done" } else { "failed" },
    );
    state.save(paths)?;
    if !result.ok {
        bail!(
            "slot {} failed: {}",
            job.slot_id,
            result.error.unwrap_or_else(|| "unknown".into())
        );
    }
    Ok(())
}

/// The longest real Claude window is `seven_day`; a stated instant further out than
/// this cannot be that window reopening (it is either a unit mismatch, e.g. epoch
/// milliseconds read as seconds, or otherwise malformed) and must not be trusted.
/// Provider-generic today because only the Claude adapter emits a typed reset instant
/// at all; an adapter with a longer real window would need this raised, not gated.
const MAX_PLAUSIBLE_RESET_HORIZON_DAYS: i64 = 8;

/// Why a stated reset instant cannot be trusted as a `Cooldown`. Kept as two variants,
/// not one boolean, because the two directions are opposite failures needing different
/// operator-facing words: one is a stale-but-sane value, the other means the payload
/// was never in the shape assumed in the first place.
enum ImplausibleReset {
    /// Not strictly in the future (clock skew, or a rejection whose reset elapsed
    /// before spar got to it). Writing it as a `Cooldown` would read as immediately
    /// available — worse than no cooldown at all, since it also overwrites the generic
    /// auto-recovering pause. Any strictly future instant is honored exactly as stated,
    /// however soon: if the window really does reopen in a few seconds, pausing for a
    /// few seconds and then being available again is correct, not a bug to guard against.
    AlreadyElapsed,
    /// Further out than any real window can be (unit mismatch, e.g. epoch milliseconds
    /// read as seconds, or otherwise malformed). Trusting it would brick the provider
    /// for decades with no auto-recovery.
    OutOfRange,
}

impl ImplausibleReset {
    fn describe(&self, until: chrono::DateTime<chrono::Utc>) -> String {
        match self {
            Self::AlreadyElapsed => format!("already elapsed ({})", until.to_rfc3339()),
            Self::OutOfRange => format!(
                "implausibly far out ({}) — a unit mismatch?",
                until.to_rfc3339()
            ),
        }
    }
}

/// A stated reset instant is only usable if it actually lies ahead of us and within any
/// real window's horizon. Deliberately refusing rather than trying to detect and convert
/// milliseconds: the CLI's own schema states epoch seconds, so a value outside plausible
/// bounds means the payload isn't in the shape assumed, and guessing at a fix-up would
/// hide that instead of falling back safely.
fn implausible_reset(until: chrono::DateTime<chrono::Utc>) -> Option<ImplausibleReset> {
    let now = chrono::Utc::now();
    if until <= now {
        Some(ImplausibleReset::AlreadyElapsed)
    } else if until >= now + chrono::Duration::days(MAX_PLAUSIBLE_RESET_HORIZON_DAYS) {
        Some(ImplausibleReset::OutOfRange)
    } else {
        None
    }
}

/// `StreamStats::quota_resets_at` is epoch **seconds**, per the adapter's own schema.
/// `chrono::DateTime::from_timestamp_secs` does not reject an out-of-range value on its
/// own — a millisecond timestamp misread as seconds still parses, just to a date
/// decades out — so this conversion alone must never be trusted as "plausible"; that
/// judgment is `implausible_reset`'s, applied by every caller of this function.
fn resets_at_from_epoch_secs(secs: Option<i64>) -> Option<chrono::DateTime<chrono::Utc>> {
    secs.and_then(chrono::DateTime::from_timestamp_secs)
}

/// Best-effort quota detection on a failed dispatch's log: pauses the provider in the
/// quota store (cheap, auto-recovering, driven by `scrape_log_hint`'s broad needles)
/// and reports whether this failure looks like a rate limit rather than a genuine
/// defect. The `bool` returned is the discriminator callers route `Phase::Quota` vs
/// `Phase::Failed` on, so it is deliberately narrower than the pause: it fires only on
/// `scrape_strong_quota_signal` — line-scoped, requires a limit phrase *and* a
/// rejection word together, for every adapter including Claude. It deliberately
/// excludes `scrape_claude_rate_limits`/`scrape_claude_stated_reset`: the `five_hour`
/// JSON branch fires on `used_percentage >= 95` alone with no failure in the log, and
/// the plain-text stated-reset scan matches "resets " plus a limit phrase *anywhere in
/// the whole tail log*, not on one line, with no rejection word required — an
/// implementer editing this very module's doc comments, or a reviewer quoting the
/// BACKLOG entry into its log, contains both and would otherwise misroute a genuine
/// defect onto the quota gate, which is worse than the bug this exists to fix. Both
/// still drive the pause below (harmless, auto-recovering) and still compute
/// `cooldown_until` for the store; they just must not alone decide that *this* failure
/// was a quota hit. The real incident line ("! rate limit  seven_day  rejected") is
/// itself line-scoped rejection text and matches `scrape_strong_quota_signal` on its
/// own, so this exclusion does not weaken detection of the case this was built for.
///
/// `structured` is the adapter's own typed verdict on this request, when it has one:
/// `StreamStats::quota_rejected`, carrying the `rateLimitType` from a `rate_limit_event`
/// whose status was `rejected`. When present it decides routing on its own and the text
/// heuristics are not consulted, because a typed rejection cannot be produced by source
/// code, documentation or a task brief that merely discusses limits — which is what every
/// false positive in this change's review history turned out to be. The prose scrape stays
/// as the fallback for adapters that emit no such event.
///
/// `quota_recovered` is `structured`'s companion: whether a `rejected` event earlier in
/// this dispatch was later cleared by an `allowed`/`allowed_warning` one. `structured:
/// None` alone is ambiguous — it means either "this adapter never spoke about quota"
/// (must still fall through to the prose scrape) or "it was rejected, and then
/// recovered" (must not route on prose alone) — and only the first should fall through
/// below. Routine `allowed` traffic with no prior rejection does *not* set this, so it
/// never disables the fallback for a dispatch that never actually saw a rejection.
/// Without this, a stream that was rejected, recovered, and then failed for an unrelated
/// reason would still route to `Phase::Quota`: the coalescer renders a line into the log
/// the moment the rejection arrives, and that line outlives the event's own recovery in
/// the log tail, so `scrape_strong_quota_signal` would match it regardless of the final
/// verdict.
///
/// When `quota_recovered` is true and there is no current `structured` rejection, the
/// prose scrapes below are skipped entirely, not just the final signal check: the same
/// stale rendered line that must not decide routing must also not leave the provider
/// paused in the store off a rejection this dispatch already recovered from.
fn detect_and_pause_quota(
    paths: &SparPaths,
    provider: &str,
    log_text: &str,
    structured: Option<&str>,
    resets_at: Option<chrono::DateTime<chrono::Utc>>,
    quota_recovered: bool,
) -> bool {
    let key = crate::quota::normalize_key(provider);
    if quota_recovered && structured.is_none() {
        return false;
    }
    if let Some(hint) = crate::quota::QuotaStore::scrape_log_hint(log_text) {
        let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        store.pause_quota(&key, hint);
        let _ = store.save(paths);
    }
    if let Some((name, until, hint)) = crate::quota::scrape_claude_rate_limits(provider, log_text) {
        let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
        store.pause_quota_until(&name, until, hint);
        let _ = store.save(paths);
    }
    if let Some(window) = structured {
        // The adapter stated when the window reopens, so pause until exactly then rather
        // than leaving the generic timer to re-probe. This is the answer the text path
        // cannot produce: an absolute instant, no wall-clock inference and no timezone.
        // Only when that instant is plausible, though (see `implausible_reset`):
        // otherwise leave whatever generic pause the scrapes above already wrote alone
        // rather than overwrite it with a `Cooldown` that is wrong in either direction.
        let usable = resets_at.and_then(|until| match implausible_reset(until) {
            None => Some(until),
            Some(reason) => {
                eprintln!(
                    "warning: {key} stated a reset instant that is {}; falling back to \
                     the generic pause",
                    reason.describe(until)
                );
                None
            }
        });
        match usable {
            Some(until) => {
                let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                store.pause_quota_until(
                    &key,
                    Some(until),
                    format!(
                        "{key} {window} limit rejected, resets {}",
                        until.to_rfc3339()
                    ),
                );
                let _ = store.save(paths);
            }
            None => {
                // The scrapes above only pause if `log_text` happens to carry rate-limit
                // prose. It usually does (the coalescer renders a line alongside every
                // structured event), but the tail is capped and can roll that line out,
                // and an adapter can emit the structured event with no prose at all — in
                // both cases a typed rejection with no usable instant must still leave
                // the provider paused, not `Available`. Write the generic pause
                // explicitly rather than assume an earlier scrape already did; only if
                // one hasn't, so a more precise cooldown a scrape *did* write is not
                // downgraded.
                let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
                if store.is_usable(&key) {
                    store.pause_quota(&key, format!("{key} {window} limit rejected"));
                    let _ = store.save(paths);
                }
            }
        }
        return true;
    }
    // `quota_recovered` was already handled above (before either scrape ran): reaching
    // here means it was false, or `structured` would have taken the branch above.
    crate::quota::QuotaStore::scrape_strong_quota_signal(log_text).is_some()
}

/// The glue between a finished `SlotOutcome` and [`detect_and_pause_quota`], shared by
/// `run_slot`'s and `apply_parallel_outcome`'s failed-dispatch arms. Takes `&SlotOutcome`
/// whole, rather than three fields pre-extracted into locals at each call site: a
/// mutation dropping `quota_resets_at` between the outcome and the call — the exact
/// class of regression review rounds kept finding here — now has to happen inside this
/// one tested function instead of silently at either caller.
fn quota_hit_for_outcome(
    paths: &SparPaths,
    provider: &str,
    log_path: &Path,
    outcome: &SlotOutcome,
) -> bool {
    let log_text = process::tail_log(log_path, 8000);
    detect_and_pause_quota(
        paths,
        provider,
        &log_text,
        outcome.quota_rejected.as_deref(),
        outcome.quota_resets_at,
        outcome.quota_recovered,
    ) || outcome.agy_quota_hit
}

/// Same discriminator as [`detect_and_pause_quota`], but also scrapes `extra`
/// (typically a propagated error's `to_string()`) alongside the log tail. On the
/// api-sdk backend a 429 propagates as a `Result::Err` from
/// `openai_compat::chat_completion` and its text never reaches `log_path` before the
/// `?` unwinds, so the log tail alone would miss it even though
/// `scrape_strong_quota_signal` already recognizes "429 Too Many Requests".
///
/// Takes no `structured`/`resets_at`/`quota_recovered`: both call sites are `Err` arms that
/// by construction never produced a `SlotOutcome`, so a typed verdict cannot exist yet
/// to thread through. That used to be a parameter every caller passed `None` to — dead
/// weight a future caller could silently fail to fill in; dropping it from the
/// signature makes the absence structural instead of a convention callers must uphold.
fn detect_and_pause_quota_with_err(
    paths: &SparPaths,
    provider: &str,
    log_text: &str,
    extra: &str,
) -> bool {
    detect_and_pause_quota(
        paths,
        provider,
        &format!("{log_text}\n{extra}"),
        None,
        None,
        false,
    )
}

/// `state.slots` lookup used by every caller that maps a `run_slot` failure onto
/// `Phase::Quota` vs `Phase::Failed` (see `detect_and_pause_quota`'s doc comment for
/// the discriminator itself).
pub fn slot_quota_hit(state: &RunState, slot_id: &str) -> bool {
    state
        .slots
        .iter()
        .find(|s| s.id == slot_id)
        .is_some_and(|s| s.quota_hit)
}

struct SlotOutcome {
    ok: bool,
    pid: Option<u32>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    error: Option<String>,
    usage: Option<SlotUsage>,
    /// Set when `enrich_agy_stats` detected exhausted agy quota telemetry during *this*
    /// dispatch. The stream's `result.error` can carry rejection prose the log-based
    /// scrape (`detect_and_pause_quota`) recognizes, but the structured gemini-* quota
    /// fraction that actually decides exhaustion is only in the statusline sink;
    /// callers OR this in as a second, more reliable signal.
    agy_quota_hit: bool,
    /// The adapter's own typed rate-limit rejection (its `rateLimitType`), reflecting
    /// the *last* `rate_limit_event` in the stream: a later `allowed`/`allowed_warning`
    /// event clears it, since a stream can be rejected, fall back to another window and
    /// keep going. Routing prefers this over any text heuristic: a typed verdict cannot
    /// be produced by source code, docs or a task brief that merely discusses limits.
    quota_rejected: Option<String>,
    /// When that window reopens, as the adapter stated it.
    quota_resets_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Whether a `rejected` event earlier in this dispatch was later cleared by an
    /// `allowed`/`allowed_warning` one. `quota_rejected: None` is ambiguous on its own:
    /// it means either "this adapter never spoke about quota" (must still fall through
    /// to the prose scrape) or "it was rejected, and then recovered" (must not route on
    /// prose alone, or a rendered rejection line the earlier event left in the log tail
    /// would misroute an unrelated later failure). Routine `allowed` traffic with no
    /// prior rejection leaves this false.
    quota_recovered: bool,
}

impl SlotOutcome {
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            pid: None,
            exit_code: None,
            signal: None,
            error: Some(msg.into()),
            usage: None,
            agy_quota_hit: false,
            quota_rejected: None,
            quota_resets_at: None,
            quota_recovered: false,
        }
    }
}

/// Config-key label for the timeout that governs a role, so a killed slot names its budget.
/// Everything the slot owes this dispatch. The implementer also owes its carry-forward
/// brief, and a nudge that names only the summary is how the round loop loses it.
fn owed_artifacts(role: SlotRole, slot_id: &str, expected: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = expected.map(str::to_string).into_iter().collect();
    if role == SlotRole::Implementer {
        v.push(crate::workflow::implement::carry_forward_name(slot_id));
    }
    v
}

fn timeout_label(role: SlotRole) -> &'static str {
    match role {
        SlotRole::Tester => "suite.timeout_secs",
        SlotRole::TestAuthor => "spec.timeout_secs",
        SlotRole::Reviewer => "timeouts.review_secs",
        _ => "timeouts.slot_secs",
    }
}

fn signal_name(sig: i32) -> &'static str {
    match sig {
        2 => "SIGINT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        15 => "SIGTERM",
        _ => "signal",
    }
}

/// Actionable one-liner for a non-zero / signal exit.
fn describe_exit(code: Option<i32>, signal: Option<i32>) -> String {
    if let Some(sig) = signal {
        return format!("killed by signal {sig} ({})", signal_name(sig));
    }
    match code {
        Some(137) => "exit 137 (OOM-killed)".into(),
        Some(c) => format!("exit {c}"),
        None => "exited without a status".into(),
    }
}

/// Error text for a non-zero, non-timeout dispatch. A usage error names spar's own
/// command line (spar's fault, never the agent's); a vendor step-budget stop is
/// reported as a budget stop, not a crash. Everything else keeps the generic line.
fn dispatch_error(
    adapter: &dyn providers::ProviderAdapter,
    log_path: &Path,
    cmdline: &str,
    code: Option<i32>,
    signal: Option<i32>,
) -> String {
    if adapter.is_usage_error(code) {
        // The command line says what spar did wrong; the provider's own stderr tail
        // says why it rejected it — the operator's only clue, so carry it along.
        return format!(
            "provider usage error ({}): spar built a command line the provider rejected, not an agent failure: {cmdline}\nprovider output (tail):\n{}",
            describe_exit(code, signal),
            process::tail_log(log_path, 2000),
        );
    }
    if adapter.step_budget_exhausted(&std::fs::read_to_string(log_path).unwrap_or_default()) {
        return format!(
            "provider stopped at its model-step budget ({}), not a crash\nprovider output (tail):\n{}",
            describe_exit(code, signal),
            process::tail_log(log_path, 2000),
        );
    }
    describe_exit(code, signal)
}

fn mark_slot_failed(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    err: &str,
    pid: Option<u32>,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> Result<()> {
    let _ = markers::write_dispatch_verdict(
        paths,
        &state.id,
        slot_id,
        &markers::DispatchVerdict {
            ok: false,
            round: state.round,
            pid,
            exit_code,
            signal,
            reason: Some(err.to_string()),
        },
    );
    if let Some(s) = state.slot_mut(slot_id) {
        s.status = SlotStatus::Failed;
        s.error = Some(err.into());
        s.pid = pid;
        s.exit_code = exit_code;
        s.signal = signal;
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot(slot_id, SlotStatus::Failed),
    );
    state.save(paths)?;
    Ok(())
}

fn run_dry(
    state: &mut RunState,
    paths: &SparPaths,
    job: &SlotJob,
    cwd: &Path,
    log_path: &Path,
    prompt: &str,
) -> Result<()> {
    let mock_note = format!(
        "dry-run slot={} role={:?} provider={}\n",
        job.slot_id, job.role, job.provider
    );
    let req = SpawnRequest {
        program: PathBuf::from("dry-run"),
        args: vec![],
        cwd: cwd.to_path_buf(),
        log_path: log_path.to_path_buf(),
        env: vec![],
        timeout: Duration::from_secs(1),
    };
    process::run_mock(&req, &mock_note)?;

    // Write role-appropriate artifacts
    write_dry_artifacts(state, paths, job, cwd, prompt)?;

    markers::write_done(paths, &state.id, &job.slot_id)?;
    if let Some(s) = state.slot_mut(&job.slot_id) {
        s.status = SlotStatus::Done;
        s.exit_code = Some(0);
        s.backend = Some("dry-run".into());
    }
    let _ = crate::events::append(
        paths,
        &state.id,
        &crate::events::Event::slot(&job.slot_id, SlotStatus::Done),
    );
    state.save(paths)?;
    Ok(())
}

fn write_dry_artifacts(
    state: &RunState,
    paths: &SparPaths,
    job: &SlotJob,
    cwd: &Path,
    _prompt: &str,
) -> Result<()> {
    let task = state.task.as_deref().unwrap_or("(no task)");
    match job.role {
        SlotRole::Planner | SlotRole::PlanCritic => {
            let plan = format!(
                "# Plan (dry-run)\n\n## Goal\n{task}\n\n## Steps\n1. Inspect codebase\n2. Implement change\n3. Test\n4. Summarize\n\n## Files likely touched\n- (determined at implement time)\n\n## Risks\n- dry-run placeholder\n\n_Generated by dry-run planner slot `{}` ({})._\n",
                job.slot_id, job.provider
            );
            std::fs::write(
                paths.artifact(&state.id, &format!("plan-{}.md", job.slot_id)),
                &plan,
            )?;
            // shared plan — last writer wins; good enough for dry-run
            std::fs::write(paths.artifact(&state.id, "plan.md"), &plan)?;
            if job.role == SlotRole::PlanCritic {
                std::fs::write(
                    paths.artifact(&state.id, &format!("plan-critique-{}.md", job.slot_id)),
                    format!("# Critique\n\nPlan is acceptable for dry-run of: {task}\n"),
                )?;
            }
        }
        SlotRole::Implementer => {
            // Test hook: an implementer that edits the contract it is judged against.
            // The slot really can do this — `artifacts_dir` is in its prompt — and it is
            // what the O43 freeze and the O52 re-freeze guard exist to bound.
            if crate::util::env_truthy("SPAR_FORCE_CONTRACT_TAMPER") {
                let contract = paths.artifact(&state.id, "test-contract.md");
                if let Ok(body) = std::fs::read_to_string(&contract) {
                    let kept: String = body
                        .lines()
                        .filter(|l| !l.contains("AC-2:"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    std::fs::write(&contract, format!("{kept}\n"))?;
                }
            }
            let stamp = cwd.join(".spar-dry-implement");
            std::fs::write(
                &stamp,
                format!("implemented (dry-run) by {} for: {task}\n", job.slot_id),
            )?;
            std::fs::write(
                paths.artifact(&state.id, &format!("summary-{}.md", job.slot_id)),
                format!(
                    "# Summary ({})\n\nDry-run implementation for:\n\n{task}\n\nWrote `{}`.\n",
                    job.slot_id,
                    stamp.display()
                ),
            )?;
        }
        SlotRole::TestAuthor => {
            let stamp = cwd.join(".spar-dry-acceptance-tests");
            std::fs::write(
                &stamp,
                format!(
                    "acceptance tests (dry-run) by {} for: {task}\n",
                    job.slot_id
                ),
            )?;
            std::fs::write(
                paths.artifact(&state.id, "test-contract.md"),
                format!(
                    "## Scenarios\n- [ ] AC-1: dry-run acceptance for: {task} — verify: `dry-run` (stub)\n- [ ] AC-2: dry-run artifacts are written — verify: `dry-run` (stub)\n\n## Non-goals\n- live test generation\n\n## How to run\n- `dry-run` (stub)\n\n## Expected before implement\nred\n\n## Notes\nDry-run test-author slot `{}` ({}); wrote `{}`.\n",
                    job.slot_id,
                    job.provider,
                    stamp.display()
                ),
            )?;
            let _ = crate::bus::chat(
                paths,
                Some(&state.id),
                &job.slot_id,
                "broadcast",
                "dry-run acceptance contract proposed",
                state.message_budget,
            );
        }
        SlotRole::Tester => {
            std::fs::write(
                paths.artifact(&state.id, "suite.md"),
                format!(
                    "## Result\npass\n\n## Commands\n- `dry-run suite` → exit 0\n\n## Summary\nDry-run suite channel ({}) for: {task}\n\n## Failures\nnone\n",
                    job.provider
                ),
            )?;
        }
        SlotRole::Reviewer => {
            let force_rc = crate::util::env_truthy("SPAR_FORCE_REQUEST_CHANGES")
                || job.slot_id.contains("harsh")
                || job.extra_vars.contains_key("request_changes");
            let verdict = if force_rc {
                "request_changes"
            } else {
                "approve"
            };
            // The acceptance gate is fail closed, so the synthetic review must be
            // schema-valid: every contract AC-n reported, or the dry-run backend would
            // wedge every run in a fix loop.
            let contract = std::fs::read_to_string(paths.artifact(&state.id, "test-contract.md"))
                .unwrap_or_default();
            let criteria = crate::workflow::review_result::parse_contract_criteria(&contract);
            // `omit` drops the last criterion, `unverified` reports it as unverified —
            // the two ways a well-meaning reviewer trips the acceptance gate.
            let force_ac = std::env::var("SPAR_FORCE_AC_STATUS").unwrap_or_default();
            let acceptance = if criteria.is_empty() {
                String::new()
            } else {
                let last = criteria.len() - 1;
                let lines: Vec<String> = criteria
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !(force_ac == "omit" && *i == last))
                    .map(|(i, id)| {
                        if force_rc && i == 0 {
                            format!("{id}: fail — dry-run forced request_changes")
                        } else if force_ac == "unverified" && i == last {
                            format!("{id}: unverified — dry-run forced unverified")
                        } else {
                            format!("{id}: pass — dry-run synthetic evidence")
                        }
                    })
                    .collect();
                if lines.is_empty() {
                    String::new()
                } else {
                    format!("## Acceptance\n{}\n\n", lines.join("\n"))
                }
            };
            let body = format!(
                "## Verdict\n{verdict}\n\n{acceptance}## Findings\n- severity: minor — dry-run synthetic review from {}\n\n## Tests\nsuite channel (dry-run); no full suite here\n",
                job.provider
            );
            if let Some(name) = &job.expected_artifact {
                std::fs::write(paths.artifact(&state.id, name), &body)?;
            }
            std::fs::write(
                paths.artifact(&state.id, &format!("review-{}.md", job.slot_id)),
                &body,
            )?;
        }
        SlotRole::Ranker => {
            let candidates: Vec<String> = state
                .slots
                .iter()
                .filter(|s| s.role == SlotRole::Implementer)
                .map(|s| s.id.clone())
                .collect();
            let winner = candidates
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            let ranking = format!(
                "# Ranking\n\nWinner: `{winner}`\n\nOrder:\n{}\n\nRationale: dry-run default order.\n",
                candidates
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{}. `{c}`", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::fs::write(paths.artifact(&state.id, "ranking.md"), ranking)?;
            let winner_json = serde_json::json!({
                "winner_slot": winner,
                "rank": candidates,
            });
            std::fs::write(
                paths.artifact(&state.id, "winner.json"),
                serde_json::to_string_pretty(&winner_json)?,
            )?;
        }
        SlotRole::Peer => {
            std::fs::write(
                paths.artifact(&state.id, &format!("summary-{}.md", job.slot_id)),
                format!(
                    "# Peer summary ({})\n\nDry-run peer work for: {task}\n",
                    job.slot_id
                ),
            )?;
            let _ = crate::bus::chat(
                paths,
                Some(&state.id),
                &job.slot_id,
                "broadcast",
                "dry-run peer ready",
                state.message_budget,
            );
        }
        SlotRole::Reconciler => {
            std::fs::write(
                paths.artifact(&state.id, "summary-reconcile.md"),
                format!("# Reconcile (dry-run)\n\nMerged best parts for: {task}\n"),
            )?;
            std::fs::write(
                paths.artifact(&state.id, &format!("summary-{}.md", job.slot_id)),
                format!("# Reconcile ({})\n\n{task}\n", job.slot_id),
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_api(
    state: &RunState,
    paths: &SparPaths,
    job: &SlotJob,
    pref: &ProviderRef,
    cwd: &Path,
    log_path: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<SlotOutcome> {
    let expected = job
        .expected_artifact
        .as_ref()
        .map(|n| paths.artifact(&state.id, n));
    let model = slot_model_for(Some(state), job);
    reset_api_slot_log(log_path);
    let (ok, err, usage) = api::run_api_slot(&api::runtime::ApiSlotRequest {
        provider_name: &pref.name,
        prompt,
        cwd,
        log_path,
        expected_artifact: expected.as_deref(),
        timeout,
        dry_run: false,
        model_override: model.clone(),
    })?;
    let slot_usage = SlotUsage {
        slot_id: job.slot_id.clone(),
        provider: pref.storage_key(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: 0,
        context_tokens: usage.peak_input_tokens,
        billed_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
        tools: 0,
        model: usage.model.or(model),
        cost_usd: None,
        subagent_stats: None,
        model_usage: Default::default(),
    };
    if ok {
        Ok(SlotOutcome {
            ok: true,
            pid: None,
            exit_code: Some(0),
            signal: None,
            error: None,
            usage: Some(slot_usage),
            agy_quota_hit: false,
            quota_rejected: None,
            quota_resets_at: None,
            quota_recovered: false,
        })
    } else {
        Ok(SlotOutcome {
            ok: false,
            pid: None,
            exit_code: Some(1),
            signal: None,
            error: err,
            usage: Some(slot_usage),
            agy_quota_hit: false,
            quota_rejected: None,
            quota_resets_at: None,
            quota_recovered: false,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_headless(
    state: &RunState,
    paths: &SparPaths,
    cfg: &Config,
    job: &SlotJob,
    cwd: &Path,
    log_path: &Path,
    prompt_path: &Path,
    prompt: &str,
    timeout: Duration,
    env: &[(String, String)],
) -> Result<SlotOutcome> {
    let pref = ProviderRef::parse(&job.provider)?;
    let cli_name = pref.cli_name().unwrap_or(job.provider.as_str());
    let adapter = providers::adapter_named(cli_name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {}", job.provider))?;
    let bin = adapter
        .resolve_binary()
        .ok_or_else(|| anyhow::anyhow!("provider {} not on PATH", job.provider))?;
    if provider_is_agy(&job.provider) {
        if let Some(root) = providers::agy_telemetry::root() {
            let _ = providers::agy_telemetry::ensure_statusline_hook(&root);
        }
    }

    let opts = SpawnOpts {
        prompt: prompt.to_string(),
        prompt_file: Some(prompt_path.to_path_buf()),
        cwd: cwd.to_path_buf(),
        trust: TrustPolicy::FullAuto,
        extra_args: vec![],
        model: slot_model_for(Some(state), job),
        timeout_secs: Some(timeout.as_secs()),
    };
    let prior_session_id =
        prior_session_for_role(paths, &state.id, &job.slot_id, cli_name, job.role);
    let (cmd, used_resume) =
        build_dispatch_command(adapter.as_ref(), &bin, &opts, prior_session_id.as_deref());
    let (program, args) = providers::command_to_parts(&cmd);
    let (program, args) = sandbox::maybe_wrap(state.isolation, cwd, &program, &args);
    let cmdline = format!("{} {}", program.display(), args.join(" "));
    // See `execute_prepared`: adapter-derived env rides the request, not the `Command`.
    let mut dispatch_env = env.to_vec();
    dispatch_env.extend(adapter.extra_env(&opts));

    let req = SpawnRequest {
        program,
        args,
        cwd: cwd.to_path_buf(),
        log_path: log_path.to_path_buf(),
        env: dispatch_env,
        timeout,
    };
    let pid_cell = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let sink_cell = pid_cell.clone();
    let run_id = state.id.clone();
    let slot_id = job.slot_id.clone();
    let sink = move |pid: u32| {
        sink_cell.store(pid, std::sync::atomic::Ordering::SeqCst);
        let _ = markers::write_pid(paths, &run_id, &slot_id, process::PidToken::capture(pid));
    };
    let beat = LivenessBeat {
        paths,
        run_id: &state.id,
        slot_id: &job.slot_id,
        last: std::cell::Cell::new(std::time::Instant::now()),
    };
    let soft = timeout_for_role(cfg, job.role);
    let watch = crate::nudge::NudgeWatch::new(
        crate::nudge::WatchSpec {
            paths,
            run_id: &state.id,
            slot_id: &job.slot_id,
            provider: &job.provider,
            role: job.role,
            log_path,
            artifacts: owed_artifacts(job.role, &job.slot_id, job.expected_artifact.as_deref()),
            soft,
            ceiling: timeout,
            label: timeout_label(job.role),
            dry_run: state.dry_run,
        },
        cfg,
    );
    let tick = || {
        beat.tick();
        watch.tick();
    };
    let resume_attempt = if used_resume {
        prior_session_id.as_deref()
    } else {
        None
    };
    // See `execute_prepared`: the artifact gate below only counts a write from
    // this dispatch.
    let dispatch_start = SystemTime::now();
    let mut res = dispatch_with_resume_recovery(
        adapter.as_ref(),
        &bin,
        &opts,
        resume_attempt,
        state.isolation,
        req,
        paths,
        &state.id,
        &job.slot_id,
        cli_name,
        &sink,
        &tick,
        &|d| sleep_ticking(d, &tick),
    )?;
    let pid = load_pid(&pid_cell);
    // See `execute_prepared`: the verdict lands on disk before the gates and before any
    // state save, so an orchestrator that dies from here on still leaves a terminal
    // record behind (O49).
    let mut verdict = markers::DispatchVerdict {
        ok: !res.timed_out && res.exit_code == Some(0),
        round: state.round,
        pid,
        exit_code: res.exit_code,
        signal: res.signal,
        reason: None,
    };
    let _ = markers::write_dispatch_verdict(paths, &state.id, &job.slot_id, &verdict);
    let agy_quota_hit = enrich_agy_stats(&mut res.stats, &job.provider, cwd, log_path, paths);
    enrich_muse_stats(&mut res.stats, &job.provider, log_path);
    enrich_opencode_stats(
        &mut res.stats,
        &job.provider,
        log_path,
        paths,
        &state.id,
        &job.slot_id,
    );
    let quota_rejected = res.stats.quota_rejected.clone();
    let quota_resets_at = resets_at_from_epoch_secs(res.stats.quota_resets_at);
    let quota_recovered = res.stats.quota_recovered;
    let usage = usage_from_stream(&job.slot_id, &job.provider, &res.stats);
    if res.timed_out {
        let error = crate::nudge::ceiling_error(timeout, soft, timeout_label(job.role));
        let _ = crate::events::append(
            paths,
            &state.id,
            &crate::events::Event::slot_note(&job.slot_id, &error),
        );
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: res.exit_code,
            signal: res.signal,
            error: Some(error),
            usage: Some(usage),
            agy_quota_hit,
            quota_rejected: quota_rejected.clone(),
            quota_resets_at,
            quota_recovered,
        });
    }
    let code = res.exit_code;
    if code != Some(0) {
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: code,
            signal: res.signal,
            error: Some(dispatch_error(
                adapter.as_ref(),
                log_path,
                &cmdline,
                code,
                res.signal,
            )),
            usage: Some(usage),
            agy_quota_hit,
            quota_rejected: quota_rejected.clone(),
            quota_resets_at,
            quota_recovered,
        });
    }
    if let Some(name) = &job.expected_artifact {
        let path = paths.artifact(&state.id, name);
        // Only a write from this dispatch counts; the previous round's file is
        // still on disk. See `execute_prepared`.
        let fresh = artifact_fresh_since(&path, dispatch_start)
            || markers::wait_for_artifact(
                paths,
                &state.id,
                name,
                dispatch_start,
                Duration::from_secs(2),
            )
            .unwrap_or(false);
        if !fresh {
            // See `execute_prepared`: name the precise cause first. `run_headless`
            // is only reached for native prefs (the caller routes api prefs to
            // `run_api`), so `is_api` re-derives false here; it is passed rather
            // than assumed so the exemption stays explicit.
            let is_api = ProviderRef::parse(&job.provider)
                .map(|p| p.is_api())
                .unwrap_or(false);
            if let Some(error) = judging_no_tool_error(job.role, &job.slot_id, usage.tools, is_api)
            {
                note_no_judgment(paths, &state.id, &job.slot_id, &mut verdict, &error);
                return Ok(SlotOutcome {
                    ok: false,
                    pid,
                    exit_code: Some(0),
                    signal: None,
                    error: Some(error),
                    usage: Some(usage),
                    agy_quota_hit,
                    quota_rejected: quota_rejected.clone(),
                    quota_resets_at,
                    quota_recovered,
                });
            }
            let recovered = recover_artifact(&ArtifactRecovery {
                paths,
                run_id: &state.id,
                slot_id: &job.slot_id,
                role: job.role,
                owns_cwd: owns_cwd(state, &job.slot_id, cwd),
                provider: &job.provider,
                model: slot_model_for(Some(state), job),
                cwd,
                log_path,
                prompt_path: &recovery_prompt_path(prompt_path, &job.slot_id),
                env,
                isolation: state.isolation,
                base_commit: state.base_commit.as_deref(),
                artifact: &path,
            });
            if !recovered {
                let error = format!("missing expected artifact {name}");
                verdict.ok = false;
                verdict.reason = Some(error.clone());
                let _ = markers::write_dispatch_verdict(paths, &state.id, &job.slot_id, &verdict);
                return Ok(SlotOutcome {
                    ok: false,
                    pid,
                    exit_code: Some(0),
                    signal: None,
                    error: Some(error),
                    usage: Some(usage),
                    agy_quota_hit,
                    quota_rejected: quota_rejected.clone(),
                    quota_resets_at,
                    quota_recovered,
                });
            }
        }
    }
    Ok(SlotOutcome {
        ok: true,
        pid,
        exit_code: Some(0),
        signal: None,
        error: None,
        usage: Some(usage),
        agy_quota_hit,
        quota_rejected: quota_rejected.clone(),
        quota_resets_at,
        quota_recovered,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerState {
    None,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxDecision {
    Wait,
    Ok,
    DoneButAlive,
    Failed,
}

/// Truncate `log_path` and drop any stale `.idx` sidecar before an api-sdk slot's own
/// `append_log` (`src/api/runtime.rs`) starts writing to it. That writer never touches the
/// index — it has no `LogWriter`/`stream_to_log` of its own — so a sidecar left over from
/// an earlier native dispatch of this same slot id, or a re-dispatch reusing this slot's log
/// path, would otherwise get bisected against api-sdk's freshly-appended text and stamp its
/// records with another dispatch's timestamps (AC-8).
fn reset_api_slot_log(log_path: &Path) {
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::File::create(log_path);
    let _ = std::fs::remove_file(process::log_index_path(log_path));
}

/// A `done` marker only means success once the agent's pane process has exited.
fn tmux_outcome(marker: MarkerState, pane_alive: bool, budget_left: bool) -> TmuxDecision {
    match marker {
        MarkerState::Failed => TmuxDecision::Failed,
        MarkerState::Done if !pane_alive => TmuxDecision::Ok,
        MarkerState::Done if budget_left => TmuxDecision::Wait,
        MarkerState::Done => TmuxDecision::DoneButAlive,
        MarkerState::None => TmuxDecision::Wait,
    }
}

/// tmux's pane runs `build_interactive`, which for opencode is the same
/// `run --format json` stream headless mode parses live (`opencode.rs`'s
/// `build_interactive` falls back to `build_headless` for exactly this reason) — but
/// `run_tmux` only tees it to `log_path`, never through a live coalescer, so without this
/// every tmux-backed opencode slot reported no spend at all, parent or child. Reconstruct
/// the parent's own stats from the completed log, then run the same descendant recovery
/// `enrich_opencode_stats` does for the headless backends. A no-op (`None`) for every
/// other provider: their tmux panes render real terminal/TUI output, not a JSON stream,
/// so there is nothing here to recover.
fn tmux_recovered_usage(
    is_opencode: bool,
    paths: &SparPaths,
    run_id: &str,
    job: &SlotJob,
    log_path: &Path,
) -> Option<SlotUsage> {
    if !is_opencode {
        return None;
    }
    let mut stats = process::stats_from_log(log_path);
    enrich_opencode_stats(
        &mut stats,
        &job.provider,
        log_path,
        paths,
        run_id,
        &job.slot_id,
    );
    Some(usage_from_stream(&job.slot_id, &job.provider, &stats))
}

#[allow(clippy::too_many_arguments)]
fn run_tmux(
    state: &mut RunState,
    paths: &SparPaths,
    job: &SlotJob,
    cwd: &Path,
    log_path: &Path,
    prompt_path: &Path,
    prompt: &str,
    timeout: Duration,
    env: &[(String, String)],
) -> Result<SlotOutcome> {
    if !tmux::available() {
        bail!("tmux not available");
    }
    let session = state
        .tmux_session
        .clone()
        .unwrap_or_else(|| tmux::session_name(&state.id));
    if state.tmux_session.is_none() {
        tmux::new_session(&session, &state.project_root)?;
        state.tmux_session = Some(session.clone());
        state.save(paths)?;
    }

    let pref = ProviderRef::parse(&job.provider)?;
    let cli_name = pref.cli_name().unwrap_or(job.provider.as_str());
    let adapter = providers::adapter_named(cli_name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider {}", job.provider))?;
    let bin = adapter
        .resolve_binary()
        .ok_or_else(|| anyhow::anyhow!("provider {} not on PATH", job.provider))?;
    let opts = SpawnOpts {
        prompt: prompt.to_string(),
        prompt_file: Some(prompt_path.to_path_buf()),
        cwd: cwd.to_path_buf(),
        trust: TrustPolicy::FullAuto,
        extra_args: vec![],
        model: slot_model_for(Some(state), job),
        timeout_secs: None,
    };
    // prefer interactive for tmux
    let cmd = adapter.build_interactive(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let shell = tmux::shell_wrap(&program, &args, log_path);

    tmux::spawn_window(&session, &job.slot_id, cwd, &shell, env)?;
    let is_opencode = is_opencode_provider(&job.provider);

    // The pane's shell pid exists as soon as `new-window` returns — tmux creates the pane
    // synchronously as part of that command. Recording it now, not only once the `done`
    // marker shows up (the old behavior), is what lets liveness checks ever see this
    // slot as alive *while it is running* instead of only in the instant between it
    // finishing and this function returning.
    let mut pane_pid = tmux::pane_pid(&session, &job.slot_id).map(process::PidToken::capture);
    if let Some(token) = pane_pid {
        let _ = markers::write_pid(paths, &state.id, &job.slot_id, token);
    }

    // `done` means the agent's own process has exited — not just that it wrote its marker.
    let done = format!("{}.done", job.slot_id);
    let failed = format!("{}.failed", job.slot_id);
    let start = std::time::Instant::now();
    loop {
        let marker = if markers::marker_exists(paths, &state.id, &failed) {
            MarkerState::Failed
        } else if markers::marker_exists(paths, &state.id, &done) {
            MarkerState::Done
        } else {
            MarkerState::None
        };
        if marker == MarkerState::Done && pane_pid.is_none() {
            if let Some(p) = tmux::pane_pid(&session, &job.slot_id) {
                let token = process::PidToken::capture(p);
                pane_pid = Some(token);
                let _ = markers::write_pid(paths, &state.id, &job.slot_id, token);
            }
        }
        // `.alive()` checks the recorded start time, not just bare liveness — a plain
        // `pid_alive` here would let a pid the OS recycled onto an unrelated process
        // after the pane's shell exited count as "still running," spinning the slot to
        // its full budget instead of reporting `DoneButAlive`.
        let pane_alive = match pane_pid {
            Some(token) => token.alive(),
            None => tmux::pane_pid(&session, &job.slot_id).is_some(),
        };
        let budget_left = start.elapsed() < timeout;
        match tmux_outcome(marker, pane_alive, budget_left) {
            TmuxDecision::Ok => {
                return Ok(SlotOutcome {
                    ok: true,
                    pid: pane_pid.map(|t| t.pid),
                    exit_code: Some(0),
                    signal: None,
                    error: None,
                    usage: tmux_recovered_usage(is_opencode, paths, &state.id, job, log_path),
                    agy_quota_hit: false,
                    quota_rejected: None,
                    quota_resets_at: None,
                    quota_recovered: false,
                })
            }
            TmuxDecision::Failed => {
                return Ok(SlotOutcome {
                    ok: false,
                    pid: pane_pid.map(|t| t.pid),
                    exit_code: Some(1),
                    signal: None,
                    error: Some("marker failed".into()),
                    usage: tmux_recovered_usage(is_opencode, paths, &state.id, job, log_path),
                    agy_quota_hit: false,
                    quota_rejected: None,
                    quota_resets_at: None,
                    quota_recovered: false,
                })
            }
            TmuxDecision::DoneButAlive => {
                return Ok(SlotOutcome {
                    ok: false,
                    pid: pane_pid.map(|t| t.pid),
                    exit_code: None,
                    signal: None,
                    error: Some("agent reported done but its process is still running".into()),
                    usage: tmux_recovered_usage(is_opencode, paths, &state.id, job, log_path),
                    agy_quota_hit: false,
                    quota_rejected: None,
                    quota_resets_at: None,
                    quota_recovered: false,
                })
            }
            TmuxDecision::Wait => {
                if !budget_left {
                    // Never success-on-timeout-alone (plan completion contract).
                    return Ok(SlotOutcome {
                        usage: tmux_recovered_usage(is_opencode, paths, &state.id, job, log_path),
                        ..SlotOutcome::err("tmux marker wait timed out")
                    });
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn slot_model_for(state: Option<&RunState>, job: &SlotJob) -> Option<String> {
    if let Some(m) = job.model.as_ref().filter(|s| !s.is_empty()) {
        return Some(m.clone());
    }
    state.and_then(|st| {
        st.slots
            .iter()
            .find(|s| s.id == job.slot_id)
            .and_then(|s| s.model.clone())
    })
}

pub fn init_slot(id: impl Into<String>, provider: impl Into<String>, role: SlotRole) -> SlotState {
    init_slot_model(id, provider, role, None)
}

pub fn init_slot_model(
    id: impl Into<String>,
    provider: impl Into<String>,
    role: SlotRole,
    model: Option<String>,
) -> SlotState {
    let provider = provider.into();
    let pref = ProviderRef::parse(&provider).expect("slot provider must be cli:… or api:…");
    SlotState {
        id: id.into(),
        // Model-free storage form: `@model` lives in `model`, not `provider`, so
        // slot ids, worktree/artifact names, and quota lookups stay unaffected.
        provider: pref.storage_key(),
        role,
        status: SlotStatus::Pending,
        backend: None,
        exec_backend: Some(pref.backend),
        cwd: None,
        log_path: None,
        error: None,
        pid: None,
        exit_code: None,
        signal: None,
        artifact: None,
        usage: None,
        // An explicit `@model` on the ref is a direct instruction and beats a
        // model chosen by `--select`'s model-select artifact (the `model` arg).
        model: pref.model.clone().or(model),
        round: 1,
        quota_hit: false,
        source: None,
    }
}

/// The resolved fleet (feature 011): one entry per seat, actual slots first, then any
/// still-projected seat (the plan gate's view of the implement panel it has not
/// dispatched yet) whose id does not already belong to a real slot — a real slot always
/// wins, so a projection can never contradict what actually got created.
pub fn run_fleet_seats(state: &RunState) -> Vec<FleetSeat> {
    let mut out: Vec<FleetSeat> = state
        .slots
        .iter()
        .map(|s| FleetSeat {
            seat: s.id.clone(),
            role: s.role,
            provider: s.provider.clone(),
            model: s.model.clone(),
            source: s.source.unwrap_or(SeatSource::Unknown),
            projected: false,
        })
        .collect();
    for seat in &state.projected_fleet {
        if out.iter().any(|s| s.seat == seat.seat) {
            continue;
        }
        out.push(seat.clone());
    }
    out
}

pub fn emit_run_json(state: &RunState) -> Result<()> {
    let v = serde_json::json!({
        // Both keys for outer agents (status uses `id`; emit historically used `run_id`).
        "run_id": state.id,
        "id": state.id,
        "workflow": state.workflow,
        "phase": state.phase,
        "task": state.task,
        "round": state.round,
        "max_rounds": state.max_rounds,
        "amendment": state.amendment,
        "dry_run": state.dry_run,
        "slots": state.slots,
        "providers": state.providers,
        "gates": state.gates,
        "error": state.error,
        "project_root": state.project_root,
        // `providers` is the pool; this is what each role actually drew from it.
        "roles": role_assignments(state),
        // One entry per seat, actual and projected, with where its provider came from
        // (feature 011). See `run_fleet_seats`.
        "fleet": run_fleet_seats(state),
        "base_ref": state.base_ref,
        "base_commit": state.base_commit,
        "parent_run": state.parent_run,
        "child_run": state.child_run,
        "usage": state.usage,
        "big": state.big,
        "autonomy": state.autonomy,
        "suite_outcome": state.suite_outcome,
        "contract_fingerprint": state.contract_fingerprint,
        "contract_modified": state.contract_modified,
        // null while in-flight; only set at terminal/gate phases
        "exit_code": state.status_exit_code(),
    });
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

/// Resolved `role=provider` assignment in slot order, reviewers joined with `+`.
///
/// `state.providers` is the run's *pool*. Printing that as the answer to "what is
/// running this" is wrong the moment `[roles]` or `--role` assigns anything: the pool
/// still lists providers no role ever drew, so an operator who deliberately excluded one
/// sees it on the launch line anyway.
pub fn role_assignments(state: &RunState) -> Vec<String> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for slot in &state.slots {
        let role = slot.role.as_config_key().to_string();
        let mut provider = slot.provider.clone();
        if !provider.contains('@') {
            if let Some(model) = slot.model.as_deref() {
                provider = format!("{provider}@{model}");
            }
        }
        match out.iter_mut().find(|(r, _)| *r == role) {
            Some((_, ps)) if ps.contains(&provider) => {}
            Some((_, ps)) => ps.push(provider),
            None => out.push((role, vec![provider])),
        }
    }
    out.into_iter()
        .map(|(role, ps)| format!("{role}={}", ps.join("+")))
        .collect()
}

pub fn print_run_human(state: &RunState) {
    println!("run_id:  {}", state.id);
    println!("phase:   {:?}", state.phase);
    if let (Some(r), Some(c)) = (&state.base_ref, &state.base_commit) {
        println!("base:    {r} ({})", c.chars().take(8).collect::<String>());
    }
    println!("workflow:{:?}", state.workflow);
    if let Some(t) = &state.task {
        println!("task:    {t}");
    }
    if let Some(a) = &state.amendment {
        println!("amendment: {a}");
    }
    let roles = role_assignments(state);
    if !roles.is_empty() {
        println!("roles:   {}", roles.join(", "));
    } else if !state.providers.is_empty() {
        println!(
            "providers: {} (pool; no slots dispatched yet)",
            state.providers.join(", ")
        );
    }
    if state.dry_run {
        println!("dry_run: true  (no git worktrees; agent processes stubbed only)");
    }
    // Gate phases only (feature 011, item C): that is where a human decides whether to
    // pay for the panel, and a table on every status print would just be noise.
    if state.phase.is_gate() {
        print_fleet_table(state);
    }
}

/// The resolved fleet, one row per seat: what a `roles:` line cannot show, because a
/// role can carry more than one reviewer and `roles:` collapses them, and because it
/// says nothing about a seat the run has not dispatched yet (feature 011, item C).
fn print_fleet_table(state: &RunState) {
    let seats = run_fleet_seats(state);
    if seats.is_empty() {
        return;
    }
    println!("fleet:");
    println!(
        "  {:<22} {:<12} {:<28} {:<16} projected",
        "seat", "role", "provider", "source"
    );
    for seat in &seats {
        let provider = match &seat.model {
            Some(m) if !seat.provider.contains('@') => format!("{}@{m}", seat.provider),
            _ => seat.provider.clone(),
        };
        println!(
            "  {:<22} {:<12} {:<28} {:<16} {}",
            seat.seat,
            seat.role.as_config_key(),
            provider,
            source_label(seat.source),
            seat.projected,
        );
    }
}

fn source_label(source: SeatSource) -> &'static str {
    match source {
        SeatSource::CliRole => "cli-role",
        SeatSource::CliProviders => "cli-providers",
        SeatSource::RolesFile => "roles-file",
        SeatSource::ProvidersOrder => "providers-order",
        SeatSource::ModelSelect => "model-select",
        SeatSource::SuitePreferences => "suite-preferences",
        SeatSource::Backup => "backup",
        SeatSource::Unknown => "unknown",
    }
}

/// How long a run must read as abandoned before `wait` gives up on it. Covers the gap
/// between `--detach` returning and the child acquiring the run lock, and the reacquire
/// window on resume. `SPAR_ABANDON_GRACE_SECS` overrides it (tests, and boxes where
/// detach is slower than this).
fn abandon_grace() -> Duration {
    std::env::var("SPAR_ABANDON_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(15))
}

pub fn wait_run(
    paths: &SparPaths,
    run_id: &str,
    timeout: Duration,
    json: bool,
    follow: bool,
) -> Result<crate::exit_codes::ExitCode> {
    let start = std::time::Instant::now();
    let poll = Duration::from_millis(250);
    let mut event_off = 0u64;
    let mut last_phase = None;
    let mut abandoned_since: Option<std::time::Instant> = None;
    loop {
        // Reconciled: `wait --json` serializes the slots it prints, and a run whose
        // orchestrator died mid-dispatch has slots frozen at `running` on disk.
        let state = RunState::load_for_display(paths, run_id)?;
        // The wait loop is a provider-agnostic delivery pulse: advance unacked-message
        // redelivery/escalation so requires_ack works even in runs with no Claude slot
        // (whose Stop hook is the only other thing that ticks acks). Best-effort.
        let _ = crate::bus::tick_acks(paths, &crate::bus::AckPolicy::default(), chrono::Utc::now());
        if follow && !json {
            let (off, evs) = crate::events::read_from_offset(paths, run_id, event_off)?;
            event_off = off;
            for ev in evs {
                println!("{}", ev.display_line());
            }
            if last_phase != Some(state.phase) {
                if last_phase.is_some() {
                    eprintln!("phase: {:?}", state.phase);
                }
                last_phase = Some(state.phase);
            }
        }
        if state.phase.is_waitable_stop() {
            if json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                print_run_human(&state);
            }
            return Ok(state.exit_code());
        }
        // Nobody owns a run in a non-resting phase: whoever was driving it died, so no
        // phase change is ever coming and blocking to the full timeout tells the caller
        // nothing. Held for a grace window first — a just-detached orchestrator has not
        // taken the lock yet, and a resume briefly drops it between load and re-acquire.
        if state.abandoned(paths) {
            let since = *abandoned_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= abandon_grace() {
                let orphans = crate::state::live_slot_pids(paths, &state);
                let mut state = state;
                state.error = Some(match orphans.len() {
                    0 => "run abandoned: no orchestrator owns it".to_string(),
                    n => format!(
                        "run abandoned: no orchestrator owns it; {n} slot process(es) still running"
                    ),
                });
                if json {
                    println!("{}", serde_json::to_string_pretty(&state)?);
                } else {
                    eprintln!("{}", state.error.as_deref().unwrap_or_default());
                    if !orphans.is_empty() {
                        eprintln!("reap them: spar stop {run_id}   (or: spar stop --abandoned)");
                    }
                    print_run_human(&state);
                }
                return Ok(crate::exit_codes::ExitCode::Stuck);
            }
        } else {
            abandoned_since = None;
        }
        if start.elapsed() >= timeout {
            if json {
                let mut s = state;
                s.error = Some("wait timed out".into());
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                eprintln!("wait timed out while phase={:?}", state.phase);
            }
            return Ok(crate::exit_codes::ExitCode::Stuck);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-8: api-sdk's own `append_log` (`src/api/runtime.rs`) never writes the offset
    /// index at all, so a stale `.idx` from an earlier native dispatch of this slot id
    /// must be cleared before it starts appending, or the TUI would bisect api-sdk's
    /// freshly-appended text against another dispatch's timestamps.
    #[test]
    fn reset_api_slot_log_truncates_log_and_drops_a_stale_offset_index() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("slot.log");
        std::fs::write(&log_path, "leftover transcript from a prior round\n").unwrap();
        std::fs::write(process::log_index_path(&log_path), "0 1700000000000\n").unwrap();

        reset_api_slot_log(&log_path);

        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "");
        assert!(!process::log_index_path(&log_path).exists());
    }

    fn dispatch_opts(prompt: &str) -> SpawnOpts {
        SpawnOpts {
            prompt: prompt.into(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: None,
            timeout_secs: None,
        }
    }

    #[test]
    fn build_dispatch_command_resumes_codex_when_a_prior_session_id_is_known() {
        // build_resume (opts.model: None) falls through to profile_model_args, which
        // reads $CODEX_HOME/<profile>.config.toml — lock and isolate CODEX_HOME so this
        // doesn't read the machine's real ~/.codex or race codex.rs's own env-mutating
        // tests, matching neither of which this test's assertions depend on.
        let _guard = providers::codex::ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CODEX_HOME", home.path());
        let opts = dispatch_opts("go");
        let (cmd, used_resume) = build_dispatch_command(
            &providers::CodexAdapter,
            Path::new("codex"),
            &opts,
            Some("thread-123"),
        );
        std::env::remove_var("CODEX_HOME");
        assert!(used_resume);
        let (_, args) = providers::command_to_parts(&cmd);
        assert_eq!(&args[..2], ["exec", "resume"]);
        assert!(args.iter().any(|a| a == "thread-123"));
    }

    #[test]
    fn build_dispatch_command_is_cold_without_a_prior_session_id() {
        let opts = dispatch_opts("go");
        let (cmd, used_resume) =
            build_dispatch_command(&providers::CodexAdapter, Path::new("codex"), &opts, None);
        assert!(!used_resume);
        let (_, args) = providers::command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(!args.iter().any(|a| a == "resume"));
    }

    #[test]
    fn build_dispatch_command_ignores_prior_session_id_for_an_adapter_without_resume() {
        // Grok's `build_resume` is the trait default (`None`), so a prior session id
        // must not change its dispatch shape even when one is present.
        let opts = dispatch_opts("go");
        let (with_prior, used_resume) = build_dispatch_command(
            &providers::GrokAdapter,
            Path::new("grok"),
            &opts,
            Some("some-id"),
        );
        assert!(!used_resume);
        let (without_prior, _) =
            build_dispatch_command(&providers::GrokAdapter, Path::new("grok"), &opts, None);
        assert_eq!(
            providers::command_to_parts(&with_prior).1,
            providers::command_to_parts(&without_prior).1
        );
    }

    #[test]
    fn resume_lost_its_session_is_true_only_for_a_resume_that_never_started() {
        use process::SpawnResult;
        let mut res = SpawnResult {
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            log_path: PathBuf::from("/tmp/x"),
            stdout_tail: String::new(),
            stats: process::StreamStats::default(),
        };
        // Resume, exited non-zero, no session id ever captured: the rollout is gone.
        assert!(resume_lost_its_session(true, &res));

        // Not a resume at all: an ordinary cold-dispatch failure is not this case.
        assert!(!resume_lost_its_session(false, &res));

        // Resume did establish a thread before failing later: a real error, not a lost
        // rollout, so no retry.
        res.stats.session_id = Some("thread-123".into());
        assert!(!resume_lost_its_session(true, &res));

        // Resume timed out rather than exiting: retrying cold would double the wall-clock
        // cost for what looks like a genuine hang, not a missing rollout.
        res.stats.session_id = None;
        res.timed_out = true;
        res.exit_code = None;
        assert!(!resume_lost_its_session(true, &res));

        // Resume exited clean but still never captured a session id: still a lost
        // rollout, regardless of exit code (see the function's own doc comment).
        res.timed_out = false;
        res.exit_code = Some(0);
        assert!(resume_lost_its_session(true, &res));

        // Session id captured, even on a clean exit: no retry.
        res.stats.session_id = Some("thread-123".into());
        assert!(!resume_lost_its_session(true, &res));
    }

    /// A test-only adapter whose `build_headless` runs an arbitrary shell script instead
    /// of a real provider binary, so `dispatch_with_resume_recovery`'s recovery-and-
    /// persist sequence can be exercised end to end (real `process::run_captured`, real
    /// marker files) without spawning `codex`/`grok`/etc.
    struct ShellAdapter {
        script: String,
        /// When true, `build_resume` echoes `RESUMED:<sid>` and then runs `script`,
        /// so tests can tell a resume retry from a cold restart in the slot log.
        resume: bool,
        /// When true, the 404 string counts as transient (muse's signature).
        transient_404: bool,
        /// When true, exit 2 counts as a spar usage error.
        usage_error_2: bool,
        /// When true, every resume attempt reports its session missing regardless
        /// of log text: stands in for muse's store-backed answer (missing session
        /// dir, or `resume: false` on a spar-requested resume), which no stdout
        /// prose can express.
        missing_session: bool,
    }

    impl ShellAdapter {
        fn cold(script: &str) -> Self {
            Self {
                script: script.into(),
                resume: false,
                transient_404: false,
                usage_error_2: false,
                missing_session: false,
            }
        }
    }

    impl providers::ProviderAdapter for ShellAdapter {
        fn name(&self) -> &'static str {
            "shell"
        }
        fn binary_names(&self) -> &[&'static str] {
            &["sh"]
        }
        fn capabilities(&self) -> providers::Capabilities {
            providers::Capabilities::default()
        }
        fn permission_args(&self, _policy: TrustPolicy) -> Vec<String> {
            vec![]
        }
        fn build_headless(&self, _bin: &Path, _opts: &SpawnOpts) -> std::process::Command {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg(&self.script);
            cmd
        }
        fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> std::process::Command {
            self.build_headless(bin, opts)
        }
        fn build_resume(
            &self,
            _bin: &Path,
            _opts: &SpawnOpts,
            session_id: &str,
        ) -> Option<std::process::Command> {
            if !self.resume {
                return None;
            }
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(format!("echo 'RESUMED:{session_id}'; {}", self.script));
            Some(cmd)
        }
        fn resume_failure_is_missing_session(
            &self,
            log_text: &str,
            _session_id: Option<&str>,
        ) -> bool {
            self.missing_session || log_text.contains("no rollout found")
        }
        fn dispatch_failure_is_transient(&self, log_text: &str) -> bool {
            self.transient_404 && log_text.contains("does not exist or you lack access")
        }
        fn is_usage_error(&self, code: Option<i32>) -> bool {
            self.usage_error_2 && code == Some(2)
        }
    }

    fn shell_req(script: &str, log_path: &Path) -> SpawnRequest {
        SpawnRequest {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.to_string()],
            cwd: PathBuf::from("/tmp"),
            log_path: log_path.to_path_buf(),
            env: vec![],
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn dispatch_with_resume_recovery_persists_a_captured_session_id() {
        // Directly guards against the regression review-1 flagged: this is what breaks
        // silently (resume never engages on a later round) if either call site's
        // `markers::write_session_id` call is ever dropped again.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let adapter =
            ShellAdapter::cold(r#"echo '{"type":"thread.started","thread_id":"cold-id-1"}'"#);
        let opts = dispatch_opts("go");
        let req = shell_req(&adapter.script, &log_path);
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            None,
            crate::config::IsolationMode::None,
            req,
            &paths,
            "run1",
            "slotA",
            "shell",
            &|_pid| {},
            &|| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(res.stats.session_id.as_deref(), Some("cold-id-1"));
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotA", "shell").as_deref(),
            Some("cold-id-1")
        );
    }

    #[test]
    fn dispatch_with_resume_recovery_clears_marker_and_retries_cold_on_a_lost_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        markers::write_session_id(&paths, "run1", "slotB", "shell", "stale-id").unwrap();

        // build_headless (the cold retry) succeeds and captures a fresh session id;
        // the initial `req` simulates a resume dispatch that died before thread.started
        // with codex's own missing-rollout text.
        let adapter =
            ShellAdapter::cold(r#"echo '{"type":"thread.started","thread_id":"fresh-id"}'"#);
        let opts = dispatch_opts("go");
        let lost_req = shell_req(
            "echo 'no rollout found for thread id stale-id' >&2; exit 1",
            &log_path,
        );
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            Some("stale-id"),
            crate::config::IsolationMode::None,
            lost_req,
            &paths,
            "run1",
            "slotB",
            "shell",
            &|_pid| {},
            &|| {},
            &|_| {},
        )
        .unwrap();
        // The cold retry ran and its captured id is what gets persisted — the marker
        // was cleared, then rewritten by the retry's own success, not left stale.
        assert_eq!(res.stats.session_id.as_deref(), Some("fresh-id"));
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotB", "shell").as_deref(),
            Some("fresh-id")
        );
    }

    #[test]
    fn dispatch_with_resume_recovery_leaves_marker_intact_on_an_unrelated_resume_failure() {
        // A pre-session failure that is not the rollout-missing signature must not clear
        // the marker or retry cold — see `resume_failure_is_missing_session`'s doc
        // comment. `ShellAdapter::build_headless` would succeed if called, so a passing
        // assertion here that the marker survives is also proof the cold path never ran.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        markers::write_session_id(&paths, "run1", "slotC", "shell", "still-valid-id").unwrap();

        let adapter =
            ShellAdapter::cold(r#"echo '{"type":"thread.started","thread_id":"should-not-run"}'"#);
        let opts = dispatch_opts("go");
        let broken_req = shell_req(
            "echo 'Model provider `openrouter` not found' >&2; exit 1",
            &log_path,
        );
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            Some("still-valid-id"),
            crate::config::IsolationMode::None,
            broken_req,
            &paths,
            "run1",
            "slotC",
            "shell",
            &|_pid| {},
            &|| {},
            &|_| {},
        )
        .unwrap();
        assert!(res.stats.session_id.is_none());
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotC", "shell").as_deref(),
            Some("still-valid-id"),
            "an unrelated pre-session failure must not destroy a still-valid marker"
        );
    }

    /// First two runs fail with the 404 signature after a tool call and a captured
    /// session id, the third succeeds. The counter file tells the attempts apart
    /// across the separate `run_captured` spawns.
    fn flaky_404_script(ctr: &Path) -> String {
        format!(
            r#"n=$(cat "{ctr}" 2>/dev/null || echo 0); echo $((n+1)) > "{ctr}"; echo '{{"type":"thread.started","thread_id":"sess-1"}}'; if [ "$n" -lt 2 ]; then echo '{{"type":"tool_call","name":"edit"}}'; echo 'model `m` does not exist or you lack access [request_id=r1]' >&2; exit 1; fi; echo '{{"type":"tool_call","name":"write"}}'; exit 0"#,
            ctr = ctr.display(),
        )
    }

    #[test]
    fn transient_failure_after_tool_calls_retries_through_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let ctr = tmp.path().join("attempts");
        let adapter = ShellAdapter {
            script: flaky_404_script(&ctr),
            resume: true,
            transient_404: true,
            usage_error_2: false,
            missing_session: false,
        };
        let opts = dispatch_opts("go");
        let req = shell_req(&adapter.script, &log_path);
        // Zero sleeps: the waits are recorded, never slept, so this never waits out
        // the real 60s/150s/300s schedule.
        let waits = std::cell::RefCell::new(Vec::new());
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            None,
            crate::config::IsolationMode::None,
            req,
            &paths,
            "run1",
            "slotT",
            "shell",
            &|_pid| {},
            &|| {},
            &|d| {
                waits.borrow_mut().push(d);
            },
        )
        .unwrap();
        assert_eq!(
            res.exit_code,
            Some(0),
            "the window clears on the third attempt"
        );
        assert!(res.stats.tools >= 1);
        assert_eq!(res.stats.session_id.as_deref(), Some("sess-1"));
        // The retries re-resolved through `build_dispatch_command`: the final log
        // carries the resume marker, proving the session continued instead of
        // cold-restarting.
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains("RESUMED:sess-1"),
            "retry must resume the captured session, not restart cold"
        );
        assert_eq!(
            *waits.borrow(),
            vec![Duration::from_secs(60), Duration::from_secs(150)],
            "two failed attempts, two backoff waits"
        );
        for n in 1..=2 {
            let sib = transient_retry_log_path(&log_path, n);
            let text = std::fs::read_to_string(&sib).unwrap();
            assert!(
                text.contains("does not exist or you lack access"),
                "attempt {n}'s 404 must survive in {}",
                sib.display()
            );
        }
        let events = std::fs::read_to_string(crate::events::events_file(&paths, "run1")).unwrap();
        assert_eq!(
            events.matches("transient provider failure").count(),
            2,
            "one slot note per retry so the operator sees the wait"
        );
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotT", "shell").as_deref(),
            Some("sess-1")
        );
    }

    #[test]
    fn transient_signature_with_zero_tool_calls_fails_fast() {
        // A first-call 404 is indistinguishable from a genuinely wrong model name or
        // a dead entitlement: no retry, no wait, no preserved sibling log.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let adapter = ShellAdapter {
            script: "echo 'model `m` does not exist or you lack access' >&2; exit 1".into(),
            resume: true,
            transient_404: true,
            usage_error_2: false,
            missing_session: false,
        };
        let opts = dispatch_opts("go");
        let req = shell_req(&adapter.script, &log_path);
        let waits = std::cell::RefCell::new(Vec::new());
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            None,
            crate::config::IsolationMode::None,
            req,
            &paths,
            "run1",
            "slotZ",
            "shell",
            &|_pid| {},
            &|| {},
            &|d| {
                waits.borrow_mut().push(d);
            },
        )
        .unwrap();
        assert_eq!(res.exit_code, Some(1));
        assert!(
            waits.borrow().is_empty(),
            "zero tool calls must not wait out a window"
        );
        assert!(
            !transient_retry_log_path(&log_path, 1).exists(),
            "no retry means no preserved sibling log"
        );
        assert!(markers::read_session_id(&paths, "run1", "slotZ", "shell").is_none());
    }

    #[test]
    fn usage_error_is_never_retried_and_leaves_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        markers::write_session_id(&paths, "run1", "slotU", "shell", "keep-me").unwrap();
        // The adapter's own script would succeed and capture a fresh id if the cold
        // path ever ran — so a passing marker assertion is also proof it never did.
        let adapter = ShellAdapter {
            script: r#"echo '{"type":"thread.started","thread_id":"should-not-run"}'"#.into(),
            resume: true,
            transient_404: true,
            usage_error_2: true,
            missing_session: false,
        };
        let opts = dispatch_opts("go");
        let bad_req = shell_req("exit 2", &log_path);
        let waits = std::cell::RefCell::new(Vec::new());
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            Some("keep-me"),
            crate::config::IsolationMode::None,
            bad_req,
            &paths,
            "run1",
            "slotU",
            "shell",
            &|_pid| {},
            &|| {},
            &|d| {
                waits.borrow_mut().push(d);
            },
        )
        .unwrap();
        assert_eq!(res.exit_code, Some(2));
        assert!(res.stats.session_id.is_none());
        assert!(
            waits.borrow().is_empty(),
            "a usage error is spar's fault, never retried"
        );
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotU", "shell").as_deref(),
            Some("keep-me"),
            "a usage error must not destroy a possibly-valid marker"
        );
    }

    #[test]
    fn unrelated_failure_after_tool_calls_does_not_retry() {
        // Only the overriding adapter's own string retries: any other failure after
        // real work still fails the slot immediately.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let adapter = ShellAdapter {
            script: r#"echo '{"type":"tool_call","name":"edit"}'; echo 'boom' >&2; exit 1"#.into(),
            resume: true,
            transient_404: false,
            usage_error_2: false,
            missing_session: false,
        };
        let opts = dispatch_opts("go");
        let req = shell_req(&adapter.script, &log_path);
        let waits = std::cell::RefCell::new(Vec::new());
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            None,
            crate::config::IsolationMode::None,
            req,
            &paths,
            "run1",
            "slotN",
            "shell",
            &|_pid| {},
            &|| {},
            &|d| {
                waits.borrow_mut().push(d);
            },
        )
        .unwrap();
        assert_eq!(res.exit_code, Some(1));
        assert!(res.stats.tools >= 1);
        assert!(waits.borrow().is_empty());
        assert!(!transient_retry_log_path(&log_path, 1).exists());
    }

    #[test]
    fn stale_exit_zero_resume_is_noted_not_retried() {
        // muse answers an unknown session id with exit 0 on a brand-new session,
        // so the failure-path gate (`session_id.is_none()`) never fires: the
        // captured id is `Some`, just not the requested one. The dispatch itself
        // succeeded, so there is nothing to retry — but the operator must see
        // that the round ran with fresh context, and the marker must point at
        // the session that actually ran.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let adapter = ShellAdapter {
            // The cold path must never run here: a passing marker assertion is
            // also proof no cold retry fired on a successful dispatch.
            script: r#"echo '{"type":"thread.started","thread_id":"should-not-run"}'"#.into(),
            resume: true,
            transient_404: false,
            usage_error_2: false,
            missing_session: true,
        };
        let opts = dispatch_opts("go");
        // Simulates the resume dispatch: exit 0, but the captured id is not the
        // requested one — the vendor minted a fresh session instead.
        let stale_req = shell_req(
            r#"echo '{"type":"thread.started","thread_id":"fresh-sess"}'"#,
            &log_path,
        );
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            Some("gone-sess"),
            crate::config::IsolationMode::None,
            stale_req,
            &paths,
            "run1",
            "slotS",
            "shell",
            &|_pid| {},
            &|| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert_eq!(res.stats.session_id.as_deref(), Some("fresh-sess"));
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotS", "shell").as_deref(),
            Some("fresh-sess"),
            "the marker follows the session that actually ran"
        );
        let events = std::fs::read_to_string(crate::events::events_file(&paths, "run1")).unwrap();
        assert!(
            events.contains("gone-sess") && events.contains("fresh-sess"),
            "the operator sees the stale resume and the fresh session: {events}"
        );

        // Same ids, same seam answer: a genuine resume notes nothing.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let opts = dispatch_opts("go");
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            Some("same-sess"),
            crate::config::IsolationMode::None,
            shell_req(
                r#"echo '{"type":"thread.started","thread_id":"same-sess"}'"#,
                &log_path,
            ),
            &paths,
            "run1",
            "slotS",
            "shell",
            &|_pid| {},
            &|| {},
            &|_| {},
        )
        .unwrap();
        assert_eq!(res.exit_code, Some(0));
        let events =
            std::fs::read_to_string(crate::events::events_file(&paths, "run1")).unwrap_or_default();
        assert!(
            !events.contains("was gone"),
            "a genuine resume notes nothing: {events}"
        );
    }

    /// Attempts 1-2 fail transient after a tool call; attempt 3 (a resume) dies
    /// pre-session with the missing-session signature; the cold retry succeeds.
    fn transient_then_lost_script(ctr: &Path) -> String {
        format!(
            r#"n=$(cat "{ctr}" 2>/dev/null || echo 0); echo $((n+1)) > "{ctr}"; if [ "$n" -lt 2 ]; then echo '{{"type":"thread.started","thread_id":"sess-1"}}'; echo '{{"type":"tool_call","name":"edit"}}'; echo 'model `m` does not exist or you lack access' >&2; exit 1; fi; if [ "$n" -lt 3 ]; then echo 'no rollout found for thread id sess-1' >&2; exit 1; fi; echo '{{"type":"thread.started","thread_id":"cold-fresh"}}'; exit 0"#,
            ctr = ctr.display(),
        )
    }

    #[test]
    fn retry_that_loses_its_session_retries_cold_once() {
        // A session can die between transient attempts: the retry loop only
        // matches the transient signature, so without the post-loop check the
        // dead id would stay in the marker and the slot would fail.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("slot.log");
        let ctr = tmp.path().join("attempts");
        let adapter = ShellAdapter {
            script: transient_then_lost_script(&ctr),
            resume: true,
            transient_404: true,
            usage_error_2: false,
            missing_session: false,
        };
        let opts = dispatch_opts("go");
        let req = shell_req(&adapter.script, &log_path);
        let waits = std::cell::RefCell::new(Vec::new());
        let res = dispatch_with_resume_recovery(
            &adapter,
            Path::new("/bin/sh"),
            &opts,
            None,
            crate::config::IsolationMode::None,
            req,
            &paths,
            "run1",
            "slotL",
            "shell",
            &|_pid| {},
            &|| {},
            &|d| {
                waits.borrow_mut().push(d);
            },
        )
        .unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert_eq!(res.stats.session_id.as_deref(), Some("cold-fresh"));
        assert_eq!(
            std::fs::read_to_string(&ctr).unwrap().trim(),
            "4",
            "two transient attempts, one lost retry, one cold retry"
        );
        assert_eq!(
            *waits.borrow(),
            vec![Duration::from_secs(60), Duration::from_secs(150)],
            "only the transient failures wait; the cold retry is immediate"
        );
        assert_eq!(
            markers::read_session_id(&paths, "run1", "slotL", "shell").as_deref(),
            Some("cold-fresh"),
            "the dead id is cleared, the cold retry's id persisted"
        );
        let events = std::fs::read_to_string(crate::events::events_file(&paths, "run1")).unwrap();
        assert!(
            events.contains("on a retry") && events.contains("retrying cold"),
            "the operator sees the mid-retry loss and the cold retry: {events}"
        );
    }

    #[test]
    fn parse_backoff_list_keeps_numbers_and_drops_garbage() {
        assert_eq!(parse_backoff_list("60,150,300"), vec![60, 150, 300]);
        assert_eq!(parse_backoff_list("0,0,0"), vec![0, 0, 0]);
        assert_eq!(parse_backoff_list("60,nope,30"), vec![60, 30]);
        assert!(parse_backoff_list("nope").is_empty());
        assert!(parse_backoff_list("").is_empty());
    }

    #[test]
    fn dispatch_error_names_usage_faults_and_budget_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("slot.log");
        std::fs::write(&log_path, "stopped: hit --max-model-steps, exiting 1\n").unwrap();
        let budget = dispatch_error(
            &providers::MuseAdapter,
            &log_path,
            "muse exec --json",
            Some(1),
            None,
        );
        assert!(
            budget.contains("model-step budget"),
            "a step-budget stop is not a crash: {budget}"
        );
        assert!(
            budget.contains("hit --max-model-steps"),
            "the budget stop carries the provider's own tail: {budget}"
        );
        let usage = dispatch_error(
            &providers::MuseAdapter,
            &log_path,
            "muse exec --session-id x",
            Some(2),
            None,
        );
        assert!(
            usage.contains("usage error") && usage.contains("muse exec --session-id x"),
            "a usage error names spar's command line: {usage}"
        );
        assert!(
            usage.contains("hit --max-model-steps"),
            "the usage error carries the provider's own tail: {usage}"
        );
        // Adapters without these signatures keep the generic line.
        assert_eq!(
            dispatch_error(
                &providers::GrokAdapter,
                &log_path,
                "grok ...",
                Some(1),
                None
            ),
            "exit 1"
        );
    }

    #[test]
    fn backoff_schedule_parses_and_defaults() {
        assert_eq!(parse_backoff_list("0,0,0"), vec![0, 0, 0]);
        assert_eq!(parse_backoff_list(" 60 , 150 , 300 "), vec![60, 150, 300]);
        assert!(parse_backoff_list("nope").is_empty());
        assert!(
            parse_backoff_list("").is_empty(),
            "an empty override falls back to the constants, it does not disable retries"
        );
        assert_eq!(
            TRANSIENT_RETRY_BACKOFF_SECS,
            [60, 150, 300],
            "four attempts over ~8.5 minutes, waiting out a ten-minute window"
        );
    }

    fn impl_job(slot_id: &str) -> SlotJob {
        SlotJob {
            slot_id: slot_id.into(),
            provider: "cli:muse".into(),
            role: SlotRole::Implementer,
            template: "implement".into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some(format!("summary-{slot_id}.md")),
            model: None,
        }
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must be available for salvage tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn dirty_repo(base: &Path) -> PathBuf {
        let repo = base.join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("foo.rs"), "fn main() {}\n").unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );
        std::fs::write(repo.join("foo.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
        std::fs::write(repo.join("new.rs"), "new file\n").unwrap();
        repo
    }

    #[test]
    fn salvage_writes_a_synthesized_carry_forward_for_a_dirty_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let repo = dirty_repo(tmp.path());
        let log_path = paths.log_file("r1", "impl");
        std::fs::write(&log_path, "→ edit  foo.rs  success\n").unwrap();
        let job = impl_job("impl");
        salvage_expected_artifact(
            &paths,
            "r1",
            &job,
            &log_path,
            "killed by signal 9 (SIGKILL)",
            &repo,
            4000,
        );
        let brief = paths.artifact("r1", "carry-forward-impl.md");
        let body = std::fs::read_to_string(&brief).unwrap();
        assert!(
            body.contains("Synthesized carry-forward"),
            "marked machine-written"
        );
        assert!(
            body.contains("foo.rs"),
            "the brief names what changed in the worktree: {body}"
        );
        assert!(
            body.len() <= 4000 + 16,
            "clamped to the carry-forward budget, found {}",
            body.len()
        );
        assert!(
            paths.artifact("r1", "summary-impl.md").is_file(),
            "the primary artifact is still salvaged alongside the brief"
        );
    }

    #[test]
    fn salvage_never_overwrites_an_agent_written_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let repo = dirty_repo(tmp.path());
        let log_path = paths.log_file("r1", "impl");
        std::fs::write(&log_path, "→ edit  foo.rs  success\n").unwrap();
        let brief = paths.artifact("r1", "carry-forward-impl.md");
        std::fs::write(&brief, "agent words\n").unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &impl_job("impl"),
            &log_path,
            "killed",
            &repo,
            4000,
        );
        assert_eq!(std::fs::read_to_string(&brief).unwrap(), "agent words\n");
    }

    #[test]
    fn salvage_clamps_the_synthesized_brief_to_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let repo = dirty_repo(tmp.path());
        std::fs::write(repo.join("big.rs"), "x\n".repeat(2000)).unwrap();
        git(&repo, &["add", "."]);
        let log_path = paths.log_file("r1", "impl");
        std::fs::write(&log_path, "→ edit  big.rs  success\n").unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &impl_job("impl"),
            &log_path,
            "killed",
            &repo,
            500,
        );
        let body = std::fs::read_to_string(paths.artifact("r1", "carry-forward-impl.md")).unwrap();
        assert!(body.contains("Synthesized carry-forward"));
        assert!(body.len() <= 500 + 16, "found {}", body.len());
    }

    #[test]
    fn salvage_writes_no_brief_for_other_roles_clean_trees_or_empty_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let repo = dirty_repo(tmp.path());
        let log_path = paths.log_file("r1", "rev");
        std::fs::write(&log_path, "partial review\n").unwrap();
        // A dirty worktree does not earn a reviewer a brief: only the implementer owes
        // one, and only the implementer gets one synthesized.
        let reviewer = SlotJob {
            role: SlotRole::Reviewer,
            expected_artifact: Some("review-rev.md".into()),
            ..impl_job("rev")
        };
        salvage_expected_artifact(&paths, "r1", &reviewer, &log_path, "killed", &repo, 4000);
        assert!(!paths.artifact("r1", "carry-forward-rev.md").exists());
        // An implementer with a clean tree and an empty log has nothing to say.
        let clean = tmp.path().join("clean");
        std::fs::create_dir_all(&clean).unwrap();
        git(&clean, &["init", "-q"]);
        std::fs::write(clean.join("a.txt"), "a\n").unwrap();
        git(&clean, &["add", "."]);
        git(
            &clean,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ],
        );
        let empty_log = paths.log_file("r1", "impl2");
        std::fs::write(&empty_log, "").unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &impl_job("impl2"),
            &empty_log,
            "killed",
            &clean,
            4000,
        );
        assert!(!paths.artifact("r1", "carry-forward-impl2.md").exists());
        // Outside any repo the diff is skipped but the transcript is kept.
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        let bare_log = paths.log_file("r1", "impl3");
        std::fs::write(&bare_log, "→ edit  foo.rs  success\n").unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &impl_job("impl3"),
            &bare_log,
            "killed",
            &bare,
            4000,
        );
        let body = std::fs::read_to_string(paths.artifact("r1", "carry-forward-impl3.md")).unwrap();
        assert!(body.contains("Partial tool transcript"));
    }

    /// `usage_from_stream` is the only place `StreamStats`'s cost/subagent/model
    /// fields reach `SlotUsage`, the run record. This exercises it end to end,
    /// including a `state.json` round-trip, so deleting the carry-through lines
    /// would fail here rather than only failing to show up in a real run.
    #[test]
    fn usage_from_stream_carries_cost_and_subagent_fields_into_state_json() {
        let mut stats = process::StreamStats {
            input_tokens: 10,
            output_tokens: 20,
            cost_usd: Some(0.4521),
            ..Default::default()
        };
        stats.subagent_stats = Some(process::SubagentStats {
            spawned: 3,
            completed: 2,
            failed: 1,
            ..Default::default()
        });
        stats.model_usage.insert(
            "claude-opus-5".to_string(),
            process::ModelUsage {
                cost_usd: Some(0.2848),
                input_tokens: 6,
                output_tokens: 294,
                ..Default::default()
            },
        );

        let usage = usage_from_stream("impl", "cli:claude", &stats);
        assert_eq!(usage.cost_usd, Some(0.4521));
        assert_eq!(usage.subagent_stats.as_ref().unwrap().spawned, 3);
        assert_eq!(
            usage.model_usage.get("claude-opus-5").unwrap().cost_usd,
            Some(0.2848)
        );

        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r-usage",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.usage.push(usage);
        state.save(&paths).unwrap();

        let loaded = RunState::load(&paths, "r-usage").unwrap();
        let loaded_usage = &loaded.usage[0];
        assert_eq!(loaded_usage.cost_usd, Some(0.4521));
        assert_eq!(loaded_usage.subagent_stats.as_ref().unwrap().spawned, 3);
        assert_eq!(
            loaded_usage
                .model_usage
                .get("claude-opus-5")
                .unwrap()
                .cost_usd,
            Some(0.2848)
        );
    }

    /// The real captured log text from the dogfooding incident (roadmap/BACKLOG.md):
    /// a rate-limited slot that died mid-dispatch. This is the discriminator `run_slot`
    /// routes `Phase::Quota` on.
    const WEEKLY_LIMIT_LOG: &str = "! rate limit  seven_day  rejected\n\
        You've hit your weekly limit \u{b7} resets 12am (America/New_York)\n";

    /// What the coalescer actually renders into the log alongside a structured
    /// rejection (`src/process.rs`'s `rate_limit_event` handler): the two always land
    /// together, so a realistic `structured` test also carries this line.
    const FIVE_HOUR_REJECTION_LOG: &str = "! rate limit  five_hour  rejected\n";

    #[test]
    fn detect_and_pause_quota_flags_a_rate_limit_log() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            WEEKLY_LIMIT_LOG,
            None,
            None,
            false
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        assert!(!store.is_usable("cli:claude"));
        let q = store.get("cli:claude");
        assert!(
            q.cooldown_until.is_none(),
            "a weekly window states a time but no day, so spar must fall back to the \
             generic timer rather than assert a reset instant it cannot know"
        );
    }

    /// The executor path end to end: a typed rejection carrying a future stated instant
    /// must pause the store until exactly that instant, not the generic timer.
    #[test]
    fn detect_and_pause_quota_with_a_future_typed_reset_pauses_until_exactly_then() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let until = chrono::Utc::now() + chrono::Duration::hours(2);
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            FIVE_HOUR_REJECTION_LOG,
            Some("five_hour"),
            Some(until),
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_eq!(q.status, crate::quota::ProviderStatus::Cooldown);
        assert_eq!(q.cooldown_until, Some(until));
        assert!(!store.is_usable("cli:claude"));
    }

    /// A stated instant only a few seconds in the future must be honored exactly, not
    /// discarded for being "too soon": if the window really does reopen in 10 seconds,
    /// pausing for 10 seconds and then being available again is correct behaviour, not
    /// the same failure as a non-future instant.
    #[test]
    fn detect_and_pause_quota_with_a_near_future_typed_reset_pauses_until_exactly_then() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let until = chrono::Utc::now() + chrono::Duration::seconds(10);
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            FIVE_HOUR_REJECTION_LOG,
            Some("five_hour"),
            Some(until),
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_eq!(q.status, crate::quota::ProviderStatus::Cooldown);
        assert_eq!(q.cooldown_until, Some(until));
    }

    /// A stated instant only an epsilon in the future (clock skew, or the gap between
    /// the event arriving and this function running) must be written exactly as stated,
    /// not extended: `implausible_reset` already rejects a non-future instant in favor
    /// of the generic fallback, so anything that reaches here passed as strictly future
    /// and must be honored as such, even if that future is only milliseconds away. If
    /// the window really does reopen that soon, pausing briefly and then being usable
    /// again is correct, not a case to guard against by padding the write.
    #[test]
    fn detect_and_pause_quota_with_an_epsilon_future_typed_reset_writes_it_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let until = chrono::Utc::now() + chrono::Duration::milliseconds(50);
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            FIVE_HOUR_REJECTION_LOG,
            Some("five_hour"),
            Some(until),
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_eq!(q.status, crate::quota::ProviderStatus::Cooldown);
        assert_eq!(
            q.cooldown_until,
            Some(until),
            "the written cooldown must match the stated instant exactly, not be extended"
        );
    }

    /// A stated instant already in the past must not be written as a `Cooldown` — that
    /// reads as immediately usable, worse than no cooldown. It must fall back to the
    /// generic auto-recovering pause instead.
    ///
    /// `log_text` is deliberately empty (no rendered rate-limit line, no prose at all):
    /// the earlier version of this test passed `FIVE_HOUR_REJECTION_LOG` and so
    /// couldn't tell the fallback pause this test names apart from the text scrape
    /// upstream in `detect_and_pause_quota` happening to have already written one. An
    /// adapter can state `structured`/`resets_at` with a log tail that rolled the
    /// rendered line out of its 8 KB window (or carries no prose at all), and the
    /// fallback must still fire from the structured branch itself.
    #[test]
    fn detect_and_pause_quota_with_a_past_typed_reset_falls_back_to_the_generic_pause() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            "",
            Some("five_hour"),
            Some(past),
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_ne!(
            q.status,
            crate::quota::ProviderStatus::Cooldown,
            "a past stated instant must not be written as a cooldown"
        );
        assert!(
            !store.is_usable("cli:claude"),
            "the generic fallback pause must still apply even with no rate-limit prose \
             anywhere in the log tail"
        );
    }

    /// A stated instant further out than any real Claude window (e.g. a millisecond
    /// timestamp misread as seconds, landing decades away) must not be trusted either:
    /// it would brick the provider with no auto-recovery. See the past-instant test
    /// above for why `log_text` is empty here too.
    #[test]
    fn detect_and_pause_quota_with_an_implausible_typed_reset_falls_back_to_the_generic_pause() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let absurd = chrono::Utc::now() + chrono::Duration::days(365 * 100);
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            "",
            Some("five_hour"),
            Some(absurd),
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_ne!(
            q.status,
            crate::quota::ProviderStatus::Cooldown,
            "an implausible stated instant must not be written as a cooldown"
        );
        assert!(
            !store.is_usable("cli:claude"),
            "the generic fallback pause must still apply even with no rate-limit prose \
             anywhere in the log tail"
        );
    }

    /// The `structured = Some(_)` with no stated instant at all (`resets_at: None`) arm
    /// has the same hole as the past/implausible cases: it must still leave the
    /// provider paused, not `Available`, even with no rate-limit prose in the log.
    #[test]
    fn detect_and_pause_quota_with_a_typed_rejection_and_no_reset_falls_back_to_the_generic_pause()
    {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            "",
            Some("unknown"),
            None,
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        assert!(
            !store.is_usable("cli:claude"),
            "a typed rejection with no stated instant must still pause the provider"
        );
    }

    /// The two implausibility directions must read differently to an operator watching
    /// stderr: a stale-but-sane instant is not the same failure as a value that was
    /// never in the assumed shape to begin with.
    #[test]
    fn implausible_reset_describes_the_two_directions_differently() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::hours(1);
        let far = now + chrono::Duration::days(365 * 100);
        assert!(matches!(
            implausible_reset(past),
            Some(ImplausibleReset::AlreadyElapsed)
        ));
        assert!(matches!(
            implausible_reset(far),
            Some(ImplausibleReset::OutOfRange)
        ));
        assert!(implausible_reset(now + chrono::Duration::hours(1)).is_none());
        let elapsed_msg = ImplausibleReset::AlreadyElapsed.describe(past);
        let range_msg = ImplausibleReset::OutOfRange.describe(far);
        assert_ne!(elapsed_msg, range_msg);
        assert!(elapsed_msg.contains("already elapsed"));
        assert!(range_msg.contains("implausibly far out"));
    }

    /// A more precise cooldown a text scrape already wrote from real prose in the same
    /// log tail must not be downgraded to the coarse generic pause just because the
    /// structured instant itself was unusable.
    #[test]
    fn detect_and_pause_quota_keeps_a_scraped_cooldown_when_the_typed_reset_is_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        assert!(detect_and_pause_quota(
            &paths,
            "cli:claude",
            WEEKLY_LIMIT_LOG,
            Some("seven_day"),
            Some(past),
            true
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_eq!(
            q.hint.as_deref(),
            Some("claude weekly limit (no stated reset instant, default cooldown)"),
            "the scrape's own pause must survive, not get overwritten by the generic fallback"
        );
    }

    /// The wiring between a `SlotOutcome` and `detect_and_pause_quota`, not just the
    /// callee: a future stated instant on the outcome must pause the store until
    /// exactly then, with no rate-limit prose in the log to do it via the text scrape
    /// instead. Deleting the `outcome.quota_resets_at` forwarding inside
    /// `quota_hit_for_outcome` (or passing `None` at either of its two call sites)
    /// makes this fail.
    #[test]
    fn quota_hit_for_outcome_carries_a_future_typed_reset_into_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("s1.log");
        std::fs::write(&log_path, b"").unwrap();
        let until = chrono::Utc::now() + chrono::Duration::hours(2);
        let outcome = SlotOutcome {
            quota_rejected: Some("five_hour".into()),
            quota_resets_at: Some(until),
            ..SlotOutcome::err("boom")
        };
        assert!(quota_hit_for_outcome(
            &paths,
            "cli:claude",
            &log_path,
            &outcome
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        let q = store.get("cli:claude");
        assert_eq!(q.status, crate::quota::ProviderStatus::Cooldown);
        assert_eq!(q.cooldown_until, Some(until));
    }

    /// Same wiring, the past/implausible half: an unusable stated instant on the
    /// outcome must still leave the provider paused via the generic fallback, with no
    /// rate-limit prose in the log to fall back onto instead.
    #[test]
    fn quota_hit_for_outcome_falls_back_when_the_typed_reset_is_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("s1.log");
        std::fs::write(&log_path, b"").unwrap();
        let outcome = SlotOutcome {
            quota_rejected: Some("five_hour".into()),
            quota_resets_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            ..SlotOutcome::err("boom")
        };
        assert!(quota_hit_for_outcome(
            &paths,
            "cli:claude",
            &log_path,
            &outcome
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        assert!(!store.is_usable("cli:claude"));
    }

    /// A slot with no typed verdict at all must still reach the prose fallback: the
    /// wiring must pass `None`/`None` through, not swallow the log tail.
    #[test]
    fn quota_hit_for_outcome_with_no_typed_verdict_still_reaches_the_prose_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("s1.log");
        std::fs::write(&log_path, WEEKLY_LIMIT_LOG).unwrap();
        let outcome = SlotOutcome::err("boom");
        assert!(quota_hit_for_outcome(
            &paths,
            "cli:claude",
            &log_path,
            &outcome
        ));
    }

    /// The AC-3 fix itself, at the level the bug actually reproduces: a stream that was
    /// rejected, then recovered (a later `allowed`/`allowed_warning` event), must not
    /// route to `Phase::Quota` on its own stale rejection line — the coalescer renders
    /// that line into the log the moment the rejection arrives, and it outlives the
    /// event's own recovery in the tail. `quota_rejected: None` alone cannot tell "this
    /// adapter never spoke" apart from "it spoke, and recovered"; only `quota_recovered`
    /// can, and this is the exact log/verdict combination that distinguishes them:
    /// same rejection line, opposite `quota_recovered`, opposite route.
    #[test]
    fn detect_and_pause_quota_a_recovered_stream_does_not_route_on_its_own_stale_rejection_line() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert!(
            !detect_and_pause_quota(
                &paths,
                "cli:claude",
                FIVE_HOUR_REJECTION_LOG,
                None,
                None,
                true
            ),
            "a recovered stream (quota_recovered, no current rejection) must not route on a \
             rendered rejection line an earlier, already-recovered event left behind"
        );
        let store = crate::quota::QuotaStore::load(&paths).unwrap_or_default();
        assert!(
            store.is_usable("cli:claude"),
            "a recovered stream must not leave the provider paused off the same stale \
             rejection line that routing correctly ignored"
        );
        assert!(
            detect_and_pause_quota(
                &paths,
                "cli:claude",
                FIVE_HOUR_REJECTION_LOG,
                None,
                None,
                false
            ),
            "the same log, with quota_recovered false (adapter never spoke this dispatch), \
             must still reach the prose fallback"
        );
    }

    /// The wiring for the AC-3 fix, not just the callee: a `SlotOutcome` whose stream
    /// recovered (`quota_recovered: true`, `quota_rejected: None`) must not route a later,
    /// genuinely unrelated failure onto `Phase::Quota` just because the log tail still
    /// carries the earlier rejection's rendered line. Deleting the `outcome.quota_recovered`
    /// forwarding inside `quota_hit_for_outcome` makes this fail.
    #[test]
    fn quota_hit_for_outcome_does_not_route_a_recovered_stream_on_its_stale_log_line() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log_path = tmp.path().join("s1.log");
        std::fs::write(
            &log_path,
            format!("{FIVE_HOUR_REJECTION_LOG}unrelated panic: index out of bounds\n"),
        )
        .unwrap();
        let outcome = SlotOutcome {
            quota_recovered: true,
            ..SlotOutcome::err("index out of bounds")
        };
        assert!(
            !quota_hit_for_outcome(&paths, "cli:claude", &log_path, &outcome),
            "a recovered stream's later unrelated failure must not be routed as quota"
        );
    }

    /// `StreamStats::quota_resets_at` is epoch seconds; this must not silently accept a
    /// value chrono can still parse into a `DateTime` at some other scale. The bound
    /// that actually catches a millisecond timestamp is `implausible_reset`, not
    /// this conversion — this test only pins down that the conversion itself stays
    /// seconds-based rather than growing its own (wrong) unit guess later.
    #[test]
    fn resets_at_from_epoch_secs_treats_the_input_as_seconds() {
        assert_eq!(
            resets_at_from_epoch_secs(Some(1_700_000_000)),
            chrono::DateTime::from_timestamp(1_700_000_000, 0)
        );
        assert_eq!(resets_at_from_epoch_secs(None), None);
    }

    /// The discriminator must not fire for an ordinary defect — a false positive here
    /// would swallow a real failure as "quota", which is worse than the bug being fixed.
    #[test]
    fn detect_and_pause_quota_ignores_an_ordinary_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = "thread 'main' panicked at src/main.rs:42:\nindex out of bounds\n";
        assert!(!detect_and_pause_quota(
            &paths,
            "cli:claude",
            log,
            None,
            None,
            false
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        assert!(store.is_usable("cli:claude"));
    }

    /// A genuine failure whose log happens to contain one of `scrape_log_hint`'s broad
    /// needles (here: a line number containing "429") is allowed to pause the provider
    /// — cheap and auto-recovering — but must not be reported as a quota hit, which
    /// would misroute the run onto `Phase::Quota` instead of failing it.
    #[test]
    fn detect_and_pause_quota_does_not_route_a_broad_needle_false_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = "thread 'main' panicked at src/state.rs:429:\nindex out of bounds\n";
        assert!(!detect_and_pause_quota(
            &paths,
            "cli:claude",
            log,
            None,
            None,
            false
        ));
    }

    /// On the api-sdk backend a 429 propagates as a `Result::Err` whose text never
    /// reaches the log file (`api/runtime.rs` appends the request/response around
    /// `chat_completion`'s call, not the error it can raise) — the log tail alone must
    /// miss it, and scraping the error text alongside it must catch it.
    #[test]
    fn detect_and_pause_quota_with_err_catches_a_429_only_in_the_propagated_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = "--- api step 0 model=gpt-5 ---\n";
        assert!(
            !detect_and_pause_quota(&paths, "api:openai", log, None, None, false),
            "the log alone carries no rate-limit signal"
        );
        let err_text = "api openai status 429: rate limit exceeded, too many requests";
        assert!(detect_and_pause_quota_with_err(
            &paths,
            "api:openai",
            log,
            err_text,
        ));
    }

    /// A generic rejection phrase (no Claude-specific shape) must still route, so
    /// non-claude adapters are covered by the same discriminator.
    #[test]
    fn detect_and_pause_quota_routes_a_generic_rejection_phrase() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = "error: usage limit reached for this account\n";
        assert!(detect_and_pause_quota(
            &paths,
            "cli:codex",
            log,
            None,
            None,
            false
        ));
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        assert!(!store.is_usable("cli:codex"));
    }

    /// A pinned model ref (`cli:codex@gpt-5.6-terra`, the fleet's normal shape per the
    /// operator's "pin the model on every role" rule) must pause under the same
    /// normalized bucket readers look up — writing the raw ref key left the pause
    /// inert for every non-claude adapter, since `is_usable`/`ensure_usable` always
    /// query `normalize_key(name)`.
    #[test]
    fn detect_and_pause_quota_pauses_a_pinned_provider_under_its_normalized_key() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = "error: usage limit reached for this account\n";
        assert!(detect_and_pause_quota(
            &paths,
            "cli:codex@gpt-5.6-terra",
            log,
            None,
            None,
            false
        ));
        let err = crate::quota::ensure_usable(&paths, &["cli:codex@gpt-5.6-terra".to_string()])
            .unwrap_err();
        assert!(
            err.to_string().contains("paused"),
            "pinned ref must resolve unusable via the same normalized bucket ensure_usable reads: {err}"
        );
    }

    /// The `rate_limits.five_hour` JSON telemetry shape carries no rejection or failure
    /// of any kind — just a usage percentage. It may still drive the (cheap,
    /// auto-recovering) pause, but must not alone decide that *this* dispatch failure
    /// was a quota hit: an implementer that panics while the window happens to read 96%
    /// used would otherwise park a real defect on the quota gate.
    #[test]
    fn detect_and_pause_quota_does_not_route_on_bare_five_hour_telemetry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = r#"{"rate_limits":{"five_hour":{"used_percentage":96.5}}}"#;
        assert!(
            !detect_and_pause_quota(&paths, "cli:claude", log, None, None, false),
            "bare usage telemetry with no rejection must not route to Phase::Quota"
        );
        let store = crate::quota::QuotaStore::load(&paths).unwrap();
        assert!(
            !store.is_usable("cli:claude"),
            "the telemetry may still drive the harmless, auto-recovering pause"
        );
    }

    /// The stated-reset scan ("resets " + a limit phrase, anywhere in the tail log)
    /// used to be part of the routing bool: it is not line-scoped and requires no
    /// rejection word, so an ordinary defect whose log happens to mention both — a
    /// doc comment being edited, a test description — misrouted onto `Phase::Quota`.
    /// Both logs below would have matched the old disjunct; neither is a line-scoped
    /// rejection, so neither may route today.
    #[test]
    fn detect_and_pause_quota_ignores_a_stray_resets_mention_without_a_rejection() {
        const FIXTURES: [&str; 2] = [
            "implementer: editing src/quota.rs\n\
             /// Parses \"resets 12am (America/New_York)\" into a cooldown.\n\
             scanning for rate limit needles\n\
             thread 'main' panicked at src/quota.rs:210: index out of bounds\n",
            "reviewer: the token bucket rate limit test resets between cases\n\
             thread 'main' panicked at src/main.rs:42\n",
        ];
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        for (i, log) in FIXTURES.iter().enumerate() {
            assert!(
                !detect_and_pause_quota(&paths, "cli:claude", log, None, None, false),
                "FIXTURES[{i}] must not route"
            );
        }
    }

    /// Recovery must fire only for a slot that actually produced something. A slot that
    /// exited clean having written nothing is a genuine failure and still fails.
    #[test]
    fn slot_has_work_needs_a_dirty_tree_or_a_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        };
        if std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            return;
        }
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);

        assert!(
            !slot_has_work(&repo, Some(&base)),
            "an untouched worktree is not recoverable work"
        );

        std::fs::write(repo.join("b.txt"), "new").unwrap();
        assert!(slot_has_work(&repo, Some(&base)), "untracked work counts");

        git(&["add", "."]);
        git(&["commit", "-q", "-m", "slot work"]);
        assert!(
            slot_has_work(&repo, Some(&base)),
            "committed work past the base counts"
        );
        assert!(
            !slot_has_work(&repo, None),
            "with no recorded base, only a dirty tree can be judged"
        );
    }

    /// A dispatch's outcome fields describe *that* dispatch. Carried into the next one
    /// they produce the corpus's two artifacts: a `running` slot still holding the prior
    /// round's `exit_code: 0`, and (worse, because it looks final) a `done` slot still
    /// naming `error: "killed by signal 9 (SIGKILL)"` from the attempt before it.
    #[test]
    fn redispatch_clears_the_previous_dispatch_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let cfg = Config::default();
        let mut state = RunState::new(
            "r-reset",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.dry_run = true;
        state.slots.push(init_slot_model(
            "impl",
            "cli:claude",
            SlotRole::Implementer,
            None,
        ));
        state.save(&paths).unwrap();
        let job = SlotJob {
            slot_id: "impl".into(),
            provider: "cli:claude".into(),
            role: SlotRole::Implementer,
            template: "implementer".into(),
            extra_vars: HashMap::new(),
            expected_artifact: None,
            model: None,
        };

        // Round 1 finishes, then dies the way a killed dispatch does.
        run_slot(&mut state, &paths, &cfg, &job).unwrap();
        {
            let s = state.slot_mut("impl").unwrap();
            assert_eq!(s.status, SlotStatus::Done);
            s.pid = Some(4242);
            s.signal = Some(9);
            s.error = Some("killed by signal 9 (SIGKILL)".into());
            s.usage = Some(SlotUsage {
                slot_id: "impl".into(),
                provider: "cli:claude".into(),
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                context_tokens: 3,
                billed_tokens: 3,
                tools: 0,
                model: None,
                cost_usd: None,
                subagent_stats: None,
                model_usage: Default::default(),
            });
        }

        // Round 2, observed at the moment it goes Running.
        state.round = 2;
        prepare_slot_execution(&mut state, &paths, &cfg, &job).unwrap();
        let s = state.slot_mut("impl").unwrap();
        assert_eq!(s.status, SlotStatus::Running);
        assert_eq!(s.round, 2);
        assert_eq!(s.exit_code, None, "a running slot cannot have an exit code");
        assert_eq!(s.signal, None);
        assert_eq!(s.pid, None);
        assert_eq!(s.error, None);
        assert!(
            s.usage.is_none(),
            "usage describes one dispatch, not the slot"
        );

        // And the reset survives to the next terminal status: no stale error on `done`.
        run_slot(&mut state, &paths, &cfg, &job).unwrap();
        let s = state.slot_mut("impl").unwrap();
        assert_eq!(s.status, SlotStatus::Done);
        assert_eq!(s.error, None, "a done slot must not carry a prior failure");
        assert_eq!(s.signal, None);
    }

    /// `prepare_slot_execution`'s own `quota_hit = false` reset sits after its fallible
    /// steps (template render, prompt write, provider parse), so a slot re-dispatched
    /// after a prior round's real quota hit that then fails one of those *this* round
    /// must not carry the stale `true` into `mark_slot_failed` — that would route a
    /// template bug onto `Phase::Quota` instead of `Phase::Failed`. `run_slots_parallel`
    /// (not `run_slot`) is the caller responsible for the reset in that arm.
    #[test]
    fn run_slots_parallel_clears_a_stale_quota_hit_when_prepare_itself_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let cfg = Config::default();
        let mut state = RunState::new(
            "r-stale-quota",
            crate::cli::WorkflowKind::Review,
            tmp.path().to_path_buf(),
        );
        // Not dry-run and >1 job: the branch that reaches `prepare_slot_execution`
        // directly rather than falling back to sequential `run_slot`.
        state.dry_run = false;
        for id in ["r1", "r2"] {
            let mut slot = init_slot_model(id, "cli:claude", SlotRole::Reviewer, None);
            slot.quota_hit = true;
            state.slots.push(slot);
        }
        state.save(&paths).unwrap();
        let jobs: Vec<SlotJob> = ["r1", "r2"]
            .iter()
            .map(|id| SlotJob {
                slot_id: (*id).into(),
                provider: "cli:claude".into(),
                role: SlotRole::Reviewer,
                // Unknown template: `templates::render` errors before any process is
                // spawned, so this never touches the network/PATH.
                template: "does-not-exist".into(),
                extra_vars: HashMap::new(),
                expected_artifact: None,
                model: None,
            })
            .collect();

        run_slots_parallel(&mut state, &paths, &cfg, &jobs).unwrap();

        for id in ["r1", "r2"] {
            let s = state.slot_mut(id).unwrap();
            assert_eq!(s.status, SlotStatus::Failed);
            assert!(
                !s.quota_hit,
                "a template-render failure must not carry a stale quota_hit from a prior round"
            );
        }
    }

    #[test]
    fn role_assignments_report_what_each_role_drew() {
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/tmp/x"),
        );
        // The pool lists a provider no role ever draws — the bug this replaces.
        state.providers = vec!["cli:grok".into(), "cli:codex".into()];
        state.slots.push(init_slot_model(
            "impl-claude",
            "cli:claude",
            SlotRole::Implementer,
            Some("sonnet".into()),
        ));
        state.slots.push(init_slot_model(
            "rev-a",
            "cli:grok",
            SlotRole::Reviewer,
            None,
        ));
        state.slots.push(init_slot_model(
            "rev-b",
            "cli:claude@opus",
            SlotRole::Reviewer,
            None,
        ));

        let roles = role_assignments(&state);
        assert_eq!(
            roles,
            vec![
                "implementer=cli:claude@sonnet".to_string(),
                "reviewer=cli:grok+cli:claude@opus".to_string(),
            ]
        );
        assert!(
            !roles.iter().any(|r| r.contains("codex")),
            "a pooled provider no role drew must not be reported as running the work"
        );
    }

    #[test]
    fn provider_is_agy_recognizes_forms() {
        // This gate decides whether telemetry recovery + statusline install run at all.
        assert!(provider_is_agy("cli:agy"));
        assert!(provider_is_agy("cli:agy@gemini-3.5-flash"));
        assert!(provider_is_agy("agy"));
        assert!(!provider_is_agy("cli:grok"));
        assert!(!provider_is_agy("cli:claude"));
        assert!(!provider_is_agy("api:google"));
    }

    #[test]
    fn provider_is_codex_recognizes_forms() {
        // This gate decides whether the `--output-last-message` file is even
        // consulted during salvage.
        assert!(provider_is_codex("cli:codex"));
        assert!(provider_is_codex("cli:codex@openai/gpt-4o-mini"));
        assert!(provider_is_codex("codex"));
        assert!(!provider_is_codex("cli:muse"));
        assert!(!provider_is_codex("cli:claude"));
        assert!(!provider_is_codex("api:openai"));
    }

    fn codex_reviewer_job(slot_id: &str) -> SlotJob {
        SlotJob {
            slot_id: slot_id.into(),
            provider: "cli:codex".into(),
            role: SlotRole::Reviewer,
            template: "review".into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some(format!("review-{slot_id}.md")),
            model: None,
        }
    }

    #[test]
    fn salvage_prefers_codex_last_message_over_the_log_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let log_path = paths.log_file("r1", "rev");
        std::fs::write(&log_path, "log tail words\n").unwrap();
        // Written after the log, so it is fresh for this dispatch.
        let last = crate::providers::codex::last_message_path(&log_path);
        std::fs::write(&last, "provider final words\n").unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &codex_reviewer_job("rev"),
            &log_path,
            "interrupted",
            tmp.path(),
            4000,
        );
        let body = std::fs::read_to_string(paths.artifact("r1", "review-rev.md")).unwrap();
        assert!(
            body.contains("provider final words"),
            "fresh last message wins: {body}"
        );
        assert!(
            !body.contains("log tail words"),
            "log tail must not shadow it: {body}"
        );
        // Still a verdict-shaped salvage, not the raw chat message: a reviewer's
        // final message is salvage input, never the review itself.
        assert!(body.contains("request_changes"));
    }

    #[test]
    fn salvage_falls_back_to_the_log_tail_without_a_last_message_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let log_path = paths.log_file("r1", "rev");
        std::fs::write(&log_path, "log tail words\n").unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &codex_reviewer_job("rev"),
            &log_path,
            "interrupted",
            tmp.path(),
            4000,
        );
        let body = std::fs::read_to_string(paths.artifact("r1", "review-rev.md")).unwrap();
        assert!(body.contains("log tail words"), "fallback: {body}");
    }

    #[test]
    fn salvage_ignores_a_stale_last_message_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let last = crate::providers::codex::last_message_path(&paths.log_file("r1", "rev"));
        std::fs::write(&last, "older round words\n").unwrap();
        let log_path = paths.log_file("r1", "rev");
        std::fs::write(&log_path, "log tail words\n").unwrap();
        // Age the log past the 1s grace so the earlier round's file reads stale.
        let future = SystemTime::now() + Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&log_path)
            .unwrap()
            .set_modified(future)
            .unwrap();
        salvage_expected_artifact(
            &paths,
            "r1",
            &codex_reviewer_job("rev"),
            &log_path,
            "interrupted",
            tmp.path(),
            4000,
        );
        let body = std::fs::read_to_string(paths.artifact("r1", "review-rev.md")).unwrap();
        assert!(
            !body.contains("older round words"),
            "stale file must not be used: {body}"
        );
        assert!(body.contains("log tail words"), "fallback: {body}");
    }

    #[test]
    fn salvage_ignores_last_message_for_other_providers() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let log_path = paths.log_file("r1", "rev");
        std::fs::write(&log_path, "log tail words\n").unwrap();
        let last = crate::providers::codex::last_message_path(&log_path);
        std::fs::write(&last, "provider final words\n").unwrap();
        let mut job = codex_reviewer_job("rev");
        job.provider = "cli:muse".into();
        salvage_expected_artifact(
            &paths,
            "r1",
            &job,
            &log_path,
            "interrupted",
            tmp.path(),
            4000,
        );
        let body = std::fs::read_to_string(paths.artifact("r1", "review-rev.md")).unwrap();
        assert!(body.contains("log tail words"), "fallback: {body}");
        assert!(
            !body.contains("provider final words"),
            "another provider's dispatch never wrote that file: {body}"
        );
    }

    #[test]
    fn tmux_done_requires_process_exit() {
        // marker done + pane dead => success
        assert_eq!(
            tmux_outcome(MarkerState::Done, false, true),
            TmuxDecision::Ok
        );
        assert_eq!(
            tmux_outcome(MarkerState::Done, false, false),
            TmuxDecision::Ok
        );
        // marker done + pane alive + budget left => keep waiting (NOT success)
        assert_eq!(
            tmux_outcome(MarkerState::Done, true, true),
            TmuxDecision::Wait
        );
        // marker done + pane alive + budget exhausted => error
        assert_eq!(
            tmux_outcome(MarkerState::Done, true, false),
            TmuxDecision::DoneButAlive
        );
        // failed marker is always a failure
        assert_eq!(
            tmux_outcome(MarkerState::Failed, true, true),
            TmuxDecision::Failed
        );
        assert_eq!(
            tmux_outcome(MarkerState::Failed, false, false),
            TmuxDecision::Failed
        );
        // no marker yet => keep waiting while budget remains
        assert_eq!(
            tmux_outcome(MarkerState::None, false, true),
            TmuxDecision::Wait
        );
    }

    #[test]
    fn is_opencode_provider_recognizes_forms() {
        // This gate decides whether `run_tmux` bothers reconstructing stats from the
        // completed log at all, and whether `enrich_opencode_stats` runs descendant
        // recovery on top of them.
        assert!(is_opencode_provider("cli:opencode"));
        assert!(is_opencode_provider("cli:opencode@google/gemini-3.7-flash"));
        assert!(is_opencode_provider("opencode"));
        assert!(!is_opencode_provider("cli:grok"));
        assert!(!is_opencode_provider("cli:claude"));
        assert!(!is_opencode_provider("api:google"));
    }

    #[test]
    fn tmux_recovered_usage_reconstructs_opencode_spend_from_the_teed_log() {
        // Round-5 review: `run_tmux` returned `usage: None` unconditionally, so a valid
        // single-slot opencode run dispatched with `--backend tmux` reported no spend at
        // all — not even its own, let alone the child spend this feature exists to
        // recover. `run_tmux`'s pane tees the same `run --format json` stream headless
        // mode parses live straight to `log_path`; this reconstructs it after the fact.
        //
        // `OPENCODE_DB=:memory:` under the shared env lock keeps this test from
        // resolving (and read-write-opening) the developer's real opencode ledger via a
        // spawned `opencode db path` (round-6 review finding).
        let _env = crate::providers::opencode_telemetry::test_support::EnvGuard::acquire();
        std::env::set_var("OPENCODE_DB", ":memory:");
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let log_path = paths.log_file("r1", "impl");
        std::fs::write(
            &log_path,
            concat!(
                r#"{"type":"text","sessionID":"ses_1","part":{"id":"prt_t","type":"text","text":"DONE"}}"#,
                "\n",
                r#"{"type":"step_finish","sessionID":"ses_1","part":{"id":"prt_f","type":"step-finish","tokens":{"input":100,"output":20,"cache":{"read":5,"write":0}}}}"#,
                "\n",
                "EXIT:0\n",
            ),
        )
        .unwrap();
        let job = SlotJob {
            slot_id: "impl".into(),
            provider: "cli:opencode".into(),
            role: SlotRole::Implementer,
            template: "implementer".into(),
            extra_vars: HashMap::new(),
            expected_artifact: None,
            model: None,
        };

        let usage = tmux_recovered_usage(true, &paths, "r1", &job, &log_path)
            .expect("opencode tmux usage must be recovered, not None");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cache_read_tokens, 5);

        // Every other provider's pane runs a real interactive TUI, not this JSON stream,
        // so the gate must keep returning `None` for them rather than mis-parsing garbage.
        assert!(tmux_recovered_usage(false, &paths, "r1", &job, &log_path).is_none());
    }

    #[test]
    fn enrich_opencode_stats_logs_a_slot_note_when_the_ledger_cannot_be_read() {
        // Round-6 review: the only coverage of `enrich_opencode_stats` was via the tmux
        // helper's happy path; the note-to-event-log branch (a ledger that resolves but
        // fails to read) was never exercised end to end.
        let _env = crate::providers::opencode_telemetry::test_support::EnvGuard::acquire();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        let db = tmp.path().join("broken.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE not_session (id TEXT);")
            .unwrap();
        std::env::set_var("OPENCODE_DB", &db);

        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let log_path = paths.log_file("r1", "impl");
        let mut stats = process::StreamStats {
            session_id: Some("parent".to_string()),
            ..Default::default()
        };

        enrich_opencode_stats(&mut stats, "cli:opencode", &log_path, &paths, "r1", "impl");

        let events = crate::events::read_all(&paths, "r1").unwrap();
        let note = events
            .iter()
            .find(|e| e.slot.as_deref() == Some("impl") && e.message.is_some())
            .and_then(|e| e.message.clone())
            .expect("a broken ledger must land a slot_note in the run's event log");
        assert!(
            note.contains("descendant-subtree query failed to prepare"),
            "note was: {note}"
        );
    }

    #[test]
    fn describe_exit_is_actionable() {
        assert_eq!(describe_exit(None, Some(9)), "killed by signal 9 (SIGKILL)");
        assert_eq!(describe_exit(Some(137), None), "exit 137 (OOM-killed)");
        assert_eq!(describe_exit(Some(2), None), "exit 2");
    }

    #[test]
    fn tester_salvage_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        let log_path = paths.log_file("r1", "suite-x");
        std::fs::write(
            &log_path,
            "## Rules\n1. run the suite\n## Report format\n## Paths\n(prompt echo, not test output)\n",
        )
        .unwrap();
        let job = SlotJob {
            slot_id: "suite-x".into(),
            provider: "cli:claude".into(),
            role: SlotRole::Tester,
            template: "tester".into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some("suite.md".into()),
            model: None,
        };
        salvage_expected_artifact(
            &paths,
            "r1",
            &job,
            &log_path,
            "interrupted: timeout",
            tmp.path(),
            4000,
        );
        let suite = paths.artifact("r1", "suite.md");
        assert!(
            !suite.exists(),
            "tester salvage must leave suite.md absent, found {}",
            suite.display()
        );
    }

    /// A pre-existing artifact the dispatch leaves untouched must fail the gate,
    /// while the same slot writing the file passes. Staleness is relative, so
    /// the tests move `since` instead of the file's mtime: a `since` an hour in
    /// the future makes a just-written file stale, and the real `since`
    /// (captured before the write) makes it fresh.
    #[test]
    fn artifact_fresh_since_rejects_a_stale_file_and_accepts_a_fresh_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("review-x.md");
        assert!(
            !artifact_fresh_since(&path, SystemTime::now()),
            "a missing artifact is not fresh"
        );
        std::fs::write(&path, "").unwrap();
        assert!(
            !artifact_fresh_since(&path, SystemTime::now()),
            "an empty artifact is not fresh"
        );
        let dispatch_start = SystemTime::now();
        std::fs::write(&path, "## Verdict\nrequest_changes\n").unwrap();
        assert!(
            artifact_fresh_since(&path, dispatch_start),
            "a write landing after the dispatch start counts"
        );
        let next_round = SystemTime::now() + Duration::from_secs(3600);
        assert!(
            !artifact_fresh_since(&path, next_round),
            "the previous round's file must not pass the next round's gate"
        );
    }

    /// The 1s mtime grace is a boundary with two sides: a write exactly at the
    /// grace floor passes (no false-negative on a coarse filesystem), a write
    /// just older than it does not (no stale verdict admitted).
    #[test]
    fn artifact_fresh_since_grace_boundary_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("review-y.md");
        std::fs::write(&path, "verdict").unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(
            artifact_fresh_since(&path, mtime + Duration::from_secs(1)),
            "mtime exactly at the grace floor must pass"
        );
        assert!(
            !artifact_fresh_since(&path, mtime + Duration::from_secs(2)),
            "mtime a second past the grace must fail"
        );
    }

    /// The role gate on the session marker: an implementer reads the prior
    /// session id back, while a reviewer (and the other judging roles) never
    /// sees it even though the marker file is still on disk.
    #[test]
    fn prior_session_is_read_for_implementer_and_hidden_from_judging_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        markers::write_session_id(&paths, "run1", "slotA", "shell", "sess-1").unwrap();
        assert_eq!(
            prior_session_for_role(&paths, "run1", "slotA", "shell", SlotRole::Implementer)
                .as_deref(),
            Some("sess-1"),
            "the implementer still resumes its prior session"
        );
        for role in [SlotRole::Reviewer, SlotRole::Tester, SlotRole::PlanCritic] {
            assert_eq!(
                prior_session_for_role(&paths, "run1", "slotA", "shell", role),
                None,
                "{role:?} must dispatch cold even with a marker on disk"
            );
        }
        assert!(
            markers::read_session_id(&paths, "run1", "slotA", "shell").is_some(),
            "the marker itself is untouched; it is only not read"
        );
    }

    /// A native-CLI reviewer or tester that ran no tools produced no judgment.
    /// Every other role, any tool count above zero, and any api-backed slot
    /// (whose usage hardcodes `tools: 0`) must not trip this gate.
    #[test]
    fn zero_tool_gate_fires_only_for_native_judging_slots() {
        let err = judging_no_tool_error(SlotRole::Reviewer, "review-0", 0, false)
            .expect("a native reviewer with no tools must fail");
        assert!(
            err.contains("review-0") && err.contains("tools == 0"),
            "the error must name the slot and the cause: {err}"
        );
        assert!(
            judging_no_tool_error(SlotRole::Tester, "suite-x", 0, false).is_some(),
            "a native tester with no tools must fail too"
        );
        assert!(
            judging_no_tool_error(SlotRole::Reviewer, "review-0", 1, false).is_none(),
            "one tool call is a judgment"
        );
        assert!(
            judging_no_tool_error(SlotRole::Implementer, "impl", 0, false).is_none(),
            "the implementer is not a judging slot"
        );
        assert!(
            judging_no_tool_error(SlotRole::PlanCritic, "critic", 0, false).is_none(),
            "plan_critic dispatches cold but is not covered by the tool gate"
        );
        assert!(
            judging_no_tool_error(SlotRole::Reviewer, "review-0", 0, true).is_none(),
            "an api-backed reviewer hardcodes tools: 0 and must not fail here"
        );
    }

    /// The operator-grep contract: a no-judgment failure lands one `events.jsonl`
    /// slot-note line naming the slot and `tools == 0`, and the on-disk dispatch
    /// verdict reads failed with the same text as its reason.
    #[test]
    fn no_judgment_records_a_greppable_event_and_downgrades_the_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("run1").unwrap();
        let mut verdict = markers::DispatchVerdict {
            ok: true,
            round: 2,
            pid: None,
            exit_code: Some(0),
            signal: None,
            reason: None,
        };
        let error =
            judging_no_tool_error(SlotRole::Reviewer, "review-1", 0, false).expect("must fire");
        note_no_judgment(&paths, "run1", "review-1", &mut verdict, &error);
        assert!(!verdict.ok, "the dispatch verdict must read failed");
        assert_eq!(verdict.reason.as_deref(), Some(error.as_str()));
        let events = std::fs::read_to_string(crate::events::events_file(&paths, "run1")).unwrap();
        assert!(
            events.contains("review-1") && events.contains("tools == 0"),
            "one line must name the slot and the cause for later greps: {events}"
        );
    }
}
