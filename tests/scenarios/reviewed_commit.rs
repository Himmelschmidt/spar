//! Reviewed-commit gate coverage (real orchestrator, real spawned slots, no
//! `--dry-run`). See DECISIONS.md O94.
//!
//! A reviewer slot that writes a *fresh* artifact loses the gate anyway when the
//! artifact is about the wrong tree: every dispatch writes `review-<slot>.md`
//! with `approve`, but names a fixed dead sha instead of the panel head, and
//! the run must refuse to count it — slot `failed` with the mismatch naming
//! both shas, never `Done` via that verdict. A second reviewer that approves
//! with the honest sha every round proves the fake's writes genuinely satisfy
//! the gate, so the failure cannot be dismissed as a broken fake. A second
//! test covers the absent-line shape with the same harness.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

const DEAD_SHA: &str = "0000000000000000000000000000000000000000";

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

fn spar_cmd() -> assert_cmd::Command {
    let mut c = cargo_bin_cmd!("spar");
    c.env("SPAR_HOME", spar_home_dir());
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c.env_remove("SPAR_DRY_RUN");
    c
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn only_run_id(proj: &std::path::Path) -> String {
    std::fs::read_dir(proj.join(".spar/runs"))
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}

/// A fake `claude` CLI. The real adapter invokes `claude -p "<prompt>" ...`,
/// so the full prompt text arrives as `$2`; the expected artifact path and the
/// review worktree are read off its lines, never hardcoded. Behavior per
/// artifact:
/// - `summary-*.md` (implementer): always written, every round.
/// - `review-*-review-0-*.md`: fresh `approve` with the honest
///   `Reviewed-Commit` every dispatch (the non-vacuity guard).
/// - `review-*-review-1-*.md`: fresh `approve` every dispatch, but naming a
///   fixed dead sha — fresh bytes about a commit that is not under review.
const FAKE_CLAUDE_STALE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
STATE="$(dirname "$0")/state"
mkdir -p "$STATE"
TOOL_LINE='{"type":"assistant","message":{"model":"fake","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read","input":{"file_path":"x"}}]}}'
DEST=""
CWD=""
for a in "$@"; do
  case "$a" in
    *"Write review to:"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write review to: \([^ ]*\).*/\1/p' | head -n 1)
      ;;
    *"Code under review (worktree):"*)
      CWD=$(printf '%s' "$a" | sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' | head -n 1)
      ;;
    *"Write a summary to"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write a summary to `\([^`]*\)`.*/\1/p' | head -n 1)
      ;;
  esac
done
case "$DEST" in
  *review-0-*)
    SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || true)
    { printf '## Verdict\napprove\n'; [ -n "$SHA" ] && printf 'Reviewed-Commit: %s\n' "$SHA"; } > "$DEST"
    echo "$TOOL_LINE"
    exit 0
    ;;
  *review-1-*)
    printf '## Verdict\napprove\n\nReviewed-Commit: 0000000000000000000000000000000000000000\n' > "$DEST"
    echo "$TOOL_LINE"
    exit 0
    ;;
  "")
    exit 0
    ;;
  *)
    printf '# Summary\nDid the stub thing.\n' > "$DEST"
    exit 0
    ;;
esac
"#;

/// Same harness, but the dishonest slot writes a fresh `approve` with no
/// `Reviewed-Commit` line at all.
const FAKE_CLAUDE_ABSENT: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
STATE="$(dirname "$0")/state"
mkdir -p "$STATE"
TOOL_LINE='{"type":"assistant","message":{"model":"fake","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read","input":{"file_path":"x"}}]}}'
DEST=""
CWD=""
for a in "$@"; do
  case "$a" in
    *"Write review to:"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write review to: \([^ ]*\).*/\1/p' | head -n 1)
      ;;
    *"Code under review (worktree):"*)
      CWD=$(printf '%s' "$a" | sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' | head -n 1)
      ;;
    *"Write a summary to"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write a summary to `\([^`]*\)`.*/\1/p' | head -n 1)
      ;;
  esac
done
case "$DEST" in
  *review-0-*)
    SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || true)
    { printf '## Verdict\napprove\n'; [ -n "$SHA" ] && printf 'Reviewed-Commit: %s\n' "$SHA"; } > "$DEST"
    echo "$TOOL_LINE"
    exit 0
    ;;
  *review-1-*)
    printf '## Verdict\napprove\n' > "$DEST"
    echo "$TOOL_LINE"
    exit 0
    ;;
  "")
    exit 0
    ;;
  *)
    printf '# Summary\nDid the stub thing.\n' > "$DEST"
    exit 0
    ;;
esac
"#;

fn run_with_fake(proj: &std::path::Path, bin: &std::path::Path, fake: &str) {
    let fake_path = bin.join("claude");
    std::fs::write(&fake_path, fake).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // Single-vendor fleet, frozen at creation (O27): a failed reviewer finds no
    // rotation candidate anywhere, so the bad verdict has nowhere to hide
    // behind a second vendor and the loop must fail closed.
    let _ = spar_cmd()
        .current_dir(proj)
        .args([
            "implement",
            "-t",
            "reviewed commit",
            "--providers",
            "cli:claude",
            "--without",
            "suite",
            "--json",
        ])
        .env("PATH", &path_env)
        .assert()
        .get_output()
        .clone();
}

fn setup(fake: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);
    std::fs::write(
        proj.join("spar.toml"),
        "[providers]\norder = [\"cli:claude\"]\n",
    )
    .unwrap();
    run_with_fake(&proj, &bin, fake);
    (tmp, proj)
}

fn state_json(proj: &std::path::Path, run_id: &str) -> serde_json::Value {
    let run_dir = proj.join(".spar/runs").join(run_id);
    serde_json::from_str(&std::fs::read_to_string(run_dir.join("state.json")).unwrap()).unwrap()
}

#[test]
fn a_fresh_approve_about_a_stale_sha_is_not_counted() {
    let (_tmp, proj) = setup(FAKE_CLAUDE_STALE);
    // Nothing in the fakes commits, so the panel head is still the init commit.
    let head = git(&proj, &["rev-parse", "HEAD"]);
    assert_ne!(head, DEAD_SHA, "the dead sha must not be the real head");

    let run_id = only_run_id(&proj);
    let run_dir = proj.join(".spar/runs").join(&run_id);
    let state = state_json(&proj, &run_id);
    let slots = state["slots"].as_array().expect("slots");
    let reviewers: Vec<&serde_json::Value> =
        slots.iter().filter(|s| s["role"] == "reviewer").collect();
    assert!(
        reviewers.len() >= 2,
        "guard is vacuous without a two-reviewer panel: {state}"
    );
    assert!(
        state["round"].as_u64().unwrap_or(0) >= 2,
        "the run must have re-dispatched slots in a second round: {state}"
    );
    assert_ne!(
        state["phase"], "done",
        "a stale approve must never ship the run"
    );

    let stale = reviewers
        .iter()
        .find(|s| s["id"].as_str().is_some_and(|id| id.contains("review-1")))
        .expect("a review-1 slot");
    assert_eq!(
        stale["status"], "failed",
        "the fresh-but-stale verdict must fail, not pass: {stale}"
    );
    // The recorded reason must be the sha gate firing, not a fake-CLI crash and
    // not the mtime layer: the artifact was this dispatch's own fresh bytes.
    let err = stale["error"].as_str().unwrap_or("");
    assert!(
        err.contains("panel dispatched against"),
        "the failure must name the sha gate: {stale}"
    );
    assert!(
        err.contains(DEAD_SHA) && err.contains(&head),
        "the failure must name both shas at a glance: {stale}"
    );
    // The stale file itself is preserved as drift evidence, never synthesized
    // over: had the gate counted it, the slot would read done.
    let stale_id = stale["id"].as_str().unwrap();
    let stale_body =
        std::fs::read_to_string(run_dir.join(format!("artifacts/review-{stale_id}.md")))
            .expect("stale review file");
    assert!(
        stale_body.contains(DEAD_SHA),
        "the dead sha must still be on disk, untouched: {stale_body}"
    );
    // The audit trail answers which commit each reviewer judged without reading
    // artifacts: slot state for the honest slot, events for the stale one.
    let events = std::fs::read_to_string(run_dir.join("events.jsonl")).expect("events.jsonl");
    assert!(
        events.contains(DEAD_SHA) && events.contains(&head),
        "events.jsonl must name both shas: {events}"
    );
    // And the blocker naming both shas reached the next round's prompt, not
    // just the slot's own record.
    let prompt_names_both = std::fs::read_dir(&run_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("prompt-"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .any(|p| p.contains(DEAD_SHA) && p.contains(&head));
    assert!(
        prompt_names_both,
        "a round prompt must carry the blocker naming both shas"
    );

    let approver = reviewers
        .iter()
        .find(|s| s["id"].as_str().is_some_and(|id| id.contains("review-0")))
        .expect("a review-0 slot");
    assert_eq!(
        approver["status"], "done",
        "a reviewer that writes fresh every round must keep passing: {approver}"
    );
    assert_eq!(
        approver["reviewed_commit"].as_str().unwrap_or(""),
        head,
        "the honest slot records which commit it judged: {approver}"
    );
}

#[test]
fn a_fresh_approve_naming_no_commit_is_not_counted() {
    let (_tmp, proj) = setup(FAKE_CLAUDE_ABSENT);
    let head = git(&proj, &["rev-parse", "HEAD"]);

    let run_id = only_run_id(&proj);
    let state = state_json(&proj, &run_id);
    let slots = state["slots"].as_array().expect("slots");
    let reviewers: Vec<&serde_json::Value> =
        slots.iter().filter(|s| s["role"] == "reviewer").collect();
    assert!(
        reviewers.len() >= 2,
        "guard is vacuous without a two-reviewer panel: {state}"
    );
    assert_ne!(
        state["phase"], "done",
        "a commit-less approve must never ship the run"
    );

    let silent = reviewers
        .iter()
        .find(|s| s["id"].as_str().is_some_and(|id| id.contains("review-1")))
        .expect("a review-1 slot");
    assert_eq!(
        silent["status"], "failed",
        "the commit-less verdict must fail, not pass: {silent}"
    );
    let err = silent["error"].as_str().unwrap_or("");
    assert!(
        err.contains("names no commit"),
        "the failure must name the absence distinctly from a mismatch: {silent}"
    );
    assert!(
        err.contains(&head),
        "the panel head must be visible alongside the absence: {silent}"
    );

    let approver = reviewers
        .iter()
        .find(|s| s["id"].as_str().is_some_and(|id| id.contains("review-0")))
        .expect("a review-0 slot");
    assert_eq!(
        approver["status"], "done",
        "a reviewer that writes fresh every round must keep passing: {approver}"
    );
}
