//! Turn-boundary delivery seam: the adapter-level counterpart to `presence.rs`.
//!
//! When a slot reaches a turn boundary the orchestrator drains its inbox with the
//! Stage 1 exactly-once claim and hands the claimed messages to the adapter's
//! [`DeliveryStrategy`]. Every strategy-specific mechanism lives here behind the seam;
//! the command layer only resolves the strategy (from the slot's adapter) and calls
//! [`deliver`]. The orchestrator never learns which provider it is talking to.

use super::DeliveryStrategy;
use crate::bus::{self, BusMessage};
use crate::paths::SparPaths;
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What the seam actually did with the claimed messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryAction {
    /// Claude: a Stop-hook `block` payload was built for the hook to relay into the model.
    StopHookBlock,
    /// Codex: `codex queue --thread` reported success. Not itself proof the model saw
    /// the message (see `codex_queue_push`'s doc comment) — the poll file was written
    /// too, same as `PolledFile`, this is just which of the two the push also hit.
    NativePushed,
    /// Grok: claimed messages appended to the durable turn-boundary queue (unread until
    /// its live push channel lands). Also codex's fallback, when no thread id has been
    /// captured yet for this dispatch.
    Queued,
    /// opencode: claimed messages appended to the durable queue for the session flush.
    Prompted,
    /// Written to the slot's poll file, which its role prompt tells it to read before
    /// starting any new major step. Codex's guaranteed drop once a thread id is known —
    /// `codex exec` is single-turn, so this is what actually reaches the *next*
    /// dispatch, not the running one.
    PolledFile,
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
/// `session_id` is the provider session this agent's current dispatch is running under
/// (`StreamStats::session_id`, e.g. codex's thread id), when the caller found one. The
/// `NativeQueue` arm uses it to attempt a best-effort push straight into a live codex
/// session; every other strategy ignores it. The push is not the guarantee — see
/// `codex_queue_push`'s doc comment for why its exit code cannot be trusted, and why the
/// poll file is what actually lands the message.
///
/// `dry_run` stubs the side-effecting injection call (queue append / session prompt) so
/// the run-lifecycle test backend exercises drain + dispatch without touching a live
/// agent. Building the Stop-hook payload is pure and runs in either mode.
pub fn deliver(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    strategy: DeliveryStrategy,
    session_id: Option<&str>,
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
            let text = render_reason(&msgs);
            // Grok never captures a session id (its stream carries none), so `Some`
            // is codex-shaped today: a real one lands only once codex's
            // `thread.started` has been parsed. With no id yet this falls through to
            // the durable queue file, same as grok always does.
            match session_id {
                Some(sid) if !dry_run => {
                    // `codex_queue_push`'s exit code is not proof of delivery (see
                    // its doc comment), so the poll file — the channel a codex role
                    // prompt is actually told to read — is the guaranteed drop
                    // regardless of what the push reports. The push stays best
                    // effort on top of it, in case a future dispatch shape (this one
                    // is single-turn `codex exec`) makes it land for real.
                    let pushed = codex_queue_push(sid, &text);
                    append_poll_file(paths, run, agent, &text, dry_run)?;
                    let action = if pushed {
                        DeliveryAction::NativePushed
                    } else {
                        DeliveryAction::PolledFile
                    };
                    (action, None)
                }
                Some(_) => {
                    // dry run: stub the push, still record the guaranteed channel.
                    append_poll_file(paths, run, agent, &text, dry_run)?;
                    (DeliveryAction::NativePushed, None)
                }
                None => {
                    enqueue(paths, run, agent, &msgs, dry_run)?;
                    (DeliveryAction::Queued, None)
                }
            }
        }
        DeliveryStrategy::SdkPrompt => {
            enqueue(paths, run, agent, &msgs, dry_run)?;
            (DeliveryAction::Prompted, None)
        }
        DeliveryStrategy::PollFile => {
            append_poll_file(paths, run, agent, &render_reason(&msgs), dry_run)?;
            (DeliveryAction::PolledFile, None)
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

/// Push `text` into the codex thread `session_id`. Best-effort and *not* a delivery
/// guarantee — the caller always also writes the poll file:
///
/// - Exit 0 is not proof the model saw the message. Verified against codex 0.152.0:
///   `codex queue --thread <id>` against a two-day-dead thread's rollout still exits 0
///   with `Queued message ... for thread ...` on stdout.
/// - Even against a genuinely running thread, `codex exec` is single-turn: it exits
///   right after completing its one assigned task, so a message queued mid-turn opens a
///   follow-up turn that gets aborted before the model sees it. A live probe (queued 15s
///   into a ~70s turn) showed `task_complete`, then a new `task_started` 43ms later
///   carrying the queued text, then `turn_aborted` (`interrupted`) 44ms after that.
///
/// stdio is nulled: `codex queue` writes its own status line to stdout and its error to
/// stderr, which would otherwise land inside spar's own `--json` output stream.
fn codex_queue_push(session_id: &str, text: &str) -> bool {
    let bin = crate::providers::adapter_named("codex")
        .and_then(|a| a.resolve_binary())
        .unwrap_or_else(|| PathBuf::from("codex"));
    Command::new(bin)
        .args(["queue", "--thread", session_id, "--message", text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Append one nudge line to the durable turn-boundary queue file — grok's channel, and
/// codex's fallback until a thread id is captured. `dry_run` stubs the write.
fn write_queue_file(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    text: &str,
    dry_run: bool,
) -> Result<std::path::PathBuf> {
    let path = queue_path(paths, run, agent);
    if dry_run {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(f, "{}", serde_json::json!({ "nudge": text }))?;
    Ok(path)
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
    session_id: Option<&str>,
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
        // See `deliver`'s NativeQueue arm: the push is best-effort, the poll file is
        // the guarantee, once a session id is known.
        DeliveryStrategy::NativeQueue => match session_id {
            Some(sid) if !dry_run => {
                let pushed = codex_queue_push(sid, text);
                let path = append_poll_file(paths, run, agent, text, dry_run)?;
                let action = if pushed {
                    DeliveryAction::NativePushed
                } else {
                    DeliveryAction::PolledFile
                };
                (action, Some(path))
            }
            Some(_) => (
                DeliveryAction::NativePushed,
                Some(append_poll_file(paths, run, agent, text, dry_run)?),
            ),
            None => (
                DeliveryAction::Queued,
                Some(write_queue_file(paths, run, agent, text, dry_run)?),
            ),
        },
        DeliveryStrategy::SdkPrompt => (
            DeliveryAction::Queued,
            Some(write_queue_file(paths, run, agent, text, dry_run)?),
        ),
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
            None,
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
            None,
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
            None,
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
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::SdkPrompt,
            None,
            false,
        )
        .unwrap();
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
        let d = deliver(&paths, Some("r1"), &ub, DeliveryStrategy::None, None, false).unwrap();
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
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::NativeQueue,
            None,
            true,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::Queued);
        assert_eq!(d.delivered, 2);
        // Injection call stubbed: no queue file written.
        assert!(!queue_path(&paths, Some("r1"), &ub).exists());
        // But the drain is real (exactly-once): nothing remains to claim.
        assert!(bus::inbox_claim(&paths, &ub).unwrap().is_empty());
    }

    #[test]
    fn dry_run_native_push_reports_pushed_without_shelling_out() {
        // With a session id known, `dry_run` must still never spawn the real `codex`
        // binary — same contract as the file-queue stub above, one step earlier.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::NativeQueue,
            Some("thread-abc"),
            true,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::NativePushed);
        assert_eq!(d.delivered, 1);
        assert!(!queue_path(&paths, Some("r1"), &ub).exists());
    }

    #[test]
    fn native_push_failure_still_lands_in_the_poll_file() {
        // A real (non-dry-run) push against a thread id that cannot exist: codex
        // rejects it (or, on a box with no `codex` on PATH, the spawn itself fails).
        // Either way `codex_queue_push` returns `false`. The message must not be lost:
        // it has to land in the poll file, the channel a codex role prompt is actually
        // told to read, regardless of what the push reported.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::NativeQueue,
            Some("00000000-0000-0000-0000-000000000000"),
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        // Not the durable queue file — nothing reads that one for codex.
        assert!(!queue_path(&paths, Some("r1"), &ub).exists());
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("New swarm messages"), "{body}");
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
            None,
            false,
        )
        .unwrap();
        deliver(
            &paths,
            Some("rA"),
            &ua,
            DeliveryStrategy::NativeQueue,
            None,
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
            None,
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
            None,
            false,
        )
        .unwrap();
        assert_eq!(to_bare.delivered, 1);
        assert!(to_bare.payload.unwrap().contains("hi bare"));
    }
}
