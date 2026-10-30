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

/// Per-test-process SPAR_HOME so the suite never writes the developer's real
/// ~/.spar/registry.json. Shared across spawns in this binary.
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
    // spar exports these into every slot (providers/presence.rs), so when the suite runs
    // *inside* a spar worktree the child would resolve the primary checkout instead of
    // this test's temp project and write real runs into it. Clear them per-Command
    // (never via process env — these binaries run tests in parallel).
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

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
    let plan = spar_cmd()
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
    spar_cmd()
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

    spar_cmd()
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
        .stdout(predicate::str::contains(
            "already has a running orchestrator",
        ));

    let after = std::fs::read(&state_path).unwrap();
    assert_eq!(
        before, after,
        "a run rejected by OrchestratorBusy must not have rewritten state.json"
    );

    drop(lock_file);
}

fn hold_run_lock(dir: &std::path::Path, run_id: &str) -> std::fs::File {
    let lock_path = dir
        .join(".spar/runs")
        .join(run_id)
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
    lock_file
}

/// `--detach` used to save the re-entry's state (round bumped, phase reset) and only
/// then ask whether someone else owned the run, so the refusal came after the live
/// run's state.json had already been overwritten.
#[test]
fn detach_busy_lock_leaves_state_untouched() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let state_path = tmp
        .path()
        .join(".spar/runs")
        .join(&run_id)
        .join("state.json");
    let before = std::fs::read(&state_path).unwrap();
    let lock = hold_run_lock(tmp.path(), &run_id);

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--detach",
            "--json",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "already has a running orchestrator",
        ));

    assert_eq!(
        before,
        std::fs::read(&state_path).unwrap(),
        "a detached re-entry refused as busy must not have rewritten state.json"
    );
    drop(lock);
}

/// `--reload-config` re-freezes the run's config.json; against a live orchestrator it
/// must refuse before writing it, not after.
#[test]
fn reload_config_busy_lock_leaves_config_untouched() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let run_dir = tmp.path().join(".spar/runs").join(&run_id);
    let config_before = std::fs::read(run_dir.join("config.json")).unwrap();
    let state_before = std::fs::read(run_dir.join("state.json")).unwrap();
    std::fs::write(tmp.path().join("spar.toml"), "[critic]\nenabled = false\n").unwrap();
    let lock = hold_run_lock(tmp.path(), &run_id);

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--reload-config",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "already has a running orchestrator",
        ));

    assert_eq!(
        config_before,
        std::fs::read(run_dir.join("config.json")).unwrap()
    );
    assert_eq!(
        state_before,
        std::fs::read(run_dir.join("state.json")).unwrap()
    );
    drop(lock);
}

/// The detaching parent now holds the lock while it mutates the run, so it has to
/// release it before the child it spawns tries to take it, or the child refuses its own
/// run. A dry run may settle at the ship gate inside the handshake, hence exit 0 or 2.
#[test]
fn detach_reentry_proceeds_once_the_lock_is_free() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--detach",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]))
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["run_id"], Value::String(run_id));
}
