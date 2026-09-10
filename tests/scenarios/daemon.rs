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
