//! Workspace-scoped swarm bus (A2A). Replaces thin mailbox as the coordination plane.
//!
//! W5 re-scope: the bus lives at the workspace root (`.spar/bus/`) and is keyed by
//! `agent_id`, independent of any run. `run` is demoted to an optional tag carried on
//! each [`BusMessage`]/[`Presence`] record, used only for grouping and run-scoped
//! views (pass `Some(run)` to filter, `None` for the whole workspace). Bare agents
//! spawned from the Composer (with a `SPAR_AGENT_ID` but no run) get a first-class
//! inbox + presence exactly like a run slot, and a bare agent and a run slot can
//! address each other directly by id.
//!
//! Run-tagged events/presence are also mirrored into the legacy
//! `.spar/runs/<id>/bus/` layout for back-compat with any reader still watching that
//! path. TODO(W5): remove the run-dir mirror once all readers consume the workspace
//! bus directly.
use crate::paths::SparPaths;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// Reserved recipient id for messages that need a human's attention. Every such
/// message is surfaced in the TUI alert panel (always on, zero config) and, if the
/// operator wired one, pushed to an external notifier (`[notify]` in config).
pub const HUMAN: &str = "@human";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgKind {
    Chat,
    Status,
    Blocked,
    Unblocked,
    Contract,
    ReviewFinding,
    TaskClaim,
    TaskDone,
    Steer,
    Ack,
    System,
    Hello,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub kind: MsgKind,
    pub body: String,
    /// Optional run this message belongs to (W5). A grouping/filtering tag only — the
    /// primary key is `from`/`to` agent ids. `None` for bare (run-less) traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default)]
    pub refs: MsgRefs,
    #[serde(default)]
    pub requires_ack: bool,
    #[serde(default)]
    pub meta: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MsgRefs {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Id of the message this one answers. An `Ack` sets it to the id of the
    /// `requires_ack` message it clears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub agent: String,
    pub status: String,
    pub ts: DateTime<Utc>,
    /// Optional run the agent is part of (W5 tag). `None` for a bare agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reserve {
    pub path: String,
    pub holder: String,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReservesFile {
    #[serde(default)]
    pub claims: Vec<Reserve>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageBudget {
    None,
    #[default]
    Lean,
    Normal,
    Chatty,
}

impl MessageBudget {
    pub fn max_messages(&self) -> Option<usize> {
        match self {
            MessageBudget::None => Some(0),
            MessageBudget::Lean => Some(40),
            MessageBudget::Normal => Some(200),
            MessageBudget::Chatty => None,
        }
    }
}

/// Workspace-level bus root (W5 canonical location): `.spar/bus/`.
pub fn bus_root(paths: &SparPaths) -> PathBuf {
    paths.bus_dir()
}

/// Legacy per-run bus dir, kept as a back-compat mirror target for run-tagged
/// events/presence and as the home for the (still run-scoped) reserves + tasks.
/// TODO(W5): drop the mirror once no reader watches `.spar/runs/<id>/bus/`.
pub fn run_bus_root(paths: &SparPaths, run_id: &str) -> PathBuf {
    paths.run_dir(run_id).join("bus")
}

pub fn ensure_bus(paths: &SparPaths) -> Result<()> {
    paths.ensure_swarm_root()?;
    let root = bus_root(paths);
    for d in [root.clone(), root.join("inbox"), root.join("pending_ack")] {
        fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    Ok(())
}

/// Ensure the legacy per-run bus dir exists (mirror target + run-scoped reserves/tasks).
pub fn ensure_run_bus(paths: &SparPaths, run_id: &str) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let root = run_bus_root(paths, run_id);
    for d in [root.clone(), root.join("tasks")] {
        fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    Ok(())
}

pub fn events_path(paths: &SparPaths) -> PathBuf {
    bus_root(paths).join("events.jsonl")
}

pub fn agents_path(paths: &SparPaths) -> PathBuf {
    bus_root(paths).join("agents.jsonl")
}

/// Back-compat mirror path for a run's event log.
pub fn run_events_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    run_bus_root(paths, run_id).join("events.jsonl")
}

fn run_agents_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    run_bus_root(paths, run_id).join("agents.jsonl")
}

/// Reserves stay run-scoped: coding slots edit the same relative paths in *separate*
/// worktrees, so a global path claim would wrongly conflict across unrelated runs.
/// A bare agent's reserves live in the workspace bus.
pub fn reserves_path(paths: &SparPaths, run: Option<&str>) -> PathBuf {
    match run {
        Some(r) => run_bus_root(paths, r).join("reserves.json"),
        None => bus_root(paths).join("reserves.json"),
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub fn join(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    provider: Option<&str>,
    backend: Option<&str>,
) -> Result<()> {
    let p = Presence {
        agent: agent.into(),
        status: "joined".into(),
        ts: Utc::now(),
        run: run.map(str::to_string),
        backend: backend.map(str::to_string),
        provider: provider.map(str::to_string),
    };
    write_presence(paths, run, &p)?;
    send(
        paths,
        BusMessage {
            id: new_id(),
            ts: Utc::now(),
            from: agent.into(),
            to: "broadcast".into(),
            kind: MsgKind::System,
            body: format!("{agent} joined"),
            run: run.map(str::to_string),
            subject: Some("join".into()),
            refs: MsgRefs::default(),
            requires_ack: false,
            meta: HashMap::new(),
        },
        MessageBudget::Chatty,
    )?;
    Ok(())
}

pub fn heartbeat(paths: &SparPaths, run: Option<&str>, agent: &str, status: &str) -> Result<()> {
    let p = Presence {
        agent: agent.into(),
        status: status.into(),
        ts: Utc::now(),
        run: run.map(str::to_string),
        backend: None,
        provider: None,
    };
    write_presence(paths, run, &p)
}

/// Append a presence record to the workspace roster, mirroring run-tagged records
/// into the legacy run bus dir for back-compat readers.
fn write_presence(paths: &SparPaths, run: Option<&str>, p: &Presence) -> Result<()> {
    ensure_bus(paths)?;
    append_jsonl(&agents_path(paths), p)?;
    if let Some(r) = run {
        // TODO(W5): remove once no reader watches `.spar/runs/<id>/bus/agents.jsonl`.
        let _ = ensure_run_bus(paths, r);
        let _ = append_jsonl(&run_agents_path(paths, r), p);
    }
    Ok(())
}

/// Cap on a message body. Bodies are inline JSONL records read whole into memory
/// by every reader; an unbounded body would let one message bloat the event log
/// and every inbox copy. 64 KiB is generous for coordination traffic.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Loop prevention (Stage 7). The message budget caps *total* traffic, but two
/// agents can auto-reply to each other and ping-pong happily inside that budget,
/// burning real quota. The loop guard caps a *directed exchange* between one
/// unordered pair `{A,B}`: once the pair has traded [`LoopGuard::max_per_pair`]
/// messages **in both directions** inside a [`LoopGuard::window`] sliding window,
/// [`send`] refuses the next one. Only genuine back-and-forth trips it — a
/// one-directional blast (covered by the budget) and broadcast/system/ack traffic
/// are exempt, so ordinary coordination is unaffected.
pub const LOOP_WINDOW_SECS: i64 = 60;
pub const LOOP_MAX_PER_PAIR: usize = 12;

#[derive(Debug, Clone, Copy)]
pub struct LoopGuard {
    pub window: Duration,
    pub max_per_pair: usize,
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self {
            window: Duration::seconds(LOOP_WINDOW_SECS),
            max_per_pair: LOOP_MAX_PER_PAIR,
        }
    }
}

impl LoopGuard {
    /// Operator overrides via `SPAR_BUS_LOOP_MAX_PER_PAIR` / `SPAR_BUS_LOOP_WINDOW_SECS`.
    /// A cap of `0` disables the guard entirely.
    fn from_env() -> Self {
        let mut g = Self::default();
        if let Ok(n) = std::env::var("SPAR_BUS_LOOP_WINDOW_SECS")
            .ok()
            .map_or(Ok(g.window.num_seconds()), |v| v.parse::<i64>())
        {
            g.window = Duration::seconds(n);
        }
        if let Ok(n) = std::env::var("SPAR_BUS_LOOP_MAX_PER_PAIR")
            .ok()
            .map_or(Ok(g.max_per_pair), |v| v.parse::<usize>())
        {
            g.max_per_pair = n;
        }
        g
    }
}

/// The unordered pair key `(lo, hi)` for a message the loop guard governs, or
/// `None` if it is exempt. Broadcasts, `@human` alerts, and `Ack`/`System`/`Hello`
/// bookkeeping are never part of a reply loop, so they pass unguarded.
fn guarded_pair(msg: &BusMessage) -> Option<(&str, &str)> {
    if matches!(msg.kind, MsgKind::Ack | MsgKind::System | MsgKind::Hello) {
        return None;
    }
    let (from, to) = (msg.from.as_str(), msg.to.as_str());
    if to == "broadcast" || to == "*" || to == HUMAN || from == to {
        return None;
    }
    Some(if from <= to { (from, to) } else { (to, from) })
}

/// Refuse `msg` if sending it would push the `{from,to}` pair past the loop cap.
/// Trips only when the recent window already holds `max_per_pair` messages for the
/// pair *and* both directions are represented (a real ping-pong, not a one-way blast).
fn check_loop(events: &[BusMessage], msg: &BusMessage, guard: LoopGuard) -> Result<()> {
    if guard.max_per_pair == 0 {
        return Ok(());
    }
    let Some((lo, hi)) = guarded_pair(msg) else {
        return Ok(());
    };
    let cutoff = msg.ts - guard.window;
    let mut total = 0usize;
    let (mut fwd, mut rev) = (false, false);
    for m in events.iter().filter(|m| m.ts >= cutoff) {
        if guarded_pair(m) != Some((lo, hi)) {
            continue;
        }
        total += 1;
        if m.from == lo {
            fwd = true;
        } else {
            rev = true;
        }
    }
    if total >= guard.max_per_pair && fwd && rev {
        bail!(
            "loop guard: {lo}<->{hi} exchanged {total} messages in the last {}s (cap {}); \
             refusing to send — two agents are ping-ponging. Break the loop, or raise \
             SPAR_BUS_LOOP_MAX_PER_PAIR / widen SPAR_BUS_LOOP_WINDOW_SECS if this is legitimate.",
            guard.window.num_seconds(),
            guard.max_per_pair,
        );
    }
    Ok(())
}

pub fn send(paths: &SparPaths, msg: BusMessage, budget: MessageBudget) -> Result<BusMessage> {
    if msg.body.len() > MAX_BODY_BYTES {
        bail!(
            "message body too large ({} bytes; max {MAX_BODY_BYTES})",
            msg.body.len()
        );
    }
    ensure_bus(paths)?;
    let run = msg.run.as_deref();
    // Loop guard and budget stay run-scoped: cap traffic within one run's cohort, not
    // across the whole workspace (a bare pair is its own `None` scope).
    check_loop(&list_events(paths, run)?, &msg, LoopGuard::from_env())?;
    // Budget check and append happen under one lock so two senders can't both
    // read a below-cap count and both write (TOCTOU across processes).
    append_event_checked(&events_path(paths), &msg, budget.max_messages(), run)?;
    // TODO(W5): remove the run-dir event mirror once no reader watches
    // `.spar/runs/<id>/bus/events.jsonl`.
    if let Some(r) = run {
        let _ = ensure_run_bus(paths, r);
        let _ = append_jsonl(&run_events_path(paths, r), &msg);
    }
    deliver_inbox(paths, &msg)?;
    // also mirror to legacy mailbox for tools that still read it (run-scoped only)
    if let Some(r) = run {
        let _ = crate::mailbox::send(
            paths,
            r,
            &crate::mailbox::Message {
                id: msg.id.clone(),
                from: msg.from.clone(),
                to: msg.to.clone(),
                subject: msg
                    .subject
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", msg.kind)),
                body: msg.body.clone(),
                created_at: msg.ts,
            },
        );
    }

    // requires_ack lifecycle + @human routing (Stage 5). Handling both at this one
    // choke point covers every producer (chat, broadcast, workflows, tasks) uniformly.
    if msg.kind == MsgKind::Ack {
        if let Some(target) = msg.refs.reply_to.as_deref() {
            clear_pending_ack(paths, target)?;
        }
    } else if msg.requires_ack {
        record_pending_ack(paths, &msg)?;
    }
    if is_human_alert(&msg) {
        // The TUI sink reads the bus directly, so only the external notifier is a push.
        crate::notify::route_human_alert(paths, &msg);
    }
    Ok(msg)
}

/// A message the human needs to see: addressed to [`HUMAN`], or any `Blocked`
/// report (an agent that stalled is a human-relevant event even when broadcast).
pub fn is_human_alert(msg: &BusMessage) -> bool {
    msg.to == HUMAN || msg.kind == MsgKind::Blocked
}

/// Send an `Ack` clearing the redelivery of `msg_id`. Broadcast so any watcher sees
/// the acknowledgement; the clear itself keys off `refs.reply_to` in [`send`].
pub fn ack(paths: &SparPaths, run: Option<&str>, from: &str, msg_id: &str) -> Result<BusMessage> {
    send(
        paths,
        BusMessage {
            id: new_id(),
            ts: Utc::now(),
            from: from.into(),
            to: "broadcast".into(),
            kind: MsgKind::Ack,
            body: format!("ack {msg_id}"),
            run: run.map(str::to_string),
            subject: Some("ack".into()),
            refs: MsgRefs {
                reply_to: Some(msg_id.into()),
                ..Default::default()
            },
            requires_ack: false,
            meta: HashMap::new(),
        },
        MessageBudget::Chatty,
    )
}

pub fn chat(
    paths: &SparPaths,
    run: Option<&str>,
    from: &str,
    to: &str,
    body: impl Into<String>,
    budget: MessageBudget,
) -> Result<BusMessage> {
    send(
        paths,
        BusMessage {
            id: new_id(),
            ts: Utc::now(),
            from: from.into(),
            to: to.into(),
            kind: MsgKind::Chat,
            body: body.into(),
            run: run.map(str::to_string),
            subject: None,
            refs: MsgRefs::default(),
            requires_ack: false,
            meta: HashMap::new(),
        },
        budget,
    )
}

pub fn broadcast(
    paths: &SparPaths,
    run: Option<&str>,
    from: &str,
    body: impl Into<String>,
    budget: MessageBudget,
) -> Result<BusMessage> {
    chat(paths, run, from, "broadcast", body, budget)
}

fn deliver_inbox(paths: &SparPaths, msg: &BusMessage) -> Result<()> {
    let targets: Vec<String> = if msg.to == "broadcast" || msg.to == "*" {
        // A broadcast reaches the sender's own scope only: agents whose run tag matches
        // the message's exactly (a bare broadcast reaches other bare agents, never run
        // slots). Cross-scope fan-out is only ever explicit, addressed by id.
        list_presence(paths, None)?
            .into_iter()
            .filter(|p| p.run.as_deref() == msg.run.as_deref())
            .map(|p| p.agent)
            .filter(|a| a != &msg.from)
            .collect()
    } else {
        vec![msg.to.clone()]
    };
    for t in targets {
        let dir = inbox_dir(paths, &t);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.json", msg.ts.timestamp_millis(), msg.id));
        fs::write(&path, serde_json::to_string_pretty(msg)?)?;
    }
    Ok(())
}

/// An agent's inbox, keyed purely by `agent_id` at the workspace root (W5).
fn inbox_dir(paths: &SparPaths, agent: &str) -> PathBuf {
    bus_root(paths).join("inbox").join(agent)
}

/// List bus events. `Some(run)` filters to that run's tag; `None` returns the whole
/// workspace log (every run plus bare traffic).
pub fn list_events(paths: &SparPaths, run: Option<&str>) -> Result<Vec<BusMessage>> {
    let all: Vec<BusMessage> = read_jsonl(&events_path(paths))?;
    Ok(match run {
        Some(r) => all
            .into_iter()
            .filter(|m| m.run.as_deref() == Some(r))
            .collect(),
        None => all,
    })
}

/// Presence snapshot (last status per agent). `Some(run)` filters to that run's
/// agents; `None` returns the whole workspace roster (bare agents included).
pub fn list_presence(paths: &SparPaths, run: Option<&str>) -> Result<Vec<Presence>> {
    let rows: Vec<Presence> = read_jsonl(&agents_path(paths))?;
    // last status per agent, honouring the run filter
    let mut map: HashMap<String, Presence> = HashMap::new();
    for p in rows {
        if let Some(r) = run {
            if p.run.as_deref() != Some(r) {
                continue;
            }
        }
        map.insert(p.agent.clone(), p);
    }
    let mut out: Vec<_> = map.into_values().collect();
    out.sort_by(|a, b| a.agent.cmp(&b.agent));
    Ok(out)
}

/// Peek an agent's undelivered inbox without consuming it. The `claimed/`
/// subdir is skipped (it has no `.json` extension at the top level).
///
/// `run` scopes the peek exactly like [`inbox_claim`] drains: `Some(r)` returns only
/// messages tagged with run `r`, `None` returns only untagged (bare) traffic. Slot ids
/// are not unique across runs, so an unfiltered peek would surface another run's
/// messages for the same slot id.
pub fn inbox(paths: &SparPaths, run: Option<&str>, agent: &str) -> Result<Vec<BusMessage>> {
    let dir = inbox_dir(paths, agent);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<BusMessage> = Vec::new();
    for e in fs::read_dir(&dir)? {
        let e = e?;
        if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<BusMessage>(&fs::read_to_string(e.path())?) {
            if m.run.as_deref() != run {
                continue;
            }
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

/// Drain an agent's inbox with exactly-once semantics: each message file is
/// atomically `rename`d into `inbox/<agent>/claimed/` and returned. `rename` on
/// the same filesystem is atomic, so under concurrent claimers exactly one wins
/// each file (the loser's source path is already gone → skipped), never a double
/// delivery. Messages already claimed are not returned again.
///
/// `run` scopes the drain to one run's traffic. W5 keys inboxes by `agent_id` at the
/// workspace root, but slot ids are deterministic per provider/role (`orchestrator`,
/// `review-0-cli-claude`, …) and therefore collide across concurrent same-shaped runs
/// sharing one inbox directory. Without scoping, run A's `deliver` would steal run B's
/// messages. `Some(r)` claims only messages tagged with run `r`; `None` claims only
/// untagged (bare) traffic. A message whose tag does not match is left in the inbox for
/// its owner to claim — the match is checked *before* the atomic rename, so a foreign
/// message is never removed.
pub fn inbox_claim(paths: &SparPaths, run: Option<&str>, agent: &str) -> Result<Vec<BusMessage>> {
    let dir = inbox_dir(paths, agent);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let claimed_dir = dir.join("claimed");
    fs::create_dir_all(&claimed_dir)?;
    let mut sources: Vec<PathBuf> = Vec::new();
    for e in fs::read_dir(&dir)? {
        let e = e?;
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        sources.push(p);
    }
    let mut out: Vec<BusMessage> = Vec::new();
    for src in sources {
        let Some(name) = src.file_name() else {
            continue;
        };
        // Read + scope-check before claiming: only messages tagged for `run` are ours.
        // A concurrent claimer may have already moved the file (ENOENT) → skip.
        let Ok(contents) = fs::read_to_string(&src) else {
            continue;
        };
        let Ok(m) = serde_json::from_str::<BusMessage>(&contents) else {
            continue;
        };
        if m.run.as_deref() != run {
            continue;
        }
        let dest = claimed_dir.join(name);
        // Whoever moves the inode owns the message. A concurrent claimer that
        // already moved it gets ENOENT here and is skipped.
        if fs::rename(&src, &dest).is_err() {
            continue;
        }
        out.push(m);
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

/// A path claim goes stale this many seconds after its holder's last sign of life
/// (presence heartbeat, falling back to the claim time). Past it, another agent may
/// reclaim the path — this is what auto-releases a claim held by a crashed agent, so
/// `reserves.json` never needs hand-editing. Matches the stall-warn horizon: a holder
/// spar already considers stalled has also lost its lease.
pub const RESERVE_LEASE_TTL_SECS: i64 = 300;

/// How often the orchestrator's process supervisor refreshes a live slot's presence
/// (see `executor::run_headless` / `execute_prepared`). Provider presence hooks only
/// fire on events (tool use, prompt submit) and a whole adapter class
/// (`PresenceSource::None`, e.g. agy) installs no hooks at all — so lease liveness
/// cannot depend on them. This supervisor beat keeps any live slot's presence fresh
/// while its child process runs, independent of provider hooks. Kept well under
/// [`RESERVE_LEASE_TTL_SECS`] so a missed beat never expires a lease on a live holder.
pub const LIVENESS_HEARTBEAT_SECS: i64 = 30;

pub fn reserve(paths: &SparPaths, run: Option<&str>, path: &str, holder: &str) -> Result<()> {
    reserve_at(paths, run, path, holder, Utc::now())
}

/// [`reserve`] with an injectable `now` so lease-expiry can be exercised in tests.
fn reserve_at(
    paths: &SparPaths,
    run: Option<&str>,
    path: &str,
    holder: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    ensure_bus(paths)?;
    let mut file = load_reserves(paths, run)?;
    let presence = list_presence(paths, run)?;
    let ttl = Duration::seconds(RESERVE_LEASE_TTL_SECS);
    if let Some(c) = file
        .claims
        .iter()
        .find(|c| c.path == path && c.holder != holder)
    {
        // The lease is tied to the holder's last heartbeat (or the claim time, if it
        // has none yet). A fresh, heartbeating holder blocks; a stale one is reclaimed.
        let basis = holder_heartbeat(&presence, &c.holder).map_or(c.ts, |hb| hb.max(c.ts));
        if now - basis <= ttl {
            bail!("path {path} already reserved by {}", c.holder);
        }
    }
    file.claims.retain(|c| c.path != path);
    file.claims.push(Reserve {
        path: path.into(),
        holder: holder.into(),
        ts: now,
    });
    save_reserves(paths, run, &file)
}

/// Timestamp of a holder's most recent presence record, if any.
fn holder_heartbeat(presence: &[Presence], holder: &str) -> Option<DateTime<Utc>> {
    presence.iter().find(|p| p.agent == holder).map(|p| p.ts)
}

pub fn release(paths: &SparPaths, run: Option<&str>, path: &str, holder: &str) -> Result<()> {
    let mut file = load_reserves(paths, run)?;
    file.claims
        .retain(|c| !(c.path == path && c.holder == holder));
    save_reserves(paths, run, &file)
}

#[allow(dead_code)]
pub fn list_reserves(paths: &SparPaths, run: Option<&str>) -> Result<Vec<Reserve>> {
    Ok(load_reserves(paths, run)?.claims)
}

fn load_reserves(paths: &SparPaths, run: Option<&str>) -> Result<ReservesFile> {
    let p = reserves_path(paths, run);
    if !p.is_file() {
        return Ok(ReservesFile::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(p)?)?)
}

fn save_reserves(paths: &SparPaths, run: Option<&str>, file: &ReservesFile) -> Result<()> {
    if let Some(r) = run {
        ensure_run_bus(paths, r)?;
    } else {
        ensure_bus(paths)?;
    }
    fs::write(
        reserves_path(paths, run),
        serde_json::to_string_pretty(file)?,
    )?;
    Ok(())
}

// ── requires_ack: redeliver-until-acked, then escalate to @human ──────────────

/// A `requires_ack` message awaiting its `Ack`. Persisted per message under
/// `bus/pending_ack/<id>.json`; [`tick_acks`] redelivers on backoff and escalates.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingAck {
    msg: BusMessage,
    /// Redeliveries performed so far (0 = only the original delivery has happened).
    attempts: u32,
    /// Earliest instant the next redelivery/escalation may fire.
    next_at: DateTime<Utc>,
}

/// Redelivery cadence for unacked messages. `max_retries` redeliveries happen
/// (exponential backoff off `base_backoff`) before the message escalates to a
/// `@human` alert.
#[derive(Debug, Clone)]
pub struct AckPolicy {
    pub base_backoff: Duration,
    pub max_retries: u32,
}

impl Default for AckPolicy {
    fn default() -> Self {
        Self {
            base_backoff: Duration::seconds(60),
            max_retries: 3,
        }
    }
}

impl AckPolicy {
    /// Exponential backoff before the `attempts`-th redelivery, capped at 30 min.
    fn backoff_for(&self, attempts: u32) -> Duration {
        let factor = 1i64 << attempts.min(6);
        let secs = self.base_backoff.num_seconds().max(0) * factor;
        Duration::seconds(secs.min(1800))
    }
}

/// What one [`tick_acks`] pass did.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AckTick {
    pub redelivered: usize,
    pub escalated: usize,
}

fn pending_ack_dir(paths: &SparPaths) -> PathBuf {
    bus_root(paths).join("pending_ack")
}

fn record_pending_ack(paths: &SparPaths, msg: &BusMessage) -> Result<()> {
    let dir = pending_ack_dir(paths);
    fs::create_dir_all(&dir)?;
    let rec = PendingAck {
        msg: msg.clone(),
        attempts: 0,
        next_at: msg.ts + AckPolicy::default().base_backoff,
    };
    fs::write(
        dir.join(format!("{}.json", msg.id)),
        serde_json::to_string_pretty(&rec)?,
    )?;
    Ok(())
}

fn clear_pending_ack(paths: &SparPaths, msg_id: &str) -> Result<()> {
    let p = pending_ack_dir(paths).join(format!("{msg_id}.json"));
    if p.is_file() {
        fs::remove_file(&p)?;
    }
    Ok(())
}

/// Advance every pending `requires_ack` message whose backoff has elapsed by `now`:
/// redeliver it to the recipient inbox, or — once `max_retries` redeliveries are
/// spent — escalate it to a `@human` alert and drop the pending record. An `Ack`
/// (handled in [`send`]) removes the record, so an acked message never ticks again.
pub fn tick_acks(paths: &SparPaths, policy: &AckPolicy, now: DateTime<Utc>) -> Result<AckTick> {
    let dir = pending_ack_dir(paths);
    if !dir.is_dir() {
        return Ok(AckTick::default());
    }
    // Several pulses tick the same run concurrently: `spar bus deliver` (one per
    // agent finishing a turn), the `spar wait` loop, and the TUI refresh thread.
    // Serialize the whole read-modify-remove/write pass under one exclusive lock so
    // a record is escalated-and-removed or redelivered by exactly one process —
    // never double-escalated, and never resurrected by a redeliver write racing
    // another process's remove.
    let lock = open_lockfile(&dir.join(".lock"))?;
    lock_exclusive(&lock)?;
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut out = AckTick::default();
    for f in files {
        let Ok(mut rec) = serde_json::from_str::<PendingAck>(&fs::read_to_string(&f)?) else {
            continue;
        };
        if rec.next_at > now {
            continue;
        }
        if rec.attempts >= policy.max_retries {
            escalate_unacked(paths, &rec, now, policy.max_retries)?;
            remove_file_if_present(&f)?;
            out.escalated += 1;
        } else {
            deliver_inbox(paths, &rec.msg)?;
            rec.attempts += 1;
            rec.next_at = now + policy.backoff_for(rec.attempts);
            fs::write(&f, serde_json::to_string_pretty(&rec)?)?;
            out.redelivered += 1;
        }
    }
    Ok(out)
}

fn escalate_unacked(
    paths: &SparPaths,
    rec: &PendingAck,
    now: DateTime<Utc>,
    max_retries: u32,
) -> Result<()> {
    let mut meta = HashMap::new();
    meta.insert("escalated_from".into(), rec.msg.id.clone());
    let esc = BusMessage {
        id: new_id(),
        ts: now,
        from: "spar".into(),
        to: HUMAN.into(),
        kind: MsgKind::Blocked,
        body: format!(
            "No ack after {max_retries} redeliveries: message {} to {} from {} — {}",
            rec.msg.id,
            rec.msg.to,
            rec.msg.from,
            rec.msg.body.chars().take(200).collect::<String>()
        ),
        // Carry the original message's run tag so the escalation stays in-scope.
        run: rec.msg.run.clone(),
        subject: Some("unacked".into()),
        refs: MsgRefs {
            reply_to: Some(rec.msg.id.clone()),
            ..Default::default()
        },
        requires_ack: false,
        meta,
    };
    send(paths, esc, MessageBudget::Chatty)?;
    Ok(())
}

/// Human-facing alerts still awaiting attention: `@human` messages with no `Ack`,
/// plus every agent still `Blocked` (no later `Unblocked` from it). Powers the TUI
/// alert panel/badge.
pub fn unresolved_alerts(paths: &SparPaths, run: Option<&str>) -> Result<Vec<BusMessage>> {
    let evs = list_events(paths, run)?;
    let acked: HashSet<String> = evs
        .iter()
        .filter(|m| m.kind == MsgKind::Ack)
        .filter_map(|m| m.refs.reply_to.clone())
        .collect();
    let mut blocked: HashMap<String, BusMessage> = HashMap::new();
    for m in &evs {
        match m.kind {
            MsgKind::Blocked => {
                blocked.insert(m.from.clone(), m.clone());
            }
            MsgKind::Unblocked => {
                blocked.remove(&m.from);
            }
            _ => {}
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<BusMessage> = Vec::new();
    for m in evs
        .iter()
        .filter(|m| m.to == HUMAN && !acked.contains(&m.id))
    {
        if seen.insert(m.id.clone()) {
            out.push(m.clone());
        }
    }
    for m in blocked.into_values() {
        if !acked.contains(&m.id) && seen.insert(m.id.clone()) {
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

/// Count the events on the workspace log that belong to `run` (the message-budget
/// unit stays a single run's cohort even though the log is now workspace-wide).
fn count_run_events(path: &PathBuf, run: Option<&str>) -> Result<usize> {
    if !path.is_file() {
        return Ok(0);
    }
    let mut n = 0usize;
    for line in fs::read_to_string(path)?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let m: BusMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if m.run.as_deref() == run {
            n += 1;
        }
    }
    Ok(n)
}

fn open_for_append(path: &PathBuf) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// Open (creating if absent) a lockfile whose only purpose is to hold an
/// advisory `flock`. The file's contents are never read or written.
fn open_lockfile(path: &PathBuf) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// Remove a file, treating an already-absent file as success so a lost
/// double-remove race can never abort the caller.
fn remove_file_if_present(path: &PathBuf) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Take an exclusive advisory lock on the file's open description. It is held
/// until `f` is dropped (fd close), serializing writers across threads and
/// processes so a record and its trailing newline can never interleave with
/// another writer's bytes.
fn lock_exclusive(f: &File) -> Result<()> {
    rustix::fs::flock(f, rustix::fs::FlockOperation::LockExclusive).context("flock exclusive")?;
    Ok(())
}

/// Serialize `value` and its newline into a single `write_all`, so under
/// `O_APPEND` the whole record lands at end-of-file in one syscall.
fn write_record<T: Serialize>(f: &mut File, value: &T) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    f.write_all(&line)?;
    Ok(())
}

fn append_jsonl<T: Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    let mut f = open_for_append(path)?;
    lock_exclusive(&f)?;
    write_record(&mut f, value)
}

/// Append a bus event while enforcing the message budget under the same lock
/// that guards the write: the count and the append are atomic with respect to
/// other senders, closing the check-then-write race.
fn append_event_checked(
    path: &PathBuf,
    msg: &BusMessage,
    max: Option<usize>,
    run: Option<&str>,
) -> Result<()> {
    let mut f = open_for_append(path)?;
    lock_exclusive(&f)?;
    if let Some(max) = max {
        if count_run_events(path, run)? >= max {
            bail!("message budget exhausted ({max} messages)");
        }
    }
    write_record(&mut f, msg)
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<Vec<T>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str(line) {
            out.push(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn send_inbox_reserve() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();
        chat(&paths, Some("r1"), "a", "b", "hello", MessageBudget::Normal).unwrap();
        let inbox_b = inbox(&paths, Some("r1"), "b").unwrap();
        assert!(!inbox_b.is_empty());
        reserve(&paths, Some("r1"), "src/foo.rs", "a").unwrap();
        assert!(reserve(&paths, Some("r1"), "src/foo.rs", "b").is_err());
        release(&paths, Some("r1"), "src/foo.rs", "a").unwrap();
        reserve(&paths, Some("r1"), "src/foo.rs", "b").unwrap();
    }

    #[test]
    fn reserve_lease_expires_with_holder_heartbeat() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();
        reserve(&paths, Some("r1"), "src/foo.rs", "a").unwrap();

        // Within the lease window a's claim still blocks b (heartbeat is fresh).
        let fresh = Utc::now() + Duration::seconds(RESERVE_LEASE_TTL_SECS - 1);
        assert!(reserve_at(&paths, Some("r1"), "src/foo.rs", "b", fresh).is_err());

        // Once a's last heartbeat is older than the TTL (a crashed, stopped beating),
        // b reclaims the path with no manual release.
        let expired = Utc::now() + Duration::seconds(RESERVE_LEASE_TTL_SECS + 1);
        reserve_at(&paths, Some("r1"), "src/foo.rs", "b", expired).unwrap();
        let claims = list_reserves(&paths, Some("r1")).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].holder, "b");

        // A heartbeating holder refreshes its lease: b now blocks a while b beats.
        heartbeat(&paths, Some("r1"), "b", "running").unwrap();
        assert!(reserve(&paths, Some("r1"), "src/foo.rs", "a").is_err());
    }

    #[test]
    fn inbox_claim_drains_exactly_once() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();
        chat(&paths, Some("r1"), "a", "b", "hello", MessageBudget::Normal).unwrap();
        chat(&paths, Some("r1"), "a", "b", "world", MessageBudget::Normal).unwrap();

        // Peek does not consume: repeated non-claim reads keep returning all.
        assert_eq!(inbox(&paths, Some("r1"), "b").unwrap().len(), 2);
        assert_eq!(inbox(&paths, Some("r1"), "b").unwrap().len(), 2);

        // First claim drains everything; second claim sees nothing.
        let first = inbox_claim(&paths, Some("r1"), "b").unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].body, "hello");
        assert_eq!(first[1].body, "world");
        assert!(inbox_claim(&paths, Some("r1"), "b").unwrap().is_empty());

        // Peek after claim is also empty (claimed/ is excluded).
        assert!(inbox(&paths, Some("r1"), "b").unwrap().is_empty());
    }

    #[test]
    fn inbox_claim_is_run_scoped_across_same_slot_id() {
        // Two runs share the deterministic slot id "b"; a message tagged for one run must
        // never be claimable under the other (slot ids are not unique across runs).
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        for run in ["rA", "rB"] {
            join(
                &paths,
                Some(run),
                "a",
                Some("cli:claude"),
                Some("native-cli"),
            )
            .unwrap();
            join(&paths, Some(run), "b", Some("cli:grok"), Some("native-cli")).unwrap();
        }
        chat(&paths, Some("rA"), "a", "b", "for A", MessageBudget::Normal).unwrap();
        chat(&paths, Some("rB"), "a", "b", "for B", MessageBudget::Normal).unwrap();

        // Run B's drain sees only run B's message; run A's stays put.
        let b = inbox_claim(&paths, Some("rB"), "b").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].body, "for B");

        // Run A's message survived B's drain and is still claimable under run A.
        let a = inbox_claim(&paths, Some("rA"), "b").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].body, "for A");
    }

    fn nonempty_event_lines(paths: &SparPaths) -> Vec<String> {
        fs::read_to_string(events_path(paths))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn concurrent_send_writes_intact_lines() {
        let tmp = tempdir().unwrap();
        let paths = std::sync::Arc::new(SparPaths::new(tmp.path()));
        ensure_bus(&paths).unwrap();
        let m = 32;
        let handles: Vec<_> = (0..m)
            .map(|i| {
                let p = paths.clone();
                std::thread::spawn(move || {
                    chat(
                        &p,
                        Some("r1"),
                        "a",
                        "b",
                        format!("msg {i}"),
                        MessageBudget::Chatty,
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Exactly M records, each a well-formed (untorn) JSONL line.
        let lines = nonempty_event_lines(&paths);
        assert_eq!(lines.len(), m);
        for l in &lines {
            serde_json::from_str::<BusMessage>(l).expect("well-formed JSONL line");
        }
    }

    #[test]
    fn concurrent_send_respects_budget() {
        let tmp = tempdir().unwrap();
        let paths = std::sync::Arc::new(SparPaths::new(tmp.path()));
        ensure_bus(&paths).unwrap();
        let max = MessageBudget::Lean.max_messages().unwrap();
        let threads = max * 3;
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let p = paths.clone();
                std::thread::spawn(move || {
                    // Some senders lose the budget race and error; that's fine.
                    let _ = chat(
                        &p,
                        Some("r1"),
                        "a",
                        "b",
                        format!("m{i}"),
                        MessageBudget::Lean,
                    );
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let lines = nonempty_event_lines(&paths);
        // Never past the cap, and every survivor is intact.
        assert_eq!(lines.len(), max);
        for l in &lines {
            serde_json::from_str::<BusMessage>(l).expect("well-formed JSONL line");
        }
    }

    fn req_ack(paths: &SparPaths, from: &str, to: &str, body: &str) -> BusMessage {
        send(
            paths,
            BusMessage {
                id: new_id(),
                ts: Utc::now(),
                from: from.into(),
                to: to.into(),
                kind: MsgKind::Chat,
                body: body.into(),
                run: Some("r1".into()),
                subject: None,
                refs: MsgRefs::default(),
                requires_ack: true,
                meta: HashMap::new(),
            },
            MessageBudget::Chatty,
        )
        .unwrap()
    }

    #[test]
    fn requires_ack_redelivers_then_escalates() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();
        let m = req_ack(&paths, "a", "b", "please confirm");
        // Original delivery landed once.
        assert_eq!(inbox_claim(&paths, Some("r1"), "b").unwrap().len(), 1);

        // The original send scheduled the first redelivery a default backoff out, so
        // tick from far enough ahead that it (and every base_backoff-0 retry) is due.
        let policy = AckPolicy {
            base_backoff: Duration::zero(),
            max_retries: 2,
        };
        let now = Utc::now() + Duration::seconds(120);
        let t1 = tick_acks(&paths, &policy, now).unwrap();
        assert_eq!((t1.redelivered, t1.escalated), (1, 0));
        assert_eq!(
            inbox_claim(&paths, Some("r1"), "b").unwrap().len(),
            1,
            "redeliver 1"
        );
        let t2 = tick_acks(&paths, &policy, now).unwrap();
        assert_eq!((t2.redelivered, t2.escalated), (1, 0));
        assert_eq!(
            inbox_claim(&paths, Some("r1"), "b").unwrap().len(),
            1,
            "redeliver 2"
        );

        // Third due tick: retries spent → escalate to @human, drop the pending record.
        let t3 = tick_acks(&paths, &policy, now).unwrap();
        assert_eq!((t3.redelivered, t3.escalated), (0, 1));
        let human = inbox(&paths, Some("r1"), HUMAN).unwrap();
        assert_eq!(human.len(), 1);
        assert_eq!(human[0].kind, MsgKind::Blocked);
        assert_eq!(human[0].refs.reply_to.as_deref(), Some(m.id.as_str()));
        assert!(is_human_alert(&human[0]));

        // Record is gone: further ticks are no-ops.
        let t4 = tick_acks(&paths, &policy, now).unwrap();
        assert_eq!((t4.redelivered, t4.escalated), (0, 0));
    }

    #[test]
    fn ack_stops_redelivery() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();
        let m = req_ack(&paths, "a", "b", "please confirm");

        ack(&paths, Some("r1"), "b", &m.id).unwrap();
        // Pending record cleared → no redelivery, no escalation, ever.
        let policy = AckPolicy {
            base_backoff: Duration::zero(),
            max_retries: 1,
        };
        let t = tick_acks(&paths, &policy, Utc::now()).unwrap();
        assert_eq!((t.redelivered, t.escalated), (0, 0));
        assert!(inbox(&paths, Some("r1"), HUMAN).unwrap().is_empty());
    }

    #[test]
    fn unresolved_alerts_tracks_blocked_and_human() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();

        // A Blocked report surfaces; an Unblocked from the same agent clears it.
        let blocked = |body: &str| {
            send(
                &paths,
                BusMessage {
                    id: new_id(),
                    ts: Utc::now(),
                    from: "a".into(),
                    to: "broadcast".into(),
                    kind: MsgKind::Blocked,
                    body: body.into(),
                    run: Some("r1".into()),
                    subject: None,
                    refs: MsgRefs::default(),
                    requires_ack: false,
                    meta: HashMap::new(),
                },
                MessageBudget::Chatty,
            )
            .unwrap()
        };
        blocked("stuck on tests");
        assert_eq!(unresolved_alerts(&paths, Some("r1")).unwrap().len(), 1);
        send(
            &paths,
            BusMessage {
                id: new_id(),
                ts: Utc::now(),
                from: "a".into(),
                to: "broadcast".into(),
                kind: MsgKind::Unblocked,
                body: "resolved".into(),
                run: Some("r1".into()),
                subject: None,
                refs: MsgRefs::default(),
                requires_ack: false,
                meta: HashMap::new(),
            },
            MessageBudget::Chatty,
        )
        .unwrap();
        assert!(unresolved_alerts(&paths, Some("r1")).unwrap().is_empty());
    }

    #[test]
    fn loop_guard_refuses_pingpong_but_passes_normal() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(
            &paths,
            Some("r1"),
            "a",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(
            &paths,
            Some("r1"),
            "b",
            Some("cli:grok"),
            Some("native-cli"),
        )
        .unwrap();

        // Rapid A<->B ping-pong: alternate direction so both sides are represented.
        // Chatty budget removes the volume cap, isolating the loop guard as the limiter.
        let mut sent = 0usize;
        let mut refused = false;
        for i in 0..(LOOP_MAX_PER_PAIR * 2) {
            let (from, to) = if i % 2 == 0 { ("a", "b") } else { ("b", "a") };
            match chat(
                &paths,
                Some("r1"),
                from,
                to,
                format!("m{i}"),
                MessageBudget::Chatty,
            ) {
                Ok(_) => sent += 1,
                Err(e) => {
                    assert!(
                        e.to_string().contains("loop guard"),
                        "unexpected error: {e}"
                    );
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused, "ping-pong past the cap should be refused");
        assert_eq!(
            sent, LOOP_MAX_PER_PAIR,
            "exactly the cap is allowed through"
        );

        // Ordinary traffic is unaffected: a different pair still passes freely,
        assert!(chat(&paths, Some("r1"), "a", "c", "hi", MessageBudget::Chatty).is_ok());
        // a one-directional stream (not a loop) passes past the cap,
        for i in 0..(LOOP_MAX_PER_PAIR + 4) {
            chat(
                &paths,
                Some("r1"),
                "d",
                "e",
                format!("n{i}"),
                MessageBudget::Chatty,
            )
            .unwrap();
        }
        // and broadcasts are exempt.
        assert!(broadcast(&paths, Some("r1"), "a", "all-hands", MessageBudget::Chatty).is_ok());
    }

    #[test]
    fn send_rejects_oversized_body() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(chat(&paths, Some("r1"), "a", "b", big, MessageBudget::Chatty).is_err());
        // A body at the cap is accepted.
        let ok = "y".repeat(MAX_BODY_BYTES);
        assert!(chat(&paths, Some("r1"), "a", "b", ok, MessageBudget::Chatty).is_ok());
    }

    /// W5 cross-scope addressing: a bare agent (no run) and a run slot can message
    /// each other by id, and per-run views stay filtered by the run tag.
    #[test]
    fn bare_and_run_agents_address_each_other() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        // A run slot and a bare (run-less) agent both join the one workspace bus.
        join(
            &paths,
            Some("r1"),
            "slot",
            Some("cli:claude"),
            Some("native-cli"),
        )
        .unwrap();
        join(&paths, None, "bare", Some("cli:grok"), Some("native-cli")).unwrap();

        // Bare → run slot and run slot → bare both land, keyed purely by agent id.
        chat(
            &paths,
            None,
            "bare",
            "slot",
            "hi slot",
            MessageBudget::Chatty,
        )
        .unwrap();
        chat(
            &paths,
            Some("r1"),
            "slot",
            "bare",
            "hi bare",
            MessageBudget::Chatty,
        )
        .unwrap();
        // Each message is scoped by its own run tag: the bare→slot message is untagged
        // (claimed under `None`), the slot→bare message carries `r1`.
        assert_eq!(inbox(&paths, None, "slot").unwrap().len(), 1);
        assert_eq!(
            inbox(&paths, Some("r1"), "bare").unwrap()[0].body,
            "hi bare"
        );

        // Run-scoped presence sees only the run slot; the workspace view sees both.
        let run_roster = list_presence(&paths, Some("r1")).unwrap();
        assert_eq!(run_roster.len(), 1);
        assert_eq!(run_roster[0].agent, "slot");
        let all = list_presence(&paths, None).unwrap();
        assert_eq!(all.len(), 2);

        // Events filter by run tag: r1 view excludes the bare traffic.
        let r1_events = list_events(&paths, Some("r1")).unwrap();
        assert!(r1_events.iter().all(|m| m.run.as_deref() == Some("r1")));
        assert!(r1_events.iter().any(|m| m.body == "hi bare"));
        assert!(!r1_events.iter().any(|m| m.body == "hi slot"));

        // The run-tagged traffic is mirrored into the legacy run bus dir.
        assert!(run_events_path(&paths, "r1").is_file());
    }
}
