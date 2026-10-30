//! Review gating scenarios (dry-run, no live AI): severity-aware gating and scoped
//! re-review (O118, O119), stale findings (O120), operator rulings and AC waivers (O121).
use assert_cmd::cargo::cargo_bin_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

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
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c.env_remove("SPAR_FORCE_REQUEST_CHANGES");
    c.env_remove("SPAR_FORCE_AC_STATUS");
    c.env_remove("SPAR_FORCE_REVIEW_FINDING");
    c
}

fn project(dir: &Path, toml: &str) {
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
    std::fs::write(dir.join("spar.toml"), toml).unwrap();
    for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
        Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .unwrap();
    }
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
    let v: serde_json::Value = serde_json::from_slice(plan.get_output().stdout.as_slice()).unwrap();
    let run_id = v["run_id"].as_str().expect("run_id").to_string();
    spar_cmd()
        .current_dir(dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_id
}

fn implement(dir: &Path, run_id: &str, env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
    implement_with(dir, run_id, env, &[])
}

fn implement_with(
    dir: &Path,
    run_id: &str,
    env: &[(&str, &str)],
    extra: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = spar_cmd();
    cmd.current_dir(dir)
        .args([
            "implement",
            "--run",
            run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .args(extra);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.assert()
}

fn run_dir(dir: &Path, run_id: &str) -> PathBuf {
    dir.join(".spar/runs").join(run_id)
}

fn state_json(dir: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(run_dir(dir, run_id).join("state.json")).unwrap())
        .unwrap()
}

fn reviewer_slots(dir: &Path, run_id: &str) -> Vec<String> {
    state_json(dir, run_id)["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == "reviewer")
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

fn last_prompt(dir: &Path, run_id: &str, slot_id: &str) -> String {
    std::fs::read_to_string(run_dir(dir, run_id).join(format!("prompt-{slot_id}.md"))).unwrap()
}

/// O118. A panel whose only reviewer findings are minor reaches the ship gate at the
/// default `block_on = "major"`, and the minors land in the PR body.
#[test]
fn minors_only_request_changes_ships_with_notes_by_default() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 4\n");
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "minor")],
    )
    .code(2);

    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_ship_confirm", "{st}");
    assert_eq!(
        st["round"].as_u64(),
        Some(2),
        "no fix round was bought: {st}"
    );
    let review = &st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "reviewer")
        .unwrap()["review"];
    assert_eq!(review["verdict"], "request_changes", "{review}");
    assert_eq!(review["synthetic"], false, "{review}");

    spar_cmd()
        .current_dir(tmp.path())
        .args(["ship", &run_id, "--confirm", "--json"])
        .assert()
        .success();
    let ship =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/ship.md")).unwrap();
    assert!(ship.contains("## Open findings (non-blocking)"), "{ship}");
    assert!(ship.contains("- minor `src/lib.rs:1`"), "{ship}");
}

/// `block_on = "minor"` restores the strict gate: the same panel holds the run.
#[test]
fn block_on_minor_holds_a_minors_only_panel() {
    let tmp = tempdir().unwrap();
    project(
        tmp.path(),
        "[rounds]\nmax = 3\n[review]\nblock_on = \"minor\"\n",
    );
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "minor")],
    )
    .code(2);
    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_round_extension", "{st}");
}

/// A major still blocks at the default, and a major filed under Follow-up does not.
#[test]
fn a_major_blocks_and_a_follow_up_does_not() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 3\n");
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "major")],
    )
    .code(2);
    assert_eq!(
        state_json(tmp.path(), &run_id)["phase"],
        "awaiting_round_extension"
    );

    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 3\n");
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "follow-up")],
    )
    .code(2);
    assert_eq!(
        state_json(tmp.path(), &run_id)["phase"],
        "awaiting_ship_confirm"
    );
}

/// O119. Round 2 is a full review; from `scoped_from_round` (3) the panel is handed the
/// previous panel's findings and told where new non-critical work goes.
#[test]
fn scoped_re_review_renders_from_its_round() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 2\n");
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "major")],
    )
    .code(2);
    let slot = reviewer_slots(tmp.path(), &run_id).remove(0);
    let round2 = last_prompt(tmp.path(), &run_id, &slot);
    assert!(!round2.contains("Scoped re-review"), "{round2}");
    assert!(!round2.contains("{{"), "{round2}");

    implement_with(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "major")],
        &["--max-rounds", "3"],
    )
    .code(2);
    assert_eq!(state_json(tmp.path(), &run_id)["round"].as_u64(), Some(3));
    let round3 = last_prompt(tmp.path(), &run_id, &slot);
    assert!(round3.contains("## Scoped re-review (round 3)"), "{round3}");
    assert!(round3.contains("### Prior findings"), "{round3}");
    assert!(
        round3.contains("- major `src/lib.rs:1`") && round3.contains("dry-run forced finding"),
        "{round3}"
    );
    assert!(round3.contains("`## Follow-up`"), "{round3}");
    assert!(!round3.contains("{{"), "{round3}");
}

#[test]
fn scoped_re_review_can_be_turned_off() {
    let tmp = tempdir().unwrap();
    project(
        tmp.path(),
        "[rounds]\nmax = 3\n[review]\nscoped_from_round = 0\n",
    );
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "major")],
    )
    .code(2);
    assert_eq!(state_json(tmp.path(), &run_id)["round"].as_u64(), Some(3));
    let slot = reviewer_slots(tmp.path(), &run_id).remove(0);
    let prompt = last_prompt(tmp.path(), &run_id, &slot);
    assert!(!prompt.contains("Scoped re-review"), "{prompt}");
}
