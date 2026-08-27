//! An abandoned run is one still in flight with no live orchestrator: whoever was
//! driving it died, so no phase change is ever coming, while its slots keep running.
//! `wait` must say so instead of blocking, `status` must name the orphans, and
//! `stop --abandoned` must reap them.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

fn spar_cmd() -> assert_cmd::Command {
    let mut c = cargo_bin_cmd!("spar");
    c.env("SPAR_HOME", spar_home_dir());
    // Also isolate the *config* dir. SPAR_HOME only moves the registry; without this
    // the spawned binary still layers the developer's ~/.config/spar/config.toml under
    // the test's project, so an ordinary local setting fails scenarios that never
    // mention it. (XDG applies on Linux, where the suite runs.)
    c.env("XDG_CONFIG_HOME", spar_home_dir());
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn init_repo(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
}

/// A run parked mid-flight with nobody holding its lock — exactly the state a run is
/// left in when the process driving it is killed.
fn abandoned_run(dir: &std::path::Path, run_id: &str) {
    let run = dir.join(".spar/runs").join(run_id);
    std::fs::create_dir_all(run.join("logs")).unwrap();
    let now = chrono::Utc::now().to_rfc3339();
    std::fs::write(
        run.join("state.json"),
        serde_json::json!({
            "id": run_id,
            "workflow": "loop",
            "phase": "wait_completion",
            "created_at": now,
            "updated_at": now,
            "project_root": dir,
            "slots": [],
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn wait_returns_stuck_instead_of_blocking_on_an_abandoned_run() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    abandoned_run(dir, "aband01");

    // A generous timeout: the point is that `wait` returns on the abandonment verdict,
    // long before it would have expired. Without this it blocks for the full 2h default
    // on a run that can never advance.
    let started = std::time::Instant::now();
    let out = spar_cmd()
        .current_dir(dir)
        .env("SPAR_ABANDON_GRACE_SECS", "1")
        .args(["wait", "aband01", "--timeout", "300s", "--json"])
        .assert()
        .code(3);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "wait must return on abandonment, not sit out its timeout"
    );
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
            .expect("json");
    let err = v["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("abandoned"),
        "the verdict must say why, got {err:?}"
    );
}

#[test]
fn wait_holds_the_grace_window_before_calling_a_run_abandoned() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    abandoned_run(dir, "aband03");

    // A just-detached orchestrator has not acquired the run lock yet, so a run reads as
    // abandoned for a moment on every launch. Calling it on the first poll would break
    // the documented `--detach` + `wait` flow outright.
    let started = std::time::Instant::now();
    spar_cmd()
        .current_dir(dir)
        .env("SPAR_ABANDON_GRACE_SECS", "30")
        .args(["wait", "aband03", "--timeout", "2s", "--json"])
        .assert()
        .code(3);
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "must have waited out its own timeout, not the abandonment verdict"
    );
}

#[test]
fn status_reports_abandonment_and_stop_abandoned_sweeps_it() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);
    abandoned_run(dir, "aband02");

    let status = spar_cmd()
        .current_dir(dir)
        .args(["status", "aband02", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        status.get_output().stdout.as_slice(),
    ))
    .expect("json");
    assert_eq!(v["abandoned"], true);
    assert!(
        v["orphan_pids"].is_array(),
        "status must always carry orphan_pids so a driver can assert on it"
    );

    let swept = spar_cmd()
        .current_dir(dir)
        .args(["stop", "--abandoned", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        swept.get_output().stdout.as_slice(),
    ))
    .expect("json");
    let ids: Vec<&str> = v["swept"]
        .as_array()
        .expect("swept")
        .iter()
        .filter_map(|r| r["run_id"].as_str())
        .collect();
    assert!(ids.contains(&"aband02"), "sweep must find it, got {ids:?}");

    // Swept runs are parked at stopped, so they stop reading as abandoned.
    let after = spar_cmd()
        .current_dir(dir)
        .args(["status", "aband02", "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        after.get_output().stdout.as_slice(),
    ))
    .expect("json");
    assert_eq!(v["phase"], "stopped");
    assert_eq!(v["abandoned"], false);
}

#[test]
fn a_run_at_rest_is_never_swept() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    // A plan waiting at its approval gate is *meant* to have no orchestrator.
    let plan = spar_cmd()
        .current_dir(dir)
        .args([
            "plan",
            "--task",
            "task A",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        plan.get_output().stdout.as_slice(),
    ))
    .expect("json");
    let run_id = v["run_id"].as_str().expect("run_id").to_string();

    spar_cmd()
        .current_dir(dir)
        .args(["stop", "--abandoned", "--json"])
        .assert()
        .success();

    let after = spar_cmd()
        .current_dir(dir)
        .args(["status", &run_id, "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        after.get_output().stdout.as_slice(),
    ))
    .expect("json");
    assert_eq!(
        v["phase"], "awaiting_plan_approval",
        "a gate is at rest, not abandoned — sweeping it would discard a live decision"
    );
}

/// A real orchestrator, real spawned slots, and the uncatchable kill.
///
/// Everything above works on hand-written state; this drives the production path — a
/// live run with a fake provider on PATH, `SIGKILL`ed so no shutdown handler can help,
/// which is exactly how a driver's command timeout orphans agents that keep burning
/// tokens.
#[test]
fn sigkilled_orchestrator_leaves_orphans_that_status_names_and_sweep_reaps() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);

    // A provider CLI that announces its pid and then sits there, like a working agent.
    let fake = bin.join("claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho $$ >> \"$SLOT_PIDS_FILE\"\nsleep 300\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let pid_file = dir.join("slot_pids");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut child = Command::new(assert_cmd::cargo::cargo_bin("spar"))
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "x",
            "--providers",
            "cli:claude",
        ])
        .current_dir(&proj)
        .env("PATH", &path_env)
        .env("SPAR_HOME", spar_home_dir())
        .env("SLOT_PIDS_FILE", &pid_file)
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn spar");

    let slot_pid = wait_for_pid(&pid_file).expect("slot never started");
    // Uncatchable: no handler runs, so the slot is orphaned by construction.
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        pid_alive(slot_pid),
        "the orphan should outlive its orchestrator"
    );

    let run_id = only_run_id(&proj);
    let status = spar_cmd()
        .current_dir(&proj)
        .args(["status", &run_id, "--json"])
        .assert()
        .success();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(
        status.get_output().stdout.as_slice(),
    ))
    .expect("json");
    assert_eq!(v["abandoned"], true);
    let orphans: Vec<u64> = v["orphan_pids"]
        .as_array()
        .expect("orphan_pids")
        .iter()
        .filter_map(|p| p.as_u64())
        .collect();
    assert!(
        orphans.contains(&(slot_pid as u64)),
        "status must name the process still burning tokens, got {orphans:?}"
    );

    spar_cmd()
        .current_dir(&proj)
        .args(["stop", "--abandoned", "--json"])
        .assert()
        .success();

    let reaped = (0..40).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(250));
        !pid_alive(slot_pid)
    });
    // Best effort: never leave a stray sleeper behind if the assert below fails.
    if !reaped {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(slot_pid.to_string())
            .status();
    }
    assert!(reaped, "sweep must reap the orphan");

    // Read the file, not `spar status`: display reconciles in memory, which would mask
    // exactly the bug: a run left on disk claiming slots that no process has been
    // behind since the orchestrator died. The sweep is the last thing that touches this
    // run, so if it does not settle the record, nothing ever will.
    let on_disk: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .expect("state.json");
    let stuck: Vec<&serde_json::Value> = on_disk["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .filter(|s| s["status"] == "running")
        .collect();
    assert!(
        stuck.is_empty(),
        "no slot may still claim to be running after the sweep: {stuck:?}"
    );
}

fn wait_for_pid(path: &std::path::Path) -> Option<u32> {
    for _ in 0..80 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(first) = text.lines().next() {
                if let Ok(pid) = first.trim().parse() {
                    return Some(pid);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn only_run_id(proj: &std::path::Path) -> String {
    let runs = proj.join(".spar/runs");
    std::fs::read_dir(&runs)
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}

#[test]
fn stop_rejects_ambiguous_invocations() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    init_repo(dir);

    spar_cmd().current_dir(dir).args(["stop"]).assert().code(1);
    spar_cmd()
        .current_dir(dir)
        .args(["stop", "someid", "--abandoned"])
        .assert()
        .code(1);
}
