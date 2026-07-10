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
    /// Best-effort context footprint (input + cache read + output seen so far)
    pub context_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub lines_in: u64,
    pub chars_out: u64,
    /// RFC3339 of last successful log append (for stall detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_log_at: Option<String>,
}

impl StreamStats {
    pub fn touch_context(&mut self) {
        self.context_tokens = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.output_tokens);
    }

    pub fn touch_log(&mut self) {
        self.last_log_at = Some(
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
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
/// loop, so callers can record a live pid.
pub fn run_captured(req: &SpawnRequest, on_spawn: Option<&dyn Fn(u32)>) -> Result<SpawnResult> {
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
                std::thread::sleep(poll);
            }
        }
    };

    let _ = t_out.join();
    let _ = t_err.join();
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
fn kill_process_group(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus> {
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

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
fn signal_process_group(pid: i32, sig: i32) {
    // libc kill(-pgid) — no dependency on `kill` binary / PATH.
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let _ = kill(-pid, sig);
    }
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
        if let Some(chunk) = c.feed(&line) {
            if append_log(log_path, &chunk).is_ok() {
                if let Ok(mut s) = stats.lock() {
                    s.chars_out += chunk.len() as u64;
                    s.tools = c.tools;
                    s.tool_errors = c.tool_errors;
                    s.input_tokens = c.input_tokens;
                    s.output_tokens = c.output_tokens.max(c.est_output_tokens);
                    s.cache_read_tokens = c.cache_read;
                    s.cache_write_tokens = c.cache_write;
                    if c.model.is_some() {
                        s.model = c.model.clone();
                    }
                    s.touch_context();
                    s.touch_log();
                    // Persist last_log_at every append so status/TUI never read a stale stamp.
                    let _ = s.save(log_path);
                }
            }
        }
    }
    if let Some(chunk) = c.finish() {
        if append_log(log_path, &chunk).is_ok() {
            if let Ok(mut s) = stats.lock() {
                s.chars_out += chunk.len() as u64;
                s.tools = c.tools;
                s.input_tokens = c.input_tokens;
                s.output_tokens = c.output_tokens.max(c.est_output_tokens);
                s.cache_read_tokens = c.cache_read;
                s.cache_write_tokens = c.cache_write;
                s.model = c.model.clone();
                s.touch_context();
                s.touch_log();
                let _ = s.save(log_path);
            }
        }
    } else if let Ok(mut s) = stats.lock() {
        // Keep any prior last_log_at (e.g. spawn header); do not wipe with defaults.
        if s.last_log_at.is_none() {
            s.touch_log();
        }
        let _ = s.save(log_path);
    }
}

struct StreamCoalescer {
    is_err: bool,
    buf: String,
    kind: CoalesceKind,
    tools: u32,
    tool_errors: u32,
    input_tokens: u64,
    output_tokens: u64,
    est_output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
    model: Option<String>,
    text_chars: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CoalesceKind {
    None,
    Text,
    Thought,
}

impl StreamCoalescer {
    fn new(is_err: bool) -> Self {
        Self {
            is_err,
            buf: String::new(),
            kind: CoalesceKind::None,
            tools: 0,
            tool_errors: 0,
            input_tokens: 0,
            output_tokens: 0,
            est_output_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            model: None,
            text_chars: 0,
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

        self.absorb_usage(&v);

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
                        let model = v
                            .get("model")
                            .and_then(|x| x.as_str())
                            .unwrap_or("claude");
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
                    self.absorb_usage(&v);
                    let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("ok");
                    out.push_str(&format!(
                        "· done  {sub}  ·  {} tools  ·  {}\n",
                        self.tools,
                        format_tokens(self.input_tokens, self.output_tokens.max(self.est_output_tokens), self.cache_read)
                    ));
                    return Some(out);
                }
                "error" => {
                    let mut out = self.flush_buf();
                    out.push_str(&format!(
                        "! {}\n",
                        v.get("error")
                            .or_else(|| v.get("message"))
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| t.into())
                    ));
                    return Some(out);
                }
                _ => {}
            }
        }

        if t.len() > 240 {
            return None;
        }
        None
    }

    fn handle_claude_assistant(&mut self, v: &serde_json::Value) -> Option<String> {
        if let Some(m) = v.pointer("/message/model").and_then(|x| x.as_str()) {
            self.model = Some(m.to_string());
        }
        self.absorb_usage(v.pointer("/message").unwrap_or(v));
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

    fn absorb_usage(&mut self, v: &serde_json::Value) {
        let u = v.get("usage").or_else(|| v.pointer("/message/usage"));
        let Some(u) = u else { return };
        if let Some(n) = u.get("input_tokens").and_then(|x| x.as_u64()) {
            self.input_tokens = self.input_tokens.max(n);
        }
        if let Some(n) = u.get("output_tokens").and_then(|x| x.as_u64()) {
            self.output_tokens = self.output_tokens.saturating_add(n);
        }
        if let Some(n) = u
            .get("cache_read_input_tokens")
            .or_else(|| u.pointer("/cache_read_input_tokens"))
            .and_then(|x| x.as_u64())
        {
            self.cache_read = self.cache_read.max(n);
        }
        if let Some(n) = u
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
        {
            self.cache_write = self.cache_write.max(n);
        }
        // nested cache_creation
        if let Some(n) = u
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(|x| x.as_u64())
        {
            self.cache_write = self.cache_write.max(n);
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
        let res = run_captured(&req, None).expect("timeout path must not error");
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
        let res = run_captured(&req, Some(&sink)).expect("run");
        let (pid, alive) = rx.recv().expect("sink must fire");
        assert!(pid > 1, "real child pid, got {pid}");
        assert!(alive, "child must be alive at the moment the sink fires");
        assert_eq!(res.exit_code, Some(0));
    }

    #[test]
    fn signal_kill_reports_signal_not_exit_code() {
        let tmp = tempdir().unwrap();
        let req = sh_req("kill -9 $$", tmp.path(), "sig.log");
        let res = run_captured(&req, None).expect("run");
        assert_eq!(res.exit_code, None, "signal death has no exit code");
        assert_eq!(res.signal, Some(9));
    }

    #[test]
    fn nonzero_exit_code_captured() {
        let tmp = tempdir().unwrap();
        let req = sh_req("exit 137", tmp.path(), "oom.log");
        let res = run_captured(&req, None).expect("run");
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
        let res = run_captured(&req, None).unwrap();
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
