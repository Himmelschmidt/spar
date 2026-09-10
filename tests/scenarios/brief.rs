//! `plan --spec` / stdin intake and `spar brief <id>`. The slug/never-overwrite logic
//! itself is unit-tested in `src/brief.rs`; these cover the CLI surface: writing
//! `.spar/briefs/<slug>.md`, recording it on the run, and `spar brief` re-hydrating a
//! fresh session without ever writing.
use assert_cmd::cargo::cargo_bin_cmd;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-brief-home-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    })
    .clone()
}

fn spar_cmd() -> assert_cmd::Command {
    let mut c = cargo_bin_cmd!("spar");
    c.env("SPAR_HOME", spar_home_dir());
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn init_repo(dir: &std::path::Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn json_of(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_str(&String::from_utf8_lossy(out.get_output().stdout.as_slice()))
        .expect("json")
}

#[test]
fn spec_file_writes_a_brief_and_records_it_on_the_run() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let spec = tmp.path().join("spec.md");
    std::fs::write(&spec, "# Durable Run Ownership\n\ndo the thing\n").unwrap();

    let out = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--spec",
            spec.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let v = json_of(&out);
    let run_id = v["run_id"].as_str().unwrap();

    let brief_path = proj.join(".spar/briefs/durable-run-ownership.md");
    assert!(brief_path.is_file(), "brief must be written to disk");
    let body = std::fs::read_to_string(&brief_path).unwrap();
    assert!(body.contains("do the thing"));

    let state_path = proj.join(".spar/runs").join(run_id).join("state.json");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(
        state["brief"].as_str().unwrap(),
        brief_path.to_str().unwrap(),
        "the run must record the brief's own path"
    );
    assert!(state["task"].as_str().unwrap().contains("do the thing"));
}

#[test]
fn stdin_intake_reads_the_spec_from_stdin() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let out = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--spec",
            "-",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .write_stdin("# From Stdin\n\nread me from stdin\n")
        .assert()
        .code(2);
    let v = json_of(&out);
    assert!(v["task"].as_str().unwrap().contains("read me from stdin"));
    assert!(proj.join(".spar/briefs/from-stdin.md").is_file());
}

#[test]
fn task_and_spec_together_is_an_error_naming_both() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let spec = tmp.path().join("spec.md");
    std::fs::write(&spec, "# X\n\nbody\n").unwrap();

    spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "-t",
            "hello",
            "--spec",
            spec.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--spec"));
}

#[test]
fn neither_task_nor_spec_is_an_error() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    spar_cmd()
        .current_dir(&proj)
        .args(["plan", "--providers", "cli:claude", "--dry-run"])
        .assert()
        .failure();
}

#[test]
fn brief_command_names_the_next_command_and_writes_nothing() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let plan = spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "-t",
            "hello",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let run_id = json_of(&plan)["run_id"].as_str().unwrap().to_string();

    let state_path = proj.join(".spar/runs").join(&run_id).join("state.json");
    let before = std::fs::read_to_string(&state_path).unwrap();
    let before_mtime = std::fs::metadata(&state_path).unwrap().modified().unwrap();

    let out = spar_cmd()
        .current_dir(&proj)
        .args(["brief", &run_id, "--json"])
        .assert()
        .success();
    let v = json_of(&out);
    assert_eq!(v["next_command"], format!("spar approve {run_id}"));

    let after = std::fs::read_to_string(&state_path).unwrap();
    let after_mtime = std::fs::metadata(&state_path).unwrap().modified().unwrap();
    assert_eq!(before, after, "spar brief must never write state.json");
    assert_eq!(before_mtime, after_mtime);
}

#[test]
fn two_briefs_with_the_same_title_never_collide() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);

    let spec = tmp.path().join("spec.md");
    std::fs::write(&spec, "# Same Title\n\nfirst\n").unwrap();
    spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--spec",
            spec.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);

    std::fs::write(&spec, "# Same Title\n\nsecond\n").unwrap();
    spar_cmd()
        .current_dir(&proj)
        .args([
            "plan",
            "--spec",
            spec.to_str().unwrap(),
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);

    assert_eq!(
        std::fs::read_to_string(proj.join(".spar/briefs/same-title.md")).unwrap(),
        "# Same Title\n\nfirst\n"
    );
    assert_eq!(
        std::fs::read_to_string(proj.join(".spar/briefs/same-title-2.md")).unwrap(),
        "# Same Title\n\nsecond\n"
    );
}
