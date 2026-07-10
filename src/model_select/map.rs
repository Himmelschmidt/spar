//! Map vals model ids → spar provider refs (+ optional model string).

use crate::providers;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct MappedModel {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Candidate mappings in preference order (first usable wins).
pub fn map_candidates(vals_id: &str) -> Vec<MappedModel> {
    let Some((lab, model)) = vals_id.split_once('/') else {
        return Vec::new();
    };
    let model = model.trim();
    if model.is_empty() {
        return Vec::new();
    }
    let model_s = normalize_model(model);

    match lab {
        "anthropic" => vec![
            MappedModel {
                provider: "cli:claude".into(),
                model: Some(model_s.clone()),
            },
            MappedModel {
                provider: "api:anthropic".into(),
                model: Some(model_s),
            },
        ],
        "openai" => vec![MappedModel {
            provider: "api:openai".into(),
            model: Some(model_s),
        }],
        "xai" => vec![
            MappedModel {
                provider: "cli:grok".into(),
                model: Some(model_s.clone()),
            },
            MappedModel {
                provider: "api:xai".into(),
                model: Some(model_s),
            },
        ],
        "google" => vec![MappedModel {
            provider: "api:google".into(),
            model: Some(model_s),
        }],
        "meta" => vec![MappedModel {
            provider: "api:meta".into(),
            model: Some(model_s),
        }],
        // No first-class spar adapter yet.
        "cursor" | "poolside" | "zai" | "deepseek" | "moonshot" | "mistral" | "minimax"
        | "alibaba" | "cohere" | "nvidia" | "xiaomi" => Vec::new(),
        _ => Vec::new(),
    }
}

/// First mapping candidate (ignores availability). Used for cost scoring (CLI = 0).
pub fn map_model(vals_id: &str) -> Option<MappedModel> {
    map_candidates(vals_id).into_iter().next()
}

/// First *usable* mapping for this environment (dry-run treats known providers as usable).
#[allow(dead_code)]
pub fn map_model_usable(vals_id: &str, dry: bool) -> Option<MappedModel> {
    map_candidates(vals_id)
        .into_iter()
        .find(|m| providers::is_provider_usable(&m.provider, dry))
}

/// Family key for diversity (lab name).
pub fn provider_family(vals_id: &str) -> String {
    vals_id
        .split_once('/')
        .map(|(lab, _)| lab.to_string())
        .unwrap_or_else(|| vals_id.to_string())
}

fn normalize_model(m: &str) -> String {
    // vals slugs → spawn ids (best-effort; API/CLI may still reject unknown names)
    m.replace(' ', "-").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_labs() {
        let m = map_model("anthropic/claude-opus-4-8").unwrap();
        assert_eq!(m.provider, "cli:claude");
        let m = map_model("openai/gpt-5.6-sol").unwrap();
        assert_eq!(m.provider, "api:openai");
        let m = map_model("xai/grok-4.5").unwrap();
        assert_eq!(m.provider, "cli:grok");
        assert!(map_model("zai/glm-5.2").is_none());
        assert_eq!(map_candidates("xai/grok-4").len(), 2);
    }
}
