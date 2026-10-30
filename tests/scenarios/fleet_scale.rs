//! Fleet scale: idempotency keys and cross-host status (O140), the shared quota store
//! (O141, O142) and the build-cache pool (O143, O144). Every provider CLI and `ssh` is a
//! fake on a `PATH` that keeps only system dirs, and `HOME` / the user config dir live
//! in a temp dir, so no real provider, host or operator config is ever reached.
use assert_cmd::Command as AssertCommand;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

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

fn write_exe(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

struct Fixture {
    tmp: tempfile::TempDir,
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
        init_repo(&proj);
        for name in ["claude", "grok", "agy", "codex", "pi", "muse", "opencode"] {
            write_exe(
                &bin.join(name),
                "if [ \"$1\" = \"--version\" ]; then echo fake; exit 0; fi\nexit 1",
            );
        }
        write_exe(&bin.join("ssh"), "echo 'ssh: not scripted' >&2\nexit 255");
        Self {
            tmp,
            proj,
            bin,
            home,
            spar_home,
        }
    }

    fn user_config(&self, toml: &str) {
        let dir = self.home.join(".config/spar");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), toml).unwrap();
    }

    fn spar_in(&self, dir: &Path) -> AssertCommand {
        let mut cmd = AssertCommand::cargo_bin("spar").unwrap();
        cmd.env_remove("SPAR_PROJECT_ROOT")
            .env_remove("SPAR_RUN_ID")
            .env_remove("SPAR_AGENT_ID")
            .env_remove("SPAR_DRY_RUN")
            .env("SPAR_HOME", &self.spar_home)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("FAKE_LOG_DIR", &self.bin)
            .current_dir(dir);
        cmd
    }

    fn spar(&self) -> AssertCommand {
        self.spar_in(&self.proj)
    }

    fn plan(&self, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut args = vec!["plan", "--providers", "cli:claude", "--dry-run", "--json"];
        args.extend_from_slice(extra);
        self.spar().args(&args).assert()
    }

    fn state(&self, run_id: &str) -> serde_json::Value {
        let p = self.proj.join(".spar/runs").join(run_id).join("state.json");
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }
}

fn stdout_json(a: &assert_cmd::assert::Assert) -> serde_json::Value {
    let out = String::from_utf8_lossy(&a.get_output().stdout).into_owned();
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("stdout is not json ({e}): {out}"))
}

fn stderr_of(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

// ---------------------------------------------------------------------------------
// #152: idempotency keys and cross-host status
// ---------------------------------------------------------------------------------

#[test]
fn a_second_run_with_a_held_key_is_refused_until_the_holder_finishes() {
    let fx = Fixture::new();
    let first = fx
        .plan(&["-t", "fix the login bug", "--key", "pr-1234"])
        .code(2);
    let first_id = stdout_json(&first)["run_id"].as_str().unwrap().to_string();
    assert_eq!(fx.state(&first_id)["key"], "pr-1234");

    let dup = fx
        .plan(&["-t", "fix the login bug", "--key", "pr-1234"])
        .code(1);
    let v = stdout_json(&dup);
    assert_eq!(v["kind"], "duplicate");
    assert_eq!(v["run_id"], first_id.as_str());
    assert!(v["error"].as_str().unwrap().contains("pr-1234"), "{v}");
    let runs = fs::read_dir(fx.proj.join(".spar/runs")).unwrap().count();
    assert_eq!(runs, 1, "a refused launch mints no run");

    let text = fx
        .spar()
        .args([
            "implement",
            "-t",
            "fix the login bug",
            "--key",
            "pr-1234",
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert()
        .code(1);
    assert!(
        stderr_of(&text).contains(&first_id),
        "text mode names the holder: {}",
        stderr_of(&text)
    );

    fx.plan(&["-t", "other work", "--key", "pr-9999"]).code(2);

    fx.spar()
        .args(["reject", &first_id])
        .assert()
        .stdout(predicates::str::contains("rejected"));
    fx.plan(&["-t", "fix the login bug", "--key", "pr-1234"])
        .code(2);
}

#[test]
fn a_key_never_moves_onto_an_existing_run() {
    let fx = Fixture::new();
    let first = fx.plan(&["-t", "fix the login bug"]).code(2);
    let id = stdout_json(&first)["run_id"].as_str().unwrap().to_string();
    fx.spar()
        .args(["implement", "--run", &id, "--key", "k", "--dry-run"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("--key names a new run"));
    fx.spar()
        .args(["plan", "--run", &id, "-t", "again", "--key", "k"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("--key"));
}

#[test]
fn plan_brief_defaults_the_key_to_the_brief_slug() {
    let fx = Fixture::new();
    let briefs = fx.proj.join(".spar/briefs");
    fs::create_dir_all(&briefs).unwrap();
    fs::write(briefs.join("durable-runs.md"), "# Durable runs\n\nbody\n").unwrap();
    let first = fx
        .plan(&["--brief", ".spar/briefs/durable-runs.md"])
        .code(2);
    let id = stdout_json(&first)["run_id"].as_str().unwrap().to_string();
    assert_eq!(fx.state(&id)["key"], "durable-runs");

    let dup = fx
        .plan(&["--brief", ".spar/briefs/durable-runs.md"])
        .code(1);
    assert_eq!(stdout_json(&dup)["run_id"], id.as_str());

    let explicit = fx
        .plan(&["--brief", ".spar/briefs/durable-runs.md", "--key", "other"])
        .code(2);
    let id2 = stdout_json(&explicit)["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(fx.state(&id2)["key"], "other", "an explicit key wins");
}

/// A fake `ssh` that logs its argv and answers per host: `box2` lists a run holding
/// `pr-1` in a project named `proj`, `dead` refuses the connection.
fn fake_ssh(fx: &Fixture) {
    write_exe(
        &fx.bin.join("ssh"),
        r#"echo "$@" >> "$FAKE_LOG_DIR/ssh.log"
host=""
for a in "$@"; do
  case "$a" in -o|BatchMode=*|ConnectTimeout=*) ;; *) host="$a"; break;; esac
done
case "$host" in
  box2) cat <<'JSON'
[{"id":"remote01","workflow":"loop","phase":"dispatch","updated_at":"2026-10-30T00:00:00Z","key":"pr-1","project_name":"proj","archived":false},
 {"id":"remote02","workflow":"loop","phase":"done","updated_at":"2026-10-30T00:00:00Z","key":"pr-2","project_name":"proj","archived":false}]
JSON
  ;;
  *) echo "ssh: connect to host $host port 22: Connection refused" >&2; exit 255;;
esac"#,
    );
}

#[test]
fn the_key_check_reaches_remotes_and_an_unreachable_host_only_warns() {
    let fx = Fixture::new();
    fake_ssh(&fx);
    fx.user_config("[registry]\nremotes = [\"dead\", \"box2\"]\n");

    let dup = fx.plan(&["-t", "work", "--key", "pr-1"]).code(1);
    let v = stdout_json(&dup);
    assert_eq!(v["kind"], "duplicate");
    assert_eq!(v["run_id"], "remote01");
    assert_eq!(v["host"], "box2");
    assert!(
        stderr_of(&dup).contains("dead"),
        "the unreachable host is named: {}",
        stderr_of(&dup)
    );

    let ok = fx.plan(&["-t", "work", "--key", "pr-2"]).code(2);
    assert!(
        stderr_of(&ok).contains("warning: dead"),
        "the launch warns and proceeds: {}",
        stderr_of(&ok)
    );

    let log = fs::read_to_string(fx.bin.join("ssh.log")).unwrap();
    assert!(log.contains("BatchMode=yes"), "{log}");
    assert!(log.contains("spar status --all --json"), "{log}");
    let calls = log.lines().count();

    fx.plan(&["-t", "work", "--key", "pr-1", "--key-scope", "local"])
        .code(2);
    let after = fs::read_to_string(fx.bin.join("ssh.log")).unwrap();
    assert_eq!(after.lines().count(), calls, "local scope never calls ssh");
}

#[test]
fn a_remote_run_in_another_project_does_not_hold_the_key() {
    let fx = Fixture::new();
    fake_ssh(&fx);
    fx.user_config("[registry]\nremotes = [\"box2\"]\n");
    let other = fx.tmp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    init_repo(&other);
    fx.spar_in(&other)
        .args([
            "plan",
            "-t",
            "work",
            "--key",
            "pr-1",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
}

#[test]
fn status_all_hosts_merges_every_host_with_a_host_field() {
    let fx = Fixture::new();
    fake_ssh(&fx);
    fx.user_config("[registry]\nremotes = [\"box2\", \"dead\"]\n");
    let a = fx
        .spar()
        .args(["status", "--all-hosts", "--json"])
        .assert()
        .success();
    let v = stdout_json(&a);
    let hosts = v["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 3, "{v}");
    let box2 = hosts.iter().find(|h| h["host"] == "box2").unwrap();
    assert_eq!(box2["ok"], true);
    assert_eq!(box2["runs"], 2);
    let dead = hosts.iter().find(|h| h["host"] == "dead").unwrap();
    assert_eq!(dead["ok"], false);
    assert!(dead["error"]
        .as_str()
        .unwrap()
        .contains("Connection refused"));
    let runs = v["runs"].as_array().unwrap();
    assert!(runs
        .iter()
        .any(|r| r["id"] == "remote01" && r["host"] == "box2"));
    assert!(
        hosts
            .iter()
            .any(|h| h["host"] != "box2" && h["host"] != "dead" && h["ok"] == true),
        "this host is listed first, from its own registry: {v}"
    );

    let only = fx
        .spar()
        .args(["status", "--hosts", "box2", "--json"])
        .assert()
        .success();
    let v = stdout_json(&only);
    assert_eq!(v["hosts"].as_array().unwrap().len(), 1);
    assert!(v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["host"] == "box2"));

    let text = fx.spar().args(["status", "--all-hosts"]).assert().success();
    let out = String::from_utf8_lossy(&text.get_output().stdout).into_owned();
    assert!(out.contains("[dead] unreachable"), "{out}");
    assert!(out.contains("remote01"), "{out}");
}

#[test]
fn status_all_hosts_without_remotes_names_the_config() {
    let fx = Fixture::new();
    fx.spar()
        .args(["status", "--all-hosts"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("[registry] remotes"));
}

// ---------------------------------------------------------------------------------
// #186: fleet-wide quota store
// ---------------------------------------------------------------------------------

fn claude_status(list: &serde_json::Value) -> String {
    list.as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"].as_str().is_some_and(|n| n.contains("claude")))
        .map(|p| p["quota_status"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("no claude row: {list}"))
}

#[test]
fn a_pause_on_one_project_reaches_another_through_the_shared_path_by_account() {
    let fx = Fixture::new();
    let shared = fx.tmp.path().join("shared");
    fs::create_dir_all(&shared).unwrap();
    let config = |label: &str| {
        format!(
            "[quota]\nshared_path = \"{}\"\n[accounts]\n\"cli:claude\" = \"{label}\"\n",
            shared.display()
        )
    };
    fx.user_config(&config("acct-a"));
    let other = fx.tmp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    init_repo(&other);

    fx.spar()
        .args(["provider", "pause", "claude"])
        .assert()
        .success();
    let fleet = fs::read_to_string(shared.join("quota.json")).unwrap();
    assert!(fleet.contains("cli:claude#acct-a"), "{fleet}");

    let list = fx
        .spar_in(&other)
        .args(["provider", "list", "--json"])
        .assert()
        .success();
    assert_eq!(claude_status(&stdout_json(&list)), "paused_manual");

    fx.user_config(&config("acct-b"));
    let list = fx
        .spar_in(&other)
        .args(["provider", "list", "--json"])
        .assert()
        .success();
    assert_eq!(
        claude_status(&stdout_json(&list)),
        "available",
        "another account is not held by acct-a's pause"
    );

    fx.user_config(&config("acct-a"));
    fx.spar_in(&other)
        .args(["provider", "resume", "claude"])
        .assert()
        .success();
    let list = fx
        .spar()
        .args(["provider", "list", "--json"])
        .assert()
        .success();
    assert_eq!(
        claude_status(&stdout_json(&list)),
        "available",
        "a resume anywhere clears the fleet pause"
    );
}

#[test]
fn a_missing_shared_path_falls_back_to_the_project_and_warns_once() {
    let fx = Fixture::new();
    let missing = fx.tmp.path().join("not-mounted");
    fx.user_config(&format!(
        "[quota]\nshared_path = \"{}\"\n",
        missing.display()
    ));
    let a = fx
        .spar()
        .args(["provider", "pause", "claude"])
        .assert()
        .success();
    let err = stderr_of(&a);
    assert_eq!(err.matches("unavailable").count(), 1, "{err}");
    assert!(!missing.exists(), "spar never creates the shared path");
    let list = fx
        .spar()
        .args(["provider", "list", "--json"])
        .assert()
        .success();
    assert_eq!(claude_status(&stdout_json(&list)), "paused_manual");
}

// ---------------------------------------------------------------------------------
// #161: build-cache pool
// ---------------------------------------------------------------------------------

fn own_starttime() -> u64 {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 1..];
    after.split_whitespace().nth(19).unwrap().parse().unwrap()
}

/// A lease held by this (live) test process, as another orchestrator's would be.
fn hold(dir: &Path) {
    let lease = serde_json::json!({
        "pid": std::process::id(),
        "starttime": own_starttime(),
        "run_id": "other",
        "slot_id": "implementer",
        "project_root": "/elsewhere",
        "leased_at": "2026-10-30T00:00:00Z",
    });
    let name = format!("{}.spar-lease", dir.file_name().unwrap().to_string_lossy());
    fs::write(dir.with_file_name(name), lease.to_string()).unwrap();
}

fn implement_dry(fx: &Fixture) -> (assert_cmd::assert::Assert, serde_json::Value) {
    let a = fx
        .spar()
        .args([
            "implement",
            "-t",
            "build the thing",
            "--providers",
            "cli:claude,cli:codex",
            "--dry-run",
            "--json",
        ])
        .assert();
    let runs: Vec<_> = fs::read_dir(fx.proj.join(".spar/runs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    let newest = runs
        .iter()
        .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let state = fx.state(&newest);
    (a, state)
}

fn slot_of<'a>(state: &'a serde_json::Value, role: &str) -> &'a serde_json::Value {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == role)
        .unwrap_or_else(|| panic!("no {role} slot: {state}"))
}

#[test]
fn an_implementer_leases_a_free_pool_slot_and_releases_it() {
    let fx = Fixture::new();
    let pool = fx.tmp.path().join("pool");
    fs::create_dir_all(&pool).unwrap();
    fs::write(
        fx.proj.join("spar.toml"),
        format!(
            "[worktree]\nbuild_cache_pool = [\"{}/t-{{1..2}}\"]\n",
            pool.display()
        ),
    )
    .unwrap();

    let (_, state) = implement_dry(&fx);
    let t1 = pool.join("t-1");
    assert_eq!(
        slot_of(&state, "implementer")["build_cache"],
        t1.display().to_string()
    );
    assert!(
        slot_of(&state, "reviewer").get("build_cache").is_none(),
        "reviewers never lease"
    );
    assert!(
        !pool.join("t-1.spar-lease").exists(),
        "released at slot exit"
    );
    let events = fs::read_to_string(
        fx.proj
            .join(".spar/runs")
            .join(state["id"].as_str().unwrap())
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(events.contains("CARGO_TARGET_DIR="), "{events}");

    hold(&t1);
    let (_, state) = implement_dry(&fx);
    assert_eq!(
        slot_of(&state, "implementer")["build_cache"],
        pool.join("t-2").display().to_string(),
        "a live lease is skipped"
    );

    hold(&pool.join("t-2"));
    let (a, state) = implement_dry(&fx);
    assert!(
        stderr_of(&a).contains("build cache pool exhausted"),
        "{}",
        stderr_of(&a)
    );
    assert!(slot_of(&state, "implementer").get("build_cache").is_none());
}
