//! Per-model-family quota pauses for claude (O152): the weekly `seven_day_opus` /
//! `seven_day_sonnet` windows and the family prose lines pause one family bucket
//! (`cli:claude@opus` / `@sonnet` / `@fable`), while login-wide windows
//! (`five_hour`, `seven_day`, `overage`, generic prose) pause `cli:claude`.
//! A fake `claude` on `PATH` emits stream-json `rate_limit_event` lines the way the
//! real CLI does; seats are told apart by the `--model` value spar passes.

mod common;
use assert_cmd::Command as AssertCommand;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn spar_cmd(home: &Path) -> AssertCommand {
    let mut cmd = common::spar();
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
        let fx = Self {
            _tmp: tmp,
            proj,
            bin,
            home,
        };
        for name in ["claude", "grok", "agy", "codex", "pi", "muse"] {
            fx.cli(name, "exit 1");
        }
        fx
    }

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
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
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
            .filter(|e| e.path().join("state.json").is_file())
            .max_by_key(|e| {
                e.path()
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH)
            })
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
}

/// Artifact body for a fake claude: satisfies whatever artifact the slot owes,
/// mirroring the agy fake in provider_health.
fn claude_artifacts() -> String {
    "run=\"${SPAR_AGENT_ID%%:*}\"; slot=\"${SPAR_AGENT_ID#*:}\"\n\
    art=\"$FAKE_PROJECT_ROOT/.spar/runs/$run/artifacts\"\n\
    mkdir -p \"$art\"\n\
    printf '## Plan\\ndone\\n' > \"$art/plan.md\"\n\
    printf '## Tests\\ndone\\n' > \"$art/test-contract.md\"\n\
    case \"$slot\" in\n\
      review-*) printf '## Verdict\\napprove\\n\\nReviewed-Commit: %s\\n\\n## Findings\\nnone\\n' \"$(git -C \"$FAKE_PROJECT_ROOT\" rev-parse HEAD)\" > \"$art/review-$slot.md\";;\n\
      *) printf '## Summary\\ndone\\n' > \"$art/summary-$slot.md\";;\n\
    esac\nexit 0"
        .into()
}

/// A fake claude headless dispatch: counts `-p` invocations, pulls `--model` out
/// of argv, and runs the per-model arm. Any model without a rejection arm
/// succeeds and writes its artifacts.
fn claude_model_fake(arms: &str) -> String {
    format!(
        "case \"$1\" in -p) echo x >> \"$FAKE_COUNT_DIR/claude.count\";; *) exit 0;; esac\n\
        model=\"\"; prev=\"\"; for a in \"$@\"; do if [ \"$prev\" = \"--model\" ]; then model=\"$a\"; fi; prev=\"$a\"; done\n\
        case \"$model\" in\n{arms}*) {};;\nesac",
        claude_artifacts()
    )
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

fn seed_quota_pause(fx: &Fixture, key: &str) {
    fs::create_dir_all(fx.proj.join(".spar")).unwrap();
    fs::write(
        fx.proj.join(".spar/quota.json"),
        serde_json::json!({"providers": {key: {
            "name": key,
            "status": "paused_quota",
            "hint": "possible quota signal: quota",
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "cooldown_until": (chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339(),
        }}})
        .to_string(),
    )
    .unwrap();
}

/// AC-1: a typed `seven_day_opus` rejection on an opus seat pauses
/// `cli:claude@opus` only. Sonnet and haiku seats stay eligible at the next
/// run-start gate.
#[test]
fn typed_opus_rejection_pauses_only_the_opus_bucket() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(2);
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &claude_model_fake(&format!(
            "opus) echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_opus\",\"resetsAt\":{}}}}}'; exit 1;;",
            reset_at.timestamp()
        )),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.count("claude"), 1, "the opus seat dispatched once");
    assert_eq!(fx.state()["phase"], "quota");
    let quota = fx.quota();
    let opus = &quota["providers"]["cli:claude@opus"];
    assert!(opus.is_object(), "the opus bucket is paused: {quota}");
    assert_eq!(opus["status"], "cooldown");
    let stored: chrono::DateTime<chrono::Utc> = opus["cooldown_until"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("a parseable cooldown instant");
    assert_eq!(
        stored.timestamp(),
        reset_at.timestamp(),
        "the stated instant lands on the family bucket"
    );
    assert!(
        quota["providers"].get("cli:claude").is_none(),
        "no login-wide pause: {quota}"
    );

    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@sonnet",
            "--role",
            "reviewer=cli:claude@haiku",
            "--without",
            "suite",
        ])
        .assert()
        .code(2);
    assert_eq!(
        fx.count("claude"),
        3,
        "sonnet and haiku seats dispatch past the opus pause"
    );

    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.count("claude"), 3, "the opus seat never dispatched");
    assert_eq!(fx.state()["phase"], "quota");
}

/// AC-1 mirror: `seven_day_sonnet` pauses only `cli:claude@sonnet`.
#[test]
fn typed_sonnet_rejection_pauses_only_the_sonnet_bucket() {
    let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp();
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &claude_model_fake(&format!(
            "sonnet) echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_sonnet\",\"resetsAt\":{reset}}}}}'; exit 1;;"
        )),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@sonnet",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    let quota = fx.quota();
    assert!(
        quota["providers"]["cli:claude@sonnet"].is_object(),
        "the sonnet bucket is paused: {quota}"
    );
    assert!(
        quota["providers"].get("cli:claude").is_none(),
        "no login-wide pause: {quota}"
    );

    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--role",
            "reviewer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(2);
}

/// AC-2: a typed `five_hour` rejection on an opus seat pauses the login:
/// sonnet and haiku seats are held.
#[test]
fn typed_five_hour_rejection_on_an_opus_seat_pauses_the_login() {
    let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp();
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &claude_model_fake(&format!(
            "opus) echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"five_hour\",\"resetsAt\":{reset}}}}}'; exit 1;;"
        )),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    let quota = fx.quota();
    assert!(
        quota["providers"]["cli:claude"].is_object(),
        "the login is paused: {quota}"
    );

    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@sonnet",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(
        fx.count("claude"),
        1,
        "the sonnet seat never dispatched past the login pause"
    );
}

/// AC-2 prose half: a generic limit line on an opus seat pauses the login.
#[test]
fn generic_prose_limit_on_an_opus_seat_pauses_the_login() {
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &claude_model_fake(
            "opus) echo \"You've hit your limit \\u{b7} resets 3pm (UTC)\"; exit 1;;",
        ),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    let quota = fx.quota();
    assert!(
        quota["providers"]["cli:claude"].is_object(),
        "the login is paused: {quota}"
    );
    assert!(
        quota["providers"].get("cli:claude@opus").is_none(),
        "no family bucket: {quota}"
    );
}

/// AC-3: `cli:claude@claude-opus-5-5` and `cli:claude@opus` are one bucket.
#[test]
fn a_full_model_id_shares_the_family_bucket() {
    let fx = Fixture::new();
    fx.cli("claude", &claude_model_fake(""));
    fx.spar()
        .args(["provider", "pause", "cli:claude@claude-opus-5-5"])
        .assert()
        .success();
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.count("claude"), 0, "the opus seat never dispatched");
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@sonnet",
            "--role",
            "reviewer=cli:claude@sonnet",
            "--without",
            "suite",
        ])
        .assert()
        .code(2);
}

/// A haiku seat has no bucket: pausing it pauses the login, and the command
/// says so.
#[test]
fn pausing_a_haiku_seat_pauses_the_login() {
    let fx = Fixture::new();
    let out = fx
        .spar()
        .args(["provider", "pause", "cli:claude@haiku"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("paused provider cli:claude"),
        "the command names the login key it paused"
    );
}

/// AC-4: prose "You've reached your Fable limit" pauses only
/// `cli:claude@fable`, and the run parks on quota.
#[test]
fn fable_prose_pauses_only_the_fable_bucket() {
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &claude_model_fake("fable) echo \"You've reached your Fable limit\"; exit 1;;"),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@fable",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.state()["phase"], "quota");
    let quota = fx.quota();
    let fable = &quota["providers"]["cli:claude@fable"];
    assert!(fable.is_object(), "the fable bucket is paused: {quota}");
    assert!(
        fable["strong"].as_bool() == Some(true),
        "a named rejection is trusted: {quota}"
    );
    assert!(
        quota["providers"].get("cli:claude").is_none(),
        "no login-wide pause: {quota}"
    );

    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--role",
            "reviewer=cli:claude@sonnet",
            "--without",
            "suite",
        ])
        .assert()
        .code(2);
}

/// AC-5: a legacy `cli:claude` pause holds every family; a CLI-level resume
/// clears every family key; a family resume clears one and says when the login
/// still holds it.
#[test]
fn legacy_login_pause_holds_every_family_and_resume_fans_out() {
    let fx = Fixture::new();
    fx.cli("claude", &claude_model_fake(""));
    seed_quota_pause(&fx, "cli:claude");
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(4);
    assert_eq!(fx.count("claude"), 0, "the opus seat never dispatched");

    let claude = list_entry(&fx, "claude");
    assert_eq!(claude["quota_status"], "paused_quota");

    fx.spar()
        .args(["provider", "pause", "cli:claude@opus"])
        .assert()
        .success();
    fx.spar()
        .args(["provider", "pause", "cli:claude@sonnet"])
        .assert()
        .success();
    fx.spar()
        .args(["provider", "resume", "cli:claude"])
        .assert()
        .success();
    let quota = fx.quota();
    for key in ["cli:claude", "cli:claude@opus", "cli:claude@sonnet"] {
        assert!(
            quota["providers"]
                .get(key)
                .is_none_or(|q| q["status"] == "available"),
            "{key} is cleared: {quota}"
        );
    }

    fx.spar()
        .args(["provider", "pause", "cli:claude@opus"])
        .assert()
        .success();
    fx.spar()
        .args(["provider", "pause", "cli:claude@sonnet"])
        .assert()
        .success();
    fx.spar()
        .args(["provider", "pause", "cli:claude"])
        .assert()
        .success();
    let out = fx
        .spar()
        .args(["provider", "resume", "cli:claude@sonnet", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["name"], "cli:claude@sonnet");
    assert_eq!(v["held_by"], "cli:claude");
    let quota = fx.quota();
    assert_eq!(
        quota["providers"]["cli:claude@opus"]["status"], "paused_manual",
        "the sibling keeps its pause: {quota}"
    );
}

/// AC-6: a same-login primary/backup pair across families is rejected at
/// launch, exactly like the agy pair.
#[test]
fn same_sub_claude_buckets_are_rejected_at_launch() {
    let fx = Fixture::new();
    fx.cli("claude", &claude_model_fake(""));
    let err = fx
        .spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "reviewer=cli:claude@opus",
            "--backup",
            "reviewer=cli:claude@sonnet",
            "--without",
            "suite",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("same provider storage key as primary (cli:claude)"),
        "same-sub pair rejected like one provider"
    );
}

/// A probe whose output is only a raw stream-json `rate_limit_event` still
/// pauses the right bucket: the probe parses the typed event, not just prose.
#[test]
fn probe_parses_a_raw_stream_json_rejection() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(2);
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &format!(
            "echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_opus\",\"resetsAt\":{}}}}}'; exit 1",
            reset_at.timestamp()
        ),
    );
    let out = fx
        .spar()
        .args(["provider", "probe", "cli:claude@opus", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r[0]["verdict"], "quota", "{r}");
    assert_eq!(r[0]["provider"], "cli:claude@opus", "{r}");
    let quota = fx.quota();
    let opus = &quota["providers"]["cli:claude@opus"];
    assert!(opus.is_object(), "the opus bucket is paused: {quota}");
    assert_eq!(opus["status"], "cooldown");
    assert!(
        quota["providers"].get("cli:claude").is_none(),
        "no login-wide pause: {quota}"
    );
}

/// A raw `seven_day_opus` event next to a generic limit line pauses the login:
/// the window votes alongside the prose, broadest wins.
#[test]
fn probe_widens_a_typed_event_with_generic_prose_to_the_login() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(2);
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &format!(
            "echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_opus\",\"resetsAt\":{}}}}}'; echo \"You've hit your limit\"; exit 1",
            reset_at.timestamp()
        ),
    );
    let out = fx
        .spar()
        .args(["provider", "probe", "cli:claude@opus", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(r[0]["verdict"], "quota", "{r}");
    assert_eq!(r[0]["provider"], "cli:claude", "{r}");
    let quota = fx.quota();
    assert!(
        quota["providers"]["cli:claude"].is_object(),
        "the login is paused: {quota}"
    );
}

/// Recovery: a stream that reports `seven_day_opus` rejected and then recovers
/// with `allowed` writes no pause on any claude key when the dispatch fails for
/// an unrelated reason.
#[test]
fn a_recovered_opus_window_writes_no_pause() {
    let reset = (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp();
    let fx = Fixture::new();
    fx.cli(
        "claude",
        &claude_model_fake(&format!(
            "opus) echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_opus\",\"resetsAt\":{reset}}}}}'; \
             echo '{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"allowed\",\"rateLimitType\":\"seven_day_opus\"}}}}'; \
             exit 1;;"
        )),
    );
    fx.spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--role",
            "implementer=cli:claude@opus",
            "--without",
            "suite",
        ])
        .assert()
        .code(1);
    let quota = fx.quota();
    assert!(
        quota["providers"].as_object().unwrap().is_empty(),
        "a recovered rejection pauses nothing: {quota}"
    );
}
