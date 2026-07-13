//! External `@human` notifier — the opt-in sink beside the always-on TUI panel.
//!
//! spar ships no notifier of its own: the operator wires their own sink in the
//! `[notify]` config section, either a `command` spar shells out to or a `webhook`
//! URL it POSTs the message JSON to. With neither configured this is a no-op and the
//! TUI panel remains the only sink. Routing is fire-and-forget: a broken notifier
//! must never fail the `send` that triggered it.

use crate::bus::BusMessage;
use crate::config::{Config, NotifyConfig};
use crate::paths::SparPaths;
use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cap on how long a notify command may run before it's killed. It runs on a
/// detached thread, but an unbounded wait would leak a thread and child process
/// per alert, so bound it defensively.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Route a human alert to the configured external notifier, if any. Errors are
/// logged to stderr and swallowed so bus delivery is never blocked by a bad sink.
pub fn route_human_alert(paths: &SparPaths, msg: &BusMessage) {
    // Detached so neither the command wait nor the webhook can stall the caller.
    // send() runs on the bus hot path (tick_acks -> bus deliver -> a Claude Stop
    // hook), so a hung command or black-holed webhook must not block the turn boundary.
    let root = paths.project_root.clone();
    let msg = msg.clone();
    std::thread::spawn(move || {
        let cfg = match Config::load(&root) {
            Ok(c) => c.notify,
            Err(e) => {
                eprintln!("notify: config load failed: {e:#}");
                return;
            }
        };
        if let Err(e) = dispatch(&cfg, &msg) {
            eprintln!("notify: {e:#}");
        }
    });
}

/// Fire whichever sinks the operator configured. Public for a direct notifier check.
pub fn dispatch(cfg: &NotifyConfig, msg: &BusMessage) -> Result<()> {
    if let Some(cmd) = cfg.command.as_deref().filter(|s| !s.is_empty()) {
        fire_command(cmd, msg)?;
    }
    if let Some(url) = cfg.webhook.as_deref().filter(|s| !s.is_empty()) {
        fire_webhook(url, msg)?;
    }
    Ok(())
}

/// One-line human summary passed to the command on argv (`$1`).
fn summary(msg: &BusMessage) -> String {
    let body: String = msg.body.chars().take(200).collect();
    format!("[{:?}] {} → {}: {}", msg.kind, msg.from, msg.to, body)
}

/// Run the operator command via `sh -c`, with the summary as `$1` and the full
/// message JSON on stdin. Waits (bounded by `COMMAND_TIMEOUT`) so a manual
/// echo-config check is observable without a hung notifier leaking forever.
fn fire_command(command: &str, msg: &BusMessage) -> Result<()> {
    let json = serde_json::to_string(msg)?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .arg("spar-notify")
        .arg(summary(msg))
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn notify command: {command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json.as_bytes()).ok();
    }
    let start = Instant::now();
    loop {
        match child.try_wait().context("wait on notify command")? {
            Some(status) => {
                if !status.success() {
                    anyhow::bail!("notify command exited with {status}");
                }
                return Ok(());
            }
            None => {
                if start.elapsed() >= COMMAND_TIMEOUT {
                    child.kill().ok();
                    child.wait().ok();
                    anyhow::bail!("notify command timed out after {COMMAND_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn fire_webhook(url: &str, msg: &BusMessage) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let resp = agent
        .post(url)
        .header("Content-Type", "application/json")
        .send_json(msg)
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("notify webhook {url} status {status}");
    }
    Ok(())
}
