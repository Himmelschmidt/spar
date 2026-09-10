//! `--detach` puts the orchestrator in a session of its own (`setsid`) and the parent
//! only reports `detached` once it has watched the child take the run's lock. The
//! `setsid` / logging mechanics themselves are unit-tested against a controllable
//! `/bin/sleep` in `src/process.rs` (a real dry run finishes inside a handful of
//! milliseconds, too fast to observe mid-flight from outside the process). These
//! scenarios cover the CLI-visible handshake behaviour instead.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-detach-home-{}", std::process::id()));
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

fn json_of(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
        .expect("json")
}

/// A `--dry-run --detach` finishes inside the handshake's first poll: the parent must
/// report the run's real outcome (here, a gate: `awaiting_plan_approval`, exit code 2)
/// rather than printing "detached" for a child it never actually watched running.
#[test]
fn dry_run_detach_that_completes_inside_the_poll_reports_the_real_outcome() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let out = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--detach",
            "--json",
        ])
        .assert()
        .code(2);
    let v = json_of(&out);
    assert_eq!(v["phase"], "awaiting_plan_approval");
    assert_eq!(v["exit_code"], 2);
}

/// A run whose `state.json` cannot be read fails the whole invocation immediately —
/// before there is even anything to detach. This is the common case (a caller can
/// only corrupt the file before the CLI reads it at all), and it must never print
/// "detached" for a child that never got spawned.
#[test]
fn detach_never_claims_success_when_state_cannot_be_loaded_at_all() {
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

    spar_cmd()
        .current_dir(&proj)
        .args(["approve", &run_id, "--json"])
        .assert()
        .code(0);

    let state_path = proj.join(".spar/runs").join(&run_id).join("state.json");
    std::fs::write(&state_path, "not json").unwrap();

    let start = std::time::Instant::now();
    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--detach",
        ])
        .assert()
        .failure()
        .stdout(predicates::str::contains("detached").not());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "an unreadable run must fail immediately"
    );
}

/// The other half of "a child that cannot start": a run id that no longer exists by
/// the time the detached process comes up (a race between the parent's check and the
/// child's own load) makes `__internal_continue` itself fail fast and loud, which is
/// exactly what the handshake in `await_detached_start` is watching `try_wait` for.
#[test]
fn internal_continue_on_a_vanished_run_fails_fast() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let start = std::time::Instant::now();
    spar_cmd()
        .current_dir(&proj)
        .args(["__internal_continue", "no-such-run"])
        .assert()
        .failure();
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

/// `implement --run --detach` against a run with a live orchestrator refuses instead
/// of spawning a second one, naming the existing owner's pid (mirrors the guard
/// `detach_implement` already had; `detach_self`/`plan --detach` on a replan now has
/// the same one).
#[test]
fn detach_refuses_when_a_live_orchestrator_already_owns_the_run() {
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
    spar_cmd()
        .current_dir(&proj)
        .args(["approve", &run_id, "--json"])
        .assert()
        .code(0);

    // Fabricate a live-looking owner: our own pid, so `alive()` reads true.
    let lock_path = proj
        .join(".spar/runs")
        .join(&run_id)
        .join("orchestrator.lock");
    std::fs::write(&lock_path, std::process::id().to_string()).unwrap();

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--detach",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "already has a running orchestrator",
        ));
}
