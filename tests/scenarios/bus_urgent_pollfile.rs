//! An urgent bus send to a mid-turn `cli:pi` slot never leaks the body into
//! the slot's poll file (issue #231, part (b)): pi reads its inbox through a
//! poll file, so the sender files the interrupt request before the message and
//! the file-time drain defers the body to the resume prompt. The test samples
//! the poll file from before `bus send` until the send returns and asserts the
//! body never appears there, then asserts the usual urgent outcome: the live
//! turn is killed, the same session resumes with the message first, the run
//! still completes in round one with no rotation, and the message reads
//! delivered.
//!
//! A fake `pi` on `PATH` models the turn: the cold dispatch announces its
//! session header (the `{"type":"session","id":…}` record spar captures) and
//! then ignores SIGTERM and sleeps, so the kill takes the full grace window
//! and the sampler cannot miss a file-time render; the resume of that session
//! writes the implementer artifacts and exits, and a reviewer approves off its
//! prompt text. `SPAR_TEST_URGENT_STATE` points at a scratch dir receiving one
//! `calls` line per dispatch.

mod common;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const BODY: &str = "URGENT-E2E-POLLFILE-231b wrong direction, again";
const SESSION: &str = "sess-pi-1";

fn spar_std(bin: &Path, state: &Path) -> std::process::Command {
    let mut c = common::spar_std();
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    c.env("PATH", &path_env)
        .env("SPAR_TEST_URGENT_STATE", state);
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
    std::fs::write(
        dir.join("spar.toml"),
        "[suite]\nenabled = false\n[rounds]\nmax = 1\n",
    )
    .unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn install_fake_pi(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let p = bin.join("pi");
    std::fs::write(
        &p,
        r##"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-pi 0"; exit 0; fi
if [ "$1" = "auth" ]; then echo '{}'; exit 0; fi
PROMPT="$(printf '%s\n' "$@")"
if printf '%s' "$PROMPT" | grep -q "Write review to:"; then
  ART=$(printf '%s' "$PROMPT" | sed -n 's/^- Write review to: \(.*\)\/review-.*/\1/p' | head -n 1)
  SLOT=$(printf '%s' "$PROMPT" | sed -n 's/^- Write review to: .*\/review-\(.*\)\.md/\1/p' | head -n 1)
  CWD=$(printf '%s' "$PROMPT" | sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' | head -n 1)
  echo "review:$SLOT" >> "$SPAR_TEST_URGENT_STATE/calls"
  SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null)
  mkdir -p "$ART"
  if [ -n "$SHA" ]; then
    printf '## Verdict\napprove\n\nReviewed-Commit: %s\n' "$SHA" > "$ART/review-$SLOT.md"
  else
    printf '## Verdict\napprove\n' > "$ART/review-$SLOT.md"
  fi
  exit 0
fi
if [ -e "$SPAR_TEST_URGENT_STATE/impl-slept" ]; then
  RSID=""
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--session-id" ]; then RSID="$a"; fi
    prev="$a"
  done
  if [ "$RSID" = "sess-pi-1" ]; then
    echo "resume:$RSID" >> "$SPAR_TEST_URGENT_STATE/calls"
    ART=$(printf '%s' "$PROMPT" | sed -n 's/^- Artifacts: //p' | head -n 1)
    mkdir -p "$ART"
    printf '# Summary\nDone after urgent resume.\n' > "$ART/summary-impl.md"
    printf '# Carry-forward\nDone after urgent resume.\n' > "$ART/carry-forward-impl.md"
    exit 0
  fi
  echo "other:$RSID" >> "$SPAR_TEST_URGENT_STATE/calls"
  exit 1
fi
touch "$SPAR_TEST_URGENT_STATE/impl-slept"
echo "cold" >> "$SPAR_TEST_URGENT_STATE/calls"
echo '{"type":"session","version":1,"id":"sess-pi-1"}'
trap "" TERM
exec sleep 300
"##,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn wait_for(msg: &str, mut found: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !found() {
        assert!(Instant::now() < deadline, "{msg}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn run_id_from_json(out: &std::process::Output) -> String {
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("implement prints --json");
    v["run_id"].as_str().expect("run_id").to_string()
}

fn state_json(proj: &Path, run_id: &str) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(run_id).join("state.json")).unwrap(),
    )
    .unwrap()
}

fn log_entries(proj: &Path, run_id: &str) -> Vec<serde_json::Value> {
    let out = spar_std(Path::new(""), Path::new(""))
        .current_dir(proj)
        .args(["bus", "log", "--run", run_id, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "bus log: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The urgent body never reaches the poll file: the sender files the interrupt
/// request first, so the file-time drain defers the body to the resume prompt.
#[test]
fn urgent_send_defers_pollfile_body_to_the_resume() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let bin = tmp.path().join("bin");
    let state = tmp.path().join("urgent-state");
    std::fs::create_dir_all(&state).unwrap();
    install_fake_pi(&bin);

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = common::spar_std();
    cmd.env("PATH", &path_env)
        .env("SPAR_TEST_URGENT_STATE", &state)
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "add a hello function",
            "--providers",
            "cli:pi",
            "--fleet",
            "small",
            "--json",
        ]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = cmd.output().expect("implement runs");
        tx.send(out).ok();
    });

    wait_for("implement never started", || {
        proj.join(".spar/runs")
            .read_dir()
            .map(|d| {
                d.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(|c: char| c.is_ascii_alphanumeric())
                        && e.path().join("state.json").is_file()
                })
            })
            .unwrap_or(false)
    });
    let run_id: String = std::fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| proj.join(".spar/runs").join(n).join("state.json").is_file())
        .expect("a run");
    let run_dir = proj.join(".spar/runs").join(&run_id);
    wait_for("implementer turn never went live with a session", || {
        let calls = std::fs::read_to_string(state.join("calls")).unwrap_or_default();
        let stats = std::fs::read_to_string(run_dir.join("logs").join("impl.stats.json"))
            .unwrap_or_default();
        calls.lines().any(|l| l == "cold") && stats.contains(SESSION)
    });
    let poll_file = run_dir.join("logs").join("nudges-impl.md");

    // Sample the poll file until the urgent send returns: a file-time render
    // lands synchronously inside `bus send`, before the kill starts, so any
    // sample after it (plus the final read below) sees the leak. The fake
    // ignores SIGTERM, so the window stays open through the kill grace.
    let stop = Arc::new(AtomicBool::new(false));
    let sampler_stop = stop.clone();
    let sampler_poll = poll_file.clone();
    let sampler = std::thread::spawn(move || {
        while !sampler_stop.load(Ordering::SeqCst) {
            let body = std::fs::read_to_string(&sampler_poll).unwrap_or_default();
            if body.contains(BODY) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    });

    let out = spar_std(&bin, &state)
        .current_dir(&proj)
        .args([
            "bus", "send", "--json", "--run", &run_id, "--to", "impl", "--urgent", "-m", BODY,
        ])
        .output()
        .unwrap();
    stop.store(true, Ordering::SeqCst);
    let leaked_during_send = sampler.join().expect("sampler joins");
    // The resume starts only after the send's kill returns, so this read still
    // predates any resume-time render.
    let leaked_after_send = std::fs::read_to_string(&poll_file)
        .unwrap_or_default()
        .contains(BODY);
    assert!(
        !leaked_during_send && !leaked_after_send,
        "the urgent body must never reach the poll file"
    );
    assert!(out.status.success(), "urgent send: {out:?}");
    let sent: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(sent["urgent"]["status"], "interrupted", "{sent}");
    let id = sent["id"].as_str().unwrap().to_string();

    let finished = rx
        .recv_timeout(Duration::from_secs(600))
        .expect("implement ends");
    assert!(
        finished.status.code().is_some_and(|c| c == 0 || c == 2),
        "the run completes: {finished:?}"
    );
    let _ = run_id_from_json(&finished);

    let calls = std::fs::read_to_string(state.join("calls")).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert!(lines.len() >= 2, "cold plus resume: {calls:?}");
    assert_eq!(lines[0], "cold");
    assert_eq!(lines[1], format!("resume:{SESSION}"));
    assert!(
        !lines.iter().any(|l| l.starts_with("other:")),
        "no unexpected extra dispatch: {calls:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("review:")),
        "the reviewer still approved after the resume: {calls:?}"
    );

    let urgent_prompt = std::fs::read_to_string(run_dir.join("prompt-impl-urgent.md")).unwrap();
    assert!(
        urgent_prompt.starts_with("[Urgent message from "),
        "{urgent_prompt:.200}"
    );
    let body_at = urgent_prompt.find(BODY).expect("urgent body in prompt");
    let normal_at = urgent_prompt
        .find("- Slot: impl")
        .expect("normal continuation in prompt");
    assert!(
        body_at < normal_at,
        "urgent body first, continuation after: {urgent_prompt:.400}"
    );

    let slot_log = std::fs::read_to_string(run_dir.join("logs").join("impl.log")).unwrap();
    assert!(
        slot_log.contains(&format!("--session-id {SESSION}")),
        "the resume names the session: {slot_log:.500}"
    );
    assert!(
        run_dir
            .join("logs")
            .join("impl.urgent-killed.log")
            .is_file(),
        "the killed turn's log survives"
    );

    let st = state_json(&proj, &run_id);
    assert_eq!(st["round"], 1, "{st}");
    let impl_slot = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "impl")
        .expect("implementer slot");
    assert_eq!(impl_slot["status"], "done", "{impl_slot}");
    assert!(
        !run_dir.join("markers").join("impl.failed").exists(),
        "no failure marker"
    );
    let entries = log_entries(&proj, &run_id);
    let entry = entries
        .iter()
        .find(|e| e["id"] == id)
        .expect("urgent message in the log");
    assert_eq!(entry["delivery"], "delivered", "{entry}");
}
