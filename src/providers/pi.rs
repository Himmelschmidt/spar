use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use crate::effort::EffortLevel;
use std::path::Path;
use std::process::Command;

/// spar's per-slot model (`--select` or a `cli:pi@<model>` ref) wins; otherwise
/// `SPAR_PI_MODEL`; otherwise none, and pi's own settings pick. Blank values are
/// ignored so `--model ""` is never emitted.
fn pi_model(opts_model: Option<&str>) -> Option<String> {
    opts_model
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("SPAR_PI_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .map(|m| pi_model_arg(&m))
}

/// OpenRouter is the explicit route (O25, kept by O99). A `vendor/slug` is an
/// OpenRouter id and gets `openrouter/` prepended, because pi would otherwise route
/// it by its first segment: `google/…`, `anthropic/…` and `openai/…` are both
/// OpenRouter vendors and pi provider ids, and the same ref must not change
/// provider with whichever keys happen to be set. An explicit `openrouter/…`
/// passes through. A bare word (`claude-sonnet-4-5`) has no vendor to route on and
/// passes through to pi's own resolution, which is how a non-OpenRouter pi
/// provider is reached.
fn pi_model_arg(model: &str) -> String {
    if model.contains('/') && !model.starts_with("openrouter/") {
        return format!("openrouter/{model}");
    }
    model.to_string()
}

/// pi's `--thinking` takes `off minimal low medium high xhigh max`. spar's `none`
/// is pi's `off`; `ultra` has no pi rung and is refused upstream by
/// `effort::check_compatible` before this is reached.
fn thinking_arg(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::None => "off",
        other => other.as_str(),
    }
}

pub struct PiAdapter;

impl PiAdapter {
    fn command(&self, bin: &Path, opts: &SpawnOpts, session_id: Option<&str>) -> Command {
        let mut cmd = Command::new(bin);
        cmd.arg("--mode").arg("json");
        // Ignore project-local pi resources: a worktree's `.pi/` is the agent's
        // own output, not configuration spar vetted.
        cmd.arg("--no-approve");
        if let Some(id) = session_id {
            cmd.arg("--session-id").arg(id);
        }
        if let Some(m) = pi_model(opts.model.as_deref()) {
            cmd.arg("--model").arg(m);
        }
        if let Some(e) = opts.effort {
            cmd.arg("--thinking").arg(thinking_arg(e));
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        let prompt = if !opts.prompt.is_empty() {
            opts.prompt.clone()
        } else if let Some(pf) = &opts.prompt_file {
            std::fs::read_to_string(pf)
                .unwrap_or_else(|_| format!("Read and follow instructions in {}", pf.display()))
        } else {
            String::new()
        };
        cmd.arg("--");
        cmd.arg(prompt);
        cmd.current_dir(&opts.cwd);
        cmd
    }
}

impl ProviderAdapter for PiAdapter {
    fn name(&self) -> &'static str {
        "pi"
    }

    // No push channel into a running `pi --mode json` and no presence stream yet:
    // a pi extension could provide both, and is deliberately not wired here.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::PollFile
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }

    fn binary_names(&self) -> &[&'static str] {
        &["pi"]
    }

    fn version_args(&self) -> &[&'static str] {
        &["--version"]
    }

    /// `pi auth check` resolves credentials locally and exits 0 ready, 1 not ready,
    /// 2 invalid. It sees a missing or unparseable key, unlike a config parse, but
    /// not a key the provider would reject. `--no-refresh` keeps it from touching
    /// the network to refresh OAuth. Detection is per adapter, not per slot, so the
    /// provider checked is the one `SPAR_PI_MODEL` routes to, else OpenRouter.
    fn readiness_probe(&self) -> Option<Vec<String>> {
        let target = match pi_model(None) {
            Some(m) => vec!["--model".to_string(), m],
            None => vec!["--provider".to_string(), "openrouter".to_string()],
        };
        let mut argv = vec!["auth".to_string(), "check".to_string()];
        argv.extend(target);
        argv.extend(["--json".to_string(), "--no-refresh".to_string()]);
        Some(argv)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            // pi has a TUI, but spar has never driven it: a tmux pane runs the same
            // `--mode json` stream so the teed log replays through the coalescer.
            interactive: false,
            resume: self.supports_resume(),
            // pi has no permission prompts, so full-auto needs no flag.
            skip_permissions: true,
            native_sandbox: false,
            assigns_session_id: true,
        }
    }

    fn permission_args(&self, _policy: TrustPolicy) -> Vec<String> {
        Vec::new()
    }

    /// `--session-id` creates the session when missing and continues it when
    /// present, so a cold dispatch and a resume are the same flag.
    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        self.command(bin, opts, opts.session_id.as_deref())
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        self.build_headless(bin, opts)
    }

    fn build_resume(&self, bin: &Path, opts: &SpawnOpts, session_id: &str) -> Option<Command> {
        Some(self.command(bin, opts, Some(session_id)))
    }

    fn supports_resume(&self) -> bool {
        true
    }

    /// `--session-id` never refuses a missing id: it creates one (verified on 1.4.0,
    /// stderr `No project session found with id …; creating a new session`). A
    /// resume that failed before the `session` header is therefore not a lost
    /// session.
    fn resume_failure_is_missing_session(
        &self,
        _log_text: &str,
        _session_id: Option<&str>,
    ) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::command_to_parts;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn opts(prompt: &str, model: Option<&str>) -> SpawnOpts {
        SpawnOpts {
            prompt: prompt.into(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            session_id: None,
            model: model.map(Into::into),
            effort: None,
            timeout_secs: None,
        }
    }

    fn args(cmd: &Command) -> Vec<String> {
        command_to_parts(cmd).1
    }

    fn flag(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    }

    #[test]
    fn fresh_dispatch_shape() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        let mut o = opts("do the thing", None);
        o.session_id = Some("123e4567-e89b-52d3-a456-426614174000".into());
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &o));
        assert_eq!(
            a,
            vec![
                "--mode",
                "json",
                "--no-approve",
                "--session-id",
                "123e4567-e89b-52d3-a456-426614174000",
                "--",
                "do the thing",
            ]
        );
    }

    #[test]
    fn resume_reuses_the_same_session_flag() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        let o = opts("next round", Some("google/gemini-2.5-flash"));
        let a = args(
            &PiAdapter
                .build_resume(Path::new("pi"), &o, "prior-id")
                .unwrap(),
        );
        assert_eq!(flag(&a, "--session-id").as_deref(), Some("prior-id"));
        assert_eq!(
            flag(&a, "--model").as_deref(),
            Some("openrouter/google/gemini-2.5-flash")
        );
        assert_eq!(a.last().map(String::as_str), Some("next round"));
        assert!(PiAdapter.capabilities().resume);
    }

    #[test]
    fn no_session_id_omits_the_flag() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("x", None)));
        assert!(!a.iter().any(|x| x == "--session-id"), "{a:?}");
        assert!(!a.iter().any(|x| x == "--model"), "{a:?}");
    }

    #[test]
    fn model_routing() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        for (given, want) in [
            (
                "google/gemini-2.5-flash",
                "openrouter/google/gemini-2.5-flash",
            ),
            (
                "anthropic/claude-sonnet-4.5",
                "openrouter/anthropic/claude-sonnet-4.5",
            ),
            ("meta/muse-spark-1.3", "openrouter/meta/muse-spark-1.3"),
            (
                "openrouter/google/gemini-2.5-flash",
                "openrouter/google/gemini-2.5-flash",
            ),
            ("claude-sonnet-4-5", "claude-sonnet-4-5"),
        ] {
            let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("x", Some(given))));
            assert_eq!(flag(&a, "--model").as_deref(), Some(want), "{given}");
        }
    }

    #[test]
    fn model_env_fallback_and_opts_precedence() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SPAR_PI_MODEL", "x-ai/grok-4");
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("x", None)));
        assert_eq!(
            flag(&a, "--model").as_deref(),
            Some("openrouter/x-ai/grok-4")
        );
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("x", Some("meta/m"))));
        assert_eq!(flag(&a, "--model").as_deref(), Some("openrouter/meta/m"));
        std::env::set_var("SPAR_PI_MODEL", "  ");
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("x", None)));
        assert!(!a.iter().any(|x| x == "--model"), "{a:?}");
        std::env::remove_var("SPAR_PI_MODEL");
    }

    #[test]
    fn thinking_maps_every_accepted_rung() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        for (level, want) in [
            (EffortLevel::None, "off"),
            (EffortLevel::Minimal, "minimal"),
            (EffortLevel::Low, "low"),
            (EffortLevel::Medium, "medium"),
            (EffortLevel::High, "high"),
            (EffortLevel::Xhigh, "xhigh"),
            (EffortLevel::Max, "max"),
        ] {
            let mut o = opts("x", None);
            o.effort = Some(level);
            let a = args(&PiAdapter.build_headless(Path::new("pi"), &o));
            assert_eq!(flag(&a, "--thinking").as_deref(), Some(want), "{level}");
            let i = args(&PiAdapter.build_interactive(Path::new("pi"), &o));
            assert_eq!(a, i, "the pane runs the headless stream");
        }
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("x", None)));
        assert!(!a.iter().any(|x| x == "--thinking"), "{a:?}");
    }

    #[test]
    fn prompt_starting_with_a_dash_stays_literal() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        let a = args(&PiAdapter.build_headless(Path::new("pi"), &opts("- list", None)));
        assert_eq!(&a[a.len() - 2..], ["--", "- list"]);
    }

    #[test]
    fn readiness_probe_checks_the_routed_provider() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_PI_MODEL");
        assert_eq!(
            PiAdapter.readiness_probe().unwrap(),
            vec![
                "auth",
                "check",
                "--provider",
                "openrouter",
                "--json",
                "--no-refresh"
            ]
        );
        std::env::set_var("SPAR_PI_MODEL", "claude-sonnet-4-5");
        assert_eq!(
            PiAdapter.readiness_probe().unwrap(),
            vec![
                "auth",
                "check",
                "--model",
                "claude-sonnet-4-5",
                "--json",
                "--no-refresh"
            ]
        );
        std::env::remove_var("SPAR_PI_MODEL");
    }

    #[test]
    fn spar_minted_session_ids_fit_pi_charset() {
        let id = crate::session_id::derive("8506ddde", "impl", 1);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
        assert!(PiAdapter.capabilities().assigns_session_id);
    }
}
