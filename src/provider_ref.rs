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

    /// Key used in slot.provider and adapter lookup. Always model-free.
    pub fn storage_key(&self) -> String {
        match self.backend {
            ExecBackend::NativeCli => format!("cli:{}", self.name),
            ExecBackend::ApiSdk => format!("api:{}", self.name),
        }
    }

    /// Where this seat's quota pause lives. A model whose adapter declares a bucket
    /// keys there (`cli:agy@flash`, `cli:claude@opus` for `claude-opus-5-5` too);
    /// everything else keys per login, exactly `storage_key()`. The scope is declared
    /// on the adapter type. Idempotent on its own output.
    pub fn quota_key(&self) -> String {
        if self.backend != ExecBackend::NativeCli {
            return self.storage_key();
        }
        let Some(model) = self.model.as_deref() else {
            return self.storage_key();
        };
        match crate::providers::adapter_named(&self.storage_key())
            .and_then(|a| a.quota_bucket(model))
        {
            Some(bucket) => format!("{}@{bucket}", self.storage_key()),
            None => self.storage_key(),
        }
    }

    /// Where a rejection's pause lives, from the signal, not just the seat: a
    /// login-wide window lands on the login even when the seat has a bucket, a named
    /// bucket lands there whichever seat reported it, and the seat's own bucket
    /// resolves through [`quota_key`](Self::quota_key). No adapter means the login.
    pub fn rejection_key(&self, signal: crate::providers::QuotaSignal<'_>) -> String {
        if self.backend != ExecBackend::NativeCli {
            return self.quota_key();
        }
        let Some(adapter) = crate::providers::adapter_named(&self.storage_key()) else {
            return self.quota_key();
        };
        match adapter.rejection_scope(self.model.as_deref(), signal) {
            crate::providers::QuotaScope::Login => self.storage_key(),
            crate::providers::QuotaScope::Seat => self.quota_key(),
            crate::providers::QuotaScope::Bucket(bucket) => {
                format!("{}@{bucket}", self.storage_key())
            }
        }
    }

    /// The ref a seat dispatches as. `provider` is the stored form (usually
    /// model-free) and `model` the seat's recorded model; a `@model` already on
    /// the ref wins over the recorded one. Writers and gates resolve through
    /// this one function so both sides key the same seat identically.
    pub fn for_seat(provider: &str, model: Option<&str>) -> Self {
        let model = model.filter(|m| !m.is_empty()).map(str::to_string);
        match Self::parse(provider) {
            Ok(mut pref) => {
                if pref.model.is_none() {
                    pref.model = model;
                }
                pref
            }
            Err(_) => Self {
                backend: ExecBackend::NativeCli,
                name: provider.to_string(),
                model,
            },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub provider: String,
    pub model: Option<String>,
}

impl Pin {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty provider");
        }
        let pref = ProviderRef::parse(raw)?;
        Ok(Self {
            provider: pref.storage_key(),
            model: pref.model,
        })
    }

    pub fn display(&self) -> String {
        match &self.model {
            Some(m) => format!("{}@{}", self.provider, m),
            None => self.provider.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_parses_and_roundtrips() {
        let p = Pin::parse("cli:claude@opus").unwrap();
        assert_eq!(p.provider, "cli:claude");
        assert_eq!(p.model.as_deref(), Some("opus"));
        assert_eq!(p.display(), "cli:claude@opus");
        let q = Pin::parse(&p.display()).unwrap();
        assert_eq!(p, q);
    }

    #[test]
    fn pin_rejects_invalid_provider() {
        assert!(Pin::parse("invalid").is_err());
        assert!(Pin::parse("cli:claude@").is_err());
    }

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
    fn quota_key_is_the_adapters_bucket() {
        let agy = ProviderRef::parse("cli:agy@gemini-3-flash").unwrap();
        assert_eq!(agy.quota_key(), "cli:agy@gemini-3-flash");
        let bare = ProviderRef::parse("cli:agy").unwrap();
        assert_eq!(bare.quota_key(), "cli:agy");
        let opus = ProviderRef::parse("cli:claude@opus").unwrap();
        assert_eq!(opus.quota_key(), "cli:claude@opus");
        let full = ProviderRef::parse("cli:claude@claude-opus-5-5").unwrap();
        assert_eq!(full.quota_key(), "cli:claude@opus");
        let haiku = ProviderRef::parse("cli:claude@haiku").unwrap();
        assert_eq!(haiku.quota_key(), "cli:claude");
        let full_haiku = ProviderRef::parse("cli:claude@claude-haiku-5-5").unwrap();
        assert_eq!(full_haiku.quota_key(), "cli:claude");
        let api = ProviderRef::parse("api:openai@gpt-5").unwrap();
        assert_eq!(api.quota_key(), "api:openai");
        let unknown = ProviderRef::parse("cli:nothing@x").unwrap();
        assert_eq!(unknown.quota_key(), "cli:nothing");
        assert_eq!(
            ProviderRef::parse(&opus.quota_key()).unwrap().quota_key(),
            "cli:claude@opus"
        );
    }

    #[test]
    fn rejection_key_follows_the_signal_for_claude() {
        use crate::providers::{QuotaSignal, RateLimitWindow as W};
        let opus = ProviderRef::parse("cli:claude@opus").unwrap();
        let window = |w: &W| opus.rejection_key(QuotaSignal::Window(w));
        assert_eq!(window(&W::SevenDayOpus), "cli:claude@opus");
        assert_eq!(window(&W::SevenDaySonnet), "cli:claude@sonnet");
        assert_eq!(window(&W::FiveHour), "cli:claude");
        assert_eq!(window(&W::SevenDay), "cli:claude");
        assert_eq!(window(&W::SevenDayOverageIncluded), "cli:claude@opus");
        let haiku = ProviderRef::parse("cli:claude@haiku").unwrap();
        assert_eq!(
            haiku.rejection_key(QuotaSignal::Window(&W::SevenDayOverageIncluded)),
            "cli:claude"
        );
        let sonnet = ProviderRef::parse("cli:claude@sonnet").unwrap();
        assert_eq!(
            sonnet.rejection_key(QuotaSignal::Window(&W::SevenDayOpus)),
            "cli:claude@opus",
            "a named bucket lands there whichever seat reported it"
        );
        assert_eq!(
            opus.rejection_key(QuotaSignal::Text("You've reached your Fable limit")),
            "cli:claude@fable"
        );
        assert_eq!(
            opus.rejection_key(QuotaSignal::Text("You've hit your weekly limit")),
            "cli:claude"
        );
        assert_eq!(
            opus.rejection_key(QuotaSignal::WindowAndText(
                &W::SevenDayOpus,
                "You've hit your limit"
            )),
            "cli:claude",
            "a generic line next to a family window widens to the login"
        );
        assert_eq!(
            opus.rejection_key(QuotaSignal::WindowAndText(&W::SevenDayOpus, "")),
            "cli:claude@opus",
            "prose-free output leaves the typed bucket standing"
        );
        let fable = ProviderRef::parse("cli:claude@fable").unwrap();
        assert_eq!(
            fable.rejection_key(QuotaSignal::WindowAndText(
                &W::SevenDayOverageIncluded,
                "You've reached your Fable limit"
            )),
            "cli:claude@fable",
            "an overage window with matching family prose pauses that family"
        );
        assert_eq!(
            opus.rejection_key(QuotaSignal::WindowAndText(
                &W::SevenDayOverageIncluded,
                "You've reached your Fable limit"
            )),
            "cli:claude",
            "an overage window with another family's prose widens to the login"
        );
        let agy = ProviderRef::parse("cli:agy@flash").unwrap();
        assert_eq!(
            agy.rejection_key(QuotaSignal::Window(&W::FiveHour)),
            "cli:agy@flash",
            "agy always writes the seat"
        );
        let codex = ProviderRef::parse("cli:codex@gpt-5").unwrap();
        assert_eq!(
            codex.rejection_key(QuotaSignal::Window(&W::SevenDayOpus)),
            "cli:codex"
        );
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
