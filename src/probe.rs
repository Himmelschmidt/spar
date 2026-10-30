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
/// is spawned and nothing is written.
pub fn probe(paths: &SparPaths, provider: &str, dry: bool) -> ProbeReport {
    let key = quota::normalize_key(provider);
    let (verdict, detail, until) = call(paths, provider, dry);
    let mut store = QuotaStore::load(paths).unwrap_or_default();
    let change = apply(&mut store, &key, verdict, until, detail.as_deref());
    if change.written {
        let _ = store.save(paths);
    }
    ProbeReport {
        provider: key,
        verdict,
        detail,
        store: change.what,
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
    if store.is_manually_paused(key) && matches!(verdict, Verdict::Healthy | Verdict::Quota) {
        return kept("manual pause kept");
    }
    match verdict {
        Verdict::Healthy if store.is_quota_paused(key) => {
            store.resume(key);
            StoreChange {
                written: true,
                what: "cleared quota pause".into(),
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

fn call(
    paths: &SparPaths,
    provider: &str,
    dry: bool,
) -> (
    Verdict,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
) {
    let Ok(pref) = ProviderRef::parse(provider) else {
        return (
            Verdict::Skipped,
            Some(format!("bad provider ref `{provider}`")),
            None,
        );
    };
    let Some(adapter) = providers::adapter_named(provider) else {
        return (
            Verdict::Skipped,
            Some("only cli providers are probed".into()),
            None,
        );
    };
    let Some(bin) = adapter.resolve_binary() else {
        return (Verdict::Skipped, Some("not on PATH".into()), None);
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
        return (
            Verdict::Skipped,
            Some(format!(
                "dry-run: would run {} {}",
                program.display(),
                args.join(" ")
            )),
            None,
        );
    }
    cmd.current_dir(&paths.project_root);
    for (k, v) in adapter.extra_env(&opts) {
        cmd.env(k, v);
    }
    let Some(out) = providers::run_with_timeout(&mut cmd, PROBE_TIMEOUT) else {
        return (
            Verdict::Failed,
            Some(format!(
                "no answer within {}s (or could not spawn)",
                PROBE_TIMEOUT.as_secs()
            )),
            None,
        );
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let (verdict, detail) = classify(out.status.success(), &text);
    let until = (verdict == Verdict::Quota)
        .then(|| quota::scrape_claude_rate_limits(provider, &text))
        .flatten()
        .and_then(|(_, until, _)| until);
    (verdict, detail, until)
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
    let last = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect());
    (Verdict::Failed, last)
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
        assert!(store.is_manually_paused("cli:grok"));
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
    fn a_dry_probe_spawns_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let r = probe(&paths, "cli:claude", true);
        assert_eq!(r.verdict, Verdict::Skipped);
        assert_eq!(r.store, "unchanged");
        assert!(!paths.quota_file().exists());
    }
}
