//! `spar daemon`: an opt-in, per-project supervisor.
//!
//! It restarts a dead orchestrator that still holds resumable (in-flight, abandoned)
//! work, pushes the one lifecycle event a `RunState::save` can never observe on its
//! own (`abandoned` is the *absence* of a further save, not a transition), and
//! enforces a cross-run concurrency cap keyed on the quota bucket
//! (`ProviderRef::storage_key()`).
//!
//! It is never an operator. It must not call `worktree::cleanup_run`, `ship::*`,
//! `providers::pick_providers`, `state::archive_sweep`, or set `state.gates.*` — a
//! supervisor is a claim about which process happens to be driving a run, never about
//! who is allowed to decide its fate.

use crate::config::Config;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::process::PidToken;
use crate::provider_ref::ProviderRef;
use crate::state::{RunState, SlotStatus};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Single-daemon-per-project guard: same `flock` + `PidToken` mechanics as `RunLock`,
/// minus the run-reclaim step. The daemon has no slots of its own for a takeover to
/// settle, so it needs none of `RunLock`'s reconcile logic.
pub struct DaemonLock {
    path: PathBuf,
    pid: u32,
    file: File,
}

impl DaemonLock {
    pub fn acquire(paths: &SparPaths) -> Result<Self> {
        let path = paths.daemon_lock();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let me = std::process::id();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "a daemon is already running for this project (pid {}); stop it first with `spar daemon stop`",
                    Self::owner(paths).map(|t| t.pid).unwrap_or(0)
                );
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("lock {}", path.display()));
            }
        }
        file.set_len(0)
            .with_context(|| format!("truncate {}", path.display()))?;
        (&file)
            .write_all(PidToken::capture(me).encode().as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(Self {
            path,
            pid: me,
            file,
        })
    }

    pub fn owner(paths: &SparPaths) -> Option<PidToken> {
        PidToken::parse(&fs::read_to_string(paths.daemon_lock()).ok()?)
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if DaemonLock::owner_pid_matches(&self.path, self.pid) {
            let _ = self.file.set_len(0);
        }
        let _ = self.file.unlock();
    }
}

impl DaemonLock {
    fn owner_pid_matches(path: &std::path::Path, pid: u32) -> bool {
        PidToken::parse(&fs::read_to_string(path).unwrap_or_default())
            .map(|t| t.pid == pid)
            .unwrap_or(false)
    }
}

/// Per-run bookkeeping, keyed by run id. Safe to delete entirely: it is a supervision
/// aid, never the record of what a run *is* — that stays in `.spar/runs/<id>/`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunBookkeeping {
    #[serde(default)]
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_restart_at: Option<DateTime<Utc>>,
    /// First tick this run was observed abandoned, this episode. Cleared once the run
    /// is no longer abandoned, so a later abandonment starts a fresh episode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_since: Option<DateTime<Utc>>,
    /// Set once the `abandoned` lifecycle alert has fired for the current episode, so
    /// it fires exactly once per (run, abandonment episode).
    #[serde(default)]
    pub abandoned_notified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub runs: HashMap<String, RunBookkeeping>,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            pid: PidToken::capture(std::process::id()).encode(),
            started_at: Utc::now(),
            runs: HashMap::new(),
        }
    }

    fn load(paths: &SparPaths) -> Self {
        fs::read_to_string(paths.daemon_state())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(Self::new)
    }

    fn save(&self, paths: &SparPaths) -> Result<()> {
        let path = paths.daemon_state();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write {}", path.display()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueEntry {
    run_id: String,
    buckets: Vec<String>,
    enqueued_at: DateTime<Utc>,
}

/// How long an admission reservation counts toward supply after being granted, before
/// it is pruned regardless of whether the run ever actually started a slot. Long
/// enough to cover `spawn_detached_orchestrator` + the handshake (capped at 10s by
/// default) plus the time for the child to dispatch and mark a slot `Running`; short
/// enough that a launch which dies on the way in self-heals instead of wasting
/// capacity forever.
const RESERVATION_TTL_SECS: i64 = 60;

/// A grant of capacity for a run between the moment it is admitted (a launch, a
/// restart, a queue release) and the moment its real slots reflect that demand.
/// `bucket_supply_in_use` cannot see a run's usage until its orchestrator has actually
/// dispatched a slot to `Running`, and that can lag admission by seconds — long enough
/// for a second admission decision, in this process or another, to race it. A
/// reservation closes that window: it is read back into `effective_supply` alongside
/// the real counts until either the real counts catch up (see `effective_supply`) or
/// the TTL above prunes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reservation {
    run_id: String,
    buckets: HashMap<String, u32>,
    reserved_at: DateTime<Utc>,
}

fn load_reservations(paths: &SparPaths) -> Vec<Reservation> {
    let now = Utc::now();
    fs::read_to_string(paths.reservations_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Reservation>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|r| now - r.reserved_at < chrono::Duration::seconds(RESERVATION_TTL_SECS))
        .collect()
}

fn save_reservations(paths: &SparPaths, reservations: &[Reservation]) -> Result<()> {
    let path = paths.reservations_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_string_pretty(reservations)?)
        .with_context(|| format!("write {}", path.display()))
}

/// Guard the read-decide-write admission sequence with a blocking file lock, so two
/// concurrent launchers (or a launcher racing the daemon's own restart/drain) can
/// never both observe the same free capacity and both admit past the cap.
fn with_admission_lock<T>(paths: &SparPaths, f: impl FnOnce() -> T) -> Result<T> {
    let path = paths.admission_lock();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    let result = f();
    let _ = file.unlock();
    Ok(result)
}

/// Try to admit `run_id`'s `demand` against `cap`, atomically with every other
/// admission decision in this project (same process or another). On success, records
/// a reservation for `run_id` before returning, so the very next decision — in this
/// loop iteration or a concurrent process — already sees the capacity as taken.
fn try_admit(paths: &SparPaths, run_id: &str, demand: &HashMap<String, u32>, cap: u32) -> bool {
    with_admission_lock(paths, || {
        let mut reservations = load_reservations(paths);
        let real_by_run = real_supply_by_run(paths);
        let effective = effective_supply(&real_by_run, &reservations, Some(run_id));
        if !fits(demand, &effective, cap) {
            return false;
        }
        reservations.retain(|r| r.run_id != run_id);
        if !demand.is_empty() {
            reservations.push(Reservation {
                run_id: run_id.to_string(),
                buckets: demand.clone(),
                reserved_at: Utc::now(),
            });
        }
        let _ = save_reservations(paths, &reservations);
        true
    })
    .unwrap_or(true) // a lock/IO failure must never block admission — the cap is advisory
}

/// Drop `run_id`'s reservation early, once it is known it will never be reflected in
/// real slot state — the spawn or its handshake failed, or (the `--dry-run --detach`
/// case) the run reached a waitable stop inside the handshake window without ever
/// holding a `Running` slot for a caller to see. Best-effort: if this is never called,
/// the TTL above still reclaims the capacity.
pub(crate) fn release_reservation(paths: &SparPaths, run_id: &str) {
    let _ = with_admission_lock(paths, || {
        let mut reservations = load_reservations(paths);
        reservations.retain(|r| r.run_id != run_id);
        let _ = save_reservations(paths, &reservations);
    });
}

fn log_line(paths: &SparPaths, msg: &str) {
    let dir = paths.workspace_logs_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
    {
        let _ = writeln!(f, "{} {msg}", Utc::now().to_rfc3339());
    }
}

/// Bucket a run's providers by `storage_key()`. Real slots once they exist; the
/// projected fleet (feature 011) when a run is queued before any slot is dispatched.
fn run_demand(state: &RunState) -> HashMap<String, u32> {
    let providers: Vec<&str> = if !state.slots.is_empty() {
        state.slots.iter().map(|s| s.provider.as_str()).collect()
    } else {
        state
            .projected_fleet
            .iter()
            .map(|s| s.provider.as_str())
            .collect()
    };
    bucket_counts(providers.into_iter())
}

fn bucket_counts<'a>(providers: impl Iterator<Item = &'a str>) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for p in providers {
        if let Ok(r) = ProviderRef::parse(p) {
            *out.entry(r.storage_key()).or_insert(0) += 1;
        }
    }
    out
}

/// Slots at `Running`, per run, bucketed by `storage_key()`. The per-run breakdown
/// (rather than a flat total) is what lets `effective_supply` tell "this reservation
/// is now reflected in real state" apart from "this reservation is still the only
/// evidence this run's slot exists" for the *same* run.
fn real_supply_by_run(paths: &SparPaths) -> HashMap<String, HashMap<String, u32>> {
    let mut out: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for summary in crate::state::list_runs(paths).unwrap_or_default() {
        if summary.archived {
            continue;
        }
        let Ok(state) = RunState::load_for_display(paths, &summary.id) else {
            continue;
        };
        let per_bucket = out.entry(summary.id).or_default();
        for slot in &state.slots {
            if slot.status == SlotStatus::Running {
                if let Ok(r) = ProviderRef::parse(&slot.provider) {
                    *per_bucket.entry(r.storage_key()).or_insert(0) += 1;
                }
            }
        }
    }
    out
}

/// Slots at `Running`, across every non-archived run, bucketed by `storage_key()`.
/// Informational only (`spar daemon status`) — admission decisions use
/// `effective_supply`, which also accounts for in-flight reservations.
fn bucket_supply_in_use(paths: &SparPaths) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for per_bucket in real_supply_by_run(paths).values() {
        for (bucket, count) in per_bucket {
            *out.entry(bucket.clone()).or_insert(0) += count;
        }
    }
    out
}

/// Real supply plus every active reservation's *unreflected* demand: a reservation
/// only adds to a bucket the amount by which it exceeds that same run's real slot
/// count there, so a run whose slots have caught up to its reservation is counted
/// once, from real state, not twice. `excluding` is the run currently being decided —
/// its own prior reservation (if any) never counts against itself.
fn effective_supply(
    real_by_run: &HashMap<String, HashMap<String, u32>>,
    reservations: &[Reservation],
    excluding: Option<&str>,
) -> HashMap<String, u32> {
    let mut out: HashMap<String, u32> = HashMap::new();
    for per_bucket in real_by_run.values() {
        for (bucket, count) in per_bucket {
            *out.entry(bucket.clone()).or_insert(0) += count;
        }
    }
    for resv in reservations {
        if Some(resv.run_id.as_str()) == excluding {
            continue;
        }
        let real_for_run = real_by_run.get(&resv.run_id);
        for (bucket, reserved_count) in &resv.buckets {
            let already = real_for_run
                .and_then(|m| m.get(bucket))
                .copied()
                .unwrap_or(0);
            let extra = reserved_count.saturating_sub(already);
            *out.entry(bucket.clone()).or_insert(0) += extra;
        }
    }
    out
}

fn fits(demand: &HashMap<String, u32>, in_use: &HashMap<String, u32>, cap: u32) -> bool {
    if cap == 0 {
        return true;
    }
    demand
        .iter()
        .all(|(bucket, need)| in_use.get(bucket).copied().unwrap_or(0) + need <= cap)
}

/// Launcher-side admission check for `plan --detach` / `implement --detach` /
/// `run --detach`, called right before it would otherwise spawn a detached
/// orchestrator. `Ok(None)` means proceed with the spawn as usual — no daemon holds
/// the lock, the cap is off, or this run's demand fits (and, on the fitting path, has
/// already been reserved so the next concurrent decision sees it as taken). `Ok(Some(message))`
/// means the caller wrote a queue entry instead of spawning and should print `message`
/// and return success without starting anything.
///
/// A launch must never block on a service the operator did not start: with no daemon
/// holding `.spar/daemon.lock`, this always returns `Ok(None)`.
pub fn maybe_enqueue(paths: &SparPaths, cfg: &Config, state: &RunState) -> Result<Option<String>> {
    let cap = cfg.daemon.max_slots_per_bucket;
    if cap == 0 {
        return Ok(None);
    }
    let daemon_alive = DaemonLock::owner(paths).map(|t| t.alive()).unwrap_or(false);
    if !daemon_alive {
        return Ok(None);
    }
    let demand = run_demand(state);
    if demand.is_empty() {
        return Ok(None);
    }
    if try_admit(paths, &state.id, &demand, cap) {
        return Ok(None);
    }
    let dir = paths.queue_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let entry = QueueEntry {
        run_id: state.id.clone(),
        buckets: demand.keys().cloned().collect(),
        enqueued_at: Utc::now(),
    };
    let path = paths.queue_file(&state.id);
    fs::write(&path, serde_json::to_string_pretty(&entry)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(Some(format!(
        "queued behind capacity on {}; spar status {}",
        entry.buckets.join(", "),
        state.id
    )))
}

/// One supervision pass: abandonment detection + notification, restart, queue
/// admission. Best-effort throughout — one unreadable run must never take the
/// daemon down.
fn tick(paths: &SparPaths, cfg: &Config, book: &mut DaemonState) {
    let Ok(summaries) = crate::state::list_runs(paths) else {
        return;
    };
    let now = Utc::now();
    let abandon_after = chrono::Duration::seconds(cfg.daemon.abandon_after_secs as i64);

    for summary in &summaries {
        if summary.archived {
            book.runs.remove(&summary.id);
            continue;
        }
        let Ok(state) = RunState::load_for_display(paths, &summary.id) else {
            continue;
        };
        let entry = book.runs.entry(summary.id.clone()).or_default();

        if !state.abandoned(paths) {
            if entry.abandoned_since.is_some() {
                log_line(paths, &format!("run {} is no longer abandoned", summary.id));
            }
            entry.abandoned_since = None;
            entry.abandoned_notified = false;
            continue;
        }

        let since = *entry.abandoned_since.get_or_insert(now);
        if now - since < abandon_after {
            continue;
        }

        if !entry.abandoned_notified {
            crate::notify::route_abandoned(paths, &state);
            entry.abandoned_notified = true;
            log_line(paths, &format!("run {} abandoned; notified", summary.id));
        }

        // Restart only genuinely in-flight, abandoned work. A phase at rest
        // (`is_waitable_stop()` — terminal, a gate, or `Stopped`) is not abandoned by
        // definition, and `Stopped` in particular was the operator's own choice to
        // park it; un-parking that is `spar resume`, never the daemon.
        if !cfg.daemon.restart || state.phase.is_waitable_stop() {
            continue;
        }
        if entry.restarts >= cfg.daemon.max_restarts {
            continue;
        }
        // A restart is an admission point exactly like a fresh launch: it must not
        // push a bucket past `max_slots_per_bucket` just because the run asking is
        // one the daemon already knew about.
        let demand = run_demand(&state);
        if !try_admit(paths, &summary.id, &demand, cfg.daemon.max_slots_per_bucket) {
            continue;
        }
        match crate::process::spawn_detached_orchestrator(paths, &summary.id) {
            Ok(detached) => {
                match crate::process::await_detached_start(paths, &summary.id, detached) {
                    Ok(_) => {
                        entry.restarts += 1;
                        entry.last_restart_at = Some(now);
                        entry.abandoned_since = None;
                        entry.abandoned_notified = false;
                        log_line(paths, &format!("run {} restarted", summary.id));
                    }
                    Err(e) => {
                        entry.restarts += 1;
                        entry.last_restart_at = Some(now);
                        release_reservation(paths, &summary.id);
                        log_line(paths, &format!("run {} restart failed: {e:#}", summary.id));
                    }
                }
            }
            Err(e) => {
                release_reservation(paths, &summary.id);
                log_line(
                    paths,
                    &format!("run {} restart spawn failed: {e:#}", summary.id),
                );
            }
        }
    }

    drain_queue(paths, cfg);
}

/// Admit queued runs, oldest first, as capacity frees up. FIFO so a wide run cannot be
/// starved by a stream of narrow ones.
fn drain_queue(paths: &SparPaths, cfg: &Config) {
    let cap = cfg.daemon.max_slots_per_bucket;
    if cap == 0 {
        return;
    }
    let dir = paths.queue_dir();
    let Ok(read) = fs::read_dir(&dir) else {
        return;
    };
    let mut entries: Vec<(PathBuf, QueueEntry)> = read
        .flatten()
        .filter_map(|e| {
            let text = fs::read_to_string(e.path()).ok()?;
            let entry: QueueEntry = serde_json::from_str(&text).ok()?;
            Some((e.path(), entry))
        })
        .collect();
    entries.sort_by_key(|(_, e)| e.enqueued_at);

    for (path, entry) in entries {
        let Ok(state) = RunState::load(paths, &entry.run_id) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let demand = run_demand(&state);
        if !try_admit(paths, &entry.run_id, &demand, cap) {
            continue;
        }
        match crate::process::spawn_detached_orchestrator(paths, &entry.run_id) {
            Ok(detached) => {
                if crate::process::await_detached_start(paths, &entry.run_id, detached).is_ok() {
                    let _ = fs::remove_file(&path);
                    log_line(paths, &format!("run {} admitted from queue", entry.run_id));
                } else {
                    release_reservation(paths, &entry.run_id);
                    log_line(
                        paths,
                        &format!("run {} failed to start from queue", entry.run_id),
                    );
                }
            }
            Err(e) => {
                release_reservation(paths, &entry.run_id);
                log_line(
                    paths,
                    &format!("run {} queue spawn failed: {e:#}", entry.run_id),
                );
            }
        }
    }
}

fn stop_marker(paths: &SparPaths) -> PathBuf {
    paths.root.join("daemon.stop")
}

pub fn start(paths: &SparPaths, foreground: bool) -> Result<ExitCode> {
    if !foreground {
        let log = paths.workspace_logs_dir().join("daemon.log");
        let detached =
            crate::process::spawn_detached(&["daemon", "start", "--foreground"], &log, &[])?;
        let pid = detached.pid;
        let deadline = Duration::from_secs(10);
        let start = std::time::Instant::now();
        let mut child = detached.child;
        loop {
            if let Some(owner) = DaemonLock::owner(paths) {
                if owner.pid == pid && owner.alive() {
                    println!("daemon started (pid {pid}); status: spar daemon status");
                    return Ok(ExitCode::Success);
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    anyhow::bail!(
                        "daemon (pid {pid}) exited {status} before starting; see {}",
                        log.display()
                    );
                }
                Ok(None) => {}
                Err(e) => return Err(e).context("wait on detached daemon"),
            }
            if start.elapsed() >= deadline {
                anyhow::bail!(
                    "daemon (pid {pid}) did not start within {}s; see {}",
                    deadline.as_secs(),
                    log.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let _lock = DaemonLock::acquire(paths)?;
    let _ = fs::remove_file(stop_marker(paths));
    crate::process::install_shutdown_handler();
    let mut book = DaemonState::new();
    book.save(paths)?;
    log_line(paths, "daemon started");

    loop {
        if stop_marker(paths).is_file() || crate::process::shutdown_requested() {
            let _ = fs::remove_file(stop_marker(paths));
            log_line(paths, "daemon stopping");
            break;
        }
        let cfg = Config::load(&paths.project_root).unwrap_or_default();
        tick(paths, &cfg, &mut book);
        let _ = book.save(paths);
        let tick_secs = cfg.daemon.tick_secs.max(1);
        for _ in 0..tick_secs {
            if stop_marker(paths).is_file() || crate::process::shutdown_requested() {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    Ok(ExitCode::Success)
}

pub fn stop(paths: &SparPaths) -> Result<ExitCode> {
    let Some(owner) = DaemonLock::owner(paths) else {
        println!("no daemon running for this project");
        return Ok(ExitCode::Success);
    };
    if !owner.alive() {
        println!("no daemon running for this project");
        return Ok(ExitCode::Success);
    }
    fs::write(stop_marker(paths), "1")?;
    println!(
        "stop requested (pid {}); it will exit within one tick",
        owner.pid
    );
    Ok(ExitCode::Success)
}

pub fn status(paths: &SparPaths, json: bool) -> Result<ExitCode> {
    let owner = DaemonLock::owner(paths).filter(|t| t.alive());
    let cfg = Config::load(&paths.project_root).unwrap_or_default();
    let book = DaemonState::load(paths);
    let queue: Vec<String> = fs::read_dir(paths.queue_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    let in_use = bucket_supply_in_use(paths);

    if json {
        let v = serde_json::json!({
            "running": owner.is_some(),
            "pid": owner.as_ref().map(|t| t.pid),
            "started_at": book.started_at,
            "tick_secs": cfg.daemon.tick_secs,
            "abandon_after_secs": cfg.daemon.abandon_after_secs,
            "max_slots_per_bucket": cfg.daemon.max_slots_per_bucket,
            "in_use": in_use,
            "queue": queue,
            "runs": book.runs,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(ExitCode::Success);
    }

    match &owner {
        Some(t) => println!("daemon: running (pid {})", t.pid),
        None => println!("daemon: not running"),
    }
    println!(
        "tick: {}s  abandon_after: {}s  cap: {}",
        cfg.daemon.tick_secs,
        cfg.daemon.abandon_after_secs,
        if cfg.daemon.max_slots_per_bucket == 0 {
            "off".to_string()
        } else {
            cfg.daemon.max_slots_per_bucket.to_string()
        }
    );
    if !in_use.is_empty() {
        println!("in use:");
        for (bucket, n) in &in_use {
            println!("  - {bucket}: {n}");
        }
    }
    if !queue.is_empty() {
        println!("queue: {}", queue.join(", "));
    }
    if !book.runs.is_empty() {
        println!("watching:");
        for (id, rb) in &book.runs {
            println!(
                "  - {id}: restarts={} abandoned_since={:?}",
                rb.restarts, rb.abandoned_since
            );
        }
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn daemon_lock_excludes_a_second_holder_and_clears_pid_on_drop() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let lock = DaemonLock::acquire(&paths).unwrap();
        assert_eq!(lock.pid, std::process::id());
        assert!(DaemonLock::owner(&paths).unwrap().alive());
        drop(lock);
        assert!(DaemonLock::owner(&paths).is_none());
    }

    #[test]
    fn fits_is_off_when_cap_is_zero() {
        let mut demand = HashMap::new();
        demand.insert("cli:claude".to_string(), 5);
        assert!(fits(&demand, &HashMap::new(), 0));
    }

    #[test]
    fn fits_respects_existing_usage() {
        let mut demand = HashMap::new();
        demand.insert("cli:claude".to_string(), 1);
        let mut in_use = HashMap::new();
        in_use.insert("cli:claude".to_string(), 1);
        assert!(!fits(&demand, &in_use, 1));
        assert!(fits(&demand, &in_use, 2));
    }

    #[test]
    fn maybe_enqueue_is_a_noop_with_no_daemon() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut cfg = Config::default();
        cfg.daemon.max_slots_per_bucket = 1;
        let state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        assert!(maybe_enqueue(&paths, &cfg, &state).unwrap().is_none());
    }

    #[test]
    fn maybe_enqueue_is_a_noop_when_the_cap_is_off() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let _lock = DaemonLock::acquire(&paths).unwrap();
        let cfg = Config::default();
        let state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        assert!(maybe_enqueue(&paths, &cfg, &state).unwrap().is_none());
    }

    /// A run with one `Running` slot on `provider`, persisted so `real_supply_by_run`
    /// / `list_runs` can see it.
    fn running_run(paths: &SparPaths, id: &str, provider: &str) -> RunState {
        let mut state = RunState::new(
            id,
            crate::cli::WorkflowKind::Loop,
            paths.project_root.clone(),
        );
        state.phase = crate::state::Phase::Review;
        let mut slot =
            crate::executor::init_slot("impl", provider, crate::state::SlotRole::Implementer);
        slot.status = SlotStatus::Running;
        state.slots.push(slot);
        state.save(paths).unwrap();
        state
    }

    fn demand_of(bucket: &str, n: u32) -> HashMap<String, u32> {
        let mut m = HashMap::new();
        m.insert(bucket.to_string(), n);
        m
    }

    #[test]
    fn try_admit_denies_over_cap_and_reserves_on_success() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let demand = demand_of("cli:claude", 1);

        assert!(
            try_admit(&paths, "r1", &demand, 1),
            "the first run into an empty bucket must be admitted"
        );
        assert!(
            !try_admit(&paths, "r2", &demand, 1),
            "a second run must not be admitted past the cap on r1's still-active reservation"
        );

        let reservations = load_reservations(&paths);
        assert_eq!(reservations.len(), 1);
        assert_eq!(reservations[0].run_id, "r1");
    }

    #[test]
    fn try_admit_replaces_the_same_runs_earlier_reservation_instead_of_stacking() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let demand = demand_of("cli:claude", 1);

        assert!(try_admit(&paths, "r1", &demand, 1));
        // Re-admitting the same run (e.g. a second detach attempt) must not double its
        // own reservation and starve itself.
        assert!(try_admit(&paths, "r1", &demand, 1));
        assert_eq!(load_reservations(&paths).len(), 1);
    }

    #[test]
    fn effective_supply_drops_a_reservations_extra_once_real_slots_catch_up() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let demand = demand_of("cli:claude", 1);
        assert!(try_admit(&paths, "r1", &demand, 1));

        // Before r1's slot is real, the reservation alone must still block a second
        // run.
        assert!(!try_admit(&paths, "r2", &demand, 1));

        // Once r1's real slot is `Running`, the reservation's contribution collapses
        // to zero (real state already counts it) rather than double-counting — but the
        // bucket is still at cap from the real slot itself, so admission still fails.
        running_run(&paths, "r1", "cli:claude");
        let real_by_run = real_supply_by_run(&paths);
        let reservations = load_reservations(&paths);
        let effective = effective_supply(&real_by_run, &reservations, None);
        assert_eq!(effective.get("cli:claude").copied(), Some(1));
    }

    #[test]
    fn release_reservation_frees_capacity_for_the_next_admission() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let demand = demand_of("cli:claude", 1);
        assert!(try_admit(&paths, "r1", &demand, 1));
        assert!(!try_admit(&paths, "r2", &demand, 1));

        release_reservation(&paths, "r1");
        assert!(
            try_admit(&paths, "r2", &demand, 1),
            "releasing r1's reservation must free the bucket for r2"
        );
    }

    #[test]
    fn reservations_past_ttl_are_pruned() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let stale = Reservation {
            run_id: "old".to_string(),
            buckets: demand_of("cli:claude", 1),
            reserved_at: Utc::now() - chrono::Duration::seconds(RESERVATION_TTL_SECS + 1),
        };
        save_reservations(&paths, &[stale]).unwrap();
        assert!(
            load_reservations(&paths).is_empty(),
            "a reservation past its TTL must not still count toward capacity"
        );
    }

    #[test]
    fn maybe_enqueue_writes_a_queue_entry_when_over_cap() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let _lock = DaemonLock::acquire(&paths).unwrap();
        let mut cfg = Config::default();
        cfg.daemon.max_slots_per_bucket = 1;
        running_run(&paths, "already-running", "cli:claude");

        let state = RunState::new(
            "r2",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        let mut state = state;
        let mut slot =
            crate::executor::init_slot("impl", "cli:claude", crate::state::SlotRole::Implementer);
        slot.status = SlotStatus::Pending;
        state.slots.push(slot);

        let msg = maybe_enqueue(&paths, &cfg, &state).unwrap();
        assert!(
            msg.is_some(),
            "a run over the cap must be queued, not spawned"
        );
        assert!(paths.queue_file("r2").is_file());
    }

    #[test]
    fn restart_is_skipped_once_max_restarts_is_reached() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.phase = crate::state::Phase::Review; // in-flight, not waitable-stop
        state.save(&paths).unwrap();

        let mut cfg = Config::default();
        cfg.daemon.restart = true;
        cfg.daemon.abandon_after_secs = 0;
        cfg.daemon.max_restarts = 0;
        let mut book = DaemonState::new();
        // Pre-seed as already having burned its one restart, so `tick` must skip the
        // spawn attempt entirely rather than trying `spawn_detached_orchestrator`
        // against a test binary that cannot stand in for a real orchestrator.
        book.runs.entry("r1".to_string()).or_default().restarts = 0;

        tick(&paths, &cfg, &mut book);

        assert!(
            !paths.logs_dir("r1").join("orchestrator.log").is_file(),
            "max_restarts = 0 must stop tick from ever attempting a spawn"
        );
    }

    #[test]
    fn queued_run_is_never_marked_abandoned_by_tick() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.phase = crate::state::Phase::Init;
        state.save(&paths).unwrap();
        std::fs::create_dir_all(paths.queue_dir()).unwrap();
        std::fs::write(paths.queue_file("r1"), "{}").unwrap();

        let mut cfg = Config::default();
        cfg.daemon.restart = false;
        cfg.daemon.abandon_after_secs = 0;
        let mut book = DaemonState::new();

        tick(&paths, &cfg, &mut book);

        assert!(
            book.runs
                .get("r1")
                .and_then(|b| b.abandoned_since)
                .is_none(),
            "a run sitting in the admission queue is owned by the queue, not abandoned"
        );
    }

    #[test]
    fn abandonment_notified_flag_is_set_once_and_stays_set() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.phase = crate::state::Phase::Review;
        state.save(&paths).unwrap();

        let mut cfg = Config::default();
        cfg.daemon.restart = false;
        cfg.daemon.abandon_after_secs = 0;
        let mut book = DaemonState::new();

        tick(&paths, &cfg, &mut book);
        assert!(book.runs["r1"].abandoned_notified);
        let since_first = book.runs["r1"].abandoned_since;

        tick(&paths, &cfg, &mut book);
        assert!(
            book.runs["r1"].abandoned_notified,
            "the notified flag must not reset while the run stays abandoned"
        );
        assert_eq!(
            book.runs["r1"].abandoned_since, since_first,
            "the abandonment episode must not restart while it is ongoing"
        );
    }

    /// D9, structural half: a source-level check that the daemon never mentions any
    /// operator-only call. Cheap, and it is what stops the prohibition in this file's
    /// own module doc from decaying into a comment nobody enforces.
    #[test]
    fn daemon_source_never_mentions_operator_only_calls() {
        // Scan only the non-test body: the `#[cfg(test)]` module below (this test
        // included) legitimately names these calls, both in doc comments and in this
        // very assertion list.
        let full = include_str!("daemon.rs");
        let body = full.split("#[cfg(test)]").next().unwrap();
        // Exclude comment/doc lines within the body too: the module doc at the top of
        // this file names these calls in English, which is the point of the comment,
        // not a violation of it.
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "cleanup_run",
            "pick_providers",
            "archive_sweep",
            "gates.plan_approved",
            "gates.ship_confirmed",
            "ship::",
        ] {
            assert!(
                !code.contains(banned),
                "src/daemon.rs must never call `{banned}` — a supervisor is not an operator"
            );
        }
    }

    /// D9, behavioural half: a tick over a run parked at a gate must not touch it.
    #[test]
    fn tick_leaves_a_gated_run_byte_identical() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.phase = crate::state::Phase::AwaitingShipConfirm;
        state.gates.ship_confirmed = false;
        state.save(&paths).unwrap();
        let before = std::fs::read_to_string(paths.state_file("r1")).unwrap();

        let cfg = Config::default();
        let mut book = DaemonState::new();
        for _ in 0..3 {
            tick(&paths, &cfg, &mut book);
        }

        let after = std::fs::read_to_string(paths.state_file("r1")).unwrap();
        // `updated_at` never moves because `save` is never called again for this run —
        // a gate is `is_waitable_stop()`, so `tick` takes none of its branches beyond
        // the very first `abandoned()` check, which reads false and `continue`s.
        assert_eq!(before, after, "a gate must be untouched by the daemon");
    }
}
