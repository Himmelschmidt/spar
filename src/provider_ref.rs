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

    /// Bucket for a model-less seat under its recorded dispatch route: the
    /// route's bucket, or the login when the route names no verified bucket.
    fn recorded_route_bucket(
        &self,
        route: crate::providers::RecordedRoute<'_>,
    ) -> crate::providers::RecordedRouteBucket {
        if self.model.is_none() {
            if let Some(adapter) = crate::providers::adapter_named(&self.storage_key()) {
                return adapter.recorded_route_bucket(route);
            }
        }
        crate::providers::RecordedRouteBucket::Ambient
    }

    /// Where this seat's quota pause lives. A model whose adapter declares a bucket
    /// keys there (`cli:agy@flash`, `cli:claude@opus` for `claude-opus-5-5` too);
    /// everything else keys per login, exactly `storage_key()`. The scope is declared
    /// on the adapter type. Idempotent on its own output.
    ///
    /// `route` is the dispatch route recorded in run state (only the codex
    /// adapter interprets it today). A model-less seat on a recorded route keys
    /// under that route, not under whatever the keying process's environment
    /// holds; `RecordedRoute::ambient()` keys from this process's environment
    /// (operator tools, history readers). Every other ref ignores the route.
    pub fn quota_key(&self, route: crate::providers::RecordedRoute<'_>) -> String {
        // api providers have no adapter; every model bills separately, so the
        // model is its own bucket and a model-less seat keys at the login.
        if self.backend == ExecBackend::ApiSdk {
            return match &self.model {
                Some(model) => format!("{}@{model}", self.storage_key()),
                None => self.storage_key(),
            };
        }
        // A model-less seat dispatches under the adapter's default route; without
        // one the seat keys at the login (O151). A recorded route overrides the
        // ambient one (`Some("")` records a set-but-empty env value).
        match self.recorded_route_bucket(route) {
            crate::providers::RecordedRouteBucket::Bucket(bucket) => {
                return format!("{}@{bucket}", self.storage_key());
            }
            crate::providers::RecordedRouteBucket::Login => return self.storage_key(),
            crate::providers::RecordedRouteBucket::Ambient => {}
        }
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
    /// No adapter means the login. See `quota_key` for what `route` means.
    pub fn rejection_key(
        &self,
        signal: crate::providers::QuotaSignal<'_>,
        route: crate::providers::RecordedRoute<'_>,
    ) -> String {
        match self.recorded_route_bucket(route) {
            crate::providers::RecordedRouteBucket::Bucket(bucket) => {
                return format!("{}@{bucket}", self.storage_key());
            }
            crate::providers::RecordedRouteBucket::Login => return self.storage_key(),
            crate::providers::RecordedRouteBucket::Ambient => {}
        }
        if self.backend != ExecBackend::NativeCli {
            return self.quota_key(route);
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
    use crate::providers::RecordedRoute;

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
        assert_eq!(
            agy.quota_key(RecordedRoute::ambient()),
            "cli:agy@gemini-3-flash"
        );
        let bare = ProviderRef::parse("cli:agy").unwrap();
        assert_eq!(bare.quota_key(RecordedRoute::ambient()), "cli:agy");
        let opus = ProviderRef::parse("cli:claude@opus").unwrap();
        assert_eq!(opus.quota_key(RecordedRoute::ambient()), "cli:claude@opus");
        let full = ProviderRef::parse("cli:claude@claude-opus-5-5").unwrap();
        assert_eq!(full.quota_key(RecordedRoute::ambient()), "cli:claude@opus");
        let haiku = ProviderRef::parse("cli:claude@haiku").unwrap();
        assert_eq!(haiku.quota_key(RecordedRoute::ambient()), "cli:claude");
        let full_haiku = ProviderRef::parse("cli:claude@claude-haiku-5-5").unwrap();
        assert_eq!(full_haiku.quota_key(RecordedRoute::ambient()), "cli:claude");
        let api = ProviderRef::parse("api:openai@gpt-5").unwrap();
        assert_eq!(api.quota_key(RecordedRoute::ambient()), "api:openai@gpt-5");
        let api_bare = ProviderRef::parse("api:openai").unwrap();
        assert_eq!(api_bare.quota_key(RecordedRoute::ambient()), "api:openai");
        let unknown = ProviderRef::parse("cli:nothing@x").unwrap();
        assert_eq!(unknown.quota_key(RecordedRoute::ambient()), "cli:nothing");
        assert_eq!(
            ProviderRef::parse(&opus.quota_key(RecordedRoute::ambient()))
                .unwrap()
                .quota_key(RecordedRoute::ambient()),
            "cli:claude@opus"
        );
    }

    #[test]
    fn rejection_key_follows_the_signal_for_claude() {
        use crate::providers::{QuotaSignal, RateLimitWindow as W};
        let key = |seat: &str, window: Option<W>, text: &str| {
            ProviderRef::parse(seat)
                .unwrap()
                .rejection_key(QuotaSignal { window, text }, RecordedRoute::ambient())
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
            agy.rejection_key(
                QuotaSignal {
                    window: Some(W::FiveHour),
                    text: ""
                },
                RecordedRoute::ambient()
            ),
            "cli:agy@flash",
            "agy always writes the seat"
        );
        let _pin = crate::providers::codex::pin_codex_env();
        let codex = ProviderRef::parse("cli:codex@gpt-5").unwrap();
        assert_eq!(
            codex.rejection_key(
                QuotaSignal {
                    window: Some(W::SevenDayOpus),
                    text: ""
                },
                RecordedRoute::ambient()
            ),
            "cli:codex@native",
            "codex ignores the signal window; the seat's own route decides"
        );
    }

    #[test]
    fn quota_key_splits_codex_by_route() {
        // The profile route is process env, so pin it: every other test thread
        // shares this process.
        let _pin = crate::providers::codex::pin_codex_env();
        let routed = ProviderRef::parse("cli:codex@gpt-5.2-codex").unwrap();
        assert_eq!(
            routed.quota_key(RecordedRoute::ambient()),
            "cli:codex@native",
            "an explicit model omits -p, so a bare model bills codex's own default"
        );
        let via_openrouter = ProviderRef::parse("cli:codex@openai/gpt-5").unwrap();
        assert_eq!(
            via_openrouter.quota_key(RecordedRoute::ambient()),
            "cli:codex@openrouter"
        );
        let bare = ProviderRef::parse("cli:codex").unwrap();
        assert_eq!(
            bare.quota_key(RecordedRoute::ambient()),
            "cli:codex@openrouter",
            "a model-less seat dispatches under the default profile, which layers OpenRouter"
        );
        let native = ProviderRef::parse("cli:codex@native").unwrap();
        assert_eq!(
            native.quota_key(RecordedRoute::ambient()),
            "cli:codex@native"
        );
        std::env::set_var("SPAR_CODEX_PROFILE", "");
        assert_eq!(
            routed.quota_key(RecordedRoute::ambient()),
            "cli:codex@native",
            "an explicit model omits -p under any profile"
        );
        assert_eq!(
            bare.quota_key(RecordedRoute::ambient()),
            "cli:codex@native",
            "without -p a model-less seat runs native too"
        );
        assert_eq!(
            via_openrouter.quota_key(RecordedRoute::ambient()),
            "cli:codex@openrouter",
            "an explicit slug forces OpenRouter under any profile"
        );
    }

    #[test]
    fn recorded_codex_profile_keys_the_seat_not_the_process_env() {
        use crate::providers::QuotaSignal;
        // The seat's bucket comes from the profile the run recorded at dispatch,
        // not from whatever SPAR_CODEX_PROFILE the keying process holds.
        let _pin = crate::providers::codex::pin_codex_env();
        std::env::set_var("SPAR_CODEX_PROFILE", "");
        let bare = ProviderRef::parse("cli:codex").unwrap();
        assert_eq!(
            bare.quota_key(RecordedRoute::ambient()),
            "cli:codex@native",
            "ambient empty profile runs native"
        );
        assert_eq!(
            bare.quota_key(RecordedRoute {
                profile: Some("muse"),
                model: None
            }),
            "cli:codex@openrouter",
            "the recorded dispatch profile decides, not the keying process's env"
        );
        assert_eq!(
            bare.quota_key(RecordedRoute {
                profile: Some(""),
                model: None
            }),
            "cli:codex@native",
            "a recorded set-but-empty profile is the native route"
        );
        assert_eq!(
            bare.quota_key(RecordedRoute {
                profile: Some("plain"),
                model: None
            }),
            "cli:codex",
            "an unverified profile keys the login"
        );
        assert_eq!(
            bare.quota_key(RecordedRoute::ambient()),
            "cli:codex@native",
            "no recorded profile falls back to the ambient env"
        );
        let modeled = ProviderRef::parse("cli:codex@gpt-5").unwrap();
        assert_eq!(
            modeled.quota_key(RecordedRoute {
                profile: Some("muse"),
                model: None
            }),
            "cli:codex@native",
            "an explicit model runs without -p regardless of profile"
        );
        let signal = QuotaSignal {
            window: None,
            text: "",
        };
        assert_eq!(
            bare.rejection_key(
                signal,
                RecordedRoute {
                    profile: Some("muse"),
                    model: None
                }
            ),
            "cli:codex@openrouter",
            "rejections land in the recorded bucket too"
        );
    }

    #[test]
    fn quota_key_splits_pi_by_upstream() {
        let slug = ProviderRef::parse("cli:pi@google/gemini-2.5-flash").unwrap();
        assert_eq!(
            slug.quota_key(RecordedRoute::ambient()),
            "cli:pi@openrouter"
        );
        let explicit = ProviderRef::parse("cli:pi@openrouter/anthropic/claude").unwrap();
        assert_eq!(
            explicit.quota_key(RecordedRoute::ambient()),
            "cli:pi@openrouter"
        );
        let direct = ProviderRef::parse("cli:pi@direct:anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(
            direct.quota_key(RecordedRoute::ambient()),
            "cli:pi@direct:anthropic"
        );
        let bare_word = ProviderRef::parse("cli:pi@gemini-2.5-flash").unwrap();
        assert_eq!(
            bare_word.quota_key(RecordedRoute::ambient()),
            "cli:pi",
            "pi's own resolution keys the login"
        );
        let bare = ProviderRef::parse("cli:pi").unwrap();
        assert_eq!(bare.quota_key(RecordedRoute::ambient()), "cli:pi");
    }

    #[test]
    fn quota_key_splits_api_per_model() {
        let modeled = ProviderRef::parse("api:anthropic@claude-opus-4-6").unwrap();
        assert_eq!(
            modeled.quota_key(RecordedRoute::ambient()),
            "api:anthropic@claude-opus-4-6"
        );
        let bare = ProviderRef::parse("api:anthropic").unwrap();
        assert_eq!(bare.quota_key(RecordedRoute::ambient()), "api:anthropic");
    }

    #[test]
    fn quota_key_is_stable_on_its_own_output() {
        // Bucket names are not seat models, but every layer re-resolves what it
        // reads (stats, operator pause/resume/probe), so feeding a stored key
        // back through `quota_key` must be a fixed point, never a collapse.
        // Codex bucketing reads the profile route from process env, so pin it:
        // every other test thread shares this process.
        let _pin = crate::providers::codex::pin_codex_env();
        for seat in [
            "cli:codex",
            "cli:codex@openrouter",
            "cli:codex@openai/gpt-5",
            "cli:codex@gpt-5.2-codex",
            "cli:codex@native",
            "cli:pi",
            "cli:pi@openrouter",
            "cli:pi@google/gemini-2.5-flash",
            "cli:pi@openrouter/anthropic/claude",
            "cli:pi@direct:anthropic/claude-sonnet-4-5",
            "cli:pi@direct:anthropic",
            "cli:pi@gemini-2.5-flash",
            "api:openai@gpt-5",
            "api:openai",
            "cli:grok",
            "cli:grok@grok-4-1-fast",
            "cli:muse",
            "cli:claude@opus",
            "cli:claude@claude-haiku-5-5",
            "cli:agy@flash",
        ] {
            let once = ProviderRef::parse(seat)
                .unwrap()
                .quota_key(RecordedRoute::ambient());
            let twice = ProviderRef::parse(&once)
                .unwrap()
                .quota_key(RecordedRoute::ambient());
            assert_eq!(twice, once, "{seat} does not round-trip");
        }
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
                ProviderRef::parse(seat)
                    .unwrap()
                    .quota_key(RecordedRoute::ambient()),
                seat.split('@').next().unwrap(),
                "{seat} has no separate windows"
            );
        }
    }

    #[test]
    fn rejection_key_lands_in_the_seat_bucket() {
        use crate::providers::{QuotaSignal, RateLimitWindow as W};
        let _pin = crate::providers::codex::pin_codex_env();
        let key = |seat: &str, window: Option<W>, text: &str| {
            ProviderRef::parse(seat)
                .unwrap()
                .rejection_key(QuotaSignal { window, text }, RecordedRoute::ambient())
        };
        assert_eq!(
            key("cli:codex@openai/gpt-5", Some(W::RateLimit), "rate limit"),
            "cli:codex@openrouter"
        );
        assert_eq!(
            key("cli:codex@gpt-5.2-codex", None, "rate limited, slow down"),
            "cli:codex@native",
            "an explicit bare model omits -p, so it bills native"
        );
        assert_eq!(
            key("cli:codex@native", None, "rate limited, slow down"),
            "cli:codex@native"
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
            "cli:pi@direct:anthropic"
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
