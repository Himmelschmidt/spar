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
use assert_cmd::cargo::cargo_bin_cmd;
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
    let mut c = cargo_bin_cmd!("spar");
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

    // An explicit positional pool is the only thing that reports `cli-providers`.
    let tmp2 = project("");
    let v2 = plan_json(tmp2.path(), &["--providers", "cli:claude,cli:grok"]);
    let planner = fleet_seats(&v2, "planner");
    assert_eq!(planner.len(), 1);
    assert_eq!(seat_field(&planner[0], "source"), "cli-providers");

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

/// AC-23: an unknown name is rejected, and the message names all three valid values so
/// the operator can fix the flag without opening the docs.
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
        .stderr(
            predicate::str::contains("critic")
                .and(predicate::str::contains("spec"))
                .and(predicate::str::contains("suite"))
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

/// AC-25: `--without` on an existing run without `--reload-config` is refused, exactly as
/// `--role` is. A run is bound to the config it was created with (O27).
#[test]
fn ac25_without_is_refused_on_a_bound_run() {
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
        .code(1)
        .stderr(predicate::str::contains("--reload-config"));
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
        .stderr(
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
