use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

pub struct GrokAdapter;

impl ProviderAdapter for GrokAdapter {
    fn name(&self) -> &'static str {
        "grok"
    }

    // Turn-boundary injection rides the Stop hook: `{"decision":"block","reason":…}`
    // feeds the reason back as a user message and runs another round in the same
    // turn, the same contract as Claude (grok docs ch. 10, Stop Decision Control).
    // Verified end to end against grok 1.0.34: a marker-guarded Stop hook blocked
    // once, the model answered its first prompt, then acted on the reason inside
    // the same dispatch, which ended `stopReason: end_turn` with `num_turns: 2`.
    // `presence::wire` derives the injecting Stop hook from this.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::StopHookInject
    }

    // Grok's native project hook location (same shape muse uses with
    // `.muse/hooks.json`). The Claude-compat source (`.claude/settings.json`) is a
    // dead letter here: this box's `~/.grok/config.toml` carries
    // `[compat.claude] hooks = false`, and project-scope hooks additionally require
    // folder trust, which no fresh spar worktree ever has.
    fn presence_source(&self) -> PresenceSource {
        PresenceSource::Hooks
    }

    fn hook_file_rel(&self) -> &'static str {
        ".grok/hooks/spar.json"
    }

    // Ungate the worktree's project scope for the dispatch: folder trust loads the
    // project hook file above plus the repo's own instructions, skills, and local
    // MCP/LSP servers. An env var rather than `~/.grok/trusted_folders.toml`
    // because spar must not mutate operator state outside the worktree it owns;
    // precedent is muse's `--yolo`, which already trusts the workspace for the run.
    fn extra_env(&self, _opts: &SpawnOpts) -> Vec<(String, String)> {
        vec![("GROK_FOLDER_TRUST".to_string(), "0".to_string())]
    }

    fn binary_names(&self) -> &[&'static str] {
        &["grok"]
    }

    // No readiness probe. Tried: `grok doctor` checks terminal/clipboard/color
    // support only, not auth or config; `grok models` exits 0 even when logged
    // out ("You are not authenticated", verified), so it cannot distinguish a
    // dispatchable box from a broken one; `grok inspect` is per-directory
    // (project trust) and per-worktree checks are out of scope. Default
    // (Unknown) stands.

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            interactive: true,
            resume: true,
            skip_permissions: true,
            native_sandbox: false,
        }
    }

    fn permission_args(&self, policy: TrustPolicy) -> Vec<String> {
        match policy {
            TrustPolicy::FullAuto => vec!["--always-approve".into()],
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // -p / --single is one flag that takes the prompt value; do not pass both.
        let mut cmd = Command::new(bin);
        // streaming-json so slot logs update while the agent runs
        cmd.arg("--output-format").arg("streaming-json");
        if let Some(pf) = &opts.prompt_file {
            cmd.arg("--prompt-file").arg(pf);
        } else {
            cmd.arg("--single").arg(&opts.prompt);
        }
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = &opts.model {
            cmd.arg("--model").arg(m);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        let mut cmd = Command::new(bin);
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = &opts.model {
            cmd.arg("--model").arg(m);
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

    /// Grok reports presence through its native project hook file and is wired for
    /// turn-boundary injection through the Stop hook (file path and trust env probed
    /// live against grok 1.0.25, the Stop-block re-drive against 1.0.34; see O90).
    /// Folder trust for the dispatch comes from the env, never from operator state
    /// outside the worktree.
    #[test]
    fn presence_hooks_and_stop_hook_injection() {
        assert_eq!(
            GrokAdapter.delivery_strategy(),
            DeliveryStrategy::StopHookInject
        );
        assert_eq!(GrokAdapter.presence_source(), PresenceSource::Hooks);
        assert_eq!(GrokAdapter.hook_file_rel(), ".grok/hooks/spar.json");
        let env = GrokAdapter.extra_env(&SpawnOpts {
            prompt: String::new(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: None,
            timeout_secs: None,
        });
        assert!(
            env.iter()
                .any(|(k, v)| k == "GROK_FOLDER_TRUST" && v == "0"),
            "grok dispatches must carry folder trust: {env:?}"
        );
    }

    #[test]
    fn headless_prompt_file_not_double_single() {
        let opts = SpawnOpts {
            prompt: "hi".into(),
            prompt_file: Some(PathBuf::from("/tmp/p.md")),
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: None,
            timeout_secs: None,
        };
        let cmd = GrokAdapter.build_headless(Path::new("grok"), &opts);
        let (_, args) = command_to_parts(&cmd);
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-p" && w[1].starts_with('-')),
            "flag must not be value of -p: {args:?}"
        );
        assert!(args.iter().any(|a| a == "--prompt-file"));
        assert!(!args.iter().any(|a| a == "-p"));
        assert!(!args.iter().any(|a| a == "--single"));
    }

    #[test]
    fn headless_inline_uses_single_with_prompt() {
        let opts = SpawnOpts {
            prompt: "do the thing".into(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: None,
            timeout_secs: None,
        };
        let cmd = GrokAdapter.build_headless(Path::new("grok"), &opts);
        let (_, args) = command_to_parts(&cmd);
        let i = args.iter().position(|a| a == "--single").expect("--single");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("do the thing"));
        assert!(!args.iter().any(|a| a == "-p"));
    }
}
