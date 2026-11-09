//! Per-bucket quota pauses (O153): one seat's rate limit pauses only that seat's
//! bucket. A run keeps running its unaffected seats and parks on quota only when
//! the seat that must run next has no usable provider.

mod common;
use assert_cmd::Command as AssertCommand;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const ANTHROPIC_MODEL: &str = "direct:anthropic/claude-sonnet-4-5";
const OPENAI_MODEL: &str = "direct:openai/gpt-5-nano";

fn init_repo(dir: &Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    fs::write(dir.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    proj: PathBuf,
    bin: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let bin = tmp.path().join("bin");
        let home = tmp.path().join("spar-home");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&home).unwrap();
        init_repo(&proj);
        let fx = Self {
            _tmp: tmp,
            proj,
            bin,
            home,
        };
        for name in ["claude", "grok", "agy", "codex", "pi", "muse"] {
            fx.cli(name, "exit 1");
        }
        fx.cli("claude", FAKE_CLAUDE_IMPLEMENTER);
        fx.cli("pi", FAKE_PI_REVIEWER);
        fx
    }

    fn cli(&self, name: &str, body: &str) {
        let p = self.bin.join(name);
        fs::write(
            &p,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fake-{name} 0; exit 0; fi\n{body}\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn spar(&self) -> AssertCommand {
        let mut cmd = common::spar();
        cmd.env_remove("SPAR_PROJECT_ROOT");
        cmd.env_remove("SPAR_RUN_ID");
        cmd.env_remove("SPAR_AGENT_ID");
        cmd.env_remove("SPAR_DRY_RUN");
        cmd.env("SPAR_HOME", &self.home);
        cmd.env("SPAR_AUTH_RETRY_WAIT_SECS", "0");
        cmd.current_dir(&self.proj)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("FAKE_PROJECT_ROOT", &self.proj);
        cmd
    }

    fn pause(&self, seat: &str) {
        self.spar()
            .args(["provider", "pause", seat])
            .assert()
            .success();
    }

    fn run_id(&self) -> String {
        fs::read_dir(self.proj.join(".spar/runs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().join("state.json").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .expect("a run")
    }

    fn state(&self) -> serde_json::Value {
        let p = self
            .proj
            .join(".spar/runs")
            .join(self.run_id())
            .join("state.json");
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    fn quota(&self) -> serde_json::Value {
        fs::read_to_string(self.proj.join(".spar/quota.json"))
            .map(|s| serde_json::from_str(&s).unwrap())
            .unwrap_or(serde_json::json!({"providers": {}}))
    }

    fn events(&self) -> String {
        let p = self
            .proj
            .join(".spar/runs")
            .join(self.run_id())
            .join("events.jsonl");
        fs::read_to_string(p).unwrap_or_default()
    }

    fn reviewer_slot(&self, model: &str) -> serde_json::Value {
        self.state()["slots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["role"] == "reviewer" && s["model"] == model)
            .cloned()
            .unwrap_or_else(|| panic!("no reviewer slot for model {model}"))
    }

    fn review_artifact(&self, slot_id: &str) -> PathBuf {
        self.proj
            .join(".spar/runs")
            .join(self.run_id())
            .join("artifacts")
            .join(format!("review-{slot_id}.md"))
    }

    fn implement(&self) -> assert_cmd::assert::Assert {
        self.spar()
            .args([
                "implement",
                "-t",
                "add a feature",
                "--role",
                "implementer=cli:claude",
                "--role",
                &format!("reviewer=cli:pi@{ANTHROPIC_MODEL}"),
                "--role",
                &format!("reviewer=cli:pi@{OPENAI_MODEL}"),
                "--without",
                "suite",
            ])
            .assert()
    }
}

/// The implementer prompt asks for a summary file; write it and exit clean.
const FAKE_CLAUDE_IMPLEMENTER: &str = r#"set -- "$@" "$(cat)"
DEST=""
for a in "$@"; do
  case "$a" in
    *"Write a summary to"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write a summary to `\([^`]*\)`.*/\1/p' | head -n 1)
      ;;
  esac
done
case "$DEST" in
  "") exit 0 ;;
  *) printf '# Summary\nDid the stub thing.\n' > "$DEST"; exit 0 ;;
esac"#;

/// Honest approve for whichever reviewer slot dispatches, anchored on the
/// checkout HEAD the prompt names so the commit gate counts it.
const FAKE_PI_REVIEWER: &str = r#"set -- "$@" "$(cat)"
DEST=""
CWD=""
for a in "$@"; do
  case "$a" in
    *"Write review to:"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write review to: \([^ ]*\).*/\1/p' | head -n 1)
      ;;
    *"Code under review (worktree):"*)
      CWD=$(printf '%s' "$a" | sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' | head -n 1)
      ;;
  esac
done
case "$DEST" in
  "") exit 0 ;;
  *)
    SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null || true)
    { printf '## Verdict\napprove\n'; [ -n "$SHA" ] && printf 'Reviewed-Commit: %s\n' "$SHA"; } > "$DEST"
    exit 0
    ;;
esac"#;

#[test]
fn a_paused_bucket_stands_down_while_the_sibling_bucket_reviews() {
    let fx = Fixture::new();
    fx.pause(&format!("cli:pi@{ANTHROPIC_MODEL}"));
    assert_eq!(
        fx.quota()["providers"]["cli:pi@anthropic"]["status"],
        "paused_manual",
        "the anthropic bucket is paused: {}",
        fx.quota()
    );

    fx.implement().code(2);
    let state = fx.state();
    assert_eq!(state["phase"], "awaiting_ship_confirm", "{state}");

    let stood_down = fx.reviewer_slot(ANTHROPIC_MODEL);
    let stood_id = stood_down["id"].as_str().unwrap();
    assert!(
        !fx.review_artifact(stood_id).is_file(),
        "the paused seat never dispatches: {}",
        stood_down
    );

    let healthy = fx.reviewer_slot(OPENAI_MODEL);
    let healthy_id = healthy["id"].as_str().unwrap();
    let text = fs::read_to_string(fx.review_artifact(healthy_id)).unwrap();
    assert!(text.contains("approve"), "the sibling seat reviews: {text}");

    assert!(
        fx.events().contains("stood down"),
        "the stand-down is said out loud"
    );
    assert_eq!(
        fx.quota()["providers"]["cli:pi@anthropic"]["status"],
        "paused_manual",
        "the pause survives the run: {}",
        fx.quota()
    );
    assert!(
        fx.quota()["providers"].get("cli:pi@openai").is_none(),
        "the sibling bucket is untouched: {}",
        fx.quota()
    );
}

#[test]
fn paused_buckets_on_every_reviewer_park_the_run() {
    let fx = Fixture::new();
    fx.pause(&format!("cli:pi@{ANTHROPIC_MODEL}"));
    fx.pause(&format!("cli:pi@{OPENAI_MODEL}"));

    fx.implement().code(4);
    let state = fx.state();
    assert_eq!(state["phase"], "quota", "{state}");
    assert!(
        state["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no eligible"),
        "the park names the empty panel: {}",
        state["error"]
    );
    for model in [ANTHROPIC_MODEL, OPENAI_MODEL] {
        let slot = fx.reviewer_slot(model);
        let id = slot["id"].as_str().unwrap();
        assert!(
            !fx.review_artifact(id).is_file(),
            "no paused seat dispatches: {slot}"
        );
    }
}
