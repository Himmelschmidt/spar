//! Fleet shaping acceptance tests (feature 011).
//!
//! Five subjects, in the order the work lands:
//!
//! * Bug A - a non-empty pinned reviewer panel is an exclusion list. Past its end the
//!   answer is nothing, never the next `[providers].order` entry, and the run's pool is
//!   `1 + panel_size` wide rather than `max_agents` wide.
//! * Bug B - `--role` outranks the positional `--providers` pool, and that provenance is
//!   frozen into the run so a later round without `--reload-config` resolves identically.
//! * Feature C - the resolved fleet is reported as `fleet` in run JSON and as a table in
//!   the human gate output, including seats the run will dispatch but has not created.
//! * Feature D - `--without critic,spec,suite` drops seats for one run.
//! * Feature E - `--fleet small|standard` presets, composition with the explicit flags,
//!   and honest `--help`.
//!
//! Every provider ref here is a dry-run stub. `cli:codex` and `api:openai` are chosen as
//! markers because neither appears in the default `[providers].order`
//! (`cli:claude, cli:grok, cli:agy`), so a seat landing on one of those is proof the seat
//! came from a pin, and a seat landing on `cli:grok`/`cli:agy` where no pin names them is
//! proof it leaked out of `order`.

mod common;
use predicates::prelude::*;
use serde_json::Value;
use std::process::Command;
use tempfile::{tempdir, TempDir};

/// Per-test-process SPAR_HOME so the suite never writes the developer's real
/// ~/.spar/registry.json.
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

/// spar exports SPAR_PROJECT_ROOT / SPAR_RUN_ID / SPAR_AGENT_ID into every slot, so when
/// this suite runs *inside* a spar worktree an un-cleared child would resolve the primary
/// checkout and write real runs into it. Cleared per-Command, never via process env.
fn spar_cmd() -> assert_cmd::Command {
    let mut c = common::spar();
    c.env("SPAR_HOME", spar_home_dir());
    c.env_remove("SPAR_PROJECT_ROOT");
    c.env_remove("SPAR_RUN_ID");
    c.env_remove("SPAR_AGENT_ID");
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
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "test\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
}

/// A git repo with `spar.toml` set to `body`.
fn project(body: &str) -> TempDir {
    let tmp = tempdir().unwrap();
    init_repo(tmp.path());
    std::fs::write(tmp.path().join("spar.toml"), body).unwrap();
    tmp
}

fn read_state(dir: &std::path::Path, run_id: &str) -> Value {
    let p = dir.join(".spar/runs").join(run_id).join("state.json");
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

fn read_run_config(dir: &std::path::Path, run_id: &str) -> Value {
    let p = dir.join(".spar/runs").join(run_id).join("config.json");
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

/// Providers of every slot with `role`, in slot order.
fn providers_for(state: &Value, role: &str) -> Vec<String> {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == role)
        .map(|s| {
            let p = s["provider"].as_str().unwrap_or_default().to_string();
            match s["model"].as_str() {
                Some(m) if !p.contains('@') => format!("{p}@{m}"),
                _ => p,
            }
        })
        .collect()
}

fn slot_ids_for(state: &Value, role: &str) -> Vec<String> {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == role)
        .map(|s| s["id"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn has_role(state: &Value, role: &str) -> bool {
    state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["role"] == role)
}

/// Run `spar <args>` in `dir`, expect `code`, and parse stdout as JSON.
fn run_json(dir: &std::path::Path, args: &[&str], code: i32) -> Value {
    let out = spar_cmd().current_dir(dir).args(args).assert().code(code);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("spar {args:?} did not emit JSON ({e}):\n{stdout}"))
}

/// `spar plan --dry-run --json`, parking at `awaiting_plan_approval` (exit 2).
fn plan_json(dir: &std::path::Path, extra: &[&str]) -> Value {
    let mut args = vec![
        "plan",
        "--task",
        "add a hello function",
        "--dry-run",
        "--json",
    ];
    args.extend_from_slice(extra);
    run_json(dir, &args, 2)
}

/// `spar implement --dry-run --json` for a fresh run, parking at `awaiting_ship_confirm`.
fn implement_json(dir: &std::path::Path, extra: &[&str]) -> Value {
    let mut args = vec![
        "implement",
        "--task",
        "add a hello function",
        "--dry-run",
        "--json",
    ];
    args.extend_from_slice(extra);
    run_json(dir, &args, 2)
}

/// `spar brief --json`: the ship gate reads `open_findings` off the review history.
fn status_json(dir: &std::path::Path, run_id: &str) -> Value {
    run_json(dir, &["brief", run_id, "--json"], 0)
}

/// Model live reviewer findings: the dry-run backend marks its own findings
/// synthetic (left out of `open_findings`); a live reviewer's would be live.
/// Only the named seat's entries liven, so the gate counts that seat alone.
fn liven_seat_findings(dir: &std::path::Path, run_id: &str, slot_id: &str) {
    let p = dir.join(".spar/runs").join(run_id).join("state.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    for e in v["review_history"].as_array_mut().unwrap() {
        if e["slot_id"] != slot_id {
            continue;
        }
        for f in e["review"]["findings"].as_array_mut().unwrap() {
            f["synthetic"] = serde_json::json!(false);
        }
    }
    std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

fn open_finding_slots(v: &Value) -> Vec<String> {
    v["open_findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["slot_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// `spar run --workflow review --dry-run --json`, which completes synchronously (exit 0).
fn review_json(dir: &std::path::Path, extra: &[&str]) -> Value {
    let mut args = vec![
        "run",
        "--workflow",
        "review",
        "--task",
        "review this",
        "--dry-run",
        "--json",
    ];
    args.extend_from_slice(extra);
    run_json(dir, &args, 0)
}

/// The `fleet` array from run JSON. Feature C: this key is the contract, so a run JSON
/// without it is a failure of the feature and not a shape this helper tolerates.
fn fleet(v: &Value) -> Vec<Value> {
    v["fleet"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "run JSON has no `fleet` array (keys: {:?})",
                v.as_object().map(|o| o.keys().collect::<Vec<_>>())
            )
        })
        .clone()
}

fn fleet_seats(v: &Value, role: &str) -> Vec<Value> {
    fleet(v).into_iter().filter(|s| s["role"] == role).collect()
}

fn seat_field(seat: &Value, key: &str) -> String {
    seat[key]
        .as_str()
        .unwrap_or_else(|| panic!("fleet seat missing `{key}`: {seat}"))
        .to_string()
}

/// Collapse whitespace so a clap-wrapped help line matches as one string.
fn help_text(cmd: &str) -> String {
    let out = spar_cmd().args([cmd, "--help"]).assert().success();
    let s = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bus events for the whole workspace (the bus is workspace-scoped, `.spar/bus/`, W5).
fn bus_events(dir: &std::path::Path) -> Vec<Value> {
    let p = dir.join(".spar/bus/events.jsonl");
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Bug A: a pinned reviewer panel is authoritative
// ---------------------------------------------------------------------------

/// AC-1: one `[roles].reviewer` pin means exactly one reviewer seat. Today the panel is
/// padded to `DEFAULT_REVIEWERS` out of `[providers].order`, so `cli:grok` appears on a
/// seat nobody pinned.
#[test]
fn ac1_single_pinned_reviewer_is_the_whole_panel() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex"]
"#,
    );
    let v = implement_json(tmp.path(), &[]);
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string()],
        "a one-entry [roles].reviewer list must not be padded from [providers].order"
    );
}

/// AC-2: the run's pool is `1 + panel_size` wide, not `max_agents` wide. `max_agents`
/// sizing is what manufactures the extra reviewer seat in the first place.
#[test]
fn ac2_pool_width_is_implementer_plus_panel_not_max_agents() {
    let tmp = project(
        r#"
max_agents = 6
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex"]
"#,
    );
    let v = implement_json(tmp.path(), &[]);
    let pool = v["providers"].as_array().unwrap().clone();
    assert_eq!(
        pool.len(),
        2,
        "pool must be 1 implementer + 1 pinned reviewer, got {pool:?}"
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(providers_for(&state, "reviewer"), vec!["cli:codex"]);
}

/// AC-3: two pins are exactly those two seats, in the pinned order.
#[test]
fn ac3_two_pins_are_exactly_two_seats_in_order() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex", "api:openai"]
"#,
    );
    let v = implement_json(tmp.path(), &[]);
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string(), "api:openai".to_string()]
    );
}

/// AC-4: an *unpinned* panel is unchanged - two reviewers drawn from `[providers].order`.
/// `DEFAULT_REVIEWERS` stays the floor only when nobody pinned a panel.
#[test]
fn ac4_unpinned_panel_still_uses_provider_order() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:codex"
"#,
    );
    let v = implement_json(tmp.path(), &[]);
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:claude".to_string(), "cli:grok".to_string()],
        "an unpinned panel keeps today's two seats from [providers].order"
    );
}

/// AC-5: escalation on a pinned panel widens by duplicating a pinned reviewer. It must
/// never import a provider from `[providers].order` that the operator excluded.
#[test]
fn ac5_widening_a_pinned_panel_never_imports_from_provider_order() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex"]
"#,
    );
    let out = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--task",
            "force stuck path",
            "--max-rounds",
            "20",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(3);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    let v: Value = serde_json::from_str(&stdout).unwrap();
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    let revs = providers_for(&state, "reviewer");
    assert!(
        revs.len() > 1,
        "the stuck ladder should have widened the panel, got {revs:?}"
    );
    assert!(
        revs.iter().all(|p| p == "cli:codex"),
        "a widened pinned panel must duplicate the pin, not draw from [providers].order: {revs:?}"
    );
}

/// AC-6: widening compares pins and slots by storage key, so `cli:codex@terra` and the
/// slot it created (`provider=cli:codex`, `model=terra`) are recognised as the same seat
/// and the panel is not silently widened onto `[providers].order`.
#[test]
fn ac6_widening_matches_pins_by_storage_key_not_raw_ref() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex@terra"]
"#,
    );
    let out = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--task",
            "force stuck path",
            "--max-rounds",
            "20",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(3);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    let v: Value = serde_json::from_str(&stdout).unwrap();
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    let revs = providers_for(&state, "reviewer");
    assert!(
        revs.iter().all(|p| p == "cli:codex@terra"),
        "every reviewer seat must stay on the pinned provider@model: {revs:?}"
    );
}

// ---------------------------------------------------------------------------
// Bug B: --role outranks the positional pool
// ---------------------------------------------------------------------------

/// AC-7: `--providers a,b,c --role reviewer=X` gives implementer `a` and exactly one
/// reviewer, `X`. Today `--providers` short-circuits the resolver and `--role` is
/// silently dropped.
#[test]
fn ac7_cli_role_outranks_positional_pool() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--role",
            "reviewer=cli:codex",
        ],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(providers_for(&state, "implementer"), vec!["cli:claude"]);
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string()],
        "one --role reviewer means one seat, even under an explicit --providers pool"
    );
}

/// AC-8: two `--role reviewer` flags under an explicit pool set the panel to exactly
/// those two, in flag order, and no pool position becomes a reviewer.
#[test]
fn ac8_two_cli_reviewer_pins_set_panel_size_and_order() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--role",
            "reviewer=cli:codex",
            "--role",
            "reviewer=api:openai",
        ],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string(), "api:openai".to_string()]
    );
}

/// AC-9: the fix does not over-correct. An explicit pool still beats `[roles]` for the
/// seats it covers; only `--role` outranks it.
#[test]
fn ac9_explicit_pool_still_beats_the_roles_file() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex", "api:openai"]
"#,
    );
    let v = implement_json(tmp.path(), &["--providers", "cli:claude,cli:grok,cli:agy"]);
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&state, "implementer"),
        vec!["cli:claude"],
        "an explicit pool overrides [roles] for the positions it covers"
    );
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:grok".to_string(), "cli:agy".to_string()]
    );
}

/// AC-10: `--role implementer=X` beats pool index 0.
#[test]
fn ac10_cli_role_beats_pool_for_a_singleton_role() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok",
            "--role",
            "implementer=cli:codex",
        ],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(providers_for(&state, "implementer"), vec!["cli:codex"]);
}

/// AC-11: the CLI provenance is frozen into the run's `config.json`, so a later round
/// without `--reload-config` resolves with the same precedence (O27). Plan under a pool
/// plus one `--role reviewer`, then implement the same run with no flags at all.
#[test]
fn ac11_cli_role_provenance_survives_the_frozen_config() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
reviewer = ["cli:grok", "cli:agy"]
"#,
    );
    let v = plan_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--role",
            "reviewer=cli:codex",
        ],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();

    let cfg = read_run_config(tmp.path(), &run_id);
    assert_eq!(
        cfg["roles"]["reviewer"],
        serde_json::json!(["cli:codex"]),
        "the CLI panel replaces the file's list in the run's frozen config"
    );

    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();

    let cont = run_json(
        tmp.path(),
        &["implement", "--run", &run_id, "--dry-run", "--json"],
        2,
    );
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string()],
        "a continuation without --reload-config keeps the CLI reviewer panel, \
         not the file's two-entry list and not the frozen pool"
    );
    let reviewers = fleet_seats(&cont, "reviewer");
    assert_eq!(reviewers.len(), 1);
    assert_eq!(
        seat_field(&reviewers[0], "source"),
        "cli-role",
        "the CLI provenance is frozen with the run, not recomputed from the pool"
    );
}

// ---------------------------------------------------------------------------
// Feature C: the resolved fleet is visible at the gate
// ---------------------------------------------------------------------------

/// AC-12: run JSON carries a `fleet` array and every seat has the full contract shape.
#[test]
fn ac12_run_json_has_a_fleet_array_with_the_full_seat_shape() {
    let tmp = project(
        r#"
[roles]
planner = "cli:claude"
plan_critic = "cli:codex"
test_author = "api:openai"
implementer = "cli:claude"
reviewer = ["cli:codex"]
"#,
    );
    let v = plan_json(tmp.path(), &[]);
    let seats = fleet(&v);
    assert!(!seats.is_empty(), "fleet must not be empty at a plan gate");
    for seat in &seats {
        for key in ["seat", "role", "provider", "source"] {
            assert!(
                seat[key].is_string(),
                "fleet seat needs a string `{key}`: {seat}"
            );
        }
        assert!(
            seat["projected"].is_boolean(),
            "fleet seat needs a boolean `projected`: {seat}"
        );
        assert!(
            seat.get("model").is_some(),
            "fleet seat needs a `model` key (null when unset): {seat}"
        );
    }
}

/// AC-13: at `awaiting_plan_approval` the plan seats are actual and the implement seats
/// the run *will* dispatch are present and marked `projected`. This is the gate where a
/// human decides whether to pay for the panel, so the panel has to be on screen.
#[test]
fn ac13_plan_gate_projects_the_implement_panel() {
    let tmp = project(
        r#"
[roles]
planner = "cli:claude"
plan_critic = "cli:codex"
test_author = "api:openai"
implementer = "cli:agy"
reviewer = ["cli:codex"]
"#,
    );
    let v = plan_json(tmp.path(), &[]);
    assert_eq!(v["phase"], "awaiting_plan_approval");

    let planner = fleet_seats(&v, "planner");
    assert_eq!(planner.len(), 1);
    assert_eq!(planner[0]["projected"], Value::Bool(false));
    assert_eq!(seat_field(&planner[0], "provider"), "cli:claude");

    let implementer = fleet_seats(&v, "implementer");
    assert_eq!(implementer.len(), 1, "the implement seat must be projected");
    assert_eq!(implementer[0]["projected"], Value::Bool(true));
    assert_eq!(seat_field(&implementer[0], "provider"), "cli:agy");

    let reviewers = fleet_seats(&v, "reviewer");
    assert_eq!(
        reviewers.len(),
        1,
        "exactly the pinned panel is projected, no phantom seat: {reviewers:?}"
    );
    assert_eq!(seat_field(&reviewers[0], "provider"), "cli:codex");
    assert_eq!(reviewers[0]["projected"], Value::Bool(true));
}

/// Regression: review round 7 found the plan gate reporting a pool narrower than the
/// panel (one explicit `--providers` entry, default two-reviewer panel) as having no
/// reviewer seats at all, because the projection read the raw un-cycled pool directly.
/// `prepare_implement_slots` cycles a short explicit pool with `pick_providers` to fill
/// `pool_width(cfg)` before dispatch, so the projection must apply the same cycling before
/// resolving seats, or it silently omits reviewer seats the run will actually create.
#[test]
fn plan_gate_projection_expands_short_pool_to_panel_width() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:codex", "--without", "critic,spec"],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();

    let implementer = fleet_seats(&v, "implementer");
    assert_eq!(implementer.len(), 1);
    assert_eq!(seat_field(&implementer[0], "provider"), "cli:codex");

    let reviewers = fleet_seats(&v, "reviewer");
    assert_eq!(
        reviewers.len(),
        2,
        "the default two-reviewer panel must be projected even though the pool has only \
         one explicit entry: {reviewers:?}"
    );
    for r in &reviewers {
        assert_eq!(seat_field(r, "provider"), "cli:codex");
        assert_eq!(r["projected"], Value::Bool(true));
    }

    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_json(
        tmp.path(),
        &["implement", "--run", &run_id, "--dry-run", "--json"],
        2,
    );
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string(), "cli:codex".to_string()],
        "the projection must match what implement actually dispatches"
    );
}

/// AC-14: `source` names where each seat's provider came from, one value per rung of the
/// precedence ladder. A provider synthesized from `[roles]` or `[providers].order` must
/// not be relabelled `cli-providers` just because it reached the resolver as a pool.
#[test]
fn ac14_seat_source_names_the_precedence_rung() {
    // No positional pool: planner is pinned on the CLI, implementer comes from the file,
    // and the critic falls through to [providers].order.
    let tmp = project("[roles]\nimplementer = \"api:openai\"\n");
    let v = plan_json(tmp.path(), &["--role", "planner=cli:codex"]);
    let seats = fleet(&v);
    let source_of = |role: &str| -> String {
        seats
            .iter()
            .find(|s| s["role"] == role)
            .map(|s| seat_field(s, "source"))
            .unwrap_or_else(|| panic!("no `{role}` seat in fleet {seats:?}"))
    };
    assert_eq!(source_of("planner"), "cli-role");
    assert_eq!(source_of("implementer"), "roles-file");
    assert_eq!(source_of("plan_critic"), "providers-order");

    // An explicit positional pool is the only thing that reports `cli-providers` — and
    // that must hold for every seat the pool covers, not just the first one. Regression
    // guard: `resolve_spec_provider` draws the test-author from `state.providers` once
    // no CLI role/`[roles].test_author` applies, and used to hardcode `providers-order`
    // for that fallback regardless of where the pool itself came from.
    let tmp2 = project("");
    let v2 = plan_json(
        tmp2.path(),
        &["--providers", "cli:codex,cli:claude,api:openai"],
    );
    let planner = fleet_seats(&v2, "planner");
    assert_eq!(planner.len(), 1);
    assert_eq!(seat_field(&planner[0], "source"), "cli-providers");
    let test_author = fleet_seats(&v2, "test_author");
    assert_eq!(test_author.len(), 1);
    assert_eq!(
        seat_field(&test_author[0], "source"),
        "cli-providers",
        "test-author drew from an explicit --providers pool, not [providers].order"
    );

    // Regression guard: the tester (suite) seat's dry-run fallback picks the first entry
    // of a hardcoded preference list, not `[providers].order` — it must say so.
    let tmp4 = project("");
    let v4 = implement_json(
        tmp4.path(),
        &["--providers", "cli:codex,cli:claude,api:openai"],
    );
    let tester = fleet_seats(&v4, "tester");
    assert_eq!(tester.len(), 1);
    assert_eq!(seat_field(&tester[0], "source"), "suite-preferences");

    // Regression guard: `run --workflow review` builds its reviewer panel outside
    // `roles_resolve` entirely and used to default every seat's source to
    // `providers-order` no matter how the pool was actually built.
    let tmp5 = project("");
    let v5 = review_json(tmp5.path(), &["--providers", "cli:codex,api:openai"]);
    let reviewers = fleet_seats(&v5, "reviewer");
    assert_eq!(reviewers.len(), 2);
    for r in &reviewers {
        assert_eq!(seat_field(r, "source"), "cli-providers");
    }

    // Whatever path a seat took, its source is a documented value.
    let tmp3 = project("[suite]\nenabled = false\n");
    let v3 = implement_json(tmp3.path(), &["--select", "auto"]);
    for seat in fleet(&v3) {
        let src = seat_field(&seat, "source");
        assert!(
            [
                "cli-role",
                "cli-providers",
                "roles-file",
                "providers-order",
                "model-select",
                "suite-preferences"
            ]
            .contains(&src.as_str()),
            "unknown seat source {src:?} in {seat}"
        );
    }
}

/// Regression: review round 7 found `peer`, `roles`, and `arena` labelling a seat drawn
/// from a cycled short pool as `unknown`, because they resolved sources against the raw
/// un-cycled `requested` pool while the dispatched provider came from `state.providers`
/// (already cycled by `pick_providers` to the workflow's slot count). A one-entry pool
/// must still label every seat `cli-providers`, not just the first.
#[test]
fn peer_roles_arena_label_a_cycled_short_pool_honestly() {
    let tmp = project("");
    let v = run_json(
        tmp.path(),
        &[
            "run",
            "--workflow",
            "peer",
            "-t",
            "p",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ],
        0,
    );
    for seat in fleet(&v) {
        assert_eq!(seat_field(&seat, "source"), "cli-providers", "{seat}");
    }

    let tmp2 = project("");
    let v2 = run_json(
        tmp2.path(),
        &[
            "run",
            "--workflow",
            "roles",
            "-t",
            "p",
            "--providers",
            "cli:codex",
            "--dry-run",
            "--json",
        ],
        0,
    );
    for seat in fleet(&v2) {
        assert_eq!(seat_field(&seat, "source"), "cli-providers", "{seat}");
    }

    let tmp3 = project("max_agents = 4\n");
    let v3 = run_json(
        tmp3.path(),
        &[
            "run",
            "--workflow",
            "arena",
            "-t",
            "p",
            "--providers",
            "cli:codex,api:openai",
            "--dry-run",
            "--json",
        ],
        2,
    );
    for seat in fleet(&v3) {
        assert_eq!(seat_field(&seat, "source"), "cli-providers", "{seat}");
    }
}

/// AC-15: a projected seat id equals the id of the slot the run later creates, so the
/// projection cannot drift from what gets dispatched.
#[test]
fn ac15_projected_seat_ids_match_the_created_slot_ids() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
planner = "cli:claude"
plan_critic = "cli:claude"
test_author = "cli:claude"
implementer = "cli:agy"
reviewer = ["cli:codex", "api:openai"]
"#,
    );
    let v = plan_json(tmp.path(), &[]);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let mut projected: Vec<String> = fleet(&v)
        .iter()
        .filter(|s| s["projected"] == Value::Bool(true))
        .filter(|s| s["role"] == "implementer" || s["role"] == "reviewer")
        .map(|s| seat_field(s, "seat"))
        .collect();
    projected.sort();
    assert!(!projected.is_empty());

    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_json(
        tmp.path(),
        &["implement", "--run", &run_id, "--dry-run", "--json"],
        2,
    );

    let state = read_state(tmp.path(), &run_id);
    let mut actual = slot_ids_for(&state, "implementer");
    actual.extend(slot_ids_for(&state, "reviewer"));
    actual.sort();
    assert_eq!(
        projected, actual,
        "projected seat ids must equal the ids the implement phase creates"
    );
}

/// AC-16: the human gate output prints an aligned `fleet:` table and keeps the existing
/// one-line `roles:` summary.
#[test]
fn ac16_human_gate_prints_a_fleet_table() {
    let tmp = project(
        r#"
[roles]
planner = "cli:claude"
plan_critic = "cli:codex"
test_author = "api:openai"
implementer = "cli:agy"
reviewer = ["cli:codex"]
"#,
    );
    let out = spar_cmd()
        .current_dir(tmp.path())
        .args(["plan", "--task", "add a hello function", "--dry-run"])
        .assert()
        .code(2);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    assert!(
        stdout.contains("fleet:"),
        "gate output must include a fleet table:\n{stdout}"
    );
    assert!(
        stdout.contains("roles:"),
        "the existing roles: line stays:\n{stdout}"
    );
    for expected in ["planner", "reviewer", "cli:codex", "source", "projected"] {
        assert!(
            stdout.contains(expected),
            "fleet table missing {expected:?}:\n{stdout}"
        );
    }
}

/// AC-17: the test author is a plan-map seat consumed by its own staged dispatch, not a
/// second planner-loop job. Exactly one test_author slot exists, and its id is the id the
/// fleet reported.
#[test]
fn ac17_test_author_is_one_staged_seat() {
    let tmp = project(
        r#"
[roles]
planner = "cli:claude"
plan_critic = "cli:codex"
test_author = "api:openai"
implementer = "cli:claude"
reviewer = ["cli:codex"]
"#,
    );
    let v = plan_json(tmp.path(), &[]);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let state = read_state(tmp.path(), &run_id);
    let ids = slot_ids_for(&state, "test_author");
    assert_eq!(ids.len(), 1, "exactly one test_author slot, got {ids:?}");

    let seats = fleet_seats(&v, "test_author");
    assert_eq!(seats.len(), 1, "one test_author seat in the fleet");
    assert_eq!(seat_field(&seats[0], "seat"), ids[0]);
    assert_eq!(seat_field(&seats[0], "provider"), "api:openai");
}

// ---------------------------------------------------------------------------
// Feature D: --without
// ---------------------------------------------------------------------------

/// AC-18: `--without critic` drops the plan_critic seat and the plan still reaches its
/// gate.
#[test]
fn ac18_without_critic_drops_the_critic_seat() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:claude", "--without", "critic"],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert!(
        !has_role(&state, "plan_critic"),
        "no plan_critic slot: {:?}",
        state["slots"]
    );
    assert!(has_role(&state, "planner"));
    assert_eq!(v["phase"], "awaiting_plan_approval");
}

/// AC-19: with the critic dropped, the spec protocol has no phantom `critic` recipient:
/// nothing is addressed to the literal id `critic`, and the test author is not told to
/// coordinate with one.
#[test]
fn ac19_without_critic_leaves_no_phantom_bus_recipient() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:claude", "--without", "critic"],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    for ev in bus_events(tmp.path()) {
        if ev["run"].as_str() != Some(run_id.as_str()) {
            continue;
        }
        assert_ne!(
            ev["to"].as_str(),
            Some("critic"),
            "a dropped critic must not be addressed on the bus: {ev}"
        );
        let body = ev["body"].as_str().unwrap_or_default();
        assert!(
            !body.contains("critic `"),
            "spec prose still names a critic that does not exist: {body}"
        );
    }
}

/// AC-20: `--without spec` drops the test author and writes no test contract.
#[test]
fn ac20_without_spec_drops_the_test_author() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:claude,cli:grok", "--without", "spec"],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let state = read_state(tmp.path(), &run_id);
    assert!(!has_role(&state, "test_author"));
    assert!(
        !tmp.path()
            .join(".spar/runs")
            .join(&run_id)
            .join("artifacts/test-contract.md")
            .exists(),
        "no test-contract.md when spec is dropped"
    );
}

/// AC-21: `--without suite` drops the agent tester seat.
#[test]
fn ac21_without_suite_drops_the_tester_seat() {
    let tmp = project("");
    let v = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--without",
            "suite",
        ],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert!(
        !has_role(&state, "tester"),
        "no tester slot: {:?}",
        state["slots"]
    );
}

/// AC-22: the three names compose in one flag.
#[test]
fn ac22_without_accepts_a_comma_list() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude",
            "--without",
            "critic,spec,suite",
        ],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert!(!has_role(&state, "plan_critic"));
    assert!(!has_role(&state, "test_author"));
    assert!(has_role(&state, "planner"));
}

/// AC-23: an unknown name is rejected, and the message names all valid values so
/// the operator can fix the flag without opening the docs. A bare `reviewer` is not
/// a seat: reviewer drops spell the ordinal (`reviewer[1]`).
#[test]
fn ac23_without_rejects_unknown_names() {
    let tmp = project("");
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "x",
            "--providers",
            "cli:claude",
            "--without",
            "reviewer",
            "--dry-run",
            "--json",
        ])
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("critic")
                .and(predicate::str::contains("spec"))
                .and(predicate::str::contains("suite"))
                .and(predicate::str::contains("reviewer[N]"))
                .and(predicate::str::contains("panic").not()),
        );
}

/// AC-24: `--without` is frozen with the run, so a continuation without
/// `--reload-config` still has the seat dropped.
#[test]
fn ac24_without_survives_into_the_next_round() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--without",
            "suite",
        ],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_json(
        tmp.path(),
        &["implement", "--run", &run_id, "--dry-run", "--json"],
        2,
    );
    let state = read_state(tmp.path(), &run_id);
    assert!(
        !has_role(&state, "tester"),
        "the frozen run config keeps suite disabled: {:?}",
        state["slots"]
    );
}

/// AC-25: `--without` on a bound run amends the frozen config without
/// `--reload-config`, exactly as `--role` does.
#[test]
fn ac25_without_amends_a_bound_run() {
    let tmp = project("");
    let v = plan_json(tmp.path(), &["--providers", "cli:claude,cli:grok,cli:agy"]);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "suite",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let suite = read_run_config(tmp.path(), &run_id)["suite"].clone();
    assert_eq!(
        suite["enabled"],
        serde_json::json!(false),
        "the amend drops the suite: {suite}"
    );
}

/// #234: `--without reviewer[1]` amends a bound run. The seat leaves the state, its
/// history entries and verdict artifacts go with it, and the next round parks
/// at ship confirm: the dropped seat alone held the gate, and now nothing does.
#[test]
fn without_reviewer_ordinal_drops_seat_and_its_findings() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = plan_json(tmp.path(), &["--providers", "cli:claude,cli:grok"]);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
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
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec!["review-0-cli-grok", "review-1-cli-claude"],
        "the panel under test: {state:?}"
    );
    liven_seat_findings(tmp.path(), &run_id, "review-1-cli-claude");
    let before = open_finding_slots(&status_json(tmp.path(), &run_id));
    assert_eq!(
        before,
        vec!["review-1-cli-claude"],
        "the gate counts the doomed seat's findings: {before:?}"
    );

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "reviewer[1]",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "dropped reviewer seat(s) review-1-cli-claude",
        ));

    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec!["review-0-cli-grok"],
        "the seat is gone: {:?}",
        state["slots"]
    );
    let history: Vec<&str> = state["review_history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slot_id"].as_str().unwrap())
        .collect();
    assert!(
        !history.contains(&"review-1-cli-claude"),
        "the seat's history goes with it: {history:?}"
    );
    for ext in ["md", "json"] {
        assert!(
            !tmp.path()
                .join(".spar/runs")
                .join(&run_id)
                .join(format!("artifacts/review-review-1-cli-claude.{ext}"))
                .exists(),
            "the seat's verdict artifacts go with it"
        );
    }
    let pin = read_run_config(tmp.path(), &run_id)["roles"]["reviewer"].clone();
    assert_eq!(
        pin,
        serde_json::json!(["cli:grok"]),
        "the frozen pin keeps the surviving seat: {pin}"
    );
    let after = open_finding_slots(&status_json(tmp.path(), &run_id));
    assert!(
        after.is_empty(),
        "the dropped seat's findings no longer gate: {after:?}"
    );
    // With the lone blocking seat gone, the next round clears the review gate
    // and parks at ship confirm: the dropped seat held the ship, and now
    // nothing does. The ceiling lifts past the rounds the forced-RC loop spent
    // hitting it.
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--max-rounds",
            "10",
            "--dry-run",
            "--json",
        ])
        .assert();
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        state["phase"], "awaiting_ship_confirm",
        "the gate clears once the dropped seat is gone: {:?}",
        state["phase"]
    );
}

/// #234: the same drop by seat id. Dropping `reviewer[0]` renumbers the survivor
/// down; its history entries keep the writing id (mapped in `former_ids`), and
/// its artifacts are renamed to the new id.
#[test]
fn without_reviewer_seat_id_drops_and_renumbers() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = plan_json(tmp.path(), &["--providers", "cli:claude,cli:grok"]);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
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

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "review-0-cli-grok",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "dropped reviewer seat(s) review-0-cli-grok",
        ));

    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec!["review-0-cli-claude"],
        "the survivor renumbers down: {:?}",
        state["slots"]
    );
    let history: Vec<&str> = state["review_history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slot_id"].as_str().unwrap())
        .collect();
    assert!(
        history.iter().all(|s| *s == "review-1-cli-claude"),
        "history keeps the id of the seat that wrote it: {history:?}"
    );
    let survivor = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "review-0-cli-claude")
        .unwrap();
    assert!(
        survivor["former_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "review-1-cli-claude"),
        "the renamed seat maps its old id: {:?}",
        survivor["former_ids"]
    );
    for ext in ["md", "json"] {
        assert!(
            tmp.path()
                .join(".spar/runs")
                .join(&run_id)
                .join(format!("artifacts/review-review-0-cli-claude.{ext}"))
                .is_file(),
            "the survivor's artifacts follow the rename"
        );
    }
    let pin = read_run_config(tmp.path(), &run_id)["roles"]["reviewer"].clone();
    assert_eq!(
        pin,
        serde_json::json!(["cli:claude"]),
        "the frozen pin keeps the surviving seat: {pin}"
    );
}

/// #234 (M1): a fresh-run `--without reviewer[1]` indexes the pool-derived panel,
/// not the provider order. With pool `[claude, codex, grok]` the implementer takes
/// pool position 0, so reviewer 1 is `cli:grok` and the frozen pin keeps `cli:codex`.
#[test]
fn without_reviewer_ordinal_indexes_the_pool_derived_panel() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = plan_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:codex,cli:grok",
            "--without",
            "reviewer[1]",
        ],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let cfg = read_run_config(tmp.path(), &run_id);
    assert_eq!(
        cfg["roles"]["reviewer"],
        serde_json::json!(["cli:codex"]),
        "the drop hits the pool-derived seat, not provider position 1: {:?}",
        cfg["roles"]["reviewer"]
    );
    assert!(
        cfg["cli_role_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "reviewer"),
        "the ordinal drop records the reviewer key: {:?}",
        cfg["cli_role_keys"]
    );
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    run_json(
        tmp.path(),
        &["implement", "--run", &run_id, "--dry-run", "--json"],
        2,
    );
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string()],
        "the continuation keeps the pool-derived survivor: {:?}",
        state["slots"]
    );
}

/// #234 (M2): a bound-run amend re-derives the reviewer pin from the live slots,
/// so a file pin edited in after the seats were cut is ignored.
#[test]
fn bound_amend_ignores_a_stale_file_reviewer_pin() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = plan_json(tmp.path(), &["--providers", "cli:claude,cli:grok"]);
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar_cmd()
        .current_dir(tmp.path())
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
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "reviewer[1]",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    std::fs::write(
        tmp.path().join("spar.toml"),
        "[roles]\nreviewer = [\"cli:codex\"]\n[suite]\nenabled = false\n",
    )
    .unwrap();
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--effort",
            "implementer=low",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec!["review-0-cli-grok"],
        "the live slots win over the file pin: {:?}",
        state["slots"]
    );
    let pin = read_run_config(tmp.path(), &run_id)["roles"]["reviewer"].clone();
    assert_eq!(
        pin,
        serde_json::json!(["cli:grok"]),
        "the frozen pin follows the slots, not the file: {pin}"
    );
}

/// #234 (M3): dropping the first seat shifts the survivor's per-ordinal backup
/// down, records the reviewer backup key, and `--reload-config` carries the
/// shifted list instead of re-reading the file's full-length list.
#[test]
fn reload_config_keeps_the_shifted_reviewer_backups() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = plan_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:codex,cli:grok",
            "--backup",
            "reviewer[0]=cli:agy@b0",
            "--backup",
            "reviewer[1]=cli:agy@b1",
        ],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:codex,cli:grok",
            "--dry-run",
            "--json",
        ])
        .assert();
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "reviewer[0]",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let cfg = read_run_config(tmp.path(), &run_id);
    assert_eq!(
        cfg["backups"]["reviewer"],
        serde_json::json!(["cli:agy@b1"]),
        "the survivor's backup shifts down: {:?}",
        cfg["backups"]["reviewer"]
    );
    assert!(
        cfg["cli_backup_keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "reviewer"),
        "the shift records the reviewer backup key: {:?}",
        cfg["cli_backup_keys"]
    );
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--reload-config",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let cfg = read_run_config(tmp.path(), &run_id);
    assert_eq!(
        cfg["backups"]["reviewer"],
        serde_json::json!(["cli:agy@b1"]),
        "the reload carries the shifted backups: {:?}",
        cfg["backups"]["reviewer"]
    );
    assert_eq!(
        cfg["roles"]["reviewer"],
        serde_json::json!(["cli:grok"]),
        "the reload keeps the surviving seat: {:?}",
        cfg["roles"]["reviewer"]
    );
}

/// #234 (M4): dropping the middle seat of a three-seat panel renumbers only the
/// seats above it, prunes the dropped seat's history, and pins the survivors.
#[test]
fn without_reviewer_middle_ordinal_drops_and_renumbers_above_only() {
    let tmp = project(
        "[roles]\nreviewer = [\"cli:codex\", \"cli:grok\", \"cli:agy\"]\n[suite]\nenabled = false\n",
    );
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:claude,cli:codex,cli:grok,cli:agy"],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:codex,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert();
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec![
            "review-0-cli-codex",
            "review-1-cli-grok",
            "review-2-cli-agy"
        ],
        "the panel under test: {:?}",
        state["slots"]
    );
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "reviewer[1]",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "dropped reviewer seat(s) review-1-cli-grok",
        ));
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec!["review-0-cli-codex", "review-1-cli-agy"],
        "only the seats above the drop renumber: {:?}",
        state["slots"]
    );
    let history: Vec<&str> = state["review_history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["slot_id"].as_str().unwrap())
        .collect();
    assert!(
        !history.contains(&"review-1-cli-grok"),
        "the dropped seat's history goes with it: {history:?}"
    );
    let survivor = state["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "review-1-cli-agy")
        .unwrap();
    assert!(
        survivor["former_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "review-2-cli-agy"),
        "the renamed seat maps its old id: {:?}",
        survivor["former_ids"]
    );
    let pin = read_run_config(tmp.path(), &run_id)["roles"]["reviewer"].clone();
    assert_eq!(
        pin,
        serde_json::json!(["cli:codex", "cli:agy"]),
        "the frozen pin keeps the survivors in order: {pin}"
    );
}

/// #234: an explicit `--role reviewer=...` replaces the panel, so when it lands
/// after a `--without reviewer[N]` drop the operator is warned the drop is lost.
#[test]
fn role_reviewer_pin_warns_when_it_discards_a_without_drop() {
    let tmp = project("[suite]\nenabled = false\n");
    let a = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "warn the drop",
            "--dry-run",
            "--json",
            "--providers",
            "cli:claude,cli:grok",
            "--role",
            "reviewer=cli:grok",
            "--without",
            "reviewer[1]",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "discarding the --without reviewer seat drop",
        ));
    let v: Value = serde_json::from_slice(&a.get_output().stdout).unwrap();
    let run_id = v["run_id"].as_str().unwrap();
    let pin = read_run_config(tmp.path(), run_id)["roles"]["reviewer"].clone();
    assert_eq!(
        pin,
        serde_json::json!(["cli:grok"]),
        "the explicit pin still wins: {pin}"
    );
}

/// #234: a survivor serving its engaged backup must not keep that backup entry.
/// The amend drops the spent same-ordinal backup (and records the backup key),
/// so the reload's backup-against-primary validation passes.
#[test]
fn reload_after_amend_disengages_a_spent_reviewer_backup() {
    let tmp = project("[suite]\nenabled = false\n");
    let v = plan_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--backup",
            "reviewer[0]=cli:codex@b0",
            "--backup",
            "reviewer[1]=cli:codex@b1",
        ],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    spar_cmd()
        .current_dir(tmp.path())
        .args(["approve", &run_id, "--json"])
        .assert()
        .success();
    let _ = spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--dry-run",
            "--json",
        ])
        .assert();
    // Fabricate the failover the dispatch would do when agy's seat dies: the
    // seat serves its backup under a backup source. The ordinal still reads
    // off the id, so the pin and the backup shift index it as before.
    let p = tmp
        .path()
        .join(".spar/runs")
        .join(&run_id)
        .join("state.json");
    let mut state: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let seat = state["slots"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["id"] == "review-1-cli-agy")
        .unwrap();
    seat["provider"] = serde_json::json!("cli:codex");
    seat["source"] = serde_json::json!("backup");
    std::fs::write(&p, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--without",
            "reviewer[0]",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let state = read_state(tmp.path(), &run_id);
    assert_eq!(
        slot_ids_for(&state, "reviewer"),
        vec!["review-0-cli-agy"],
        "the rotated survivor renumbers down keeping its id: {:?}",
        state["slots"]
    );
    let cfg = read_run_config(tmp.path(), &run_id);
    assert_eq!(
        cfg["roles"]["reviewer"],
        serde_json::json!(["cli:codex"]),
        "the pin names the serving provider: {:?}",
        cfg["roles"]["reviewer"]
    );
    assert!(
        cfg["backups"]["reviewer"].is_null(),
        "the spent backup is gone: {:?}",
        cfg["backups"]["reviewer"]
    );
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "implement",
            "--run",
            &run_id,
            "--reload-config",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(2);
    let cfg = read_run_config(tmp.path(), &run_id);
    assert!(
        cfg["backups"]["reviewer"].is_null(),
        "the reload carries the disengaged list: {:?}",
        cfg["backups"]["reviewer"]
    );
}

// ---------------------------------------------------------------------------
// Feature E: --fleet presets, composition, help
// ---------------------------------------------------------------------------

/// AC-26: `--fleet small` on plan drops the critic and the test author.
#[test]
fn ac26_fleet_small_drops_critic_and_spec_on_plan() {
    let tmp = project("");
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:claude", "--fleet", "small"],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert!(has_role(&state, "planner"));
    assert!(!has_role(&state, "plan_critic"));
    assert!(!has_role(&state, "test_author"));
}

/// AC-27: `--fleet small` on implement is one reviewer and no agent tester. The single
/// reviewer is the one the unpinned panel's first seat would have got, so `small` narrows
/// the panel without also re-homing it.
#[test]
fn ac27_fleet_small_is_one_reviewer_and_no_agent_tester() {
    let tmp = project("");
    let standard = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--fleet",
            "standard",
        ],
    );
    let standard_state = read_state(tmp.path(), standard["run_id"].as_str().unwrap());
    let standard_revs = providers_for(&standard_state, "reviewer");
    assert_eq!(standard_revs.len(), 2, "standard is today's two-seat panel");

    let small = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--fleet",
            "small",
        ],
    );
    let small_state = read_state(tmp.path(), small["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&small_state, "reviewer"),
        vec![standard_revs[0].clone()],
        "small keeps the first seat of the panel it shrank"
    );
    assert!(
        !has_role(&small_state, "tester"),
        "small drops the agent tester"
    );
}

/// AC-28: a configured `[suite].command` is deterministic and free, so `--fleet small`
/// keeps running it and only drops the agent tester.
#[test]
fn ac28_fleet_small_keeps_a_configured_builtin_suite() {
    let tmp = project("[suite]\ncommand = \"true\"\n");
    let v = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--fleet",
            "small",
        ],
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();
    let state = read_state(tmp.path(), &run_id);
    assert!(
        !has_role(&state, "tester"),
        "no agent tester under --fleet small"
    );
    assert!(
        tmp.path()
            .join(".spar/runs")
            .join(&run_id)
            .join("artifacts/suite.md")
            .exists(),
        "the deterministic suite still ran"
    );
}

/// AC-29: `--fleet standard` is a no-op over the file. It must not re-enable a channel
/// the project deliberately disabled.
#[test]
fn ac29_fleet_standard_does_not_re_enable_a_disabled_channel() {
    let tmp = project("[spec]\nenabled = false\n");
    let v = plan_json(
        tmp.path(),
        &["--providers", "cli:claude,cli:grok", "--fleet", "standard"],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert!(
        !has_role(&state, "test_author"),
        "standard preserves [spec] enabled = false"
    );
    assert!(has_role(&state, "plan_critic"));
}

/// AC-30: the explicit flags win over the preset. `--fleet small --role reviewer=A --role
/// reviewer=B` is a two-seat panel on A and B, because a reviewer list sets panel size
/// exactly.
#[test]
fn ac30_role_flags_override_the_small_preset_panel() {
    let tmp = project("");
    let v = implement_json(
        tmp.path(),
        &[
            "--providers",
            "cli:claude,cli:grok,cli:agy",
            "--fleet",
            "small",
            "--role",
            "reviewer=cli:codex",
            "--role",
            "reviewer=api:openai",
        ],
    );
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(
        providers_for(&state, "reviewer"),
        vec!["cli:codex".to_string(), "api:openai".to_string()]
    );
}

/// AC-31: `--fleet small` over a pinned multi-provider panel keeps the *first* pin.
#[test]
fn ac31_fleet_small_keeps_the_first_pinned_reviewer() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex", "api:openai"]
"#,
    );
    let v = implement_json(tmp.path(), &["--fleet", "small"]);
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    assert_eq!(providers_for(&state, "reviewer"), vec!["cli:codex"]);
}

/// AC-32: widening after `--fleet small` truncated a two-pin panel duplicates the pin
/// that was actually dispatched, rather than quietly restoring the pin `small` dropped.
#[test]
fn ac32_small_panel_widens_by_duplicating_the_dispatched_pin() {
    let tmp = project(
        r#"
[suite]
enabled = false
[roles]
implementer = "cli:agy"
reviewer = ["cli:codex", "api:openai"]
"#,
    );
    let out = spar_cmd()
        .current_dir(tmp.path())
        .env("SPAR_FORCE_REQUEST_CHANGES", "1")
        .args([
            "implement",
            "--task",
            "force stuck path",
            "--fleet",
            "small",
            "--max-rounds",
            "20",
            "--dry-run",
            "--json",
        ])
        .assert()
        .code(3);
    let stdout = String::from_utf8_lossy(out.get_output().stdout.as_slice()).to_string();
    let v: Value = serde_json::from_str(&stdout).unwrap();
    let state = read_state(tmp.path(), v["run_id"].as_str().unwrap());
    let revs = providers_for(&state, "reviewer");
    assert!(revs.len() > 1, "the ladder should have widened: {revs:?}");
    assert!(
        revs.iter().all(|p| p == "cli:codex"),
        "widening a small panel duplicates the dispatched pin: {revs:?}"
    );
}

/// AC-33: an unknown preset is rejected by name.
#[test]
fn ac33_fleet_rejects_an_unknown_preset() {
    let tmp = project("");
    spar_cmd()
        .current_dir(tmp.path())
        .args([
            "plan",
            "--task",
            "x",
            "--providers",
            "cli:claude",
            "--fleet",
            "deep",
            "--dry-run",
            "--json",
        ])
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("small")
                .and(predicate::str::contains("standard"))
                .and(predicate::str::contains("panic").not()),
        );
}

/// AC-34: `--help` is honest on all three run-shaping commands: `--providers` says it is
/// positional and that it overrides the file's roles, `--role` says a reviewer list sets
/// the panel size exactly, and the two new flags are documented.
#[test]
fn ac34_help_documents_the_pool_mapping_and_the_new_flags() {
    for cmd in ["plan", "implement", "run"] {
        let h = help_text(cmd);
        assert!(
            h.contains("positional"),
            "{cmd} --help must say --providers is positional:\n{h}"
        );
        assert!(
            h.contains("index 0"),
            "{cmd} --help must state the positional mapping (index 0 is the planner or \
             implementer, the rest are reviewers):\n{h}"
        );
        assert!(
            h.contains("panel size"),
            "{cmd} --help must say a --role reviewer list sets the panel size exactly:\n{h}"
        );
        assert!(
            h.contains("--fleet") && h.contains("small") && h.contains("standard"),
            "{cmd} --help must document --fleet small|standard:\n{h}"
        );
        assert!(
            h.contains("--without") && h.contains("critic") && h.contains("suite"),
            "{cmd} --help must document --without critic,spec,suite:\n{h}"
        );
    }
}

/// Not one of the 34 frozen criteria: this closes a review finding on a path the contract
/// does not reach. `spar reconcile` builds the reconciler and its review panel straight
/// out of `state.providers`, and stamped both with the pool's origin. A `Synth` pool is
/// built per position from `[roles]` *and* `[providers].order`, so that reported
/// `providers-order` for a seat whose provider came from `[roles].implementer`.
#[test]
fn reconcile_seats_report_the_rung_their_pool_came_from() {
    let tmp = project("[roles]\nimplementer = \"cli:codex\"\n");
    let v = run_json(
        tmp.path(),
        &[
            "run",
            "--workflow",
            "arena",
            "--task",
            "add a hello function",
            "--dry-run",
            "--json",
        ],
        2,
    );
    let run_id = v["run_id"].as_str().unwrap().to_string();

    let done = run_json(tmp.path(), &["reconcile", &run_id, "--json"], 2);
    let seats: Vec<Value> = fleet(&done)
        .into_iter()
        .filter(|s| seat_field(s, "seat").starts_with("reconcile"))
        .collect();
    assert!(
        seats.len() >= 2,
        "expected a reconciler and its review panel in the fleet: {:?}",
        fleet(&done)
    );
    for seat in &seats {
        assert_eq!(
            seat_field(seat, "provider"),
            "cli:codex",
            "reconcile seat drew from the [roles]-synthesized pool: {seat}"
        );
        assert_eq!(
            seat_field(seat, "source"),
            "roles-file",
            "reconcile seat must name the rung its pool entry came from, not the pool \
             origin: {seat}"
        );
    }
}
