//! Codex route unification: one resolver feeds the spawn and every quota and
//! rejection key, so a model-less `cli:codex` seat keys the bucket it bills.
//! All fourteen drive the real binary with `--dry-run` (no provider CLI needed);
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
            .env("SPAR_AUTH_RETRY_WAIT_SECS", "0")
            .env("FAKE_COUNT_DIR", &self.bin)
            .env_remove("SPAR_PROJECT_ROOT")
            .env_remove("SPAR_RUN_ID")
            .env_remove("SPAR_AGENT_ID")
            .env_remove("SPAR_DRY_RUN")
            .env_remove("SPAR_CODEX_PROFILE")
            .env_remove("SPAR_CODEX_MODEL");
        c
    }

    /// A fake provider CLI answering `--version` and counting dispatches.
    fn cli(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.bin.join(name);
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fake; exit 0; fi\necho x >> \"$FAKE_COUNT_DIR/{name}.count\"\n{body}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn count(&self, name: &str) -> usize {
        std::fs::read_to_string(self.bin.join(format!("{name}.count")))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// A real implement whose claude implementer fails fast, leaving an
    /// implementer seat on claude to reassign. Creation shell records the
    /// default profile.
    fn failed_claude_run(&self) -> String {
        self.cli("claude", "echo 'boom: the model crashed'\nexit 1");
        self.cli("codex", "exit 1");
        let out = self
            .spar()
            .args([
                "implement",
                "-t",
                "add a feature",
                "--providers",
                "cli:claude",
                "--fleet",
                "small",
                "--without",
                "suite",
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let mut runs: Vec<String> = std::fs::read_dir(self.project.join(".spar/runs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(self.count("claude"), 1, "implementer dispatched");
        runs.pop().unwrap()
    }

    /// The one run dir in the sandbox project.
    fn single_run(&self) -> String {
        let mut runs: Vec<String> = std::fs::read_dir(self.project.join(".spar/runs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs.pop().unwrap()
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

/// A bare probe with a model in the ambient environment keys the bucket the
/// spawn bills: a bare model runs under codex's native default, not the
/// default profile's OpenRouter bucket.
#[test]
fn bare_probe_with_a_model_in_env_keys_the_billed_bucket() {
    let sb = Sandbox::new();
    sb.plan_codex(None, None);
    let out = sb
        .spar()
        .env("SPAR_CODEX_MODEL", "gpt-5")
        .args(["provider", "probe", "cli:codex", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r[0]["provider"], "cli:codex@native", "{r}");
}

/// `--reassign` keys the route the dispatch will stamp, not the run's previous
/// record: the stamp overwrites the record from this process's environment, so
/// the check must key that same route. Pausing `native` while the shell records
/// the emptied profile refuses; a shell on the default profile proceeds even
/// though the previous record named `native`.
#[test]
fn reassign_keys_the_route_the_dispatch_will_stamp() {
    let sb = Sandbox::new();
    sb.spar()
        .args(["provider", "pause", "cli:codex@native"])
        .assert()
        .success();

    // Shell native, run recorded the default profile: the stamp overwrites the
    // record from this shell, so the dispatch would run native-paused.
    let id = sb.failed_claude_run();
    assert_eq!(sb.state(&id)["codex_profile"], "muse");
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
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
    assert_eq!(sb.count("codex"), 0, "refused before any dispatch");

    // Shell on the default profile: the dispatch stamps openrouter-unpaused,
    // so the pause another shell recorded must not refuse.
    let out = sb
        .spar()
        .args([
            "implement",
            "--run",
            &id,
            "--reassign",
            "implementer=cli:codex",
            "--dry-run",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("is paused"),
        "stale record must not refuse: {stdout} {out:?}"
    );
    assert!(
        stdout.contains("reassigned seat impl (cli:claude) to cli:codex"),
        "reassign proceeds: {stdout} {out:?}"
    );
}

/// A review dispatches seats too, so it stamps the codex route like every
/// other entry: the emptied profile records `native` on a dry review run.
#[test]
fn review_stamps_the_codex_route() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "review this",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    let id = v["run_id"].as_str().expect("review mints a run: {v}");
    assert_eq!(sb.state(id)["codex_profile"], "", "{v}");
}

/// An arena run dispatches seats without going through the plan or implement
/// entries, so it must stamp the codex route itself: a dry-run arena under an
/// emptied profile records `native`.
#[test]
fn arena_stamps_the_codex_route() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args([
            "run",
            "--workflow",
            "arena",
            "-t",
            "race two",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    let id = v["run_id"].as_str().expect("arena mints a run: {v}");
    assert_eq!(sb.state(id)["codex_profile"], "", "{v}");
}

/// A `--run` probe bills the keyed bucket: a run stamped model-less under an
/// emptied profile keys `native`, and the spawn omits `-p` even when this
/// shell's ambient profile would select one.
#[test]
fn probe_run_spawns_the_recorded_route() {
    let sb = Sandbox::new();
    let id = sb.plan_codex(None, Some(""));
    sb.cli("codex", "exit 1");
    let out = sb
        .spar()
        .args([
            "provider",
            "probe",
            "cli:codex",
            "--dry-run",
            "--json",
            "--run",
            &id,
        ])
        .output()
        .unwrap();
    let r = json(&out);
    let rows = r.as_array().expect("probe returns rows: {r}");
    let row = rows
        .iter()
        .find(|r| r["provider"] == "cli:codex@native")
        .expect("native key: {r}");
    let detail = row["detail"].as_str().unwrap_or("");
    assert!(
        !detail.contains(" -p "),
        "spawn renders the record, not ambient: {detail}"
    );
}

/// A recorded codex model stays inside the codex adapter: probing another
/// provider from the same run never renders it into that provider's spawn.
#[test]
fn probe_run_keeps_a_recorded_codex_model_off_other_adapters() {
    let sb = Sandbox::new();
    let id = sb.plan_codex(Some("gpt-5"), None);
    sb.cli("claude", "exit 1");
    sb.cli("codex", "exit 1");
    let out = sb
        .spar()
        .args([
            "provider",
            "probe",
            "cli:claude",
            "--dry-run",
            "--json",
            "--run",
            &id,
        ])
        .output()
        .unwrap();
    let r = json(&out);
    let rows = r.as_array().expect("probe returns rows: {r}");
    let row = rows
        .iter()
        .find(|r| r["provider"] == "cli:claude")
        .expect("claude row: {r}");
    let detail = row["detail"].as_str().unwrap_or("");
    assert!(!detail.contains("gpt-5"), "codex model leaks: {detail}");
    let out = sb
        .spar()
        .args([
            "provider",
            "probe",
            "cli:codex",
            "--dry-run",
            "--json",
            "--run",
            &id,
        ])
        .output()
        .unwrap();
    let r = json(&out);
    let rows = r.as_array().expect("probe returns rows: {r}");
    let row = rows
        .iter()
        .find(|r| r["provider"] == "cli:codex@native")
        .expect("native key: {r}");
    let detail = row["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("-m gpt-5"),
        "codex replays the recorded model: {detail}"
    );
}

/// Roles and peer runs dispatch seats without going through the plan or
/// implement entries, so they stamp the codex route themselves.
#[test]
fn roles_stamps_the_codex_route() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args([
            "run",
            "--workflow",
            "roles",
            "-t",
            "split this",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    let id = v["run_id"].as_str().expect("roles mints a run: {v}");
    assert_eq!(sb.state(id)["codex_profile"], "", "{v}");
}

#[test]
fn peer_stamps_the_codex_route() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args([
            "run",
            "--workflow",
            "peer",
            "-t",
            "pair on this",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    let id = v["run_id"].as_str().expect("peer mints a run: {v}");
    assert_eq!(sb.state(id)["codex_profile"], "", "{v}");
}

/// `reconcile` starts a new dispatch on the arena run, so it re-stamps the
/// codex route from its own environment instead of keeping the arena record.
#[test]
fn reconcile_restamps_the_codex_route() {
    let sb = Sandbox::new();
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args([
            "run",
            "--workflow",
            "arena",
            "-t",
            "race two",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    let id = v["run_id"].as_str().expect("arena mints a run: {v}");
    assert_eq!(sb.state(id)["codex_profile"], "", "{v}");
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "muse")
        .args(["reconcile", id, "--json"])
        .output()
        .unwrap();
    let v = json(&out);
    assert_eq!(sb.state(id)["codex_profile"], "muse", "{v}");
}

/// `amend` dispatches the test author, so it re-stamps the codex route from
/// its own environment instead of keeping the plan record.
#[test]
fn amend_restamps_the_codex_route() {
    let sb = Sandbox::new();
    let id = sb.plan_codex(None, Some(""));
    assert_eq!(sb.state(&id)["codex_profile"], "");
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "muse")
        .args([
            "amend",
            "--run",
            &id,
            "--contract",
            "add a portability criterion",
            "--json",
        ])
        .output()
        .unwrap();
    let v = json(&out);
    assert_eq!(sb.state(&id)["codex_profile"], "muse", "{v}");
}

/// An `implement --run` resume starts a new dispatch, so it re-stamps the
/// codex route: a live run that fails fast under an emptied profile resumes
/// dry under the default profile and records it.
#[test]
fn implement_resume_restamps_the_codex_route() {
    let sb = Sandbox::new();
    sb.cli("codex", "exit 1");
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "")
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:codex",
            "--fleet",
            "small",
            "--without",
            "suite",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let id = sb.single_run();
    assert_eq!(sb.state(&id)["codex_profile"], "");
    let out = sb
        .spar()
        .env("SPAR_CODEX_PROFILE", "muse")
        .args(["implement", "--run", &id, "--dry-run", "--json"])
        .output()
        .unwrap();
    let v = json(&out);
    assert_eq!(sb.state(&id)["codex_profile"], "muse", "{v}");
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

/// A paused `cli:codex@native` parks a `SPAR_CODEX_MODEL` seat at the run-start
/// gate: the model routes the model-less seat to `native`, so in-run
/// eligibility refuses before any dispatch. Without the model the same seat
/// keys `openrouter` and dispatches past the gate.
#[test]
fn paused_native_parks_a_model_seat_before_dispatch() {
    let sb = Sandbox::new();
    sb.cli("codex", "exit 1");
    sb.spar()
        .args(["provider", "pause", "cli:codex@native"])
        .assert()
        .success();

    let out = sb
        .spar()
        .env("SPAR_CODEX_MODEL", "gpt-5")
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:codex",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cli:codex@native"), "{stderr}");
    let id = only_run_id(&sb.project);
    assert_eq!(sb.state(&id)["phase"], "quota", "{stderr}");
    assert_eq!(sb.count("codex"), 0, "refused before any dispatch");

    let out = sb
        .spar()
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:codex",
        ])
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(4), "{out:?}");
    assert!(
        sb.count("codex") >= 1,
        "model-less seat dispatches: {out:?}"
    );
}

fn only_run_id(proj: &Path) -> String {
    std::fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}
