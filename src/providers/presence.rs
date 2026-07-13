//! Presence wiring: the adapter-level seam that connects a freshly-spawned slot to
//! its `PresenceSource`. The orchestrator calls [`wire`] once per spawn and gets back
//! the env every agent must carry plus an optional degraded-mode note to log. All the
//! provider-specific mechanism lives here behind the adapter's `presence_source()` —
//! the orchestrator never learns which provider it is.

use super::{DeliveryStrategy, PresenceSource, ProviderAdapter};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    // Only StopHookInject adapters (Claude) close the delivery loop through the Stop
    // hook. Grok shares the presence hook file but delivers via its native queue, so it
    // gets presence hooks without the injecting Stop hook.
    let inject_on_stop = adapter.delivery_strategy() == DeliveryStrategy::StopHookInject;
    let note = match adapter.presence_source() {
        PresenceSource::Hooks => install_claude_hooks(id, inject_on_stop).err().map(|e| {
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

/// Merge spar presence hooks into `<worktree>/.claude/settings.json`, preserving any
/// existing keys. When `inject_on_stop` is set, also install a Stop hook that runs
/// `spar bus deliver` — the pane-free channel that injects claimed bus messages by
/// blocking the turn (`{"decision":"block",…}`) instead of letting the agent go idle.
/// Refuses to write into the primary checkout (would dirty the repo).
fn install_claude_hooks(id: &SlotIdentity, inject_on_stop: bool) -> Result<()> {
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
        // Idempotent across resumes: drop any prior spar group before re-adding.
        arr.retain(|g| !is_spar_group(g));
        arr.push(entry);
    }

    if inject_on_stop {
        let stop = hooks
            .entry("Stop")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .context("hooks.Stop is not an array")?;
        stop.push(hook_group(
            None,
            &spar_deliver_cmd(&exe, id.run_id, id.agent_id),
        ));
    }

    let text = serde_json::to_string_pretty(&root)?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    // The hook file embeds the operator's absolute spar path plus the run/agent ids, so
    // an agent's `git add -A` or `spar ship` must never stage it into the PR. Two cases:
    // an untracked settings.json is held back by `info/exclude`; a *tracked* one (a repo
    // that commits `.claude/settings.json`) is unaffected by excludes, so mark it
    // `--skip-worktree` to make git ignore spar's rewrite. Both are best-effort.
    exclude_from_index(id.worktree, SETTINGS_REL)?;
    skip_worktree_if_tracked(id.worktree, SETTINGS_REL);
    Ok(())
}

/// Worktree-relative path of the settings file spar writes.
const SETTINGS_REL: &str = ".claude/settings.json";

/// Add `rel_path` to the worktree's git exclude file so an untracked copy is never
/// staged. Worktrees share the main checkout's `info/exclude` via `$GIT_COMMON_DIR`, so
/// resolve that first. Idempotent: a line already present is left untouched. A non-git
/// directory has no index to protect, so it is a safe no-op.
fn exclude_from_index(worktree: &Path, rel_path: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(worktree)
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        // git missing or not a repository: nothing can be staged, so nothing to exclude.
        _ => return Ok(()),
    };
    let common_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    // A relative git-common-dir resolves against the worktree.
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        worktree.join(common_dir)
    };
    let info = common_dir.join("info");
    std::fs::create_dir_all(&info).with_context(|| format!("create {}", info.display()))?;
    let exclude = info.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == rel_path) {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(rel_path);
    text.push('\n');
    std::fs::write(&exclude, text).with_context(|| format!("write {}", exclude.display()))?;
    Ok(())
}

/// If `rel_path` is tracked in the worktree's index, set its `--skip-worktree` bit so git
/// ignores spar's local rewrite. `info/exclude` only governs untracked files, so a repo
/// that *commits* `.claude/settings.json` would otherwise still show spar's edit as a
/// modification and stage it on `git add -A`, leaking the operator path and run/agent
/// ids. Best-effort: a non-git dir or an untracked path is a harmless no-op.
fn skip_worktree_if_tracked(worktree: &Path, rel_path: &str) {
    let tracked = Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", rel_path])
        .current_dir(worktree)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !tracked {
        return;
    }
    let _ = Command::new("git")
        .args(["update-index", "--skip-worktree", "--", rel_path])
        .current_dir(worktree)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn spar_heartbeat_cmd(exe: &str, run_id: &str, agent: &str, status: &str) -> String {
    format!(
        "{} bus heartbeat {run_id} {agent} --status {status}",
        shell_quote(exe)
    )
}

fn spar_deliver_cmd(exe: &str, run_id: &str, agent: &str) -> String {
    format!("{} bus deliver {run_id} {agent}", shell_quote(exe))
}

fn hook_group(matcher: Option<&str>, command: &str) -> Value {
    let hooks = json!([{ "type": "command", "command": command }]);
    match matcher {
        Some(m) => json!({ "matcher": m, "hooks": hooks }),
        None => json!({ "hooks": hooks }),
    }
}

/// True if a matcher group is one spar wrote (its command runs a `bus` subcommand spar
/// installs — `heartbeat` for presence or `deliver` for turn-boundary injection).
fn is_spar_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains("bus heartbeat") || c.contains("bus deliver"))
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
        // StopHookInject closes the delivery loop via a Stop `bus deliver` hook.
        assert!(
            text.contains("bus deliver abc123 impl-1"),
            "claude Stop hook must inject via `bus deliver`: {text}"
        );
    }

    #[test]
    fn grok_gets_no_deliver_hook() {
        let tmp = tempdir().unwrap();
        let wt = tmp.path().join("wt");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let exe = PathBuf::from("/usr/bin/spar");
        wire(&GrokAdapter, &id(&wt, &root, &exe));
        let text = std::fs::read_to_string(settings_path(&wt)).unwrap();
        // Grok delivers via its native queue, not the injecting Stop hook.
        assert!(
            !text.contains("bus deliver"),
            "grok must not get a deliver Stop hook: {text}"
        );
        assert!(text.contains("--status idle"));
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
        // user's echo hook preserved.
        assert!(stop.iter().any(|g| !is_spar_group(g)));
        // Exactly one spar heartbeat + one deliver group survive the second wire.
        let count_with = |needle: &str| {
            stop.iter()
                .filter(|g| serde_json::to_string(g).unwrap().contains(needle))
                .count()
        };
        assert_eq!(count_with("bus heartbeat"), 1, "heartbeat must be deduped");
        assert_eq!(count_with("bus deliver"), 1, "deliver must be deduped");
    }

    /// Shared helper: run a git command in `cwd`, asserting success.
    fn git_ok(args: &[&str], cwd: &Path) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn git_common_dir(cwd: &Path) -> PathBuf {
        let out = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(cwd)
            .output()
            .unwrap();
        let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
        if dir.is_absolute() {
            dir
        } else {
            cwd.join(dir)
        }
    }

    #[test]
    fn untracked_settings_file_is_excluded_from_git_in_a_worktree() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_ok(&["init", "-q"], &repo);
        git_ok(&["config", "user.email", "t@t"], &repo);
        git_ok(&["config", "user.name", "t"], &repo);
        std::fs::write(repo.join("README"), "x").unwrap();
        git_ok(&["add", "README"], &repo);
        git_ok(&["commit", "-qm", "init"], &repo);

        // A real linked worktree so info/exclude resolves via the common gitdir.
        let wt = tmp.path().join("wt");
        git_ok(
            &["worktree", "add", "-q", wt.to_str().unwrap(), "HEAD"],
            &repo,
        );
        let exe = PathBuf::from("/usr/bin/spar");

        let w = wire(&ClaudeAdapter, &id(&wt, &repo, &exe));
        assert!(w.note.is_none(), "note: {:?}", w.note);
        assert!(settings_path(&wt).is_file());

        let out = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(
            !status.contains(".claude/settings.json"),
            "settings.json must be excluded from git status: {status}"
        );

        // Idempotent: a second wire must not duplicate the exclude line.
        wire(&ClaudeAdapter, &id(&wt, &repo, &exe));
        let exclude =
            std::fs::read_to_string(git_common_dir(&wt).join("info").join("exclude")).unwrap();
        assert_eq!(
            exclude.lines().filter(|l| l.trim() == SETTINGS_REL).count(),
            1,
            "exclude line must not be duplicated: {exclude}"
        );
    }

    #[test]
    fn tracked_settings_file_is_not_staged_after_wire() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        git_ok(&["init", "-q"], &repo);
        git_ok(&["config", "user.email", "t@t"], &repo);
        git_ok(&["config", "user.name", "t"], &repo);
        // The target repo *commits* .claude/settings.json — the case info/exclude misses.
        std::fs::write(settings_path(&repo), r#"{"model":"opus"}"#).unwrap();
        git_ok(&["add", ".claude/settings.json"], &repo);
        git_ok(&["commit", "-qm", "init"], &repo);

        let wt = tmp.path().join("wt");
        git_ok(
            &["worktree", "add", "-q", wt.to_str().unwrap(), "HEAD"],
            &repo,
        );
        let exe = PathBuf::from("/usr/bin/spar");

        // spar overwrites the tracked file with its hook config.
        let w = wire(&ClaudeAdapter, &id(&wt, &repo, &exe));
        assert!(w.note.is_none(), "note: {:?}", w.note);
        let written = std::fs::read_to_string(settings_path(&wt)).unwrap();
        assert!(written.contains("bus heartbeat"), "hooks must be written");

        // git status must not report the rewrite (skip-worktree bit).
        let out = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(
            !status.contains(".claude/settings.json"),
            "tracked settings.json must not appear in git status: {status}"
        );

        // `git add -A` must not stage the operator path / run ids.
        git_ok(&["add", "-A"], &wt);
        let staged = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let staged = String::from_utf8_lossy(&staged.stdout);
        assert!(
            !staged.contains(".claude/settings.json"),
            "tracked settings.json must not be staged by `git add -A`: {staged}"
        );
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
