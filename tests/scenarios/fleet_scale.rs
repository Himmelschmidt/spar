//! Fleet scale: idempotency keys and cross-host status (O140), the shared quota store
//! (O141, O142) and the build-cache pool (O143, O144). Every provider CLI and `ssh` is a
//! fake on a `PATH` that keeps only system dirs, and `HOME` / the user config dir live
//! in a temp dir, so no real provider, host or operator config is ever reached.

mod common;
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
        let mut cmd = common::spar();
        // HOME and the user config are this fixture's own temp dirs, and these tests
        // exercise the user-config remotes and shared path the guard would ignore.
        cmd.env_remove("SPAR_TEST_ISOLATED")
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

    /// A dry-run plan (the fixture), with `extra` args.
    fn plan(&self, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut args = vec!["plan", "--providers", "cli:claude", "--dry-run", "--json"];
        args.extend_from_slice(extra);
        self.spar().args(&args).assert()
    }

    /// A real (non-dry) plan launch in `dir`. Its fake claude fails fast; these tests
    /// only care whether the key guard let it through.
    fn launch_in(&self, dir: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut args = vec!["plan", "-t", "work", "--providers", "cli:claude", "--json"];
        args.extend_from_slice(extra);
        self.spar_in(dir).args(&args).assert()
    }

    fn launch(&self, extra: &[&str]) -> assert_cmd::assert::Assert {
        self.launch_in(&self.proj, extra)
    }

    /// A run that holds `extra`'s key: minted by a dry plan, then marked real, since dry
    /// runs never hold keys.
    fn seed(&self, extra: &[&str]) -> String {
        let a = self.plan(extra).code(2);
        let id = stdout_json(&a)["run_id"].as_str().unwrap().to_string();
        let p = self.proj.join(".spar/runs").join(&id).join("state.json");
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        v["dry_run"] = serde_json::json!(false);
        fs::write(&p, v.to_string()).unwrap();
        id
    }

    fn runs(&self) -> usize {
        fs::read_dir(self.proj.join(".spar/runs"))
            .map(|d| d.count())
            .unwrap_or(0)
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

fn stdout_of(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn stderr_of(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

fn duplicate_of(a: &assert_cmd::assert::Assert) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(&stdout_of(a))
        .ok()
        .filter(|v| v["kind"] == "duplicate")
}

// ---------------------------------------------------------------------------------
// #152: idempotency keys and cross-host status
// ---------------------------------------------------------------------------------

#[test]
fn a_second_run_with_a_held_key_is_refused_until_the_holder_finishes() {
    let fx = Fixture::new();
    let holder = fx.seed(&["-t", "fix the login bug", "--key", "pr-1234"]);
    assert_eq!(fx.state(&holder)["key"], "pr-1234");

    let dup = fx.launch(&["--key", "pr-1234"]).code(1);
    let v = duplicate_of(&dup).expect("a duplicate refusal");
    assert_eq!(v["run_id"], holder.as_str());
    assert!(v["error"].as_str().unwrap().contains("pr-1234"), "{v}");
    assert_eq!(fx.runs(), 1, "a refused launch mints no run");

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
        ])
        .assert()
        .code(1);
    assert!(stderr_of(&text).contains(&holder), "{}", stderr_of(&text));

    fx.plan(&["-t", "rehearse it", "--key", "pr-1234"]).code(2);
    assert_eq!(fx.runs(), 2, "a dry run neither checks nor is refused");

    fx.spar()
        .args(["reject", &holder])
        .assert()
        .stdout(predicates::str::contains("rejected"));
    let after = fx.launch(&["--key", "pr-1234"]);
    assert!(duplicate_of(&after).is_none(), "{}", stdout_of(&after));
    assert_eq!(fx.runs(), 3);
}

#[test]
fn a_dry_run_never_holds_a_key() {
    let fx = Fixture::new();
    fx.plan(&["-t", "rehearsal", "--key", "pr-7"]).code(2);
    let real = fx.launch(&["--key", "pr-7"]);
    assert!(duplicate_of(&real).is_none(), "{}", stdout_of(&real));
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
fn only_an_intaken_brief_defaults_the_key_to_its_slug() {
    let fx = Fixture::new();
    let briefs = fx.proj.join(".spar/briefs");
    fs::create_dir_all(&briefs).unwrap();
    fs::write(briefs.join("durable-runs.md"), "# Durable runs\n\nbody\n").unwrap();
    let id = fx.seed(&["--brief", ".spar/briefs/durable-runs.md"]);
    assert_eq!(fx.state(&id)["key"], "durable-runs");

    let dup = fx
        .spar()
        .args([
            "plan",
            "--brief",
            ".spar/briefs/durable-runs.md",
            "--providers",
            "cli:claude",
            "--json",
        ])
        .assert()
        .code(1);
    assert_eq!(duplicate_of(&dup).unwrap()["run_id"], id.as_str());

    let a = fx
        .plan(&["--brief", ".spar/briefs/durable-runs.md", "--key", "other"])
        .code(2);
    let explicit = stdout_json(&a)["run_id"].as_str().unwrap().to_string();
    assert_eq!(fx.state(&explicit)["key"], "other", "an explicit key wins");

    let a = fx
        .plan(&["--brief", ".spar/briefs/durable-runs.md", "--no-key"])
        .code(2);
    let none = stdout_json(&a)["run_id"].as_str().unwrap().to_string();
    assert!(fx.state(&none).get("key").is_none(), "--no-key sets none");

    let loose = fx.tmp.path().join("notes.md");
    fs::write(&loose, "# Notes\n\nbody\n").unwrap();
    let a = fx.plan(&["--brief", loose.to_str().unwrap()]).code(2);
    let id = stdout_json(&a)["run_id"].as_str().unwrap().to_string();
    assert!(
        fx.state(&id).get("key").is_none(),
        "a file outside .spar/briefs is not an intaken brief"
    );
}

/// A fake `ssh` that logs its argv and answers per host: `box2` lists runs holding
/// `pr-1` (by name) and `pr-3` (by origin URL), `old` is a spar without run keys, and
/// `dead` refuses the connection.
fn fake_ssh(fx: &Fixture) {
    write_exe(
        &fx.bin.join("ssh"),
        r#"echo "$@" >> "$FAKE_LOG_DIR/ssh.log"
host=""
for a in "$@"; do
  case "$a" in -o|BatchMode=*|ConnectTimeout=*|--) ;; *) host="$a"; break;; esac
done
case "$host" in
  box2) cat <<'JSON'
[{"id":"remote01","workflow":"loop","phase":"dispatch","updated_at":"2026-10-30T00:00:00Z","key":"pr-1","dry_run":false,"project_name":"proj","archived":false},
 {"id":"remote02","workflow":"loop","phase":"done","updated_at":"2026-10-30T00:00:00Z","key":"pr-2","dry_run":false,"project_name":"proj","archived":false},
 {"id":"remote03","workflow":"loop","phase":"dispatch","updated_at":"2026-10-30T00:00:00Z","key":"pr-3","dry_run":false,"project_name":"renamed","project_repo":"github.com/acme/app","archived":false}]
JSON
  ;;
  old) echo '[{"id":"old00001","workflow":"loop","phase":"dispatch","updated_at":"2026-10-30T00:00:00Z","project_name":"proj"}]';;
  *) echo "ssh: connect to host $host port 22: Connection refused" >&2; exit 255;;
esac"#,
    );
}

fn ssh_calls(fx: &Fixture) -> usize {
    fs::read_to_string(fx.bin.join("ssh.log"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[test]
fn the_key_check_reaches_remotes_and_an_unreachable_host_only_warns() {
    let fx = Fixture::new();
    fake_ssh(&fx);
    fx.user_config("[registry]\nremotes = [\"dead\", \"old\", \"box2\"]\n");

    let dup = fx.launch(&["--key", "pr-1"]).code(1);
    let v = duplicate_of(&dup).expect("refused");
    assert_eq!(v["run_id"], "remote01");
    assert_eq!(v["host"], "box2");
    let err = stderr_of(&dup);
    assert!(
        err.contains("warning: dead"),
        "the unreachable host is named: {err}"
    );
    assert!(
        err.contains("old runs a spar that does not report run keys"),
        "{err}"
    );

    let ok = fx.launch(&["--key", "pr-2"]);
    assert!(duplicate_of(&ok).is_none(), "{}", stdout_of(&ok));
    assert!(
        stderr_of(&ok).contains("warning: dead"),
        "warns and proceeds"
    );

    let log = fs::read_to_string(fx.bin.join("ssh.log")).unwrap();
    assert!(log.contains("BatchMode=yes"), "{log}");
    assert!(log.contains("-- box2 spar status --all --json"), "{log}");

    let calls = ssh_calls(&fx);
    fx.launch(&["--key", "pr-9", "--key-scope", "local"]);
    assert_eq!(ssh_calls(&fx), calls, "local scope never calls ssh");
    fx.plan(&["-t", "dry", "--key", "pr-1"]).code(2);
    assert_eq!(ssh_calls(&fx), calls, "a dry run never checks");
}

#[test]
fn a_remote_run_matches_on_origin_url_before_directory_name() {
    let fx = Fixture::new();
    fake_ssh(&fx);
    fx.user_config("[registry]\nremotes = [\"box2\"]\n");
    let other = fx.tmp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    init_repo(&other);
    let a = fx.launch_in(&other, &["--key", "pr-1"]);
    assert!(
        duplicate_of(&a).is_none(),
        "another project's run does not hold the key: {}",
        stdout_of(&a)
    );

    let out = std::process::Command::new("git")
        .args(["remote", "add", "origin", "git@github.com:acme/app.git"])
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(out.status.success());
    let dup = fx.launch_in(&other, &["--key", "pr-3"]).code(1);
    assert_eq!(duplicate_of(&dup).unwrap()["run_id"], "remote03");
}

#[test]
fn an_isolated_test_spawn_ignores_user_config_remotes() {
    let fx = Fixture::new();
    fake_ssh(&fx);
    fx.user_config("[registry]\nremotes = [\"box2\"]\n");
    let a = fx
        .spar()
        .env("SPAR_TEST_ISOLATED", "1")
        .args([
            "plan",
            "-t",
            "work",
            "--key",
            "pr-1",
            "--providers",
            "cli:claude",
            "--json",
        ])
        .assert();
    assert!(duplicate_of(&a).is_none(), "{}", stdout_of(&a));
    assert_eq!(ssh_calls(&fx), 0, "the guard never reaches ssh");
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
    assert_eq!(box2["runs"], 3);
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

/// The implementer's model dispatch (`claude -p`) records the CARGO_TARGET_DIR it sees.
fn recording_claude(fx: &Fixture) {
    write_exe(
        &fx.bin.join("claude"),
        "if [ \"$1\" = \"--version\" ]; then echo fake; exit 0; fi\n\
         if [ \"$1\" = \"-p\" ]; then echo \"$CARGO_TARGET_DIR\" >> \"$FAKE_LOG_DIR/target.log\"; fi\n\
         exit 1",
    );
}

fn implement_real(fx: &Fixture) {
    // The fake fails every dispatch; only what it saw matters here.
    let _ = fx
        .spar()
        .env("CARGO_TARGET_DIR", "/shared/target")
        .args([
            "implement",
            "-t",
            "build the thing",
            "--role",
            "implementer=cli:claude",
            "--without",
            "suite",
        ])
        .assert();
}

/// `[env] capture` puts the creating shell's CARGO_TARGET_DIR back on the orchestrator
/// (O130); the implementer slot's own value, a leased pool directory or its worktree's
/// target, must still be what the agent sees (O143).
#[test]
fn the_slot_env_beats_a_captured_cargo_target_dir() {
    let fx = Fixture::new();
    recording_claude(&fx);
    let pool = fx.tmp.path().join("pool");
    fs::create_dir_all(&pool).unwrap();
    fs::write(
        fx.proj.join("spar.toml"),
        format!(
            "[env]\ncapture = [\"CARGO_TARGET_DIR\"]\n[worktree]\nbuild_cache_pool = [\"{}/t-1\"]\n",
            pool.display()
        ),
    )
    .unwrap();

    implement_real(&fx);
    let seen = fs::read_to_string(fx.bin.join("target.log")).expect("the implementer ran");
    let first = seen.lines().next().unwrap();
    assert_eq!(first, pool.join("t-1").display().to_string(), "{seen}");

    fs::remove_file(fx.bin.join("target.log")).unwrap();
    hold(&pool.join("t-1"));
    implement_real(&fx);
    let seen = fs::read_to_string(fx.bin.join("target.log")).expect("the implementer ran");
    let first = seen.lines().next().unwrap();
    assert_ne!(
        first, "/shared/target",
        "an exhausted pool never inherits the captured value"
    );
    assert!(first.ends_with("/target"), "{seen}");
}
