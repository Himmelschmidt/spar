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

pub fn write_failed(paths: &SparPaths, run_id: &str, slot_id: &str, reason: &str) -> Result<()> {
    write_marker(paths, run_id, &format!("{slot_id}.failed"), reason)
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

        write_failed(&paths, "r1", "slot-a", "boom").unwrap();
        assert_eq!(
            terminal_marker(&paths, "r1", "slot-a"),
            Some(TerminalMarker::Failed)
        );
    }

    #[test]
    fn clear_slot_removes_stale_verdict_on_redispatch() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        write_failed(&paths, "r1", "impl", "died at print-timeout").unwrap();
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
