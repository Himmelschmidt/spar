//! Built-in suite channel (O54): with `[suite].command` set, spar runs the suite itself
//! and no `tester` slot is ever created.
//!
//! `--dry-run` deliberately does not execute the commands (that would run a project's
//! real suite from spar's own test backend), so what these scenarios pin is the wiring:
//! the slot is gone, `suite.md` is spar's, and the reviewers still get a suite report.
//! Execution and the exit-code verdict are covered by the unit tests in `src/suite.rs`.
use assert_cmd::cargo::cargo_bin_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
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
    // See the trap in CLAUDE.md: without these the child resolves the primary checkout
    // when the suite runs inside a spar worktree and writes real runs into it.
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn init_git_repo(dir: &Path) {
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

fn project(dir: &Path, suite_block: &str) {
    init_git_repo(dir);
    std::fs::write(dir.join("spar.toml"), suite_block).unwrap();
}

fn planned_run(dir: &Path) -> String {
    let plan = spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("plan json");
    let run_id = v["run_id"].as_str().expect("run_id").to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

/// Asserts the ship gate (exit 2). Without checking the code, a scenario that looks for
/// the *absence* of a tester slot passes just as well when `implement` died before it
/// created any slots at all.
fn implement(dir: &Path, run_id: &str) {
    spar_cmd()
        .current_dir(dir)
        .args([
            "implement",
            "--run",
            run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
}

fn state_json(dir: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(run_dir(dir, run_id).join("state.json")).unwrap())
        .unwrap()
}

fn run_dir(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(".spar/runs").join(run_id)
}

fn roles(dir: &Path, run_id: &str) -> Vec<String> {
    state_json(dir, run_id)["slots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["role"].as_str().unwrap_or("").to_string())
        .collect()
}

/// The whole point: a configured command means no model is paid to run the suite.
#[test]
fn a_configured_command_spawns_no_tester_slot() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    project(dir, "[suite]\ncommand = [\"true\"]\n");
    let run_id = planned_run(dir);
    implement(dir, &run_id);

    let roles = roles(dir, &run_id);
    assert!(
        !roles.iter().any(|r| r == "tester"),
        "built-in suite still spawned a tester slot: {roles:?}"
    );
    assert!(roles.iter().any(|r| r == "implementer"), "{roles:?}");
}

/// Without a command the agent channel is unchanged, so this feature is opt-in.
#[test]
fn no_command_keeps_the_agent_tester() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    project(dir, "[suite]\nenabled = true\n");
    let run_id = planned_run(dir);
    implement(dir, &run_id);

    let roles = roles(dir, &run_id);
    assert!(
        roles.iter().any(|r| r == "tester"),
        "agent suite channel lost its tester slot: {roles:?}"
    );
}

/// spar writes `suite.md` itself, in the shape the reviewers already read.
#[test]
fn spar_writes_the_suite_report_itself() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    project(dir, "[suite]\ncommand = [\"true\", \"echo ok\"]\n");
    let run_id = planned_run(dir);
    implement(dir, &run_id);

    let body = std::fs::read_to_string(run_dir(dir, &run_id).join("artifacts/suite.md"))
        .expect("suite.md");
    assert!(body.contains("## Result"), "{body}");
    assert!(body.contains("`echo ok`"), "{body}");
    // The gate value, not the prose: this is what `suite_blocks_ship` reads.
    assert_eq!(state_json(dir, &run_id)["suite_outcome"], "pass");
}

/// `--dry-run` must not execute a project's suite, and must not claim it did: this body
/// is interpolated verbatim into every reviewer's prompt.
#[test]
fn a_dry_run_neither_executes_nor_claims_to_have() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let canary = dir.join("EXECUTED");
    project(
        dir,
        &format!("[suite]\ncommand = [\"touch {}\"]\n", canary.display()),
    );
    let run_id = planned_run(dir);
    implement(dir, &run_id);

    assert!(!canary.exists(), "dry-run executed the suite command");
    let body = std::fs::read_to_string(run_dir(dir, &run_id).join("artifacts/suite.md"))
        .expect("suite.md");
    assert!(body.contains("executed none of"), "{body}");
    assert!(!body.contains("All exited 0"), "{body}");
}

/// A zero budget would spend itself before the first command and wedge the run at a gate
/// nothing can clear, so it is refused at config load instead.
#[test]
fn a_zero_timeout_with_commands_is_refused() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    project(dir, "[suite]\ntimeout_secs = 0\ncommand = [\"true\"]\n");
    spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("[suite].timeout_secs is 0"));
}

/// A blank entry is a config error at load, not a command that silently runs `sh -c ""`
/// and reports a green gate.
#[test]
fn an_empty_command_entry_is_refused() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    project(dir, "[suite]\ncommand = [\"cargo test\", \"  \"]\n");
    spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("[suite].command[1] is empty"));
}
