//! Typed domain records for the structured views (feature 010): a run's log stream,
//! its activity timeline, and its planning/review documents, each parsed into typed
//! fields with an immutable source identity — never a pre-rendered `head`/`body`
//! string. `Record` is the one shared *render row* every view paints through; it is
//! derived from a domain type (`LogRecord`, `ActivityRecord`) or built directly (for
//! documents, which have no richer domain shape than heading + body) at paint time.
//! This module is pure: no `Frame`, no disk I/O.

use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::time::Duration;

/// One tool kind, each with its own glyph so a screen can be skimmed by shape alone
/// (AC-5): `Run` is `◆`, `Read` is `◈`, ground-truthed against grok's own TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Run,
    Read,
    Edit,
    Write,
    Search,
    Fetch,
    Agent,
    Plan,
    Other,
}

impl ToolKind {
    pub fn glyph(self) -> &'static str {
        match self {
            ToolKind::Run => "◆",
            ToolKind::Read => "◈",
            ToolKind::Edit => "✎",
            ToolKind::Write => "✚",
            ToolKind::Search => "⌕",
            ToolKind::Fetch => "⇣",
            ToolKind::Agent => "☍",
            ToolKind::Plan => "☐",
            ToolKind::Other => "◇",
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            ToolKind::Run => "Run",
            ToolKind::Read => "Read",
            ToolKind::Edit => "Edit",
            ToolKind::Write => "Write",
            ToolKind::Search => "Search",
            ToolKind::Fetch => "Fetch",
            ToolKind::Agent => "Agent",
            ToolKind::Plan => "Plan",
            ToolKind::Other => "Run",
        }
    }

    fn classify(name: &str) -> ToolKind {
        match name {
            "Bash" | "Run" | "Shell" | "BashOutput" => ToolKind::Run,
            "Read" | "Cat" | "NotebookRead" => ToolKind::Read,
            "Edit" | "MultiEdit" | "NotebookEdit" => ToolKind::Edit,
            "Write" => ToolKind::Write,
            "Grep" | "Glob" | "Search" => ToolKind::Search,
            "WebFetch" | "WebSearch" | "Fetch" => ToolKind::Fetch,
            "Task" | "Agent" => ToolKind::Agent,
            "TodoWrite" | "ExitPlanMode" | "Plan" => ToolKind::Plan,
            _ => ToolKind::Other,
        }
    }
}

/// A coarse categorical tag for a log record — the "direction" field the spec names
/// alongside `time`/`tool`/`argument`/`result`. Distinct from `RecordKind` (which
/// carries payload, e.g. `Tool(ToolKind)`/`Result{ok}`) so callers can match on shape
/// without destructuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogDirection {
    Prose,
    Thought,
    Tool,
    Result,
    Note,
    Error,
}

/// The shape of a record, shared by every view's render row. Carries payload where
/// the shape needs it (`Tool`'s kind, `Result`'s pass/fail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Section,
    Prose,
    Thought,
    Tool(ToolKind),
    Result { ok: bool },
    Note,
    Alert,
    Error,
    Doc,
    Criterion,
    FileDiff,
}

/// An immutable identity for a record, used to key fold state, the cursor, and raw
/// mode. Never derived from rendered text (a shortened path or a rebuilt summary must
/// not change identity) — always a source range or an event's own stamp (correction
/// #6). `Eq`/`Hash` so it can key a `HashSet`/`HashMap` directly.
///
/// `Log.end` is deliberately excluded from equality/hashing (implemented by hand
/// below, not derived): a streaming record's `end` is mutated as later continuation
/// lines or its result merge in (`parse_log_records`), so keying identity on it made
/// an expanded running tool call re-fold itself and lose the cursor the moment its
/// next line landed. `start` (plus `run_id`/`slot_id`) is the part that never moves.
#[derive(Debug, Clone)]
pub enum SourceId {
    Log {
        run_id: String,
        slot_id: String,
        start: u64,
        end: u64,
    },
    Document {
        path: String,
        start: u64,
        end: u64,
    },
    Activity {
        at_millis: i64,
        sequence: u64,
    },
    Diff {
        worktree: String,
        path: String,
    },
}

impl PartialEq for SourceId {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                SourceId::Log {
                    run_id,
                    slot_id,
                    start,
                    ..
                },
                SourceId::Log {
                    run_id: r2,
                    slot_id: s2,
                    start: st2,
                    ..
                },
            ) => run_id == r2 && slot_id == s2 && start == st2,
            (
                SourceId::Document { path, start, end },
                SourceId::Document {
                    path: p2,
                    start: s2,
                    end: e2,
                },
            ) => path == p2 && start == s2 && end == e2,
            (
                SourceId::Activity {
                    at_millis,
                    sequence,
                },
                SourceId::Activity {
                    at_millis: a2,
                    sequence: s2,
                },
            ) => at_millis == a2 && sequence == s2,
            (
                SourceId::Diff { worktree, path },
                SourceId::Diff {
                    worktree: w2,
                    path: p2,
                },
            ) => worktree == w2 && path == p2,
            _ => false,
        }
    }
}

impl Eq for SourceId {}

impl std::hash::Hash for SourceId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            SourceId::Log {
                run_id,
                slot_id,
                start,
                ..
            } => {
                0u8.hash(state);
                run_id.hash(state);
                slot_id.hash(state);
                start.hash(state);
            }
            SourceId::Document { path, start, end } => {
                1u8.hash(state);
                path.hash(state);
                start.hash(state);
                end.hash(state);
            }
            SourceId::Activity {
                at_millis,
                sequence,
            } => {
                2u8.hash(state);
                at_millis.hash(state);
                sequence.hash(state);
            }
            SourceId::Diff { worktree, path } => {
                3u8.hash(state);
                worktree.hash(state);
                path.hash(state);
            }
        }
    }
}

/// One entry from a run's log: a tool call merged with its result where the pairing
/// is unambiguous, a reasoning block, a note, an error, or a coalesced run of prose.
/// The stored fields are the domain data (`direction`/`tool`/`argument`/`result`);
/// `kind` is the shared shape tag `Record` (the render row) is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub time: Option<DateTime<Utc>>,
    pub direction: LogDirection,
    pub kind: RecordKind,
    pub tool: Option<ToolKind>,
    pub argument: String,
    pub result: Option<String>,
    /// Pass/fail of the attached result, once one has merged in. `None` for a call
    /// with no result yet (or a shape that never has one) — distinct from `Result`'s
    /// own `ok`, which only standalone results carry via `kind`.
    pub ok: Option<bool>,
    pub elapsed: Option<Duration>,
    pub body: Vec<String>,
    pub source: SourceId,
}

impl LogRecord {
    /// Derives this run's paint-time render row. Folding a tool result or a thought
    /// is the *default* here (AC-6): `folded_by_default` is true whenever there is
    /// body content to hide, never something `RecordView` decides later. Prose is
    /// the one shape that never folds (plan step 9, constraint 3): a narrative
    /// paragraph is exactly what the Log tab exists to show on first paint, unlike a
    /// tool's raw output.
    pub fn to_record(&self) -> Record {
        let (glyph, verb): (&'static str, &'static str) = match &self.kind {
            RecordKind::Tool(tool) => (tool.glyph(), tool.verb()),
            RecordKind::Result { ok: true } => ("◂", "Result"),
            RecordKind::Result { ok: false } => ("◂", "Failed"),
            RecordKind::Thought => ("…", "Thought"),
            RecordKind::Note => ("·", "Note"),
            RecordKind::Error => ("!", "Error"),
            _ => ("│", ""),
        };
        let summary = if !self.argument.is_empty() {
            self.argument.clone()
        } else {
            self.result.clone().unwrap_or_default()
        };
        Record {
            kind: self.kind,
            glyph,
            verb: verb.to_string(),
            head: summary.clone(),
            summary,
            body: self.body.clone(),
            time: self.time,
            elapsed: self.elapsed,
            actor: None,
            ok: self.ok,
            source: self.source.clone(),
            folded_by_default: match self.kind {
                RecordKind::Prose => false,
                RecordKind::Thought => true,
                _ => !self.body.is_empty(),
            },
        }
    }
}

/// One entry on the Activity tab: a phase boundary, a slot status change, a gate
/// result, a bus message, or an alert.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityRecord {
    pub time: Option<DateTime<Utc>>,
    pub actor: String,
    pub event: String,
    pub detail: String,
    pub kind: RecordKind,
    pub source: SourceId,
}

impl ActivityRecord {
    pub fn to_record(&self) -> Record {
        let glyph = match self.kind {
            RecordKind::Alert => "!",
            RecordKind::Section => "§",
            _ => "·",
        };
        Record {
            kind: self.kind,
            glyph,
            verb: self.event.clone(),
            head: self.event.clone(),
            summary: self.detail.clone(),
            body: if self.detail.is_empty() {
                Vec::new()
            } else {
                vec![self.detail.clone()]
            },
            time: self.time,
            elapsed: None,
            actor: Some(self.actor.clone()),
            ok: None,
            source: self.source.clone(),
            folded_by_default: false,
        }
    }
}

/// The one shared render row every Main tab paints through (`RecordView`). Derived
/// from a domain record at paint time — never the thing that gets parsed or stored.
#[derive(Debug, Clone)]
pub struct Record {
    pub kind: RecordKind,
    pub glyph: &'static str,
    pub verb: String,
    pub head: String,
    pub summary: String,
    pub body: Vec<String>,
    pub time: Option<DateTime<Utc>>,
    pub elapsed: Option<Duration>,
    pub actor: Option<String>,
    /// Pass/fail once a tool call's result has merged in (AC-6/AC-13's "e/E must
    /// reach a failed tool call" — the merge keeps `kind: Tool(_)` for its glyph, so
    /// failure has to travel on its own field rather than displacing `kind`).
    pub ok: Option<bool>,
    pub source: SourceId,
    pub folded_by_default: bool,
}

/// Column x-positions for a record row, a pure function of view width (U32/U11
/// applied to content): glyph < verb < summary < meta always, and the meta column is
/// right-aligned so `meta + meta_width == width` exactly. Width earns fields — the
/// actor column appears at 120, and the meta column widens at 100 to also carry an
/// absolute time — never more whitespace.
#[derive(Debug, Clone, Copy)]
pub struct Columns {
    pub gutter: u16,
    pub glyph: u16,
    pub actor: Option<u16>,
    pub verb: u16,
    pub summary: u16,
    pub meta: u16,
    pub meta_width: u16,
}

impl Columns {
    pub fn for_width(width: u16) -> Self {
        const ACTOR_BREAK: u16 = 120;
        const WIDE_META_BREAK: u16 = 100;
        const ACTOR_WIDTH: u16 = 10;
        const VERB_WIDTH: u16 = 9;
        const NARROW_META_WIDTH: u16 = 6;
        const WIDE_META_WIDTH: u16 = 15;

        let gutter = 0;
        let glyph = gutter + 2;
        let (actor, verb) = if width >= ACTOR_BREAK {
            (Some(glyph + 2), glyph + 2 + ACTOR_WIDTH)
        } else {
            (None, glyph + 2)
        };
        let summary = verb + VERB_WIDTH;
        let meta_width = if width >= WIDE_META_BREAK {
            WIDE_META_WIDTH
        } else {
            NARROW_META_WIDTH
        };
        let meta = width.saturating_sub(meta_width).max(summary + 1);
        Self {
            gutter,
            glyph,
            actor,
            verb,
            summary,
            meta,
            meta_width,
        }
    }
}

/// Longest-prefix strip against the run's own worktree roots, then fish-style
/// single-letter abbreviation of any leading components that remain, keeping the
/// last two components whole (U34). Built fresh per paint from in-memory paths
/// (worktree cwds + project root) — never a process-global default.
#[derive(Debug, Clone, Default)]
pub struct PathShortener {
    roots: Vec<PathBuf>,
}

impl PathShortener {
    pub fn new(mut roots: Vec<PathBuf>) -> Self {
        roots.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
        Self { roots }
    }

    pub fn shorten(&self, path: &str) -> String {
        for root in &self.roots {
            let root_str = root.to_string_lossy();
            let trimmed = root_str.trim_end_matches('/');
            if path == trimmed {
                // A path exactly equal to the root has nothing left to strip; `.`
                // is a visible, unambiguous "this is the root itself" — an empty
                // string would look identical to a parse failure (AC-12).
                return ".".to_string();
            }
            let prefix = format!("{trimmed}/");
            if let Some(rest) = path.strip_prefix(prefix.as_str()) {
                return rest.to_string();
            }
        }
        Self::abbreviate(path)
    }

    fn abbreviate(path: &str) -> String {
        let leading_slash = path.starts_with('/');
        let parts: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() <= 2 {
            return path.to_string();
        }
        let keep_from = parts.len() - 2;
        let mut out = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            if i < keep_from {
                out.push(part.chars().next().unwrap_or('_').to_string());
            } else {
                out.push((*part).to_string());
            }
        }
        let joined = out.join("/");
        if leading_slash {
            format!("/{joined}")
        } else {
            joined
        }
    }
}

/// `0.8s`, `12s`, `1m02s`, `1h04m` — never more precision than the magnitude needs.
pub fn fmt_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{:.1}s", ms as f64 / 1000.0);
    }
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Bisects a sorted `(byte_offset, time)` index for the timestamp of the entry at or
/// immediately before `offset`. Absent index -> `None`, never an invented time.
fn time_at(index: &[(u64, DateTime<Utc>)], offset: u64) -> Option<DateTime<Utc>> {
    if index.is_empty() {
        return None;
    }
    match index.binary_search_by_key(&offset, |(o, _)| *o) {
        Ok(i) => Some(index[i].1),
        Err(0) => None,
        Err(i) => Some(index[i - 1].1),
    }
}

/// Splits `text` into `(line, start, end)` triples with byte offsets *within `text`*,
/// preserving exact positions (unlike `str::lines`, which discards them) — the parser
/// needs these to build `SourceId::Log` ranges and to bisect the time index.
fn scan_lines(text: &str) -> Vec<(&str, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let start = pos;
        let end = pos + raw.len();
        out.push((line, start, end));
        pos = end;
    }
    out
}

/// A header/prompt line, suppressed only in the spawn header (correction #3) — not
/// anywhere a line happens to start with `#`, which is routine in agent markdown
/// output (`## Result`, `# Verdict`) and must stay visible (AC-6c).
fn is_header_line(line: &str) -> bool {
    line.is_empty() || line.starts_with('#') || line == "---" || line.starts_with("cwd=")
}

fn split_tool_line(rest: &str) -> (&str, &str) {
    if let Some(pos) = rest.find("  ") {
        (rest[..pos].trim(), rest[pos..].trim_start())
    } else {
        (rest.trim(), "")
    }
}

/// Drops the provider's opaque tool-call id (`toolu_…`, `call_…`, …) from a result
/// line: it pairs with nothing on screen (the matching call line never carries it)
/// and only eats column budget.
fn strip_tool_id(rest: &str) -> &str {
    let mut it = rest.splitn(2, char::is_whitespace);
    let Some(first) = it.next() else {
        return rest;
    };
    let opaque = first == "tool"
        || (first.len() >= 10
            && ["toolu_", "tooluse_", "call_", "fc_", "msg_"]
                .iter()
                .any(|p| first.starts_with(p)));
    if opaque {
        it.next().unwrap_or("").trim_start()
    } else {
        rest
    }
}

/// Splits a `←` result line's remainder into (ok, preview). Unlike the old
/// behaviour, the returned preview keeps the provider's tool id: stripping it here
/// meant it was gone from both the head *and* the body once merged, and AC-6
/// requires expansion to preserve every persisted byte. Callers that want the
/// clean, tool-id-free label for the head/summary run `strip_tool_id` themselves.
fn split_result_line(rest: &str) -> (bool, &str) {
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('✓') {
        (true, stripped.trim_start())
    } else if let Some(stripped) = rest.strip_prefix('✗') {
        (false, stripped.trim_start())
    } else {
        (true, rest)
    }
}

/// The api-sdk backend's own tool-observation line (`src/api/runtime.rs`'s
/// `append_log(... "tool => {observation}\n")`): the api runtime writes a
/// distinct, private log shape (never the native coalescer's `→`/`←` markers —
/// changing that is provider/adapter work, out of scope here), so the record
/// parser recognizes its literal prefix directly rather than the api runtime
/// being made to speak the native marker vocabulary (AC-8).
fn api_tool_observation(line: &str) -> Option<&str> {
    line.strip_prefix("tool => ")
}

/// The api-sdk backend's own tool-*call* line: a bare `{"tool": "...", ...}` JSON
/// object `src/api/runtime.rs::run_tool` reads, written to the log verbatim as
/// part of the assistant's own text (`extract_tool_json`'s primary, non-fenced
/// case). Recognized here so an api-sdk log gets a real `Tool` record — without
/// this, the JSON line parsed as generic prose and its `tool => ` observation
/// became an orphan `Result` with no call to merge into (AC-8).
fn api_tool_call(line: &str) -> Option<(ToolKind, String)> {
    let t = line.trim();
    if !t.starts_with('{') || !t.contains("\"tool\"") {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Call {
        tool: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        cmd: Option<String>,
    }
    let call: Call = serde_json::from_str(t).ok()?;
    let kind = match call.tool.as_str() {
        "read" => ToolKind::Read,
        "write" => ToolKind::Write,
        "cmd" | "run" | "shell" => ToolKind::Run,
        _ => ToolKind::Other,
    };
    let detail = call.path.or(call.cmd).unwrap_or_default();
    Some((kind, detail))
}

/// A stream marker line — the same shape `stream_content`'s prompt-dump filter
/// looks for. Used only to find where the headless prompt echo ends (U36/AC-10):
/// the record parser must suppress the same boilerplate the old raw viewer did,
/// or the Log tab regains the wall of prompt text this feature exists to remove.
fn is_marker_line(line: &str) -> bool {
    line.starts_with('→')
        || line.starts_with('←')
        || line.starts_with('·')
        || line.starts_with('…')
        || line.starts_with('!')
        || line.starts_with("I'll ")
        || line.starts_with("I ")
}

/// The relative byte offset the headless prompt dump ends at, or `0` if no marker
/// line ever appears (in which case nothing beyond the structural header is
/// suppressed — mirroring `stream_content`'s `position(..).unwrap_or(0)`, which
/// never hides a marker-less stream). Only meaningful at `start_offset == 0`: a
/// later incremental tail has already passed the prompt.
fn prompt_skip_end(text: &str) -> usize {
    let mut in_header = true;
    for (line, rel_start, _) in scan_lines(text) {
        if in_header {
            if is_header_line(line) {
                continue;
            }
            in_header = false;
        }
        if is_marker_line(line) {
            return rel_start;
        }
    }
    0
}

/// Parses one slot's raw log tail into typed `LogRecord`s. `text` is exactly what was
/// tailed from disk — no header/prompt suppression or joining has been done to it —
/// and `start_offset` is the absolute byte offset `text` begins at, so every record's
/// `SourceId::Log` range is a real position in the file on disk. `index` is the
/// (possibly empty) sidecar `<offset, time>` table; an empty index means every record
/// parses with `time: None` rather than a fabricated time (AC-8).
///
/// A tool call and its result merge into one record only when the pairing is
/// unambiguous — exactly one open call when the `←` line arrives. Zero open calls
/// (an orphan result) or two-or-more (parallel calls in flight) leave the result as
/// its own standalone record rather than guessing which call it belongs to
/// (correction #5). Ambiguity also stops tracking every call that was open at that
/// point: leaving them in the pending queue would silently misattribute every later
/// result too, since the queue would never drop back to exactly one.
///
/// Every line is scanned individually and stamped via `time_at`'s bisection against
/// the sidecar index, rather than only inspecting the first line of each indexed
/// append (an earlier chunk-oriented parser did that, and it silently dropped every
/// marker that was not chunk-initial — the common `<prose>\n→ Bash <cmd>\n` shape a
/// single native `assistant` turn writes in one append). Lines within the same
/// append share that append's timestamp, which is exactly what bisection already
/// gives for any offset inside it; a marker that lands anywhere in the text is still
/// found. Without an index (older logs, tmux-teed panes, dry-run/mock), every line
/// still parses, just with `time: None`.
pub fn parse_log_records(
    text: &str,
    start_offset: u64,
    index: &[(u64, DateTime<Utc>)],
    shortener: &PathShortener,
    run_id: &str,
    slot_id: &str,
) -> Vec<LogRecord> {
    let mut records: Vec<LogRecord> = Vec::new();
    let mut pending: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    // The most recently pushed record: an unmarked line immediately following one
    // becomes its body (continuation), the same grouping the chunk-based parser gets
    // for free from atomic writer chunks — a `←` result's own output (`total 1184`,
    // a file listing) must fold with it rather than surface as loose, unfolded prose.
    let mut last_idx: Option<usize> = None;

    let source_of = |start: u64, end: u64| SourceId::Log {
        run_id: run_id.to_string(),
        slot_id: slot_id.to_string(),
        start,
        end,
    };

    let mut in_header = start_offset == 0;
    let skip_until = if start_offset == 0 {
        prompt_skip_end(text)
    } else {
        0
    };
    for (line, rel_start, rel_end) in scan_lines(text) {
        if in_header {
            if is_header_line(line) {
                last_idx = None;
                continue;
            }
            in_header = false;
        }
        if rel_start < skip_until {
            last_idx = None;
            continue;
        }
        let abs_start = start_offset + rel_start as u64;
        let abs_end = start_offset + rel_end as u64;
        let time = time_at(index, abs_start);

        if let Some(rest) = line.strip_prefix('→') {
            let (name, detail) = split_tool_line(rest.trim_start());
            let tool = ToolKind::classify(name);
            let argument = if matches!(
                tool,
                ToolKind::Read
                    | ToolKind::Write
                    | ToolKind::Edit
                    | ToolKind::Search
                    | ToolKind::Fetch
            ) {
                shortener.shorten(detail)
            } else {
                detail.to_string()
            };
            records.push(LogRecord {
                time,
                direction: LogDirection::Tool,
                kind: RecordKind::Tool(tool),
                tool: Some(tool),
                argument,
                result: None,
                ok: None,
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            pending.push_back(records.len() - 1);
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some(rest) = line.strip_prefix('←') {
            let (ok, preview) = split_result_line(rest);
            let clean = strip_tool_id(preview);
            if pending.len() == 1 {
                let idx = pending.pop_front().expect("checked len == 1");
                let call_time = records[idx].time;
                records[idx].result = Some(clean.to_string());
                records[idx].ok = Some(ok);
                if !preview.is_empty() {
                    // Raw, not `clean`: expansion must preserve every persisted
                    // byte, including a provider tool id (AC-6).
                    records[idx].body.push(preview.to_string());
                }
                records[idx].elapsed = match (call_time, time) {
                    (Some(a), Some(b)) if b >= a => (b - a).to_std().ok(),
                    _ => None,
                };
                if let SourceId::Log { end, .. } = &mut records[idx].source {
                    *end = abs_end;
                }
                last_idx = Some(idx);
                continue;
            }
            // Zero or ambiguous (2+) open calls: never guess which one a result
            // belongs to (correction #5, AC-11). On ambiguity, stop tracking every
            // call currently open rather than leaving them in `pending` forever —
            // otherwise one ambiguous result permanently blocks every later
            // single-open merge too, since `pending` never drops back to 1.
            pending.clear();
            records.push(LogRecord {
                time,
                direction: LogDirection::Result,
                kind: RecordKind::Result { ok },
                tool: None,
                argument: String::new(),
                result: Some(clean.to_string()),
                ok: Some(ok),
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some(rest) = line.strip_prefix('·') {
            records.push(LogRecord {
                time,
                direction: LogDirection::Note,
                kind: RecordKind::Note,
                tool: None,
                argument: String::new(),
                result: Some(rest.trim().to_string()),
                ok: None,
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some(rest) = line.strip_prefix('…') {
            records.push(LogRecord {
                time,
                direction: LogDirection::Thought,
                kind: RecordKind::Thought,
                tool: None,
                argument: String::new(),
                result: Some(rest.trim().to_string()),
                ok: None,
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some(rest) = line.strip_prefix('!') {
            records.push(LogRecord {
                time,
                direction: LogDirection::Error,
                kind: RecordKind::Error,
                tool: None,
                argument: String::new(),
                result: Some(rest.trim().to_string()),
                ok: None,
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some((tool, detail)) = api_tool_call(line) {
            let argument = if matches!(tool, ToolKind::Read | ToolKind::Write) {
                shortener.shorten(&detail)
            } else {
                detail
            };
            records.push(LogRecord {
                time,
                direction: LogDirection::Tool,
                kind: RecordKind::Tool(tool),
                tool: Some(tool),
                argument,
                result: None,
                ok: None,
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            pending.push_back(records.len() - 1);
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some(observation) = api_tool_observation(line) {
            let ok = !observation.starts_with("tool error:");
            if pending.len() == 1 {
                let idx = pending.pop_front().expect("checked len == 1");
                let call_time = records[idx].time;
                records[idx].result = Some(observation.to_string());
                records[idx].ok = Some(ok);
                records[idx].elapsed = match (call_time, time) {
                    (Some(a), Some(b)) if b >= a => (b - a).to_std().ok(),
                    _ => None,
                };
                if let SourceId::Log { end, .. } = &mut records[idx].source {
                    *end = abs_end;
                }
                last_idx = Some(idx);
                continue;
            }
            // See the `←` branch's comment: an ambiguous observation clears every
            // currently-open call rather than leaving them stuck in `pending`.
            pending.clear();
            records.push(LogRecord {
                time,
                direction: LogDirection::Result,
                kind: RecordKind::Result { ok },
                tool: None,
                argument: String::new(),
                result: Some(observation.to_string()),
                ok: Some(ok),
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        // An unmarked line continues whatever record was last pushed (prose
        // paragraphs coalesce, and a tool/result's own output folds with it) —
        // the line-scan fallback's approximation of the chunk parser's atomic
        // per-write grouping.
        if let Some(idx) = last_idx {
            records[idx].body.push(line.to_string());
            if let SourceId::Log { end, .. } = &mut records[idx].source {
                *end = abs_end;
            }
            continue;
        }
        records.push(LogRecord {
            time,
            direction: LogDirection::Prose,
            kind: RecordKind::Prose,
            tool: None,
            argument: String::new(),
            result: Some(line.to_string()),
            ok: None,
            elapsed: None,
            body: Vec::new(),
            source: source_of(abs_start, abs_end),
        });
        last_idx = Some(records.len() - 1);
    }
    records
}

/// Splits a markdown document into one foldable `Doc` record per `#`/`##` heading.
/// A document with no headings becomes one record named after `name`. Each section
/// gets its own byte range within `body` as its `SourceId` (AC-7): sharing one
/// identity across every section of a document made `Space` expand all of them at
/// once and made `J`/`K` unable to move between them, since the cursor always
/// re-resolved to the first record with that id.
pub fn parse_document(name: &str, body: &str, path: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let mut current: Option<(String, Vec<String>, usize)> = None;
    let mut pos = 0usize;
    // A fenced code block's own lines are never heading candidates: a `#` comment
    // inside a fence (`## Result` in a shell transcript, say) must not split the
    // section it lives in.
    let mut in_fence = false;
    for raw in body.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line_start = pos;
        pos += raw.len();
        let is_fence_delim = line.trim_start().starts_with("```");
        let heading = (!in_fence)
            .then(|| line.strip_prefix("## ").or_else(|| line.strip_prefix("# ")))
            .flatten();
        if is_fence_delim {
            in_fence = !in_fence;
        }
        if let Some(heading) = heading {
            if let Some((head, lines, start)) = current.take() {
                records.push(doc_record(head, lines, doc_source(path, start, line_start)));
            }
            current = Some((heading.trim().to_string(), Vec::new(), line_start));
            continue;
        }
        // Once a section has real content, a blank line is a paragraph break or a
        // fenced block's own blank interior — both must survive verbatim (`R`'s
        // "every persisted byte" guarantee extends to the Plan/Review documents'
        // own body). Only *leading* blank lines before the first real line are
        // dropped, so the summary (`body.first()`) is never an empty string.
        match current.as_mut() {
            Some((_, lines, _)) => {
                if !line.trim().is_empty() || !lines.is_empty() {
                    lines.push(line.to_string());
                }
            }
            None if !line.trim().is_empty() => {
                current = Some((name.to_string(), vec![line.to_string()], line_start));
            }
            None => {}
        }
    }
    if let Some((head, lines, start)) = current.take() {
        records.push(doc_record(head, lines, doc_source(path, start, pos)));
    }
    records
}

fn doc_source(path: &str, start: usize, end: usize) -> SourceId {
    SourceId::Document {
        path: path.to_string(),
        start: start as u64,
        end: end as u64,
    }
}

fn doc_record(head: String, body: Vec<String>, source: SourceId) -> Record {
    let summary = body.first().cloned().unwrap_or_default();
    Record {
        kind: RecordKind::Doc,
        glyph: "▤",
        verb: "Doc".to_string(),
        head,
        summary,
        body,
        time: None,
        elapsed: None,
        actor: None,
        ok: None,
        source,
        folded_by_default: true,
    }
}

/// Which line of a record a flattened row paints: its head, or one of its body
/// lines (only present once the record is expanded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Head,
    Body(usize),
}

/// One paintable row, cheap enough to rebuild every frame (U13: it touches no
/// disk — the read already happened when `records` was built).
#[derive(Debug, Clone, Copy)]
pub struct FlatRow {
    pub record_idx: usize,
    pub kind: RowKind,
}

/// Expands `records` into paintable rows: one head row per record, plus one row per
/// body line when `is_expanded` says that record is open. A folded record with a
/// non-empty body still shows only its head — that is the fold (U36).
pub fn flatten(records: &[Record], is_expanded: impl Fn(&Record) -> bool) -> Vec<FlatRow> {
    let mut out = Vec::new();
    for (i, r) in records.iter().enumerate() {
        out.push(FlatRow {
            record_idx: i,
            kind: RowKind::Head,
        });
        if is_expanded(r) {
            for j in 0..r.body.len() {
                out.push(FlatRow {
                    record_idx: i,
                    kind: RowKind::Body(j),
                });
            }
        }
    }
    out
}

/// Splits a `git diff HEAD` (U4) into one `FileDiff` record per `diff --git` boundary,
/// folded by default, plus a `Section` for the `--stat` preamble (if any). Never a
/// second implementation of the diff — the bytes are exactly what `git diff` wrote.
pub fn parse_diff(text: &str, worktree: &str) -> Vec<Record> {
    let mut out = Vec::new();
    let mut preamble: Vec<String> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;

    fn flush(current: Option<(String, Vec<String>)>, worktree: &str, out: &mut Vec<Record>) {
        let Some((path, body)) = current else {
            return;
        };
        let added = body
            .iter()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count();
        let removed = body
            .iter()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .count();
        let status = if body.iter().any(|l| l.starts_with("new file mode")) {
            "A"
        } else if body.iter().any(|l| l.starts_with("deleted file mode")) {
            "D"
        } else if body.iter().any(|l| l.starts_with("rename from")) {
            "R"
        } else {
            "M"
        };
        out.push(Record {
            kind: RecordKind::FileDiff,
            glyph: "±",
            verb: status.to_string(),
            head: path.clone(),
            summary: format!("{path}  +{added} −{removed}"),
            body,
            time: None,
            elapsed: None,
            actor: None,
            ok: None,
            source: SourceId::Diff {
                worktree: worktree.to_string(),
                path,
            },
            folded_by_default: true,
        });
    }

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(current.take(), worktree, &mut out);
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            current = Some((path, Vec::new()));
            continue;
        }
        match current.as_mut() {
            Some((_, body)) => body.push(line.to_string()),
            None => preamble.push(line.to_string()),
        }
    }
    flush(current.take(), worktree, &mut out);

    if preamble.iter().any(|l| !l.trim().is_empty()) {
        out.insert(
            0,
            Record {
                kind: RecordKind::Section,
                glyph: "§",
                verb: "Stat".to_string(),
                head: "Summary".to_string(),
                summary: preamble
                    .iter()
                    .find(|l| !l.trim().is_empty())
                    .cloned()
                    .unwrap_or_default(),
                body: preamble,
                time: None,
                elapsed: None,
                actor: None,
                ok: None,
                source: SourceId::Diff {
                    worktree: worktree.to_string(),
                    path: "__stat__".to_string(),
                },
                folded_by_default: true,
            },
        );
    }
    out
}

/// One `Note` record naming a missing artifact — Plan/Review never render a blank
/// tab for a document that has not landed yet.
pub fn missing_document(name: &str, path: &str) -> Record {
    Record {
        kind: RecordKind::Note,
        glyph: "·",
        verb: "Missing".to_string(),
        head: name.to_string(),
        summary: format!("not written yet ({path})"),
        body: Vec::new(),
        time: None,
        elapsed: None,
        actor: None,
        ok: None,
        source: SourceId::Document {
            path: path.to_string(),
            start: 0,
            end: 0,
        },
        folded_by_default: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_glyphs_match_ground_truth() {
        assert_eq!(ToolKind::Run.glyph(), "◆");
        assert_eq!(ToolKind::Read.glyph(), "◈");
    }

    #[test]
    fn merged_result_preview_survives_into_body_even_when_one_line() {
        let records = parse_log_records(
            "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        assert_eq!(records.len(), 1);
        assert!(
            records[0].body.iter().any(|l| l == "total 1184"),
            "a one-line result must still surface in the record's body, {:?}",
            records[0].body
        );
        assert_eq!(records[0].ok, Some(true));
        let rendered = records[0].to_record();
        assert!(
            rendered.folded_by_default,
            "a result-bearing tool call folds by default"
        );
    }

    #[test]
    fn failed_merged_tool_call_is_flagged_ok_false() {
        let records = parse_log_records(
            "→ Bash  false\n← ✗  exit 1\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ok, Some(false));
    }

    #[test]
    fn header_hash_lines_are_only_suppressed_at_the_true_head() {
        let text = "# Role: impl\ncwd=/x\n---\n→ Bash  cargo test\n← ✓  ## Result\nok\n";
        let records = parse_log_records(text, 0, &[], &PathShortener::default(), "r", "s");
        assert_eq!(records.len(), 1);
        assert!(
            records[0].body.iter().any(|l| l == "## Result"),
            "a `#`-prefixed line after the header must stay visible, {:?}",
            records[0].body
        );
    }

    #[test]
    fn prompt_dump_is_suppressed_up_to_the_first_stream_marker() {
        let text = "# Role: impl\ncwd=/x\n---\n## Task\nDo the thing, in great detail,\nacross several lines of prose.\nI'll start now.\n→ Bash  ls\n← ✓  ok\n";
        let records = parse_log_records(text, 0, &[], &PathShortener::default(), "r", "s");
        assert!(
            records
                .iter()
                .all(|r| !r.body.iter().any(|l| l.contains("Do the thing"))
                    && !r.argument.contains("Do the thing")),
            "the headless prompt dump must not surface as a record, {records:#?}"
        );
        assert!(
            records
                .iter()
                .any(|r| r.argument.contains("I'll start now")
                    || r.result.as_deref() == Some("I'll start now.")),
            "the first real content line (the thought that ends the prompt echo) must survive, {records:#?}"
        );
    }

    #[test]
    fn api_backend_tool_observations_parse_as_typed_result_records() {
        // The api-sdk backend's own private log shape (`src/api/runtime.rs`): a
        // step marker, the raw assistant text (a bare `{"tool": ...}` JSON call),
        // then its `tool => ` observation line — never the native `→`/`←`
        // coalescer markers. AC-8: the call/observation pair merges into one typed
        // `Tool` record, exactly like a native call/result pair, so `t`/`T` can
        // reach it and it carries elapsed when an index is present.
        let text = "\n--- api step 0 model=gpt ---\n{\"tool\":\"read\",\"path\":\"a.rs\"}\ntool => file contents here\n";
        let records = parse_log_records(text, 0, &[], &PathShortener::default(), "r", "s");
        let obs = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Tool(ToolKind::Read)))
            .unwrap_or_else(|| panic!("no typed Tool record in {records:#?}"));
        assert_eq!(obs.argument, "a.rs");
        assert_eq!(obs.result.as_deref(), Some("file contents here"));
        assert_eq!(obs.ok, Some(true));
    }

    #[test]
    fn api_backend_tool_error_observation_is_flagged_not_ok() {
        let text = "tool => tool error: no such file\n";
        let records = parse_log_records(text, 0, &[], &PathShortener::default(), "r", "s");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ok, Some(false));
    }

    #[test]
    fn ambiguous_result_does_not_permanently_wedge_future_merges() {
        // Two calls open before either result arrives (ambiguous), then a third call
        // with its own unambiguous result: that third pair must still merge, which
        // it cannot if the first two calls are left stuck in `pending` forever.
        let text = "→ Bash  a\n→ Bash  b\n← ✓  r1\n← ✓  r2\n→ Bash  c\n← ✓  r3\n";
        let records = parse_log_records(text, 0, &[], &PathShortener::default(), "r", "s");
        // a, b (open, unmerged) + r1, r2 (standalone) + c+r3 (merged) = 5 records.
        assert_eq!(records.len(), 5, "{records:#?}");
        let last = records.last().unwrap();
        assert_eq!(last.argument, "c");
        assert_eq!(last.result.as_deref(), Some("r3"));
    }

    #[test]
    fn fmt_elapsed_scales_with_magnitude() {
        assert_eq!(fmt_elapsed(Duration::from_millis(800)), "0.8s");
        assert_eq!(fmt_elapsed(Duration::from_secs(12)), "12s");
        assert_eq!(fmt_elapsed(Duration::from_secs(62)), "1m02s");
        assert_eq!(fmt_elapsed(Duration::from_secs(3900)), "1h05m");
    }

    #[test]
    fn missing_document_never_renders_a_blank_tab() {
        let r = missing_document("Plan critique", "plan-critic.md");
        assert_eq!(r.kind, RecordKind::Note);
        assert!(r.summary.contains("plan-critic.md"));
    }

    #[test]
    fn flatten_only_expands_when_told_to() {
        let records = vec![LogRecord {
            time: None,
            direction: LogDirection::Prose,
            kind: RecordKind::Prose,
            tool: None,
            argument: String::new(),
            result: Some("one".into()),
            ok: None,
            elapsed: None,
            body: vec!["a".into(), "b".into()],
            source: SourceId::Log {
                run_id: "r".into(),
                slot_id: "s".into(),
                start: 0,
                end: 1,
            },
        }
        .to_record()];
        let folded = flatten(&records, |_| false);
        assert_eq!(folded.len(), 1);
        let expanded = flatten(&records, |_| true);
        assert_eq!(expanded.len(), 3);
    }

    /// AC-7: a streaming tool call's identity must not change as its body grows
    /// (a continuation line arriving, then its result merging in) — `SourceId::Log`
    /// mutates `end` in place for exactly that reason, so equality/hashing must
    /// ignore it or an expanded call silently re-folds and the cursor loses it the
    /// moment more of it streams in.
    #[test]
    fn log_source_identity_survives_end_growing_as_the_record_streams() {
        let before = SourceId::Log {
            run_id: "r".into(),
            slot_id: "s".into(),
            start: 10,
            end: 20,
        };
        let after = SourceId::Log {
            run_id: "r".into(),
            slot_id: "s".into(),
            start: 10,
            end: 80,
        };
        assert_eq!(before, after, "growing `end` must not change identity");
        use std::hash::{Hash, Hasher};
        let hash_of = |id: &SourceId| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash_of(&before), hash_of(&after));

        let mut fold_open: std::collections::HashSet<SourceId> = std::collections::HashSet::new();
        fold_open.insert(before);
        assert!(
            fold_open.contains(&after),
            "an expansion keyed on the pre-growth id must still be found post-growth"
        );
    }

    #[test]
    fn log_source_identity_differs_by_start_run_or_slot() {
        let base = SourceId::Log {
            run_id: "r".into(),
            slot_id: "s".into(),
            start: 10,
            end: 20,
        };
        let different_start = SourceId::Log {
            run_id: "r".into(),
            slot_id: "s".into(),
            start: 11,
            end: 20,
        };
        let different_slot = SourceId::Log {
            run_id: "r".into(),
            slot_id: "other".into(),
            start: 10,
            end: 20,
        };
        assert_ne!(base, different_start);
        assert_ne!(base, different_slot);
    }

    #[test]
    fn parse_diff_splits_on_file_boundaries() {
        let text = "diff --git a/src/a.rs b/src/a.rs\n+one\n-two\ndiff --git a/src/b.rs b/src/b.rs\n+three\n";
        let records = parse_diff(text, "wt");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].head, "src/a.rs");
        assert_eq!(records[1].head, "src/b.rs");
    }

    #[test]
    fn parse_document_preserves_internal_blank_lines_and_paragraph_breaks() {
        let body = "## Section\n\nFirst paragraph.\n\nSecond paragraph.\n";
        let records = parse_document("doc", body, "p.md");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].body,
            vec!["First paragraph.", "", "Second paragraph."],
            "a blank line between paragraphs must survive, not be dropped: {:?}",
            records[0].body
        );
    }

    #[test]
    fn parse_document_ignores_a_heading_shaped_line_inside_a_fence() {
        let body = "## Real heading\n```\n## Result\nok\n```\nafter\n";
        let records = parse_document("doc", body, "p.md");
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].head, "Real heading");
        assert!(
            records[0].body.iter().any(|l| l == "## Result"),
            "a fenced `#`-shaped line must not split the section: {:?}",
            records[0].body
        );
    }
}
