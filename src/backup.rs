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

/// Whether a seat may stay on its provider: quota only (effective status plus
/// manual pauses). Readiness never parks a seat here: an unhealthy non-pin seat
/// yields to its backup or the order rung at resolution time, while pins warn
/// at launch and dispatch anyway. Exit 4 stays quota-only.
pub fn seat_eligible(provider: &str, store: &QuotaStore) -> bool {
    is_provider_eligible(provider, store)
}

/// The readiness failure for a seat no candidate can fill: every skipped
/// provider with its failed probe's detail. `None` when nothing was skipped
/// for health, so callers keep their own error for the genuinely empty pool.
/// Callers surface this as exit 1; the quota gate's exit 4 stays quota-only.
pub fn readiness_error(skips: &[(String, String)]) -> Option<anyhow::Error> {
    if skips.is_empty() {
        return None;
    }
    let detail = skips
        .iter()
        .map(|(provider, message)| format!("{provider}: {message}"))
        .collect::<Vec<_>>()
        .join("; ");
    Some(anyhow::anyhow!(
        "no healthy provider available; readiness probes failed: {detail}"
    ))
}

/// Quota-aware: ineligible when the provider's fleet key sits under a pause
/// record (O106). No availability set: health is checked once, where a seat's
/// provider is resolved and its source is known, never through the quota gate.
pub fn is_provider_eligible(provider: &str, store: &QuotaStore) -> bool {
    let key = crate::quota::quota_key(provider);
    if !store.is_usable(&key) {
        return false;
    }
    if store.effective_status(&key) != crate::quota::ProviderStatus::Available {
        return false;
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
    paths: &crate::paths::SparPaths,
    dry: bool,
    health: &dyn crate::providers::HealthSource,
    cli: bool,
) -> bool {
    if is_provider_eligible(backup, store) && (cli || health.check(backup).usable) {
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
    is_provider_eligible(backup, &fresh) && (cli || health.check(backup).usable)
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
    paths: &crate::paths::SparPaths,
    dry: bool,
    health: &dyn crate::providers::HealthSource,
) -> Option<crate::state::SeatSource> {
    let cli = cfg.cli_backup_keys.contains(role.as_config_key());
    backup_eligible(backup, store, paths, dry, health, cli).then_some(if cli {
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
pub fn resolve_with_backup(
    role: crate::state::SlotRole,
    pool_index: usize,
    role_index: usize,
    pool: &[String],
    pool_origin: crate::state::PoolOrigin,
    cfg: &crate::config::Config,
    primary_model: Option<&str>,
    store: &QuotaStore,
    paths: &crate::paths::SparPaths,
    dry: bool,
    health: &dyn crate::providers::HealthSource,
) -> Option<(String, crate::state::SeatSource)> {
    use crate::workflow::roles_resolve as rr;
    let (prov, src) =
        rr::resolve_seat(role, pool_index, role_index, pool, pool_origin, cfg, health)?;
    let seat = crate::provider_ref::ProviderRef::for_seat(&prov, primary_model).display();
    // An auto-selected seat on a probe-failed provider yields to the declared
    // backup, then to the order rung: resolution carries the source, so the
    // quota gate never sees health. An explicit pin stays (quota and manual
    // pauses still bench it) and the launch warns.
    let unhealthy_auto = !src.is_explicit_pin() && !health.check(&prov).usable;
    if seat_eligible(&seat, store) && !unhealthy_auto {
        return Some((prov, src));
    }
    if let Some(backup_raw) = backup_for_role(role, role_index, cfg) {
        if let Some(source) = takeover(&backup_raw, role, cfg, store, paths, dry, health) {
            if let Ok(parsed) = crate::provider_ref::Pin::parse(&backup_raw) {
                return Some((parsed.display(), source));
            }
        }
    }
    if unhealthy_auto {
        if role == crate::state::SlotRole::Reviewer {
            return None;
        }
        let (order_prov, order_src) = rr::resolve_order_rung(pool_index, cfg, health)?;
        return Some((order_prov, order_src));
    }
    Some((prov, src))
}

pub fn dispatch_stop_cause(slot: &SlotState, store: &QuotaStore) -> StopCause {
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
                store.effective_status(&key) != crate::quota::ProviderStatus::Available
                    || !store.is_usable(&key)
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
                store.effective_status(&key) != crate::quota::ProviderStatus::Available
                    || !store.is_usable(&key)
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

    fn launch_inputs() -> (tempfile::TempDir, crate::config::Config, QuotaStore) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::default();
        (tmp, cfg, QuotaStore::default())
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve(
        role: SlotRole,
        pool: &[String],
        origin: crate::state::PoolOrigin,
        cfg: &crate::config::Config,
        model: Option<&str>,
        store: &QuotaStore,
        paths: &crate::paths::SparPaths,
        dry: bool,
        health: &dyn crate::providers::HealthSource,
    ) -> Option<(String, crate::state::SeatSource)> {
        resolve_with_backup(
            role, 0, 0, pool, origin, cfg, model, store, paths, dry, health,
        )
    }

    #[test]
    fn an_explicit_pin_stays_despite_probe_failure() {
        use crate::providers::StubHealth;
        let (tmp, cfg, store) = launch_inputs();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let pool = vec!["cli:claude".to_string()];
        let health = StubHealth::refuse("cli:claude", "probe failed: not logged in");
        assert_eq!(
            resolve(
                SlotRole::Planner,
                &pool,
                crate::state::PoolOrigin::CliProviders,
                &cfg,
                None,
                &store,
                &paths,
                true,
                &health,
            ),
            Some((
                "cli:claude".to_string(),
                crate::state::SeatSource::CliProviders
            ))
        );
    }

    #[test]
    fn an_unhealthy_file_seat_with_no_healthy_backup_or_order_resolves_nothing() {
        use crate::providers::StubHealth;
        let (tmp, mut cfg, store) = launch_inputs();
        cfg.roles.planner = Some("cli:codex".to_string());
        cfg.providers.order = vec!["cli:codex".to_string()];
        let paths = crate::paths::SparPaths::new(tmp.path());
        let health = StubHealth::refuse("cli:codex", "probe failed: not logged in");
        assert_eq!(
            resolve(
                SlotRole::Planner,
                &[],
                crate::state::PoolOrigin::Synth,
                &cfg,
                None,
                &store,
                &paths,
                true,
                &health,
            ),
            None
        );
    }

    #[test]
    fn an_unhealthy_file_seat_yields_to_the_declared_backup() {
        use crate::providers::StubHealth;
        let (tmp, mut cfg, store) = launch_inputs();
        cfg.roles.planner = Some("cli:claude".to_string());
        cfg.backups.planner = Some("cli:codex".to_string());
        let paths = crate::paths::SparPaths::new(tmp.path());
        let health = StubHealth::refuse("cli:claude", "probe failed: not logged in");
        assert_eq!(
            resolve(
                SlotRole::Planner,
                &[],
                crate::state::PoolOrigin::Synth,
                &cfg,
                None,
                &store,
                &paths,
                true,
                &health,
            ),
            Some(("cli:codex".to_string(), crate::state::SeatSource::Backup))
        );
    }

    #[test]
    fn an_unhealthy_file_seat_with_no_backup_falls_back_to_the_order_rung() {
        use crate::providers::StubHealth;
        let (tmp, mut cfg, store) = launch_inputs();
        cfg.roles.planner = Some("cli:codex".to_string());
        let paths = crate::paths::SparPaths::new(tmp.path());
        let health = StubHealth::refuse("cli:codex", "probe failed: not logged in");
        assert_eq!(
            resolve(
                SlotRole::Planner,
                &[],
                crate::state::PoolOrigin::Synth,
                &cfg,
                None,
                &store,
                &paths,
                true,
                &health,
            ),
            Some((
                "cli:claude".to_string(),
                crate::state::SeatSource::ProvidersOrder
            ))
        );
    }

    #[test]
    fn a_quota_paused_primary_still_yields_to_a_healthy_backup() {
        use crate::providers::StubHealth;
        let (tmp, mut cfg, _) = launch_inputs();
        cfg.roles.planner = Some("cli:claude".to_string());
        cfg.backups.planner = Some("cli:codex".to_string());
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "limit");
        let paths = crate::paths::SparPaths::new(tmp.path());
        let health = StubHealth::allow_all();
        assert_eq!(
            resolve(
                SlotRole::Planner,
                &[],
                crate::state::PoolOrigin::Synth,
                &cfg,
                None,
                &store,
                &paths,
                true,
                &health,
            ),
            Some(("cli:codex".to_string(), crate::state::SeatSource::Backup))
        );
    }

    #[test]
    fn quota_hit_is_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(true);
        slot.status = SlotStatus::Failed;
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Environmental);
    }

    #[test]
    fn paused_provider_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit exceeded".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Environmental);
    }

    #[test]
    fn readiness_failure_without_quota_pause_is_work() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("provider unavailable: cli:claude not found".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn readiness_failure_with_quota_pause_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("provider unavailable: cli:claude not found".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Environmental);
    }

    #[test]
    fn succeeded_slot_is_not_environmental_even_when_provider_paused() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Done;
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn pending_slot_is_not_environmental_even_when_unavailable() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Pending;
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn work_failure_is_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn unavailable_without_set_is_not_environmental_for_work_failure() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn is_provider_eligible_respects_quota() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "q");
        assert!(!is_provider_eligible("cli:claude", &store));
        assert!(is_provider_eligible("cli:grok", &store));
    }

    #[test]
    fn is_provider_eligible_checks_quota_only() {
        let store = QuotaStore::default();
        assert!(is_provider_eligible("cli:totally-bogus-provider", &store));
    }

    #[test]
    fn api_provider_needs_only_quota() {
        let store = QuotaStore::default();
        assert!(is_provider_eligible("api:openai@gpt-5", &store));
        assert_eq!(dispatch_stop_cause(&api_slot(), &store), StopCause::Work);
    }

    #[test]
    fn api_provider_still_respects_quota_pause() {
        let mut store = QuotaStore::default();
        store.pause_quota("api:openai", "q");
        assert!(!is_provider_eligible("api:openai@gpt-5", &store));
        let mut slot = api_slot();
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit exceeded".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Environmental);
    }

    #[test]
    fn timeout_is_work_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("hard ceiling: slot timed out after 10s".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn missing_artifact_is_work_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("missing expected artifact: summary-impl.md".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn adverse_review_is_work_not_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("review verdict: request_changes".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn neutralization_would_fail_work_branch() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        // If Work branch were neutralized to Environmental, this would be Environmental
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
        // Verify environmental still distinct
        let mut env_slot = slot_with_quota(true);
        env_slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&env_slot, &store),
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
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn missing_artifact_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("missing expected artifact: summary-impl.md".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn adverse_review_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("review verdict: request_changes".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn paused_provider_without_work_signal_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("some other failure".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
    }

    #[test]
    fn generic_failure_with_paused_provider_is_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("exit code 1".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
        // Neutralisation: if paused check were leak, this would be Environmental
        let mut env_slot = slot_with_quota(true);
        env_slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&env_slot, &store),
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
        let cause = dispatch_stop_cause(&slot, &store);
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
    fn backup_takeover_needs_quota_and_health_not_binaries() {
        use crate::providers::StubHealth;
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let cfg = crate::config::Config::default();
        let role = crate::state::SlotRole::Implementer;
        let healthy = StubHealth::allow_all();
        let store = QuotaStore::default();
        assert!(takeover("cli:codex", role, &cfg, &store, &paths, true, &healthy).is_some());

        let mut paused = QuotaStore::default();
        paused.pause_quota("cli:codex", "possible quota signal: quota");
        assert!(
            takeover("cli:codex", role, &cfg, &paused, &paths, true, &healthy).is_none(),
            "dry-run decides on the file alone"
        );

        let sick = StubHealth::refuse("cli:codex", "probe failed: not logged in");
        assert!(
            takeover("cli:codex", role, &cfg, &store, &paths, true, &sick).is_none(),
            "a quota-fine file backup still skips probe-failed readiness"
        );

        let mut manual = QuotaStore::default();
        manual.pause_manual("cli:grok", None);
        assert!(
            takeover("cli:grok", role, &cfg, &manual, &paths, false, &healthy).is_none(),
            "a manual pause is never probed past"
        );
        assert!(
            !paths.quota_file().exists(),
            "no probe ran, so nothing was written"
        );
    }

    #[test]
    fn cli_backup_pin_dispatches_despite_probe_failure() {
        use crate::providers::StubHealth;
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let mut cfg = crate::config::Config::default();
        cfg.cli_backup_keys.insert(
            crate::state::SlotRole::Implementer
                .as_config_key()
                .to_string(),
        );
        let sick = StubHealth::refuse("cli:codex", "probe failed: not logged in");
        let store = QuotaStore::default();
        assert_eq!(
            takeover(
                "cli:codex",
                crate::state::SlotRole::Implementer,
                &cfg,
                &store,
                &paths,
                true,
                &sick,
            ),
            Some(crate::state::SeatSource::CliBackup)
        );
    }

    #[test]
    fn rate_limit_error_is_environmental_even_without_paused_store() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit seven_day rejected".into());
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Environmental);
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
        assert!(!is_provider_eligible("cli:agy@flash", &store));
        assert!(store.blocks_live_dispatch("cli:agy@flash"));
        assert!(is_provider_eligible("cli:agy@pro", &store));
        assert!(!store.blocks_live_dispatch("cli:agy@pro"));

        let mut store = QuotaStore::default();
        store.pause_quota("cli:agy@pro", "pro limit");
        store.mark_strong("cli:agy@pro");
        assert!(!is_provider_eligible("cli:agy@pro", &store));
        assert!(is_provider_eligible("cli:agy@flash", &store));
    }

    #[test]
    fn agy_cli_level_pause_holds_every_model() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:agy", None);
        assert!(!is_provider_eligible("cli:agy@flash", &store));
        assert!(!is_provider_eligible("cli:agy@pro", &store));
        assert!(!is_provider_eligible("cli:agy", &store));

        let mut legacy = QuotaStore::default();
        legacy.pause_quota_until(
            "cli:agy",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "legacy record",
        );
        legacy.mark_strong("cli:agy");
        assert!(!is_provider_eligible("cli:agy@flash", &legacy));
        assert!(!is_provider_eligible("cli:agy@pro", &legacy));
        assert!(legacy.blocks_live_dispatch("cli:agy@flash"));
        assert!(legacy.blocks_live_dispatch("cli:agy@pro"));
    }

    #[test]
    fn a_selected_model_decides_whether_the_backup_takes_the_seat() {
        use crate::providers::StubHealth;
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let mut cfg = crate::config::Config::default();
        cfg.backups.planner = Some("cli:codex".into());
        let mut store = QuotaStore::default();
        store.pause_manual("cli:agy@flash", None);
        let health = StubHealth::allow_all();
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
                &paths,
                true,
                &health,
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
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Work);
        store.pause_quota("cli:agy@flash", "limit");
        assert_eq!(dispatch_stop_cause(&slot, &store), StopCause::Environmental);
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
        assert!(!is_provider_eligible("cli:claude@opus", &store));
        assert!(is_provider_eligible("cli:claude@sonnet", &store));
        assert!(is_provider_eligible("cli:claude@haiku", &store));
        assert!(is_provider_eligible("cli:claude", &store));
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
                !is_provider_eligible(other, &store),
                "{seat} must hold {other}"
            );
            assert_eq!(
                crate::quota::quota_key(seat),
                crate::quota::quota_key(other),
            );
        }
    }

    #[test]
    fn a_quota_pause_holds_the_paused_model_only() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:agy@flash", "flash limit");
        assert!(!is_provider_eligible("cli:agy@flash", &store));
        assert!(is_provider_eligible("cli:agy@pro", &store));
        assert!(is_provider_eligible("cli:codex", &store));
    }
}
