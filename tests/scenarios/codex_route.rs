//! Codex route unification: one resolver feeds the spawn and every quota and
//! rejection key, so a model-less `cli:codex` seat keys the bucket it bills.
//! All three drive the real binary with `--dry-run` (no provider CLI needed);
//! the recorded route in `state.json` is the seam under test.

mod common;
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

    fn git(&self, args: &[&str]) {
        let out = Command::new(which_git())
            .args(args)
            .current_dir(&self.project)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn spar(&self) -> assert_cmd::Command {
        let mut c = common::spar();
        c.current_dir(&self.project)
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("SPAR_HOME", self.home.join(".spar"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env_remove("SPAR_PROJECT_ROOT")
            .env_remove("SPAR_RUN_ID")
            .env_remove("SPAR_AGENT_ID")
            .env_remove("SPAR_DRY_RUN")
            .env_remove("SPAR_CODEX_PROFILE")
            .env_remove("SPAR_CODEX_MODEL");
        c
    }

    fn state(&self, id: &str) -> serde_json::Value {
        let p = self.project.join(".spar/runs").join(id).join("state.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    /// A dry-run plan against `cli:codex` only; returns the run id.
    fn plan_codex(&self, model: Option<&str>, profile: Option<&str>) -> String {
        let mut cmd = self.spar();
        if let Some(m) = model {
            cmd.env("SPAR_CODEX_MODEL", m);
        }
        if let Some(p) = profile {
            cmd.env("SPAR_CODEX_PROFILE", p);
        }
        let out = cmd
            .args([
                "plan",
                "-t",
                "add a hello function",
                "--providers",
                "cli:codex",
                "--dry-run",
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        json(&out)["run_id"].as_str().unwrap().to_string()
    }

    fn probe_provider(&self, id: Option<&str>) -> serde_json::Value {
        let mut args = vec!["provider", "probe", "cli:codex", "--dry-run", "--json"];
        if let Some(id) = id {
            args.extend(["--run", id]);
        }
        let out = self.spar().args(&args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
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

/// A `SPAR_CODEX_MODEL` seat keys the bucket it bills: the bare model bills
/// native, so the run-keyed probe reads `cli:codex@native` while the bare
/// probe under the default profile reads `cli:codex@openrouter`.
#[test]
fn spar_codex_model_seat_probes_the_bucket_it_bills() {
    let sb = Sandbox::new();
    let id = sb.plan_codex(Some("gpt-5"), None);
    let st = sb.state(&id);
    assert_eq!(st["codex_model"], "gpt-5");
    assert_eq!(st["codex_profile"], "muse");
    let r = sb.probe_provider(Some(&id));
    assert_eq!(r[0]["provider"], "cli:codex@native", "{r}");
    let bare = sb.probe_provider(None);
    assert_eq!(bare[0]["provider"], "cli:codex@openrouter", "{bare}");
}

/// `--reassign` refuses a paused recorded-route bucket: the run recorded the
/// emptied profile (native), so reassigning onto codex is refused even though
/// the ambient default profile would key the unpaused openrouter bucket.
#[test]
fn reassign_refuses_a_paused_recorded_route_bucket() {
    let sb = Sandbox::new();
    let id = sb.plan_codex(None, Some(""));
    assert_eq!(sb.state(&id)["codex_profile"], "");
    sb.spar()
        .args(["provider", "pause", "cli:codex@native"])
        .assert()
        .success();
    sb.spar()
        .args(["approve", &id, "--json"])
        .assert()
        .success();
    let out = sb
        .spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let v = json(&out);
    let err = v["error"].as_str().unwrap_or_default();
    assert!(err.contains("cli:codex@native"), "{v}");
    assert!(err.contains("paused"), "{v}");
}

/// A replan is a new dispatch: it re-stamps the codex route instead of
/// keeping the previous round's record.
#[test]
fn replan_restamps_the_codex_route() {
    let sb = Sandbox::new();
    let id = sb.plan_codex(None, Some("muse"));
    assert_eq!(sb.state(&id)["codex_profile"], "muse");
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args(["plan", "--run", &id, "--task", "narrow it to one module"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let st = sb.state(&id);
    assert_eq!(st["round"], 2);
    assert_eq!(st["codex_profile"], "", "replan re-stamps: {st}");
}
