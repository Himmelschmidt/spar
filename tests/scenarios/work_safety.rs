//! Work safety and run lifecycle: cleanup that cannot destroy unlanded work, a round that
//! is only charged when it dispatched, orchestrators that notice their run was removed,
//! reclaim over halted runs, and reconcile/resume over abandoned ones.
use assert_cmd::cargo::cargo_bin_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-worksafety-home-{}", std::process::id()));
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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    std::fs::write(dir.join(".gitignore"), ".spar/\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn state_path(proj: &Path, run_id: &str) -> PathBuf {
    proj.join(".spar/runs").join(run_id).join("state.json")
}

fn read_state(proj: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(state_path(proj, run_id)).unwrap()).unwrap()
}

fn write_state(proj: &Path, run_id: &str, state: &serde_json::Value) {
    std::fs::write(state_path(proj, run_id), state.to_string()).unwrap();
}

/// A dry-run `implement -t` parked at the ship gate.
fn implement_dry_run(proj: &Path) -> String {
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

/// Re-point a dry run's implementer at a real git worktree with one commit on its branch,
/// so cleanup's git evidence has something to judge. Returns (worktree, branch tip).
fn give_run_a_real_worktree(proj: &Path, run_id: &str) -> (PathBuf, String) {
    let wt = proj
        .parent()
        .unwrap()
        .join(format!("proj-spar-{run_id}-impl"));
    let branch = format!("spar/{run_id}/impl");
    git(
        proj,
        &["worktree", "add", "-q", "-b", &branch, wt.to_str().unwrap()],
    );
    std::fs::write(wt.join("work.rs"), "fn work() {}\n").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", "work"]);
    let tip = git(&wt, &["rev-parse", "HEAD"]);
    let mut state = read_state(proj, run_id);
    state["dry_run"] = serde_json::Value::Bool(false);
    state["worktrees"] = serde_json::json!([{ "slot_id": "impl", "path": wt, "branch": branch }]);
    write_state(proj, run_id, &state);
    (wt, tip)
}

/// A `gh` that answers `pr view` with a fixed PR state.
fn fake_gh(dir: &Path, state: &str, head: &str) -> String {
    let bin = dir.join(format!("fakebin-{state}"));
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        format!("#!/bin/sh\necho '{{\"state\":\"{state}\",\"headRefOid\":\"{head}\"}}'\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// #157: an open PR is not a merge. `--if-merged` refuses and removes nothing.
#[test]
fn cleanup_if_merged_refuses_a_run_whose_pr_is_open() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    let run_id = implement_dry_run(&proj);
    let (wt, tip) = give_run_a_real_worktree(&proj, &run_id);

    let out = spar_cmd()
        .current_dir(&proj)
        .env("PATH", fake_gh(tmp.path(), "OPEN", &tip))
        .args(["cleanup", &run_id, "--if-merged", "--purge"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not merged"), "{err}");
    assert!(wt.join("work.rs").is_file(), "the worktree must survive");
    assert!(
        state_path(&proj, &run_id).is_file(),
        "the record must survive"
    );
}

/// #157: a squash-merged PR whose head is the branch tip is landed, so `--if-merged`
/// reaps without `--force`, and the work is still archived on the way out.
#[test]
fn cleanup_if_merged_reaps_a_run_whose_pr_merged() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    let run_id = implement_dry_run(&proj);
    let (wt, tip) = give_run_a_real_worktree(&proj, &run_id);

    spar_cmd()
        .current_dir(&proj)
        .env("PATH", fake_gh(tmp.path(), "MERGED", &tip))
        .args(["cleanup", &run_id, "--if-merged"])
        .assert()
        .code(0);
    assert!(!wt.exists(), "a landed run's worktree is reaped");
    assert_eq!(
        git(
            &proj,
            &["rev-parse", &format!("refs/spar-archive/{run_id}/impl")]
        ),
        tip
    );
}

#[test]
fn cleanup_if_merged_is_refused_for_a_sweep() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    let out = spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", "--all", "--if-merged"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

/// #157: `--purge` removed the reviews and suite report merge decisions are made from.
/// It now keeps `artifacts/`; `--purge-artifacts` is the way to drop them too.
#[test]
fn purge_keeps_artifacts_unless_purge_artifacts() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);

    let kept = implement_dry_run(&proj);
    let out = spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", &kept, "--purge", "--json"])
        .assert()
        .code(0);
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).expect("json");
    assert_eq!(v["purged"], true);
    assert_eq!(v["artifacts_kept"], true);
    let run_dir = proj.join(".spar/runs").join(&kept);
    assert!(!run_dir.join("state.json").exists(), "the record is purged");
    assert!(
        run_dir.join("artifacts/summary-impl.md").is_file(),
        "artifacts survive"
    );

    let gone = implement_dry_run(&proj);
    spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", &gone, "--purge-artifacts"])
        .assert()
        .code(0);
    assert!(!proj.join(".spar/runs").join(&gone).exists());
}
