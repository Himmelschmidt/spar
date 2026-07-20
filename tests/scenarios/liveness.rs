//! Slot liveness surfaced through `spar status --json`.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;
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

#[test]
fn dry_run_status_reports_pid_liveness() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--task",
            "liveness check",
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
    let v: Value = serde_json::from_slice(&out).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let st = spar_cmd()
        .current_dir(tmp.path())
        .args(["status", run_id, "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let sv: Value = serde_json::from_slice(&st).unwrap();
    let slots = sv["slots"].as_array().expect("slots array");
    assert!(!slots.is_empty(), "expected slots in status");

    for slot in slots {
        assert!(
            slot.get("pid_alive").is_some(),
            "every slot must expose pid_alive: {slot}"
        );
        if slot["status"] == "done" {
            assert_eq!(
                slot["pid_alive"],
                Value::Bool(false),
                "a done slot must not report a live process: {slot}"
            );
        }
    }
    assert!(
        slots.iter().any(|s| s["status"] == "done"),
        "expected at least one done slot"
    );
}

#[cfg(unix)]
#[test]
fn foreground_workflows_acquire_orchestrator_lock() {
    for workflow in ["review", "arena", "roles", "peer"] {
        assert_foreground_lock(workflow);
    }
}

#[cfg(unix)]
fn assert_foreground_lock(workflow: &str) {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_git_repo(&proj);
    // No worktrees: slots run in the project root so the blocking fake CLI is
    // reached regardless of per-workflow slot-id shapes.
    std::fs::write(proj.join("spar.toml"), "isolation = \"none\"\n").unwrap();

    // Fake provider CLIs that block, so the foreground orchestrator stays inside
    // execute() (holding its lock) long enough to observe it.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["claude", "grok", "agy"] {
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let exe = assert_cmd::cargo::cargo_bin("spar");
    let mut child = Command::new(&exe)
        .current_dir(&proj)
        .env("SPAR_HOME", spar_home_dir())
        .env("PATH", &path_env)
        // Raw spawn, so it must strip the slot identity vars itself — see `spar_cmd`.
        // This one runs a *live* (non-dry) workflow, so a leak here writes real runs.
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .args([
            "run",
            "--workflow",
            workflow,
            "--task",
            "lock guard regression",
            "--providers",
            "cli:claude,cli:grok",
            "--json",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let runs = proj.join(".spar/runs");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut lock_pid: Option<u64> = None;
    let mut run_id: Option<String> = None;
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&runs) {
            for e in entries.flatten() {
                let lock = e.path().join("orchestrator.lock");
                if let Ok(s) = std::fs::read_to_string(&lock) {
                    // Lock body is `pid` or `pid:starttime`; the pid is the identity we assert on.
                    if let Ok(pid) = s.trim().split(':').next().unwrap_or("").parse::<u64>() {
                        lock_pid = Some(pid);
                        run_id = e.file_name().to_str().map(str::to_string);
                    }
                }
            }
        }
        if lock_pid.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let observed = lock_pid.zip(run_id.clone());
    let (pid, run_id) = match observed {
        Some(v) => v,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "foreground `spar run --workflow {workflow}` created no orchestrator.lock -- \
                 a concurrent orchestrator on the same run would not be refused"
            );
        }
    };
    assert_eq!(
        pid,
        child.id() as u64,
        "orchestrator.lock for {workflow} must name the live orchestrator process"
    );

    // While the orchestrator holds its lock, status must expose it as alive.
    let st = spar_cmd()
        .current_dir(&proj)
        .args(["status", &run_id, "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let sv: Value = serde_json::from_slice(&st).unwrap();

    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        sv["orchestrator_pid"].as_u64(),
        Some(pid),
        "status must expose the {workflow} orchestrator pid: {sv}"
    );
    assert_eq!(
        sv["orchestrator_alive"],
        Value::Bool(true),
        "status must report the {workflow} orchestrator as alive: {sv}"
    );
}

#[test]
fn implement_refuses_second_orchestrator() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    let plan = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "lock check",
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
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();

    // A live orchestrator (this test process) already owns the run: hold the
    // advisory lock and publish our pid, exactly as a real run does.
    let lock = tmp
        .path()
        .join(".spar/runs")
        .join(&run_id)
        .join("orchestrator.lock");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    held.try_lock().unwrap();
    std::fs::write(&lock, std::process::id().to_string()).unwrap();

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
        .stderr(
            predicate::str::contains("already has a running orchestrator")
                .and(predicate::str::contains(std::process::id().to_string())),
        );

    // status must let an operator see who is driving the run.
    let st = spar_cmd()
        .current_dir(tmp.path())
        .args(["status", &run_id, "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let sv: Value = serde_json::from_slice(&st).unwrap();
    assert_eq!(
        sv["orchestrator_pid"].as_u64(),
        Some(std::process::id() as u64),
        "status must expose the owning orchestrator pid: {sv}"
    );
    assert_eq!(
        sv["orchestrator_alive"],
        Value::Bool(true),
        "status must report the orchestrator as alive: {sv}"
    );
}
