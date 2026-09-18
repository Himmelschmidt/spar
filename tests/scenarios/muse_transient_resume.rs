//! A `cli:muse` slot survives a transient provider failure by resuming, not restarting.
//!
//! A fake `muse` on `PATH` fails its cold dispatch with the backend-404 signature
//! after real work (a session id plus a tool call), then succeeds once the retry
//! resumes the same session. The run completes instead of going `failed`. A second
//! test pins the fail-fast side: the same string with zero tool calls never retries.
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

/// A fake `muse` that models the transient window: the cold dispatch does real work
/// (session id plus a tool call) and then dies with the backend 404, while a resume
/// of that session succeeds and writes every artifact any slot of the run could owe
/// (keyed off the prompt filename, so no state parsing is needed). It speaks the
/// real `muse exec --json` envelope (`run.model.configured` + `tool.result`), so the
/// retry gate runs through muse's own session/tool capture, not the generic lines.
///
/// `SPAR_TEST_MUSE_STATE` points at a scratch dir receiving one `calls` line per
/// invocation (`cold` or `resume:<sid>`). `SPAR_TEST_MUSE_MODE=fail-fast` makes every
/// dispatch fail with the same string but zero tool calls and no session.
///
/// Cold-vs-resume models the real CLI since spar started assigning session ids:
/// every dispatch carries `--session-id`, so flag presence no longer distinguishes
/// them. Instead the fake keeps a session store (`$STATE/sessions`): an incoming id
/// it has never issued is a cold dispatch (it does real work, then dies with the
/// 404), while an incoming id it issued before is a resume of that session. The
/// cold dispatch always issues the fixed `sess-e2e-1`, so the retry the test
/// asserts on is `resume:sess-e2e-1` regardless of the derived cold id.
fn install_fake_muse(bin: &std::path::Path) {
    fs::create_dir_all(bin).unwrap();
    let p = bin.join("muse");
    fs::write(
        &p,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-muse 0"; exit 0; fi
if [ "$SPAR_TEST_MUSE_MODE" = "fail-fast" ]; then
  echo 'model `nope` does not exist or you lack access' >&2
  echo "cold" >> "$SPAR_TEST_MUSE_STATE/calls"
  exit 1
fi
SID=""
PROMPT_FILE=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ]; then SID="$a"; fi
  if [ "$prev" = "--prompt-file" ]; then PROMPT_FILE="$a"; fi
  prev="$a"
done
touch "$SPAR_TEST_MUSE_STATE/sessions"
RESUMED=0
if [ -n "$SID" ] && grep -qxF "$SID" "$SPAR_TEST_MUSE_STATE/sessions" 2>/dev/null; then
  RESUMED=1
else
  if [ -n "$SID" ]; then echo "$SID" >> "$SPAR_TEST_MUSE_STATE/sessions"; fi
  echo "sess-e2e-1" >> "$SPAR_TEST_MUSE_STATE/sessions"
fi
if [ "$RESUMED" = "0" ]; then
  echo "cold" >> "$SPAR_TEST_MUSE_STATE/calls"
  echo '{"schema_version":1,"stream":{"kind":"session","id":"sess-e2e-1"},"record_type":"event","payload_type":"run.model.configured","payload":{"kind":"run_model_configured","model_id":"muse-spark-1.3-contributor"}}'
  echo '{"stream":{"kind":"session","id":"sess-e2e-1"},"payload_type":"tool.result","payload":{"kind":"tool_result","call_id":"c1","correlation_facts":{"outcome":"success","tool_name":"edit"},"edit_facts":{"path":"hello.txt","tool_name":"edit"}}}'
  echo 'model `muse-spark-1.3-contributor` does not exist or you lack access [request_id=e2e]' >&2
  exit 1
fi
echo "resume:$SID" >> "$SPAR_TEST_MUSE_STATE/calls"
SLOT=$(basename "$PROMPT_FILE" .md)
SLOT=${SLOT#prompt-}
ART="$(dirname "$PROMPT_FILE")/artifacts"
echo '{"schema_version":1,"stream":{"kind":"session","id":"sess-e2e-1"},"record_type":"event","payload_type":"run.model.configured","payload":{"kind":"run_model_configured","model_id":"muse-spark-1.3-contributor"}}'
echo '{"stream":{"kind":"session","id":"sess-e2e-1"},"payload_type":"tool.result","payload":{"kind":"tool_result","call_id":"c2","correlation_facts":{"outcome":"success","tool_name":"write"},"edit_facts":{"path":"summary.txt","tool_name":"write"}}}'
mkdir -p "$ART"
printf '# Summary\nDone by the fake.\n' > "$ART/summary-$SLOT.md"
printf '# Carry-forward\nDone by the fake.\n' > "$ART/carry-forward-$SLOT.md"
CWD=$(sed -n 's/.*Code under review (worktree): \([^ ]*\).*/\1/p' "$PROMPT_FILE" | head -n 1)
SHA=""
if [ -n "$CWD" ]; then SHA=$(git -C "$CWD" rev-parse HEAD 2>/dev/null); fi
if [ -n "$SHA" ]; then
  printf '## Verdict\napprove\n\nReviewed-Commit: %s\n' "$SHA" > "$ART/review-$SLOT.md"
else
  printf '## Verdict\napprove\n' > "$ART/review-$SLOT.md"
fi
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
    // Zero backoff: this run must never sleep the real 60s/150s/300s schedule.
    // A test seam, not a user knob — see TRANSIENT_RETRY_BACKOFF_SECS.
    cmd.env("PATH", &path_env)
        .env("SPAR_TRANSIENT_RETRY_BACKOFF_SECS", "0,0,0")
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

/// The 404 after real work is retried, the retry resumes the captured session, and
/// the run completes instead of going `failed`.
#[test]
fn muse_transient_404_resumes_and_completes() {
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

    // Every slot's cold dispatch hits the window once, then its retry resumes the
    // captured session: the calls come in cold/resume pairs, never a cold repeat.
    let calls = std::fs::read_to_string(state.join("calls")).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert!(lines.len() >= 2, "at least one cold/resume pair: {calls:?}");
    assert_eq!(lines[0], "cold");
    for pair in lines.chunks(2) {
        assert_eq!(
            pair,
            ["cold", "resume:sess-e2e-1"],
            "each cold 404 is followed by a resume of the captured session: {calls}"
        );
    }

    // The retries went through `build_resume`: every slot log's spawn header names
    // the session id instead of cold-restarting.
    let logs: Vec<String> = fs::read_dir(run_dir.join("logs"))
        .unwrap()
        .flatten()
        .map(|e| e.path().display().to_string())
        .collect();
    let slot_logs: Vec<&String> = logs
        .iter()
        .filter(|p| p.ends_with(".log") && !p.contains("recovery") && !p.contains("retry"))
        .collect();
    assert!(!slot_logs.is_empty(), "slot logs exist: {logs:?}");
    for slot_log in &slot_logs {
        let log_text = std::fs::read_to_string(slot_log).unwrap();
        assert!(
            log_text.contains("--session-id sess-e2e-1"),
            "the retry must resume the session: {slot_log}"
        );
    }
    // Each failed attempt's own 404 survives in a preserved sibling log.
    let retry_logs: Vec<&String> = logs
        .iter()
        .filter(|p| p.contains("transient-retry"))
        .collect();
    assert_eq!(
        retry_logs.len(),
        slot_logs.len(),
        "one preserved 404 per slot: {logs:?}"
    );
    for retry_log in retry_logs {
        assert!(
            std::fs::read_to_string(retry_log)
                .unwrap()
                .contains("does not exist or you lack access"),
            "the preserved log carries the 404 that caused the wait"
        );
    }

    // The run completed: implementer done, ship gate reached, brief written.
    let st = state_json(&proj, &run_id);
    let impl_slot = st["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["role"] == "implementer")
        .expect("implementer slot");
    assert_eq!(impl_slot["status"], "done", "{impl_slot}");
    assert!(
        run_dir
            .join("artifacts")
            .join("carry-forward-impl.md")
            .exists()
            || fs::read_dir(run_dir.join("artifacts"))
                .unwrap()
                .flatten()
                .any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("carry-forward-")),
        "the implementer's carry-forward brief exists"
    );
}

/// The same string with zero tool calls is a wrong model or dead entitlement, not a
/// transient window: it fails immediately with no retry.
#[test]
fn muse_first_call_404_fails_without_retry() {
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
    cmd.env("SPAR_TEST_MUSE_MODE", "fail-fast");
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
        .failure();
    // A failed run prints no `--json` run id on stdout; it is the only run here.
    let runs: Vec<String> = fs::read_dir(proj.join(".spar/runs"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(runs.len(), 1, "one run: {runs:?}");
    let run_id = runs[0].clone();
    let run_dir = proj.join(".spar/runs").join(&run_id);

    // One round, one dispatch, no retry: `[rounds] max = 1` plus fail-fast means any
    // second call would be a retry, and there is none.
    let calls = std::fs::read_to_string(state.join("calls")).unwrap();
    assert_eq!(
        calls.lines().count(),
        1,
        "no retry on zero tool calls: {calls}"
    );
    let logs: Vec<String> = fs::read_dir(run_dir.join("logs"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !logs.iter().any(|l| l.contains("transient-retry")),
        "no preserved retry log without a retry: {logs:?}"
    );
    let st = state_json(&proj, &run_id);
    assert_eq!(st["phase"], "failed", "{st}");
}
