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

    // No turn-boundary push spar drives (verified against agy 1.2.5: still no
    // `hooks` subcommand), so messages wait for the next turn and presence is
    // degraded to the process/output heuristic. Note 1.2.5 does add a real
    // injection channel spar deliberately does not wire: `--input-format
    // stream-json` reads one NDJSON message per line from stdin and runs a turn
    // for each, which would make `DeliveryStrategy::None` no longer forced.
    // But spar spawns detached with stdin null and never feeds it, so the
    // channel stays unused. `--output-format stream-json` (build_headless below)
    // does give us a structured event stream, but it's parsed for telemetry
    // (StreamCoalescer::handle_agy in process.rs), not a channel for mid-turn
    // injection. Also deliberately not wired, each its own decision: `--effort`,
    // `--json-schema`, `--add-dir`, `--sandbox`, `--conversation`/`--continue`
    // (see `supports_resume`). `cli:agy` is out of both fleets on this machine.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::None
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }

    fn binary_names(&self) -> &[&'static str] {
        &["agy"]
    }

    // No readiness probe. Tried `agy models`: it prints "Fetching available
    // models..." — a network fetch, not a local-state read, so it fails the
    // probe's local-only constraint. Default (Unknown) stands.

    fn version_args(&self) -> &[&'static str] {
        &["--help"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            interactive: true,
            // No `build_resume`: a real implementation would continue via
            // `agy --continue` or `agy --conversation <ID>` (verified against
            // agy 1.2.5). Unimplemented, so the derived flag stays false.
            resume: self.supports_resume(),
            skip_permissions: true,
            native_sandbox: true,
            // Capture-only: agy resumes by `--conversation <ID>` but offers no flag
            // to name a new session, so `SpawnOpts::session_id` is never read here.
            assigns_session_id: false,
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
        // Print timeout = the resolved slot budget so agy runs the full wall clock the
        // orchestrator granted it, not the short built-in default (5m0s on agy 1.2.5)
        // that silently kills long slots. Shave a small margin so agy hits its own
        // timeout and exits cleanly a beat before spar's process backstop (same
        // budget) SIGKILLs it mid-write. Go durations require a unit ("1800" alone
        // is rejected as `missing unit in duration`); fall back to 1800s when no
        // budget was supplied.
        let print_timeout = opts
            .timeout_secs
            .map(|s| format!("{}s", if s > 20 { s - 10 } else { s }))
            .unwrap_or_else(|| "1800s".into());
        cmd.arg("--print-timeout").arg(print_timeout);
        // Structured NDJSON on stdout (verified against agy 1.2.5) so the coalescer can
        // parse real tools/tokens/session id instead of the ~empty plain-text stream.
        cmd.arg("--output-format").arg("stream-json");
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
            session_id: None,
            model: model.map(str::to_string),
            timeout_secs: None,
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
    fn capture_only_adapter_ignores_assigned_session_id() {
        // agy cannot name a new session: the command line must be byte-identical
        // with and without the field set.
        let plain = opts("review this", None, None);
        let mut assigned = plain.clone();
        assigned.session_id = Some("123e4567-e89b-52d3-a456-426614174000".into());
        let (_, a) = command_to_parts(&AgyAdapter.build_headless(Path::new("agy"), &plain));
        let (_, b) = command_to_parts(&AgyAdapter.build_headless(Path::new("agy"), &assigned));
        assert_eq!(a, b);
        assert!(!AgyAdapter.capabilities().assigns_session_id);
        assert!(!AgyAdapter.assigned_session_refused("anything", "abc", Some(1)));
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
        // No budget supplied by this helper → adapter default.
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
    fn headless_requests_stream_json_output() {
        let cmd = AgyAdapter.build_headless(Path::new("agy"), &opts("hi", None, None));
        let (_, args) = command_to_parts(&cmd);
        let i = args
            .iter()
            .position(|a| a == "--output-format")
            .expect("--output-format");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("stream-json"));
        let p = args.iter().position(|a| a == "--print").unwrap();
        assert!(i < p, "--output-format must precede --print: {args:?}");
    }

    #[test]
    fn print_timeout_derives_from_slot_budget() {
        // The resolved slot budget must reach agy's self-timeout, or a long slot dies
        // at the 1800s default regardless of `[timeouts] slot_secs`.
        let mut o = opts("hi", None, None);
        o.timeout_secs = Some(10800);
        let cmd = AgyAdapter.build_headless(Path::new("agy"), &o);
        let (_, args) = command_to_parts(&cmd);
        let t = args
            .iter()
            .position(|a| a == "--print-timeout")
            .expect("--print-timeout");
        // Budget minus a small self-terminate margin (10s) under spar's backstop.
        assert_eq!(args.get(t + 1).map(String::as_str), Some("10790s"));
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
