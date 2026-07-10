//! Dynamic model selection from vals.ai benchmarks + user profiles.

mod cache;
mod map;
mod score;
mod vals;

pub use cache::{cache_age_secs, cache_path, load_cached, save_cached};
pub use map::map_model;
pub use score::{
    apply_urgency, default_profiles, pick_fleet, pick_ranked, ProfileWeights, RankedModel, Urgency,
};
pub use vals::{fetch_bench, BenchSnapshot, ModelScore};
#[cfg(test)]
pub use vals::parse_overall_rsc;

use crate::config::{Config, ModelSelectConfig};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Resolve a fleet of provider refs from explicit list or `--select`.
pub fn resolve_providers(
    providers: &[String],
    select: Option<&[String]>,
    urgency: Urgency,
    n: usize,
    roles: &[&str],
    cfg: &Config,
    dry: bool,
) -> Result<ResolvedProviders> {
    if !providers.is_empty() && select.is_some() {
        bail!("use either --providers or --select, not both");
    }
    if !providers.is_empty() {
        for p in providers {
            crate::provider_ref::ProviderRef::parse(p)?;
        }
        return Ok(ResolvedProviders {
            providers: providers.to_vec(),
            artifact: None,
        });
    }
    let Some(select) = select else {
        bail!(
            "--providers is required (or pass --select <profile>, e.g. --select value)"
        );
    };
    if select.is_empty() {
        bail!("--select requires at least one profile name (or auto)");
    }
    if n == 0 {
        bail!("need at least one slot to select");
    }

    let ms = &cfg.model_select;
    let snap = ensure_bench_data(ms)?;
    let ranked = rank_for_select(select, urgency, roles, n, ms, &snap, dry)?;

    let mut out_providers = Vec::with_capacity(ranked.len());
    let mut chosen = Vec::with_capacity(ranked.len());
    for (i, r) in ranked.iter().enumerate() {
        let mapped = map_usable_allowed(&r.model.id, dry, ms).with_context(|| {
            format!("no usable spar mapping for vals model {}", r.model.id)
        })?;
        out_providers.push(mapped.provider.clone());
        chosen.push(SelectChoice {
            slot: i,
            role: roles.get(i).map(|s| s.to_string()),
            profile: r.profile.clone(),
            vals_id: r.model.id.clone(),
            provider: mapped.provider,
            model: mapped.model,
            score: r.score,
            accuracy: r.model.accuracy,
            latency: r.model.latency,
            cost_per_test: r.model.cost_per_test,
            reason: r.reason.clone(),
        });
    }

    let artifact = SelectArtifact {
        source: "vals".into(),
        bench: snap.bench.clone(),
        fetched_at: snap.fetched_at.clone(),
        stale: snap.stale,
        urgency: urgency.as_str().into(),
        select: select.to_vec(),
        choices: chosen,
    };

    Ok(ResolvedProviders {
        providers: out_providers,
        artifact: Some(artifact),
    })
}

/// Pick a single model for a role (e.g. suite `tester` → fast profile).
pub fn pick_one_for_role(
    role: &str,
    urgency: Urgency,
    cfg: &Config,
    dry: bool,
    exclude_vals: &[String],
) -> Result<SelectChoice> {
    let ms = &cfg.model_select;
    let profile = ms.role_profile(role).to_string();
    let snap = ensure_bench_data(ms)?;
    let profiles = ms.resolved_profiles();
    let weights = profiles
        .get(&profile)
        .cloned()
        .with_context(|| format!("unknown profile '{profile}'"))?;
    let weights = apply_urgency(&weights, urgency);
    let candidates: Vec<ModelScore> = snap
        .models
        .iter()
        .filter(|m| !exclude_vals.iter().any(|e| e == &m.id))
        .filter(|m| map_usable_allowed(&m.id, dry, ms).is_some())
        .cloned()
        .collect();
    let ranked = pick_ranked(&candidates, &weights, ms.min_accuracy_for(&profile));
    let r = ranked
        .first()
        .with_context(|| format!("no models for role={role} profile={profile}"))?;
    let mapped = map_usable_allowed(&r.model.id, dry, ms).context("map failed after rank")?;
    Ok(SelectChoice {
        slot: 0,
        role: Some(role.into()),
        profile,
        vals_id: r.model.id.clone(),
        provider: mapped.provider,
        model: mapped.model,
        score: r.score,
        accuracy: r.model.accuracy,
        latency: r.model.latency,
        cost_per_test: r.model.cost_per_test,
        reason: format!("role={role}; {}", r.reason),
    })
}

pub fn load_select_artifact(
    paths: &crate::paths::SparPaths,
    run_id: &str,
) -> Result<Option<SelectArtifact>> {
    let path = paths.artifact(run_id, "model-select.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn rank_for_select(
    select: &[String],
    urgency: Urgency,
    roles: &[&str],
    n: usize,
    ms: &ModelSelectConfig,
    snap: &BenchSnapshot,
    dry: bool,
) -> Result<Vec<RankedModel>> {
    let profiles = ms.resolved_profiles();
    let candidates: Vec<ModelScore> = snap
        .models
        .iter()
        .filter(|m| map_usable_allowed(&m.id, dry, ms).is_some())
        .cloned()
        .collect();

    if candidates.is_empty() {
        bail!(
            "no usable models after mapping/allow/availability filters (bench={}, allow={:?})",
            snap.bench,
            ms.allow
        );
    }

    let per_slot: Vec<String> = if select.len() == 1 && select[0].eq_ignore_ascii_case("auto") {
        (0..n)
            .map(|i| {
                let role = roles.get(i).copied().unwrap_or("implementer");
                ms.role_profile(role).to_string()
            })
            .collect()
    } else if select.len() == 1 {
        vec![select[0].clone(); n]
    } else {
        if select.len() != n {
            bail!(
                "--select listed {} profiles but need {n} slots (or pass one profile / auto)",
                select.len()
            );
        }
        select.to_vec()
    };

    pick_fleet(&candidates, &per_slot, &profiles, urgency, ms)
}

/// Load bench data from cache (refresh if expired and network ok).
/// Multiple `model_select.benches` → blended average of metrics for shared model ids.
pub fn ensure_bench_data(ms: &ModelSelectConfig) -> Result<BenchSnapshot> {
    let benches = if ms.benches.is_empty() {
        vec!["swebench".to_string()]
    } else {
        ms.benches.clone()
    };
    let ttl = ms.cache_ttl_secs;

    let mut snaps = Vec::new();
    let mut any_stale = false;
    let mut stale_reasons = Vec::new();

    for bench in &benches {
        let path = cache_path(bench);
        let snap = if let Some(meta) = load_cached(&path)? {
            let age = cache_age_secs(&meta);
            if age <= ttl {
                let mut s = meta.snapshot;
                s.stale = false;
                s
            } else {
                match fetch_and_cache(bench, &path) {
                    Ok(s) => s,
                    Err(e) => {
                        any_stale = true;
                        stale_reasons.push(format!("{bench}: refresh failed ({e:#})"));
                        let mut s = meta.snapshot;
                        s.stale = true;
                        s
                    }
                }
            }
        } else {
            fetch_and_cache(bench, &path)?
        };
        snaps.push(snap);
    }

    if snaps.len() == 1 {
        let mut s = snaps.pop().unwrap();
        if any_stale {
            s.stale = true;
            s.stale_reason = Some(stale_reasons.join("; "));
        }
        return Ok(s);
    }

    Ok(blend_snapshots(&snaps, any_stale, &stale_reasons))
}

fn blend_snapshots(
    snaps: &[BenchSnapshot],
    any_stale: bool,
    stale_reasons: &[String],
) -> BenchSnapshot {
    // Average metrics for model ids present in the primary (first) bench.
    let primary = &snaps[0];
    let mut by_id: HashMap<String, Vec<&ModelScore>> = HashMap::new();
    for s in snaps {
        for m in &s.models {
            by_id.entry(m.id.clone()).or_default().push(m);
        }
    }
    let mut models = Vec::new();
    for m in &primary.models {
        let Some(group) = by_id.get(&m.id) else {
            continue;
        };
        let n = group.len() as f64;
        let accuracy = group.iter().map(|x| x.accuracy).sum::<f64>() / n;
        let latency = group.iter().map(|x| x.latency).sum::<f64>() / n;
        let cost_per_test = group.iter().map(|x| x.cost_per_test).sum::<f64>() / n;
        models.push(ModelScore {
            id: m.id.clone(),
            accuracy,
            latency,
            cost_per_test,
            provider_label: m.provider_label.clone(),
        });
    }
    models.sort_by(|a, b| {
        b.accuracy
            .partial_cmp(&a.accuracy)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let benches: Vec<&str> = snaps.iter().map(|s| s.bench.as_str()).collect();
    BenchSnapshot {
        bench: benches.join("+"),
        source: "vals".into(),
        url: primary.url.clone(),
        fetched_at: primary.fetched_at.clone(),
        models,
        stale: any_stale,
        stale_reason: if stale_reasons.is_empty() {
            None
        } else {
            Some(stale_reasons.join("; "))
        },
    }
}

pub fn refresh_bench(bench: &str) -> Result<BenchSnapshot> {
    let path = cache_path(bench);
    fetch_and_cache(bench, &path)
}

/// Refresh every configured bench (or a single override).
pub fn refresh_all(ms: &ModelSelectConfig, bench: Option<&str>) -> Result<Vec<BenchSnapshot>> {
    let benches: Vec<String> = if let Some(b) = bench {
        vec![b.to_string()]
    } else if ms.benches.is_empty() {
        vec!["swebench".into()]
    } else {
        ms.benches.clone()
    };
    let mut out = Vec::new();
    for b in benches {
        out.push(refresh_bench(&b)?);
    }
    Ok(out)
}

fn fetch_and_cache(bench: &str, path: &Path) -> Result<BenchSnapshot> {
    let snap = fetch_bench(bench)?;
    save_cached(path, &snap)?;
    Ok(snap)
}

fn allow_matches(ms: &ModelSelectConfig, provider: &str) -> bool {
    if ms.allow.is_empty() {
        return true;
    }
    ms.allow.iter().any(|pat| {
        if pat == "*" {
            return true;
        }
        if let Some(prefix) = pat.strip_suffix('*') {
            return provider.starts_with(prefix);
        }
        provider == pat
    })
}

/// First mapping that is both usable and allowlisted.
fn map_usable_allowed(
    vals_id: &str,
    dry: bool,
    ms: &ModelSelectConfig,
) -> Option<crate::model_select::map::MappedModel> {
    crate::model_select::map::map_candidates(vals_id)
        .into_iter()
        .find(|m| {
            allow_matches(ms, &m.provider)
                && crate::providers::is_provider_usable(&m.provider, dry)
        })
}

/// Rank models for `spar model list|pick` (no availability filter unless requested).
pub fn list_ranked(
    ms: &ModelSelectConfig,
    profile: &str,
    urgency: Urgency,
    require_usable: bool,
    dry: bool,
) -> Result<(BenchSnapshot, Vec<RankedModel>)> {
    let snap = ensure_bench_data(ms)?;
    let profiles = ms.resolved_profiles();
    let weights = profiles
        .get(profile)
        .cloned()
        .with_context(|| format!("unknown profile '{profile}' (have {:?})", profiles.keys().collect::<Vec<_>>()))?;
    let weights = apply_urgency(&weights, urgency);

    let candidates: Vec<ModelScore> = snap
        .models
        .iter()
        .filter(|m| {
            if require_usable {
                map_usable_allowed(&m.id, dry, ms).is_some()
            } else {
                map_model(&m.id)
                    .map(|mapped| allow_matches(ms, &mapped.provider))
                    .unwrap_or(false)
                    || crate::model_select::map::map_candidates(&m.id)
                        .into_iter()
                        .any(|mapped| allow_matches(ms, &mapped.provider))
            }
        })
        .cloned()
        .collect();

    let ranked = pick_ranked(&candidates, &weights, ms.min_accuracy_for(profile));
    Ok((snap, ranked))
}

pub fn write_select_artifact(paths: &crate::paths::SparPaths, run_id: &str, art: &SelectArtifact) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let path = paths.artifact(run_id, "model-select.json");
    let text = serde_json::to_string_pretty(art)?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ResolvedProviders {
    pub providers: Vec<String>,
    pub artifact: Option<SelectArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectArtifact {
    pub source: String,
    pub bench: String,
    pub fetched_at: String,
    pub stale: bool,
    pub urgency: String,
    pub select: Vec<String>,
    pub choices: Vec<SelectChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectChoice {
    pub slot: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub profile: String,
    pub vals_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub score: f64,
    pub accuracy: f64,
    pub latency: f64,
    pub cost_per_test: f64,
    pub reason: String,
}

/// CLI entry for `spar model …`
pub fn run_cmd(action: crate::cli::ModelAction, cfg: &Config) -> Result<crate::exit_codes::ExitCode> {
    use crate::cli::ModelAction;
    use crate::exit_codes::ExitCode;

    match action {
        ModelAction::List {
            bench,
            profile,
            urgency,
            json,
            usable,
        } => {
            let mut ms = cfg.model_select.clone();
            if let Some(b) = bench {
                ms.benches = vec![b];
            }
            let urgency = Urgency::parse(&urgency)?;
            let (snap, ranked) = list_ranked(&ms, &profile, urgency, usable, false)?;
            if json {
                let out = serde_json::json!({
                    "bench": snap.bench,
                    "fetched_at": snap.fetched_at,
                    "stale": snap.stale,
                    "stale_reason": snap.stale_reason,
                    "profile": profile,
                    "urgency": urgency.as_str(),
                    "models": ranked.iter().map(|r| {
                        let mapped = map_usable_allowed(&r.model.id, false, &ms)
                            .or_else(|| map_model(&r.model.id));
                        serde_json::json!({
                            "id": r.model.id,
                            "accuracy": r.model.accuracy,
                            "latency": r.model.latency,
                            "cost_per_test": r.model.cost_per_test,
                            "score": r.score,
                            "provider": mapped.as_ref().map(|m| &m.provider),
                            "model": mapped.as_ref().and_then(|m| m.model.as_ref()),
                            "reason": r.reason,
                        })
                    }).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                if snap.stale {
                    eprintln!(
                        "warning: cache stale{}",
                        snap.stale_reason
                            .as_ref()
                            .map(|s| format!(" ({s})"))
                            .unwrap_or_default()
                    );
                }
                println!(
                    "bench={} profile={} urgency={} fetched={} ({} models)",
                    snap.bench,
                    profile,
                    urgency.as_str(),
                    snap.fetched_at,
                    ranked.len()
                );
                println!(
                    "{:<36} {:>8} {:>10} {:>10} {:>8}  PROVIDER",
                    "ID", "ACC", "LAT(s)", "COST", "SCORE"
                );
                for r in ranked.iter().take(30) {
                    let mapped = map_usable_allowed(&r.model.id, false, &ms)
                        .or_else(|| map_model(&r.model.id));
                    let prov = mapped
                        .as_ref()
                        .map(|m| m.provider.as_str())
                        .unwrap_or("-");
                    println!(
                        "{:<36} {:>7.1}% {:>10.1} {:>10.4} {:>8.3}  {}",
                        r.model.id,
                        r.model.accuracy,
                        r.model.latency,
                        r.model.cost_per_test,
                        r.score,
                        prov
                    );
                }
            }
            Ok(ExitCode::Success)
        }
        ModelAction::Pick {
            role,
            profile,
            urgency,
            count,
            json,
        } => {
            let ms = &cfg.model_select;
            let urgency = Urgency::parse(&urgency)?;
            let profile = profile.unwrap_or_else(|| ms.role_profile(&role).to_string());
            let (snap, ranked) = list_ranked(ms, &profile, urgency, true, false)?;
            let take = ranked.into_iter().take(count.max(1)).collect::<Vec<_>>();
            if take.is_empty() {
                bail!("no models matched profile '{profile}' with usable providers");
            }
            if json {
                let out = serde_json::json!({
                    "bench": snap.bench,
                    "fetched_at": snap.fetched_at,
                    "stale": snap.stale,
                    "role": role,
                    "profile": profile,
                    "urgency": urgency.as_str(),
                    "picks": take.iter().map(|r| {
                        let mapped = map_usable_allowed(&r.model.id, false, ms)
                            .or_else(|| map_model(&r.model.id))
                            .expect("filtered");
                        serde_json::json!({
                            "vals_id": r.model.id,
                            "provider": mapped.provider,
                            "model": mapped.model,
                            "score": r.score,
                            "accuracy": r.model.accuracy,
                            "latency": r.model.latency,
                            "cost_per_test": r.model.cost_per_test,
                            "reason": r.reason,
                        })
                    }).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for r in &take {
                    let mapped = map_usable_allowed(&r.model.id, false, ms)
                        .or_else(|| map_model(&r.model.id))
                        .expect("filtered");
                    println!(
                        "{}  {}  score={:.3}  acc={:.1}%  cost={:.4}  ({})",
                        mapped.provider,
                        r.model.id,
                        r.score,
                        r.model.accuracy,
                        r.model.cost_per_test,
                        r.reason
                    );
                }
            }
            Ok(ExitCode::Success)
        }
        ModelAction::Refresh { bench, json } => {
            let snaps = refresh_all(&cfg.model_select, bench.as_deref())?;
            if json {
                let out: Vec<_> = snaps
                    .iter()
                    .map(|snap| {
                        serde_json::json!({
                            "bench": snap.bench,
                            "fetched_at": snap.fetched_at,
                            "models": snap.models.len(),
                            "cache": cache_path(&snap.bench).display().to_string(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for snap in &snaps {
                    println!(
                        "refreshed {} ({} models) → {}",
                        snap.bench,
                        snap.models.len(),
                        cache_path(&snap.bench).display()
                    );
                }
            }
            Ok(ExitCode::Success)
        }
        ModelAction::Cache { json } => {
            let bench = cfg
                .model_select
                .benches
                .first()
                .map(|s| s.as_str())
                .unwrap_or("swebench");
            let path = cache_path(bench);
            let meta = load_cached(&path)?;
            if json {
                let out = match &meta {
                    Some(m) => serde_json::json!({
                        "path": path.display().to_string(),
                        "age_secs": cache_age_secs(m),
                        "fetched_at": m.snapshot.fetched_at,
                        "models": m.snapshot.models.len(),
                        "bench": m.snapshot.bench,
                    }),
                    None => serde_json::json!({
                        "path": path.display().to_string(),
                        "present": false,
                    }),
                };
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                match meta {
                    Some(m) => {
                        println!(
                            "cache {} age={}s fetched={} models={}",
                            path.display(),
                            cache_age_secs(&m),
                            m.snapshot.fetched_at,
                            m.snapshot.models.len()
                        );
                    }
                    None => println!("no cache at {}", path.display()),
                }
            }
            Ok(ExitCode::Success)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelSelectConfig;

    fn fixture_snap() -> BenchSnapshot {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vals/swebench_overall.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let models = v["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| ModelScore {
                id: m["id"].as_str().unwrap().into(),
                accuracy: m["accuracy"].as_f64().unwrap(),
                latency: m["latency"].as_f64().unwrap(),
                cost_per_test: m["cost_per_test"].as_f64().unwrap_or(0.0),
                provider_label: m["provider"].as_str().map(|s| s.into()),
            })
            .collect();
        BenchSnapshot {
            bench: "swebench".into(),
            source: "vals".into(),
            url: "https://www.vals.ai/benchmarks/swebench".into(),
            fetched_at: "2026-07-10T00:00:00Z".into(),
            models,
            stale: false,
            stale_reason: None,
        }
    }

    #[test]
    fn value_profile_prefers_lower_cost_among_strong() {
        let snap = fixture_snap();
        let ms = ModelSelectConfig::default();
        let profiles = ms.resolved_profiles();
        let w = apply_urgency(profiles.get("value").unwrap(), Urgency::Normal);
        let ranked = pick_ranked(&snap.models, &w, Some(70.0));
        assert!(!ranked.is_empty());
        // Top should be mappable.
        assert!(map_model(&ranked[0].model.id).is_some());
    }

    #[test]
    fn fleet_diversity_prefers_different_families() {
        let snap = fixture_snap();
        let ms = ModelSelectConfig::default();
        let profiles = ms.resolved_profiles();
        let per = vec!["best".into(), "best".into()];
        // Only mappable models that dry-run accepts.
        let candidates: Vec<_> = snap
            .models
            .iter()
            .filter(|m| map_model(&m.id).is_some())
            .cloned()
            .collect();
        let fleet = pick_fleet(&candidates, &per, &profiles, Urgency::Normal, &ms).unwrap();
        assert_eq!(fleet.len(), 2);
        let p0 = map_model(&fleet[0].model.id).unwrap().provider;
        let p1 = map_model(&fleet[1].model.id).unwrap().provider;
        // Prefer different provider strings when possible.
        if candidates
            .iter()
            .filter_map(|m| map_model(&m.id).map(|x| x.provider))
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1
        {
            assert_ne!(p0, p1, "expected diversity across provider families");
        }
    }

    #[test]
    fn parse_rsc_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vals/swebench_overall_rsc.txt");
        let text = std::fs::read_to_string(path).unwrap();
        let models = parse_overall_rsc(&text).unwrap();
        assert!(models.len() >= 5);
        assert!(models.iter().any(|m| m.id.contains("gpt-5.6-sol")));
    }
}
