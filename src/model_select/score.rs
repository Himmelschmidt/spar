//! Profile weights, urgency, and ranking.

use crate::config::ModelSelectConfig;
use crate::model_select::map::{map_model, provider_family};
use crate::model_select::vals::ModelScore;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    High,
    Critical,
}

impl Urgency {
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "normal" | "med" | "medium" | "" => Ok(Self::Normal),
            "high" => Ok(Self::High),
            "critical" | "urgent" => Ok(Self::Critical),
            other => bail!("unknown urgency '{other}' (low|normal|high|critical)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileWeights {
    pub quality: f64,
    pub cost: f64,
    pub speed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_accuracy: Option<f64>,
}

impl ProfileWeights {
    pub fn best() -> Self {
        Self {
            quality: 1.0,
            cost: 0.1,
            speed: 0.2,
            min_accuracy: None,
        }
    }

    pub fn value() -> Self {
        Self {
            quality: 0.6,
            cost: 0.8,
            speed: 0.3,
            min_accuracy: Some(70.0),
        }
    }

    pub fn fast() -> Self {
        Self {
            quality: 0.4,
            cost: 0.3,
            speed: 1.0,
            min_accuracy: Some(50.0),
        }
    }
}

pub fn default_profiles() -> HashMap<String, ProfileWeights> {
    let mut m = HashMap::new();
    m.insert("best".into(), ProfileWeights::best());
    m.insert("value".into(), ProfileWeights::value());
    m.insert("fast".into(), ProfileWeights::fast());
    m
}

/// Remap weights for urgency (MS5).
pub fn apply_urgency(w: &ProfileWeights, urgency: Urgency) -> ProfileWeights {
    let mut out = w.clone();
    match urgency {
        Urgency::Low => {
            out.cost *= 1.4;
            out.quality *= 0.9;
            out.speed *= 0.7;
        }
        Urgency::Normal => {}
        Urgency::High => {
            out.speed *= 1.5;
            out.cost *= 0.6;
            out.quality *= 1.05;
        }
        Urgency::Critical => {
            out.speed *= 2.0;
            out.quality *= 1.15;
            out.cost *= 0.25;
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct RankedModel {
    pub model: ModelScore,
    pub score: f64,
    pub profile: String,
    pub reason: String,
}

pub fn pick_ranked(
    candidates: &[ModelScore],
    weights: &ProfileWeights,
    min_accuracy: Option<f64>,
) -> Vec<RankedModel> {
    let min_acc = min_accuracy.or(weights.min_accuracy).unwrap_or(0.0);
    let filtered: Vec<&ModelScore> = candidates
        .iter()
        .filter(|m| m.accuracy >= min_acc)
        .collect();
    if filtered.is_empty() {
        return Vec::new();
    }

    let (acc_min, acc_max) = min_max(filtered.iter().map(|m| m.accuracy));
    let (lat_min, lat_max) = min_max(filtered.iter().map(|m| m.latency));
    let costs: Vec<f64> = filtered
        .iter()
        .map(|m| effective_cost(m))
        .collect();
    let (cost_min, cost_max) = min_max(costs.iter().copied());

    let mut ranked: Vec<RankedModel> = filtered
        .iter()
        .map(|m| {
            let acc_n = norm(m.accuracy, acc_min, acc_max);
            let lat_n = norm(m.latency, lat_min, lat_max);
            let cost_n = norm(effective_cost(m), cost_min, cost_max);
            // Higher accuracy better; lower cost/latency better.
            let score = weights.quality * acc_n - weights.cost * cost_n - weights.speed * lat_n;
            RankedModel {
                model: (*m).clone(),
                score,
                profile: String::new(),
                reason: format!(
                    "acc={:.1} lat={:.0}s cost={:.3} (norm q={:.2} c={:.2} s={:.2})",
                    m.accuracy,
                    m.latency,
                    effective_cost(m),
                    acc_n,
                    cost_n,
                    lat_n
                ),
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

/// CLI cost = 0 (MS7).
fn effective_cost(m: &ModelScore) -> f64 {
    if let Some(mapped) = map_model(&m.id) {
        if mapped.provider.starts_with("cli:") {
            return 0.0;
        }
    }
    m.cost_per_test
}

fn min_max(iter: impl Iterator<Item = f64>) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for v in iter {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    if !min.is_finite() {
        (0.0, 1.0)
    } else {
        (min, max)
    }
}

fn norm(v: f64, min: f64, max: f64) -> f64 {
    if (max - min).abs() < 1e-12 {
        return 0.5;
    }
    ((v - min) / (max - min)).clamp(0.0, 1.0)
}

/// Pick one model per slot profile with provider-family diversity (MS9).
pub fn pick_fleet(
    candidates: &[ModelScore],
    per_slot_profiles: &[String],
    profiles: &HashMap<String, ProfileWeights>,
    urgency: Urgency,
    ms: &ModelSelectConfig,
) -> Result<Vec<RankedModel>> {
    let mut used_ids: HashSet<String> = HashSet::new();
    let mut used_families: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(per_slot_profiles.len());

    for (slot, pname) in per_slot_profiles.iter().enumerate() {
        let base = profiles.get(pname).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown profile '{pname}' (have {:?})",
                profiles.keys().collect::<Vec<_>>()
            )
        })?;
        let weights = apply_urgency(base, urgency);
        let min_acc = ms.min_accuracy_for(pname).or(weights.min_accuracy);
        let ranked = pick_ranked(candidates, &weights, min_acc);
        if ranked.is_empty() {
            bail!("no candidates for profile '{pname}' (slot {slot})");
        }

        // Prefer unused family, then unused model id.
        let pick = ranked
            .iter()
            .find(|r| {
                !used_ids.contains(&r.model.id)
                    && !used_families.contains(&provider_family(&r.model.id))
            })
            .or_else(|| ranked.iter().find(|r| !used_ids.contains(&r.model.id)))
            .or_else(|| ranked.first())
            .cloned()
            .unwrap();

        used_ids.insert(pick.model.id.clone());
        used_families.insert(provider_family(&pick.model.id));
        out.push(RankedModel {
            profile: pname.clone(),
            reason: format!("slot={slot} profile={pname}; {}", pick.reason),
            ..pick
        });
    }
    Ok(out)
}
