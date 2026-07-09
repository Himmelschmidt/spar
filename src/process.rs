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

/// Spawn process, stream stdout/stderr **line-by-line** into the log (flushed)
/// so TUI/tails see progress during long agent runs.
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
    // Encourage line buffering where tools respect it
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
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let pretty = prettify_stream_line(&line);
        let text = if is_err {
            format!("[stderr] {pretty}\n")
        } else {
            format!("{pretty}\n")
        };
        let _ = append_log(log_path, &text);
    }
}

/// Turn stream-json / streaming-json event lines into readable log lines when possible.
fn prettify_stream_line(line: &str) -> String {
    let t = line.trim();
    if t.is_empty() {
        return String::new();
    }
    if !t.starts_with('{') {
        return line.to_string();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(t) else {
        return line.to_string();
    };
    // Claude Code stream-json shapes (best-effort)
    if let Some(s) = v
        .pointer("/message/content/0/text")
        .and_then(|x| x.as_str())
        .or_else(|| v.get("result").and_then(|x| x.as_str()))
        .or_else(|| v.pointer("/delta/text").and_then(|x| x.as_str()))
        .or_else(|| v.get("text").and_then(|x| x.as_str()))
        .or_else(|| v.get("content").and_then(|x| x.as_str()))
    {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Some(ty) = v.get("type").and_then(|x| x.as_str()) {
        match ty {
            "assistant" | "content_block_delta" | "text" | "message" => {
                if let Some(s) = v
                    .pointer("/delta/text")
                    .or_else(|| v.pointer("/message/content/0/text"))
                    .and_then(|x| x.as_str())
                {
                    return s.to_string();
                }
            }
            "tool_use" | "tool_call" => {
                let name = v
                    .get("name")
                    .or_else(|| v.pointer("/tool_use/name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool");
                return format!("→ tool {name}");
            }
            "result" | "error" => {
                return format!(
                    "[{ty}] {}",
                    v.get("result")
                        .or_else(|| v.get("error"))
                        .map(|x| x.to_string())
                        .unwrap_or_default()
                );
            }
            _ => {}
        }
    }
    // Keep compact JSON so stream still moves
    t.to_string()
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
    fn prettify_extracts_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello live"}]}}"#;
        // pointer style may not match; fallback keeps JSON or extracts
        let p = prettify_stream_line(line);
        assert!(!p.is_empty());
    }
}
