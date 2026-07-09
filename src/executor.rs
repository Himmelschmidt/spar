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

pub struct SlotJob {
    pub slot_id: String,
    pub provider: String,
    pub role: SlotRole,
    pub template: String,
    pub extra_vars: HashMap<String, String>,
    /// Expected primary artifact name under artifacts/
    pub expected_artifact: Option<String>,
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

    let pref = ProviderRef::parse(&job.provider);
    if let Some(s) = state.slot_mut(&job.slot_id) {
        s.status = SlotStatus::Running;
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
    let _ = crate::bus::heartbeat(paths, &state.id, &job.slot_id, "running");
    state.save(paths)?;

    let timeout = Duration::from_secs(cfg.timeouts.slot_secs);

    if state.dry_run {
        return run_dry(state, paths, job, &cwd, &log_path, &prompt);
    }

    let result = if pref.is_api() {
        match run_api(state, paths, job, &pref, &cwd, &log_path, &prompt, timeout) {
            Ok(r) => r,
            Err(e) => {
                mark_slot_failed(state, paths, &job.slot_id, &e.to_string())?;
                return Err(e);
            }
        }
    } else {
        match backend {
            Backend::Tmux => {
                match run_tmux(state, paths, job, &cwd, &log_path, &prompt_path, &prompt) {
                    Ok(r) => r,
                    Err(e) => {
                        mark_slot_failed(state, paths, &job.slot_id, &e.to_string())?;
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
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        mark_slot_failed(state, paths, &job.slot_id, &e.to_string())?;
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
            s.exit_code = Some(0);
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
        markers::write_failed(
            paths,
            &state.id,
            &job.slot_id,
            result.error.as_deref().unwrap_or("failed"),
        )?;
        if let Some(s) = state.slot_mut(&job.slot_id) {
            s.status = SlotStatus::Failed;
            s.error = result.error.clone();
            s.exit_code = result.exit_code;
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
        &state.id,
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
    exit_code: Option<i32>,
    error: Option<String>,
    usage: Option<SlotUsage>,
}

impl SlotOutcome {
    fn ok() -> Self {
        Self {
            ok: true,
            exit_code: Some(0),
            error: None,
            usage: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            exit_code: None,
            error: Some(msg.into()),
            usage: None,
        }
    }
}

fn mark_slot_failed(
    state: &mut RunState,
    paths: &SparPaths,
    slot_id: &str,
    err: &str,
) -> Result<()> {
    let _ = markers::write_failed(paths, &state.id, slot_id, err);
    if let Some(s) = state.slot_mut(slot_id) {
        s.status = SlotStatus::Failed;
        s.error = Some(err.into());
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
        SlotRole::Reviewer => {
            let force_rc = crate::util::env_truthy("SPAR_FORCE_REQUEST_CHANGES")
                || job.slot_id.contains("harsh")
                || job.extra_vars.contains_key("request_changes");
            let verdict = if force_rc {
                "request_changes"
            } else {
                "approve"
            };
            std::fs::write(
                paths.artifact(&state.id, &format!("review-{}.md", job.slot_id)),
                format!(
                    "## Verdict\n{verdict}\n\n## Findings\n- severity: minor — dry-run synthetic review from {}\n\n## Tests\nnot run (dry-run)\n",
                    job.provider
                ),
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
                &state.id,
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
    let (ok, err, usage) = api::run_api_slot(&api::runtime::ApiSlotRequest {
        provider_name: &pref.name,
        prompt,
        cwd,
        log_path,
        expected_artifact: expected.as_deref(),
        timeout,
        dry_run: false,
    })?;
    let slot_usage = SlotUsage {
        slot_id: job.slot_id.clone(),
        provider: pref.storage_key(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        model: usage.model,
    };
    if ok {
        Ok(SlotOutcome {
            ok: true,
            exit_code: Some(0),
            error: None,
            usage: Some(slot_usage),
        })
    } else {
        Ok(SlotOutcome {
            ok: false,
            exit_code: Some(1),
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
) -> Result<SlotOutcome> {
    let pref = ProviderRef::parse(&job.provider);
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
    };
    let cmd = adapter.build_headless(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let (program, args) = sandbox::maybe_wrap(state.isolation, cwd, &program, &args);

    let req = SpawnRequest {
        program,
        args,
        cwd: cwd.to_path_buf(),
        log_path: log_path.to_path_buf(),
        env: vec![],
        timeout,
    };
    let res = process::run_captured(&req)?;
    if res.timed_out {
        return Ok(SlotOutcome::err("timeout"));
    }
    let code = res.exit_code;
    if code != Some(0) {
        return Ok(SlotOutcome {
            ok: false,
            exit_code: code,
            error: Some(format!("exit {:?}", code)),
            usage: None,
        });
    }
    if let Some(name) = &job.expected_artifact {
        let path = paths.artifact(&state.id, name);
        if !path.is_file() || std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) == 0 {
            // short grace for late writers
            let found = markers::wait_for_artifact(paths, &state.id, name, Duration::from_secs(2))
                .unwrap_or(false);
            if !found {
                return Ok(SlotOutcome {
                    ok: false,
                    exit_code: Some(0),
                    error: Some(format!("missing expected artifact {name}")),
                    usage: None,
                });
            }
        }
    }
    Ok(SlotOutcome::ok())
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
    // timeout not used for full wait here — workflow waits on markers
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

    let pref = ProviderRef::parse(&job.provider);
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
    };
    // prefer interactive for tmux
    let cmd = adapter.build_interactive(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let shell = tmux::shell_wrap(&program, &args, log_path);
    tmux::spawn_window(&session, &job.slot_id, cwd, &shell)?;

    // Wait for marker with timeout
    let timeout = Duration::from_secs(30); // short for marker; real runs use wait command
    let done = format!("{}.done", job.slot_id);
    let failed = format!("{}.failed", job.slot_id);
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if markers::marker_exists(paths, &state.id, &done) {
            return Ok(SlotOutcome::ok());
        }
        if markers::marker_exists(paths, &state.id, &failed) {
            return Ok(SlotOutcome {
                ok: false,
                exit_code: Some(1),
                error: Some("marker failed".into()),
                usage: None,
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Never success-on-timeout-alone (plan completion contract).
    Ok(SlotOutcome::err("tmux marker wait timed out"))
}

pub fn init_slot(id: impl Into<String>, provider: impl Into<String>, role: SlotRole) -> SlotState {
    let provider = provider.into();
    let pref = ProviderRef::parse(&provider);
    SlotState {
        id: id.into(),
        provider,
        role,
        status: SlotStatus::Pending,
        backend: None,
        exec_backend: Some(pref.backend),
        cwd: None,
        log_path: None,
        error: None,
        pid: None,
        exit_code: None,
        artifact: None,
        usage: None,
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
        "dry_run": state.dry_run,
        "slots": state.slots,
        "providers": state.providers,
        "gates": state.gates,
        "error": state.error,
        "project_root": state.project_root,
        "parent_run": state.parent_run,
        "child_run": state.child_run,
        "usage": state.usage,
        "big": state.big,
        "autonomy": state.autonomy,
        // null while in-flight; only set at terminal/gate phases
        "exit_code": state.status_exit_code(),
    });
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

pub fn print_run_human(state: &RunState) {
    println!("run_id:  {}", state.id);
    println!("phase:   {:?}", state.phase);
    println!("workflow:{:?}", state.workflow);
    if let Some(t) = &state.task {
        println!("task:    {t}");
    }
    if !state.providers.is_empty() {
        println!("providers: {}", state.providers.join(", "));
    }
    if state.dry_run {
        println!("dry_run: true  (no git worktrees; agent processes stubbed only)");
    }
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
    loop {
        let state = RunState::load(paths, run_id)?;
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
