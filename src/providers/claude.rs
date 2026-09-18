use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

pub struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn delivery_strategy(&self) -> DeliveryStrategy {
        DeliveryStrategy::StopHookInject
    }

    fn presence_source(&self) -> PresenceSource {
        PresenceSource::Hooks
    }

    fn binary_names(&self) -> &[&'static str] {
        &["claude"]
    }

    // No readiness probe: the only local candidate, `claude doctor`, is an
    // installation check, not dispatch readiness — it exits 0 even with no
    // credentials (verified with an empty HOME). A probe that cannot fail on a
    // broken-auth box would report false-healthy, so the default (Unknown)
    // stands.

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            interactive: true,
            // No `build_resume`: a real implementation would hand the captured
            // session id to `claude -r/--resume <id>` (fork forms add
            // `--fork-session` / `--session-id`; verified against claude
            // 2.1.248). Unimplemented, so the derived flag stays false.
            resume: self.supports_resume(),
            skip_permissions: true,
            native_sandbox: false,
            // `claude --session-id <uuid>` names a new conversation (2.1.248).
            assigns_session_id: true,
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
        if let Some(m) = &opts.model {
            cmd.arg("--model").arg(m);
        }
        if let Some(id) = opts.session_id.as_deref() {
            cmd.arg("--session-id").arg(id);
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
        if let Some(id) = opts.session_id.as_deref() {
            cmd.arg("--session-id").arg(id);
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

    /// claude refuses a `--session-id` naming an already-existing session on stderr
    /// before doing any work (exit 1, `Error: Session ID <uuid> is already in
    /// use.`, probed live on 2.1.248 with two same-id dispatches: the first ran,
    /// the second refused with no model call). Malformed ids refuse the same way
    /// (`Error: Invalid session ID. Must be a valid UUID.`); spar only ever sends
    /// well-formed v5 ids, so that arm is defense-in-depth for a broken
    /// derivation. Both arms require a vendor-error start; the reuse arm
    /// additionally requires the dispatch's own assigned id in the line, so agent
    /// prose quoting a refusal can never match. The matcher accepts the log with
    /// or without the coalescer's `! ` stderr prefix (headless vs tmux tee).
    fn assigned_session_refused(
        &self,
        log_text: &str,
        assigned_id: &str,
        _code: Option<i32>,
    ) -> bool {
        log_text.lines().any(|l| {
            let line = l.strip_prefix("! ").unwrap_or(l);
            line.starts_with("Error")
                && (line.contains("Invalid session ID")
                    || (line.contains("Session ID")
                        && line.contains(assigned_id)
                        && line.contains("is already in use")))
        })
    }

    /// claude refuses `--session-id` naming an already-existing session (probed
    /// live on 2.1.248), so a recovery turn cannot reuse the marker id the failed
    /// dispatch already created. It derives a distinct session instead; the marker
    /// keeps naming the main session for forensics.
    fn recovery_session_id(
        &self,
        _marker_id: Option<&str>,
        run_id: &str,
        slot_id: &str,
        round: u32,
    ) -> Option<String> {
        Some(crate::session_id::derive_recovery(run_id, slot_id, round))
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
            session_id: None,
            model: None,
            timeout_secs: None,
        };
        let cmd = ClaudeAdapter.build_headless(Path::new("claude"), &opts);
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("-p"));
        assert_eq!(args.get(1).map(String::as_str), Some("implement feature"));
        assert!(args.iter().any(|a| a == "stream-json"));
        assert!(!args.iter().any(|a| a.starts_with('@')));
    }

    fn opts_with_session(session_id: Option<&str>) -> SpawnOpts {
        SpawnOpts {
            prompt: "implement feature".into(),
            prompt_file: None,
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::FullAuto,
            extra_args: vec![],
            session_id: session_id.map(str::to_string),
            model: None,
            timeout_secs: None,
        }
    }

    fn dash_val(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    }

    #[test]
    fn assigned_session_id_rendered_when_present() {
        let id = "123e4567-e89b-52d3-a456-426614174000";
        for cmd in [
            ClaudeAdapter.build_headless(Path::new("claude"), &opts_with_session(Some(id))),
            ClaudeAdapter.build_interactive(Path::new("claude"), &opts_with_session(Some(id))),
        ] {
            let (_, args) = command_to_parts(&cmd);
            assert_eq!(dash_val(&args, "--session-id").as_deref(), Some(id));
        }
    }

    #[test]
    fn assigned_session_id_absent_when_none() {
        for cmd in [
            ClaudeAdapter.build_headless(Path::new("claude"), &opts_with_session(None)),
            ClaudeAdapter.build_interactive(Path::new("claude"), &opts_with_session(None)),
        ] {
            let (_, args) = command_to_parts(&cmd);
            assert!(
                !args.iter().any(|a| a == "--session-id"),
                "no flag without an assigned id: {args:?}"
            );
        }
    }

    #[test]
    fn session_refusal_matches_probed_vendor_stderr() {
        let id = "dced66eb-ac83-4381-bf62-e9d21fc2a051";
        // Reuse refusal, probed live: exact line, with and without the coalescer
        // prefix (headless slot log vs tmux pane tee).
        assert!(ClaudeAdapter.assigned_session_refused(
            &format!("! Error: Session ID {id} is already in use.\n"),
            id,
            Some(1),
        ));
        assert!(ClaudeAdapter.assigned_session_refused(
            &format!("Error: Session ID {id} is already in use.\n"),
            id,
            Some(1),
        ));
        // Malformed refusal, probed live (defense-in-depth: spar never sends one).
        assert!(ClaudeAdapter.assigned_session_refused(
            "! Error: Invalid session ID. Must be a valid UUID.\n",
            id,
            Some(1),
        ));
        // A refusal naming a *different* session is not ours.
        assert!(!ClaudeAdapter.assigned_session_refused(
            "! Error: Session ID 00000000-0000-0000-0000-000000000000 is already in use.\n",
            id,
            Some(1),
        ));
        assert!(!ClaudeAdapter.assigned_session_refused("", id, Some(1)));
        // Agent prose quoting a refusal never matches: no vendor-error start.
        assert!(!ClaudeAdapter.assigned_session_refused(
            &format!("the agent wrote Session ID {id} is already in use in its summary\n"),
            id,
            Some(1),
        ));
    }

    #[test]
    fn recovery_names_a_distinct_session() {
        let id = ClaudeAdapter
            .recovery_session_id(Some("main-session"), "run1", "slotA", 2)
            .expect("claude recovery always names a session");
        assert_ne!(id, "main-session");
        assert!(crate::session_id::is_uuid_shape(&id));
    }
}
