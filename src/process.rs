use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
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
}

impl StreamStats {
    pub fn touch_context(&mut self) {
        self.context_tokens = self
            .input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.output_tokens);
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
    pub timed_out: bool,
    #[allow(dead_code)]
    pub log_path: PathBuf,
    #[allow(dead_code)]
    pub stdout_tail: String,
    pub stats: StreamStats,
}

/// Spawn process; stream structured events into a human log + live stats file.
pub fn run_captured(req: &SpawnRequest) -> Result<SpawnResult> {
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
    let _ = StreamStats::default().save(&req.log_path);

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
                    kill_process_group(&mut child);
                    let status = child.wait()?;
                    let _ = t_out.join();
                    let _ = t_err.join();
                    append_log(&req.log_path, "\n! timed out\n")?;
                    let stats = stats_holder.lock().map(|s| s.clone()).unwrap_or_default();
                    let _ = stats.save(&req.log_path);
                    return Ok(SpawnResult {
                        exit_code: status.code(),
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
        timed_out: false,
        log_path: req.log_path.clone(),
        stdout_tail: tail_log(&req.log_path, 4000),
        stats,
    })
}

/// SIGTERM the process group, brief grace, then SIGKILL group + child.
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Negative pid = process group (child is group leader via process_group(0)).
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
        let grace = Instant::now();
        while grace.elapsed() < Duration::from_secs(2) {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
        let _ = child.kill();
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
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
    let mut n = 0u32;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Ok(mut s) = stats.lock() {
            s.lines_in += 1;
        }
        if let Some(chunk) = c.feed(&line) {
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
                n += 1;
                if n.is_multiple_of(8) {
                    let _ = s.save(log_path);
                }
            }
            let _ = append_log(log_path, &chunk);
        }
    }
    if let Some(chunk) = c.finish() {
        if let Ok(mut s) = stats.lock() {
            s.chars_out += chunk.len() as u64;
            s.tools = c.tools;
            s.input_tokens = c.input_tokens;
            s.output_tokens = c.output_tokens.max(c.est_output_tokens);
            s.cache_read_tokens = c.cache_read;
            s.cache_write_tokens = c.cache_write;
            s.model = c.model.clone();
            s.touch_context();
            let _ = s.save(log_path);
        }
        let _ = append_log(log_path, &chunk);
    } else if let Ok(s) = stats.lock() {
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

pub fn tail_log(path: &Path, max_bytes: usize) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    if buf.len() > max_bytes {
        let start = buf.len() - max_bytes;
        String::from_utf8_lossy(&buf[start..]).into_owned()
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    }
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
        let res = run_captured(&req).unwrap();
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
}
