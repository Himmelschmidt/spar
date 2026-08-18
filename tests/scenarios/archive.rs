//! Archiving is the third state between "listed forever" and "deleted". A project that
//! drives spar from the CLI accumulates one run record per run and nothing ever removed
//! them, so the listing became mostly finished work — 69 rows in the case that prompted
//! this, with 12 runs actually waiting on a human buried among them.
//!
//! These cover the CLI surface an outer agent drives, on real run stores.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-archive-home-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

fn spar_cmd() -> assert_cmd::Command {
    let mut c = cargo_bin_cmd!("spar");
    c.env("SPAR_HOME", spar_home_dir());
    // Also isolate the *config* dir. SPAR_HOME only moves the registry; without this
    // the spawned binary still layers the developer's ~/.config/spar/config.toml under
    // the test's project, so an ordinary local setting fails scenarios that never
    // mention it. (XDG applies on Linux, where the suite runs.)
    c.env("XDG_CONFIG_HOME", spar_home_dir());
    // spar exports these into every slot; without stripping them a suite run *inside* a
    // spar worktree resolves the primary checkout and writes real runs into it.
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
    assert!(out.status.success(), "git {args:?}");
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

/// A dry-run plan parked at `phase`, idle for `idle_days`. No provider ever spawns.
fn run_in(proj: &std::path::Path, phase: &str, idle_days: i64) -> String {
    let plan = spar_cmd()
        .current_dir(proj)
        .args([
            "plan",
            "--task",
            "task",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"]
        .as_str()
        .expect("run_id")
        .to_string();

    let state_path = proj.join(".spar/runs").join(&run_id).join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state["phase"] = serde_json::Value::String(phase.into());
    state["updated_at"] = serde_json::Value::String(
        (chrono::Utc::now() - chrono::Duration::days(idle_days)).to_rfc3339(),
    );
    std::fs::write(&state_path, state.to_string()).unwrap();
    run_id
}

fn listed_ids(proj: &std::path::Path, archived: bool) -> Vec<String> {
    let mut args = vec!["status", "--json"];
    if archived {
        args.push("--archived");
    }
    let out = spar_cmd().current_dir(proj).args(args).assert().code(0);
    json_of(&out)
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn archiving_hides_a_run_from_listings_without_deleting_it() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let done = run_in(&proj, "done", 0);
    spar_cmd()
        .current_dir(&proj)
        .args(["archive", &done, "--json"])
        .assert()
        .code(0);

    assert!(
        !listed_ids(&proj, false).contains(&done),
        "hidden by default"
    );
    assert!(
        listed_ids(&proj, true).contains(&done),
        "--archived shows it"
    );

    // The record and its artifacts survive, and the id stays addressable.
    assert!(proj
        .join(".spar/runs")
        .join(&done)
        .join("state.json")
        .is_file());
    spar_cmd()
        .current_dir(&proj)
        .args(["status", &done, "--json"])
        .assert()
        .code(0);

    spar_cmd()
        .current_dir(&proj)
        .args(["archive", &done, "--undo", "--json"])
        .assert()
        .code(0);
    assert!(
        listed_ids(&proj, false).contains(&done),
        "--undo brings it back"
    );
}

/// The whole point: finished work goes quiet, gates stay in your face.
#[test]
fn archive_all_takes_finished_runs_and_spares_gates() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    // Auto-archive off here: `spar plan` fires the launch hook, which would archive
    // `old_done` before the explicit `--all` ever ran. The hook has its own test.
    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"off\"\n").unwrap();

    let old_done = run_in(&proj, "done", 30);
    let fresh_done = run_in(&proj, "done", 1);
    let gate = run_in(&proj, "awaiting_plan_approval", 30);
    let stopped = run_in(&proj, "stopped", 30);

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["archive", "--all", "--older-than", "14d", "--json"])
        .assert()
        .code(0);
    let archived: Vec<String> = json_of(&out)["archived"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    assert_eq!(archived, vec![old_done.clone()]);
    let visible = listed_ids(&proj, false);
    assert!(!visible.contains(&old_done));
    assert!(visible.contains(&fresh_done), "younger than --older-than");
    assert!(
        visible.contains(&gate),
        "a run waiting on a human must never be auto-archived"
    );
    assert!(
        visible.contains(&stopped),
        "stopped is ambiguous, not finished"
    );
}

/// An in-flight run cannot be hidden, and a resumed one comes back on its own.
#[test]
fn in_flight_runs_refuse_and_resuming_unarchives() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let live = run_in(&proj, "review", 0);
    spar_cmd()
        .current_dir(&proj)
        .args(["archive", &live])
        .assert()
        .failure();

    // Archived, then re-approved and resumed: the phase change clears the flag.
    let parked = run_in(&proj, "awaiting_plan_approval", 0);
    spar_cmd()
        .current_dir(&proj)
        .args(["archive", &parked, "--json"])
        .assert()
        .code(0);
    assert!(!listed_ids(&proj, false).contains(&parked));

    spar_cmd()
        .current_dir(&proj)
        .args(["approve", &parked, "--json"])
        .assert()
        .code(0);
    // Exit code is whatever the dry-run implement lands on; the point is that the run
    // moved, which is what has to clear the archived flag.
    let _ = spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &parked,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert();
    assert!(
        listed_ids(&proj, false).contains(&parked),
        "a run that started moving again must be visible"
    );
}

/// Launching anything archives finished runs that have gone quiet. Read commands must
/// never do this — `status` is observe-only, so it can never be what hid a run from you.
#[test]
fn launch_archives_quiet_finished_runs_and_status_does_not() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    // Seed with the hook off: creating a run *is* a launch, so it would fire mid-setup.
    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"off\"\n").unwrap();
    let old_done = run_in(&proj, "done", 30);
    let gate = run_in(&proj, "awaiting_plan_approval", 30);
    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"14d\"\n").unwrap();

    // Reading never mutates, however many times you look.
    let _ = listed_ids(&proj, false);
    assert!(
        listed_ids(&proj, false).contains(&old_done),
        "status must not archive"
    );

    // Launching does.
    run_in(&proj, "done", 0);
    let visible = listed_ids(&proj, false);
    assert!(
        !visible.contains(&old_done),
        "quiet finished run archived at launch"
    );
    assert!(visible.contains(&gate), "a gate is never auto-archived");
}

#[test]
fn auto_archive_can_be_turned_off() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"off\"\n").unwrap();

    let old_done = run_in(&proj, "done", 90);
    run_in(&proj, "done", 0);
    assert!(
        listed_ids(&proj, false).contains(&old_done),
        "off means off, however old"
    );
}

/// Approving an auto-archived rejected plan must bring it back. It is approved and
/// waiting for `spar implement`; leaving it hidden means nothing in any listing says so.
#[test]
fn approving_an_archived_rejected_plan_unarchives_it() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"off\"\n").unwrap();
    let rejected = run_in(&proj, "plan_rejected", 30);
    std::fs::write(proj.join("spar.toml"), "auto_archive_after = \"14d\"\n").unwrap();

    // The launch hook archives it automatically -- no manual archive anywhere.
    run_in(&proj, "done", 0);
    assert!(
        !listed_ids(&proj, false).contains(&rejected),
        "auto-archived"
    );

    spar_cmd()
        .current_dir(&proj)
        .args(["approve", &rejected, "--json"])
        .assert()
        .code(0);
    assert!(
        listed_ids(&proj, false).contains(&rejected),
        "an approved run must be visible"
    );
}
