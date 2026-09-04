use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamStats {
    pub tools: u32,
    pub tool_errors: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Peak prompt footprint of a single request (input + cache read + cache write),
    /// i.e. how full the agent's window got. Not a running total: it is what the
    /// context gauge is read against, so a long run does not drift past every
    /// threshold just by making more calls.
    pub context_tokens: u64,
    /// Cumulative billed tokens for the whole slot, under each adapter's own
    /// convention: `input + cache_read + cache_write + output` as written above, with
    /// reasoning already folded into `output`. This is the spend meter and the number
    /// a token budget is enforced on; `context_tokens` is the gauge and the two are
    /// never interchangeable.
    #[serde(default)]
    pub billed_tokens: u64,
    /// The provider's own structured rate-limit rejection, when it emitted one:
    /// the `rateLimitType` (`five_hour`, `seven_day`, …) from a `rate_limit_event`
    /// whose status was `rejected`. This is a typed fact from the adapter, not a
    /// phrase recovered from rendered log text, so it cannot be produced by source
    /// code, documentation or a task brief that merely discusses limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_rejected: Option<String>,
    /// When that window reopens, epoch seconds, as the provider stated it. An absolute
    /// instant, so it needs no inference from a wall-clock time and no timezone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider-side session id, when the stream names one. muse's usage lives outside
    /// its stdout stream, so this is how the slot's session log is found afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub lines_in: u64,
    pub chars_out: u64,
    /// RFC3339 of last successful log append (for stall detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_log_at: Option<String>,
}

impl StreamStats {
    pub fn touch_log(&mut self) {
        self.last_log_at =
            Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }

    pub fn stats_path(log_path: &Path) -> PathBuf {
        let mut p = log_path.to_path_buf();
        let stem = log_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("slot");
        p.set_file_name(format!("{stem}.stats.json"));
        p
    }

    pub fn save(&self, log_path: &Path) -> Result<()> {
        let path = Self::stats_path(log_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load(log_path: &Path) -> Option<Self> {
        let path = Self::stats_path(log_path);
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

#[derive(Debug)]
pub struct SpawnResult {
    pub exit_code: Option<i32>,
    /// Terminating signal number for signal-killed children (Unix); None otherwise.
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[allow(dead_code)]
    pub log_path: PathBuf,
    #[allow(dead_code)]
    pub stdout_tail: String,
    pub stats: StreamStats,
}

/// True if a process with `pid` is still addressable (`kill(pid, 0) == 0`).
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid as i32, 0) == 0
    }
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

/// Kernel start-time (jiffies since boot, field 22 of `/proc/<pid>/stat`).
/// Unique per live pid, so it distinguishes a process from a later reuse of its pid.
#[cfg(target_os = "linux")]
pub fn pid_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesised and may itself contain ')' / spaces; skip past
    // the last ')', then starttime is the 20th whitespace field (field 22 overall).
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn pid_starttime(_pid: u32) -> Option<u64> {
    None
}

/// A recorded pid tied to the start-time of the process that owned it, so a pid
/// recycled by the OS onto an unrelated process is never mistaken for the original
/// and never signalled. Persisted as `pid` or `pid:starttime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidToken {
    pub pid: u32,
    pub starttime: Option<u64>,
}

impl PidToken {
    /// Capture identity for a live pid (call while the process is known to be running).
    pub fn capture(pid: u32) -> Self {
        Self {
            pid,
            starttime: pid_starttime(pid),
        }
    }

    /// A pid with no identity token (legacy record or a platform without `/proc`).
    pub fn from_pid(pid: u32) -> Self {
        Self {
            pid,
            starttime: None,
        }
    }

    /// True only if the pid is live AND still the same process we recorded. When no
    /// start-time was captured (unknown platform / legacy record) we cannot prove
    /// identity, so fall back to bare liveness rather than refusing to act.
    pub fn alive(&self) -> bool {
        match self.starttime {
            Some(st) => pid_starttime(self.pid) == Some(st),
            None => pid_alive(self.pid),
        }
    }

    pub fn encode(&self) -> String {
        match self.starttime {
            Some(st) => format!("{}:{st}", self.pid),
            None => self.pid.to_string(),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        match s.split_once(':') {
            Some((pid, st)) => Some(Self {
                pid: pid.trim().parse().ok()?,
                starttime: st.trim().parse().ok(),
            }),
            None => Some(Self {
                pid: s.parse().ok()?,
                starttime: None,
            }),
        }
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Spawn process; stream structured events into a human log + live stats file.
/// `on_spawn` fires with the child pid the moment spawn succeeds, before the wait
/// loop, so callers can record a live pid. `on_tick` fires on every wait-poll
/// iteration while the child is still alive; the supervisor uses it to refresh the
/// slot's liveness (the callback self-throttles the actual cadence).
pub fn run_captured(
    req: &SpawnRequest,
    on_spawn: Option<&dyn Fn(u32)>,
    on_tick: Option<&dyn Fn()>,
) -> Result<SpawnResult> {
    if let Some(parent) = req.log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }

    let header = format!(
        "# spawn {} {}\ncwd={}\n---\n",
        req.program.display(),
        req.args.join(" "),
        req.cwd.display()
    );
    {
        let mut f = File::create(&req.log_path)
            .with_context(|| format!("create log {}", req.log_path.display()))?;
        f.write_all(header.as_bytes())?;
        f.flush()?;
    }
    let mut initial = StreamStats::default();
    initial.touch_log();
    let _ = initial.save(&req.log_path);

    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args)
        .current_dir(&req.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    cmd.env("PYTHONUNBUFFERED", "1");
    // Own process group so timeout can kill nested suites (cargo test, etc.).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", req.program.display()))?;
    #[cfg(unix)]
    shutdown::track(child.id());
    let tracked_pid = child.id();
    if let Some(cb) = on_spawn {
        cb(child.id());
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log_path = req.log_path.clone();
    let log_err = req.log_path.clone();
    let stats_holder = std::sync::Arc::new(std::sync::Mutex::new(StreamStats::default()));
    let stats_out = stats_holder.clone();
    let stats_err = stats_holder.clone();

    let t_out = std::thread::spawn(move || {
        if let Some(out) = stdout {
            stream_to_log(out, &log_path, false, stats_out);
        }
    });
    let t_err = std::thread::spawn(move || {
        if let Some(err) = stderr {
            stream_to_log(err, &log_err, true, stats_err);
        }
    });

    let start = Instant::now();
    let poll = Duration::from_millis(50);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if start.elapsed() >= req.timeout {
                    let status = kill_process_group(&mut child)?;
                    let _ = t_out.join();
                    let _ = t_err.join();
                    #[cfg(unix)]
                    shutdown::untrack(tracked_pid);
                    append_log(&req.log_path, "\n! timed out\n")?;
                    let stats = stats_holder.lock().map(|s| s.clone()).unwrap_or_default();
                    let _ = stats.save(&req.log_path);
                    return Ok(SpawnResult {
                        exit_code: status.code(),
                        signal: exit_signal(&status),
                        timed_out: true,
                        log_path: req.log_path.clone(),
                        stdout_tail: tail_log(&req.log_path, 4000),
                        stats,
                    });
                }
                if let Some(tick) = on_tick {
                    tick();
                }
                std::thread::sleep(poll);
            }
        }
    };

    let _ = t_out.join();
    let _ = t_err.join();
    #[cfg(unix)]
    shutdown::untrack(tracked_pid);
    let stats = stats_holder.lock().map(|s| s.clone()).unwrap_or_default();
    let _ = stats.save(&req.log_path);
    Ok(SpawnResult {
        exit_code: status.code(),
        signal: exit_signal(&status),
        timed_out: false,
        log_path: req.log_path.clone(),
        stdout_tail: tail_log(&req.log_path, 4000),
        stats,
    })
}

/// SIGTERM the process group, brief grace, then always SIGKILL the group
/// (even if the leader already reaped — grandchildren may still be alive).
/// Returns the reaped exit status; sole owner of `wait`.
fn kill_process_group(child: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Negative pid = process group (child is group leader via process_group(0)).
        signal_process_group(pid, SIGTERM);
        let grace = Instant::now();
        while grace.elapsed() < Duration::from_secs(2) {
            if let Ok(Some(st)) = child.try_wait() {
                // Leader gone; still SIGKILL group for nested suite orphans.
                signal_process_group(pid, SIGKILL);
                return Ok(st);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        signal_process_group(pid, SIGKILL);
        let _ = child.kill();
        child.wait().context("wait after process-group kill")
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        child.wait().context("wait after kill")
    }
}

/// Graceful shutdown for the orchestrator process.
///
/// Slots are spawned into their **own process groups** (so a slot timeout can reap
/// nested `cargo test` / `pnpm build` children), which also means they survive the
/// orchestrator's death — the orphaned-agents-still-burning-tokens case. On SIGINT or
/// SIGTERM the orchestrator now signals every live slot group before it goes.
///
/// SIGKILL cannot be caught, so this covers the polite kill only; `spar stop --abandoned`
/// and `spar wait`'s abandonment check exist for the rest.
#[cfg(unix)]
mod shutdown {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Slot groups to signal from the handler. A fixed array, not a Vec behind a lock:
    /// a signal handler may only touch async-signal-safe things, which rules out
    /// allocation and locking. Fleets are single digits; 64 is far past any real run.
    /// `sighandler_t` as the C library sees it: either the default disposition or a
    /// handler function. Modelled as an enum so the handler is passed as a typed fn
    /// pointer rather than cast through an integer.
    #[repr(transparent)]
    struct SigHandler(usize);

    impl SigHandler {
        const DFL: SigHandler = SigHandler(0);

        #[allow(non_snake_case)]
        fn Handler(f: extern "C" fn(i32)) -> SigHandler {
            SigHandler(f as *const () as usize)
        }
    }

    const SIG_DFL: SigHandler = SigHandler::DFL;

    extern "C" {
        fn signal(signum: i32, handler: SigHandler) -> SigHandler;
        fn getpid() -> i32;
    }

    const MAX_TRACKED: usize = 64;
    static SLOT_PIDS: [AtomicU32; MAX_TRACKED] = [const { AtomicU32::new(0) }; MAX_TRACKED];
    static SHUTDOWN: AtomicBool = AtomicBool::new(false);
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    pub fn track(pid: u32) {
        for slot in SLOT_PIDS.iter() {
            if slot
                .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn untrack(pid: u32) {
        for slot in SLOT_PIDS.iter() {
            let _ = slot.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    pub fn requested() -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }

    /// Reap, then die as asked.
    ///
    /// Async-signal-safe: atomics and `kill(2)`, nothing else — no allocation, no locks,
    /// no file I/O, so the run's phase is deliberately *not* written here. After
    /// signalling the slot groups the handler restores the default disposition and
    /// re-raises, so `spar` still terminates on the operator's signal instead of
    /// surviving it and reporting some later phase as the run's outcome. The run is left
    /// mid-phase, which reads as abandoned — `spar wait`, `spar status` and
    /// `spar stop --abandoned` all handle that, and the tokens have stopped burning.
    extern "C" fn on_signal(sig: i32) {
        SHUTDOWN.store(true, Ordering::SeqCst);
        for slot in SLOT_PIDS.iter() {
            let pid = slot.load(Ordering::SeqCst);
            if pid != 0 {
                super::raw_kill(-(pid as i32), super::SIGTERM);
            }
        }
        unsafe {
            signal(sig, SIG_DFL);
            super::raw_kill(getpid(), sig);
        }
    }

    pub fn install() {
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return;
        }
        const SIGINT: i32 = 2;
        unsafe {
            signal(SIGINT, SigHandler::Handler(on_signal));
            signal(super::SIGTERM, SigHandler::Handler(on_signal));
        }
    }
}

/// Install the orchestrator's shutdown handler. Call once per orchestrating process —
/// never from the TUI, where Ctrl+C belongs to the agent in the Shell tab.
pub fn install_shutdown_handler() {
    #[cfg(unix)]
    shutdown::install();
}

/// True once SIGINT/SIGTERM arrived: dispatch loops stop at their next boundary and
/// park the run at `Stopped` instead of leaving slots orphaned.
pub fn shutdown_requested() -> bool {
    #[cfg(unix)]
    {
        shutdown::requested()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
fn raw_kill(pid: i32, sig: i32) {
    // libc kill — no dependency on the `kill` binary / PATH.
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let _ = kill(pid, sig);
    }
}

#[cfg(unix)]
fn signal_process_group(pid: i32, sig: i32) {
    // Negative pid = whole process group (leader via process_group(0)).
    raw_kill(-pid, sig);
}

/// SIGTERM a live target, brief grace, then SIGKILL. With `group`, signals the
/// whole process group (negative pid) so nested suite children are reaped too.
/// Slots run in their own group (`process_group(0)`); the orchestrator does not,
/// so it must be signalled by its bare pid.
#[cfg(unix)]
pub fn terminate_tree(pid: u32, group: bool) {
    let target = if group { -(pid as i32) } else { pid as i32 };
    raw_kill(target, SIGTERM);
    let grace = Instant::now();
    while grace.elapsed() < Duration::from_secs(2) {
        if !pid_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    raw_kill(target, SIGKILL);
}

#[cfg(not(unix))]
pub fn terminate_tree(_pid: u32, _group: bool) {}

/// SIGTERM every pid, one shared grace window, then SIGKILL whatever survived.
#[cfg(unix)]
pub fn terminate_all(pids: &[u32]) {
    for &pid in pids {
        raw_kill(pid as i32, SIGTERM);
    }
    let grace = Instant::now();
    while grace.elapsed() < Duration::from_secs(2) {
        if pids.iter().all(|&p| !pid_alive(p)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for &pid in pids {
        if pid_alive(pid) {
            raw_kill(pid as i32, SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub fn terminate_all(_pids: &[u32]) {}

/// Pids whose working directory is inside `dir`, via `/proc/<pid>/cwd`.
///
/// Matching on cwd rather than command line is deliberate: a command-line match
/// self-matches the `spar cleanup` invocation (and its shell), and killing those is how
/// you take out your own terminal. Our own pid and every ancestor are excluded for the
/// same reason — cleanup is often run from inside the worktree it is reaping.
#[cfg(target_os = "linux")]
pub fn pids_with_cwd_under(dir: &Path) -> Vec<u32> {
    let Ok(root) = dir.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let skip = self_and_ancestors();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if skip.contains(&pid) {
            continue;
        }
        let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        if cwd.starts_with(&root) {
            out.push(pid);
        }
    }
    out.sort_unstable();
    out
}

#[cfg(not(target_os = "linux"))]
pub fn pids_with_cwd_under(_dir: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn self_and_ancestors() -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = std::process::id();
    while pid > 1 && !out.contains(&pid) {
        out.push(pid);
        match pid_parent(pid) {
            Some(p) => pid = p,
            None => break,
        }
    }
    out
}

/// Parent pid: field 4 of `/proc/<pid>/stat`, i.e. the second field after `comm`.
#[cfg(target_os = "linux")]
fn pid_parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

fn stream_to_log(
    pipe: impl Read,
    log_path: &Path,
    is_err: bool,
    stats: std::sync::Arc<std::sync::Mutex<StreamStats>>,
) {
    let reader = BufReader::new(pipe);
    let mut c = StreamCoalescer::new(is_err);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Ok(mut s) = stats.lock() {
            s.lines_in += 1;
        }
        // Counters are merged per line, not per log append: opencode's `step_finish`
        // carries a whole model call's tokens and prints nothing, so gating the merge on
        // an append left the sidecar behind the stream by however many silent events came
        // last. A live token budget reads that sidecar, so "behind" means "never fires".
        let chunk = c.feed(&line);
        let appended = chunk
            .as_ref()
            .map(|ch| append_log(log_path, ch).is_ok())
            .unwrap_or(false);
        if let Ok(mut s) = stats.lock() {
            if appended {
                s.chars_out += chunk.map(|ch| ch.len() as u64).unwrap_or_default();
                // Persist last_log_at every append so status/TUI never read a stale stamp.
                s.touch_log();
            }
            c.merge_counters_into(&mut s);
            let _ = s.save(log_path);
        }
    }
    // The final merge is unconditional. Counters used to ride along with a log append,
    // so an event that updates them without printing anything (opencode's terminal
    // `step_finish`, which is where its last call's tokens live) never reached the
    // sidecar at all.
    let tail = c.finish();
    let appended = tail
        .as_ref()
        .map(|chunk| append_log(log_path, chunk).is_ok())
        .unwrap_or(false);
    if let Ok(mut s) = stats.lock() {
        if appended {
            s.chars_out += tail.map(|c| c.len() as u64).unwrap_or_default();
            s.touch_log();
        } else if s.last_log_at.is_none() {
            // Keep any prior last_log_at (e.g. spawn header); do not wipe with defaults.
            s.touch_log();
        }
        c.merge_counters_into(&mut s);
        let _ = s.save(log_path);
    }
}

struct StreamCoalescer {
    is_err: bool,
    /// Set from the provider's `rate_limit_event` when its status is `rejected`.
    quota_rejected: Option<String>,
    /// The instant that window reopens, epoch seconds, straight from the same event.
    quota_resets_at: Option<i64>,
    buf: String,
    kind: CoalesceKind,
    tools: u32,
    tool_errors: u32,
    input_tokens: u64,
    output_tokens: u64,
    est_output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
    /// Largest single-request prompt footprint seen, from per-request usage only.
    context_peak: u64,
    /// A cumulative end-of-invocation usage record has been absorbed, so per-request
    /// records must no longer touch the billed components.
    saw_terminal_usage: bool,
    model: Option<String>,
    session_id: Option<String>,
    text_chars: u64,
    /// opencode double-emits every event in dash and underscore spellings with the
    /// same `part.id`; keyed by `(normalized type, part.id)` to count each once.
    seen_opencode: std::collections::HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CoalesceKind {
    None,
    Text,
    Thought,
}

/// What one `usage` record covers. Providers report both shapes on the same stream and
/// they cannot be added together: claude's `result` and codex's `turn.completed` are the
/// invocation total, every other record is one model call.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UsageScope {
    Request,
    Terminal,
}

impl StreamCoalescer {
    fn new(is_err: bool) -> Self {
        Self {
            is_err,
            quota_rejected: None,
            quota_resets_at: None,
            buf: String::new(),
            kind: CoalesceKind::None,
            tools: 0,
            tool_errors: 0,
            input_tokens: 0,
            output_tokens: 0,
            est_output_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            context_peak: 0,
            saw_terminal_usage: false,
            model: None,
            session_id: None,
            text_chars: 0,
            seen_opencode: std::collections::HashSet::new(),
        }
    }

    fn feed(&mut self, line: &str) -> Option<String> {
        let t = line.trim();
        if t.is_empty() {
            return None;
        }
        if self.is_err {
            return Some(format!("! {line}\n"));
        }
        if !t.starts_with('{') {
            let mut out = self.flush_buf();
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
            self.note_text(line);
            return Some(out);
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(t) else {
            let mut out = self.flush_buf();
            out.push_str(t);
            out.push('\n');
            return Some(out);
        };

        let scope = match v.get("type").and_then(|x| x.as_str()) {
            Some("result") | Some("turn.completed") => UsageScope::Terminal,
            _ => UsageScope::Request,
        };
        self.absorb_usage(&v, scope);

        // opencode `run --format json` NDJSON. Each line carries a top-level `sessionID`
        // and a `part` object; that pair is unique to opencode and gates it off before
        // the shared `type`-matched branches below (opencode reuses type strings like
        // "text"/"tool_use" that Grok's stream also uses). Returned unconditionally so an
        // opencode line never falls through to another provider's parser.
        if v.get("sessionID").is_some() && v.get("part").is_some() {
            return self.handle_opencode(&v);
        }

        // `muse exec --json` event envelope. Every line carries `payload_type` plus a
        // `stream` object; that pair is unique to muse and gates it off before the
        // `type`-matched branches below, which muse lines would otherwise fall through.
        if let Some(pt) = v.get("payload_type").and_then(|x| x.as_str()) {
            if v.get("stream").is_some() {
                return self.handle_muse(pt, &v);
            }
        }

        // Grok token stream
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            if matches!(ty, "text" | "thought" | "output" | "response") {
                if let Some(data) = v.get("data").and_then(|x| x.as_str()) {
                    let kind = if ty == "thought" {
                        CoalesceKind::Thought
                    } else {
                        CoalesceKind::Text
                    };
                    return self.push_token(kind, data);
                }
            }
            if matches!(ty, "tool_call" | "tool_use" | "function_call") {
                let mut out = self.flush_buf();
                self.tools += 1;
                let name = v
                    .get("name")
                    .or_else(|| v.pointer("/tool/name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool");
                let detail = v
                    .get("arguments")
                    .or_else(|| v.get("input"))
                    .map(|x| truncate_json(x, 80))
                    .unwrap_or_default();
                if detail.is_empty() {
                    out.push_str(&format!("→ {name}\n"));
                } else {
                    out.push_str(&format!("→ {name}  {detail}\n"));
                }
                return Some(out);
            }
        }

        // Claude stream-json
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            match ty {
                "system" => {
                    let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("");
                    if sub == "init" {
                        let mut out = self.flush_buf();
                        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("claude");
                        self.model = Some(model.to_string());
                        out.push_str(&format!("· session  {model}\n"));
                        return Some(out);
                    }
                    return None;
                }
                "rate_limit_event" => {
                    let status = v
                        .pointer("/rate_limit_info/status")
                        .and_then(|x| x.as_str())
                        .unwrap_or("?");
                    let kind = v
                        .pointer("/rate_limit_info/rateLimitType")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    if status == "rejected" {
                        // Captured here, typed, rather than recovered downstream from
                        // the rendered line: `status` is the provider's own verdict on
                        // this request, so routing on it cannot false-positive on prose
                        // that merely mentions limits.
                        self.quota_rejected = Some(if kind.is_empty() {
                            "unknown".to_string()
                        } else {
                            kind.to_string()
                        });
                        // The same object states when the window reopens, as an absolute
                        // instant in epoch seconds — day included, no timezone, nothing
                        // inferred. Prefer the window-specific entry when the payload
                        // carries `unifiedWindows`, else the top-level one. Reading the
                        // rendered sentence's wall-clock time instead is what made the
                        // weekly reset look unknowable when it never was.
                        self.quota_resets_at = v
                            .pointer(&format!("/rate_limit_info/unifiedWindows/{kind}/resetsAt"))
                            .and_then(|x| x.as_i64())
                            .or_else(|| {
                                v.pointer("/rate_limit_info/resetsAt")
                                    .and_then(|x| x.as_i64())
                            });
                    }
                    if status != "allowed" {
                        return Some(format!("! rate limit  {kind}  {status}\n"));
                    }
                    return None;
                }
                "stream_event" => {
                    if let Some(text) = v
                        .pointer("/event/delta/text")
                        .and_then(|x| x.as_str())
                        .or_else(|| {
                            v.pointer("/event/delta")
                                .and_then(|d| d.get("text"))
                                .and_then(|x| x.as_str())
                        })
                    {
                        return self.push_token(CoalesceKind::Text, text);
                    }
                    let ev = v.pointer("/event/type").and_then(|x| x.as_str());
                    if matches!(ev, Some("content_block_stop") | Some("message_stop")) {
                        return self.flush_buf_opt();
                    }
                    return None;
                }
                "assistant" => return self.handle_claude_assistant(&v),
                "user" => return self.handle_claude_user(&v),
                "result" => {
                    let mut out = self.flush_buf();
                    let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("ok");
                    out.push_str(&format!(
                        "· done  {sub}  ·  {} tools  ·  {}\n",
                        self.tools,
                        format_tokens(
                            self.input_tokens,
                            self.output_tokens.max(self.est_output_tokens),
                            self.cache_read
                        )
                    ));
                    return Some(out);
                }
                "error" => {
                    let mut out = self.flush_buf();
                    // Unwrap JSON string values so codex's top-level
                    // `{"type":"error","message":"…"}` renders without literal quotes.
                    let msg = v
                        .get("error")
                        .or_else(|| v.get("message"))
                        .map(|x| match x {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_else(|| t.into());
                    out.push_str(&format!("! {msg}\n"));
                    return Some(out);
                }
                _ => {}
            }
        }

        // Codex exec JSONL (thread.started / turn.* / item.* events).
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            match ty {
                "thread.started" | "turn.started" | "item.started" => return None,
                // `codex exec` emits exactly one turn.completed per invocation, and it
                // is the only usage record it emits at all, so it is Terminal-scoped and
                // settles every counter outright.
                "turn.completed" => {
                    let mut out = self.flush_buf();
                    out.push_str(&format!(
                        "· turn  ·  {} tools  ·  {}\n",
                        self.tools,
                        format_tokens(
                            self.input_tokens,
                            self.output_tokens.max(self.est_output_tokens),
                            self.cache_read
                        )
                    ));
                    return Some(out);
                }
                "turn.failed" => {
                    let mut out = self.flush_buf();
                    let msg = v
                        .pointer("/error/message")
                        .and_then(|x| x.as_str())
                        .unwrap_or("turn failed");
                    out.push_str(&format!("! {}\n", first_line(msg, 160)));
                    return Some(out);
                }
                "item.completed" => return self.handle_codex_item(&v),
                _ => {}
            }
        }

        if t.len() > 240 {
            return None;
        }
        None
    }

    /// muse's stream carries no token counts at all; `providers::muse_telemetry` recovers
    /// those from the session log after the slot exits, keyed by the session id captured here.
    fn handle_muse(&mut self, payload_type: &str, v: &serde_json::Value) -> Option<String> {
        match payload_type {
            "run.model.configured" => {
                let mut out = self.flush_buf();
                let model = v
                    .pointer("/payload/model_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("muse");
                self.model = Some(model.to_string());
                if let Some(id) = v
                    .pointer("/stream/id")
                    .and_then(|x| x.as_str())
                    .filter(|_| {
                        v.pointer("/stream/kind").and_then(|x| x.as_str()) == Some("session")
                    })
                {
                    self.session_id = Some(id.to_string());
                }
                out.push_str(&format!("· session  {model}\n"));
                Some(out)
            }
            // Deltas arrive mid-word, so they buffer like any other token stream; emitting
            // each one as its own line breaks identifiers and paths across lines.
            "run.output.delta" => {
                let text = v.pointer("/payload/text").and_then(|x| x.as_str())?;
                if text.is_empty() {
                    return None;
                }
                self.push_token(CoalesceKind::Text, text)
            }
            "tool.result" => {
                let mut out = self.flush_buf();
                self.tools += 1;
                let name = v
                    .pointer("/payload/correlation_facts/tool_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool");
                let outcome = v
                    .pointer("/payload/correlation_facts/outcome")
                    .and_then(|x| x.as_str())
                    .unwrap_or("success");
                if outcome != "success" {
                    self.tool_errors += 1;
                }
                let detail = v
                    .pointer("/payload/edit_facts/path")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                if detail.is_empty() {
                    out.push_str(&format!("→ {name}  {outcome}\n"));
                } else {
                    out.push_str(&format!("→ {name}  {detail}  {outcome}\n"));
                }
                Some(out)
            }
            "run.terminal.completed" => {
                let mut out = self.flush_buf();
                out.push_str(&format!("· done  ·  {} tools\n", self.tools));
                Some(out)
            }
            "run.terminal.failed" | "run.terminal.cancelled" => {
                let mut out = self.flush_buf();
                let reason = v
                    .pointer("/payload/reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("run did not complete");
                out.push_str(&format!("! {}\n", first_line(reason, 160)));
                Some(out)
            }
            _ => None,
        }
    }

    fn handle_opencode(&mut self, v: &serde_json::Value) -> Option<String> {
        let part = v.get("part")?;
        // opencode keeps its own per-session token ledger, so recording the id makes a
        // slot's numbers checkable against `opencode.db` afterwards.
        if self.session_id.is_none() {
            if let Some(id) = v.get("sessionID").and_then(|x| x.as_str()) {
                self.session_id = Some(id.to_string());
            }
        }
        // opencode republishes `PartUpdated` for a part it already sent (every consumer
        // in its own bundle upserts parts by `messageID:partID` rather than appending),
        // so one `part.id` can legitimately arrive more than once. Since every field is
        // summed below, a repeat would bill the step twice; the dedupe is what makes the
        // sum safe. The key includes the type so `step_start:X` and `step_finish:X`
        // (same part.id, distinct events) are not collapsed.
        //
        // The `-` to `_` normalization is belt and braces: the json emitter writes five
        // hardcoded underscore literals (`tool_use`, `step_start`, `step_finish`, `text`,
        // `reasoning`) and the dash spelling appears only on `part.type`, so no top-level
        // `type` observed carries a dash.
        let ptype = v.get("type").and_then(|x| x.as_str())?.replace('-', "_");
        let pid = part.get("id").and_then(|x| x.as_str()).unwrap_or_default();
        if !self.seen_opencode.insert(format!("{ptype}:{pid}")) {
            return None;
        }
        match ptype.as_str() {
            // One opencode step is one LLM call and every field on it is that call's
            // own delta, never a running total: opencode's session ledger is the plain
            // sum of its steps (verified against `opencode.db`, where `tokens.total`
            // per step is likewise input + output + reasoning + cache read + write).
            // Summing is only safe because the republish dedupe above runs first.
            "step_finish" => {
                if let Some(tok) = part.get("tokens") {
                    let n = |k: &str| tok.get(k).and_then(|x| x.as_u64()).unwrap_or_default();
                    let ptr = |k: &str| tok.pointer(k).and_then(|x| x.as_u64()).unwrap_or_default();
                    let (input, cache_read, cache_write) =
                        (n("input"), ptr("/cache/read"), ptr("/cache/write"));
                    self.input_tokens = self.input_tokens.saturating_add(input);
                    // Reasoning is billed at output rates and opencode reports it
                    // outside `output`, so it belongs in the output component.
                    self.output_tokens = self
                        .output_tokens
                        .saturating_add(n("output"))
                        .saturating_add(n("reasoning"));
                    self.cache_read = self.cache_read.saturating_add(cache_read);
                    self.cache_write = self.cache_write.saturating_add(cache_write);
                    self.context_peak = self
                        .context_peak
                        .max(input.saturating_add(cache_read).saturating_add(cache_write));
                }
                None
            }
            "text" => {
                let text = part
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                if text.is_empty() {
                    return None;
                }
                let mut out = self.flush_buf();
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
                self.note_text(text);
                Some(out)
            }
            "tool" | "tool_use" => {
                self.tools += 1;
                let mut out = self.flush_buf();
                if let Some(name) = part.get("tool").and_then(|x| x.as_str()) {
                    out.push_str(&format!("→ {name}\n"));
                }
                if out.is_empty() {
                    None
                } else {
                    Some(out)
                }
            }
            _ => None,
        }
    }

    fn handle_codex_item(&mut self, v: &serde_json::Value) -> Option<String> {
        let item = v.get("item")?;
        let ity = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let mut out = self.flush_buf();
        match ity {
            "agent_message" => {
                if let Some(text) = item.get("text").and_then(|x| x.as_str()) {
                    if !text.is_empty() {
                        out.push_str(text);
                        if !text.ends_with('\n') {
                            out.push('\n');
                        }
                        self.note_text(text);
                    }
                }
            }
            "reasoning" => {
                // Fall through to the out.is_empty() check rather than returning
                // early, so any buffer flushed above is never silently dropped.
                if let Some(text) = item
                    .get("text")
                    .or_else(|| item.get("summary"))
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                {
                    out.push_str(&format!("… {}\n", first_line(text, 90)));
                }
            }
            "command_execution" | "file_change" | "mcp_tool_call" | "web_search" => {
                self.tools += 1;
                let failed = item
                    .get("status")
                    .and_then(|x| x.as_str())
                    .map(|s| s.eq_ignore_ascii_case("failed") || s.eq_ignore_ascii_case("error"))
                    .unwrap_or(false);
                if failed {
                    self.tool_errors += 1;
                }
                let detail = item
                    .get("command")
                    .or_else(|| item.get("query"))
                    .or_else(|| item.pointer("/changes/0/path"))
                    .and_then(|x| x.as_str())
                    .map(|s| first_line(s, 90))
                    .unwrap_or_default();
                if detail.is_empty() {
                    out.push_str(&format!("→ {ity}\n"));
                } else {
                    out.push_str(&format!("→ {ity}  {detail}\n"));
                }
            }
            "error" => {
                let msg = item
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("error");
                out.push_str(&format!("! {}\n", first_line(msg, 160)));
            }
            _ => {}
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn handle_claude_assistant(&mut self, v: &serde_json::Value) -> Option<String> {
        if let Some(m) = v.pointer("/message/model").and_then(|x| x.as_str()) {
            self.model = Some(m.to_string());
        }
        let mut out = self.flush_buf();
        let content = v.pointer("/message/content")?.as_array()?;
        for block in content {
            let bty = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
            match bty {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|x| x.as_str()) {
                        if !text.is_empty() {
                            out.push_str(text);
                            if !text.ends_with('\n') {
                                out.push('\n');
                            }
                            self.note_text(text);
                        }
                    }
                }
                "tool_use" => {
                    self.tools += 1;
                    let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                    let detail = block
                        .pointer("/input/description")
                        .and_then(|x| x.as_str())
                        .map(|s| first_line(s, 90))
                        .or_else(|| {
                            block
                                .pointer("/input/command")
                                .and_then(|x| x.as_str())
                                .map(|s| first_line(s, 90))
                        })
                        .or_else(|| {
                            block
                                .pointer("/input/file_path")
                                .or_else(|| block.pointer("/input/path"))
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_default();
                    if detail.is_empty() {
                        out.push_str(&format!("→ {name}\n"));
                    } else {
                        out.push_str(&format!("→ {name}  {detail}\n"));
                    }
                }
                _ => {}
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn handle_claude_user(&mut self, v: &serde_json::Value) -> Option<String> {
        let mut out = self.flush_buf();
        let content = v.pointer("/message/content")?.as_array()?;
        for block in content {
            if block.get("type").and_then(|x| x.as_str()) != Some("tool_result") {
                continue;
            }
            let id = block
                .get("tool_use_id")
                .and_then(|x| x.as_str())
                .unwrap_or("tool");
            let body = block
                .get("content")
                .map(|c| match c {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            let is_err = block
                .get("is_error")
                .and_then(|x| x.as_bool())
                .unwrap_or(false)
                || body.to_ascii_lowercase().contains("error");
            if is_err {
                self.tool_errors += 1;
            }
            let preview = first_line(&body, 100);
            let mark = if is_err { "✗" } else { "✓" };
            out.push_str(&format!("← {mark}  {id}  {preview}\n"));
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// **Known gap: `cli:grok`.** grok is spawned with `--output-format streaming-json`
    /// (`providers/grok.rs`), which its own help calls "NDJSON of the agent native ACP
    /// session updates"; the Anthropic-wire option is `streaming-messages-json` and spar
    /// does not ask for it. No grok slot log on this box contains a `· session`, `· done`
    /// or `· turn` marker, so grok reaches neither the claude `result` arm nor codex's
    /// `turn.completed` and **never gets a Terminal-scope record**, so its numbers come
    /// entirely from the Request arm below. Measured against grok's own session store for
    /// biddesk run 92ae513a (`~/.grok/sessions/<cwd>/<id>/updates.jsonl`, whose
    /// `turn_completed` update carries the truth): `cache_read` matched exactly
    /// (2,853,504) and `input_tokens` matched exactly as the uncached remainder (124,866
    /// = 2,978,370 - 2,853,504), because grok emits both cumulatively and `max` lands on
    /// the final value. `output_tokens` did **not**: 61,292 recorded against 30,646 real,
    /// i.e. a cumulative value summed more than once. Removing the duplicate
    /// `absorb_usage` calls (this change) is the likely fix but is **unverified**: grok
    /// is out of quota and was not probed. Two consequences to fix separately, not here:
    /// `context_tokens` for grok is a cumulative total wearing a peak's name (grok's
    /// `modelCalls: 31` says the real window is far smaller), and `model` / `tool_errors`
    /// / tool *names* are never recovered at all, though the tool *count* is exact.
    fn absorb_usage(&mut self, v: &serde_json::Value, scope: UsageScope) {
        let u = v.get("usage").or_else(|| v.pointer("/message/usage"));
        let Some(u) = u else { return };
        let input = u
            .get("input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or_default();
        let output = u
            .get("output_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or_default();
        // The two conventions are not interchangeable and adding them the same way is a
        // 2x error. Anthropic's `cache_read_input_tokens` is disjoint from `input_tokens`;
        // OpenAI/codex's `cached_input_tokens` is a *component of* `input_tokens`
        // (verified across 105 `token_count` records in `~/.codex/sessions`: every one has
        // `total_tokens == input_tokens + output_tokens` and none has `cached > input`).
        // Codex is therefore normalized to the Anthropic shape here, with `input_tokens`
        // reduced to the uncached remainder, so `input + cache_read + cache_write + output`
        // stays the honest billed total for both.
        let (input, cache_read) = match (
            u.get("cache_read_input_tokens").and_then(|x| x.as_u64()),
            u.get("cached_input_tokens").and_then(|x| x.as_u64()),
        ) {
            (Some(cr), _) => (input, cr),
            (None, Some(cached)) => (input.saturating_sub(cached), cached),
            (None, None) => (input, 0),
        };
        let cache_write = u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or_default()
            .max(
                u.pointer("/cache_creation/ephemeral_1h_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or_default(),
            );

        if scope == UsageScope::Terminal {
            // The invocation total supersedes whatever the per-call records added up
            // to, and re-absorbing the same record must be idempotent.
            self.input_tokens = input;
            self.output_tokens = output;
            self.cache_read = cache_read;
            self.cache_write = cache_write;
            self.saw_terminal_usage = true;
            return;
        }

        self.context_peak = self
            .context_peak
            .max(input.saturating_add(cache_read).saturating_add(cache_write));
        if self.saw_terminal_usage {
            return;
        }
        // Providers disagree on whether a per-call record is a delta or a running
        // total, so the billed components stay on the conservative max/sum shape until
        // a terminal record settles them.
        self.input_tokens = self.input_tokens.max(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.cache_read = self.cache_read.max(cache_read);
        self.cache_write = self.cache_write.max(cache_write);
    }

    /// Peak single-request prompt footprint. Adapters that report usage only once, at
    /// the end of the invocation (codex), have no per-request record to peak over, so
    /// their invocation total stands in.
    fn context_tokens(&self) -> u64 {
        if self.context_peak > 0 {
            return self.context_peak;
        }
        self.input_tokens
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// Cumulative billed tokens: exactly the sum of the four component counters this
    /// coalescer writes into `StreamStats`, so the sidecar's own numbers always add up.
    fn billed_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens.max(self.est_output_tokens))
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// Fold this coalescer's counters into the shared stats.
    ///
    /// A no-op on stderr. `run_captured` runs one coalescer per stream against one
    /// `StreamStats`, and the stderr coalescer never parses anything, so copying its
    /// counters over meant whichever stream wrote last decided the numbers: a slot
    /// whose final line was stderr reported zero tools and zero tokens.
    fn merge_counters_into(&self, s: &mut StreamStats) {
        if self.is_err {
            return;
        }
        s.tools = self.tools;
        s.tool_errors = self.tool_errors;
        s.input_tokens = self.input_tokens;
        s.output_tokens = self.output_tokens.max(self.est_output_tokens);
        s.cache_read_tokens = self.cache_read;
        s.cache_write_tokens = self.cache_write;
        s.context_tokens = self.context_tokens();
        s.billed_tokens = self.billed_tokens();
        if self.quota_rejected.is_some() {
            s.quota_rejected = self.quota_rejected.clone();
            s.quota_resets_at = self.quota_resets_at;
        }
        if self.model.is_some() {
            s.model = self.model.clone();
        }
        if self.session_id.is_some() {
            s.session_id = self.session_id.clone();
        }
    }

    fn note_text(&mut self, s: &str) {
        self.text_chars += s.len() as u64;
        // rough output estimate when provider doesn't report tokens
        self.est_output_tokens = (self.text_chars / 4).max(self.est_output_tokens);
    }

    fn push_token(&mut self, kind: CoalesceKind, data: &str) -> Option<String> {
        let mut out = String::new();
        if self.kind != kind && self.kind != CoalesceKind::None {
            out.push_str(&self.flush_buf());
        }
        if self.kind == CoalesceKind::None {
            self.kind = kind;
        }
        self.kind = kind;
        self.buf.push_str(data);
        if kind == CoalesceKind::Text {
            self.note_text(data);
        }

        if self.buf.contains('\n') {
            let parts: Vec<&str> = self.buf.split('\n').collect();
            let last = parts.last().copied().unwrap_or("");
            for p in &parts[..parts.len().saturating_sub(1)] {
                if kind == CoalesceKind::Thought {
                    // skip dumping thoughts line-by-line; keep collapsed
                } else {
                    out.push_str(p);
                    out.push('\n');
                }
            }
            self.buf = last.to_string();
        }

        if kind == CoalesceKind::Text && self.buf.len() > 160 && self.buf.contains(". ") {
            if let Some(i) = self.buf.rfind(". ") {
                let split = i + 2;
                let done = self.buf[..split].to_string();
                let rest = self.buf[split..].to_string();
                out.push_str(&done);
                out.push('\n');
                self.buf = rest;
            }
        }

        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    fn flush_buf(&mut self) -> String {
        if self.buf.is_empty() {
            self.kind = CoalesceKind::None;
            return String::new();
        }
        let mut s = std::mem::take(&mut self.buf);
        let kind = self.kind;
        self.kind = CoalesceKind::None;
        if kind == CoalesceKind::Thought {
            // one collapsed thinking line, not token soup
            let preview = first_line(&s, 80);
            return format!("… {preview}\n");
        }
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    fn flush_buf_opt(&mut self) -> Option<String> {
        let s = self.flush_buf();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn finish(&mut self) -> Option<String> {
        self.flush_buf_opt()
    }
}

fn format_tokens(input: u64, output: u64, cache: u64) -> String {
    let mut parts = Vec::new();
    if input > 0 {
        parts.push(format!("in {}", compact_num(input)));
    }
    if output > 0 {
        parts.push(format!("out {}", compact_num(output)));
    }
    if cache > 0 {
        parts.push(format!("cache {}", compact_num(cache)));
    }
    if parts.is_empty() {
        "tokens —".into()
    } else {
        parts.join(" · ")
    }
}

fn compact_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let t: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn truncate_json(v: &serde_json::Value, max: usize) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    first_line(&s, max)
}

fn append_log(path: &Path, text: &str) -> Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(text.as_bytes())?;
    f.flush()?;
    Ok(())
}

pub struct TailLog {
    pub text: String,
    pub truncated: bool,
    /// True when open/read failed (caller should not cache as a successful empty).
    pub io_error: bool,
}

pub fn tail_log(path: &Path, max_bytes: usize) -> String {
    tail_log_info(path, max_bytes).text
}

/// Read only the last `max_bytes` of a log (seek from end). Avoids loading multi-MB logs.
pub fn tail_log_info(path: &Path, max_bytes: usize) -> TailLog {
    let Ok(mut f) = File::open(path) else {
        return TailLog {
            text: String::new(),
            truncated: false,
            io_error: true,
        };
    };
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return TailLog {
            text: String::new(),
            truncated: false,
            io_error: true,
        };
    };
    let truncated = len > max_bytes as u64;
    if truncated {
        let back = max_bytes as u64;
        if f.seek(SeekFrom::End(-(back as i64))).is_err() {
            return TailLog {
                text: String::new(),
                truncated: false,
                io_error: true,
            };
        }
    } else if f.seek(SeekFrom::Start(0)).is_err() {
        return TailLog {
            text: String::new(),
            truncated: false,
            io_error: true,
        };
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return TailLog {
            text: String::new(),
            truncated: false,
            io_error: true,
        };
    }
    if truncated {
        let start = next_char_boundary(&buf, 0);
        if start > 0 {
            buf = buf[start..].to_vec();
        }
    }
    TailLog {
        text: String::from_utf8_lossy(&buf).into_owned(),
        truncated,
        io_error: false,
    }
}

fn next_char_boundary(buf: &[u8], mut i: usize) -> usize {
    while i < buf.len() && (buf[i] & 0b1100_0000) == 0b1000_0000 {
        i += 1;
    }
    i
}

pub fn run_mock(req: &SpawnRequest, mock_output: &str) -> Result<SpawnResult> {
    if let Some(parent) = req.log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = File::create(&req.log_path)?;
    writeln!(
        f,
        "# mock {} {}\n{}",
        req.program.display(),
        req.args.join(" "),
        mock_output
    )?;
    f.flush()?;
    let stats = StreamStats::default();
    let _ = stats.save(&req.log_path);
    Ok(SpawnResult {
        exit_code: Some(0),
        signal: None,
        timed_out: false,
        log_path: req.log_path.clone(),
        stdout_tail: mock_output.into(),
        stats,
    })
}

#[cfg(test)]
mod tests {

    /// The provider states its own verdict on the request in a typed field. Routing on
    /// that fact is what makes prose incapable of faking a quota stop: this whole change
    /// oscillated for seven rounds tuning substring rules against log text that spar
    /// itself had rendered from these very fields.
    #[test]
    fn a_rejected_rate_limit_event_is_captured_typed() {
        let mut c = StreamCoalescer::new(false);
        c.feed(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day"}}"#);
        assert_eq!(c.quota_rejected.as_deref(), Some("seven_day"));

        // An allowed event, and a warning short of rejection, are not rejections.
        for status in ["allowed", "allowed_warning"] {
            let mut c = StreamCoalescer::new(false);
            c.feed(&format!(
                r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"{status}","rateLimitType":"five_hour"}}}}"#
            ));
            assert_eq!(c.quota_rejected, None, "status {status} must not route");
        }
    }

    /// Prose cannot manufacture the typed verdict, however exactly it quotes a real
    /// rejection — including spar's own rendered form of one.
    ///
    /// The fixtures are deliberately NOT interpolated into the assertion messages. Cargo
    /// prints a failing assert's message verbatim into the slot log, and these strings are
    /// live rejection text: a slot that broke this very test would have its own cargo
    /// output match the prose fallback and route its genuine defect to `Phase::Quota`.
    /// That trap is what `dc22bf7` narrowed the phrase matcher to close, and it would have
    /// been reintroduced here from the other side, in the file anyone working on this
    /// feature is most likely to break. Print the index instead.
    #[test]
    fn prose_never_produces_a_typed_rate_limit_rejection() {
        const FIXTURES: [&str; 3] = [
            "! rate limit  seven_day  rejected",
            "You've hit your weekly limit \u{b7} resets 12am (America/New_York)",
            "implementer: editing src/quota.rs, the rate limit rejected path",
        ];
        for (i, line) in FIXTURES.iter().enumerate() {
            let mut c = StreamCoalescer::new(false);
            c.feed(line);
            assert_eq!(c.quota_rejected, None, "FIXTURES[{i}] must not route");
        }
    }

    /// The whole data path, end to end: a rejection event fed to the coalescer must reach
    /// `StreamStats`, carrying both the window and the stated reopening instant. Deleting
    /// either the capture in the event handler or the propagation in `merge_counters_into`
    /// must fail this.
    #[test]
    fn a_rejection_reaches_stream_stats_with_its_reset_instant() {
        let mut c = StreamCoalescer::new(false);
        c.feed(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","resetsAt":1788000000}}"#,
        );
        let mut stats = StreamStats::default();
        c.merge_counters_into(&mut stats);
        assert_eq!(stats.quota_rejected.as_deref(), Some("seven_day"));
        assert_eq!(
            stats.quota_resets_at,
            Some(1788000000),
            "the stated instant must survive to the stats the executor reads"
        );
    }

    /// A window-specific `unifiedWindows` entry outranks the top-level one.
    #[test]
    fn the_window_specific_reset_instant_wins() {
        let mut c = StreamCoalescer::new(false);
        c.feed(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","resetsAt":1,"unifiedWindows":{"seven_day":{"resetsAt":1788000000}}}}"#,
        );
        assert_eq!(c.quota_resets_at, Some(1788000000));
    }
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn mock_writes_log() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("t.log");
        let req = SpawnRequest {
            program: PathBuf::from("mock"),
            args: vec![],
            cwd: tmp.path().to_path_buf(),
            log_path: log.clone(),
            env: vec![],
            timeout: Duration::from_secs(1),
        };
        run_mock(&req, "hello").unwrap();
        assert!(std::fs::read_to_string(log).unwrap().contains("hello"));
    }

    #[test]
    fn timeout_sets_timed_out_and_kills_group() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("to.log");
        // Leader sleeps; grandchild would be in same process group.
        let req = SpawnRequest {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: tmp.path().to_path_buf(),
            log_path: log,
            env: vec![],
            timeout: Duration::from_millis(200),
        };
        let res = run_captured(&req, None, None).expect("timeout path must not error");
        assert!(res.timed_out, "expected timed_out");
    }

    fn sh_req(script: &str, dir: &Path, log: &str) -> SpawnRequest {
        SpawnRequest {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            log_path: dir.join(log),
            env: vec![],
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn pid_sink_fires_with_live_pid_before_exit() {
        let tmp = tempdir().unwrap();
        let req = sh_req("sleep 0.3", tmp.path(), "sink.log");
        let (tx, rx) = std::sync::mpsc::channel();
        let sink = move |pid: u32| {
            let _ = tx.send((pid, pid_alive(pid)));
        };
        let res = run_captured(&req, Some(&sink), None).expect("run");
        let (pid, alive) = rx.recv().expect("sink must fire");
        assert!(pid > 1, "real child pid, got {pid}");
        assert!(alive, "child must be alive at the moment the sink fires");
        assert_eq!(res.exit_code, Some(0));
    }

    #[test]
    fn on_tick_fires_while_child_is_alive() {
        let tmp = tempdir().unwrap();
        let req = sh_req("sleep 0.3", tmp.path(), "tick.log");
        let ticks = std::cell::Cell::new(0u32);
        let tick = || ticks.set(ticks.get() + 1);
        let res = run_captured(&req, None, Some(&tick)).expect("run");
        assert_eq!(res.exit_code, Some(0));
        assert!(
            ticks.get() > 0,
            "on_tick must fire at least once during a live child"
        );
    }

    #[test]
    fn signal_kill_reports_signal_not_exit_code() {
        let tmp = tempdir().unwrap();
        let req = sh_req("kill -9 $$", tmp.path(), "sig.log");
        let res = run_captured(&req, None, None).expect("run");
        assert_eq!(res.exit_code, None, "signal death has no exit code");
        assert_eq!(res.signal, Some(9));
    }

    #[test]
    fn nonzero_exit_code_captured() {
        let tmp = tempdir().unwrap();
        let req = sh_req("exit 137", tmp.path(), "oom.log");
        let res = run_captured(&req, None, None).expect("run");
        assert_eq!(res.exit_code, Some(137));
        assert_eq!(res.signal, None);
    }

    #[test]
    fn pid_alive_true_self_false_reaped() {
        assert!(pid_alive(std::process::id()));
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid), "reaped child pid must be dead");
    }

    /// Cleanup is routinely run from inside the very tree it reaps; scanning our own cwd
    /// must never hand back this process (or the shell that spawned it) as a kill target.
    #[cfg(target_os = "linux")]
    #[test]
    fn cwd_scan_never_returns_self_or_ancestors() {
        let here = std::env::current_dir().unwrap();
        let found = pids_with_cwd_under(&here);
        for pid in self_and_ancestors() {
            assert!(
                !found.contains(&pid),
                "pid {pid} (self or ancestor) must be excluded from reap targets"
            );
        }
        assert!(self_and_ancestors().contains(&std::process::id()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cwd_scan_of_empty_dir_finds_nothing() {
        let tmp = tempdir().unwrap();
        assert!(pids_with_cwd_under(tmp.path()).is_empty());
        assert!(pids_with_cwd_under(&tmp.path().join("gone")).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_starttime_token_is_not_alive_but_fresh_is() {
        let me = std::process::id();
        let fresh = PidToken::capture(me);
        assert_eq!(fresh.pid, me);
        assert!(fresh.starttime.is_some(), "linux must record a start-time");
        assert!(
            fresh.alive(),
            "our own live pid with matching start-time is alive"
        );

        // Same live pid, but a start-time that no longer matches models a recycled pid
        // now owned by an unrelated process: it must never be treated as ours.
        let recycled = PidToken {
            pid: me,
            starttime: Some(fresh.starttime.unwrap() + 1),
        };
        assert!(
            !recycled.alive(),
            "a live pid whose recorded start-time differs must be treated as dead"
        );
    }

    #[test]
    fn pid_token_roundtrips_through_encode_parse() {
        let with_st = PidToken {
            pid: 4242,
            starttime: Some(987654),
        };
        assert_eq!(with_st.encode(), "4242:987654");
        assert_eq!(PidToken::parse("4242:987654"), Some(with_st));
        // Legacy bare-pid record parses with no start-time and falls back to liveness.
        assert_eq!(PidToken::parse("4242"), Some(PidToken::from_pid(4242)));
        assert_eq!(PidToken::parse("  "), None);
    }

    #[test]
    fn captures_echo() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("e.log");
        let req = SpawnRequest {
            program: PathBuf::from("echo"),
            args: vec!["stream-me".into()],
            cwd: tmp.path().to_path_buf(),
            log_path: log.clone(),
            env: vec![],
            timeout: Duration::from_secs(5),
        };
        let res = run_captured(&req, None, None).unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert!(std::fs::read_to_string(log).unwrap().contains("stream-me"));
    }

    #[test]
    fn grok_tokens_coalesce() {
        let mut c = StreamCoalescer::new(false);
        let mut out = String::new();
        for tok in ["I'll", " pull", " PR", " 167", "."] {
            let line = format!(r#"{{"type":"text","data":"{tok}"}}"#);
            if let Some(chunk) = c.feed(&line) {
                out.push_str(&chunk);
            }
        }
        out.push_str(&c.finish().unwrap_or_default());
        assert!(out.contains("I'll pull PR 167."));
        assert!(out.lines().count() <= 2);
    }

    #[test]
    fn claude_tools_and_usage() {
        let mut c = StreamCoalescer::new(false);
        let line = r#"{"type":"assistant","message":{"model":"claude-opus","usage":{"input_tokens":100,"output_tokens":5,"cache_read_input_tokens":50},"content":[{"type":"text","text":"Checking scope."},{"type":"tool_use","name":"Bash","input":{"description":"Get PR diff","command":"gh pr diff 167"}}]}}"#;
        let chunk = c.feed(line).unwrap();
        assert!(chunk.contains("Checking scope."));
        assert!(chunk.contains("→ Bash"));
        assert_eq!(c.tools, 1);
        assert_eq!(c.input_tokens, 100);
        assert!(c.cache_read >= 50);
        assert_eq!(c.model.as_deref(), Some("claude-opus"));
    }

    #[test]
    fn codex_jsonl_text_and_usage() {
        // Real `codex exec --json` event sequence (captured from codex-cli 0.144.4).
        let mut c = StreamCoalescer::new(false);
        let mut out = String::new();
        for line in [
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"done reviewing"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":39189,"cached_input_tokens":39185,"output_tokens":117,"reasoning_output_tokens":106}}"#,
        ] {
            if let Some(chunk) = c.feed(line) {
                out.push_str(&chunk);
            }
        }
        assert!(out.contains("done reviewing"), "assistant text: {out:?}");
        assert!(out.contains("· turn"), "turn marker: {out:?}");
        // codex's `cached_input_tokens` sits *inside* `input_tokens`, so it is split out
        // rather than added: `input` becomes the uncached remainder. This exact record's
        // own `token_count` entry in `~/.codex/sessions` reports `total_tokens: 39306`.
        assert_eq!(c.input_tokens, 39189 - 39185);
        assert_eq!(c.output_tokens, 117);
        assert_eq!(c.cache_read, 39185);
        assert_eq!(
            c.billed_tokens(),
            39306,
            "codex's own total_tokens, not 2x it"
        );
        // codex reports usage only once, at the end, so there is no per-request record
        // to peak over and the turn total stands in for the gauge. The whole prompt is
        // `input_tokens`, cached part included.
        assert_eq!(c.context_tokens(), 39189);
    }

    #[test]
    fn claude_result_supersedes_per_message_usage() {
        // Real `claude -p --output-format stream-json` shape: one `assistant` event per
        // content block, all repeating that message's usage, then a `result` carrying
        // the invocation total (input 49 = 10 + 39, cache_read 54623 = 18052 + 36571).
        // Summing the assistant events would bill the first message three times.
        let msg = |input: u64, cw: u64, cr: u64, out: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"model":"claude-haiku","usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"output_tokens":{out}}},"content":[{{"type":"text","text":"hi"}}]}}}}"#
            )
        };
        let mut c = StreamCoalescer::new(false);
        for _ in 0..3 {
            c.feed(&msg(10, 18519, 18052, 4));
        }
        for _ in 0..2 {
            c.feed(&msg(39, 259, 36571, 1));
        }
        // Mid-run the gauge is already right, off per-request records alone.
        assert_eq!(c.context_tokens(), 39 + 259 + 36571);
        c.feed(
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":49,"cache_creation_input_tokens":18778,"cache_read_input_tokens":54623,"output_tokens":241}}"#,
        );
        assert_eq!(c.input_tokens, 49);
        assert_eq!(c.output_tokens, 241, "the total replaces the per-call sum");
        assert_eq!(c.cache_read, 54623);
        assert_eq!(c.cache_write, 18778);
        assert_eq!(c.billed_tokens(), 49 + 241 + 54623 + 18778);
        // The gauge does not inherit the terminal record's cumulative cache read,
        // which is what made every claude slot read as permanently over threshold.
        assert_eq!(c.context_tokens(), 39 + 259 + 36571);
    }

    #[test]
    fn muse_jsonl_renders_session_tools_and_text() {
        // Real `muse exec --json` events (captured from Muse Code 0.1.0-R708.1).
        let lines = [
            r#"{"schema_version":1,"stream":{"kind":"session","id":"11111111-2222-3333-4444-555555555555"},"record_type":"event","payload_type":"run.model.configured","payload":{"display_label":"muse-spark-1.2-contributor","kind":"run_model_configured","model_id":"muse-spark-1.2-contributor","provider_id":"meta","source":"startup"}}"#,
            r#"{"schema_version":1,"stream":{"kind":"session","id":"11111111-2222-3333-4444-555555555555"},"record_type":"event","payload_type":"task.lifecycle.started","payload":{"kind":"task_lifecycle"}}"#,
            r#"{"schema_version":1,"stream":{"kind":"session","id":"11111111-2222-3333-4444-555555555555"},"record_type":"event","payload_type":"tool.result","payload":{"call_id":"call_01","correlation_facts":{"outcome":"success","tool_name":"write_file"},"edit_facts":{"added":1,"path":"hello.txt","tool_name":"write_file"},"kind":"tool_result","text":"wrote 3 bytes"}}"#,
            r#"{"schema_version":1,"stream":{"kind":"session","id":"11111111-2222-3333-4444-555555555555"},"record_type":"status","payload_type":"run.output.delta","payload":{"kind":"run_output_delta","text":"D"}}"#,
            r#"{"schema_version":1,"stream":{"kind":"session","id":"11111111-2222-3333-4444-555555555555"},"record_type":"status","payload_type":"run.output.delta","payload":{"kind":"run_output_delta","text":"ONE"}}"#,
            r#"{"schema_version":1,"stream":{"kind":"session","id":"11111111-2222-3333-4444-555555555555"},"record_type":"event","payload_type":"run.terminal.completed","payload":{"kind":"run_terminal","reason":null,"terminal":"completed","text":"DONE"}}"#,
        ];
        let mut c = StreamCoalescer::new(false);
        let mut out = String::new();
        for l in lines {
            if let Some(chunk) = c.feed(l) {
                out.push_str(&chunk);
            }
        }
        assert_eq!(c.model.as_deref(), Some("muse-spark-1.2-contributor"));
        assert_eq!(
            c.session_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555"),
            "session id is how the usage log is found afterwards"
        );
        assert_eq!(c.tools, 1);
        assert_eq!(c.tool_errors, 0);
        assert!(out.contains("· session  muse-spark-1.2-contributor"));
        assert!(out.contains("→ write_file  hello.txt  success"));
        assert!(out.contains("DONE"));
        assert!(
            !out.contains("D\nONE"),
            "output deltas must coalesce, not break mid-word: {out:?}"
        );
        assert!(out.contains("· done  ·  1 tools"));
        // muse reports no usage on stdout; muse_telemetry fills these in post-exit.
        assert_eq!(c.input_tokens, 0);
        assert_eq!(c.output_tokens, 0);
    }

    #[test]
    fn muse_tool_failure_counts_as_error() {
        let mut c = StreamCoalescer::new(false);
        let out = c
            .feed(
                r#"{"stream":{"kind":"session","id":"s1"},"payload_type":"tool.result","payload":{"correlation_facts":{"outcome":"error","tool_name":"shell"},"kind":"tool_result"}}"#,
            )
            .unwrap_or_default();
        assert_eq!(c.tools, 1);
        assert_eq!(c.tool_errors, 1);
        assert!(out.contains("→ shell  error"));
    }

    #[test]
    fn opencode_jsonl_dedupes_a_republished_part() {
        // Real `opencode run --format json` events (captured from opencode 1.17.4). The
        // top-level `type` is always the underscore spelling the emitter hardcodes; the
        // dash spelling lives on `part.type`. What repeats is the *part*: opencode
        // republishes `PartUpdated` for an id it already sent, so the same `prt_f1`
        // arrives twice and must be billed once. Real single-step tokens: input 12738,
        // output 19, cache.read 1920.
        let mut c = StreamCoalescer::new(false);
        let mut out = String::new();
        let finish = r#"{"type":"step_finish","sessionID":"ses_1","part":{"id":"prt_f1","type":"step-finish","tokens":{"total":14677,"input":12738,"output":19,"reasoning":0,"cache":{"write":0,"read":1920}}}}"#;
        for line in [
            r#"{"type":"step_start","sessionID":"ses_1","part":{"id":"prt_s1","type":"step-start"}}"#,
            r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"write","callID":"call_1","state":{"status":"completed"}}}"#,
            finish,
            finish,
            r#"{"type":"text","sessionID":"ses_1","part":{"id":"prt_t1","type":"text","text":"DONE"}}"#,
        ] {
            if let Some(chunk) = c.feed(line) {
                out.push_str(&chunk);
            }
        }
        out.push_str(&c.finish().unwrap_or_default());
        assert!(out.contains("DONE"), "assistant text: {out:?}");
        assert!(out.contains("→ write"), "tool marker: {out:?}");
        assert_eq!(c.tools, 1);
        // Load-bearing now that every field sums: without it the republished step is
        // billed twice.
        assert_eq!(c.output_tokens, 19, "output must count once, not double");
        assert_eq!(c.input_tokens, 12738, "input must count once, not double");
        assert_eq!(c.cache_read, 1920, "cache.read must count once, not double");
        assert_eq!(
            c.session_id.as_deref(),
            Some("ses_1"),
            "session id makes the slot checkable against opencode's own ledger"
        );
    }

    #[test]
    fn opencode_dash_spelling_on_a_top_level_type_would_still_dedupe() {
        // No opencode build observed emits a dash-spelled top-level `type` -- the json
        // writer uses five hardcoded underscore literals -- so the `-` to `_` mapping is
        // defensive. Pinned rather than deleted: if a future build did emit both, the
        // dedupe key has to fold them together or every step bills twice.
        let mut c = StreamCoalescer::new(false);
        for line in [
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"id":"prt_f1","type":"step-finish","tokens":{"input":12738,"output":19,"cache":{"read":1920,"write":0}}}}"#,
            r#"{"type":"step-finish","sessionID":"ses_1","part":{"id":"prt_f1","type":"step-finish","tokens":{"input":12738,"output":19,"cache":{"read":1920,"write":0}}}}"#,
        ] {
            c.feed(line);
        }
        assert_eq!(c.input_tokens, 12738);
        assert_eq!(c.output_tokens, 19);
        assert_eq!(c.cache_read, 1920);
    }

    #[test]
    fn opencode_sums_step_deltas() {
        // Two distinct steps (different part.id). Every opencode step field is that
        // call's own delta: opencode.db's session totals equal the sum of its steps
        // (ses_fc0c768c2f: 20 steps, cache_read 2,550,766 = sum, max only 170,295), so
        // maxing under-reported the cache-heavy fields by an order of magnitude.
        let mut c = StreamCoalescer::new(false);
        for line in [
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"id":"prt_a","type":"step-finish","tokens":{"input":12738,"output":19,"cache":{"read":1920,"write":0}}}}"#,
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"id":"prt_b","type":"step-finish","tokens":{"input":97,"output":2,"cache":{"read":14592,"write":0}}}}"#,
        ] {
            c.feed(line);
        }
        assert_eq!(c.input_tokens, 12835);
        assert_eq!(c.output_tokens, 21);
        assert_eq!(c.cache_read, 16512);
        // The gauge is the biggest single call, not the running total.
        assert_eq!(c.context_tokens(), 14689, "97 + 14592 beats 12738 + 1920");
        assert_eq!(c.billed_tokens(), 12835 + 21 + 16512);
    }

    #[test]
    fn opencode_sums_reasoning_as_output_and_dedupes_each_step() {
        // Each step republished once, as opencode's `PartUpdated` really does.
        // `tokens.total` in opencode.db is input + output + reasoning + cache read +
        // write, so reasoning is billed and is not already inside `output`.
        let step = |id: &str, input: u64, out: u64, reason: u64, cr: u64| {
            format!(
                r#"{{"type":"step_finish","sessionID":"ses_1","part":{{"id":"{id}","type":"step-finish","tokens":{{"input":{input},"output":{out},"reasoning":{reason},"cache":{{"read":{cr},"write":0}}}}}}}}"#
            )
        };
        let mut c = StreamCoalescer::new(false);
        for line in [
            step("prt_a", 15193, 23, 363, 1920),
            step("prt_a", 15193, 23, 363, 1920),
            step("prt_b", 18932, 177, 502, 64),
            step("prt_b", 18932, 177, 502, 64),
        ] {
            c.feed(&line);
        }
        assert_eq!(c.input_tokens, 15193 + 18932);
        assert_eq!(
            c.output_tokens,
            23 + 363 + 177 + 502,
            "reasoning bills as output"
        );
        assert_eq!(c.cache_read, 1920 + 64);
        // opencode's own `tokens.total` per step, summed.
        assert_eq!(c.billed_tokens(), 17499 + 19675);
    }

    #[test]
    fn stderr_stream_never_zeroes_the_shared_counters() {
        // `run_captured` runs one coalescer per stream against one `StreamStats`, and
        // the stderr coalescer parses nothing, so its counters are permanently zero.
        // With stderr writing last, a real slot reported `"tools": 0` beside a log
        // holding 191 tool lines (biddesk run 2736a545, whose final line is
        // `! runtime command acknowledgement timed out after 30s`).
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("slot.log");
        let stats = std::sync::Arc::new(std::sync::Mutex::new(StreamStats::default()));

        let stdout = concat!(
            r#"{"type":"assistant","message":{"model":"claude-opus","usage":{"input_tokens":100,"output_tokens":5,"cache_read_input_tokens":50},"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            "
",
            r#"{"type":"result","subtype":"success","usage":{"input_tokens":100,"output_tokens":7,"cache_read_input_tokens":50}}"#,
            "
",
        );
        stream_to_log(
            std::io::Cursor::new(stdout.as_bytes()),
            &log,
            false,
            stats.clone(),
        );
        stream_to_log(
            std::io::Cursor::new(
                b"runtime command acknowledgement timed out after 30s
",
            ),
            &log,
            true,
            stats.clone(),
        );

        let s = stats.lock().unwrap();
        assert_eq!(s.tools, 1, "stderr must not wipe the tool count");
        assert_eq!(s.input_tokens, 100, "stderr must not wipe the tokens");
        assert_eq!(s.output_tokens, 7);
        assert_eq!(s.cache_read_tokens, 50);
        assert_eq!(s.billed_tokens, 157);
        assert_eq!(s.model.as_deref(), Some("claude-opus"));
        // stderr still counts as output and as liveness.
        assert_eq!(s.lines_in, 3);
        assert!(s.chars_out > 0);
        assert!(s.last_log_at.is_some());
        assert!(std::fs::read_to_string(&log)
            .unwrap()
            .contains("! runtime command acknowledgement timed out"));
    }

    #[test]
    fn last_event_without_log_output_still_lands_in_the_sidecar() {
        // Counters used to be written only alongside a log append. opencode's terminal
        // `step_finish` prints nothing, so the last call's tokens never reached
        // `stats.json`, and a stream of nothing but step_finish reported all zeros.
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("slot.log");
        let stats = std::sync::Arc::new(std::sync::Mutex::new(StreamStats::default()));
        let stream = concat!(
            r#"{"type":"text","sessionID":"ses_1","part":{"id":"prt_t","type":"text","text":"DONE"}}"#,
            "
",
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"id":"prt_f","type":"step-finish","tokens":{"input":12738,"output":19,"cache":{"read":1920,"write":0}}}}"#,
            "
",
        );
        stream_to_log(
            std::io::Cursor::new(stream.as_bytes()),
            &log,
            false,
            stats.clone(),
        );
        let s = stats.lock().unwrap();
        assert_eq!(s.input_tokens, 12738);
        assert_eq!(s.cache_read_tokens, 1920);
        assert_eq!(s.billed_tokens, 12738 + 1920 + s.output_tokens);
        assert_eq!(s.session_id.as_deref(), Some("ses_1"));
    }

    #[test]
    fn finish_without_a_model_keeps_the_one_already_detected() {
        // The finish branch used to assign `model` unconditionally, so a stream that
        // ended with nothing buffered wiped a model the feed branch had found.
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("slot.log");
        let stats = std::sync::Arc::new(std::sync::Mutex::new(StreamStats::default()));
        stats.lock().unwrap().model = Some("claude-opus".into());
        stream_to_log(
            std::io::Cursor::new(
                b"plain trailing line
",
            ),
            &log,
            false,
            stats.clone(),
        );
        assert_eq!(stats.lock().unwrap().model.as_deref(), Some("claude-opus"));
    }

    #[test]
    fn codex_top_level_error_renders_unquoted() {
        // Codex emits `{"type":"error","message":"…"}`; it must render without quotes.
        let mut c = StreamCoalescer::new(false);
        let chunk = c
            .feed(
                r#"{"type":"error","message":"Missing environment variable: OPENROUTER_API_KEY."}"#,
            )
            .unwrap();
        assert!(chunk.contains("! Missing environment variable: OPENROUTER_API_KEY."));
        assert!(
            !chunk.contains('"'),
            "message must not be quoted: {chunk:?}"
        );
    }

    #[test]
    fn codex_jsonl_command_counts_tool() {
        let mut c = StreamCoalescer::new(false);
        let line = r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"cargo test","status":"completed"}}"#;
        let chunk = c.feed(line).unwrap();
        assert!(chunk.contains("→ command_execution"));
        assert!(chunk.contains("cargo test"));
        assert_eq!(c.tools, 1);
        assert_eq!(c.tool_errors, 0);
    }

    #[test]
    fn tool_result_preview() {
        let mut c = StreamCoalescer::new(false);
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a.rs\nb.rs"}]}}"#;
        let chunk = c.feed(line).unwrap();
        assert!(chunk.contains("←"));
        assert!(chunk.contains("a.rs"));
    }

    #[test]
    fn tail_log_seeks_window() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("big.log");
        let mut body = String::new();
        body.push_str("PREFIX_SHOULD_DROP\n");
        body.push_str(&"x".repeat(200));
        body.push_str("\nTAIL_MARKER\n");
        std::fs::write(&log, &body).unwrap();
        let t = tail_log_info(&log, 50);
        assert!(t.truncated);
        assert!(!t.io_error);
        assert!(t.text.contains("TAIL_MARKER"));
        assert!(!t.text.contains("PREFIX_SHOULD_DROP"));
        assert!(t.text.len() <= 50 + 4); // boundary may drop a few lead bytes
    }

    #[test]
    fn tail_log_small_file_not_truncated() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("small.log");
        std::fs::write(&log, "hello\nworld\n").unwrap();
        let t = tail_log_info(&log, 10_000);
        assert!(!t.truncated);
        assert!(!t.io_error);
        assert_eq!(t.text, "hello\nworld\n");
    }

    #[test]
    fn tail_log_utf8_boundary() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("utf8.log");
        // 2-byte UTF-8 chars so a naive mid-window start can land on a continuation.
        let mut bytes = Vec::new();
        bytes.extend(std::iter::repeat_n(0xC3u8, 1)); // incomplete alone; we'll write full chars
                                                      // Write many "é" (C3 A9) then ASCII marker.
        for _ in 0..40 {
            bytes.extend_from_slice("é".as_bytes());
        }
        bytes.extend_from_slice(b"\nEND\n");
        std::fs::write(&log, &bytes).unwrap();
        let t = tail_log_info(&log, 25);
        assert!(t.truncated);
        assert!(!t.io_error);
        // Must be valid UTF-8 view (lossless for our content after boundary).
        assert!(t.text.contains("END"));
        assert!(!t.text.chars().any(|c| c == '\u{FFFD}'));
    }
}
