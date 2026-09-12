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
        crate::state::SlotRole::Reviewer => cfg.backups.reviewer.get(ordinal).cloned(),
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
    if slot.quota_hit {
        return StopCause::Environmental;
    }
    let key = crate::quota::normalize_key(provider);
    if store.effective_status(&key) != crate::quota::ProviderStatus::Available {
        return StopCause::Environmental;
    }
    if !store.is_usable(&key) {
        return StopCause::Environmental;
    }
    if let Ok(pref) = crate::provider_ref::ProviderRef::parse(provider) {
        if pref.backend == crate::provider_ref::ExecBackend::ApiSdk {
            return StopCause::Work;
        }
    }
    if let Some(set) = available {
        if !set.contains(&key) {
            return StopCause::Environmental;
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
        let slot = slot_with_quota(true);
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn paused_provider_is_environmental() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:claude", "test");
        let slot = slot_with_quota(false);
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, None),
            StopCause::Environmental
        );
    }

    #[test]
    fn unavailable_provider_is_environmental() {
        let store = QuotaStore::default();
        let slot = slot_with_quota(false);
        let mut set = std::collections::HashSet::new();
        set.insert("cli:grok".to_string());
        assert_eq!(
            dispatch_stop_cause(&slot, "cli:claude", &store, Some(&set)),
            StopCause::Environmental
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
        assert_eq!(
            dispatch_stop_cause(
                &slot_with_quota(false),
                "api:openai@gpt-5",
                &store,
                Some(&set)
            ),
            StopCause::Environmental
        );
    }
}
