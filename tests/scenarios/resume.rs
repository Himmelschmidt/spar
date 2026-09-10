//! `spar resume <id>`: dispatch on what the run actually is, rather than aliasing
//! `implement --run`. A gate is refused (a decision is waiting), a live owner is
//! refused (naming its pid), a finished run is refused, and an at-rest resumable run
//! (`stopped`, in these tests) is picked back up through the same path a bare
//! `implement --run <id>` would take.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-resume-home-{}", std::process::id()));
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

/// A dry-run `implement -t` reaches the ship gate (default `[gates] ship = true`), no
/// live orchestrator ever spawns for a dry run.
fn implement_dry_run(proj: &std::path::Path) -> String {
    let out = spar_cmd()
        .current_dir(proj)
        .args([
            "implement",
            "-t",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("json");
    v["id"].as_str().expect("run id").to_string()
}

/// An approved plan the operator parked (`spar stop`) before implementation ever
/// dispatched — the common shape of a `stopped` run, and the one `run_from_approved`
/// is built to pick back up (it clears the `stopped` marker and dispatches fresh
/// implementer slots exactly as a bare `implement --run <id>` would).
#[test]
fn a_stopped_run_resumes_and_advances() {
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
    set_phase(&proj, &run_id, "stopped");

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id, "--json"])
        .assert();
    let v = json_of(&out);
    assert_ne!(
        v["phase"], "stopped",
        "resume must move the run off `stopped`"
    );
}

#[test]
fn a_gate_refuses_and_names_the_human_command() {
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
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains("spar approve"));
}

#[test]
fn a_live_orchestrator_refuses_naming_its_pid() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = implement_dry_run(&proj);
    set_phase(&proj, &run_id, "dispatch");
    let lock_path = proj
        .join(".spar/runs")
        .join(&run_id)
        .join("orchestrator.lock");
    std::fs::write(&lock_path, std::process::id().to_string()).unwrap();

    spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "already has a running orchestrator",
        ));
}

#[test]
fn a_finished_run_refuses() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = implement_dry_run(&proj);
    set_phase(&proj, &run_id, "done");

    spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains("nothing to resume"));
}
