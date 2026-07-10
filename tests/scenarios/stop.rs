//! `spar stop`: halt dispatch, keep the worktree/branch, stay resumable.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};
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

fn save_state(dir: &std::path::Path, run_id: &str, v: &Value) {
    let p = dir.join(".spar/runs").join(run_id).join("state.json");
    std::fs::write(p, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

/// A terminal slot (Done/Failed/Stuck) has a reaped pid; that pid may have been
/// recycled by an unrelated process. `stop` must never signal it. Encoded here:
/// a live process whose bare pid is recorded on a Done slot survives `stop`.
/// Pre-fix the ungated loop SIGTERMs its process group and kills it.
#[test]
fn stop_leaves_terminal_slot_pid_untouched() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    let mut state = load_state(tmp.path(), &run_id);
    let slot_id = state["slots"][0]["id"].as_str().unwrap().to_string();
    state["slots"][0]["status"] = Value::from("done");
    save_state(tmp.path(), &run_id, &state);

    // Its own process group so a stray kill(-pid) would reach it.
    let mut child = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let markers = tmp.path().join(".spar/runs").join(&run_id).join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(markers.join(format!("{slot_id}.pid")), child.id().to_string()).unwrap();

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);

    // The Done slot's process must still be running after stop returned.
    std::thread::sleep(Duration::from_millis(300));
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(alive, "stop must not signal a terminal slot's recorded pid");
}

/// `stop` snapshots state before a multi-second kill window, then persists that
/// snapshot. Any slot exit/usage the orchestrator writes while being killed must
/// not be clobbered. Encoded: a write that lands during the kill window survives.
#[test]
fn stop_preserves_state_written_during_kill_window() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    let mut state = load_state(tmp.path(), &run_id);
    let slot_id = state["slots"][0]["id"].as_str().unwrap().to_string();
    state["slots"][0]["status"] = Value::from("running");
    save_state(tmp.path(), &run_id, &state);

    // A SIGTERM-ignoring group leader keeps the kill window open the full grace.
    let mut child = Command::new("sh")
        .args(["-c", "trap '' TERM; while true; do sleep 1; done"])
        .process_group(0)
        .spawn()
        .unwrap();
    let markers = tmp.path().join(".spar/runs").join(&run_id).join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(markers.join(format!("{slot_id}.pid")), child.id().to_string()).unwrap();

    // Simulate the orchestrator persisting a slot result during the kill window:
    // wait for stop's `stopped` marker (written after it loads state), then write.
    let dir = tmp.path().to_path_buf();
    let rid = run_id.clone();
    let writer = std::thread::spawn(move || {
        let stopped = dir
            .join(".spar/runs")
            .join(&rid)
            .join("markers/stopped");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stopped.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut s = load_state(&dir, &rid);
        s["slots"][0]["exit_code"] = Value::from(99);
        save_state(&dir, &rid, &s);
    });

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);
    writer.join().unwrap();
    let _ = child.kill();
    let _ = child.wait();

    let state = load_state(tmp.path(), &run_id);
    assert_eq!(state["phase"], "stopped");
    assert_eq!(
        state["slots"][0]["exit_code"], 99,
        "stop must not clobber a slot result persisted during the kill window: {state}"
    );
}

/// If the orchestrator finishes naturally during the kill window and persists a
/// gate/terminal phase, `stop` must leave it there — not stamp `Stopped` over it,
/// which would make a later `implement --run` redo finished work. Encoded: a phase
/// that reaches a gate during the kill window survives. Pre-fix step 4 overwrote it.
#[test]
fn stop_does_not_downgrade_a_run_that_finished_during_the_kill_window() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    let mut state = load_state(tmp.path(), &run_id);
    let slot_id = state["slots"][0]["id"].as_str().unwrap().to_string();
    state["slots"][0]["status"] = Value::from("running");
    save_state(tmp.path(), &run_id, &state);

    let mut child = Command::new("sh")
        .args(["-c", "trap '' TERM; while true; do sleep 1; done"])
        .process_group(0)
        .spawn()
        .unwrap();
    let markers = tmp.path().join(".spar/runs").join(&run_id).join("markers");
    std::fs::create_dir_all(&markers).unwrap();
    std::fs::write(markers.join(format!("{slot_id}.pid")), child.id().to_string()).unwrap();

    let dir = tmp.path().to_path_buf();
    let rid = run_id.clone();
    let writer = std::thread::spawn(move || {
        let stopped = dir.join(".spar/runs").join(&rid).join("markers/stopped");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stopped.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut s = load_state(&dir, &rid);
        s["phase"] = Value::from("awaiting_ship_confirm");
        save_state(&dir, &rid, &s);
    });

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);
    writer.join().unwrap();
    let _ = child.kill();
    let _ = child.wait();

    let state = load_state(tmp.path(), &run_id);
    assert_eq!(
        state["phase"], "awaiting_ship_confirm",
        "stop must not downgrade a run that reached a gate during the kill window: {state}"
    );
    assert!(
        !markers.join("stopped").is_file(),
        "stop must drop its marker when it declines to stop a finished run"
    );
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
