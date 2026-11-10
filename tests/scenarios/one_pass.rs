//! `--one-pass` (O196): one small round, then ship. Clean gate ships; a failed
//! gate ships anyway when no open finding reaches `[review] block_on`, with the
//! findings recorded as follow-ups; a blocking finding parks at exit 2.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

/// Per-test-process SPAR_HOME so the suite never writes the developer's real
/// `~/.spar/registry.json`. Shared across spawns in this binary.
fn spar_home_dir() -> PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

fn spar_cmd() -> assert_cmd::Command {
    let mut c = common::spar();
    c.env("SPAR_HOME", spar_home_dir());
    // spar exports these into every slot (providers/presence.rs), so when the suite runs
    // *inside* a spar worktree the child would resolve the primary checkout instead of
    // this test's temp project and write real runs into it. Clear them per-Command
    // (never via process env — these binaries run tests in parallel).
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c.env_remove("SPAR_FORCE_AC_STATUS");
    c.env_remove("SPAR_FORCE_REVIEW_FINDING");
    c
}

fn init_git_repo(dir: &Path) {
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .unwrap();
    }
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir)
        .status()
        .unwrap();
}

fn run_dir(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(".spar/runs").join(run_id)
}

fn state_json(dir: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(run_dir(dir, run_id).join("state.json")).unwrap())
        .unwrap()
}

fn status_json(dir: &Path, run_id: &str) -> serde_json::Value {
    let out = spar_cmd()
        .current_dir(dir)
        .args(["status", run_id, "--json"])
        .assert()
        .success();
    serde_json::from_slice(out.get_output().stdout.as_slice()).unwrap()
}

fn config_json(dir: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(run_dir(dir, run_id).join("config.json")).unwrap(),
    )
    .unwrap()
}

fn only_run_id(dir: &Path) -> String {
    let mut ids: Vec<String> = std::fs::read_dir(dir.join(".spar/runs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), 1, "exactly one run: {ids:?}");
    ids.pop().unwrap()
}

fn reviewer_seats(dir: &Path, run_id: &str) -> usize {
    state_json(dir, run_id)["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == "reviewer")
        .count()
}

fn plan_and_approve(dir: &Path) -> String {
    let plan = spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("plan json");
    let run_id = v["run_id"].as_str().expect("run_id").to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

/// A passed gate ships: exit 0, done, no override recorded, and the run config
/// freezes the mode with a one-reviewer panel and a one-round ceiling.
#[test]
fn one_pass_clean_review_ships() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
            "--one-pass",
        ])
        .assert()
        .code(0);
    let run_id = only_run_id(tmp.path());
    assert_eq!(status_json(tmp.path(), &run_id)["phase"], "done");
    let cfg = config_json(tmp.path(), &run_id);
    assert_eq!(cfg["one_pass"], true);
    assert_eq!(state_json(tmp.path(), &run_id)["max_rounds"], 1);
    assert_eq!(reviewer_seats(tmp.path(), &run_id), 1);
    let st = state_json(tmp.path(), &run_id);
    assert!(
        st["ship_override"].is_null(),
        "clean ship takes no override"
    );
    let ship = std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/ship.md"))
        .expect("dry-run ship writes ship.md");
    assert!(
        !ship.contains("Shipped over open findings"),
        "no follow-ups on a clean ship"
    );
}

/// A failed gate with only sub-threshold findings ships anyway, with the
/// findings recorded as follow-ups through the override machinery. The missing
/// acceptance criterion (SPAR_FORCE_AC_STATUS=omit on a planned run) fails the
/// gate while the forced minor finding stays below the default `block_on`.
#[test]
fn one_pass_minor_findings_ship_as_follow_ups() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_AC_STATUS", "omit")
        .env("SPAR_FORCE_REVIEW_FINDING", "minor")
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
            "--one-pass",
        ])
        .assert()
        .code(0);
    assert_eq!(status_json(tmp.path(), &run_id)["phase"], "done");
    assert_eq!(config_json(tmp.path(), &run_id)["one_pass"], true);
    assert_eq!(reviewer_seats(tmp.path(), &run_id), 1);
    let st = state_json(tmp.path(), &run_id);
    let reason = st["ship_override"]["reason"]
        .as_str()
        .expect("override reason");
    assert!(reason.contains("one-pass"), "{reason}");
    let ship = std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/ship.md"))
        .expect("dry-run ship writes ship.md");
    assert!(
        ship.contains("Shipped over open findings"),
        "follow-ups ride along in the PR body"
    );
    assert!(
        ship.contains("src/lib.rs"),
        "finding location recorded: {ship}"
    );
    assert!(ship.contains("minor"), "finding severity recorded");
}

/// A finding at `block_on` parks at the existing gate (exit 2), as without
/// the flag: no ship, no override.
#[test]
fn one_pass_blocking_finding_parks() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REVIEW_FINDING", "major")
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
            "--one-pass",
        ])
        .assert()
        .code(2);
    let run_id = only_run_id(tmp.path());
    assert_eq!(
        status_json(tmp.path(), &run_id)["phase"],
        "awaiting_round_extension"
    );
    let st = state_json(tmp.path(), &run_id);
    assert!(
        st["ship_override"].is_null(),
        "parked runs take no override"
    );
    assert!(
        !run_dir(tmp.path(), &run_id)
            .join("artifacts/ship.md")
            .exists(),
        "parked runs ship nothing"
    );
}

/// An explicit `--fleet` still wins over the implied small preset: two
/// reviewers, and the clean gate still ships.
#[test]
fn one_pass_explicit_fleet_wins() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok,cli:codex",
            "--fleet",
            "standard",
            "--dry-run",
            "--json",
            "--one-pass",
        ])
        .assert()
        .code(0);
    let run_id = only_run_id(tmp.path());
    assert_eq!(status_json(tmp.path(), &run_id)["phase"], "done");
    assert_eq!(config_json(tmp.path(), &run_id)["one_pass"], true);
    assert_eq!(reviewer_seats(tmp.path(), &run_id), 2);
}
