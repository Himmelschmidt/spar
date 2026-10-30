//! Account utilisation for `spar provider list --usage` (O132), read only where a CLI
//! exposes it locally and without a model turn: `muse usage --json`, and the newest
//! `rate_limits` record codex writes into its own session logs. Everything else says
//! why it has nothing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const USAGE_SCHEMA: u32 = 1;

const MUSE_USAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many of the newest codex session files to search for a `rate_limits` record.
const CODEX_FILES_SCANNED: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    /// The source exists for this adapter but gave nothing this time.
    Unavailable,
    /// spar has no local, non-billed source for this adapter.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    /// `5h`, `weekly`, or `<n>m` for a window of another length.
    pub name: String,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub window_minutes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderUsage {
    pub schema_version: u32,
    pub status: UsageStatus,
    pub source: Option<String>,
    /// When the source measured it. A codex record is as old as the last codex turn.
    pub as_of: Option<DateTime<Utc>>,
    pub windows: Vec<Window>,
    pub plan: Option<String>,
    /// Codex credit balance as codex printed it.
    pub credits: Option<String>,
    pub message: Option<String>,
}

impl ProviderUsage {
    fn empty(status: UsageStatus, message: impl Into<String>) -> Self {
        Self {
            schema_version: USAGE_SCHEMA,
            status,
            source: None,
            as_of: None,
            windows: Vec::new(),
            plan: None,
            credits: None,
            message: Some(message.into()),
        }
    }
}

fn window_name(minutes: Option<u64>, fallback: &str) -> String {
    match minutes {
        Some(300) => "5h".into(),
        Some(10080) => "weekly".into(),
        Some(m) => format!("{m}m"),
        None => fallback.into(),
    }
}

fn from_ms(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms)
}

#[derive(Deserialize)]
struct MuseWindow {
    #[serde(alias = "resetsAtMs")]
    resets_at_ms: Option<i64>,
    #[serde(alias = "usedPercent")]
    used_percent: Option<f64>,
}

#[derive(Deserialize)]
struct MuseAllowance {
    #[serde(alias = "asOfMs")]
    as_of_ms: Option<i64>,
    tier: Option<String>,
    weekly: Option<MuseWindow>,
    window: Option<MuseWindow>,
}

/// `muse usage --json` (camelCase), or the `allowance` of a `run.usage` stream record
/// (snake_case): the same object.
pub fn parse_muse(text: &str) -> Option<ProviderUsage> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let a: MuseAllowance = serde_json::from_value(v.get("allowance")?.clone()).ok()?;
    let mut windows = Vec::new();
    for (name, minutes, w) in [("5h", 300, &a.window), ("weekly", 10080, &a.weekly)] {
        let Some(w) = w else { continue };
        let Some(used) = w.used_percent else { continue };
        windows.push(Window {
            name: name.into(),
            used_percent: used,
            resets_at: w.resets_at_ms.and_then(from_ms),
            window_minutes: Some(minutes),
        });
    }
    Some(ProviderUsage {
        schema_version: USAGE_SCHEMA,
        status: UsageStatus::Ok,
        source: Some("muse usage --json".into()),
        as_of: a.as_of_ms.and_then(from_ms),
        windows,
        plan: a.tier,
        credits: None,
        message: None,
    })
}

fn muse(bin: &Path) -> ProviderUsage {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["usage", "--json"]);
    let Some(out) = super::run_with_timeout(&mut cmd, MUSE_USAGE_TIMEOUT) else {
        return ProviderUsage::empty(
            UsageStatus::Unavailable,
            "muse usage --json did not answer in 10s",
        );
    };
    let text = String::from_utf8_lossy(&out.stdout);
    match parse_muse(&text) {
        Some(u) if out.status.success() => u,
        _ => ProviderUsage::empty(
            UsageStatus::Unavailable,
            format!(
                "muse usage --json exited {} without an allowance",
                out.status
                    .code()
                    .map_or("by signal".into(), |c| c.to_string())
            ),
        ),
    }
}

#[derive(Deserialize)]
struct CodexWindow {
    used_percent: Option<f64>,
    window_minutes: Option<u64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct CodexCredits {
    balance: Option<String>,
    #[serde(default)]
    unlimited: bool,
}

#[derive(Deserialize)]
struct CodexLimits {
    primary: Option<CodexWindow>,
    secondary: Option<CodexWindow>,
    credits: Option<CodexCredits>,
    plan_type: Option<String>,
}

/// The last line of a codex session log that carries a non-null `rate_limits`.
pub fn parse_codex_session(text: &str, file: &Path) -> Option<ProviderUsage> {
    text.lines().rev().find_map(|line| {
        if !line.contains("\"rate_limits\"") {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let limits = v.pointer("/payload/rate_limits").filter(|l| !l.is_null())?;
        let limits: CodexLimits = serde_json::from_value(limits.clone()).ok()?;
        let mut windows = Vec::new();
        for (fallback, w) in [
            ("primary", &limits.primary),
            ("secondary", &limits.secondary),
        ] {
            let Some(w) = w else { continue };
            let Some(used) = w.used_percent else { continue };
            windows.push(Window {
                name: window_name(w.window_minutes, fallback),
                used_percent: used,
                resets_at: w.resets_at.and_then(|s| DateTime::from_timestamp(s, 0)),
                window_minutes: w.window_minutes,
            });
        }
        let credits = limits.credits.and_then(|c| {
            if c.unlimited {
                Some("unlimited".into())
            } else {
                c.balance
            }
        });
        Some(ProviderUsage {
            schema_version: USAGE_SCHEMA,
            status: UsageStatus::Ok,
            source: Some(format!("codex session log {}", file.display())),
            as_of: v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.to_utc()),
            windows,
            plan: limits.plan_type,
            credits,
            message: None,
        })
    })
}

/// Session logs live at `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, so a reverse
/// sort of the full paths is newest first without a stat per file.
fn newest_sessions(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root.join("sessions"), &mut files);
    files.sort();
    files.reverse();
    files.truncate(CODEX_FILES_SCANNED);
    files
}

fn codex_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    dirs::home_dir().map(|h| h.join(".codex"))
}

fn codex() -> ProviderUsage {
    let Some(home) = codex_home() else {
        return ProviderUsage::empty(UsageStatus::Unavailable, "no home directory");
    };
    for file in newest_sessions(&home) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(u) = parse_codex_session(&text, &file) {
            return u;
        }
    }
    ProviderUsage::empty(
        UsageStatus::Unavailable,
        format!(
            "no rate_limits record in the newest codex sessions under {}",
            home.join("sessions").display()
        ),
    )
}

/// One line for `provider list --usage`.
pub fn describe(u: &ProviderUsage) -> String {
    if u.status != UsageStatus::Ok {
        return u.message.clone().unwrap_or_default();
    }
    let mut parts: Vec<String> = u
        .windows
        .iter()
        .map(|w| {
            let reset = w
                .resets_at
                .map(|r| format!(" (resets {})", r.format("%a %H:%M UTC")))
                .unwrap_or_default();
            format!("{} {:.0}%{reset}", w.name, w.used_percent)
        })
        .collect();
    if let Some(c) = &u.credits {
        parts.push(format!("credits {c}"));
    }
    if let Some(at) = u.as_of {
        parts.push(format!("as of {}", at.format("%Y-%m-%d %H:%M UTC")));
    }
    parts.join(", ")
}

/// Utilisation for the adapter `name` (`muse`, `cli:codex`, ...). `bin` is the binary
/// `provider list` resolved; `None` when the adapter is not installed.
pub fn probe(name: &str, bin: Option<&str>) -> ProviderUsage {
    let bare = name.strip_prefix("cli:").unwrap_or(name);
    match (bare, bin) {
        ("muse", Some(bin)) => muse(Path::new(bin)),
        ("codex", _) => codex(),
        ("muse", None) => ProviderUsage::empty(UsageStatus::Unavailable, "muse is not installed"),
        ("claude", _) => ProviderUsage::empty(
            UsageStatus::Unsupported,
            "claude has no local, documented, non-billed usage source; its /usage reads an \
             account endpoint spar does not call",
        ),
        _ => ProviderUsage::empty(
            UsageStatus::Unsupported,
            format!("spar reads no usage for {bare}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MUSE_CLI: &str = r#"{"allowance":{"asOfMs":1793397310429,"source":"mint","tier":"Muse Code Max Usage","weekly":{"resetsAtMs":1793577600000,"usedPercent":27},"window":{"resetsAtMs":1793404987000,"usedPercent":4}},"hint":null,"reason":null,"schemaVersion":1}"#;

    const MUSE_STREAM: &str = r#"{"schema_version":1,"payload_type":"run.usage","payload":{"allowance":{"as_of_ms":1793384293153,"source":"stream","tier":null,"weekly":{"resets_at_ms":1793577600000,"used_percent":25},"window":{"resets_at_ms":1793386985000,"used_percent":2}}}}"#;

    const CODEX_LINE: &str = r#"{"timestamp":"2026-10-30T21:47:05.485Z","type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":{"limit_id":"codex","primary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1793774154},"secondary":{"used_percent":12.5,"window_minutes":300,"resets_at":1793400000},"credits":{"has_credits":true,"unlimited":false,"balance":"20047.55"},"plan_type":"pro"}}}"#;

    #[test]
    fn muse_cli_json_reads_both_windows() {
        let u = parse_muse(MUSE_CLI).unwrap();
        assert_eq!(u.status, UsageStatus::Ok);
        assert_eq!(u.plan.as_deref(), Some("Muse Code Max Usage"));
        assert_eq!(u.as_of, from_ms(1793397310429));
        assert_eq!(
            u.windows,
            vec![
                Window {
                    name: "5h".into(),
                    used_percent: 4.0,
                    resets_at: from_ms(1793404987000),
                    window_minutes: Some(300),
                },
                Window {
                    name: "weekly".into(),
                    used_percent: 27.0,
                    resets_at: from_ms(1793577600000),
                    window_minutes: Some(10080),
                },
            ]
        );
    }

    #[test]
    fn muse_stream_allowance_is_the_same_object() {
        let payload: serde_json::Value = serde_json::from_str(MUSE_STREAM).unwrap();
        let u = parse_muse(&payload["payload"].to_string()).unwrap();
        assert_eq!(u.windows[1].used_percent, 25.0);
        assert!(parse_muse("{}").is_none());
        assert!(parse_muse("not json").is_none());
    }

    #[test]
    fn codex_takes_the_last_record_with_rate_limits() {
        let older = CODEX_LINE.replace("100.0", "50.0");
        let null = r#"{"timestamp":"2026-10-30T21:48:00Z","payload":{"type":"token_count","rate_limits":null}}"#;
        let text = format!("{older}\n{CODEX_LINE}\n{null}\n{{\"payload\":{{}}}}\n");
        let u = parse_codex_session(&text, Path::new("/x.jsonl")).unwrap();
        assert_eq!(u.windows[0].name, "weekly");
        assert_eq!(u.windows[0].used_percent, 100.0);
        assert_eq!(
            u.windows[0].resets_at,
            DateTime::from_timestamp(1793774154, 0)
        );
        assert_eq!(u.windows[1].name, "5h");
        assert_eq!(u.credits.as_deref(), Some("20047.55"));
        assert_eq!(u.plan.as_deref(), Some("pro"));
        assert_eq!(
            u.as_of,
            Some(
                DateTime::parse_from_rfc3339("2026-10-30T21:47:05.485Z")
                    .unwrap()
                    .to_utc()
            )
        );
        assert!(parse_codex_session(null, Path::new("/x")).is_none());
    }

    #[test]
    fn newest_codex_session_wins_and_files_without_limits_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let day = |d: &str| tmp.path().join("sessions/2026/10").join(d);
        std::fs::create_dir_all(day("29")).unwrap();
        std::fs::create_dir_all(day("30")).unwrap();
        std::fs::write(day("29").join("rollout-a.jsonl"), format!("{CODEX_LINE}\n")).unwrap();
        std::fs::write(
            day("30").join("rollout-b.jsonl"),
            CODEX_LINE.replace("100.0", "42.0") + "\n",
        )
        .unwrap();
        std::fs::write(day("30").join("rollout-c.jsonl"), "{\"payload\":{}}\n").unwrap();
        let files = newest_sessions(tmp.path());
        assert!(files[0].ends_with("30/rollout-c.jsonl"));
        let u = files
            .iter()
            .find_map(|f| parse_codex_session(&std::fs::read_to_string(f).unwrap(), f))
            .unwrap();
        assert_eq!(u.windows[0].used_percent, 42.0);
    }

    #[test]
    fn claude_and_unknown_adapters_say_why_they_have_nothing() {
        let c = probe("cli:claude", Some("/bin/claude"));
        assert_eq!(c.status, UsageStatus::Unsupported);
        assert!(c.message.unwrap().contains("non-billed"));
        assert_eq!(probe("grok", None).status, UsageStatus::Unsupported);
        assert_eq!(probe("muse", None).status, UsageStatus::Unavailable);
    }
}
