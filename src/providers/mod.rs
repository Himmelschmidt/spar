mod agy;
mod claude;
mod grok;

use crate::provider_ref::{ExecBackend, ProviderRef};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use agy::AgyAdapter;
pub use claude::ClaudeAdapter;
pub use grok::GrokAdapter;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub name: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub capabilities: Capabilities,
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
        }
    }
    fn permission_args(&self, policy: TrustPolicy) -> Vec<String>;
    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command;
    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command;
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

pub fn adapter_named(name: &str) -> Option<Box<dyn ProviderAdapter>> {
    let pref = ProviderRef::parse(name);
    let cli = pref.cli_name().unwrap_or(name);
    all_adapters().into_iter().find(|a| a.name() == cli)
}

/// Whether a provider ref can be used for a live slot.
pub fn is_provider_usable(raw: &str, allow_missing: bool) -> bool {
    let pref = ProviderRef::parse(raw);
    match pref.backend {
        ExecBackend::NativeCli => {
            if allow_missing {
                return true;
            }
            adapter_named(&pref.name)
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

/// Providers that are on PATH, optionally filtered/ordered by `order`.
pub fn available_providers(order: &[String]) -> Vec<String> {
    let detected = detect_all();
    let available: Vec<String> = if order.is_empty() {
        detected
            .into_iter()
            .filter(|p| p.available)
            .map(|p| p.name)
            .collect()
    } else {
        order
            .iter()
            .filter(|n| is_provider_usable(n, false))
            .cloned()
            .collect()
    };
    available
}

/// Prefer multi-provider when possible; fall back to repeating available ones.
///
/// When `allow_missing` is true (CLI `--dry-run` or `SPAR_DRY_RUN`), names
/// need not be on PATH / have API keys.
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
            .map(|n| {
                // Normalize bare names; keep api:/cli: storage keys for API
                let pref = ProviderRef::parse(n);
                if pref.is_api() {
                    pref.storage_key()
                } else {
                    // Prefer bare CLI name for adapters
                    pref.name
                }
            })
            .collect::<Vec<_>>()
    } else if allow_missing {
        if order.is_empty() {
            vec!["claude".into(), "grok".into(), "agy".into()]
        } else {
            order
                .iter()
                .map(|n| {
                    let pref = ProviderRef::parse(n);
                    if pref.is_api() {
                        pref.storage_key()
                    } else {
                        pref.name
                    }
                })
                .collect()
        }
    } else {
        available_providers(order)
    };

    if base.is_empty() {
        if allow_missing {
            let fallback = if order.is_empty() {
                vec!["claude".into(), "grok".into(), "agy".into()]
            } else {
                order.to_vec()
            };
            return cycle_take(&fallback, n);
        }
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
        assert!(picked[0].contains("openai") || picked[0].starts_with("api:"));
        assert_eq!(picked[1], "grok");
    }

    #[test]
    fn live_accepts_api_names() {
        assert!(is_provider_usable("api:openai", false));
        assert!(is_provider_usable("xai", false));
        assert!(!is_provider_usable("api:notreal", false));
    }
}
