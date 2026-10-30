use crate::quota::QuotaStore;
use crate::state::SlotState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum StopCause {
    Environmental,
    Work,
}

pub fn is_provider_eligible(
    provider: &str,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
) -> bool {
    let key = crate::quota::normalize_key(provider);
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
        if !set.contains(&key) {
            return false;
        }
    }
    true
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
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
) -> Option<(String, crate::state::SeatSource)> {
    let primary = crate::workflow::roles_resolve::resolve_seat(
        role,
        pool_index,
        role_index,
        pool,
        pool_origin,
        cfg,
    );
    let (prov, src) = primary?;
    if is_provider_eligible(&prov, store, available) {
        return Some((prov, src));
    }
    let backup_raw = backup_for_role(role, role_index, cfg)?;
    if !is_provider_eligible(&backup_raw, store, available) {
        return Some((prov, src));
    }
    let parsed = crate::runspec::Pin::parse(&backup_raw).ok()?;
    Some((parsed.display(), crate::state::SeatSource::Backup))
}

pub fn dispatch_stop_cause(
    slot: &SlotState,
    provider: &str,
    store: &QuotaStore,
    available: Option<&std::collections::HashSet<String>>,
) -> StopCause {
    if slot.status != crate::state::SlotStatus::Failed {
        return StopCause::Work;
    }
    if slot.quota_hit {
        return StopCause::Environmental;
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
                let key = crate::quota::normalize_key(provider);
                store.effective_status(&key) != crate::quota::ProviderStatus::Available
                    || !store.is_usable(&key)
                    || available.is_some_and(|s| !s.contains(&key))
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
                let key = crate::quota::normalize_key(provider);
                store.effective_status(&key) != crate::quota::ProviderStatus::Available
                    || !store.is_usable(&key)
                    || available.is_some_and(|s| !s.contains(&key))
            }
        {
            return StopCause::Environmental;
        }
    }
    if let Ok(pref) = crate::provider_ref::ProviderRef::parse(provider) {
        if pref.backend == crate::provider_ref::ExecBackend::ApiSdk {
            return StopCause::Work;
        }
    }
    StopCause::Work
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{SlotRole, SlotState, SlotStatus};

    fn slot_with_quota(hit: bool) -> SlotState {
        let mut s = crate::executor::init_slot("test", "cli:claude", SlotRole::Implementer);
        s.quota_hit = hit;
        s
    }

    #[test]
    fn quota_hit_is_environmental() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(true);
        slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
            StopCause::Environmental
        );
    }

    #[test]
    fn succeeded_slot_is_not_environmental_even_when_provider_paused() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Done;
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
    }

    #[test]
    fn pending_slot_is_not_environmental_even_when_unavailable() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Pending;
        let mut set = std::collections::HashSet::new();
        set.insert("cli:grok".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
            StopCause::Work
        );
    }

    #[test]
    fn unavailable_without_set_is_not_environmental_for_work_failure() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
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
            dispatch_stop_cause(
                &slot_with_quota(false),
                "api:openai@gpt-5",
                &store,
                Some(&set)
            ),
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
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit exceeded".into());
        assert_eq!(
            dispatch_stop_cause(&slot, "api:openai@gpt-5", &store, Some(&set)),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
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
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
            StopCause::Work
        );
        // Verify environmental still distinct
        let mut env_slot = slot_with_quota(true);
        env_slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&env_slot, "cli:claude", &store, Some(&set)),
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
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
    }

    #[test]
    fn missing_artifact_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("missing expected artifact: summary-impl.md".into());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
    }

    #[test]
    fn adverse_review_with_paused_provider_still_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("review verdict: request_changes".into());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
    }

    #[test]
    fn paused_provider_without_work_signal_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("some other failure".into());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
    }

    #[test]
    fn generic_failure_with_paused_provider_is_work() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("exit code 1".into());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Work
        );
        // Neutralisation: if paused check were leak, this would be Environmental
        let mut env_slot = slot_with_quota(true);
        env_slot.status = SlotStatus::Failed;
        assert_eq!(
            dispatch_stop_cause(&env_slot, "cli:claude", &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn rate_limit_error_is_environmental_even_without_paused_store() {
        let store = QuotaStore::default();
        let mut slot = slot_with_quota(false);
        slot.status = SlotStatus::Failed;
        slot.error = Some("rate limit seven_day rejected".into());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Environmental
        );
    }
}
