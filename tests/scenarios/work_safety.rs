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

/// A fake `muse` for the ceiling test. The implementer commits its work and then hangs
/// past the hard ceiling; the artifact-only recovery turn (`prompt-impl-artifact.md`)
/// writes the summary; reviewers approve the commit they were shown.
fn install_ceiling_muse(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let p = bin.join("muse");
    std::fs::write(
        &p,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-muse 0"; exit 0; fi
PROMPT_FILE=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--prompt-file" ]; then PROMPT_FILE="$a"; fi
  prev="$a"
done
NAME=$(basename "$PROMPT_FILE" .md)
NAME=${NAME#prompt-}
ART="$(dirname "$PROMPT_FILE")/artifacts"
mkdir -p "$ART"
echo '{"schema_version":1,"stream":{"kind":"session","id":"sess-c"},"record_type":"event","payload_type":"run.model.configured","payload":{"kind":"run_model_configured","model_id":"m"}}'
case "$NAME" in
  impl)
    echo ceiling >> "$SPAR_TEST_MUSE_STATE/calls"
    echo work > work.txt
    git add work.txt
    git commit -qm work
    sleep 60
    ;;
  impl-artifact)
    echo recovery >> "$SPAR_TEST_MUSE_STATE/calls"
    printf '# Summary\nCommitted work.txt.\n' > "$ART/summary-impl.md"
    ;;
  *)
    CWD=$(sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' "$PROMPT_FILE" | head -n 1)
    SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null)
    printf '## Verdict\napprove\n\nReviewed-Commit: %s\n' "$SHA" > "$ART/review-$NAME.md"
    ;;
esac
exit 0
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// #180: a slot killed at the hard ceiling after committing its work is not a failed
/// slot. It takes the recovery turn a clean exit without its artifact takes, and the run
/// goes on to review in the same round.
#[test]
fn a_ceiling_killed_implementer_with_new_commits_is_recovered() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    std::fs::write(
        proj.join("spar.toml"),
        "[suite]\nenabled = false\n[rounds]\nmax = 1\n[timeouts]\nslot_secs = 2\nhard_ceiling_multiple = 1.5\n",
    )
    .unwrap();
    let bin = tmp.path().join("bin");
    install_ceiling_muse(&bin);
    let calls_dir = tmp.path().join("muse-state");
    std::fs::create_dir_all(&calls_dir).unwrap();

    let out = spar_cmd()
        .current_dir(&proj)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("SPAR_TEST_MUSE_STATE", &calls_dir)
        .args([
            "implement",
            "-t",
            "add work",
            "--providers",
            "cli:muse",
            "--fleet",
            "small",
            "--json",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("json: {}", String::from_utf8_lossy(&out.stderr)));
    let run_id = v["run_id"].as_str().expect("run_id").to_string();
    let calls = std::fs::read_to_string(calls_dir.join("calls")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["ceiling", "recovery"],
        "{calls}"
    );
    let st = read_state(&proj, &run_id);
    let impl_slot = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot")
        .clone();
    assert_eq!(impl_slot["status"], "done", "{impl_slot}");
    assert_eq!(st["round"], 1, "{st}");
    assert_eq!(st["phase"], "awaiting_ship_confirm", "{st}");
}

fn set_phase(proj: &Path, run_id: &str, phase: &str) {
    let mut state = read_state(proj, run_id);
    state["phase"] = serde_json::Value::String(phase.into());
    write_state(proj, run_id, &state);
}

/// Make this test process the run's live orchestrator, as far as the lock file says.
fn hold_lock(proj: &Path, run_id: &str) {
    let lock = proj
        .join(".spar/runs")
        .join(run_id)
        .join("orchestrator.lock");
    std::fs::write(lock, std::process::id().to_string()).unwrap();
}

/// #179: purging a run under its live orchestrator left that orchestrator respawning
/// slots into a directory that was gone. Cleanup refuses while one is alive.
#[test]
fn cleanup_refuses_a_run_with_a_live_orchestrator() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    let run_id = implement_dry_run(&proj);
    hold_lock(&proj, &run_id);

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["cleanup", &run_id, "--purge"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("live orchestrator"),
        "{out:?}"
    );
    assert!(state_path(&proj, &run_id).is_file());
}

/// #179: re-entry after an orchestrator died between opening a round and dispatching
/// anything in it reuses that round instead of charging another.
#[test]
fn reentry_reuses_a_round_that_never_dispatched() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    let run_id = implement_dry_run(&proj);
    let mut state = read_state(&proj, &run_id);
    assert_eq!(state["round"], 1);
    state["round"] = serde_json::json!(2);
    state["phase"] = serde_json::json!("stopped");
    write_state(&proj, &run_id, &state);

    spar_cmd()
        .current_dir(&proj)
        .args(["implement", "--run", &run_id, "--dry-run", "--json"])
        .assert()
        .code(2);
    assert_eq!(
        read_state(&proj, &run_id)["round"],
        2,
        "round 2 never dispatched"
    );

    set_phase(&proj, &run_id, "stopped");
    spar_cmd()
        .current_dir(&proj)
        .args(["implement", "--run", &run_id, "--dry-run", "--json"])
        .assert()
        .code(2);
    assert_eq!(
        read_state(&proj, &run_id)["round"],
        3,
        "round 2 did dispatch, so the next re-entry is a new round"
    );
}

/// #179: a run whose stdin is not a terminal and that was not detached is told, loudly,
/// that it dies with its shell. It is not detached behind the caller's back.
#[test]
fn a_foreground_launch_without_a_tty_warns_that_it_dies_with_the_shell() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    let run_id = implement_dry_run(&proj);
    set_phase(&proj, &run_id, "dispatch");
    hold_lock(&proj, &run_id);

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("dies with the shell"), "{err}");

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["resume", &run_id, "--detach"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("dies with the shell"), "{err}");
}

/// A fake `muse` whose implementer deletes its own run dir mid-dispatch, the way a purge
/// from another shell does.
fn install_purging_muse(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let p = bin.join("muse");
    std::fs::write(
        &p,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-muse 0"; exit 0; fi
PROMPT_FILE=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--prompt-file" ]; then PROMPT_FILE="$a"; fi
  prev="$a"
done
echo "dispatch" >> "$SPAR_TEST_MUSE_STATE/calls"
rm -rf "$(dirname "$PROMPT_FILE")"
exit 0
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// #179: an orchestrator whose run dir disappears stops at its next write instead of
/// recreating the run and dispatching more slots into it.
#[test]
fn an_orchestrator_whose_run_dir_is_deleted_exits_without_recreating_it() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    init_repo(&proj);
    std::fs::write(proj.join("spar.toml"), "[suite]\nenabled = false\n").unwrap();
    let bin = tmp.path().join("bin");
    install_purging_muse(&bin);
    let calls_dir = tmp.path().join("muse-state");
    std::fs::create_dir_all(&calls_dir).unwrap();

    let out = spar_cmd()
        .current_dir(&proj)
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("SPAR_TEST_MUSE_STATE", &calls_dir)
        .args([
            "implement",
            "-t",
            "anything",
            "--providers",
            "cli:muse",
            "--fleet",
            "small",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("removed from disk"), "{err}");
    let calls = std::fs::read_to_string(calls_dir.join("calls")).unwrap();
    assert_eq!(
        calls.lines().count(),
        1,
        "no slot dispatched after: {calls}"
    );
    let runs: Vec<_> = std::fs::read_dir(proj.join(".spar/runs"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(runs.is_empty(), "the run dir was recreated: {runs:?}");
}
