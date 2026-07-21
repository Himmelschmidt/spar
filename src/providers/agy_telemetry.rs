//! agy (Antigravity CLI) telemetry recovery.
//!
//! agy drives in `--print` mode and emits almost nothing to stdout, so the generic
//! stdout-stream usage parser reports zero tools/tokens for every agy slot. The real
//! signal lives in two places on disk, both keyed by the run's `conversation_id`:
//!
//! * **transcript** — `<root>/brain/<cid>/.system_generated/logs/transcript.jsonl`,
//!   one JSON record per step. Tool calls, tool errors, step count, and a real
//!   last-activity timestamp come from here. Written regardless of TUI/print mode.
//! * **statusline payload** — agy fires the configured `statusLine.command` on every
//!   agent state change (verified: it fires in `--print` too), piping a JSON payload
//!   carrying token counts and quota that are *not* persisted anywhere else. We install
//!   a wrapper that tees each payload to a sink and chains to the user's own statusline,
//!   then read the sink back per slot by matching the payload `cwd` to the slot worktree.
//!
//! Note: the payload's own `transcript_path` field is unreliable in agy 1.1.5 (points at
//! a non-existent `~/.gemini/antigravity/` root), so the transcript path is derived from
//! the reliable `conversation_id`, not that field.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// agy's config root, `$HOME/.gemini/antigravity-cli`.
pub fn root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let r = PathBuf::from(home).join(".gemini/antigravity-cli");
    r.is_dir().then_some(r)
}

fn spar_dir(root: &Path) -> PathBuf {
    root.join(".spar")
}
fn sink_path(root: &Path) -> PathBuf {
    spar_dir(root).join("statusline.jsonl")
}
fn wrapper_path(root: &Path) -> PathBuf {
    spar_dir(root).join("statusline-wrapper.sh")
}
fn original_path(root: &Path) -> PathBuf {
    spar_dir(root).join("original-statusline")
}
fn settings_path(root: &Path) -> PathBuf {
    root.join("settings.json")
}
fn transcript_path(root: &Path, cid: &str) -> PathBuf {
    root.join("brain")
        .join(cid)
        .join(".system_generated/logs/transcript.jsonl")
}

/// Install (idempotently) a statusline wrapper that tees agy's payloads to our sink and
/// chains to whatever statusline the user already had. Self-healing: if the user later
/// points `statusLine.command` somewhere else, the next call re-captures it as the chain
/// target and re-installs, so we never clobber their statusline — we wrap it.
pub fn ensure_statusline_hook(root: &Path) -> Result<()> {
    let sdir = spar_dir(root);
    std::fs::create_dir_all(&sdir).with_context(|| format!("create {}", sdir.display()))?;
    let wrapper = wrapper_path(root);
    let wrapper_cmd = format!("bash {}", wrapper.display());

    let settings_file = settings_path(root);
    let mut settings: Value = if settings_file.is_file() {
        serde_json::from_str(&std::fs::read_to_string(&settings_file)?).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    if !settings.is_object() {
        settings = serde_json::json!({});
    }

    let current = settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    // Capture the chain target unless we're already installed.
    if current != wrapper_cmd {
        std::fs::write(original_path(root), current.as_bytes())?;
    }

    write_wrapper(&wrapper, &sink_path(root), &original_path(root))?;
    prune_sink(&sink_path(root));

    if current != wrapper_cmd {
        settings["statusLine"] = serde_json::json!({
            "type": "command",
            "command": wrapper_cmd,
        });
        std::fs::write(&settings_file, serde_json::to_string_pretty(&settings)?)
            .with_context(|| format!("write {}", settings_file.display()))?;
    }
    Ok(())
}

/// The append-only sink lives in the user's config dir; keep it bounded. Called at spawn
/// (before new payloads), so dropping the oldest lines can't lose an in-flight slot's
/// recent activity. Best-effort: a rewrite racing a concurrent append loses at most one
/// status-change payload of many.
const SINK_MAX_BYTES: u64 = 4 * 1024 * 1024;
const SINK_KEEP_LINES: usize = 2000;

fn prune_sink(sink: &Path) {
    let Ok(meta) = std::fs::metadata(sink) else {
        return;
    };
    if meta.len() <= SINK_MAX_BYTES {
        return;
    }
    let Ok(text) = std::fs::read_to_string(sink) else {
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= SINK_KEEP_LINES {
        return;
    }
    let kept = lines[lines.len() - SINK_KEEP_LINES..].join("\n");
    let _ = std::fs::write(sink, format!("{kept}\n"));
}

fn write_wrapper(wrapper: &Path, sink: &Path, original: &Path) -> Result<()> {
    // Read stdin once, append the payload to the sink, then replay it to the user's
    // original statusline (if any) so their status bar keeps working.
    let script = format!(
        r#"#!/usr/bin/env bash
# spar agy statusline tee (auto-generated). Tees agy's payload to a sink for
# telemetry recovery, then chains to the user's original statusline command.
sink="{sink}"
orig_file="{original}"
payload="$(cat)"
printf '%s\n' "$payload" >> "$sink" 2>/dev/null
if [ -s "$orig_file" ]; then
  orig="$(cat "$orig_file")"
  printf '%s' "$payload" | eval "$orig"
fi
"#,
        sink = sink.display(),
        original = original.display(),
    );
    std::fs::write(wrapper, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// A statusline payload we care about (tolerant to missing fields).
#[derive(Debug, Default, Clone)]
pub struct Payload {
    pub conversation_id: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// Smallest remaining fraction across the account's `gemini-*` quota buckets, with the
    /// bucket name and its reset horizon — the binding constraint for an agy cooldown.
    pub quota_hint: Option<String>,
    pub quota_reset_secs: Option<i64>,
    pub quota_remaining_fraction: Option<f64>,
}

fn parse_payload(v: &Value) -> Payload {
    let cw = v.get("context_window");
    let get_u64 = |obj: Option<&Value>, k: &str| -> u64 {
        obj.and_then(|o| o.get(k))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    let current = cw.and_then(|c| c.get("current_usage"));
    let mut p = Payload {
        conversation_id: v
            .get("conversation_id")
            .or_else(|| v.get("session_id"))
            .and_then(|c| c.as_str())
            .map(str::to_string),
        input_tokens: get_u64(cw, "total_input_tokens"),
        output_tokens: get_u64(cw, "total_output_tokens"),
        cache_read_tokens: get_u64(current, "cache_read_input_tokens"),
        ..Default::default()
    };
    // Quota: the account exposes gemini-5h / gemini-weekly (and 3p-* for other models).
    // For an agy (Gemini) slot the binding limit is the smallest gemini-* remaining.
    if let Some(q) = v.get("quota").and_then(|q| q.as_object()) {
        let mut best: Option<(f64, String, i64)> = None;
        for (name, bucket) in q {
            if !name.starts_with("gemini-") {
                continue;
            }
            let frac = bucket
                .get("remaining_fraction")
                .and_then(|x| x.as_f64())
                .unwrap_or(1.0);
            let reset = bucket
                .get("reset_in_seconds")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(bf, _, _)| frac < *bf) {
                best = Some((frac, name.clone(), reset));
            }
        }
        if let Some((frac, name, reset)) = best {
            p.quota_remaining_fraction = Some(frac);
            p.quota_reset_secs = Some(reset);
            p.quota_hint = Some(format!(
                "{name} {:.1}% remaining (resets in {}m)",
                frac * 100.0,
                reset / 60
            ));
        }
    }
    p
}

/// Latest sink payload whose `cwd` matches the slot worktree. `cwd` is unique per slot
/// (each coding slot gets its own worktree), so this is race-free across parallel slots.
pub fn latest_payload_for_cwd(root: &Path, cwd: &Path) -> Option<Payload> {
    let text = std::fs::read_to_string(sink_path(root)).ok()?;
    let want = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut latest = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let matches = v
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(|c| Path::new(c) == want.as_path())
            .unwrap_or(false);
        if matches {
            latest = Some(parse_payload(&v));
        }
    }
    latest
}

/// Tool/activity stats parsed from a conversation transcript.
#[derive(Debug, Default, Clone)]
pub struct TranscriptStats {
    pub tools: u32,
    pub tool_errors: u32,
    pub steps: u32,
    pub last_activity: Option<DateTime<Utc>>,
}

const TOOL_TYPES: &[&str] = &["RUN_COMMAND", "VIEW_FILE", "CODE_ACTION", "GREP_SEARCH"];

fn is_error_status(status: &str) -> bool {
    let s = status.to_ascii_uppercase();
    s.contains("ERROR") || s.contains("FAIL") || s.contains("TIMEOUT") || s.contains("CANCEL")
}

pub fn transcript_stats(root: &Path, cid: &str) -> Option<TranscriptStats> {
    let text = std::fs::read_to_string(transcript_path(root, cid)).ok()?;
    let mut st = TranscriptStats::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        st.steps += 1;
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if TOOL_TYPES.contains(&ty) {
            st.tools += 1;
            if is_error_status(status) {
                st.tool_errors += 1;
            }
        }
        if let Some(ts) = v
            .get("created_at")
            .and_then(|c| c.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            let ts = ts.with_timezone(&Utc);
            if st.last_activity.is_none_or(|prev| ts > prev) {
                st.last_activity = Some(ts);
            }
        }
    }
    Some(st)
}

/// Recovered telemetry for one agy slot: tool counts from the transcript, tokens/quota
/// from the statusline sink. Any field is best-effort — `None`/0 when the source is absent.
#[derive(Debug, Default, Clone)]
pub struct AgyTelemetry {
    pub tools: u32,
    pub tool_errors: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub context_tokens: u64,
    pub last_activity: Option<DateTime<Utc>>,
    pub quota_hint: Option<String>,
    pub quota_reset_secs: Option<i64>,
    pub quota_remaining_fraction: Option<f64>,
}

/// Collect everything for a slot given its worktree `cwd`. Returns `None` only when no
/// telemetry could be found at all (no sink payload and no discoverable transcript).
pub fn collect(root: &Path, cwd: &Path) -> Option<AgyTelemetry> {
    let payload = latest_payload_for_cwd(root, cwd);
    let cid = payload.as_ref().and_then(|p| p.conversation_id.clone());
    let tstats = cid.as_deref().and_then(|c| transcript_stats(root, c));
    if payload.is_none() && tstats.is_none() {
        return None;
    }
    let payload = payload.unwrap_or_default();
    let tstats = tstats.unwrap_or_default();
    Some(AgyTelemetry {
        tools: tstats.tools,
        tool_errors: tstats.tool_errors,
        input_tokens: payload.input_tokens,
        output_tokens: payload.output_tokens,
        cache_read_tokens: payload.cache_read_tokens,
        context_tokens: payload.input_tokens.saturating_add(payload.output_tokens),
        last_activity: tstats.last_activity,
        quota_hint: payload.quota_hint,
        quota_reset_secs: payload.quota_reset_secs,
        quota_remaining_fraction: payload.quota_remaining_fraction,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn install_wraps_and_chains_existing_statusline() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(
            &settings_path(root),
            r#"{"model":"x","statusLine":{"type":"command","command":"bash /orig.sh"}}"#,
        );
        ensure_statusline_hook(root).unwrap();

        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(root)).unwrap()).unwrap();
        // statusLine now points at our wrapper, other keys preserved.
        assert_eq!(settings["model"], "x");
        assert!(settings["statusLine"]["command"]
            .as_str()
            .unwrap()
            .contains("statusline-wrapper.sh"));
        // The user's original command is captured as the chain target.
        assert_eq!(
            std::fs::read_to_string(original_path(root)).unwrap(),
            "bash /orig.sh"
        );
        assert!(wrapper_path(root).is_file());
    }

    #[test]
    fn install_is_idempotent_and_does_not_rechain_itself() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(
            &settings_path(root),
            r#"{"statusLine":{"command":"bash /orig.sh"}}"#,
        );
        ensure_statusline_hook(root).unwrap();
        // Second call must not capture the wrapper as its own chain target.
        ensure_statusline_hook(root).unwrap();
        assert_eq!(
            std::fs::read_to_string(original_path(root)).unwrap(),
            "bash /orig.sh"
        );
    }

    #[test]
    fn install_with_no_prior_statusline() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(&settings_path(root), r#"{"model":"x"}"#);
        ensure_statusline_hook(root).unwrap();
        assert_eq!(std::fs::read_to_string(original_path(root)).unwrap(), "");
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(root)).unwrap()).unwrap();
        assert_eq!(settings["model"], "x");
    }

    #[test]
    fn payload_and_transcript_compose_by_cwd() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let cid = "conv-123";
        let cwd = tmp.path().join("wt");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = std::fs::canonicalize(&cwd).unwrap();
        // two payloads for this cwd; the later one wins. Plus a decoy for another cwd.
        let sink = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"cwd": "/other", "conversation_id": "nope",
                "context_window": {"total_input_tokens": 1, "total_output_tokens": 1}}),
            serde_json::json!({"cwd": cwd_s, "conversation_id": cid,
                "context_window": {"total_input_tokens": 100, "total_output_tokens": 50,
                    "current_usage": {"cache_read_input_tokens": 20}},
                "quota": {"gemini-5h": {"remaining_fraction": 0.01, "reset_in_seconds": 3600},
                          "gemini-weekly": {"remaining_fraction": 0.9, "reset_in_seconds": 99999}}}),
            serde_json::json!({"cwd": cwd_s, "conversation_id": cid,
                "context_window": {"total_input_tokens": 200, "total_output_tokens": 90,
                    "current_usage": {"cache_read_input_tokens": 40}},
                "quota": {"gemini-5h": {"remaining_fraction": 0.005, "reset_in_seconds": 1800}}}),
        );
        write(&sink_path(root), &sink);
        write(
            &transcript_path(root, cid),
            &format!(
                "{}\n{}\n{}\n{}\n",
                serde_json::json!({"type": "USER_INPUT", "status": "DONE", "created_at": "2026-07-21T12:00:00Z"}),
                serde_json::json!({"type": "RUN_COMMAND", "status": "DONE", "created_at": "2026-07-21T12:01:00Z"}),
                serde_json::json!({"type": "VIEW_FILE", "status": "ERROR", "created_at": "2026-07-21T12:02:00Z"}),
                serde_json::json!({"type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-07-21T12:03:00Z"}),
            ),
        );

        let t = collect(root, &cwd).expect("telemetry");
        assert_eq!(t.tools, 2, "RUN_COMMAND + VIEW_FILE");
        assert_eq!(t.tool_errors, 1, "the ERROR VIEW_FILE");
        assert_eq!(t.input_tokens, 200, "latest payload wins");
        assert_eq!(t.output_tokens, 90);
        assert_eq!(t.cache_read_tokens, 40);
        assert_eq!(t.context_tokens, 290);
        assert_eq!(
            t.last_activity.unwrap().to_rfc3339(),
            "2026-07-21T12:03:00+00:00"
        );
        // Binding gemini quota is the near-exhausted 5h bucket.
        assert_eq!(t.quota_reset_secs, Some(1800));
        assert!(t.quota_remaining_fraction.unwrap() < 0.01);
    }

    #[test]
    fn collect_none_when_nothing_on_disk() {
        let tmp = tempdir().unwrap();
        assert!(collect(tmp.path(), &tmp.path().join("wt")).is_none());
    }
}
