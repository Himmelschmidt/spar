//! The launch surface a fleet launcher drives: pre-checks, refusals as data, round
//! extensions, slot identity and the config a run is created under.
//!
//! Every spawn here runs with `PATH` holding only `git`, and `HOME` /
//! `XDG_CONFIG_HOME` inside the test's own tempdir, so no provider CLI and no user
//! config on the host can ever be reached, whatever a test gets wrong.
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct Sandbox {
    _tmp: TempDir,
    project: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        for d in [&project, &home, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        let git = which_git();
        std::os::unix::fs::symlink(&git, bin.join("git")).unwrap();
        let sb = Self {
            _tmp: tmp,
            project,
            home,
            bin,
        };
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            sb.git(&args);
        }
        std::fs::write(sb.project.join("README.md"), "test\n").unwrap();
        sb.git(&["add", "."]);
        sb.git(&["commit", "-q", "-m", "init"]);
        sb
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new(which_git())
            .args(args)
            .current_dir(&self.project)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn spar_std(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_spar"));
        c.current_dir(&self.project)
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("SPAR_HOME", self.home.join(".spar"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("SPAR_PROJECT_ROOT")
            .env_remove("SPAR_RUN_ID")
            .env_remove("SPAR_AGENT_ID")
            .env_remove("SPAR_SLOT")
            .env_remove("SPAR_DRY_RUN");
        for name in [
            "CARGO_TARGET_DIR",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "https_proxy",
            "http_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            c.env_remove(name);
        }
        c
    }

    fn spar(&self) -> assert_cmd::Command {
        assert_cmd::Command::from_std(self.spar_std())
    }

    fn runs(&self) -> Vec<String> {
        let dir = self.project.join(".spar/runs");
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }
}

fn which_git() -> PathBuf {
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        let p = Path::new(dir).join("git");
        if p.is_file() {
            return p;
        }
    }
    panic!("git not found");
}

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// #187: a launcher pre-checks its roles without minting a run.
#[test]
fn validate_only_reports_and_creates_nothing() {
    let sb = Sandbox::new();
    for args in [
        vec!["plan", "-t", "task", "--providers", "cli:claude,cli:codex"],
        vec!["implement", "-t", "task", "--providers", "cli:claude"],
        vec![
            "run",
            "--workflow",
            "review",
            "-t",
            "task",
            "--providers",
            "cli:claude",
        ],
    ] {
        let mut full = args.clone();
        full.extend(["--dry-run", "--json", "--validate-only"]);
        let out = sb.spar().args(&full).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{full:?}: {out:?}");
        let v = json(&out);
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["kind"], "validation", "{v}");
        assert!(v["run_id"].is_null(), "{v}");
    }
    assert!(
        sb.runs().is_empty(),
        "validate-only made runs: {:?}",
        sb.runs()
    );
}

/// #187: every refusal before a run exists is exit 1 with one parseable shape, and
/// leaves nothing under `.spar/runs`.
#[test]
fn launch_refusals_are_json_with_no_run() {
    let sb = Sandbox::new();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![
                "plan",
                "-t",
                "x",
                "--role",
                "implementer=cli:claude@opus",
                "--backup",
                "implementer=cli:claude@sonnet",
            ],
            "same provider storage key",
        ),
        (
            vec!["plan", "-t", "x", "--providers", "cli:nosuch"],
            "unknown provider",
        ),
        (
            vec!["implement", "-t", "x", "--role", "reviewer=cli:typo"],
            "unknown provider",
        ),
        (
            vec![
                "run",
                "--workflow",
                "loop",
                "-t",
                "x",
                "--providers",
                "cli:claude",
                "--fleet",
                "huge",
            ],
            "huge",
        ),
        (
            vec!["plan", "-t", "   ", "--providers", "cli:claude"],
            "task",
        ),
        (vec!["plan", "-t", "x", "--no-such-flag"], "--no-such-flag"),
        (
            vec![
                "implement",
                "-t",
                "x",
                "--providers",
                "cli:claude",
                "--base",
                "nope/nope",
            ],
            "--base nope/nope",
        ),
    ];
    for (args, needle) in cases {
        for extra in [vec!["--json"], vec!["--json", "--validate-only"]] {
            let mut full = args.clone();
            full.extend(extra.iter().copied());
            full.push("--dry-run");
            let out = sb.spar().args(&full).output().unwrap();
            assert_eq!(out.status.code(), Some(1), "{full:?}: {out:?}");
            let v = json(&out);
            assert_eq!(v["kind"], "validation", "{full:?}: {v}");
            assert_eq!(v["ok"], false, "{v}");
            assert!(v["run_id"].is_null(), "{v}");
            let err = v["error"].as_str().unwrap_or_default();
            assert!(err.contains(needle), "{full:?}: {err}");
        }
    }
    assert!(sb.runs().is_empty(), "refusals made runs: {:?}", sb.runs());
}

/// clap's own usage exit is 2, which is spar's human gate. Without `--json` the text
/// stays, the code does not.
#[test]
fn usage_errors_exit_one() {
    let sb = Sandbox::new();
    let out = sb.spar().args(["status", "--nope"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--nope"));
    let out = sb.spar().args(["--help"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

impl Sandbox {
    /// A dry-run plan, approved, then implemented up to its `[rounds] max` gate.
    fn run_at_ceiling(&self, max: u32) -> String {
        std::fs::write(
            self.project.join("spar.toml"),
            format!("[rounds]\nmax = {max}\n"),
        )
        .unwrap();
        let out = self
            .spar()
            .args([
                "plan",
                "-t",
                "add a hello function",
                "--providers",
                "cli:claude,cli:grok",
                "--dry-run",
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let id = json(&out)["run_id"].as_str().unwrap().to_string();
        self.spar()
            .args(["approve", &id, "--json"])
            .assert()
            .success();
        self.implement(&id, &[]).code(2);
        assert_eq!(self.state(&id)["phase"], "awaiting_round_extension");
        id
    }

    fn implement(&self, id: &str, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut args = vec!["implement", "--run", id, "--dry-run", "--json"];
        args.extend_from_slice(extra);
        self.spar()
            .env("SPAR_FORCE_REQUEST_CHANGES", "1")
            .args(&args)
            .assert()
    }

    fn state(&self, id: &str) -> serde_json::Value {
        let p = self.project.join(".spar/runs").join(id).join("state.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }
}

/// #165: `+N` is relative to the round the run is on, so a launcher need not read the
/// state first to buy two more rounds.
#[test]
fn max_rounds_plus_n_is_relative_to_the_current_round() {
    let sb = Sandbox::new();
    let id = sb.run_at_ceiling(3);
    let round = sb.state(&id)["round"].as_u64().unwrap();
    sb.implement(&id, &["--max-rounds", "+2"]).code(2);
    let st = sb.state(&id);
    assert_eq!(st["max_rounds"].as_u64(), Some(round + 2), "{st}");
    assert_eq!(st["round"].as_u64(), Some(round + 2), "{st}");
}

/// #165: a ceiling at or below the current round used to re-park silently.
#[test]
fn max_rounds_at_or_below_the_current_round_is_refused() {
    let sb = Sandbox::new();
    let id = sb.run_at_ceiling(3);
    let round = sb.state(&id)["round"].as_u64().unwrap().to_string();
    let out = sb
        .implement(&id, &["--max-rounds", &round])
        .code(1)
        .get_output()
        .clone();
    let v = json(&out);
    assert_eq!(v["kind"], "validation", "{v}");
    assert_eq!(v["run_id"], id.as_str(), "{v}");
    assert!(v["error"].as_str().unwrap().contains("at or below"), "{v}");
    let st = sb.state(&id);
    assert_eq!(st["phase"], "awaiting_round_extension", "{st}");
    assert_eq!(st["max_rounds"].as_u64(), Some(3), "{st}");
    sb.implement(&id, &["--max-rounds", "+0"]).code(1);
}

impl Sandbox {
    /// Hold the run's orchestrator lock the way a live orchestrator does.
    fn hold_lock(&self, id: &str) -> std::fs::File {
        use std::io::Write;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(
                self.project
                    .join(".spar/runs")
                    .join(id)
                    .join("orchestrator.lock"),
            )
            .unwrap();
        lock.try_lock().unwrap();
        (&lock)
            .write_all(std::process::id().to_string().as_bytes())
            .unwrap();
        lock
    }

    fn queued_marker(&self, id: &str) -> PathBuf {
        self.project
            .join(".spar/runs")
            .join(id)
            .join("markers/max-rounds.queued")
    }

    fn set_phase(&self, id: &str, phase: &str) {
        let p = self.project.join(".spar/runs").join(id).join("state.json");
        let mut st = self.state(id);
        st["phase"] = serde_json::json!(phase);
        std::fs::write(p, serde_json::to_string_pretty(&st).unwrap()).unwrap();
    }
}

/// #165 / review: a run already parked at the gate whose orchestrator is still
/// finishing up is not queued (nothing would read the marker): the extension waits
/// for the lock and is applied normally.
#[test]
fn max_rounds_on_a_parked_run_waits_for_the_lock_instead_of_queueing() {
    let sb = Sandbox::new();
    let id = sb.run_at_ceiling(3);
    let round = sb.state(&id)["round"].as_u64().unwrap();
    let lock = sb.hold_lock(&id);
    let child = sb
        .spar_std()
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--run",
            &id,
            "--dry-run",
            "--json",
            "--max-rounds",
            "+2",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert!(
        !sb.queued_marker(&id).exists(),
        "a parked run must not be queued"
    );
    drop(lock);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let st = sb.state(&id);
    assert_eq!(st["max_rounds"].as_u64(), Some(round + 2), "{st}");
    assert_eq!(st["round"].as_u64(), Some(round + 2), "{st}");
}

/// The lock never frees: fail loudly, as JSON under `--json`, and leave nothing queued.
#[test]
fn max_rounds_on_a_parked_run_held_too_long_is_busy() {
    let sb = Sandbox::new();
    let id = sb.run_at_ceiling(3);
    let _lock = sb.hold_lock(&id);
    let out = sb
        .implement(&id, &["--max-rounds", "+2"])
        .code(1)
        .get_output()
        .clone();
    let v = json(&out);
    assert_eq!(v["kind"], "validation", "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("running orchestrator"),
        "{v}"
    );
    assert!(!sb.queued_marker(&id).exists());
    assert_eq!(sb.state(&id)["max_rounds"].as_u64(), Some(3));
}

/// In flight, the extension is queued for the live orchestrator; anything beyond an
/// extension still needs the lock; and a re-entry after that orchestrator exits still
/// honours what was queued.
#[test]
fn max_rounds_against_an_in_flight_orchestrator_is_queued() {
    let sb = Sandbox::new();
    let id = sb.run_at_ceiling(3);
    let round = sb.state(&id)["round"].as_u64().unwrap();
    sb.set_phase(&id, "fix");
    let lock = sb.hold_lock(&id);

    let out = sb
        .implement(&id, &["--max-rounds", "+2"])
        .code(0)
        .get_output()
        .clone();
    let v = json(&out);
    assert_eq!(v["queued_max_rounds"].as_u64(), Some(round + 2), "{v}");
    assert!(sb.queued_marker(&id).is_file(), "queued marker missing");
    assert_eq!(sb.state(&id)["max_rounds"].as_u64(), Some(3));

    let out = sb
        .implement(&id, &["--max-rounds", "+2", "-t", "also this"])
        .code(1)
        .get_output()
        .clone();
    assert_eq!(json(&out)["kind"], "validation");

    // The orchestrator parked without reaching another ceiling check and exited.
    sb.set_phase(&id, "awaiting_round_extension");
    drop(lock);
    sb.implement(&id, &[]).code(2);
    let st = sb.state(&id);
    assert_eq!(st["max_rounds"].as_u64(), Some(round + 2), "{st}");
    assert!(!sb.queued_marker(&id).exists(), "consumed once");
}

fn run_config(sb: &Sandbox, id: &str) -> serde_json::Value {
    let p = sb.project.join(".spar/runs").join(id).join("config.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// #182: `implement --spec` keeps a long brief out of spar's own argv, from a file or
/// from stdin.
#[test]
fn implement_spec_reads_the_task_from_a_file_or_stdin() {
    let sb = Sandbox::new();
    let spec = sb.project.join("brief.md");
    std::fs::write(&spec, "SPEC-FROM-FILE: add a hello function\n").unwrap();
    for (arg, stdin) in [
        (spec.to_str().unwrap(), None),
        ("-", Some("SPEC-FROM-STDIN\n")),
    ] {
        let mut cmd = sb.spar();
        cmd.args([
            "implement",
            "--spec",
            arg,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ]);
        if let Some(body) = stdin {
            cmd.write_stdin(body);
        }
        let out = cmd.output().unwrap();
        let v = json(&out);
        let id = v["run_id"].as_str().expect("run_id").to_string();
        let task = sb.state(&id)["task"].as_str().unwrap().to_string();
        let want = if stdin.is_some() {
            "SPEC-FROM-STDIN"
        } else {
            "SPEC-FROM-FILE"
        };
        assert!(task.contains(want), "{task}");
    }
    sb.spar()
        .args([
            "implement",
            "-t",
            "x",
            "--spec",
            "-",
            "--providers",
            "cli:claude",
        ])
        .assert()
        .code(1);
}

/// #189: a base commit whose spar.toml differs from the working file is flagged, and
/// the run still froze the working file.
#[test]
fn a_base_with_different_spar_toml_is_flagged_not_followed() {
    let sb = Sandbox::new();
    std::fs::write(sb.project.join("spar.toml"), "max_agents = 2\n").unwrap();
    sb.git(&["add", "spar.toml"]);
    sb.git(&["commit", "-q", "-m", "config"]);
    let launch = |sb: &Sandbox| {
        sb.spar()
            .args([
                "implement",
                "-t",
                "x",
                "--providers",
                "cli:claude",
                "--dry-run",
                "--json",
            ])
            .output()
            .unwrap()
    };

    let out = launch(&sb);
    let v = json(&out);
    assert_eq!(v["config_source_drift"], false, "{v}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("differs from base"));

    std::fs::write(sb.project.join("spar.toml"), "max_agents = 3\n").unwrap();
    let out = launch(&sb);
    let v = json(&out);
    assert_eq!(v["config_source_drift"], true, "{v}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("differs from base"), "{stderr}");
    let id = v["run_id"].as_str().unwrap();
    assert_eq!(sb.state(id)["config_source_drift"], true);
    assert_eq!(
        run_config(&sb, id)["max_agents"],
        3,
        "the working file still wins"
    );
}

/// #173: the creating shell's proxy is frozen into the run and comes back on a
/// re-entry from a shell without it, with a warning naming the variable only.
#[test]
fn captured_env_is_recorded_and_reapplied_on_reentry() {
    let sb = Sandbox::new();
    std::fs::write(sb.project.join("spar.toml"), "[rounds]\nmax = 3\n").unwrap();
    let proxy = "http://user:pw@proxy.invalid:3128";
    let out = sb
        .spar()
        .env("HTTPS_PROXY", proxy)
        .env("CARGO_TARGET_DIR", "/nonexistent/target")
        .args([
            "plan",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let id = json(&out)["run_id"].as_str().unwrap().to_string();
    let cfg = run_config(&sb, &id);
    assert_eq!(cfg["env"]["captured"]["HTTPS_PROXY"], proxy, "{cfg}");
    assert_eq!(
        cfg["env"]["captured"]["CARGO_TARGET_DIR"],
        "/nonexistent/target"
    );
    sb.spar()
        .args(["approve", &id, "--json"])
        .assert()
        .success();

    let out = sb.implement(&id, &[]).get_output().clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[env]") && stderr.contains("HTTPS_PROXY"),
        "{stderr}"
    );
    assert!(!stderr.contains("pw@"), "values never printed: {stderr}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("pw@"));

    // Credentials may sit in a proxy URL: the snapshot is owner-only, and no
    // observing command prints it.
    use std::os::unix::fs::PermissionsExt;
    let cfg_path = sb.project.join(".spar/runs").join(&id).join("config.json");
    let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "config.json mode {mode:o}");
    for args in [
        vec!["status", id.as_str(), "--json"],
        vec!["status", "--json"],
        vec!["brief", id.as_str(), "--json"],
    ] {
        let out = sb.spar().args(&args).output().unwrap();
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("pw@"),
            "{args:?} printed a captured value"
        );
    }
}

/// Review item 3: continuing a run through the plan it wrote (`implement --plan`) is a
/// re-entry like `--run`, so it re-applies the run's captured env too.
#[test]
fn implement_plan_continuation_reapplies_captured_env() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .env("HTTPS_PROXY", "http://proxy.invalid:3128")
        .args([
            "plan",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let id = json(&out)["run_id"].as_str().unwrap().to_string();
    sb.spar()
        .args(["approve", &id, "--json"])
        .assert()
        .success();
    let plan = sb
        .project
        .join(".spar/runs")
        .join(&id)
        .join("artifacts/plan.md");
    assert!(plan.is_file(), "dry-run plan artifact");
    let out = sb
        .spar()
        .args([
            "implement",
            "--plan",
            plan.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(json(&out)["run_id"], id.as_str(), "the same run continued");
    assert!(
        stderr.contains("[env]") && stderr.contains("HTTPS_PROXY"),
        "{stderr}"
    );
}

/// Review item 9: a reload keeps the creation shell's values unless asked to
/// re-capture them.
#[test]
fn reload_config_recaptures_env_only_when_asked() {
    let sb = Sandbox::new();
    std::fs::write(sb.project.join("spar.toml"), "[rounds]\nmax = 3\n").unwrap();
    let out = sb
        .spar()
        .env("HTTPS_PROXY", "http://old.invalid:1")
        .args([
            "plan",
            "-t",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let id = json(&out)["run_id"].as_str().unwrap().to_string();
    sb.spar()
        .args(["approve", &id, "--json"])
        .assert()
        .success();
    let captured = |sb: &Sandbox| run_config(sb, &id)["env"]["captured"]["HTTPS_PROXY"].clone();

    sb.spar()
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .env("HTTPS_PROXY", "http://new.invalid:2")
        .args([
            "implement",
            "--run",
            &id,
            "--reload-config",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    assert_eq!(captured(&sb), "http://old.invalid:1");

    sb.spar()
        .env("HTTPS_PROXY", "http://new.invalid:2")
        .args([
            "implement",
            "--run",
            &id,
            "--reload-config",
            "--recapture-env",
            "--max-rounds",
            "+1",
            "--dry-run",
            "--json",
        ])
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .assert()
        .code(2);
    assert_eq!(captured(&sb), "http://new.invalid:2");

    let out = sb
        .spar()
        .args(["implement", "--run", &id, "--recapture-env", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json(&out)["kind"], "validation");
}

/// Review item 5: a usage error is still reported as JSON when global flags (and
/// their values) come before the subcommand.
#[test]
fn json_usage_errors_see_past_global_flags() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .current_dir(&sb.home)
        .args([
            "--cwd",
            sb.project.to_str().unwrap(),
            "plan",
            "--json",
            "--bogus",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let v = json(&out);
    assert_eq!(v["kind"], "validation", "{v}");
    assert!(v["error"].as_str().unwrap().contains("--bogus"), "{v}");
}

/// Review item 6: a `--spec` launch refused at the pre-check leaves no brief copy.
#[test]
fn a_refused_spec_launch_leaves_no_brief() {
    let sb = Sandbox::new();
    let spec = sb.project.join("brief.md");
    std::fs::write(&spec, "# Refused\n\nbody\n").unwrap();
    let out = sb
        .spar()
        .args([
            "plan",
            "--spec",
            spec.to_str().unwrap(),
            "--providers",
            "cli:nosuch",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let briefs = sb.project.join(".spar/briefs");
    let left: Vec<_> = std::fs::read_dir(&briefs)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "brief left behind: {left:?}");
}

/// Review item 8: a project nested inside its repo compares `<prefix>/spar.toml`.
#[test]
fn config_drift_resolves_a_nested_projects_path() {
    let sb = Sandbox::new();
    let app = sb.project.join("app");
    std::fs::create_dir_all(app.join(".spar")).unwrap();
    std::fs::write(app.join("spar.toml"), "max_agents = 2\n").unwrap();
    sb.git(&["add", "app/spar.toml"]);
    sb.git(&["commit", "-q", "-m", "nested config"]);
    let out = sb
        .spar()
        .current_dir(&app)
        .args([
            "implement",
            "-t",
            "x",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    assert_eq!(v["config_source_drift"], false, "{v}");
    std::fs::write(app.join("spar.toml"), "max_agents = 3\n").unwrap();
    let out = sb
        .spar()
        .current_dir(&app)
        .args([
            "implement",
            "-t",
            "x",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(json(&out)["config_source_drift"], true);
}

#[test]
fn env_capture_of_a_secret_name_is_refused() {
    let sb = Sandbox::new();
    std::fs::write(
        sb.project.join("spar.toml"),
        "[env]\ncapture = [\"GITHUB_TOKEN\"]\n",
    )
    .unwrap();
    let out = sb
        .spar()
        .args([
            "plan",
            "-t",
            "x",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert!(v["error"].as_str().unwrap().contains("GITHUB_TOKEN"), "{v}");
    assert!(sb.runs().is_empty());
}
