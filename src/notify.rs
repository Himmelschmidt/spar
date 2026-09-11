//! External `@human` notifier — the opt-in sink beside the always-on TUI panel.
//!
//! spar ships no notifier of its own: the operator wires their own sink in the
//! `[notify]` config section, either a `command` spar shells out to or a `webhook`
//! URL it POSTs the message JSON to. With neither configured this is a no-op and the
//! TUI panel remains the only sink. Routing is fire-and-forget: a broken notifier
//! must never fail the `send` that triggered it.

use crate::bus::BusMessage;
use crate::config::{Config, NotifyConfig};
use crate::paths::SparPaths;
use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Cap on how long a notify command may run before it's killed. It runs on a
/// detached thread, but an unbounded wait would leak a thread and child process
/// per alert, so bound it defensively.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// In-flight `fire` threads. A one-shot CLI invocation (`spar plan`, `spar implement
/// --run` without `--detach`) frequently has nothing left to do after the very
/// transition that just fired an alert, so `main()` returning can outrace a bare
/// `thread::spawn` by orders of magnitude. `drain_before_exit` polls this to zero
/// (bounded) so fire-and-forget still means *delivered* for the common case, without
/// making `fire` itself synchronous.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Block until every in-flight `fire` thread has finished, or `COMMAND_TIMEOUT` (plus a
/// small margin) has passed. Call once, right before the process actually exits.
pub fn drain_before_exit() {
    let deadline = COMMAND_TIMEOUT + Duration::from_secs(1);
    let start = Instant::now();
    while PENDING.load(Ordering::SeqCst) > 0 && start.elapsed() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Where `fire` resolves the `[notify]` sink from. `route_human_alert` is a bus-scoped
/// path with no particular run behind it (a bare-agent `@human` send), so it reads the
/// live project config. `route_lifecycle` is scoped to one run and must honor O27:
/// `Config::for_run`, never the live file a concurrent edit could have changed under
/// an in-flight run.
enum CfgSource {
    Live,
    Run(String),
}

/// Shared tail of both notify entry points: hermetic guard, detached thread, config
/// resolution, dispatch. Fire-and-forget by construction — this returns `()`, so a
/// broken notifier can never fail the caller that triggered it.
fn fire(paths: &SparPaths, source: CfgSource, msg: BusMessage) {
    // Hermetic in tests and dry runs. The notify config is user-global (config.rs
    // reads `dirs::config_dir()`, isolated by neither SPAR_HOME nor project_root), so
    // on a machine whose global spar config has a `[notify]` sink, every Blocked/@human
    // send in a unit test would otherwise run that command or POST that webhook.
    if cfg!(test) || crate::util::env_truthy("SPAR_DRY_RUN") {
        return;
    }
    // Detached so neither the command wait nor the webhook can stall the caller.
    // `route_human_alert` runs on the bus hot path (tick_acks -> bus deliver -> a
    // Claude Stop hook) and `route_lifecycle` runs inside `RunState::save`, which is
    // called from every orchestrator hot path — neither may block on a hung command or
    // a black-holed webhook.
    let root = paths.project_root.clone();
    let paths = paths.clone();
    PENDING.fetch_add(1, Ordering::SeqCst);
    std::thread::spawn(move || {
        let cfg = match &source {
            CfgSource::Live => Config::load(&root).map(|c| c.notify),
            CfgSource::Run(id) => Config::for_run(&paths, id).map(|c| c.notify),
        };
        let cfg = match cfg {
            Ok(c) => c,
            Err(e) => {
                eprintln!("notify: config load failed: {e:#}");
                PENDING.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        };
        if let Err(e) = dispatch(&cfg, &msg) {
            eprintln!("notify: {e:#}");
        }
        PENDING.fetch_sub(1, Ordering::SeqCst);
    });
}

/// Route a human alert to the configured external notifier, if any. Errors are
/// logged to stderr and swallowed so bus delivery is never blocked by a bad sink.
pub fn route_human_alert(paths: &SparPaths, msg: &BusMessage) {
    fire(paths, CfgSource::Live, msg.clone());
}

/// Which lifecycle event, if any, a phase transition represents. `None` means silent.
///
/// Deliberately narrow: `Done` is silence (success needs no push — the push at the end
/// of a *good* run is the ship gate, `AwaitingShipConfirm`, already covered below as a
/// gate). `PlanRejected` and `Stopped` are silence (the operator did those; telling
/// them what they just did is noise). `abandoned` is not classified here at all — it
/// is structurally unobservable from a `save`, since abandonment is defined as the
/// *absence* of a further save. That event belongs to the daemon (feature 003 phase D),
/// which is the one thing that can notice a save that never came.
fn classify(phase: crate::state::Phase) -> Option<&'static str> {
    if phase.is_gate() {
        return Some("gate");
    }
    match phase {
        crate::state::Phase::Stuck | crate::state::Phase::Escalated => Some("stuck"),
        crate::state::Phase::Quota => Some("quota"),
        crate::state::Phase::Failed => Some("failed"),
        _ => None,
    }
}

/// The exact command that resolves this phase, named in the alert body so the operator
/// never has to look it up.
pub(crate) fn next_command(state: &crate::state::RunState) -> String {
    let id = &state.id;
    match state.phase {
        crate::state::Phase::AwaitingPlanApproval => format!("spar approve {id}"),
        crate::state::Phase::AwaitingWinnerConfirm => format!("spar confirm {id}"),
        crate::state::Phase::AwaitingReconcile => format!("spar reconcile {id}"),
        crate::state::Phase::AwaitingShipConfirm => format!("spar ship {id} --confirm"),
        crate::state::Phase::AwaitingRoundExtension => {
            format!("spar implement --run {id} --max-rounds <N>")
        }
        _ => format!("spar resume {id}"),
    }
}

/// Pure message construction, split out from [`route_lifecycle`] so the shape of a
/// lifecycle alert is testable without touching a thread, a config file or a process.
/// `None` when `state.phase` is not one `classify` recognises.
fn lifecycle_message(
    state: &crate::state::RunState,
    prev_phase: Option<crate::state::Phase>,
) -> Option<BusMessage> {
    let event = classify(state.phase)?;
    let kind = if event == "failed" {
        crate::bus::MsgKind::Status
    } else {
        crate::bus::MsgKind::Blocked
    };
    let task_preview: String = state
        .task
        .as_deref()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    let subject = format!("{event}: {:?}", state.phase);
    let body = format!(
        "run {} in {}\ntask: {task_preview}\nphase: {:?}\nnext: {}",
        state.id,
        state.project_root.display(),
        state.phase,
        next_command(state)
    );
    let mut meta = std::collections::HashMap::new();
    meta.insert("event".to_string(), event.to_string());
    meta.insert("phase".to_string(), format!("{:?}", state.phase));
    if let Some(p) = prev_phase {
        meta.insert("prev_phase".to_string(), format!("{p:?}"));
    }
    meta.insert("run".to_string(), state.id.clone());
    meta.insert("round".to_string(), state.round.to_string());
    if let Some(code) = state.status_exit_code() {
        meta.insert("exit_code".to_string(), code.to_string());
    }
    meta.insert(
        "project_root".to_string(),
        state.project_root.display().to_string(),
    );

    Some(BusMessage {
        id: crate::bus::new_id(),
        ts: chrono::Utc::now(),
        from: "spar".to_string(),
        to: crate::bus::HUMAN.to_string(),
        kind,
        body,
        run: Some(state.id.clone()),
        subject: Some(subject),
        refs: crate::bus::MsgRefs::default(),
        requires_ack: false,
        meta,
    })
}

/// Fire a lifecycle notification from the one choke point that sees every phase
/// transition, `RunState::save`. Covers gate / stuck / quota / terminal failure;
/// silence means healthy. Resolves the sink through `Config::for_run` (O27): a
/// `[notify]` block added to `spar.toml` after this run was created will not fire for
/// it until `implement --run <id> --reload-config` re-freezes its snapshot.
pub fn route_lifecycle(
    paths: &SparPaths,
    state: &crate::state::RunState,
    prev_phase: Option<crate::state::Phase>,
) {
    let Some(msg) = lifecycle_message(state, prev_phase) else {
        return;
    };
    fire(paths, CfgSource::Run(state.id.clone()), msg);
}

/// The one lifecycle event `route_lifecycle` structurally cannot see: abandonment is
/// the *absence* of a further save, not a transition. Only `spar daemon` can notice a
/// save that never came, so it calls this directly rather than through `save`.
pub fn route_abandoned(paths: &SparPaths, state: &crate::state::RunState) {
    let task_preview: String = state
        .task
        .as_deref()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    let body = format!(
        "run {} in {}\ntask: {task_preview}\nphase: {:?}\nno live orchestrator; resume with: spar resume {}",
        state.id,
        state.project_root.display(),
        state.phase,
        state.id
    );
    let mut meta = std::collections::HashMap::new();
    meta.insert("event".to_string(), "abandoned".to_string());
    meta.insert("phase".to_string(), format!("{:?}", state.phase));
    meta.insert("run".to_string(), state.id.clone());
    meta.insert("round".to_string(), state.round.to_string());
    meta.insert(
        "project_root".to_string(),
        state.project_root.display().to_string(),
    );
    let msg = BusMessage {
        id: crate::bus::new_id(),
        ts: chrono::Utc::now(),
        from: "spar".to_string(),
        to: crate::bus::HUMAN.to_string(),
        kind: crate::bus::MsgKind::Blocked,
        body,
        run: Some(state.id.clone()),
        subject: Some(format!("abandoned: {:?}", state.phase)),
        refs: crate::bus::MsgRefs::default(),
        requires_ack: false,
        meta,
    };
    fire(paths, CfgSource::Run(state.id.clone()), msg);
}

/// Fire whichever sinks the operator configured. Public for a direct notifier check.
pub fn dispatch(cfg: &NotifyConfig, msg: &BusMessage) -> Result<()> {
    if let Some(cmd) = cfg.command.as_deref().filter(|s| !s.is_empty()) {
        fire_command(cmd, msg)?;
    }
    if let Some(url) = cfg.webhook.as_deref().filter(|s| !s.is_empty()) {
        fire_webhook(url, msg)?;
    }
    Ok(())
}

/// One-line human summary passed to the command on argv (`$1`).
fn summary(msg: &BusMessage) -> String {
    let body: String = msg.body.chars().take(200).collect();
    format!("[{:?}] {} → {}: {}", msg.kind, msg.from, msg.to, body)
}

/// Run the operator command via `sh -c`, with the summary as `$1` and the full
/// message JSON on stdin. Waits (bounded by `COMMAND_TIMEOUT`) so a manual
/// echo-config check is observable without a hung notifier leaking forever.
fn fire_command(command: &str, msg: &BusMessage) -> Result<()> {
    let json = serde_json::to_string(msg)?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .arg("spar-notify")
        .arg(summary(msg))
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn notify command: {command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json.as_bytes()).ok();
    }
    let start = Instant::now();
    loop {
        match child.try_wait().context("wait on notify command")? {
            Some(status) => {
                if !status.success() {
                    anyhow::bail!("notify command exited with {status}");
                }
                return Ok(());
            }
            None => {
                if start.elapsed() >= COMMAND_TIMEOUT {
                    child.kill().ok();
                    child.wait().ok();
                    anyhow::bail!("notify command timed out after {COMMAND_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn fire_webhook(url: &str, msg: &BusMessage) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let resp = agent
        .post(url)
        .header("Content-Type", "application/json")
        .send_json(msg)
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("notify webhook {url} status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::WorkflowKind;
    use crate::state::{Phase, RunState};
    use tempfile::tempdir;

    fn state_at(phase: Phase) -> RunState {
        let mut s = RunState::new("r1", WorkflowKind::Loop, std::path::PathBuf::from("/proj"));
        s.task = Some("do the thing".to_string());
        s.phase = phase;
        s
    }

    #[test]
    fn gates_classify_as_gate() {
        for phase in [
            Phase::AwaitingPlanApproval,
            Phase::AwaitingWinnerConfirm,
            Phase::AwaitingReconcile,
            Phase::AwaitingShipConfirm,
            Phase::AwaitingRoundExtension,
        ] {
            assert_eq!(classify(phase), Some("gate"), "{phase:?}");
        }
    }

    #[test]
    fn stuck_escalated_quota_failed_classify_distinctly() {
        assert_eq!(classify(Phase::Stuck), Some("stuck"));
        assert_eq!(classify(Phase::Escalated), Some("stuck"));
        assert_eq!(classify(Phase::Quota), Some("quota"));
        assert_eq!(classify(Phase::Failed), Some("failed"));
    }

    /// `Done` is silence (the push at the end of a good run is the ship gate).
    /// `PlanRejected` / `Stopped` are silence (the operator did those).
    #[test]
    fn done_plan_rejected_and_stopped_are_silent() {
        for phase in [Phase::Done, Phase::PlanRejected, Phase::Stopped] {
            assert_eq!(classify(phase), None, "{phase:?}");
        }
    }

    #[test]
    fn in_flight_phases_are_silent() {
        for phase in [
            Phase::Init,
            Phase::Dispatch,
            Phase::Review,
            Phase::Suite,
            Phase::Shipping,
        ] {
            assert_eq!(classify(phase), None, "{phase:?}");
        }
    }

    #[test]
    fn a_gate_message_is_blocked_kind_and_names_the_resolving_command() {
        let state = state_at(Phase::AwaitingPlanApproval);
        let msg = lifecycle_message(&state, Some(Phase::PlanReady)).expect("gate fires");
        assert_eq!(msg.kind, crate::bus::MsgKind::Blocked);
        assert_eq!(msg.to, crate::bus::HUMAN);
        assert_eq!(msg.run.as_deref(), Some("r1"));
        assert!(msg.body.contains("spar approve r1"));
        assert!(msg.body.contains("do the thing"));
        assert_eq!(msg.meta.get("event").map(String::as_str), Some("gate"));
        assert_eq!(
            msg.meta.get("phase").map(String::as_str),
            Some("AwaitingPlanApproval")
        );
        assert_eq!(
            msg.meta.get("prev_phase").map(String::as_str),
            Some("PlanReady")
        );
    }

    #[test]
    fn a_failed_message_is_status_kind() {
        let state = state_at(Phase::Failed);
        let msg = lifecycle_message(&state, None).expect("failed fires");
        assert_eq!(msg.kind, crate::bus::MsgKind::Status);
        assert_eq!(msg.meta.get("event").map(String::as_str), Some("failed"));
        assert!(msg.body.contains("spar resume r1"));
    }

    #[test]
    fn a_silent_phase_builds_no_message() {
        let state = state_at(Phase::Done);
        assert!(lifecycle_message(&state, Some(Phase::Shipping)).is_none());
    }

    #[test]
    fn ship_confirm_gate_names_the_ship_command() {
        let state = state_at(Phase::AwaitingShipConfirm);
        let msg = lifecycle_message(&state, None).unwrap();
        assert!(msg.body.contains("spar ship r1 --confirm"));
    }

    /// `route_lifecycle` is compiled and run inside `cargo test`, so `cfg!(test)` is
    /// always true here. `fire`'s guard returns before ever spawning the delivery
    /// thread, so calling this against a project that does not even exist on disk
    /// (a `Config::for_run` / `Config::load` inside the thread would error loudly) must
    /// still return cleanly — the only way that happens is if the thread never runs.
    #[test]
    fn route_lifecycle_is_hermetic_under_cfg_test() {
        let paths = SparPaths::new(std::path::PathBuf::from("/no/such/project"));
        let state = state_at(Phase::AwaitingPlanApproval);
        route_lifecycle(&paths, &state, None);
    }

    /// `dispatch` itself (unlike `fire`) carries no hermetic guard — it is the thing a
    /// real, non-test notifier process actually runs. This is what proves the message
    /// `route_lifecycle` builds is usable by a real sink: a `[notify] command` receives
    /// the JSON body on stdin, which is written to `out.json` for this test to inspect.
    #[test]
    fn dispatch_hands_a_gate_message_to_a_real_command_sink() {
        let tmp = tempdir().unwrap();
        let out = tmp.path().join("out.json");
        let cfg = NotifyConfig {
            command: Some(format!("cat > {}", out.display())),
            webhook: None,
        };
        let state = state_at(Phase::AwaitingPlanApproval);
        let msg = lifecycle_message(&state, None).unwrap();
        dispatch(&cfg, &msg).unwrap();

        let written = std::fs::read_to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["meta"]["event"], "gate");
        assert_eq!(v["run"], "r1");
        assert_eq!(v["to"], crate::bus::HUMAN);
    }

    /// A notify command that exits non-zero surfaces as an `Err` from `dispatch` — but
    /// `route_lifecycle`'s return type is `()`, so nothing upstream (the `save` that
    /// triggered it) can ever observe or be failed by that error.
    #[test]
    fn a_broken_notify_command_errors_from_dispatch_but_route_lifecycle_cannot_propagate_it() {
        let cfg = NotifyConfig {
            command: Some("exit 1".to_string()),
            webhook: None,
        };
        let state = state_at(Phase::Quota);
        let msg = lifecycle_message(&state, None).unwrap();
        assert!(dispatch(&cfg, &msg).is_err());
        // route_lifecycle's signature is `fn(...) -> ()`: a broken sink structurally
        // cannot fail the caller. Calling it here (through the hermetic guard, so no
        // process actually spawns) documents that contract at the type level.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let () = route_lifecycle(&paths, &state, None);
    }
}
