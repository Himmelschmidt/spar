//! End-to-end (real orchestrator, real spawned slot, no `--dry-run`) coverage for the
//! mid-dispatch quota routing fix: a slot whose log ends in the real captured
//! dogfooding text must park the run at `Phase::Quota` with exit `4`, and an ordinary
//! failure must still fail the run at exit `1`. Everything else in the tree tests the
//! pieces of this chain in isolation; these two drive `run_slot` -> `execute_loop`'s
//! `fail()` -> process exit through the actual `spar` binary, the seam the pre-fix
//! coverage never crossed.
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
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

/// Drops a fake CLI named `name` on `bin_dir` that prints `body` to stdout and exits 1.
/// Returns nothing; call `fake_binary` once per provider and prefix `PATH` once.
fn fake_binary(bin_dir: &std::path::Path, name: &str, body: &str) {
    std::fs::create_dir_all(bin_dir).unwrap();
    let fake = bin_dir.join(name);
    std::fs::write(
        &fake,
        format!("#!/bin/sh\ncat <<'SPAR_TEST_EOF'\n{body}\nSPAR_TEST_EOF\nexit 1\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Drops a fake CLI named `name` that runs `script` (a full shell body) and returns
/// whatever exit code the script itself chooses, unlike `fake_binary`'s canned
/// print-and-`exit 1`. Used to fabricate a slot that genuinely succeeds and produces
/// its own expected artifact, which `fake_binary` alone cannot do.
fn fake_binary_script(bin_dir: &std::path::Path, name: &str, script: &str) {
    std::fs::create_dir_all(bin_dir).unwrap();
    let fake = bin_dir.join(name);
    std::fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn prefixed_path(bin_dir: &std::path::Path) -> String {
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Drops a fake `claude` CLI on its own `PATH` prefix that prints `body` to stdout and
/// exits 1, mirroring the real captured incident text.
fn fake_claude_binary(bin_dir: &std::path::Path, body: &str) -> String {
    fake_binary(bin_dir, "claude", body);
    prefixed_path(bin_dir)
}

fn only_run_id(proj: &std::path::Path) -> String {
    let runs = proj.join(".spar/runs");
    std::fs::read_dir(&runs)
        .expect("runs dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().join("state.json").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .expect("a run")
}

/// The real captured log text from the dogfooding incident (roadmap/BACKLOG.md).
const WEEKLY_LIMIT_LOG: &str = "! rate limit  seven_day  rejected\nYou've hit your weekly limit \u{b7} resets 12am (America/New_York)";

#[test]
fn implementer_rate_limited_mid_dispatch_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude_binary(&bin, WEEKLY_LIMIT_LOG);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");

    // The sharper half of the bug: a quota-parked run must accept `implement --run`,
    // not refuse with "plan is not approved (phase=Failed)" the way `Phase::Failed`
    // would.
    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("awaiting_ship_confirm"))
        .stdout(predicate::str::contains("plan is not approved").not());
}

/// The seam `WEEKLY_LIMIT_LOG` above never crosses: that fixture is plain prose, so it
/// only ever drives `scrape_strong_quota_signal`, never the typed `rate_limit_event`
/// path (`StreamCoalescer` -> `StreamStats` -> `SlotOutcome` -> `quota_hit_for_outcome`).
/// A real Claude CLI emits that event as one NDJSON line on stdout; this fakes exactly
/// that line, with a `resetsAt` 2 hours out, and checks both that the run parks on
/// quota *and* that the store's `cooldown_until` is that exact stated instant, not a
/// generic auto-recovering pause — the flagship behaviour a mutation neutering
/// `run_headless`'s `res.stats -> SlotOutcome` extraction (the sequential `implement`
/// dispatch path this test drives) could silently drop with every other test in this
/// file still green. `execute_prepared` has its own, identically-shaped extraction for
/// the parallel path (`run_slots_parallel`); see
/// `structured_rate_limit_event_on_a_parallel_dispatch_pauses_until_the_stated_instant`
/// below for that seam.
#[test]
fn structured_rate_limit_event_pauses_until_the_stated_instant() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let until = chrono::Utc::now() + chrono::Duration::hours(2);
    let body = format!(
        r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"rejected","rateLimitType":"five_hour","resetsAt":{}}}}}"#,
        until.timestamp()
    );
    let path_env = fake_claude_binary(&bin, &body);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");

    let quota: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(proj.join(".spar/quota.json")).unwrap())
            .unwrap();
    let provider = &quota["providers"]["cli:claude"];
    assert_eq!(provider["status"], "cooldown");
    let cooldown_until: chrono::DateTime<chrono::Utc> = provider["cooldown_until"]
        .as_str()
        .expect("cooldown_until present")
        .parse()
        .unwrap();
    assert_eq!(
        cooldown_until.timestamp(),
        until.timestamp(),
        "cooldown_until must be the adapter's own stated instant, not a generic pause"
    );
}

/// Same seam as `structured_rate_limit_event_pauses_until_the_stated_instant`, but
/// through `run_slots_parallel` -> `execute_prepared`, not `run_slot` -> `run_headless`:
/// two providers dispatch concurrently whenever more than one job is passed, and
/// `execute_prepared` has its own copy of the `res.stats -> SlotOutcome` extraction
/// (`quota_rejected`/`quota_resets_at` locals) that `run_headless`'s copy cannot cover.
/// Mutating either copy alone to drop the typed reset must fail exactly one of these
/// two tests.
#[test]
fn structured_rate_limit_event_on_a_parallel_dispatch_pauses_until_the_stated_instant() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let until = chrono::Utc::now() + chrono::Duration::hours(2);
    let body = format!(
        r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"rejected","rateLimitType":"five_hour","resetsAt":{}}}}}"#,
        until.timestamp()
    );
    let path_env = fake_claude_binary(&bin, &body);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "run",
            "--workflow",
            "peer",
            "-t",
            "split stack",
            "--providers",
            "cli:claude,cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");

    let quota: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(proj.join(".spar/quota.json")).unwrap())
            .unwrap();
    let provider = &quota["providers"]["cli:claude"];
    assert_eq!(provider["status"], "cooldown");
    let cooldown_until: chrono::DateTime<chrono::Utc> = provider["cooldown_until"]
        .as_str()
        .expect("cooldown_until present")
        .parse()
        .unwrap();
    assert_eq!(
        cooldown_until.timestamp(),
        until.timestamp(),
        "cooldown_until must be the adapter's own stated instant, not a generic pause"
    );
}

/// `--workflow review` dispatches its N reviewers concurrently through
/// `run_slots_parallel`, a different code path from `implement`'s `run_slot` ->
/// `bail!`. Both reviewers hitting the same rate limit must still park the run at
/// `Phase::Quota`/exit `4`, not `Phase::Failed`/exit `1` — the terminal-phase mapping
/// in `workflow/review.rs` has its own `any_failed` check that must consult
/// `quota_hit` rather than defaulting straight to `Failed`.
#[test]
fn review_workflow_rate_limited_on_every_reviewer_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude_binary(&bin, WEEKLY_LIMIT_LOG);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "review this",
            "--providers",
            "cli:claude,cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

/// A mixed panel — one reviewer genuinely rate-limited, the other failing for an
/// ordinary reason — must not read as `Phase::Quota` (that would mask the real defect
/// behind "wait and retry") and must not read as `Phase::Done` either (both slots did
/// fail; `salvage_expected_artifact` writing a synthetic `request_changes` for each is
/// not a real review). It must fail the run.
#[test]
fn review_workflow_mixed_quota_and_genuine_failure_fails_the_run() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    fake_binary(&bin, "claude", WEEKLY_LIMIT_LOG);
    fake_binary(
        &bin,
        "grok",
        "thread 'main' panicked at src/main.rs:42:\nindex out of bounds",
    );
    let path_env = prefixed_path(&bin);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "review this",
            "--providers",
            "cli:claude,cli:grok",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(1);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "failed");
}

/// Both reviewers failing for an ordinary reason (no quota signal at all) must still
/// fail the run — the pre-existing bug this closes: `salvage_expected_artifact`'s
/// synthetic `request_changes` for every interrupted reviewer meant the old
/// `changes == 0` guard on the `Failed` branch was dead code, so an all-genuine-failure
/// panel silently read `Phase::Done`/exit `0`.
#[test]
fn review_workflow_ordinary_failure_on_every_reviewer_fails_the_run() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    fake_binary(
        &bin,
        "claude",
        "thread 'main' panicked at src/main.rs:42:\nindex out of bounds",
    );
    fake_binary(
        &bin,
        "grok",
        "thread 'main' panicked at src/main.rs:99:\nindex out of bounds",
    );
    let path_env = prefixed_path(&bin);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "review this",
            "--providers",
            "cli:claude,cli:grok",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(1);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "failed");
}

/// `--workflow peer` dispatches through `run_slots_parallel` too. Both peers dying on
/// the same rate limit must park at `Phase::Quota`/exit `4`, mirroring the review fix.
#[test]
fn peer_workflow_rate_limited_on_every_slot_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude_binary(&bin, WEEKLY_LIMIT_LOG);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "run",
            "--workflow",
            "peer",
            "-t",
            "split stack",
            "--providers",
            "cli:claude,cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

/// `--workflow roles` dispatches sequentially through `run_slot`, discarding its
/// `Result` — same discriminator, different call shape from `peer`/`review`.
#[test]
fn roles_workflow_rate_limited_on_every_slot_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude_binary(&bin, WEEKLY_LIMIT_LOG);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "run",
            "--workflow",
            "roles",
            "-t",
            "fe/be",
            "--providers",
            "cli:claude,cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

/// codex/opencode/muse/grok have no adapter-specific rate-limit text handling (only
/// claude does, via `scrape_claude_rate_limits`) — a mid-dispatch limit on any of them
/// is detectable only through `scrape_strong_quota_signal`'s generic phrase list, and
/// that list has never been checked against real captured output from those four CLIs
/// (see DECISIONS.md O54). This does not close that gap — it has no real captured text
/// to drive with — but it does prove the generic path is actually wired end-to-end for
/// a non-claude provider (not just unit-tested in isolation): a `cli:grok` dispatch
/// whose log contains a strong generic rejection phrase must still park on `Phase::Quota`.
#[test]
fn non_claude_provider_generic_rate_limit_phrase_still_parks_on_quota() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    fake_binary(&bin, "grok", "429 too many requests, try again later");
    let path_env = prefixed_path(&bin);

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:grok",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

/// agy emits ~nothing to stdout, so `detect_and_pause_quota`'s log scrape can never see
/// its rejection there — the only place exhausted quota shows up is the statusline
/// telemetry sink `enrich_agy_stats` reads. Before this fix that telemetry paused the
/// provider but never flowed into `slot.quota_hit`, so a failed agy dispatch still fell
/// through to `Phase::Failed`/exit `1` despite spar having correctly identified (and
/// paused on) the real cause.
#[test]
fn agy_exhausted_quota_telemetry_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    let home = dir.join("home");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    init_repo(&proj);
    // Writes the near-exhausted-quota statusline payload for its own cwd (the slot's
    // worktree, discovered via `pwd` since the run id isn't known ahead of time) under
    // agy's telemetry root ($HOME/.gemini/antigravity-cli), then exits non-zero with no
    // rate-limit wording anywhere in its own stdout/log.
    fake_binary_script(
        &bin,
        "agy",
        r#"root="$HOME/.gemini/antigravity-cli/.spar"
mkdir -p "$root"
printf '{"cwd": "%s", "quota": {"gemini-5h": {"remaining_fraction": 0.01, "reset_in_seconds": 1800}}}\n' "$(pwd)" >> "$root/statusline.jsonl"
exit 1"#,
    );
    let path_env = prefixed_path(&bin);

    spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        .env("HOME", &home)
        .args(["implement", "-t", "add a feature", "--providers", "cli:agy"])
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

#[test]
fn implementer_ordinary_failure_still_fails_the_run_at_exit_1() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    let path_env = fake_claude_binary(
        &bin,
        "thread 'main' panicked at src/main.rs:42:\nindex out of bounds",
    );

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude",
        ])
        .env("PATH", &path_env)
        .assert()
        .code(1);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "failed");

    spar_cmd()
        .current_dir(&proj)
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("plan is not approved"));
}

/// A single quota-hit reviewer with a successful sibling used to tally as `Done`/exit
/// `0` — the surviving reviewer's real "approve" vote buried the fact that the other
/// half of the panel never ran at all. That slot's absence is a resource block, not a
/// vote, so it must still park the run rather than silently finishing on half a panel.
#[test]
fn review_workflow_one_quota_hit_reviewer_with_a_successful_sibling_parks_on_quota() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    fake_binary(&bin, "claude", WEEKLY_LIMIT_LOG);
    // Discovers the run's artifacts dir at runtime (the run id is not known until
    // `spar` mints it) via `FAKE_PROJECT_ROOT`, inherited from the outer test process,
    // and writes exactly the artifact `run_slot` expects from `review-1-cli-grok`
    // (`--providers cli:claude,cli:grok` assigns index 1 to the second provider).
    fake_binary_script(
        &bin,
        "grok",
        r#"run_dir=$(ls -d "$FAKE_PROJECT_ROOT"/.spar/runs/*/ | head -1)
mkdir -p "$run_dir/artifacts"
printf '## Verdict\napprove\n' > "$run_dir/artifacts/review-review-1-cli-grok.md"
exit 0"#,
    );
    let path_env = prefixed_path(&bin);

    spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        .env("FAKE_PROJECT_ROOT", &proj)
        .args([
            "run",
            "--workflow",
            "review",
            "-t",
            "review this",
            "--providers",
            "cli:claude,cli:grok",
        ])
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

/// The plan critic is dispatched every plan run but was never checked for `quota_hit`
/// — a rate-limited critic just marked its own slot `Failed` and the plan proceeded to
/// approval as if nothing happened, silently losing the critic's feedback to a rate
/// limit rather than surfacing it on the quota gate the way the planner's own dispatch
/// already does.
#[test]
fn plan_workflow_rate_limited_critic_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    // `[spec]` defaults on, which would ask for a third (test-author) fleet slot; keep
    // this test to exactly the two explicit `--providers` below.
    std::fs::write(proj.join("spar.toml"), "[spec]\nenabled = false\n").unwrap();
    // Both the planner and critic jobs share the same `expected_artifact: "plan.md"`
    // (`workflow/plan.rs`), so the planner (dispatched first, sequentially) must write
    // that exact name for its own dispatch to read as `ok` before the critic ever runs.
    fake_binary_script(
        &bin,
        "claude",
        r#"run_dir=$(ls -d "$FAKE_PROJECT_ROOT"/.spar/runs/*/ | head -1)
mkdir -p "$run_dir/artifacts"
printf '# Plan\n\ndone\n' > "$run_dir/artifacts/plan.md"
exit 0"#,
    );
    fake_binary(&bin, "grok", WEEKLY_LIMIT_LOG);
    let path_env = prefixed_path(&bin);

    spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        .env("FAKE_PROJECT_ROOT", &proj)
        .args([
            "plan",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude,cli:grok",
        ])
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}

/// `--workflow arena`'s main dispatch discarded every implementer's `run_slot` error
/// (marking the slot `Failed` but never checking `quota_hit`) and the ranker's result
/// outright (`let _ = ...`), so an all-rate-limited wave still walked through to
/// `Phase::AwaitingWinnerConfirm`/exit `2` with a fabricated winner rather than parking
/// on the quota gate.
#[test]
fn arena_workflow_all_implementers_rate_limited_parks_on_quota_and_exits_4() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let proj = dir.join("proj");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&proj).unwrap();
    init_repo(&proj);
    // Cap the fleet to exactly the 2 explicit providers below; the default
    // `max_agents` (4) would otherwise reuse/pad the fleet past what's asserted here.
    std::fs::write(proj.join("spar.toml"), "max_agents = 2\n").unwrap();
    let path_env = fake_claude_binary(&bin, WEEKLY_LIMIT_LOG);

    spar_cmd()
        .current_dir(&proj)
        .env("PATH", &path_env)
        .args([
            "run",
            "--workflow",
            "arena",
            "-t",
            "add a feature",
            "--providers",
            "cli:claude,cli:claude",
        ])
        .assert()
        .code(4);

    let run_id = only_run_id(&proj);
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(proj.join(".spar/runs").join(&run_id).join("state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["phase"], "quota");
}
