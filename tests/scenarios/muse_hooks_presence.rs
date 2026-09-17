//! A `cli:muse` slot gets presence and turn-boundary injection through muse's
//! project hook file (`.muse/hooks.json`), verified end to end.
//!
//! The fake `muse` below plays the agent lifecycle the way the real binary does:
//! on each dispatch it runs every command in the installed `.muse/hooks.json`
//! (submit, pre-tool, stop heartbeats plus the Stop `deliver` hook), then writes
//! every artifact any slot could owe and exits 0. The run completing with
//! `working` + `idle` presence rows on the bus proves spar installed the file,
//! the commands resolve back to the primary checkout, and the hooks did not break
//! the dispatch. A second test pins `--workspace` on the dispatch argv.
//!
//! A live test at the bottom runs the real `muse` binary (echo provider, no API
//! cost) against a spar-shaped hook file and asserts the hooks actually fire.
//! It runs only when `SPAR_TEST_REAL_MUSE=1` with `muse` on `PATH`.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::predicate;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn spar_home_dir() -> std::path::PathBuf {
    use std::sync::OnceLock;
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("spar-muse-home-{}", std::process::id()));
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
    // One reviewer, no agent tester: the fake only has to satisfy the implementer
    // and a single approving reviewer. One round: a failing slot must not buy more.
    std::fs::write(
        dir.join("spar.toml"),
        "[suite]\nenabled = false\n[rounds]\nmax = 1\n",
    )
    .unwrap();
}

/// A fake `muse` that honors the installed project hook file: on each dispatch it
/// records its argv, runs every command in `<workspace>/.muse/hooks.json` for the
/// lifecycle events the real binary fires (submit, pre-tool, stop — plus the Stop
/// `deliver` hook), snapshots the hook file per slot, then writes every artifact any
/// slot could owe and exits 0. Speaks just enough of the real `muse exec --json`
/// envelope (`run.model.configured` + a tool result) for session capture to run.
fn install_fake_muse(bin: &std::path::Path) {
    fs::create_dir_all(bin).unwrap();
    let p = bin.join("muse");
    fs::write(
        &p,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-muse 0"; exit 0; fi
WS=""
PROMPT_FILE=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--workspace" ]; then WS="$a"; fi
  if [ "$prev" = "--prompt-file" ]; then PROMPT_FILE="$a"; fi
  prev="$a"
done
echo "$@" >> "$SPAR_TEST_MUSE_STATE/args"
echo "$WS" >> "$SPAR_TEST_MUSE_STATE/workspaces"
SLOT=$(basename "$PROMPT_FILE" .md)
SLOT=${SLOT#prompt-}
if [ -f "$WS/.muse/hooks.json" ]; then
  cp "$WS/.muse/hooks.json" "$SPAR_TEST_MUSE_STATE/hooks-$SLOT.json"
  SPAR_TEST_HOOKS="$WS/.muse/hooks.json" python3 - <<'PYEOF'
import json, os, subprocess
doc = json.load(open(os.environ["SPAR_TEST_HOOKS"]))
events = doc.get("hooks", {})
for event in ["UserPromptSubmit", "PreToolUse", "Stop"]:
    for group in events.get(event, []):
        for h in group.get("hooks", []):
            cmd = h.get("command")
            if cmd:
                subprocess.run(cmd, shell=True, check=False)
PYEOF
fi
echo '{"schema_version":1,"stream":{"kind":"session","id":"sess-e2e-1"},"record_type":"event","payload_type":"run.model.configured","payload":{"kind":"run_model_configured","model_id":"muse-spark-1.3-contributor"}}'
echo '{"stream":{"kind":"session","id":"sess-e2e-1"},"payload_type":"tool.result","payload":{"kind":"tool_result","call_id":"c2","correlation_facts":{"outcome":"success","tool_name":"write"},"edit_facts":{"path":"summary.txt","tool_name":"write"}}}'
ART="$(dirname "$PROMPT_FILE")/artifacts"
mkdir -p "$ART"
printf '# Summary\nDone by the fake.\n' > "$ART/summary-$SLOT.md"
printf '# Carry-forward\nDone by the fake.\n' > "$ART/carry-forward-$SLOT.md"
printf '## Verdict\napprove\n' > "$ART/review-$SLOT.md"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn live_env(cmd: &mut assert_cmd::Command, bin: &std::path::Path, state: &std::path::Path) {
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    cmd.env("PATH", &path_env)
        .env("SPAR_TEST_MUSE_STATE", state);
}

fn run_id_from_json(out: &assert_cmd::assert::Assert) -> String {
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    v["run_id"].as_str().expect("run_id").to_string()
}

fn state_json(proj: &std::path::Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(run_id).join("state.json")).unwrap(),
    )
    .unwrap()
}

fn presence_statuses(proj: &std::path::Path, run_id: &str, agent: &str) -> Vec<String> {
    let p = proj
        .join(".spar/runs")
        .join(run_id)
        .join("bus/agents.jsonl");
    let text = std::fs::read_to_string(&p).unwrap_or_default();
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["agent"].as_str() == Some(agent))
        .filter_map(|v| v["status"].as_str().map(str::to_string))
        .collect()
}

/// The full loop: spar installs `.muse/hooks.json` into the slot worktree, the slot's
/// lifecycle runs the heartbeat commands, and the bus shows `working` + `idle`.
#[test]
fn muse_slot_reports_presence_through_its_project_hook_file() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let bin = tmp.path().join("bin");
    install_fake_muse(&bin);
    let state = tmp.path().join("muse-state");
    fs::create_dir_all(&state).unwrap();

    let mut cmd = spar_cmd();
    live_env(&mut cmd, &bin, &state);
    let out = cmd
        .current_dir(&proj)
        .env("SPAR_HOME", spar_home_dir())
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:muse",
            "--fleet",
            "small",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]));
    let run_id = run_id_from_json(&out);
    let run_dir = proj.join(".spar/runs").join(&run_id);

    // The implementer slot ran and completed: the hooks did not break the dispatch.
    let st = state_json(&proj, &run_id);
    let impl_slot = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot");
    assert_eq!(impl_slot["status"], "done", "{impl_slot}");
    let slot_id = impl_slot["id"].as_str().unwrap().to_string();

    // The hook file the slot ran is snapshotted per slot: four transitions plus the
    // injecting Stop hook, scoped to this run, no exported-env dependency.
    let hooks_path = state.join(format!("hooks-{slot_id}.json"));
    let hooks_text = std::fs::read_to_string(&hooks_path).unwrap();
    let hooks: serde_json::Value = serde_json::from_str(&hooks_text).unwrap();
    let events = hooks.get("hooks").unwrap().as_object().unwrap();
    for ev in ["UserPromptSubmit", "PreToolUse", "Notification", "Stop"] {
        assert!(events.contains_key(ev), "missing {ev}");
    }
    assert!(hooks_text.contains("--status working"), "{hooks_text}");
    assert!(hooks_text.contains("--status blocked"), "{hooks_text}");
    assert!(hooks_text.contains("--status idle"), "{hooks_text}");
    assert!(
        hooks_text.contains(&format!("bus deliver {slot_id} --run {run_id}")),
        "Stop hook must inject via `bus deliver`: {hooks_text}"
    );
    assert!(
        !hooks_text.contains("SPAR_AGENT_ID"),
        "hook commands must bake ids as args: {hooks_text}"
    );

    // ... and the heartbeats those hooks ran landed on the bus.
    let addr = format!("{run_id}:{slot_id}");
    let statuses = presence_statuses(&proj, &run_id, &addr);
    assert!(
        statuses.iter().any(|s| s == "working"),
        "no working presence for {addr}: {statuses:?}"
    );
    assert!(
        statuses.iter().any(|s| s == "idle"),
        "no idle presence for {addr}: {statuses:?}"
    );
    assert!(
        run_dir.join("bus/agents.jsonl").is_file(),
        "run bus mirror exists"
    );
}

/// Every dispatch — cold and resume alike — pins the workspace root explicitly.
#[test]
fn muse_dispatch_argv_pins_the_workspace() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let bin = tmp.path().join("bin");
    install_fake_muse(&bin);
    let state = tmp.path().join("muse-state");
    fs::create_dir_all(&state).unwrap();

    let mut cmd = spar_cmd();
    live_env(&mut cmd, &bin, &state);
    cmd.current_dir(&proj)
        .env("SPAR_HOME", spar_home_dir())
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:muse",
            "--fleet",
            "small",
            "--json",
        ])
        .assert()
        .code(predicate::in_iter([0, 2]));

    let args = std::fs::read_to_string(state.join("args")).unwrap();
    let workspaces: Vec<String> = std::fs::read_to_string(state.join("workspaces"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(!workspaces.is_empty(), "no dispatches recorded");
    for ws in &workspaces {
        assert!(!ws.is_empty(), "a dispatch missed --workspace:\n{args}");
    }
    assert!(
        !args.contains("--allow-workspace-switch"),
        "workspace mismatch must refuse:\n{args}"
    );
    // The slot logs' spawn headers carry the same pin.
    let run_id = fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
        .expect("one run");
    let logs: Vec<String> = fs::read_dir(proj.join(".spar/runs").join(&run_id).join("logs"))
        .unwrap()
        .flatten()
        .map(|e| e.path().display().to_string())
        .filter(|p| p.ends_with(".log"))
        .collect();
    assert!(!logs.is_empty(), "slot logs exist");
    for log in &logs {
        let text = std::fs::read_to_string(log).unwrap();
        assert!(
            text.contains("--workspace"),
            "spawn header must pin the workspace: {log}"
        );
    }
}

/// Live gate: the real `muse` binary (echo provider, no API cost) fires a
/// spar-shaped project hook file. Runs only with `SPAR_TEST_REAL_MUSE=1`.
#[test]
fn real_muse_binary_fires_project_hooks() {
    if std::env::var("SPAR_TEST_REAL_MUSE").as_deref() != Ok("1") {
        return;
    }
    let muse = which_muse().expect("SPAR_TEST_REAL_MUSE=1 but no muse on PATH");
    let tmp = tempdir().unwrap();
    let ws = tmp.path().join("ws");
    fs::create_dir_all(ws.join(".muse")).unwrap();
    // A minimal spar project so `spar bus heartbeat` resolves its paths from cwd.
    git(&ws, &["init", "-q"]);
    fs::write(ws.join("spar.toml"), "[suite]\nenabled = false\n").unwrap();
    let spar_exe = assert_cmd::cargo::cargo_bin("spar");
    let hook_cmd = |status: &str| {
        format!(
            "{} bus heartbeat probe-1 --status {status} --run rlive",
            spar_exe.display()
        )
    };
    let doc = serde_json::json!({
        "hooks": {
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": hook_cmd("working") }] }],
            "Stop": [{ "hooks": [{ "type": "command", "command": hook_cmd("idle") }] }],
        }
    });
    fs::write(
        ws.join(".muse/hooks.json"),
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();

    let out = Command::new(&muse)
        .args(["exec", "--yolo", "--provider", "echo", "--", "say hello"])
        .current_dir(&ws)
        .env("SPAR_HOME", spar_home_dir())
        .env_remove("SPAR_PROJECT_ROOT")
        .env_remove("SPAR_RUN_ID")
        .env_remove("SPAR_AGENT_ID")
        .output()
        .expect("run muse");
    assert!(out.status.success(), "muse exec failed: {out:?}");

    let agents = ws.join(".spar/bus/agents.jsonl");
    let text = std::fs::read_to_string(&agents).unwrap_or_default();
    let statuses: Vec<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["agent"].as_str() == Some("rlive:probe-1"))
        .filter_map(|v| v["status"].as_str().map(str::to_string))
        .collect();
    assert!(
        statuses.iter().any(|s| s == "working"),
        "UserPromptSubmit hook never fired: {statuses:?}"
    );
    assert!(
        statuses.iter().any(|s| s == "idle"),
        "Stop hook never fired: {statuses:?}"
    );
}

fn which_muse() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("muse"))
            .find(|p| p.is_file())
    })
}
