//! Operator surgery on a run that already exists: moving a seat to another provider
//! (`implement --reassign`, O136), resuming a failed task-only run (O91) and amending a
//! frozen contract (`spar amend`, O137), and reviewers judging from detached checkouts
//! of their own (O138). Every provider CLI is faked and `PATH` holds only the fake bin
//! and the system dirs, so nothing here can reach a real, billed provider.
use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    proj: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    spar_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let bin = tmp.path().join("bin");
        let home = tmp.path().join("home");
        let spar_home = tmp.path().join("spar-home");
        for d in [&proj, &bin, &home, &spar_home] {
            fs::create_dir_all(d).unwrap();
        }
        git(&proj, &["init", "-q"]);
        git(&proj, &["config", "user.email", "test@example.com"]);
        git(&proj, &["config", "user.name", "Test"]);
        fs::write(proj.join("README.md"), "test\n").unwrap();
        git(&proj, &["add", "."]);
        git(&proj, &["commit", "-q", "-m", "init"]);
        let fx = Self {
            _tmp: tmp,
            proj,
            bin,
            home,
            spar_home,
        };
        for name in ["claude", "grok", "agy", "codex", "pi", "muse"] {
            fx.cli(name, "exit 1");
        }
        fx
    }

    /// A fake CLI that answers `--version` and otherwise counts the dispatch in
    /// `$FAKE_COUNT_DIR/<name>.count` before running `body`.
    fn cli(&self, name: &str, body: &str) {
        let p = self.bin.join(name);
        fs::write(
            &p,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fake; exit 0; fi\n\
                 echo x >> \"$FAKE_COUNT_DIR/{name}.count\"\n{body}\n"
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn count(&self, name: &str) -> usize {
        fs::read_to_string(self.bin.join(format!("{name}.count")))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn spar(&self) -> AssertCommand {
        let mut cmd = AssertCommand::cargo_bin("spar").unwrap();
        cmd.current_dir(&self.proj)
            .env_remove("SPAR_PROJECT_ROOT")
            .env_remove("SPAR_RUN_ID")
            .env_remove("SPAR_AGENT_ID")
            .env_remove("SPAR_DRY_RUN")
            .env("HOME", &self.home)
            .env("SPAR_HOME", &self.spar_home)
            .env("SPAR_AUTH_RETRY_WAIT_SECS", "0")
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("FAKE_PROJECT_ROOT", &self.proj)
            .env("FAKE_COUNT_DIR", self.bin.as_os_str());
        cmd
    }

    fn run_id(&self) -> String {
        fs::read_dir(self.proj.join(".spar/runs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().join("state.json").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .expect("a run")
    }

    fn run_dir(&self) -> PathBuf {
        self.proj.join(".spar/runs").join(self.run_id())
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(self.run_dir().join("state.json")).unwrap())
            .unwrap()
    }

    fn write_state(&self, state: &serde_json::Value) {
        fs::write(
            self.run_dir().join("state.json"),
            serde_json::to_string_pretty(state).unwrap(),
        )
        .unwrap();
    }

    fn slot(&self, id: &str) -> Option<serde_json::Value> {
        self.state()["slots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
    }

    /// A task-only run whose implementer (fake claude) fails: `phase = failed`, one
    /// implementer seat `impl` on claude and one reviewer seat on codex.
    fn failed_task_run(&self) -> String {
        self.cli("claude", "echo 'boom: the model crashed'\nexit 1");
        self.spar()
            .args([
                "implement",
                "-t",
                "add a feature",
                "--providers",
                "cli:claude,cli:codex",
                "--fleet",
                "small",
                "--without",
                "suite",
            ])
            .assert()
            .code(1);
        assert_eq!(self.state()["phase"], "failed");
        self.run_id()
    }
}

#[test]
fn a_failed_task_only_run_is_picked_back_up_by_resume_and_by_implement_run() {
    let fx = Fixture::new();
    let id = fx.failed_task_run();
    assert_eq!(fx.count("claude"), 1);

    fx.spar()
        .args(["resume", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not resumable").not())
        .stderr(predicate::str::contains("not approved").not());
    assert_eq!(
        fx.count("claude"),
        2,
        "resume re-dispatched the implementer"
    );
    assert_eq!(fx.state()["phase"], "failed");

    fx.spar()
        .args(["implement", "--run", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not resumable").not())
        .stderr(predicate::str::contains("not approved").not());
    assert_eq!(
        fx.count("claude"),
        3,
        "implement --run re-dispatched it again"
    );
}

#[test]
fn reassign_moves_seats_in_place_and_keeps_worktree_carry_forward_and_history() {
    let fx = Fixture::new();
    let id = fx.failed_task_run();
    let before = fx.state();
    let reviewer = before["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "reviewer")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(reviewer, "review-0-cli-codex");
    let impl_wt = before["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["slot_id"] == "impl")
        .cloned()
        .expect("impl worktree");
    let round = before["round"].as_u64().unwrap();
    let artifacts = fx.run_dir().join("artifacts");
    fs::write(
        artifacts.join("carry-forward-impl.md"),
        "CARRIED-BRIEF-MARKER\n",
    )
    .unwrap();
    fs::write(
        artifacts.join(format!("review-{reviewer}.md")),
        "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — OLD-REVIEW-MARKER\n",
    )
    .unwrap();
    let markers = fx.run_dir().join("markers");
    fs::create_dir_all(&markers).unwrap();
    fs::write(markers.join("impl.codex.session_id"), "stale-thread").unwrap();

    fx.spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:codex",
            "--reassign",
            "review-0-cli-codex=cli:claude@opus",
            "--dry-run",
        ])
        .assert()
        .stdout(predicate::str::contains(
            "reassigned seat impl (cli:claude) to cli:codex",
        ))
        .stdout(predicate::str::contains("now seat review-0-cli-claude"));

    let state = fx.state();
    let imp = fx.slot("impl").expect("impl keeps its id");
    assert_eq!(imp["provider"], "cli:codex");
    assert_eq!(imp["source"], "reassigned");
    assert!(
        fx.slot("review-0-cli-codex").is_none(),
        "old reviewer id retired"
    );
    let rev = fx.slot("review-0-cli-claude").expect("renamed reviewer");
    assert_eq!(rev["provider"], "cli:claude");
    assert_eq!(rev["model"], "opus");
    assert_eq!(rev["source"], "reassigned");
    let wt = state["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["slot_id"] == "impl")
        .cloned()
        .unwrap();
    assert_eq!(wt, impl_wt, "same worktree and branch");
    assert!(state["round"].as_u64().unwrap() >= round);
    let moves = state["seat_reassignments"].as_array().unwrap();
    assert_eq!(moves.len(), 2);
    assert_eq!(moves[0]["from_seat"], "impl");
    assert_eq!(moves[0]["from_provider"], "cli:claude");
    assert_eq!(moves[0]["to_provider"], "cli:codex");
    assert_eq!(moves[1]["to_seat"], "review-0-cli-claude");
    assert!(
        !markers.join("impl.codex.session_id").exists(),
        "the new provider starts cold"
    );
    let prompt = fs::read_to_string(fx.run_dir().join("prompt-impl.md")).unwrap();
    assert!(
        prompt.contains("CARRIED-BRIEF-MARKER"),
        "carry-forward reached the new implementer"
    );
    assert!(
        prompt.contains("review-0-cli-claude"),
        "the retired reviewer's blockers follow the renamed seat"
    );
    let events = fs::read_to_string(fx.run_dir().join("events.jsonl")).unwrap();
    assert!(events.contains("reassigned by the operator"));
    assert_eq!(fx.count("codex"), 0, "the dry re-entry spawned nothing");
}

#[test]
fn a_reassigned_seat_survives_a_later_reload_config() {
    let fx = Fixture::new();
    let id = fx.failed_task_run();
    fx.spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:codex",
            "--dry-run",
        ])
        .assert()
        .stdout(predicate::str::contains("reassigned seat impl"));
    assert_eq!(fx.slot("impl").unwrap()["provider"], "cli:codex");
    let mut st = fx.state();
    st["phase"] = "failed".into();
    fx.write_state(&st);
    fx.spar()
        .args(["implement", "--run", &id, "--reload-config", "--dry-run"])
        .assert()
        .stderr(predicate::str::contains("Refusing to re-point").not());
    let imp = fx.slot("impl").unwrap();
    assert_eq!(imp["provider"], "cli:codex");
    assert_eq!(imp["source"], "reassigned");
}

#[test]
fn reassign_is_refused_inside_a_slot_onto_a_paused_provider_and_under_a_live_orchestrator() {
    let fx = Fixture::new();
    let id = fx.failed_task_run();

    fx.spar()
        .env("SPAR_AGENT_ID", "impl")
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:codex",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "operator-only command; refused inside a slot",
        ));

    fx.spar()
        .args(["provider", "pause", "cli:grok"])
        .assert()
        .success();
    fx.spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:grok",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cli:grok is paused"));

    fx.spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "reviewer=cli:bogus",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no adapter named bogus"));

    let lock = fx.run_dir().join("orchestrator.lock");
    let mut holder = std::process::Command::new("flock")
        .arg(&lock)
        .args(["sleep", "30"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    fx.spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:codex",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "already has a running orchestrator",
        ));
    holder.kill().unwrap();
    let _ = holder.wait();

    let st = fx.state();
    assert_eq!(fx.slot("impl").unwrap()["provider"], "cli:claude");
    assert!(
        st.get("seat_reassignments").is_none(),
        "nothing moved: {st}"
    );
}

/// A dry-run plan run parked at the plan gate, with a hand-written contract standing in
/// for what a real test author froze.
fn planned_run(fx: &Fixture) -> String {
    fx.spar()
        .args([
            "plan",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert()
        .code(2);
    assert_eq!(fx.state()["phase"], "awaiting_plan_approval");
    fs::write(
        fx.run_dir().join("artifacts/test-contract.md"),
        "## Scenarios\n- [ ] AC-1: BASH-HARNESS journey passes — verify: `./journey.sh`\n",
    )
    .unwrap();
    fx.run_id()
}

#[test]
fn amend_reruns_only_the_test_author_refreezes_and_leaves_the_run_where_it_was() {
    let fx = Fixture::new();
    let id = planned_run(&fx);
    let planner_prompt =
        fs::read_to_string(fx.run_dir().join("prompt-planner-cli-claude.md")).unwrap();

    fx.spar()
        .args([
            "amend",
            "--run",
            &id,
            "--contract",
            "PLAYWRIGHT-DIRECTIVE: verify the journey with Playwright",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing was resumed"))
        .stdout(predicate::str::contains(format!("spar approve {id}")));

    let st = fx.state();
    assert_eq!(st["phase"], "awaiting_plan_approval", "parked where it was");
    let am = &st["contract_amendments"].as_array().unwrap()[0];
    assert!(am["directive"]
        .as_str()
        .unwrap()
        .starts_with("PLAYWRIGHT-DIRECTIVE"));
    assert_ne!(am["old_fingerprint"], am["new_fingerprint"]);
    assert_eq!(st["contract_fingerprint"], am["new_fingerprint"]);
    let archived =
        fs::read_to_string(fx.run_dir().join("artifacts/test-contract-amend1.md")).unwrap();
    assert!(archived.contains("BASH-HARNESS"));
    let author_prompt =
        fs::read_to_string(fx.run_dir().join("prompt-test-author-cli-claude.md")).unwrap();
    assert!(author_prompt.contains("PLAYWRIGHT-DIRECTIVE"));
    assert!(
        author_prompt.contains("BASH-HARNESS"),
        "the author sees the contract it amends"
    );
    assert_eq!(
        fs::read_to_string(fx.run_dir().join("prompt-planner-cli-claude.md")).unwrap(),
        planner_prompt,
        "the planner was not re-run"
    );
    let events = fs::read_to_string(fx.run_dir().join("events.jsonl")).unwrap();
    assert!(events.contains("contract amended by the operator"));

    fx.spar().args(["approve", &id]).assert().success();
    fx.spar()
        .args(["implement", "--run", &id, "--dry-run"])
        .assert()
        .stderr(predicate::str::contains("changed while the previous round").not())
        .stderr(predicate::str::contains("contract re-frozen").not());
    assert_eq!(fx.state()["contract_fingerprint"], am["new_fingerprint"]);

    let markers = fx.run_dir().join("markers");
    fs::write(
        markers.join("impl.claude.session_id"),
        "steered-by-old-contract",
    )
    .unwrap();
    fs::write(
        markers.join("review-0-cli-claude.claude.session_id"),
        "judge",
    )
    .unwrap();
    fx.spar()
        .args(["amend", "--run", &id, "--contract", "drop AC-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "spar implement --run {id}"
        )));
    assert!(
        !markers.join("impl.claude.session_id").exists(),
        "the implementer re-reads the amended contract cold"
    );
    assert!(markers
        .join("review-0-cli-claude.claude.session_id")
        .exists());
    assert_eq!(
        fx.state()["contract_amendments"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn amend_is_refused_inside_a_slot_under_a_live_orchestrator_and_without_a_contract() {
    let fx = Fixture::new();
    let id = planned_run(&fx);
    fx.spar()
        .env("SPAR_RUN_ID", &id)
        .args(["amend", "--run", &id, "--contract", "x"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "operator-only command; refused inside a slot",
        ));

    let lock = fx.run_dir().join("orchestrator.lock");
    let mut holder = std::process::Command::new("flock")
        .arg(&lock)
        .args(["sleep", "30"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    fx.spar()
        .args(["amend", "--run", &id, "--contract", "x"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "already has a running orchestrator",
        ));
    holder.kill().unwrap();
    let _ = holder.wait();
    assert!(fx.state().get("contract_amendments").is_none());

    let other = Fixture::new();
    let task_run = other.failed_task_run();
    other
        .spar()
        .args(["amend", "--run", &task_run, "--contract", "x"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("has no test author"));
}

/// A fake `muse`: the implementer commits `work.txt` and writes its summary; a reviewer
/// records where it ran and what `CARGO_TARGET_DIR` it was given, approves the commit it
/// was shown, then misbehaves the way #158's incidents did by switching to a branch of
/// its own and committing on it.
const MISBEHAVING_REVIEWER_MUSE: &str = r#"PROMPT_FILE=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--prompt-file" ]; then PROMPT_FILE="$a"; fi
  prev="$a"
done
NAME=$(basename "$PROMPT_FILE" .md)
NAME=${NAME#prompt-}
ART="$(dirname "$PROMPT_FILE")/artifacts"
mkdir -p "$ART"
echo '{"schema_version":1,"stream":{"kind":"session","id":"sess-r"},"record_type":"event","payload_type":"run.model.configured","payload":{"kind":"run_model_configured","model_id":"m"}}'
case "$NAME" in
  impl)
    echo work > work.txt
    git add work.txt
    git commit -qm work
    printf '# Summary\nCommitted work.txt.\n' > "$ART/summary-impl.md"
    ;;
  review-*)
    pwd > "$ART/where-$NAME"
    printf '%s' "$CARGO_TARGET_DIR" > "$ART/target-$NAME"
    SHA=$(git rev-parse HEAD)
    printf '## Verdict\napprove\n\nReviewed-Commit: %s\n' "$SHA" > "$ART/review-$NAME.md"
    git checkout -q -b review-evil
    echo evil > evil.txt
    git add evil.txt
    git commit -qm evil
    ;;
esac
exit 0"#;

#[test]
fn reviewers_judge_from_detached_checkouts_and_never_move_the_implementer_branch() {
    let fx = Fixture::new();
    fx.cli("muse", MISBEHAVING_REVIEWER_MUSE);
    fx.spar()
        .args([
            "implement",
            "-t",
            "add work",
            "--providers",
            "cli:muse",
            "--fleet",
            "small",
            "--without",
            "suite",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(" was on ").not());
    let id = fx.run_id();
    let st = fx.state();
    assert_eq!(st["phase"], "awaiting_ship_confirm", "{st}");
    let impl_wt = st["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["slot_id"] == "impl")
        .unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let impl_wt = PathBuf::from(impl_wt);
    assert_eq!(
        git(&impl_wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        format!("spar/{id}/impl"),
        "the implementer's worktree stays on its branch"
    );
    let impl_head = git(&impl_wt, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(&fx.proj, &["log", "-1", "--format=%s", &impl_head]),
        "work"
    );

    let area = fx
        .proj
        .parent()
        .unwrap()
        .join(format!("proj-spar-{id}-reviewers"));
    let seat = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "reviewer")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let artifacts = fx.run_dir().join("artifacts");
    let where_ran = fs::read_to_string(artifacts.join(format!("where-{seat}"))).unwrap();
    assert_eq!(
        PathBuf::from(where_ran.trim()).canonicalize().unwrap(),
        area.join(&seat).canonicalize().unwrap(),
        "the reviewer ran in its own checkout"
    );
    assert_eq!(
        fs::read_to_string(artifacts.join(format!("target-{seat}"))).unwrap(),
        area.join("target").display().to_string(),
        "one shared build dir per run"
    );
    let review = fs::read_to_string(artifacts.join(format!("review-{seat}.md"))).unwrap();
    assert!(
        review.contains(&impl_head),
        "the checkout was at the panel head"
    );
    assert!(
        git(&fx.proj, &["worktree", "list"]).contains("-reviewers"),
        "the checkout is a registered worktree"
    );

    fx.spar().args(["cleanup", &id]).output().unwrap();
    assert!(!area.exists(), "cleanup removes the reviewer checkouts");
    assert!(!git(&fx.proj, &["worktree", "list"]).contains("-reviewers"));
}
