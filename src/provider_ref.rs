//! Provider identity: always `cli:name` or `api:name` (no bare names).
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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
        // api providers have no adapter; every model bills separately, so the
        // model is its own bucket and a model-less seat keys at the login.
        if self.backend == ExecBackend::ApiSdk {
            return match &self.model {
                Some(model) => format!("{}@{model}", self.storage_key()),
                None => self.storage_key(),
            };
        }
        // A model-less seat dispatches under the adapter's default route
        // (codex's default profile layers OpenRouter); without one the seat
        // keys at the login (O151).
        let bucket = match self.model.as_deref() {
            Some(model) => crate::providers::adapter_named(&self.storage_key())
                .and_then(|a| a.quota_bucket(model)),
            None => crate::providers::adapter_named(&self.storage_key())
                .and_then(|a| a.default_bucket())
                .map(Cow::Borrowed),
        };
        match bucket {
            Some(bucket) => format!("{}@{bucket}", self.storage_key()),
            None => self.storage_key(),
        }
    }

    /// Where a rejection's pause lives, from the signal resolved against this seat:
    /// the bucket the adapter names, or the login when it names none (no votes, a
    /// login-wide vote, disagreeing votes, or a vote for another seat's family).
    /// No adapter means the login.
    pub fn rejection_key(&self, signal: crate::providers::QuotaSignal<'_>) -> String {
        if self.backend != ExecBackend::NativeCli {
            return self.quota_key();
        }
        match crate::providers::adapter_named(&self.storage_key())
            .and_then(|a| a.rejection_scope(self.model.as_deref(), signal))
        {
            Some(bucket) => format!("{}@{bucket}", self.storage_key()),
            None => self.storage_key(),
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
        assert_eq!(api.quota_key(), "api:openai@gpt-5");
        let api_bare = ProviderRef::parse("api:openai").unwrap();
        assert_eq!(api_bare.quota_key(), "api:openai");
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
        let key = |seat: &str, window: Option<W>, text: &str| {
            ProviderRef::parse(seat)
                .unwrap()
                .rejection_key(QuotaSignal { window, text })
        };
        assert_eq!(
            key("cli:claude@opus", Some(W::SevenDayOpus), ""),
            "cli:claude@opus"
        );
        assert_eq!(
            key("cli:claude@opus", Some(W::SevenDaySonnet), ""),
            "cli:claude",
            "another family's window widens to the login"
        );
        assert_eq!(key("cli:claude@opus", Some(W::FiveHour), ""), "cli:claude");
        assert_eq!(key("cli:claude@opus", Some(W::SevenDay), ""), "cli:claude");
        assert_eq!(
            key("cli:claude@opus", Some(W::SevenDayOverageIncluded), ""),
            "cli:claude@opus"
        );
        assert_eq!(
            key("cli:claude@haiku", Some(W::SevenDayOverageIncluded), ""),
            "cli:claude",
            "haiku has no bucket"
        );
        assert_eq!(
            key("cli:claude", Some(W::SevenDayOpus), ""),
            "cli:claude",
            "a bare seat has no family bucket to land on"
        );
        assert_eq!(
            key("cli:claude@opusplan", Some(W::SevenDayOpus), ""),
            "cli:claude",
            "an unrecognized model keys the login"
        );
        assert_eq!(
            key(
                "cli:claude@fable",
                Some(W::SevenDayOverageIncluded),
                "You've reached your Fable limit"
            ),
            "cli:claude@fable",
            "an overage window with matching family prose pauses that family"
        );
        assert_eq!(
            key(
                "cli:claude@opus",
                Some(W::SevenDayOverageIncluded),
                "You've reached your Fable limit"
            ),
            "cli:claude",
            "an overage window with another family's prose widens to the login"
        );
        assert_eq!(
            key("cli:claude@opus", None, "You've reached your Fable limit"),
            "cli:claude",
            "prose naming another family widens to the login"
        );
        assert_eq!(
            key("cli:claude@fable", None, "You've reached your Fable limit"),
            "cli:claude@fable"
        );
        assert_eq!(
            key("cli:claude@opus", None, "You've hit your weekly limit"),
            "cli:claude"
        );
        assert_eq!(
            key("cli:claude@sonnet", None, ""),
            "cli:claude",
            "text with no rejection signal widens to the login"
        );
        let agy = ProviderRef::parse("cli:agy@flash").unwrap();
        assert_eq!(
            agy.rejection_key(QuotaSignal {
                window: Some(W::FiveHour),
                text: ""
            }),
            "cli:agy@flash",
            "agy always writes the seat"
        );
        let codex = ProviderRef::parse("cli:codex@gpt-5").unwrap();
        assert_eq!(
            codex.rejection_key(QuotaSignal {
                window: Some(W::SevenDayOpus),
                text: ""
            }),
            "cli:codex"
        );
    }

    #[test]
    fn quota_key_splits_codex_by_route() {
        let native = ProviderRef::parse("cli:codex@gpt-5.2-codex").unwrap();
        assert_eq!(native.quota_key(), "cli:codex");
        let via_openrouter = ProviderRef::parse("cli:codex@openai/gpt-5").unwrap();
        assert_eq!(via_openrouter.quota_key(), "cli:codex@openrouter");
        let bare = ProviderRef::parse("cli:codex").unwrap();
        assert_eq!(
            bare.quota_key(),
            "cli:codex@openrouter",
            "a model-less seat dispatches under the default profile, which layers OpenRouter"
        );
    }

    #[test]
    fn quota_key_splits_pi_by_upstream() {
        let slug = ProviderRef::parse("cli:pi@google/gemini-2.5-flash").unwrap();
        assert_eq!(slug.quota_key(), "cli:pi@openrouter");
        let explicit = ProviderRef::parse("cli:pi@openrouter/anthropic/claude").unwrap();
        assert_eq!(explicit.quota_key(), "cli:pi@openrouter");
        let direct = ProviderRef::parse("cli:pi@direct:anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(direct.quota_key(), "cli:pi@anthropic");
        let bare_word = ProviderRef::parse("cli:pi@gemini-2.5-flash").unwrap();
        assert_eq!(
            bare_word.quota_key(),
            "cli:pi",
            "pi's own resolution keys the login"
        );
        let bare = ProviderRef::parse("cli:pi").unwrap();
        assert_eq!(bare.quota_key(), "cli:pi");
    }

    #[test]
    fn quota_key_splits_api_per_model() {
        let modeled = ProviderRef::parse("api:anthropic@claude-opus-4-6").unwrap();
        assert_eq!(modeled.quota_key(), "api:anthropic@claude-opus-4-6");
        let bare = ProviderRef::parse("api:anthropic").unwrap();
        assert_eq!(bare.quota_key(), "api:anthropic");
    }

    #[test]
    fn quota_key_stays_login_wide_for_grok_and_muse() {
        for seat in [
            "cli:grok",
            "cli:grok@grok-4-1-fast",
            "cli:muse",
            "cli:muse@auto",
        ] {
            assert_eq!(
                ProviderRef::parse(seat).unwrap().quota_key(),
                seat.split('@').next().unwrap(),
                "{seat} has no separate windows"
            );
        }
    }

    #[test]
    fn rejection_key_lands_in_the_seat_bucket() {
        use crate::providers::{QuotaSignal, RateLimitWindow as W};
        let key = |seat: &str, window: Option<W>, text: &str| {
            ProviderRef::parse(seat)
                .unwrap()
                .rejection_key(QuotaSignal { window, text })
        };
        assert_eq!(
            key("cli:codex@openai/gpt-5", Some(W::RateLimit), "rate limit"),
            "cli:codex@openrouter"
        );
        assert_eq!(
            key("cli:codex@gpt-5.2-codex", None, "rate limited, slow down"),
            "cli:codex"
        );
        assert_eq!(
            key("cli:codex", None, "too many requests"),
            "cli:codex@openrouter",
            "a model-less rejection lands in the default route's bucket"
        );
        assert_eq!(
            key(
                "cli:pi@direct:anthropic/claude-sonnet-4-5",
                None,
                "error 429"
            ),
            "cli:pi@anthropic"
        );
        assert_eq!(
            key("cli:pi@google/gemini-2.5-flash", None, "rate limit"),
            "cli:pi@openrouter"
        );
        assert_eq!(key("cli:pi@gemini-2.5-flash", None, "rate limit"), "cli:pi");
        assert_eq!(
            key("api:openai@gpt-5", None, "too many requests"),
            "api:openai@gpt-5"
        );
        assert_eq!(key("api:openai", None, "too many requests"), "api:openai");
        assert_eq!(
            key("cli:grok@grok-4-1-fast", None, "rate limit"),
            "cli:grok"
        );
        assert_eq!(key("cli:muse@auto", None, "rate limit"), "cli:muse");
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
