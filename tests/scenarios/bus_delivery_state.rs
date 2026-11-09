//! Per-message delivery state in `bus log` (issue #231, part (a)): every entry
//! carries `delivery` — `queued` while a copy still waits in an inbox,
//! `delivered` once it has landed in the recipient's channel, `acknowledged`
//! once an ack answers it — in both `--json` and human output. Derived from the
//! receipts plus the ack events at read time; nothing new is stored.

mod common;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::{tempdir, TempDir};

struct Env {
    home: TempDir,
    bin: PathBuf,
}

fn env() -> Env {
    let home = tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let git = which_git();
    std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
    Env { home, bin }
}

fn which_git() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

fn spar(env: &Env, dir: &Path) -> assert_cmd::Command {
    let mut c = common::spar();
    c.current_dir(dir)
        .env("PATH", &env.bin)
        .env("HOME", env.home.path())
        .env("SPAR_HOME", env.home.path().join(".spar-home"))
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .env_remove("SPAR_DRY_RUN")
        .env_remove("SPAR_FORCE_REQUEST_CHANGES");
    c
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn json(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_slice(out.get_output().stdout.as_slice()).expect("json on stdout")
}

fn implemented_run(env: &Env, dir: &Path) -> String {
    std::fs::write(dir.join("spar.toml"), "[rounds]\nmax = 3\n").unwrap();
    let plan = spar(env, dir)
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
    let run_id = json(&plan)["run_id"].as_str().unwrap().to_string();
    spar(env, dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar(env, dir)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert();
    run_id
}

fn send(env: &Env, dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut all = vec!["bus", "send", "--json"];
    all.extend_from_slice(args);
    json(&spar(env, dir).args(&all).assert().success())
}

fn log_json(env: &Env, dir: &Path, run: &str) -> Vec<serde_json::Value> {
    json(
        &spar(env, dir)
            .args(["bus", "log", "--run", run, "--json"])
            .assert()
            .success(),
    )
    .as_array()
    .unwrap()
    .clone()
}

fn log_text(env: &Env, dir: &Path, run: &str) -> String {
    let out = spar(env, dir)
        .args(["bus", "log", "--run", run])
        .assert()
        .success();
    String::from_utf8_lossy(&out.get_output().stdout).to_string()
}

fn entry(entries: &[serde_json::Value], id: &str) -> serde_json::Value {
    entries
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("{id} in {entries:?}"))
        .clone()
}

fn line_for(text: &str, id: &str) -> String {
    text.lines()
        .find(|l| l.contains(id))
        .unwrap_or_else(|| panic!("{id} in:\n{text}"))
        .to_string()
}

/// A plain message reads `queued` until claimed, `delivered` after; a
/// `requires_ack` message reads `delivered` while it waits for its ack and
/// `acknowledged` once the ack lands — identically in `--json` and human output.
#[test]
fn bus_log_reports_queued_delivered_acknowledged() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());
    let dir = tmp.path();

    let plain = send(
        &env,
        dir,
        &["--run", &run, "--to", "impl", "-m", "plain hello"],
    );
    let plain_id = plain["id"].as_str().unwrap().to_string();
    let needy = send(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--requires-ack",
            "-m",
            "confirm this",
        ],
    );
    let needy_id = needy["id"].as_str().unwrap().to_string();

    let entries = log_json(&env, dir, &run);
    assert_eq!(entry(&entries, &plain_id)["delivery"], "queued");
    assert_eq!(entry(&entries, &needy_id)["delivery"], "queued");
    let text = log_text(&env, dir, &run);
    assert!(line_for(&text, &plain_id).contains("[queued"), "{text}");
    assert!(line_for(&text, &needy_id).contains("[queued"), "{text}");

    spar(&env, dir)
        .args(["bus", "inbox", "impl", "--run", &run, "--claim"])
        .assert()
        .success();
    let entries = log_json(&env, dir, &run);
    assert_eq!(entry(&entries, &plain_id)["delivery"], "delivered");
    assert_eq!(
        entry(&entries, &needy_id)["delivery"],
        "delivered",
        "claimed but unacked is landed, not confirmed"
    );
    let text = log_text(&env, dir, &run);
    assert!(line_for(&text, &plain_id).contains("[delivered"), "{text}");

    spar(&env, dir)
        .args(["bus", "ack", &needy_id, "--from", "impl", "--run", &run])
        .assert()
        .success();
    let entries = log_json(&env, dir, &run);
    assert_eq!(
        entry(&entries, &plain_id)["delivery"],
        "delivered",
        "an unacked message is never confirmed by someone else's ack"
    );
    assert_eq!(entry(&entries, &needy_id)["delivery"], "acknowledged");
    let text = log_text(&env, dir, &run);
    assert!(
        line_for(&text, &needy_id).contains("[acknowledged"),
        "{text}"
    );
}
