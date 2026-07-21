use crate::paths::SparPaths;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A pause with no explicit `cooldown_until` auto-recovers this long after it was
/// set: the provider is re-probed by the next run rather than staying dead forever.
/// If it is still rate-limited the run re-pauses it with a fresh window.
const DEFAULT_COOLDOWN_MINS: i64 = 30;

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
            ProviderStatus::Cooldown => q.cooldown_until.is_none_or(|until| Utc::now() >= until),
            // Pauses auto-recover: an explicit `cooldown_until` wins, otherwise the
            // pause lapses DEFAULT_COOLDOWN_MINS after it was set so the provider is
            // retried instead of staying unusable indefinitely.
            ProviderStatus::PausedManual | ProviderStatus::PausedQuota => match q.cooldown_until {
                Some(until) => Utc::now() >= until,
                None => q.updated_at.is_some_and(|set| {
                    Utc::now() >= set + chrono::Duration::minutes(DEFAULT_COOLDOWN_MINS)
                }),
            },
        }
    }

    /// Status as a run would see it: a pause that has lapsed (auto-recovered) reads
    /// `Available`, so `provider list` matches what `plan`/`implement` will do rather
    /// than showing a stale `Paused*` for a provider a run will happily pick up.
    pub fn effective_status(&self, name: &str) -> ProviderStatus {
        if self.is_usable(name) {
            ProviderStatus::Available
        } else {
            self.get(name).status
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

/// Canonical quota bucket key. Model-free (`cli:claude@sonnet` and `cli:claude@haiku`
/// share one bucket — rate limits are per account) and prefix-normalized, so a bare
/// `claude` from the CLI or `provider list` maps to the same `cli:claude` bucket that
/// slot providers and the auto-pause path write. Callers on both sides must go through
/// this or the store keys silently disagree.
pub fn normalize_key(raw: &str) -> String {
    let candidate = if raw.contains(':') {
        raw.to_string()
    } else {
        format!("cli:{raw}")
    };
    crate::provider_ref::ProviderRef::parse(&candidate)
        .map(|p| p.storage_key())
        .unwrap_or_else(|_| raw.to_string())
}

pub fn filter_usable(names: &[String], store: &QuotaStore) -> Vec<String> {
    names
        .iter()
        .filter(|n| store.is_usable(&normalize_key(n)))
        .cloned()
        .collect()
}

/// Drop paused providers. Returns empty when every named provider is unusable
/// (caller should exit with `ExitCode::Quota` rather than re-enabling them).
///
/// Only safe for a *pool* of interchangeable slots (e.g. arena competitors). For a
/// positional, role-keyed fleet use [`ensure_usable`] — dropping an entry there would
/// reindex the fleet and slide a different model into a role's slot.
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

/// Gate a positional fleet in place: role→slot assignment maps by index, so a paused
/// provider must fail the run loud rather than be dropped (which would collapse the
/// per-role fleet onto one model silently). Errors naming the paused providers.
pub fn ensure_usable(paths: &SparPaths, names: &[String]) -> Result<()> {
    let store = QuotaStore::load(paths).unwrap_or_default();
    let mut paused: Vec<String> = Vec::new();
    for n in names {
        let key = normalize_key(n);
        if !store.is_usable(&key) && !paused.contains(&key) {
            paused.push(key);
        }
    }
    if !paused.is_empty() {
        bail!(
            "provider(s) paused or on cooldown: {}. resume with `spar provider resume <name>` or reassign the role",
            paused.join(", ")
        );
    }
    Ok(())
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

    #[test]
    fn pause_auto_recovers_after_cooldown() {
        // A pause with no explicit cooldown lapses once DEFAULT_COOLDOWN_MINS has
        // passed since it was set: the provider is re-probed, not dead forever.
        let mut store = QuotaStore::default();
        store.pause_manual("cli:claude", None);
        assert!(!store.is_usable("cli:claude"), "fresh pause is unusable");

        let stale = Utc::now() - chrono::Duration::minutes(DEFAULT_COOLDOWN_MINS + 1);
        store.providers.get_mut("cli:claude").unwrap().updated_at = Some(stale);
        assert!(
            store.is_usable("cli:claude"),
            "pause older than the cooldown auto-recovers"
        );
    }

    #[test]
    fn ensure_usable_names_paused_without_reordering() {
        // The positional fleet gate must fail loud naming the paused provider, never
        // silently drop it (which would collapse per-role assignment onto one model).
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut store = QuotaStore::default();
        store.pause_manual("cli:claude", None);
        store.save(&paths).unwrap();
        let fleet = vec!["cli:grok".into(), "cli:claude".into(), "cli:grok".into()];
        let err = ensure_usable(&paths, &fleet).unwrap_err();
        assert!(err.to_string().contains("cli:claude"));
    }

    #[test]
    fn effective_status_reflects_auto_recovery() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:claude", None);
        assert_eq!(
            store.effective_status("cli:claude"),
            ProviderStatus::PausedManual,
            "fresh pause shows its real status"
        );

        let stale = Utc::now() - chrono::Duration::minutes(DEFAULT_COOLDOWN_MINS + 1);
        store.providers.get_mut("cli:claude").unwrap().updated_at = Some(stale);
        assert_eq!(
            store.effective_status("cli:claude"),
            ProviderStatus::Available,
            "a lapsed pause reads Available, matching what a run will do"
        );
    }

    #[test]
    fn ensure_usable_passes_when_all_available() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let fleet = vec!["cli:grok".into(), "cli:claude".into()];
        assert!(ensure_usable(&paths, &fleet).is_ok());
    }

    #[test]
    fn normalize_key_bare_and_prefixed_match() {
        // `provider list` (bare "claude"), the CLI arg, and slot providers must all
        // resolve to the same bucket the auto-pause path writes.
        assert_eq!(normalize_key("claude"), "cli:claude");
        assert_eq!(normalize_key("cli:claude"), "cli:claude");
        assert_eq!(normalize_key("cli:claude@opus"), "cli:claude");
        assert_eq!(normalize_key("api:openai"), "api:openai");
    }
}
