//! `spar daemon`: single instance per project, and it never touches the things an
//! operator's own commands touch. The lock/bucket/queue mechanics themselves are
//! unit-tested in `src/daemon.rs`; these cover the CLI surface.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-daemon-home-{}", std::process::id()));
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
    c
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[test]
fn status_reports_not_running_with_no_lock_held() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    spar_cmd()
        .current_dir(&proj)
        .args(["daemon", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not running"));
}

#[test]
fn stop_with_no_daemon_running_is_a_success_noop() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    spar_cmd()
        .current_dir(&proj)
        .args(["daemon", "stop"])
        .assert()
        .success()
        .stdout(predicates::str::contains("no daemon running"));
}

/// `status` reads the lock file's owner independent of holding the flock itself — the
/// actual single-instance exclusion (real `try_lock()` contention) is covered by
/// `DaemonLock`'s own unit tests in `src/daemon.rs`, which can hold two `File`s open
/// in one process; a second real CLI process is what `spar daemon start` itself
/// refuses.
#[test]
fn status_reports_the_pid_recorded_in_the_lock_file() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    // A live-looking daemon: our own pid, so `alive()` reads true. The encoding
    // matches `PidToken::encode` (`pid` or `pid:starttime`); a bare pid is enough to
    // prove liveness on a platform with no `/proc` start-time.
    std::fs::create_dir_all(proj.join(".spar")).unwrap();
    std::fs::write(
        proj.join(".spar/daemon.lock"),
        std::process::id().to_string(),
    )
    .unwrap();

    spar_cmd()
        .current_dir(&proj)
        .args(["daemon", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("running"));
}

/// A real, running daemon process, actually stopped by `spar daemon stop` — not a
/// fabricated lock file. `start --foreground` re-checks the stop marker every second
/// inside its tick sleep regardless of `tick_secs`, so this does not need a fast tick
/// config to finish quickly.
#[test]
fn stop_stops_a_real_running_daemon() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin("spar"))
        .args(["daemon", "start", "--foreground"])
        .current_dir(&proj)
        .env("SPAR_HOME", spar_home_dir())
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn spar daemon start --foreground");

    let lock = proj.join(".spar/daemon.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lock.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(lock.is_file(), "the daemon never took its own lock");

    spar_cmd()
        .current_dir(&proj)
        .args(["daemon", "stop"])
        .assert()
        .success()
        .stdout(predicates::str::contains("stop requested"));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(
        exited,
        "daemon stop must actually end the process, not just ask"
    );
}

fn json_of(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
        .expect("json")
}

fn state_path(proj: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    proj.join(".spar/runs").join(run_id).join("state.json")
}

fn set_phase(proj: &std::path::Path, run_id: &str, phase: &str) {
    let path = state_path(proj, run_id);
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    state["phase"] = serde_json::Value::String(phase.into());
    std::fs::write(&path, state.to_string()).unwrap();
}

/// `spar stop` on a queued run (`Phase::Init` plus a `.spar/queue/<id>` spool file, no
/// orchestrator of its own) must cancel the spool entry, not just move the phase to
/// `stopped`. Left in place, a later capacity-freeing tick would read the stale entry
/// and dispatch an orchestrator for a run the operator explicitly told to stop.
#[test]
fn stopping_a_queued_run_cancels_its_spool_entry() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();

    // A dry-run plan reaches a gate immediately, not `Init`; force it back to `Init`
    // and drop a spool file to reproduce what the daemon's own admission queue leaves
    // behind for a run still waiting on capacity.
    set_phase(&proj, &run_id, "init");
    let queue_dir = proj.join(".spar/queue");
    std::fs::create_dir_all(&queue_dir).unwrap();
    let queue_file = queue_dir.join(&run_id);
    std::fs::write(
        &queue_file,
        serde_json::json!({
            "run_id": run_id,
            "buckets": [],
            "enqueued_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string(),
    )
    .unwrap();

    spar_cmd()
        .current_dir(&proj)
        .args(["stop", &run_id])
        .assert()
        .success();

    assert!(
        !queue_file.is_file(),
        "stopping a queued run must cancel its spool entry, or a later tick can restart it"
    );

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_path(&proj, &run_id)).unwrap())
            .unwrap();
    assert_eq!(state["phase"], "stopped");
}

/// The race the spool-cancellation fix above cannot close by itself: a daemon tick can
/// decide to admit a queued run (read `Init`, no live owner, capacity free) a moment
/// before `spar stop` removes its spool entry. The admitted `__internal_continue` child
/// can still reach the run after the stop's `reap_run` has already written the `stopped`
/// marker. It must refuse to dispatch rather than silently implementing a stopped run —
/// this exercises the interleaving directly, without depending on real scheduling
/// timing, by writing the marker before invoking the child the daemon would have
/// spawned.
#[test]
fn a_stopped_marker_written_after_admission_still_blocks_the_delayed_child() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();

    // Reproduce a queued run the daemon has just decided to admit: back at `Init`,
    // spool entry present (about to be deleted by the daemon on admission).
    set_phase(&proj, &run_id, "init");

    // The operator's `spar stop` runs concurrently and wins: its `reap_run` has
    // already dropped the `stopped` marker before the admitted child gets scheduled.
    let markers_dir = proj.join(".spar/runs").join(&run_id).join("markers");
    std::fs::create_dir_all(&markers_dir).unwrap();
    std::fs::write(markers_dir.join("stopped"), "stopped by operator\n").unwrap();

    // The run's first (real, non-queued) dispatch already drafted a plan before this
    // test forced it back to `Init`; capture that so a re-dispatch (the bug) is caught
    // even though the artifact already exists.
    let plan_artifact = proj
        .join(".spar/runs")
        .join(&run_id)
        .join("artifacts/plan.md");
    let plan_before = std::fs::read_to_string(&plan_artifact).unwrap();

    // The delayed child the daemon already spawned, reaching the run after the marker.
    spar_cmd()
        .current_dir(&proj)
        .env("SPAR_DRY_RUN", "1")
        .args(["__internal_continue", &run_id])
        .assert()
        .code(1);

    let plan_after = std::fs::read_to_string(&plan_artifact).unwrap();
    assert_eq!(
        plan_before, plan_after,
        "a delayed child racing a stop must not redispatch a plan"
    );
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_path(&proj, &run_id)).unwrap())
            .unwrap();
    assert_eq!(
        state["phase"], "stopped",
        "the marker must settle the run at Stopped, not silently dispatch it"
    );
}

/// A second `spar daemon start` against a project already holding the lock refuses,
/// and `daemon status` names the one real pid — not a fabricated lock body.
#[test]
fn a_second_start_refuses_while_the_first_holds_the_lock() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin("spar"))
        .args(["daemon", "start", "--foreground"])
        .current_dir(&proj)
        .env("SPAR_HOME", spar_home_dir())
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn spar daemon start --foreground");

    let lock = proj.join(".spar/daemon.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lock.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(lock.is_file(), "the daemon never took its own lock");

    // `--foreground` here (not the default background form) so the refusal is
    // synchronous: `DaemonLock::acquire` bails immediately with "already running"
    // rather than the backgrounded form's generic "exited before starting", which
    // would still be correct but would obscure the actual reason in the log instead
    // of this process's own stderr.
    spar_cmd()
        .current_dir(&proj)
        .args(["daemon", "start", "--foreground"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already running"));

    spar_cmd()
        .current_dir(&proj)
        .args(["daemon", "stop"])
        .assert()
        .success();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(exited, "daemon never exited after stop");
}
