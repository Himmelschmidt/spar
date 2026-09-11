//! Acceptance scenarios for the disposable resident orchestrator.
//!
//! These stay at the public CLI/source boundary because the conversation is a TUI
//! surface but its two durable handoffs are observable: a pre-intaken brief is passed
//! to `plan` unchanged, and an agent reply is a fully scoped bus event. The UI-specific
//! record, compose, and navigation checks live beside the private TUI types.
use assert_cmd::cargo::cargo_bin_cmd;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-orchestrator-home-{}", std::process::id()));
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
    c.env_remove("SPAR_CONVERSATION_ID");
    c
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
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
    serde_json::from_slice(&out.get_output().stdout).expect("command JSON")
}

/// AC-1: a valid already-intaken brief launches without being copied again, and the
/// new run records that exact durable path.
#[test]
fn plan_brief_uses_the_existing_brief_once_and_verbatim() {
    let tmp = tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_repo(&project);

    let brief_dir = project.join(".spar/briefs");
    std::fs::create_dir_all(&brief_dir).unwrap();
    let brief = brief_dir.join("chat-intake.md");
    let body = "# Chat intake\n\nKeep this body byte-for-byte.\n";
    std::fs::write(&brief, body).unwrap();

    let launched = spar_cmd()
        .current_dir(&project)
        .args([
            "plan",
            "--brief",
            ".spar/briefs/chat-intake.md",
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("\"run_id\""));
    let run_id = json(&launched)["run_id"].as_str().unwrap().to_string();

    assert_eq!(std::fs::read_to_string(&brief).unwrap(), body);
    assert!(
        !brief_dir.join("chat-intake-2.md").exists(),
        "plan --brief must consume the intaken brief, not intake a duplicate"
    );
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.join(".spar/runs").join(run_id).join("state.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(state["brief"], ".spar/briefs/chat-intake.md");
    assert_eq!(state["task"], body);
}

/// AC-2: the pre-intaken brief contract is exclusive with both existing task inputs.
#[test]
fn plan_brief_is_exclusive_with_task_and_spec() {
    let tmp = tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_repo(&project);
    let brief = tmp.path().join("brief.md");
    let spec = tmp.path().join("spec.md");
    std::fs::write(&brief, "# Brief\n\nbody\n").unwrap();
    std::fs::write(&spec, "# Spec\n\nbody\n").unwrap();

    for args in [
        vec!["plan", "-t", "task", "--brief", brief.to_str().unwrap()],
        vec![
            "plan",
            "--spec",
            spec.to_str().unwrap(),
            "--brief",
            brief.to_str().unwrap(),
        ],
    ] {
        spar_cmd()
            .current_dir(&project)
            .args(args)
            .assert()
            .failure()
            .stderr(predicates::str::contains("--brief"));
    }
}

/// AC-3: an orchestrator reply is a normal bus event, but it carries enough metadata
/// to distinguish its conversation, scope, and dispatch turn from ordinary @human
/// traffic. The reply sender is explicit, never the bus command's human default.
#[test]
fn conversation_reply_is_scoped_and_carries_protocol_metadata() {
    let tmp = tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_repo(&project);

    let out = spar_cmd()
        .current_dir(&project)
        .args([
            "bus",
            "send",
            "--run",
            "run-123",
            "--from",
            "run-123:talk-abc",
            "--to",
            "@human",
            "--surface",
            "chat",
            "--conversation",
            "talk-abc",
            "--turn",
            "turn-42",
            "--message",
            "I need the acceptance criteria.",
            "--json",
        ])
        .assert()
        .success();
    let event = json(&out);
    assert_eq!(event["from"], "run-123:talk-abc");
    assert_eq!(event["to"], "@human");
    assert_eq!(event["run"], "run-123");
    assert_eq!(event["meta"]["surface"], "chat");
    assert_eq!(event["meta"]["conversation"], "talk-abc");
    assert_eq!(event["meta"]["turn"], "turn-42");
}

/// AC-4: the authority prohibition is enforced in code, not merely documented. The
/// test follows feature 003's source-level pattern and intentionally limits itself to
/// the conversation module, where proposals and evidence must stay read-only.
#[test]
fn conversation_module_never_mentions_operator_only_calls() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/orchestrator.rs"))
        .expect("feature 008 must provide src/orchestrator.rs");
    let body = source.split("#[cfg(test)]").next().unwrap();
    let code = body
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        "workflow::plan::approve",
        "workflow::plan::reject",
        "workflow::implement::ship",
        "cleanup_run",
        "archive_sweep",
        "pick_providers",
        "gates.plan_approved",
        "gates.ship_confirmed",
    ] {
        assert!(
            !code.contains(banned),
            "src/orchestrator.rs must never call `{banned}`: the operator disposes"
        );
    }
}
