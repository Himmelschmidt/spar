//! The launch surface a fleet launcher drives: pre-checks, refusals as data, round
//! extensions, slot identity and the config a run is created under.
//!
//! Every spawn here runs with `PATH` holding only `git`, and `HOME` /
//! `XDG_CONFIG_HOME` inside the test's own tempdir, so no provider CLI and no user
//! config on the host can ever be reached, whatever a test gets wrong.
use assert_cmd::cargo::cargo_bin_cmd;
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

    fn spar(&self) -> assert_cmd::Command {
        let mut c = cargo_bin_cmd!("spar");
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

/// #165: an extension sent while an orchestrator holds the run is queued for it
/// instead of failing busy, and a later re-entry still honours it.
#[test]
fn max_rounds_against_a_live_orchestrator_is_queued() {
    use std::io::Write;
    let sb = Sandbox::new();
    let id = sb.run_at_ceiling(3);
    let round = sb.state(&id)["round"].as_u64().unwrap();
    let lock_path = sb
        .project
        .join(".spar/runs")
        .join(&id)
        .join("orchestrator.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    lock.try_lock().unwrap();
    (&lock)
        .write_all(std::process::id().to_string().as_bytes())
        .unwrap();

    let out = sb
        .implement(&id, &["--max-rounds", "+2"])
        .code(0)
        .get_output()
        .clone();
    let v = json(&out);
    assert_eq!(v["queued_max_rounds"].as_u64(), Some(round + 2), "{v}");
    assert_eq!(sb.state(&id)["max_rounds"].as_u64(), Some(3));
    let marker = sb
        .project
        .join(".spar/runs")
        .join(&id)
        .join("markers/max-rounds.queued");
    assert!(marker.is_file(), "queued marker missing");

    // Anything beyond the extension still needs the lock.
    sb.implement(&id, &["--max-rounds", "+2", "-t", "also this"])
        .code(1);

    drop(lock);
    sb.implement(&id, &[]).code(2);
    let st = sb.state(&id);
    assert_eq!(st["max_rounds"].as_u64(), Some(round + 2), "{st}");
    assert_eq!(st["round"].as_u64(), Some(round + 2), "{st}");
    assert!(!marker.exists(), "the queued extension is consumed once");
}
