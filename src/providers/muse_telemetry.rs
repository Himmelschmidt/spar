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
//!
//! **`cached_tokens` is a subset of `input_tokens` here**, not a sibling of it as in
//! claude's disjoint `input` / `cache_read` pair, so it is never added to a total
//! (O47). [`apply`] normalizes the pair to the Anthropic shape on the way into
//! `StreamStats`, which is what keeps the published
//! `billed = input + cache_read + cache_write + output` identity true for muse. Records
//! also carry `reported`, which is the field that says whether Meta bills them; the
//! `provider` and `compaction` families set it, `tool` and `reminder` do not and report
//! only zeros.

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
    /// Part of `input_tokens`, not additional to it.
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    /// Largest single billed call, prompt side: the context gauge's peak.
    pub peak_input_tokens: u64,
    /// Number of billed usage records seen; zero means nothing was recovered.
    pub records: u64,
    /// `usage_family == "tool"` records in the main session log, one per tool call.
    pub tool_records: u32,
}

impl Usage {
    fn absorb(&mut self, q: &serde_json::Value) {
        let n = |k: &str| q.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let input = n("input_tokens");
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(n("output_tokens"));
        self.cached_tokens = self.cached_tokens.saturating_add(n("cached_tokens"));
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(n("reasoning_tokens"));
        self.peak_input_tokens = self.peak_input_tokens.max(input);
        self.records += 1;
    }

    /// What Meta charges for: `cached_tokens` is inside `input_tokens` and reasoning is
    /// billed as output.
    pub fn billed_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.reasoning_tokens)
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

/// Sum billed usage across a session log and every subagent session under it.
///
/// Tool records are counted from the **main** log only: a subagent's tool calls never
/// reach the parent's stdout stream, so including them would overshoot the stream's own
/// count instead of recovering it.
pub fn collect(session_dir: &Path) -> Usage {
    let mut usage = Usage::default();
    absorb_log(&session_dir.join("session.jsonl"), true, &mut usage);
    if let Ok(rd) = std::fs::read_dir(session_dir.join("subagent")) {
        for child in rd.flatten() {
            absorb_log(&child.path().join("session.jsonl"), false, &mut usage);
        }
    }
    usage
}

fn absorb_log(path: &Path, main: bool, usage: &mut Usage) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    absorb_text(&text, main, usage);
}

fn absorb_text(text: &str, main: bool, usage: &mut Usage) {
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
        let family = rec.get("usage_family").and_then(|x| x.as_str());
        if main && family == Some("tool") {
            usage.tool_records += 1;
        }
        let Some(q) = rec.get("quantity") else {
            continue;
        };
        // `reported` is the record's own answer to "does Meta bill this", so it also
        // picks up families beyond `provider`: `compaction` is billed and an allowlist
        // silently dropped it. `tool` and `reminder` set it false and carry only zeros.
        if q.get("reported").and_then(|x| x.as_bool()) != Some(true) {
            continue;
        }
        usage.absorb(q);
    }
}

/// Incremental reader over a muse session tree that is still being written.
///
/// muse appends `session.jsonl` as the turn runs, verified by watching one: 92 billed
/// `goal_usage_attribution` records and 1.47M billed tokens accumulated monotonically
/// across a 190-second dispatch while the process was still alive. That is what makes a
/// mid-dispatch token budget possible for muse at all, since [`enrich`] runs only after
/// the child is waited on.
///
/// Each poll reads only the bytes appended since the last one and stops at the final
/// newline, so a record half-written when the read lands is picked up whole on the next
/// one. The subagent directory is rescanned every poll: muse fans out new sessions
/// mid-turn and their spend is spend.
pub struct LiveUsage {
    dir: PathBuf,
    offsets: std::collections::HashMap<PathBuf, u64>,
    usage: Usage,
}

impl LiveUsage {
    /// `None` until muse has created the session directory, which lags the session id
    /// spar reads out of the stream by a moment.
    pub fn open(session_id: &str) -> Option<Self> {
        let root = sessions_root()?;
        Some(Self::at(session_dir(&root, session_id)?))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            offsets: std::collections::HashMap::new(),
            usage: Usage::default(),
        }
    }

    /// Cumulative billed tokens for the session and every subagent under it, as of now.
    pub fn billed(&mut self) -> u64 {
        let main = self.dir.join("session.jsonl");
        self.absorb_new(&main, true);
        let mut children = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.dir.join("subagent")) {
            for child in rd.flatten() {
                children.push(child.path().join("session.jsonl"));
            }
        }
        for child in children {
            self.absorb_new(&child, false);
        }
        self.usage.billed_tokens()
    }

    fn absorb_new(&mut self, path: &Path, main: bool) {
        use std::io::{Read, Seek, SeekFrom};
        let offset = self.offsets.get(path).copied().unwrap_or(0);
        let Ok(mut f) = std::fs::File::open(path) else {
            return;
        };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return;
        }
        let Some(end) = buf.rfind('\n') else { return };
        absorb_text(&buf[..end], main, &mut self.usage);
        self.offsets
            .insert(path.to_path_buf(), offset + end as u64 + 1);
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
    apply(stats, &collect(&dir));
}

/// Overwrite a slot's token counters from recovered session usage.
pub fn apply(stats: &mut StreamStats, usage: &Usage) {
    // The tool count survives a stream that never reported one: a killed round, or a
    // slot whose last log line came from stderr.
    stats.tools = stats.tools.max(usage.tool_records);
    if usage.records == 0 {
        return;
    }
    // muse's `cached_tokens` is a *component of* `input_tokens`, while `StreamStats`
    // uses the Anthropic convention where `cache_read_tokens` is disjoint from it. Storing
    // the raw pair would leave the published identity
    // `billed = input + cache_read + cache_write + output` reading ~2x the truth on a
    // cache-heavy slot, so `input_tokens` is reduced to the uncached remainder here and
    // the two agree by construction.
    stats.input_tokens = usage.input_tokens.saturating_sub(usage.cached_tokens);
    // Reasoning tokens are billed as output and are not included in `output_tokens`.
    stats.output_tokens = usage.output_tokens.saturating_add(usage.reasoning_tokens);
    stats.cache_read_tokens = usage.cached_tokens;
    stats.billed_tokens = usage.billed_tokens();
    // The gauge wants the biggest single call, not the run's cumulative input, and each
    // record's `input_tokens` is already the whole prompt (cached part included).
    stats.context_tokens = usage.peak_input_tokens;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn usage_line(family: &str, input: u64, output: u64, cached: u64, reasoning: u64) -> String {
        // Every family carries `reported`; only the billed ones set it true.
        let reported = matches!(family, "provider" | "compaction");
        serde_json::json!({
            "payload_type": "runtime.session",
            "payload": {"event": {"kind": "goal_usage_attribution", "record": {
                "usage_family": family,
                "quantity": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cached_tokens": cached,
                    "reasoning_tokens": reasoning,
                    "reported": reported,
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
                // `tool` and `reminder` are unreported and all-zero: they must not
                // inflate the record count, but tool records are the tool tally.
                usage_line("tool", 0, 0, 0, 0),
                usage_line("reminder", 0, 0, 0, 0),
                usage_line("provider", 23351, 21, 23025, 10),
                // Compaction is billed and carries real quantities; the old
                // `usage_family == "provider"` allowlist dropped it.
                usage_line("compaction", 4180, 96, 0, 0),
            ],
        );
        write_session(
            &root.join("subagent/child-a"),
            &[
                usage_line("provider", 3262, 1176, 2801, 1052),
                // A subagent's tools never reach the parent's stdout stream.
                usage_line("tool", 0, 0, 0, 0),
            ],
        );
        write_session(
            &root.join("subagent/child-b"),
            &[usage_line("provider", 9795, 369, 0, 199)],
        );

        let u = collect(&root);
        assert_eq!(
            u.records, 5,
            "four provider records plus the compaction one"
        );
        assert_eq!(u.tool_records, 1, "main log only");
        assert_eq!(u.input_tokens, 23030 + 23351 + 4180 + 3262 + 9795);
        assert_eq!(u.output_tokens, 247 + 21 + 96 + 1176 + 369);
        assert_eq!(u.cached_tokens, 23025 + 2801);
        assert_eq!(u.reasoning_tokens, 164 + 10 + 1052 + 199);
        assert_eq!(u.peak_input_tokens, 23351);
        assert!(
            u.cached_tokens <= u.input_tokens,
            "cached is a subset of input in muse's records, never a sibling of it"
        );
        assert_eq!(
            u.billed_tokens(),
            u.input_tokens + u.output_tokens + u.reasoning_tokens,
            "cached must not be added on top of the input it is already inside"
        );
    }

    /// muse appends its session log as the turn runs (verified against a live 190s
    /// dispatch: 92 billed records accumulating monotonically while the process was up),
    /// which is the only reason a muse slot can be token-nudged mid-dispatch at all.
    /// What this pins is the tail: each poll must see only what was appended since the
    /// last one, and must not lose a record that was half-written when it read.
    #[test]
    fn live_usage_tails_an_appending_session_log() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("sess");
        std::fs::create_dir_all(dir.join("subagent/child")).unwrap();
        let main = dir.join("session.jsonl");
        let append = |path: &std::path::Path, s: &str| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            f.write_all(s.as_bytes()).unwrap();
        };

        let mut live = LiveUsage::at(dir.clone());
        append(
            &main,
            &format!("{}\n", usage_line("provider", 1000, 10, 0, 5)),
        );
        assert_eq!(live.billed(), 1015);

        // Re-polling the same bytes must not bill them twice.
        assert_eq!(live.billed(), 1015);

        // A record still being written is not lost, just deferred to the next poll.
        let partial = usage_line("provider", 2000, 20, 0, 0);
        let cut = partial.len() / 2;
        append(&main, &partial[..cut]);
        assert_eq!(live.billed(), 1015, "half a line bills nothing");
        append(&main, &format!("{}\n", &partial[cut..]));
        assert_eq!(live.billed(), 1015 + 2020);

        // Subagents muse fans out mid-turn are real spend and are picked up.
        append(
            &dir.join("subagent/child/session.jsonl"),
            &format!("{}\n", usage_line("provider", 300, 3, 0, 0)),
        );
        assert_eq!(live.billed(), 1015 + 2020 + 303);
    }

    #[test]
    fn enrich_does_not_double_count_the_cached_subset() {
        // 40,258 provider records across the local corpus, zero with cached > input:
        // the generic input + cache_read + output context sum roughly doubled every
        // muse figure. Real numbers, from run 2736a545's session tree.
        let mut stats = StreamStats {
            tools: 0,
            ..Default::default()
        };
        let usage = Usage {
            input_tokens: 33_575_824,
            output_tokens: 1_249_159,
            cached_tokens: 32_442_851,
            reasoning_tokens: 0,
            peak_input_tokens: 180_000,
            records: 669,
            tool_records: 191,
        };
        apply(&mut stats, &usage);
        assert_eq!(
            stats.input_tokens,
            33_575_824 - 32_442_851,
            "the uncached remainder, so the cached part is not billed twice"
        );
        assert_eq!(stats.output_tokens, 1_249_159);
        assert_eq!(stats.cache_read_tokens, 32_442_851);
        assert_eq!(stats.billed_tokens, usage.billed_tokens());
        // The identity `skills/core.md` publishes, holding for muse without a carve-out.
        assert_eq!(
            stats.billed_tokens,
            stats.input_tokens
                + stats.cache_read_tokens
                + stats.cache_write_tokens
                + stats.output_tokens
        );
        assert_eq!(
            stats.context_tokens, 180_000,
            "peak call, not the run total"
        );
        assert_eq!(
            stats.tools, 191,
            "tool count survives a stream that lost it"
        );
    }

    #[test]
    fn enrich_keeps_a_stream_tool_count_that_beats_the_log() {
        let mut stats = StreamStats {
            tools: 629,
            ..Default::default()
        };
        apply(
            &mut stats,
            &Usage {
                tool_records: 628,
                ..Default::default()
            },
        );
        assert_eq!(stats.tools, 629);
        // No billed records: the token fields stay untouched rather than zeroed.
        assert_eq!(stats.input_tokens, 0);
        assert_eq!(stats.billed_tokens, 0);
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
