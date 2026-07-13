use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

pub fn available() -> bool {
    which::which("tmux").is_ok()
}

pub fn session_name(run_id: &str) -> String {
    format!("spar-{run_id}")
}

pub fn has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a detached session with a shell in `cwd`.
pub fn new_session(name: &str, cwd: &Path) -> Result<()> {
    if has_session(name) {
        return Ok(());
    }
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            name,
            "-c",
            cwd.to_str().unwrap_or("."),
        ])
        .status()
        .context("tmux new-session")?;
    if !status.success() {
        bail!("tmux new-session failed for {name}");
    }
    Ok(())
}

/// Create a window for a slot and run `command` inside it. `env` entries are set on
/// the new pane (`-e KEY=VAL`), so a spawned agent and any hooks it runs inherit
/// its `SPAR_AGENT_ID` / `SPAR_RUN_ID` / `SPAR_PROJECT_ROOT` identity.
pub fn spawn_window(
    session: &str,
    window: &str,
    cwd: &Path,
    shell_cmd: &str,
    env: &[(String, String)],
) -> Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args([
        "new-window",
        "-t",
        session,
        "-n",
        window,
        "-c",
        cwd.to_str().unwrap_or("."),
    ]);
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(shell_cmd);
    let status = cmd.status().context("tmux new-window")?;
    if !status.success() {
        bail!("tmux new-window failed: {window}");
    }
    Ok(())
}

#[allow(dead_code)]
pub fn send_keys(session: &str, window: &str, keys: &str) -> Result<()> {
    let target = format!("{session}:{window}");
    let status = Command::new("tmux")
        .args(["send-keys", "-t", &target, keys, "Enter"])
        .status()
        .context("tmux send-keys")?;
    if !status.success() {
        bail!("tmux send-keys failed for {target}");
    }
    Ok(())
}

#[allow(dead_code)]
pub fn capture_pane(session: &str, window: &str) -> Result<String> {
    let target = format!("{session}:{window}");
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &target, "-S", "-200"])
        .output()
        .context("tmux capture-pane")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pid of the process running in a slot's window (the pane's shell), if the pane exists.
pub fn pane_pid(session: &str, window: &str) -> Option<u32> {
    let target = format!("{session}:{window}");
    let out = Command::new("tmux")
        .args(["list-panes", "-t", &target, "-F", "#{pane_pid}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

pub fn kill_session(name: &str) -> Result<()> {
    if !has_session(name) {
        return Ok(());
    }
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .status();
    Ok(())
}

pub fn attach_command(session: &str) -> Result<()> {
    if !has_session(session) {
        bail!("no tmux session {session}");
    }
    let status = Command::new("tmux")
        .args(["attach-session", "-t", session])
        .status()
        .context("tmux attach")?;
    if !status.success() {
        bail!("tmux attach failed");
    }
    Ok(())
}

/// Build a shell command string that runs program with args and logs.
pub fn shell_wrap(program: &Path, args: &[String], log_path: &Path) -> String {
    let prog = shell_escape(&program.display().to_string());
    let args_s: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    let log = shell_escape(&log_path.display().to_string());
    format!(
        "{prog} {} 2>&1 | tee {log}; echo EXIT:$? >> {log}",
        args_s.join(" ")
    )
}

fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}
