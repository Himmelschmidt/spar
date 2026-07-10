//! `spar implement --run <id>` must serialize on the RunLock before mutating or
//! persisting shared state: a loser that hits OrchestratorBusy must leave
//! state.json byte-identical to the pre-invocation snapshot.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
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

/// A concurrent orchestrator holds the RunLock. A second `implement --run`
/// invocation must fail closed (OrchestratorBusy) WITHOUT having mutated and
/// persisted state.json first. Pre-fix, provider resolution + slot prep + save
/// run before the lock is acquired, so state.json is clobbered with a stale
/// pre-dispatch snapshot before the loser discovers it lost.
#[test]
fn busy_lock_leaves_state_untouched() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    let state_path = tmp
        .path()
        .join(".spar/runs")
        .join(&run_id)
        .join("state.json");
    let before = std::fs::read(&state_path).unwrap();

    // Simulate a live orchestrator: hold an exclusive flock on the run's
    // orchestrator.lock for the duration of the invocation. The fd is
    // close-on-exec so the spawned child cannot inherit the held lock.
    let lock_path = tmp
        .path()
        .join(".spar/runs")
        .join(&run_id)
        .join("orchestrator.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock_file.try_lock().expect("test must hold the run lock");
    (&lock_file)
        .write_all(std::process::id().to_string().as_bytes())
        .unwrap();

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
        .code(1)
        .stderr(predicate::str::contains(
            "already has a running orchestrator",
        ));

    let after = std::fs::read(&state_path).unwrap();
    assert_eq!(
        before, after,
        "a run rejected by OrchestratorBusy must not have rewritten state.json"
    );

    drop(lock_file);
}
