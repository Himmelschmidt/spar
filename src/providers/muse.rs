use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use crate::providers::muse_telemetry;
use std::path::Path;
use std::process::Command;

/// muse's transient-backend signature, observed when a gateway returns the wrong status
/// for a backend problem on a model request: an HTTP 404 on stream open rendered as
/// ``model `<id>` does not exist or you lack access [request_id=…]`` with exit 1. Case
/// and wording are matched literally as observed — no other adapter's error strings
/// may be swallowed by this, so only `MuseAdapter` implements the predicate.
pub const TRANSIENT_MODEL_ACCESS_MESSAGE: &str = "does not exist or you lack access";

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
    // presence stream is wired, so presence still degrades to the process/output
    // heuristic. Delivery pushes into the running session: `muse session-message send
    // --target <session-uuid>` injects directly, keyed off the session id
    // `StreamCoalescer` captures from the exec JSONL's first `/stream/id` line, guarded on
    // the slot's pid still being alive (a sidecar outlives its process). The delivery seam
    // (`providers::delivery`) falls back to the poll file only when the push is not
    // confirmed — id unknown yet, send failed, or the `--json` reply's own `status` field
    // didn't say `"ok"` or `"accepted"`; a confirmed push is never also duplicated into
    // the poll file. No box this has run on has ever had muse's `external_agent_ingress`
    // gate open, so a confirmed push actually surfacing inside a *headless* `muse exec`
    // run has never been observed end to end — the poll file is the one channel proven to
    // work, and the push is a bonus delivery once its reply can be trusted for real.
    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::MuseSessionMessage
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
            // Headless resume: `muse exec --session-id <uuid> <follow-up>`, which
            // continues the named session in a fresh process. (The interactive
            // `muse resume` exists too, but spar never drives an adapter that way.)
            resume: true,
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
        let mut cmd = self.exec_prefix(bin, opts);
        append_prompt_tail(&mut cmd, opts);
        cmd.current_dir(&opts.cwd);
        cmd
    }

    /// Resume the session `session_id` names instead of a cold dispatch: the same
    /// command `build_headless` builds, plus `--session-id <id>` ahead of the prompt
    /// tail. No `--allow-workspace-switch`: spar re-dispatches in the same worktree,
    /// and muse refusing a workspace mismatch by design is the guard we want. Never
    /// `--no-session-log`: muse rejects a session id without retained logging.
    fn build_resume(&self, bin: &Path, opts: &SpawnOpts, session_id: &str) -> Option<Command> {
        let mut cmd = self.exec_prefix(bin, opts);
        cmd.arg("--session-id").arg(session_id);
        append_prompt_tail(&mut cmd, opts);
        cmd.current_dir(&opts.cwd);
        Some(cmd)
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        // muse has no wired interactive-takeover mode. If a run is forced onto the tmux
        // backend, run the same headless command in the pane so full-auto is preserved:
        // watchable, not takeover-able (capabilities().interactive is false).
        self.build_headless(bin, opts)
    }

    /// A resume dispatch that never established a session is only a lost session when
    /// muse's own session store says so. The dispatch's stdout never carries the typed
    /// `session.opened.observed` resume marker (`muse exec --json` emits no
    /// `session.opened*` record at all), so `log_text` is ignored and the session id
    /// spar asked to resume is looked up under `sessions_root()` instead: a missing
    /// session directory, or a last `record.resume == false` on a spar-requested
    /// resume, means muse no longer has it — clear the marker and retry cold. Anything
    /// else (a `resume: true` record, no record at all, an unreadable log) keeps the
    /// marker: fail closed, never destroy a possibly-valid session.
    fn resume_failure_is_missing_session(&self, _log_text: &str, session_id: Option<&str>) -> bool {
        let Some(sid) = session_id else {
            return false;
        };
        let Some(root) = muse_telemetry::sessions_root() else {
            return false;
        };
        let Some(dir) = muse_telemetry::session_dir(&root, sid) else {
            return true;
        };
        muse_telemetry::resume_observed(&dir) == Some(false)
    }

    /// Only muse's own transient-backend string matches, and only on a diagnostic line.
    /// The coalescer prefixes every stderr line with `! ` and never prefixes the model's
    /// own stdout prose (`StreamCoalescer::feed`), so anchoring there is what separates
    /// muse reporting the 404 from an agent that merely wrote the sentence. Without the
    /// anchor a slot whose task quotes the string — spar-on-spar work, since the literal
    /// lives in this repo's own tests — would buy the full backoff schedule on any
    /// unrelated exit-1 failure before reporting the real error.
    ///
    /// A first-call 404 with zero tool calls still matches here: the `tools >= 1` gate
    /// that tells a transient backend blip from a genuinely wrong model name or dead
    /// entitlement lives in `executor::dispatch_with_resume_recovery`, next to the retry.
    fn dispatch_failure_is_transient(&self, log_text: &str) -> bool {
        log_text
            .lines()
            .any(|l| l.starts_with("! ") && l.contains(TRANSIENT_MODEL_ACCESS_MESSAGE))
    }

    /// muse's documented exit codes: 0 success, 1 failure or cancellation (including
    /// hitting `--max-model-steps`), 2 usage error, 130/143 for SIGINT/SIGTERM. Exit 2
    /// means spar built a command line muse rejected — spar's fault, never the agent's.
    fn is_usage_error(&self, code: Option<i32>) -> bool {
        code == Some(2)
    }

    /// Exit 1 after the model-step budget stops the run is a budget stop, not a crash.
    /// Keyed on the log prose (spar passes no `--max-model-steps` today, so a future
    /// flag is already classified); case-insensitive since muse's own casing of the
    /// flag name in prose is not a contract.
    fn step_budget_exhausted(&self, log_text: &str) -> bool {
        log_text.to_ascii_lowercase().contains("max-model-steps")
            || log_text.to_ascii_lowercase().contains("max model steps")
    }

    /// Align muse's own stream kill with the slot ceiling: without these muse's stream
    /// idle timeout (180000ms by default) kills long dispatches spar would otherwise
    /// let run to their hard ceiling. Set to the ceiling itself so muse's kill lands
    /// no earlier than spar's; unset when there is no ceiling, leaving muse's default.
    /// These are undocumented vendor env vars (`TBH_STREAM_*`), not a public contract —
    /// if muse ever drops them, the dispatches just lose the alignment, nothing else.
    fn extra_env(&self, opts: &SpawnOpts) -> Vec<(String, String)> {
        let Some(secs) = opts.timeout_secs else {
            return Vec::new();
        };
        vec![
            ("TBH_STREAM_IDLE_TIMEOUT_SECS".to_string(), secs.to_string()),
            (
                "TBH_STREAM_FIRST_EVENT_TIMEOUT_SECS".to_string(),
                secs.to_string(),
            ),
        ]
    }
}

impl MuseAdapter {
    /// `muse exec` plus every flag short of the prompt itself, shared by
    /// `build_headless` and `build_resume` so the two cannot drift. The prompt tail
    /// must come last (positional prompt, or `--prompt-file`), so `--session-id` is
    /// inserted here by the caller before `append_prompt_tail` runs — appending it to
    /// an already-built command would land it after the positional prompt and break
    /// parsing.
    fn exec_prefix(&self, bin: &Path, opts: &SpawnOpts) -> Command {
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
        cmd
    }
}

/// The prompt tail: `--prompt-file` avoids the arg-length and leading-dash hazards of
/// a positional prompt. Every slot call site fills `prompt_file` with the same bytes
/// it puts in `prompt`, so the file wins whenever there is one.
fn append_prompt_tail(cmd: &mut Command, opts: &SpawnOpts) {
    match &opts.prompt_file {
        Some(pf) => {
            cmd.arg("--prompt-file").arg(pf);
        }
        None => {
            cmd.arg("--");
            cmd.arg(&opts.prompt);
        }
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

    /// Every delivery-seam test constructs `DeliveryStrategy::MuseSessionMessage` as a
    /// literal, so nothing else pins the one line that actually routes muse onto it —
    /// reverting `delivery_strategy` to `PollFile` would leave every other test green.
    #[test]
    fn delivery_strategy_is_muse_session_message() {
        assert_eq!(
            MuseAdapter.delivery_strategy(),
            DeliveryStrategy::MuseSessionMessage
        );
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

    #[test]
    fn capabilities_advertise_headless_resume() {
        assert!(MuseAdapter.capabilities().resume);
        assert!(MuseAdapter.capabilities().headless);
    }

    #[test]
    fn resume_inserts_session_id_before_the_prompt_tail() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        // Positional-prompt form: `--session-id` must precede `--` and the prompt.
        let cmd = MuseAdapter
            .build_resume(Path::new("muse"), &opts("do the thing", None), "sess-abc")
            .expect("muse supports resume");
        let (_, args) = command_to_parts(&cmd);
        let si = args
            .iter()
            .position(|a| a == "--session-id")
            .expect("--session-id");
        assert_eq!(args.get(si + 1).map(String::as_str), Some("sess-abc"));
        let di = args.iter().position(|a| a == "--").expect("-- separator");
        assert!(si < di, "session id must precede the prompt tail: {args:?}");
        assert_eq!(args.last().map(String::as_str), Some("do the thing"));
        assert!(
            !args.iter().any(|a| a == "--allow-workspace-switch"),
            "same-worktree re-dispatch keeps muse's workspace-mismatch refusal: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--no-session-log"),
            "muse rejects a session id without retained logging: {args:?}"
        );
    }

    #[test]
    fn resume_with_prompt_file_keeps_the_file_tail() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let mut o = opts("inline", None);
        o.prompt_file = Some(PathBuf::from("/run/prompts/slot.md"));
        let cmd = MuseAdapter
            .build_resume(Path::new("muse"), &o, "sess-abc")
            .expect("muse supports resume");
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(dash_val(&args, "--session-id").as_deref(), Some("sess-abc"));
        assert_eq!(
            dash_val(&args, "--prompt-file").as_deref(),
            Some("/run/prompts/slot.md")
        );
        assert!(!args.iter().any(|a| a == "inline"));
    }

    #[test]
    fn resume_matches_headless_prefix() {
        // The resume command is the headless command plus `--session-id`, so the two
        // cannot drift apart as flags are added.
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let o = opts("go", Some("muse-spark-1.2-contributor"));
        let (_, headless) = command_to_parts(&MuseAdapter.build_headless(Path::new("muse"), &o));
        let (_, resume) = command_to_parts(
            &MuseAdapter
                .build_resume(Path::new("muse"), &o, "sess-abc")
                .expect("muse supports resume"),
        );
        let mut expected = headless.clone();
        let tail_at = expected
            .iter()
            .position(|a| a == "--")
            .expect("-- separator");
        expected.insert(tail_at, "sess-abc".into());
        expected.insert(tail_at, "--session-id".into());
        assert_eq!(resume, expected);
    }

    #[test]
    fn transient_matches_only_muses_own_404_string() {
        // As the coalescer renders it: muse's stderr, `! `-prefixed.
        assert!(MuseAdapter.dispatch_failure_is_transient(
            "→ bash  success\n! model `muse-spark-1.3-contributor` does not exist or you lack access [request_id=abc]\n"
        ));
        assert!(!MuseAdapter
            .dispatch_failure_is_transient("Error: Model provider 'openrouter' not found"));
        assert!(!MuseAdapter.dispatch_failure_is_transient("no rollout found for thread id x"));
        assert!(!MuseAdapter.dispatch_failure_is_transient(""));
    }

    /// The anchor, not the substring, is what makes this safe on spar-on-spar work:
    /// an agent that writes the sentence into its own stdout must not buy the backoff
    /// schedule on an unrelated failure.
    #[test]
    fn transient_ignores_the_string_in_agent_output() {
        assert!(!MuseAdapter.dispatch_failure_is_transient(
            "I added a test asserting `model `x` does not exist or you lack access` is matched.\n"
        ));
        assert!(!MuseAdapter.dispatch_failure_is_transient(
            "→ write_file  src/providers/muse.rs  success\ndoes not exist or you lack access\n"
        ));
    }

    #[test]
    fn usage_error_is_exit_2_only() {
        assert!(MuseAdapter.is_usage_error(Some(2)));
        assert!(!MuseAdapter.is_usage_error(Some(1)));
        assert!(!MuseAdapter.is_usage_error(Some(0)));
        assert!(!MuseAdapter.is_usage_error(None));
    }

    #[test]
    fn budget_stop_keys_on_the_step_flag_prose() {
        assert!(MuseAdapter.step_budget_exhausted("stopped: hit --max-model-steps, exiting 1"));
        assert!(MuseAdapter.step_budget_exhausted("hit Max Model Steps"));
        assert!(!MuseAdapter.step_budget_exhausted("does not exist or you lack access"));
        assert!(!MuseAdapter.step_budget_exhausted(""));
    }

    #[test]
    fn extra_env_aligns_muse_timeouts_with_the_slot_ceiling() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        let mut o = opts("x", None);
        o.timeout_secs = Some(3600);
        let env = MuseAdapter.extra_env(&o);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TBH_STREAM_IDLE_TIMEOUT_SECS")
                .map(|(_, v)| v.as_str()),
            Some("3600")
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "TBH_STREAM_FIRST_EVENT_TIMEOUT_SECS")
                .map(|(_, v)| v.as_str()),
            Some("3600")
        );
        let o = opts("x", None);
        assert!(
            MuseAdapter.extra_env(&o).is_empty(),
            "no ceiling leaves muse's default alone"
        );
    }

    /// Session markers are keyed `(slot, provider)`, which is what stops a resume
    /// across a provider rotation (muse refuses a foreign session id outright). That
    /// holds for muse only because the executor keys its marker with this same name —
    /// pin the name here so a rename breaks loudly instead of silently resuming
    /// across providers.
    #[test]
    fn marker_provider_key_is_the_adapter_name() {
        assert_eq!(MuseAdapter.name(), "muse");
        // The executor keys markers with `ProviderRef::cli_name`, one indirection
        // from this literal: pin that `cli:muse` actually resolves to this adapter's
        // name, which is the property that keeps a resume from crossing a provider
        // rotation. A rename breaks this; a keying regression breaks it too.
        let pref = crate::provider_ref::ProviderRef::parse("cli:muse").unwrap();
        assert_eq!(pref.cli_name(), Some(MuseAdapter.name()));
    }

    /// Point `sessions_root` at a scratch dir for the duration of the closure.
    /// Serialized with every other env-mutating muse test via `ENV_LOCK`.
    fn with_isolated_store(f: impl FnOnce(&Path)) {
        let tmp = tempfile::tempdir().unwrap();
        // `sessions_root` only answers when the directory exists.
        std::fs::create_dir_all(tmp.path().join("muse/sessions")).unwrap();
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let old_home = std::env::var_os("HOME");
        std::env::set_var("XDG_DATA_HOME", tmp.path());
        f(tmp.path());
        if let Some(v) = old_xdg {
            std::env::set_var("XDG_DATA_HOME", v);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        if let Some(v) = old_home {
            std::env::set_var("HOME", v);
        }
    }

    fn write_session_log(root: &Path, session_id: &str, lines: &[String]) {
        let dir = root.join(format!("muse/sessions/2026/09/17/{session_id}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), lines.join("\n")).unwrap();
    }

    fn observed(resume: bool) -> String {
        serde_json::json!({
            "payload_type": "session.opened.observed",
            "payload": {"record": {"resume": resume}}
        })
        .to_string()
    }

    #[test]
    fn missing_session_comes_from_the_store_not_stdout() {
        let _guard = ENV_LOCK.lock().unwrap();
        with_isolated_store(|root| {
            // No session id: nothing was resumed, so nothing can be missing.
            assert!(!MuseAdapter.resume_failure_is_missing_session("anything", None));
            // Unknown session id starts a brand-new session and exits 0, so no log
            // text can name it — but the missing directory still says it is gone.
            assert!(MuseAdapter.resume_failure_is_missing_session("", Some("no-such-session")));
            // A resume the store records as resumed keeps its marker whatever stdout says.
            write_session_log(root, "live-sess", &[observed(true)]);
            assert!(!MuseAdapter.resume_failure_is_missing_session("", Some("live-sess")));
            assert!(!MuseAdapter.resume_failure_is_missing_session(
                "does not exist or you lack access",
                Some("live-sess"),
            ));
            // A spar-requested resume the store records as a cold start is gone.
            write_session_log(root, "dead-sess", &[observed(false)]);
            assert!(MuseAdapter.resume_failure_is_missing_session("", Some("dead-sess")));
            // A directory with no usable record fails closed: the marker survives.
            write_session_log(
                root,
                "mute-sess",
                &["{\"payload_type\":\"run.lifecycle.started\"}".into()],
            );
            assert!(!MuseAdapter.resume_failure_is_missing_session("", Some("mute-sess")));
        });
    }
}
