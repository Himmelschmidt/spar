use crate::paths::SparPaths;
use anyhow::{Context, Result};
use std::time::{Duration, Instant, SystemTime};

pub fn write_marker(paths: &SparPaths, run_id: &str, name: &str, body: &str) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let p = paths.marker(run_id, name);
    // Temp file plus rename: a reader polling for the marker never sees it half written.
    let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, body).with_context(|| format!("write marker {}", tmp.display()))?;
    std::fs::rename(&tmp, &p).with_context(|| format!("write marker {}", p.display()))?;
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
/// otherwise outranks the live process (reconciliation keys off markers) and `status` would
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

/// Record the native session/thread id a dispatch captured (e.g. codex's `thread.started`
/// id), so a later round of the *same* slot can resume it instead of a cold dispatch.
/// Unlike `.pid`, this is never cleared by `clear_slot`: it is meant to outlive the
/// dispatch that wrote it, across every round the slot id survives.
///
/// The marker is a record, not a discovery: on the assign path (adapters whose CLI
/// accepts a caller-supplied id) spar derives the id and writes this file *before*
/// the spawn; on the capture-only path it is written from the stream-captured stats
/// after the dispatch. Both paths end with the same file.
///
/// Scoped by `provider` (the adapter's bare name, e.g. `"codex"`), not just `slot_id`:
/// slot ids outlive a provider rotation (`try_rotate_implementer` / `try_rotate_reviewer_provider`
/// reuse the existing slot id under a new provider), and a session id captured under one
/// provider is meaningless to another — resuming codex with a muse session id is a hard,
/// immediate failure, not a degradation. One marker file per `(slot, provider)` means a
/// rotation away and back finds each provider's own thread right where it left it.
pub fn write_session_id(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    provider: &str,
    session_id: &str,
) -> Result<()> {
    write_marker(
        paths,
        run_id,
        &format!("{slot_id}.{provider}.session_id"),
        session_id,
    )
}

/// One `--urgent` bus message waiting to interrupt a slot's live turn. Written by
/// `spar bus send --urgent` before it kills the slot's process group, consumed
/// once by the orchestrator when the killed dispatch returns: the resume
/// prompt leads with these bodies, oldest first, ahead of the normal
/// continuation. One file per message so concurrent urgents do not overwrite
/// each other; the orchestrator drains and deletes them all together.
/// `target_pid`/`target_starttime` name the live dispatch the sender verified:
/// the take keeps only requests keyed to the dispatch that is actually
/// returning, so a stale file can never misclassify a later, unrelated kill.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UrgentInterrupt {
    pub msg_id: String,
    pub from: String,
    pub body: String,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub target_pid: u32,
    pub target_starttime: Option<u64>,
}

fn urgent_name(slot_id: &str, msg_id: &str) -> String {
    format!("{slot_id}.urgent.{msg_id}.json")
}

/// File an urgent interrupt for `slot_id`'s current turn. The sender kills the
/// slot's process group right after, so a request is never left pointing at a
/// turn the orchestrator already reaped: on wake it either resumes with these
/// bodies first or, when the turn ended on its own, drops them and lets the
/// message flow through the normal turn-boundary channels.
pub fn write_urgent_interrupt(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    urgent: &UrgentInterrupt,
) -> Result<()> {
    let body = serde_json::to_string(urgent)?;
    write_marker(paths, run_id, &urgent_name(slot_id, &urgent.msg_id), &body)
}

/// Drain every pending urgent interrupt for `slot_id`, oldest first, deleting
/// each file as it is read so a later dispatch never replays it. A torn write
/// the sender never finished renaming into place is simply not there yet.
/// Only requests keyed to `current` (the returning dispatch's pid token) are
/// kept: anything else targets a turn that is already over, so it is deleted
/// unread and the message flows through the normal turn-boundary channels.
/// With no current token there is no dispatch to attribute a request to, and
/// every file is stale by the same argument.
fn urgent_files(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
) -> Vec<(std::path::PathBuf, Option<UrgentInterrupt>)> {
    let prefix = format!("{slot_id}.urgent.");
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(paths.markers_dir(run_id)) else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        out.push((
            entry.path(),
            serde_json::from_str::<UrgentInterrupt>(&text).ok(),
        ));
    }
    out
}

fn urgent_matches(urgent: &UrgentInterrupt, token: &crate::process::PidToken) -> bool {
    token.pid == urgent.target_pid && token.starttime == urgent.target_starttime
}

pub fn take_urgent_interrupts(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    current: Option<&crate::process::PidToken>,
) -> Vec<UrgentInterrupt> {
    let mut out = Vec::new();
    for (path, parsed) in urgent_files(paths, run_id, slot_id) {
        let Some(urgent) = parsed else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let _ = std::fs::remove_file(&path);
        if current.is_some_and(|t| urgent_matches(&urgent, t)) {
            out.push(urgent);
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.msg_id.cmp(&b.msg_id)));
    out
}

/// Whether an urgent interrupt is waiting for the dispatch `current` names.
/// Non-destructive: the orchestrator's drain is the only consumer that takes, so
/// recovery checks and delivery pulses observe without consuming.
pub fn has_pending_urgent(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    current: Option<&crate::process::PidToken>,
) -> bool {
    let Some(token) = current else {
        return false;
    };
    urgent_files(paths, run_id, slot_id)
        .iter()
        .filter_map(|(_, parsed)| parsed.as_ref())
        .any(|urgent| urgent_matches(urgent, token))
}

/// Msg ids the file-time drain and renders must skip: urgent requests filed for
/// the live dispatch. They stay inbox-pending until the resume-time claim, so no
/// poll-file render or turn-boundary pulse presents them before the resume prompt
/// does. Empty when no dispatch is live: with no current token every file is
/// stale and the normal channels own the messages.
pub fn urgent_deferred_msg_ids(paths: &SparPaths, run_id: &str, slot_id: &str) -> Vec<String> {
    let current = read_pid(paths, run_id, slot_id);
    let Some(token) = current.as_ref() else {
        return Vec::new();
    };
    let mut ids: Vec<String> = urgent_files(paths, run_id, slot_id)
        .into_iter()
        .filter_map(|(_, parsed)| parsed)
        .filter(|urgent| urgent_matches(urgent, token))
        .map(|urgent| urgent.msg_id)
        .collect();
    ids.sort();
    ids
}

/// Delete one filed request (an urgent send whose message failed to file, so no
/// turn was ever killed for it).
pub fn remove_urgent_interrupt(paths: &SparPaths, run_id: &str, slot_id: &str, msg_id: &str) {
    let _ = std::fs::remove_file(paths.marker(run_id, &urgent_name(slot_id, msg_id)));
}

pub fn read_session_id(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    provider: &str,
) -> Option<String> {
    let p = paths.marker(run_id, &format!("{slot_id}.{provider}.session_id"));
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Drop a slot/provider's captured session id after a resume attempt finds the vendor's
/// rollout gone (pruned, a different `CODEX_HOME`, a moved box) — the marker is now a
/// dead end pointing at a thread that will never resume, so the next dispatch must go
/// cold instead of repeating the same failure every round.
pub fn clear_session_id(paths: &SparPaths, run_id: &str, slot_id: &str, provider: &str) {
    let _ = std::fs::remove_file(paths.marker(run_id, &format!("{slot_id}.{provider}.session_id")));
}

/// The mtime floor a fresh artifact write must meet: the dispatch start instant
/// with a 1s grace, so a same-second write on a coarse-mtime filesystem does
/// not false-negative. Stale round artifacts are minutes old, so the grace
/// cannot admit one. Shared with the executor's freshness gate so the two
/// cannot drift apart.
pub(crate) fn freshness_floor(since: SystemTime) -> SystemTime {
    since
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Wait until an artifact file holds a *fresh* non-empty write: one with an mtime
/// at or after `since` (this dispatch's start instant, with the same 1s grace
/// for coarse-mtime filesystems). A stale file from a previous round must not
/// satisfy the wait, or the grace window returns instantly on a dead verdict.
pub fn wait_for_artifact(
    paths: &SparPaths,
    run_id: &str,
    name: &str,
    since: SystemTime,
    timeout: Duration,
) -> Result<bool> {
    let path = paths.artifact(run_id, name);
    let floor = freshness_floor(since);
    let start = Instant::now();
    let poll = Duration::from_millis(200);
    loop {
        // A transient metadata error (e.g. mid-write) must not abort the wait — keep
        // polling until the deadline instead of failing the slot prematurely.
        let fresh = std::fs::metadata(&path)
            .map(|m| m.len() > 0 && m.modified().map(|t| t >= floor).unwrap_or(false))
            .unwrap_or(false);
        if fresh {
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
    fn session_id_roundtrips_and_survives_clear_slot() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        assert_eq!(read_session_id(&paths, "r1", "slot-a", "codex"), None);

        write_session_id(&paths, "r1", "slot-a", "codex", "thread-abc").unwrap();
        assert_eq!(
            read_session_id(&paths, "r1", "slot-a", "codex"),
            Some("thread-abc".to_string())
        );

        // A re-dispatch clears the terminal markers, but a thread id must outlive it —
        // that is the whole point of resuming instead of a cold dispatch next round.
        clear_slot(&paths, "r1", "slot-a");
        assert_eq!(
            read_session_id(&paths, "r1", "slot-a", "codex"),
            Some("thread-abc".to_string())
        );
    }

    #[test]
    fn urgent_interrupts_drain_oldest_first_and_delete() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let token = crate::process::PidToken {
            pid: 4242,
            starttime: Some(99),
        };
        let first = UrgentInterrupt {
            msg_id: "m1".into(),
            from: "@human".into(),
            body: "stop, wrong direction".into(),
            ts: chrono::Utc::now(),
            target_pid: token.pid,
            target_starttime: token.starttime,
        };
        let second = UrgentInterrupt {
            msg_id: "m2".into(),
            body: "also check the logs".into(),
            ts: first.ts + chrono::Duration::seconds(1),
            ..first.clone()
        };
        write_urgent_interrupt(&paths, "r1", "slot-a", &second).unwrap();
        write_urgent_interrupt(&paths, "r1", "slot-a", &first).unwrap();
        let drained = take_urgent_interrupts(&paths, "r1", "slot-a", Some(&token));
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].msg_id, "m1");
        assert_eq!(drained[1].msg_id, "m2");
        assert!(take_urgent_interrupts(&paths, "r1", "slot-a", Some(&token)).is_empty());
    }

    #[test]
    fn urgent_take_keeps_only_requests_keyed_to_the_returning_dispatch() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let live = crate::process::PidToken {
            pid: 1111,
            starttime: Some(7),
        };
        let current = UrgentInterrupt {
            msg_id: "m-now".into(),
            from: "@human".into(),
            body: "this turn".into(),
            ts: chrono::Utc::now(),
            target_pid: live.pid,
            target_starttime: live.starttime,
        };
        let recycled = UrgentInterrupt {
            msg_id: "m-old".into(),
            body: "previous turn".into(),
            target_pid: live.pid,
            target_starttime: Some(6),
            ..current.clone()
        };
        write_urgent_interrupt(&paths, "r1", "slot-a", &current).unwrap();
        write_urgent_interrupt(&paths, "r1", "slot-a", &recycled).unwrap();
        let drained = take_urgent_interrupts(&paths, "r1", "slot-a", Some(&live));
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].msg_id, "m-now");
        assert!(take_urgent_interrupts(&paths, "r1", "slot-a", Some(&live)).is_empty());
        write_urgent_interrupt(&paths, "r1", "slot-a", &recycled).unwrap();
        let other = crate::process::PidToken {
            pid: 2222,
            starttime: Some(7),
        };
        assert!(take_urgent_interrupts(&paths, "r1", "slot-a", Some(&other)).is_empty());
        assert!(take_urgent_interrupts(&paths, "r1", "slot-a", Some(&live)).is_empty());
    }

    fn urgent_fixture(pid: u32, starttime: Option<u64>, msg_id: &str) -> UrgentInterrupt {
        UrgentInterrupt {
            msg_id: msg_id.into(),
            from: "@human".into(),
            body: "body".into(),
            ts: chrono::Utc::now(),
            target_pid: pid,
            target_starttime: starttime,
        }
    }

    #[test]
    fn urgent_peek_sees_pending_without_consuming() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let live = crate::process::PidToken {
            pid: 1111,
            starttime: Some(7),
        };
        let stale = crate::process::PidToken {
            pid: 1111,
            starttime: Some(6),
        };
        assert!(!has_pending_urgent(&paths, "r1", "slot-a", Some(&live)));
        assert!(!has_pending_urgent(&paths, "r1", "slot-a", None));
        write_urgent_interrupt(
            &paths,
            "r1",
            "slot-a",
            &urgent_fixture(1111, Some(7), "m-now"),
        )
        .unwrap();
        assert!(has_pending_urgent(&paths, "r1", "slot-a", Some(&live)));
        assert!(!has_pending_urgent(&paths, "r1", "slot-a", Some(&stale)));
        assert!(!has_pending_urgent(&paths, "r1", "slot-a", None));
        assert_eq!(
            take_urgent_interrupts(&paths, "r1", "slot-a", Some(&live)).len(),
            1,
            "the peek consumed nothing"
        );
        assert!(!has_pending_urgent(&paths, "r1", "slot-a", Some(&live)));
    }

    #[test]
    fn urgent_deferred_ids_follow_the_live_pid() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("r1").unwrap();
        write_pid(
            &paths,
            "r1",
            "slot-a",
            crate::process::PidToken {
                pid: 1111,
                starttime: Some(7),
            },
        )
        .unwrap();
        write_urgent_interrupt(
            &paths,
            "r1",
            "slot-a",
            &urgent_fixture(1111, Some(7), "m-now"),
        )
        .unwrap();
        write_urgent_interrupt(
            &paths,
            "r1",
            "slot-a",
            &urgent_fixture(1111, Some(6), "m-old"),
        )
        .unwrap();
        assert_eq!(urgent_deferred_msg_ids(&paths, "r1", "slot-a"), ["m-now"]);
        remove_urgent_interrupt(&paths, "r1", "slot-a", "m-now");
        assert!(urgent_deferred_msg_ids(&paths, "r1", "slot-a").is_empty());
        clear_pid(&paths, "r1", "slot-a");
        write_urgent_interrupt(
            &paths,
            "r1",
            "slot-a",
            &urgent_fixture(1111, Some(7), "m-now"),
        )
        .unwrap();
        assert!(
            urgent_deferred_msg_ids(&paths, "r1", "slot-a").is_empty(),
            "with no live dispatch every file is stale"
        );
    }

    #[test]
    fn session_id_is_scoped_per_provider() {
        // A slot id survives a provider rotation (try_rotate_implementer /
        // try_rotate_reviewer_provider re-dispatch the same slot id under a new
        // provider). A session id captured under one provider must never surface as the
        // prior session for a different provider on that same slot: it is a different
        // vendor's session format entirely, and resuming with it is a hard failure, not a
        // degradation.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        write_session_id(&paths, "r1", "slot-a", "muse", "muse-session-1").unwrap();
        assert_eq!(read_session_id(&paths, "r1", "slot-a", "codex"), None);
        assert_eq!(
            read_session_id(&paths, "r1", "slot-a", "muse"),
            Some("muse-session-1".to_string())
        );

        write_session_id(&paths, "r1", "slot-a", "codex", "thread-abc").unwrap();
        assert_eq!(
            read_session_id(&paths, "r1", "slot-a", "codex"),
            Some("thread-abc".to_string())
        );
        // Rotating away and back finds each provider's own thread untouched.
        assert_eq!(
            read_session_id(&paths, "r1", "slot-a", "muse"),
            Some("muse-session-1".to_string())
        );
    }

    #[test]
    fn clear_session_id_drops_only_that_providers_marker() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        write_session_id(&paths, "r1", "slot-a", "codex", "thread-abc").unwrap();
        write_session_id(&paths, "r1", "slot-a", "muse", "muse-session-1").unwrap();

        clear_session_id(&paths, "r1", "slot-a", "codex");
        assert_eq!(read_session_id(&paths, "r1", "slot-a", "codex"), None);
        assert_eq!(
            read_session_id(&paths, "r1", "slot-a", "muse"),
            Some("muse-session-1".to_string())
        );
    }

    #[test]
    fn wait_for_artifact_ignores_a_stale_file() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(paths.artifacts_dir("r1")).unwrap();
        std::fs::write(paths.artifact("r1", "review-x.md"), "stale verdict").unwrap();
        // A previous round's file, however fresh-looking: `since` after its mtime
        // must wait out the deadline instead of returning instantly on it.
        let since = SystemTime::now() + Duration::from_secs(3600);
        let found = wait_for_artifact(
            &paths,
            "r1",
            "review-x.md",
            since,
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(!found, "a stale artifact must not satisfy the wait");
    }

    #[test]
    fn wait_for_artifact_accepts_a_fresh_write_and_a_late_one() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(paths.artifacts_dir("r1")).unwrap();
        let since = SystemTime::now();
        assert!(
            !wait_for_artifact(
                &paths,
                "r1",
                "missing.md",
                since,
                Duration::from_millis(100)
            )
            .unwrap(),
            "no file at all must still time out"
        );
        let artifact = paths.artifact("r1", "review-y.md");
        std::fs::write(&artifact, "fresh verdict").unwrap();
        assert!(
            wait_for_artifact(&paths, "r1", "review-y.md", since, Duration::from_secs(2)).unwrap(),
            "a write from this dispatch must satisfy the wait at once"
        );
        // A dispatch that is still writing when the gate first looks must be
        // waited for, not failed: the file lands mid-deadline.
        let late = paths.artifact("r1", "review-z.md");
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                std::fs::write(&late, "late verdict").unwrap();
            });
            assert!(
                wait_for_artifact(&paths, "r1", "review-z.md", since, Duration::from_secs(5))
                    .unwrap(),
                "a write landing inside the deadline must satisfy the wait"
            );
        });
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
