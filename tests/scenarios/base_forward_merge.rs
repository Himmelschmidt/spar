//! Base forward merge (#229): a run whose base moved merges it forward at the
//! next round start and before ship pushes, instead of landing behind.
//!
//! The harness pattern is half `ship_flow` (a temp repo against a bare origin,
//! a fake `gh` on PATH) and half `round_loop` (dry-run implement with the
//! review panel forced to request changes). Round-start merges need a real
//! worktree, so these tests swap one in like `ship_flow` does but keep the run
//! dry-run (synthetic dispatch) and opt the merge back in with
//! `SPAR_MERGE_BASE_DRY_RUN=1`: a plain `--dry-run` preview never touches
//! branches.

mod common;
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
    let mut c = common::spar();
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

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

const FAKE_GH: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_GH_LOG"
case "$1 $2" in
  "--version "*) echo "gh version 0.0.0-fake" ;;
  "pr list") printf '%s' "${FAKE_GH_LIST:-[]}" ;;
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
    /// Temp repo with a bare origin and `main` pushed, `spar.toml` written when
    /// `toml` is non-empty, then a dry-run plan off `main` (or off a detached
    /// HEAD when `detached`) and an approve.
    fn new(toml: &str, detached: bool) -> Self {
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
        if detached {
            git(&root, &["checkout", "-q", "--detach", "HEAD"]);
        }

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
                "base forward merge change",
                "--providers",
                "cli:claude,cli:grok",
                "--dry-run",
                "--json",
            ])
            .assert()
            .code(2);
        let v: Value = serde_json::from_slice(out.get_output().stdout.as_slice()).unwrap();
        let run_id = v["run_id"].as_str().unwrap().to_string();
        spar_cmd()
            .current_dir(&root)
            .args(["approve", &run_id, "--json"])
            .assert()
            .success();
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

    fn events_text(&self) -> String {
        std::fs::read_to_string(
            self.root
                .join(".spar/runs")
                .join(&self.run_id)
                .join("events.jsonl"),
        )
        .unwrap_or_default()
    }

    fn implementer_slot_id(&self) -> String {
        self.state()["slots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["role"] == "implementer")
            .and_then(|s| s["id"].as_str())
            .expect("implementer slot")
            .to_string()
    }

    fn prompt(&self, slot_id: &str) -> String {
        std::fs::read_to_string(
            self.root
                .join(".spar/runs")
                .join(&self.run_id)
                .join(format!("prompt-{slot_id}.md")),
        )
        .unwrap()
    }

    /// Pre-create the exact dir dry-run adopts as the implementer cwd
    /// (`.spar/runs/<id>/cwd-impl`) as a real repo: cloned from the origin,
    /// on the run's branch with `seed` committed. `prepare_isolation` records
    /// the dir as-is, so round two's merge runs against a real remote-tracking
    /// branch. The run itself stays dry-run (synthetic dispatch).
    fn adoptable_repo(&self, seed: &[(&str, &str)]) -> PathBuf {
        let dir = self
            .root
            .join(".spar/runs")
            .join(&self.run_id)
            .join("cwd-impl");
        let branch = format!("spar/{}/impl", self.run_id);
        let origin = self._tmp.path().join("origin.git");
        git(
            &self.root,
            &[
                "clone",
                "-q",
                "-b",
                "main",
                origin.to_str().unwrap(),
                dir.to_str().unwrap(),
            ],
        );
        git(&dir, &["config", "user.email", "t@t.com"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["checkout", "-q", "-b", &branch]);
        for (file, content) in seed {
            std::fs::write(dir.join(file), content).unwrap();
            git(&dir, &["add", file]);
        }
        git(&dir, &["commit", "-q", "-m", "run work"]);
        dir
    }

    /// Commit `file` with `content` on main and push; returns the new tip.
    fn advance_main(&self, file: &str, content: &str) -> String {
        std::fs::write(self.root.join(file), content).unwrap();
        git(&self.root, &["add", file]);
        git(&self.root, &["commit", "-q", "-m", "base moves"]);
        git(&self.root, &["push", "-q", "origin", "main"]);
        git(&self.root, &["rev-parse", "HEAD"])
    }

    /// Dry-run implement with the review panel forced to request changes, so the
    /// loop runs to the `[rounds] max` ceiling from `spar.toml`. The round-start
    /// merge is opted back in; it only fires when the implementer cwd is a
    /// real repo, which `adoptable_repo` arranges.
    fn implement_rounds(&self) -> assert_cmd::assert::Assert {
        spar_cmd()
            .current_dir(&self.root)
            .env("SPAR_FORCE_REQUEST_CHANGES", "1")
            .env("SPAR_MERGE_BASE_DRY_RUN", "1")
            .args([
                "implement",
                "--run",
                &self.run_id,
                "--providers",
                "cli:claude,cli:grok",
                "--dry-run",
                "--json",
            ])
            .assert()
    }

    /// Swap a real worktree in and mark the run real, for ship: ship does no
    /// dispatch, so this side needs no dry-run opt-out.
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

    fn ship(&self, extra_env: &[(&str, &str)]) -> assert_cmd::assert::Assert {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut c = spar_cmd();
        c.current_dir(&self.root)
            .env("PATH", path)
            .env("FAKE_GH_LOG", &self.log)
            .env("FAKE_GH_LIST", "[]");
        for (k, v) in extra_env {
            c.env(k, v);
        }
        c.args(["ship", &self.run_id, "--confirm", "--json"])
            .assert()
    }
}

#[test]
fn round_two_merges_a_moved_base_and_records_it() {
    let fx = Fixture::new("[rounds]\nmax = 2\n", false);
    let wt = fx.adoptable_repo(&[("feature.txt", "run work\n")]);
    let tip = fx.advance_main("base-feature.txt", "base work\n");

    fx.implement_rounds().code(2);

    let st = fx.state();
    assert_eq!(st["merged_base"].as_str(), Some(tip.as_str()), "{st}");
    assert_eq!(
        std::fs::read_to_string(wt.join("base-feature.txt")).unwrap(),
        "base work\n"
    );
    assert!(
        fx.events_text().contains("merged base main forward"),
        "{}",
        fx.events_text()
    );
}

#[test]
fn round_two_conflict_hands_a_section_to_the_implementer() {
    let fx = Fixture::new("[rounds]\nmax = 2\n", false);
    let wt = fx.adoptable_repo(&[("conflict.txt", "run side\n")]);
    fx.advance_main("conflict.txt", "base side\n");

    fx.implement_rounds().code(1);

    let slot = fx.implementer_slot_id();
    let prompt = fx.prompt(&slot);
    assert!(
        prompt.contains("Base merge conflict"),
        "the round's first job has to head the prompt:\n{prompt}"
    );
    assert!(prompt.contains("conflict.txt"), "{prompt}");
    assert!(
        git_ok(&wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]),
        "the merge stays in progress for the implementer"
    );
    let st = fx.state();
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| e.contains("did not commit a resolution")),
        "the round fails at review instead of judging a mid-merge tree: {st}"
    );
    assert!(
        fx.events_text().contains("handed to the round"),
        "{}",
        fx.events_text()
    );
}

#[test]
fn ship_conflict_parks_the_run_with_the_remedy() {
    let fx = Fixture::new("", false);
    spar_cmd()
        .current_dir(&fx.root)
        .args([
            "implement",
            "--run",
            &fx.run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let wt = fx.real_worktree();
    fx.advance_main("feature.txt", "base side\n");
    let before = git(&wt, &["rev-parse", "HEAD"]);

    fx.ship(&[]).code(2);

    let st = fx.state();
    assert!(
        st["error"]
            .as_str()
            .is_some_and(|e| e.contains("conflicts with the run branch")
                && e.contains(&format!("spar implement --run {}", fx.run_id))),
        "the park has to name the conflict and the way out: {st}"
    );
    assert!(
        !git_ok(&wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]),
        "the conflicted merge is aborted, not left in progress"
    );
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), before);
    assert!(
        fx.events_text().contains("ship parked"),
        "{}",
        fx.events_text()
    );
}

#[test]
fn ship_merges_a_moved_base_before_pushing() {
    let fx = Fixture::new("", false);
    spar_cmd()
        .current_dir(&fx.root)
        .args([
            "implement",
            "--run",
            &fx.run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let wt = fx.real_worktree();
    let tip = fx.advance_main("base-feature.txt", "base work\n");

    fx.ship(&[]).success();

    assert_eq!(fx.state()["merged_base"].as_str(), Some(tip.as_str()));
    assert_eq!(
        std::fs::read_to_string(wt.join("base-feature.txt")).unwrap(),
        "base work\n"
    );
}

#[test]
fn sha_base_skips_the_forward_merge() {
    let fx = Fixture::new("[rounds]\nmax = 2\n", true);
    let base = fx.state()["base_ref"].as_str().unwrap().to_string();
    assert_eq!(
        base,
        git(&fx.root, &["rev-parse", "HEAD"]),
        "detached plans base on the sha"
    );
    fx.adoptable_repo(&[("feature.txt", "run work\n")]);

    fx.implement_rounds().code(2);

    assert!(
        fx.events_text().contains("detached sha"),
        "{}",
        fx.events_text()
    );
    let st = fx.state();
    assert_eq!(
        st["merged_base"].as_str(),
        st["base_commit"].as_str(),
        "nothing merged, so the diff base is still the creation cut: {st}"
    );
}
