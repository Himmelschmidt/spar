//! Role → provider resolution (Priority 9). One function replaces the open-coded
//! positional assignment sites so every flow shares the same precedence:
//!
//! **explicit `--providers` (positional `fleet`) > `[roles]` > `[providers].order`**
//!
//! A non-empty `fleet` is an explicit one-off override and always wins positionally.
//! Otherwise the role's `[roles]` entry is used, and finally `[providers].order` as the
//! last resort. `[roles].reviewer` is a list: for `SlotRole::Reviewer`, `idx` selects the
//! reviewer, and past the list end it falls through to `[providers].order[idx]`.
use crate::config::Config;
use crate::state::SlotRole;

/// Resolve the provider ref for one slot. `idx` indexes the positional `fleet` and, for
/// `Reviewer`, selects into the `[roles].reviewer` list; single-valued roles use it only
/// as the `[providers].order` fallback index.
pub fn provider_for(role: SlotRole, idx: usize, fleet: &[String], cfg: &Config) -> Option<String> {
    // 1. Explicit positional fleet wins outright.
    if !fleet.is_empty() {
        return fleet.get(idx).or_else(|| fleet.last()).cloned();
    }
    // 2. [roles].
    let from_roles = match role {
        SlotRole::Planner => cfg.roles.planner.clone(),
        SlotRole::PlanCritic => cfg.roles.plan_critic.clone(),
        SlotRole::Implementer => cfg.roles.implementer.clone(),
        SlotRole::Tester => cfg.roles.tester.clone(),
        SlotRole::TestAuthor => cfg.roles.test_author.clone(),
        SlotRole::Reviewer => cfg.roles.reviewer.get(idx).cloned(),
        _ => None,
    };
    // 3. [providers].order last resort.
    from_roles.or_else(|| cfg.providers.order.get(idx).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_fleet_wins_over_roles() {
        let mut cfg = Config::default();
        cfg.roles.implementer = Some("cli:roles".into());
        let fleet = vec!["cli:explicit".into()];
        assert_eq!(
            provider_for(SlotRole::Implementer, 0, &fleet, &cfg).as_deref(),
            Some("cli:explicit")
        );
    }

    #[test]
    fn roles_used_when_fleet_empty() {
        let mut cfg = Config::default();
        cfg.roles.implementer = Some("cli:grok".into());
        assert_eq!(
            provider_for(SlotRole::Implementer, 0, &[], &cfg).as_deref(),
            Some("cli:grok")
        );
    }

    #[test]
    fn provider_order_is_last_resort() {
        // Default order is [cli:claude, cli:grok, cli:agy]; empty [roles].
        let cfg = Config::default();
        assert_eq!(
            provider_for(SlotRole::Implementer, 1, &[], &cfg).as_deref(),
            Some("cli:grok")
        );
    }

    #[test]
    fn reviewer_list_indexes() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:a".into(), "cli:b".into()];
        assert_eq!(
            provider_for(SlotRole::Reviewer, 0, &[], &cfg).as_deref(),
            Some("cli:a")
        );
        assert_eq!(
            provider_for(SlotRole::Reviewer, 1, &[], &cfg).as_deref(),
            Some("cli:b")
        );
    }

    #[test]
    fn reviewer_list_exhausted_falls_back() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:a".into()];
        // idx 1 is past the one-entry list → [providers].order[1] = cli:grok.
        assert_eq!(
            provider_for(SlotRole::Reviewer, 1, &[], &cfg).as_deref(),
            Some("cli:grok")
        );
    }
}
