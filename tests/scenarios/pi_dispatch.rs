//! A `cli:pi` slot end to end against a fake `pi` on `PATH` that replays real pi
//! 1.4.0 captures (`tests/fixtures/pi/`).
//!
//! pi exits 0 when the provider fails a request; the only signal is the final
//! assistant message's `stopReason: "error"`. These pin that such a dispatch fails
//! the slot, that a 429 in that message routes the run to the quota gate, and that a
//! clean replay is billed from the stream with spar's assigned session id on argv.
use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-pi-home-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
    fs::write(
        dir.join("spar.toml"),
        "[suite]\nenabled = false\n[rounds]\nmax = 1\n",
    )
    .unwrap();
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pi")
        .join(name)
}

/// Replays `$SPAR_TEST_PI_STREAM` on stdout and exits 0, the way pi does on both
/// success and provider error. Records its argv to `$SPAR_TEST_PI_STATE/calls`.
fn install_fake_pi(bin: &Path) {
    fs::create_dir_all(bin).unwrap();
    let p = bin.join("pi");
    fs::write(
        &p,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "1.4.0"; exit 0; fi
if [ "$1" = "auth" ]; then echo '{"status":"ready","provider":"openrouter"}'; exit 0; fi
echo "$*" >> "$SPAR_TEST_PI_STATE/calls"
cat "$SPAR_TEST_PI_STREAM"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

struct Fake {
    _tmp: tempfile::TempDir,
    proj: PathBuf,
    bin: PathBuf,
    state: PathBuf,
}

fn setup() -> Fake {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let bin = tmp.path().join("bin");
    install_fake_pi(&bin);
    let state = tmp.path().join("pi-state");
    fs::create_dir_all(&state).unwrap();
    Fake {
        _tmp: tmp,
        proj,
        bin,
        state,
    }
}

fn spar_implement(f: &Fake, stream: &Path) -> assert_cmd::assert::Assert {
    let mut c = cargo_bin_cmd!("spar");
    c.env("SPAR_HOME", spar_home_dir())
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .env_remove("SPAR_PI_MODEL")
        .env(
            "PATH",
            format!(
                "{}:{}",
                f.bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("SPAR_TEST_PI_STATE", &f.state)
        .env("SPAR_TEST_PI_STREAM", stream)
        .current_dir(&f.proj)
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:pi@google/gemini-2.5-flash",
            "--fleet",
            "small",
            "--json",
        ])
        .assert()
}

fn only_run_state(proj: &Path) -> serde_json::Value {
    let runs: Vec<PathBuf> = fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(runs.len(), 1, "one run: {runs:?}");
    serde_json::from_str(&fs::read_to_string(runs[0].join("state.json")).unwrap()).unwrap()
}

fn implementer(st: &serde_json::Value) -> serde_json::Value {
    st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot")
        .clone()
}

#[test]
fn error_stop_reason_fails_the_slot_despite_exit_zero() {
    let f = setup();
    spar_implement(&f, &fixture("run3-badmodel.jsonl")).code(1);
    let slot = implementer(&only_run_state(&f.proj));
    assert_eq!(slot["status"], "failed", "{slot}");
    let err = slot["error"].as_str().unwrap_or_default();
    assert!(err.contains("not a valid model ID"), "{slot}");
    assert_ne!(slot["quota_hit"], true, "a 400 is not a quota stop: {slot}");
}

#[test]
fn error_stop_reason_with_a_429_parks_on_quota() {
    let f = setup();
    let stream = f.state.join("429.jsonl");
    fs::write(
        &stream,
        fs::read_to_string(fixture("run3-badmodel.jsonl")).unwrap().replace(
            r#"400: {\"message\":\"nonexistent/model-xyz is not a valid model ID\",\"code\":400}"#,
            "429 Too Many Requests: rate limit exceeded",
        ),
    )
    .unwrap();
    spar_implement(&f, &stream).code(4);
    let slot = implementer(&only_run_state(&f.proj));
    assert_eq!(slot["quota_hit"], true, "{slot}");
}

#[test]
fn clean_replay_is_billed_and_carries_the_assigned_session() {
    let f = setup();
    spar_implement(&f, &fixture("run1.jsonl"));
    let slot = implementer(&only_run_state(&f.proj));
    let usage = &slot["usage"];
    assert_eq!(usage["input_tokens"], 3132 + 529, "{slot}");
    assert_eq!(usage["cache_read_tokens"], 2698, "{slot}");
    assert_eq!(usage["tools"], 1, "{slot}");

    let calls = fs::read_to_string(f.state.join("calls")).unwrap();
    let first = calls.lines().next().unwrap();
    assert!(
        first.starts_with("--mode json --no-approve --session-id "),
        "{first}"
    );
    assert!(
        first.contains("--model openrouter/google/gemini-2.5-flash"),
        "{first}"
    );
}
