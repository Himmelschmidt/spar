use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("spar-composition-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    })
    .clone()
}

fn spar_cmd() -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("spar");
    command.env("SPAR_HOME", spar_home_dir());
    command.env_remove("SPAR_PROJECT_ROOT");
    command.env_remove("SPAR_RUN_ID");
    command.env_remove("SPAR_AGENT_ID");
    command
}

fn init_repo(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
}

fn run_json(dir: &std::path::Path, args: &[&str], code: i32) -> Value {
    let output = spar_cmd().current_dir(dir).args(args).assert().code(code);
    let stdout = String::from_utf8_lossy(output.get_output().stdout.as_slice());
    serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("spar {args:?} did not emit JSON ({error}): {stdout}"))
}

fn frozen_config(dir: &std::path::Path, run: &str) -> Value {
    let path = dir.join(".spar/runs").join(run).join("config.json");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

#[test]
fn plan_role_pins_and_backups_are_frozen_without_writing_project_config() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    let run = run_json(
        dir,
        &[
            "plan",
            "--task",
            "compose a run",
            "--dry-run",
            "--json",
            "--role",
            "planner=cli:claude@opus",
            "--role",
            "plan_critic=cli:codex@gpt-5.6-terra",
            "--role",
            "test_author=cli:grok@fast",
            "--backup",
            "planner=cli:codex@gpt-5.6-terra",
            "--backup",
            "plan_critic=cli:claude@sonnet",
            "--backup",
            "test_author=cli:codex@gpt-5.6-luna",
        ],
        2,
    );
    let config = frozen_config(dir, run["run_id"].as_str().unwrap());

    assert_eq!(config["roles"]["planner"], "cli:claude@opus");
    assert_eq!(config["roles"]["plan_critic"], "cli:codex@gpt-5.6-terra");
    assert_eq!(config["roles"]["test_author"], "cli:grok@fast");
    assert_eq!(config["backups"]["planner"], "cli:codex@gpt-5.6-terra");
    assert_eq!(config["backups"]["plan_critic"], "cli:claude@sonnet");
    assert_eq!(config["backups"]["test_author"], "cli:codex@gpt-5.6-luna");
    assert!(
        !dir.join("spar.toml").exists(),
        "one-run composition must not write shared project configuration"
    );
}

#[test]
fn direct_implement_preserves_reviewer_backup_ordinals_in_the_snapshot() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    let run = run_json(
        dir,
        &[
            "implement",
            "--task",
            "compose a direct run",
            "--dry-run",
            "--json",
            "--without",
            "suite",
            "--role",
            "implementer=cli:claude@opus",
            "--role",
            "reviewer=cli:codex@gpt-5.6-terra",
            "--role",
            "reviewer=cli:grok@fast",
            "--backup",
            "implementer=cli:codex@gpt-5.6-luna",
            "--backup",
            "reviewer=cli:claude@sonnet",
            "--backup",
            "reviewer=cli:agy@mini",
        ],
        2,
    );
    let config = frozen_config(dir, run["run_id"].as_str().unwrap());

    assert_eq!(config["roles"]["implementer"], "cli:claude@opus");
    assert_eq!(
        config["roles"]["reviewer"],
        serde_json::json!(["cli:codex@gpt-5.6-terra", "cli:grok@fast"])
    );
    assert_eq!(config["backups"]["implementer"], "cli:codex@gpt-5.6-luna");
    assert_eq!(
        config["backups"]["reviewer"],
        serde_json::json!(["cli:claude@sonnet", "cli:agy@mini"])
    );
}

#[test]
fn review_accepts_an_exact_pinned_reviewer_panel_with_ordinal_backups() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    let run = run_json(
        dir,
        &[
            "run",
            "--workflow",
            "review",
            "--task",
            "review composition",
            "--dry-run",
            "--json",
            "--role",
            "reviewer=cli:claude@opus",
            "--role",
            "reviewer=cli:codex@gpt-5.6-terra",
            "--role",
            "reviewer=cli:grok@fast",
            "--backup",
            "reviewer=cli:opencode@luna",
            "--backup",
            "reviewer=api:openai@gpt-4",
            "--backup",
            "reviewer=cli:agy@sonnet",
        ],
        0,
    );
    let config = frozen_config(dir, run["run_id"].as_str().unwrap());

    assert_eq!(
        config["roles"]["reviewer"],
        serde_json::json!([
            "cli:claude@opus",
            "cli:codex@gpt-5.6-terra",
            "cli:grok@fast"
        ])
    );
    assert_eq!(
        config["backups"]["reviewer"],
        serde_json::json!(["cli:opencode@luna", "api:openai@gpt-4", "cli:agy@sonnet"])
    );
}

#[test]
fn backup_rejects_the_same_provider_storage_key_as_its_primary() {
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "reject duplicate provider",
            "--dry-run",
            "--role",
            "planner=cli:claude@opus",
            "--backup",
            "planner=cli:claude@sonnet",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("backup").and(predicate::str::contains("planner")));
}

#[test]
fn arena_rejects_role_backups_instead_of_treating_them_as_a_positional_pool() {
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--workflow",
            "arena",
            "--task",
            "compare implementations",
            "--providers",
            "cli:claude,cli:codex",
            "--dry-run",
            "--backup",
            "implementer=cli:grok",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("backup").and(predicate::str::contains("arena")));
}

#[test]
fn backups_are_new_run_only_and_cannot_mutate_a_frozen_run() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    let run = run_json(
        dir,
        &[
            "plan",
            "--task",
            "compose a run",
            "--dry-run",
            "--json",
            "--role",
            "planner=cli:claude@opus",
            "--backup",
            "planner=cli:codex@gpt-5.6-terra",
        ],
        2,
    );
    let id = run["run_id"].as_str().unwrap();

    spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--run",
            id,
            "--task",
            "replan",
            "--backup",
            "planner=cli:grok@fast",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--backup"));
    assert_eq!(
        frozen_config(dir, id)["backups"]["planner"],
        "cli:codex@gpt-5.6-terra",
        "a rejected continuation flag must leave the original frozen backup intact"
    );
}
