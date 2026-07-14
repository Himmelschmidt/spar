//! End-to-end dry-run scenarios (no live AI).
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

fn primary_branch(dir: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn plan_approve_implement_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let branch_before = primary_branch(tmp.path());

    let plan = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "add a hello world module",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_plan_approval"));

    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let run_id = v["run_id"].as_str().expect("run_id");

    let plan_path = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/plan.md");
    assert!(plan_path.is_file(), "plan.md should exist");
    let contract_path = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/test-contract.md");
    assert!(
        contract_path.is_file(),
        "test-contract.md should exist after plan (spec channel)"
    );
    let state_after_plan: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            tmp.path()
                .join(".spar/runs")
                .join(run_id)
                .join("state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let has_test_author = state_after_plan["slots"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["role"] == "test_author");
    assert!(has_test_author, "expected test_author slot after plan");
    let events_path = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("events.jsonl");
    assert!(events_path.is_file(), "events.jsonl should exist");
    let events_text = std::fs::read_to_string(&events_path).unwrap();
    assert!(
        events_text.contains("awaiting_plan_approval") || events_text.contains("\"phase\""),
        "events should record phases"
    );
    let done_marker = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("markers")
        .join("planner-claude.done");
    // planner id may be planner-{provider}; at least one done marker
    let markers = tmp.path().join(".spar/runs").join(run_id).join("markers");
    let has_done = std::fs::read_dir(&markers)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".done"));
    assert!(
        has_done,
        "expected done markers under {}",
        markers.display()
    );
    let _ = done_marker;

    assert_eq!(primary_branch(tmp.path()), branch_before);

    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", run_id, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("plan_approved"));

    let impl_out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));

    let impl_stdout = String::from_utf8_lossy(impl_out.get_output().stdout.as_slice());
    let impl_v: serde_json::Value = serde_json::from_str(&impl_stdout).expect("implement json");
    let impl_id = impl_v["run_id"].as_str().expect("implement run_id");
    // One run id plan → implement (O1)
    assert_eq!(impl_id, run_id);

    // dry-run: cwd under .spar/runs/<id>/cwd-* (no real git worktrees)
    let state_path = tmp
        .path()
        .join(".spar/runs")
        .join(impl_id)
        .join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_path).unwrap()).unwrap();
    let wts = state["worktrees"].as_array().unwrap();
    assert!(!wts.is_empty());
    let wt_path = wts[0]["path"].as_str().unwrap();
    assert!(
        wt_path.contains(".spar") && wt_path.contains("cwd-") && wt_path.contains(impl_id),
        "unexpected dry-run cwd path {wt_path}"
    );
    // Spec channel: dry-run acceptance stamp must land in implementer cwd.
    let impl_cwd = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .and_then(|s| s["cwd"].as_str())
        .expect("implementer cwd");
    assert!(
        std::path::Path::new(impl_cwd)
            .join(".spar-dry-acceptance-tests")
            .is_file(),
        "acceptance tests should be overlaid into implementer cwd {impl_cwd}"
    );

    assert_eq!(primary_branch(tmp.path()), branch_before);

    // ship without confirm → gate 2
    spar_cmd()
        .current_dir(tmp.path())
        .args(["ship", impl_id, "--json"])
        .assert()
        .code(2);

    // dry-run ship with confirm writes ship.md, no real push
    spar_cmd()
        .current_dir(tmp.path())
        .args(["ship", impl_id, "--confirm", "--json"])
        .assert()
        .success();
    assert!(tmp
        .path()
        .join(".spar/runs")
        .join(impl_id)
        .join("artifacts/ship.md")
        .is_file());

    spar_cmd()
        .current_dir(tmp.path())
        .args(["cleanup", run_id, "--purge", "--json"])
        .assert()
        .success();
    // purge should not leave an empty runs/ shell behind
    let runs = tmp.path().join(".spar/runs");
    assert!(
        !runs.is_dir() || std::fs::read_dir(&runs).unwrap().next().is_some(),
        "empty .spar/runs should be removed"
    );
}

#[test]
fn plan_spec_disabled_skips_test_author() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    std::fs::write(tmp.path().join("spar.toml"), "[spec]\nenabled = false\n").unwrap();
    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "no spec",
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
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    let slots = v["slots"].as_array().unwrap();
    assert!(
        slots.iter().all(|s| s["role"] != "test_author"),
        "no test_author when spec disabled: {slots:?}"
    );
    assert!(!tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/test-contract.md")
        .is_file());
}

#[test]
fn plan_dry_run_writes_test_contract() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "spec channel check",
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
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    let contract = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/test-contract.md");
    assert!(contract.is_file());
    let body = std::fs::read_to_string(&contract).unwrap();
    assert!(body.contains("## Scenarios"), "contract shape: {body}");
    let slots = v["slots"].as_array().unwrap();
    assert!(
        slots.iter().any(|s| s["role"] == "test_author"),
        "expected test_author in slots: {slots:?}"
    );
}

#[test]
fn implement_dry_run_writes_suite_artifact() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--task",
            "suite channel check",
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
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    let suite = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/suite.md");
    assert!(suite.is_file(), "suite.md missing");
    let body = std::fs::read_to_string(&suite).unwrap();
    assert!(body.contains("## Result"), "suite report shape: {body}");
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            tmp.path()
                .join(".spar/runs")
                .join(run_id)
                .join("state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let has_tester = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["role"] == "tester");
    assert!(has_tester, "expected tester slot in state");
}

#[test]
fn implement_dry_run_surfaces_suite_outcome() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--task",
            "suite outcome surfaced",
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
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    // state.json carries the tri-state suite outcome (dry-run tester writes pass).
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            tmp.path()
                .join(".spar/runs")
                .join(run_id)
                .join("state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state["suite_outcome"], "pass", "state.json suite_outcome");

    // status --json surfaces it too.
    let st = spar_cmd()
        .current_dir(tmp.path())
        .args(["status", run_id, "--json"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let sv: serde_json::Value = serde_json::from_slice(&st).unwrap();
    assert_eq!(sv["suite_outcome"], "pass", "status --json suite_outcome");
}

#[test]
fn dry_run_does_not_create_git_worktrees() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let parent = tmp.path().parent().unwrap();
    let before: std::collections::HashSet<_> = std::fs::read_dir(parent)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--task",
            "no real worktrees",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);

    let after: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| !before.contains(n))
        .collect();
    // only the temp project itself might appear as "new" if something else; no *-spar-* siblings
    for name in &after {
        let s = name.to_string_lossy();
        assert!(
            !s.contains("-spar-"),
            "dry-run must not create sibling worktree dirs, found {s}"
        );
    }
    // cwd lives under .spar
    let spar = tmp.path().join(".spar/runs");
    assert!(spar.is_dir());
    let has_cwd = walkdir_has_cwd(&spar);
    assert!(has_cwd, "expected .spar/runs/*/cwd-* dirs");
}

fn walkdir_has_cwd(dir: &std::path::Path) -> bool {
    fn rec(d: &std::path::Path) -> bool {
        let Ok(rd) = std::fs::read_dir(d) else {
            return false;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("cwd-") {
                return true;
            }
            if e.path().is_dir() && rec(&e.path()) {
                return true;
            }
        }
        false
    }
    rec(dir)
}

#[test]
fn status_exit_zero_when_gated() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let plan = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "gate",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let st = spar_cmd()
        .current_dir(tmp.path())
        .args(["status", run_id, "--json"])
        .assert()
        .code(0); // observe always succeeds
    let s = String::from_utf8_lossy(st.get_output().stdout.as_slice());
    let sv: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(sv["exit_code"], 2);
    assert_eq!(sv["phase"], "awaiting_plan_approval");
}

#[test]
fn stop_preserves_gate_phase() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let plan = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "gate then stop",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    let stop = spar_cmd()
        .current_dir(tmp.path())
        .args(["stop", run_id, "--json"])
        .assert()
        .success();
    let sout = String::from_utf8_lossy(stop.get_output().stdout.as_slice());
    let sv: serde_json::Value = serde_json::from_str(&sout).unwrap();
    assert_eq!(
        sv["phase"], "awaiting_plan_approval",
        "stop must not clobber the gate phase"
    );

    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            tmp.path()
                .join(".spar/runs")
                .join(run_id)
                .join("state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["phase"], "awaiting_plan_approval",
        "state.json phase must stay at the gate after stop"
    );
    let stopped_marker = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("markers/stopped");
    assert!(
        !stopped_marker.is_file(),
        "stop must not drop a resumable stopped marker on a gated run"
    );
}

#[test]
fn stop_preserves_terminal_done_phase() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--workflow",
            "review",
            "--task",
            "review then stop",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(run.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["phase"], "done");
    let run_id = v["run_id"].as_str().unwrap();

    let stop = spar_cmd()
        .current_dir(tmp.path())
        .args(["stop", run_id, "--json"])
        .assert()
        .success();
    let sout = String::from_utf8_lossy(stop.get_output().stdout.as_slice());
    let sv: serde_json::Value = serde_json::from_str(&sout).unwrap();
    assert_eq!(
        sv["phase"], "done",
        "stop must not clobber a finished run's terminal phase"
    );

    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            tmp.path()
                .join(".spar/runs")
                .join(run_id)
                .join("state.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["phase"], "done",
        "state.json phase must stay done after stop"
    );
    let stopped_marker = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("markers/stopped");
    assert!(
        !stopped_marker.is_file(),
        "stop must not drop a resumable stopped marker on a finished run"
    );
}

#[test]
fn arena_reconcile_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--workflow",
            "arena",
            "--task",
            "feature X",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_winner_confirm"));
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();

    spar_cmd()
        .current_dir(tmp.path())
        .args(["reconcile", run_id, "--json"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));
    assert!(tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/summary-reconcile.md")
        .is_file());
}

#[test]
fn dual_backend_dry_run_providers() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "dual backend",
            "--providers",
            "api:openai,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(v.get("run_id").is_some());
    assert_eq!(v.get("id"), v.get("run_id"));
    let slots = v["slots"].as_array().unwrap();
    assert!(
        !slots.is_empty(),
        "expected planner slots for api/cli providers"
    );
    let providers = v["providers"].as_array().unwrap();
    let joined = providers
        .iter()
        .filter_map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        joined.contains("openai") || joined.contains("cli:grok"),
        "providers={joined}"
    );
}

#[test]
fn empty_fake_providers_fail_closed() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    // Live (no dry-run): unknown names must not become a fake plan gate.
    let r = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "x",
            "--providers",
            "cli:notreal1,cli:notreal2",
            "--json",
        ])
        .output()
        .unwrap();
    let code = r.status.code().unwrap_or(1);
    assert_ne!(code, 2, "must not return human gate with zero providers");
    assert!(code == 1 || code == 4, "expected failure/quota, got {code}");
}

#[test]
fn skills_and_bus_commands() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .args(["skills", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("core"));
    spar_cmd()
        .current_dir(tmp.path())
        .args(["skills", "get", "core"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit codes"));

    let plan = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "bus seed",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
            "--big",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    assert!(tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("bus/tasks/graph.json")
        .is_file());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "bus",
            "send",
            "--run",
            run_id,
            "--from",
            "human",
            "--to",
            "broadcast",
            "-m",
            "hello fleet",
            "--json",
        ])
        .assert()
        .success();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["bus", "log", "--run", run_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello fleet"));
}

#[test]
fn stuck_policy_dry_run_request_changes() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    // Force request_changes every review → fix rounds → rotate → widen → stuck
    let out = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--task",
            "force stuck path",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(3) // stuck
        .stdout(predicate::str::contains("stuck"));

    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    let esc = tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/escalation.md");
    assert!(esc.is_file());
    // implementer id stays "impl" after rotation
    let slots = v["slots"].as_array().unwrap();
    let impls: Vec<_> = slots
        .iter()
        .filter(|s| s["role"] == "implementer")
        .collect();
    assert_eq!(impls.len(), 1);
    assert_eq!(impls[0]["id"], "impl");
    // widened reviewers: more than 2 reviewer slots eventually
    let revs = slots.iter().filter(|s| s["role"] == "reviewer").count();
    assert!(revs >= 3, "expected widen to add a reviewer, got {revs}");
}

#[test]
fn quota_exit_when_all_paused() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    for p in ["cli:claude", "cli:grok", "cli:agy"] {
        spar_cmd()
            .current_dir(tmp.path())
            .args(["provider", "pause", p])
            .assert()
            .success();
    }

    // Force provider names so we hit quota filter even if some are missing on PATH.
    let r = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "x",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--json",
        ])
        .output()
        .unwrap();
    let code = r.status.code().unwrap_or(1);
    // If no providers on PATH, pick may still use empty base → different path.
    // When any are available-but-paused, expect 4 + JSON exit_code 4 + phase quota.
    if code == 4 {
        let v: serde_json::Value = serde_json::from_slice(&r.stdout).expect("quota json");
        assert_eq!(v["exit_code"], 4, "JSON exit_code must match process 4");
        assert_eq!(v["phase"], "quota");
        let run_id = v["run_id"].as_str().unwrap();
        spar_cmd()
            .current_dir(tmp.path())
            .args(["status", run_id, "--json"])
            .assert()
            .code(0) // status is observe-only
            .stdout(predicate::str::contains("\"phase\": \"quota\""))
            .stdout(predicate::str::contains("\"exit_code\": 4"));
    } else {
        // Offline / no binaries: still accept 1 only when JSON is not a quota run
        assert_eq!(
            code,
            1,
            "expected quota(4) when providers exist, got {code} stderr={}",
            String::from_utf8_lossy(&r.stderr)
        );
    }
}

#[test]
fn arena_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--workflow",
            "arena",
            "--task",
            "implement feature X",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_winner_confirm"));
}

#[test]
fn review_workflow_concurrent_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let out = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--workflow",
            "review",
            "--task",
            "review the auth changes",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"phase\": \"done\""));
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    let slots = v["slots"].as_array().unwrap();
    assert!(slots.len() >= 2);
    assert!(tmp
        .path()
        .join(".spar/runs")
        .join(run_id)
        .join("artifacts/summary.md")
        .is_file());
}

#[test]
fn peer_and_roles_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--providers",
            "cli:claude,cli:grok",
            "--workflow",
            "peer",
            "--task",
            "split stack",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"phase\": \"done\""));

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "run",
            "--providers",
            "cli:claude,cli:grok",
            "--workflow",
            "roles",
            "--task",
            "fe/be",
            "--dry-run",
            "--json",
        ])
        .assert()
        .success();
}

#[test]
fn provider_pause_resume() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args(["provider", "pause", "cli:claude", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("paused_manual"));

    spar_cmd()
        .current_dir(tmp.path())
        .args(["provider", "resume", "cli:claude", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("available"));
}

#[test]
fn providers_required_on_plan() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .args(["plan", "--task", "x", "--dry-run", "--json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("providers").or(predicate::str::contains("required")));
}

#[test]
fn path_b_implement_task() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--task",
            "fix the flaky test",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));
}
