//! agy (Antigravity CLI) statusline telemetry.
//!
//! `--output-format stream-json` (see `providers::agy::AgyAdapter::build_headless` and
//! `StreamCoalescer::handle_agy` in `process.rs`) now supplies tools and tokens directly,
//! so this module no longer scrapes agy's on-disk transcript for them. Two things are
//! still not on the stream and have no other source, both verified by inspection of a
//! live payload:
//!
//! * agy's quota buckets (`gemini-5h` / `gemini-weekly`, `remaining_fraction`,
//!   `reset_in_seconds`)
//! * the context-window snapshot (`context_window.current_usage`) — the resident context
//!   (system prompt + history) the model actually processed for the latest call, as
//!   opposed to a per-step token delta
//!
//! agy fires the configured `statusLine.command` on every agent state change (verified:
//! it fires in `--print` too), piping a JSON payload carrying both. We install a wrapper
//! that tees each payload to a sink and chains to the user's own statusline, then read
//! the sink back per slot by matching the payload `cwd` to the slot worktree.

use anyhow::{Context, Result};
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

const WRAPPER_NAME: &str = "statusline-wrapper.sh";

/// Serializes the read-modify-write of the user's global `settings.json` across parallel
/// agy slots in this process (arena/parallel implement spawn several at once).
static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Install (idempotently) a statusline wrapper that tees agy's payloads to our sink and
/// chains to whatever statusline the user already had. Self-healing: if the user later
/// points `statusLine.command` somewhere else, the next call re-captures it as the chain
/// target and re-installs, so we never clobber their statusline — we wrap it.
///
/// Safety: this edits the user's *global* config. It (a) refuses to touch a `settings.json`
/// that doesn't parse to an object — never resets it to `{}` and strips the user's keys;
/// (b) writes atomically (temp + rename) so a reader never sees a torn file; (c) serializes
/// concurrent installs; (d) detects "already ours" by the wrapper filename, not an exact
/// string, so a reserialized command can't be captured as its own chain target (no loop).
pub fn ensure_statusline_hook(root: &Path) -> Result<()> {
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sdir = spar_dir(root);
    std::fs::create_dir_all(&sdir).with_context(|| format!("create {}", sdir.display()))?;
    let wrapper = wrapper_path(root);
    let wrapper_cmd = format!("bash {}", wrapper.display());

    let settings_file = settings_path(root);
    // Read existing settings. If the file exists but does not parse to a JSON object, abort
    // the settings edit entirely: overwriting it would discard the user's keys (model, auth,
    // preferences). Still lay down the wrapper + sink so a later fixed config can chain.
    let mut settings: Value = if settings_file.is_file() {
        let text = std::fs::read_to_string(&settings_file)?;
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            _ => {
                write_wrapper(&wrapper, &sink_path(root), &original_path(root))?;
                return Ok(());
            }
        }
    } else {
        serde_json::json!({})
    };

    let current = settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let already_ours = current.contains(WRAPPER_NAME);

    // Capture the chain target only when it isn't already our wrapper — matched by filename
    // so a reformatted/requoted wrapper command is never mistaken for a user command and
    // captured as its own chain target.
    if !already_ours {
        atomic_write(&original_path(root), current.as_bytes())?;
    }
    write_wrapper(&wrapper, &sink_path(root), &original_path(root))?;
    prune_sink(&sink_path(root));

    if !already_ours {
        settings["statusLine"] = serde_json::json!({
            "type": "command",
            "command": wrapper_cmd,
        });
        atomic_write(
            &settings_file,
            serde_json::to_string_pretty(&settings)?.as_bytes(),
        )
        .with_context(|| format!("write {}", settings_file.display()))?;
    }
    Ok(())
}

/// Remove the wrapper and restore the user's captured original statusline command.
pub fn uninstall_statusline_hook(root: &Path) -> Result<()> {
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let settings_file = settings_path(root);
    if !settings_file.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&settings_file)?;
    let Ok(mut settings) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    if !settings.is_object() {
        return Ok(());
    }
    let is_ours = settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .map(|c| c.contains(WRAPPER_NAME))
        .unwrap_or(false);
    if !is_ours {
        return Ok(()); // user pointed it elsewhere; leave it alone
    }
    let original = std::fs::read_to_string(original_path(root)).unwrap_or_default();
    if original.trim().is_empty() {
        settings.as_object_mut().unwrap().remove("statusLine");
    } else {
        settings["statusLine"] = serde_json::json!({"type": "command", "command": original});
    }
    atomic_write(
        &settings_file,
        serde_json::to_string_pretty(&settings)?.as_bytes(),
    )?;
    Ok(())
}

/// Write via temp file + rename so a concurrent reader sees either the old or the new
/// complete file, never a truncated one. The temp name is pid-scoped to avoid collisions.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
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
    /// The resident context (`context_window.current_usage`) the model processed for the
    /// latest call: a window gauge, not a spend counter, so it is not summed across calls.
    pub context_tokens: u64,
    /// Smallest remaining fraction across the account's `gemini-*` quota buckets, with the
    /// bucket name and its reset horizon — the binding constraint for an agy cooldown.
    pub quota_hint: Option<String>,
    pub quota_reset_secs: Option<i64>,
    pub quota_remaining_fraction: Option<f64>,
}

fn parse_payload(v: &Value) -> Payload {
    let current = v.get("context_window").and_then(|c| c.get("current_usage"));
    let get_u64 = |k: &str| -> u64 {
        current
            .and_then(|o| o.get(k))
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    let mut p = Payload {
        context_tokens: get_u64("input_tokens").saturating_add(get_u64("cache_read_input_tokens")),
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

/// Best sink payload for the slot worktree. `cwd` is unique per slot, so matching is
/// race-free across parallel slots. agy fires many payloads per run — early ones
/// (`authenticating`/`initializing`) and any teardown frame carry a zeroed context
/// window — so the context snapshot comes from the last cwd-matching payload that has a
/// non-zero one, falling back to the last match when none do. Quota is independent of
/// that: a teardown frame can carry a zeroed context window and still be the freshest
/// quota reading (e.g. the account went exhausted on the final call), so quota is taken
/// from the last cwd-matching payload that has *any* quota bucket, not gated on context.
pub fn latest_payload_for_cwd(root: &Path, cwd: &Path) -> Option<Payload> {
    let text = std::fs::read_to_string(sink_path(root)).ok()?;
    let want = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut last_any: Option<Payload> = None;
    let mut last_with_context: Option<Payload> = None;
    let mut last_with_quota: Option<Payload> = None;
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
            // Canonicalize the payload cwd too (symlinked worktree / trailing slash) so a
            // non-canonical path from agy still matches the slot.
            .map(|c| std::fs::canonicalize(c).unwrap_or_else(|_| PathBuf::from(c)) == want)
            .unwrap_or(false);
        if !matches {
            continue;
        }
        let p = parse_payload(&v);
        if p.context_tokens > 0 {
            last_with_context = Some(p.clone());
        }
        if p.quota_hint.is_some() {
            last_with_quota = Some(p.clone());
        }
        last_any = Some(p);
    }
    let mut result = last_with_context.or(last_any)?;
    if let Some(q) = last_with_quota {
        result.quota_hint = q.quota_hint;
        result.quota_reset_secs = q.quota_reset_secs;
        result.quota_remaining_fraction = q.quota_remaining_fraction;
    }
    Some(result)
}

/// Recovered telemetry for one agy slot: the context-window snapshot and quota, both
/// only available from the statusline sink. Best-effort — `None`/0 when absent.
#[derive(Debug, Default, Clone)]
pub struct AgyTelemetry {
    pub context_tokens: u64,
    pub quota_hint: Option<String>,
    pub quota_reset_secs: Option<i64>,
    pub quota_remaining_fraction: Option<f64>,
}

/// Collect the statusline-only telemetry for a slot given its worktree `cwd`. Returns
/// `None` when no sink payload for this cwd was found at all.
pub fn collect(root: &Path, cwd: &Path) -> Option<AgyTelemetry> {
    let payload = latest_payload_for_cwd(root, cwd)?;
    Some(AgyTelemetry {
        context_tokens: payload.context_tokens,
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
    fn payload_composes_context_snapshot_and_quota_by_cwd() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let cwd = tmp.path().join("wt");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = std::fs::canonicalize(&cwd).unwrap();
        // Init frame (zeroed) + two snapshot-bearing frames + a decoy cwd + a final zeroed
        // teardown frame. The last *snapshot-bearing* frame must win, not the teardown.
        let sink = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            serde_json::json!({"cwd": cwd_s, "agent_state": "initializing",
                "context_window": {"total_input_tokens": 0, "total_output_tokens": 0}}),
            serde_json::json!({"cwd": "/other",
                "context_window": {"current_usage": {"input_tokens": 1, "cache_read_input_tokens": 1}}}),
            serde_json::json!({"cwd": cwd_s,
                "context_window": {"current_usage": {"input_tokens": 12000, "cache_read_input_tokens": 20}},
                "quota": {"gemini-5h": {"remaining_fraction": 0.01, "reset_in_seconds": 3600},
                          "gemini-weekly": {"remaining_fraction": 0.9, "reset_in_seconds": 99999}}}),
            serde_json::json!({"cwd": cwd_s,
                "context_window": {"current_usage": {"input_tokens": 34743, "cache_read_input_tokens": 40}},
                "quota": {"gemini-5h": {"remaining_fraction": 0.005, "reset_in_seconds": 1800}}}),
            serde_json::json!({"cwd": cwd_s, "agent_state": "idle",
                "context_window": {"total_input_tokens": 0, "total_output_tokens": 0}}),
        );
        write(&sink_path(root), &sink);

        let t = collect(root, &cwd).expect("telemetry");
        assert_eq!(
            t.context_tokens,
            34743 + 40,
            "last snapshot-bearing frame wins, not the zeroed teardown"
        );
        // Binding gemini quota is the near-exhausted 5h bucket.
        assert_eq!(t.quota_reset_secs, Some(1800));
        assert!(t.quota_remaining_fraction.unwrap() < 0.01);
    }

    #[test]
    fn quota_comes_from_the_freshest_frame_even_when_context_is_zeroed_there() {
        // The account can go exhausted on the final call: the last frame is a teardown
        // with a zeroed context window but a fresher (more severe) quota reading than
        // the earlier snapshot-bearing frame. Quota must not be pinned to whichever
        // frame won the context snapshot.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let cwd = tmp.path().join("wt");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_s = std::fs::canonicalize(&cwd).unwrap();
        let sink = format!(
            "{}\n{}\n",
            serde_json::json!({"cwd": cwd_s,
                "context_window": {"current_usage": {"input_tokens": 12000, "cache_read_input_tokens": 20}},
                "quota": {"gemini-5h": {"remaining_fraction": 0.5, "reset_in_seconds": 3600}}}),
            serde_json::json!({"cwd": cwd_s, "agent_state": "idle",
                "context_window": {"total_input_tokens": 0, "total_output_tokens": 0},
                "quota": {"gemini-5h": {"remaining_fraction": 0.005, "reset_in_seconds": 60}}}),
        );
        write(&sink_path(root), &sink);

        let t = collect(root, &cwd).expect("telemetry");
        assert_eq!(
            t.context_tokens,
            12000 + 20,
            "context still comes from the snapshot-bearing frame"
        );
        assert_eq!(
            t.quota_reset_secs,
            Some(60),
            "quota must come from the freshest frame, not the context-winning one"
        );
        assert!(t.quota_remaining_fraction.unwrap() < 0.01);
    }

    #[test]
    fn malformed_settings_is_never_overwritten() {
        // A syntax error in the user's global config must NOT cause us to reset it to {}
        // and strip their keys. We leave it untouched and skip the settings edit.
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let raw = "{ \"model\": \"x\", }  // trailing comma + comment: invalid json";
        write(&settings_path(root), raw);
        ensure_statusline_hook(root).unwrap();
        assert_eq!(
            std::fs::read_to_string(settings_path(root)).unwrap(),
            raw,
            "malformed settings must be left byte-for-byte intact"
        );
    }

    #[test]
    fn uninstall_restores_original() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(
            &settings_path(root),
            r#"{"model":"x","statusLine":{"type":"command","command":"bash /orig.sh"}}"#,
        );
        ensure_statusline_hook(root).unwrap();
        uninstall_statusline_hook(root).unwrap();
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(root)).unwrap()).unwrap();
        assert_eq!(settings["statusLine"]["command"], "bash /orig.sh");
        assert_eq!(settings["model"], "x");
    }

    #[test]
    fn collect_none_when_nothing_on_disk() {
        let tmp = tempdir().unwrap();
        assert!(collect(tmp.path(), &tmp.path().join("wt")).is_none());
    }
}
