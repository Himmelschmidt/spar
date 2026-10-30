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

fn commit_file(dir: &Path, rel: &str, subject: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("{subject}\n")).unwrap();
    for args in [vec!["add", rel], vec!["commit", "-q", "-m", subject]] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
    }
}

fn brief_json(dir: &Path, run_id: &str) -> serde_json::Value {
    let out = spar_cmd()
        .current_dir(dir)
        .args(["brief", run_id, "--json"])
        .assert()
        .success();
    serde_json::from_slice(out.get_output().stdout.as_slice()).unwrap()
}

/// O120. At the ceiling each open finding is listed; once a commit touches its cited
/// file, `spar brief` marks it possibly addressed and names the commit. No model call.
#[test]
fn stale_findings_are_listed_at_the_ceiling_and_marked_in_brief() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 2\n");
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "major")],
    )
    .code(2)
    .stderr(predicates::str::contains("## Open findings"));
    let escalation =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/escalation.md"))
            .unwrap();
    assert!(escalation.contains("## Open findings"), "{escalation}");
    assert!(
        escalation.contains("- major `src/lib.rs:1`"),
        "{escalation}"
    );
    assert!(!escalation.contains("possibly addressed"), "{escalation}");

    let v = brief_json(tmp.path(), &run_id);
    let open = v["open_findings"].as_array().unwrap();
    assert!(!open.is_empty(), "{v}");
    assert!(open.iter().all(|f| f["possibly_addressed"] == false), "{v}");
    assert_eq!(open[0]["file"], "src/lib.rs");

    commit_file(tmp.path(), "src/lib.rs", "fix the retry");
    commit_file(tmp.path(), "src/other.rs", "unrelated");
    let v = brief_json(tmp.path(), &run_id);
    let f = &v["open_findings"][0];
    assert_eq!(f["possibly_addressed"], true, "{v}");
    let touched = f["touched_since"].as_array().unwrap();
    assert_eq!(
        touched.len(),
        1,
        "only commits touching the cited file: {v}"
    );
    assert!(
        touched[0].as_str().unwrap().ends_with("fix the retry"),
        "{v}"
    );

    let text = spar_cmd()
        .current_dir(tmp.path())
        .args(["brief", &run_id])
        .assert()
        .success();
    let text = String::from_utf8_lossy(text.get_output().stdout.as_slice()).to_string();
    assert!(text.contains("open findings"), "{text}");
    assert!(text.contains("possibly addressed: 1 commit(s)"), "{text}");
}

/// O120 at ship: a minor the panel left, whose file a later commit touched, reaches
/// the PR body marked possibly addressed.
#[test]
fn the_pr_body_marks_findings_whose_file_moved() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 4\n");
    let run_id = planned_run(tmp.path());
    implement(
        tmp.path(),
        &run_id,
        &[("SPAR_FORCE_REVIEW_FINDING", "minor")],
    )
    .code(2);
    commit_file(tmp.path(), "src/lib.rs", "tidy the doc comment");
    spar_cmd()
        .current_dir(tmp.path())
        .args(["ship", &run_id, "--confirm", "--json"])
        .assert()
        .success();
    let ship =
        std::fs::read_to_string(run_dir(tmp.path(), &run_id).join("artifacts/ship.md")).unwrap();
    assert!(ship.contains("## Open findings (non-blocking)"), "{ship}");
    assert!(ship.contains("possibly addressed: 1 commit(s)"), "{ship}");
    assert!(ship.contains("tidy the doc comment"), "{ship}");
}

fn rule(dir: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    spar_cmd().current_dir(dir).arg("rule").args(args).assert()
}

fn rule_ids(dir: &Path, run_id: &str) -> Vec<String> {
    let out = rule(dir, &["list", "--run", run_id, "--json"]).success();
    let v: serde_json::Value = serde_json::from_slice(out.get_output().stdout.as_slice()).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

/// O121. Rulings are added, listed and removed on a run and on the project, and a run's
/// `status --json` lists the ones in force.
#[test]
fn rule_add_list_rm() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "");
    let run_id = planned_run(tmp.path());

    let out = rule(
        tmp.path(),
        &[
            "add",
            "--run",
            &run_id,
            "AC-2 is void: retired guard",
            "--waive",
            "ac-2",
            "--json",
        ],
    )
    .success();
    let r: serde_json::Value = serde_json::from_slice(out.get_output().stdout.as_slice()).unwrap();
    assert_eq!(r["id"], "R1");
    assert_eq!(r["scope"], "run");
    assert_eq!(r["waives"], serde_json::json!(["AC-2"]));
    rule(tmp.path(), &["add", "--project", "dates are UTC"]).success();
    assert!(tmp.path().join(".spar/rules/rules.json").is_file());

    assert_eq!(rule_ids(tmp.path(), &run_id), ["P1", "R1"]);
    let status = spar_cmd()
        .current_dir(tmp.path())
        .args(["status", &run_id, "--json"])
        .assert()
        .success();
    let status: serde_json::Value =
        serde_json::from_slice(status.get_output().stdout.as_slice()).unwrap();
    let ids: Vec<&str> = status["rulings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["P1", "R1"], "{status}");

    rule(
        tmp.path(),
        &["add", "--run", &run_id, "x", "--waive", "AC-9"],
    )
    .code(1)
    .stderr(predicates::str::contains("AC-9 is not a criterion"));
    rule(tmp.path(), &["add", "--project", "x", "--waive", "AC-1"]).code(1);
    rule(tmp.path(), &["add", "no target"]).code(2);

    rule(tmp.path(), &["rm", "--run", &run_id, "R1"]).success();
    assert_eq!(rule_ids(tmp.path(), &run_id), ["P1"]);
    rule(tmp.path(), &["rm", "--run", &run_id, "R1"]).code(1);
    rule(tmp.path(), &["rm", "--project", "P1"]).success();
    assert!(rule_ids(tmp.path(), &run_id).is_empty());
}

/// O121. A criterion the reviewer leaves unverified holds the run at the ceiling. A
/// waiver added to the parked run counts from the next dispatch: the prompts carry the
/// ruling, the gate stops counting the criterion, and the run reaches the ship gate.
#[test]
fn a_waived_criterion_stops_counting_at_the_acceptance_gate() {
    let tmp = tempdir().unwrap();
    project(tmp.path(), "[rounds]\nmax = 2\n");
    let run_id = planned_run(tmp.path());
    let unverified = [("SPAR_FORCE_AC_STATUS", "unverified")];
    implement(tmp.path(), &run_id, &unverified).code(2);
    assert_eq!(
        state_json(tmp.path(), &run_id)["phase"],
        "awaiting_round_extension"
    );

    rule(
        tmp.path(),
        &[
            "add",
            "--run",
            &run_id,
            "AC-2 is void: client-side, another unit owns it",
            "--waive",
            "AC-2",
        ],
    )
    .success();
    implement_with(tmp.path(), &run_id, &unverified, &["--max-rounds", "3"])
        .code(2)
        .stderr(predicates::str::contains(
            "AC-2 waived by operator ruling R1: AC-2 is void",
        ));
    let st = state_json(tmp.path(), &run_id);
    assert_eq!(st["phase"], "awaiting_ship_confirm", "{st}");

    let reviewer = reviewer_slots(tmp.path(), &run_id).remove(0);
    let prompt = last_prompt(tmp.path(), &run_id, &reviewer);
    assert!(prompt.contains("## Operator rulings"), "{prompt}");
    assert!(prompt.contains("- R1: AC-2 is void"), "{prompt}");
    assert!(prompt.contains("[waives AC-2]"), "{prompt}");
    let implementer = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let prompt = last_prompt(tmp.path(), &run_id, &implementer);
    assert!(prompt.contains("- R1: AC-2 is void"), "{prompt}");
    assert!(!prompt.contains("{{"), "{prompt}");

    let brief = spar_cmd()
        .current_dir(tmp.path())
        .args(["brief", &run_id])
        .assert()
        .success();
    let brief = String::from_utf8_lossy(brief.get_output().stdout.as_slice()).to_string();
    assert!(brief.contains("AC-2 waived by R1: AC-2 is void"), "{brief}");
}
