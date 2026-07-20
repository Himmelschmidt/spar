use crate::paths::SparPaths;
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
    pub fn load(paths: &SparPaths) -> Result<Self> {
        paths.ensure_swarm_root()?;
        let file = paths.quota_file();
        if !file.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, paths: &SparPaths) -> Result<()> {
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

    pub fn pause_quota_until(
        &mut self,
        name: &str,
        until: Option<DateTime<Utc>>,
        hint: impl Into<String>,
    ) {
        self.providers.insert(
            name.into(),
            ProviderQuota {
                name: name.into(),
                status: if until.is_some() {
                    ProviderStatus::Cooldown
                } else {
                    ProviderStatus::PausedQuota
                },
                cooldown_until: until,
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
            "five_hour",
            "rate_limits",
        ];
        for n in needles {
            if lower.contains(n) {
                return Some(format!("possible quota signal: {n}"));
            }
        }
        None
    }
}

/// Parse Claude-style `rate_limits.five_hour` JSON fragments from logs/statusline.
/// Returns (provider_name, cooldown_until, hint).
pub fn scrape_claude_rate_limits(log: &str) -> Option<(String, Option<DateTime<Utc>>, String)> {
    // Look for embedded JSON objects containing rate_limits
    for line in log.lines() {
        let t = line.trim();
        if !(t.contains("rate_limits") || t.contains("five_hour")) {
            continue;
        }
        // try whole line as JSON
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            if let Some(hit) = parse_rate_limits_value(&v) {
                return Some(hit);
            }
        }
        // scan for JSON object substrings
        if let Some(start) = t.find('{') {
            if let Some(end) = t.rfind('}') {
                if end > start {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[start..=end]) {
                        if let Some(hit) = parse_rate_limits_value(&v) {
                            return Some(hit);
                        }
                    }
                }
            }
        }
    }
    None
}

fn parse_rate_limits_value(
    v: &serde_json::Value,
) -> Option<(String, Option<DateTime<Utc>>, String)> {
    let rl = v
        .get("rate_limits")
        .or_else(|| v.get("status").and_then(|s| s.get("rate_limits")))?;
    let five = rl.get("five_hour")?;
    let used = five
        .get("used_percentage")
        .and_then(|x| x.as_f64())
        .or_else(|| five.get("used_percent").and_then(|x| x.as_f64()))?;
    if used < 95.0 {
        return None;
    }
    let until = five
        .get("resets_at")
        .and_then(|x| x.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    Some((
        "cli:claude".into(),
        until,
        format!("claude five_hour used_percentage={used}"),
    ))
}

/// Model-free quota bucket key: `cli:claude@sonnet` and `cli:claude@haiku`
/// share one bucket (rate limits are per account, not per model).
fn quota_key(raw: &str) -> String {
    crate::provider_ref::ProviderRef::parse(raw)
        .map(|p| p.storage_key())
        .unwrap_or_else(|_| raw.to_string())
}

pub fn filter_usable(names: &[String], store: &QuotaStore) -> Vec<String> {
    names
        .iter()
        .filter(|n| store.is_usable(&quota_key(n)))
        .cloned()
        .collect()
}

/// Drop paused providers. Returns empty when every named provider is unusable
/// (caller should exit with `ExitCode::Quota` rather than re-enabling them).
pub fn apply_quota_filter(paths: &SparPaths, names: &[String]) -> Result<Vec<String>> {
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
        let paths = SparPaths::new(tmp.path());
        let mut store = QuotaStore::default();
        store.pause_manual("cli:claude", None);
        store.save(&paths).unwrap();
        let loaded = QuotaStore::load(&paths).unwrap();
        assert!(!loaded.is_usable("cli:claude"));
        let mut loaded = loaded;
        loaded.resume("cli:claude");
        assert!(loaded.is_usable("cli:claude"));
    }

    #[test]
    fn model_variants_share_bucket() {
        // Pausing the bare provider filters out its @model variants: the model
        // must not leak into the quota key.
        let mut store = QuotaStore::default();
        store.pause_manual("cli:claude", None);
        let kept = filter_usable(&["cli:claude@sonnet".into(), "cli:grok".into()], &store);
        assert_eq!(kept, vec!["cli:grok".to_string()]);
    }

    #[test]
    fn filter_empty_errors() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = QuotaStore::default();
        store.pause_manual("cli:claude", None);
        store.pause_manual("cli:grok", None);
        store.save(&paths).unwrap();
        let err =
            apply_quota_filter(&paths, &["cli:claude".into(), "cli:grok".into()]).unwrap_err();
        assert!(err.to_string().contains("no usable providers"));
    }
}
