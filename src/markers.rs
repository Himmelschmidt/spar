use crate::paths::SparPaths;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};

pub fn write_marker(paths: &SparPaths, run_id: &str, name: &str, body: &str) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let p = paths.marker(run_id, name);
    std::fs::write(&p, body).with_context(|| format!("write marker {}", p.display()))?;
    Ok(())
}

pub fn marker_exists(paths: &SparPaths, run_id: &str, name: &str) -> bool {
    paths.marker(run_id, name).is_file()
}

/// A slot's on-disk verdict. Markers are written by the slot itself as it finishes,
/// so they outlive an orchestrator that died before it could update `state.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalMarker {
    Done,
    Failed,
}

/// Ground truth for a finished slot. `.failed` wins over `.done`: a slot that somehow
/// left both did not finish cleanly.
pub fn terminal_marker(paths: &SparPaths, run_id: &str, slot_id: &str) -> Option<TerminalMarker> {
    if marker_exists(paths, run_id, &format!("{slot_id}.failed")) {
        return Some(TerminalMarker::Failed);
    }
    marker_exists(paths, run_id, &format!("{slot_id}.done")).then_some(TerminalMarker::Done)
}

/// Remove a slot's terminal markers so a re-dispatched slot isn't reported with a prior
/// attempt's verdict. Called at (re-)dispatch, before the slot runs: a stale `<slot>.failed`
/// otherwise outranks the live process (reconciliation keys off markers) and `status`/TUI
/// show the working slot as `failed` for its entire life. Best-effort — a missing marker is
/// the expected case on a first dispatch.
///
/// The `.pid` marker is deliberately left in place: the spawn sink overwrites it, and until
/// then the prior token (which carries a start-time) still lets `stop` correctly see the old
/// process as dead. Removing it would expose the start-time-less `slot.pid` fallback to a
/// racing `stop` and defeat its recycled-pid protection.
pub fn clear_slot(paths: &SparPaths, run_id: &str, slot_id: &str) {
    for suffix in ["done", "failed"] {
        let _ = std::fs::remove_file(paths.marker(run_id, &format!("{slot_id}.{suffix}")));
    }
}

pub fn write_done(paths: &SparPaths, run_id: &str, slot_id: &str) -> Result<()> {
    write_marker(paths, run_id, &format!("{slot_id}.done"), "ok\n")
}

/// What spar itself saw when a dispatch's child was waited on.
///
/// Written as the terminal marker's body so a spar-authored verdict can be told apart
/// from one an agent wrote by hand into the same path. `terminal_marker` deliberately
/// still accepts either: an agent's marker is the only terminal record a slot has when
/// its orchestrator never came back, and refusing it would lose that.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DispatchVerdict {
    pub ok: bool,
    pub round: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Record a dispatch's verdict on disk.
///
/// Called the moment the child is reaped, before the artifact and recovery gates that
/// can still downgrade a clean exit. `state.json` is only written by an orchestrator
/// that survives its whole dispatch, while this survives one that merely got its child
/// back. The gates re-call it with `ok: false` to upgrade `.done` to `.failed`.
///
/// **Purely additive, like the `write_done` it replaced.** It must never remove the
/// other marker, and `ok: true` least of all. Six templates tell the agent to write its
/// own `<slot>.failed` with a reason, and a CLI agent exits 0 once its turn ends
/// whatever it concluded, so "agent declares failure, process exits clean" is the normal
/// case rather than a corner. Deleting `.failed` there would invert the precedence
/// `terminal_marker` documents and reconcile a self-declared failure to `done`. Leaving
/// both is what that precedence is for, and it also means there is never an instant with
/// no terminal marker at all.
pub fn write_dispatch_verdict(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    verdict: &DispatchVerdict,
) -> Result<()> {
    let body = serde_json::to_string(verdict).unwrap_or_default();
    let name = if verdict.ok { "done" } else { "failed" };
    write_marker(
        paths,
        run_id,
        &format!("{slot_id}.{name}"),
        &format!("{body}\n"),
    )
}

/// Record a running slot's pid (with its start-time identity) so an out-of-process
/// `spar status`/`stop` can observe it mid-run without risking a recycled pid.
pub fn write_pid(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    token: crate::process::PidToken,
) -> Result<()> {
    write_marker(paths, run_id, &format!("{slot_id}.pid"), &token.encode())
}

pub fn clear_pid(paths: &SparPaths, run_id: &str, slot_id: &str) {
    let _ = std::fs::remove_file(paths.marker(run_id, &format!("{slot_id}.pid")));
}

pub fn read_pid(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
) -> Option<crate::process::PidToken> {
    let p = paths.marker(run_id, &format!("{slot_id}.pid"));
    crate::process::PidToken::parse(&std::fs::read_to_string(p).ok()?)
}

/// Wait until an artifact file is non-empty.
#[allow(dead_code)]
pub fn wait_for_artifact(
    paths: &SparPaths,
    run_id: &str,
    name: &str,
    timeout: Duration,
) -> Result<bool> {
    let path = paths.artifact(run_id, name);
    let start = Instant::now();
    let poll = Duration::from_millis(200);
    loop {
        // A transient metadata error (e.g. mid-write) must not abort the wait — keep
        // polling until the deadline instead of failing the slot prematurely.
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 0 {
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn markers_roundtrip() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        write_done(&paths, "r1", "slot-a").unwrap();
        assert!(marker_exists(&paths, "r1", "slot-a.done"));
    }

    #[test]
    fn terminal_marker_reads_disk_and_prefers_failed() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert_eq!(terminal_marker(&paths, "r1", "slot-a"), None);

        write_done(&paths, "r1", "slot-a").unwrap();
        assert_eq!(
            terminal_marker(&paths, "r1", "slot-a"),
            Some(TerminalMarker::Done)
        );

        write_marker(&paths, "r1", "slot-a.failed", "boom").unwrap();
        assert_eq!(
            terminal_marker(&paths, "r1", "slot-a"),
            Some(TerminalMarker::Failed)
        );
    }

    #[test]
    fn dispatch_verdict_stamps_a_body_and_the_gate_downgrade_wins() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut v = DispatchVerdict {
            ok: true,
            round: 2,
            pid: Some(4242),
            exit_code: Some(0),
            signal: None,
            reason: None,
        };
        write_dispatch_verdict(&paths, "r1", "impl", &v).unwrap();
        assert_eq!(
            terminal_marker(&paths, "r1", "impl"),
            Some(TerminalMarker::Done)
        );
        let body = std::fs::read_to_string(paths.marker("r1", "impl.done")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(parsed["pid"], 4242);
        assert_eq!(parsed["round"], 2);

        // The artifact gate downgrades a clean exit. `.failed` wins from then on, and
        // there is no instant in between with no terminal marker at all.
        v.ok = false;
        v.reason = Some("missing expected artifact plan.md".into());
        write_dispatch_verdict(&paths, "r1", "impl", &v).unwrap();
        assert_eq!(
            terminal_marker(&paths, "r1", "impl"),
            Some(TerminalMarker::Failed)
        );
    }

    /// Six templates tell the agent to write its own `<slot>.failed` with a reason, and
    /// a CLI agent exits 0 when its turn ends regardless of what it concluded. spar's
    /// clean-exit stamp must not delete that verdict: doing so reconciles a slot that
    /// declared failure to `done`, inverting the precedence this module documents.
    #[test]
    fn a_clean_exit_never_overrides_an_agents_own_failed_marker() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        write_marker(&paths, "r1", "impl.failed", "could not build: no crate\n").unwrap();

        write_dispatch_verdict(
            &paths,
            "r1",
            "impl",
            &DispatchVerdict {
                ok: true,
                round: 1,
                pid: Some(7),
                exit_code: Some(0),
                signal: None,
                reason: None,
            },
        )
        .unwrap();

        assert!(marker_exists(&paths, "r1", "impl.failed"));
        assert_eq!(
            terminal_marker(&paths, "r1", "impl"),
            Some(TerminalMarker::Failed),
            "the agent's own verdict outranks its exit status"
        );
        assert_eq!(
            std::fs::read_to_string(paths.marker("r1", "impl.failed")).unwrap(),
            "could not build: no crate\n",
            "and its reason is preserved verbatim"
        );
    }

    #[test]
    fn clear_slot_removes_stale_verdict_on_redispatch() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        write_marker(&paths, "r1", "impl.failed", "died at print-timeout").unwrap();
        write_done(&paths, "r1", "impl").unwrap();
        write_pid(
            &paths,
            "r1",
            "impl",
            crate::process::PidToken::from_pid(4095415),
        )
        .unwrap();
        assert_eq!(
            terminal_marker(&paths, "r1", "impl"),
            Some(TerminalMarker::Failed)
        );

        clear_slot(&paths, "r1", "impl");

        // No stale verdict left: a re-dispatched Running slot reconciles as Running.
        assert_eq!(terminal_marker(&paths, "r1", "impl"), None);
        // The pid marker is intentionally preserved (start-time-carrying; stop-safe).
        assert!(marker_exists(&paths, "r1", "impl.pid"));
        // Clearing a slot with no markers (first dispatch) is a harmless no-op.
        clear_slot(&paths, "r1", "never-ran");
    }
}
