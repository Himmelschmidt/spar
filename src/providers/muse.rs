use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

/// Model override (`--model`). spar's per-slot model (`--select` or a `cli:muse@<model>`
/// ref) wins; otherwise `SPAR_MUSE_MODEL`; otherwise none, so muse's own
/// `settings.json` picks the model (currently `muse-spark-1.2-contributor`). Leaving the
/// no-model case to muse keeps the data-sharing tier a single decision on this box
/// rather than something spar hardcodes into every repo it touches.
fn muse_model(opts: &SpawnOpts) -> Option<String> {
    opts.model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("SPAR_MUSE_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Meta reasoning effort (`--reasoning-effort`): none|minimal|low|medium|high|xhigh|ultra.
/// spar has no per-role effort knob, so this is env-only; unset leaves muse's default (high).
fn muse_reasoning_effort() -> Option<String> {
    std::env::var("SPAR_MUSE_REASONING_EFFORT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub struct MuseAdapter;

impl ProviderAdapter for MuseAdapter {
    fn name(&self) -> &'static str {
        "muse"
    }

    // `muse exec --json` emits an event-envelope JSONL (`payload_type` + `stream`) which
    // the stream coalescer renders, but it carries **no** token usage. Usage lands only
    // in muse's session log, which `muse_telemetry` sums after the slot exits. No
    // turn-boundary inject channel and no presence stream are wired, so messages wait for
    // the next turn and presence degrades to the process/output heuristic. muse does ship
    // `session-message send|serve` over a unix socket, which is a real turn-boundary
    // channel; wiring it would make this adapter first-class later.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::None
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::None
    }

    fn binary_names(&self) -> &[&'static str] {
        &["muse"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            // Only `muse exec` (headless) is verified; interactive TUI takeover is not.
            interactive: false,
            // `muse resume` exists but spar drives no adapter that way.
            resume: false,
            skip_permissions: true,
            // `--yolo` turns muse's own sandbox off; the worktree is the boundary,
            // matching the other adapters.
            native_sandbox: false,
        }
    }

    fn permission_args(&self, policy: TrustPolicy) -> Vec<String> {
        match policy {
            // Trust the workspace (loads its skills/rules), no approval prompts, no sandbox.
            TrustPolicy::FullAuto => vec!["--yolo".into()],
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // `muse exec [flags] [PROMPT]`. spar spawns detached with null stdin, so
        // `--user-input-auto-resolve` matters: without it a `request_user_input` call
        // hangs the slot until the wall-clock timeout instead of being cancelled.
        let mut cmd = Command::new(bin);
        cmd.arg("exec");
        cmd.arg("--json");
        cmd.arg("--user-input-auto-resolve");
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = muse_model(opts) {
            cmd.arg("--model").arg(m);
        }
        if let Some(e) = muse_reasoning_effort() {
            cmd.arg("--reasoning-effort").arg(e);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        // `--prompt-file` avoids the arg-length and leading-dash hazards of a positional
        // prompt. Every slot call site fills `prompt_file` with the same bytes it puts in
        // `prompt`, so the file wins whenever there is one.
        match &opts.prompt_file {
            Some(pf) => {
                cmd.arg("--prompt-file").arg(pf);
            }
            None => {
                cmd.arg("--");
                cmd.arg(&opts.prompt);
            }
        }
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // muse has no wired interactive-takeover mode. If a run is forced onto the tmux
        // backend, run the same headless command in the pane so full-auto is preserved:
        // watchable, not takeover-able (capabilities().interactive is false).
        self.build_headless(bin, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::command_to_parts;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // Serializes the tests that mutate SPAR_MUSE_* process env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn opts(prompt: &str, model: Option<&str>) -> SpawnOpts {
        SpawnOpts {
            prompt: prompt.into(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            model: model.map(Into::into),
            timeout_secs: None,
        }
    }

    fn dash_val(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    }

    fn clear_env() {
        std::env::remove_var("SPAR_MUSE_MODEL");
        std::env::remove_var("SPAR_MUSE_REASONING_EFFORT");
    }

    #[test]
    fn headless_shape_and_prompt_last() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let cmd = MuseAdapter.build_headless(Path::new("muse"), &opts("do the thing", None));
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--user-input-auto-resolve"));
        assert!(args.iter().any(|a| a == "--yolo"));
        assert_eq!(args.last().map(String::as_str), Some("do the thing"));
        let di = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(
            di,
            args.len() - 2,
            "-- must sit just before the prompt: {args:?}"
        );
    }

    #[test]
    fn prompt_file_replaces_positional_prompt() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let mut o = opts("", None);
        o.prompt_file = Some(PathBuf::from("/run/prompts/slot.md"));
        let (_, args) = command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &o));
        assert_eq!(
            dash_val(&args, "--prompt-file").as_deref(),
            Some("/run/prompts/slot.md")
        );
        assert!(
            !args.iter().any(|a| a == "--"),
            "prompt-file form needs no separator: {args:?}"
        );
    }

    #[test]
    fn prompt_file_wins_over_the_inline_copy() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        // Slots set both to the same bytes; the file form avoids the arg-length limit.
        let mut o = opts("inline", None);
        o.prompt_file = Some(PathBuf::from("/run/prompts/slot.md"));
        let (_, args) = command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &o));
        assert_eq!(
            dash_val(&args, "--prompt-file").as_deref(),
            Some("/run/prompts/slot.md")
        );
        assert!(!args.iter().any(|a| a == "inline"));
    }

    #[test]
    fn model_from_opts_precedes_prompt() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let cmd = MuseAdapter.build_headless(
            Path::new("muse"),
            &opts("go", Some("muse-spark-1.2-contributor")),
        );
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(
            dash_val(&args, "--model").as_deref(),
            Some("muse-spark-1.2-contributor")
        );
        let mi = args.iter().position(|a| a == "--model").unwrap();
        let pi = args.iter().position(|a| a == "go").expect("prompt present");
        assert!(mi < pi, "model must precede positional prompt: {args:?}");
    }

    #[test]
    fn no_model_omits_flag_so_muse_settings_decide() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let (_, args) =
            command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &opts("x", None)));
        assert!(
            !args.iter().any(|a| a == "--model"),
            "no model -> no --model: {args:?}"
        );
    }

    #[test]
    fn model_env_fallback_and_opts_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("SPAR_MUSE_MODEL", "muse-spark-1.2");
        let (_, a) =
            command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &opts("x", None)));
        assert_eq!(dash_val(&a, "--model").as_deref(), Some("muse-spark-1.2"));

        let (_, a) = command_to_parts(&MuseAdapter.build_headless(
            Path::new("muse"),
            &opts("x", Some("muse-spark-1.2-contributor")),
        ));
        assert_eq!(
            dash_val(&a, "--model").as_deref(),
            Some("muse-spark-1.2-contributor")
        );
        clear_env();
    }

    #[test]
    fn reasoning_effort_is_env_only() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let (_, a) =
            command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &opts("x", None)));
        assert!(!a.iter().any(|x| x == "--reasoning-effort"));

        std::env::set_var("SPAR_MUSE_REASONING_EFFORT", "xhigh");
        let (_, a) =
            command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &opts("x", None)));
        assert_eq!(dash_val(&a, "--reasoning-effort").as_deref(), Some("xhigh"));
        clear_env();
    }

    #[test]
    fn prompt_policy_omits_yolo() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let mut o = opts("x", None);
        o.trust = TrustPolicy::Prompt;
        let (_, args) = command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &o));
        assert!(!args.iter().any(|a| a == "--yolo"));
    }
}
