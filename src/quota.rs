use crate::paths::SwarmPaths;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Available,
    PausedManual,
    PausedQuota,
    Cooldown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderQuota {
    pub name: String,
    pub status: ProviderStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaStore {
    #[serde(default)]
    pub providers: HashMap<String, ProviderQuota>,
}

impl QuotaStore {
    pub fn load(paths: &SwarmPaths) -> Result<Self> {
        paths.ensure_swarm_root()?;
        let file = paths.quota_file();
        if !file.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, paths: &SwarmPaths) -> Result<()> {
        paths.ensure_swarm_root()?;
        let file = paths.quota_file();
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> ProviderQuota {
        self.providers.get(name).cloned().unwrap_or(ProviderQuota {
            name: name.into(),
            status: ProviderStatus::Available,
            cooldown_until: None,
            hint: None,
            updated_at: None,
        })
    }

    pub fn is_usable(&self, name: &str) -> bool {
        let q = self.get(name);
        match q.status {
            ProviderStatus::Available => true,
            ProviderStatus::Cooldown => {
                if let Some(until) = q.cooldown_until {
                    Utc::now() >= until
                } else {
                    true
                }
            }
            ProviderStatus::PausedManual | ProviderStatus::PausedQuota => false,
        }
    }

    pub fn pause_manual(&mut self, name: &str, until: Option<DateTime<Utc>>) {
        let status = if until.is_some() {
            ProviderStatus::Cooldown
        } else {
            ProviderStatus::PausedManual
        };
        self.providers.insert(
            name.into(),
            ProviderQuota {
                name: name.into(),
                status,
                cooldown_until: until,
                hint: Some("manual pause".into()),
                updated_at: Some(Utc::now()),
            },
        );
    }

    pub fn pause_quota(&mut self, name: &str, hint: impl Into<String>) {
        self.providers.insert(
            name.into(),
            ProviderQuota {
                name: name.into(),
                status: ProviderStatus::PausedQuota,
                cooldown_until: None,
                hint: Some(hint.into()),
                updated_at: Some(Utc::now()),
            },
        );
    }

    pub fn resume(&mut self, name: &str) {
        self.providers.insert(
            name.into(),
            ProviderQuota {
                name: name.into(),
                status: ProviderStatus::Available,
                cooldown_until: None,
                hint: None,
                updated_at: Some(Utc::now()),
            },
        );
    }

    /// Best-effort scan of log text for quota / rate-limit language.
    pub fn scrape_log_hint(log: &str) -> Option<String> {
        let lower = log.to_ascii_lowercase();
        let needles = [
            "rate limit",
            "quota",
            "usage limit",
            "too many requests",
            "429",
            "out of credits",
            "billing",
            "capacity",
        ];
        for n in needles {
            if lower.contains(n) {
                return Some(format!("possible quota signal: {n}"));
            }
        }
        None
    }
}

pub fn filter_usable(names: &[String], store: &QuotaStore) -> Vec<String> {
    names
        .iter()
        .filter(|n| store.is_usable(n))
        .cloned()
        .collect()
}

/// Drop paused providers. Returns empty when every named provider is unusable
/// (caller should exit with `ExitCode::Quota` rather than re-enabling them).
pub fn apply_quota_filter(paths: &SwarmPaths, names: &[String]) -> Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let store = QuotaStore::load(paths).unwrap_or_default();
    let filtered = filter_usable(names, &store);
    if filtered.is_empty() {
        bail!("no usable providers (all paused or on quota cooldown)");
    }
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pause_resume() {
        let tmp = tempdir().unwrap();
        let paths = SwarmPaths::new(tmp.path());
        let mut store = QuotaStore::default();
        store.pause_manual("claude", None);
        store.save(&paths).unwrap();
        let loaded = QuotaStore::load(&paths).unwrap();
        assert!(!loaded.is_usable("claude"));
        let mut loaded = loaded;
        loaded.resume("claude");
        assert!(loaded.is_usable("claude"));
    }

    #[test]
    fn filter_empty_errors() {
        let tmp = tempdir().unwrap();
        let paths = SwarmPaths::new(tmp.path());
        let mut store = QuotaStore::default();
        store.pause_manual("claude", None);
        store.pause_manual("grok", None);
        store.save(&paths).unwrap();
        let err = apply_quota_filter(&paths, &["claude".into(), "grok".into()]).unwrap_err();
        assert!(err.to_string().contains("no usable providers"));
    }
}
