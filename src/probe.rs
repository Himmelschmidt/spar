//! `spar provider probe`: one cheap real call per provider, so quota state reflects what
//! the provider says now instead of a failure recorded days ago (#154, O105).
use crate::paths::SparPaths;
use crate::provider_ref::ProviderRef;
use crate::providers::{self, SpawnOpts, TrustPolicy};
use crate::quota::{self, QuotaStore};
use serde::Serialize;
use std::time::Duration;

const PROBE_PROMPT: &str = "Reply with the single word OK. Do not use any tools.";

/// Long enough for a cold CLI start plus one short model turn.
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The call succeeded: the provider can serve right now.
    Healthy,
    /// The provider rejected the call on a limit.
    Quota,
    /// The provider refused the credential.
    Auth,
    /// The call failed for another reason; quota state is left alone.
    Failed,
    /// No call was made (dry-run, an api provider, no binary on PATH).
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeReport {
    pub provider: String,
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the probe did to `.spar/quota.json`.
    pub store: String,
}

/// Probe one provider and write the verdict into the quota store. Under `dry` nothing
/// is spawned and nothing is written. A quota verdict lands on the rejection key the
/// output names: a login-wide claude rejection pauses `cli:claude`, an Opus-limit
/// one pauses `cli:claude@opus`. Any other verdict uses the seat's own key.
pub fn probe(paths: &SparPaths, provider: &str, dry: bool) -> ProbeReport {
    let out = call(paths, provider, dry);
    let key = ProviderRef::parse(provider)
        .map(|pref| {
            if out.verdict == Verdict::Quota {
                let text = out.output.as_deref().unwrap_or("");
                match out.window {
                    // Finished output carries no stream state: the window votes
                    // alongside the prose, broadest wins, and prose-free output
                    // leaves the typed bucket standing.
                    Some(window) => pref
                        .rejection_key(crate::providers::QuotaSignal::WindowAndText(&window, text)),
                    None => pref.rejection_key(crate::providers::QuotaSignal::Text(text)),
                }
            } else {
                pref.quota_key()
            }
        })
        .unwrap_or_else(|_| quota::quota_key(provider));
    let mut store = QuotaStore::load(paths).unwrap_or_default();
    let change = apply(
        &mut store,
        &key,
        out.verdict,
        out.until,
        out.detail.as_deref(),
    );
    if change.written {
        let _ = store.save(paths);
    }
    ProbeReport {
        provider: key,
        verdict: out.verdict,
        detail: out.detail,
        store: change.what,
    }
}

/// What one probe call found: the verdict, its detail, the stated reopening instant,
/// the raw output the verdict was read from, and the typed rejection window when the
/// output carried a `rate_limit_event` whose final status was rejected.
struct CallOutcome {
    verdict: Verdict,
    detail: Option<String>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    output: Option<String>,
    window: Option<crate::providers::RateLimitWindow>,
}

impl CallOutcome {
    fn without_output(verdict: Verdict, detail: Option<String>) -> Self {
        Self {
            verdict,
            detail,
            until: None,
            output: None,
            window: None,
        }
    }
}

struct StoreChange {
    written: bool,
    what: String,
}

/// Only a quota pause is ever cleared, and a manual one is never touched: the operator
/// paused it, and only `provider resume` undoes that.
fn apply(
    store: &mut QuotaStore,
    key: &str,
    verdict: Verdict,
    until: Option<chrono::DateTime<chrono::Utc>>,
    detail: Option<&str>,
) -> StoreChange {
    let kept = |what: &str| StoreChange {
        written: false,
        what: what.into(),
    };
    if store.record_manually_paused(key) && matches!(verdict, Verdict::Healthy | Verdict::Quota) {
        return kept("manual pause kept");
    }
    match verdict {
        Verdict::Healthy
            if store.record_quota_paused(key)
                || quota::cli_level(key).is_some_and(|p| store.record_quota_paused(&p)) =>
        {
            store.resume_exact(key);
            let mut what = "cleared quota pause".to_string();
            if let Some(parent) = quota::cli_level(key) {
                if store.record_quota_paused(&parent) {
                    store.resume_exact(&parent);
                    what.push_str(&format!(" (and {parent})"));
                }
            }
            StoreChange {
                written: true,
                what,
            }
        }
        Verdict::Quota => {
            let hint = format!("probe: {}", detail.unwrap_or("limit rejected"));
            store.pause_quota_until(key, until, hint);
            store.mark_strong(key);
            StoreChange {
                written: true,
                what: match until {
                    Some(u) => format!("paused until {}", u.to_rfc3339()),
                    None => "paused (quota)".into(),
                },
            }
        }
        _ => kept("unchanged"),
    }
}

fn call(paths: &SparPaths, provider: &str, dry: bool) -> CallOutcome {
    let Ok(pref) = ProviderRef::parse(provider) else {
        return CallOutcome::without_output(
            Verdict::Skipped,
            Some(format!("bad provider ref `{provider}`")),
        );
    };
    let Some(adapter) = providers::adapter_named(provider) else {
        return CallOutcome::without_output(
            Verdict::Skipped,
            Some("only cli providers are probed".into()),
        );
    };
    let Some(bin) = adapter.resolve_binary() else {
        return CallOutcome::without_output(Verdict::Skipped, Some("not on PATH".into()));
    };
    let opts = SpawnOpts {
        prompt: PROBE_PROMPT.into(),
        prompt_file: None,
        cwd: paths.project_root.clone(),
        trust: TrustPolicy::Prompt,
        extra_args: vec![],
        session_id: None,
        model: pref.model.clone(),
        effort: None,
        timeout_secs: Some(PROBE_TIMEOUT.as_secs()),
        session_dir: None,
    };
    let mut cmd = adapter.build_headless(&bin, &opts);
    if dry {
        let (program, args) = providers::command_to_parts(&cmd);
        return CallOutcome::without_output(
            Verdict::Skipped,
            Some(format!(
                "dry-run: would run {} {}",
                program.display(),
                args.join(" ")
            )),
        );
    }
    cmd.current_dir(&paths.project_root);
    for (k, v) in adapter.extra_env(&opts) {
        cmd.env(k, v);
    }
    let Some(out) = providers::run_with_timeout(&mut cmd, PROBE_TIMEOUT) else {
        return CallOutcome::without_output(
            Verdict::Failed,
            Some(format!(
                "no answer within {}s (or could not spawn)",
                PROBE_TIMEOUT.as_secs()
            )),
        );
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut decided = decide(out.status.success(), provider, &text);
    decided.output = Some(text);
    decided
}

/// The verdict for finished probe output: the text-only `classify`, upgraded by a
/// typed `rate_limit_event` whose final status is rejected (a raw stream-json line
/// carries no prose for `classify` to read), and downgraded when the stream
/// recovered (a rejected event later cleared by `allowed`, mirroring the dispatch
/// rule that a recovered rejection routes no pause). The typed `resetsAt` wins over
/// the text-scraped instant when both exist.
fn decide(exit_ok: bool, provider: &str, text: &str) -> CallOutcome {
    let typed = crate::process::scan_rate_limit_events(text);
    if let Some(window) = typed.window {
        let (verdict, detail) = classify(exit_ok, text);
        let until = typed
            .resets_at
            .and_then(chrono::DateTime::from_timestamp_secs)
            .or_else(|| {
                (verdict == Verdict::Quota)
                    .then(|| quota::scrape_claude_rate_limits(provider, text))
                    .flatten()
                    .and_then(|(until, _)| until)
            });
        return CallOutcome {
            verdict: Verdict::Quota,
            detail: match detail {
                Some(d) if verdict == Verdict::Quota => Some(d),
                _ => Some(format!("rate_limit_event {window} rejected")),
            },
            until,
            output: None,
            window: Some(window),
        };
    }
    let (verdict, detail) = classify(exit_ok, text);
    if typed.recovered && verdict == Verdict::Quota {
        let detail = (!exit_ok).then(|| last_line(text)).flatten();
        return CallOutcome {
            verdict: if exit_ok {
                Verdict::Healthy
            } else {
                Verdict::Failed
            },
            detail,
            until: None,
            output: None,
            window: None,
        };
    }
    let until = (verdict == Verdict::Quota)
        .then(|| quota::scrape_claude_rate_limits(provider, text))
        .flatten()
        .and_then(|(until, _)| until);
    CallOutcome {
        verdict,
        detail,
        until,
        output: None,
        window: None,
    }
}

fn last_line(output: &str) -> Option<String> {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
}

/// Quota first (the more specific refusal), then auth, then the exit status.
fn classify(exit_ok: bool, output: &str) -> (Verdict, Option<String>) {
    if let Some(line) = QuotaStore::scrape_strong_quota_signal(output) {
        return (Verdict::Quota, Some(line));
    }
    if let Some(line) = crate::backup::scrape_auth_signal(output) {
        return (Verdict::Auth, Some(line));
    }
    if exit_ok {
        return (Verdict::Healthy, None);
    }
    (Verdict::Failed, last_line(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reads_the_real_wordings() {
        assert_eq!(
            classify(
                false,
                "! rate limit  seven_day  rejected\nYou've hit your weekly limit \u{b7} resets 12am (America/New_York)"
            )
            .0,
            Verdict::Quota
        );
        assert_eq!(
            classify(false, "You've hit your usage limit. Try again later.").0,
            Verdict::Quota
        );
        assert_eq!(classify(false, "Not logged in").0, Verdict::Auth);
        assert_eq!(
            classify(
                false,
                r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"OAuth access token has been revoked."}}"#
            )
            .0,
            Verdict::Auth
        );
        assert_eq!(classify(true, "OK\n").0, Verdict::Healthy);
        let (v, d) = classify(false, "starting\nerror: unknown flag --frobnicate\n");
        assert_eq!(v, Verdict::Failed);
        assert_eq!(d.as_deref(), Some("error: unknown flag --frobnicate"));
    }

    #[test]
    fn a_healthy_probe_clears_a_quota_pause_but_never_a_manual_one() {
        let mut store = QuotaStore::default();
        store.pause_quota("cli:codex", "possible quota signal: usage limit");
        let c = apply(&mut store, "cli:codex", Verdict::Healthy, None, None);
        assert!(c.written);
        assert!(store.is_usable("cli:codex"));

        store.pause_manual("cli:grok", None);
        let c = apply(&mut store, "cli:grok", Verdict::Healthy, None, None);
        assert!(!c.written);
        assert_eq!(c.what, "manual pause kept");
        assert!(store.record_manually_paused("cli:grok"));
    }

    #[test]
    fn a_quota_probe_pauses_until_the_stated_reset() {
        let mut store = QuotaStore::default();
        let until = chrono::Utc::now() + chrono::Duration::hours(3);
        let c = apply(
            &mut store,
            "cli:claude",
            Verdict::Quota,
            Some(until),
            Some("rate limit reached"),
        );
        assert!(c.written);
        let q = store.get("cli:claude");
        assert_eq!(q.cooldown_until, Some(until));
        assert!(!store.is_usable("cli:claude"));
    }

    #[test]
    fn a_healthy_model_probe_clears_a_cli_quota_pause_but_never_a_manual_one() {
        let mut store = QuotaStore::default();
        store.pause_quota_until(
            "cli:agy@flash",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "flash limit",
        );
        store.pause_quota_until(
            "cli:agy@pro",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "pro limit",
        );
        store.pause_quota_until(
            "cli:agy",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "whole login limited",
        );
        let c = apply(&mut store, "cli:agy@flash", Verdict::Healthy, None, None);
        assert!(c.written);
        assert!(store.is_usable("cli:agy@flash"));
        assert!(store.is_usable("cli:agy"));
        assert!(
            !store.is_usable("cli:agy@pro"),
            "an independently paused sibling is not cleared with the parent"
        );

        let mut store = QuotaStore::default();
        store.pause_manual("cli:agy@flash", None);
        let c = apply(&mut store, "cli:agy@flash", Verdict::Healthy, None, None);
        assert!(!c.written);
        assert_eq!(c.what, "manual pause kept");

        let mut store = QuotaStore::default();
        store.pause_quota("cli:agy@flash", "flash limit");
        store.pause_manual("cli:agy", None);
        let c = apply(&mut store, "cli:agy@flash", Verdict::Healthy, None, None);
        assert!(c.written);
        assert!(
            !store.is_usable("cli:agy@flash"),
            "the model still reads held through the CLI-level manual pause"
        );
        assert!(!store.is_usable("cli:agy@pro"));
        assert!(store.record_manually_paused("cli:agy"));
        assert_eq!(
            store.record("cli:agy@flash").status,
            crate::quota::ProviderStatus::Available,
            "the probe cleared the model's own quota pause, not the parent manual one"
        );

        let mut store = QuotaStore::default();
        store.pause_quota_until(
            "cli:agy",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "legacy login-wide pause, no model records",
        );
        let c = apply(&mut store, "cli:agy@flash", Verdict::Healthy, None, None);
        assert!(c.written);
        assert!(store.is_usable("cli:agy@flash"));
        assert!(store.is_usable("cli:agy@pro"));
    }

    #[test]
    fn a_quota_probe_records_the_model_pause_past_a_cli_manual_pause() {
        let mut store = QuotaStore::default();
        store.pause_manual("cli:agy", None);
        let c = apply(
            &mut store,
            "cli:agy@flash",
            Verdict::Quota,
            None,
            Some("limit reached"),
        );
        assert!(c.written);
        assert!(!store.is_usable("cli:agy@flash"));
        let rec = store.record("cli:agy@flash");
        assert_eq!(rec.status, crate::quota::ProviderStatus::PausedQuota);
        assert!(
            rec.strong,
            "the model's own quota signal is recorded despite the CLI-level manual pause"
        );
        store.resume("cli:agy");
        assert!(
            store.is_usable("cli:agy@flash"),
            "a CLI-level resume clears every model key by design"
        );
        assert!(store.is_usable("cli:agy@pro"));
    }

    #[test]
    fn auth_and_failed_probes_leave_the_store_alone() {
        for v in [Verdict::Auth, Verdict::Failed, Verdict::Skipped] {
            let mut store = QuotaStore::default();
            store.pause_quota("cli:codex", "q");
            let before = store.get("cli:codex").updated_at;
            let c = apply(&mut store, "cli:codex", v, None, Some("x"));
            assert!(!c.written, "{v:?}");
            assert_eq!(store.get("cli:codex").updated_at, before);
        }
    }

    #[test]
    fn decide_upgrades_a_raw_stream_json_rejection() {
        let reset = 1_788_000_000i64;
        let text = format!(
            "{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_opus\",\"resetsAt\":{reset}}}}}"
        );
        let out = decide(false, "cli:claude@opus", &text);
        assert_eq!(out.verdict, Verdict::Quota);
        assert_eq!(
            out.window,
            Some(crate::providers::RateLimitWindow::SevenDayOpus)
        );
        assert_eq!(
            out.until,
            chrono::DateTime::from_timestamp_secs(reset),
            "the typed resetsAt sets the instant prose could never state"
        );
    }

    #[test]
    fn decide_keeps_prose_detail_when_both_fire() {
        let text = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"five_hour\",\"resetsAt\":1788000000}}\n\
            You've hit your weekly limit";
        let out = decide(false, "cli:claude@opus", text);
        assert_eq!(out.verdict, Verdict::Quota);
        assert_eq!(
            out.window,
            Some(crate::providers::RateLimitWindow::FiveHour)
        );
        assert!(
            out.detail
                .as_deref()
                .is_some_and(|d| d.contains("weekly limit")),
            "the prose line stays the detail: {:?}",
            out.detail
        );
    }

    #[test]
    fn decide_downgrades_a_recovered_stream() {
        let text = "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"seven_day_opus\"}}\n\
            {\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"allowed\",\"rateLimitType\":\"seven_day_opus\"}}\n\
            You've hit your weekly limit";
        let out = decide(false, "cli:claude@opus", text);
        assert_eq!(
            out.verdict,
            Verdict::Failed,
            "a recovered rejection routes no pause even with prose in the tail"
        );
        assert_eq!(out.window, None);
        let out = decide(true, "cli:claude@opus", text);
        assert_eq!(out.verdict, Verdict::Healthy);
    }

    #[test]
    fn quota_key_selection_follows_the_output_named_bucket() {
        use crate::providers::QuotaSignal;
        let key_for = |provider: &str, output: &str| {
            ProviderRef::parse(provider)
                .unwrap()
                .rejection_key(QuotaSignal::Text(output))
        };
        assert_eq!(
            key_for("cli:claude@opus", "You've hit your weekly limit"),
            "cli:claude"
        );
        assert_eq!(
            key_for("cli:claude@opus", "! rate limit  five_hour  rejected"),
            "cli:claude"
        );
        assert_eq!(
            key_for("cli:claude@opus", "You've reached your Opus limit"),
            "cli:claude@opus"
        );
        assert_eq!(
            key_for("cli:claude@sonnet", "You've reached your Opus limit"),
            "cli:claude@opus"
        );
        assert_eq!(
            key_for("cli:claude@haiku", "You've reached your Opus limit"),
            "cli:claude@opus",
            "the named bucket wins over the reporting seat"
        );
    }

    #[test]
    fn a_dry_probe_spawns_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let r = probe(&paths, "cli:claude", true);
        assert_eq!(r.verdict, Verdict::Skipped);
        assert_eq!(r.store, "unchanged");
        assert!(!paths.quota_file().exists());
    }
}
