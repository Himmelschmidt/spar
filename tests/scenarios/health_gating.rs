//! Health gating at seat resolution (issue #232 follow-up).
//!
//! Health is checked once, where a seat's provider is resolved and its source
//! is known: non-pin sources skip unhealthy providers and fail with a
//! readiness error (exit 1, never exit 4), while explicit pins warn and still
//! dispatch. Fake CLIs fail their login probe but run dispatches fine, so a
//! skipped provider is one that was never dispatched.

mod common;

use predicates::prelude::predicate;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn spar_cmd() -> assert_cmd::Command {
    let mut cmd = common::spar();
    cmd.env_remove("SPAR_PROJECT_ROOT");
    cmd.env_remove("SPAR_RUN_ID");
    cmd.env_remove("SPAR_AGENT_ID");
    cmd
}

fn init_repo(dir: &Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
}

fn spar_home_dir() -> std::path::PathBuf {
    let d = tempdir().unwrap();
    let p = d.path().to_path_buf();
    std::mem::forget(d);
    p
}

fn read_state(proj: &Path, run_id: &str) -> serde_json::Value {
    let p = proj.join(format!(".spar/runs/{run_id}/state.json"));
    let s = fs::read_to_string(&p).expect("state.json");
    serde_json::from_str(&s).unwrap()
}

fn run_id_from_state_dir(proj: &Path) -> String {
    fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run should have been created")
}

/// Dispatch body every fake shares: count the call, write the artifact the
/// slot's reader expects (reconstructed from `SPAR_AGENT_ID`, which is
/// `run:slot`), and print one tool-call line so the slot counts as having
/// run. Reviewers write the verdict the test wants (`approve` lets the run
/// finish, `request_changes` drives the fix loop); implementers always write
/// a summary.
const SUCCESS_DISPATCH: &str = r#"echo "$FAKE_NAME $SPAR_AGENT_ID" >> "$FAKE_COUNT_DIR/calls.count"
cat > /dev/null
echo '{"type":"assistant","message":{"model":"fake","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read","input":{"file_path":"x"}}]}}'
run="${SPAR_RUN_ID:-${SPAR_AGENT_ID%%:*}}"
slot="${SPAR_SLOT:-${SPAR_AGENT_ID#*:}}"
art="$FAKE_PROJECT_ROOT/.spar/runs/$run/artifacts"
mkdir -p "$art"
SHA=$(git -C "$FAKE_PROJECT_ROOT" rev-parse HEAD 2>/dev/null || true)
case "$slot" in
impl)
if [ -n "$FAKE_PROGRESS" ]; then date +%s%N >> progress.txt; fi
printf '# Summary\nDid the stub thing.\n' > "$art/summary-impl.md"
;;
review-*)
case "$FAKE_VERDICT" in
approve)
{ printf '## Verdict\napprove\n'; if [ -n "$SHA" ]; then printf 'Reviewed-Commit: %s\n' "$SHA"; fi; } > "$art/review-$slot.md"
;;
request_changes)
{ printf '## Verdict\nrequest_changes\n'; if [ -n "$SHA" ]; then printf 'Reviewed-Commit: %s\n' "$SHA"; fi; printf '\n## Findings\n- severity: major - stub defect\n'; } > "$art/review-$slot.md"
;;
esac
;;
esac
exit 0"#;

/// A fake CLI: `--version` keeps detection fast, `$token` is the readiness
/// probe (`auth` for claude, `login` for codex, a token that never matches
/// for grok, which has no probe and counts as healthy when present).
/// `$verdict` is the review verdict this fake writes (`approve` or
/// `request_changes`); implementer summaries are always written. `FAKE_PROGRESS`
/// makes the implementer touch its worktree so the no-progress park (O178) does
/// not end a run that must reach the stuck exit or a later panel.
fn write_fake(bin: &Path, name: &str, token: &str, probe: &str, verdict: &str) {
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fake-{name}; exit 0; fi\nif [ \"$1\" = \"{token}\" ]; then\n{probe}\nfi\nexport FAKE_NAME={name} FAKE_VERDICT={verdict}\nif [ \"$1\" = \"-p\" ] || [ \"$1\" = \"exec\" ]; then\n{dispatch}\nfi\ncase \" $* \" in\n*\" --prompt-file \"*)\n{dispatch}\n;;\nesac\nexit 1\n",
        dispatch = SUCCESS_DISPATCH,
    );
    let p = bin.join(name);
    fs::write(&p, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn failing_probe() -> &'static str {
    "echo \"Error: credentials missing\" >&2; exit 1"
}

fn calls(bin: &Path, provider: &str) -> usize {
    let p = bin.join("calls.count");
    let s = fs::read_to_string(&p).unwrap_or_default();
    s.lines()
        .filter(|l| l.starts_with(&format!("{provider} ")))
        .count()
}

fn implementer_of(state: &serde_json::Value) -> &serde_json::Value {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot")
}

fn reviewers_of(state: &serde_json::Value) -> Vec<&serde_json::Value> {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == "reviewer")
        .collect()
}

/// The quota-park wording an unhealthy non-pin seat must never produce.
fn assert_no_quota_park(combined: &str) {
    assert!(
        !combined.contains("paused or on cooldown"),
        "quota wording leaked into a readiness failure: {combined}"
    );
    assert!(
        !combined.contains("provider resume"),
        "quota wording leaked into a readiness failure: {combined}"
    );
}

struct Project {
    proj: std::path::PathBuf,
    bin: std::path::PathBuf,
    home: std::path::PathBuf,
    path_env: String,
}

fn project(toml: &str) -> (tempfile::TempDir, Project) {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    fs::write(proj.join("spar.toml"), toml).unwrap();
    let home = spar_home_dir();
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (
        tmp,
        Project {
            proj,
            bin,
            home,
            path_env,
        },
    )
}

/// A `[roles]` file seat on an unhealthy provider falls through to the first
/// healthy `[providers].order` entry instead of parking the run in quota.
#[test]
fn file_seat_skips_unhealthy_provider_for_healthy_order_entry() {
    let (_tmp, p) = project(
        "[providers]\norder = [\"cli:codex\", \"cli:claude\", \"cli:grok\"]\n[roles]\nimplementer = \"cli:codex\"\nreviewer = [\"cli:claude\"]\n",
    );
    write_fake(&p.bin, "claude", "auth", "exit 0", "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "sick file seat",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]))
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_quota_park(&combined);
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:claude", "{state}");
    assert_eq!(calls(&p.bin, "codex"), 0, "sick codex must never dispatch");
}

/// Nothing healthy anywhere is a readiness failure (exit 1 naming the probe),
/// never a quota park (exit 4).
#[test]
fn all_unhealthy_order_fails_readiness_never_quota() {
    let (_tmp, p) = project(
        "[providers]\norder = [\"cli:codex\"]\n[roles]\nimplementer = \"cli:codex\"\nreviewer = []\n",
    );
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "nothing healthy",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("no healthy provider"),
        "readiness error names the failure: {combined}"
    );
    assert!(
        combined.contains("cli:codex: probe `codex login status` failed"),
        "names the failed probe: {combined}"
    );
    assert!(
        combined.contains("probe `codex login status` failed"),
        "names the failed probe: {combined}"
    );
    assert_no_quota_park(&combined);
    assert_eq!(calls(&p.bin, "codex"), 0, "sick codex must never dispatch");
}

/// An unrelated `[backups]` entry must not turn a readiness skip into a quota
/// park: the backup is for another role, so the order fallback still seats.
#[test]
fn unrelated_backup_entry_does_not_quota_park() {
    let (_tmp, p) = project(
        "[providers]\norder = [\"cli:codex\", \"cli:claude\", \"cli:grok\"]\n[roles]\nimplementer = \"cli:codex\"\nreviewer = [\"cli:claude\"]\n[backups]\nplanner = \"cli:pi\"\n",
    );
    write_fake(&p.bin, "claude", "auth", "exit 0", "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "unrelated backup",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]))
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_quota_park(&combined);
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:claude", "{state}");
    assert_eq!(calls(&p.bin, "codex"), 0, "sick codex must never dispatch");
}

/// The stuck ladder rotates off the pinned sick implementer, passes over the
/// sick order entry, and lands on the healthy one.
#[test]
fn stuck_rotation_passes_over_unhealthy_order_entry() {
    let (_tmp, p) = project("[providers]\norder = [\"cli:claude\", \"cli:codex\", \"cli:grok\"]\n");
    write_fake(&p.bin, "claude", "auth", failing_probe(), "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "request_changes");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .env("FAKE_PROGRESS", "1")
        .args([
            "implement",
            "-t",
            "rotation skips sick",
            "--providers",
            "cli:claude,cli:grok",
            "--max-rounds",
            "20",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(3)
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("explicitly pinned provider cli:claude is unhealthy"),
        "pin warns instead of failing: {combined}"
    );
    assert_no_quota_park(&combined);
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:grok", "{state}");
    assert_eq!(
        calls(&p.bin, "codex"),
        0,
        "rotation must pass over sick codex"
    );
}

/// Reviewer widening draws from the order but never adds a sick entry; with
/// nothing healthy left it duplicates a dispatched reviewer instead.
#[test]
fn widening_never_adds_unhealthy_order_entry() {
    let (_tmp, p) = project(
        "[providers]\norder = [\"cli:claude\", \"cli:codex\", \"cli:grok\"]\n[roles]\nimplementer = \"cli:grok\"\nreviewer = [\"cli:grok\", \"cli:grok\"]\n",
    );
    write_fake(&p.bin, "claude", "auth", failing_probe(), "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "request_changes");
    spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .env("FAKE_PROGRESS", "1")
        .args([
            "implement",
            "-t",
            "widening skips sick",
            "--max-rounds",
            "20",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(3);
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:grok", "{state}");
    for r in reviewers_of(&state) {
        assert_eq!(
            r["provider"], "cli:grok",
            "widening added a sick reviewer: {state}"
        );
    }
    assert_eq!(calls(&p.bin, "codex"), 0, "sick codex must never dispatch");
    assert_eq!(
        calls(&p.bin, "claude"),
        0,
        "sick claude must never dispatch"
    );
}

/// `--validate-only` projects the same implementer a real launch seats.
#[test]
fn validate_only_projects_the_panel_launch_seats() {
    let (_tmp, p) = project(
        "[providers]\norder = [\"cli:codex\", \"cli:claude\", \"cli:grok\"]\n[roles]\nimplementer = \"cli:codex\"\nreviewer = [\"cli:claude\"]\n",
    );
    write_fake(&p.bin, "claude", "auth", "exit 0", "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "parity",
            "--without",
            "suite",
            "--validate-only",
            "--json",
        ])
        .assert()
        .code(0)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("cli:claude"),
        "validate-only projects claude: {stdout}"
    );
    assert_eq!(calls(&p.bin, "codex"), 0, "validate-only must not dispatch");
    spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args(["implement", "-t", "parity", "--without", "suite", "--json"])
        .assert()
        .code(predicate::in_iter([0, 2]));
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:claude", "{state}");
}

/// An explicitly pinned sick provider warns and still dispatches: the pin is
/// the operator's word, not the gate's.
#[test]
fn pinned_unhealthy_provider_warns_and_still_dispatches() {
    let (_tmp, p) = project("[providers]\norder = [\"cli:codex\", \"cli:claude\"]\n");
    write_fake(&p.bin, "claude", "auth", "exit 0", "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "pin warns",
            "--providers",
            "cli:codex,cli:claude",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]))
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("explicitly pinned provider cli:codex is unhealthy"),
        "pin warns: {combined}"
    );
    assert_no_quota_park(&combined);
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:codex", "{state}");
    assert!(calls(&p.bin, "codex") >= 1, "pinned codex still dispatches");
}

#[test]
fn singleton_past_a_sick_order_head_keeps_its_pool_position() {
    let (_tmp, p) = project("[providers]\norder = [\"cli:claude\", \"cli:grok\", \"cli:codex\"]\n");
    write_fake(&p.bin, "claude", "auth", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "approve");
    write_fake(&p.bin, "codex", "login", "exit 0", "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "plan",
            "-t",
            "singleton keeps pool position",
            "--role",
            "planner=cli:grok@model",
            "--validate-only",
            "--json",
        ])
        .assert()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "validate-only must project the later singleton past the sick order head: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("cli:codex"),
        "projected panel seats test-author on cli:codex: {stdout}"
    );
    assert_eq!(calls(&p.bin, "codex"), 0, "validate-only must not dispatch");
}

#[test]
fn unrelated_backups_entry_does_not_park_an_unhealthy_seat_at_quota() {
    let (_tmp, p) = project(
        "[providers]\norder = [\"cli:claude\", \"cli:codex\", \"cli:grok\"]\n[roles]\nimplementer = \"cli:codex\"\n[backups]\nreviewer = [\"cli:grok@backup\"]\n",
    );
    write_fake(&p.bin, "claude", "auth", "exit 0", "approve");
    write_fake(&p.bin, "codex", "login", failing_probe(), "approve");
    write_fake(&p.bin, "grok", "grok", "exit 0", "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "unrelated backup",
            "--without",
            "suite",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]))
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_quota_park(&combined);
    let run_id = run_id_from_state_dir(&p.proj);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(implementer_of(&state)["provider"], "cli:claude", "{state}");
    assert_eq!(calls(&p.bin, "codex"), 0, "sick file seat never dispatches");
}

/// Continuing a run re-checks auto-selected seats through the same point as
/// launch: an implementer whose provider went sick between rounds is re-seated
/// onto the healthy order entry, never redispatched.
#[test]
fn continued_run_reseats_an_implementer_whose_provider_went_sick() {
    let (_tmp, p) = project("[providers]\norder = [\"cli:codex\", \"cli:claude\"]\n[roles]\nimplementer = \"cli:codex\"\nreviewer = [\"cli:claude\"]\n");
    write_fake(&p.bin, "claude", "auth", "exit 0", "request_changes");
    write_fake(
        &p.bin,
        "codex",
        "login",
        "if [ -f \"$FAKE_COUNT_DIR/sick\" ]; then echo \"Error: credentials missing\" >&2; exit 1; fi\nexit 0",
        "approve",
    );
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "-t",
            "continuation reseats sick",
            "--without",
            "suite",
            "--max-rounds",
            "1",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([2, 3]))
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let run_id: String = serde_json::from_str::<serde_json::Value>(&stdout).expect("run json")
        ["id"]
        .as_str()
        .expect("run id")
        .to_string();
    let state = read_state(&p.proj, &run_id);
    assert_eq!(
        implementer_of(&state)["provider"],
        "cli:codex",
        "round 1 seats the healthy order head: {state}"
    );
    let codex_round_1 = calls(&p.bin, "codex");
    assert!(codex_round_1 >= 1, "round 1 dispatches codex");
    let claude_round_1 = calls(&p.bin, "claude");

    fs::write(p.bin.join("sick"), "codex went sick between rounds").unwrap();

    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .args([
            "implement",
            "--run",
            &run_id,
            "--max-rounds",
            "+2",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2, 3]))
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_quota_park(&combined);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(
        implementer_of(&state)["provider"],
        "cli:claude",
        "continuation re-seats the sick implementer: {state}"
    );
    assert_eq!(
        calls(&p.bin, "codex"),
        codex_round_1,
        "sick codex is never redispatched"
    );
    assert!(
        calls(&p.bin, "claude") > claude_round_1,
        "the fix round dispatches claude"
    );
}

/// Continuing a run re-checks reviewer seats through the same point as launch:
/// a reviewer whose provider went sick between rounds is re-seated onto the
/// healthy order entry no sibling seat holds (the implementer keeps cli:claude),
/// never redispatched.
#[test]
fn continued_run_reseats_a_reviewer_whose_provider_went_sick() {
    let (_tmp, p) = project("[providers]\norder = [\"cli:claude\", \"cli:codex\", \"cli:grok\"]\n[roles]\nimplementer = \"cli:claude\"\nreviewer = [\"cli:codex\"]\n");
    write_fake(&p.bin, "claude", "auth", "exit 0", "approve");
    write_fake(
        &p.bin,
        "codex",
        "login",
        "if [ -f \"$FAKE_COUNT_DIR/sick\" ]; then echo \"Error: credentials missing\" >&2; exit 1; fi\nexit 0",
        "request_changes",
    );
    write_fake(&p.bin, "grok", "grok", "exit 0", "approve");
    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .env("FAKE_PROGRESS", "1")
        .args([
            "implement",
            "-t",
            "continuation reseats sick reviewer",
            "--without",
            "suite",
            "--max-rounds",
            "1",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([2, 3]))
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let run_id: String = serde_json::from_str::<serde_json::Value>(&stdout).expect("run json")
        ["id"]
        .as_str()
        .expect("run id")
        .to_string();
    let state = read_state(&p.proj, &run_id);
    assert_eq!(
        reviewers_of(&state)[0]["provider"],
        "cli:codex",
        "round 1 seats the reviewer on the healthy order entry: {state}"
    );
    let codex_round_1 = calls(&p.bin, "codex");
    assert!(codex_round_1 >= 1, "round 1 dispatches codex");

    fs::write(p.bin.join("sick"), "codex went sick between rounds").unwrap();

    let out = spar_cmd()
        .current_dir(&p.proj)
        .env("SPAR_HOME", &p.home)
        .env("PATH", &p.path_env)
        .env("FAKE_PROJECT_ROOT", &p.proj)
        .env("FAKE_COUNT_DIR", &p.bin)
        .env("FAKE_PROGRESS", "1")
        .args([
            "implement",
            "--run",
            &run_id,
            "--max-rounds",
            "+2",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2, 3]))
        .get_output()
        .clone();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_quota_park(&combined);
    let state = read_state(&p.proj, &run_id);
    assert_eq!(
        reviewers_of(&state)[0]["provider"],
        "cli:grok",
        "continuation re-seats the sick reviewer: {state}"
    );
    assert_eq!(
        calls(&p.bin, "codex"),
        codex_round_1,
        "sick codex is never redispatched"
    );
    assert!(
        reviewers_of(&state)[0]["id"] == "review-0-cli-grok",
        "the reviewer slot is renamed for its new provider: {state}"
    );
}
