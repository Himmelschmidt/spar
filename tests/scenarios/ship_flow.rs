//! Ship against a fake `gh` on PATH and a bare-repo origin: adopting an existing PR
//! (#164), `[ship] draft` and `sync-prs`.
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
  "pr view") printf '%s' "$FAKE_GH_VIEW" ;;
  "pr create") echo "https://github.com/o/r/pull/7" ;;
  "pr edit") echo "https://github.com/o/r/pull/5" ;;
  *) echo "fake gh: unexpected $*" >&2; exit 1 ;;
esac
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
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
        .env(
            "FAKE_GH_LIST",
            r#"[{"number":5,"url":"https://github.com/o/r/pull/5","state":"OPEN","isDraft":true}]"#,
        )
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
    assert!(fx.artifact("ship.md").contains("adopted open PR #5"));

    // A second ship of a done run adopts again rather than failing.
    fx.cmd()
        .env(
            "FAKE_GH_LIST",
            r#"[{"number":5,"url":"https://github.com/o/r/pull/5","state":"OPEN","isDraft":false}]"#,
        )
        .args(["ship", &fx.run_id, "--json"])
        .assert()
        .success();
    assert_eq!(fx.state()["phase"], "done");
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

fn shipped_run(fx: &Fixture, phase: &str) {
    fx.edit_state(|v| {
        v["dry_run"] = Value::Bool(false);
        v["phase"] = Value::String(phase.into());
        v["pr_url"] = Value::String("https://github.com/o/r/pull/5".into());
    });
}

fn pr_view(state: &str) -> String {
    let merged = state == "MERGED";
    serde_json::json!({
        "number": 5,
        "url": "https://github.com/o/r/pull/5",
        "state": state,
        "mergedAt": if merged { Value::String("2026-10-30T12:00:00Z".into()) } else { Value::Null },
        "mergeCommit": if merged { serde_json::json!({"oid": "abc123"}) } else { Value::Null },
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

/// A run with no PR recorded is found through its branch; a dry run is never synced.
#[test]
fn sync_prs_finds_the_pr_by_branch_and_skips_dry_runs() {
    let fx = Fixture::new("");
    fx.cmd()
        .args(["sync-prs", "--all", "--json"])
        .assert()
        .success();
    assert!(!fx.gh_log().contains("pr "), "{}", fx.gh_log());

    fx.edit_state(|v| {
        v["dry_run"] = Value::Bool(false);
        v["ship_commands"] = serde_json::json!(["git push"]);
    });
    let list = format!("[{}]", pr_view("MERGED"));
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
