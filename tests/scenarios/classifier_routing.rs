//! O148 end to end: a provider fails with a quota wording no regex knows, the classifier
//! (a local fake System One endpoint, never the network) reads it as quota, and the run
//! parks at `Phase::Quota` with the provider strongly paused and the call logged.

mod common;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

const NOVEL_QUOTA: &str =
    "Error: this workspace has used up its included agent hours; more become available on the 1st.";

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

fn fake_claude(bin_dir: &std::path::Path) -> String {
    std::fs::create_dir_all(bin_dir).unwrap();
    let fake = bin_dir.join("claude");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho '{NOVEL_QUOTA}' >&2\nexit 1\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Answers every `POST .../systemone` with a quota verdict and records request bodies.
fn fake_system_one() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut buf = vec![0u8; len];
                let _ = reader.read_exact(&mut buf);
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let body = serde_json::json!({
                    "answers": {"failure_class": {
                        "type": "choice",
                        "choice": "quota_or_rate_limit",
                        "probabilities": {
                            "quota_or_rate_limit": 0.93, "auth": 0.02, "transient_infra": 0.02,
                            "killed_by_operator_or_signal": 0.01, "genuine_failure": 0.01, "other": 0.01
                        },
                        "confidence": 0.93
                    }},
                    "usage": {"input_tokens": 900, "output_tokens": 0}
                })
                .to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (format!("http://{addr}/api/v1"), bodies)
}

#[test]
fn a_novel_quota_wording_parks_on_quota_through_the_classifier() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude(&tmp.path().join("bin"));
    let (base_url, bodies) = fake_system_one();

    let key_file = tmp.path().join("openrouter.key");
    std::fs::write(&key_file, "sk-or-v1-scenario-fake\n").unwrap();
    let user_cfg = common::test_root().join("home/.config/spar");
    std::fs::create_dir_all(&user_cfg).unwrap();
    std::fs::write(
        user_cfg.join("config.toml"),
        format!(
            "[classifier]\nenabled = true\nbase_url = \"{base_url}\"\nkey_file = \"{}\"\ntimeout_secs = 2\n",
            key_file.display()
        ),
    )
    .unwrap();

    common::spar()
        .current_dir(&proj)
        .env_remove("SPAR_DRY_RUN")
        .env_remove("OPENROUTER_API_KEY")
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let runs = proj.join(".spar/runs");
    let run_dir = std::fs::read_dir(&runs)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .expect("a run")
        .path();
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("state.json")).unwrap())
            .unwrap();
    assert_eq!(state["phase"], "quota");
    let slot = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot");
    assert_eq!(slot["quota_hit"], true);
    assert_eq!(slot["classifier"]["action"], "pause_quota");

    let quota: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(proj.join(".spar/quota.json")).unwrap())
            .unwrap();
    let claude = &quota["providers"]["cli:claude"];
    assert_eq!(claude["status"], "paused_quota");
    assert_eq!(claude["strong"], true);

    let events = std::fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
    let acted: Vec<serde_json::Value> = events
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["type"] == "classifier" && e["classifier"]["acted"] == true)
        .collect();
    assert_eq!(acted.len(), 1, "{events}");
    let c = &acted[0]["classifier"];
    assert_eq!(c["model"], "typesafe/jev-1.14");
    assert_eq!(c["choice"], "quota_or_rate_limit");
    assert_eq!(c["regex_verdict"], "work");
    assert!(!events.contains("sk-or-v1-scenario-fake"));

    let sent = bodies.lock().unwrap();
    assert!(!sent.is_empty());
    let req: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(req["state"]["provider"], "cli:claude");
    assert!(req["state"]["log_tail"]
        .as_str()
        .unwrap()
        .contains("included agent hours"));
}

/// The control: with the classifier off (the default) the same failure is the work's.
#[test]
fn with_the_classifier_off_the_same_failure_fails_the_run() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude(&tmp.path().join("bin"));

    common::spar()
        .current_dir(&proj)
        .env_remove("SPAR_DRY_RUN")
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(1);
    assert!(
        !proj.join(".spar/quota.json").is_file()
            || !std::fs::read_to_string(proj.join(".spar/quota.json"))
                .unwrap()
                .contains("paused_quota")
    );
}
