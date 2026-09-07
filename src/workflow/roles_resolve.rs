//! Role -> provider resolution and the fleet shape it implies (feature 011). One place
//! keys every seat off the same precedence:
//!
//! **CLI `--role` > CLI `--providers`/`--select` pool > `[roles]` > `[providers].order`**
//!
//! `--role` for a given role always wins, even under an explicit pool (Bug B). A
//! non-empty reviewer list — from `--role reviewer=…` or `[roles].reviewer` — is an exact
//! panel: past its end the answer is `None`, never the next `[providers].order` entry
//! (Bug A). `[providers].order` is the last resort, and only for a role nothing else
//! named: for `Reviewer` that means an *unpinned* panel, keyed by reviewer ordinal, not
//! by pool position.
use super::CommonOpts;
use crate::config::Config;
use crate::provider_ref::ProviderRef;
use crate::state::{FleetSeat, PoolOrigin, SeatSource, SlotRole};
use crate::util::sanitize_slot;

/// A run's positional pool provenance for one invocation, from the flags actually
/// passed: an explicit `--providers` or `--select` is a real override; absent both, the
/// pool was (or will be) synthesized from `[roles]` / `[providers].order`.
pub fn pool_origin_for(opts: &CommonOpts) -> PoolOrigin {
    if !opts.providers.is_empty() {
        PoolOrigin::CliProviders
    } else if !opts.select.is_empty() {
        PoolOrigin::Selected
    } else {
        PoolOrigin::Synth
    }
}

/// Reviewer panel width when nothing pins it (Bug A: this is a floor for an *unpinned*
/// panel, never a target the pinned case gets padded up to).
pub const DEFAULT_REVIEWERS: usize = 2;

/// How many reviewer seats this run's panel has. A CLI `--role reviewer=…` list is exact
/// even under `--fleet small`'s override (a second explicit `--role reviewer` always
/// means a bigger panel than the preset asked for). A file `[roles].reviewer` list is
/// exact too, except `--fleet small` may narrow it (never widen it) to keep the first
/// pin. Unpinned falls back to the preset override, else `DEFAULT_REVIEWERS`.
pub fn panel_size(cfg: &Config) -> usize {
    if cfg
        .cli_role_keys
        .contains(SlotRole::Reviewer.as_config_key())
    {
        return cfg.roles.reviewer.len();
    }
    if !cfg.roles.reviewer.is_empty() {
        return match cfg.fleet_reviewer_override {
            Some(n) => n.min(cfg.roles.reviewer.len()),
            None => cfg.roles.reviewer.len(),
        };
    }
    cfg.fleet_reviewer_override.unwrap_or(DEFAULT_REVIEWERS)
}

/// The implement loop's pool width: implementer plus exactly the panel it will dispatch.
/// Independent of `max_agents`, which used to size this and manufactured phantom reviewer
/// seats out of `[providers].order` whenever the panel was pinned smaller than it (Bug A).
pub fn pool_width(cfg: &Config) -> usize {
    1 + panel_size(cfg)
}

/// Whether the reviewer panel is pinned (by CLI `--role`, by `[roles].reviewer`, or
/// narrowed by `--fleet small`), i.e. an exclusion list rather than a floor. Widening a
/// pinned panel must duplicate an already-dispatched seat, never draw from
/// `[providers].order` (Bug A) — including the unpinned-but-narrowed `small` panel, or
/// widening would silently re-add the seat the preset was asked to drop.
pub fn reviewer_panel_pinned(cfg: &Config) -> bool {
    cfg.cli_role_keys
        .contains(SlotRole::Reviewer.as_config_key())
        || !cfg.roles.reviewer.is_empty()
        || cfg.fleet_reviewer_override.is_some()
}

/// Whether the reviewer panel has an actual pin *list* to rotate within (CLI
/// `--role reviewer=…` or `[roles].reviewer`), as opposed to `--fleet small`'s panel-size
/// override alone. `reviewer_panel_pinned` above also counts a preset-only narrowing as
/// "pinned" so widening never imports from `[providers].order` — but rotation on a panel
/// with no real pins has nothing to rotate *within*: it must still fall through to the
/// pool / `[providers].order` the way an ordinary unpinned panel does, or a failed
/// unpinned-but-narrowed reviewer loses its only retry.
pub fn reviewer_panel_has_pins(cfg: &Config) -> bool {
    cfg.cli_role_keys
        .contains(SlotRole::Reviewer.as_config_key())
        || !cfg.roles.reviewer.is_empty()
}

fn singleton_role_value(role: SlotRole, cfg: &Config) -> Option<String> {
    match role {
        SlotRole::Planner => cfg.roles.planner.clone(),
        SlotRole::PlanCritic => cfg.roles.plan_critic.clone(),
        SlotRole::Implementer => cfg.roles.implementer.clone(),
        SlotRole::Tester => cfg.roles.tester.clone(),
        SlotRole::TestAuthor => cfg.roles.test_author.clone(),
        _ => None,
    }
}

/// Resolve one seat's provider and where it came from.
///
/// `pool_index` is this seat's position in the run's positional pool (`state.providers`);
/// `role_index` is only distinct from it for `Reviewer`, where it is the reviewer's own
/// ordinal (0-based) into `[roles].reviewer` / the order fallback. `pool` plus
/// `pool_origin` describe the run's resolved pool: a `Synth` pool was itself built from
/// `[roles]`/`[providers].order` (see `[model_select::synthesize_from_roles]`) and must
/// never be read back as if it were an explicit override — every seat drawn from it is
/// resolved fresh from `cfg` instead, which is what keeps a `Synth` pool's own values
/// consistent with what this function would say anyway.
pub fn resolve_seat(
    role: SlotRole,
    pool_index: usize,
    role_index: usize,
    pool: &[String],
    pool_origin: PoolOrigin,
    cfg: &Config,
) -> Option<(String, SeatSource)> {
    // 1. CLI --role for this role always wins, even under an explicit pool (Bug B).
    if cfg.cli_role_keys.contains(role.as_config_key()) {
        let value = if role == SlotRole::Reviewer {
            cfg.roles.reviewer.get(role_index).cloned()
        } else {
            singleton_role_value(role, cfg)
        };
        return value.map(|p| (p, SeatSource::CliRole));
    }
    // 2. An explicit pool (CLI --providers, or a --select result) wins outright.
    match pool_origin {
        PoolOrigin::CliProviders if !pool.is_empty() => {
            return pool
                .get(pool_index)
                .cloned()
                .map(|p| (p, SeatSource::CliProviders));
        }
        PoolOrigin::Selected if !pool.is_empty() => {
            return pool
                .get(pool_index)
                .cloned()
                .map(|p| (p, SeatSource::ModelSelect));
        }
        _ => {}
    }
    // 3. [roles]. A non-empty reviewer list is exact: past its end the answer is None,
    // never [providers].order (Bug A) — only an empty list falls through to step 4.
    if role == SlotRole::Reviewer {
        if !cfg.roles.reviewer.is_empty() {
            return cfg
                .roles
                .reviewer
                .get(role_index)
                .cloned()
                .map(|p| (p, SeatSource::RolesFile));
        }
    } else if let Some(p) = singleton_role_value(role, cfg) {
        return Some((p, SeatSource::RolesFile));
    }
    // 4. [providers].order last resort: reviewer keys by its own ordinal (so an unpinned
    // panel reads order[0], order[1], … regardless of pool width); singles key by pool
    // position.
    let order_idx = if role == SlotRole::Reviewer {
        role_index
    } else {
        pool_index
    };
    cfg.providers
        .order
        .get(order_idx)
        .cloned()
        .map(|p| (p, SeatSource::ProvidersOrder))
}

/// Split a resolved ref into `(storage_key, model)`, matching `executor::init_slot_model`:
/// an explicit `@model` on the ref beats a model chosen elsewhere (e.g. model-select).
fn split_model(raw: &str, fallback_model: Option<String>) -> (String, Option<String>) {
    match ProviderRef::parse(raw) {
        Ok(pref) => (pref.storage_key(), pref.model.or(fallback_model)),
        Err(_) => (raw.to_string(), fallback_model),
    }
}

/// Build the implement panel — implementer plus the resolved reviewer panel — as
/// `FleetSeat`s. Used both to create the real slots (`projected: false`) and to report
/// the panel a plan gate has not dispatched yet (`projected: true`); the id formula is
/// identical either way; so a projection can never drift from what gets dispatched.
pub fn build_implement_seats(
    cfg: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    projected: bool,
    model_for: &dyn Fn(usize) -> Option<String>,
) -> Vec<FleetSeat> {
    let mut out = Vec::new();
    if let Some((raw, source)) = resolve_seat(SlotRole::Implementer, 0, 0, pool, pool_origin, cfg) {
        let (provider, model) = split_model(&raw, model_for(0));
        out.push(FleetSeat {
            seat: "impl".into(),
            role: SlotRole::Implementer,
            provider,
            model,
            source,
            projected,
        });
    }
    for r in 0..panel_size(cfg) {
        let pool_index = r + 1;
        let Some((raw, source)) =
            resolve_seat(SlotRole::Reviewer, pool_index, r, pool, pool_origin, cfg)
        else {
            continue;
        };
        let (provider, model) = split_model(&raw, model_for(pool_index));
        out.push(FleetSeat {
            seat: format!("review-{r}-{}", sanitize_slot(&provider)),
            role: SlotRole::Reviewer,
            provider,
            model,
            source,
            projected,
        });
    }
    out
}

/// Per-seat `SeatSource` for a workflow that dispatches `count` uniform-role slots
/// (review, peer, roles, arena) directly, without going through `build_implement_seats`.
/// Resolves each position through `resolve_seat` instead of stamping every seat with
/// `pool_origin.as_seat_source()`: a `Synth` pool can be a mix of `[roles]` and
/// `[providers].order` seats, and a single blanket guess mislabels whichever positions
/// did not actually take that rung.
pub fn resolve_seat_sources(
    role: SlotRole,
    count: usize,
    pool: &[String],
    pool_origin: PoolOrigin,
    cfg: &Config,
) -> Vec<SeatSource> {
    (0..count)
        .map(|i| {
            resolve_seat(role, i, i, pool, pool_origin, cfg)
                .map(|(_, s)| s)
                .unwrap_or(SeatSource::Unknown)
        })
        .collect()
}

/// Projection of the implement panel a plan run has not dispatched yet, shown at the
/// plan gate (feature 011, item C) so the human deciding whether to pay for it can
/// actually see it. Simulates the exact pool a bare `implement --run <id>` (no flags)
/// resolves: a straight call into `build_implement_seats` with the run's own `pool` /
/// `pool_origin`, the same two `resolve_seat` already uses to decide whether the pool
/// even applies (rung 2, only for an explicit `CliProviders`/`Selected` origin) — no
/// separate branch is needed here to mirror that decision, and one previously caused the
/// projection to silently drop the pool whenever `[roles]` held anything at all (e.g. a
/// single CLI-pinned reviewer alongside an explicit `--providers` pool).
///
/// Reads `model-select.json` read-only, the same `c.slot == idx` lookup
/// `run_from_approved`'s own `model_for` uses, so a model already chosen for one of these
/// pool positions (e.g. a prior round's `implement --select` preserved across a re-plan)
/// shows up here too instead of always reporting `model: null`. `paths`/`run_id` are
/// `None` only where no run is bound yet; this never writes the artifact.
pub fn project_implement_fleet(
    cfg: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    paths: Option<&crate::paths::SparPaths>,
    run_id: Option<&str>,
) -> Vec<FleetSeat> {
    let art = match (paths, run_id) {
        (Some(paths), Some(run_id)) => crate::model_select::load_select_artifact(paths, run_id)
            .ok()
            .flatten(),
        _ => None,
    };
    let model_for = |idx: usize| -> Option<String> {
        art.as_ref().and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.slot == idx)
                .and_then(|c| c.model.clone())
        })
    };
    build_implement_seats(cfg, pool, pool_origin, true, &model_for)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_reviewer_list_is_exact_no_order_fallback() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        assert_eq!(
            resolve_seat(SlotRole::Reviewer, 1, 0, &[], PoolOrigin::Synth, &cfg).map(|(p, _)| p),
            Some("cli:codex".into())
        );
        // Past the one-entry list: None, never [providers].order[1].
        assert_eq!(
            resolve_seat(SlotRole::Reviewer, 2, 1, &[], PoolOrigin::Synth, &cfg),
            None
        );
    }

    #[test]
    fn unpinned_reviewer_uses_order_by_ordinal() {
        let cfg = Config::default();
        assert_eq!(
            resolve_seat(SlotRole::Reviewer, 1, 0, &[], PoolOrigin::Synth, &cfg),
            Some(("cli:claude".into(), SeatSource::ProvidersOrder))
        );
        assert_eq!(
            resolve_seat(SlotRole::Reviewer, 2, 1, &[], PoolOrigin::Synth, &cfg),
            Some(("cli:grok".into(), SeatSource::ProvidersOrder))
        );
    }

    #[test]
    fn cli_role_outranks_explicit_pool() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        cfg.cli_role_keys.insert("reviewer".into());
        let pool = vec!["cli:claude".into(), "cli:grok".into(), "cli:agy".into()];
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                1,
                0,
                &pool,
                PoolOrigin::CliProviders,
                &cfg
            ),
            Some(("cli:codex".into(), SeatSource::CliRole))
        );
    }

    #[test]
    fn explicit_pool_beats_roles_file() {
        let mut cfg = Config::default();
        cfg.roles.implementer = Some("cli:roles".into());
        let pool = vec!["cli:explicit".into()];
        assert_eq!(
            resolve_seat(
                SlotRole::Implementer,
                0,
                0,
                &pool,
                PoolOrigin::CliProviders,
                &cfg
            )
            .map(|(p, _)| p),
            Some("cli:explicit".into())
        );
    }

    #[test]
    fn default_pool_width_ignores_max_agents() {
        let mut cfg = Config {
            max_agents: 6,
            ..Config::default()
        };
        cfg.roles.reviewer = vec!["cli:codex".into()];
        assert_eq!(pool_width(&cfg), 2);
    }

    #[test]
    fn panel_size_two_pins() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:a".into(), "cli:b".into()];
        assert_eq!(panel_size(&cfg), 2);
        assert_eq!(pool_width(&cfg), 3);
    }

    /// A `--fleet small` narrowing with no underlying `[roles].reviewer` pin is "pinned"
    /// for widening (never import from `[providers].order`), but has no real pin list to
    /// rotate within: `reviewer_panel_has_pins` must say `false` so rotation still falls
    /// through to the pool / order, unlike `reviewer_panel_pinned`.
    #[test]
    fn preset_only_narrowing_is_pinned_but_has_no_pins() {
        let cfg = Config {
            fleet_reviewer_override: Some(1),
            ..Config::default()
        };
        assert!(reviewer_panel_pinned(&cfg));
        assert!(!reviewer_panel_has_pins(&cfg));
    }

    #[test]
    fn a_real_pin_list_counts_as_has_pins() {
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        assert!(reviewer_panel_has_pins(&cfg));
    }
}
