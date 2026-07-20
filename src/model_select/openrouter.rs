//! Fetch and cache the OpenRouter model catalog (public, unauthenticated).
//!
//! `spar model list --provider openrouter` uses this to list models, filtered by
//! default to those that can actually function as agents — i.e. whose
//! `supported_parameters` include `"tools"`. A model without tool support silently
//! fails as a coding/review agent (it generates text and never calls a tool), so
//! hiding those by default is a guardrail (see DECISIONS MS16).

use crate::registry;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrPricing {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub completion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: OrPricing,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrCatalog {
    pub fetched_at: String,
    pub models: Vec<OrModel>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<OrModel>,
}

/// A model can act as an agent only if it accepts tool calls.
pub fn tool_capable(m: &OrModel) -> bool {
    m.supported_parameters.iter().any(|p| p == "tools")
}

/// Per-token price string → USD per million tokens. Free models return `"0"`, which
/// parses cleanly to `0.0`; an unparseable value yields `None`.
pub fn price_per_million(raw: &str) -> Option<f64> {
    raw.parse::<f64>().ok().map(|p| p * 1_000_000.0)
}

/// Render a per-token price string as `$/M` for a text table.
pub fn fmt_price(raw: &str) -> String {
    match price_per_million(raw) {
        Some(v) => format!("{v:.2}"),
        None => "?".to_string(),
    }
}

fn parse_catalog(body: &str) -> Result<Vec<OrModel>> {
    let parsed: ModelsResponse = serde_json::from_str(body)
        .with_context(|| format!("parse OpenRouter catalog from {MODELS_URL}"))?;
    Ok(parsed.data)
}

pub fn fetch_models() -> Result<Vec<OrModel>> {
    let resp = ureq::get(MODELS_URL)
        .header("User-Agent", "spar-model-select/0.0.1")
        .call()
        .with_context(|| format!("GET {MODELS_URL} (is the network available?)"))?;
    let body = resp
        .into_body()
        .read_to_string()
        .with_context(|| format!("read body {MODELS_URL}"))?;
    let models = parse_catalog(&body)?;
    if models.is_empty() {
        bail!("OpenRouter returned zero models from {MODELS_URL}");
    }
    Ok(models)
}

pub fn cache_path() -> PathBuf {
    registry::spar_home()
        .join("cache")
        .join("openrouter")
        .join("models.json")
}

fn load_cached(path: &Path) -> Result<Option<(OrCatalog, u64)>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read cache {}", path.display()))?;
    let catalog: OrCatalog =
        serde_json::from_str(&text).with_context(|| format!("parse cache {}", path.display()))?;
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(Some((catalog, mtime)))
}

fn save_cached(path: &Path, catalog: &OrCatalog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(catalog)?;
    std::fs::write(path, text).with_context(|| format!("write cache {}", path.display()))?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the catalog, using a fresh cache when present, otherwise fetching. If the fetch
/// fails but a stale cache exists, fall back to it; if there is no cache at all, surface
/// a clean actionable error (never a panic).
pub fn ensure_catalog(ttl_secs: u64) -> Result<OrCatalog> {
    let path = cache_path();
    let cached = load_cached(&path)?;
    if let Some((catalog, mtime)) = &cached {
        if now_secs().saturating_sub(*mtime) <= ttl_secs {
            return Ok(catalog.clone());
        }
    }
    match fetch_models() {
        Ok(models) => {
            let catalog = OrCatalog {
                fetched_at: chrono::Utc::now().to_rfc3339(),
                models,
            };
            save_cached(&path, &catalog)?;
            Ok(catalog)
        }
        Err(e) => match cached {
            Some((catalog, _)) => Ok(catalog),
            None => Err(e.context(
                "no OpenRouter cache and the catalog fetch failed; check your network connection",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed real API response: one tool-capable model, one with no
    // `supported_parameters` key and a null `context_length`, and one non-tool model
    // whose id carries a `:free` suffix (the slug users paste into `cli:codex@…`).
    const FIXTURE: &str = r#"{
  "data": [
    {
      "id": "anthropic/claude-x",
      "name": "Anthropic: Claude X",
      "context_length": 200000,
      "pricing": { "prompt": "0.000003", "completion": "0.000015" },
      "supported_parameters": ["max_tokens", "tools", "temperature"]
    },
    {
      "id": "meta/free-model",
      "name": "Meta: Free Model",
      "context_length": null,
      "pricing": { "prompt": "0", "completion": "0" }
    },
    {
      "id": "tencent/hy3:free",
      "name": "Tencent: HY3",
      "context_length": 32000,
      "pricing": { "prompt": "0", "completion": "0" },
      "supported_parameters": ["temperature", "top_p"]
    }
  ]
}"#;

    #[test]
    fn deserializes_real_response_shape() {
        let models = parse_catalog(FIXTURE).unwrap();
        assert_eq!(models.len(), 3);
        // null context_length → None
        assert_eq!(models[1].context_length, None);
        // absent supported_parameters key → empty via serde default
        assert!(models[1].supported_parameters.is_empty());
        // present context_length preserved
        assert_eq!(models[0].context_length, Some(200000));
    }

    #[test]
    fn tool_capable_true() {
        let models = parse_catalog(FIXTURE).unwrap();
        assert!(tool_capable(&models[0]));
    }

    #[test]
    fn tool_capable_false() {
        let models = parse_catalog(FIXTURE).unwrap();
        // no tools listed
        assert!(!tool_capable(&models[2]));
        // no supported_parameters key at all
        assert!(!tool_capable(&models[1]));
    }

    #[test]
    fn free_tier_pricing_formats() {
        let models = parse_catalog(FIXTURE).unwrap();
        let free = &models[1];
        // "0" must parse, not error
        assert_eq!(price_per_million(&free.pricing.prompt), Some(0.0));
        assert_eq!(price_per_million(&free.pricing.completion), Some(0.0));
        assert_eq!(fmt_price(&free.pricing.prompt), "0.00");
        // a real per-token price scales to per-million
        assert_eq!(price_per_million("0.000003"), Some(3.0));
    }

    #[test]
    fn slug_with_colon_preserved() {
        let models = parse_catalog(FIXTURE).unwrap();
        let id = &models[2].id;
        assert_eq!(id, "tencent/hy3:free");
        // survives a serialize → deserialize round-trip
        let catalog = OrCatalog {
            fetched_at: "2026-07-20T00:00:00Z".into(),
            models: models.clone(),
        };
        let text = serde_json::to_string(&catalog).unwrap();
        let back: OrCatalog = serde_json::from_str(&text).unwrap();
        assert_eq!(back.models[2].id, "tencent/hy3:free");
    }
}
