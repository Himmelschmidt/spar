use super::{
    Capabilities, DeliveryStrategy, PresenceSource, ProviderAdapter, QuotaScope, QuotaSignal,
    RateLimitWindow, SpawnOpts, TrustPolicy,
};
use std::path::Path;
use std::process::Command;

pub struct ClaudeAdapter;

/// The model family a claude seat dispatches as. Per-model weekly windows cover one
/// family (`seven_day_opus`, `seven_day_sonnet`, the "Fable limit" prose), so one
/// family shares one quota bucket regardless of seat spelling (`opus` and
/// `claude-opus-5-5` are one bucket); haiku has no per-model window and keys the login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeFamily {
    Opus,
    Sonnet,
    Haiku,
    Fable,
}

impl ClaudeFamily {
    pub const ALL: [Self; 4] = [Self::Opus, Self::Sonnet, Self::Haiku, Self::Fable];

    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Haiku => "haiku",
            Self::Fable => "fable",
        }
    }

    /// The seat spellings that dispatch as this family: the bare alias, or a
    /// `claude-<family>-` full id (dated or versioned), case-insensitive, with any
    /// `[...]` suffix ignored (`opus[1m]`, `claude-opus-5-5[1m]`).
    pub fn of(model: &str) -> Option<Self> {
        let lower = model.to_ascii_lowercase();
        let base = lower.split('[').next().unwrap_or("").trim();
        Self::ALL.into_iter().find(|f| {
            let name = f.name();
            base == name
                || base == format!("claude-{name}")
                || base.starts_with(&format!("claude-{name}-"))
        })
    }

    /// The per-model weekly bucket, `None` for a family with no per-model window.
    fn bucket(self) -> Option<&'static str> {
        match self {
            Self::Haiku => None,
            _ => Some(self.name()),
        }
    }

    /// The family a per-model rejection line names ("You've reached your Fable limit").
    pub fn limit_line(lower_line: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| lower_line.contains(&format!("{} limit", f.name())))
    }
}

fn bucket_scope(family: ClaudeFamily) -> QuotaScope {
    match family.bucket() {
        Some(bucket) => QuotaScope::Bucket(bucket),
        None => QuotaScope::Login,
    }
}

fn window_scope(window: &RateLimitWindow) -> QuotaScope {
    match window {
        RateLimitWindow::FiveHour
        | RateLimitWindow::SevenDay
        | RateLimitWindow::Overage
        | RateLimitWindow::RateLimit
        | RateLimitWindow::Unknown => QuotaScope::Login,
        RateLimitWindow::SevenDayOpus => bucket_scope(ClaudeFamily::Opus),
        RateLimitWindow::SevenDaySonnet => bucket_scope(ClaudeFamily::Sonnet),
        RateLimitWindow::SevenDayOverageIncluded => QuotaScope::Seat,
    }
}

/// The scope one rejection line votes for, when the line is a rejection at all: a
/// rendered `! rate limit <window> rejected` line maps through the window table, a
/// line the strong signal accepts maps through the family it names (or login-wide
/// when it names none). `None` for any other line.
fn line_scope(line: &str) -> Option<QuotaScope> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('!').unwrap_or(trimmed).trim();
    if let Some(rest) = body.strip_prefix("rate limit") {
        let mut parts = rest.split_whitespace();
        if let (Some(window), Some(status)) = (parts.next(), parts.next()) {
            if status == "rejected" {
                return Some(window_scope(&RateLimitWindow::parse(window)));
            }
        }
    }
    if crate::quota::QuotaStore::scrape_strong_quota_signal(line).is_some() {
        let lower = line.to_ascii_lowercase();
        return Some(match ClaudeFamily::limit_line(&lower) {
            Some(family) => bucket_scope(family),
            None => QuotaScope::Login,
        });
    }
    None
}

/// The rejection votes across text lines: the named family bucket when every line
/// agrees on one, whether any vote is seat-scoped, and whether any vote is
/// login-wide (or two votes disagree, which also widens to the login).
fn scope_votes(log: &str) -> (Option<&'static str>, bool, bool) {
    let mut bucket: Option<&'static str> = None;
    let mut seat = false;
    let mut login = false;
    for line in log.lines() {
        match line_scope(line) {
            None => {}
            Some(QuotaScope::Login) => login = true,
            Some(QuotaScope::Seat) => seat = true,
            Some(QuotaScope::Bucket(b)) => match bucket {
                None => bucket = Some(b),
                Some(cur) if cur == b => {}
                Some(_) => login = true,
            },
        }
    }
    (bucket, seat, login)
}

fn reduce_votes(bucket: Option<&'static str>, seat: bool, login: bool) -> QuotaScope {
    if login || (bucket.is_some() && seat) || (bucket.is_none() && !seat) {
        return QuotaScope::Login;
    }
    match bucket {
        Some(b) => QuotaScope::Bucket(b),
        None => QuotaScope::Seat,
    }
}

fn text_scope(log: &str) -> QuotaScope {
    let (bucket, seat, login) = scope_votes(log);
    reduce_votes(bucket, seat, login)
}

/// A typed window voting alongside text lines: the window's scope joins the same
/// ballot, broadest wins, and text with no rejection lines contributes nothing, so
/// a lone typed event keeps its bucket. A seat-scoped window vote is resolved
/// against the dispatched seat's bucket: when the text names that same bucket,
/// both votes agree and the bucket stands, otherwise the vote stays seat-scoped
/// and widens exactly as an unknown seat would.
fn combined_scope(seat: Option<&str>, window: &RateLimitWindow, log: &str) -> QuotaScope {
    let (mut bucket, mut seat_vote, mut login) = scope_votes(log);
    let seat_bucket = seat
        .and_then(ClaudeFamily::of)
        .and_then(ClaudeFamily::bucket);
    match window_scope(window) {
        QuotaScope::Login => login = true,
        QuotaScope::Seat => match (seat_bucket, bucket) {
            (Some(own), Some(named)) if own != named => login = true,
            (Some(_), Some(_)) => {}
            _ => seat_vote = true,
        },
        QuotaScope::Bucket(b) => match bucket {
            None => bucket = Some(b),
            Some(cur) if cur == b => {}
            Some(_) => login = true,
        },
    }
    reduce_votes(bucket, seat_vote, login)
}

impl ProviderAdapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn quota_bucket<'m>(&self, model: &'m str) -> Option<std::borrow::Cow<'m, str>> {
        ClaudeFamily::of(model)
            .and_then(|f| f.bucket())
            .map(std::borrow::Cow::Borrowed)
    }

    fn rejection_scope(&self, seat: Option<&str>, signal: QuotaSignal<'_>) -> QuotaScope {
        match signal {
            QuotaSignal::Window(window) => window_scope(window),
            QuotaSignal::Text(log) => text_scope(log),
            QuotaSignal::WindowAndText(window, log) => combined_scope(seat, window, log),
        }
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

    // `claude auth status` exits 1 with `"loggedIn": false` when logged out and
    // 0 when logged in (verified on 2.1.314 with an empty HOME and a real one).
    // It phones home for telemetry unless nonessential traffic is off; with it
    // off, strace shows no network connection. It does not check expiry: a
    // credentials file whose refresh token expired still reads logged in, so
    // `credential_fault` reads that from the file. (`claude doctor` is an
    // install check that exits 0 with no credentials at all.)
    fn readiness_probe(&self) -> Option<Vec<String>> {
        Some(vec!["auth".into(), "status".into()])
    }

    fn readiness_env(&self) -> &[(&'static str, &'static str)] {
        &[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")]
    }

    fn credential_fault(&self) -> Option<String> {
        let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
        let text = std::fs::read_to_string(dir.join(".credentials.json")).ok()?;
        expired_refresh_token(&text, chrono::Utc::now())
    }

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

    fn prompt_stdin(&self, opts: &SpawnOpts) -> Option<std::path::PathBuf> {
        opts.prompt_file.clone()
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        let mut cmd = Command::new(bin);
        cmd.arg("-p");
        // `-p` with no prompt argument reads the prompt from stdin, which
        // `prompt_stdin` hands it whenever a prompt file exists.
        if opts.prompt_file.is_none() {
            cmd.arg(&opts.prompt);
        }
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
        if let Some(e) = opts.effort {
            cmd.arg("--effort").arg(e.as_str());
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
        if let Some(e) = opts.effort {
            cmd.arg("--effort").arg(e.as_str());
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

    /// #182: a prompt in argv is matched by every `pgrep -f`; with a prompt file it
    /// rides stdin and argv carries none of it.
    #[test]
    fn headless_with_a_prompt_file_reads_the_prompt_from_stdin() {
        let opts = SpawnOpts {
            prompt: "implement feature".into(),
            prompt_file: Some(PathBuf::from("/tmp/p.md")),
            cwd: PathBuf::from("/tmp"),
            trust: TrustPolicy::Prompt,
            extra_args: vec![],
            session_id: None,
            model: None,
            effort: None,
            timeout_secs: None,
            session_dir: None,
        };
        let cmd = ClaudeAdapter.build_headless(Path::new("claude"), &opts);
        let (_, args) = command_to_parts(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("-p"));
        assert_eq!(args.get(1).map(String::as_str), Some("--output-format"));
        assert!(
            !args.iter().any(|a| a.contains("implement feature")),
            "{args:?}"
        );
        assert!(!args.iter().any(|a| a.starts_with('@')));
        assert_eq!(
            ClaudeAdapter.prompt_stdin(&opts),
            Some(PathBuf::from("/tmp/p.md"))
        );
    }

    #[test]
    fn headless_without_a_prompt_file_keeps_the_prompt_in_argv() {
        let opts = opts_with_session(None);
        let (_, args) = command_to_parts(&ClaudeAdapter.build_headless(Path::new("claude"), &opts));
        assert_eq!(args.get(1).map(String::as_str), Some("implement feature"));
        assert_eq!(ClaudeAdapter.prompt_stdin(&opts), None);
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
            effort: None,
            timeout_secs: None,
            session_dir: None,
        }
    }

    #[test]
    fn effort_renders_flag_when_set_and_nothing_when_unset() {
        use crate::effort::EffortLevel;
        let mut opts = opts_with_session(None);
        opts.effort = Some(EffortLevel::Xhigh);
        let (_, args) = command_to_parts(&ClaudeAdapter.build_headless(Path::new("claude"), &opts));
        assert_eq!(dash_val(&args, "--effort").as_deref(), Some("xhigh"));
        opts.effort = None;
        let (_, args) = command_to_parts(&ClaudeAdapter.build_headless(Path::new("claude"), &opts));
        assert!(
            !args.iter().any(|a| a == "--effort"),
            "no flag without an effort: {args:?}"
        );
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
    fn family_parsing_canonicalizes_seat_spellings() {
        use ClaudeFamily as F;
        for m in [
            "opus",
            "Opus",
            "OPUS",
            "claude-opus-4-1",
            "claude-opus-5-5",
            "opus[1m]",
        ] {
            assert_eq!(F::of(m), Some(F::Opus), "{m}");
        }
        for m in ["sonnet", "claude-sonnet-5-5", "claude-sonnet-4-5-20250929"] {
            assert_eq!(F::of(m), Some(F::Sonnet), "{m}");
        }
        assert_eq!(F::of("haiku"), Some(F::Haiku));
        assert_eq!(F::of("claude-haiku-5-5"), Some(F::Haiku));
        assert_eq!(F::of("fable"), Some(F::Fable));
        assert_eq!(F::of("claude-fable-5-1"), Some(F::Fable));
        assert_eq!(F::of("pro"), None);
        assert_eq!(F::of(""), None);
        assert_eq!(F::of("opus-degraded"), None);
    }

    #[test]
    fn haiku_has_no_bucket_while_siblings_do() {
        assert_eq!(ClaudeFamily::Haiku.bucket(), None);
        assert_eq!(ClaudeFamily::Opus.bucket(), Some("opus"));
        assert_eq!(ClaudeFamily::Sonnet.bucket(), Some("sonnet"));
        assert_eq!(ClaudeFamily::Fable.bucket(), Some("fable"));
    }

    #[test]
    fn quota_bucket_is_the_family_or_nothing() {
        use std::borrow::Cow;
        assert_eq!(
            ClaudeAdapter.quota_bucket("claude-opus-5-5"),
            Some(Cow::Borrowed("opus"))
        );
        assert_eq!(
            ClaudeAdapter.quota_bucket("opus"),
            Some(Cow::Borrowed("opus"))
        );
        assert_eq!(ClaudeAdapter.quota_bucket("haiku"), None);
        assert_eq!(ClaudeAdapter.quota_bucket("pro"), None);
    }

    #[test]
    fn rejection_scope_maps_the_window_table() {
        use QuotaScope as S;
        use RateLimitWindow as W;
        for w in [
            W::FiveHour,
            W::SevenDay,
            W::Overage,
            W::RateLimit,
            W::Unknown,
        ] {
            assert_eq!(
                ClaudeAdapter.rejection_scope(None, QuotaSignal::Window(&w)),
                S::Login,
                "{w}"
            );
        }
        assert_eq!(
            ClaudeAdapter.rejection_scope(None, QuotaSignal::Window(&W::SevenDayOpus)),
            S::Bucket("opus")
        );
        assert_eq!(
            ClaudeAdapter.rejection_scope(None, QuotaSignal::Window(&W::SevenDaySonnet)),
            S::Bucket("sonnet")
        );
        assert_eq!(
            ClaudeAdapter.rejection_scope(None, QuotaSignal::Window(&W::SevenDayOverageIncluded)),
            S::Seat
        );
    }

    #[test]
    fn rejection_scope_reads_prose_lines() {
        use QuotaScope as S;
        let text = |l: &str| ClaudeAdapter.rejection_scope(None, QuotaSignal::Text(l));
        assert_eq!(text("You've reached your Fable limit"), S::Bucket("fable"));
        assert_eq!(
            text("! rate limit  seven_day_opus  rejected"),
            S::Bucket("opus")
        );
        assert_eq!(text("You've hit your weekly limit"), S::Login);
        assert_eq!(
            text("You've reached your Fable limit\nYou've hit your weekly limit"),
            S::Login,
            "any login-wide line wins"
        );
        assert_eq!(
            text("You've reached your Opus limit\nYou've reached your Sonnet limit"),
            S::Login,
            "mixed families widen to the login"
        );
        assert_eq!(text("all quiet here"), S::Login);
    }

    #[test]
    fn rejection_scope_combines_a_window_with_prose() {
        use QuotaScope as S;
        use RateLimitWindow as W;
        let both = |s: Option<&str>, w: &W, l: &str| {
            ClaudeAdapter.rejection_scope(s, QuotaSignal::WindowAndText(w, l))
        };
        assert_eq!(
            both(None, &W::SevenDayOpus, "{\"type\":\"rate_limit_event\"}"),
            S::Bucket("opus"),
            "prose-free output leaves the typed bucket standing"
        );
        assert_eq!(
            both(None, &W::SevenDayOpus, "You've hit your limit"),
            S::Login,
            "a generic line next to a family window widens to the login"
        );
        assert_eq!(
            both(None, &W::SevenDayOpus, "You've reached your Opus limit"),
            S::Bucket("opus")
        );
        assert_eq!(
            both(None, &W::SevenDayOpus, "You've reached your Fable limit"),
            S::Login,
            "conflicting families widen to the login"
        );
        assert_eq!(
            both(None, &W::SevenDayOverageIncluded, "all quiet here"),
            S::Seat
        );
        assert_eq!(both(None, &W::FiveHour, "all quiet here"), S::Login);
        assert_eq!(
            both(
                Some("fable"),
                &W::SevenDayOverageIncluded,
                "You've reached your Fable limit"
            ),
            S::Bucket("fable"),
            "both signals name the dispatched family"
        );
        assert_eq!(
            both(
                Some("opus"),
                &W::SevenDayOverageIncluded,
                "You've reached your Fable limit"
            ),
            S::Login,
            "the text names another family's bucket"
        );
        assert_eq!(
            both(Some("opus"), &W::SevenDayOverageIncluded, "all quiet here"),
            S::Seat,
            "prose-free output stays seat-scoped for the key to resolve"
        );
    }

    #[test]
    fn recovery_names_a_distinct_session() {
        let id = ClaudeAdapter
            .recovery_session_id(Some("main-session"), "run1", "slotA", 2)
            .expect("claude recovery always names a session");
        assert_ne!(id, "main-session");
        assert!(crate::session_id::is_uuid_shape(&id));
    }

    #[test]
    fn effort_interactive_matches_headless() {
        use crate::effort::EffortLevel;
        let mut opts = opts_with_session(None);
        opts.effort = Some(EffortLevel::High);
        let (_, args) =
            command_to_parts(&ClaudeAdapter.build_interactive(Path::new("claude"), &opts));
        let i = args.iter().position(|a| a == "--effort").expect("--effort");
        assert_eq!(args.get(i + 1).map(String::as_str), Some("high"));
        opts.effort = None;
        let (_, args) =
            command_to_parts(&ClaudeAdapter.build_interactive(Path::new("claude"), &opts));
        assert!(!args.iter().any(|a| a == "--effort"));
    }
}

/// The OAuth login is dead once its refresh token expires: the access token can no
/// longer be renewed and every dispatch fails with a 401. Reads `claudeAiOauth` in
/// `.credentials.json` (Linux; macOS keeps it in the keychain, where this finds no
/// file and reports nothing).
fn expired_refresh_token(text: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let ms = v
        .pointer("/claudeAiOauth/refreshTokenExpiresAt")?
        .as_i64()?;
    let at = chrono::DateTime::from_timestamp_millis(ms)?;
    (at <= now).then(|| {
        format!(
            "claude login expired at {} (refresh token); run `claude auth login`",
            at.to_rfc3339()
        )
    })
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn an_expired_refresh_token_is_a_credential_fault() {
        let now = chrono::DateTime::from_timestamp_millis(1_793_400_000_000).unwrap();
        let expired = r#"{"claudeAiOauth":{"accessToken":"x","refreshToken":"y","expiresAt":1793300000000,"refreshTokenExpiresAt":1793300000000,"scopes":["user:inference"]}}"#;
        let msg = expired_refresh_token(expired, now).expect("expired");
        assert!(msg.contains("claude auth login"), "{msg}");

        let live = r#"{"claudeAiOauth":{"expiresAt":1793300000000,"refreshTokenExpiresAt":1795923537481}}"#;
        assert!(
            expired_refresh_token(live, now).is_none(),
            "an expired access token alone is renewed by the CLI"
        );
        assert!(expired_refresh_token(r#"{"other":1}"#, now).is_none());
        assert!(expired_refresh_token("not json", now).is_none());
    }
}
