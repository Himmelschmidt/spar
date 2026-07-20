//! `spar bus deliver <agent> --run <run>`: drain the inbox (exactly-once) and dispatch the
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
        .args(["bus", "send", "--run", run_id, "--to", to, "-m", body])
        .assert()
        .success();
}

fn deliver_json(dir: &std::path::Path, run_id: &str, agent: &str) -> Value {
    let out = spar_cmd()
        .current_dir(dir)
        .args(["bus", "deliver", agent, "--run", run_id, "--json"])
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
        .args(["bus", "deliver", &agent, "--run", &run_id])
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

/// Two concurrent runs in one project share a deterministic *role* id (same provider/role),
/// but their unique bus ids differ (`run_a:role` vs `run_b:role`) so a collision is
/// structurally impossible: each run's deliver drains its own inbox directory, and neither
/// can reach the other's. `deliver`/`send` accept the short role id + `--run` and resolve
/// the unique id internally.
#[test]
fn deliver_is_run_scoped_across_identical_slot_ids() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    let run_a = plan_and_approve(tmp.path());
    let run_b = plan_and_approve(tmp.path());
    let agent_a = slot_for_provider(tmp.path(), &run_a, "cli:claude");
    let agent_b = slot_for_provider(tmp.path(), &run_b, "cli:claude");
    // Premise: the role id collides across the two runs (deterministic per provider/role).
    assert_eq!(agent_a, agent_b, "role ids must collide for this scenario");
    let agent = agent_a;

    send(tmp.path(), &run_a, &agent, "message for run A");
    send(tmp.path(), &run_b, &agent, "message for run B");

    // The two runs resolve to distinct unique bus ids, so they land in distinct inbox dirs.
    let inbox_root = tmp.path().join(".spar/bus/inbox");
    let ia = inbox_root.join(format!("{run_a}:{agent}"));
    let ib = inbox_root.join(format!("{run_b}:{agent}"));
    assert_ne!(ia, ib, "unique ids must differ across runs");
    assert!(ia.is_dir() && ib.is_dir(), "each run has its own inbox dir");

    // Run B drains only its own run's traffic; run A's message is in a different inbox.
    // (Planning emits run-tagged broadcasts to the slot, so `delivered` is >1 — the point
    // is that run A's message is never among them.)
    let db = deliver_json(tmp.path(), &run_b, &agent);
    let reason_b = block_reason(&db);
    assert!(reason_b.contains("message for run B"), "{reason_b}");
    assert!(
        !reason_b.contains("message for run A"),
        "run B stole run A's message: {reason_b}"
    );

    // Run A's message survived B's drain and is still deliverable under run A.
    let da = deliver_json(tmp.path(), &run_a, &agent);
    let reason_a = block_reason(&da);
    assert!(reason_a.contains("message for run A"), "{reason_a}");
    assert!(
        !reason_a.contains("message for run B"),
        "run A drained run B's message: {reason_a}"
    );
}

/// Extract the Stop-hook block `reason` string from a `deliver --json` report.
fn block_reason(deliver: &Value) -> String {
    deliver["payload"]
        .as_str()
        .and_then(|p| serde_json::from_str::<Value>(p).ok())
        .map(|v| v["reason"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default()
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

    // The agent's own claim still finds the message (deliver did not strand it). The
    // slot scopes the claim to its run (`--run`), matching how a run slot self-drains.
    let claimed = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "bus", "inbox", &agent, "--claim", "--run", &run_id, "--json",
        ])
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
