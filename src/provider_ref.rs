//! Provider identity: always `cli:name` or `api:name` (no bare names).
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecBackend {
    #[default]
    NativeCli,
    ApiSdk,
}

impl ExecBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecBackend::NativeCli => "native-cli",
            ExecBackend::ApiSdk => "api-sdk",
        }
    }
}

impl std::fmt::Display for ExecBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRef {
    pub backend: ExecBackend,
    /// Provider id within backend: claude|grok|agy or openai|anthropic|xai|google
    pub name: String,
    /// Optional model, split off the ref on the first `@` (e.g. `cli:codex@openai/gpt-4o-mini`).
    /// May contain `:` and `/` (OpenRouter slugs); the adapter `name` may not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ProviderRef {
    /// Parse a provider ref. **Requires** `cli:` or `api:` prefix.
    ///
    /// An optional `@model` suffix is split on the **first** `@`, before the
    /// colon-in-name check — the model may carry `:` and `/` (OpenRouter slugs)
    /// while the adapter name may not.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty provider name");
        }
        if let Some(rest) = raw.strip_prefix("api:") {
            let rest = rest.trim();
            if rest.is_empty() {
                bail!("api: requires a provider name (e.g. api:openai)");
            }
            let (name, model) = split_model(raw, rest)?;
            if name.contains(':') {
                bail!("invalid provider '{raw}' (use api:name)");
            }
            return Ok(Self {
                backend: ExecBackend::ApiSdk,
                name,
                model,
            });
        }
        if let Some(rest) = raw.strip_prefix("cli:") {
            let rest = rest.trim();
            if rest.is_empty() {
                bail!("cli: requires a provider name (e.g. cli:claude)");
            }
            let (name, model) = split_model(raw, rest)?;
            if name.contains(':') {
                bail!("invalid provider '{raw}' (use cli:name)");
            }
            return Ok(Self {
                backend: ExecBackend::NativeCli,
                name,
                model,
            });
        }
        bail!("provider '{raw}' must be 'cli:…' or 'api:…' (e.g. cli:claude, api:openai)");
    }

    /// Human-readable / round-trippable form — carries `@model` when present.
    pub fn display(&self) -> String {
        let base = self.storage_key();
        match &self.model {
            Some(m) => format!("{base}@{m}"),
            None => base,
        }
    }

    /// Key used in slot.provider, quota buckets, and adapter lookup. Always
    /// model-free: `cli:claude@opus` and `cli:claude@haiku` share one bucket.
    pub fn storage_key(&self) -> String {
        match self.backend {
            ExecBackend::NativeCli => format!("cli:{}", self.name),
            ExecBackend::ApiSdk => format!("api:{}", self.name),
        }
    }

    pub fn cli_name(&self) -> Option<&str> {
        match self.backend {
            ExecBackend::NativeCli => Some(self.name.as_str()),
            ExecBackend::ApiSdk => None,
        }
    }

    pub fn is_api(&self) -> bool {
        self.backend == ExecBackend::ApiSdk
    }
}

/// Split `name@model` on the first `@`. The model may contain `:` and `/`.
fn split_model(raw: &str, rest: &str) -> Result<(String, Option<String>)> {
    let Some((name, model)) = rest.split_once('@') else {
        return Ok((rest.to_string(), None));
    };
    let name = name.trim();
    let model = model.trim();
    if name.is_empty() {
        bail!("invalid provider '{raw}' (missing provider name before '@')");
    }
    if model.is_empty() {
        bail!("invalid provider '{raw}' (model after '@' is empty)");
    }
    Ok((name.to_string(), Some(model.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_prefix() {
        assert!(ProviderRef::parse("claude").is_err());
        assert!(ProviderRef::parse("openai").is_err());
        assert!(ProviderRef::parse("xai").is_err());
        let g = ProviderRef::parse("cli:grok").unwrap();
        assert_eq!(g.name, "grok");
        assert_eq!(g.backend, ExecBackend::NativeCli);
        assert_eq!(g.storage_key(), "cli:grok");
        assert_eq!(g.model, None);
        assert!(ProviderRef::parse("api:openai").unwrap().is_api());
    }

    #[test]
    fn parses_openrouter_slug_with_slash_and_colon() {
        let r = ProviderRef::parse("cli:codex@tencent/hy3:free").unwrap();
        assert_eq!(r.name, "codex");
        assert_eq!(r.model.as_deref(), Some("tencent/hy3:free"));
    }

    #[test]
    fn storage_key_drops_model() {
        let r = ProviderRef::parse("cli:codex@tencent/hy3:free").unwrap();
        assert_eq!(r.storage_key(), "cli:codex");
    }

    #[test]
    fn display_round_trips() {
        let r = ProviderRef::parse("cli:codex@openai/gpt-4o-mini").unwrap();
        assert_eq!(r.display(), "cli:codex@openai/gpt-4o-mini");
        assert_eq!(ProviderRef::parse(&r.display()).unwrap(), r);
    }

    #[test]
    fn splits_on_first_at() {
        let r = ProviderRef::parse("cli:claude@a@b").unwrap();
        assert_eq!(r.name, "claude");
        assert_eq!(r.model.as_deref(), Some("a@b"));
    }

    #[test]
    fn bare_ref_has_no_model() {
        assert_eq!(ProviderRef::parse("cli:claude").unwrap().model, None);
    }

    #[test]
    fn empty_model_errors() {
        assert!(ProviderRef::parse("cli:codex@").is_err());
    }

    #[test]
    fn colon_in_name_still_rejected() {
        assert!(ProviderRef::parse("cli:foo:bar").is_err());
    }

    #[test]
    fn api_backend_carries_model() {
        let r = ProviderRef::parse("api:openai@gpt-5").unwrap();
        assert_eq!(r.backend, ExecBackend::ApiSdk);
        assert_eq!(r.name, "openai");
        assert_eq!(r.model.as_deref(), Some("gpt-5"));
    }
}
