//! A run is a unit of work, not an invocation (O45/O46). Dry-run end-to-end.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::process::Command;
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
    // See plan_implement.rs: without this the child resolves the primary checkout when
    // the suite runs inside a spar worktree and writes real runs into it.
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
        Command::new("git")
            .args(args)
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

fn plan(dir: &std::path::Path, task: &str) -> String {
    let out = spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            task,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    v["run_id"].as_str().expect("run_id").to_string()
}

fn state(dir: &std::path::Path, run: &str) -> serde_json::Value {
    let p = dir.join(".spar/runs").join(run).join("state.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn run_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir.join(".spar/runs")).unwrap().count()
}

/// The bypass that split biddesk's work across two ids 35 times: a plan spar wrote
/// belongs to the run that wrote it, so implementing it continues that run.
#[test]
fn implementing_a_runs_own_plan_continues_that_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run])
        .assert()
        .success();
    assert_eq!(run_count(tmp.path()), 1);
    assert_eq!(state(tmp.path(), &run)["round"], 1);

    let plan_path = tmp
        .path()
        .join(".spar/runs")
        .join(&run)
        .join("artifacts/plan.md");
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--plan",
            plan_path.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert()
        .stderr(predicate::str::contains(format!("continuing run {run}")));

    assert_eq!(run_count(tmp.path()), 1, "no second run was minted");
    let st = state(tmp.path(), &run);
    assert_eq!(st["round"], 2, "implementing the plan is round 2");
    let roles: Vec<&str> = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["role"].as_str().unwrap())
        .collect();
    assert!(roles.contains(&"planner"), "roles were: {roles:?}");
    assert!(roles.contains(&"implementer"), "roles were: {roles:?}");
}

/// A plan spar cannot trace to a run is refused, with the runs it could have meant.
#[test]
fn an_untraceable_plan_is_refused_and_names_the_candidates() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run])
        .assert()
        .success();

    let loose = tmp.path().join("loose-plan.md");
    std::fs::write(&loose, "# Plan\ndo the thing\n").unwrap();

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--plan",
            loose.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not belong to a run"))
        .stderr(predicate::str::contains(format!(
            "spar implement --run {run}"
        )));
    assert_eq!(run_count(tmp.path()), 1, "the refusal minted nothing");

    // `--new` is the explicit escape, and it does mint one.
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--plan",
            loose.to_str().unwrap(),
            "--new",
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert();
    assert_eq!(run_count(tmp.path()), 2, "--new mints a second run");
}

/// Replanning is a round on the same run, not a second run.
#[test]
fn replanning_is_a_round_on_the_same_run() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args(["reject", &run, "--reason", "too vague"])
        .assert();

    spar_cmd()
        .current_dir(tmp.path())
        .args(["plan", "--run", &run, "--task", "narrow it to one module"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("round 2"));

    assert_eq!(run_count(tmp.path()), 1, "replan minted nothing");
    let st = state(tmp.path(), &run);
    assert_eq!(st["round"], 2);
    assert_eq!(
        st["task"], "add a hello world module",
        "the brief is identity"
    );
    assert_eq!(st["amendment"], "narrow it to one module");
    assert_eq!(st["phase"], "awaiting_plan_approval", "the gate reopened");
}

/// Linking folds a leg into a unit of work, and `--undo` puts it back.
#[test]
fn link_folds_a_leg_and_undo_restores_it() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let parent = plan(tmp.path(), "the real brief");
    let leg = plan(tmp.path(), "a stray implement leg");

    spar_cmd()
        .current_dir(tmp.path())
        .args(["link", &leg, "--to", &parent])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("leg of {parent}")));
    assert_eq!(state(tmp.path(), &leg)["parent_run"], parent.as_str());

    // A run cannot be a leg of itself, and a cycle is refused.
    spar_cmd()
        .current_dir(tmp.path())
        .args(["link", &leg, "--to", &leg])
        .assert()
        .failure();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["link", &parent, "--to", &leg])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cycle"));

    spar_cmd()
        .current_dir(tmp.path())
        .args(["link", &leg, "--undo"])
        .assert()
        .success();
    assert!(state(tmp.path(), &leg)["parent_run"].is_null());
}

/// `--new` forks off a plan spar CAN trace, instead of being silently ignored.
#[test]
fn new_forks_even_from_a_traceable_plan() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    let plan_path = tmp
        .path()
        .join(".spar/runs")
        .join(&run)
        .join("artifacts/plan.md");
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--plan",
            plan_path.to_str().unwrap(),
            "--new",
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert();
    assert_eq!(run_count(tmp.path()), 2, "--new must fork, not attach");
    assert_eq!(
        state(tmp.path(), &run)["round"],
        1,
        "the original is untouched"
    );
}

/// Only an artifact is a plan. A log path under the run dir must not re-dispatch the
/// whole implement fleet on a typo.
#[test]
fn a_log_path_is_not_a_plan() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    let logs = tmp.path().join(".spar/runs").join(&run).join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let log = logs.join("planner-cli-claude.log");
    std::fs::write(&log, "not a plan\n").unwrap();
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--plan",
            log.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not belong to a run"));
    assert_eq!(
        state(tmp.path(), &run)["round"],
        1,
        "nothing was dispatched"
    );
}

/// Replanning a run someone else is driving must not half-write it, and a run
/// mid-flight is refused outright.
#[test]
fn replan_refuses_a_run_in_flight_without_touching_it() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    let sp = tmp.path().join(".spar/runs").join(&run).join("state.json");
    let mut st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sp).unwrap()).unwrap();
    st["phase"] = serde_json::json!("review");
    st["gates"]["plan_approved"] = serde_json::json!(true);
    std::fs::write(&sp, serde_json::to_string_pretty(&st).unwrap()).unwrap();

    spar_cmd()
        .current_dir(tmp.path())
        .args(["plan", "--run", &run, "--task", "redo it"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mid-flight"));

    let after = state(tmp.path(), &run);
    assert_eq!(after["phase"], "review", "phase untouched");
    assert_eq!(after["round"], 1, "round untouched");
    assert_eq!(after["gates"]["plan_approved"], true, "approval untouched");
}

/// A replan must not be able to present the previous round's plan at the gate.
#[test]
fn replan_moves_the_previous_rounds_artifacts_aside() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    let art = tmp.path().join(".spar/runs").join(&run).join("artifacts");
    assert!(art.join("plan.md").is_file());

    spar_cmd()
        .current_dir(tmp.path())
        .args(["plan", "--run", &run, "--task", "narrow it"])
        .assert()
        .code(2);

    assert!(
        art.join("plan-round1.md").is_file(),
        "the old plan is kept, out of the way"
    );
    assert!(
        art.join("test-contract-round1.md").is_file(),
        "and so is the contract it froze"
    );
}

/// Flags a replan cannot honor are refused, not dropped.
#[test]
fn replan_refuses_flags_it_cannot_apply() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let run = plan(tmp.path(), "add a hello world module");
    for flag in [
        vec!["--providers", "cli:grok"],
        vec!["--detach"],
        vec!["--big"],
    ] {
        let mut args = vec!["plan", "--run", run.as_str(), "--task", "redo"];
        args.extend(flag.iter().copied());
        spar_cmd()
            .current_dir(tmp.path())
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("cannot apply"));
    }
    assert_eq!(state(tmp.path(), &run)["round"], 1);
}

/// `--halted` reaches the phases auto-archiving refuses, and still never a gate.
#[test]
fn halted_archive_sweep_spares_gates() {
    let tmp = tempdir().unwrap();
    init_git_repo(tmp.path());
    let gated = plan(tmp.path(), "waiting on a human");
    let stopped = plan(tmp.path(), "halted work");
    // `stop` halts an in-flight run; this one is parked at a gate, so the phase is
    // set directly — the subject here is the sweep, not how a run gets halted.
    let sp = tmp
        .path()
        .join(".spar/runs")
        .join(&stopped)
        .join("state.json");
    let mut st: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sp).unwrap()).unwrap();
    st["phase"] = serde_json::json!("stopped");
    std::fs::write(&sp, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    assert_eq!(state(tmp.path(), &stopped)["phase"], "stopped");

    spar_cmd()
        .current_dir(tmp.path())
        .args(["archive", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to archive"));

    spar_cmd()
        .current_dir(tmp.path())
        .args(["archive", "--all", "--halted"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&stopped));

    assert!(!state(tmp.path(), &stopped)["archived_at"].is_null());
    assert!(
        state(tmp.path(), &gated)["archived_at"].is_null(),
        "a run waiting on a human is never swept"
    );

    // `plan_approved` is `is_terminal()` but it is NOT halted: it is the resting state
    // between `approve` and `implement --run`, and the very thing an unlinked-plan
    // error tells the operator to go continue.
    let approved = plan(tmp.path(), "approved, waiting to be implemented");
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &approved])
        .assert();
    assert_eq!(state(tmp.path(), &approved)["phase"], "plan_approved");
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args(["archive", "--all", "--halted"])
        .assert();
    assert!(
        state(tmp.path(), &approved)["archived_at"].is_null(),
        "an approved plan is not halted work"
    );

    spar_cmd()
        .current_dir(tmp.path())
        .args(["archive", "--undo", &stopped])
        .assert()
        .success();
    assert!(state(tmp.path(), &stopped)["archived_at"].is_null());
}
