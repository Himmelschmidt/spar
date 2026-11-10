//! `bus send --urgent` (issue #231, part (b)): interrupting a mid-turn slot
//! kills its live turn and files the request for an in-place resume, while an
//! idle slot, a broadcast, an unknown slot, or a provider without resume gets
//! a normal delivery and the send says so.

mod common;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::{tempdir, TempDir};

struct Env {
    home: TempDir,
    bin: PathBuf,
}

fn env() -> Env {
    let home = tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let git = which_git();
    std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
    Env { home, bin }
}

fn which_git() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
}

fn spar(env: &Env, dir: &Path) -> assert_cmd::Command {
    let mut c = common::spar();
    c.current_dir(dir)
        .env("PATH", &env.bin)
        .env("HOME", env.home.path())
        .env("SPAR_HOME", env.home.path().join(".spar-home"))
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .env_remove("SPAR_DRY_RUN")
        .env_remove("SPAR_FORCE_REQUEST_CHANGES");
    c
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn json(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_slice(out.get_output().stdout.as_slice()).expect("json on stdout")
}

/// A planned, approved, dry-run-implemented run: its implementer slot is idle.
fn implemented_run(env: &Env, dir: &Path) -> String {
    std::fs::write(dir.join("spar.toml"), "[rounds]\nmax = 3\n").unwrap();
    let plan = spar(env, dir)
        .args([
            "plan",
            "--task",
            "add a hello function",
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json(&plan)["run_id"].as_str().unwrap().to_string();
    spar(env, dir)
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar(env, dir)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert();
    run_id
}

fn run_dir(dir: &Path) -> PathBuf {
    let runs = dir.join(".spar/runs");
    std::fs::read_dir(&runs)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("state.json").is_file())
        .expect("a run")
}

fn send_json(env: &Env, dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut all = vec!["bus", "send", "--json"];
    all.extend_from_slice(args);
    json(&spar(env, dir).args(&all).assert().success())
}

fn log_ids(env: &Env, dir: &Path, run: &str) -> Vec<String> {
    json(
        &spar(env, dir)
            .args(["bus", "log", "--run", run, "--json"])
            .assert()
            .success(),
    )
    .as_array()
    .unwrap()
    .iter()
    .map(|e| e["id"].as_str().unwrap().to_string())
    .collect()
}

/// Same field spar's own `pid_starttime` reads: `/proc/<pid>/stat` field 22
/// (starttime), skipping the parenthesised comm.
fn proc_starttime(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 1..];
    after.split_whitespace().nth(19).unwrap().parse().unwrap()
}

/// Mark `slot` running with a live, group-led `sleep` behind its `.pid`
/// marker, the way a real dispatch leaves it. Returns the child: the caller
/// owns reaping it.
fn stage_running_slot(runs: &Path, slot: &str, provider: Option<&str>) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let state_path = runs.join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let slots = state["slots"].as_array_mut().unwrap();
    let entry = slots
        .iter_mut()
        .find(|s| s["id"] == slot)
        .expect("the slot");
    entry["status"] = "running".into();
    if let Some(p) = provider {
        entry["provider"] = p.into();
    }
    std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    let mut child = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    // The token must prove identity the way the sender demands: pid plus the
    // starttime of the process that owns it, so a recycled pid never reads live.
    std::fs::write(
        runs.join("markers").join(format!("{slot}.pid")),
        format!("{pid}:{}", proc_starttime(pid)),
    )
    .unwrap();
    // The marker must name a process that is actually there when the send runs.
    assert!(child.try_wait().unwrap().is_none());
    child
}

fn reap(mut child: std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn urgent_markers(runs: &Path, slot: &str) -> Vec<PathBuf> {
    let prefix = format!("{slot}.urgent.");
    std::fs::read_dir(runs.join("markers"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                && p.extension().is_some_and(|x| x == "json")
        })
        .collect()
}

#[test]
fn urgent_needs_a_run() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let out = spar(&env, tmp.path())
        .args(["bus", "send", "--urgent", "--to", "impl", "-m", "hey"])
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr).to_string();
    assert!(err.contains("--urgent needs --run"), "{err}");
}

#[test]
fn urgent_to_an_idle_slot_delivers_normally() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());

    let sent = send_json(
        &env,
        tmp.path(),
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--urgent",
            "-m",
            "read this next",
        ],
    );
    assert_eq!(sent["urgent"]["status"], "idle");
    let id = sent["id"].as_str().unwrap().to_string();
    assert!(log_ids(&env, tmp.path(), &run).contains(&id));
}

#[test]
fn urgent_broadcast_and_unknown_slots_stay_normal() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());
    let dir = tmp.path();

    let b = send_json(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "broadcast",
            "--urgent",
            "-m",
            "all hands",
        ],
    );
    assert_eq!(b["urgent"]["status"], "needs_single_slot");

    let u = send_json(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "nope",
            "--urgent",
            "-m",
            "nobody home",
        ],
    );
    assert_eq!(u["urgent"]["status"], "unknown_slot");
}

#[test]
fn urgent_kills_a_running_slot_and_files_the_request() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());
    let runs = run_dir(tmp.path());
    let mut child = stage_running_slot(&runs, "impl", None);

    let sent = send_json(
        &env,
        tmp.path(),
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--urgent",
            "-m",
            "stop, wrong direction",
        ],
    );
    assert_eq!(sent["urgent"]["status"], "interrupted");
    let id = sent["id"].as_str().unwrap().to_string();
    assert!(log_ids(&env, tmp.path(), &run).contains(&id));

    // The sender's group kill lands before the send returns: the turn is dead.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the running turn was not killed");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    drop(child);

    let files = urgent_markers(&runs, "impl");
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(
        files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&id),
        "{files:?}"
    );
    let filed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(filed["body"], "stop, wrong direction");
    assert_eq!(filed["msg_id"], id);
}

#[test]
fn urgent_to_a_provider_without_resume_falls_back() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());
    let runs = run_dir(tmp.path());
    let mut child = stage_running_slot(&runs, "impl", Some("cli:agy"));

    let sent = send_json(
        &env,
        tmp.path(),
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--urgent",
            "-m",
            "read this next",
        ],
    );
    assert_eq!(sent["urgent"]["status"], "no_resume");
    let id = sent["id"].as_str().unwrap().to_string();
    assert!(log_ids(&env, tmp.path(), &run).contains(&id));

    // No kill, no filed request: the turn it targeted is still alive and the
    // message waits for the normal turn-boundary channels.
    assert!(child.try_wait().unwrap().is_none());
    assert!(urgent_markers(&runs, "impl").is_empty());
    reap(child);
}
