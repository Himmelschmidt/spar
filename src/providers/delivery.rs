//! Turn-boundary delivery seam: the adapter-level counterpart to `presence.rs`.
//!
//! When a slot reaches a turn boundary the orchestrator drains its inbox with the
//! Stage 1 exactly-once claim and hands the claimed messages to the adapter's
//! [`DeliveryStrategy`]. Every strategy-specific mechanism lives here behind the seam;
//! the command layer only resolves the strategy (from the slot's adapter) and calls
//! [`deliver`]. The orchestrator never learns which provider it is talking to.

use super::{DeliveryStrategy, MuseAdapter, ProviderAdapter};
use crate::bus::{self, BusMessage};
use crate::paths::SparPaths;
use crate::process::StreamStats;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
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
                Some(target) => send_muse_session_message(&target, &body, dry_run)?,
                None => false,
            };
            if sent {
                (DeliveryAction::SessionMessaged, None)
            } else {
                append_poll_file(paths, run, agent, &body, dry_run)?;
                (DeliveryAction::PolledFile, None)
            }
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
/// that line arrives, and always `None` for a bare agent (`run` is `None`), which has no
/// slot log to read — either way the caller falls back to the poll file.
fn muse_session_id(paths: &SparPaths, run: Option<&str>, agent: &str) -> Option<String> {
    let run = run?;
    let log_path = paths.log_file(run, slot_part(run, agent));
    StreamStats::load(&log_path)?.session_id
}

/// Hard ceiling on `muse session-message send` before spar gives up on it. `nudge`'s
/// caller runs inside `run_captured`'s own poll loop (`process.rs`), the same loop that
/// enforces the slot's wall-clock ceiling, so an unbounded wait here would stall that
/// ceiling check for as long as the send hangs (unresponsive session host, ingress stuck
/// on a lock). Ten seconds is generous for a local IPC call and small next to the
/// minutes-to-hours a slot actually runs.
const MUSE_SESSION_MESSAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Push `body` into a running muse session via its cross-session inject channel
/// (`muse session-message send --target <session-uuid>`, message body on stdin).
/// Returns `Ok(true)` only on a clean, successful exit. Every other outcome — `muse`
/// missing from `PATH`, a spawn or stdin-write failure, a nonzero exit (e.g. muse's
/// `external_agent_ingress` feature gate off, which fails closed with
/// `external_agent_ingress_closed`), or a hang past `MUSE_SESSION_MESSAGE_TIMEOUT` —
/// returns `Ok(false)` so the caller falls back to the poll file instead of reporting a
/// delivery that never landed anywhere.
fn send_muse_session_message(target: &str, body: &str, dry_run: bool) -> Result<bool> {
    if dry_run {
        return Ok(true);
    }
    let Some(bin) = MuseAdapter.resolve_binary() else {
        return Ok(false);
    };
    let mut child = match Command::new(bin)
        .args(["session-message", "send", "--target", target, "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Ok(false),
    };

    let write_ok = child
        .stdin
        .take()
        .expect("stdin piped above")
        .write_all(body.as_bytes())
        .is_ok();
    if !write_ok {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(false);
    }

    let deadline = Instant::now() + MUSE_SESSION_MESSAGE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(false);
            }
            Err(_) => return Ok(false),
        }
    }
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
                Some(target) => send_muse_session_message(&target, text, dry_run)?,
                None => false,
            };
            if sent {
                (DeliveryAction::SessionMessaged, None)
            } else {
                (
                    DeliveryAction::PolledFile,
                    Some(append_poll_file(paths, run, agent, text, dry_run)?),
                )
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

    /// Guards against test bleed on `PATH`, which the fake-`muse` tests below mutate
    /// process-wide; cargo runs tests in this file on multiple threads.
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Installs a fake `muse` binary at the front of `PATH` that records its invocation
    /// (args on one line, stdin body on the next) to `capture` and restores the original
    /// `PATH` when the returned guard drops.
    fn fake_muse(dir: &std::path::Path, capture: &std::path::Path) -> impl Drop {
        fake_muse_exit(dir, capture, 0)
    }

    /// Like [`fake_muse`], but the script exits `code` after recording its invocation —
    /// used to reproduce muse's own failure mode (`external_agent_ingress_closed` exits
    /// 1 with a JSON body describing why) without a live muse install.
    fn fake_muse_exit(dir: &std::path::Path, capture: &std::path::Path, code: i32) -> impl Drop {
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

        let guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{original}", dir.display()));

        struct Restore(
            #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
            String,
        );
        impl Drop for Restore {
            fn drop(&mut self) {
                std::env::set_var("PATH", &self.1);
            }
        }
        Restore(guard, original)
    }

    /// Points `PATH` at an empty directory (no `muse` binary anywhere on it), guarded by
    /// the same lock as [`fake_muse`] since both mutate process-wide `PATH`.
    fn no_muse_on_path(dir: &std::path::Path) -> impl Drop {
        let guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", dir.display().to_string());

        struct Restore(
            #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
            String,
        );
        impl Drop for Restore {
            fn drop(&mut self) {
                std::env::set_var("PATH", &self.1);
            }
        }
        Restore(guard, original)
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

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-123".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let _guard = fake_muse(tmp.path(), &capture);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::MuseSessionMessage,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::SessionMessaged);
        assert_eq!(d.delivered, 1);
        assert!(!poll_file(&paths, Some("r1"), &ub).exists());

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

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-456".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let _guard = fake_muse(tmp.path(), &capture);

        let ub = agent_ref(Some("r1"), "b");
        let d = nudge(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::MuseSessionMessage,
            "land your artifact",
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::SessionMessaged);
        let args = fs::read_to_string(capture.with_extension("args")).unwrap();
        assert!(args.contains("--target sess-456"), "{args}");
        let stdin = fs::read_to_string(capture.with_extension("stdin")).unwrap();
        assert!(stdin.contains("land your artifact"), "{stdin}");
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

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-789".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let _guard = fake_muse_exit(tmp.path(), &capture, 1);

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

    /// The nudge path takes the same fallback on a rejected send — a nudge is spar's own
    /// control message, and it must not be silently dropped either.
    #[test]
    fn muse_nudge_falls_back_to_poll_file_on_nonzero_exit() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        muse_seed(&paths, 0);

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-abc".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let capture = tmp.path().join("capture");
        let _guard = fake_muse_exit(tmp.path(), &capture, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = nudge(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::MuseSessionMessage,
            "land your artifact",
            false,
        )
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

        let log_path = paths.log_file("r1", "b");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        StreamStats {
            session_id: Some("sess-none".into()),
            ..Default::default()
        }
        .save(&log_path)
        .unwrap();

        let empty_bin_dir = tmp.path().join("no-muse-here");
        fs::create_dir_all(&empty_bin_dir).unwrap();
        let _guard = no_muse_on_path(&empty_bin_dir);

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
}
