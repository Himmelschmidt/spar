//! An urgent bus send to a mid-turn `cli:claude` slot kills the live turn and
//! resumes the same session with the urgent message first (issue #231, part
//! (b)): the resume carries `-r <captured session>`, the resume prompt leads
//! with the urgent body ahead of the normal continuation, the message reads
//! `delivered`, and the interrupt costs no round and no rotation.
//!
//! A fake `claude` on `PATH` models the turn: the implementer's cold dispatch
//! announces a session id (the real `system/subtype=init` envelope spar
//! captures) and then sleeps until the urgent kill lands, the resume of that
//! session writes the implementer artifacts and exits, and any reviewer
//! approves off its prompt text (`Write review to:` line for the path,
//! `Code under review (worktree):` for the sha). `SPAR_TEST_URGENT_STATE`
//! points at a scratch dir receiving one `calls` line per dispatch.

mod common;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const BODY: &str = "URGENT-E2E-PING-231b stop, wrong direction";
const SESSION: &str = "sess-e2e-1";

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
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
    // One reviewer, no agent tester: the fake only has to satisfy the
    // implementer and a single approving reviewer. One round: an urgent
    // interrupt must not buy more.
    std::fs::write(
        dir.join("spar.toml"),
        "[suite]\nenabled = false\n[rounds]\nmax = 1\n",
    )
    .unwrap();
}

fn install_fake_claude(bin: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let p = bin.join("claude");
    std::fs::write(
        &p,
        r##"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
if [ "$1" = "auth" ]; then echo '{"loggedIn":true}'; exit 0; fi
GOT_P=0; RSID=""; prev=""
for a in "$@"; do
  if [ "$a" = "-p" ]; then GOT_P=1; fi
  if [ "$prev" = "-r" ]; then RSID="$a"; fi
  prev="$a"
done
if [ "$GOT_P" = "0" ]; then exit 0; fi
PROMPT=$(cat)
SLOT=$(printf '%s' "$PROMPT" | sed -n 's/^- Slot: \([^ ]*\) .*/\1/p' | head -n 1)
ART=$(printf '%s' "$PROMPT" | sed -n 's/^- Artifacts: //p' | head -n 1)
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
if [ -n "$RSID" ]; then
  echo "resume:$RSID" >> "$SPAR_TEST_URGENT_STATE/calls"
  SLOT=${SLOT:-impl}
  mkdir -p "$ART"
  printf '# Summary\nDone after urgent resume.\n' > "$ART/summary-$SLOT.md"
  printf '# Carry-forward\nDone after urgent resume.\n' > "$ART/carry-forward-$SLOT.md"
  exit 0
fi
if [ "$SLOT" = "impl" ] && [ ! -e "$SPAR_TEST_URGENT_STATE/impl-slept" ]; then
  touch "$SPAR_TEST_URGENT_STATE/impl-slept"
  echo "cold" >> "$SPAR_TEST_URGENT_STATE/calls"
  echo '{"type":"system","subtype":"init","model":"fake","session_id":"sess-e2e-1"}'
  exec sleep 120
fi
echo "other:$SLOT" >> "$SPAR_TEST_URGENT_STATE/calls"
SLOT=${SLOT:-impl}
mkdir -p "$ART"
printf '# Summary\nDone by a non-sleeping cold turn.\n' > "$ART/summary-$SLOT.md"
printf '# Carry-forward\nDone by a non-sleeping cold turn.\n' > "$ART/carry-forward-$SLOT.md"
exit 0
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
        std::thread::sleep(Duration::from_millis(100));
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

/// The urgent send kills the sleeping turn, resumes the same session with the
/// message first, and the run still completes in round one with no rotation.
#[test]
fn urgent_interrupt_resumes_same_session_with_message_first() {
    let tmp = tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let bin = tmp.path().join("bin");
    let state = tmp.path().join("urgent-state");
    std::fs::create_dir_all(&state).unwrap();
    install_fake_claude(&bin);

    // The implement run blocks in another thread; this thread sends the
    // urgent mid-turn. The command is built here so both threads share the
    // main thread's isolated HOME.
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
            "cli:claude",
            "--fleet",
            "small",
            "--json",
        ]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = cmd.output().expect("implement runs");
        tx.send(out).ok();
    });

    // Wait for the run to exist, then for the implementer's turn to be
    // mid-flight with its session captured (stats are saved live), so the
    // kill cannot land before spar knows the session to resume.
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

    let out = spar_std(&bin, &state)
        .current_dir(&proj)
        .args([
            "bus", "send", "--json", "--run", &run_id, "--to", "impl", "--urgent", "-m", BODY,
        ])
        .output()
        .unwrap();
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

    // Kill plus same-session resume: one cold, then the resume of the session
    // the killed turn announced. No second cold, no fallback dispatch.
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

    // The resume prompt leads with the urgent body ahead of the normal
    // continuation.
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

    // The resume went through `build_resume` with the captured session, and
    // the killed turn's log was preserved beside the truncated slot log.
    let slot_log = std::fs::read_to_string(run_dir.join("logs").join("impl.log")).unwrap();
    assert!(
        slot_log.contains(&format!("-r {SESSION}")),
        "the resume names the session: {slot_log:.500}"
    );
    assert!(
        run_dir
            .join("logs")
            .join("impl.urgent-killed.log")
            .is_file(),
        "the killed turn's log survives"
    );

    // Not a failure, no rotation, no round cost: round one, slot done, no
    // failure marker, and the message reads delivered.
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
