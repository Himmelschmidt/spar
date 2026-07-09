use super::{Capabilities, ProviderAdapter, SpawnOpts, TrustPolicy};
use std::path::Path;
use std::process::Command;

pub struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn binary_names(&self) -> &[&'static str] {
        &["claude"]
    }

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
            TrustPolicy::FullAuto => vec![
                "--dangerously-skip-permissions".into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
            ],
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        let mut cmd = Command::new(bin);
        cmd.arg("-p");
        // Prefer full prompt text; if only a prompt file was provided, read it.
        let prompt = if !opts.prompt.is_empty() {
            opts.prompt.clone()
        } else if let Some(pf) = &opts.prompt_file {
            std::fs::read_to_string(pf)
                .unwrap_or_else(|_| format!("Read and follow instructions in {}", pf.display()))
        } else {
            String::new()
        };
        cmd.arg(prompt);
        // stream-json emits events as they happen so spar can tail the slot log live
        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--verbose");
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
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

    #[test]
    fn headless_passes_prompt_not_at_path() {
        let opts = SpawnOpts {
            prompt: "implement feature".into(),
            prompt_file: Some(PathBuf::from("/tmp/p.md")),
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::Prompt,
            extra_args: vec![],
        };
        let cmd = ClaudeAdapter.build_headless(Path::new("claude"), &opts);
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("-p"));
        assert_eq!(args.get(1).map(String::as_str), Some("implement feature"));
        assert!(args.iter().any(|a| a == "stream-json"));
        assert!(!args.iter().any(|a| a.starts_with('@')));
    }
}
