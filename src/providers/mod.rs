mod agy;
pub mod agy_telemetry;
mod claude;
pub(crate) mod codex;
pub mod conversation_turn;
pub mod delivery;
mod grok;
mod muse;
pub mod muse_telemetry;
mod opencode;
pub mod opencode_telemetry;
pub mod presence;

use crate::provider_ref::{ExecBackend, ProviderRef};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use agy::AgyAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use grok::GrokAdapter;
pub use muse::MuseAdapter;
pub use opencode::OpencodeAdapter;

/// How the orchestrator hands a queued message to a *running* adapter at its next
/// turn boundary. The orchestrator asks the adapter for this; it never branches on
/// provider name inline (orchestrator / backend / adapter split).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStrategy {
    /// Claude Code, muse and grok: a `Stop` hook injects the claimed messages
    /// (`{"decision":"block","reason":…}`). Headless, no pane.
    StopHookInject,
    /// Codex: a best-effort native push (`codex queue --thread`), but the *guaranteed*
    /// fallback is the poll file, not the durable queue file — nothing reads the queue
    /// file for codex, while a codex role prompt is told to check its poll file. Used
    /// whenever no session id is captured yet (thread id not seen) as well as whenever
    /// a push with one fails; see W10.
    NativeQueuePollFallback,
    /// opencode: `client.session.prompt()` / `prompt_async` into the live session.
    /// Declared for matrix completeness; constructed once the opencode adapter lands.
    #[allow(dead_code)]
    SdkPrompt,
    /// No push channel into the running process, so spar writes to a slot-scoped file
    /// and the role prompt tells the agent to read it before it starts any new major
    /// step. That is the only moment a nudge is actionable anyway, so it needs no polling
    /// loop, and one `cat` of a small file is nothing against a 60M-token dispatch.
    PollFile,
    /// No injection channel — messages wait in the inbox for the agent's next turn.
    None,
}

/// Where an adapter's `working` / `blocked` / `idle` presence signal originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceSource {
    /// Lifecycle hooks call back into `spar bus heartbeat`. Claude reads
    /// `.claude/settings.json`; grok reads its own `.grok/hooks/spar.json` and muse
    /// its own `.muse/hooks.json`, same shape.
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

/// Would-a-dispatch-start verdict for a provider whose binary resolves.
/// Distinct from `available` (binary on PATH): a broken local config can leave
/// the binary present while every dispatch fails instantly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    /// No probe (adapter default), or the probe timed out / could not run.
    /// Never blocks: the provider stays as available as its binary makes it.
    Unknown,
    /// The probe ran and passed.
    Healthy,
    /// The probe ran and failed. The provider is still `available`, but
    /// `doctor` and `provider list` report it as unhealthy instead.
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub name: String,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub readiness: Readiness,
    /// The probe's own failure message, trimmed to one line. Set only when
    /// `readiness` is `Unhealthy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness_message: Option<String>,
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
    /// Resolved slot wall-clock budget in seconds: the role's **hard ceiling**, not its
    /// soft budget (`executor::hard_ceiling_for_role`, i.e. whichever of `slot_secs` /
    /// `review_secs` / `suite.timeout_secs` / `spec.timeout_secs` the role drew, times
    /// `timeouts.hard_ceiling_multiple`). Adapters whose CLI has its own self-timeout
    /// (e.g. agy's `--print-timeout`) derive it from this so the CLI's own kill lands no
    /// earlier than spar's. `None` = adapter default.
    pub timeout_secs: Option<u64>,
}

pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn binary_names(&self) -> &[&'static str];
    fn capabilities(&self) -> Capabilities;
    fn version_args(&self) -> &[&'static str] {
        &["--version"]
    }
    /// Optional readiness probe: argv run against the resolved binary answering
    /// "would a dispatch to this provider start". `None` (the default) means no
    /// probe, which reports `Unknown` and leaves the adapter unchanged.
    ///
    /// Every probe is non-negotiable local-only: config, auth-file and
    /// local-state reads, never a model call, never quota, never a mutation of
    /// the user's state. It runs under `READINESS_TIMEOUT`; a timeout (or a
    /// spawn failure) is `Unknown`, never `Unhealthy`.
    fn readiness_probe(&self) -> Option<&[&'static str]> {
        None
    }
    fn resolve_binary(&self) -> Option<PathBuf> {
        self.binary_names()
            .iter()
            .find_map(|n| which::which(n).ok())
    }
    fn detect(&self) -> ProviderReport {
        let path = self.resolve_binary();
        let (available, path_str, version, readiness, readiness_message) = match path {
            Some(p) => {
                let version = probe_version(&p, self.version_args());
                let (readiness, readiness_message) =
                    probe_readiness(&p, self.readiness_probe(), READINESS_TIMEOUT);
                (
                    true,
                    Some(p.display().to_string()),
                    version,
                    readiness,
                    readiness_message,
                )
            }
            None => (false, None, None, Readiness::Unknown, None),
        };
        ProviderReport {
            name: self.name().into(),
            available,
            path: path_str,
            version,
            readiness,
            readiness_message,
            capabilities: self.capabilities(),
            delivery: self.delivery_strategy(),
            presence: self.presence_source(),
        }
    }
    fn permission_args(&self, policy: TrustPolicy) -> Vec<String>;
    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command;
    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command;

    /// Build a command that resumes a previously captured native session (e.g. codex's
    /// `thread.started` id) instead of a cold `build_headless` dispatch. `session_id` is
    /// the value this adapter itself reported (`StreamStats::session_id`) on an earlier
    /// round of the same slot. Returns `None` when the adapter has no such capability, or
    /// declines to use it here; the caller falls back to `build_headless`.
    fn build_resume(&self, _bin: &Path, _opts: &SpawnOpts, _session_id: &str) -> Option<Command> {
        None
    }

    /// Whether a resume dispatch that never established a session (no native session id
    /// captured) failed because the vendor session itself is gone, as opposed to some
    /// other failure that happened to occur before the session announced itself. Given
    /// the failed dispatch's raw log text plus the session id the resume was attempted
    /// with (`None` when the dispatch was cold — callers only ask this on a resume, so
    /// that is always `Some` in practice). The caller (`executor::resume_lost_its_session`
    /// call sites) only clears the slot's session marker and retries cold when this
    /// returns `true` — otherwise the marker is left alone and the failure is reported
    /// like any other, since the vendor session may still be resumable once whatever
    /// else went wrong clears up. Default `true`: adapters with no session-loss signature
    /// of their own keep the pre-existing behavior (any pre-session failure was assumed
    /// to be a lost session). Adapters whose stdout never carries the answer (muse's
    /// resume marker lives only in its on-disk session log) ignore `log_text` and answer
    /// from their own session store via `session_id` instead.
    fn resume_failure_is_missing_session(
        &self,
        _log_text: &str,
        _session_id: Option<&str>,
    ) -> bool {
        true
    }

    /// Whether a failed dispatch's log carries this adapter's signature of a transient
    /// provider-side failure worth retrying (see `executor::dispatch_with_resume_recovery`).
    /// Default `false`: no adapter retries except one that names its own string. Only
    /// the overriding adapter's failures ever match, so no other provider's behavior
    /// changes.
    fn dispatch_failure_is_transient(&self, _log_text: &str) -> bool {
        false
    }

    /// Whether `code` from this adapter means spar built a bad command line (a usage
    /// error), rather than the agent failing. Default `false`; muse overrides for its
    /// exit 2. A usage error is reported as spar's fault, never retried, and never
    /// treated as an agent failure.
    fn is_usage_error(&self, _code: Option<i32>) -> bool {
        false
    }

    /// Whether a failed dispatch's log carries this adapter's signature of a deliberately
    /// stopped run (a model-step or token budget the vendor enforced), rather than a
    /// crash. Default `false`; muse overrides. Classifies the error text only — it never
    /// gates a retry on its own.
    fn step_budget_exhausted(&self, _log_text: &str) -> bool {
        false
    }

    /// Extra environment for this adapter's dispatches, derived from the spawn options
    /// (e.g. a vendor self-timeout aligned to `SpawnOpts::timeout_secs`, following the
    /// contract its doc comment describes). Default empty. This exists because
    /// `command_to_parts` reduces a built `Command` to program-plus-args, so anything
    /// set via `cmd.env` in `build_headless`/`build_resume` would silently never reach
    /// the child — the executor merges this into `SpawnRequest::env` instead.
    fn extra_env(&self, _opts: &SpawnOpts) -> Vec<(String, String)> {
        Vec::new()
    }

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

    /// Worktree-relative path of the project hook file `presence::wire` installs for
    /// a `PresenceSource::Hooks` adapter. Claude reads `.claude/settings.json`;
    /// grok reads `.grok/hooks/spar.json` and muse `.muse/hooks.json` (same
    /// `{"hooks": …}` shape, probed live against grok 1.0.25 and muse 1.3.0 — a bare
    /// event map without the wrapper never fires).
    fn hook_file_rel(&self) -> &'static str {
        ".claude/settings.json"
    }
}

/// How long a readiness probe may run. Much longer than the 2s version bound:
/// `opencode models` startup on this box varies from ~2s warm to ~10s cold,
/// so anything under that reports Unknown on a slow day and the probe never
/// catches a broken config. Only adapters with a probe pay this, and only up
/// to the bound. A timeout still reports `Unknown`, never `Unhealthy`.
pub(crate) const READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Longest `readiness_message` kept, in chars. The first-line rule bounds lines
/// but not line length; without this a single multi-KB line lands verbatim in
/// human output, notes, and JSON.
pub(crate) const READINESS_MESSAGE_CHARS: usize = 300;

/// Run an adapter's readiness probe: `bin` plus `probe` argv. `None` (no probe)
/// is `Unknown`. Otherwise exit 0 is `Healthy`; a non-zero exit is `Unhealthy`
/// with the probe's own message trimmed to one line (stderr first, where CLIs
/// put errors), ANSI-stripped and capped at `READINESS_MESSAGE_CHARS`. A
/// timeout, a spawn failure, or death by signal with no output to judge it by
/// is `Unknown`: the probe could not run, so there is no verdict.
fn probe_readiness(
    bin: &PathBuf,
    probe: Option<&[&str]>,
    timeout: std::time::Duration,
) -> (Readiness, Option<String>) {
    let Some(args) = probe else {
        return (Readiness::Unknown, None);
    };
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args);
    let Some(output) = run_with_timeout(&mut cmd, timeout) else {
        return (Readiness::Unknown, None);
    };
    if output.status.success() {
        return (Readiness::Healthy, None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stderr
        .lines()
        .map(|l| strip_ansi(l).into_owned())
        .map(|l| l.trim().to_string())
        .find(|l| !l.is_empty())
        .or_else(|| {
            stdout
                .lines()
                .map(|l| strip_ansi(l).into_owned())
                .map(|l| l.trim().to_string())
                .find(|l| !l.is_empty())
        });
    match line {
        Some(l) => (
            Readiness::Unhealthy,
            Some(truncate_chars(&l, READINESS_MESSAGE_CHARS)),
        ),
        None if output.status.code().is_some() => (
            Readiness::Unhealthy,
            Some(format!(
                "probe exited {} with no output",
                output.status.code().expect("checked is_some")
            )),
        ),
        None => (Readiness::Unknown, None),
    }
}

/// Strip ANSI CSI escape sequences (`\x1b[` … final byte, e.g. color codes) so
/// a probe's message is plain text in human output and `--json`. No new dep
/// for this: CLIs emit color, not cursor games, and anything unrecognized is
/// left in place rather than eaten.
fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('\x1b') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.clone().next() == Some('[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Keep at most `max` chars (never splitting UTF-8), appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}…")
}

fn probe_version(bin: &PathBuf, args: &[&str]) -> Option<String> {
    use std::time::Duration;
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args);
    let output = run_with_timeout(&mut cmd, Duration::from_secs(2))?;
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

fn run_with_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    // stdin is null so a probed CLI can never block on a prompt; the pipes are
    // drained on reader threads from spawn, because draining only after exit
    // deadlocks once the child fills the pipe buffer (~64KB) and the timeout
    // then misreports a chatty success as never-finishing.
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });
    let stderr = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The child is dead so its write ends are closed; the joins
                // only wait out bytes already in flight.
                let out = stdout
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                let err = stderr
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                return Some(std::process::Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

pub fn all_adapters() -> Vec<Box<dyn ProviderAdapter>> {
    vec![
        Box::new(ClaudeAdapter),
        Box::new(GrokAdapter),
        Box::new(AgyAdapter),
        Box::new(CodexAdapter),
        Box::new(OpencodeAdapter),
        Box::new(MuseAdapter),
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
            api_provider_supported(&pref.name)
        }
    }
}

/// Named API providers the api-sdk backend actually has an adapter for.
/// Slot dispatch fails later if keys are missing; this only screens the name itself.
pub fn api_provider_supported(name: &str) -> bool {
    matches!(name, "openai" | "xai" | "anthropic" | "google" | "meta")
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
            .filter_map(|n| ProviderRef::parse(n).ok().map(|p| p.display()))
            .collect::<Vec<_>>()
    } else if allow_missing {
        if order.is_empty() {
            vec!["cli:claude".into(), "cli:grok".into(), "cli:agy".into()]
        } else {
            order
                .iter()
                .filter_map(|n| ProviderRef::parse(n).ok().map(|p| p.display()))
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
    fn resume_failure_is_missing_session_defaults_true() {
        // Adapters with no session-loss signature of their own (i.e. everyone but
        // codex) keep the pre-existing behavior: any pre-session resume failure is
        // treated as a lost session, since they have no better signal to distinguish.
        assert!(GrokAdapter.resume_failure_is_missing_session("anything, or nothing", None));
        assert!(GrokAdapter.resume_failure_is_missing_session("", Some("sess-1")));
        assert!(!GrokAdapter.dispatch_failure_is_transient("does not exist or you lack access"));
        assert!(!GrokAdapter.is_usage_error(Some(2)));
        assert!(!GrokAdapter.step_budget_exhausted("max-model-steps"));
        // Grok dispatches carry run-scoped folder trust (never operator state).
        let env = GrokAdapter.extra_env(&SpawnOpts {
            prompt: String::new(),
            prompt_file: None,
            cwd: std::path::PathBuf::from("/tmp"),
            trust: TrustPolicy::Prompt,
            extra_args: vec![],
            model: None,
            timeout_secs: Some(60),
        });
        assert_eq!(
            env,
            vec![("GROK_FOLDER_TRUST".to_string(), "0".to_string())]
        );
    }

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
    fn model_survives_fleet_selection() {
        let picked = pick_providers(
            &[],
            2,
            Some(&[
                "cli:codex@openai/gpt-4o-mini".into(),
                "api:openai@gpt-5".into(),
            ]),
            true,
        );
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0], "cli:codex@openai/gpt-4o-mini");
        assert_eq!(picked[1], "api:openai@gpt-5");
    }

    #[test]
    fn model_variant_usable_and_adapter_model_free() {
        // The @model ref is usable and its adapter lookup ignores the model.
        assert!(is_provider_usable("cli:claude@sonnet", true));
        assert!(adapter_named("cli:claude@sonnet").is_some());
    }

    struct StubAdapter {
        probe: Option<&'static [&'static str]>,
    }

    impl ProviderAdapter for StubAdapter {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn binary_names(&self) -> &[&'static str] {
            &["sh"]
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn readiness_probe(&self) -> Option<&[&'static str]> {
            self.probe
        }
        fn permission_args(&self, _policy: TrustPolicy) -> Vec<String> {
            Vec::new()
        }
        fn build_headless(&self, bin: &Path, _opts: &SpawnOpts) -> Command {
            Command::new(bin)
        }
        fn build_interactive(&self, bin: &Path, _opts: &SpawnOpts) -> Command {
            Command::new(bin)
        }
    }

    #[test]
    fn no_probe_reports_unknown_and_stays_available() {
        let r = StubAdapter { probe: None }.detect();
        assert!(
            r.available,
            "binary on PATH stays available without a probe"
        );
        assert_eq!(r.readiness, Readiness::Unknown);
        assert_eq!(r.readiness_message, None);
    }

    #[test]
    fn passing_probe_reports_healthy() {
        let r = StubAdapter {
            probe: Some(&["-c", "exit 0"]),
        }
        .detect();
        assert!(r.available);
        assert_eq!(r.readiness, Readiness::Healthy);
        assert_eq!(r.readiness_message, None);
    }

    #[test]
    fn failing_probe_reports_unhealthy_with_first_line() {
        let r = StubAdapter {
            probe: Some(&["-c", "echo first >&2; echo second >&2; exit 1"]),
        }
        .detect();
        assert!(r.available, "binary still resolves; only readiness changes");
        assert_eq!(r.readiness, Readiness::Unhealthy);
        assert_eq!(r.readiness_message.as_deref(), Some("first"));
    }

    #[test]
    fn timed_out_probe_reports_unknown_not_unhealthy() {
        let bin = which::which("sh").expect("sh on PATH for probe tests");
        let (readiness, message) = probe_readiness(
            &bin,
            Some(&["-c", "sleep 30"]),
            std::time::Duration::from_millis(100),
        );
        assert_eq!(readiness, Readiness::Unknown);
        assert_eq!(message, None);
    }

    #[test]
    fn failing_probe_message_is_ansi_stripped() {
        let bin = which::which("sh").expect("sh on PATH for probe tests");
        // opencode's real failure shape: color codes around the message.
        let (readiness, message) = probe_readiness(
            &bin,
            Some(&[
                "-c",
                "printf '\\033[91m\\033[1mError: \\033[0mbroken\\n' >&2; exit 1",
            ]),
            std::time::Duration::from_secs(10),
        );
        assert_eq!(readiness, Readiness::Unhealthy);
        assert_eq!(message.as_deref(), Some("Error: broken"));
    }

    #[test]
    fn failing_probe_message_is_capped_at_one_short_line() {
        let bin = which::which("sh").expect("sh on PATH for probe tests");
        let (readiness, message) = probe_readiness(
            &bin,
            Some(&["-c", "head -c 1000 /dev/zero | tr '\\0' 'x' >&2; exit 1"]),
            std::time::Duration::from_secs(10),
        );
        assert_eq!(readiness, Readiness::Unhealthy);
        let m = message.expect("failing probe with output names it");
        assert_eq!(m.chars().count(), READINESS_MESSAGE_CHARS + 1);
        assert!(m.ends_with('…'), "capped message carries an ellipsis");
        assert!(!m.contains('\n'));
    }

    #[test]
    fn signal_killed_probe_with_no_output_is_unknown() {
        let bin = which::which("sh").expect("sh on PATH for probe tests");
        let (readiness, message) = probe_readiness(
            &bin,
            Some(&["-c", "kill -9 $$"]),
            std::time::Duration::from_secs(10),
        );
        assert_eq!(readiness, Readiness::Unknown);
        assert_eq!(message, None);
    }

    #[test]
    fn chatty_probe_does_not_deadlock_the_pipe() {
        // ~1.4MB on stdout: with drain-after-exit this fills the pipe buffer
        // and the child never exits, misreporting as Unknown on timeout.
        let bin = which::which("sh").expect("sh on PATH for probe tests");
        let (readiness, message) = probe_readiness(
            &bin,
            Some(&["-c", "seq 1 200000; exit 3"]),
            std::time::Duration::from_secs(20),
        );
        assert_eq!(readiness, Readiness::Unhealthy);
        assert_eq!(message.as_deref(), Some("1"));
    }

    #[test]
    fn only_opencode_wires_a_probe() {
        assert_eq!(OpencodeAdapter.readiness_probe(), Some(&["models"][..]));
        assert_eq!(ClaudeAdapter.readiness_probe(), None);
        assert_eq!(GrokAdapter.readiness_probe(), None);
        assert_eq!(AgyAdapter.readiness_probe(), None);
        assert_eq!(CodexAdapter.readiness_probe(), None);
        assert_eq!(MuseAdapter.readiness_probe(), None);
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
