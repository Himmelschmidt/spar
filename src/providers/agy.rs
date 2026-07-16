use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

pub struct AgyAdapter;

impl ProviderAdapter for AgyAdapter {
    fn name(&self) -> &'static str {
        "agy"
    }

    // No idle-injection and no structured event stream (verified against agy 1.1.1:
    // no `hooks` subcommand; Stop is notify-only). Messages wait for the next turn and
    // presence is degraded to the process/output heuristic.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::None
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }

    fn binary_names(&self) -> &[&'static str] {
        &["agy"]
    }

    fn version_args(&self) -> &[&'static str] {
        &["--help"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            interactive: true,
            resume: true,
            skip_permissions: true,
            native_sandbox: true,
        }
    }

    fn permission_args(&self, policy: TrustPolicy) -> Vec<String> {
        match policy {
            TrustPolicy::FullAuto => vec!["--dangerously-skip-permissions".into()],
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // agy uses Go's `flag` package: `-p`/`--print`/`--prompt` is a *string-valued*
        // flag whose value IS the prompt (not a boolean with a positional), and there is
        // no `--prompt-file`. Go's `flag` stops parsing at the first positional, so a bare
        // positional would orphan every flag after it. Emit our own flags, then the prompt
        // as `--print <value>`, and put caller `extra_args` LAST — a stray positional there
        // is then a harmless trailing arg, not something that swallows `--print`.
        let mut cmd = Command::new(bin);
        // Default print timeout. Go durations require a unit ("1800" alone is rejected as
        // `missing unit in duration`). Override via extra_args if needed.
        cmd.arg("--print-timeout").arg("1800s");
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = &opts.model {
            cmd.arg("--model").arg(m);
        }
        let prompt = if !opts.prompt.is_empty() {
            opts.prompt.clone()
        } else if let Some(pf) = &opts.prompt_file {
            std::fs::read_to_string(pf)
                .unwrap_or_else(|_| format!("Read and follow instructions in {}", pf.display()))
        } else {
            String::new()
        };
        cmd.arg("--print").arg(prompt);
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // Same flag-package semantics: the initial prompt rides `--prompt-interactive`
        // (a value flag), never a positional; `extra_args` come after it (see build_headless).
        let mut cmd = Command::new(bin);
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = &opts.model {
            cmd.arg("--model").arg(m);
        }
        if !opts.prompt.is_empty() {
            cmd.arg("--prompt-interactive").arg(&opts.prompt);
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

    fn opts(prompt: &str, file: Option<&str>, model: Option<&str>) -> SpawnOpts {
        opts_with(prompt, file, model, vec![])
    }

    fn opts_with(
        prompt: &str,
        file: Option<&str>,
        model: Option<&str>,
        extra_args: Vec<String>,
    ) -> SpawnOpts {
        SpawnOpts {
            prompt: prompt.into(),
            prompt_file: file.map(PathBuf::from),
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args,
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn headless_prompt_is_value_of_print() {
        let cmd = AgyAdapter.build_headless(Path::new("agy"), &opts("review this", None, None));
        let (_, args) = command_to_parts(&cmd);
        let i = args.iter().position(|a| a == "--print").expect("--print");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("review this"));
    }

    #[test]
    fn headless_extra_args_positional_cannot_orphan_prompt() {
        // A positional in extra_args must land AFTER `--print <prompt>`, so Go's flag
        // parser has already bound the prompt before it stops at the positional.
        let cmd = AgyAdapter.build_headless(
            Path::new("agy"),
            &opts_with("review this", None, None, vec!["a-positional".into()]),
        );
        let (_, args) = command_to_parts(&cmd);
        let p = args.iter().position(|a| a == "--print").expect("--print");
        assert_eq!(args.get(p + 1).map(String::as_str), Some("review this"));
        let pos = args.iter().position(|a| a == "a-positional").unwrap();
        assert!(
            pos > p,
            "extra_args positional must follow --print: {args:?}"
        );
    }

    #[test]
    fn headless_flags_precede_prompt_and_timeout_has_unit() {
        let cmd = AgyAdapter.build_headless(Path::new("agy"), &opts("hi", None, Some("gemini")));
        let (_, args) = command_to_parts(&cmd);
        let t = args
            .iter()
            .position(|a| a == "--print-timeout")
            .expect("--print-timeout");
        assert_eq!(args.get(t + 1).map(String::as_str), Some("1800s"));
        // permission + model flags land before `--print`, never after (Go flag stops at
        // the first positional; the prompt value must not orphan them).
        let p = args.iter().position(|a| a == "--print").unwrap();
        let skip = args
            .iter()
            .position(|a| a == "--dangerously-skip-permissions")
            .unwrap();
        let model = args.iter().position(|a| a == "--model").unwrap();
        assert!(
            skip < p && model < p,
            "flags must precede --print: {args:?}"
        );
    }

    #[test]
    fn headless_reads_prompt_file_when_prompt_empty() {
        let dir = tempfile::tempdir().unwrap();
        let pf = dir.path().join("p.md");
        std::fs::write(&pf, "# Role: reviewer\nfindings").unwrap();
        let cmd = AgyAdapter.build_headless(Path::new("agy"), &opts("", pf.to_str(), None));
        let (_, args) = command_to_parts(&cmd);
        let i = args.iter().position(|a| a == "--print").unwrap();
        assert_eq!(
            args.get(i + 1).map(String::as_str),
            Some("# Role: reviewer\nfindings")
        );
    }

    #[test]
    fn interactive_prompt_uses_value_flag_not_positional() {
        let cmd = AgyAdapter.build_interactive(Path::new("agy"), &opts("start here", None, None));
        let (_, args) = command_to_parts(&cmd);
        let i = args
            .iter()
            .position(|a| a == "--prompt-interactive")
            .expect("--prompt-interactive");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("start here"));
    }
}
