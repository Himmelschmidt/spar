//! Round-loop economy scenarios (dry-run, no live AI). See DECISIONS O52.
//!
//! Measured over 197 real runs, a fix round is a 6.6x median run cost and `state.round`
//! had no bound at all: the corpus contains runs that reached rounds 9, 13, 17, 19, 20,
//! 26 and 34. These cover the two halves of the fix — a ceiling that escalates to a
//! human instead of looping, and a compact brief that carries between rounds so a cold
//! re-dispatch does not re-derive what the last one already learned.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

/// Per-test-process SPAR_HOME so the suite never writes the developer's real
/// ~/.spar/registry.json. Shared across spawns in this binary.
fn spar_home_dir() -> PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
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
    // this test's temp project and write real runs into it.
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn init_git_repo(dir: &Path) {
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

/// A git repo with a project `spar.toml` capping the run at `max` rounds.
fn project_with_ceiling(dir: &Path, max: u32) {
    init_git_repo(dir);
    std::fs::write(
        dir.join("spar.toml"),
        format!("[rounds]\nmax = {max}\ncarry_forward_chars = 800\n"),
    )
    .unwrap();
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

fn status_json(dir: &Path, run_id: &str) -> serde_json::Value {
    let out = spar_cmd()
        .current_dir(dir)
        .args(["status", run_id, "--json"])
        .assert()
        .success();
    serde_json::from_slice(out.get_output().stdout.as_slice()).unwrap()
}

/// The implementer slot id, which is also the carry-forward file's scope.
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

/// The prompt last rendered for a slot. Overwritten per dispatch, so this is the most
/// recent round's.
fn last_prompt(dir: &Path, run_id: &str, slot_id: &str) -> String {
    std::fs::read_to_string(run_dir(dir, run_id).join(format!("prompt-{slot_id}.md"))).unwrap()
}

/// `--dry-run` implement with the synthetic review panel forced to request changes,
/// which is what drives the fix loop without a live model.
fn implement(dir: &Path, run_id: &str, extra: &[&str]) -> assert_cmd::assert::Assert {
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
        .args(&args)
        .assert()
}

/// AC-1. Unbounded `state.round` is what produced the round-34 runs. The ceiling stops
/// the loop at a **human gate** (exit 2), not at `stuck` (exit 3): nothing is broken,
/// the run has just spent the re-dispatch budget it may spend on its own.
#[test]
fn a_run_that_keeps_failing_stops_at_the_round_ceiling() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());

    implement(tmp.path(), &run_id, &[])
        .code(2)
        .stdout(predicate::str::contains("awaiting_round_extension"));

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["round"].as_u64(), Some(3), "{st}");
    assert_eq!(st["max_rounds"].as_u64(), Some(3), "{st}");
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| e.contains("round ceiling")),
        "the gate has to say which gate it is: {st}"
    );

    let v = status_json(tmp.path(), &run_id);
    assert_eq!(v["phase"], "awaiting_round_extension");
    assert_eq!(
        v["exit_code"].as_u64(),
        Some(2),
        "an outer agent reads the reason off status --json: {v}"
    );
    assert_eq!(v["max_rounds"].as_u64(), Some(3), "{v}");

    let escalation =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/escalation.md"))
            .unwrap();
    assert!(
        escalation.contains("--max-rounds"),
        "the escalation must name the way out:\n{escalation}"
    );
}

/// AC-7. `print_run_human` shows the phase and not `error`, so an operator running
/// implement by hand would otherwise get a phase name and no way out of it.
#[test]
fn the_gate_says_how_to_lift_it_on_stderr() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 2);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[])
        .code(2)
        .stderr(predicate::str::contains("round ceiling reached"))
        .stderr(predicate::str::contains("--max-rounds"));
}

/// AC-2. Re-entering `implement` at the ceiling must gate again *before* dispatching
/// anything — otherwise the ceiling costs a full round every time it is hit.
#[test]
fn re_entering_at_the_ceiling_buys_no_round() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);
    let before = state_json(tmp.path(), &run_id)["round"].as_u64();

    implement(tmp.path(), &run_id, &[])
        .code(2)
        .stdout(predicate::str::contains("awaiting_round_extension"));

    assert_eq!(
        state_json(tmp.path(), &run_id)["round"].as_u64(),
        before,
        "a bounced re-entry must not claim a round it did not run"
    );
}

/// AC-3. `--max-rounds` is the operator saying the next round is worth paying for, and
/// it sticks: a continuation without the flag must not silently drop back to the
/// project default and re-gate the run the operator just released.
#[test]
fn max_rounds_lifts_the_ceiling_and_persists() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    implement(tmp.path(), &run_id, &["--max-rounds", "5"]).code(2);
    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["max_rounds"].as_u64(), Some(5), "{st}");
    assert_eq!(st["round"].as_u64(), Some(5), "the run kept going: {st}");

    implement(tmp.path(), &run_id, &[]).code(2);
    assert_eq!(
        state_json(tmp.path(), &run_id)["max_rounds"].as_u64(),
        Some(5),
        "the raised ceiling must survive a continuation with no flag"
    );
}

/// AC-4. The first round of a run has nothing to carry. An empty carry-forward heading
/// would tell the model there was a previous attempt when there was not.
#[test]
fn the_first_round_carries_nothing() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 2);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    let slot = implementer_slot(tmp.path(), &run_id);
    let prompt = last_prompt(tmp.path(), &run_id, &slot);
    assert!(
        !prompt.contains("Carry-forward"),
        "round 2 is this run's first implement round:\n{prompt}"
    );
    assert!(
        !prompt.contains("{{"),
        "an unseeded placeholder reaches the model as literal text:\n{prompt}"
    );
}

/// AC-5. The failed criterion and its evidence reach the next round's implementer.
/// Before this the fix round's prompt was byte-identical to round 1's: the block reason
/// was computed and sent only to the bus.
#[test]
fn the_next_round_is_told_which_criterion_failed() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    let slot = implementer_slot(tmp.path(), &run_id);
    let prompt = last_prompt(tmp.path(), &run_id, &slot);
    assert!(prompt.contains("Carry-forward"), "{prompt}");
    assert!(
        prompt.contains("AC-1: fail"),
        "the fix round must learn which AC blocked it:\n{prompt}"
    );
    assert!(
        prompt.contains("requested changes"),
        "and where the review that blocked it lives:\n{prompt}"
    );
}

/// AC-6. The slot's own brief reaches the next round, is capped, and is **consumed**:
/// a brief that survived its round would describe a worktree that has since moved.
#[test]
fn the_slot_brief_is_carried_capped_and_consumed() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 2);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    // The dry-run backend stubs the implementer, so stand in for what a real one writes.
    let slot = implementer_slot(tmp.path(), &run_id);
    let brief = run_dir(tmp.path(), &run_id).join(format!("artifacts/carry-forward-{slot}.md"));
    let body = format!(
        "- touched src/auth.rs: SENTINEL-CARRIED\n{}",
        "- padding line that a real slot would never write\n".repeat(200)
    );
    assert!(
        body.len() > 800,
        "guard is vacuous unless the brief is long"
    );
    std::fs::write(&brief, &body).unwrap();

    // Exactly one more round, so the prompt left on disk is the one that read the brief.
    implement(tmp.path(), &run_id, &["--max-rounds", "3"]).code(2);

    let prompt = last_prompt(tmp.path(), &run_id, &slot);
    assert!(
        prompt.contains("SENTINEL-CARRIED"),
        "the brief must reach the next round:\n{prompt}"
    );
    assert!(
        prompt.contains("truncated by spar at 800 characters"),
        "[rounds] carry_forward_chars must actually bound it:\n{prompt}"
    );
    assert!(
        !brief.exists(),
        "the brief is consumed on read, or round N+2 inherits round N's tree description"
    );
}

/// AC-8. **The gate is a contract re-freeze door.** `execute_loop` re-freezes from disk on
/// every entry (O43), the ceiling makes that a routine act spar itself prompts for, and
/// the implementer can write to `artifacts/`. So: delete the criterion you cannot pass,
/// let the ceiling invite the operator to run the command spar printed, and the shortened
/// contract becomes the gate. Re-entry must refuse to adopt a contract it watched move.
#[test]
fn a_tampered_contract_is_not_adopted_by_the_re_entry_the_gate_asks_for() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());

    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .env("SPAR_FORCE_CONTRACT_TAMPER", "1")
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_round_extension", "{st}");
    assert_eq!(
        st["contract_modified"].as_bool(),
        Some(true),
        "guard is vacuous unless spar saw the contract move: {st}"
    );
    let frozen_before = st["contract_fingerprint"].as_str().unwrap().to_string();
    let contract =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/test-contract.md"))
            .unwrap();
    assert!(
        !contract.contains("AC-2:"),
        "guard is vacuous unless the criterion was actually deleted:\n{contract}"
    );

    // The command the gate itself printed must not silently adopt the shortened contract.
    implement(tmp.path(), &run_id, &["--max-rounds", "6"])
        .code(1)
        .stderr(predicate::str::contains("test-contract.md changed"))
        .stderr(predicate::str::contains("--accept-contract"));

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(
        st["contract_fingerprint"].as_str(),
        Some(frozen_before.as_str()),
        "a refused re-entry must not move the freeze: {st}"
    );
    assert_eq!(
        st["contract_modified"].as_bool(),
        Some(true),
        "nor quietly mark the run clean: {st}"
    );
}

/// AC-9. The refusal has a door, and going through it is loud. `--accept-contract` is
/// the operator saying they read the diff.
#[test]
fn accept_contract_adopts_the_drift_and_says_so() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .env("SPAR_FORCE_CONTRACT_TAMPER", "1")
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let before = state_json(tmp.path(), &run_id)["contract_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    implement(
        tmp.path(),
        &run_id,
        &["--max-rounds", "6", "--accept-contract"],
    )
    .code(2)
    .stderr(predicate::str::contains("contract re-frozen"))
    .stderr(predicate::str::contains("--accept-contract"));

    let st = state_json(tmp.path(), &run_id);
    assert_ne!(st["contract_fingerprint"].as_str(), Some(before.as_str()));
}

/// AC-10. A re-freeze changes what the ship gate enforces. One line in `events.jsonl` is
/// not somewhere anyone looks, so the sanctioned amendment path is loud on stderr too.
#[test]
fn an_ordinary_re_freeze_is_announced_on_stderr() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 8);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    let contract = run_dir(tmp.path(), &run_id).join("artifacts/test-contract.md");
    let mut body = std::fs::read_to_string(&contract).unwrap();
    body.push_str("- [ ] AC-3: added by the operator — verify: `dry-run` (stub)\n");
    std::fs::write(&contract, body).unwrap();

    implement(tmp.path(), &run_id, &["--max-rounds", "20"])
        .stderr(predicate::str::contains("contract re-frozen"))
        .stderr(predicate::str::contains("3 criteria"));
}

/// AC-11. **The round the human paid for must not run blind.** `blockers` is rebuilt from
/// the review artifacts on disk, so the most expensive round of the run — the one bought
/// at a gate, in a fresh process — still learns which criterion it failed.
#[test]
fn the_round_bought_at_the_gate_still_knows_what_failed() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    // One more round, in a brand-new process, across the gate boundary.
    implement(tmp.path(), &run_id, &["--max-rounds", "4"]).code(2);

    let slot = implementer_slot(tmp.path(), &run_id);
    let prompt = last_prompt(tmp.path(), &run_id, &slot);
    assert!(
        prompt.contains("AC-1: fail"),
        "the bought round must still be told which AC blocked it:\n{prompt}"
    );
    assert!(
        prompt.contains("requested changes"),
        "and which review said so:\n{prompt}"
    );
}

/// AC-12. `stuck` (exit 3, "this cannot be fixed") outranks the ceiling gate (exit 2,
/// "this is costing too much") when both come due on the same iteration. The other
/// ordering inverted the exit-code contract for the hardest class of run.
#[test]
fn stuck_outranks_the_ceiling_when_both_are_due() {
    let tmp = tempdir().unwrap();
    // 13 is exactly where the full rotate → widen → stuck ladder lands for a plan run,
    // so `round_ceiling_reached()` is true on the same iteration that exhausts the
    // ladder. Whichever check runs first decides the exit code.
    project_with_ceiling(tmp.path(), 13);
    let run_id = planned_run(tmp.path());

    implement(tmp.path(), &run_id, &[])
        .code(3)
        .stdout(predicate::str::contains("stuck"));

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "stuck", "{st}");
    assert_eq!(st["round"].as_u64(), Some(13), "{st}");
    assert_eq!(
        st["max_rounds"].as_u64(),
        Some(13),
        "guard is vacuous unless the ceiling was also due: {st}"
    );
    assert_eq!(
        st["widened_reviewers"].as_bool(),
        Some(true),
        "the ladder must really have been exhausted: {st}"
    );
}

/// AC-13. Lifting the ceiling buys **rounds**, not a fresh stuck ladder. Resetting the
/// ladder on every entry made exit 3 unreachable for an outer agent that lifts in a loop.
#[test]
fn a_lift_does_not_re_buy_the_escalation_ladder() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 6);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);
    let st = state_json(tmp.path(), &run_id);
    assert_eq!(
        st["rotated_implementer"].as_bool(),
        Some(true),
        "guard is vacuous unless the ladder had already rotated: {st}"
    );

    implement(tmp.path(), &run_id, &["--max-rounds", "9"]).code(2);
    let st = state_json(tmp.path(), &run_id);
    assert_eq!(
        st["rotated_implementer"].as_bool(),
        Some(true),
        "the lift re-bought a rotation the run had already spent: {st}"
    );

    // Keep lifting and the ladder finishes: exit 3 stays reachable.
    implement(tmp.path(), &run_id, &["--max-rounds", "20"])
        .code(3)
        .stdout(predicate::str::contains("stuck"));
}

/// AC-14. A run whose ceiling was lifted and which then reaches the ship gate must not
/// still report "round ceiling reached" — an outer agent reading `error` at exit 2 would
/// re-issue `--max-rounds` instead of shipping.
#[test]
fn lifting_the_ceiling_clears_the_stale_reason() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);
    assert!(state_json(tmp.path(), &run_id)["error"]
        .as_str()
        .is_some_and(|e| e.contains("round ceiling")));

    // No forced request_changes this time: the run gets to finish.
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--max-rounds",
            "8",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));

    let v = status_json(tmp.path(), &run_id);
    assert_eq!(v["phase"], "awaiting_ship_confirm");
    assert!(
        v["error"].is_null(),
        "a ship gate must not carry the round gate's reason: {v}"
    );
}

/// AC-15. `--max-rounds 0` read as "off forever" and the help never said so; turning the
/// ceiling off is a deliberate project setting, not a flag typed under pressure at a gate.
#[test]
fn max_rounds_zero_is_refused_on_the_flag() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 3);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &["--max-rounds", "0"])
        .failure()
        .stderr(predicate::str::contains(
            "would remove the ceiling entirely",
        ))
        .stderr(predicate::str::contains("[rounds] max = 0"));
    assert_eq!(
        state_json(tmp.path(), &run_id)["max_rounds"].as_u64(),
        Some(3),
        "a rejected flag must not have reached the run"
    );
}

/// AC-16. The gate must not point at a command that exits 1: `confirm_ship` accepts only
/// the ship and winner gates.
#[test]
fn the_escalation_does_not_offer_a_ship_that_would_fail() {
    let tmp = tempdir().unwrap();
    project_with_ceiling(tmp.path(), 2);
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id, &[]).code(2);

    let escalation =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/escalation.md"))
            .unwrap();
    assert!(!escalation.contains("ship"), "{escalation}");

    spar_cmd()
        .current_dir(tmp.path())
        .args(["ship", &run_id, "--confirm"])
        .assert()
        .failure();
}
