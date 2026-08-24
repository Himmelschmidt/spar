use crate::api;
use crate::cli::Backend;
use crate::config::Config;
use crate::markers;
use crate::paths::SparPaths;
use crate::process::{self, SpawnRequest};
use crate::provider_ref::ProviderRef;
use crate::providers::{self, SpawnOpts, TrustPolicy};
use crate::sandbox;
use crate::state::{RunState, SlotRole, SlotState, SlotStatus, SlotUsage};
use crate::templates;
use crate::tmux;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
        let timeout = timeout_for_role(cfg, prep.job.role);
        handles.push(std::thread::spawn(move || {
            let outcome = execute_prepared(&prep, isolation, backend_policy, timeout);
            (prep.job.slot_id.clone(), outcome, prep)
        }));
    }

    for h in handles {
        match h.join() {
            Ok((slot_id, outcome, prep)) => {
                apply_parallel_outcome(state, paths, &slot_id, outcome, &prep)?;
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
    _cfg: &Config,
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

fn execute_prepared(
    prep: &PreparedSlot,
    isolation: crate::config::IsolationMode,
    backend_policy: Backend,
    timeout: Duration,
) -> Result<SlotOutcome> {
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
            context_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
            tools: 0,
            model: usage.model.or(model),
        };
        return Ok(if ok {
            SlotOutcome {
                ok: true,
                pid: None,
                exit_code: Some(0),
                signal: None,
                error: None,
                usage: Some(slot_usage),
            }
        } else {
            SlotOutcome {
                ok: false,
                pid: None,
                exit_code: Some(1),
                signal: None,
                error: err,
                usage: Some(slot_usage),
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
    let cmd = adapter.build_headless(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let (program, args) = sandbox::maybe_wrap(isolation, &prep.cwd, &program, &args);
    let req = SpawnRequest {
        program,
        args,
        cwd: prep.cwd.clone(),
        log_path: prep.log_path.clone(),
        env: prep.env.clone(),
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
    let tick = || beat.tick();
    let mut res = process::run_captured(&req, Some(&sink), Some(&tick))?;
    let pid = load_pid(&pid_cell);
    enrich_agy_stats(
        &mut res.stats,
        &prep.job.provider,
        &prep.cwd,
        &prep.log_path,
        &prep.paths,
    );
    enrich_muse_stats(&mut res.stats, &prep.job.provider, &prep.log_path);
    let usage = usage_from_stream(&prep.job.slot_id, &prep.job.provider, &res.stats);
    if res.timed_out {
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: res.exit_code,
            signal: res.signal,
            error: Some(format!(
                "timeout after {}s ({})",
                timeout.as_secs(),
                timeout_label(prep.job.role)
            )),
            usage: Some(usage),
        });
    }
    if res.exit_code != Some(0) {
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: res.exit_code,
            signal: res.signal,
            error: Some(describe_exit(res.exit_code, res.signal)),
            usage: Some(usage),
        });
    }
    // A clean exit is not success on its own: a slot that produced no artifact (e.g. an
    // adapter that never received its prompt) must fail, not silently pass. Mirrors the
    // gate in the sequential `run_headless` path.
    if let Some(name) = &prep.job.expected_artifact {
        let path = prep.paths.artifact(&prep.run_id, name);
        let empty = !artifact_written(&path);
        if empty
            && !markers::wait_for_artifact(&prep.paths, &prep.run_id, name, Duration::from_secs(2))
                .unwrap_or(false)
            && !recover_artifact(&ArtifactRecovery {
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
            })
        {
            return Ok(SlotOutcome {
                ok: false,
                pid,
                exit_code: Some(0),
                signal: None,
                error: Some(format!("missing expected artifact {name}")),
                usage: Some(usage),
            });
        }
    }
    Ok(SlotOutcome {
        ok: true,
        pid,
        exit_code: Some(0),
        signal: None,
        error: None,
        usage: Some(usage),
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
    let req = SpawnRequest {
        program,
        args,
        cwd: r.cwd.to_path_buf(),
        // Its own log. `run_captured` opens with `File::create`, so reusing the slot's
        // would truncate the transcript of the turn that did all the work — which is both
        // the operator's only diagnosis and what `salvage_expected_artifact` tails when
        // recovery itself fails.
        log_path: recovery_log_path(r.log_path),
        env: r.env.to_vec(),
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
    if process::run_captured(&req, Some(&sink), Some(&tick)).is_err() {
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

fn markers_pid_path(paths: &SparPaths, run_id: &str, slot_id: &str) -> PathBuf {
    paths.markers_dir(run_id).join(format!("{slot_id}.pid"))
}

fn artifact_written(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 0
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

/// agy emits ~nothing to stdout, so the stream stats are all zero. Recover the real
/// tool/token/activity counts from agy's transcript + statusline sink and rewrite the
/// slot's stats sidecar so `stats.json` and the TUI reflect what actually happened.
/// Also drives a real agy quota cooldown from the payload's reset horizon (finding #3).
fn enrich_agy_stats(
    stats: &mut process::StreamStats,
    provider: &str,
    cwd: &Path,
    log_path: &Path,
    paths: &SparPaths,
) {
    if !provider_is_agy(provider) {
        return;
    }
    let Some(root) = providers::agy_telemetry::root() else {
        return;
    };
    let Some(t) = providers::agy_telemetry::collect(&root, cwd) else {
        return;
    };
    if t.tools > 0 {
        stats.tools = t.tools;
    }
    stats.tool_errors = stats.tool_errors.max(t.tool_errors);
    if t.input_tokens > 0 {
        stats.input_tokens = t.input_tokens;
    }
    if t.output_tokens > 0 {
        stats.output_tokens = t.output_tokens;
    }
    if t.cache_read_tokens > 0 {
        stats.cache_read_tokens = t.cache_read_tokens;
    }
    if t.context_tokens > 0 {
        stats.context_tokens = t.context_tokens;
    }
    if let Some(ts) = t.last_activity {
        stats.last_log_at = Some(ts.to_rfc3339());
    }
    let _ = stats.save(log_path);

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
        }
    }
}

fn usage_from_stream(slot_id: &str, provider: &str, s: &process::StreamStats) -> SlotUsage {
    SlotUsage {
        slot_id: slot_id.into(),
        provider: provider.into(),
        input_tokens: s.input_tokens,
        output_tokens: s.output_tokens,
        cache_read_tokens: s.cache_read_tokens,
        context_tokens: s.context_tokens,
        tools: s.tools,
        model: s.model.clone(),
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
            markers::write_done(paths, &state.id, slot_id)?;
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
            let err = result.error.unwrap_or_else(|| "failed".into());
            salvage_expected_artifact(paths, &state.id, &prep.job, &prep.log_path, &err);
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
            salvage_expected_artifact(paths, &state.id, &prep.job, &prep.log_path, &e.to_string());
            mark_slot_failed(state, paths, slot_id, &e.to_string(), None, None, None)?;
        }
    }
    let _ = crate::bus::heartbeat(paths, Some(&state.id), slot_id, "done");
    Ok(())
}

pub fn timeout_for_role(cfg: &Config, role: SlotRole) -> Duration {
    let secs = match role {
        SlotRole::Tester => cfg.suite.timeout_secs,
        SlotRole::TestAuthor => cfg.spec.timeout_secs,
        SlotRole::Reviewer => cfg.timeouts.review_secs(),
        _ => cfg.timeouts.slot_secs,
    };
    Duration::from_secs(secs)
}

/// On timeout/fail, keep any non-empty expected artifact; else salvage from the slot log.
pub fn salvage_expected_artifact(
    paths: &SparPaths,
    run_id: &str,
    job: &SlotJob,
    log_path: &Path,
    reason: &str,
) {
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
    let tail = process::tail_log(log_path, 6000);
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

    let timeout = timeout_for_role(cfg, job.role);

    if state.dry_run {
        return run_dry(state, paths, job, &cwd, &log_path, &prompt);
    }

    let presence_env = wire_slot_presence(state, paths, job, &cwd, &pref);

    let result = if pref.is_api() {
        match run_api(state, paths, job, &pref, &cwd, &log_path, &prompt, timeout) {
            Ok(r) => r,
            Err(e) => {
                salvage_expected_artifact(paths, &state.id, job, &log_path, &e.to_string());
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
                        salvage_expected_artifact(paths, &state.id, job, &log_path, &e.to_string());
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
                        salvage_expected_artifact(paths, &state.id, job, &log_path, &e.to_string());
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
        markers::write_done(paths, &state.id, &job.slot_id)?;
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
        let err = result.error.as_deref().unwrap_or("failed");
        salvage_expected_artifact(paths, &state.id, job, &log_path, err);
        markers::write_failed(paths, &state.id, &job.slot_id, err)?;
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
        let log_text = process::tail_log(&log_path, 8000);
        if let Some(hint) = crate::quota::QuotaStore::scrape_log_hint(&log_text) {
            let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
            store.pause_quota(&job.provider, hint);
            let _ = store.save(paths);
        }
        if let Some((name, until, hint)) = crate::quota::scrape_claude_rate_limits(&log_text) {
            let mut store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
            store.pause_quota_until(&name, until, hint);
            let _ = store.save(paths);
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

struct SlotOutcome {
    ok: bool,
    pid: Option<u32>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    error: Option<String>,
    usage: Option<SlotUsage>,
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
        }
    }
}

/// Config-key label for the timeout that governs a role, so a killed slot names its budget.
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

fn mark_slot_failed(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    err: &str,
    pid: Option<u32>,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> Result<()> {
    let _ = markers::write_failed(paths, &state.id, slot_id, err);
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
        context_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
        tools: 0,
        model: usage.model.or(model),
    };
    if ok {
        Ok(SlotOutcome {
            ok: true,
            pid: None,
            exit_code: Some(0),
            signal: None,
            error: None,
            usage: Some(slot_usage),
        })
    } else {
        Ok(SlotOutcome {
            ok: false,
            pid: None,
            exit_code: Some(1),
            signal: None,
            error: err,
            usage: Some(slot_usage),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_headless(
    state: &RunState,
    paths: &SparPaths,
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
    let cmd = adapter.build_headless(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let (program, args) = sandbox::maybe_wrap(state.isolation, cwd, &program, &args);

    let req = SpawnRequest {
        program,
        args,
        cwd: cwd.to_path_buf(),
        log_path: log_path.to_path_buf(),
        env: env.to_vec(),
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
    let tick = || beat.tick();
    let mut res = process::run_captured(&req, Some(&sink), Some(&tick))?;
    let pid = load_pid(&pid_cell);
    enrich_agy_stats(&mut res.stats, &job.provider, cwd, log_path, paths);
    enrich_muse_stats(&mut res.stats, &job.provider, log_path);
    let usage = usage_from_stream(&job.slot_id, &job.provider, &res.stats);
    if res.timed_out {
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: res.exit_code,
            signal: res.signal,
            error: Some(format!(
                "timeout after {}s ({})",
                timeout.as_secs(),
                timeout_label(job.role)
            )),
            usage: Some(usage),
        });
    }
    let code = res.exit_code;
    if code != Some(0) {
        return Ok(SlotOutcome {
            ok: false,
            pid,
            exit_code: code,
            signal: res.signal,
            error: Some(describe_exit(code, res.signal)),
            usage: Some(usage),
        });
    }
    if let Some(name) = &job.expected_artifact {
        let path = paths.artifact(&state.id, name);
        if !artifact_written(&path) {
            // short grace for late writers
            let found = markers::wait_for_artifact(paths, &state.id, name, Duration::from_secs(2))
                .unwrap_or(false);
            let recovered = !found
                && recover_artifact(&ArtifactRecovery {
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
            if !found && !recovered {
                return Ok(SlotOutcome {
                    ok: false,
                    pid,
                    exit_code: Some(0),
                    signal: None,
                    error: Some(format!("missing expected artifact {name}")),
                    usage: Some(usage),
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

    // `done` means the agent's own process has exited — not just that it wrote its marker.
    let done = format!("{}.done", job.slot_id);
    let failed = format!("{}.failed", job.slot_id);
    let start = std::time::Instant::now();
    let mut pane_pid: Option<u32> = None;
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
                pane_pid = Some(p);
                let _ = markers::write_pid(
                    paths,
                    &state.id,
                    &job.slot_id,
                    process::PidToken::capture(p),
                );
            }
        }
        let pane_alive = match pane_pid {
            Some(p) => process::pid_alive(p),
            None => tmux::pane_pid(&session, &job.slot_id).is_some(),
        };
        let budget_left = start.elapsed() < timeout;
        match tmux_outcome(marker, pane_alive, budget_left) {
            TmuxDecision::Ok => {
                return Ok(SlotOutcome {
                    ok: true,
                    pid: pane_pid,
                    exit_code: Some(0),
                    signal: None,
                    error: None,
                    usage: None,
                })
            }
            TmuxDecision::Failed => {
                return Ok(SlotOutcome {
                    ok: false,
                    pid: pane_pid,
                    exit_code: Some(1),
                    signal: None,
                    error: Some("marker failed".into()),
                    usage: None,
                })
            }
            TmuxDecision::DoneButAlive => {
                return Ok(SlotOutcome {
                    ok: false,
                    pid: pane_pid,
                    exit_code: None,
                    signal: None,
                    error: Some("agent reported done but its process is still running".into()),
                    usage: None,
                })
            }
            TmuxDecision::Wait => {
                if !budget_left {
                    // Never success-on-timeout-alone (plan completion contract).
                    return Ok(SlotOutcome::err("tmux marker wait timed out"));
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
    }
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
        "amendment": state.amendment,
        "dry_run": state.dry_run,
        "slots": state.slots,
        "providers": state.providers,
        "gates": state.gates,
        "error": state.error,
        "project_root": state.project_root,
        // `providers` is the pool; this is what each role actually drew from it.
        "roles": role_assignments(state),
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
        let state = RunState::load(paths, run_id)?;
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
        salvage_expected_artifact(&paths, "r1", &job, &log_path, "interrupted: timeout");
        let suite = paths.artifact("r1", "suite.md");
        assert!(
            !suite.exists(),
            "tester salvage must leave suite.md absent, found {}",
            suite.display()
        );
    }
}
