//! A run is bound to the config it was created with.
//!
//! `spar.toml` is one mutable file per project, read fresh by every process. Parallel
//! agents each write their own `[roles]` into it, so without a per-run snapshot one
//! agent's edit silently re-fleets another agent's in-flight run.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

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
    // Also isolate the *config* dir. SPAR_HOME only moves the registry; without this
    // the spawned binary still layers the developer's ~/.config/spar/config.toml under
    // the test's project, so an ordinary local setting fails scenarios that never
    // mention it. (XDG applies on Linux, where the suite runs.)
    c.env("XDG_CONFIG_HOME", spar_home_dir());
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn init_repo(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
}

fn write_roles(dir: &std::path::Path, provider: &str) {
    std::fs::write(
        dir.join("spar.toml"),
        format!(
            "[roles]\nplanner = \"{provider}\"\nplan_critic = \"{provider}\"\n\
             implementer = \"{provider}\"\ntest_author = \"{provider}\"\n\
             reviewer = [\"{provider}\"]\n"
        ),
    )
    .unwrap();
}

fn json_of(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
        .expect("json")
}

fn providers_of(v: &serde_json::Value) -> Vec<String> {
    v["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|p| p.as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn a_concurrent_spar_toml_rewrite_cannot_refleet_an_in_flight_run() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_roles(dir, "cli:grok");

    let plan = spar_cmd()
        .current_dir(dir)
        .args(["plan", "--task", "task A", "--dry-run", "--json"])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    assert!(
        dir.join(".spar/runs")
            .join(&run_id)
            .join("config.json")
            .is_file(),
        "run creation must freeze the config"
    );

    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();

    // A second agent claims the shared file for its own run.
    write_roles(dir, "cli:codex");

    let resumed = spar_cmd()
        .current_dir(dir)
        .args(["implement", "--run", &run_id, "--dry-run", "--json"])
        .assert()
        .code(2);
    let got = providers_of(&json_of(&resumed));
    assert!(
        got.iter().any(|p| p.starts_with("cli:grok")),
        "resume must use the run's own fleet, got {got:?}"
    );
    assert!(
        !got.iter().any(|p| p.starts_with("cli:codex")),
        "the other agent's spar.toml must not reach this run, got {got:?}"
    );
}

#[test]
fn reload_config_is_the_way_to_pick_up_the_file_again() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_roles(dir, "cli:grok");

    let plan = spar_cmd()
        .current_dir(dir)
        .args(["plan", "--task", "task A", "--dry-run", "--json"])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();

    write_roles(dir, "cli:codex");
    let reloaded = spar_cmd()
        .current_dir(dir)
        .args([
            "implement",
            "--run",
            &run_id,
            "--dry-run",
            "--json",
            "--reload-config",
        ])
        .assert()
        .code(2);
    let got = providers_of(&json_of(&reloaded));
    assert!(
        got.iter().any(|p| p.starts_with("cli:codex")),
        "--reload-config must re-read spar.toml, got {got:?}"
    );

    // And it re-freezes: a later resume without the flag keeps the reloaded fleet.
    write_roles(dir, "cli:grok");
    let after = spar_cmd()
        .current_dir(dir)
        .args(["implement", "--run", &run_id, "--dry-run", "--json"])
        .assert()
        .code(2);
    let got = providers_of(&json_of(&after));
    assert!(
        got.iter().any(|p| p.starts_with("cli:codex")),
        "reload must replace the snapshot, not bypass it once, got {got:?}"
    );
}

/// The reason agents were editing the shared file at all: `--providers` is positional
/// and cannot express per-role assignment, so the documented launch path was to write
/// `[roles]` into `spar.toml`. `--role` removes that need entirely.
#[test]
fn role_flag_fleets_a_run_without_touching_the_project_file() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    let plan = spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "task A",
            "--dry-run",
            "--json",
            "--role",
            "planner=cli:grok",
            "--role",
            "plan_critic=cli:claude@opus",
            "--role",
            "test_author=cli:grok",
        ])
        .assert()
        .code(2);
    let got = providers_of(&json_of(&plan));
    assert_eq!(got, vec!["cli:grok", "cli:claude@opus", "cli:grok"]);
    assert!(
        !dir.join("spar.toml").exists(),
        "--role must never write the shared project config"
    );

    // Rejected input, and never silently ignored.
    spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "x",
            "--dry-run",
            "--json",
            "--role",
            "nope=cli:grok",
        ])
        .assert()
        .code(1);
    spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "x",
            "--dry-run",
            "--json",
            "--role",
            "planner=not-a-provider",
        ])
        .assert()
        .code(1);
}

#[test]
fn role_on_a_resume_requires_reload_config() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    write_roles(dir, "cli:grok");

    let plan = spar_cmd()
        .current_dir(dir)
        .args(["plan", "--task", "task A", "--dry-run", "--json"])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();

    spar_cmd()
        .current_dir(dir)
        .args([
            "implement",
            "--run",
            &run_id,
            "--dry-run",
            "--json",
            "--role",
            "implementer=cli:codex",
        ])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("--reload-config"));
}
