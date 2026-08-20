//! Acceptance-gate contract scenarios (dry-run, no live AI).
//!
//! Two failures observed on run `d995e566`: an `AC-n` mentioned in contract prose became
//! a criterion no reviewer could report (an unreachable ship gate), and an edit to
//! `test-contract.md` after the round loop started had no effect and said nothing.
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
    // this test's temp project and write real runs into it. Clear them per-Command
    // (never via process env — these binaries run tests in parallel).
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

fn commit_gitignore(dir: &Path, pattern: &str) {
    std::fs::write(dir.join(".gitignore"), format!("{pattern}\n")).unwrap();
    Command::new("git")
        .args(["add", ".gitignore"])
        .current_dir(dir)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "ignore"])
        .current_dir(dir)
        .status()
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

fn contract_path(dir: &Path, run_id: &str) -> PathBuf {
    run_dir(dir, run_id).join("artifacts/test-contract.md")
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

/// Files in the run directory whose name starts with `prefix`, read to string.
fn run_files(dir: &Path, run_id: &str, sub: &str, prefix: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(run_dir(dir, run_id).join(sub))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

fn reviews(dir: &Path, run_id: &str) -> Vec<(String, String)> {
    let r = run_files(dir, run_id, "artifacts", "review-");
    assert!(!r.is_empty(), "no review artifacts were written");
    r
}

fn implement(dir: &Path, run_id: &str) -> assert_cmd::assert::Assert {
    spar_cmd()
        .current_dir(dir)
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
}

/// AC-7. The `d995e566` wedge: an aside in `## Notes` and a fenced sample of the house
/// format must not become criteria. In dry-run the synthetic reviewer reports exactly the
/// contract's criteria, so the review artifacts show what the gate was built from.
#[test]
fn phantom_prose_and_fenced_samples_never_become_criteria() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = planned_run(tmp.path());

    let contract = contract_path(tmp.path(), &run_id);
    let mut body = std::fs::read_to_string(&contract).unwrap();
    body.push_str(
        "\n**The 2 ids are frozen.** Later rounds append `AC-9` onward; they do not renumber.\n\n```markdown\n- [ ] AC-8: the house format — verify: `x`\n```\n",
    );
    std::fs::write(&contract, &body).unwrap();

    implement(tmp.path(), &run_id)
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));

    let body = std::fs::read_to_string(&contract).unwrap();
    assert!(
        body.contains("AC-9") && body.contains("AC-8"),
        "guard is vacuous unless the mentions survive in the contract file"
    );
    for (name, text) in reviews(tmp.path(), &run_id) {
        assert!(
            text.contains("AC-1:") && text.contains("AC-2:"),
            "{name} must still report the declared criteria:\n{text}"
        );
        assert!(
            !text.contains("AC-8") && !text.contains("AC-9"),
            "{name} was handed a criterion the contract only mentions:\n{text}"
        );
    }
}

/// AC-10. The git-ignored-overlay note is appended to the prompt copy of the contract and
/// names paths, so an ignored file called `AC-99:fixture.json` is declaration-shaped. The
/// gate must be built from the bytes on disk, so the run still reaches the ship gate.
#[test]
fn overlay_note_cannot_add_a_criterion() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_gitignore(tmp.path(), "AC-99:fixture.json");
    let run_id = planned_run(tmp.path());

    let author_cwd = state_json(tmp.path(), &run_id)["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "test_author")
        .and_then(|s| s["cwd"].as_str())
        .expect("test-author slot cwd")
        .to_string();
    std::fs::write(Path::new(&author_cwd).join("AC-99:fixture.json"), "{}\n").unwrap();

    implement(tmp.path(), &run_id)
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"));

    let prompts = run_files(tmp.path(), &run_id, "", "prompt-review-");
    assert!(
        prompts
            .iter()
            .any(|(_, t)| t.contains("AC-99:fixture.json")),
        "guard is vacuous unless the overlay note actually reached a reviewer prompt"
    );
    for (name, text) in reviews(tmp.path(), &run_id) {
        assert!(
            !text.contains("AC-99"),
            "{name} treated the overlay note as a criterion:\n{text}"
        );
    }
}

/// AC-12. The freeze is reported, so an operator can tell which contract judged the run.
#[test]
fn implement_reports_the_contract_freeze() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id).code(2);

    for (source, v) in [
        ("state.json", state_json(tmp.path(), &run_id)),
        ("status --json", status_json(tmp.path(), &run_id)),
    ] {
        let fp = v["contract_fingerprint"].as_str();
        assert!(
            fp.is_some_and(|f| !f.is_empty()),
            "{source} must carry a non-empty contract_fingerprint: {v}"
        );
        assert_eq!(
            v["contract_modified"].as_bool(),
            Some(false),
            "{source} must report an unmodified contract as false, not absent: {v}"
        );
    }
}

/// AC-13. The sanctioned amendment path: the operator edits the contract between rounds
/// and re-enters `implement`, which re-freezes against the file on disk.
#[test]
fn re_entering_implement_refreezes_the_amended_contract() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id).code(2);
    let before = state_json(tmp.path(), &run_id)["contract_fingerprint"]
        .as_str()
        .expect("fingerprint after the first round")
        .to_string();

    let contract = contract_path(tmp.path(), &run_id);
    let mut body = std::fs::read_to_string(&contract).unwrap();
    body.push_str("- [ ] AC-3: added by the operator — verify: `dry-run` (stub)\n");
    std::fs::write(&contract, body).unwrap();

    implement(tmp.path(), &run_id).code(2);

    let st = state_json(tmp.path(), &run_id);
    assert_ne!(
        st["contract_fingerprint"].as_str(),
        Some(before.as_str()),
        "re-entry must re-freeze against the amended file: {st}"
    );
    assert_eq!(
        st["contract_modified"].as_bool(),
        Some(false),
        "a re-freeze clears the drift flag: {st}"
    );
    for (name, text) in reviews(tmp.path(), &run_id) {
        assert!(
            text.contains("AC-3:"),
            "{name} must be judged against the amended contract:\n{text}"
        );
    }
}

/// AC-14. Runs created before the freeze was recorded still load.
#[test]
fn state_without_contract_fields_still_loads() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id).code(2);

    let path = run_dir(tmp.path(), &run_id).join("state.json");
    let mut st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let obj = st.as_object_mut().unwrap();
    obj.remove("contract_fingerprint");
    obj.remove("contract_modified");
    std::fs::write(&path, serde_json::to_string_pretty(&st).unwrap()).unwrap();

    let v = status_json(tmp.path(), &run_id);
    assert_eq!(
        v["contract_modified"].as_bool(),
        Some(false),
        "a pre-existing run defaults to unmodified: {v}"
    );
}

/// AC-6. A contract whose ids are all mentions and no declarations produces an empty
/// criteria list, which silently disarms the gate. That has to be loud.
#[test]
fn contract_with_no_declared_criteria_warns_loudly() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = planned_run(tmp.path());
    std::fs::write(
        contract_path(tmp.path(), &run_id),
        "## Scenarios\nThe run is judged on AC-1 and AC-2, described in the plan.\n",
    )
    .unwrap();

    implement(tmp.path(), &run_id)
        .code(2)
        .stderr(predicate::str::contains("declares no criteria"));

    let events =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("events.jsonl")).unwrap();
    assert!(
        events.contains("declares no criteria"),
        "the warning must survive in the event log, not just on a terminal:\n{events}"
    );
}

/// AC-15. The reviewer template carries the drift slot, and every rendered reviewer
/// prompt substitutes it (an unsubstituted `{{...}}` reaches the model as literal text).
#[test]
fn reviewer_prompt_substitutes_the_contract_drift_slot() {
    let template = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/reviewer_adversarial.md"),
    )
    .unwrap();
    assert!(
        template.contains("{{contract_drift_note}}"),
        "the reviewer template must have somewhere to say the contract moved"
    );

    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run_id = planned_run(tmp.path());
    implement(tmp.path(), &run_id).code(2);

    let prompts = run_files(tmp.path(), &run_id, "", "prompt-review-");
    assert!(!prompts.is_empty(), "no reviewer prompts were rendered");
    for (name, text) in prompts {
        assert!(
            !text.contains("{{"),
            "{name} carries an unsubstituted placeholder:\n{text}"
        );
    }
}
