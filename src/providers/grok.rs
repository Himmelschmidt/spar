use super::{Capabilities, ProviderAdapter, SpawnOpts, TrustPolicy};
use std::path::Path;
use std::process::Command;

pub struct GrokAdapter;

impl ProviderAdapter for GrokAdapter {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn binary_names(&self) -> &[&'static str] {
        &["grok"]
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

    #[test]
    fn headless_prompt_file_not_double_single() {
        let opts = SpawnOpts {
            prompt: "hi".into(),
            prompt_file: Some(PathBuf::from("/tmp/p.md")),
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: None,
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
        };
        let cmd = GrokAdapter.build_headless(Path::new("grok"), &opts);
        let (_, args) = command_to_parts(&cmd);
        let i = args.iter().position(|a| a == "--single").expect("--single");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("do the thing"));
        assert!(!args.iter().any(|a| a == "-p"));
    }
}
