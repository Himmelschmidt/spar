//! Run-level provider health: auth faults (O104), reviewer infrastructure failures
//! (O103), sticky manual pauses and `provider probe` (O105), and per-dispatch pause
//! re-checks (O106). Fake CLIs on `PATH` stand in for providers where a failure has to
//! happen; `--dry-run` covers what needs no spawn at all.
use assert_cmd::Command as AssertCommand;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// The real claude wording from the 2026-10-24 incident (#174).
const CLAUDE_REVOKED: &str = r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"OAuth access token has been revoked. Please obtain a new token or refresh your existing token."}}"#;

fn spar_cmd(home: &Path) -> AssertCommand {
    let mut cmd = AssertCommand::cargo_bin("spar").unwrap();
    cmd.env_remove("SPAR_PROJECT_ROOT");
    cmd.env_remove("SPAR_RUN_ID");
    cmd.env_remove("SPAR_AGENT_ID");
    cmd.env_remove("SPAR_DRY_RUN");
    cmd.env("SPAR_HOME", home);
    cmd.env("SPAR_AUTH_RETRY_WAIT_SECS", "0");
    cmd
}

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
        Self {
            _tmp: tmp,
            proj,
            bin,
            home,
        }
    }

    /// A fake CLI that answers `--version` and runs `body` for anything else. The body
    /// sees `$FAKE_PROJECT_ROOT` and `$FAKE_COUNT_DIR`.
    fn cli(&self, name: &str, body: &str) {
        let p = self.bin.join(name);
        fs::write(
            &p,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fake; exit 0; fi\n{body}\n"
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
        let mut cmd = spar_cmd(&self.home);
        cmd.current_dir(&self.proj)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("FAKE_PROJECT_ROOT", &self.proj)
            .env("FAKE_COUNT_DIR", self.bin.as_os_str());
        cmd
    }

    fn count(&self, name: &str) -> usize {
        fs::read_to_string(self.bin.join(format!("{name}.count")))
            .map(|s| s.lines().count())
            .unwrap_or(0)
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

    fn slot(&self, role: &str) -> serde_json::Value {
        self.state()["slots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["role"] == role)
            .cloned()
            .unwrap_or_else(|| panic!("no {role} slot"))
    }
}

/// Appends a line to `$FAKE_COUNT_DIR/<name>.count` on every model dispatch, so a test
/// can tell one dispatch from a retry. `-p`/`exec` are claude's and codex's headless
/// verbs; anything else (a readiness probe) is not counted.
fn counted(name: &str, body: &str) -> String {
    format!(
        "case \"$1\" in -p|exec) echo x >> \"$FAKE_COUNT_DIR/{name}.count\";; *) exit 0;; esac\n{body}"
    )
}

#[test]
fn an_implementer_auth_rejection_retries_once_then_parks_on_quota_with_reason_auth() {
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &counted("claude", &format!("echo '{CLAUDE_REVOKED}'\nexit 1")),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.count("claude"), 2, "one dispatch, one retry");
    let state = fx.state();
    assert_eq!(state["phase"], "quota");
    let err = state["error"].as_str().unwrap();
    assert!(err.contains("auth"), "the park names the reason: {err}");
    assert_eq!(fx.slot("implementer")["auth_fault"], true);
    assert!(
        fx.quota()["providers"].get("cli:claude").is_none(),
        "auth never pauses the provider: {}",
        fx.quota()
    );
}

#[test]
fn an_implementer_auth_rejection_moves_to_the_declared_backup() {
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &counted("claude", &format!("echo '{CLAUDE_REVOKED}'\nexit 1")),
    );
    fx.cli(
        "codex",
        &counted("codex", "echo 'Not logged in' >&2\nexit 1"),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude",
            "--backup",
            "implementer=cli:codex",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    let slot = fx.slot("implementer");
    assert_eq!(slot["provider"], "cli:codex", "{slot}");
    assert_eq!(slot["source"], "backup");
    assert_eq!(fx.count("codex"), 2, "the backup gets its own retry");
    assert!(fx.state()["error"].as_str().unwrap().contains("auth"));
}
