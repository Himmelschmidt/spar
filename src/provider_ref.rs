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
}

impl ProviderRef {
    /// Parse a provider ref. **Requires** `cli:` or `api:` prefix.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            bail!("empty provider name");
        }
        if let Some(rest) = raw.strip_prefix("api:") {
            let name = rest.trim();
            if name.is_empty() {
                bail!("api: requires a provider name (e.g. api:openai)");
            }
            if name.contains(':') {
                bail!("invalid provider '{raw}' (use api:name)");
            }
            return Ok(Self {
                backend: ExecBackend::ApiSdk,
                name: name.to_string(),
            });
        }
        if let Some(rest) = raw.strip_prefix("cli:") {
            let name = rest.trim();
            if name.is_empty() {
                bail!("cli: requires a provider name (e.g. cli:claude)");
            }
            if name.contains(':') {
                bail!("invalid provider '{raw}' (use cli:name)");
            }
            return Ok(Self {
                backend: ExecBackend::NativeCli,
                name: name.to_string(),
            });
        }
        bail!(
            "provider '{raw}' must be 'cli:…' or 'api:…' (e.g. cli:claude, api:openai)"
        );
    }

    pub fn display(&self) -> String {
        match self.backend {
            ExecBackend::NativeCli => format!("cli:{}", self.name),
            ExecBackend::ApiSdk => format!("api:{}", self.name),
        }
    }

    /// Key used in slot.provider field (stable string form).
    pub fn storage_key(&self) -> String {
        self.display()
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
        assert!(ProviderRef::parse("api:openai").unwrap().is_api());
    }
}
