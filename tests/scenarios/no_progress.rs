//! No-progress rounds (DECISIONS O168): an implementer that finishes a round
//! with HEAD and the tree unchanged while a `request_changes` review against that
//! same commit is open must not be answered with another full review of the same
//! commit. The next dispatch carries a warning instead, and the second consecutive
//! empty round parks the run at the existing human gate (exit 2).
//!
//! Dry-run only, no live AI. `SPAR_DRY_IMPL_NOOP` turns the dry-run implementer
//! into one that changes nothing on disk, which is exactly the stall, and
//! `SPAR_FORCE_REVIEW_FINDING=major` stands in for the reviewer's open finding.

mod common;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn spar_cmd() -> assert_cmd::Command {
    let mut c = common::spar();
    // spar exports these into every slot, so when the suite runs inside a spar
    // worktree the child would resolve the primary checkout instead of this
    // test's temp project and write real runs into it.
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn git(dir: &Path, args: &[&str]) {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git ran");
}

/// A clean git repo: `spar.toml` committed, so a dry-run implementer that writes
/// nothing really leaves HEAD and the tree exactly as the review judged them.
fn project_with_clean_tree(dir: &Path, max: u32) {
    project_with_clean_tree_budget(dir, max, 800);
}

fn project_with_clean_tree_budget(dir: &Path, max: u32, chars: usize) {
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        git(dir, &args);
    }
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    std::fs::write(
        dir.join("spar.toml"),
        format!("[rounds]\nmax = {max}\ncarry_forward_chars = {chars}\n"),
    )
    .unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", "init"]);
}

fn planned_run(dir: &Path) -> String {
    let plan = spar_cmd()
        .current_dir(dir)
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
    let stdout = String::from_utf8_lossy(plan.get_output().stdout.as_slice());
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("plan json");
    let run_id = v["run_id"].as_str().expect("run_id").to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

fn run_dir(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(".spar/runs").join(run_id)
}

fn state_json(dir: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(run_dir(dir, run_id).join("state.json")).unwrap())
        .unwrap()
}

fn implementer_slot(dir: &Path, run_id: &str) -> String {
    state_json(dir, run_id)["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .and_then(|s| s["id"].as_str())
        .expect("implementer slot")
        .to_string()
}

/// `--dry-run` implement with the panel forced to request changes on an open
/// major finding, and the implementer turned into a no-op for the first `noop`
/// dispatches of the process (`all` stalls every round).
fn implement_noop(
    dir: &Path,
    run_id: &str,
    noop: &str,
    extra: &[&str],
) -> assert_cmd::assert::Assert {
    let mut args = vec![
        "implement",
        "--run",
        run_id,
        "--providers",
        "cli:claude,cli:grok,cli:agy",
        "--dry-run",
        "--json",
    ];
    args.extend_from_slice(extra);
    spar_cmd()
        .current_dir(dir)
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .env("SPAR_FORCE_REVIEW_FINDING", "major")
        .env("SPAR_DRY_IMPL_NOOP", noop)
        .args(&args)
        .assert()
}

fn review_rounds(dir: &Path, run_id: &str) -> Vec<u64> {
    state_json(dir, run_id)["review_history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["round"].as_u64().unwrap())
        .collect()
}

/// Two consecutive empty rounds park at the human gate instead of paying for a
/// second full review of the same commit: the panel runs once, the warning
/// re-dispatch names the round, commit and findings, and the run exits 2.
#[test]
fn two_empty_rounds_park_at_the_human_gate() {
    let tmp = tempdir().unwrap();
    project_with_clean_tree(tmp.path(), 8);
    let run_id = planned_run(tmp.path());

    implement_noop(tmp.path(), &run_id, "all", &[]).code(2);

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_round_extension", "{st}");
    assert_eq!(st["no_progress_rounds"].as_u64(), Some(2), "{st}");
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| e.contains("no progress in two consecutive")),
        "the gate has to say which stall it is: {st}"
    );
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| !e.contains("dry-run forced finding")),
        "the park reason stays short; findings live in escalation.md: {st}"
    );
    let rounds = review_rounds(tmp.path(), &run_id);
    assert_eq!(rounds.len(), 2, "the panel runs once, one review per seat");
    assert!(
        rounds.iter().all(|&r| r == 2),
        "the panel judges the first implementer round, then the ladder takes over: {rounds:?}"
    );

    let slot = implementer_slot(tmp.path(), &run_id);
    let prompt =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join(format!("prompt-{slot}.md")))
            .unwrap();
    assert!(
        prompt.starts_with("## No-progress warning"),
        "the warning goes at the top of the re-dispatched prompt:\n{prompt}"
    );
    assert!(
        prompt.contains("(round 2, Reviewed-Commit"),
        "the carry-forward names the round and commit it judges:\n{prompt}"
    );
    assert!(
        prompt.contains("dry-run forced finding"),
        "the warning names the unaddressed finding:\n{prompt}"
    );

    let escalation =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/escalation.md"))
            .unwrap();
    assert!(
        escalation.contains("## No-progress warning"),
        "the escalation carries the warning for the human:\n{escalation}"
    );
}

/// Re-entering a stall-parked run below the ceiling with the bare command
/// resumes it: one warned dispatch, then it parks again with no new panel.
#[test]
fn re_entering_a_stall_park_below_the_ceiling_parks_again_with_no_panel() {
    let tmp = tempdir().unwrap();
    project_with_clean_tree(tmp.path(), 8);
    let run_id = planned_run(tmp.path());

    implement_noop(tmp.path(), &run_id, "all", &[]).code(2);
    let rounds_before = review_rounds(tmp.path(), &run_id);

    implement_noop(tmp.path(), &run_id, "all", &[]).code(2);

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_round_extension", "{st}");
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| e.contains("no progress in two consecutive")),
        "the re-entry stalls again on the still-unchanged tree: {st}"
    );
    assert_eq!(
        review_rounds(tmp.path(), &run_id),
        rounds_before,
        "the re-entry parks again with no new panel"
    );
}

/// The carry-forward section itself, and not the warning, names the round
/// that wrote each blocking review, the commit it judged, and its open
/// findings, so a round that never stalls still learns what blocked the ship.
/// A generous budget keeps clamping out of what this asserts.
#[test]
fn the_carry_forward_section_names_round_commit_and_findings() {
    let tmp = tempdir().unwrap();
    project_with_clean_tree_budget(tmp.path(), 8, 4000);
    let run_id = planned_run(tmp.path());

    implement_noop(tmp.path(), &run_id, "all", &[]).code(2);

    let slot = implementer_slot(tmp.path(), &run_id);
    let prompt =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join(format!("prompt-{slot}.md")))
            .unwrap();
    let carry = prompt
        .split_once("## Carry-forward")
        .map(|(_, rest)| rest)
        .expect("a carry-forward section in the re-dispatched prompt");
    assert!(
        carry.contains("What blocked the ship last round"),
        "the verdicts render as carry-forward, not only as warning:\n{carry}"
    );
    assert!(
        carry.contains("(round 2, Reviewed-Commit"),
        "each blocking review names the round that wrote it and the commit it judged:\n{carry}"
    );
    assert!(
        carry.contains("requested changes"),
        "the section says what the reviews did:\n{carry}"
    );
    assert!(
        carry.contains("dry-run forced finding"),
        "the open findings ride along, severity and line:\n{carry}"
    );
    assert_eq!(
        carry.matches("current verdict").count(),
        1,
        "the verdict sentence renders once per section:\n{carry}"
    );
}

/// One empty round re-dispatches the implementer with the warning and no new
/// panel; when that dispatch makes progress, the panel runs again and the run
/// continues to the round ceiling instead of the stall gate.
#[test]
fn warning_round_then_progress_reaches_the_panel_again() {
    let tmp = tempdir().unwrap();
    project_with_clean_tree(tmp.path(), 8);
    let run_id = planned_run(tmp.path());

    // Only rounds 1-2 stall; round 3 runs the normal dry-run stamp, so the
    // warned re-dispatch counts as progress and the panel runs again.
    implement_noop(tmp.path(), &run_id, "2", &[]).code(2);

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_round_extension", "{st}");
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| e.contains("round ceiling")),
        "progress must steer past the stall gate to the ceiling: {st}"
    );
    let rounds = review_rounds(tmp.path(), &run_id);
    assert!(
        rounds.contains(&2),
        "the first panel judges the first implementer round: {rounds:?}"
    );
    assert!(
        rounds.iter().any(|&r| r > 2),
        "the panel must run again once the warned re-dispatch makes progress: {rounds:?}"
    );
    assert_eq!(st["no_progress_rounds"].as_u64(), Some(0), "{st}");

    let events =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("events.jsonl")).unwrap();
    assert!(
        events.contains("no-progress round"),
        "the skipped panel must leave a trace"
    );
}
