//! Recover real token usage for a `cli:muse` slot.
//!
//! `muse exec --json` emits an event envelope on stdout but **no** token counts. Usage
//! is recorded only in muse's own session log, as `runtime.session` records whose
//! `event.kind` is `goal_usage_attribution`:
//!
//! ```text
//! {"payload_type":"runtime.session","payload":{"event":{"kind":"goal_usage_attribution",
//!   "record":{"usage_family":"provider","quantity":{"input_tokens":23030,"output_tokens":247,
//!             "cached_tokens":0,"reasoning_tokens":164}}}}}
//! ```
//!
//! Logs live at `${XDG_DATA_HOME:-~/.local/share}/muse/sessions/YYYY/MM/DD/<session-id>/`,
//! with a `subagent/<id>/session.jsonl` per child session muse spawned. The children are
//! real spend (a trivial one-file task fanned out to six of them), so they are summed too.
//!
//! **Input is summed, not maxed.** Each record is one billed model call that re-sends the
//! context, so the sum is what Meta charges; the `max` convention the other adapters use
//! reports final context size instead (O35 in DECISIONS.md).

use crate::process::StreamStats;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// `${XDG_DATA_HOME:-$HOME/.local/share}/muse/sessions`, if it exists.
pub fn sessions_root() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    let r = base.join("muse/sessions");
    r.is_dir().then_some(r)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    /// Number of usage records seen; zero means nothing was recovered.
    pub records: u64,
}

impl Usage {
    fn absorb(&mut self, q: &serde_json::Value) {
        let n = |k: &str| q.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        self.input_tokens = self.input_tokens.saturating_add(n("input_tokens"));
        self.output_tokens = self.output_tokens.saturating_add(n("output_tokens"));
        self.cached_tokens = self.cached_tokens.saturating_add(n("cached_tokens"));
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(n("reasoning_tokens"));
        self.records += 1;
    }
}

/// The session directory for `session_id`, found under `sessions/YYYY/MM/DD/`.
pub fn session_dir(root: &Path, session_id: &str) -> Option<PathBuf> {
    let dirs = |p: &Path| -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(p) else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect()
    };
    for year in dirs(root) {
        for month in dirs(&year) {
            for day in dirs(&month) {
                let cand = day.join(session_id);
                if cand.is_dir() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// Sum provider usage across a session log and every subagent session under it.
pub fn collect(session_dir: &Path) -> Usage {
    let mut usage = Usage::default();
    absorb_log(&session_dir.join("session.jsonl"), &mut usage);
    if let Ok(rd) = std::fs::read_dir(session_dir.join("subagent")) {
        for child in rd.flatten() {
            absorb_log(&child.path().join("session.jsonl"), &mut usage);
        }
    }
    usage
}

fn absorb_log(path: &Path, usage: &mut Usage) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        // Cheap prefilter: these logs run to hundreds of KB and only a handful of lines
        // carry usage.
        if !line.contains("goal_usage_attribution") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let rec = v.pointer("/payload/event/record");
        let Some(rec) = rec else { continue };
        // `usage_family` also takes non-provider values (e.g. `tool`) that report zeros.
        if rec.get("usage_family").and_then(|x| x.as_str()) != Some("provider") {
            continue;
        }
        if let Some(q) = rec.get("quantity") {
            usage.absorb(q);
        }
    }
}

/// Rewrite a muse slot's stats from its session log. The stdout stream has no token
/// counts at all, so without this every `cli:muse` run reports zero spend.
pub fn enrich(stats: &mut StreamStats) {
    let Some(session_id) = stats.session_id.clone() else {
        return;
    };
    let Some(root) = sessions_root() else { return };
    let Some(dir) = session_dir(&root, &session_id) else {
        return;
    };
    let usage = collect(&dir);
    if usage.records == 0 {
        return;
    }
    stats.input_tokens = usage.input_tokens;
    // Reasoning tokens are billed as output and are not included in `output_tokens`.
    stats.output_tokens = usage.output_tokens.saturating_add(usage.reasoning_tokens);
    stats.cache_read_tokens = usage.cached_tokens;
    stats.touch_context();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn usage_line(family: &str, input: u64, output: u64, cached: u64, reasoning: u64) -> String {
        serde_json::json!({
            "payload_type": "runtime.session",
            "payload": {"event": {"kind": "goal_usage_attribution", "record": {
                "usage_family": family,
                "quantity": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cached_tokens": cached,
                    "reasoning_tokens": reasoning,
                    "unit": "tokens"
                }
            }}}
        })
        .to_string()
    }

    fn write_session(dir: &Path, lines: &[String]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut text = String::from("{\"payload_type\":\"run.lifecycle.started\"}\n");
        for l in lines {
            text.push_str(l);
            text.push('\n');
        }
        std::fs::write(dir.join("session.jsonl"), text).unwrap();
    }

    #[test]
    fn sums_input_across_steps_and_subagents() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("sessions/2026/08/14/sess-1");
        write_session(
            &root,
            &[
                usage_line("provider", 23030, 247, 0, 164),
                // Non-provider families report zeros and must not inflate the record count.
                usage_line("tool", 0, 0, 0, 0),
                usage_line("provider", 23351, 21, 23025, 10),
            ],
        );
        write_session(
            &root.join("subagent/child-a"),
            &[usage_line("provider", 3262, 1176, 2801, 1052)],
        );
        write_session(
            &root.join("subagent/child-b"),
            &[usage_line("provider", 9795, 369, 0, 199)],
        );

        let u = collect(&root);
        assert_eq!(u.records, 4);
        assert_eq!(u.input_tokens, 23030 + 23351 + 3262 + 9795);
        assert_eq!(u.output_tokens, 247 + 21 + 1176 + 369);
        assert_eq!(u.cached_tokens, 23025 + 2801);
        assert_eq!(u.reasoning_tokens, 164 + 10 + 1052 + 199);
    }

    #[test]
    fn finds_session_dir_under_date_path() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("sessions");
        write_session(&root.join("2026/08/14/sess-2"), &[]);
        assert_eq!(
            session_dir(&root, "sess-2"),
            Some(root.join("2026/08/14/sess-2"))
        );
        assert_eq!(session_dir(&root, "missing"), None);
    }

    #[test]
    fn missing_log_yields_no_records() {
        let tmp = tempdir().unwrap();
        assert_eq!(collect(&tmp.path().join("nope")).records, 0);
    }
}
