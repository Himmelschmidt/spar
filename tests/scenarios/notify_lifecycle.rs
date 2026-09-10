//! `RunState::save` pushes a lifecycle alert into the `[notify]` sink on gate / stuck /
//! quota / terminal-failure transitions. Silence means healthy. This exercises the
//! real (non-hermetic) delivery path end to end: a real `[notify] command` sink, a
//! real (non-`--dry-run`) run so `SPAR_DRY_RUN` never trips the guard in `notify::fire`,
//! forced into `Phase::Quota` by pausing its only provider — deterministic, offline,
//! and fast, with no real provider process ever spawned.
//!
//! `[notify]` may only come from the *user*-level config (`config.rs`'s `Trust::User`
//! gate: a cloned project's `spar.toml` must never be able to shell out), so each test
//! points `HOME` at its own throwaway directory and writes
//! `<HOME>/.config/spar/config.toml` directly.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-notify-home-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

/// A real (non-dry-run) invocation needs `SPAR_DRY_RUN` to stay unset — unlike every
/// other scenario file's `spar_cmd`, which sets it via `--dry-run` to stay hermetic.
fn spar_cmd(user_home: &std::path::Path) -> assert_cmd::Command {
    let mut c = cargo_bin_cmd!("spar");
    c.env("SPAR_HOME", spar_home_dir());
    c.env("HOME", user_home);
    c.env_remove("XDG_CONFIG_HOME");
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c.env_remove("SPAR_DRY_RUN");
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

/// `<HOME>/.config/spar/config.toml` with `[notify] command = "cat > <out>"`, so a
/// fired alert's JSON body lands in `out` for inspection.
fn write_user_notify_config(home: &std::path::Path, out: &std::path::Path) {
    let dir = home.join(".config/spar");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!("[notify]\ncommand = \"cat > {}\"\n", out.display()),
    )
    .unwrap();
}

fn wait_for(path: &std::path::Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.is_file()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Pausing the run's only provider makes `quota::ensure_usable` fail before any
/// provider process spawns, parking the run at `Phase::Quota` through a real
/// (non-dry-run) `state.save`. The fired alert's `[notify] command` sink must receive
/// a JSON body naming the `quota` event.
#[test]
fn reaching_quota_fires_a_real_notify_command() {
    let user_home = tempdir().unwrap();
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let out = tmp.path().join("out.json");
    write_user_notify_config(user_home.path(), &out);

    spar_cmd(user_home.path())
        .current_dir(&proj)
        .args(["provider", "pause", "cli:claude"])
        .assert()
        .success();

    spar_cmd(user_home.path())
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--json",
        ])
        .assert()
        .code(4);

    assert!(
        wait_for(&out, Duration::from_secs(3)),
        "notify command never received a body"
    );
    let text = std::fs::read_to_string(&out).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("valid json body");
    assert_eq!(v["meta"]["event"], "quota");
    assert_eq!(v["to"], "@human");
}

/// A run's config is frozen at creation (O27): a `[notify]` sink added to the user
/// config *after* the run was created must not fire for it without `--reload-config`.
#[test]
fn a_notify_sink_added_after_the_run_was_created_does_not_fire_without_reload() {
    let user_home = tempdir().unwrap();
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let out = tmp.path().join("out.json");
    // No [notify] configured yet when the run is created.

    let plan = spar_cmd(user_home.path())
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
    let run_id: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&plan.get_output().stdout)).unwrap();
    let run_id = run_id["run_id"].as_str().unwrap().to_string();

    // Now wire the sink — too late for this run, whose `config.json` was already
    // frozen without it.
    write_user_notify_config(user_home.path(), &out);

    spar_cmd(user_home.path())
        .current_dir(&proj)
        .args(["approve", &run_id, "--json"])
        .assert()
        .code(0);
    // Drive it on to another gate (a fresh notify-eligible transition on this same,
    // still notify-less-by-snapshot run).
    let _ = spar_cmd(user_home.path())
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert();

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !out.exists(),
        "a pre-existing run must not pick up a sink added after creation"
    );
}
