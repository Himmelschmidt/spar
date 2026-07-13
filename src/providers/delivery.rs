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
/// `run` scopes the drain to the draining slot's run so a slot never claims another
/// concurrent run's messages (slot ids are not unique across runs). It is threaded
/// straight through to [`bus::inbox_claim`] / [`bus::inbox`].
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
        let pending = bus::inbox(paths, run, agent)?.len();
        return Ok(Delivery {
            strategy,
            action: DeliveryAction::LeftForInbox,
            delivered: 0,
            pending,
            payload: None,
        });
    }

    let msgs = bus::inbox_claim(paths, run, agent)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{chat, join, MessageBudget};
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

        let d = deliver(
            &paths,
            Some("r1"),
            "b",
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
            "b",
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

        let d = deliver(
            &paths,
            Some("r1"),
            "b",
            DeliveryStrategy::NativeQueue,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::Queued);
        assert_eq!(d.delivered, 3);
        assert!(d.payload.is_none());
        let queued = fs::read_to_string(queue_path(&paths, Some("r1"), "b")).unwrap();
        assert_eq!(queued.lines().filter(|l| !l.is_empty()).count(), 3);
    }

    #[test]
    fn sdk_prompt_appends_durable_queue() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 1);

        let d = deliver(&paths, Some("r1"), "b", DeliveryStrategy::SdkPrompt, false).unwrap();
        assert_eq!(d.action, DeliveryAction::Prompted);
        assert_eq!(d.delivered, 1);
        assert!(queue_path(&paths, Some("r1"), "b").is_file());
    }

    #[test]
    fn none_leaves_inbox_untouched() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 2);

        let d = deliver(&paths, Some("r1"), "b", DeliveryStrategy::None, false).unwrap();
        assert_eq!(d.action, DeliveryAction::LeftForInbox);
        assert_eq!(d.delivered, 0);
        assert_eq!(d.pending, 2);
        // The agent must still be able to claim them itself.
        assert_eq!(bus::inbox_claim(&paths, Some("r1"), "b").unwrap().len(), 2);
    }

    #[test]
    fn dry_run_stubs_the_queue_write_but_still_drains() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 2);

        let d = deliver(&paths, Some("r1"), "b", DeliveryStrategy::NativeQueue, true).unwrap();
        assert_eq!(d.action, DeliveryAction::Queued);
        assert_eq!(d.delivered, 2);
        // Injection call stubbed: no queue file written.
        assert!(!queue_path(&paths, Some("r1"), "b").exists());
        // But the drain is real (exactly-once): nothing remains to claim.
        assert!(bus::inbox_claim(&paths, Some("r1"), "b")
            .unwrap()
            .is_empty());
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

        deliver(
            &paths,
            Some("rB"),
            "b",
            DeliveryStrategy::NativeQueue,
            false,
        )
        .unwrap();
        deliver(
            &paths,
            Some("rA"),
            "b",
            DeliveryStrategy::NativeQueue,
            false,
        )
        .unwrap();

        let qa = fs::read_to_string(queue_path(&paths, Some("rA"), "b")).unwrap();
        let qb = fs::read_to_string(queue_path(&paths, Some("rB"), "b")).unwrap();
        assert!(
            qa.contains("for run A") && !qa.contains("for run B"),
            "run A queue: {qa}"
        );
        assert!(
            qb.contains("for run B") && !qb.contains("for run A"),
            "run B queue: {qb}"
        );
    }
}
