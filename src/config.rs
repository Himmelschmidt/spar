use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// User/project config. Project `agent-swarm.toml` field-overlays user config.
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
    #[serde(default = "default_wait_timeout")]
    pub wait: String,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            slot_secs: default_slot_timeout_secs(),
            wait: default_wait_timeout(),
        }
    }
}

fn default_max_agents() -> u32 {
    4
}

fn default_provider_order() -> Vec<String> {
    vec!["claude".into(), "grok".into(), "agy".into()]
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
        }
    }
}

/// Partial file shape so project/user files overlay only set keys.
#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigFile {
    max_agents: Option<u32>,
    default_backend: Option<crate::cli::Backend>,
    isolation: Option<IsolationMode>,
    providers: Option<ProviderConfigFile>,
    ship: Option<ShipConfigFile>,
    timeouts: Option<TimeoutConfigFile>,
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
    wait: Option<String>,
}

impl Config {
    pub fn load(project_root: &Path) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(user_path) = user_config_path() {
            if user_path.is_file() {
                cfg.apply_file(&load_file(&user_path)?)?;
            }
        }
        let project_path = project_root.join("agent-swarm.toml");
        if project_path.is_file() {
            cfg.apply_file(&load_file(&project_path)?)?;
        }
        Ok(cfg)
    }

    fn apply_file(&mut self, file: &ConfigFile) -> Result<()> {
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
            if let Some(v) = &t.wait {
                self.timeouts.wait = v.clone();
            }
        }
        Ok(())
    }
}

fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("agent-swarm").join("config.toml"))
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
        std::fs::write(project.join("agent-swarm.toml"), "max_agents = 2\n").unwrap();
        let cfg = Config::load(project).unwrap();
        assert_eq!(cfg.max_agents, 2);
        assert_eq!(cfg.providers.order, default_provider_order());
        assert!(!cfg.ship.auto_confirm);
    }
}
