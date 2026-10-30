use crate::paths::SparPaths;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A quota pause with no explicit `cooldown_until` auto-recovers this long after it
/// was set: the provider is re-probed by the next run rather than staying dead forever.
/// If it is still rate-limited the run re-pauses it with a fresh window. Manual pauses
/// never lapse on this timer (O105).
const DEFAULT_COOLDOWN_MINS: i64 = 30;

/// `lower.contains(phrase)` but only accepts a word boundary immediately after the
/// match, or the plural/past-tense inflections "s"/"ed" followed by a word boundary
/// ("rate limits", "rate limited"). Rejects "er"/"ing" continuations ("rate limiter",
/// "rate limiting") and anything else — an ordinary implementer building or testing a
/// rate limiter must not trip the same discriminator that routes a run to
/// `Phase::Quota`, but a provider reporting "rate limits exceeded" must.
fn contains_phrase(lower: &str, phrase: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = lower[start..].find(phrase) {
        let idx = start + pos;
        let after = &lower[idx + phrase.len()..];
        if word_boundary_after_phrase(after) {
            return true;
        }
        start = idx + phrase.len();
    }
    false
}

/// A 402 only where the line frames it as an HTTP status, never a bare number: a
/// `402` in a line number, a count or a path would otherwise read as a billing stop.
fn names_http_402(lower: &str) -> bool {
    [
        "http 402",
        "status 402",
        "status: 402",
        "status code 402",
        "status code: 402",
        "\"status\":402",
        "\"status\": 402",
        "\"code\":402",
        "\"code\": 402",
        "error 402",
        "error: 402",
    ]
    .iter()
    .any(|p| {
        lower.match_indices(p).any(|(i, _)| {
            !lower[i + p.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    })
}

fn word_boundary_after_phrase(after: &str) -> bool {
    let mut chars = after.chars();
    match chars.next() {
        None => true,
        Some(c) if !c.is_ascii_alphabetic() => true,
        Some('s') => chars.next().is_none_or(|c| !c.is_ascii_alphabetic()),
        Some('e') => {
            chars.next() == Some('d') && chars.next().is_none_or(|c| !c.is_ascii_alphabetic())
        }
        _ => false,
    }
}

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
    /// Written `true` by `provider pause`: the pause holds until its `--until` or
    /// `provider resume` (O105). A `paused_manual` record without it predates that rule
    /// and keeps the old auto-lapse, so pauses operators set expecting it to lapse
    /// never start blocking a provider when this ships. Absence is the rule; no backfill.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sticky: bool,
    /// Set when the pause came from a rejection spar can trust: an adapter's typed
    /// verdict or a line-scoped strong signal (`mark_strong`). A pause written only by
    /// `scrape_log_hint`'s broad needles ("429" in "429 passed") is weak: it still
    /// steers the next run's seat choice, but never parks or reseats a live run
    /// (O106). Legacy records read weak.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strong: bool,
    /// Per-account write counter (O141): every write is one past the highest version
    /// this box had seen for the key, so "written after seeing that pause" is decided by
    /// the counter, not by two boxes' clocks. 0 on records that predate it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub version: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl ProviderQuota {
    fn available(name: &str) -> Self {
        Self {
            name: name.into(),
            status: ProviderStatus::Available,
            cooldown_until: None,
            hint: None,
            updated_at: None,
            sticky: false,
            strong: false,
            version: 0,
        }
    }

    /// When this record stops holding the provider; `None` when it holds until resumed.
    fn lapses_at(&self) -> Option<DateTime<Utc>> {
        match self.status {
            ProviderStatus::Available => Some(DateTime::<Utc>::MIN_UTC),
            ProviderStatus::Cooldown => {
                Some(self.cooldown_until.unwrap_or(DateTime::<Utc>::MIN_UTC))
            }
            // An operator's pause holds until its own `--until`, or until `provider
            // resume` when it has none: the operator said stop, and a timer that
            // quietly undoes that is how a removed provider got dispatched again.
            ProviderStatus::PausedManual if self.sticky => self.cooldown_until,
            ProviderStatus::PausedManual | ProviderStatus::PausedQuota => {
                self.cooldown_until.or_else(|| {
                    self.updated_at
                        .map(|set| set + chrono::Duration::minutes(DEFAULT_COOLDOWN_MINS))
                })
            }
        }
    }

    fn holds(&self, now: DateTime<Utc>) -> bool {
        self.lapses_at().is_none_or(|at| now < at)
    }
}

/// Combine two records for one account (O141). A pause that has lapsed counts for
/// nothing. Between two live pauses, a manual one beats a quota one, the longest-lived
/// manual pause wins, and among quota pauses the strong one wins, then the longest.
/// Between a live pause and `available`, the higher `version` wins: a `provider resume`
/// written after its box saw the pause clears it fleet-wide, and a pause written after
/// its box saw the resume holds. At equal versions (concurrent writes, or records from
/// before the counter) the resume wins unless the pause is newer by more than the clock
/// skew allowance. The result carries the higher version.
pub fn merge(a: ProviderQuota, b: ProviderQuota, now: DateTime<Utc>) -> ProviderQuota {
    let version = a.version.max(b.version);
    let skew = chrono::Duration::seconds(crate::fleet::CLOCK_SKEW_SECS);
    let resume_wins = |pause: &ProviderQuota, other: &ProviderQuota| {
        other.status == ProviderStatus::Available
            && match other.version.cmp(&pause.version) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => match (pause.updated_at, other.updated_at) {
                    (Some(p), Some(r)) => p <= r + skew,
                    (_, r) => r.is_some(),
                },
            }
    };
    let mut out = match (a.holds(now), b.holds(now)) {
        (false, false) => {
            if (b.version, b.updated_at) > (a.version, a.updated_at) {
                b
            } else {
                a
            }
        }
        (true, false) if resume_wins(&a, &b) => b,
        (true, false) => a,
        (false, true) if resume_wins(&b, &a) => a,
        (false, true) => b,
        (true, true) => {
            let manual = |q: &ProviderQuota| q.status == ProviderStatus::PausedManual;
            let longest = |q: &ProviderQuota| q.lapses_at().unwrap_or(DateTime::<Utc>::MAX_UTC);
            let rank = |q: &ProviderQuota| (manual(q), manual(q) || q.strong, longest(q));
            if rank(&b) > rank(&a) {
                b
            } else {
                a
            }
        }
    };
    out.version = version;
    out
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaStore {
    #[serde(default)]
    pub providers: HashMap<String, ProviderQuota>,
    /// The fleet file's records for this box's accounts, by storage key (O141). Read on
    /// load and merged into every answer, never written to the project file.
    #[serde(skip)]
    fleet: HashMap<String, ProviderQuota>,
    /// Keys this process changed, which `save` merges into the fleet file.
    #[serde(skip)]
    touched: std::collections::BTreeSet<String>,
    /// Pauses made since load, appended to `quota-events.jsonl` by `save`.
    #[serde(skip)]
    pending: Vec<QuotaEvent>,
}

pub const QUOTA_EVENTS_SCHEMA: u32 = 1;

/// One line of `.spar/quota-events.jsonl`: a provider paused, by an operator or on a
/// quota signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaEvent {
    pub schema_version: u32,
    pub ts: DateTime<Utc>,
    pub provider: String,
    pub manual: bool,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub hint: Option<String>,
}

pub fn read_events(paths: &SparPaths) -> Vec<QuotaEvent> {
    std::fs::read_to_string(paths.quota_events_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

impl QuotaStore {
    pub fn load(paths: &SparPaths) -> Result<Self> {
        paths.ensure_swarm_root()?;
        let file = paths.quota_file();
        let mut store: Self = if file.is_file() {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            serde_json::from_str(&text)?
        } else {
            Self::default()
        };
        if let Some(shared) = crate::fleet::Shared::open() {
            store.fleet =
                crate::fleet::with_timeout("read fleet quota", move || read_fleet(&shared))
                    .unwrap_or_default();
        }
        Ok(store)
    }

    pub fn save(&mut self, paths: &SparPaths) -> Result<()> {
        paths.ensure_swarm_root()?;
        let file = paths.quota_file();
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;
        if !self.pending.is_empty() {
            let mut lines = String::new();
            for ev in self.pending.drain(..) {
                lines.push_str(&serde_json::to_string(&ev)?);
                lines.push('\n');
            }
            let path = paths.quota_events_file();
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, lines.as_bytes()))
                .with_context(|| format!("append {}", path.display()))?;
        }
        if self.touched.is_empty() {
            return Ok(());
        }
        let Some(shared) = crate::fleet::Shared::open() else {
            return Ok(());
        };
        let changed: Vec<(String, ProviderQuota)> = self
            .touched
            .iter()
            .filter_map(|name| {
                let q = self.providers.get(name)?;
                Some((shared.account_key(name), q.clone()))
            })
            .collect();
        let published = crate::fleet::with_timeout("write fleet quota", move || {
            publish(&shared, changed).map_err(|e| format!("{e:#}"))
        });
        if let Some(Err(e)) = published {
            crate::fleet::warn_once(&e);
        }
        Ok(())
    }

    /// Logs a pause only when it starts one: a provider already held by a live pause of
    /// the same kind is the same pause re-stated, not a new event.
    fn note_pause(&mut self, name: &str, manual: bool, until: Option<DateTime<Utc>>, hint: &str) {
        let held = !self.is_usable(name);
        let was_manual = self.get(name).status == ProviderStatus::PausedManual;
        if held && was_manual == manual {
            return;
        }
        self.pending.push(QuotaEvent {
            schema_version: QUOTA_EVENTS_SCHEMA,
            ts: Utc::now(),
            provider: name.into(),
            manual,
            until,
            hint: Some(hint.into()),
        });
    }

    fn insert(&mut self, mut q: ProviderQuota) {
        let seen = [self.providers.get(&q.name), self.fleet.get(&q.name)]
            .into_iter()
            .flatten()
            .map(|r| r.version)
            .max()
            .unwrap_or(0);
        q.version = seen + 1;
        self.touched.insert(q.name.clone());
        self.providers.insert(q.name.clone(), q);
    }

    /// Every key with a record, here or in the fleet file.
    pub fn names(&self) -> std::collections::BTreeSet<String> {
        self.providers
            .keys()
            .chain(self.fleet.keys())
            .cloned()
            .collect()
    }

    pub fn get(&self, name: &str) -> ProviderQuota {
        let local = self.providers.get(name).cloned();
        let fleet = self.fleet.get(name).cloned();
        match (local, fleet) {
            (Some(l), Some(f)) => merge(l, f, Utc::now()),
            (Some(q), None) | (None, Some(q)) => q,
            (None, None) => ProviderQuota::available(name),
        }
    }

    pub fn is_usable(&self, name: &str) -> bool {
        !self.get(name).holds(Utc::now())
    }

    /// Whether the provider is held by an operator's pause that has not lapsed. Quota
    /// writers consult this so a rate-limit event never replaces the operator's
    /// decision, and probes consult it so they never clear one.
    pub fn is_manually_paused(&self, name: &str) -> bool {
        self.get(name).status == ProviderStatus::PausedManual && !self.is_usable(name)
    }

    /// Whether the provider is held by a quota pause that has not lapsed: the only kind
    /// of pause a probe may clear.
    pub fn is_quota_paused(&self, name: &str) -> bool {
        matches!(
            self.get(name).status,
            ProviderStatus::PausedQuota | ProviderStatus::Cooldown
        ) && !self.is_usable(name)
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
        self.note_pause(name, true, until, "manual pause");
        self.insert(ProviderQuota {
            name: name.into(),
            status: ProviderStatus::PausedManual,
            cooldown_until: until,
            hint: Some("manual pause".into()),
            updated_at: Some(Utc::now()),
            sticky: true,
            strong: false,
            version: 0,
        });
    }

    pub fn pause_quota(&mut self, name: &str, hint: impl Into<String>) {
        if self.is_manually_paused(name) {
            return;
        }
        let strong = self.is_quota_paused(name) && self.get(name).strong;
        let hint = hint.into();
        self.note_pause(name, false, None, &hint);
        self.insert(ProviderQuota {
            name: name.into(),
            status: ProviderStatus::PausedQuota,
            cooldown_until: None,
            hint: Some(hint),
            updated_at: Some(Utc::now()),
            sticky: false,
            strong,
            version: 0,
        });
    }

    pub fn pause_quota_until(
        &mut self,
        name: &str,
        until: Option<DateTime<Utc>>,
        hint: impl Into<String>,
    ) {
        if self.is_manually_paused(name) {
            return;
        }
        let strong = self.is_quota_paused(name) && self.get(name).strong;
        let hint = hint.into();
        self.note_pause(name, false, until, &hint);
        self.insert(ProviderQuota {
            name: name.into(),
            status: if until.is_some() {
                ProviderStatus::Cooldown
            } else {
                ProviderStatus::PausedQuota
            },
            cooldown_until: until,
            hint: Some(hint),
            updated_at: Some(Utc::now()),
            sticky: false,
            strong,
            version: 0,
        });
    }

    /// Upgrade a live quota pause to strong: the rejection behind it was typed or a
    /// line-scoped strong signal, not a broad needle.
    pub fn mark_strong(&mut self, name: &str) {
        if !self.is_quota_paused(name) {
            return;
        }
        let mut q = self.get(name);
        q.strong = true;
        self.insert(q);
    }

    /// Whether a pause should stop a live run's next dispatch (O106): a manual pause,
    /// or a quota pause from a trusted rejection. A weak, heuristic quota pause does not.
    pub fn blocks_live_dispatch(&self, name: &str) -> bool {
        if self.is_usable(name) {
            return false;
        }
        let q = self.get(name);
        q.status == ProviderStatus::PausedManual || q.strong
    }

    pub fn resume(&mut self, name: &str) {
        self.insert(ProviderQuota {
            name: name.into(),
            status: ProviderStatus::Available,
            cooldown_until: None,
            hint: None,
            updated_at: Some(Utc::now()),
            sticky: false,
            strong: false,
            version: 0,
        });
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
            let hit = (contains_phrase(&lower, "rate limit")
                && (lower.contains("rejected")
                    || lower.contains("exceeded")
                    || lower.contains("reached")))
                || (contains_phrase(&lower, "usage limit")
                    && (lower.contains("reached") || lower.contains("exceeded")))
                || lower.contains("too many requests")
                || lower.contains("out of credits")
                || lower.contains("quota exceeded")
                // Claude's and codex's own rejection sentences ("You've hit your weekly
                // limit", "You've hit your usage limit.") are complete, one-line rejection
                // statements in the same class as the bare "out of credits" above — no
                // separate rejection word needed, since "hit your ... limit" already is
                // one. Matched as exact phrases, not "hit your" and "limit" appearing
                // anywhere on the line: that looser form also matched unrelated prose
                // ("hit your retry limit") and this crate's own test assertion text.
                || contains_phrase(&lower, "hit your weekly limit")
                || contains_phrase(&lower, "hit your usage limit")
                // claude 2.1.312 renamed the five-hour window from "session limit" to
                // "5-hour limit" in its usage-limit messages (changelog). The full
                // sentence is inferred from the weekly one; no live capture yet.
                || contains_phrase(&lower, "hit your 5-hour limit")
                || is_agy_quota_rejection(&lower)
                // muse's dispatch-time 429 reads "Subscription quota exhausted" (#153).
                || lower.contains("quota exhausted")
                || lower.contains("402 payment required")
                || names_http_402(&lower);
            if hit {
                return Some(line.trim().to_string());
            }
        }
        None
    }
}

/// Merge `changed` (account key, record) into the fleet file under its lock, replacing
/// the file by rename so a reader on another box never sees a torn write. A file that
/// does not parse is moved aside and the fleet starts again from these records.
fn publish(shared: &crate::fleet::Shared, changed: Vec<(String, ProviderQuota)>) -> Result<()> {
    let lock_path = shared.quota_lock();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("lock {}", lock_path.display()))?;
    let file = shared.quota_file();
    let mut fleet: QuotaStore = match std::fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                let aside =
                    file.with_extension(format!("corrupt-{}", Utc::now().format("%Y%m%dT%H%M%S")));
                std::fs::rename(&file, &aside)
                    .with_context(|| format!("move aside {}", file.display()))?;
                eprintln!(
                    "warning: {} did not parse ({e}); moved to {} and started fresh",
                    file.display(),
                    aside.display()
                );
                QuotaStore::default()
            }
        },
        Err(_) => QuotaStore::default(),
    };
    let now = Utc::now();
    for (key, mine) in changed {
        let merged = match fleet.providers.remove(&key) {
            Some(theirs) => merge(theirs, mine, now),
            None => mine,
        };
        fleet.providers.insert(key, merged);
    }
    let tmp = crate::fleet::unique_tmp(&file);
    std::fs::write(&tmp, serde_json::to_string_pretty(&fleet)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &file).with_context(|| format!("replace {}", file.display()))?;
    let _ = lock.unlock();
    Ok(())
}

/// The fleet file's records for the accounts this box uses, keyed by local storage key.
fn read_fleet(shared: &crate::fleet::Shared) -> HashMap<String, ProviderQuota> {
    let file = shared.quota_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            crate::fleet::warn_once(&format!("read {}: {e}", file.display()));
            return HashMap::new();
        }
    };
    let fleet: QuotaStore = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            crate::fleet::warn_once(&format!("parse {}: {e}", file.display()));
            return HashMap::new();
        }
    };
    fleet
        .providers
        .into_iter()
        .filter_map(|(account, mut q)| {
            let storage = shared.storage_key_for(&account)?;
            q.name = storage.clone();
            Some((storage, q))
        })
        .collect()
}

/// agy 1.4.9's quota rejection, captured live (exit 3): the result's `error` is
/// "Individual quota reached. Please upgrade your subscription to increase your
/// limits. Resets in 125h56m29s." and stderr adds `AGY_ERROR: {"short_error":
/// "RESOURCE_EXHAUSTED (code 429): ..."}`. A bare `RESOURCE_EXHAUSTED` is a gRPC status
/// any client code can print, so it only counts beside agy's own `(code 429)`.
fn is_agy_quota_rejection(lower: &str) -> bool {
    contains_phrase(lower, "individual quota reached")
        || lower.contains("resource_exhausted (code 429)")
}

/// agy states its reset as a relative Go duration on the rejection line ("Resets in
/// 125h56m29s."), so unlike claude's wall-clock form it needs no timezone or calendar
/// inference. Gated on `cli:agy` for the same reason as [`scrape_claude_rate_limits`].
/// A rejection whose duration does not parse still reports the hit with no instant,
/// leaving the generic cooldown.
pub fn scrape_agy_stated_reset(
    provider: &str,
    log: &str,
) -> Option<(String, Option<DateTime<Utc>>, String)> {
    if normalize_key(provider) != "cli:agy" {
        return None;
    }
    let lower = log
        .lines()
        .map(str::to_ascii_lowercase)
        .find(|l| is_agy_quota_rejection(l) && l.contains("resets in "))?;
    let after = &lower[lower.find("resets in ")? + "resets in ".len()..];
    let Some(until) = parse_go_duration(after).and_then(|d| Utc::now().checked_add_signed(d))
    else {
        return Some((
            "cli:agy".into(),
            None,
            "agy individual quota reached (stated reset unparseable, default cooldown)".into(),
        ));
    };
    Some((
        "cli:agy".into(),
        Some(until),
        format!(
            "agy individual quota reached, resets {}",
            until.to_rfc3339()
        ),
    ))
}

/// Whole `h`/`m`/`s` components only, as Go prints a duration of a second or more
/// ("125h56m29s"). A fraction or a `ms` unit is refused rather than rounded.
fn parse_go_duration(s: &str) -> Option<chrono::Duration> {
    let mut total: i64 = 0;
    let mut n: Option<i64> = None;
    let mut seen = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(d) = c.to_digit(10) {
            n = Some(n.unwrap_or(0).checked_mul(10)?.checked_add(i64::from(d))?);
            continue;
        }
        let unit = match c {
            'h' => 3600,
            'm' if chars.peek() == Some(&'s') => return None,
            'm' => 60,
            's' => 1,
            _ => break,
        };
        total = total.checked_add(n.take()?.checked_mul(unit)?)?;
        seen = true;
    }
    if !seen || n.is_some() || total <= 0 {
        return None;
    }
    chrono::Duration::try_seconds(total)
}

/// Parse Claude-style `rate_limits.five_hour` JSON fragments from logs/statusline, or
/// (failing that) the CLI's plain-text weekly/five-hour rejection line, e.g. `! rate
/// limit  seven_day  rejected` followed by "You've hit your weekly limit · resets 12am
/// (America/New_York)". Returns (provider_name, cooldown_until, hint).
///
/// Gated on `provider` actually being a claude adapter: both shapes are specific to
/// the Claude CLI's own output, and a codex/grok/pi log that happens to contain
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
        || lower.contains("usage limit")
        || lower.contains("5-hour limit"))
    {
        return None;
    }
    let period = if lower.contains("seven_day") || lower.contains("weekly") {
        "weekly"
    } else {
        "rate"
    };
    // The *rendered* sentence carries a wall-clock time and no calendar day, so inferring
    // the next occurrence of it is sound for a five-hour window and a guess for a weekly
    // one. That is why this text path never infers a weekly reset. It is not the whole
    // story though: the structured `rate_limit_event` states the reopening instant
    // outright (`StreamStats::quota_resets_at`), and when a caller has that it must be
    // preferred over anything derived here — see `detect_and_pause_quota`.
    if period == "weekly" {
        return Some((
            "cli:claude".into(),
            None,
            format!("claude {period} limit (no stated reset instant, default cooldown)"),
        ));
    }
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
/// Scans every line containing "resets " (not just the first occurrence in the whole
/// tail log) and returns the first one that parses as a complete `<time> (<tz>)`
/// clause. An earlier, unrelated "resets " elsewhere in an 8000-byte tail must not
/// make a real reset line downstream go unparsed and silently degrade to the generic
/// cooldown.
fn parse_stated_reset(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    for line in text.lines() {
        if line.to_ascii_lowercase().contains("resets ") {
            if let Some(until) = parse_reset_clause_in_line(line, now) {
                return Some(until);
            }
        }
    }
    None
}

fn parse_reset_clause_in_line(line: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = line.to_ascii_lowercase();
    let resets_at = lower.find("resets ")? + "resets ".len();
    let after_resets = &line[resets_at..];
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
#[allow(dead_code)]
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
    /// Each pause, quota or manual, lands one line in `quota-events.jsonl` on save;
    /// re-stating a live pause (another 429, a typed reset), a resume or a second save
    /// adds none.
    #[test]
    fn pauses_are_logged_once_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let mut store = super::QuotaStore::default();
        store.pause_quota("cli:codex", "429");
        store.pause_quota("cli:codex", "429 again");
        store.pause_quota_until(
            "cli:codex",
            Some(Utc::now() + chrono::Duration::hours(1)),
            "typed",
        );
        store.pause_manual("cli:claude", None);
        store.save(&paths).unwrap();
        let mut again = super::QuotaStore::load(&paths).unwrap();
        again.pause_quota("cli:codex", "still limited");
        again.save(&paths).unwrap();
        store.resume("cli:codex");
        store.save(&paths).unwrap();
        let evs = super::read_events(&paths);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].provider, "cli:codex");
        assert!(!evs[0].manual);
        assert_eq!(evs[0].hint.as_deref(), Some("429"));
        assert!(evs[1].manual);
        assert_eq!(evs[1].schema_version, super::QUOTA_EVENTS_SCHEMA);
    }

    /// A stated reset gives a wall-clock time and no calendar day. Inferring the next
    /// occurrence is sound for a five-hour window and a fabrication for a weekly one,
    /// where the true reset can be days out. Guessing there fails expensively: spar
    /// re-probes at a time it asserted and walks into the same wall. Both real captured
    /// messages, one per window.
    #[test]
    fn a_weekly_limit_yields_no_inferred_cooldown_and_a_five_hour_one_does() {
        let weekly = "! rate limit  seven_day  rejected\n\
                      You've hit your weekly limit \u{b7} resets 12am (America/New_York)\n";
        let (_, until, hint) = scrape_claude_stated_reset(weekly).expect("weekly must be a hit");
        assert!(
            until.is_none(),
            "a weekly window must not invent a reset day, got {until:?}"
        );
        assert!(
            hint.contains("weekly"),
            "hint should name the window: {hint}"
        );

        let five_hour = "! rate limit  five_hour  rejected\n\
                         You've hit your session limit \u{b7} resets 12:40pm (America/New_York)\n";
        let (_, until, _) = scrape_claude_stated_reset(five_hour).expect("five-hour must be a hit");
        assert!(
            until.is_some(),
            "a five-hour window's next occurrence is a sound inference and must be kept"
        );
    }
    use super::*;
    use tempfile::tempdir;

    fn rec(status: ProviderStatus, age_mins: i64, until_mins: Option<i64>) -> ProviderQuota {
        let now = Utc::now();
        ProviderQuota {
            name: "cli:claude".into(),
            status,
            cooldown_until: until_mins.map(|m| now + chrono::Duration::minutes(m)),
            hint: None,
            updated_at: Some(now - chrono::Duration::minutes(age_mins)),
            sticky: status == ProviderStatus::PausedManual,
            strong: false,
            version: 0,
        }
    }

    #[test]
    fn merge_keeps_the_longest_manual_pause_and_the_strongest_quota_pause() {
        let now = Utc::now();
        let short = rec(ProviderStatus::PausedManual, 1, Some(60));
        let forever = rec(ProviderStatus::PausedManual, 5, None);
        assert!(merge(short.clone(), forever.clone(), now)
            .cooldown_until
            .is_none());
        assert!(merge(forever, short, now).cooldown_until.is_none());

        let weak = rec(ProviderStatus::Cooldown, 1, Some(300));
        let mut strong = rec(ProviderStatus::PausedQuota, 2, Some(10));
        strong.strong = true;
        assert!(merge(weak.clone(), strong.clone(), now).strong);
        assert!(merge(strong.clone(), weak.clone(), now).strong);

        let manual = rec(ProviderStatus::PausedManual, 10, Some(5));
        assert_eq!(
            merge(strong, manual.clone(), now).status,
            ProviderStatus::PausedManual,
            "an operator's pause outranks any quota pause"
        );

        let lapsed = rec(ProviderStatus::PausedManual, 10, Some(-1));
        assert_eq!(
            merge(lapsed, weak.clone(), now).status,
            ProviderStatus::Cooldown
        );
    }

    #[test]
    fn merge_lets_a_later_resume_clear_a_pause_but_not_an_earlier_one() {
        let now = Utc::now();
        let pause = rec(ProviderStatus::PausedManual, 10, None);
        let resumed = rec(ProviderStatus::Available, 1, None);
        assert_eq!(
            merge(pause.clone(), resumed.clone(), now).status,
            ProviderStatus::Available
        );
        assert_eq!(
            merge(resumed, pause.clone(), now).status,
            ProviderStatus::Available
        );
        let stale = rec(ProviderStatus::Available, 60, None);
        assert_eq!(
            merge(stale, pause, now).status,
            ProviderStatus::PausedManual
        );
    }

    #[test]
    fn versions_decide_resume_against_pause_and_skew_only_breaks_ties() {
        let now = Utc::now();
        let mut pause = rec(ProviderStatus::PausedQuota, 0, Some(60));
        let mut resume = rec(ProviderStatus::Available, 0, None);
        resume.updated_at = Some(now + chrono::Duration::minutes(10));
        pause.version = 3;
        resume.version = 2;
        let got = merge(pause.clone(), resume.clone(), now);
        assert_eq!(
            got.status,
            ProviderStatus::PausedQuota,
            "a pause written after its box saw the resume holds, whatever the clocks say"
        );
        assert_eq!(got.version, 3);

        resume.version = 4;
        resume.updated_at = Some(now - chrono::Duration::minutes(10));
        assert_eq!(
            merge(pause.clone(), resume.clone(), now).status,
            ProviderStatus::Available,
            "a resume that saw the pause clears it, even stamped by a slow clock"
        );

        pause.version = 0;
        resume.version = 0;
        resume.updated_at = Some(now - chrono::Duration::seconds(30));
        pause.updated_at = Some(now);
        assert_eq!(
            merge(pause.clone(), resume.clone(), now).status,
            ProviderStatus::Available,
            "inside the skew allowance a concurrent resume wins"
        );
        resume.updated_at = Some(now - chrono::Duration::minutes(5));
        assert_eq!(
            merge(pause, resume, now).status,
            ProviderStatus::PausedQuota,
            "a pause newer than the resume by more than the allowance wins"
        );
    }

    #[test]
    fn a_corrupt_fleet_file_is_moved_aside_and_rebuilt() {
        let tmp = tempdir().unwrap();
        let shared = crate::fleet::Shared::at(tmp.path(), &[]);
        std::fs::write(shared.quota_file(), "{not json").unwrap();
        let mut q = rec(ProviderStatus::PausedManual, 0, None);
        q.version = 1;
        publish(&shared, vec![("cli:claude".into(), q)]).unwrap();
        let rebuilt: QuotaStore =
            serde_json::from_str(&std::fs::read_to_string(shared.quota_file()).unwrap()).unwrap();
        assert!(rebuilt.providers.contains_key("cli:claude"));
        let aside: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1, "the unreadable file is kept for inspection");
    }

    #[test]
    fn a_shared_store_carries_pauses_between_projects_keyed_by_account() {
        let tmp = tempdir().unwrap();
        let shared_dir = tmp.path().join("shared");
        std::fs::create_dir_all(&shared_dir).unwrap();
        let cfg = tmp.path().join("config.toml");
        let write_cfg = |label: &str| {
            std::fs::write(
                &cfg,
                format!(
                    "[quota]\nshared_path = \"{}\"\n[accounts]\n\"cli:claude\" = \"{label}\"\n",
                    shared_dir.display()
                ),
            )
            .unwrap();
        };
        write_cfg("acct-a");
        crate::config::set_test_user_config(Some(cfg.clone()));
        let a = SparPaths::new(tmp.path().join("a"));
        let b = SparPaths::new(tmp.path().join("b"));

        let mut store = QuotaStore::load(&a).unwrap();
        store.pause_quota("cli:claude", "limit");
        store.mark_strong("cli:claude");
        store.save(&a).unwrap();
        let fleet = std::fs::read_to_string(shared_dir.join("quota.json")).unwrap();
        assert!(fleet.contains("cli:claude#acct-a"), "{fleet}");

        let seen = QuotaStore::load(&b).unwrap();
        assert!(!seen.is_usable("cli:claude"), "project b sees a's pause");
        assert!(seen.blocks_live_dispatch("cli:claude"), "strength travels");
        assert!(seen.is_usable("cli:codex"));

        write_cfg("acct-b");
        assert!(
            QuotaStore::load(&b).unwrap().is_usable("cli:claude"),
            "another account is not paused"
        );

        write_cfg("acct-a");
        let mut b_store = QuotaStore::load(&b).unwrap();
        b_store.resume("cli:claude");
        b_store.save(&b).unwrap();
        assert!(
            QuotaStore::load(&a).unwrap().is_usable("cli:claude"),
            "a resume on one project clears the fleet pause, overriding a's own record"
        );

        crate::config::set_test_user_config(Some(tmp.path().join("missing.toml")));
        assert!(QuotaStore::load(&b).unwrap().is_usable("cli:claude"));
        crate::config::set_test_user_config(None);
    }

    #[test]
    fn an_unavailable_shared_path_falls_back_to_the_project_file() {
        let tmp = tempdir().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "[quota]\nshared_path = \"{}\"\n",
                tmp.path().join("not-mounted").display()
            ),
        )
        .unwrap();
        crate::config::set_test_user_config(Some(cfg));
        let paths = SparPaths::new(tmp.path().join("p"));
        let mut store = QuotaStore::load(&paths).unwrap();
        store.pause_manual("cli:claude", None);
        store.save(&paths).unwrap();
        crate::config::set_test_user_config(None);
        assert!(!QuotaStore::load(&paths).unwrap().is_usable("cli:claude"));
        assert!(!tmp.path().join("not-mounted").exists(), "never created");
    }

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
    fn quota_pause_auto_recovers_after_cooldown() {
        // A quota pause with no explicit cooldown lapses once DEFAULT_COOLDOWN_MINS has
        // passed since it was set: the provider is re-probed, not dead forever.
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "possible quota signal: rate limit");
        assert!(!store.is_usable("cli:claude"), "fresh pause is unusable");

        let stale = Utc::now() - chrono::Duration::minutes(DEFAULT_COOLDOWN_MINS + 1);
        store.providers.get_mut("cli:claude").unwrap().updated_at = Some(stale);
        assert!(
            store.is_usable("cli:claude"),
            "quota pause older than the cooldown auto-recovers"
        );
    }

    #[test]
    fn manual_pause_without_until_never_lapses() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:grok", None);
        let a_week_ago = Utc::now() - chrono::Duration::days(7);
        store.providers.get_mut("cli:grok").unwrap().updated_at = Some(a_week_ago);
        assert!(!store.is_usable("cli:grok"));
        assert_eq!(
            store.effective_status("cli:grok"),
            ProviderStatus::PausedManual
        );
        store.resume("cli:grok");
        assert!(store.is_usable("cli:grok"), "only resume lifts it");
    }

    /// The primary checkout's real record: `cli:claude` paused by hand 1171 hours ago,
    /// before `sticky` existed. It was written expecting the 30-minute lapse, so it must
    /// keep lapsing rather than start blocking claude the day this ships.
    #[test]
    fn only_manual_and_strong_pauses_block_a_live_dispatch() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:codex", "possible quota signal: 429");
        assert!(
            !store.is_usable("cli:codex"),
            "a weak pause still steers new runs"
        );
        assert!(!store.blocks_live_dispatch("cli:codex"));

        store.mark_strong("cli:codex");
        assert!(store.blocks_live_dispatch("cli:codex"));
        store.pause_quota("cli:codex", "possible quota signal: quota");
        assert!(
            store.get("cli:codex").strong,
            "a later broad scrape does not downgrade a live strong pause"
        );

        store.pause_manual("cli:grok", None);
        assert!(store.blocks_live_dispatch("cli:grok"));

        store.mark_strong("cli:claude");
        assert!(!store.blocks_live_dispatch("cli:claude"), "nothing to mark");

        let legacy: QuotaStore = serde_json::from_str(&format!(
            r#"{{"providers":{{"cli:muse":{{"name":"cli:muse","status":"paused_quota","updated_at":"{}"}}}}}}"#,
            Utc::now().to_rfc3339()
        ))
        .unwrap();
        assert!(
            !legacy.blocks_live_dispatch("cli:muse"),
            "legacy records read weak"
        );
    }

    #[test]
    fn a_legacy_manual_pause_without_sticky_keeps_the_old_lapse() {
        let legacy: QuotaStore = serde_json::from_str(
            r#"{"providers":{"cli:claude":{"name":"cli:claude","status":"paused_manual","hint":"manual pause","updated_at":"2026-09-11T03:00:00Z"}}}"#,
        )
        .unwrap();
        assert!(!legacy.get("cli:claude").sticky);
        assert!(legacy.is_usable("cli:claude"));
        assert!(!legacy.is_manually_paused("cli:claude"));
        assert_eq!(
            legacy.effective_status("cli:claude"),
            ProviderStatus::Available
        );

        let mut fresh = legacy.clone();
        fresh.providers.get_mut("cli:claude").unwrap().updated_at = Some(Utc::now());
        assert!(
            !fresh.is_usable("cli:claude"),
            "a legacy pause still holds for its first 30 minutes"
        );
    }

    #[test]
    fn provider_pause_writes_sticky_and_it_round_trips() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:grok", None);
        let text = serde_json::to_string(&store).unwrap();
        assert!(text.contains("\"sticky\":true"), "{text}");
        let back: QuotaStore = serde_json::from_str(&text).unwrap();
        assert!(back.get("cli:grok").sticky);

        store.pause_quota("cli:codex", "q");
        let text = serde_json::to_string(&store.providers["cli:codex"]).unwrap();
        assert!(
            !text.contains("sticky"),
            "quota pauses do not carry the field: {text}"
        );
    }

    #[test]
    fn manual_pause_with_until_lapses_at_until_and_stays_manual_until_then() {
        let mut store = QuotaStore::default();
        let until = Utc::now() + chrono::Duration::days(60);
        store.pause_manual("cli:grok", Some(until));
        assert_eq!(store.get("cli:grok").status, ProviderStatus::PausedManual);
        assert_eq!(store.get("cli:grok").cooldown_until, Some(until));
        assert!(store.is_manually_paused("cli:grok"));
        assert!(!store.is_quota_paused("cli:grok"));

        store.providers.get_mut("cli:grok").unwrap().cooldown_until =
            Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(store.is_usable("cli:grok"));
        assert!(!store.is_manually_paused("cli:grok"));
    }

    /// The 2026-10-22 shape: grok paused by hand until 2027, then a live run's slot hit
    /// a rate limit. The quota write used to replace the record, turning a pause that
    /// held until 2027 into one that lapsed in thirty minutes.
    #[test]
    fn a_quota_event_never_overwrites_a_manual_pause() {
        let mut store = QuotaStore::default();
        let until = DateTime::parse_from_rfc3339("2027-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        store.pause_manual("cli:grok", Some(until));
        store.pause_quota("cli:grok", "possible quota signal: 429");
        store.pause_quota_until(
            "cli:grok",
            Some(Utc::now() + chrono::Duration::minutes(5)),
            "grok rate limit rejected",
        );
        let q = store.get("cli:grok");
        assert_eq!(q.status, ProviderStatus::PausedManual);
        assert_eq!(q.cooldown_until, Some(until));
        assert_eq!(q.hint.as_deref(), Some("manual pause"));

        store.pause_manual("cli:claude", None);
        store.pause_quota("cli:claude", "possible quota signal: quota");
        assert_eq!(store.get("cli:claude").status, ProviderStatus::PausedManual);
    }

    #[test]
    fn a_lapsed_manual_pause_can_be_replaced_by_a_quota_pause() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:codex", Some(Utc::now() - chrono::Duration::minutes(1)));
        store.pause_quota("cli:codex", "possible quota signal: usage limit");
        assert_eq!(store.get("cli:codex").status, ProviderStatus::PausedQuota);
        assert!(store.is_quota_paused("cli:codex"));
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
        store.pause_quota("cli:claude", "possible quota signal: rate limit");
        assert_eq!(
            store.effective_status("cli:claude"),
            ProviderStatus::PausedQuota,
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

    /// Inverted deliberately. This test previously asserted that a weekly reset parses
    /// into a cooldown "within ~25h". That was the fabrication: the message carries a
    /// wall-clock time and no calendar day, so for a seven-day window the next
    /// occurrence of midnight is a guess that can be wrong by six days, and spar acts
    /// on it by re-probing and walking into the same wall. The generic timer re-probes
    /// cheaply and asserts nothing, so no inferred cooldown beats a wrong one.
    #[test]
    fn scrape_claude_rate_limits_refuses_to_invent_a_weekly_reset_day() {
        let (name, until, hint) =
            scrape_claude_rate_limits("cli:claude", WEEKLY_LIMIT_LOG).unwrap();
        assert_eq!(name, "cli:claude");
        assert!(
            until.is_none(),
            "a weekly window states a time but not a day; inferring one is a guess: {until:?}"
        );
        assert!(hint.contains("weekly"), "hint must name the window: {hint}");
    }

    /// The unparseable-reset fallback is exercised on a *five-hour* window, because
    /// that is the only window whose reset spar infers at all now; a weekly one returns
    /// early without consulting the clause.
    #[test]
    fn scrape_claude_rate_limits_falls_back_when_reset_unparseable() {
        let log = "! rate limit  five_hour  rejected\n\
            You've hit your session limit \u{b7} resets whenever (Nowhere/Fake)\n";
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
    fn parse_stated_reset_skips_an_earlier_unrelated_resets_line() {
        // A noisy real log can mention "resets " once before the actual limit line
        // (e.g. an unrelated cache/session message). The anchor must not lock onto
        // that first occurrence and give up — it has to keep scanning to the real one.
        let log = "cache resets every 10 minutes during warmup\n\
            ! rate limit  seven_day  rejected\n\
            You've hit your weekly limit \u{b7} resets 12am (America/New_York)\n";
        let now = DateTime::parse_from_rfc3339("2026-09-04T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let until = parse_stated_reset(log, now).expect("must find the real reset line");
        assert!(until > now);
        assert!(until - now < chrono::Duration::hours(9));
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

    /// Claude's stated-reset sentence is a separate line from "! rate limit ...
    /// rejected". Before this fix, an output mode that printed only the sentence (no
    /// separate rejection line) fell through to `Phase::Failed` while the provider was
    /// simultaneously paused for hours — the worst of both outcomes.
    #[test]
    fn strong_quota_signal_matches_claudes_sentence_alone() {
        assert!(QuotaStore::scrape_strong_quota_signal(
            "You've hit your weekly limit \u{b7} resets 12am (America/New_York)"
        )
        .is_some());
    }

    /// codex's own wordings: a "hit your ... limit" rejection sentence, and a "rate
    /// limit reached" line the "rejected"/"exceeded" arm didn't cover.
    #[test]
    fn strong_quota_signal_matches_codexs_wordings() {
        assert!(QuotaStore::scrape_strong_quota_signal(
            "You've hit your usage limit. Try again later."
        )
        .is_some());
        assert!(QuotaStore::scrape_strong_quota_signal(
            "Rate limit reached for gpt-5 in organization org-abc on requests per min. Limit: 3/min."
        )
        .is_some());
    }

    /// claude 2.1.312's changelog: "Changed usage limit messages to call the 5-hour
    /// window the \"5-hour limit\" instead of the \"session limit\"". The sentence below
    /// is the old captured one with that rename applied, not a live capture.
    #[test]
    fn strong_quota_signal_matches_claudes_5_hour_wording() {
        let line = "You've hit your 5-hour limit \u{b7} resets 12:40pm (America/New_York)";
        assert!(QuotaStore::scrape_strong_quota_signal(line).is_some());
        let (_, until, _) =
            scrape_claude_stated_reset(line).expect("the renamed window still states a reset");
        assert!(until.is_some(), "a five-hour reset is a sound inference");
        for prose in [
            "warning: resuming would use a noticeable share of your 5-hour usage limit",
            "docs: explain the 5-hour limit to users",
            "you must not have hit your 5-hour limiter",
        ] {
            assert!(
                QuotaStore::scrape_strong_quota_signal(prose).is_none(),
                "false positive on: {prose}"
            );
        }
    }

    /// Both lines agy 1.4.9 printed on a real quota rejection (exit 3), verbatim, plus
    /// the `! ` line the coalescer renders from the stream's `result.error`.
    const AGY_QUOTA_STDERR: &str = "error: Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 125h56m29s.\n\
        AGY_ERROR: {\"short_error\":\"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 125h56m29s.\",\"status\":\"RESOURCE_EXHAUSTED\",\"error_code\":429,\"code_kind\":\"http\",\"retryable\":true,\"error_id\":\"cef80e6b-34ef-40a4-8e9c-41afd12d4b27-3\"}\n";

    #[test]
    fn strong_quota_signal_matches_agys_real_rejection() {
        for line in AGY_QUOTA_STDERR.lines() {
            assert!(
                QuotaStore::scrape_strong_quota_signal(line).is_some(),
                "each agy line routes on its own: {line}"
            );
        }
        assert!(QuotaStore::scrape_strong_quota_signal(
            "! Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 125h56m29s."
        )
        .is_some());
        for prose in [
            "grpc: status RESOURCE_EXHAUSTED returned by the mock server ... FAILED",
            "match code { Code::ResourceExhausted => retry() }",
            "the per-user quota reached its soft threshold in the test fixture",
        ] {
            assert!(
                QuotaStore::scrape_strong_quota_signal(prose).is_none(),
                "false positive on: {prose}"
            );
        }
    }

    /// muse's dispatch-time rejection, verbatim from the 2026-10 fleet incidents
    /// (#153): a 429 whose text is "Subscription quota exhausted". Before this it was
    /// read as an ordinary failure, so the run failed instead of moving to its backup.
    #[test]
    fn strong_quota_signal_matches_muses_subscription_quota_exhausted() {
        assert!(QuotaStore::scrape_strong_quota_signal("Subscription quota exhausted").is_some());
        assert!(
            QuotaStore::scrape_strong_quota_signal("429 Subscription quota exhausted").is_some()
        );
        assert!(QuotaStore::scrape_strong_quota_signal("error: quota exhausted").is_some());
    }

    /// A 402 is a billing stop when the line frames it as an HTTP status.
    #[test]
    fn strong_quota_signal_matches_an_http_402() {
        for line in [
            "402 Payment Required",
            "HTTP 402",
            "request failed with status 402",
            "API Error: 402",
            "{\"error\":{\"code\":402,\"message\":\"insufficient balance\"}}",
        ] {
            assert!(
                QuotaStore::scrape_strong_quota_signal(line).is_some(),
                "{line:?} must route as quota"
            );
        }
    }

    /// A 402 that is a line number, a count or part of a longer number is not one.
    #[test]
    fn strong_quota_signal_ignores_a_402_that_is_not_an_http_status() {
        for line in [
            "thread 'main' panicked at src/quota.rs:402:5: error in parser",
            "error at line 402 of the fixture",
            "test result: ok. 402 passed; 0 failed",
            "HTTP 4020 bytes sent",
            "the quota is not exhausted yet",
        ] {
            assert!(
                QuotaStore::scrape_strong_quota_signal(line).is_none(),
                "{line:?} must not route as quota"
            );
        }
    }

    #[test]
    fn agy_stated_reset_pauses_for_the_stated_duration() {
        let before = Utc::now();
        let (name, until, hint) =
            scrape_agy_stated_reset("cli:agy@gemini-3.5-flash", AGY_QUOTA_STDERR).unwrap();
        assert_eq!(name, "cli:agy");
        let until = until.expect("125h56m29s parses");
        let stated = chrono::Duration::seconds(125 * 3600 + 56 * 60 + 29);
        assert!(until >= before + stated && until <= Utc::now() + stated);
        assert!(hint.contains("resets"), "{hint}");
        assert!(scrape_agy_stated_reset("cli:claude", AGY_QUOTA_STDERR).is_none());
        assert!(scrape_agy_stated_reset("cli:agy", "Resets in 5m0s.").is_none());
        let (_, until, _) =
            scrape_agy_stated_reset("cli:agy", "Individual quota reached. Resets in 1m29.5s.")
                .expect("still a hit");
        assert!(
            until.is_none(),
            "a fractional duration falls back, not rounds"
        );
        for absurd in ["Resets in 9999999999999h.", "Resets in 99999999999h."] {
            let (_, until, _) =
                scrape_agy_stated_reset("cli:agy", &format!("Individual quota reached. {absurd}"))
                    .expect("still a hit");
            assert!(until.is_none(), "{absurd} must fall back, not panic");
        }
    }

    #[test]
    fn go_duration_parses_whole_components_only() {
        let d = |s| parse_go_duration(s).map(|d| d.num_seconds());
        assert_eq!(d("125h56m29s."), Some(125 * 3600 + 56 * 60 + 29));
        assert_eq!(d("5m0s"), Some(300));
        assert_eq!(d("30s"), Some(30));
        assert_eq!(d("500ms"), None);
        assert_eq!(d("1m29.5s"), None);
        assert_eq!(d("0s"), None);
        assert_eq!(d("12"), None);
        assert_eq!(d(""), None);
    }

    /// Round 6's `"hit your" && "limit"` disjunct required neither adjacency nor a
    /// rejection word, so ordinary prose that happens to contain both substrings
    /// anywhere on the same line matched — swallowing a real failure as quota, worse
    /// than the bug this whole fix exists to close. Fixed by matching the two known
    /// exact phrases instead of the loose pair.
    #[test]
    fn strong_quota_signal_ignores_unrelated_hit_your_limit_prose() {
        assert!(QuotaStore::scrape_strong_quota_signal(
            "you must not have hit your retry limit before this point"
        )
        .is_none());
    }

    /// `contains_phrase`'s word-boundary guard rejected any letter following the
    /// phrase, which excluded the plural/past-tense forms providers actually use.
    #[test]
    fn strong_quota_signal_matches_plural_and_past_tense_rate_limit() {
        assert!(QuotaStore::scrape_strong_quota_signal(
            "Error: rate limits exceeded for model claude-opus"
        )
        .is_some());
        assert!(QuotaStore::scrape_strong_quota_signal(
            "you have been rate limited; limit reached, retry after 60s"
        )
        .is_some());
    }

    /// "too many requests" alone is already an unambiguous rejection sentence; the
    /// prior `&& "429"` pairing missed common JSON error-body shapes that carry no
    /// numeric status text.
    #[test]
    fn strong_quota_signal_matches_too_many_requests_without_a_429() {
        assert!(QuotaStore::scrape_strong_quota_signal(
            r#"{"error":{"message":"Too many requests"}}"#
        )
        .is_some());
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
        // "rate limiter rejected" contains both "rate limit" (as a prefix of "rate
        // limiter") and "rejected" — without the word-boundary guard in
        // `contains_phrase`, this line alone would match and misroute an ordinary
        // implementer building a rate limiter as a quota hit.
        let rate_limiter_rejected =
            "test: the rate limiter rejected the request as expected ... FAILED";
        let capacity_doc_edit = "updated docs/capacity-planning.md with Q3 projections";
        for log in [
            panic_with_429,
            quota_module_edit,
            rate_limiter_under_test,
            rate_limiter_rejected,
            capacity_doc_edit,
        ] {
            assert!(
                QuotaStore::scrape_strong_quota_signal(log).is_none(),
                "false positive on: {log}"
            );
        }
    }
}
