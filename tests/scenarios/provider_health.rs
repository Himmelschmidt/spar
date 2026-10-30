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

fn list_entry(fx: &Fixture, name: &str) -> serde_json::Value {
    let out = fx
        .spar()
        .args(["provider", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list: serde_json::Value = serde_json::from_slice(&out).unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} not listed: {list}"))
}

#[test]
fn provider_list_shows_the_age_and_until_of_every_pause() {
    let fx = Fixture::new();
    fx.spar()
        .args(["provider", "pause", "cli:grok"])
        .assert()
        .success();
    fx.spar()
        .args([
            "provider",
            "pause",
            "codex",
            "--until",
            "2027-01-01T00:00:00Z",
        ])
        .assert()
        .success();
    fx.spar()
        .args(["provider", "pause", "api:openai"])
        .assert()
        .success();

    let grok = list_entry(&fx, "grok");
    assert_eq!(grok["quota_status"], "paused_manual");
    assert!(grok["paused_since"].is_string(), "{grok}");
    assert!(grok["pause_age_secs"].as_i64().unwrap() >= 0);
    assert!(grok["paused_until"].is_null());

    let codex = list_entry(&fx, "codex");
    assert_eq!(codex["quota_status"], "paused_manual");
    assert!(codex["paused_until"]
        .as_str()
        .unwrap()
        .starts_with("2027-01-01"));

    let api = list_entry(&fx, "api:openai");
    assert_eq!(api["quota_status"], "paused_manual");

    let human = fx
        .spar()
        .args(["provider", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("until `spar provider resume`"), "{human}");
    assert!(human.contains("until 2027-01-01T00:00:00+00:00"), "{human}");
    assert!(human.contains("api:openai"), "{human}");
}

/// The fake records every invocation, `--version` included, so an empty count proves
/// the dry probe spawned nothing at all.
#[test]
fn a_dry_run_probe_spawns_nothing_and_writes_nothing() {
    let fx = Fixture::new();
    let p = fx.bin.join("claude");
    fs::write(
        &p,
        "#!/bin/sh\necho x >> \"$FAKE_COUNT_DIR/claude.count\"\necho OK\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fx.spar()
        .args(["provider", "pause", "cli:claude"])
        .assert()
        .success();
    let before = fs::read_to_string(fx.proj.join(".spar/quota.json")).unwrap();
    for args in [
        vec!["provider", "probe", "cli:claude", "--dry-run", "--json"],
        vec!["provider", "probe", "--all", "--dry-run", "--json"],
    ] {
        let out = fx
            .spar()
            .args(&args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let reports: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let claude = reports
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["provider"] == "cli:claude")
            .unwrap();
        assert_eq!(claude["verdict"], "skipped");
        assert!(claude["detail"]
            .as_str()
            .unwrap()
            .starts_with("dry-run: would run"));
    }
    fx.spar()
        .env("SPAR_DRY_RUN", "1")
        .args(["provider", "probe", "cli:claude"])
        .assert()
        .success();
    assert_eq!(fx.count("claude"), 0, "dry-run must not spawn the provider");
    assert_eq!(
        fs::read_to_string(fx.proj.join(".spar/quota.json")).unwrap(),
        before
    );
}

fn seed_quota_pause(fx: &Fixture, key: &str, days_ago: i64) {
    let since = chrono::Utc::now() - chrono::Duration::days(days_ago);
    fs::create_dir_all(fx.proj.join(".spar")).unwrap();
    fs::write(
        fx.proj.join(".spar/quota.json"),
        serde_json::json!({"providers": {key: {
            "name": key,
            "status": "paused_quota",
            "hint": "possible quota signal: quota",
            "updated_at": since.to_rfc3339(),
            "cooldown_until": (chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339(),
        }}})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn a_probe_that_gets_an_answer_clears_a_stale_quota_pause() {
    let fx = Fixture::new();
    fx.cli("codex", &counted("codex", "echo OK\nexit 0"));
    seed_quota_pause(&fx, "cli:codex", 13);
    let out = fx
        .spar()
        .args(["provider", "probe", "cli:codex", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r[0]["verdict"], "healthy", "{r}");
    assert_eq!(r[0]["store"], "cleared quota pause");
    assert_eq!(fx.count("codex"), 1, "one call");
    assert_eq!(list_entry(&fx, "codex")["quota_status"], "available");
}

#[test]
fn a_probe_that_hits_the_limit_pauses_and_one_that_is_refused_reports_auth() {
    let fx = Fixture::new();
    fx.cli(
        "codex",
        &counted(
            "codex",
            "echo \"You've hit your usage limit. Try again later.\" >&2\nexit 1",
        ),
    );
    fx.cli(
        "claude",
        &counted("claude", &format!("echo '{CLAUDE_REVOKED}'\nexit 1")),
    );
    let out = fx
        .spar()
        .args(["provider", "probe", "cli:codex", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r[0]["verdict"], "quota", "{r}");
    assert_eq!(list_entry(&fx, "codex")["quota_status"], "paused_quota");

    let out = fx
        .spar()
        .args(["provider", "probe", "cli:claude", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r[0]["verdict"], "auth", "{r}");
    assert_eq!(r[0]["store"], "unchanged");
    assert_eq!(list_entry(&fx, "claude")["quota_status"], "available");
}

#[test]
fn a_probe_never_clears_a_manual_pause() {
    let fx = Fixture::new();
    fx.cli("codex", &counted("codex", "echo OK\nexit 0"));
    fx.spar()
        .args(["provider", "pause", "cli:codex"])
        .assert()
        .success();
    let out = fx
        .spar()
        .args(["provider", "probe", "cli:codex", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r[0]["verdict"], "healthy");
    assert_eq!(r[0]["store"], "manual pause kept");
    assert_eq!(list_entry(&fx, "codex")["quota_status"], "paused_manual");
}

/// #153's second half: the Sonnet backup sat `paused_quota` from 09-20 while 13 runs
/// died on a muse 429. A quota-paused backup is probed before it is skipped.
#[test]
fn a_quota_paused_backup_is_probed_and_used_when_it_answers() {
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &counted("claude", &format!("echo '{CLAUDE_REVOKED}'\nexit 1")),
    );
    fx.cli("codex", &counted("codex", "echo OK\nexit 0"));
    seed_quota_pause(&fx, "cli:codex", 13);
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
        .code(1);
    let slot = fx.slot("implementer");
    assert_eq!(slot["provider"], "cli:codex", "{slot}");
    assert_eq!(slot["source"], "backup");
    assert_eq!(fx.count("codex"), 2, "one probe, one dispatch");
    assert!(
        fx.quota()["providers"].get("cli:codex").is_none()
            || fx.quota()["providers"]["cli:codex"]["status"] == "available",
        "the probe cleared the stale pause: {}",
        fx.quota()
    );
}

#[test]
fn a_manually_paused_backup_is_never_probed() {
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &counted("claude", &format!("echo '{CLAUDE_REVOKED}'\nexit 1")),
    );
    fx.cli("codex", &counted("codex", "echo OK\nexit 0"));
    fx.spar()
        .args(["provider", "pause", "cli:codex"])
        .assert()
        .success();
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
    assert_eq!(fx.count("codex"), 0);
    assert_eq!(fx.slot("implementer")["provider"], "cli:claude");
}

/// Writes this slot's expected artifact the way a real agent would: the implementer a
/// summary, a reviewer an approving verdict naming the commit it judged.
const WRITE_ARTIFACT: &str = r#"run="${SPAR_AGENT_ID%%:*}"; slot="${SPAR_AGENT_ID#*:}"
art="$FAKE_PROJECT_ROOT/.spar/runs/$run/artifacts"
mkdir -p "$art"
case "$slot" in
  review-*) printf '## Verdict\napprove\n\nReviewed-Commit: %s\n\n## Findings\nnone\n' "$(git rev-parse HEAD)" > "$art/review-$slot.md";;
  *) printf '## Summary\ndone\n' > "$art/summary-$slot.md";;
esac
exit 0"#;

/// #175: `provider pause grok` while a run was live did not stop it dispatching its
/// next grok reviewer. Here the implementer itself pauses grok mid-run, and the
/// reviewer seat must move to its declared backup before dispatch.
#[test]
fn a_pause_set_mid_run_moves_the_next_reviewer_to_its_backup() {
    let fx = Fixture::new();
    let spar = assert_cmd::cargo::cargo_bin("spar");
    fx.cli(
        "claude",
        &counted(
            "claude",
            &format!(
                "\"{}\" provider pause cli:grok >/dev/null\n{WRITE_ARTIFACT}",
                spar.display()
            ),
        ),
    );
    fx.cli("grok", "echo x >> \"$FAKE_COUNT_DIR/grok.count\"\nexit 1");
    fx.cli("codex", &counted("codex", WRITE_ARTIFACT));
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude",
            "--role",
            "reviewer=cli:grok",
            "--backup",
            "reviewer=cli:codex",
            "--without",
            "suite",
        ])
        .assert()
        .code(2);
    assert_eq!(
        fx.count("grok"),
        0,
        "the paused provider was never dispatched"
    );
    let rev = fx.slot("reviewer");
    assert_eq!(rev["id"], "review-0-cli-codex", "{rev}");
    assert_eq!(rev["provider"], "cli:codex");
    assert_eq!(rev["source"], "backup");
    assert_eq!(fx.state()["phase"], "awaiting_ship_confirm");
}

#[test]
fn a_pause_set_mid_run_with_no_backup_parks_before_the_reviewer_dispatches() {
    let fx = Fixture::new();
    let spar = assert_cmd::cargo::cargo_bin("spar");
    fx.cli(
        "claude",
        &counted(
            "claude",
            &format!(
                "\"{}\" provider pause cli:grok >/dev/null\n{WRITE_ARTIFACT}",
                spar.display()
            ),
        ),
    );
    fx.cli("grok", "echo x >> \"$FAKE_COUNT_DIR/grok.count\"\nexit 1");
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude",
            "--role",
            "reviewer=cli:grok",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.count("grok"), 0);
    let state = fx.state();
    assert_eq!(state["phase"], "quota");
    let err = state["error"].as_str().unwrap();
    assert!(
        err.contains("cli:grok") && err.contains("no eligible backup"),
        "{err}"
    );
}

/// The box2 incident (#155): codex could not spawn its `codex-code-mode-host` sibling,
/// did zero tool calls, and the slot was salvaged as a blocking major. Here the backup
/// takes the seat and the panel approves: no fix round, no fake verdict.
#[test]
fn a_reviewer_that_cannot_run_is_retried_on_its_backup_not_counted_as_a_verdict() {
    let fx = Fixture::new();
    fx.cli("claude", &counted("claude", WRITE_ARTIFACT));
    fx.cli(
        "codex",
        &counted(
            "codex",
            "echo 'Error: failed to spawn codex-code-mode-host: No such file or directory (os error 2)' >&2\nexit 1",
        ),
    );
    fx.cli(
        "grok",
        &format!("echo x >> \"$FAKE_COUNT_DIR/grok.count\"\n{WRITE_ARTIFACT}"),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude",
            "--role",
            "reviewer=cli:codex",
            "--backup",
            "reviewer=cli:grok",
            "--without",
            "suite",
        ])
        .assert()
        .code(2);
    let state = fx.state();
    assert_eq!(state["phase"], "awaiting_ship_confirm", "{state}");
    assert_eq!(
        state["fix_rounds"], 0,
        "no round was spent on the broken CLI"
    );
    let rev = fx.slot("reviewer");
    assert_eq!(rev["id"], "review-0-cli-grok");
    assert_eq!(rev["source"], "backup");
    assert_eq!(fx.count("codex"), 1);
    assert_eq!(fx.count("grok"), 1);
    let stub = fx
        .proj
        .join(".spar/runs")
        .join(fx.run_id())
        .join("artifacts/review-review-0-cli-codex.md");
    assert!(
        !stub.exists(),
        "no verdict is recorded for a CLI that never ran"
    );
}

#[test]
fn a_reviewer_whose_backup_also_cannot_run_parks_on_quota_with_no_verdict() {
    let fx = Fixture::new();
    fx.cli("claude", &counted("claude", WRITE_ARTIFACT));
    fx.cli(
        "codex",
        &counted(
            "codex",
            "echo 'Error: failed to spawn codex-code-mode-host: No such file or directory (os error 2)' >&2\nexit 1",
        ),
    );
    fx.cli(
        "grok",
        "echo x >> \"$FAKE_COUNT_DIR/grok.count\"\necho 'You are not authenticated.' >&2\nexit 1",
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude",
            "--role",
            "reviewer=cli:codex",
            "--backup",
            "reviewer=cli:grok",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    let state = fx.state();
    assert_eq!(state["phase"], "quota");
    assert_eq!(state["fix_rounds"], 0);
    let err = state["error"].as_str().unwrap();
    assert!(
        err.contains("could not run") && err.contains("no verdict recorded"),
        "{err}"
    );
    assert_eq!(fx.count("grok"), 2, "the auth retry ran once on the backup");
    let artifacts = fx
        .proj
        .join(".spar/runs")
        .join(fx.run_id())
        .join("artifacts");
    for entry in fs::read_dir(&artifacts).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with("review-"),
            "fake verdict left behind: {name}"
        );
    }
    assert!(
        fx.quota()["providers"].get("cli:grok").is_none(),
        "auth never pauses: {}",
        fx.quota()
    );
}
