//! `--one-pass` (O196): one small round, then the verdict. A passed gate ships;
//! a sub-threshold `request_changes` counts as approve-with-notes and ships
//! clean with the findings as follow-ups in the PR body. Anything that fails
//! the gate parks at it (exit 2), as without the flag.

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
/// freezes the mode with a one-reviewer panel and a one-round ceiling. The
/// entry prints exactly one JSON document: the ship reports itself and the
/// loop does not emit a second one.
#[test]
fn one_pass_clean_review_ships() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let out = spar_cmd()
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
        .code(0)
        .get_output()
        .stdout
        .clone();
    // `from_slice` rejects trailing bytes, so a second document fails here.
    let v: serde_json::Value =
        serde_json::from_slice(&out).expect("implement --json prints one JSON document");
    assert_eq!(v["phase"], "done");
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

/// A real sub-threshold-findings case: the forced minor finding carries
/// `request_changes` below the default `block_on` while every acceptance
/// criterion passes, so the gate counts it as approve-with-notes and passes.
/// The run ships clean with no override. Runs on a planned run to cover the
/// bound (`--run`) ceiling too.
#[test]
fn one_pass_minor_notes_ship_clean_on_bound_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
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
    assert!(
        st["ship_override"].is_null(),
        "approve-with-notes takes no override"
    );
    let ship = std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/ship.md"))
        .expect("dry-run ship writes ship.md");
    assert!(
        ship.contains("## Open findings (non-blocking)"),
        "the minor findings ride along as follow-ups in the PR body:\n{ship}"
    );
    assert!(
        ship.contains("dry-run forced finding"),
        "the minor finding text is in the PR body:\n{ship}"
    );
    assert!(
        !ship.contains("## Gate override"),
        "a passed gate ships clean, never through the override machinery:\n{ship}"
    );
}

/// A bound `--run --one-pass` on a never-dispatched reused round gets exactly
/// that one dispatch: with a blocking finding the gate fails and the run parks
/// at the ceiling gate (exit 2) without opening another round. Resetting
/// `dispatched_round` reproduces the crash window the reuse path exists for (a
/// round opened with nothing spawned in it).
#[test]
fn one_pass_reused_round_blocking_parks_after_one_dispatch() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let state_path = run_dir(tmp.path(), &run_id).join("state.json");
    let mut st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let entry_round = st["round"].as_u64().unwrap();
    st["dispatched_round"] = serde_json::json!(entry_round - 1);
    std::fs::write(&state_path, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REVIEW_FINDING", "major")
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
        .code(2);
    assert_eq!(
        status_json(tmp.path(), &run_id)["phase"],
        "awaiting_round_extension"
    );
    assert_eq!(
        state_json(tmp.path(), &run_id)["round"].as_u64().unwrap(),
        entry_round,
        "a reused round is the one dispatch; the failed gate must not open another"
    );
    assert!(
        !run_dir(tmp.path(), &run_id)
            .join("artifacts/ship.md")
            .exists(),
        "parked runs ship nothing"
    );
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

/// A gate failed on acceptance alone parks: the review approves, but the
/// omitted criterion trips the fail-closed acceptance check. Exit 2, no ship,
/// no override. Needs a planned run:
/// the small fleet has no TestAuthor seat, so a fresh run's contract is empty
/// and there is nothing to omit.
#[test]
fn one_pass_missing_acceptance_parks() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_AC_STATUS", "omit")
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

/// `spar run --workflow loop --one-pass` shares the loop launch: a clean gate
/// ships, the small preset is implied (one reviewer), and the mode freezes.
#[test]
fn one_pass_run_loop_ships_clean() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--workflow",
            "loop",
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
    assert_eq!(config_json(tmp.path(), &run_id)["one_pass"], true);
    assert_eq!(reviewer_seats(tmp.path(), &run_id), 1);
}

/// `--one-pass` is loop-only: any other workflow refuses before minting a run.
#[test]
fn one_pass_run_non_loop_refused() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
            "--one-pass",
        ])
        .assert()
        .code(1);
    assert!(
        std::fs::read_dir(tmp.path().join(".spar/runs"))
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "a refused launch mints no run"
    );
}

/// A parked one-pass run ships once the operator buys a round and the gate
/// passes: the clean re-review clears the ship confirm, and the frozen
/// `one_pass` config (no flag re-passed) still routes the real ship.
#[test]
fn one_pass_bought_round_ships_after_park() {
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
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--max-rounds",
            "2",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(0);
    assert_eq!(status_json(tmp.path(), &run_id)["phase"], "done");
    assert_eq!(config_json(tmp.path(), &run_id)["one_pass"], true);
    assert!(
        run_dir(tmp.path(), &run_id)
            .join("artifacts/ship.md")
            .exists(),
        "the bought round ships"
    );
}

/// The daemon entry (`__internal_continue`) re-runs the verdict on a parked
/// one-pass run and keeps the park while the gate still fails: still exit 2,
/// still at the gate, nothing shipped. (The daemon itself never continues a
/// run at a gate; this covers the shared `continue_run` verdict directly.)
#[test]
fn one_pass_daemon_continue_keeps_park() {
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
    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REVIEW_FINDING", "major")
        .args(["__internal_continue", &run_id])
        .assert()
        .code(2);
    assert_eq!(
        status_json(tmp.path(), &run_id)["phase"],
        "awaiting_round_extension"
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
