//! Driving spar from a linked worktree must base the run on that worktree's HEAD.
//!
//! The regression this guards: `find_project_root()` resolves a linked worktree to the
//! repo's main checkout (so `.spar/` stays one per repo), and the run used to take its
//! base from there — handing every slot the main checkout's branch while the run looked
//! perfectly healthy.
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
        // The developer's global config (gpg signing, hooks, init.defaultBranch) must
        // not decide what this fixture looks like or whether it builds at all.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed in {dir:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `<tmp>/repo` on its default branch + `<tmp>/wt` on `feat` with an extra commit.
fn repo_with_linked_worktree(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = tmp.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("README.md"), "test\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);

    let wt = tmp.join("wt");
    git(
        &root,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    std::fs::write(wt.join("feature.txt"), "work\n").unwrap();
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", "feature work"]);
    (root, wt)
}

fn plan_run_base(cwd: &std::path::Path, extra: &[&str]) -> (String, String) {
    let mut args = vec![
        "plan",
        "--task",
        "review this branch",
        "--providers",
        "cli:claude,cli:grok",
        "--dry-run",
        "--json",
    ];
    args.extend_from_slice(extra);
    let out = spar_cmd().current_dir(cwd).args(&args).assert().code(2);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    (
        v["run_id"].as_str().expect("run_id").to_string(),
        v["base_commit"]
            .as_str()
            .expect("base_commit in run json")
            .to_string(),
    )
}

#[test]
fn run_driven_from_a_worktree_is_based_on_that_worktree() {
    let tmp = tempdir().unwrap();
    let (root, wt) = repo_with_linked_worktree(tmp.path());
    let feat_head = git(&wt, &["rev-parse", "HEAD"]);
    let main_head = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(feat_head, main_head);

    let (run_id, base_commit) = plan_run_base(&wt, &[]);
    assert_eq!(
        base_commit, feat_head,
        "base must come from the invoking worktree, not the main checkout"
    );

    // The run still lives in the main checkout's `.spar/`, and status reports the base.
    let status = spar_cmd()
        .current_dir(&wt)
        .args(["status", &run_id, "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        status.get_output().stdout.as_slice(),
    ))
    .expect("json");
    assert_eq!(v["base_ref"], "feat");
    assert_eq!(v["base_commit"], feat_head);
    assert!(root.join(".spar/runs").join(&run_id).is_dir());
}

#[test]
fn explicit_base_overrides_the_invoking_branch() {
    let tmp = tempdir().unwrap();
    let (root, wt) = repo_with_linked_worktree(tmp.path());
    let main_branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let main_head = git(&root, &["rev-parse", "HEAD"]);

    let (_, base_commit) = plan_run_base(&wt, &["--base", &main_branch]);
    assert_eq!(base_commit, main_head);

    // `HEAD` is per-worktree: the explicit spelling of "cut from where I am" must not
    // silently resolve against the main checkout.
    let (_, from_head) = plan_run_base(&wt, &["--base", "HEAD"]);
    assert_eq!(from_head, git(&wt, &["rev-parse", "HEAD"]));

    // Exit 1, not merely non-zero: the dry-run happy path already exits 2 (plan gate),
    // so `.failure()` alone would pass even if the bad ref were silently ignored.
    spar_cmd()
        .current_dir(&wt)
        .args([
            "plan",
            "--task",
            "x",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
            "--base",
            "no/such/ref",
        ])
        .assert()
        .code(1);
}

/// `implement --run <id>` inherits the plan's base, and refuses to re-point an existing
/// run: the plan-phase test-author tree is overlaid onto the implementer wholesale, so a
/// run straddling two bases silently reverts every file that differs between them.
#[test]
fn implement_inherits_the_runs_base_and_refuses_to_rebase_it() {
    let tmp = tempdir().unwrap();
    let (root, wt) = repo_with_linked_worktree(tmp.path());
    let feat_head = git(&wt, &["rev-parse", "HEAD"]);

    let (run_id, base_commit) = plan_run_base(&wt, &[]);
    assert_eq!(base_commit, feat_head);

    spar_cmd()
        .current_dir(&wt)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();

    // Resume from the MAIN checkout — a detached orchestrator or the TUI does exactly
    // this. Running it from `wt` could not tell inheritance apart from re-resolution.
    let out = spar_cmd()
        .current_dir(&root)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
            .expect("json");
    assert_eq!(
        v["base_commit"], feat_head,
        "resume must keep the run's base"
    );

    // Re-point: refused now that the run has worktrees.
    spar_cmd()
        .current_dir(&wt)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
            "--base",
            "master",
        ])
        .assert()
        .code(1)
        .stderr(predicates::str::contains(
            "base is fixed when it is created",
        ));
}

/// `ship` targets the PR at the run's base branch, and `--base` overrides it. Dry-run
/// ship records the exact `gh pr create` argv, which is where the flag has to land.
#[test]
fn ship_targets_the_runs_base_branch() {
    let tmp = tempdir().unwrap();
    let (root, wt) = repo_with_linked_worktree(tmp.path());
    // A remote-tracking ref makes `feat` look pushed without a real remote.
    git(
        &root,
        &[
            "update-ref",
            "refs/remotes/origin/feat",
            &git(&wt, &["rev-parse", "HEAD"]),
        ],
    );

    let (run_id, _) = plan_run_base(&wt, &[]);
    spar_cmd()
        .current_dir(&wt)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    spar_cmd()
        .current_dir(&wt)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2); // implement --dry-run stops at the ship gate
    spar_cmd()
        .current_dir(&wt)
        .args(["ship", &run_id, "--confirm", "--json"])
        .assert()
        .success();

    let ship_md = std::fs::read_to_string(
        root.join(".spar/runs")
            .join(&run_id)
            .join("artifacts/ship.md"),
    )
    .expect("ship.md");
    assert!(
        ship_md.contains("PR base: `feat`") && ship_md.contains("--base 'feat'"),
        "PR must target the run's base branch; got:\n{ship_md}"
    );

    // Explicit override wins over the run's own base.
    spar_cmd()
        .current_dir(&wt)
        .args(["ship", &run_id, "--json", "--base", "release/9"])
        .assert()
        .success();
    let ship_md = std::fs::read_to_string(
        root.join(".spar/runs")
            .join(&run_id)
            .join("artifacts/ship.md"),
    )
    .unwrap();
    assert!(
        ship_md.contains("PR base: `release/9`") && ship_md.contains("--base 'release/9'"),
        "ship --base must override; got:\n{ship_md}"
    );
}
