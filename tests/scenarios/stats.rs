//! `spar stats` (O131): aggregates from `.spar/history.jsonl` and live run state. A
//! dry-run run is counted live while in flight and from history once purged, with the
//! same numbers. PATH holds only git and HOME is a temp dir, so no provider CLI runs.
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

fn stats(env: &Env, dir: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["stats", "--json"];
    args.extend_from_slice(extra);
    json(&spar(env, dir).args(&args).assert().success())
}

#[test]
fn a_run_counts_live_then_from_history_with_the_same_numbers() {
    let env = env();
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    let run_id = implemented_run(&env, tmp.path());

    let status = json(
        &spar(&env, tmp.path())
            .args(["status", &run_id, "--json"])
            .assert()
            .success(),
    );
    assert_eq!(status["cost"]["schema_version"], 1, "{status}");
    assert_eq!(status["cost"]["total"]["cost_reported"], "none");
    assert!(status["cost"]["by_provider"].as_array().is_some());
    assert!(status["cost"]["by_slot"].as_array().is_some());

    let hidden = stats(&env, tmp.path(), &[]);
    assert_eq!(hidden["schema_version"], 1);
    assert_eq!(hidden["runs"], 0, "{hidden}");
    assert_eq!(hidden["excluded_dry_runs"], 1, "{hidden}");

    let live = stats(&env, tmp.path(), &["--include-dry-run", "--by", "role"]);
    assert_eq!(live["runs_live"], 1, "{live}");
    assert_eq!(live["by"], "role");
    let groups = live["groups"].as_array().unwrap();
    let implementer = groups
        .iter()
        .find(|g| g["key"] == "implementer")
        .unwrap_or_else(|| panic!("{live}"));
    assert_eq!(implementer["runs"], 1);
    assert_eq!(implementer["outcomes"]["ship_gate"], 1, "{live}");
    assert!(
        implementer["wall_time"]["rounds_timed"].as_u64().unwrap() >= 1,
        "phase events carry the round: {live}"
    );
    let reviewer = groups.iter().find(|g| g["key"] == "reviewer").unwrap();
    assert_eq!(reviewer["reviews"]["reviews"], 0, "{live}");
    assert!(
        reviewer["reviews"]["synthetic_reviews"].as_u64().unwrap() >= 1,
        "dry-run reviews are spar's: {live}"
    );

    spar(&env, tmp.path())
        .args(["cleanup", &run_id, "--purge-artifacts", "--force", "--json"])
        .assert()
        .success();
    let purged = stats(&env, tmp.path(), &["--include-dry-run", "--by", "role"]);
    assert_eq!(purged["runs_live"], 0, "{purged}");
    assert_eq!(purged["runs_from_history"], 1, "{purged}");
    assert_eq!(purged["groups"], live["groups"]);
}

#[test]
fn since_filters_and_all_projects_reads_the_registry() {
    let env = env();
    let a = tempdir().unwrap();
    let b = tempdir().unwrap();
    init_repo(a.path());
    init_repo(b.path());
    implemented_run(&env, a.path());
    implemented_run(&env, b.path());

    let one = stats(&env, a.path(), &["--include-dry-run", "--by", "project"]);
    assert_eq!(one["runs"], 1, "{one}");
    let registry = env.home.path().join(".spar-home");
    std::fs::create_dir_all(&registry).unwrap();
    let projects: Vec<serde_json::Value> = [a.path(), b.path()]
        .iter()
        .map(|p| serde_json::json!({"root": p, "last_seen": "2026-10-30T00:00:00Z"}))
        .collect();
    std::fs::write(
        registry.join("registry.json"),
        serde_json::json!({"version": 1, "projects": projects}).to_string(),
    )
    .unwrap();
    let all = stats(
        &env,
        a.path(),
        &["--include-dry-run", "--by", "project", "--all-projects"],
    );
    assert_eq!(all["runs"], 2, "{all}");
    assert_eq!(all["groups"].as_array().unwrap().len(), 2, "{all}");

    let hist = a.path().join(".spar/history.jsonl");
    let row = serde_json::json!({
        "schema_version": 1, "written_at": "2020-01-02T00:00:00Z", "trigger": "terminal",
        "id": "old00001", "task": null, "workflow": "loop", "dry_run": false, "phase": "done",
        "roles": [], "rounds": 1, "max_rounds": 5, "review_rounds": [],
        "findings": {"critical": 0, "major": 0, "minor": 0, "synthetic": 0},
        "billed_tokens": 0, "cost_usd": null, "pr_url": null,
        "created_at": "2020-01-01T00:00:00Z", "ended_at": "2020-01-02T00:00:00Z"
    });
    std::fs::write(&hist, format!("{row}\n")).unwrap();
    let everything = stats(&env, a.path(), &["--include-dry-run"]);
    assert_eq!(everything["runs"], 2, "{everything}");
    let recent = stats(&env, a.path(), &["--include-dry-run", "--since", "7d"]);
    assert_eq!(recent["runs"], 1, "{recent}");
    assert!(recent["since"].as_str().is_some());

    let human = spar(&env, a.path())
        .args(["stats", "--include-dry-run"])
        .assert()
        .success();
    let text = String::from_utf8_lossy(&human.get_output().stdout).to_string();
    assert!(text.contains("2 runs"), "{text}");
    assert!(text.contains("cost not reported"), "{text}");
}
