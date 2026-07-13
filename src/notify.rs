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

/// Route a human alert to the configured external notifier, if any. Errors are
/// logged to stderr and swallowed so bus delivery is never blocked by a bad sink.
pub fn route_human_alert(paths: &SparPaths, msg: &BusMessage) {
    let cfg = match Config::load(&paths.project_root) {
        Ok(c) => c.notify,
        Err(e) => {
            eprintln!("notify: config load failed: {e:#}");
            return;
        }
    };
    if let Err(e) = dispatch(&cfg, msg) {
        eprintln!("notify: {e:#}");
    }
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
/// message JSON on stdin. Waits for it so a manual echo-config check is observable.
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
    let status = child.wait().context("wait on notify command")?;
    if !status.success() {
        anyhow::bail!("notify command exited with {status}");
    }
    Ok(())
}

fn fire_webhook(url: &str, msg: &BusMessage) -> Result<()> {
    let resp = ureq::post(url)
        .header("Content-Type", "application/json")
        .send_json(msg)
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("notify webhook {url} status {status}");
    }
    Ok(())
}
