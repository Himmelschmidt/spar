//! Worktrees accumulate because cleanup was per-run and manual: a successful run ends at
//! a *gate*, not at Done, and `auto_cleanup` is off by default, so nothing ever reaped
//! them. These cover the sweep and the rejected-plan case, on real git worktrees.
use assert_cmd::cargo::cargo_bin_cmd;
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
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// A run with **real** git worktrees on disk, parked at `phase`.
///
/// The run itself is created `--dry-run` (no provider ever spawns), then the worktrees
/// are cut by hand at the paths and branches spar itself would use, and stitched into
/// `state.json`. That exercises the real `git worktree remove` path without launching an
/// agent — a live plan here would spawn the actual provider CLIs.
fn run_with_worktrees(proj: &std::path::Path, phase: &str, idle_days: i64) -> String {
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

    let parent = proj.parent().expect("project has a parent");
    let repo = proj.file_name().unwrap().to_string_lossy().into_owned();
    let mut records = Vec::new();
    for slot in ["planner", "critic"] {
        let path = parent.join(format!("{repo}-spar-{run_id}-{slot}"));
        let branch = format!("spar/{run_id}/{slot}");
        git(
            proj,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                path.to_str().unwrap(),
            ],
        );
        records.push(serde_json::json!({
            "slot_id": slot,
            "path": path,
            "branch": branch,
        }));
    }

    let state_path = proj.join(".spar/runs").join(&run_id).join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    state["phase"] = serde_json::Value::String(phase.into());
    state["dry_run"] = serde_json::Value::Bool(false);
    state["updated_at"] = serde_json::Value::String(
        (chrono::Utc::now() - chrono::Duration::days(idle_days)).to_rfc3339(),
    );
    state["worktrees"] = serde_json::Value::Array(records);
    std::fs::write(&state_path, state.to_string()).unwrap();
    run_id
}

fn worktree_paths(proj: &std::path::Path, run_id: &str) -> Vec<std::path::PathBuf> {
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    state["worktrees"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|w| w["path"].as_str().map(std::path::PathBuf::from))
        .collect()
}

#[test]
fn sweep_reaps_finished_runs_and_spares_resumable_ones() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let done = run_with_worktrees(&proj, "done", 0);
    let stopped = run_with_worktrees(&proj, "stopped", 30);
    let gated = run_with_worktrees(&proj, "awaiting_plan_approval", 30);

    let done_trees = worktree_paths(&proj, &done);
    let stopped_trees = worktree_paths(&proj, &stopped);
    assert!(
        done_trees.iter().all(|p| p.is_dir()),
        "fixture must have real worktrees"
    );

    spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", "--all", "--json"])
        .assert()
        .success();

    assert!(
        done_trees.iter().all(|p| !p.exists()),
        "a finished run's worktrees are garbage"
    );
    assert!(
        stopped_trees.iter().all(|p| p.is_dir()),
        "a stopped run is resumable: never swept without --older-than"
    );
    assert!(worktree_paths(&proj, &gated).iter().all(|p| p.is_dir()));

    // Age is the evidence nobody is coming back for the resumable ones.
    spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", "--all", "--older-than", "7d", "--json"])
        .assert()
        .success();
    assert!(
        stopped_trees.iter().all(|p| !p.exists()),
        "--older-than must reach stale resumable runs"
    );

    // Run state (plan.md, the critique) survives a sweep; only worktrees go.
    assert!(proj.join(".spar/runs").join(&done).is_dir());
}

#[test]
fn sweep_never_touches_a_run_in_flight() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let flying = run_with_worktrees(&proj, "review", 30);
    let trees = worktree_paths(&proj, &flying);

    spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", "--all", "--older-than", "1d", "--json"])
        .assert()
        .success();

    assert!(
        trees.iter().all(|p| p.is_dir()),
        "an in-flight run keeps its worktrees however stale its state file looks; \
         stop it first"
    );
}

#[test]
fn rejecting_a_plan_reaps_its_worktrees() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let run_id = run_with_worktrees(&proj, "awaiting_plan_approval", 0);
    let trees = worktree_paths(&proj, &run_id);
    assert!(!trees.is_empty() && trees.iter().all(|p| p.is_dir()));

    spar_cmd()
        .current_dir(&proj)
        .args(["reject", &run_id, "--reason", "wrong approach", "--json"])
        .assert()
        .code(1);

    assert!(
        trees.iter().all(|p| !p.exists()),
        "nothing can resume a rejected plan, so its worktrees are garbage at once"
    );
    // The plan and the critique are why it was rejected: they stay readable.
    assert!(proj
        .join(".spar/runs")
        .join(&run_id)
        .join("artifacts/plan.md")
        .is_file());
    // And no dangling git registration is left behind.
    let listed = git(&proj, &["worktree", "list"]);
    assert!(
        !listed.contains(&run_id),
        "git must not keep the worktree registered: {listed}"
    );
}

#[test]
fn cleanup_rejects_ambiguous_invocations() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    spar_cmd()
        .current_dir(&proj)
        .args(["cleanup"])
        .assert()
        .code(1);
    spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", "someid", "--all"])
        .assert()
        .code(1);
}
