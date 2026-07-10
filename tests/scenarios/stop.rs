//! `spar stop`: halt dispatch, keep the worktree/branch, stay resumable.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;

fn init_git_repo(dir: &std::path::Path) {
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

fn primary_branch(dir: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn plan_and_approve(dir: &std::path::Path) -> String {
    let plan = cargo_bin_cmd!("spar")
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "add a hello world module",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&plan).unwrap();
    let run_id = v["run_id"].as_str().unwrap().to_string();
    cargo_bin_cmd!("spar")
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

fn load_state(dir: &std::path::Path, run_id: &str) -> Value {
    let p = dir.join(".spar/runs").join(run_id).join("state.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// The encoded bug: with the `stopped` marker present, `execute_loop` must
/// dispatch NOTHING. Pre-fix the marker is meaningless and the implementer runs.
#[test]
fn stop_marker_halts_dispatch() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let branch_before = primary_branch(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    // Drop the marker before implement dispatches anything.
    let markers = tmp.path().join(".spar/runs").join(&run_id).join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(markers.join("stopped"), "stopped by operator\n").unwrap();

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(1); // Stopped maps to Failure(1)

    let state = load_state(tmp.path(), &run_id);
    assert_eq!(
        state["phase"], "stopped",
        "run must halt at Stopped: {state}"
    );

    // The implementer slot must never have run.
    let impl_slot = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot prepared");
    assert_ne!(
        impl_slot["status"], "done",
        "implementer must not dispatch under a stop marker: {impl_slot}"
    );

    // Worktree records and branch are untouched (dry-run records cwd under .spar).
    let wts = state["worktrees"].as_array().unwrap();
    assert!(
        !wts.is_empty(),
        "worktree records must survive a stop: {state}"
    );
    assert!(tmp.path().join(".spar/runs").join(&run_id).is_dir());
    assert_eq!(primary_branch(tmp.path()), branch_before);
}

#[test]
fn stop_command_keeps_worktrees_and_run_dir() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    let st = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let sv: Value = serde_json::from_slice(&st).unwrap();
    assert_eq!(sv["phase"], "stopped");
    assert_eq!(sv["run_id"].as_str().unwrap(), run_id);

    assert!(
        tmp.path()
            .join(".spar/runs")
            .join(&run_id)
            .join("markers/stopped")
            .is_file(),
        "stop must write the marker"
    );
    assert!(
        tmp.path().join(".spar/runs").join(&run_id).is_dir(),
        "stop must not remove the run dir"
    );
    let state = load_state(tmp.path(), &run_id);
    assert_eq!(state["phase"], "stopped");
}

#[test]
fn stopped_run_resumes_after_stop() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);
    assert_eq!(load_state(tmp.path(), &run_id)["phase"], "stopped");

    // Resume: marker cleared, phase leaves Stopped, dispatch proceeds.
    let out = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_ne!(v["phase"], "stopped", "resume must leave Stopped: {v}");
    assert_eq!(v["phase"], "awaiting_ship_confirm");
    assert!(
        !tmp.path()
            .join(".spar/runs")
            .join(&run_id)
            .join("markers/stopped")
            .is_file(),
        "resume must clear the stop marker"
    );
}
