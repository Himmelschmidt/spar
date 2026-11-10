use crate::quota::QuotaStore;
use crate::state::SlotState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum StopCause {
    Environmental,
    /// The provider refused the credential (O104). Engages the backup like any
    /// environmental stop, but is told apart so it never pauses the provider for a
    /// quota window and parks with reason "auth".
    Auth,
    Work,
}

impl StopCause {
    pub fn engages_backup(self) -> bool {
        matches!(self, StopCause::Environmental | StopCause::Auth)
    }
}

/// How many trailing non-empty lines of a dead dispatch's log are read for an auth
/// rejection. The provider's own refusal is the last thing a dead dispatch prints; an
/// agent that quoted these phrases earlier in its turn (reviewing login code, say) did
/// not lose its credential.
const AUTH_SCAN_LINES: usize = 15;

/// The provider refused the credential, as the CLIs actually word it: claude's `API
/// Error: 401 {"type":"authentication_error","message":"OAuth access token has been
/// revoked..."}` and "Please run /login", codex's `Not logged in` and failed token
/// refresh, muse's ``not logged in: run `muse login` first`` and `[auth]: credentials
/// rejected`, grok's `You are not authenticated.`, and an OAuth `invalid_grant`.
fn is_auth_line(lower: &str) -> bool {
    const PHRASES: &[&str] = &[
        "token has been revoked",
        "oauth token has expired",
        "authentication_error",
        "invalid_grant",
        "not logged in",
        "please login",
        "please log in",
        "please run /login",
        "you are not authenticated",
        "credentials rejected",
        "token could not be refreshed",
        "refresh token was already used",
        "invalid api key",
        "invalid x-api-key",
        "401 unauthorized",
    ];
    PHRASES.iter().any(|p| lower.contains(p)) || names_http_401(lower)
}

/// A 401 named as an HTTP status, not a bare number: `401` alone is a line number, a
/// test count or a byte offset far more often than it is a status.
fn names_http_401(lower: &str) -> bool {
    [
        "http 401",
        "status 401",
        "status: 401",
        "status code 401",
        "status code: 401",
        "\"status\":401",
        "\"status\": 401",
        "\"code\":401",
        "\"code\": 401",
        "error 401",
        "error: 401",
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

/// The last auth-rejection line in the tail of `text`, if any. Used on a failed
/// dispatch's log, its error string, and a probe's raw output.
pub fn scrape_auth_signal(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(AUTH_SCAN_LINES);
    lines[start..]
        .iter()
        .rev()
        .find(|l| is_auth_line(&l.to_ascii_lowercase()))
        .map(|l| l.trim().chars().take(300).collect())
}

/// Quota-aware: also ineligible when the provider's fleet key sits under a pause
/// record (O106) or when the availability set (when provided) excludes it.
/// The set is built from `ProviderReport::usable`, so probe-failed providers
/// are excluded upstream; a `None` set means dry-run or no detection and
/// checks quota only. Readiness itself is gated in `backup_eligible` and
/// `resolve_with_backup`, which know whether probing is allowed.
/// Whether a seat may stay on its provider: explicit CLI pins dispatch despite
/// probe-failed readiness (they warn at launch instead), so only a quota or
/// manual pause benches them; any other seat must also sit in the usable set.
/// The set is an auto-selection artifact — it must not park or rotate a pin —
/// and `None` (no backup declared, or dry-run) leaves every seat to the store.
pub fn seat_eligible(
    provider: &str,
    source: Option<crate::state::SeatSource>,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
) -> bool {
    if source.is_some_and(crate::state::SeatSource::is_explicit_pin) {
        return is_provider_eligible(provider, store, None);
    }
    is_provider_eligible(provider, store, available)
}

pub fn is_provider_eligible(
    provider: &str,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
) -> bool {
    let key = crate::quota::quota_key(provider);
    if !store.is_usable(&key) {
        return false;
    }
    if store.effective_status(&key) != crate::quota::ProviderStatus::Available {
        return false;
    }
    if let Ok(pref) = crate::provider_ref::ProviderRef::parse(provider) {
        if pref.backend == crate::provider_ref::ExecBackend::ApiSdk {
            return true;
        }
    }
    if let Some(set) = available {
        if !set.contains(&crate::quota::sub_key(provider)) {
            return false;
        }
    }
    true
}

/// Whether a declared backup can take a seat. A backup the store holds on a quota pause
/// is probed before it is skipped (#153, O106): that pause is a failure recorded at some
/// point in the past, and a week-old one benched a working backup while its primary
/// died. A manual pause is the operator's decision and is never probed past, and
/// `dry` never spawns. A `[backups]` file backup whose readiness probe failed is
/// never eligible: rotation skips it while the run stays on (or falls back to)
/// the primary. A CLI `--backup` entry is an explicit pin and dispatches despite
/// `Unhealthy` readiness (warning instead): see `takeover`.
fn backup_eligible(
    backup: &str,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
    paths: &crate::paths::SparPaths,
    dry: bool,
    cli: bool,
) -> bool {
    if is_provider_eligible(backup, store, if cli { None } else { available })
        && (cli || health_allows(backup, dry))
    {
        return true;
    }
    let key = crate::quota::quota_key(backup);
    if dry || !store.is_quota_paused(&key) {
        return false;
    }
    if first_probe(&key) {
        let report = crate::probe::probe(paths, backup, false);
        eprintln!(
            "note: backup {key} was on a quota pause; probe says {:?} ({})",
            report.verdict, report.store
        );
    }
    let fresh = QuotaStore::load(paths).unwrap_or_default();
    is_provider_eligible(backup, &fresh, if cli { None } else { available })
        && (cli || health_allows(backup, dry))
}

/// Whether `backup` may take `role`'s seat, and the source the winning seat records.
/// A CLI `--backup` entry (`cfg.cli_backup_keys`) dispatches even while `Unhealthy`
/// and warns like any other pin; a `[backups]` file entry still skips `Unhealthy`
/// providers. `None` when the backup cannot take the seat.
pub fn takeover(
    backup: &str,
    role: crate::state::SlotRole,
    cfg: &crate::config::Config,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
    paths: &crate::paths::SparPaths,
    dry: bool,
) -> Option<crate::state::SeatSource> {
    let cli = cfg.cli_backup_keys.contains(role.as_config_key());
    backup_eligible(backup, store, available, paths, dry, cli).then_some(if cli {
        crate::state::SeatSource::CliBackup
    } else {
        crate::state::SeatSource::Backup
    })
}

/// Point `slot_id` at an engaged backup: re-point, record the takeover source, warn
/// when the backup is an `Unhealthy` pin, and save. One call site per engagement
/// so the warning cannot be forgotten on one path.
pub fn engage(
    state: &mut crate::state::RunState,
    paths: &crate::paths::SparPaths,
    slot_id: &str,
    backup_raw: &str,
    source: crate::state::SeatSource,
) -> anyhow::Result<()> {
    let pin = crate::provider_ref::Pin::parse(backup_raw)
        .map(|p| p.display())
        .unwrap_or_else(|_| backup_raw.to_string());
    if let Some(s) = state.slot_mut(slot_id) {
        s.point_at(&pin);
        s.source = Some(source);
    }
    state.warn_unhealthy_pin(&pin, Some(source));
    state.save(paths)?;
    Ok(())
}

/// Rotation-target readiness gate. `dry` never spawns: without a probe there is
/// no signal, so dry runs keep today's quota-only verdict.
fn health_allows(provider: &str, dry: bool) -> bool {
    dry || crate::providers::is_provider_usable(provider, false)
}

static PROBED_BACKUPS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// True the first time this process asks to probe `key`: N seats sharing one quota-paused
/// backup cost one probe, not N. Later askers re-read the store the probe wrote.
fn first_probe(key: &str) -> bool {
    PROBED_BACKUPS
        .lock()
        .map(|mut set| set.insert(key.to_string()))
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn backup_for_role(
    role: crate::state::SlotRole,
    ordinal: usize,
    cfg: &crate::config::Config,
) -> Option<String> {
    match role {
        crate::state::SlotRole::Planner => cfg.backups.planner.clone(),
        crate::state::SlotRole::PlanCritic => cfg.backups.plan_critic.clone(),
        crate::state::SlotRole::Implementer => cfg.backups.implementer.clone(),
        crate::state::SlotRole::Tester => cfg.backups.tester.clone(),
        crate::state::SlotRole::TestAuthor => cfg.backups.test_author.clone(),
        crate::state::SlotRole::Reviewer => cfg
            .backups
            .reviewer
            .get(ordinal)
            .filter(|s| !s.is_empty())
            .cloned(),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub fn resolve_with_backup(
    role: crate::state::SlotRole,
    pool_index: usize,
    role_index: usize,
    pool: &[String],
    pool_origin: crate::state::PoolOrigin,
    cfg: &crate::config::Config,
    primary_model: Option<&str>,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
    paths: &crate::paths::SparPaths,
    dry: bool,
) -> Option<(String, crate::state::SeatSource)> {
    let primary = crate::workflow::roles_resolve::resolve_seat(
        role,
        pool_index,
        role_index,
        pool,
        pool_origin,
        cfg,
        dry,
    );
    let (prov, src) = primary?;
    let seat = crate::provider_ref::ProviderRef::for_seat(&prov, primary_model).display();
    // An auto-selected seat on a probe-failed provider yields to the backup; an
    // explicit pin stays (quota and manual pauses still bench it) and the launch
    // warns. `dry` never probes.
    if seat_eligible(&seat, Some(src), store, available) {
        return Some((prov, src));
    }
    let backup_raw = backup_for_role(role, role_index, cfg)?;
    let Some(source) = takeover(&backup_raw, role, cfg, store, available, paths, dry) else {
        return Some((prov, src));
    };
    let parsed = crate::provider_ref::Pin::parse(&backup_raw).ok()?;
    Some((parsed.display(), source))
}

pub fn dispatch_stop_cause(
    slot: &SlotState,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
) -> StopCause {
    if slot.status != crate::state::SlotStatus::Failed {
        return StopCause::Work;
    }
    if slot.quota_hit {
        return StopCause::Environmental;
    }
    if slot.auth_fault {
        return StopCause::Auth;
    }
    if let Some(err) = &slot.error {
        let lower = err.to_ascii_lowercase();
        if lower.contains("hard ceiling")
            || lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("missing expected artifact")
            || lower.contains("request_changes")
            || lower.contains("adverse review")
        {
            return StopCause::Work;
        }
        if lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("429")
            || lower.contains("quota exceeded")
            || lower.contains("too many requests")
        {
            return StopCause::Environmental;
        }
        if (lower.contains("paused")
            || lower.contains("cooldown")
            || lower.contains("unavailable")
            || lower.contains("capacity")
            || lower.contains("billing"))
            && {
                let seat = slot.provider_ref();
                let key = seat.quota_key();
                let sub = seat.storage_key();
                store.effective_status(&key) != crate::quota::ProviderStatus::Available
                    || !store.is_usable(&key)
                    || available.is_some_and(|s| !s.contains(&sub))
            }
        {
            return StopCause::Environmental;
        }
        if (lower.contains("no such file")
            || lower.contains("not found")
            || lower.contains("spawn failed")
            || lower.contains("os error")
            || lower.contains("executable not found"))
            && {
                let seat = slot.provider_ref();
                let key = seat.quota_key();
                let sub = seat.storage_key();
                store.effective_status(&key) != crate::quota::ProviderStatus::Available
                    || !store.is_usable(&key)
                    || available.is_some_and(|s| !s.contains(&sub))
            }
        {
            return StopCause::Environmental;
        }
    }
    StopCause::Work
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SlotRole, SlotState, SlotStatus};

    fn api_slot() -> SlotState {
        let mut s = slot_with_quota(false);
        s.point_at("api:openai@gpt-5");
        s
    }

    fn slot_with_quota(hit: bool) -> SlotState {
        let mut s = crate::executor::init_slot("test", "cli:claude", SlotRole::Implementer);
        s.quota_hit = hit;
        s
    }

    fn launch_inputs() -> (
        tempfile::TempDir,
        crate::config::Config,
        QuotaStore,
        Option<std::collections::HashSet<String>>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::default();
        (
            tmp,
            cfg,
            QuotaStore::default(),
            Some(std::collections::HashSet::new()),
        )
    }

    #[test]
    fn an_explicit_pin_stays_when_nothing_is_usable() {
        let (tmp, cfg, store, available) = launch_inputs();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let pool = vec!["cli:claude".to_string()];
        assert_eq!(
            resolve_with_backup(
                SlotRole::Planner,
                0,
                0,
                &pool,
                crate::state::PoolOrigin::CliProviders,
                &cfg,
                None,
                &store,
                available.as_ref(),
                &paths,
                false,
            ),
            Some((
                "cli:claude".to_string(),
                crate::state::SeatSource::CliProviders
            ))
        );
    }

    #[test]
    fn an_auto_seat_with_no_usable_provider_and_no_backup_resolves_nothing() {
        let (tmp, cfg, store, available) = launch_inputs();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let pool = vec!["cli:claude".to_string()];
        assert_eq!(
            resolve_with_backup(
                SlotRole::Planner,
                0,
                0,
                &pool,
                crate::state::PoolOrigin::Synth,
                &cfg,
                None,
                &store,
                available.as_ref(),
                &paths,
                false,
            ),
            None
        );
    }

    #[test]
    fn an_auto_seat_yields_to_the_declared_backup() {
        let (tmp, mut cfg, store, _) = launch_inputs();
        cfg.backups.planner = Some("cli:codex".to_string());
        let paths = crate::paths::SparPaths::new(tmp.path());
        let pool = vec!["cli:claude".to_string()];
        let mut set = std::collections::HashSet::new();
        set.insert("cli:codex".to_string());
        assert_eq!(
            resolve_with_backup(
                SlotRole::Planner,
                0,
                0,
                &pool,
                crate::state::PoolOrigin::Synth,
                &cfg,
                None,
                &store,
                Some(&set),
                &paths,
                true,
            ),
            Some(("cli:codex".to_string(), crate::state::SeatSource::Backup))
        );
    }

    #[test]
    fn quota_hit_is_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(true);
        slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&slot, &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn paused_provider_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit exceeded".into());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn unavailable_provider_is_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("provider unavailable: cli:claude not found".into());
        let mut set = std::collections::HashSet::new();
        set.insert("cli:grok".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Environmental
        );
    }

    #[test]
    fn succeeded_slot_is_not_environmental_even_when_provider_paused() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Done;
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
    }

    #[test]
    fn pending_slot_is_not_environmental_even_when_unavailable() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Pending;
        let mut set = std::collections::HashSet::new();
        set.insert("cli:grok".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn work_failure_is_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn unavailable_without_set_is_not_environmental_for_work_failure() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
    }

    #[test]
    fn is_provider_eligible_respects_quota() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "q");
        assert!(!is_provider_eligible("cli:claude", &store, None));
        assert!(is_provider_eligible("cli:grok", &store, None));
    }

    #[test]
    fn is_provider_eligible_respects_availability_set() {
        let store = QuotaStore::default();
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert!(is_provider_eligible("cli:claude", &store, Some(&set)));
        assert!(!is_provider_eligible("cli:grok", &store, Some(&set)));
    }

    #[test]
    fn api_provider_ignores_availability_set() {
        let store = QuotaStore::default();
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert!(is_provider_eligible("api:openai@gpt-5", &store, Some(&set)));
        assert_eq!(
            dispatch_stop_cause(&api_slot(), &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn api_provider_still_respects_quota_pause() {
        let mut store = QuotaStore::default();
        store.pause_quota("api:openai", "q");
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert!(!is_provider_eligible(
            "api:openai@gpt-5",
            &store,
            Some(&set)
        ));
        let mut slot = api_slot();
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit exceeded".into());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Environmental
        );
    }

    #[test]
    fn timeout_is_work_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("hard ceiling: slot timed out after 10s".into());
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn missing_artifact_is_work_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("missing expected artifact: summary-impl.md".into());
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn adverse_review_is_work_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("review verdict: request_changes".into());
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn neutralization_would_fail_work_branch() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        let mut set = std::collections::HashSet::new();
        set.insert("cli:claude".to_string());
        // If Work branch were neutralized to Environmental, this would be Environmental
        assert_eq!(
            dispatch_stop_cause(&slot, &store, Some(&set)),
            StopCause::Work
        );
        // Verify environmental still distinct
        let mut env_slot = slot_with_quota(true);
        env_slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&env_slot, &store, Some(&set)),
            StopCause::Environmental
        );
    }

    #[test]
    fn timeout_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("hard ceiling: slot timed out after 10s".into());
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
    }

    #[test]
    fn missing_artifact_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("missing expected artifact: summary-impl.md".into());
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
    }

    #[test]
    fn adverse_review_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("review verdict: request_changes".into());
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
    }

    #[test]
    fn paused_provider_without_work_signal_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("some other failure".into());
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
    }

    #[test]
    fn generic_failure_with_paused_provider_is_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("exit code 1".into());
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
        // Neutralisation: if paused check were leak, this would be Environmental
        let mut env_slot = slot_with_quota(true);
        env_slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&env_slot, &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn auth_signal_matches_the_real_wordings() {
        for line in [
            r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"OAuth access token has been revoked. Please obtain a new token or refresh your existing token."}}"#,
            "Invalid API key \u{b7} Please run /login",
            "Not logged in",
            "muse usage: not logged in: run `muse login` first",
            "! transport error [auth]: credentials rejected",
            "You are not authenticated.",
            r#"{"error":"invalid_grant","error_description":"Invalid refresh token"}"#,
            "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.",
            "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header",
            "HTTP 401",
        ] {
            assert!(scrape_auth_signal(line).is_some(), "{line:?} must read as auth");
        }
    }

    #[test]
    fn auth_signal_ignores_ordinary_failures() {
        for line in [
            "thread 'main' panicked at src/auth.rs:401:9",
            "test result: ok. 401 passed; 0 failed",
            "error at line 401 of the fixture",
            "HTTP 4010 bytes sent",
            "! rate limit  seven_day  rejected",
            "the login form now renders",
        ] {
            assert!(scrape_auth_signal(line).is_none(), "{line:?} is not auth");
        }
    }

    #[test]
    fn auth_signal_reads_only_the_tail_of_a_log() {
        let mut log =
            String::from("reviewing: the handler returns 401 Unauthorized when not logged in\n");
        for i in 0..AUTH_SCAN_LINES {
            log.push_str(&format!("-> edit src/handler_{i}.rs\n"));
        }
        log.push_str("error: tests failed\n");
        assert!(
            scrape_auth_signal(&log).is_none(),
            "an agent discussing auth early in its turn did not lose its credential"
        );
        log.push_str("Not logged in\n");
        assert_eq!(scrape_auth_signal(&log).as_deref(), Some("Not logged in"));
    }

    #[test]
    fn an_auth_fault_is_its_own_stop_cause_and_engages_the_backup() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.auth_fault = true;
        slot.error = Some("exit 1".into());
        let cause = dispatch_stop_cause(&slot, &store, None);
        assert_eq!(cause, StopCause::Auth);
        assert!(cause.engages_backup());
        assert!(StopCause::Environmental.engages_backup());
        assert!(!StopCause::Work.engages_backup());
        assert!(slot.parks_on_quota());
    }

    #[test]
    fn a_backup_is_probed_at_most_once_per_process() {
        assert!(first_probe("cli:memo-test"));
        assert!(!first_probe("cli:memo-test"));
        assert!(!first_probe("cli:memo-test"));
        assert!(first_probe("cli:memo-test-other"));
    }

    #[test]
    fn backup_takeover_never_probes_under_dry_run_or_past_a_manual_pause() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let cfg = crate::config::Config::default();
        let role = crate::state::SlotRole::Implementer;
        let mut store = QuotaStore::default();
        assert!(takeover("cli:codex", role, &cfg, &store, None, &paths, false).is_some());

        store.pause_quota("cli:codex", "possible quota signal: quota");
        assert!(
            takeover("cli:codex", role, &cfg, &store, None, &paths, true).is_none(),
            "dry-run decides on the file alone"
        );

        store.pause_manual("cli:grok", None);
        assert!(
            takeover("cli:grok", role, &cfg, &store, None, &paths, false).is_none(),
            "a manual pause is never probed past"
        );
        assert!(
            !paths.quota_file().exists(),
            "no probe ran, so nothing was written"
        );
    }

    #[test]
    fn rate_limit_error_is_environmental_even_without_paused_store() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit seven_day rejected".into());
        assert_eq!(
            dispatch_stop_cause(&slot, &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn agy_model_pause_holds_only_that_model() {
        let mut store = QuotaStore::default();
        store.pause_quota_until(
            "cli:agy@flash",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "agy individual quota reached",
        );
        store.mark_strong("cli:agy@flash");
        assert!(!is_provider_eligible("cli:agy@flash", &store, None));
        assert!(store.blocks_live_dispatch("cli:agy@flash"));
        assert!(is_provider_eligible("cli:agy@pro", &store, None));
        assert!(!store.blocks_live_dispatch("cli:agy@pro"));

        let mut store = QuotaStore::default();
        store.pause_quota("cli:agy@pro", "pro limit");
        store.mark_strong("cli:agy@pro");
        assert!(!is_provider_eligible("cli:agy@pro", &store, None));
        assert!(is_provider_eligible("cli:agy@flash", &store, None));
    }

    #[test]
    fn agy_cli_level_pause_holds_every_model() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:agy", None);
        assert!(!is_provider_eligible("cli:agy@flash", &store, None));
        assert!(!is_provider_eligible("cli:agy@pro", &store, None));
        assert!(!is_provider_eligible("cli:agy", &store, None));

        let mut legacy = QuotaStore::default();
        legacy.pause_quota_until(
            "cli:agy",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "legacy record",
        );
        legacy.mark_strong("cli:agy");
        assert!(!is_provider_eligible("cli:agy@flash", &legacy, None));
        assert!(!is_provider_eligible("cli:agy@pro", &legacy, None));
        assert!(legacy.blocks_live_dispatch("cli:agy@flash"));
        assert!(legacy.blocks_live_dispatch("cli:agy@pro"));
    }

    #[test]
    fn a_selected_model_decides_whether_the_backup_takes_the_seat() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let mut cfg = crate::config::Config::default();
        cfg.backups.planner = Some("cli:codex".into());
        let mut store = QuotaStore::default();
        store.pause_manual("cli:agy@flash", None);
        let pool = vec!["cli:agy".to_string()];
        let seat = |model| {
            resolve_with_backup(
                SlotRole::Planner,
                0,
                0,
                &pool,
                crate::state::PoolOrigin::Selected,
                &cfg,
                model,
                &store,
                None,
                &paths,
                true,
            )
            .unwrap()
            .0
        };
        assert_eq!(seat(Some("pro")), "cli:agy");
        assert_eq!(seat(Some("flash")), "cli:codex");
    }

    #[test]
    fn a_stop_cause_reads_the_seats_model_key() {
        let mut slot = slot_with_quota(false);
        slot.point_at("cli:agy@flash");
        slot.status = SlotStatus::Failed;
        slot.error = Some("no capacity".into());
        let mut store = QuotaStore::default();
        store.pause_quota("cli:agy@pro", "limit");
        assert_eq!(dispatch_stop_cause(&slot, &store, None), StopCause::Work);
        store.pause_quota("cli:agy@flash", "limit");
        assert_eq!(
            dispatch_stop_cause(&slot, &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn agy_models_are_one_sub_for_the_backup_rule() {
        assert_eq!(
            crate::quota::sub_key("cli:agy@pro"),
            crate::quota::sub_key("cli:agy@flash")
        );
        assert_ne!(
            crate::quota::quota_key("cli:agy@pro"),
            crate::quota::quota_key("cli:agy@flash")
        );
    }

    #[test]
    fn a_family_pause_holds_only_its_bucket_for_eligibility() {
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota_until(
            "cli:claude@opus",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "opus window",
        );
        assert!(!is_provider_eligible("cli:claude@opus", &store, None));
        assert!(is_provider_eligible("cli:claude@sonnet", &store, None));
        assert!(is_provider_eligible("cli:claude@haiku", &store, None));
        assert!(is_provider_eligible("cli:claude", &store, None));
    }

    #[test]
    fn account_scoped_models_share_one_bucket() {
        // claude is not here: its families are separate buckets since O152 (see
        // `a_family_pause_holds_only_its_bucket_for_eligibility`).
        for (seat, other) in [
            ("cli:codex@gpt-5", "cli:codex@mini"),
            ("cli:muse@pro", "cli:muse@flash"),
            ("cli:grok@heavy", "cli:grok@fast"),
            ("cli:pi@model-a", "cli:pi@model-b"),
        ] {
            let mut store = QuotaStore::default();
            store.pause_quota(&crate::quota::quota_key(seat), "limit");
            assert!(
                !is_provider_eligible(other, &store, None),
                "{seat} must hold {other}"
            );
            assert_eq!(
                crate::quota::quota_key(seat),
                crate::quota::quota_key(other),
            );
        }
    }

    #[test]
    fn agy_model_seat_survives_the_detected_cli_set() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:agy@flash", "flash limit");
        let mut set = std::collections::HashSet::new();
        set.insert("cli:agy".to_string());
        assert!(!is_provider_eligible("cli:agy@flash", &store, Some(&set)));
        assert!(is_provider_eligible("cli:agy@pro", &store, Some(&set)));
        assert!(!is_provider_eligible("cli:codex", &store, Some(&set)));
    }
}
