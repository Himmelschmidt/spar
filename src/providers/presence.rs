//! Presence wiring: the adapter-level seam that connects a freshly-spawned slot to
//! its `PresenceSource`. The orchestrator calls [`wire`] once per spawn and gets back
//! the env every agent must carry plus an optional degraded-mode note to log. All the
//! provider-specific mechanism lives here behind the adapter's `presence_source()` —
//! the orchestrator never learns which provider it is.

use super::{PresenceSource, ProviderAdapter};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Stable identity + locations a slot needs to phone home for presence/delivery.
pub struct SlotIdentity<'a> {
    /// Stable agent id exported as `SPAR_AGENT_ID` (the slot id).
    pub agent_id: &'a str,
    pub run_id: &'a str,
    /// Primary checkout that owns `.spar/runs/<id>`; heartbeats resolve against it.
    pub project_root: &'a Path,
    /// Directory the agent runs in (its worktree). Hook files land here.
    pub worktree: &'a Path,
    /// Absolute path to the running `spar` binary, baked into the hook command.
    pub spar_exe: &'a Path,
}

/// Result of wiring presence for one slot.
pub struct PresenceWiring {
    /// Env vars to attach to the spawned agent process.
    pub env: Vec<(String, String)>,
    /// A line to log when presence is degraded (e.g. agy has no event stream).
    pub note: Option<String>,
}

/// Wire the adapter's presence source into the slot and return the env + any note.
///
/// Best-effort: presence is telemetry, so a failure to install hooks degrades to a
/// note rather than failing the spawn. Every agent still gets the identity env.
pub fn wire(adapter: &dyn ProviderAdapter, id: &SlotIdentity) -> PresenceWiring {
    let env = vec![
        ("SPAR_AGENT_ID".to_string(), id.agent_id.to_string()),
        ("SPAR_RUN_ID".to_string(), id.run_id.to_string()),
        (
            "SPAR_PROJECT_ROOT".to_string(),
            id.project_root.display().to_string(),
        ),
    ];
    let note = match adapter.presence_source() {
        PresenceSource::Hooks => install_claude_hooks(id).err().map(|e| {
            format!(
                "{}: presence hooks not installed ({e}) — degraded presence",
                id.agent_id
            )
        }),
        PresenceSource::Sse => Some(format!(
            "{}: SSE presence bus not yet subscribed — degraded presence",
            id.agent_id
        )),
        PresenceSource::HttpPush => Some(format!(
            "{}: http-push presence not yet wired — degraded presence",
            id.agent_id
        )),
        PresenceSource::None => Some(format!(
            "{}: no event stream — inbox-on-next-turn, degraded presence",
            id.agent_id
        )),
    };
    PresenceWiring { env, note }
}

/// The four Claude-format hook events spar maps to bus presence transitions.
fn hook_events() -> [(&'static str, Option<&'static str>, &'static str); 4] {
    [
        ("UserPromptSubmit", None, "working"),
        ("PreToolUse", Some("*"), "working"),
        ("Notification", None, "blocked"),
        ("Stop", None, "idle"),
    ]
}

/// Merge spar heartbeat hooks into `<worktree>/.claude/settings.json`, preserving any
/// existing keys. Refuses to write into the primary checkout (would dirty the repo).
fn install_claude_hooks(id: &SlotIdentity) -> Result<()> {
    if same_dir(id.worktree, id.project_root) {
        anyhow::bail!("would pollute primary checkout working tree");
    }
    let path = settings_path(id.worktree);
    let dir = path.parent().expect("settings path has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?
        }
        _ => Value::Object(Map::new()),
    };
    let obj = root
        .as_object_mut()
        .context("existing .claude/settings.json is not a JSON object")?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("existing hooks is not a JSON object")?;

    let exe = id.spar_exe.display().to_string();
    for (event, matcher, status) in hook_events() {
        let entry = hook_group(
            matcher,
            &spar_heartbeat_cmd(&exe, id.run_id, id.agent_id, status),
        );
        let arr = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;
        // Idempotent across resumes: drop any prior spar heartbeat group before re-adding.
        arr.retain(|g| !is_spar_heartbeat_group(g));
        arr.push(entry);
    }

    let text = serde_json::to_string_pretty(&root)?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn spar_heartbeat_cmd(exe: &str, run_id: &str, agent: &str, status: &str) -> String {
    format!(
        "{} bus heartbeat {run_id} {agent} --status {status}",
        shell_quote(exe)
    )
}

fn hook_group(matcher: Option<&str>, command: &str) -> Value {
    let hooks = json!([{ "type": "command", "command": command }]);
    match matcher {
        Some(m) => json!({ "matcher": m, "hooks": hooks }),
        None => json!({ "hooks": hooks }),
    }
}

/// True if a matcher group is one spar wrote (its command runs `bus heartbeat`).
fn is_spar_heartbeat_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains("bus heartbeat"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Minimal POSIX single-quote for the exe path baked into a hook command line.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Path to the settings file spar writes for a worktree (test helper / callers).
pub fn settings_path(worktree: &Path) -> PathBuf {
    worktree.join(".claude").join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{AgyAdapter, ClaudeAdapter, GrokAdapter};
    use tempfile::tempdir;

    fn id<'a>(worktree: &'a Path, project_root: &'a Path, exe: &'a Path) -> SlotIdentity<'a> {
        SlotIdentity {
            agent_id: "impl-1",
            run_id: "abc123",
            project_root,
            worktree,
            spar_exe: exe,
        }
    }

    #[test]
    fn claude_hooks_written_with_all_transitions() {
        let tmp = tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let exe = PathBuf::from("/usr/bin/spar");

        let w = wire(&ClaudeAdapter, &id(&wt, &root, &exe));
        assert!(w.note.is_none(), "note: {:?}", w.note);
        assert!(w
            .env
            .iter()
            .any(|(k, v)| k == "SPAR_AGENT_ID" && v == "impl-1"));

        let text = std::fs::read_to_string(settings_path(&wt)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let hooks = v.get("hooks").unwrap().as_object().unwrap();
        for ev in ["UserPromptSubmit", "PreToolUse", "Notification", "Stop"] {
            assert!(hooks.contains_key(ev), "missing {ev}");
        }
        assert!(text.contains("--status working"));
        assert!(text.contains("--status blocked"));
        assert!(text.contains("--status idle"));
        assert!(text.contains("abc123 impl-1"));
    }

    #[test]
    fn grok_shares_the_same_hook_file() {
        let tmp = tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let exe = PathBuf::from("/usr/bin/spar");
        let w = wire(&GrokAdapter, &id(&wt, &root, &exe));
        assert!(w.note.is_none());
        assert!(settings_path(&wt).is_file());
    }

    #[test]
    fn agy_degrades_with_documented_note() {
        let tmp = tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let exe = PathBuf::from("/usr/bin/spar");
        let w = wire(&AgyAdapter, &id(&wt, &root, &exe));
        assert_eq!(
            w.note.as_deref(),
            Some("impl-1: no event stream — inbox-on-next-turn, degraded presence")
        );
        assert!(!settings_path(&wt).exists(), "agy must not write hooks");
    }

    #[test]
    fn merge_preserves_existing_keys_and_is_idempotent() {
        let tmp = tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            settings_path(&wt),
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo user-hook"}]}]}}"#,
        )
        .unwrap();
        let exe = PathBuf::from("/usr/bin/spar");

        wire(&ClaudeAdapter, &id(&wt, &root, &exe));
        wire(&ClaudeAdapter, &id(&wt, &root, &exe)); // twice: must not duplicate

        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(&wt)).unwrap()).unwrap();
        assert_eq!(v.get("model").unwrap(), "opus");
        let stop = v
            .get("hooks")
            .unwrap()
            .get("Stop")
            .unwrap()
            .as_array()
            .unwrap();
        // user's echo hook preserved + exactly one spar heartbeat group
        assert!(stop.iter().any(|g| !is_spar_heartbeat_group(g)));
        let spar_groups = stop.iter().filter(|g| is_spar_heartbeat_group(g)).count();
        assert_eq!(spar_groups, 1, "heartbeat group must be deduped");
    }

    #[test]
    fn refuses_to_write_into_primary_checkout() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let exe = PathBuf::from("/usr/bin/spar");
        // worktree == project_root
        let w = wire(&ClaudeAdapter, &id(&root, &root, &exe));
        assert!(w.note.is_some());
        assert!(!settings_path(&root).exists());
    }
}
