use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
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
    }

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&req.log_path)
        .with_context(|| format!("open log {}", req.log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .with_context(|| format!("clone log {}", req.log_path.display()))?;

    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args)
        .current_dir(&req.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));
    for (k, v) in &req.env {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", req.program.display()))?;

    let start = Instant::now();
    let poll = Duration::from_millis(100);
    loop {
        match child.try_wait()? {
            Some(status) => {
                return Ok(finish(status, false, &req.log_path));
            }
            None => {
                if start.elapsed() >= req.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    append_log(&req.log_path, "\n# timed out\n")?;
                    return Ok(SpawnResult {
                        exit_code: None,
                        timed_out: true,
                        log_path: req.log_path.clone(),
                        stdout_tail: tail_log(&req.log_path, 4000),
                    });
                }
                std::thread::sleep(poll);
            }
        }
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

/// Dry-run mock: write a log and succeed without spawning.
pub fn run_mock(req: &SpawnRequest, mock_output: &str) -> Result<SpawnResult> {
    if let Some(parent) = req.log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        "# dry-run mock\nprogram={}\nargs={}\ncwd={}\n---\n{mock_output}\n",
        req.program.display(),
        req.args.join(" "),
        req.cwd.display()
    );
    std::fs::write(&req.log_path, body)?;
    Ok(SpawnResult {
        exit_code: Some(0),
        timed_out: false,
        log_path: req.log_path.clone(),
        stdout_tail: mock_output.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn captures_echo() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("out.log");
        let req = SpawnRequest {
            program: PathBuf::from("echo"),
            args: vec!["hello-swarm".into()],
            cwd: tmp.path().to_path_buf(),
            log_path: log.clone(),
            env: vec![],
            timeout: Duration::from_secs(5),
        };
        let res = run_captured(&req).unwrap();
        assert_eq!(res.exit_code, Some(0));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("hello-swarm"));
    }

    #[test]
    fn mock_writes_log() {
        let tmp = tempdir().unwrap();
        let log = tmp.path().join("m.log");
        let req = SpawnRequest {
            program: PathBuf::from("mock"),
            args: vec![],
            cwd: tmp.path().to_path_buf(),
            log_path: log.clone(),
            env: vec![],
            timeout: Duration::from_secs(1),
        };
        let res = run_mock(&req, "ok").unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert!(std::fs::read_to_string(&log).unwrap().contains("dry-run"));
    }
}
