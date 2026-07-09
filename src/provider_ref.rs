//! Provider identity: `cli:claude`, `api:openai`, or bare `claude` (native-cli).
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
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if let Some(rest) = raw.strip_prefix("api:") {
            return Self {
                backend: ExecBackend::ApiSdk,
                name: rest.to_string(),
            };
        }
        if let Some(rest) = raw.strip_prefix("cli:") {
            return Self {
                backend: ExecBackend::NativeCli,
                name: rest.to_string(),
            };
        }
        // bare api provider names
        match raw {
            "openai" | "anthropic" | "xai" | "google" | "meta" => Self {
                backend: ExecBackend::ApiSdk,
                name: raw.into(),
            },
            _ => Self {
                backend: ExecBackend::NativeCli,
                name: raw.into(),
            },
        }
    }

    pub fn display(&self) -> String {
        match self.backend {
            ExecBackend::NativeCli => {
                if self.name.contains(':') {
                    self.name.clone()
                } else {
                    format!("cli:{}", self.name)
                }
            }
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
    fn parses_forms() {
        assert_eq!(ProviderRef::parse("claude").backend, ExecBackend::NativeCli);
        assert_eq!(ProviderRef::parse("cli:grok").name, "grok");
        assert!(ProviderRef::parse("api:openai").is_api());
        assert!(ProviderRef::parse("xai").is_api());
    }
}
