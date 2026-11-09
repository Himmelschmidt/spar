//! `spar bus deliver <agent> --run <run>`: drain the inbox (exactly-once) and dispatch the
//! claimed messages to the agent adapter's `DeliveryStrategy`.
//!
//! One scenario per landed strategy, all under `--dry-run` so the side-effecting
//! injection call is stubbed and only drain + dispatch are exercised:
//!   - Claude  → `StopHookInject`       (emits a Stop-hook `block` payload)
//!   - Grok    → `StopHookInject`       (same turn-boundary injection as Claude: grok's
//!     Stop hook honors the same `{"decision":"block",…}` payload per docs ch. 10;
//!     the end-to-end re-drive is not yet observed live, see O90)
//!   - agy     → `PollFile`             (fanned out on send into the poll file its
//!     prompt tells it to read before any new major step)
//!   - muse    → `StopHookInject`       (same turn-boundary injection as Claude: muse's
//!     Stop hook honors the same `{"decision":"block",…}` payload, probed live)

mod common;
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
    let mut c = common::spar();
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
    plan_and_approve_with_providers(dir, "cli:claude,cli:grok,cli:agy")
}

/// `plan`'s three roles (planner/plan_critic/test_author) zip positionally against
/// `--providers`, so a fourth entry (e.g. adding `cli:muse` alongside the usual three)
/// is silently dropped rather than picking up a role — a provider needs its own
/// three-provider list to actually land a slot.
fn plan_and_approve_with_providers(dir: &std::path::Path, providers: &str) -> String {
    let plan = spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "add a hello world module",
            "--providers",
            providers,
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

/// Grok: deliver drains once and hands the batch to the Stop-hook block channel,
/// the same turn-boundary injection Claude uses. A second deliver drains nothing
/// (exactly-once).
#[test]
fn grok_stop_hook_inject_drains_once_and_builds_block() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:grok");

    send(tmp.path(), &run_id, &agent, "review the diff");

    let d = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(d["strategy"], "stop_hook_inject");
    assert_eq!(d["action"], "stop_hook_block");
    assert!(d["delivered"].as_u64().unwrap() >= 1, "{d}");

    let payload = d["payload"].as_str().expect("block payload");
    let block: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(block["decision"], "block");
    assert!(
        block["reason"]
            .as_str()
            .unwrap()
            .contains("review the diff"),
        "{block}"
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

/// muse: resolves `StopHookInject` end to end through the real CLI + adapter lookup
/// (`main.rs`'s `agent_delivery_strategy`) — the same turn-boundary injection Claude
/// uses. This test pins the strategy/action resolving correctly end to end through the
/// real `muse` adapter; the payload shape itself is covered by the
/// `providers::delivery` unit tests.
#[test]
fn muse_stop_hook_inject_claims_and_builds_block_payload() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve_with_providers(tmp.path(), "cli:claude,cli:grok,cli:muse");
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:muse");

    send(tmp.path(), &run_id, &agent, "keep going");

    let d = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(d["strategy"], "stop_hook_inject");
    assert_eq!(d["action"], "stop_hook_block");
    assert!(d["delivered"].as_u64().unwrap() >= 1, "{d}");

    let payload = d["payload"].as_str().expect("block payload");
    let block: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(block["decision"], "block");
    assert!(
        block["reason"].as_str().unwrap().contains("keep going"),
        "{block}"
    );
}

/// agy: no push channel into the running process, so a send fans the slot's inbox
/// straight into its poll file — the file every role prompt (agy's included)
/// tells the agent to read before starting any new major step. No explicit `bus
/// deliver`: the body is in the file, the log reads delivered, and a later
/// deliver drains nothing (exactly-once).
#[test]
fn agy_send_fans_out_to_the_poll_file() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:agy");

    // Dry runs stub the poll-file write; flip the run live so this exercises the
    // real fan-out path. Nothing else in this test dispatches, so nothing else
    // can observe the flip.
    set_dry_run(tmp.path(), &run_id, false);

    send(tmp.path(), &run_id, &agent, "write the acceptance test");

    let poll = std::fs::read_to_string(
        tmp.path()
            .join(".spar/runs")
            .join(&run_id)
            .join("logs")
            .join(format!("nudges-{agent}.md")),
    )
    .unwrap();
    assert!(
        poll.contains("write the acceptance test"),
        "sent body is in the poll file with no explicit deliver"
    );

    let log = spar_cmd()
        .current_dir(tmp.path())
        .args(["bus", "log", "--run", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let entries: Value = serde_json::from_slice(&log).unwrap();
    assert!(
        entries
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["body"] == "write the acceptance test" && e["delivery"] == "delivered"),
        "{entries}"
    );

    // Exactly-once: a later deliver drains nothing.
    let d = deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(d["strategy"], "poll_file");
    assert_eq!(d["action"], "empty");
    assert_eq!(d["delivered"], 0);

    // The slot's prompt names its poll file, so the fanned-out batch is where the
    // agent is told to look before its next major step.
    let prompt = std::fs::read_to_string(
        tmp.path()
            .join(".spar/runs")
            .join(&run_id)
            .join(format!("prompt-{agent}.md")),
    )
    .unwrap();
    assert!(
        prompt.contains(&format!("nudges-{agent}.md")),
        "agy prompt names its poll file"
    );
}

/// A `requires_ack` redelivery reaches the agy poll file too: the tick re-files
/// the message into the inbox, and file-time fan-out drains it like any other
/// producer's filing — no explicit `bus deliver` needed for the copy to land.
#[test]
fn agy_redelivery_reaches_the_poll_file() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = plan_and_approve(tmp.path());
    let agent = slot_for_provider(tmp.path(), &run_id, "cli:agy");
    set_dry_run(tmp.path(), &run_id, false);

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "bus",
            "send",
            "--run",
            &run_id,
            "--to",
            &agent,
            "--requires-ack",
            "-m",
            "confirm the migration plan",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sent: Value = serde_json::from_slice(&out).unwrap();
    let msg_id = sent["id"].as_str().unwrap().to_string();

    let poll_path = tmp
        .path()
        .join(".spar/runs")
        .join(&run_id)
        .join("logs")
        .join(format!("nudges-{agent}.md"));
    let poll = || std::fs::read_to_string(&poll_path).unwrap();
    assert_eq!(
        poll().matches("confirm the migration plan").count(),
        1,
        "the send itself fans out exactly one copy"
    );

    // Make the redelivery due now by backdating its pending_ack record; the
    // message JSON comes straight from the send above.
    let pending = tmp
        .path()
        .join(".spar/bus/pending_ack")
        .join(format!("{msg_id}.json"));
    let record = serde_json::json!({
        "msg": sent,
        "attempts": 0,
        "next_at": "2000-01-01T00:00:00Z",
    });
    std::fs::write(&pending, serde_json::to_string_pretty(&record).unwrap()).unwrap();

    // `bus deliver` ticks acks before draining: the due redelivery re-files and
    // fans straight out to the poll file. The poll file renders the inbox, so the
    // same message still lists once — redelivery refreshes, never duplicates.
    deliver_json(tmp.path(), &run_id, &agent);
    assert_eq!(
        poll().matches("confirm the migration plan").count(),
        1,
        "the redelivered copy refreshes the poll file with no explicit drain"
    );

    let log = spar_cmd()
        .current_dir(tmp.path())
        .args(["bus", "log", "--run", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let entries: Value = serde_json::from_slice(&log).unwrap();
    assert!(
        entries
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["id"] == msg_id && e["delivery"] == "delivered"),
        "{entries}"
    );
}

/// Flip a run between dry-run and live by editing its state file. Only for tests
/// that need the real write path (dry runs stub poll-file writes).
fn set_dry_run(dir: &std::path::Path, run_id: &str, dry_run: bool) {
    let path = dir.join(".spar/runs").join(run_id).join("state.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut state: Value = serde_json::from_str(&text).unwrap();
    state["dry_run"] = Value::Bool(dry_run);
    std::fs::write(&path, serde_json::to_string_pretty(&state).unwrap()).unwrap();
}

/// A live run with one PollFile slot for `provider` (`cli:agy` or `cli:pi`).
fn live_poll_run(dir: &std::path::Path, provider: &str) -> (String, String) {
    let run_id = plan_and_approve_with_providers(dir, &format!("cli:claude,cli:grok,{provider}"));
    let agent = slot_for_provider(dir, &run_id, provider);
    set_dry_run(dir, &run_id, false);
    (run_id, agent)
}

fn poll_body(dir: &std::path::Path, run_id: &str, agent: &str) -> String {
    std::fs::read_to_string(
        dir.join(".spar/runs")
            .join(run_id)
            .join("logs")
            .join(format!("nudges-{agent}.md")),
    )
    .unwrap()
}

fn send_json(dir: &std::path::Path, run_id: &str, extra: &[&str]) -> Value {
    let mut args = vec!["bus", "send", "--run", run_id, "--json"];
    args.extend(extra);
    let out = spar_cmd()
        .current_dir(dir)
        .args(&args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

fn log_entries(dir: &std::path::Path, run_id: &str) -> Value {
    let out = spar_cmd()
        .current_dir(dir)
        .args(["bus", "log", "--run", run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

fn log_entry(entries: &Value, id: &str) -> Value {
    entries
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("{id} in {entries}"))
        .clone()
}

/// Withdrawing a superseded message takes it back from a PollFile slot: the
/// withdrawal reports the slot as withdrawn (not already delivered) and the
/// old body disappears from the poll file, leaving only the superseding one.
fn poll_file_supersede_withdraws(provider: &str) {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let (run_id, agent) = live_poll_run(tmp.path(), provider);
    let dir = tmp.path();

    let old = send_json(
        dir,
        &run_id,
        &["--to", &agent, "-m", "old instruction alpha"],
    );
    let old_id = old["id"].as_str().unwrap().to_string();
    assert!(
        poll_body(dir, &run_id, &agent).contains("old instruction alpha"),
        "the send itself fans out to the poll file"
    );

    let new = send_json(
        dir,
        &run_id,
        &[
            "--to",
            &agent,
            "--supersedes",
            &old_id,
            "-m",
            "new instruction beta",
        ],
    );
    let new_id = new["id"].as_str().unwrap().to_string();
    let withdrawal = &new["withdrawal"];
    assert_eq!(
        withdrawal["withdrawn_from"],
        serde_json::json!([format!("{run_id}:{agent}")]),
        "{withdrawal}"
    );
    assert_eq!(
        withdrawal["already_delivered"],
        serde_json::json!([]),
        "{withdrawal}"
    );

    let poll = poll_body(dir, &run_id, &agent);
    assert!(
        !poll.contains("old instruction alpha"),
        "withdrawn body leaves the file:\n{poll}"
    );
    assert!(poll.contains("new instruction beta"), "{poll}");

    let entries = log_entries(dir, &run_id);
    let old_entry = log_entry(&entries, &old_id);
    assert_eq!(old_entry["superseded_by"], new_id, "{old_entry}");
    assert_eq!(
        old_entry["delivery"], "delivered",
        "it landed in the poll file before the withdrawal took it back: {old_entry}"
    );
}

#[test]
fn poll_file_supersede_withdraws_for_agy() {
    poll_file_supersede_withdraws("cli:agy");
}

#[test]
fn poll_file_supersede_withdraws_for_pi() {
    poll_file_supersede_withdraws("cli:pi");
}

/// A message filed in its last valid round leaves the poll file once the run
/// moves past it: the next filing re-renders, and the expired body is gone.
fn poll_file_expiry_drops(provider: &str) {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let (run_id, agent) = live_poll_run(tmp.path(), provider);
    let dir = tmp.path();

    let expiring = send_json(
        dir,
        &run_id,
        &[
            "--to",
            &agent,
            "--expires-after-round",
            "-m",
            "limited-time ruling",
        ],
    );
    let expiring_id = expiring["id"].as_str().unwrap().to_string();
    let last_round = expiring["expires_after_round"].as_u64().unwrap();
    assert!(
        poll_body(dir, &run_id, &agent).contains("limited-time ruling"),
        "filed in its last valid round, so it lists"
    );

    let state_path = dir.join(".spar/runs").join(&run_id).join("state.json");
    let text = std::fs::read_to_string(&state_path).unwrap();
    let mut state: Value = serde_json::from_str(&text).unwrap();
    state["round"] = serde_json::json!(last_round + 1);
    std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    send_json(dir, &run_id, &["--to", &agent, "-m", "later note"]);
    let poll = poll_body(dir, &run_id, &agent);
    assert!(
        !poll.contains("limited-time ruling"),
        "expired body leaves the file:\n{poll}"
    );
    assert!(poll.contains("later note"), "{poll}");

    let entries = log_entries(dir, &run_id);
    let entry = log_entry(&entries, &expiring_id);
    assert_eq!(entry["expired"], true, "{entry}");
}

#[test]
fn poll_file_expiry_drops_for_agy() {
    poll_file_expiry_drops("cli:agy");
}

#[test]
fn poll_file_expiry_drops_for_pi() {
    poll_file_expiry_drops("cli:pi");
}

/// Acks and broadcasts stay in the inbox and the bus log, but they never become
/// poll-file action items; an acked message leaves the file too.
#[test]
fn poll_file_lists_only_directed_action_items() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let (run_id, agent) = live_poll_run(tmp.path(), "cli:agy");
    let dir = tmp.path();

    let needy = send_json(
        dir,
        &run_id,
        &[
            "--to",
            &agent,
            "--requires-ack",
            "-m",
            "confirm the migration plan",
        ],
    );
    let needy_id = needy["id"].as_str().unwrap().to_string();
    assert!(poll_body(dir, &run_id, &agent).contains("confirm the migration plan"));

    send_json(
        dir,
        &run_id,
        &["--to", "broadcast", "-m", "all hands notice"],
    );
    spar_cmd()
        .current_dir(dir)
        .args(["bus", "ack", &needy_id, "--from", &agent, "--run", &run_id])
        .assert()
        .success();
    deliver_json(dir, &run_id, &agent);

    let poll = poll_body(dir, &run_id, &agent);
    assert!(
        !poll.contains("all hands notice"),
        "broadcasts are not action items:\n{poll}"
    );
    assert!(
        !poll.contains("[Ack]"),
        "acks are not action items:\n{poll}"
    );
    assert!(
        !poll.contains("confirm the migration plan"),
        "an acked message leaves the file:\n{poll}"
    );
}
