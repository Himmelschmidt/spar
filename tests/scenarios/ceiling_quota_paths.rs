//! Ceiling-meets-quota and backup-retry ceiling paths through the real loop
//! (real orchestrator, real spawned slots, no `--dry-run`). The unit tests pin
//! the helpers; these pin the `execute_loop` wiring: a ceiling kill whose log
//! also carries a quota signal must park at exit 4 instead of going to review
//! on a banked partial, and a backup implementer cut short at its ceiling must
//! bank a partial and continue to review like a primary dispatch.

mod common;
use std::path::Path;
use tempfile::tempdir;

fn spar_cmd() -> assert_cmd::Command {
    let mut c = common::spar();
    c.env_remove("SPAR_DRY_RUN");
    c
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Short ceiling so the test never waits on a real timeout: 2s soft, 4s kill.
fn tiny_ceiling(proj: &Path) {
    std::fs::write(
        proj.join("spar.toml"),
        "[timeouts]\nslot_secs = 2\nhard_ceiling_multiple = 2.0\n",
    )
    .unwrap();
}

fn write_fake(bin: &Path, name: &str, body: &str) {
    let p = bin.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn only_run_id(proj: &Path) -> String {
    std::fs::read_dir(proj.join(".spar/runs"))
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}

fn state_json(proj: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(run_id).join("state.json")).unwrap(),
    )
    .unwrap()
}

/// Prints the quota signal, then hangs past the 4s ceiling so the dispatch dies
/// by hard-ceiling kill with the signal already in its log.
const FAKE_QUOTA_THEN_HANG: &str = r#"if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
cat <<'SPAR_TEST_EOF'
! rate limit  seven_day  rejected
You've hit your weekly limit · resets 12am (America/New_York)
SPAR_TEST_EOF
sleep 30
"#;

/// Leaves an uncommitted file, then prints the quota signal and exits: the
/// primary fails environmental, so the backup seat takes the round carrying
/// the primary's leftovers.
const FAKE_QUOTA_FAIL_FAST: &str = r#"if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
printf 'primary work\n' > primary-work.txt
cat <<'SPAR_TEST_EOF'
! rate limit  seven_day  rejected
You've hit your weekly limit · resets 12am (America/New_York)
SPAR_TEST_EOF
exit 1
"#;

/// Commits work, then prints the quota signal and exits: the failover must
/// still measure the round from its start, not from the retry dispatch.
const FAKE_COMMIT_THEN_QUOTA: &str = r#"if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
printf 'primary work\n' > primary-work.txt
git add primary-work.txt
git -c user.email=test@example.com -c user.name=Test commit -qm "primary work"
cat <<'SPAR_TEST_EOF'
! rate limit  seven_day  rejected
You've hit your weekly limit · resets 12am (America/New_York)
SPAR_TEST_EOF
exit 1
"#;

/// Role-aware implementer/reviewer. The prompt arrives on stdin or argv
/// depending on the provider adapter, and some adapters never close stdin, so
/// the stdin read has a timeout. A reviewer prompt approves with the honest
/// head sha; an implementer prompt writes work plus its summary and hangs past
/// the ceiling so the kill banks a partial.
const FAKE_WORK_THEN_HANG: &str = r#"if [ "$1" = "--version" ]; then echo "fake 0"; exit 0; fi
# Seat env persists across `implement --run` re-entry, so cross-pass modes
# ride in the run dir next to the prompt file, never in the environment.
PROMPT_FILE=""
PROMPT_TEXT="$(timeout 2 cat 2>/dev/null || true)"
PREV=""
for a in "$@"; do
  if [ "$PREV" = "--prompt-file" ]; then PROMPT_FILE="$a"; fi
  if [ "$PREV" = "--prompt-file" ] && [ -f "$a" ]; then PROMPT_TEXT="$PROMPT_TEXT $(cat "$a")"; fi
  PREV="$a"
done
RUN_DIR="$(dirname "$PROMPT_FILE")"
# A seed planted before the run exists covers pass 1; per-round flags in the
# run dir cover later passes.
SPAR_DIR="$(dirname "$(dirname "$RUN_DIR")")"
set -- "$@" "$PROMPT_TEXT"
TOOL_LINE='{"type":"assistant","message":{"model":"fake","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read","input":{"file_path":"x"}}]}}'
DEST=""
CWD=""
SUMMARY=""
IS_REVIEWER=0
for a in "$@"; do
  case "$a" in
    *"Adversarial reviewer"*) IS_REVIEWER=1 ;;
  esac
  case "$a" in
    *"Write review to:"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write review to: \([^ ]*\).*/\1/p' | head -n 1)
      ;;
  esac
  case "$a" in
    *"Code under review (worktree):"*)
      CWD=$(printf '%s' "$a" | sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' | head -n 1)
      ;;
  esac
  DEST_ONE=$(printf '%s' "$a" | sed -n 's/.*Write a summary to `\([^`]*\)`.*/\1/p' | head -n 1)
  [ -n "$DEST_ONE" ] && SUMMARY="$DEST_ONE"
done
if [ "$IS_REVIEWER" = 1 ]; then
  if [ -f "$RUN_DIR/fail-reviewers" ] || [ -f "$SPAR_DIR/seed-fail-reviewers" ]; then
    echo "$TOOL_LINE"
    printf 'review unavailable\n! rate limit  seven_day  rejected\n'
    exit 1
  fi
  SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || true)
  { printf '## Verdict\napprove\n'; [ -n "$SHA" ] && printf 'Reviewed-Commit: %s\n' "$SHA"; } > "$DEST"
  echo "$TOOL_LINE"
  exit 0
fi
# No summary: the ceiling kill must land before the artifact exists, or the
# kill reads as kept work (done) instead of a banked partial.
if [ ! -f "$RUN_DIR/idle-impl" ] && [ -z "$SPAR_TEST_NO_WORK" ]; then printf 'slot work\n' > work.txt; fi
if [ -n "$SPAR_TEST_WRITE_SUMMARY" ] && [ -n "$SUMMARY" ]; then
  printf '# Summary\nDid the stub thing.\n' > "$SUMMARY"
fi
echo "$TOOL_LINE"
sleep 30
"#;

#[test]
fn ceiling_kill_with_a_quota_signal_parks_at_4_and_banks_nothing() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);
    tiny_ceiling(&proj);
    write_fake(&bin, "claude", FAKE_QUOTA_THEN_HANG);
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "ceiling plus quota",
            "--providers",
            "cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state = state_json(&proj, &run_id);
    assert_eq!(state["phase"], "quota", "a quota hit parks: {state}");
    assert_eq!(
        state["ceiling_partial_rounds"].as_array().map(Vec::len),
        Some(0),
        "quota wins over the ceiling: no partial is banked for review"
    );
}

#[test]
fn backup_retry_ceiling_kill_banks_a_partial_and_goes_to_review() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);
    tiny_ceiling(&proj);
    // The primary fails environmental (quota), so the backup seat takes the
    // round; the reviewer stays on the healthy provider. The backup's ceiling
    // kill must bank a partial like a primary's.
    write_fake(&bin, "claude", FAKE_QUOTA_FAIL_FAST);
    write_fake(&bin, "grok", FAKE_WORK_THEN_HANG);
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        .args([
            "implement",
            "-t",
            "backup ceiling",
            "--role",
            "implementer=cli:claude@opus",
            "--backup",
            "implementer=cli:grok@backup",
            "--role",
            "reviewer=cli:grok",
            "--role",
            "reviewer=cli:grok",
            "--without",
            "suite",
        ])
        .output()
        .expect("implement run");

    let run_id = only_run_id(&proj);
    let state = state_json(&proj, &run_id);
    let partials = state["ceiling_partial_rounds"]
        .as_array()
        .expect("partial rounds");
    assert_eq!(
        partials.len(),
        1,
        "the backup kill banks one partial: {state}"
    );
    assert_eq!(partials[0]["round"], 1);
    assert!(
        state["slots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "cli-backup"),
        "the round ran on the backup seat: {state}"
    );
    let banked = partials[0]["commit"].as_str().expect("banked commit");
    // Both seats' leftovers land in the one banked commit: the failover keeps
    // the primary's uncommitted file in the same worktree.
    let wt = state["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["slot_id"] == "impl")
        .expect("impl worktree");
    let files = git_out(
        Path::new(wt["path"].as_str().unwrap()),
        &["show", "--name-only", "--format=", banked],
    );
    assert!(
        files.contains("primary-work.txt") && files.contains("work.txt"),
        "the bank holds both seats' work: {files}"
    );
    let reviews = review_text(&proj, &run_id);
    assert!(
        reviews.contains("approve") && reviews.contains(banked),
        "review approved the banked partial commit: {state}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "review of the backup partial parks for a human: {state}"
    );
}

/// A backup retry measures the round from its start (O189): the primary
/// commits work, then fails quota, and the backup delivers its summary before
/// the ceiling kill, so the round is kept as done on the primary's commit.
/// The retry must not re-stamp the round-start snapshot at failover, or the
/// primary's commit would read as the baseline instead of new work.
#[test]
fn backup_retry_measures_work_against_the_round_start() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);
    tiny_ceiling(&proj);
    write_fake(&bin, "claude", FAKE_COMMIT_THEN_QUOTA);
    write_fake(&bin, "grok", FAKE_WORK_THEN_HANG);
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        // The backup delivers its summary (kept as done) without adding its
        // own work file, isolating the primary's commit as the round's work.
        .env("SPAR_TEST_NO_WORK", "1")
        .env("SPAR_TEST_WRITE_SUMMARY", "1")
        .args([
            "implement",
            "-t",
            "backup round start",
            "--role",
            "implementer=cli:claude@opus",
            "--backup",
            "implementer=cli:grok@backup",
            "--role",
            "reviewer=cli:grok",
            "--role",
            "reviewer=cli:grok",
            "--without",
            "suite",
        ])
        .output()
        .expect("implement run");

    let run_id = only_run_id(&proj);
    let state = state_json(&proj, &run_id);
    let head = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "impl")
        .expect("impl slot")["dispatch_head"]
        .as_str()
        .expect("dispatch head");
    assert_eq!(
        head,
        state["base_commit"].as_str().unwrap(),
        "the retry keeps the round-start snapshot: {state}"
    );
    assert!(
        state["slots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "cli-backup"),
        "the round ran on the backup seat: {state}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "the kept round goes to review like a finished one: {state}"
    );
}

/// A resume that hits the ceiling a second time without new edits continues on
/// the banked partial instead of failing the run: pass one banks the
/// implementer's work and then parks on its reviewers' quota; re-entering the
/// parked run opens a new round, the idle implementer dies at the ceiling with
/// the previous round's bank still carried, and the round goes to review on
/// that commit like the first hit.
#[test]
fn resume_after_a_ceiling_kill_continues_on_the_banked_partial() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);
    tiny_ceiling(&proj);
    write_fake(&bin, "grok", FAKE_WORK_THEN_HANG);
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // Pass 1 reviewers fail on quota. The run dir does not exist yet, so the
    // fake also honors a seed planted at .spar/seed-fail-reviewers.
    std::fs::create_dir_all(proj.join(".spar")).unwrap();
    std::fs::write(proj.join(".spar/seed-fail-reviewers"), "").unwrap();
    let first = spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        // Bank work, then park the round on reviewer quota.
        .args([
            "implement",
            "-t",
            "same round resume",
            "--role",
            "implementer=cli:grok",
            "--role",
            "reviewer=cli:grok",
            "--role",
            "reviewer=cli:grok",
            "--without",
            "suite",
        ])
        .output()
        .expect("first implement run");

    let run_id = only_run_id(&proj);
    let state = state_json(&proj, &run_id);
    assert_eq!(first.status.code(), Some(4), "quota parks the run: {state}");
    let banked = state["ceiling_partial_rounds"]
        .as_array()
        .expect("partial rounds")
        .first()
        .expect("the first hit banks a partial")
        .clone();
    assert_eq!(banked["round"], 1);
    let banked_commit = banked["commit"].as_str().expect("banked commit");

    // Quota pauses clear with the store: the re-entry must see usable seats.
    let _ = std::fs::remove_file(proj.join(".spar/quota.json"));

    // Flip the modes: reviewers approve now, the implementer idles (no new
    // edits) and dies at the ceiling with the bank still carried.
    let run_dir = proj.join(".spar/runs").join(&run_id);
    let _ = std::fs::remove_file(proj.join(".spar/seed-fail-reviewers"));
    let _ = std::fs::remove_file(run_dir.join("fail-reviewers"));
    std::fs::write(run_dir.join("idle-impl"), "").unwrap();
    let second = spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        .args(["implement", "--run", &run_id])
        .output()
        .expect("resume implement run");

    let state = state_json(&proj, &run_id);
    assert_eq!(state["round"], 2, "the resume opens a new round: {state}");
    let reviews = review_text(&proj, &run_id);
    assert!(
        reviews.contains("approve") && reviews.contains(banked_commit),
        "the second hit reviews the carried banked commit: {state}"
    );
    assert_eq!(
        second.status.code(),
        Some(2),
        "the resumed round parks for a human instead of failing: {state}"
    );
}

fn review_text(proj: &Path, run_id: &str) -> String {
    let artifacts = proj.join(".spar/runs").join(run_id).join("artifacts");
    let mut out = String::new();
    if let Ok(entries) = std::fs::read_dir(&artifacts) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("review-") && name.ends_with(".md") {
                out.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
    }
    out
}
