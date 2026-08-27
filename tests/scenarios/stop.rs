//! `spar stop`: halt dispatch, keep the worktree/branch, stay resumable.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};
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
    // Also isolate the *config* dir. SPAR_HOME only moves the registry; without this
    // the spawned binary still layers the developer's ~/.config/spar/config.toml under
    // the test's project, so an ordinary local setting fails scenarios that never
    // mention it. (XDG applies on Linux, where the suite runs.)
    c.env("XDG_CONFIG_HOME", spar_home_dir());
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

fn primary_branch(dir: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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
    std::fs::write(
        markers.join(format!("{slot_id}.pid")),
        child.id().to_string(),
    )
    .unwrap();

    spar_cmd()
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
    std::fs::write(
        markers.join(format!("{slot_id}.pid")),
        child.id().to_string(),
    )
    .unwrap();

    // Simulate the orchestrator persisting a slot result during the kill window:
    // wait for stop's `stopped` marker (written after it loads state), then write.
    let dir = tmp.path().to_path_buf();
    let rid = run_id.clone();
    let writer = std::thread::spawn(move || {
        let stopped = dir.join(".spar/runs").join(&rid).join("markers/stopped");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stopped.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut s = load_state(&dir, &rid);
        s["slots"][0]["exit_code"] = Value::from(99);
        save_state(&dir, &rid, &s);
    });

    spar_cmd()
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
    std::fs::write(
        markers.join(format!("{slot_id}.pid")),
        child.id().to_string(),
    )
    .unwrap();

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

    spar_cmd()
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

    let st = spar_cmd()
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

    spar_cmd()
        .current_dir(tmp.path())
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);
    assert_eq!(load_state(tmp.path(), &run_id)["phase"], "stopped");

    // Resume: marker cleared, phase leaves Stopped, dispatch proceeds.
    let out = spar_cmd()
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

/// A real orchestrator, a fake provider that hangs like a working agent, blocked until
/// the implementer has actually spawned. The only way to reach the mid-dispatch states
/// `stop` has to settle.
fn hanging_run(dir: &std::path::Path) -> (std::process::Child, std::path::PathBuf, String) {
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_git_repo(&proj);

    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho $$ >> \"$SLOT_PIDS_FILE\"\nsleep 300\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let pid_file = dir.join("slot_pids");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let child = Command::new(assert_cmd::cargo::cargo_bin("spar"))
        .args([
            "run",
            "--workflow",
            "loop",
            "-t",
            "x",
            "--providers",
            "cli:claude",
        ])
        .current_dir(&proj)
        .env("PATH", &path_env)
        .env("SPAR_HOME", spar_home_dir())
        .env("SLOT_PIDS_FILE", &pid_file)
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn spar");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !pid_file.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(pid_file.is_file(), "the implementer never spawned");
    let run_id = only_run_id(&proj);
    (child, proj, run_id)
}

fn slot(state: &Value, slot_id: &str) -> Value {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == slot_id)
        .unwrap_or_else(|| panic!("slot {slot_id} in {state}"))
        .clone()
}

/// `stop` on a run that still has a live orchestrator is a **halt**, not a crash. The
/// reap kills that orchestrator, so a reason derived after it reports every operator
/// stop as "orchestrator died mid-dispatch", a false incident report on the operator's
/// own run, and one they would reasonably go investigating.
#[test]
fn stopping_a_healthy_run_records_a_halt_not_a_crash() {
    let tmp = tempdir().unwrap();
    let (mut child, proj, run_id) = hanging_run(tmp.path());

    spar_cmd()
        .current_dir(&proj)
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);
    let _ = child.wait();

    let impl_slot = slot(&load_state(&proj, &run_id), "impl");
    assert_ne!(impl_slot["status"], "running", "stop must settle it");
    assert_eq!(
        impl_slot["error"], "halted by operator (spar stop)",
        "the operator halted this run; nothing crashed: {impl_slot}"
    );
}

/// A real orchestrator, killed the way `spar stop` kills one: SIGTERM, inside a
/// dispatch. `running` was persisted when the slot was dispatched; the terminal status
/// never was, and no marker was written either because the wait never returned. On disk
/// the run keeps claiming a slot that nothing has been behind since. `stop` is the last
/// command to touch it, so it is where the record has to be settled.
#[test]
fn stop_settles_a_slot_its_orchestrator_died_inside() {
    let tmp = tempdir().unwrap();
    let (mut child, proj, run_id) = hanging_run(tmp.path());

    let running = load_state(&proj, &run_id);
    let dispatched = running["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["status"] == "running")
        .expect("a dispatched slot")
        .clone();
    assert!(
        dispatched["exit_code"].is_null(),
        "a running slot cannot carry an exit code: {dispatched}"
    );
    let slot_id = dispatched["id"].as_str().unwrap().to_string();

    // The signal `spar stop` sends. The handler is async-signal-safe and re-raises, so
    // the orchestrator dies inside `run_captured` with no terminal status and no marker.
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let _ = child.wait();
    assert_eq!(
        load_state(&proj, &run_id)["slots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == slot_id.as_str())
            .unwrap()["status"],
        "running",
        "precondition: the killed dispatch leaves `running` on disk"
    );

    spar_cmd()
        .current_dir(&proj)
        .args(["stop", &run_id, "--json"])
        .assert()
        .code(0);

    let settled = load_state(&proj, &run_id);
    let slot = settled["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == slot_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(settled["phase"], "stopped");
    assert_ne!(
        slot["status"], "running",
        "stop must settle a slot its orchestrator died inside: {slot}"
    );
    assert_eq!(
        slot["error"], "orchestrator died mid-dispatch",
        "the settled slot must say why, not read as a failure of the work: {slot}"
    );

    // The next round must not inherit that verdict. Dry-run so the resume is cheap and
    // deterministic; the slot id and worktree carry, which is the whole point.
    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);

    let after = load_state(&proj, &run_id);
    let slot = after["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == slot_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(slot["status"], "done", "the resumed round must run: {slot}");
    assert!(
        slot["error"].is_null(),
        "a done slot must not carry the killed round's error: {slot}"
    );
    assert!(slot["signal"].is_null(), "stale signal carried: {slot}");
}

fn only_run_id(proj: &std::path::Path) -> String {
    let runs = proj.join(".spar/runs");
    std::fs::read_dir(&runs)
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}
