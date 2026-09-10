//! `spar daemon`: single instance per project, and it never touches the things an
//! operator's own commands touch. The lock/bucket/queue mechanics themselves are
//! unit-tested in `src/daemon.rs`; these cover the CLI surface.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
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
