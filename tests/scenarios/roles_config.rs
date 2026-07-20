//! Role-keyed fleet scenarios (Priority 9): a project `[roles]` block satisfies the
//! provider invariant and assigns slots by role, explicit `--providers` still overrides
//! positionally, a bad ref fails cleanly, and reviewer widening draws from the role list.
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

fn init_git_repo(dir: &std::path::Path) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(dir)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .status()
        .unwrap();
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

/// A `[roles]` block whose per-role assignment differs from `[providers].order`
/// (`cli:claude, cli:grok, cli:agy`), so a positional fallback and a role-keyed one
/// produce visibly different slot providers.
const ROLES_TOML: &str = r#"
[roles]
planner = "cli:grok"
plan_critic = "cli:claude"
implementer = "cli:agy"
reviewer = ["cli:claude", "cli:grok"]
tester = "cli:agy"
test_author = "cli:agy"
"#;

fn write_config(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join("spar.toml"), body).unwrap();
}

fn read_state(dir: &std::path::Path, run_id: &str) -> serde_json::Value {
    let text =
        std::fs::read_to_string(dir.join(".spar/runs").join(run_id).join("state.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn slot_provider(state: &serde_json::Value, role: &str) -> Option<String> {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == role)
        .and_then(|s| s["provider"].as_str())
        .map(|s| s.to_string())
}

/// A populated `[roles]` block is a third way to satisfy the
/// "`--providers` or `--select` is required" invariant: no `--providers` here.
#[test]
fn roles_config_satisfies_provider_invariant() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    write_config(tmp.path(), ROLES_TOML);

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2) // awaiting_plan_approval (manual autonomy), NOT a "--providers required" error
        .stdout(predicate::str::contains("awaiting_plan_approval"));
}

/// Slots carry the provider named for each role, not the positional `[providers].order`.
#[test]
fn roles_config_assigns_by_role() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    write_config(tmp.path(), ROLES_TOML);

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let state = read_state(tmp.path(), run_id);
    // Positional order would give planner=cli:claude, plan_critic=cli:grok; [roles] swaps them.
    assert_eq!(
        slot_provider(&state, "planner").as_deref(),
        Some("cli:grok"),
        "planner must come from [roles].planner, not [providers].order[0]"
    );
    assert_eq!(
        slot_provider(&state, "plan_critic").as_deref(),
        Some("cli:claude"),
        "plan_critic must come from [roles].plan_critic"
    );
    assert_eq!(
        slot_provider(&state, "test_author").as_deref(),
        Some("cli:agy"),
        "test_author must come from [roles].test_author"
    );
}

/// Explicit `--providers` is a positional one-off override and beats a populated `[roles]`.
#[test]
fn explicit_providers_override_roles() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    write_config(tmp.path(), ROLES_TOML);

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--providers",
            "cli:agy,cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let state = read_state(tmp.path(), run_id);
    // Positional: slot 0 = cli:agy, slot 1 = cli:claude — overriding [roles].planner=cli:grok.
    assert_eq!(
        slot_provider(&state, "planner").as_deref(),
        Some("cli:agy"),
        "explicit --providers must win positionally over [roles]"
    );
    assert_eq!(
        slot_provider(&state, "plan_critic").as_deref(),
        Some("cli:claude")
    );
}

/// A malformed ref in `[roles]` exits non-zero naming the role — a clean error, not a panic.
#[test]
fn bad_role_ref_errors_cleanly() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    // `claude` is missing the required `cli:`/`api:` prefix.
    write_config(tmp.path(), "[roles]\nimplementer = \"claude\"\n");

    spar_cmd()
        .current_dir(tmp.path())
        .args(["plan", "--task", "x", "--dry-run", "--json"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("implementer").and(predicate::str::contains("panic").not()),
        );
}

/// Drive the stuck ladder and assert the widened reviewer is drawn from `[roles].reviewer`
/// (here `api:openai`, which never appears in the old hardcoded default or `[providers].order`).
#[test]
fn reviewer_widening_draws_from_role_list() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    write_config(
        tmp.path(),
        r#"
[suite]
enabled = false

[roles]
implementer = "cli:agy"
reviewer = ["cli:claude", "cli:grok", "api:openai"]
"#,
    );

    let out = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--task",
            "force stuck path",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(3) // stuck
        .stdout(predicate::str::contains("stuck"));
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let state = read_state(tmp.path(), run_id);
    let reviewer_provs: Vec<String> = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == "reviewer")
        .filter_map(|s| s["provider"].as_str().map(|p| p.to_string()))
        .collect();
    assert!(
        reviewer_provs.iter().any(|p| p == "api:openai"),
        "widened reviewer must be drawn from [roles].reviewer (expected api:openai), got {reviewer_provs:?}"
    );
}
