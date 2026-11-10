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
///
/// Health is read here, once, where the source is known: the order rung skips
/// probe-failed entries, while pins and `[roles]` file entries return unchecked
/// (pins warn at launch; an unhealthy file seat yields to its backup in the
/// backup-resolution layer, never to the quota gate).
#[allow(clippy::too_many_arguments)]
pub fn resolve_seat(
    role: SlotRole,
    pool_index: usize,
    role_index: usize,
    pool: &[String],
    pool_origin: PoolOrigin,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
    used: &[String],
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
    resolve_order_rung(role, order_idx, cfg, health, used)
}

/// The `[providers].order` last-resort rung on its own. Reviewers keep their
/// ordinal: the order filtered to usable entries first, then indexed, so a
/// probe-failed entry never annexes a seat and surviving reviewers never
/// collapse onto one provider. A held ordinal entry falls forward to the
/// first unheld usable entry, so a re-seated reviewer never lands on a
/// sibling's provider. A singleton consults the usable entries from
/// its pool position forward and takes the first one no earlier seat holds,
/// so parallel singletons spread across the order instead of collapsing onto
/// one provider; when every usable entry already seats an earlier singleton
/// it collapses onto the first usable one rather than fail while a provider
/// stands ready. Past the usable entries the seat is `None`, never a wrong
/// provider. Callers pass the providers earlier seats took as `used`.
pub fn resolve_order_rung(
    role: SlotRole,
    order_idx: usize,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
    used: &[String],
) -> Option<(String, SeatSource)> {
    if role == SlotRole::Reviewer {
        let usable: Vec<&String> = cfg
            .providers
            .order
            .iter()
            .filter(|p| health.check(p).usable)
            .collect();
        return usable
            .get(order_idx)
            .copied()
            .filter(|p| !seated(used, p))
            .or_else(|| usable.iter().copied().find(|p| !seated(used, p)))
            .cloned()
            .map(|p| (p, SeatSource::ProvidersOrder));
    }
    let usable_from_here = cfg
        .providers
        .order
        .iter()
        .skip(order_idx)
        .filter(|p| health.check(p).usable);
    usable_from_here
        .clone()
        .find(|p| !seated(used, p))
        // Every usable entry already seats an earlier singleton: collapse onto
        // the first usable one rather than fail while a provider stands ready.
        .or_else(|| usable_from_here.clone().next())
        .cloned()
        .map(|p| (p, SeatSource::ProvidersOrder))
}

pub(crate) fn seated(used: &[String], candidate: &str) -> bool {
    used.iter().any(|u| {
        crate::provider_ref::ProviderRef::parse(u)
            .map(|r| r.storage_key())
            .unwrap_or_else(|_| u.to_string())
            == crate::provider_ref::ProviderRef::parse(candidate)
                .map(|r| r.storage_key())
                .unwrap_or_else(|_| candidate.to_string())
    })
}

/// Every provider a non-pin rung named but health refused, with the failed
/// probe's detail: the `[roles]` singleton file seat plus the `[providers].order`
/// entries the seat actually consulted. Reviewer file lists are excluded (an
/// exact panel narrows rather than errors). A singleton scans the order from
/// its pool position forward, so earlier entries belong to earlier seats and
/// are not its failure; a reviewer ordinal draws from the whole usable list,
/// so every failed entry shrank it. Callers use this on the `None` path to
/// fail with a readiness-specific message instead of a generic "no provider" one.
pub fn health_skips_for_seat(
    role: SlotRole,
    order_idx: usize,
    pool: &[String],
    pool_origin: PoolOrigin,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
) -> Vec<(String, String)> {
    if matches!(pool_origin, PoolOrigin::CliProviders | PoolOrigin::Selected) && !pool.is_empty() {
        return Vec::new();
    }
    let mut skips = Vec::new();
    if role != SlotRole::Reviewer {
        if let Some(entry) = singleton_role_value(role, cfg) {
            if let Some(message) = health.check(&entry).probe_message {
                skips.push((entry, message));
            }
        }
    }
    let consulted = if role == SlotRole::Reviewer {
        &cfg.providers.order[..]
    } else {
        cfg.providers.order.get(order_idx..).unwrap_or(&[])
    };
    for entry in consulted {
        if let Some(message) = health.check(entry).probe_message {
            skips.push((entry.clone(), message));
        }
    }
    skips
}

/// Split a resolved ref into `(storage_key, model)`, matching `executor::init_slot_model`:
/// an explicit `@model` on the ref beats a model chosen elsewhere (e.g. model-select).
pub(crate) fn split_model(raw: &str, fallback_model: Option<String>) -> (String, Option<String>) {
    match ProviderRef::parse(raw) {
        Ok(pref) => (pref.storage_key(), pref.model.or(fallback_model)),
        Err(_) => (raw.to_string(), fallback_model),
    }
}

/// Build the implement panel — implementer plus the resolved reviewer panel — as
/// `FleetSeat`s. Used both to create the real slots (`projected: false`) and to report
/// the panel a plan gate has not dispatched yet (`projected: true`); the id formula is
/// identical either way; so a projection can never drift from what gets dispatched.
///
/// Every seat settles through `backup::resolve_with_backup`, the same single point
/// plan slots resolve through: an unhealthy non-pin provider yields to its backup,
/// then the `[providers].order` rung (a reviewer entry narrows the panel instead),
/// and only when nothing healthy remains does the missing implementer fail with a
/// readiness error naming the failed probes. Pins stay and warn at launch. Health
/// never reaches the quota gate, so an unhealthy seat can never park with exit 4.
#[allow(clippy::too_many_arguments)]
pub fn build_implement_seats(
    cfg: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    projected: bool,
    health: &dyn crate::providers::HealthSource,
    store: &crate::quota::QuotaStore,
    paths: &crate::paths::SparPaths,
    dry: bool,
    route: crate::providers::RecordedRoute<'_>,
    model_for: &dyn Fn(usize) -> Option<String>,
) -> anyhow::Result<Vec<FleetSeat>> {
    let mut out = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let model = model_for(0);
    match crate::backup::resolve_with_backup(
        SlotRole::Implementer,
        0,
        0,
        pool,
        pool_origin,
        cfg,
        model.as_deref(),
        store,
        paths,
        dry,
        route,
        health,
        &used,
    ) {
        Some((raw, source)) => {
            let (provider, model) = split_model(&raw, model_for(0));
            used.push(provider.clone());
            out.push(FleetSeat {
                seat: "impl".into(),
                role: SlotRole::Implementer,
                provider,
                model,
                source,
                projected,
            });
        }
        None => {
            let skips =
                health_skips_for_seat(SlotRole::Implementer, 0, pool, pool_origin, cfg, health);
            if let Some(err) = crate::backup::readiness_error(&skips) {
                return Err(err);
            }
            anyhow::bail!("no provider resolved for implementer");
        }
    }
    for r in 0..panel_size(cfg) {
        let pool_index = r + 1;
        let model = model_for(pool_index);
        let Some((raw, source)) = crate::backup::resolve_with_backup(
            SlotRole::Reviewer,
            pool_index,
            r,
            pool,
            pool_origin,
            cfg,
            model.as_deref(),
            store,
            paths,
            dry,
            route,
            health,
            &used,
        ) else {
            continue;
        };
        let (provider, model) = split_model(&raw, model_for(pool_index));
        used.push(provider.clone());
        out.push(FleetSeat {
            seat: format!("review-{r}-{}", sanitize_slot(&provider)),
            role: SlotRole::Reviewer,
            provider,
            model,
            source,
            projected,
        });
    }
    Ok(out)
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
    health: &dyn crate::providers::HealthSource,
) -> Vec<SeatSource> {
    (0..count)
        .map(|i| {
            resolve_seat(role, i, i, pool, pool_origin, cfg, health, &[])
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
///
/// `pool` is expanded to `pool_width(cfg)` with `providers::pick_providers` before
/// resolution, the same cycling `prepare_implement_slots` applies to `state.providers` —
/// otherwise a pool shorter than the panel (e.g. one explicit `--providers` entry with the
/// two-reviewer default) resolves `None` past its own length and the projection silently
/// drops reviewer seats the implement panel still dispatches, cycled from that same entry.
/// `pool_dry` mirrors the launch's own dry flag for that expansion (a dry launch keeps
/// providers whose binaries are absent); `dry` stays write-suppression for the quota
/// store, always true in a projection.
#[allow(clippy::too_many_arguments)]
pub fn project_implement_fleet(
    cfg: &Config,
    pool: &[String],
    pool_origin: PoolOrigin,
    health: &dyn crate::providers::HealthSource,
    store: &crate::quota::QuotaStore,
    paths: &crate::paths::SparPaths,
    dry: bool,
    pool_dry: bool,
    route: crate::providers::RecordedRoute<'_>,
    run_id: Option<&str>,
) -> anyhow::Result<Vec<FleetSeat>> {
    let width = pool_width(cfg);
    let expanded =
        crate::providers::pick_providers(pool, width, Some(pool), pool_dry, pool_origin, health);
    let pool = if expanded.is_empty() { pool } else { &expanded };
    let art = match run_id {
        Some(run_id) => crate::model_select::load_select_artifact(paths, run_id)
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
    build_implement_seats(
        cfg,
        pool,
        pool_origin,
        true,
        health,
        store,
        paths,
        dry,
        route,
        &model_for,
    )
}

/// The seats a NEW launch of `kind` would fill, for `--validate-only` (O147): the pool the
/// launch resolves (`model_select::resolve_providers`, the plan gate's own projection for
/// the implement panel), probed for real through `health`, so the projection seats the
/// same panel dispatch would. `None` for `--select`, which ranks the bench cache at
/// launch, and for the positional workflows (arena, peer, roles), whose seats are not
/// role-resolved.
pub fn launch_fleet(
    kind: crate::cli::WorkflowKind,
    opts: &CommonOpts,
    cfg: &Config,
    health: &dyn crate::providers::HealthSource,
    paths: &crate::paths::SparPaths,
) -> anyhow::Result<Option<Vec<FleetSeat>>> {
    use crate::cli::WorkflowKind;
    if !opts.select.is_empty() {
        return Ok(None);
    }
    let urgency = crate::model_select::Urgency::parse(&opts.urgency)?;
    let origin = pool_origin_for(opts);
    let store = crate::quota::QuotaStore::load(paths).unwrap_or_default();
    // Validate-only never re-probes a quota pause and never records anything:
    // `probe::probe` writes the store, so a projection always resolves dry.
    // Primary health still probes for real through `health`.
    let dry = true;
    let resolve = |n: usize, roles: &[&str]| -> anyhow::Result<Vec<String>> {
        Ok(crate::model_select::resolve_providers(
            &opts.providers,
            None,
            urgency,
            n,
            roles,
            cfg,
            true,
            health,
        )?
        .providers)
    };
    let seat = |role: SlotRole, id: String, raw: String, source: SeatSource, projected: bool| {
        let (provider, model) = split_model(&raw, None);
        FleetSeat {
            seat: id,
            role,
            provider,
            model,
            source,
            projected,
        }
    };
    let implement_panel = |pool: &[String], projected: bool| -> anyhow::Result<Vec<FleetSeat>> {
        // A fresh launch has no run to record a route yet: the ambient
        // environment is the route the launch would stamp.
        let mut out = project_implement_fleet(
            cfg,
            pool,
            origin,
            health,
            &store,
            paths,
            dry,
            opts.dry_run,
            crate::providers::RecordedRoute::ambient(),
            None,
        )?;
        let used: Vec<String> = out.iter().map(|s| s.provider.clone()).collect();
        out.extend(crate::workflow::implement::project_tester_seat(
            cfg,
            opts.dry_run,
            health,
            pool,
            origin,
            None,
            None,
            &used,
        ));
        for s in &mut out {
            s.projected = projected;
        }
        Ok(out)
    };
    Ok(match kind {
        WorkflowKind::Plan => {
            let mut specs = vec![(SlotRole::Planner, "planner")];
            if cfg.critic.enabled {
                specs.push((SlotRole::PlanCritic, "critic"));
            }
            let mut roles: Vec<&str> = specs.iter().map(|(r, _)| r.as_config_key()).collect();
            if cfg.spec.enabled {
                roles.push(SlotRole::TestAuthor.as_config_key());
            }
            let requested = resolve(roles.len(), &roles)?;
            let pool = crate::providers::pick_providers(
                &requested,
                roles.len(),
                Some(&requested),
                opts.dry_run,
                origin,
                health,
            );
            let mut out: Vec<FleetSeat> = Vec::new();
            let mut skips = Vec::new();
            for (idx, (role, prefix)) in specs.into_iter().enumerate() {
                let used: Vec<String> = out.iter().map(|s| s.provider.clone()).collect();
                match crate::backup::resolve_with_backup(
                    role,
                    idx,
                    idx,
                    &pool,
                    origin,
                    cfg,
                    None,
                    &store,
                    paths,
                    dry,
                    crate::providers::RecordedRoute::ambient(),
                    health,
                    &used,
                ) {
                    Some((raw, source)) => {
                        let id = format!("{prefix}-{}", sanitize_slot(&raw));
                        out.push(seat(role, id, raw, source, false));
                    }
                    None => {
                        skips.extend(health_skips_for_seat(role, idx, &pool, origin, cfg, health))
                    }
                }
            }
            if out.is_empty() {
                if let Some(err) = crate::backup::readiness_error(&skips) {
                    return Err(err);
                }
            }
            if cfg.spec.enabled {
                let used: Vec<String> = out.iter().map(|s| s.provider.clone()).collect();
                let (raw, source) = crate::workflow::plan::resolve_spec_provider(
                    cfg, health, &pool, origin, &used,
                )?;
                let id = format!("test-author-{}", sanitize_slot(&raw));
                out.push(seat(SlotRole::TestAuthor, id, raw, source, false));
            }
            out.extend(implement_panel(&requested, true)?);
            Some(out)
        }
        WorkflowKind::Loop => {
            let n = pool_width(cfg);
            let roles: Vec<&str> = std::iter::once(SlotRole::Implementer.as_config_key())
                .chain(std::iter::repeat(SlotRole::Reviewer.as_config_key()))
                .take(n)
                .collect();
            Some(implement_panel(&resolve(n, &roles)?, false)?)
        }
        WorkflowKind::Review => {
            let n = if opts.providers.is_empty() {
                panel_size(cfg)
            } else {
                opts.providers.len()
            };
            let roles: Vec<&str> = (0..n).map(|_| SlotRole::Reviewer.as_config_key()).collect();
            let requested = resolve(n, &roles)?;
            let pool = crate::providers::pick_providers(
                &requested,
                n,
                Some(&requested),
                opts.dry_run,
                origin,
                health,
            );
            let sources = resolve_seat_sources(
                SlotRole::Reviewer,
                pool.len(),
                &requested,
                origin,
                cfg,
                health,
            );
            let out = pool
                .iter()
                .enumerate()
                .map(|(i, raw)| {
                    let id = format!("review-{i}-{}", sanitize_slot(raw));
                    let source = sources.get(i).copied().unwrap_or(SeatSource::Unknown);
                    seat(SlotRole::Reviewer, id, raw.clone(), source, false)
                })
                .collect();
            Some(out)
        }
        WorkflowKind::Arena | WorkflowKind::Peer | WorkflowKind::Roles => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::StubHealth;

    #[test]
    fn pinned_reviewer_list_is_exact_no_order_fallback() {
        let health = StubHealth::allow_all();
        let mut cfg = Config::default();
        cfg.roles.reviewer = vec!["cli:codex".into()];
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                1,
                0,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            )
            .map(|(p, _)| p),
            Some("cli:codex".into())
        );
        // Past the one-entry list: None, never [providers].order[1].
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                2,
                1,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            ),
            None
        );
    }

    #[test]
    fn unpinned_reviewer_uses_order_by_ordinal() {
        let health = StubHealth::allow_all();
        let cfg = Config::default();
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                1,
                0,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            ),
            Some(("cli:claude".into(), SeatSource::ProvidersOrder))
        );
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                2,
                1,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            ),
            Some(("cli:grok".into(), SeatSource::ProvidersOrder))
        );
    }

    #[test]
    fn order_rung_filters_unhealthy_first_then_indexes() {
        let health = StubHealth::refuse("cli:claude", "probe failed: not logged in");
        let cfg = Config::default();
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                1,
                0,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            ),
            Some(("cli:grok".into(), SeatSource::ProvidersOrder))
        );
        assert_eq!(
            resolve_seat(
                SlotRole::Reviewer,
                2,
                1,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            ),
            Some(("cli:agy".into(), SeatSource::ProvidersOrder))
        );
        assert_eq!(
            resolve_order_rung(SlotRole::Reviewer, 2, &cfg, &health, &[]),
            None,
            "past the usable entries is None, never a wrong provider"
        );
    }

    #[test]
    fn unhealthy_singleton_and_order_are_reported_as_skips() {
        let health = StubHealth::refuse_all(&[
            ("cli:roles", "probe failed: not logged in"),
            ("cli:claude", "probe failed: not logged in"),
        ]);
        let mut cfg = Config::default();
        cfg.roles.implementer = Some("cli:roles".into());
        let skips = health_skips_for_seat(
            SlotRole::Implementer,
            0,
            &[],
            PoolOrigin::Synth,
            &cfg,
            &health,
        );
        assert_eq!(skips.len(), 2);
        assert!(skips.iter().any(|(p, _)| p == "cli:roles"));
        assert!(skips.iter().any(|(p, _)| p == "cli:claude"));
    }

    #[test]
    fn singleton_keeps_pool_position_and_falls_forward() {
        let health = StubHealth::refuse("cli:claude", "probe failed: not logged in");
        let cfg = Config::default();
        assert_eq!(
            resolve_seat(
                SlotRole::Implementer,
                0,
                0,
                &[],
                PoolOrigin::Synth,
                &cfg,
                &health,
                &[]
            ),
            Some(("cli:grok".into(), SeatSource::ProvidersOrder))
        );
        assert_eq!(
            resolve_order_rung(SlotRole::Implementer, 2, &cfg, &health, &[]),
            Some(("cli:agy".into(), SeatSource::ProvidersOrder)),
            "a later singleton is not failed by an earlier sick entry"
        );
        assert_eq!(
            resolve_order_rung(SlotRole::Implementer, 3, &cfg, &health, &[]),
            None,
            "past the end is None, never a wrong provider"
        );
    }

    #[test]
    fn singleton_prefers_an_unseated_usable_order_entry() {
        let health = StubHealth::refuse("cli:claude", "probe failed: not logged in");
        let cfg = Config::default();
        let used = vec!["cli:grok".to_string()];
        assert_eq!(
            resolve_order_rung(SlotRole::Implementer, 0, &cfg, &health, &used),
            Some(("cli:agy".into(), SeatSource::ProvidersOrder)),
            "a later singleton is not collapsed onto an earlier seat while agy stands unused"
        );
    }

    #[test]
    fn singleton_collapses_rather_than_failing_when_all_usable_are_seated() {
        let health = StubHealth::refuse("cli:claude", "probe failed: not logged in");
        let cfg = Config::default();
        let used = vec!["cli:grok".to_string(), "cli:agy".to_string()];
        assert_eq!(
            resolve_order_rung(SlotRole::Implementer, 0, &cfg, &health, &used),
            Some(("cli:grok".into(), SeatSource::ProvidersOrder)),
            "every usable entry seated collapses onto the first usable one, never None"
        );
    }

    #[test]
    fn singleton_skips_name_only_consulted_entries() {
        let health = StubHealth::refuse_all(&[
            ("cli:claude", "probe failed: not logged in"),
            ("cli:agy", "probe failed: not logged in"),
        ]);
        let cfg = Config::default();
        let skips = health_skips_for_seat(
            SlotRole::TestAuthor,
            2,
            &[],
            PoolOrigin::Synth,
            &cfg,
            &health,
        );
        assert_eq!(skips.len(), 1);
        assert!(skips.iter().any(|(p, _)| p == "cli:agy"));
    }

    #[test]
    fn skips_are_empty_when_nothing_named_a_probe_failure() {
        let health = StubHealth::allow_all();
        let cfg = Config::default();
        assert!(health_skips_for_seat(
            SlotRole::Implementer,
            0,
            &[],
            PoolOrigin::Synth,
            &cfg,
            &health,
        )
        .is_empty());
    }

    #[test]
    fn cli_role_outranks_explicit_pool() {
        let health = StubHealth::allow_all();
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
                &cfg,
                &health,
                &[]
            ),
            Some(("cli:codex".into(), SeatSource::CliRole))
        );
    }

    #[test]
    fn explicit_pool_beats_roles_file() {
        let health = StubHealth::allow_all();
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
                &cfg,
                &health,
                &[]
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

    #[test]
    fn reviewer_rung_skips_a_held_ordinal_entry() {
        let health = StubHealth::allow_all();
        let mut cfg = Config::default();
        cfg.providers.order = vec!["cli:a".into(), "cli:b".into(), "cli:c".into()];
        assert_eq!(
            resolve_order_rung(SlotRole::Reviewer, 1, &cfg, &health, &[]),
            Some(("cli:b".to_string(), SeatSource::ProvidersOrder)),
            "nothing held: ordinal indexing is unchanged"
        );
        assert_eq!(
            resolve_order_rung(
                SlotRole::Reviewer,
                0,
                &cfg,
                &health,
                &["cli:a".to_string()]
            ),
            Some(("cli:b".to_string(), SeatSource::ProvidersOrder)),
            "a held ordinal entry falls forward instead of double-seating"
        );
        assert_eq!(
            resolve_order_rung(
                SlotRole::Reviewer,
                0,
                &cfg,
                &health,
                &[
                    "cli:a".to_string(),
                    "cli:b".to_string(),
                    "cli:c".to_string()
                ]
            ),
            None,
            "no unheld usable entry narrows the panel, never double-seats"
        );
    }

    #[test]
    fn implement_seats_key_quota_against_the_recorded_route() {
        // Bare `cli:codex` bills OpenRouter under the recorded `muse` profile
        // but native under an empty ambient one: pausing the recorded bucket
        // must yield the implementer to its backup, while ambient keeps it.
        let _pin = crate::providers::codex::pin_codex_env();
        std::env::set_var("SPAR_CODEX_PROFILE", "");
        let health = StubHealth::allow_all();
        let mut cfg = Config::default();
        cfg.roles.implementer = Some("cli:codex".into());
        cfg.backups.implementer = Some("cli:claude".into());
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::SparPaths::new(tmp.path());
        let mut store = crate::quota::QuotaStore::default();
        store.pause_quota("cli:codex@openrouter", "test");
        let mut state = crate::state::RunState::new(
            "route-test",
            crate::cli::WorkflowKind::Loop,
            tmp.path().to_path_buf(),
        );
        state.codex_profile = Some("muse".into());
        let seats = |route: crate::providers::RecordedRoute<'_>| {
            build_implement_seats(
                &cfg,
                &[],
                PoolOrigin::Synth,
                false,
                &health,
                &store,
                &paths,
                true,
                route,
                &|_| None,
            )
            .unwrap()
        };
        assert_eq!(
            seats(state.recorded_route())[0].provider,
            "cli:claude",
            "the paused recorded bucket yields to the backup"
        );
        assert_eq!(
            seats(crate::providers::RecordedRoute::ambient())[0].provider,
            "cli:codex",
            "the unpaused ambient bucket keeps the primary"
        );
    }
}
