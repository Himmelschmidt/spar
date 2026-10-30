//! Bus receipts and send options (O134, O135): `delivered_at` / `read_at` per recipient
//! in `bus log --json`, `--to impl`, `--requires-ack`, `--persist` into the implementer's
//! carry-forward, `--supersedes` and `--expires-after-round`. Dry-run backend; PATH holds
//! only git and HOME is a temp dir, so no provider CLI runs.
use assert_cmd::cargo::cargo_bin_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    let mut c = cargo_bin_cmd!("spar");
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

fn send(env: &Env, dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut all = vec!["bus", "send", "--json"];
    all.extend_from_slice(args);
    json(&spar(env, dir).args(&all).assert().success())
}

fn log(env: &Env, dir: &Path, run: &str) -> Vec<serde_json::Value> {
    json(
        &spar(env, dir)
            .args(["bus", "log", "--run", run, "--json"])
            .assert()
            .success(),
    )
    .as_array()
    .unwrap()
    .clone()
}

fn entry(entries: &[serde_json::Value], id: &str) -> serde_json::Value {
    entries
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("{id} in {entries:?}"))
        .clone()
}

#[test]
fn receipts_ack_and_supersede_through_the_cli() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());
    let dir = tmp.path();

    let first = send(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--requires-ack",
            "-m",
            "rebase on main",
        ],
    );
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["to"], "impl", "{first}");
    assert_eq!(first["requires_ack"], true);
    let first_id = first["id"].as_str().unwrap().to_string();
    assert!(dir
        .join(".spar/bus/pending_ack")
        .join(format!("{first_id}.json"))
        .is_file());

    let e = entry(&log(&env, dir, &run), &first_id);
    assert_eq!(e["schema_version"], 1);
    let r = &e["receipts"][0];
    assert_eq!(r["recipient"], format!("{run}:impl"));
    assert!(r["queued_at"].is_string(), "{e}");
    assert!(r["delivered_at"].is_null() && r["read_at"].is_null(), "{e}");

    spar(&env, dir)
        .args(["bus", "inbox", "impl", "--run", &run, "--claim", "--json"])
        .assert()
        .success();
    let e = entry(&log(&env, dir, &run), &first_id);
    assert!(e["receipts"][0]["delivered_at"].is_string(), "{e}");
    assert!(
        e["receipts"][0]["read_at"].is_string(),
        "the agent claimed it itself: {e}"
    );
    spar(&env, dir)
        .args(["bus", "ack", &first_id, "--from", "impl", "--run", &run])
        .assert()
        .success();
    assert!(!dir
        .join(".spar/bus/pending_ack")
        .join(format!("{first_id}.json"))
        .exists());

    let stale = send(
        &env,
        dir,
        &["--run", &run, "--to", "impl", "-m", "use approach A"],
    );
    let stale_id = stale["id"].as_str().unwrap().to_string();
    let err = spar(&env, dir)
        .args([
            "bus",
            "send",
            "--run",
            &run,
            "--to",
            "impl",
            "--from",
            "review-0-cli-grok",
            "--supersedes",
            &stale_id,
            "-m",
            "no",
        ])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&err.get_output().stderr).contains("was sent by human"));
    let newer = send(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--supersedes",
            &stale_id,
            "-m",
            "use approach B",
        ],
    );
    assert_eq!(newer["refs"]["supersedes"], stale_id.as_str());
    assert_eq!(
        newer["withdrawal"]["withdrawn_from"],
        serde_json::json!([format!("{run}:impl")])
    );
    let inbox = json(
        &spar(&env, dir)
            .args(["bus", "inbox", "impl", "--run", &run, "--json"])
            .assert()
            .success(),
    );
    let bodies: Vec<&str> = inbox
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, vec!["use approach B"]);
    let entries = log(&env, dir, &run);
    let e = entry(&entries, &stale_id);
    assert_eq!(e["superseded_by"], newer["id"]);
    assert!(e["receipts"][0]["withdrawn_at"].is_string(), "{e}");

    let human = spar(&env, dir)
        .args(["bus", "log", "--run", &run])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&human.get_output().stdout).to_string();
    assert!(text.contains("superseded by"), "{text}");
    assert!(text.contains("delivered 1/1, read 1/1"), "{text}");
}

#[test]
fn persist_reaches_the_next_carry_forward_and_expiry_withdraws() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run = implemented_run(&env, tmp.path());
    let dir = tmp.path();

    let err = spar(&env, dir)
        .args(["bus", "send", "--persist", "--to", "impl", "-m", "x"])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&err.get_output().stderr).contains("need --run"));
    let err = spar(&env, dir)
        .args([
            "bus",
            "send",
            "--run",
            &run,
            "--persist",
            "--to",
            "review-0-cli-grok",
            "-m",
            "x",
        ])
        .assert()
        .failure();
    assert!(String::from_utf8_lossy(&err.get_output().stderr).contains("implementer"));

    let ruling = send(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--persist",
            "-m",
            "OPERATOR RULING: keep the API",
        ],
    );
    assert_eq!(ruling["persist"], true);
    let status = json(
        &spar(&env, dir)
            .args(["status", &run, "--json"])
            .assert()
            .success(),
    );
    let round = status["round"].as_u64().unwrap();
    let expiring = send(
        &env,
        dir,
        &[
            "--run",
            &run,
            "--to",
            "impl",
            "--expires-after-round",
            "-m",
            "this round only",
        ],
    );
    assert_eq!(expiring["expires_after_round"], round, "{expiring}");

    let _ = spar(&env, dir)
        .args([
            "implement",
            "--run",
            &run,
            "--providers",
            "cli:claude,cli:grok",
            "--dry-run",
            "-t",
            "another pass",
        ])
        .assert();
    let prompt =
        std::fs::read_to_string(dir.join(".spar/runs").join(&run).join("prompt-impl.md")).unwrap();
    assert!(prompt.contains("OPERATOR RULING: keep the API"), "{prompt}");
    assert!(prompt.contains("Operator messages"), "{prompt}");

    let entries = log(&env, dir, &run);
    let r = &entry(&entries, ruling["id"].as_str().unwrap())["receipts"];
    assert!(
        r.as_array()
            .unwrap()
            .iter()
            .any(|x| x["read_at"].is_string()),
        "in the dispatch prompt: {r}"
    );
    let e = entry(&entries, expiring["id"].as_str().unwrap());
    assert_eq!(e["expired"], true, "{e}");
    let inbox = json(
        &spar(&env, dir)
            .args(["bus", "inbox", "impl", "--run", &run, "--json"])
            .assert()
            .success(),
    );
    assert!(
        inbox
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"] != expiring["id"]),
        "{inbox}"
    );
}
