//! Ship against a fake `gh` on PATH and a bare-repo origin: adopting an existing PR
//! (#164), `[ship] draft`, `sync-prs`, `--override-gate` (#166) and `--adopt-remote`
//! (#163).
use assert_cmd::cargo::cargo_bin_cmd;
use serde_json::Value;
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
    c.env("GIT_CONFIG_GLOBAL", "/dev/null");
    c.env("GIT_CONFIG_SYSTEM", "/dev/null");
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
    assert!(
        out.status.success(),
        "git {args:?} failed in {dir:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const FAKE_GH: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_GH_LOG"
case "$1 $2" in
  "--version "*) echo "gh version 0.0.0-fake" ;;
  "pr list") printf '%s' "${FAKE_GH_LIST:-[]}" ;;
  "pr view")
    if [ -n "$FAKE_GH_VIEW_FAIL" ]; then echo "$FAKE_GH_VIEW_FAIL" >&2; exit 1; fi
    printf '%s' "$FAKE_GH_VIEW" ;;
  "pr create")
    if [ -n "$FAKE_GH_CREATE_FAIL" ]; then echo "create failed" >&2; exit 1; fi
    echo "https://github.com/o/r/pull/7" ;;
  "pr edit") echo "https://github.com/o/r/pull/5" ;;
  *) echo "fake gh: unexpected $*" >&2; exit 1 ;;
esac
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    origin: PathBuf,
    bin: PathBuf,
    log: PathBuf,
    run_id: String,
}

impl Fixture {
    /// `repo` with a bare `origin`, a run planned and implemented under `--dry-run` up
    /// to the ship gate, and a fake `gh` in `bin`. `toml` goes in `spar.toml` first so
    /// the run freezes it.
    fn new(toml: &str) -> Self {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let origin = tmp.path().join("origin.git");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare"]);
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("README.md"), "x\n").unwrap();
        std::fs::write(root.join(".gitignore"), ".spar/\n").unwrap();
        if !toml.is_empty() {
            std::fs::write(root.join("spar.toml"), toml).unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&root, &["push", "-q", "origin", "main"]);

        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(&gh, FAKE_GH).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = tmp.path().join("gh.log");

        let out = spar_cmd()
            .current_dir(&root)
            .args([
                "plan",
                "--task",
                "ship flow change",
                "--providers",
                "cli:claude,cli:grok",
                "--dry-run",
                "--json",
            ])
            .assert()
            .code(2);
        let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
        let run_id = v["run_id"].as_str().unwrap().to_string();
        spar_cmd()
            .current_dir(&root)
            .args(["approve", &run_id, "--json"])
            .assert()
            .success();
        spar_cmd()
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
        Fixture {
            _tmp: tmp,
            root,
            origin,
            bin,
            log,
            run_id,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.root
            .join(".spar/runs")
            .join(&self.run_id)
            .join("state.json")
    }

    fn state(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.state_path()).unwrap()).unwrap()
    }

    fn edit_state(&self, f: impl FnOnce(&mut Value)) {
        let mut v = self.state();
        f(&mut v);
        std::fs::write(self.state_path(), serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }

    fn artifact(&self, name: &str) -> String {
        std::fs::read_to_string(
            self.root
                .join(".spar/runs")
                .join(&self.run_id)
                .join("artifacts")
                .join(name),
        )
        .unwrap()
    }

    /// Swap the dry-run's stand-in implementer directory for a real worktree on the
    /// run's branch with one commit, and mark the run as a real one.
    fn real_worktree(&self) -> PathBuf {
        let branch = format!("spar/{}/impl", self.run_id);
        let wt = self._tmp.path().join("impl-wt");
        git(
            &self.root,
            &["worktree", "add", "-q", "-b", &branch, wt.to_str().unwrap()],
        );
        std::fs::write(wt.join("feature.txt"), "work\n").unwrap();
        git(&wt, &["add", "feature.txt"]);
        git(&wt, &["commit", "-q", "-m", "feature"]);
        let path = wt.to_str().unwrap().to_string();
        self.edit_state(|v| {
            v["dry_run"] = Value::Bool(false);
            for w in v["worktrees"].as_array_mut().unwrap() {
                if w["slot_id"] == "impl" {
                    w["path"] = Value::String(path.clone());
                }
            }
        });
        wt
    }

    fn cmd(&self) -> assert_cmd::Command {
        let mut c = spar_cmd();
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.current_dir(&self.root)
            .env("PATH", path)
            .env("FAKE_GH_LOG", &self.log);
        c
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn ship_adopts_an_open_pr_instead_of_failing_at_create() {
    let fx = Fixture::new("");
    fx.real_worktree();
    fx.cmd()
        .env("FAKE_GH_LIST", open_pr_list("implementer wrote this"))
        .args(["ship", &fx.run_id, "--confirm", "--json"])
        .assert()
        .success();

    let state = fx.state();
    assert_eq!(state["phase"], "done");
    assert_eq!(state["pr_url"], "https://github.com/o/r/pull/5");
    let log = fx.gh_log();
    assert!(
        log.contains(&format!(
            "pr list --head spar/{}/impl --state open",
            fx.run_id
        )),
        "{log}"
    );
    assert!(log.contains("pr edit 5 --body-file "), "{log}");
    assert!(!log.contains("pr create"), "{log}");
    let ship_md = fx.artifact("ship.md");
    assert!(ship_md.contains("adopted open PR #5"), "{ship_md}");
    assert!(
        ship_md.contains("body differs from what spar last wrote"),
        "{ship_md}"
    );

    // A second ship of a done run adopts again rather than failing, and a body that is
    // still spar's own is replaced without a warning.
    let ours = fx.artifact("pr-body.md");
    let out = fx
        .cmd()
        .env("FAKE_GH_LIST", open_pr_list(&ours))
        .args(["ship", &fx.run_id, "--json"])
        .assert()
        .success();
    assert_eq!(fx.state()["phase"], "done");
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(!stderr.contains("body differs"), "{stderr}");
}

fn open_pr_list(body: &str) -> String {
    serde_json::json!([{
        "number": 5,
        "url": "https://github.com/o/r/pull/5",
        "state": "OPEN",
        "isDraft": true,
        "baseRefName": "main",
        "isCrossRepository": false,
        "body": body,
    }])
    .to_string()
}

/// A fork's PR on the same branch name, or one targeting another base, is not adopted.
#[test]
fn ship_does_not_adopt_a_fork_or_another_bases_pr() {
    let fx = Fixture::new("");
    fx.real_worktree();
    let list = serde_json::json!([
        {"number": 9, "url": "https://github.com/x/r/pull/9", "state": "OPEN",
         "baseRefName": "main", "isCrossRepository": true},
        {"number": 8, "url": "https://github.com/o/r/pull/8", "state": "OPEN",
         "baseRefName": "release", "isCrossRepository": false},
    ])
    .to_string();
    fx.cmd()
        .env("FAKE_GH_LIST", list)
        .args(["ship", &fx.run_id, "--confirm", "--json"])
        .assert()
        .success();
    let log = fx.gh_log();
    assert!(log.contains("pr create"), "{log}");
    assert!(!log.contains("pr edit"), "{log}");
    assert_eq!(fx.state()["pr_url"], "https://github.com/o/r/pull/7");
}

#[test]
fn ship_creates_a_draft_by_default() {
    let fx = Fixture::new("");
    fx.real_worktree();
    fx.cmd()
        .args(["ship", &fx.run_id, "--confirm", "--json"])
        .assert()
        .success();
    let log = fx.gh_log();
    assert!(log.contains("pr create --draft --head "), "{log}");
    assert_eq!(fx.state()["pr_url"], "https://github.com/o/r/pull/7");
}

#[test]
fn ship_draft_false_creates_a_ready_pr() {
    let fx = Fixture::new("[ship]\ndraft = false\n");
    fx.real_worktree();
    fx.cmd()
        .args(["ship", &fx.run_id, "--confirm", "--json"])
        .assert()
        .success();
    let log = fx.gh_log();
    let create = log
        .lines()
        .find(|l| l.starts_with("pr create"))
        .unwrap_or_else(|| panic!("no pr create in {log}"));
    assert!(!create.contains("--draft"), "{create}");
    assert!(
        create.starts_with(&format!("pr create --head spar/{}/impl", fx.run_id)),
        "{create}"
    );
}

/// #163: the remote branch moved under the run. Plain ship refuses and leaves it alone;
/// `--adopt-remote` fast-forwards the worktree to it and ships.
#[test]
fn adopt_remote_fast_forwards_then_ships() {
    let fx = Fixture::new("");
    let wt = fx.real_worktree();
    let branch = format!("spar/{}/impl", fx.run_id);
    git(
        &wt,
        &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
    );
    let hand = fx._tmp.path().join("hand");
    git(
        fx._tmp.path(),
        &[
            "clone",
            "-q",
            "-b",
            &branch,
            fx.origin.to_str().unwrap(),
            hand.to_str().unwrap(),
        ],
    );
    git(&hand, &["config", "user.email", "h@h.com"]);
    git(&hand, &["config", "user.name", "h"]);
    std::fs::write(hand.join("hand.txt"), "by hand\n").unwrap();
    git(&hand, &["add", "hand.txt"]);
    git(&hand, &["commit", "-q", "-m", "by hand"]);
    git(&hand, &["push", "-q", "origin", &branch]);
    let hand_tip = git(&hand, &["rev-parse", "HEAD"]);

    let out = fx
        .cmd()
        .args(["ship", &fx.run_id, "--confirm", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    let state = fx.state();
    assert!(
        state["error"]
            .as_str()
            .unwrap_or("")
            .contains("--adopt-remote"),
        "{state}\n{stderr}"
    );
    assert_eq!(
        git(&fx.origin, &["rev-parse", &format!("refs/heads/{branch}")]),
        hand_tip
    );

    fx.cmd()
        .args(["ship", &fx.run_id, "--adopt-remote", "--json"])
        .assert()
        .success();
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), hand_tip);
    assert_eq!(
        git(&fx.origin, &["rev-parse", &format!("refs/heads/{branch}")]),
        hand_tip
    );
    assert_eq!(fx.state()["phase"], "done");
    assert_eq!(fx.state()["pr_url"], "https://github.com/o/r/pull/7");
}

const SHIPPED: &str = "1111111111111111111111111111111111111111";

/// A real run whose current round shipped `SHIPPED` to a PR against `main`.
fn shipped_run(fx: &Fixture, phase: &str) {
    fx.edit_state(|v| {
        v["dry_run"] = Value::Bool(false);
        v["phase"] = Value::String(phase.into());
        v["pr_url"] = Value::String("https://github.com/o/r/pull/5".into());
        v["shipped_round"] = v["round"].clone();
        v["shipped_head"] = Value::String(SHIPPED.into());
        v["shipped_base"] = Value::String("main".into());
    });
}

fn pr_view(state: &str) -> String {
    pr_json(state, 5, SHIPPED, false)
}

fn pr_json(state: &str, number: u32, head: &str, fork: bool) -> String {
    let merged = state == "MERGED";
    serde_json::json!({
        "number": number,
        "url": format!("https://github.com/o/r/pull/{number}"),
        "state": state,
        "mergedAt": if merged { Value::String("2026-10-30T12:00:00Z".into()) } else { Value::Null },
        "mergeCommit": if merged { serde_json::json!({"oid": "abc123"}) } else { Value::Null },
        "headRefOid": head,
        "baseRefName": "main",
        "isCrossRepository": fork,
    })
    .to_string()
}

#[test]
fn sync_prs_moves_a_merged_run_to_done() {
    let fx = Fixture::new("");
    shipped_run(&fx, "awaiting_ship_confirm");

    let out = fx
        .cmd()
        .env("FAKE_GH_VIEW", pr_view("MERGED"))
        .args(["sync-prs", "--run", &fx.run_id, "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["runs"][0]["to"], "done", "{v}");

    let state = fx.state();
    assert_eq!(state["phase"], "done");
    assert_eq!(state["merge_commit"], "abc123");
    assert_eq!(state["merged_at"], "2026-10-30T12:00:00Z");
    assert!(fx
        .gh_log()
        .contains("pr view https://github.com/o/r/pull/5 --json"));
    let history = std::fs::read_to_string(fx.root.join(".spar/history.jsonl")).unwrap();
    let last: Value = serde_json::from_str(history.lines().last().unwrap()).unwrap();
    assert_eq!(last["phase"], "done");
    assert_eq!(last["merge_commit"], "abc123");
    assert_eq!(last["merged_at"], "2026-10-30T12:00:00Z");
}

/// A run already `done` still gets a history row when its merge is recorded.
#[test]
fn sync_prs_records_a_merge_of_a_done_run_in_history() {
    let fx = Fixture::new("");
    shipped_run(&fx, "done");
    fx.cmd()
        .env("FAKE_GH_VIEW", pr_view("MERGED"))
        .args(["sync-prs", "--all"])
        .assert()
        .success();
    let history = std::fs::read_to_string(fx.root.join(".spar/history.jsonl")).unwrap();
    let last: Value = serde_json::from_str(history.lines().last().unwrap()).unwrap();
    assert_eq!(last["merge_commit"], "abc123", "{last}");
}

/// The recorded PR's head is an earlier round's commit: it does not settle this round.
#[test]
fn sync_prs_ignores_a_pr_that_does_not_hold_the_shipped_commit() {
    let fx = Fixture::new("");
    shipped_run(&fx, "awaiting_ship_confirm");
    let stale = pr_json(
        "MERGED",
        5,
        "2222222222222222222222222222222222222222",
        false,
    );
    let out = fx
        .cmd()
        .env("FAKE_GH_VIEW", stale)
        .args(["sync-prs", "--run", &fx.run_id, "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(v["runs"][0]["to"].is_null(), "{v}");
    assert_eq!(fx.state()["phase"], "awaiting_ship_confirm");
}

/// Only a run that shipped its current round, at the ship gate or done, is settled.
#[test]
fn sync_prs_never_moves_an_unshipped_or_in_flight_run() {
    let fx = Fixture::new("");
    shipped_run(&fx, "stuck");
    let out = fx
        .cmd()
        .env("FAKE_GH_VIEW", pr_view("MERGED"))
        .args(["sync-prs", "--run", &fx.run_id, "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["runs"][0]["note"], "not done or at the ship gate", "{v}");
    assert_eq!(fx.state()["phase"], "stuck");

    fx.edit_state(|v| {
        v["phase"] = Value::String("awaiting_ship_confirm".into());
        v["round"] = serde_json::json!(v["round"].as_u64().unwrap() + 1);
    });
    let out = fx
        .cmd()
        .env("FAKE_GH_VIEW", pr_view("MERGED"))
        .args(["sync-prs", "--run", &fx.run_id, "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(
        v["runs"][0]["note"], "has not shipped its current round",
        "{v}"
    );
    assert_eq!(fx.state()["phase"], "awaiting_ship_confirm");
    assert!(!fx.gh_log().contains("pr view"), "{}", fx.gh_log());
}

/// A rate-limited `gh` ends the `--all` pass instead of hammering the API per run.
#[test]
fn sync_prs_stops_the_pass_on_a_rate_limit() {
    let fx = Fixture::new("");
    shipped_run(&fx, "done");
    let runs = fx.root.join(".spar/runs");
    let twin = format!("{}b", fx.run_id);
    copy_dir(&runs.join(&fx.run_id), &runs.join(&twin));
    let twin_state = runs.join(&twin).join("state.json");
    let mut v: Value =
        serde_json::from_str(&std::fs::read_to_string(&twin_state).unwrap()).unwrap();
    v["id"] = Value::String(twin.clone());
    std::fs::write(&twin_state, v.to_string()).unwrap();

    fx.cmd()
        .env("FAKE_GH_VIEW_FAIL", "API rate limit exceeded for user")
        .args(["sync-prs", "--all", "--json"])
        .assert()
        .code(1);
    assert_eq!(fx.gh_log().matches("pr view").count(), 1, "{}", fx.gh_log());
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn sync_prs_and_override_are_refused_inside_a_slot() {
    let fx = Fixture::new("");
    for var in ["SPAR_AGENT_ID", "SPAR_RUN_ID"] {
        let out = fx
            .cmd()
            .env(var, "x")
            .args(["sync-prs", "--all"])
            .assert()
            .code(1);
        let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
        assert!(
            stderr.contains("operator-only command; refused inside a slot"),
            "{stderr}"
        );
        let out = fx
            .cmd()
            .env(var, "x")
            .args(["ship", &fx.run_id, "--override-gate", "--reason", "x"])
            .assert()
            .code(1);
        let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
        assert!(
            stderr.contains("operator-only command; refused inside a slot"),
            "{stderr}"
        );
    }
    assert_eq!(fx.state()["phase"], "awaiting_ship_confirm");
}

#[test]
fn sync_prs_moves_a_closed_run_to_stopped() {
    let fx = Fixture::new("");
    shipped_run(&fx, "done");
    fx.cmd()
        .env("FAKE_GH_VIEW", pr_view("CLOSED"))
        .args(["sync-prs", "--all", "--json"])
        .assert()
        .success();
    let state = fx.state();
    assert_eq!(state["phase"], "stopped");
    assert_eq!(state["pr_state"], "CLOSED");
    assert!(state.get("merged_at").is_none());
}

#[test]
fn sync_prs_leaves_an_open_pr_alone() {
    let fx = Fixture::new("");
    shipped_run(&fx, "done");
    let before = std::fs::read_to_string(fx.state_path()).unwrap();
    let out = fx
        .cmd()
        .env("FAKE_GH_VIEW", pr_view("OPEN"))
        .args(["sync-prs", "--run", &fx.run_id, "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(v["runs"][0]["to"].is_null(), "{v}");
    assert_eq!(std::fs::read_to_string(fx.state_path()).unwrap(), before);
}

#[test]
fn sync_prs_dry_run_reports_without_writing() {
    let fx = Fixture::new("");
    shipped_run(&fx, "awaiting_ship_confirm");
    let before = std::fs::read_to_string(fx.state_path()).unwrap();
    let out = fx
        .cmd()
        .env("FAKE_GH_VIEW", pr_view("MERGED"))
        .args(["sync-prs", "--all", "--dry-run", "--json"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["runs"][0]["to"], "done", "{v}");
    assert_eq!(std::fs::read_to_string(fx.state_path()).unwrap(), before);
}

/// A run with no PR recorded is found through its branch, skipping a fork's PR and an
/// earlier round's; a dry run is never synced.
#[test]
fn sync_prs_finds_the_pr_by_branch_and_skips_dry_runs() {
    let fx = Fixture::new("");
    fx.cmd()
        .args(["sync-prs", "--all", "--json"])
        .assert()
        .success();
    assert!(!fx.gh_log().contains("pr "), "{}", fx.gh_log());

    shipped_run(&fx, "awaiting_ship_confirm");
    fx.edit_state(|v| {
        v.as_object_mut().unwrap().remove("pr_url");
    });
    let list = format!(
        "[{},{},{}]",
        pr_json("MERGED", 9, SHIPPED, true),
        pr_json(
            "MERGED",
            6,
            "2222222222222222222222222222222222222222",
            false
        ),
        pr_view("MERGED")
    );
    fx.cmd()
        .env("FAKE_GH_LIST", list)
        .args(["sync-prs", "--all", "--json"])
        .assert()
        .success();
    assert!(
        fx.gh_log().contains(&format!(
            "pr list --head spar/{}/impl --state all",
            fx.run_id
        )),
        "{}",
        fx.gh_log()
    );
    let state = fx.state();
    assert_eq!(state["phase"], "done");
    assert_eq!(state["pr_url"], "https://github.com/o/r/pull/5");
}

#[test]
fn sync_prs_without_gh_is_a_clear_error() {
    let fx = Fixture::new("");
    let bare = fx._tmp.path().join("nogh");
    std::fs::create_dir_all(&bare).unwrap();
    let git_bin = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let git_bin = String::from_utf8_lossy(&git_bin.stdout).trim().to_string();
    std::os::unix::fs::symlink(&git_bin, bare.join("git")).unwrap();
    let out = spar_cmd()
        .current_dir(&fx.root)
        .env("PATH", &bare)
        .args(["sync-prs", "--all"])
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("gh not found on PATH"), "{stderr}");
}

#[test]
fn sync_prs_needs_a_run_or_all() {
    let fx = Fixture::new("");
    fx.cmd().args(["sync-prs"]).assert().code(1);
    fx.cmd()
        .args(["sync-prs", "--run", &fx.run_id, "--all"])
        .assert()
        .code(1);
}

/// `[daemon] sync_prs = true`: the daemon's first tick settles a merged run.
#[test]
fn daemon_tick_syncs_prs_when_enabled() {
    let fx = Fixture::new("[daemon]\nsync_prs = true\ntick_secs = 1\n");
    shipped_run(&fx, "awaiting_ship_confirm");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_spar"));
    daemon
        .current_dir(&fx.root)
        .env("SPAR_HOME", spar_home_dir())
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .env(
            "PATH",
            format!(
                "{}:{}",
                fx.bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("FAKE_GH_LOG", &fx.log)
        .env("FAKE_GH_VIEW", pr_view("MERGED"))
        .args(["daemon", "start", "--foreground"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = daemon.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while fx.state()["phase"] != "done" && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    spar_cmd()
        .current_dir(&fx.root)
        .args(["daemon", "stop"])
        .assert()
        .success();
    let _ = child.wait();
    assert_eq!(fx.state()["phase"], "done");
    assert_eq!(fx.state()["merge_commit"], "abc123");
}

fn stuck_with_findings(fx: &Fixture) {
    fx.edit_state(|v| {
        v["phase"] = Value::String("awaiting_round_extension".into());
        v["review_history"] = serde_json::json!([
            {"round": 3, "slot_id": "review-0-cli-grok", "review": {
                "verdict": "request_changes",
                "reviewed_commit": "0123456789abcdef0123456789abcdef01234567",
                "findings": [
                    {"severity": "major", "file": "src/lib.rs", "line": 42, "text": "unchecked unwrap"},
                    {"severity": "minor", "text": "naming"}
                ]
            }}
        ]);
    });
}

/// #166 under `--dry-run`: a run at the round ceiling ships with every open finding in
/// the PR body and the override recorded in ship.md and as an event.
#[test]
fn override_gate_ships_from_round_extension_with_findings() {
    let fx = Fixture::new("");
    stuck_with_findings(&fx);
    fx.cmd()
        .env("USER", "operator-x")
        .args([
            "ship",
            &fx.run_id,
            "--override-gate",
            "--reason",
            "ceiling hit, remaining findings are follow-ups",
            "--json",
        ])
        .assert()
        .success();
    assert_eq!(fx.state()["phase"], "done");

    let ship = fx.artifact("ship.md");
    assert!(
        ship.contains("## Gate override\n\nBy: operator-x\n"),
        "{ship}"
    );
    assert!(
        ship.contains("Reason: ceiling hit, remaining findings are follow-ups"),
        "{ship}"
    );
    let body = &ship[ship.find("## PR body").unwrap()..];
    let section = body.find("## Shipped over open findings").expect(body);
    assert!(section < body.find("## Implementer summary").unwrap());
    assert!(
        body.contains(
            "- **major** `src/lib.rs:42`: unchecked unwrap (review-0-cli-grok, round 3, Reviewed-Commit `0123456789ab`)"
        ),
        "{body}"
    );
    assert!(
        body.contains("- **minor**: naming (review-0-cli-grok"),
        "{body}"
    );

    let events = std::fs::read_to_string(
        fx.root
            .join(".spar/runs")
            .join(&fx.run_id)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(
        events.contains("ship gate overridden by operator-x from AwaitingRoundExtension"),
        "{events}"
    );
}

#[test]
fn override_gate_ships_from_stuck_failed_and_quota() {
    for phase in ["stuck", "failed", "quota"] {
        let fx = Fixture::new("");
        fx.edit_state(|v| v["phase"] = Value::String(phase.into()));
        fx.cmd()
            .args(["ship", &fx.run_id, "--override-gate", "--reason", "why"])
            .assert()
            .success();
        assert_eq!(fx.state()["phase"], "done", "{phase}");
    }
}

#[test]
fn override_gate_requires_a_reason_and_a_settled_phase() {
    let fx = Fixture::new("");
    stuck_with_findings(&fx);
    let out = fx
        .cmd()
        .args(["ship", &fx.run_id, "--override-gate"])
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("--reason"), "{stderr}");
    fx.cmd()
        .args(["ship", &fx.run_id, "--reason", "x"])
        .assert()
        .code(1);
    assert_eq!(fx.state()["phase"], "awaiting_round_extension");

    // Without the override, the round-extension gate does not ship.
    fx.cmd().args(["ship", &fx.run_id]).assert().code(1);

    fx.edit_state(|v| v["phase"] = Value::String("dispatch".into()));
    fx.cmd()
        .args(["ship", &fx.run_id, "--override-gate", "--reason", "x"])
        .assert()
        .code(1);
    assert_eq!(fx.state()["phase"], "dispatch");
}

/// The orchestrator lock held by another process refuses the override; ship's own lock
/// (taken after the check) never does, which the dry-run override tests above cover.
#[test]
fn override_gate_refuses_a_lock_held_by_another_process() {
    let fx = Fixture::new("");
    stuck_with_findings(&fx);
    let lock = fx
        .root
        .join(".spar/runs")
        .join(&fx.run_id)
        .join("orchestrator.lock");
    let ready = fx._tmp.path().join("locked");
    let mut holder = Command::new("flock")
        .arg("-x")
        .arg(&lock)
        .args(["sh", "-c", "echo $$ > \"$0\"; touch \"$1\"; sleep 30"])
        .arg(&lock)
        .arg(&ready)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = fx
        .cmd()
        .args(["ship", &fx.run_id, "--override-gate", "--reason", "why"])
        .assert()
        .code(1);
    let _ = holder.kill();
    let _ = holder.wait();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(stderr.contains("live orchestrator"), "{stderr}");
    assert_eq!(fx.state()["phase"], "awaiting_round_extension");
}

/// A failed override ship goes back to the phase it came from, so a plain `ship
/// --confirm` cannot finish a stuck run; from the ship gate, the re-ship keeps the
/// override's findings section.
#[test]
fn a_failed_override_restores_its_phase_and_a_reship_keeps_the_findings() {
    let fx = Fixture::new("");
    fx.real_worktree();
    stuck_with_findings(&fx);
    fx.cmd()
        .env("FAKE_GH_CREATE_FAIL", "1")
        .args(["ship", &fx.run_id, "--override-gate", "--reason", "ceiling"])
        .assert()
        .code(1);
    let state = fx.state();
    assert_eq!(state["phase"], "awaiting_round_extension");
    assert_eq!(state["ship_override"]["reason"], "ceiling");
    fx.cmd()
        .args(["ship", &fx.run_id, "--confirm"])
        .assert()
        .code(1);
    assert_eq!(fx.state()["phase"], "awaiting_round_extension");

    fx.edit_state(|v| v["phase"] = Value::String("awaiting_ship_confirm".into()));
    fx.cmd()
        .env("FAKE_GH_CREATE_FAIL", "1")
        .args([
            "ship",
            &fx.run_id,
            "--override-gate",
            "--reason",
            "minors only",
        ])
        .assert()
        .code(1);
    assert_eq!(fx.state()["phase"], "awaiting_ship_confirm");
    fx.cmd()
        .args(["ship", &fx.run_id, "--confirm", "--json"])
        .assert()
        .success();
    assert_eq!(fx.state()["phase"], "done");
    let body = fx.artifact("pr-body.md");
    assert!(body.contains("## Shipped over open findings"), "{body}");
    assert!(body.contains(": minors only\n"), "{body}");
    assert!(body.contains("unchecked unwrap"), "{body}");
}
