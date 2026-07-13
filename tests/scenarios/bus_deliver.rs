//! `spar bus deliver <run> <agent>`: drain the inbox (exactly-once) and dispatch the
//! claimed messages to the agent adapter's `DeliveryStrategy`.
//!
//! One scenario per landed strategy, all under `--dry-run` so the side-effecting
//! injection call is stubbed and only drain + dispatch are exercised:
//!   - Claude  → `StopHookInject`  (emits a Stop-hook `block` payload)
//!   - Grok    → `NativeQueue`     (dispatches to the durable turn-boundary queue)
//!   - agy     → `None`            (inbox left untouched for the agent's next turn)
//!
//! `SdkPrompt` (opencode) has no adapter yet, so its dispatch is covered by the
//! `providers::delivery` unit tests rather than end-to-end here.
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
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
    c
}

fn init_git_repo(dir: &std::path::Path) {
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .unwrap();
    }
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .unwrap();
    std::process::Command::new("git")
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
            "add a hello world module",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&plan).unwrap();
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

/// First slot whose provider ref matches `provider` (e.g. `cli:claude`).
fn slot_for_provider(dir: &std::path::Path, run_id: &str, provider: &str) -> String {
    let p = dir.join(".spar/runs").join(run_id).join("state.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["provider"] == provider)
        .unwrap_or_else(|| panic!("no slot with provider {provider}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn send(dir: &std::path::Path, run_id: &str, to: &str, body: &str) {
    spar_cmd()
        .current_dir(dir)
        .args(["bus", "send", run_id, "--to", to, "-m", body])
        .assert()
        .success();
}

fn deliver_json(dir: &std::path::Path, run_id: &str, agent: &str) -> Value {
    let out = spar_cmd()
        .current_dir(dir)
        .args(["bus", "deliver", run_id, agent, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

/// Claude: deliver drains the inbox once and hands the batch to the Stop-hook block
/// channel. A second deliver drains nothing (exactly-once).
#[test]
fn stop_hook_inject_drains_once_and_builds_block() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:claude");

    send(tmp.path(), &run_id, &agent, "please rebase onto main");

    let d = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(d["strategy"], "stop_hook_inject");
    assert_eq!(d["action"], "stop_hook_block");
    assert!(d["delivered"].as_u64().unwrap() >= 1, "{d}");

    // The payload is a well-formed Claude Stop-hook block carrying the message.
    let payload = d["payload"].as_str().expect("block payload");
    let block: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(block["decision"], "block");
    assert!(
        block["reason"]
            .as_str()
            .unwrap()
            .contains("please rebase onto main"),
        "{block}"
    );

    // Exactly-once: nothing left to drain.
    let again = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(again["action"], "empty");
    assert_eq!(again["delivered"], 0);
}

/// Claude hook mode (no `--json`): stdout carries only the raw block JSON so Claude's
/// hook runner can consume it; no operator-report keys leak onto stdout.
#[test]
fn stop_hook_inject_hook_mode_emits_only_payload() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:claude");

    send(tmp.path(), &run_id, &agent, "ping from hook mode");

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args(["bus", "deliver", &run_id, &agent])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let block: Value = serde_json::from_str(text.trim()).expect("stdout is a single block JSON");
    assert_eq!(block["decision"], "block");
    assert!(
        block.get("strategy").is_none(),
        "no report keys in hook mode"
    );
}

/// Grok: deliver drains once and dispatches to the native-queue strategy. Under
/// dry-run the queue write is stubbed, but the drain is real (exactly-once).
#[test]
fn native_queue_drains_once_and_dispatches() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:grok");

    send(tmp.path(), &run_id, &agent, "review the diff");

    let d = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(d["strategy"], "native_queue");
    assert_eq!(d["action"], "queued");
    assert!(d["delivered"].as_u64().unwrap() >= 1, "{d}");
    assert!(
        d.get("payload").is_none(),
        "queue strategy emits no stdout payload"
    );

    let again = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(again["action"], "empty");
    assert_eq!(again["delivered"], 0);
}

/// agy: no injection channel, so deliver must NOT consume the inbox — the agent reads
/// it itself on its next turn. `--claim` afterwards still returns the message.
#[test]
fn none_leaves_inbox_for_agent_to_claim() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:agy");

    send(tmp.path(), &run_id, &agent, "write the acceptance test");

    let d = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(d["strategy"], "none");
    assert_eq!(d["action"], "left_for_inbox");
    assert_eq!(d["delivered"], 0);
    assert!(d["pending"].as_u64().unwrap() >= 1, "{d}");

    // The agent's own claim still finds the message (deliver did not strand it).
    let claimed = spar_cmd()
        .current_dir(tmp.path())
        .args(["bus", "inbox", &run_id, &agent, "--claim", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let msgs: Value = serde_json::from_slice(&claimed).unwrap();
    assert!(
        msgs.as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"] == "write the acceptance test"),
        "{msgs}"
    );
}
