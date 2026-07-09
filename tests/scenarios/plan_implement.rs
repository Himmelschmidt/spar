//! End-to-end dry-run scenarios (no live AI).
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::process::Command;
use tempfile::tempdir;

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

    let plan = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "add a hello world module",
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

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["approve", run_id, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("plan_approved"));

    let impl_out = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["implement", "--run", run_id, "--dry-run", "--json"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));

    let impl_stdout = String::from_utf8_lossy(impl_out.get_output().stdout.as_slice());
    let impl_v: serde_json::Value = serde_json::from_str(&impl_stdout).expect("implement json");
    let impl_id = impl_v["run_id"].as_str().expect("implement run_id");
    // One run id plan → implement (O1)
    assert_eq!(impl_id, run_id);

    // worktree sibling naming for implementer
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
        wt_path.contains("-spar-") && wt_path.contains(impl_id),
        "unexpected worktree path {wt_path}"
    );

    assert_eq!(primary_branch(tmp.path()), branch_before);

    // ship without confirm → gate 2
    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["ship", impl_id, "--json"])
        .assert()
        .code(2);

    // dry-run ship with confirm writes ship.md, no real push
    cargo_bin_cmd!("spar")
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

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["cleanup", run_id, "--purge", "--json"])
        .assert()
        .success();
}

#[test]
fn arena_reconcile_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let out = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "run",
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

    cargo_bin_cmd!("spar")
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
fn skills_and_bus_commands() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["skills", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("core"));
    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["skills", "get", "core"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit codes"));

    let plan = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["plan", "--task", "bus seed", "--dry-run", "--json", "--big"])
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

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "bus",
            "send",
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
    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["bus", "log", run_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello fleet"));
}

#[test]
fn stuck_policy_dry_run_request_changes() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    // Force request_changes every review → fix rounds → rotate → widen → stuck
    let out = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--task",
            "force stuck path",
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

    for p in ["claude", "grok", "agy"] {
        cargo_bin_cmd!("spar")
            .current_dir(tmp.path())
            .args(["provider", "pause", p])
            .assert()
            .success();
    }

    // Force provider names so we hit quota filter even if some are missing on PATH.
    let r = cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "x",
            "--providers",
            "claude,grok,agy",
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
        cargo_bin_cmd!("spar")
            .current_dir(tmp.path())
            .args(["status", run_id, "--json"])
            .assert()
            .code(4)
            .stdout(predicate::str::contains("\"phase\": \"quota\""));
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

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "run",
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
fn peer_and_roles_dry_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "run",
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

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "run",
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

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["provider", "pause", "claude", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("paused_manual"));

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args(["provider", "resume", "claude", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("available"));
}

#[test]
fn path_b_implement_task() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());

    cargo_bin_cmd!("spar")
        .current_dir(tmp.path())
        .args([
            "implement",
            "--task",
            "fix the flaky test",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));
}
