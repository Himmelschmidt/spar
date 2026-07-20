//! `spar implement --run <id> -t "..."` applies the task text as a per-round amendment.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
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

const PLAN_TASK: &str = "add a hello world module";
const SENTINEL: &str = "AMENDMENT-SENTINEL-XYZ";

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
            PLAN_TASK,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

fn impl_prompt(dir: &std::path::Path, run_id: &str) -> String {
    std::fs::read_to_string(dir.join(".spar/runs").join(run_id).join("prompt-impl.md")).unwrap()
}

fn state_json(dir: &std::path::Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(dir.join(".spar/runs").join(run_id).join("state.json")).unwrap(),
    )
    .unwrap()
}

#[test]
fn implement_run_with_task_applies_amendment() {
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
            "--json",
            "-t",
            SENTINEL,
        ])
        .assert()
        .code(2);

    let prompt = impl_prompt(tmp.path(), &run_id);
    assert!(
        prompt.contains(SENTINEL),
        "impl prompt must carry the -t amendment; got:\n{prompt}"
    );
    assert!(
        prompt.contains(PLAN_TASK),
        "impl prompt must still carry the original plan task; got:\n{prompt}"
    );
    assert!(
        !prompt.contains("{{amendment_section}}"),
        "amendment_section must be substituted; got:\n{prompt}"
    );

    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        v["amendment"].as_str(),
        Some(SENTINEL),
        "--json run object must expose the amendment"
    );
    assert_eq!(
        v["task"].as_str(),
        Some(PLAN_TASK),
        "amendment must not overwrite the run task"
    );

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["amendment"].as_str(), Some(SENTINEL));
}

#[test]
fn implement_run_without_task_clears_stale_amendment() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

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
            "-t",
            SENTINEL,
        ])
        .assert()
        .code(2);

    // Second round, no -t: stale amendment must be cleared.
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
        .code(2);

    let prompt = impl_prompt(tmp.path(), &run_id);
    assert!(
        !prompt.contains(SENTINEL),
        "stale amendment must not re-apply to a later round; got:\n{prompt}"
    );
    assert!(prompt.contains(PLAN_TASK), "original task still present");

    let st = state_json(tmp.path(), &run_id);
    assert!(
        st.get("amendment").is_none() || st["amendment"].is_null(),
        "state.json must not retain a stale amendment: {}",
        st
    );
}

#[test]
fn implement_run_amendment_notice_human() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "-t",
            SENTINEL,
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains(SENTINEL));
}
