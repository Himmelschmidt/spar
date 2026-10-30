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
