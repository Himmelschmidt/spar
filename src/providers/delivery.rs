//! Turn-boundary delivery seam: the adapter-level counterpart to `presence.rs`.
//!
//! When a slot reaches a turn boundary the orchestrator drains its inbox with the
//! Stage 1 exactly-once claim and hands the claimed messages to the adapter's
//! [`DeliveryStrategy`]. Every strategy-specific mechanism lives here behind the seam;
//! the command layer only resolves the strategy (from the slot's adapter) and calls
//! [`deliver`]. The orchestrator never learns which provider it is talking to.

use super::{DeliveryStrategy, MuseAdapter, ProviderAdapter};
use crate::bus::{self, BusMessage};
use crate::markers;
use crate::paths::SparPaths;
use crate::process::StreamStats;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the seam actually did with the claimed messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryAction {
    /// Claude: a Stop-hook `block` payload was built for the hook to relay into the model.
    StopHookBlock,
    /// Grok: claimed messages appended to the durable turn-boundary queue.
    Queued,
    /// opencode: claimed messages appended to the durable queue for the session flush.
    Prompted,
    /// Written to the slot's poll file, which its role prompt tells it to read before
    /// starting any new major step.
    PolledFile,
    /// muse: pushed into the running session via `muse session-message send`.
    SessionMessaged,
    /// No injection channel (agy / unknown provider): the inbox is left untouched so
    /// the agent claims it itself on its next turn.
    LeftForInbox,
    /// Nothing to do — the inbox was empty.
    Empty,
}

/// Outcome of one `deliver` call. `payload`, when set, is the raw Stop-hook JSON the
/// command prints to stdout for Claude's hook runner to consume.
#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub strategy: DeliveryStrategy,
    pub action: DeliveryAction,
    /// Messages claimed and injected this call.
    pub delivered: usize,
    /// Messages still waiting in the inbox (only non-zero for `LeftForInbox`).
    #[serde(skip_serializing_if = "is_zero")]
    pub pending: usize,
    /// Stop-hook block JSON to emit to stdout (only for `StopHookBlock`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Drain `agent`'s inbox and dispatch the claimed messages to `strategy`.
///
/// `agent` is the unique bus id (run slots: `run:slot`, bare agents: their own id), so
/// the inbox directory already isolates one agent's traffic — no run filter is needed on
/// the drain. `run` is still threaded to [`queue_path`] to keep each run's durable
/// turn-boundary queue partitioned.
///
/// `None` never consumes the inbox — an agent with no injection channel reads its own
/// inbox on its next turn, so claiming here would strand the messages. Every other
/// strategy claims (exactly-once) and injects; an empty inbox is a no-op.
///
/// `dry_run` stubs the side-effecting injection call (queue append / session prompt) so
/// the run-lifecycle test backend exercises drain + dispatch without touching a live
/// agent. Building the Stop-hook payload is pure and runs in either mode.
pub fn deliver(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    strategy: DeliveryStrategy,
    dry_run: bool,
) -> Result<Delivery> {
    if strategy == DeliveryStrategy::None {
        let pending = bus::inbox(paths, agent)?.len();
        return Ok(Delivery {
            strategy,
            action: DeliveryAction::LeftForInbox,
            delivered: 0,
            pending,
            payload: None,
        });
    }

    let msgs = bus::inbox_claim(paths, agent)?;
    if msgs.is_empty() {
        return Ok(Delivery {
            strategy,
            action: DeliveryAction::Empty,
            delivered: 0,
            pending: 0,
            payload: None,
        });
    }
    let delivered = msgs.len();

    let (action, payload) = match strategy {
        DeliveryStrategy::StopHookInject => {
            (DeliveryAction::StopHookBlock, Some(block_payload(&msgs)))
        }
        DeliveryStrategy::NativeQueue => {
            enqueue(paths, run, agent, &msgs, dry_run)?;
            (DeliveryAction::Queued, None)
        }
        DeliveryStrategy::SdkPrompt => {
            enqueue(paths, run, agent, &msgs, dry_run)?;
            (DeliveryAction::Prompted, None)
        }
        DeliveryStrategy::PollFile => {
            append_poll_file(paths, run, agent, &render_reason(&msgs), dry_run)?;
            (DeliveryAction::PolledFile, None)
        }
        DeliveryStrategy::MuseSessionMessage => {
            let body = render_reason(&msgs);
            let sent = match muse_session_id(paths, run, agent) {
                Some(target) => send_muse_session_message(
                    resolve_muse_bin().as_deref(),
                    &target,
                    &body,
                    dry_run,
                    MUSE_SESSION_MESSAGE_TIMEOUT,
                ),
                None => false,
            };
            // The poll file is the fallback, not a belt-and-suspenders copy: a confirmed
            // push (`--json`'s own `status` field said `"ok"`, not just a zero exit) is
            // the one channel `render_reason`'s "delivered once" header can promise; a
            // second copy in the poll file would make that a lie.
            let action = if sent {
                DeliveryAction::SessionMessaged
            } else {
                append_poll_file(paths, run, agent, &body, dry_run)?;
                DeliveryAction::PolledFile
            };
            (action, None)
        }
        DeliveryStrategy::None => unreachable!("None handled above"),
    };

    Ok(Delivery {
        strategy,
        action,
        delivered,
        pending: 0,
        payload,
    })
}

/// Render claimed messages as the `reason` a Claude Stop hook injects. Returning
/// `{"decision":"block","reason":…}` makes the model continue with this text as new
/// input instead of stopping — the headless, pane-free injection channel.
fn block_payload(msgs: &[BusMessage]) -> String {
    serde_json::json!({
        "decision": "block",
        "reason": render_reason(msgs),
    })
    .to_string()
}

fn render_reason(msgs: &[BusMessage]) -> String {
    let mut s = String::from("New swarm messages (delivered once — act on them, then continue):");
    for m in msgs {
        s.push_str(&format!("\n- [{:?}] from {}: {}", m.kind, m.from, m.body));
    }
    s
}

/// Per-agent durable turn-boundary queue path. Grok's own `/queue` and opencode's
/// session prompt are in-process channels into a *running* slot; until the live push
/// lands (Track A panes / the opencode adapter) spar persists the claimed prompts here
/// so nothing is lost between the claim and the flush.
///
/// `run` scopes the queue file exactly like the inbox drain: slot ids are deterministic
/// per provider/role and collide across concurrent same-shaped runs, so a bare
/// `queue/<agent>.jsonl` would mix two runs' claimed messages into one file (the flush
/// would then leak run A's prompts into run B's slot). `Some(r)` nests the file under
/// `queue/<r>/`, keeping each run's queue isolated; `None` (bare agents) stays at the
/// queue root.
pub fn queue_path(paths: &SparPaths, run: Option<&str>, agent: &str) -> std::path::PathBuf {
    let root = bus::bus_root(paths).join("queue");
    let dir = match run {
        Some(r) => root.join(r),
        None => root,
    };
    dir.join(format!("{agent}.jsonl"))
}

/// Append claimed messages to the durable queue. `dry_run` stubs the write so the test
/// backend never mutates delivery state.
fn enqueue(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    msgs: &[BusMessage],
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let path = queue_path(paths, run, agent);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    for m in msgs {
        let mut line = serde_json::to_vec(m)?;
        line.push(b'\n');
        f.write_all(&line)?;
    }
    Ok(())
}

/// Where spar leaves messages for an adapter with no push channel into its running
/// process. Slot-scoped, like every other per-slot file: `artifacts_dir` is shared by
/// every slot in a run, and arena spawns N concurrent implementers off one template, so a
/// run-scoped name would have them reading each other's nudges.
///
/// Lives under the run's `logs/`, not `artifacts/`: it is orchestrator-to-slot chatter,
/// not a deliverable, and nothing downstream should mistake it for one.
pub fn poll_file(paths: &SparPaths, run: Option<&str>, agent: &str) -> std::path::PathBuf {
    match run {
        Some(r) => paths
            .logs_dir(r)
            .join(format!("nudges-{}.md", slot_part(r, agent))),
        None => bus::bus_root(paths)
            .join("nudges")
            .join(format!("{agent}.md")),
    }
}

/// The inbox drain keys on the unique `run:slot` id, but the poll file sits in that run's
/// own directory, so the run prefix would only be repeated in the name.
fn slot_part<'a>(run: &str, agent: &'a str) -> &'a str {
    agent.strip_prefix(&format!("{run}:")).unwrap_or(agent)
}

/// The muse session id for `agent`'s slot, once muse has emitted its first session line
/// (`StreamCoalescer::session_id`, persisted to the slot's log sidecar). `None` before
/// that line arrives, always `None` for a bare agent (`run` is `None`), which has no slot
/// log to read, and `None` once the slot's process is no longer alive — the sidecar
/// outlives the process (nothing clears it on exit), so an unguarded read can target a
/// finished session. Every `None` falls back to the poll file.
fn muse_session_id(paths: &SparPaths, run: Option<&str>, agent: &str) -> Option<String> {
    let run = run?;
    let slot_id = slot_part(run, agent);
    if !markers::read_pid(paths, run, slot_id).is_some_and(|t| t.alive()) {
        return None;
    }
    let log_path = paths.log_file(run, slot_id);
    StreamStats::load(&log_path)?.session_id
}

/// Hard ceiling on `muse session-message send` before spar gives up on it — covering both
/// the stdin write and the exit wait, together. `nudge`'s caller runs inside
/// `run_captured`'s own poll loop (`process.rs`), the same loop that enforces the slot's
/// wall-clock ceiling, so an unbounded wait here would stall that ceiling check for as
/// long as the send hangs (unresponsive session host, ingress stuck on a lock, or a large
/// bus message filling the pipe before muse starts draining stdin). Ten seconds is
/// generous for a local IPC call and small next to the minutes-to-hours a slot actually
/// runs.
const MUSE_SESSION_MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve the `muse` binary to spawn for `session-message send`. Delegates to the
/// adapter's own `PATH` lookup in production; tests inject a fake binary path through
/// [`tests::with_muse_bin`] instead of mutating process-wide `PATH` (which would race
/// every other test in the binary that spawns a subprocess by name).
fn resolve_muse_bin() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    {
        if let Some(over) = tests::MUSE_BIN_OVERRIDE.with(|c| c.borrow().clone()) {
            return over;
        }
    }
    MuseAdapter.resolve_binary()
}

/// Push `body` into a running muse session via its cross-session inject channel
/// (`muse session-message send --target <session-uuid>`, message body on stdin).
/// `bin` is the resolved `muse` binary (`None` means it wasn't found). Returns `true`
/// only when the exit is clean *and* the `--json` reply explicitly says `"status":"ok"`
/// — a zero exit alone is not proof the ingress accepted the message, only that the
/// process didn't crash. Every other outcome — `muse` missing, a spawn failure, a
/// nonzero exit (e.g. muse's `external_agent_ingress` feature gate off, which fails
/// closed with `external_agent_ingress_closed`), a reply that doesn't parse as JSON at
/// all, a JSON reply missing `status` or naming anything other than `"ok"`, or a hang
/// past `timeout` in either the stdin write or the exit wait — returns `false` so the
/// caller falls back to the poll file instead of reporting a delivery that never landed
/// anywhere. There is no `Err` path: every failure mode here is an expected outcome of
/// talking to an external process, not a bug in spar.
///
/// The write happens on a detached thread so a body larger than the pipe buffer can never
/// block this call past `timeout`: if muse hasn't started draining stdin by the deadline,
/// the child is killed, which unblocks the writer with a broken pipe. Reading stdout runs
/// on its own detached thread for the same reason: the child's stdout pipe can fill while
/// its stdin write is still in flight, and this call must stay bounded by one deadline
/// covering the whole exchange, not the sum of independent reads and writes.
///
/// Spawned with no `current_dir` override, so it inherits the orchestrator's own cwd
/// rather than the target slot's worktree — deliberately: `target` is a session id, not a
/// path, and `session-message send` addresses muse's own session registry, which is not
/// scoped to any one workspace.
fn send_muse_session_message(
    bin: Option<&Path>,
    target: &str,
    body: &str,
    dry_run: bool,
    timeout: Duration,
) -> bool {
    if dry_run {
        return true;
    }
    let Some(bin) = bin else {
        return false;
    };
    let mut child = match Command::new(bin)
        .args(["session-message", "send", "--target", target, "--json"])
        .env("MUSE_NO_AUTO_UPDATE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let deadline = Instant::now() + timeout;

    let mut stdin = child.stdin.take().expect("stdin piped above");
    let body = body.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(stdin.write_all(body.as_bytes()).is_ok());
    });

    let mut stdout = child.stdout.take().expect("stdout piped above");
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = out_tx.send(buf);
    });

    loop {
        match rx.try_recv() {
            // A write failure (broken pipe) is not proof the send was rejected: muse may
            // have read enough of stdin to accept the message and closed its end before
            // the writer drained the rest of the buffer. Either outcome falls through to
            // the same exit-and-reply check below rather than killing outright here — an
            // accepted send that happens to fail the write is reported once instead of
            // being duplicated into the poll-file fallback.
            Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    // Killing the child drops its stdin reader, so a writer blocked on a
                    // full pipe unblocks with a broken-pipe error shortly after — the
                    // thread is left to finish on its own rather than joined here.
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return false;
                }
                let out = out_rx
                    .recv_timeout(Duration::from_millis(500))
                    .unwrap_or_default();
                return muse_send_reply_ok(&out);
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Err(_) => return false,
        }
    }
}

/// Read `muse session-message send --json`'s reply as spar's confirmation that the
/// message actually landed, not just that the process exited zero. Success requires an
/// explicit `"status":"ok"` in a reply that parses as JSON; anything else — output that
/// doesn't parse at all (a banner, a login notice, or any other line sharing stdout with
/// the reply on a zero exit), valid JSON missing `status`, or a `status` naming anything
/// but `"ok"` — reports failure so the caller falls back to the poll file. The exact
/// reply shape on a real successful send has never been observed (every box this ran on
/// had `external_agent_ingress` closed): failing toward the fallback on anything
/// unrecognized only costs an extra poll-file copy, while trusting it risks reporting a
/// claimed message delivered when it never landed.
fn muse_send_reply_ok(stdout: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str().map(str::to_string)))
        .is_some_and(|s| s == "ok")
}

fn append_poll_file(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    body: &str,
    dry_run: bool,
) -> Result<std::path::PathBuf> {
    let path = poll_file(paths, run, agent);
    if dry_run {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "\n## {stamp}\n\n{body}")?;
    Ok(path)
}

/// What spar did with one nudge, for the event line and for tests.
#[derive(Debug, Clone, Serialize)]
pub struct NudgeDelivery {
    pub strategy: DeliveryStrategy,
    pub action: DeliveryAction,
    /// Set when the nudge landed in a file the agent can read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// The sender id spar's own nudges carry on the bus. Not a slot, so it can never collide
/// with one, and readable in an agent's inbox.
pub const ORCHESTRATOR: &str = "spar";

/// Push one orchestrator nudge at `agent` through whichever channel its adapter exposes.
///
/// The caller passes the strategy it got from the adapter and nothing else; which of
/// these branches runs is the seam's business, not the orchestrator's. Text sent to a
/// *busy* CLI agent's TTY only queues unsubmitted, so every branch here lands somewhere
/// the agent reads at a turn boundary instead: claude's Stop hook drains its inbox, grok
/// applies its native queue, and everything else reads the poll file its prompt names.
pub fn nudge(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    strategy: DeliveryStrategy,
    text: &str,
    dry_run: bool,
) -> Result<NudgeDelivery> {
    let (action, path) = match strategy {
        // The hook is pull-based: it fires at the turn boundary and drains the inbox,
        // so the inbox is how spar pushes into a claude slot.
        DeliveryStrategy::StopHookInject => {
            if !dry_run {
                bus::send(
                    paths,
                    BusMessage {
                        id: bus::new_id(),
                        ts: chrono::Utc::now(),
                        from: ORCHESTRATOR.into(),
                        to: agent.into(),
                        // `System` is exempt from the loop guard, which governs two
                        // agents ping-ponging and has nothing to say about spar.
                        kind: bus::MsgKind::System,
                        body: text.to_string(),
                        run: run.map(str::to_string),
                        subject: Some("spar nudge".into()),
                        refs: bus::MsgRefs::default(),
                        requires_ack: false,
                        meta: std::collections::HashMap::new(),
                    },
                    // Nudges are spar's own control channel; they must not be dropped
                    // because the run's agents have been chatty.
                    bus::MessageBudget::Chatty,
                )?;
            }
            (DeliveryAction::StopHookBlock, None)
        }
        DeliveryStrategy::MuseSessionMessage => {
            let sent = match muse_session_id(paths, run, agent) {
                Some(target) => send_muse_session_message(
                    resolve_muse_bin().as_deref(),
                    &target,
                    text,
                    dry_run,
                    MUSE_SESSION_MESSAGE_TIMEOUT,
                ),
                None => false,
            };
            // Same as `deliver`: the poll file is only the fallback for an unconfirmed
            // push, not a standing duplicate of a confirmed one.
            if sent {
                (DeliveryAction::SessionMessaged, None)
            } else {
                let path = append_poll_file(paths, run, agent, text, dry_run)?;
                (DeliveryAction::PolledFile, Some(path))
            }
        }
        DeliveryStrategy::NativeQueue | DeliveryStrategy::SdkPrompt => {
            let path = queue_path(paths, run, agent);
            if !dry_run {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                writeln!(f, "{}", serde_json::json!({ "nudge": text }))?;
            }
            (DeliveryAction::Queued, Some(path))
        }
        // `None` is agy, which has no channel at all. The poll file is still the best
        // available drop: worst case the agent never reads it, which is where it started.
        DeliveryStrategy::PollFile | DeliveryStrategy::None => (
            DeliveryAction::PolledFile,
            Some(append_poll_file(paths, run, agent, text, dry_run)?),
        ),
    };
    Ok(NudgeDelivery {
        strategy,
        action,
        path: path.map(|p| p.display().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{agent_ref, chat, join, MessageBudget};
    use tempfile::tempdir;

    fn seed(paths: &SparPaths, n: usize) {
        join(
            paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(paths, Some("r1"), "b", Some("cli:grok"), Some("native-cli")).unwrap();
        for i in 0..n {
            chat(
                paths,
                Some("r1"),
                "a",
                "b",
                format!("msg {i}"),
                MessageBudget::Chatty,
            )
            .unwrap();
        }
    }

    #[test]
    fn stop_hook_inject_claims_and_builds_block_payload() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 2);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::StopHookInject,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::StopHookBlock);
        assert_eq!(d.delivered, 2);
        let payload = d.payload.expect("block payload");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["decision"], "block");
        let reason = v["reason"].as_str().unwrap();
        assert!(
            reason.contains("msg 0") && reason.contains("msg 1"),
            "{reason}"
        );

        // Exactly-once: a second delivery drains nothing.
        let again = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::StopHookInject,
            false,
        )
        .unwrap();
        assert_eq!(again.action, DeliveryAction::Empty);
        assert_eq!(again.delivered, 0);
    }

    #[test]
    fn native_queue_appends_durable_queue() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 3);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::NativeQueue,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::Queued);
        assert_eq!(d.delivered, 3);
        assert!(d.payload.is_none());
        let queued = fs::read_to_string(queue_path(&paths, Some("r1"), &ub)).unwrap();
        assert_eq!(queued.lines().filter(|l| !l.is_empty()).count(), 3);
    }

    #[test]
    fn sdk_prompt_appends_durable_queue() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(&paths, Some("r1"), &ub, DeliveryStrategy::SdkPrompt, false).unwrap();
        assert_eq!(d.action, DeliveryAction::Prompted);
        assert_eq!(d.delivered, 1);
        assert!(queue_path(&paths, Some("r1"), &ub).is_file());
    }

    #[test]
    fn none_leaves_inbox_untouched() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 2);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(&paths, Some("r1"), &ub, DeliveryStrategy::None, false).unwrap();
        assert_eq!(d.action, DeliveryAction::LeftForInbox);
        assert_eq!(d.delivered, 0);
        assert_eq!(d.pending, 2);
        // The agent must still be able to claim them itself.
        assert_eq!(bus::inbox_claim(&paths, &ub).unwrap().len(), 2);
    }

    #[test]
    fn dry_run_stubs_the_queue_write_but_still_drains() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 2);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(&paths, Some("r1"), &ub, DeliveryStrategy::NativeQueue, true).unwrap();
        assert_eq!(d.action, DeliveryAction::Queued);
        assert_eq!(d.delivered, 2);
        // Injection call stubbed: no queue file written.
        assert!(!queue_path(&paths, Some("r1"), &ub).exists());
        // But the drain is real (exactly-once): nothing remains to claim.
        assert!(bus::inbox_claim(&paths, &ub).unwrap().is_empty());
    }

    /// Two concurrent runs share a deterministic slot id ("b"), hence one workspace inbox,
    /// but the durable queue must stay run-isolated: run B's flush must never surface run
    /// A's claimed prompts. Each run enqueues into its own `queue/<run>/b.jsonl`.
    #[test]
    fn native_queue_is_run_scoped_across_identical_slot_ids() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        // Two runs, each with the same slot id "b" fed by its own sender "a".
        for r in ["rA", "rB"] {
            join(&paths, Some(r), "a", Some("cli:claude"), Some("native-cli")).unwrap();
            join(&paths, Some(r), "b", Some("cli:grok"), Some("native-cli")).unwrap();
        }
        chat(
            &paths,
            Some("rA"),
            "a",
            "b",
            "for run A".to_string(),
            MessageBudget::Chatty,
        )
        .unwrap();
        chat(
            &paths,
            Some("rB"),
            "a",
            "b",
            "for run B".to_string(),
            MessageBudget::Chatty,
        )
        .unwrap();

        let (ua, ub) = (agent_ref(Some("rA"), "b"), agent_ref(Some("rB"), "b"));
        deliver(
            &paths,
            Some("rB"),
            &ub,
            DeliveryStrategy::NativeQueue,
            false,
        )
        .unwrap();
        deliver(
            &paths,
            Some("rA"),
            &ua,
            DeliveryStrategy::NativeQueue,
            false,
        )
        .unwrap();

        let qa = fs::read_to_string(queue_path(&paths, Some("rA"), &ua)).unwrap();
        let qb = fs::read_to_string(queue_path(&paths, Some("rB"), &ub)).unwrap();
        assert!(
            qa.contains("for run A") && !qa.contains("for run B"),
            "run A queue: {qa}"
        );
        assert!(
            qb.contains("for run B") && !qb.contains("for run A"),
            "run B queue: {qb}"
        );
    }

    /// Cross-scope directed delivery (W5) through the *real* `deliver` path: a bare agent
    /// and a run slot each address the other by its unique id, and each message is claimed
    /// by the recipient's own `deliver` drain — the turn-boundary path — proving delivery
    /// keys on the unique id with no run filter.
    #[test]
    fn cross_scope_directed_delivery_via_deliver() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "slot",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(&paths, None, "bare", Some("cli:grok"), Some("native-cli")).unwrap();
        let slot_id = agent_ref(Some("r1"), "slot"); // "r1:slot"

        // bare → r1:slot and r1:slot → bare, each addressed by the recipient's unique id.
        chat(
            &paths,
            None,
            "bare",
            &slot_id,
            "hi slot".to_string(),
            MessageBudget::Chatty,
        )
        .unwrap();
        chat(
            &paths,
            None,
            &slot_id,
            "bare",
            "hi bare".to_string(),
            MessageBudget::Chatty,
        )
        .unwrap();

        // The slot claims only its own message via its unique-id drain.
        let to_slot = deliver(
            &paths,
            Some("r1"),
            &slot_id,
            DeliveryStrategy::StopHookInject,
            false,
        )
        .unwrap();
        assert_eq!(to_slot.delivered, 1);
        assert!(to_slot.payload.unwrap().contains("hi slot"));

        // The bare agent claims only its own message via its unique-id drain.
        let to_bare = deliver(
            &paths,
            None,
            "bare",
            DeliveryStrategy::StopHookInject,
            false,
        )
        .unwrap();
        assert_eq!(to_bare.delivered, 1);
        assert!(to_bare.payload.unwrap().contains("hi bare"));
    }

    // Injectable override for `resolve_muse_bin`, read by production code but written only
    // from tests. The default test harness gives every `#[test]` fn its own thread, so a
    // thread-local needs no cross-test lock — unlike mutating process-wide `PATH` (the
    // previous approach), which raced every other test in the binary that spawns a
    // subprocess by name (e.g. `worktree.rs`'s `git` spawns).
    thread_local! {
        pub(super) static MUSE_BIN_OVERRIDE: std::cell::RefCell<Option<Option<std::path::PathBuf>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Run `f` with [`resolve_muse_bin`] forced to return `bin` (`None` simulates muse not
    /// being found anywhere).
    fn with_muse_bin<T>(bin: Option<std::path::PathBuf>, f: impl FnOnce() -> T) -> T {
        MUSE_BIN_OVERRIDE.with(|c| *c.borrow_mut() = Some(bin));
        let result = f();
        MUSE_BIN_OVERRIDE.with(|c| *c.borrow_mut() = None);
        result
    }

    /// Writes a fake `muse` binary at `dir/muse` that records its invocation (args on one
    /// line, stdin body on the next) to `capture`, then exits `code`. Returns the script's
    /// path, to be handed to [`with_muse_bin`].
    fn fake_muse_exit(
        dir: &std::path::Path,
        capture: &std::path::Path,
        code: i32,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("muse");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {args:?}\ncat > {stdin:?}\necho '{{\"schema_version\":1,\"status\":\"ok\"}}'\nexit {code}\n",
                args = capture.with_extension("args"),
                stdin = capture.with_extension("stdin"),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Like [`fake_muse_exit`] with a clean exit — the common case.
    fn fake_muse(dir: &std::path::Path, capture: &std::path::Path) -> std::path::PathBuf {
        fake_muse_exit(dir, capture, 0)
    }

    /// A `muse` that exits 0 but whose `--json` reply states a non-`"ok"` status — the
    /// case a bare exit-code check cannot see: the process didn't crash, but the ingress
    /// itself says nothing landed.
    fn fake_muse_unconfirmed(
        dir: &std::path::Path,
        capture: &std::path::Path,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("muse");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {args:?}\ncat > {stdin:?}\necho '{{\"status\":\"unavailable\",\"error_code\":\"external_agent_ingress_closed\"}}'\nexit 0\n",
                args = capture.with_extension("args"),
                stdin = capture.with_extension("stdin"),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// A `muse` that exits 0 but prints a banner ahead of the `--json` reply on the same
    /// stdout — the shape that made a lenient "unparseable means success" reading unsafe:
    /// the combined buffer is not valid JSON at all, even though a real reply is in there.
    fn fake_muse_banner_before_reply(
        dir: &std::path::Path,
        capture: &std::path::Path,
    ) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("muse");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {args:?}\ncat > {stdin:?}\necho 'a new version of muse is available'\necho '{{\"status\":\"ok\"}}'\nexit 0\n",
                args = capture.with_extension("args"),
                stdin = capture.with_extension("stdin"),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Writes a fake `muse` binary at `dir/muse` running raw shell `body` instead of the
    /// capture-and-exit script — for tests that need to control stdin draining / timing
    /// directly (the write-bound and wait-bound regression tests below).
    fn fake_muse_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("muse");
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Marks `run:slot` alive for `muse_session_id`'s liveness guard, using the test
    /// process's own pid — genuinely alive for the duration of the test.
    fn mark_alive(paths: &SparPaths, run: &str, slot: &str) {
        markers::write_pid(
            paths,
            run,
            slot,
            crate::process::PidToken::capture(std::process::id()),
        )
        .unwrap();
    }

    fn muse_seed(paths: &SparPaths, n: usize) {
        join(
            paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(paths, Some("r1"), "b", Some("cli:muse"), Some("native-cli")).unwrap();
        for i in 0..n {
            chat(
                paths,
                Some("r1"),
                "a",
                "b",
                format!("msg {i}"),
                MessageBudget::Chatty,
            )
            .unwrap();
        }
    }

    /// The window `muse.rs`'s doc comment calls out: a nudge can be queued before muse has
    /// emitted its first session line, so with no sidecar session id yet the strategy must
    /// still land somewhere the slot reads, not silently drop the message.
    #[test]
    fn muse_session_message_falls_back_to_poll_file_before_session_id_known() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::MuseSessionMessage,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("msg 0"), "{body}");
    }

    /// Once the sidecar has a session id (`StreamCoalescer` captured it from the exec
    /// JSONL's first `/stream/id` line), delivery must use the real push channel — not
    /// the poll file — via `muse session-message send --target <id>`.
    #[test]
    fn muse_session_message_pushes_into_the_running_session_once_known() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-123".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let bin = fake_muse(tmp.path(), &capture);

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(Some(bin), || {
            deliver(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::SessionMessaged);
        assert_eq!(d.delivered, 1);
        // A confirmed push is the only channel: no duplicate poll-file copy.
        assert!(
            !poll_file(&paths, Some("r1"), &ub).exists(),
            "confirmed push must not also write the poll file"
        );

        let args = fs::read_to_string(capture.with_extension("args")).unwrap();
        assert!(args.contains("--target sess-123"), "{args}");
        let stdin = fs::read_to_string(capture.with_extension("stdin")).unwrap();
        assert!(stdin.contains("msg 0"), "{stdin}");
    }

    /// The nudge path (spar's own control messages, not claimed bus traffic) must take the
    /// same fork: real channel once the id is known.
    #[test]
    fn muse_nudge_pushes_into_the_running_session_once_known() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 0);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-456".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let bin = fake_muse(tmp.path(), &capture);

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(Some(bin), || {
            nudge(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                "land your artifact",
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::SessionMessaged);
        let args = fs::read_to_string(capture.with_extension("args")).unwrap();
        assert!(args.contains("--target sess-456"), "{args}");
        let stdin = fs::read_to_string(capture.with_extension("stdin")).unwrap();
        assert!(stdin.contains("land your artifact"), "{stdin}");
        // A confirmed push is the only channel here too: no duplicate poll-file copy.
        assert!(
            !poll_file(&paths, Some("r1"), &ub).exists(),
            "confirmed push must not also write the poll file"
        );
    }

    /// muse's own `external_agent_ingress` feature gate fails closed with a nonzero exit
    /// (`external_agent_ingress_closed`) rather than an `Err` spar can catch structurally.
    /// A rejected send must fall back to the poll file, not be reported as delivered —
    /// otherwise the claimed bus message is gone with nowhere it landed.
    #[test]
    fn muse_session_message_falls_back_to_poll_file_on_nonzero_exit() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-789".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let bin = fake_muse_exit(tmp.path(), &capture, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(Some(bin), || {
            deliver(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("msg 0"), "{body}");
    }

    /// A zero exit is not proof of delivery: when the `--json` reply's own `status` field
    /// says the send was not accepted, that must fall back to the poll file exactly like
    /// a nonzero exit would — otherwise a rejected send gets reported `SessionMessaged`.
    #[test]
    fn muse_session_message_falls_back_to_poll_file_on_unconfirmed_status() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-unconfirmed".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let bin = fake_muse_unconfirmed(tmp.path(), &capture);

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(Some(bin), || {
            deliver(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("msg 0"), "{body}");
    }

    /// A zero exit with a reply that doesn't parse as JSON at all (a banner ahead of the
    /// real reply on the same stdout) must also fall back — treating unparseable output as
    /// success would report a delivery no one can prove landed.
    #[test]
    fn muse_session_message_falls_back_to_poll_file_on_unparseable_reply() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-banner".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let bin = fake_muse_banner_before_reply(tmp.path(), &capture);

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(Some(bin), || {
            deliver(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("msg 0"), "{body}");
    }

    #[test]
    fn muse_send_reply_ok_requires_explicit_ok_status() {
        assert!(muse_send_reply_ok(r#"{"status":"ok"}"#));
        assert!(!muse_send_reply_ok(r#"{"status":"unavailable"}"#));
        assert!(!muse_send_reply_ok("{}"));
        assert!(!muse_send_reply_ok(""));
        assert!(!muse_send_reply_ok(
            "a new version of muse is available\n{\"status\":\"ok\"}"
        ));
    }

    /// The nudge path takes the same fallback on a rejected send — a nudge is spar's own
    /// control message, and it must not be silently dropped either.
    #[test]
    fn muse_nudge_falls_back_to_poll_file_on_nonzero_exit() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 0);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-abc".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let bin = fake_muse_exit(tmp.path(), &capture, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(Some(bin), || {
            nudge(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                "land your artifact",
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("land your artifact"), "{body}");
    }

    /// A slot log sidecar can outlive the environment `muse` was reachable from (a stale
    /// session id from a prior box, or `muse` simply not installed where the delivery seam
    /// runs). Must fall back cleanly rather than propagate a spawn error and lose the
    /// message.
    #[test]
    fn muse_session_message_falls_back_to_poll_file_when_muse_missing() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);
        mark_alive(&paths, "r1", "b");

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-none".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let ub = agent_ref(Some("r1"), "b");
        let d = with_muse_bin(None, || {
            deliver(
                &paths,
                Some("r1"),
                &ub,
                DeliveryStrategy::MuseSessionMessage,
                false,
            )
        })
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("msg 0"), "{body}");
    }

    /// A slot log sidecar outlives the process (nothing clears it on exit), so a stale
    /// session id from a finished slot must not be targeted — otherwise a claimed message
    /// is reported delivered into a session nobody is reading anymore.
    #[test]
    fn muse_session_message_falls_back_to_poll_file_when_slot_not_alive() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 1);
        // Deliberately no `mark_alive`: no pid marker at all, same as a slot that finished
        // in a round nothing ever recorded a pid for.

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-stale".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::MuseSessionMessage,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("msg 0"), "{body}");
    }

    /// Reproduces the hang the 10s bound exists to cap: a body larger than the pipe
    /// buffer, spawned against a `muse` that never reads stdin at all. Before the write
    /// moved onto its own thread this blocked forever on the main thread, defeating the
    /// deadline loop below it — proven here by asserting the call actually returns inside
    /// a small multiple of the (shortened, for the test) timeout.
    #[test]
    fn send_bounded_when_muse_never_reads_stdin() {
        let tmp = tempdir().unwrap();
        let bin = fake_muse_script(tmp.path(), "sleep 5");
        let big_body = "x".repeat(256 * 1024); // well past a typical 64KiB pipe buffer

        let start = Instant::now();
        let ok = send_muse_session_message(
            Some(&bin),
            "sess-hang",
            &big_body,
            false,
            Duration::from_millis(200),
        );
        assert!(!ok);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "write must be bounded by the timeout, took {:?}",
            start.elapsed()
        );
    }

    /// Same bound, but the hang is *after* stdin is fully drained (muse reads everything,
    /// then never exits) — the exit-wait half of the same deadline.
    #[test]
    fn send_bounded_when_muse_reads_then_hangs() {
        let tmp = tempdir().unwrap();
        let bin = fake_muse_script(tmp.path(), "cat > /dev/null\nsleep 5");

        let start = Instant::now();
        let ok = send_muse_session_message(
            Some(&bin),
            "sess-hang2",
            "hello",
            false,
            Duration::from_millis(200),
        );
        assert!(!ok);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "wait must be bounded by the timeout, took {:?}",
            start.elapsed()
        );
    }

    /// A body larger than the pipe buffer must still succeed against a `muse` that starts
    /// reading only after a short delay — proving the threaded write and the deadline loop
    /// cooperate rather than one starving the other.
    #[test]
    fn send_succeeds_with_large_body_and_slow_reader() {
        let tmp = tempdir().unwrap();
        let bin = fake_muse_script(
            tmp.path(),
            "sleep 0.2\ncat > /dev/null\necho '{\"status\":\"ok\"}'\nexit 0",
        );
        let big_body = "y".repeat(256 * 1024);

        let ok = send_muse_session_message(
            Some(&bin),
            "sess-slow",
            &big_body,
            false,
            Duration::from_secs(5),
        );
        assert!(ok);
    }
}
