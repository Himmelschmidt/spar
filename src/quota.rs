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

    /// Best-effort scan of log text for quota / rate-limit language. Deliberately
    /// broad: a false positive here only pauses a provider, which is cheap and
    /// auto-recovers (see `is_usable`). This alone must never decide whether a run
    /// routes to `Phase::Quota` — that decision needs [`scrape_strong_quota_signal`]
    /// or [`scrape_claude_rate_limits`], which require a rejection phrase rather than
    /// a single bare word like "quota" or "capacity" or "429".
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

    /// Narrow, line-scoped signal used only to decide whether a failed dispatch
    /// routes the *run* to `Phase::Quota`. Each pattern pairs the limit phrase with a
    /// rejection word on the same line, so a stack trace with a `429` in a line
    /// number, a test file that mentions "rate limiter", or an implementer editing
    /// this very module's "quota" code do not misroute a genuine defect as a quota
    /// hit — the failure mode a broad single-word match (`scrape_log_hint`) is prone
    /// to and that a routing decision cannot afford.
    pub fn scrape_strong_quota_signal(log: &str) -> Option<String> {
        for line in log.lines() {
            let lower = line.to_ascii_lowercase();
            let hit = (lower.contains("rate limit")
                && (lower.contains("rejected") || lower.contains("exceeded")))
                || (lower.contains("usage limit")
                    && (lower.contains("reached") || lower.contains("exceeded")))
                || (lower.contains("too many requests") && lower.contains("429"))
                || lower.contains("out of credits")
                || lower.contains("quota exceeded");
            if hit {
                return Some(line.trim().to_string());
            }
        }
        None
    }
}

/// Parse Claude-style `rate_limits.five_hour` JSON fragments from logs/statusline, or
/// (failing that) the CLI's plain-text weekly/five-hour rejection line, e.g. `! rate
/// limit  seven_day  rejected` followed by "You've hit your weekly limit · resets 12am
/// (America/New_York)". Returns (provider_name, cooldown_until, hint).
///
/// Gated on `provider` actually being a claude adapter: both shapes are specific to
/// the Claude CLI's own output, and a codex/grok/opencode log that happens to contain
/// "resets " next to a limit phrase must not pause `cli:claude` (a provider that is
/// fine) instead of, or in addition to, the one that actually failed.
pub fn scrape_claude_rate_limits(
    provider: &str,
    log: &str,
) -> Option<(String, Option<DateTime<Utc>>, String)> {
    if normalize_key(provider) != "cli:claude" {
        return None;
    }
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
    scrape_claude_stated_reset(log)
}

/// The CLI's plain-text rejection states its own reset ("resets 12am
/// (America/New_York)") rather than emitting the `five_hour` JSON shape above. Only
/// fires when the log both names a rate/usage limit and states a reset, so it never
/// fires on an unrelated log that happens to mention "rate limit" in passing. When the
/// stated reset can be found but not parsed (unknown tz, unexpected format), still
/// reports the hit so the caller pauses the provider — the fallback is the generic
/// default-cooldown pause, made visible on stderr rather than silently guessed.
pub(crate) fn scrape_claude_stated_reset(
    log: &str,
) -> Option<(String, Option<DateTime<Utc>>, String)> {
    let lower = log.to_ascii_lowercase();
    if !lower.contains("resets ") {
        return None;
    }
    if !(lower.contains("rate limit")
        || lower.contains("weekly limit")
        || lower.contains("usage limit"))
    {
        return None;
    }
    let period = if lower.contains("seven_day") || lower.contains("weekly") {
        "weekly"
    } else {
        "rate"
    };
    match parse_stated_reset(log, Utc::now()) {
        Some(until) => Some((
            "cli:claude".into(),
            Some(until),
            format!("claude {period} limit, resets {}", until.to_rfc3339()),
        )),
        None => {
            eprintln!(
                "warning: claude {period} limit stated a reset time spar could not parse; \
                 falling back to the default cooldown window"
            );
            Some((
                "cli:claude".into(),
                None,
                format!("claude {period} limit (stated reset unparseable, default cooldown)"),
            ))
        }
    }
}

/// Parses "resets 12am (America/New_York)" / "resets 3:30pm (UTC)" into the next
/// occurrence of that local time, in UTC. `now` is threaded through for tests.
///
/// The `(tz)` search is bounded to the line containing "resets " so an unrelated
/// parenthesis elsewhere in a multi-KB tail log can't be picked up as the timezone.
fn parse_stated_reset(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_ascii_lowercase();
    let resets_at = lower.find("resets ")? + "resets ".len();
    let line_end = text[resets_at..]
        .find('\n')
        .map(|i| resets_at + i)
        .unwrap_or(text.len());
    let after_resets = &text[resets_at..line_end];
    let paren_start = after_resets.find('(')?;
    let paren_end = after_resets[paren_start..].find(')')? + paren_start;
    let time_part = after_resets[..paren_start].trim();
    let tz_name = after_resets[paren_start + 1..paren_end].trim();
    let tz: chrono_tz::Tz = tz_name.parse().ok()?;

    let time_lower = time_part.to_ascii_lowercase();
    let (digits, is_pm) = if let Some(d) = time_lower.strip_suffix("am") {
        (d.trim(), false)
    } else {
        (time_lower.strip_suffix("pm")?.trim(), true)
    };
    let mut parts = digits.splitn(2, ':');
    let hour_raw: u32 = parts.next()?.trim().parse().ok()?;
    let minute: u32 = match parts.next() {
        Some(m) => m.trim().parse().ok()?,
        None => 0,
    };
    if !(1..=12).contains(&hour_raw) || minute > 59 {
        return None;
    }
    let hour = match (hour_raw, is_pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, false) => h,
        (h, true) => h + 12,
    };

    // Calendar-day roll-forward (not `+= Duration::days(1)` on the zoned instant,
    // which adds a flat 24h and drifts by an hour across a DST transition).
    let now_local = now.with_timezone(&tz);
    let mut date = now_local.date_naive();
    let mut candidate = date
        .and_hms_opt(hour, minute, 0)?
        .and_local_timezone(tz)
        .single()?;
    if candidate <= now_local {
        date += chrono::Duration::days(1);
        candidate = date
            .and_hms_opt(hour, minute, 0)?
            .and_local_timezone(tz)
            .single()?;
    }
    Some(candidate.with_timezone(&Utc))
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

    /// The real captured log text from the dogfooding incident (roadmap/BACKLOG.md).
    const WEEKLY_LIMIT_LOG: &str = "! rate limit  seven_day  rejected\n\
        You've hit your weekly limit \u{b7} resets 12am (America/New_York)\n";

    #[test]
    fn scrape_claude_rate_limits_parses_stated_weekly_reset() {
        let (name, until, hint) =
            scrape_claude_rate_limits("cli:claude", WEEKLY_LIMIT_LOG).unwrap();
        assert_eq!(name, "cli:claude");
        let until = until.expect("stated reset must parse into a cooldown");
        // "12am America/New_York" is midnight Eastern, which is 04:00 or 05:00 UTC
        // depending on DST; either way it must be within a day, not the ~30min default.
        let now = Utc::now();
        assert!(until > now, "reset must be in the future");
        assert!(
            until <= now + chrono::Duration::hours(25),
            "midnight ET is at most ~25h out, got {until}"
        );
        assert!(hint.contains("weekly"), "hint: {hint}");
    }

    #[test]
    fn scrape_claude_rate_limits_falls_back_when_reset_unparseable() {
        let log = "! rate limit  seven_day  rejected\n\
            You've hit your weekly limit \u{b7} resets whenever (Nowhere/Fake)\n";
        let (name, until, hint) = scrape_claude_rate_limits("cli:claude", log).unwrap();
        assert_eq!(name, "cli:claude");
        assert!(
            until.is_none(),
            "unparseable reset must fall back to the generic default cooldown, not guess"
        );
        assert!(hint.contains("unparseable"), "hint: {hint}");
    }

    #[test]
    fn scrape_claude_rate_limits_ignores_unrelated_logs() {
        // Must not fire on an ordinary log that happens to contain "resets" or
        // "rate limit" out of context — that would misroute a genuine failure.
        assert!(
            scrape_claude_rate_limits("cli:claude", "build succeeded, resets are fine").is_none()
        );
        assert!(
            scrape_claude_rate_limits("cli:claude", "connection rate limited by nginx").is_none()
        );
    }

    #[test]
    fn scrape_claude_rate_limits_ignores_non_claude_providers() {
        // The plain-text stated-reset shape is Claude-specific output; a codex/grok
        // log that happens to say "resets " next to a limit phrase must not pause
        // `cli:claude`, a provider that has nothing to do with the failure.
        assert!(scrape_claude_rate_limits("cli:codex", WEEKLY_LIMIT_LOG).is_none());
        assert!(scrape_claude_rate_limits("cli:grok", WEEKLY_LIMIT_LOG).is_none());
    }

    #[test]
    fn parse_stated_reset_rolls_to_next_day_when_already_past() {
        // Fixed "now" well past midnight ET: the next weekly reset is tomorrow, not today.
        let now = DateTime::parse_from_rfc3339("2026-09-04T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let until = parse_stated_reset(
            "You've hit your weekly limit \u{b7} resets 12am (America/New_York)",
            now,
        )
        .unwrap();
        assert!(until > now);
        assert!(until - now < chrono::Duration::hours(9));
    }

    #[test]
    fn strong_quota_signal_matches_the_real_incident_line() {
        assert!(QuotaStore::scrape_strong_quota_signal(WEEKLY_LIMIT_LOG).is_some());
        assert!(
            QuotaStore::scrape_strong_quota_signal("429 Too Many Requests from upstream").is_some()
        );
        assert!(QuotaStore::scrape_strong_quota_signal("usage limit reached, try later").is_some());
        assert!(QuotaStore::scrape_strong_quota_signal("account is out of credits").is_some());
    }

    /// Four realistic non-quota failures that each contain one of `scrape_log_hint`'s
    /// broad needles. Driven, not read: before this fix each of these misrouted a real
    /// defect onto the quota gate.
    #[test]
    fn strong_quota_signal_ignores_realistic_false_positives() {
        let panic_with_429 = "thread 'main' panicked at src/state.rs:429:\nindex out of bounds";
        let quota_module_edit = "implementer: refactoring QuotaStore::pause_quota in src/quota.rs";
        let rate_limiter_under_test =
            "test: app rate limiter rejects requests after threshold ... FAILED\nassertion failed";
        let capacity_doc_edit = "updated docs/capacity-planning.md with Q3 projections";
        for log in [
            panic_with_429,
            quota_module_edit,
            rate_limiter_under_test,
            capacity_doc_edit,
        ] {
            assert!(
                QuotaStore::scrape_strong_quota_signal(log).is_none(),
                "false positive on: {log}"
            );
        }
    }
}
