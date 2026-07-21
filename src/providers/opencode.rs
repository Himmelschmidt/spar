use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

/// Model override (`-m`). spar's per-slot model (`--select` or a `cli:opencode@<model>`
/// ref) wins; otherwise `SPAR_OPENCODE_MODEL`; otherwise none (opencode's own config
/// default applies). Empty/whitespace values are ignored so we never emit `-m ""`.
fn opencode_model(opts: &SpawnOpts) -> Option<String> {
    opts.model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("SPAR_OPENCODE_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Map a model id to opencode's `-m provider/model` form, defaulting to OpenRouter.
/// A vendor slug (`meta/muse-spark-1.1`) is prefixed with `openrouter/` so spar drives
/// OpenRouter by default; an explicit `openrouter/…` passes through; a bare word
/// (`gpt-5`) passes through to opencode's own default provider resolution.
fn opencode_model_arg(model: &str) -> String {
    if model.contains('/') && !model.starts_with("openrouter/") {
        format!("openrouter/{model}")
    } else {
        model.to_string()
    }
}

pub struct OpencodeAdapter;

impl ProviderAdapter for OpencodeAdapter {
    fn name(&self) -> &'static str {
        "opencode"
    }

    // `opencode run --format json` emits NDJSON (session events with per-step
    // `part.tokens`) which the stream coalescer parses for tokens. v1 has no
    // turn-boundary inject channel and no presence stream, so messages wait for the
    // next turn and presence degrades to the process/output heuristic. opencode's SSE
    // event stream (`GET /event`: session.idle / tool.execute.* / permission.ask) plus
    // `client.session.prompt()` could make this adapter first-class later
    // (DeliveryStrategy::SdkPrompt / PresenceSource::Sse — already reserved in mod.rs);
    // that is deliberately not wired here.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::None
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }

    fn binary_names(&self) -> &[&'static str] {
        &["opencode"]
    }

    fn version_args(&self) -> &[&'static str] {
        &["--version"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            // Only `opencode run` (headless) is verified; interactive TUI takeover and
            // session resume are not.
            interactive: false,
            resume: false,
            skip_permissions: true,
            // `--dangerously-skip-permissions` edits autonomously; the worktree is the
            // boundary (matching the other adapters), so no native sandbox is relied on.
            native_sandbox: false,
        }
    }

    fn permission_args(&self, policy: TrustPolicy) -> Vec<String> {
        match policy {
            TrustPolicy::FullAuto => vec!["--dangerously-skip-permissions".into()],
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // `opencode run [flags] -- <prompt>` — the prompt is a variadic positional
        // (`message..`), so every flag precedes it and `--` separates it. stdin is null
        // (spar spawns detached); opencode takes the prompt from the argument.
        let mut cmd = Command::new(bin);
        cmd.arg("run");
        cmd.arg("--format").arg("json");
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = opencode_model(opts) {
            cmd.arg("-m").arg(opencode_model_arg(&m));
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
        // `--` ends option parsing so a prompt starting with `-` is taken literally.
        cmd.arg("--");
        cmd.arg(prompt);
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // opencode has no wired interactive-takeover mode. If a run is forced onto the
        // tmux backend, run the same headless `run --format json` command in the pane so
        // full-auto + token tracking are preserved (capabilities().interactive is false).
        self.build_headless(bin, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::command_to_parts;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // Serializes the tests that mutate SPAR_OPENCODE_* process env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn opts(prompt: &str, model: Option<&str>) -> SpawnOpts {
        SpawnOpts {
            prompt: prompt.into(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: model.map(Into::into),
        }
    }

    fn dash_val(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    }

    #[test]
    fn headless_shape_and_prompt_last() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_OPENCODE_MODEL");
        let cmd =
            OpencodeAdapter.build_headless(Path::new("opencode"), &opts("do the thing", None));
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("run"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--format" && w[1] == "json"));
        assert!(args.iter().any(|a| a == "--dangerously-skip-permissions"));
        // Prompt is the final positional, preceded by `--`.
        assert_eq!(args.last().map(String::as_str), Some("do the thing"));
        let di = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(
            di,
            args.len() - 2,
            "-- must sit just before the prompt: {args:?}"
        );
    }

    #[test]
    fn slug_model_prepends_openrouter() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_OPENCODE_MODEL");
        let cmd = OpencodeAdapter.build_headless(
            Path::new("opencode"),
            &opts("go", Some("meta/muse-spark-1.1")),
        );
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(
            dash_val(&args, "-m").as_deref(),
            Some("openrouter/meta/muse-spark-1.1")
        );
        let mi = args.iter().position(|a| a == "-m").expect("-m present");
        let pi = args.iter().position(|a| a == "go").expect("prompt present");
        assert!(mi < pi, "model must precede positional prompt: {args:?}");
    }

    #[test]
    fn explicit_openrouter_model_passes_through() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_OPENCODE_MODEL");
        let cmd = OpencodeAdapter.build_headless(
            Path::new("opencode"),
            &opts("x", Some("openrouter/meta/muse-spark-1.1")),
        );
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(
            dash_val(&args, "-m").as_deref(),
            Some("openrouter/meta/muse-spark-1.1"),
            "already-openrouter model must not be double-prefixed"
        );
    }

    #[test]
    fn bare_model_passes_through() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_OPENCODE_MODEL");
        let cmd = OpencodeAdapter.build_headless(Path::new("opencode"), &opts("x", Some("gpt-5")));
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(dash_val(&args, "-m").as_deref(), Some("gpt-5"));
    }

    #[test]
    fn model_env_fallback_and_opts_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SPAR_OPENCODE_MODEL fills -m when opts.model is absent, and is routed too.
        std::env::set_var("SPAR_OPENCODE_MODEL", "x-ai/grok-4");
        let (_, a) = command_to_parts(
            &OpencodeAdapter.build_headless(Path::new("opencode"), &opts("x", None)),
        );
        assert_eq!(
            dash_val(&a, "-m").as_deref(),
            Some("openrouter/x-ai/grok-4")
        );

        // opts.model still wins over the env.
        let (_, a) = command_to_parts(&OpencodeAdapter.build_headless(
            Path::new("opencode"),
            &opts("x", Some("meta/muse-spark-1.1")),
        ));
        assert_eq!(
            dash_val(&a, "-m").as_deref(),
            Some("openrouter/meta/muse-spark-1.1")
        );
        std::env::remove_var("SPAR_OPENCODE_MODEL");
    }

    #[test]
    fn no_model_omits_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_OPENCODE_MODEL");
        let (_, a) = command_to_parts(
            &OpencodeAdapter.build_headless(Path::new("opencode"), &opts("x", None)),
        );
        assert!(!a.iter().any(|x| x == "-m"), "no model -> no -m: {a:?}");
    }

    #[test]
    fn full_auto_present_prompt_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_OPENCODE_MODEL");
        let (_, a) = command_to_parts(
            &OpencodeAdapter.build_headless(Path::new("opencode"), &opts("x", None)),
        );
        assert!(a.iter().any(|x| x == "--dangerously-skip-permissions"));

        let mut o = opts("x", None);
        o.trust = TrustPolicy::Prompt;
        let (_, a) = command_to_parts(&OpencodeAdapter.build_headless(Path::new("opencode"), &o));
        assert!(!a.iter().any(|x| x == "--dangerously-skip-permissions"));
    }
}
