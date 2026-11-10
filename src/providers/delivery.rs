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
use crate::state::RunState;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the seam actually did with the claimed messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryAction {
    /// Claude/muse: a Stop-hook `block` payload was built for the hook to relay into
    /// the model.
    StopHookBlock,
    /// Codex: `codex queue --thread` reported success. Not proof the *current* dispatch
    /// saw the message — it does not, see `codex_queue_push`'s doc comment — but it does
    /// mean this thread's *next* `build_resume` dispatch will. The poll file was written
    /// too, same as `PolledFile`; this is just which of the two the push also hit.
    NativePushed,
    /// Written to the slot's poll file, which its role prompt tells it to read before
    /// starting any new major step. Codex's guaranteed drop, whether or not a thread id
    /// is known yet — `codex exec` is single-turn, so this is what actually reaches the
    /// *next* dispatch, not the running one.
    PolledFile,
    /// No injection channel (unknown provider / bare agent): the inbox is left
    /// untouched so the agent claims it itself on its next turn.
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
/// the drain. `run` is still threaded through to scope the poll file to its run.
///
/// `None` never consumes the inbox — an agent with no injection channel reads its own
/// inbox on its next turn, so claiming here would strand the messages. Every other
/// strategy claims (exactly-once) and injects; an empty inbox is a no-op.
///
/// `session_id` is the provider session this agent's current dispatch is running under
/// (`StreamStats::session_id`, e.g. codex's thread id), when the caller found one. The
/// `NativeQueuePollFallback` arm uses it to attempt a best-effort push straight into a
/// live codex session; every other strategy ignores it. The push is not the guarantee —
/// see `codex_queue_push`'s doc comment for why its exit code cannot be trusted, and why
/// the poll file is what actually lands the message.
///
/// `dry_run` stubs the side-effecting injection call (queue append / session prompt)
/// so the run-lifecycle test backend exercises drain + dispatch without touching a
/// live agent. Building the Stop-hook payload is pure and runs in either mode.
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

    let via = match strategy {
        DeliveryStrategy::StopHookInject => "stop_hook",
        DeliveryStrategy::NativeQueuePollFallback => "native_queue",
        DeliveryStrategy::PollFile => "poll_file",
        DeliveryStrategy::None => unreachable!("None handled above"),
    };
    // The PollFile drain claims only what the render would list: `Ack`/`System`
    // copies stay pending, so `delivered` keeps meaning "posted into the live
    // poll file" instead of "claimed by a file-time drain the slot never reads".
    // Copies with an urgent interrupt pending for the live dispatch stay pending
    // under every strategy: the resume-time claim is their only delivery, so no
    // file-time drain or turn-boundary pulse presents them before the resume
    // prompt does.
    let deferred = deferred_for(paths, run, agent);
    let msgs = if strategy == DeliveryStrategy::PollFile {
        bus::inbox_claim(paths, agent, via, |m| {
            bus::poll_lists(m, agent) && !deferred.contains(&m.id)
        })?
    } else {
        bus::inbox_claim(paths, agent, via, |m| !deferred.contains(&m.id))?
    };
    if msgs.is_empty() {
        if strategy == DeliveryStrategy::PollFile
            && !dry_run
            && poll_file(paths, run, agent).exists()
        {
            render_poll_file(paths, run, agent)?;
        }
        return Ok(Delivery {
            strategy,
            action: DeliveryAction::Empty,
            delivered: 0,
            pending: 0,
            payload: None,
        });
    }
    let mut delivered = msgs.len();

    let (action, payload) = match strategy {
        DeliveryStrategy::StopHookInject => {
            for m in &msgs {
                bus::record_receipt(paths, &m.id, agent, bus::ReceiptEvent::Read, via);
            }
            (DeliveryAction::StopHookBlock, Some(block_payload(&msgs)))
        }
        DeliveryStrategy::NativeQueuePollFallback => {
            let text = render_reason(&msgs);
            let (action, _path) =
                codex_native_deliver(paths, run, agent, session_id, &text, dry_run)?;
            (action, None)
        }
        DeliveryStrategy::PollFile => {
            if !dry_run {
                delivered = render_poll_file(paths, run, agent)?;
            }
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
    let mut s = String::from("New swarm messages (act on them, then continue):");
    for m in msgs {
        s.push_str(&format!("\n- [{:?}] from {}: {}", m.kind, m.from, m.body));
    }
    s
}

const POLL_SECTION_START: &str = "<!-- spar:poll:start -->";
const POLL_SECTION_END: &str = "<!-- spar:poll:end -->";

/// Rewrite the slot's poll file from its inbox: one line per pending message,
/// oldest first. The inbox is the only source, a message withdrawn (including
/// superseded, even when a stale redelivery re-files its copy), expired or
/// acked stops being listed, wherever its copies were.
///
/// Only action items render: messages addressed to the slot, plus chat
/// broadcasts from humans, peer slots and the orchestrator (agent-to-agent
/// coordination and the default `--to` reach every slot through here). Acks and
/// system prompts stay in the inbox and the bus log but never become poll-file
/// action items. Text outside the section markers (budget and time
/// nudges) is preserved.
///
/// Escape the poll-file section markers inside message bodies: a body containing
/// the literal marker would corrupt the file on the next render, which splits
/// the previous file on those markers to preserve the operator section.
fn escape_poll_markers(body: &str) -> String {
    body.replace(POLL_SECTION_START, "<!-- spar poll start -->")
        .replace(POLL_SECTION_END, "<!-- spar poll end -->")
}

/// Returns how many messages are listed, so delivery counts what the slot can
/// actually read rather than what was claimed.
///
/// A message leaves the file once the slot acks it. Without `requires-ack` it
/// also leaves once the slot has finished a dispatch that started after the
/// round it was filed in, so the file cannot grow without bound yet a render
/// mid-turn never drops what the slot may not have opened. `requires-ack`
/// items survive until the slot acks them. Acks and system control traffic
/// never list.
/// Msg ids the renders and claims must skip: urgent requests filed for the live
/// dispatch stay inbox-pending until the resume-time claim consumes them.
fn deferred_for(paths: &SparPaths, run: Option<&str>, agent: &str) -> HashSet<String> {
    let Some(r) = run else {
        return HashSet::new();
    };
    crate::markers::urgent_deferred_msg_ids(paths, r, slot_part(r, agent))
        .into_iter()
        .collect()
}

pub(crate) fn render_poll_file(paths: &SparPaths, run: Option<&str>, agent: &str) -> Result<usize> {
    let _lock = lock_poll_file(paths, run, agent)?;
    let deferred = deferred_for(paths, run, agent);
    // The ack scan runs under the poll-file lock: an ack filed between the scan
    // and the write would otherwise lose to a stale render.
    let mut scan = bus::scan_for_render(paths, run, &HashSet::from([agent.to_string()]));
    let acked = scan.acked.remove(agent).unwrap_or_default();
    let superseded = scan.superseded;
    let mut gate = bus::ExpiryGate::new();
    let path = poll_file(paths, run, agent);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut seen = HashSet::new();
    let mut msgs: Vec<bus::BusMessage> = bus::filed_copies(paths, agent)
        .into_iter()
        .filter(|m| {
            if !seen.insert(m.id.clone()) {
                return false;
            }
            if !bus::poll_lists(m, agent) {
                return false;
            }
            if deferred.contains(&m.id) {
                return false;
            }
            if superseded.contains(&m.id) {
                return false;
            }
            if acked.contains(&m.id) {
                return false;
            }
            if gate.expired(paths, m) {
                return false;
            }
            if !m.requires_ack {
                if let (Some(filed), Some(r)) = (m.filed_round, m.run.as_deref()) {
                    // The item may leave once the slot finished a dispatch that
                    // started after it was filed. A running slot keeps it.
                    if gate.turn_finished_after(paths, r, agent, filed) {
                        return false;
                    }
                }
            }
            true
        })
        .collect();
    msgs.sort_by_key(|m| m.ts);
    let ack_cmd = match run {
        Some(r) => {
            let slot = agent.split_once(':').map(|(_, s)| s).unwrap_or(agent);
            format!("`spar bus ack <id> --run {r} --from {slot}`")
        }
        None => format!("`spar bus ack <id> --from {agent}`"),
    };
    let mut section = format!(
        "## Pending swarm messages (ack handled items with {ack_cmd} so they leave the file; act on the rest, then continue)\n"
    );
    if msgs.is_empty() {
        section.push_str("\n_Nothing waiting._\n");
    } else {
        for m in &msgs {
            section.push_str(&format!(
                "\n- [{:?}] from {} ({}): {}",
                m.kind,
                escape_poll_markers(&m.from),
                m.id,
                escape_poll_markers(&m.body)
            ));
        }
        section.push('\n');
    }
    let block = format!("{POLL_SECTION_START}\n{section}{POLL_SECTION_END}\n");
    let prev = fs::read_to_string(&path).unwrap_or_default();
    let next = match prev.split_once(POLL_SECTION_START) {
        Some((head, rest)) => match rest.split_once(POLL_SECTION_END) {
            Some((_, tail)) => format!("{head}{block}{}", tail.trim_start_matches('\n')),
            None => format!("{prev}\n{block}"),
        },
        None if prev.is_empty() => block,
        None => format!("{}\n{block}", prev.trim_end()),
    };
    fs::write(path.with_extension("tmp"), &next)?;
    fs::rename(path.with_extension("tmp"), &path)?;
    let listed: HashSet<String> = msgs.iter().map(|m| m.id.clone()).collect();
    bus::prune_aged_claims(paths, agent, &seen, &listed, &acked, &superseded);
    Ok(msgs.len())
}

/// The lock serializing poll-file writers: the render's read-modify-rename and
/// the nudge appender hold it, so a render never splices its section into a
/// half-appended nudge and two renders never interleave. Held until dropped.
pub(crate) fn lock_poll_file(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
) -> Result<std::fs::File> {
    let lock_path = poll_file(paths, run, agent).with_extension("lock");
    let f = bus::open_lockfile(&lock_path)?;
    bus::lock_exclusive(&f)?;
    Ok(f)
}

/// Re-render every PollFile slot's poll file on `run_id`: the round-start and
/// replan sweep that drops acked, expired and aged-out messages with no new
/// filing. Dry-run runs never get poll files, so there is nothing to sweep.
pub fn refresh_run_poll_files(paths: &SparPaths, run_id: &str) {
    let Ok(state) = RunState::load(paths, run_id) else {
        return;
    };
    if state.dry_run {
        return;
    }
    let agents: HashSet<String> = state
        .slots
        .iter()
        .filter(|s| strategy_for_slot(&state, &s.id) == DeliveryStrategy::PollFile)
        .map(|s| bus::agent_ref(Some(run_id), &s.id))
        .collect();
    for agent in &agents {
        if let Err(e) = render_poll_file(paths, Some(run_id), agent) {
            eprintln!("warning: poll-file refresh for {agent} failed: {e:#}");
        }
    }
}

/// Push `text` into the codex thread `session_id`. Best-effort and *not* a delivery
/// guarantee for *this* dispatch — the caller always also writes the poll file:
///
/// - Exit 0 is not proof the model saw the message. Verified against codex 0.152.0:
///   `codex queue --thread <id>` against a two-day-dead thread's rollout still exits 0
///   with `Queued message ... for thread ...` on stdout.
/// - A message queued against a genuinely running thread does not land in that same
///   turn. Live probe against codex 0.152.0: pushing ~6s into a live `codex exec` turn
///   produced exactly one `turn.completed` for the whole dispatch and the queued text
///   never appeared anywhere in the stream — no aborted follow-up turn, nothing.
/// - It does land at that thread's *next* dispatch. Live probe: queuing a message to an
///   already-exited thread, then `codex exec resume <id>` on it, produced a single turn
///   whose reply addressed both the queued message and the new resume prompt. This is
///   what `build_resume` (DECISIONS.md O63) turns into a real channel: a nudge queued
///   here surfaces the next time `executor::build_dispatch_command` resumes this slot's
///   thread, not mid-flight in the dispatch it was queued against.
///
/// stdio is nulled: `codex queue` writes its own status line to stdout and its error to
/// stderr, which would otherwise land inside spar's own `--json` output stream.
///
/// Bounded to `CODEX_QUEUE_PUSH_TIMEOUT`: `nudge` runs on the orchestrator's own
/// `try_wait` poll loop (`process::run_captured`'s tick, which also enforces that slot's
/// hard-ceiling kill), so a `Command::status()` with no timeout of its own would let a
/// wedged `codex queue` process stall that entire loop — no hard-ceiling kill, no
/// liveness beat — for as long as the hang lasts. Ordinary cost is well under a second
/// (measured ~0.46s against a rejected thread id on codex 0.152.0); the bound only
/// matters for the unresponsive tail. Halved from an initial 15s: `NudgeWatch::tick`
/// can call `send` twice in one tick (`check_time` then `check_tokens`, both crossed at
/// once), so two wedged pushes at the old bound cost 30s against the 30s poll cadence
/// (`nudge::POLL_SECS`) that timeout was reasoned against; at 7s the same worst case is
/// 14s.
const CODEX_QUEUE_PUSH_TIMEOUT: Duration = Duration::from_secs(7);

fn codex_queue_push(session_id: &str, text: &str) -> bool {
    let bin = crate::providers::adapter_named("codex")
        .and_then(|a| a.resolve_binary())
        .unwrap_or_else(|| PathBuf::from("codex"));
    let mut child = match Command::new(bin)
        .args(["queue", "--thread", session_id, "--message", text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = Instant::now() + CODEX_QUEUE_PUSH_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return false,
        }
    }
}

/// Codex's `NativeQueuePollFallback` delivery: attempt the best-effort native push when
/// a thread id is known, then always write the poll file, the one guaranteed drop —
/// whether or not a session id is known, whether or not the push succeeded, whether or
/// not this is a dry run. Splitting the push out behind `push` lets tests exercise both
/// branches without shelling out to the real `codex` binary.
fn codex_native_deliver(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    session_id: Option<&str>,
    text: &str,
    dry_run: bool,
) -> Result<(DeliveryAction, PathBuf)> {
    codex_native_deliver_with(
        codex_queue_push,
        paths,
        run,
        agent,
        session_id,
        text,
        dry_run,
    )
}

fn codex_native_deliver_with(
    push: impl FnOnce(&str, &str) -> bool,
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    session_id: Option<&str>,
    text: &str,
    dry_run: bool,
) -> Result<(DeliveryAction, PathBuf)> {
    if dry_run {
        let path = append_poll_file(paths, run, agent, text, dry_run)?;
        return Ok((DeliveryAction::PolledFile, path));
    }
    let pushed = session_id.is_some_and(|sid| push(sid, text));
    let path = append_poll_file(paths, run, agent, text, false)?;
    let action = if pushed {
        DeliveryAction::NativePushed
    } else {
        DeliveryAction::PolledFile
    };
    Ok((action, path))
}

/// Resolve `slot`'s delivery strategy from its run slot's provider adapter. A slot
/// with no adapter has no injection channel.
pub fn strategy_for_slot(state: &RunState, slot: &str) -> DeliveryStrategy {
    state
        .slots
        .iter()
        .find(|s| s.id == slot)
        .and_then(|s| crate::providers::adapter_named(&s.provider))
        .map(|a| a.delivery_strategy())
        .unwrap_or(DeliveryStrategy::None)
}

/// File-time fan-out for PollFile slots: [`crate::bus`] calls this on every inbox
/// filing, so every producer (CLI sends, broadcasts, acks, ack redeliveries)
/// lands in a PollFile slot's poll file, not just the producers that remember
/// to drain. Each recipient's whole inbox drains through [`deliver`], which also
/// picks up copies filed while the run state was unreadable. Other strategies
/// keep their own pulse; bare ids resolve to no slot and stay inbox-queued.
/// A PollFile slot with no running turn (finished, or never dispatched) is not
/// drained: its copies stay pending and the log reads `queued` until a turn
/// that can read the poll file starts.
/// Best effort with a stderr line, never an error: the filing already succeeded,
/// the inbox stays the durable queue, and the next filing retries the drain.
/// Skipped for dry-run runs: no live slot polls their files, so the messages
/// stay in the inbox for inspection and no poll file is written. Gated on the run's
/// own `dry_run` flag, never the sender's `SPAR_DRY_RUN` env: an ambient export
/// in the sender's shell must not silence delivery to a live run's slots. The
/// drain writes for real here (`deliver` is called with dry-run off); the stub
/// only applies to the explicit turn-boundary `bus deliver` under dry runs.
pub fn drain_poll_files(paths: &SparPaths, targets: &[String]) {
    let mut by_run: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for t in targets {
        if let Some((run, _)) = t.split_once(':') {
            by_run.entry(run).or_default().push(t);
        }
    }
    for (run, agents) in by_run {
        let Ok(state) = RunState::load(paths, run) else {
            continue;
        };
        if state.dry_run {
            continue;
        }
        let poll_agents: Vec<&str> = agents
            .into_iter()
            .filter(|agent| {
                let short = agent.strip_prefix(&format!("{run}:")).unwrap_or(agent);
                strategy_for_slot(&state, short) == DeliveryStrategy::PollFile
                    && state
                        .slots
                        .iter()
                        .any(|s| s.id == short && s.status == crate::state::SlotStatus::Running)
            })
            .collect();
        for agent in poll_agents {
            if let Err(e) = deliver(
                paths,
                Some(run),
                agent,
                DeliveryStrategy::PollFile,
                None,
                false,
            ) {
                eprintln!("warning: poll-file fan-out to {agent} failed: {e:#}");
            }
        }
    }
}

/// Claim a PollFile slot's backlog as its dispatch starts. A message sent while
/// the slot was idle stays `queued` until a turn that can read the poll file
/// starts; the round-start sweep renders but never claims, and PollFile slots
/// have no turn-end deliver hook, so without this the copy would sit in the
/// file with the word stuck at `queued` for the whole turn. Skipped for dry
/// runs (no live slot reads) and non-PollFile strategies. Best effort with a
/// stderr line, like the file-time fan-out.
pub fn drain_slot_backlog(paths: &SparPaths, state: &RunState, slot_id: &str) {
    if state.dry_run || strategy_for_slot(state, slot_id) != DeliveryStrategy::PollFile {
        return;
    }
    let agent = bus::agent_ref(Some(&state.id), slot_id);
    if let Err(e) = deliver(
        paths,
        Some(&state.id),
        &agent,
        DeliveryStrategy::PollFile,
        None,
        false,
    ) {
        eprintln!("warning: poll-file backlog drain for {slot_id} failed: {e:#}");
    }
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
    let _lock = lock_poll_file(paths, run, agent)?;
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
/// the agent reads at a turn boundary instead: a Stop-hook adapter's hook drains its
/// inbox at the turn boundary, and everything else reads the poll file its prompt
/// names.
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
                        persist: false,
                        expires_after_round: None,
                        filed_round: None,
                        meta: std::collections::HashMap::new(),
                    },
                    // Nudges are spar's own control channel; they must not be dropped
                    // because the run's agents have been chatty.
                    bus::MessageBudget::Chatty,
                )?;
            }
            (DeliveryAction::StopHookBlock, None)
        }
        // See `deliver`'s NativeQueuePollFallback arm: the push is best-effort, the poll
        // file is the guarantee, whether or not a session id is known.
        DeliveryStrategy::NativeQueuePollFallback => {
            let (action, path) =
                codex_native_deliver(paths, run, agent, session_id, text, dry_run)?;
            (action, Some(path))
        }
        // `None` is an adapter with no channel at all (unknown provider / bare
        // agent); the poll file is still the best available nudge drop. This is
        // deliberately looser than `deliver`, which leaves a `None` inbox
        // untouched for the agent to claim itself: a nudge names one file the
        // agent already polls, while a deliver is the whole drain.
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
    fn poll_file_drain_defers_copies_with_a_pending_urgent() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(&paths, Some("r1"), "p", Some("cli:pi"), Some("native-cli")).unwrap();
        let live = crate::process::PidToken {
            pid: 4242,
            starttime: Some(9),
        };
        crate::markers::write_pid(&paths, "r1", "p", live).unwrap();
        // The urgent request is filed before the message, the way the sender
        // orders it: the file-time drain must skip the copy, never claim or
        // render it, and leave it for the resume-time claim.
        let id = bus::new_id();
        crate::markers::write_urgent_interrupt(
            &paths,
            "r1",
            "p",
            &crate::markers::UrgentInterrupt {
                msg_id: id.clone(),
                from: "@human".into(),
                body: "now-now".into(),
                ts: chrono::Utc::now(),
                target_pid: live.pid,
                target_starttime: live.starttime,
            },
        )
        .unwrap();
        bus::send(
            &paths,
            bus::BusMessage {
                id: id.clone(),
                ts: chrono::Utc::now(),
                from: "a".into(),
                to: "p".into(),
                kind: bus::MsgKind::Chat,
                body: "now-now".into(),
                run: Some("r1".into()),
                subject: None,
                refs: Default::default(),
                requires_ack: false,
                persist: false,
                expires_after_round: None,
                filed_round: None,
                meta: Default::default(),
            },
            MessageBudget::Chatty,
        )
        .unwrap();

        let agent = agent_ref(Some("r1"), "p");
        assert_eq!(bus::inbox(&paths, &agent).unwrap().len(), 1);
        let file = poll_file(&paths, Some("r1"), &agent);
        assert!(
            !file.exists() || !std::fs::read_to_string(&file).unwrap().contains("now-now"),
            "the urgent body must not reach the poll file"
        );
        let claimed = bus::inbox_claim(&paths, &agent, "urgent", |m| m.id == id).unwrap();
        assert_eq!(claimed.len(), 1, "the resume-time claim consumes it once");
        assert!(bus::inbox(&paths, &agent).unwrap().is_empty());
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
        assert_eq!(
            bus::inbox_claim(&paths, &ub, "test", |_| true)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn poll_file_deliver_writes_the_body_where_the_agent_polls() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(&paths, Some("r1"), "p", Some("cli:pi"), Some("native-cli")).unwrap();
        chat(
            &paths,
            Some("r1"),
            "a",
            "p",
            "operator says go left",
            MessageBudget::Chatty,
        )
        .unwrap();

        let agent = agent_ref(Some("r1"), "p");
        let d = deliver(
            &paths,
            Some("r1"),
            &agent,
            DeliveryStrategy::PollFile,
            None,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        let body = std::fs::read_to_string(poll_file(&paths, Some("r1"), &agent)).unwrap();
        assert!(body.contains("operator says go left"), "{body}");
        assert_eq!(bus::inbox(&paths, &agent).unwrap().len(), 0);
    }

    #[test]
    fn dry_run_stubs_the_poll_file_write_but_still_drains() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 2);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::PollFile,
            None,
            true,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 2);
        assert!(!poll_file(&paths, Some("r1"), &ub).exists());
        // But the drain is real (exactly-once): nothing remains to claim.
        assert!(bus::inbox_claim(&paths, &ub, "test", |_| true)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn dry_run_codex_reports_polled_file_without_shelling_out() {
        // dry_run must never spawn the real `codex` binary, and must never claim a push
        // it did not attempt — the guaranteed channel (poll file) is the honest answer.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::NativeQueuePollFallback,
            Some("thread-abc"),
            true,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        assert!(
            !poll_file(&paths, Some("r1"), &ub).exists(),
            "dry run writes nothing"
        );
    }

    #[test]
    fn no_session_id_yet_lands_in_the_poll_file() {
        // Before `thread.started` is captured, codex has no session id. The claimed
        // message must still land somewhere codex's role prompt reads. No real `codex`
        // binary is spawned here: with `session_id: None` the push is never attempted.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        seed(&paths, 1);

        let ub = agent_ref(Some("r1"), "b");
        let d = deliver(
            &paths,
            Some("r1"),
            &ub,
            DeliveryStrategy::NativeQueuePollFallback,
            None,
            false,
        )
        .unwrap();
        assert_eq!(d.action, DeliveryAction::PolledFile);
        assert_eq!(d.delivered, 1);
        let body = fs::read_to_string(poll_file(&paths, Some("r1"), &ub)).unwrap();
        assert!(body.contains("New swarm messages"), "{body}");
    }

    #[test]
    fn native_push_success_reports_pushed_and_still_writes_the_poll_file() {
        // The push closure is injected so this never touches the real `codex` binary.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let (action, path) = codex_native_deliver_with(
            |_sid, _text| true,
            &paths,
            Some("r1"),
            "b",
            Some("thread-abc"),
            "hello",
            false,
        )
        .unwrap();
        assert_eq!(action, DeliveryAction::NativePushed);
        assert!(path.is_file());
        assert!(fs::read_to_string(&path).unwrap().contains("hello"));
    }

    #[test]
    fn native_push_failure_still_lands_in_the_poll_file() {
        // A push failure (rejected thread id, dead session, spawn error, or a timeout —
        // see `codex_queue_push`'s bound) must not lose the message: it has to land in
        // the poll file, the channel a codex role prompt is actually told to read,
        // regardless of what the push reported. Stubbed so no real `codex` binary runs.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let (action, path) = codex_native_deliver_with(
            |_sid, _text| false,
            &paths,
            Some("r1"),
            "b",
            Some("00000000-0000-0000-0000-000000000000"),
            "hello",
            false,
        )
        .unwrap();
        assert_eq!(action, DeliveryAction::PolledFile);
        assert!(fs::read_to_string(&path).unwrap().contains("hello"));
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

    #[test]
    fn refresh_rebuilds_poll_files_for_run_slots() {
        // The quota early-return and round start share one refresh path: a deleted
        // poll file comes back with the slot's pending items still listed.
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.slots.push(crate::executor::init_slot(
            "impl",
            "cli:agy",
            crate::state::SlotRole::Implementer,
        ));
        state.slots[0].status = crate::state::SlotStatus::Running;
        state.save(&paths).unwrap();

        let slot = agent_ref(Some("r1"), "impl");
        chat(
            &paths,
            Some("r1"),
            "human",
            &slot,
            "quota note",
            MessageBudget::Chatty,
        )
        .unwrap();
        let file = poll_file(&paths, Some("r1"), &slot);
        assert!(file.is_file());
        std::fs::remove_file(&file).unwrap();

        refresh_run_poll_files(&paths, "r1");
        let body = std::fs::read_to_string(&file).unwrap();
        assert!(body.contains("quota note"), "{body}");
    }

    #[test]
    fn dispatch_start_claims_the_idle_backlog() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.slots.push(crate::executor::init_slot(
            "impl",
            "cli:agy",
            crate::state::SlotRole::Implementer,
        ));
        state.slots.push(crate::executor::init_slot(
            "rev",
            "cli:claude",
            crate::state::SlotRole::Reviewer,
        ));
        state.save(&paths).unwrap();

        let poll_slot = agent_ref(Some("r1"), "impl");
        let hook_slot = agent_ref(Some("r1"), "rev");
        chat(
            &paths,
            Some("r1"),
            "human",
            &poll_slot,
            "idle ruling",
            MessageBudget::Chatty,
        )
        .unwrap();
        chat(
            &paths,
            Some("r1"),
            "human",
            &hook_slot,
            "idle review note",
            MessageBudget::Chatty,
        )
        .unwrap();
        let claimed_count = |agent: &str| {
            bus::bus_root(&paths)
                .join("inbox")
                .join(agent)
                .join("claimed")
                .read_dir()
                .map(|r| r.count())
                .unwrap_or(0)
        };
        assert_eq!(
            claimed_count(&poll_slot),
            0,
            "no claim into a file no turn can read"
        );

        state.slots[0].status = crate::state::SlotStatus::Running;
        state.slots[1].status = crate::state::SlotStatus::Running;
        drain_slot_backlog(&paths, &state, "impl");
        drain_slot_backlog(&paths, &state, "rev");
        assert_eq!(claimed_count(&poll_slot), 1);
        assert_eq!(claimed_count(&hook_slot), 0);
        let body = std::fs::read_to_string(poll_file(&paths, Some("r1"), &poll_slot)).unwrap();
        assert!(body.contains("idle ruling"), "{body}");

        state.dry_run = true;
        chat(
            &paths,
            Some("r1"),
            "human",
            &poll_slot,
            "dry ruling",
            MessageBudget::Chatty,
        )
        .unwrap();
        drain_slot_backlog(&paths, &state, "impl");
        assert_eq!(claimed_count(&poll_slot), 1);
    }
}
