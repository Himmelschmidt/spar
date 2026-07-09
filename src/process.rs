use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
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

#[derive(Debug)]
pub struct SpawnResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    #[allow(dead_code)]
    pub log_path: PathBuf,
    #[allow(dead_code)]
    pub stdout_tail: String,
}

/// Spawn process, stream stdout/stderr into the log with **coalesced** human text
/// (not one JSON token per line).
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

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", req.program.display()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log_path = req.log_path.clone();
    let log_path_err = req.log_path.clone();

    let t_out = std::thread::spawn(move || {
        if let Some(out) = stdout {
            stream_to_log(out, &log_path, false);
        }
    });
    let t_err = std::thread::spawn(move || {
        if let Some(err) = stderr {
            stream_to_log(err, &log_path_err, true);
        }
    });

    let start = Instant::now();
    let poll = Duration::from_millis(50);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if start.elapsed() >= req.timeout {
                    let _ = child.kill();
                    let status = child.wait()?;
                    let _ = t_out.join();
                    let _ = t_err.join();
                    append_log(&req.log_path, "\n# timed out\n")?;
                    return Ok(SpawnResult {
                        exit_code: status.code(),
                        timed_out: true,
                        log_path: req.log_path.clone(),
                        stdout_tail: tail_log(&req.log_path, 4000),
                    });
                }
                std::thread::sleep(poll);
            }
        }
    };

    let _ = t_out.join();
    let _ = t_err.join();
    Ok(finish(status, false, &req.log_path))
}

fn stream_to_log(pipe: impl Read, log_path: &Path, is_err: bool) {
    let reader = BufReader::new(pipe);
    let mut coalescer = StreamCoalescer::new(is_err);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if let Some(chunk) = coalescer.feed(&line) {
            let _ = append_log(log_path, &chunk);
        }
    }
    if let Some(chunk) = coalescer.finish() {
        let _ = append_log(log_path, &chunk);
    }
}

/// Coalesce token/delta stream-json into readable prose.
struct StreamCoalescer {
    is_err: bool,
    /// Open text/thought run being built without newlines per token.
    buf: String,
    kind: CoalesceKind,
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
        }
    }

    fn feed(&mut self, line: &str) -> Option<String> {
        let t = line.trim();
        if t.is_empty() {
            return None;
        }

        // stderr: keep as tagged lines, no JSON parsing required
        if self.is_err {
            return Some(format!("[stderr] {line}\n"));
        }

        // Non-JSON: plain agent text (Claude sometimes prints a line before JSON)
        if !t.starts_with('{') {
            let mut out = self.flush_buf();
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
            return Some(out);
        }

        let Ok(v) = serde_json::from_str::<serde_json::Value>(t) else {
            let mut out = self.flush_buf();
            out.push_str(line);
            out.push('\n');
            return Some(out);
        };

        // ── Grok: {"type":"text"|"thought", "data":"token"} ──────────────
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            if matches!(ty, "text" | "thought" | "output" | "response") {
                if let Some(data) = v.get("data").and_then(|x| x.as_str()) {
                    return self.push_token(
                        if ty == "thought" {
                            CoalesceKind::Thought
                        } else {
                            CoalesceKind::Text
                        },
                        data,
                    );
                }
            }
            // Grok tool events
            if matches!(ty, "tool_call" | "tool_use" | "function_call") {
                let mut out = self.flush_buf();
                let name = v
                    .get("name")
                    .or_else(|| v.pointer("/tool/name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool");
                out.push_str(&format!("→ {name}\n"));
                return Some(out);
            }
        }

        // ── Claude stream-json ────────────────────────────────────────────
        if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
            match ty {
                "system" => {
                    // init / thinking_tokens / etc — one short line, skip noise
                    let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("");
                    if sub == "init" {
                        let mut out = self.flush_buf();
                        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("claude");
                        out.push_str(&format!("· session start ({model})\n"));
                        return Some(out);
                    }
                    return None; // drop thinking_tokens spam
                }
                "rate_limit_event" => return None,
                "stream_event" => {
                    // Nested: event.delta.text_delta
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
                    let ev_ty = v.pointer("/event/type").and_then(|x| x.as_str());
                    if matches!(ev_ty, Some("content_block_stop") | Some("message_stop")) {
                        return self.flush_buf_opt();
                    }
                    return None;
                }
                "assistant" => {
                    return self.handle_claude_assistant(&v);
                }
                "user" => {
                    // tool results — summarize, never dump multi-KB payloads
                    let mut out = self.flush_buf();
                    if let Some(content) = v.pointer("/message/content") {
                        if let Some(arr) = content.as_array() {
                            for block in arr {
                                let bty = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
                                if bty == "tool_result" {
                                    let id = block
                                        .get("tool_use_id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("tool");
                                    let body = block
                                        .get("content")
                                        .and_then(|c| c.as_str())
                                        .unwrap_or("");
                                    let preview = first_line_preview(body, 120);
                                    out.push_str(&format!("← result {id}: {preview}\n"));
                                }
                            }
                        }
                    }
                    return if out.is_empty() { None } else { Some(out) };
                }
                "result" => {
                    let mut out = self.flush_buf();
                    let subtype = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("result");
                    let cost = v
                        .get("total_cost_usd")
                        .or_else(|| v.get("cost_usd"))
                        .map(|x| x.to_string())
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "· done ({subtype}{})\n",
                        if cost.is_empty() {
                            String::new()
                        } else {
                            format!(", ${cost}")
                        }
                    ));
                    return Some(out);
                }
                "error" => {
                    let mut out = self.flush_buf();
                    out.push_str(&format!(
                        "! error: {}\n",
                        v.get("error")
                            .or_else(|| v.get("message"))
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| t.to_string())
                    ));
                    return Some(out);
                }
                _ => {}
            }
        }

        // Unknown JSON: skip bulky blobs, keep short ones
        if t.len() > 200 {
            return None;
        }
        let mut out = self.flush_buf();
        out.push_str(t);
        out.push('\n');
        Some(out)
    }

    fn handle_claude_assistant(&mut self, v: &serde_json::Value) -> Option<String> {
        let mut out = self.flush_buf();
        let content = v.pointer("/message/content")?.as_array()?;
        for block in content {
            let bty = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
            match bty {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|x| x.as_str()) {
                        // Full text block (not token delta) — write as paragraph
                        if !text.is_empty() {
                            out.push_str(text);
                            if !text.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                    }
                }
                "tool_use" => {
                    let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                    let desc = block
                        .pointer("/input/description")
                        .and_then(|x| x.as_str())
                        .or_else(|| block.pointer("/input/command").and_then(|x| x.as_str()))
                        .map(|s| first_line_preview(s, 100))
                        .unwrap_or_default();
                    if desc.is_empty() {
                        out.push_str(&format!("→ {name}\n"));
                    } else {
                        out.push_str(&format!("→ {name}: {desc}\n"));
                    }
                }
                "thinking" => {
                    // skip or one-liner
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

    fn push_token(&mut self, kind: CoalesceKind, data: &str) -> Option<String> {
        let mut out = String::new();
        if self.kind != kind && self.kind != CoalesceKind::None {
            out.push_str(&self.flush_buf());
        }
        if self.kind == CoalesceKind::None {
            self.kind = kind;
            if kind == CoalesceKind::Thought {
                self.buf.push_str("… ");
            }
        }
        self.kind = kind;
        self.buf.push_str(data);

        // If token contains newlines, flush completed lines keep rest
        if self.buf.contains('\n') {
            let parts: Vec<&str> = self.buf.split('\n').collect();
            let last = parts.last().copied().unwrap_or("");
            let complete = &parts[..parts.len().saturating_sub(1)];
            for p in complete {
                if kind == CoalesceKind::Thought {
                    out.push_str(&format!("… {p}\n"));
                } else {
                    out.push_str(p);
                    out.push('\n');
                }
            }
            self.buf = last.to_string();
        }

        // Flush if buffer gets long without newline (paragraph break for UX)
        if self.buf.len() > 200 && self.buf.contains(". ") {
            if let Some(i) = self.buf.rfind(". ") {
                let split = i + 2;
                let done = self.buf[..split].to_string();
                let rest = self.buf[split..].to_string();
                if kind == CoalesceKind::Thought {
                    out.push_str(&format!("… {done}\n"));
                } else {
                    out.push_str(&done);
                    out.push('\n');
                }
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
            s = format!("… {s}");
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

fn first_line_preview(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let t: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn finish(status: ExitStatus, timed_out: bool, log_path: &Path) -> SpawnResult {
    SpawnResult {
        exit_code: status.code(),
        timed_out,
        log_path: log_path.to_path_buf(),
        stdout_tail: tail_log(log_path, 4000),
    }
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
    Ok(SpawnResult {
        exit_code: Some(0),
        timed_out: false,
        log_path: req.log_path.clone(),
        stdout_tail: mock_output.into(),
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
        let body = std::fs::read_to_string(log).unwrap();
        assert!(body.contains("stream-me"), "{body}");
    }

    #[test]
    fn grok_tokens_coalesce_into_prose() {
        let mut c = StreamCoalescer::new(false);
        let mut out = String::new();
        for tok in ["I'll", " pull", " PR", " 167", "."] {
            let line = format!(r#"{{"type":"text","data":"{tok}"}}"#);
            if let Some(chunk) = c.feed(&line) {
                out.push_str(&chunk);
            }
        }
        if let Some(chunk) = c.finish() {
            out.push_str(&chunk);
        }
        assert!(
            out.contains("I'll pull PR 167."),
            "got {out:?} — should reassemble tokens without per-word newlines"
        );
        // Should not be one token per line
        assert!(
            out.lines().count() <= 2,
            "too many lines (word-by-word?): {out:?}"
        );
    }

    #[test]
    fn claude_assistant_text_and_tool() {
        let mut c = StreamCoalescer::new(false);
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I'll start by pulling the PR diff."},{"type":"tool_use","name":"Bash","input":{"command":"gh pr diff 167","description":"Get PR diff"}}]}}"#;
        let chunk = c.feed(line).unwrap_or_default();
        assert!(chunk.contains("I'll start by pulling the PR diff."));
        assert!(chunk.contains("→ Bash"));
        assert!(!chunk.contains("toolu_"));
    }

    #[test]
    fn claude_skips_init_noise_and_rate_limit() {
        let mut c = StreamCoalescer::new(false);
        let init = r#"{"type":"system","subtype":"init","model":"claude-opus-4"}"#;
        let rl = r#"{"type":"rate_limit_event","rate_limit_info":{}}"#;
        let a = c.feed(init).unwrap_or_default();
        assert!(a.contains("session start"));
        assert!(c.feed(rl).is_none());
    }

    #[test]
    fn claude_tool_result_summarized() {
        let mut c = StreamCoalescer::new(false);
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_abc","content":"file1.rs\nfile2.rs\nfile3.rs"}]}}"#;
        let chunk = c.feed(line).unwrap_or_default();
        assert!(chunk.contains("← result"));
        assert!(chunk.contains("file1.rs"));
        assert!(!chunk.contains("file3.rs")); // only first line preview
    }
}
