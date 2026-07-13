mod agy;
mod claude;
mod grok;
pub mod presence;

use crate::provider_ref::{ExecBackend, ProviderRef};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use agy::AgyAdapter;
pub use claude::ClaudeAdapter;
pub use grok::GrokAdapter;

/// How the orchestrator hands a queued message to a *running* adapter at its next
/// turn boundary. The orchestrator asks the adapter for this; it never branches on
/// provider name inline (orchestrator / backend / adapter split).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStrategy {
    /// Claude Code: a `Stop` hook injects the claimed messages
    /// (`{"decision":"block","reason":…}` / `additionalContext`). Headless, no pane.
    StopHookInject,
    /// Grok: push to the native `/queue`; applied at the turn boundary even mid-turn.
    NativeQueue,
    /// opencode: `client.session.prompt()` / `prompt_async` into the live session.
    /// Declared for matrix completeness; constructed once the opencode adapter lands.
    #[allow(dead_code)]
    SdkPrompt,
    /// No injection channel — messages wait in the inbox for the agent's next turn.
    None,
}

/// Where an adapter's `working` / `blocked` / `idle` presence signal originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceSource {
    /// Claude-format `.claude/settings.json` hooks call back into `spar bus heartbeat`.
    /// Grok reads the same file, so one hook file covers both.
    Hooks,
    /// Provider posts lifecycle notifications to an HTTP endpoint (e.g. Grok push hooks).
    /// Declared for matrix completeness; constructed once that adapter path lands.
    #[allow(dead_code)]
    HttpPush,
    /// Server-sent events bus (opencode `GET /event`: session.idle / tool.execute.* / permission.ask).
    /// Declared for matrix completeness; constructed once the opencode adapter lands.
    #[allow(dead_code)]
    Sse,
    /// No event stream — presence is degraded to a process/output heuristic.
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub name: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub capabilities: Capabilities,
    /// Turn-boundary delivery channel this adapter exposes.
    pub delivery: DeliveryStrategy,
    /// Where this adapter's presence transitions come from.
    pub presence: PresenceSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub headless: bool,
    pub interactive: bool,
    pub resume: bool,
    pub skip_permissions: bool,
    pub native_sandbox: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            headless: false,
            interactive: true,
            resume: false,
            skip_permissions: false,
            native_sandbox: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustPolicy {
    /// Strongest auto-approve flags each CLI allows (default for swarm).
    FullAuto,
    /// No skip-permission flags.
    #[allow(dead_code)]
    Prompt,
}

#[derive(Debug, Clone)]
pub struct SpawnOpts {
    pub prompt: String,
    pub prompt_file: Option<PathBuf>,
    pub cwd: PathBuf,
    pub trust: TrustPolicy,
    /// Extra args appended after provider defaults.
    pub extra_args: Vec<String>,
    /// Preferred model id (`--model` on CLIs that support it).
    pub model: Option<String>,
}

pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn binary_names(&self) -> &[&'static str];
    fn capabilities(&self) -> Capabilities;
    fn version_args(&self) -> &[&'static str] {
        &["--version"]
    }
    fn resolve_binary(&self) -> Option<PathBuf> {
        self.binary_names()
            .iter()
            .find_map(|n| which::which(n).ok())
    }
    fn detect(&self) -> ProviderReport {
        let path = self.resolve_binary();
        let (available, path_str, version) = match path {
            Some(p) => {
                let version = probe_version(&p, self.version_args());
                (true, Some(p.display().to_string()), version)
            }
            None => (false, None, None),
        };
        ProviderReport {
            name: self.name().into(),
            available,
            path: path_str,
            version,
            capabilities: self.capabilities(),
            delivery: self.delivery_strategy(),
            presence: self.presence_source(),
        }
    }
    fn permission_args(&self, policy: TrustPolicy) -> Vec<String>;
    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command;
    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command;

    /// Turn-boundary delivery channel for this adapter (see `DeliveryStrategy`).
    /// Defaults to inbox-on-next-turn; adapters with a live channel override.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::None
    }

    /// Where this adapter's presence transitions come from (see `PresenceSource`).
    /// Defaults to none (degraded); adapters with an event stream override.
    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }
}

fn probe_version(bin: &PathBuf, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(bin).args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if !stdout.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    if line.to_ascii_lowercase().starts_with("usage") {
        return Some("available".into());
    }
    Some(line.to_string())
}

pub fn all_adapters() -> Vec<Box<dyn ProviderAdapter>> {
    vec![
        Box::new(ClaudeAdapter),
        Box::new(GrokAdapter),
        Box::new(AgyAdapter),
    ]
}

pub fn detect_all() -> Vec<ProviderReport> {
    all_adapters().iter().map(|a| a.detect()).collect()
}

/// Resolve a CLI adapter by `cli:name` **or** bare adapter name (`claude`).
/// API refs (`api:…`) return `None` here — they use the api-sdk path.
pub fn adapter_named(name: &str) -> Option<Box<dyn ProviderAdapter>> {
    let bare = if let Ok(pref) = ProviderRef::parse(name) {
        pref.cli_name()?.to_string()
    } else {
        // Internal call sites pass bare adapter ids (e.g. "claude").
        name.trim().to_string()
    };
    all_adapters().into_iter().find(|a| a.name() == bare)
}

/// Whether a provider ref can be used for a live slot.
pub fn is_provider_usable(raw: &str, allow_missing: bool) -> bool {
    let Ok(pref) = ProviderRef::parse(raw) else {
        return false;
    };
    match pref.backend {
        ExecBackend::NativeCli => {
            if allow_missing {
                return true;
            }
            adapter_named(&pref.storage_key())
                .or_else(|| adapter_named(&pref.name))
                .map(|a| a.resolve_binary().is_some())
                .unwrap_or(false)
        }
        ExecBackend::ApiSdk => {
            if allow_missing {
                return true;
            }
            // Live: accept named API providers; slot fails later if keys missing.
            matches!(
                pref.name.as_str(),
                "openai" | "xai" | "anthropic" | "google" | "meta"
            )
        }
    }
}

/// Providers that are on PATH, as `cli:name` keys, optionally filtered by `order`.
pub fn available_providers(order: &[String]) -> Vec<String> {
    if order.is_empty() {
        detect_all()
            .into_iter()
            .filter(|p| p.available)
            .map(|p| format!("cli:{}", p.name))
            .collect()
    } else {
        order
            .iter()
            .filter(|n| is_provider_usable(n, false))
            .cloned()
            .collect()
    }
}

/// Prefer multi-provider when possible; fall back to repeating available ones.
///
/// When `allow_missing` is true (CLI `--dry-run` or `SPAR_DRY_RUN`), names
/// need not be on PATH / have API keys.
///
/// Returned strings are always `cli:…` or `api:…`.
pub fn pick_providers(
    order: &[String],
    n: usize,
    requested: Option<&[String]>,
    allow_missing: bool,
) -> Vec<String> {
    let allow_missing = allow_missing || crate::util::env_truthy("SPAR_DRY_RUN");
    let base = if let Some(req) = requested {
        req.iter()
            .filter(|n| is_provider_usable(n, allow_missing))
            .filter_map(|n| ProviderRef::parse(n).ok().map(|p| p.storage_key()))
            .collect::<Vec<_>>()
    } else if allow_missing {
        if order.is_empty() {
            vec!["cli:claude".into(), "cli:grok".into(), "cli:agy".into()]
        } else {
            order
                .iter()
                .filter_map(|n| ProviderRef::parse(n).ok().map(|p| p.storage_key()))
                .collect()
        }
    } else {
        available_providers(order)
    };

    if base.is_empty() {
        return Vec::new();
    }
    cycle_take(&base, n)
}

fn cycle_take(items: &[String], n: usize) -> Vec<String> {
    if items.is_empty() || n == 0 {
        return Vec::new();
    }
    (0..n).map(|i| items[i % items.len()].clone()).collect()
}

pub fn command_to_parts(cmd: &Command) -> (PathBuf, Vec<String>) {
    let program = PathBuf::from(cmd.get_program());
    let args = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    (program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_keeps_api_and_cli_prefix() {
        let picked = pick_providers(
            &[],
            2,
            Some(&["api:openai".into(), "cli:grok".into()]),
            true,
        );
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0], "api:openai");
        assert_eq!(picked[1], "cli:grok");
    }

    #[test]
    fn live_accepts_api_names() {
        assert!(is_provider_usable("api:openai", false));
        assert!(!is_provider_usable("xai", false)); // bare rejected
        assert!(!is_provider_usable("api:notreal", false));
        assert!(!is_provider_usable("claude", true)); // bare rejected even dry
        assert!(is_provider_usable("cli:claude", true));
    }
}
