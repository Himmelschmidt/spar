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

/// The trailing prompt positional: inline `opts.prompt` if set, else the prompt file's
/// contents, else empty. stdin is null (spar spawns detached), so codex only ever sees
/// the prompt from this argument.
fn resolved_prompt(opts: &SpawnOpts) -> String {
    if !opts.prompt.is_empty() {
        opts.prompt.clone()
    } else if let Some(pf) = &opts.prompt_file {
        std::fs::read_to_string(pf)
            .unwrap_or_else(|_| format!("Read and follow instructions in {}", pf.display()))
    } else {
        String::new()
    }
}

pub struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    // `codex exec --json` emits JSONL (thread/turn/item events with turn.completed
    // usage) which the stream coalescer parses for tokens, and its first line names the
    // thread id (`thread.started`), captured into `StreamStats::session_id`. That id is
    // spar's handle for a `codex queue --thread <id> --message <text>` push. It does not
    // land inside the dispatch it was pushed against — `codex exec` is single-turn and
    // exits right after its one assigned task, so a queued follow-up turn is aborted
    // before the model sees it (verified against codex 0.152.0; see `codex_queue_push`'s
    // doc comment in `delivery.rs`) — so the seam always also writes the poll file. What
    // makes the push a real channel rather than a no-op is `build_resume` below: a
    // message queued against a thread *is* delivered, folded into the same turn as the
    // next prompt, the next time that thread is resumed (verified live: a message queued
    // to an idle thread surfaced in the model's reply on the following `codex exec resume`
    // call). So the push's payoff is at the *next round's* turn boundary, not the current
    // one. Codex still has no presence stream, so presence still degrades to the
    // process/output heuristic.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::NativeQueuePollFallback
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
            // `codex exec resume <SESSION_ID> [PROMPT]` (also `--last`) is wired: see
            // `build_resume`. `executor::execute_prepared` calls it in place of a cold
            // `build_headless` dispatch whenever a prior round of the same slot captured
            // a thread id (DECISIONS.md O63). This reopens the vendor's own transcript for
            // the resumed thread, which is exactly the cost O52 measured and chose a
            // compact carry-forward brief over for the general fix-round case; O63 records
            // the tradeoff for codex specifically rather than reversing O52's default.
            resume: true,
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
        // `--` ends option parsing so a prompt starting with `-` (or matching a
        // `codex exec` subcommand like `review`/`resume`) is taken literally.
        cmd.arg("--");
        cmd.arg(resolved_prompt(opts));
        cmd.current_dir(&opts.cwd);
        cmd
    }

    // `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]` (verified against codex
    // 0.152.0 — `codex exec resume --help`). It takes the same `--json` /
    // `--skip-git-repo-check` / permission / `-m` flags as `exec`, but has no
    // `-p/--profile`: a resumed thread already carries whatever profile started it, so
    // `codex_profile()` is not applied here (it would also be rejected as an unknown
    // flag). `session_id` is the thread id a prior round's `build_headless` (or an
    // earlier resume) captured for this same slot.
    fn build_resume(&self, bin: &Path, opts: &SpawnOpts, session_id: &str) -> Option<Command> {
        let mut cmd = Command::new(bin);
        cmd.arg("exec");
        cmd.arg("resume");
        cmd.arg("--json");
        cmd.arg("--skip-git-repo-check");
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if let Some(m) = codex_model(opts) {
            for a in model_args(&m) {
                cmd.arg(a);
            }
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.arg(session_id);
        cmd.arg("--");
        cmd.arg(resolved_prompt(opts));
        cmd.current_dir(&opts.cwd);
        Some(cmd)
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
            timeout_secs: None,
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

    #[test]
    fn native_queue_poll_fallback_delivery_and_resume_capability() {
        assert_eq!(
            CodexAdapter.delivery_strategy(),
            DeliveryStrategy::NativeQueuePollFallback
        );
        // `build_resume` is wired (DECISIONS.md O63), so the reported capability must
        // say so.
        assert!(CodexAdapter.capabilities().resume);
    }

    #[test]
    fn build_resume_shape() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SPAR_CODEX_PROFILE");
        std::env::remove_var("SPAR_CODEX_MODEL");
        let cmd = CodexAdapter
            .build_resume(
                Path::new("codex"),
                &opts("keep going", None),
                "01a07c85-9f8e-7ee1-b429-4fc000e90add",
            )
            .expect("codex supports resume");
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(&args[..2], ["exec", "resume"]);
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args
            .iter()
            .any(|a| a == "01a07c85-9f8e-7ee1-b429-4fc000e90add"));
        // No profile flag: `codex exec resume` has no `-p/--profile`.
        assert!(!args.iter().any(|a| a == "-p"));
        // Prompt is still the final positional, preceded by `--`.
        assert_eq!(args.last().map(String::as_str), Some("keep going"));
        let di = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(di, args.len() - 2);
        // Session id precedes the `--` / prompt.
        let si = args
            .iter()
            .position(|a| a == "01a07c85-9f8e-7ee1-b429-4fc000e90add")
            .unwrap();
        assert!(si < di);
    }

    #[test]
    fn build_resume_carries_model_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let cmd = CodexAdapter.build_resume(
            Path::new("codex"),
            &opts("x", Some("meta/muse-spark-1.1")),
            "thread-1",
        );
        let (_, args) = command_to_parts(&cmd.unwrap());
        let mi = args.iter().position(|a| a == "-m").expect("-m present");
        assert_eq!(
            args.get(mi + 1).map(String::as_str),
            Some("meta/muse-spark-1.1")
        );
    }
}
