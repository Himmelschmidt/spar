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
    /// role name → profile name
    #[serde(default)]
    pub roles: std::collections::HashMap<String, String>,
}

impl Default for ModelSelectConfig {
    fn default() -> Self {
        Self {
            source: default_model_select_source(),
            benches: default_model_select_benches(),
            cache_ttl_secs: default_model_select_ttl(),
            allow: Vec::new(),
            profiles: crate::model_select::default_profiles(),
            roles: default_model_select_roles(),
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
        self.roles
            .get(role)
            .map(|s| s.as_str())
            .unwrap_or(match role {
                "planner" | "critic" => "best",
                "tester" => "fast",
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

fn default_model_select_roles() -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    m.insert("planner".into(), "best".into());
    m.insert("critic".into(), "best".into());
    m.insert("implementer".into(), "value".into());
    m.insert("reviewer".into(), "value".into());
    m.insert("tester".into(), "fast".into());
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
    /// Prefer a cheap provider (`cli:claude`, `cli:grok`, `api:xai`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default = "default_suite_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for SuiteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            timeout_secs: default_suite_timeout_secs(),
        }
    }
}

fn default_suite_timeout_secs() -> u64 {
    7200
}

/// Pre-coding test-author channel (plan → before human gate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Prefer a third provider distinct from planner/critic when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default = "default_spec_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for SpecConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
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
    roles: Option<std::collections::HashMap<String, String>>,
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
    provider: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SpecConfigFile {
    enabled: Option<bool>,
    provider: Option<String>,
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
            if let Some(v) = &s.provider {
                self.suite.provider = Some(v.clone());
            }
            if let Some(v) = s.timeout_secs {
                self.suite.timeout_secs = v;
            }
        }
        if let Some(s) = &file.spec {
            if let Some(v) = s.enabled {
                self.spec.enabled = v;
            }
            if let Some(v) = &s.provider {
                self.spec.provider = Some(v.clone());
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
            if let Some(v) = &ms.roles {
                for (k, role) in v {
                    self.model_select.roles.insert(k.clone(), role.clone());
                }
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
provider = "cli:grok"
timeout_secs = 3600

[spec]
enabled = false
provider = "cli:agy"
timeout_secs = 900
"#,
        )
        .unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(cfg.timeouts.slot_secs, 100);
        assert_eq!(cfg.timeouts.review_secs(), 200);
        assert!(!cfg.suite.enabled);
        assert_eq!(cfg.suite.provider.as_deref(), Some("cli:grok"));
        assert_eq!(cfg.suite.timeout_secs, 3600);
        assert!(!cfg.spec.enabled);
        assert_eq!(cfg.spec.provider.as_deref(), Some("cli:agy"));
        assert_eq!(cfg.spec.timeout_secs, 900);
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
