use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

/// Default codex profile: layers the OpenRouter provider + Muse Spark default
/// (`$CODEX_HOME/muse.config.toml`). A codex profile is a (backend, model) bundle.
const DEFAULT_CODEX_PROFILE: &str = "muse";

/// Which codex profile (`-p`) to run — a profile is codex's own (backend, model) unit.
/// `SPAR_CODEX_PROFILE` unset → the `muse` default; set-but-empty → omit `-p` so codex
/// falls back to its own config default (e.g. plain OpenAI).
fn codex_profile() -> Option<String> {
    match std::env::var("SPAR_CODEX_PROFILE") {
        Ok(p) if p.trim().is_empty() => None,
        Ok(p) => Some(p.trim().to_string()),
        Err(_) => Some(DEFAULT_CODEX_PROFILE.to_string()),
    }
}

/// Model override (`-m`). spar's per-slot model (`--select` or a `cli:codex@<model>` ref)
/// wins; otherwise `SPAR_CODEX_MODEL`; otherwise none (the profile's default model applies).
/// Empty/whitespace values are ignored so we never emit `-m ""`.
fn codex_model(opts: &SpawnOpts) -> Option<String> {
    opts.model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("SPAR_CODEX_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Flags that select a model. A `/` marks an OpenRouter slug (`openai/gpt-4o-mini`,
/// `tencent/hy3:free`) and routes explicitly through the openrouter provider; a bare
/// model (`gpt-5`) goes to codex's own default provider.
fn model_args(model: &str) -> Vec<String> {
    if model.contains('/') {
        vec![
            "-c".into(),
            "model_provider=openrouter".into(),
            "-m".into(),
            model.into(),
        ]
    } else {
        vec!["-m".into(), model.into()]
    }
}

pub struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    // `codex exec --json` emits JSONL (thread/turn/item events with turn.completed
    // usage) which the stream coalescer parses for tokens. No turn-boundary inject
    // channel and no presence stream, so messages wait for the next turn and
    // presence degrades to the process/output heuristic.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::None
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }

    fn binary_names(&self) -> &[&'static str] {
        &["codex"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            // Only `codex exec` (headless) is verified; interactive TUI takeover is not.
            interactive: false,
            resume: false,
            skip_permissions: true,
            // FullAuto bypasses codex's own sandbox (the worktree is the boundary,
            // matching the other adapters), so we do not rely on a native sandbox.
            native_sandbox: false,
        }
    }

    fn permission_args(&self, policy: TrustPolicy) -> Vec<String> {
        match policy {
            // Match the other adapters: run unsandboxed with no approval prompts.
            TrustPolicy::FullAuto => vec!["--dangerously-bypass-approvals-and-sandbox".into()],
            // Fall back to codex config defaults (approval on-failure, workspace-write).
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // `codex exec [flags] [PROMPT]` — prompt is the trailing positional. stdin is
        // null (spar spawns detached), so codex takes the prompt from the argument.
        let mut cmd = Command::new(bin);
        cmd.arg("exec");
        cmd.arg("--json");
        cmd.arg("--skip-git-repo-check");
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        // An explicit model is self-describing (it names its own provider), so it
        // supersedes the profile; the profile is only the no-model default.
        match codex_model(opts) {
            Some(m) => {
                for a in model_args(&m) {
                    cmd.arg(a);
                }
            }
            None => {
                if let Some(p) = codex_profile() {
                    cmd.arg("-p").arg(p);
                }
            }
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
        // `--` ends option parsing so a prompt starting with `-` (or matching a
        // `codex exec` subcommand like `review`/`resume`) is taken literally.
        cmd.arg("--");
        cmd.arg(prompt);
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // codex has no wired interactive-takeover mode. If a run is forced onto the
        // tmux backend (`--backend tmux`), run the same headless `exec --json`
        // command in the pane so full-auto + token tracking are preserved — it is
        // watchable, just not takeover-able (capabilities().interactive is false).
        self.build_headless(bin, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::command_to_parts;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // Serializes the tests that mutate SPAR_CODEX_* process env.
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

    #[test]
    fn headless_shape_and_prompt_last() {
        // Lock: build_headless reads SPAR_CODEX_* env, which another test mutates.
        let _guard = ENV_LOCK.lock().unwrap();
        // Structural flags are env-independent; profile value is covered separately.
        let cmd = CodexAdapter.build_headless(Path::new("codex"), &opts("do the thing", None));
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
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
    fn model_from_opts_precedes_prompt() {
        let _guard = ENV_LOCK.lock().unwrap();
        // opts.model (from --select) wins regardless of ambient env.
        let cmd = CodexAdapter
            .build_headless(Path::new("codex"), &opts("go", Some("meta/muse-spark-1.1")));
        let (_, args) = command_to_parts(&cmd);
        let mi = args.iter().position(|a| a == "-m").expect("-m present");
        assert_eq!(
            args.get(mi + 1).map(String::as_str),
            Some("meta/muse-spark-1.1")
        );
        let pi = args.iter().position(|a| a == "go").expect("prompt present");
        assert!(mi < pi, "model must precede positional prompt: {args:?}");
    }

    #[test]
    fn prompt_policy_omits_bypass() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut o = opts("x", None);
        o.trust = TrustPolicy::Prompt;
        let cmd = CodexAdapter.build_headless(Path::new("codex"), &o);
        let (_, args) = command_to_parts(&cmd);
        assert!(!args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn profile_and_model_env_selection() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dash_val = |args: &[String], flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1).cloned())
        };

        // Unset -> the `muse` default profile, no -m (profile's own model).
        std::env::remove_var("SPAR_CODEX_PROFILE");
        std::env::remove_var("SPAR_CODEX_MODEL");
        let (_, a) =
            command_to_parts(&CodexAdapter.build_headless(Path::new("codex"), &opts("x", None)));
        assert_eq!(dash_val(&a, "-p").as_deref(), Some("muse"));
        assert!(!a.iter().any(|x| x == "-m"));

        // SPAR_CODEX_MODEL fills -m; an explicit model supersedes the profile, so -p is gone.
        std::env::set_var("SPAR_CODEX_PROFILE", "gpt");
        std::env::set_var("SPAR_CODEX_MODEL", "x-ai/grok-4");
        let (_, a) =
            command_to_parts(&CodexAdapter.build_headless(Path::new("codex"), &opts("x", None)));
        assert!(
            !a.iter().any(|x| x == "-p"),
            "explicit model omits the profile"
        );
        assert_eq!(dash_val(&a, "-m").as_deref(), Some("x-ai/grok-4"));

        // opts.model still wins over SPAR_CODEX_MODEL.
        let (_, a) = command_to_parts(
            &CodexAdapter
                .build_headless(Path::new("codex"), &opts("x", Some("meta/muse-spark-1.1"))),
        );
        assert_eq!(dash_val(&a, "-m").as_deref(), Some("meta/muse-spark-1.1"));

        std::env::remove_var("SPAR_CODEX_PROFILE");
        std::env::remove_var("SPAR_CODEX_MODEL");
    }

    #[test]
    fn slug_model_routes_to_openrouter_and_omits_profile() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_CODEX_PROFILE");
        std::env::remove_var("SPAR_CODEX_MODEL");
        let (_, a) = command_to_parts(
            &CodexAdapter.build_headless(Path::new("codex"), &opts("x", Some("tencent/hy3:free"))),
        );
        // -c model_provider=openrouter -m <slug>, and NO profile.
        let ci = a.iter().position(|x| x == "-c").expect("-c present");
        assert_eq!(
            a.get(ci + 1).map(String::as_str),
            Some("model_provider=openrouter")
        );
        let mi = a.iter().position(|x| x == "-m").expect("-m present");
        assert_eq!(a.get(mi + 1).map(String::as_str), Some("tencent/hy3:free"));
        assert!(mi > ci, "-c must precede -m");
        assert!(!a.iter().any(|x| x == "-p"), "slug model omits the profile");
        std::env::remove_var("SPAR_CODEX_PROFILE");
    }

    #[test]
    fn bare_model_omits_provider_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_CODEX_PROFILE");
        std::env::remove_var("SPAR_CODEX_MODEL");
        let (_, a) = command_to_parts(
            &CodexAdapter.build_headless(Path::new("codex"), &opts("x", Some("gpt-5"))),
        );
        let mi = a.iter().position(|x| x == "-m").expect("-m present");
        assert_eq!(a.get(mi + 1).map(String::as_str), Some("gpt-5"));
        assert!(
            !a.iter().any(|x| x == "model_provider=openrouter"),
            "bare model must not force the openrouter provider"
        );
        std::env::remove_var("SPAR_CODEX_PROFILE");
    }

    #[test]
    fn no_model_uses_default_profile() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_CODEX_PROFILE");
        std::env::remove_var("SPAR_CODEX_MODEL");
        let (_, a) =
            command_to_parts(&CodexAdapter.build_headless(Path::new("codex"), &opts("x", None)));
        assert!(a.windows(2).any(|w| w[0] == "-p" && w[1] == "muse"));
        assert!(!a.iter().any(|x| x == "-m"));
    }
}
