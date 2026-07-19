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
        Ok(p) => Some(p),
        Err(_) => Some(DEFAULT_CODEX_PROFILE.to_string()),
    }
}

/// Model override (`-m`). spar's per-slot model (`--select`) wins; otherwise
/// `SPAR_CODEX_MODEL`; otherwise none (the profile's default model applies).
fn codex_model(opts: &SpawnOpts) -> Option<String> {
    opts.model.clone().or_else(|| {
        std::env::var("SPAR_CODEX_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
    })
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
        if let Some(p) = codex_profile() {
            cmd.arg("-p").arg(p);
        }
        if let Some(m) = codex_model(opts) {
            cmd.arg("-m").arg(m);
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
        cmd.arg(prompt);
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        let mut cmd = Command::new(bin);
        if let Some(p) = codex_profile() {
            cmd.arg("-p").arg(p);
        }
        if let Some(m) = codex_model(opts) {
            cmd.arg("-m").arg(m);
        }
        if !opts.prompt.is_empty() {
            cmd.arg(&opts.prompt);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.current_dir(&opts.cwd);
        cmd
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
        // Structural flags are env-independent; profile value is covered separately.
        let cmd = CodexAdapter.build_headless(Path::new("codex"), &opts("do the thing", None));
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
        assert_eq!(args.last().map(String::as_str), Some("do the thing"));
    }

    #[test]
    fn model_from_opts_precedes_prompt() {
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

        // SPAR_CODEX_PROFILE picks a different backend bundle; SPAR_CODEX_MODEL fills -m.
        std::env::set_var("SPAR_CODEX_PROFILE", "gpt");
        std::env::set_var("SPAR_CODEX_MODEL", "x-ai/grok-4");
        let (_, a) =
            command_to_parts(&CodexAdapter.build_headless(Path::new("codex"), &opts("x", None)));
        assert_eq!(dash_val(&a, "-p").as_deref(), Some("gpt"));
        assert_eq!(dash_val(&a, "-m").as_deref(), Some("x-ai/grok-4"));

        // opts.model still wins over SPAR_CODEX_MODEL.
        let (_, a) = command_to_parts(
            &CodexAdapter
                .build_headless(Path::new("codex"), &opts("x", Some("meta/muse-spark-1.1"))),
        );
        assert_eq!(dash_val(&a, "-m").as_deref(), Some("meta/muse-spark-1.1"));

        // Set-but-empty profile -> omit -p entirely (codex's own config default backend).
        std::env::set_var("SPAR_CODEX_PROFILE", "");
        std::env::remove_var("SPAR_CODEX_MODEL");
        let (_, a) =
            command_to_parts(&CodexAdapter.build_headless(Path::new("codex"), &opts("x", None)));
        assert!(!a.iter().any(|x| x == "-p"));

        std::env::remove_var("SPAR_CODEX_PROFILE");
    }
}
