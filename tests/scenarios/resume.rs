//! `spar resume <id>`: dispatch on what the run actually is, rather than aliasing
//! `implement --run`. A gate is refused (a decision is waiting), a live owner is
//! refused (naming its pid), a finished run is refused, and an at-rest resumable run
//! (`stopped`, in these tests) is picked back up through the same path a bare
//! `implement --run <id>` would take.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-resume-home-{}", std::process::id()));
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
    c
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn json_of(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
        .expect("json")
}

fn state_path(proj: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    proj.join(".spar/runs").join(run_id).join("state.json")
}

fn set_phase(proj: &std::path::Path, run_id: &str, phase: &str) {
    let path = state_path(proj, run_id);
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    state["phase"] = serde_json::Value::String(phase.into());
    std::fs::write(&path, state.to_string()).unwrap();
}

/// A dry-run `implement -t` reaches the ship gate (default `[gates] ship = true`), no
/// live orchestrator ever spawns for a dry run.
fn implement_dry_run(proj: &std::path::Path) -> String {
    let out = spar_cmd()
        .current_dir(proj)
        .args([
            "implement",
            "-t",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("json");
    v["id"].as_str().expect("run id").to_string()
}

/// An approved plan the operator parked (`spar stop`) before implementation ever
/// dispatched — the common shape of a `stopped` run, and the one `run_from_approved`
/// is built to pick back up (it clears the `stopped` marker and dispatches fresh
/// implementer slots exactly as a bare `implement --run <id>` would).
#[test]
fn a_stopped_run_resumes_and_advances() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(&proj)
        .args(["approve", &run_id, "--json"])
        .assert()
        .code(0);
    set_phase(&proj, &run_id, "stopped");

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id, "--json"])
        .assert();
    let v = json_of(&out);
    assert_ne!(
        v["phase"], "stopped",
        "resume must move the run off `stopped`"
    );
}

/// The same hazard `Phase::Quota` was fixed for: `spar stop` can park a plan run at
/// `Stopped` before it was ever approved (mid-dispatch, before it ever reached its own
/// gate). `spar resume` must refuse it explicitly, naming the human command, rather than
/// falling through to `run_from_approved`'s generic "plan is not approved" bail.
#[test]
fn a_stopped_unapproved_plan_run_refuses_naming_the_human_command() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();

    let path = state_path(&proj, &run_id);
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(state["gates"]["plan_approved"], false, "never approved");
    assert_eq!(state["workflow"], "plan");
    set_phase(&proj, &run_id, "stopped");

    spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains("spar plan --run"))
        .stderr(predicates::str::contains("spar approve"));
}

/// An *approved* plan run parked at `Stopped` stays resumable through `resume` — this is
/// `a_stopped_run_resumes_and_advances` above, restated to make the `plan_approved`
/// boundary explicit rather than incidental.
#[test]
fn a_stopped_approved_plan_run_stays_resumable() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(&proj)
        .args(["approve", &run_id, "--json"])
        .assert()
        .code(0);
    set_phase(&proj, &run_id, "stopped");

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id, "--json"])
        .assert();
    let v = json_of(&out);
    assert_ne!(v["phase"], "stopped");
}

/// A `--workflow loop` run has no approval step at all, so a `Stopped` park there must
/// stay resumable regardless of `gates.plan_approved` — the same carve-out `Phase::Quota`
/// already has for loop workflows.
#[test]
fn a_stopped_loop_workflow_run_stays_resumable() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = implement_dry_run(&proj);
    let path = state_path(&proj, &run_id);
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(state["workflow"], "loop");
    set_phase(&proj, &run_id, "stopped");

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id, "--json"])
        .assert();
    let v = json_of(&out);
    assert_ne!(
        v["phase"], "stopped",
        "a loop-workflow stopped run must stay resumable"
    );
}

/// A dry-run `--workflow review`: no plan-approval gate exists for this workflow at
/// all, so `gates.plan_approved` stays `false` the whole time — exactly the shape that
/// used to make `resume`'s old `workflow != Loop` guard misfire.
fn review_dry_run(proj: &std::path::Path) -> String {
    let out = spar_cmd()
        .current_dir(proj)
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "review this",
            "--providers",
            "cli:claude,cli:claude",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("json");
    v["run_id"].as_str().expect("run id").to_string()
}

/// The bug this round fixes: the previous guard refused *every* non-`Loop` workflow
/// parked at `Stopped` with `gates.plan_approved == false`, including `Review`, which
/// has no plan-approval gate at all and so never sets that flag `true`. `resume` must
/// route a stopped `Review` run to its own continuation (`workflow::review::execute`,
/// via `implement::continue_run`'s dispatch) instead of refusing it or driving it
/// through `run_from_approved`, which would rewrite its workflow to `Loop`.
#[test]
fn a_stopped_review_workflow_run_resumes_into_its_own_continuation() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = review_dry_run(&proj);
    let path = state_path(&proj, &run_id);
    let before: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(before["workflow"], "review");
    assert_eq!(
        before["gates"]["plan_approved"], false,
        "review has no approval gate; this must never gate resume"
    );

    set_phase(&proj, &run_id, "stopped");

    // Unlike `run_from_approved`'s path, `continue_run`'s per-workflow dispatch
    // (`review::execute`, here) prints nothing on its own — the same as the existing
    // abandoned-in-flight foreground path. So the result is read back off disk, not
    // off stdout.
    let _ = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_ne!(
        after["phase"], "stopped",
        "a stopped review run must resume, not stay parked"
    );
    assert_eq!(
        after["workflow"], "review",
        "resuming must not rewrite the run's workflow to loop"
    );
}

#[test]
fn a_gate_refuses_and_names_the_human_command() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--task",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();

    spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains("spar approve"));
}

#[test]
fn a_live_orchestrator_refuses_naming_its_pid() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = implement_dry_run(&proj);
    set_phase(&proj, &run_id, "dispatch");
    let lock_path = proj
        .join(".spar/runs")
        .join(&run_id)
        .join("orchestrator.lock");
    std::fs::write(&lock_path, std::process::id().to_string()).unwrap();

    spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "already has a running orchestrator",
        ));
}

#[test]
fn a_finished_run_refuses() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = implement_dry_run(&proj);
    set_phase(&proj, &run_id, "done");

    spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicates::str::contains("nothing to resume"));
}

/// An abandoned in-flight run (not a waitable stop, no live owner) is the one case
/// `resume --detach` actually exists for. It must route through the same
/// `spawn_detached_orchestrator` / handshake machinery `implement --detach` uses,
/// rather than silently ignoring `--detach` and calling `continue_run` in this
/// process. `spawn_detached_orchestrator` always opens `logs/orchestrator.log` before
/// the child even runs, so its presence is proof the detach path was taken — checking
/// only that the run advanced would not catch a regression back to the foreground
/// path, since `continue_run` also advances the run, just never through that log.
#[test]
fn an_abandoned_in_flight_run_resumes_through_the_detach_path() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = implement_dry_run(&proj);
    set_phase(&proj, &run_id, "dispatch");

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id, "--detach", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.code() == Some(0) || out.status.code() == Some(2),
        "resume --detach must not fail the launch: {out:?}"
    );

    let log = proj
        .join(".spar/runs")
        .join(&run_id)
        .join("logs/orchestrator.log");
    assert!(
        log.is_file(),
        "resume --detach on an in-flight abandoned run must spawn a detached \
         orchestrator, not run continue_run in this process"
    );

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_path(&proj, &run_id)).unwrap())
            .unwrap();
    assert_ne!(
        state["phase"], "dispatch",
        "the resumed run must actually advance"
    );
}
