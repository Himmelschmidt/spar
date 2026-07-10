//! Slot liveness surfaced through `spar status --json`.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
use std::process::Command;
use tempfile::tempdir;

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

#[test]
fn dry_run_status_reports_pid_liveness() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    let out = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "implement",
            "--task",
            "liveness check",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let st = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["status", run_id, "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let sv: Value = serde_json::from_slice(&st).unwrap();
    let slots = sv["slots"].as_array().expect("slots array");
    assert!(!slots.is_empty(), "expected slots in status");

    for slot in slots {
        assert!(
            slot.get("pid_alive").is_some(),
            "every slot must expose pid_alive: {slot}"
        );
        if slot["status"] == "done" {
            assert_eq!(
                slot["pid_alive"],
                Value::Bool(false),
                "a done slot must not report a live process: {slot}"
            );
        }
    }
    assert!(
        slots.iter().any(|s| s["status"] == "done"),
        "expected at least one done slot"
    );
}
