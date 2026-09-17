//! Round-over-round stale-verdict coverage (real orchestrator, real spawned
//! slots, no `--dry-run`). See DECISIONS.md O89.
//!
//! A reviewer slot re-dispatched in a later round must not pass the artifact
//! gate on the previous round's file: the first dispatch writes
//! `review-<slot>.md` with `request_changes`, the second dispatch exits 0 with
//! a tool call but writes nothing, and the run must record the second dispatch
//! as failed (`missing expected artifact`) rather than re-present the first
//! round's verdict as the second round's. A second reviewer that approves every
//! round proves the fake's writes genuinely satisfy the gate, so the failure
//! cannot be dismissed as a broken fake.
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
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
    c.env_remove("SPAR_DRY_RUN");
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

fn only_run_id(proj: &std::path::Path) -> String {
    std::fs::read_dir(proj.join(".spar/runs"))
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}

/// A fake `claude` CLI. The real adapter invokes `claude -p "<prompt>" ...`,
/// so the full prompt text arrives as `$2`; the expected artifact path is read
/// off its `Write ... to:` line, never hardcoded. Behavior per artifact:
/// - `summary-*.md` (implementer): always written, every round.
/// - `review-*-review-0-*.md`: approved on every dispatch, proving a fresh
///   write satisfies the gate (the non-vacuity guard).
/// - `review-*-review-1-*.md`: written once with `request_changes`, then never
///   again. Later dispatches still exit 0 with one tool call on stdout, which
///   is exactly the stale-verdict shape: the verdict on disk is real, recent,
///   and not this dispatch's.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "fake-claude 0"; exit 0; fi
STATE="$(dirname "$0")/state"
mkdir -p "$STATE"
TOOL_LINE='{"type":"assistant","message":{"model":"fake","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"Looking."},{"type":"tool_use","name":"Read","input":{"file_path":"x"}}]}}'
DEST=""
for a in "$@"; do
  case "$a" in
    *"Write review to:"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write review to: \([^ ]*\).*/\1/p' | head -n 1)
      ;;
    *"Write a summary to"*)
      DEST=$(printf '%s' "$a" | sed -n 's/.*Write a summary to `\([^`]*\)`.*/\1/p' | head -n 1)
      ;;
  esac
done
case "$DEST" in
  *review-0-*)
    printf '## Verdict\napprove\n' > "$DEST"
    echo "$TOOL_LINE"
    exit 0
    ;;
  *review-1-*)
    MARK="$STATE/review-1-written"
    if [ -f "$MARK" ]; then
      echo "$TOOL_LINE"
      exit 0
    fi
    touch "$MARK"
    printf '## Verdict\nrequest_changes\n\n## Findings\n- severity: major - stub defect\n' > "$DEST"
    echo "$TOOL_LINE"
    exit 0
    ;;
  "")
    exit 0
    ;;
  *)
    printf '# Summary\nDid the stub thing.\n' > "$DEST"
    exit 0
    ;;
esac
"#;

#[test]
fn a_reviewer_that_writes_nothing_on_redispatch_does_not_represent_its_old_verdict() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    init_repo(&proj);
    // Single-vendor fleet, frozen at creation (O27): a failed reviewer finds no
    // rotation candidate anywhere (pool, then this order), so the stale verdict
    // has nowhere to hide behind a second vendor and the loop must fail closed.
    std::fs::write(
        proj.join("spar.toml"),
        "[providers]\norder = [\"cli:claude\"]\n",
    )
    .unwrap();
    let fake = bin.join("claude");
    std::fs::write(&fake, FAKE_CLAUDE).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // One provider for the whole fleet: a failed reviewer cannot rotate away,
    // so the stale verdict has nowhere to hide behind a second vendor.
    let _ = spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "stale verdict",
            "--providers",
            "cli:claude",
            "--without",
            "suite",
            "--json",
        ])
        .env("PATH", &path_env)
        .assert()
        .get_output()
        .clone();

    let run_id = only_run_id(&proj);
    let run_dir = proj.join(".spar/runs").join(&run_id);
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("state.json")).unwrap())
            .unwrap();
    let slots = state["slots"].as_array().expect("slots");
    let reviewers: Vec<&serde_json::Value> =
        slots.iter().filter(|s| s["role"] == "reviewer").collect();
    assert!(
        reviewers.len() >= 2,
        "guard is vacuous without a two-reviewer panel: {state}"
    );
    assert!(
        state["round"].as_u64().unwrap_or(0) >= 2,
        "the run must have re-dispatched slots in a second round: {state}"
    );

    let stale = reviewers
        .iter()
        .find(|s| s["id"].as_str().is_some_and(|id| id.contains("review-1")))
        .expect("a review-1 slot");
    assert_eq!(
        stale["status"], "failed",
        "the redispatch that wrote nothing must fail, not re-present round 1: {stale}"
    );
    // The recorded reason must be the gate firing, not a fake-CLI crash: a
    // crash would pass this assertion's shape for the wrong reason.
    let err = stale["error"].as_str().unwrap_or("");
    assert!(
        err.contains("missing expected artifact"),
        "the failure must name the freshness gate: {stale}"
    );
    // And the stale file itself must still hold round 1's verdict, unparsed:
    // had the gate passed it, the slot would read done.
    let stale_id = stale["id"].as_str().unwrap();
    let stale_body =
        std::fs::read_to_string(run_dir.join(format!("artifacts/review-{stale_id}.md")))
            .expect("stale review file");
    assert!(
        stale_body.contains("request_changes"),
        "round 1's verdict must still be on disk, untouched: {stale_body}"
    );

    let approver = reviewers
        .iter()
        .find(|s| s["id"].as_str().is_some_and(|id| id.contains("review-0")))
        .expect("a review-0 slot");
    assert_eq!(
        approver["status"], "done",
        "a reviewer that writes fresh every round must keep passing: {approver}"
    );
}
