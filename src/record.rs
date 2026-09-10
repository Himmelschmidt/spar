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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    pub elapsed: Option<Duration>,
    pub body: Vec<String>,
    pub source: SourceId,
}

impl LogRecord {
    /// Derives this run's paint-time render row. Folding a multi-line result or a
    /// thought is the *default* here (AC-6): `folded_by_default` is true whenever
    /// there is body content to hide, never something `RecordView` decides later.
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
            source: self.source.clone(),
            folded_by_default: !self.body.is_empty() || matches!(self.kind, RecordKind::Thought),
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
                return String::new();
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

/// Header/prompt lines suppressed as a parser rule (correction #3) rather than a
/// string transform, so every kept line's offset stays exactly where it was in the
/// original tailed bytes.
fn is_boilerplate_line(line: &str) -> bool {
    line.is_empty()
        || line.starts_with('#')
        || line == "---"
        || line.starts_with("cwd=")
        || line.starts_with("# Role:")
        || line.starts_with("## Task")
}

fn split_tool_line(rest: &str) -> (&str, &str) {
    if let Some(pos) = rest.find("  ") {
        (rest[..pos].trim(), rest[pos..].trim_start())
    } else {
        (rest.trim(), "")
    }
}

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

/// Parses one slot's raw log tail into typed `LogRecord`s. `text` is exactly what was
/// tailed from disk — no header/prompt suppression or joining has been done to it —
/// and `start_offset` is the absolute byte offset `text` begins at, so every record's
/// `SourceId::Log` range is a real position in the file on disk. `index` is the
/// (possibly empty) sidecar `<offset, time>` table; an empty index means every record
/// parses with `time: None` rather than a fabricated time (AC-8).
///
/// A tool call and its result merge into one record only when the pairing is
/// unambiguous — exactly one open call when the result line arrives. Two open calls
/// (or none) leave the result as its own standalone record rather than guessing
/// (correction #5's FIFO rule).
///
/// With a non-empty index, each index entry is one atomic `LogWriter::append` chunk
/// (correction #2 makes that guarantee), so this parses chunk-by-chunk between
/// consecutive index offsets — the chunk's own recorded offset is the record's
/// identity, not wherever a marker character happens to land after trimming
/// separators. Without an index (older logs, tmux-teed panes, dry-run/mock), it
/// falls back to a best-effort newline scan with no times attached.
pub fn parse_log_records(
    text: &str,
    start_offset: u64,
    index: &[(u64, DateTime<Utc>)],
    shortener: &PathShortener,
    run_id: &str,
    slot_id: &str,
) -> Vec<LogRecord> {
    if index.is_empty() {
        parse_log_records_by_line(text, start_offset, shortener, run_id, slot_id)
    } else {
        parse_log_records_by_chunk(text, start_offset, index, shortener, run_id, slot_id)
    }
}

fn parse_log_records_by_line(
    text: &str,
    start_offset: u64,
    shortener: &PathShortener,
    run_id: &str,
    slot_id: &str,
) -> Vec<LogRecord> {
    let index: &[(u64, DateTime<Utc>)] = &[];
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

    for (line, rel_start, rel_end) in scan_lines(text) {
        if is_boilerplate_line(line) {
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
            if pending.len() == 1 {
                let idx = pending.pop_front().expect("checked len == 1");
                let call_time = records[idx].time;
                records[idx].result = Some(preview.to_string());
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
            records.push(LogRecord {
                time,
                direction: LogDirection::Result,
                kind: RecordKind::Result { ok },
                tool: None,
                argument: String::new(),
                result: Some(preview.to_string()),
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
            elapsed: None,
            body: Vec::new(),
            source: source_of(abs_start, abs_end),
        });
        last_idx = Some(records.len() - 1);
    }
    records
}

fn parse_log_records_by_chunk(
    text: &str,
    start_offset: u64,
    index: &[(u64, DateTime<Utc>)],
    shortener: &PathShortener,
    run_id: &str,
    slot_id: &str,
) -> Vec<LogRecord> {
    let mut records: Vec<LogRecord> = Vec::new();
    let mut pending: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let text_len = text.len() as u64;

    let source_of = |start: u64, end: u64| SourceId::Log {
        run_id: run_id.to_string(),
        slot_id: slot_id.to_string(),
        start,
        end,
    };

    for (i, (offset, time)) in index.iter().enumerate() {
        let abs_start = *offset;
        if abs_start < start_offset {
            continue;
        }
        let rel_start = (abs_start - start_offset).min(text_len) as usize;
        let rel_end = index
            .get(i + 1)
            .map(|(next, _)| (next.saturating_sub(start_offset)).min(text_len) as usize)
            .unwrap_or(text.len());
        if rel_start >= rel_end || rel_end > text.len() {
            continue;
        }
        let raw_chunk = &text[rel_start..rel_end];
        let trimmed = raw_chunk.trim_matches('\n');
        if trimmed.is_empty() || is_boilerplate_line(trimmed) {
            continue;
        }
        let mut lines = trimmed.split('\n');
        let head_line = lines.next().unwrap_or("");
        let extra: Vec<String> = lines.map(|l| l.to_string()).collect();
        let time = Some(*time);

        if let Some(rest) = head_line.strip_prefix('→') {
            let end = abs_start + rest.len() as u64;
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
                elapsed: None,
                body: extra,
                source: source_of(abs_start, end),
            });
            pending.push_back(records.len() - 1);
            continue;
        }

        if let Some(rest) = head_line.strip_prefix('←') {
            let end = abs_start + rest.len() as u64;
            let (ok, preview) = split_result_line(rest);
            if pending.len() == 1 {
                let idx = pending.pop_front().expect("checked len == 1");
                let call_time = records[idx].time;
                records[idx].result = Some(preview.to_string());
                records[idx].elapsed = match (call_time, time) {
                    (Some(a), Some(b)) if b >= a => (b - a).to_std().ok(),
                    _ => None,
                };
                records[idx].body.extend(extra);
                if let SourceId::Log { end: e, .. } = &mut records[idx].source {
                    *e = end;
                }
                continue;
            }
            records.push(LogRecord {
                time,
                direction: LogDirection::Result,
                kind: RecordKind::Result { ok },
                tool: None,
                argument: String::new(),
                result: Some(preview.to_string()),
                elapsed: None,
                body: extra,
                source: source_of(abs_start, end),
            });
            continue;
        }

        if let Some(rest) = head_line.strip_prefix('·') {
            records.push(LogRecord {
                time,
                direction: LogDirection::Note,
                kind: RecordKind::Note,
                tool: None,
                argument: String::new(),
                result: Some(rest.trim().to_string()),
                elapsed: None,
                body: extra,
                source: source_of(abs_start, abs_start + rest.len() as u64),
            });
            continue;
        }

        if let Some(rest) = head_line.strip_prefix('…') {
            records.push(LogRecord {
                time,
                direction: LogDirection::Thought,
                kind: RecordKind::Thought,
                tool: None,
                argument: String::new(),
                result: Some(rest.trim().to_string()),
                elapsed: None,
                body: extra,
                source: source_of(abs_start, abs_start + rest.len() as u64),
            });
            continue;
        }

        if let Some(rest) = head_line.strip_prefix('!') {
            records.push(LogRecord {
                time,
                direction: LogDirection::Error,
                kind: RecordKind::Error,
                tool: None,
                argument: String::new(),
                result: Some(rest.trim().to_string()),
                elapsed: None,
                body: extra,
                source: source_of(abs_start, abs_start + rest.len() as u64),
            });
            continue;
        }

        records.push(LogRecord {
            time,
            direction: LogDirection::Prose,
            kind: RecordKind::Prose,
            tool: None,
            argument: String::new(),
            result: Some(head_line.to_string()),
            elapsed: None,
            body: extra,
            source: source_of(abs_start, abs_start + head_line.len() as u64),
        });
    }
    records
}

/// Splits a markdown document into one foldable `Doc` record per `#`/`##` heading.
/// A document with no headings becomes one record named after `name`.
pub fn parse_document(name: &str, body: &str, source: SourceId) -> Vec<Record> {
    let mut records = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for line in body.lines() {
        let heading = line.strip_prefix("## ").or_else(|| line.strip_prefix("# "));
        if let Some(heading) = heading {
            if let Some((head, lines)) = current.take() {
                records.push(doc_record(head, lines, source.clone()));
            }
            current = Some((heading.trim().to_string(), Vec::new()));
        } else if let Some((_, lines)) = current.as_mut() {
            if !line.trim().is_empty() {
                lines.push(line.to_string());
            }
        } else if !line.trim().is_empty() {
            current = Some((name.to_string(), vec![line.to_string()]));
        }
    }
    if let Some((head, lines)) = current.take() {
        records.push(doc_record(head, lines, source));
    }
    records
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
        out.push(Record {
            kind: RecordKind::FileDiff,
            glyph: "±",
            verb: "M".to_string(),
            head: path.clone(),
            summary: format!("{path}  +{added} −{removed}"),
            body,
            time: None,
            elapsed: None,
            actor: None,
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

    #[test]
    fn parse_diff_splits_on_file_boundaries() {
        let text = "diff --git a/src/a.rs b/src/a.rs\n+one\n-two\ndiff --git a/src/b.rs b/src/b.rs\n+three\n";
        let records = parse_diff(text, "wt");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].head, "src/a.rs");
        assert_eq!(records[1].head, "src/b.rs");
    }
}
