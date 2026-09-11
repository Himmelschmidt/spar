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
///
/// `Document` never had an `end` field to exclude in the first place (round-10
/// review, AC-7): a document's final section closes at the whole document's byte
/// length (`parse_document`), and `review_records` (`src/tui.rs`) built every
/// reviewer record with `end: text.len()` — a value that moves every time the
/// document grows between two snapshot rebuilds, so a field that was never read
/// anywhere still broke identity by being compared. `path` + `start` (a section's
/// own heading position, which never moves) fully identify a document section.
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
                SourceId::Document { path, start, .. },
                SourceId::Document {
                    path: p2,
                    start: s2,
                    ..
                },
            ) => path == p2 && start == s2,
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
            SourceId::Document { path, start, .. } => {
                1u8.hash(state);
                path.hash(state);
                start.hash(state);
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
    /// The standalone-result preview's persisted bytes, before `shorten_in_text`
    /// rewrote `result` for the head (AC-6/AC-12 both hold: the head is allowed to
    /// shorten a path for column width, but `to_record`'s body synthesis must
    /// expand into what was actually on disk). `None` for every other kind, whose
    /// own `result`/`argument` was never shortened in the first place.
    pub raw_result: Option<String>,
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
        // A tool's command/path is its own body row (`CODE`, AC-19) whether the
        // call is still open or has already merged with a result — the result's
        // own output is never the only thing left once a result arrives.
        let is_tool = matches!(self.kind, RecordKind::Tool(_));
        let mut body = self.body.clone();
        let has_command_row = is_tool && !self.argument.is_empty();
        if has_command_row {
            body.insert(0, self.argument.clone());
        }
        // A tool's summary must never repeat what `body[0]` just carried verbatim
        // (round-9 finding 1): once a result has merged in, its own preview is a
        // distinct string and becomes the head; until then the head stays empty
        // rather than showing the same command twice, adjacent, on head and body.
        let summary = if is_tool {
            self.result.clone().unwrap_or_default()
        } else if !self.argument.is_empty() {
            self.argument.clone()
        } else {
            self.result.clone().unwrap_or_default()
        };
        let verb = if matches!(self.kind, RecordKind::Thought) {
            match self.elapsed {
                Some(e) => format!("Thought for {}", fmt_elapsed(e)),
                None => verb.to_string(),
            }
        } else {
            verb.to_string()
        };
        // Every non-Tool kind's full text lives only in `summary` — a tool call
        // keeps its command row and result preview as separate body entries
        // already, but a Note/Error/Thought/standalone-Result record has
        // nowhere else to keep it. `build_head_row` paints the head as exactly
        // one truncated row, so a long line (or one with continuation lines
        // already in `body`, which used to gate this off entirely and drop the
        // record's *own* first line while showing only what followed it) needs
        // its full text copied into `body[0]` unconditionally, not only when
        // `body` happened to start empty (round-N review, AC-6). The copy uses
        // `raw_result` where one exists (standalone results only — the only
        // place `summary` itself has already been shortened for the head,
        // AC-12) so expansion reveals exactly what was persisted, never a
        // rewritten path.
        //
        // Prose is excluded from the unconditional case: it never folds, so a
        // one-line Prose record's body is *always* painted (never behind
        // `Space`), and copying `summary` in for the common short-and-complete
        // case would paint the identical line twice, adjacent, shifting every
        // row after it — the same duplicate-text defect round 9 fixed for Tool
        // heads, reintroduced here. Prose still gets the copy once it already
        // has continuation lines (a real multi-line paragraph, where the first
        // line is otherwise the only one missing from `body`); a single long
        // line with no continuation stays a known, accepted gap (round-N
        // review's "two more, both small" scope: it named Note/Error/Thought,
        // not Prose).
        if !is_tool && !summary.is_empty() {
            let synthesize = !matches!(self.kind, RecordKind::Prose) || !body.is_empty();
            if synthesize {
                let raw = self.raw_result.clone().unwrap_or_else(|| summary.clone());
                body.insert(0, raw);
            }
        }
        let folded_by_default = match self.kind {
            RecordKind::Prose => false,
            RecordKind::Thought => true,
            // Consults the final `body` (post-synthesis above), not `self.body`:
            // a record with no continuation lines of its own only becomes
            // foldable once its full text is copied in.
            RecordKind::Note | RecordKind::Error | RecordKind::Result { .. } => !body.is_empty(),
            _ => !self.body.is_empty(),
        };
        Record {
            kind: self.kind,
            glyph,
            verb,
            head: summary.clone(),
            summary,
            body,
            time: self.time,
            elapsed: self.elapsed,
            actor: None,
            ok: self.ok,
            source: self.source.clone(),
            folded_by_default,
            has_command_row,
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
        // `detail` lives in `body[0]`, never also in `summary` — the same string
        // painted on both the head and its own row, adjacent, is round-9 finding
        // 1's defect. Activity is never folded by default, so the body row is
        // always visible right under the head; it does not need to repeat there.
        let body = if self.detail.is_empty() {
            Vec::new()
        } else {
            vec![self.detail.clone()]
        };
        Record {
            kind: self.kind,
            glyph,
            verb: self.event.clone(),
            head: self.event.clone(),
            summary: String::new(),
            body,
            time: self.time,
            elapsed: None,
            actor: Some(self.actor.clone()),
            ok: None,
            source: self.source.clone(),
            folded_by_default: false,
            has_command_row: false,
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
    /// True only when `body[0]` is a genuine command/path row, set explicitly at
    /// construction rather than inferred later by comparing text (round-10 review:
    /// comparing `body[0]` to `summary` broke on the native Claude coalescer path,
    /// where a detail-less call's `body[0]` is the result preview carrying a
    /// provider tool id that never matches the id-stripped `summary`). Everywhere
    /// but `LogRecord::to_record`'s insert branch this is `false`.
    pub has_command_row: bool,
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
    /// Below 80 there is no room for a dedicated verb column (AC-4): the verb
    /// text is combined into the summary span at render time instead of
    /// occupying its own 9-column field. `verb` and `summary` still differ by a
    /// minimal amount even when folded — collapsing them to the same x-position
    /// would violate the "glyph < verb < summary < meta" column ordering every
    /// width must hold.
    pub verb_folded: bool,
}

impl Columns {
    pub fn for_width(width: u16) -> Self {
        const ACTOR_BREAK: u16 = 120;
        const WIDE_META_BREAK: u16 = 100;
        const VERB_BREAK: u16 = 80;
        const ACTOR_WIDTH: u16 = 10;
        const VERB_WIDTH: u16 = 9;
        const NARROW_VERB_WIDTH: u16 = 1;
        const NARROW_META_WIDTH: u16 = 6;
        const WIDE_META_WIDTH: u16 = 15;

        let gutter = 0;
        let glyph = gutter + 2;
        let (actor, verb) = if width >= ACTOR_BREAK {
            (Some(glyph + 2), glyph + 2 + ACTOR_WIDTH)
        } else {
            (None, glyph + 2)
        };
        let verb_folded = width < VERB_BREAK;
        let summary = verb
            + if verb_folded {
                NARROW_VERB_WIDTH
            } else {
                VERB_WIDTH
            };
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
            verb_folded,
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

    /// Shortens every worktree-rooted (or otherwise abbreviate-able) path
    /// substring *within* arbitrary text, rather than requiring the whole string
    /// to be one path. Applies uniformly to every record's argument and result
    /// text regardless of which tool emitted it (AC-12): a Run/Bash argument is a
    /// shell command with paths inline (`cd /worktree/src && cargo test`), and a
    /// non-claude adapter's JSON-truncated argument (`truncate_json`) puts a path
    /// inside a quoted, space-free token (`{"path":"/long/root/src/a.rs"}`) that a
    /// whitespace split alone can never reach.
    ///
    /// A candidate path starts at a `/` whose preceding character (if any) is
    /// neither a path character nor `:` — the `:` exclusion is what keeps a URL's
    /// `scheme://` untouched, since a real absolute path is never introduced by a
    /// bare colon. It then extends through a maximal run of path characters
    /// (alnum, `/.-_+~@`), so it stops cleanly at a closing quote, brace, comma,
    /// or pipe without needing the caller to pre-tokenize the surrounding text.
    pub fn shorten_in_text(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < n {
            let starts_path = chars[i] == '/'
                && (i == 0 || (!Self::is_path_char(chars[i - 1]) && chars[i - 1] != ':'));
            if starts_path {
                let start = i;
                i += 1;
                while i < n && Self::is_path_char(chars[i]) {
                    i += 1;
                }
                let candidate: String = chars[start..i].iter().collect();
                out.push_str(&self.shorten(&candidate));
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }

    fn is_path_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '~' | '@')
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

/// True when `offset` is exactly where an indexed append began — a fresh
/// `LogWriter::append` chunk, not a line wrapped inside the previous one. Used to
/// tell a tool's own continuing output from the next turn's opening line (AC-8).
fn chunk_boundary_at(offset: u64, index: &[(u64, DateTime<Utc>)]) -> bool {
    index.binary_search_by_key(&offset, |(o, _)| *o).is_ok()
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
            // Shortened regardless of `tool`: an unrecognised name (every adapter
            // but claude's own emits its own tool names — `write_file`,
            // `run_command`, `command_execution`, …) must not fall back to the
            // unshortened detail just because `ToolKind::classify` has no entry
            // for it (AC-12). `shorten_in_text` scans for path-shaped substrings
            // anywhere in the text, so it is safe over a bare path, a shell
            // command, or a space-free JSON blob alike.
            let argument = shortener.shorten_in_text(detail);
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
                raw_result: None,
            });
            pending.push_back(records.len() - 1);
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some(rest) = line.strip_prefix('←') {
            let (ok, preview) = split_result_line(rest);
            let clean = shortener.shorten_in_text(strip_tool_id(preview));
            if pending.len() == 1 {
                let idx = pending.pop_front().expect("checked len == 1");
                let call_time = records[idx].time;
                records[idx].result = Some(clean);
                records[idx].ok = Some(ok);
                if !preview.is_empty() {
                    // Raw, not `clean`: `body` is what `Space` reveals, and AC-6
                    // requires expansion to preserve every persisted byte in
                    // order. Shortening is a head/summary-column concession
                    // (AC-12), not a rewrite of the record's own history — the
                    // shortened form already lives in `result` above.
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
                result: Some(clean),
                ok: Some(ok),
                elapsed: None,
                body: Vec::new(),
                source: source_of(abs_start, abs_end),
                // The un-shortened preview: `to_record`'s body synthesis expands
                // into this, not into the shortened `result`, so `Space` on an
                // orphan result shows the real path (AC-6), not the abbreviated
                // one that only the head is allowed to show (AC-12).
                raw_result: Some(preview.to_string()),
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
                raw_result: None,
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
                raw_result: None,
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
                raw_result: None,
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        if let Some((tool, detail)) = api_tool_call(line) {
            let argument = shortener.shorten_in_text(&detail);
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
                raw_result: None,
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
                if !observation.is_empty() {
                    // Mirrors the `←` branch: without this, a merged call's result
                    // exists only in `result` (used for the head summary) and never
                    // in `body`, so a one-line api-sdk result has nothing to fold
                    // and nothing to expand into (AC-6).
                    records[idx].body.push(observation.to_string());
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
                raw_result: None,
            });
            last_idx = Some(records.len() - 1);
            continue;
        }

        // An unmarked line either continues the record just pushed, or starts a
        // fresh `Prose` record — the two must never be confused (AC-8 review
        // finding). Continuation is unconditionally safe only for `Prose`
        // itself: a narrated paragraph's own wrapped lines are meant to
        // coalesce. For a `Tool`/`Result`, it is safe only *within the same
        // indexed append* as the line before it — a genuine multi-line command
        // dump (`ls -la`'s several rows) writes in one `LogWriter::append`
        // alongside its `←` line, but the agent's next turn is a separate
        // append with its own index entry, and treating that as more tool
        // output is exactly what folded real operator-facing narration out of
        // sight in a live run's own log (reproduced against `logs/impl.log`).
        // `Note`/`Error`/`Thought` never accept continuation at all: the
        // coalescer always emits each as one complete line. Without an index
        // there is no signal to tell "same append" from "new turn" apart, so
        // the unindexed case favors visibility (AC-8's "unknown formats remain
        // visible prose") and never continues a marker record either.
        let continues_last = match last_idx.map(|idx| records[idx].kind) {
            Some(RecordKind::Prose) => true,
            // Without an index there is no way to tell "this is the tool's own
            // continuing output" from "this is the next turn's opening line" —
            // in that case, favor the pre-existing grouping (every un-indexed
            // caller, including logs with no sidecar at all, already relies on
            // it to fold multi-line tool output together). *With* an index, use
            // it: a chunk boundary is a real signal a genuinely separate turn
            // started, which is exactly the case that folded real narration out
            // of sight in a live run's own log (AC-8 review finding).
            Some(RecordKind::Tool(_)) | Some(RecordKind::Result { .. }) => {
                index.is_empty() || !chunk_boundary_at(abs_start, index)
            }
            _ => false,
        };
        if continues_last {
            let idx = last_idx.expect("continues_last only set from a resolved last_idx");
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
            raw_result: None,
        });
        last_idx = Some(records.len() - 1);
    }
    // A thought has no result of its own to time itself against (round-9 finding
    // 3, AC-3): its elapsed is how long the reasoning ran before the *next*
    // indexed record started, mirroring how a tool call's elapsed comes from its
    // own result's time rather than an invented duration.
    for i in 0..records.len() {
        if !matches!(records[i].kind, RecordKind::Thought) || records[i].elapsed.is_some() {
            continue;
        }
        let next_time = records.get(i + 1).and_then(|r| r.time);
        if let (Some(start), Some(end)) = (records[i].time, next_time) {
            if end >= start {
                records[i].elapsed = (end - start).to_std().ok();
            }
        }
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
                records.push(doc_record(head, lines, doc_source(path, start)));
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
        records.push(doc_record(head, lines, doc_source(path, start)));
    }
    records
}

fn doc_source(path: &str, start: usize) -> SourceId {
    SourceId::Document {
        path: path.to_string(),
        start: start as u64,
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
        has_command_row: false,
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
        let expanded = is_expanded(r);
        if expanded {
            for j in 0..r.body.len() {
                out.push(FlatRow {
                    record_idx: i,
                    kind: RowKind::Body(j),
                });
            }
        } else if !r.body.is_empty() && r.has_command_row {
            // The command/path row (`to_record`'s inserted `body[0]`) is never
            // part of the fold: AC-19's "standalone command surface" must survive
            // a completed call's result collapsing, not just an open call's. A
            // detail-less call has no such row (`has_command_row` is false), so
            // it folds like any other result-bearing record (AC-6).
            out.push(FlatRow {
                record_idx: i,
                kind: RowKind::Body(0),
            });
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
            has_command_row: false,
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
                has_command_row: false,
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
        },
        folded_by_default: false,
        has_command_row: false,
    }
}

/// One `Note` record saying the Log tab's parsed view is missing its earlier
/// history — the same fact `stream_content`'s raw banner already carries, made
/// visible on the parsed path too (round-9 finding 5): the parse-path never read
/// `_truncated` before this, so a truncated log said nothing was missing.
pub fn truncated_log_notice(run_id: &str, slot_id: &str, tail_kb: usize) -> Record {
    Record {
        kind: RecordKind::Note,
        glyph: "·",
        verb: "Truncated".to_string(),
        head: "Truncated".to_string(),
        summary: format!("earlier log truncated (showing last ~{tail_kb} KB)"),
        body: Vec::new(),
        time: None,
        elapsed: None,
        actor: None,
        ok: None,
        source: SourceId::Log {
            run_id: run_id.to_string(),
            slot_id: slot_id.to_string(),
            start: 0,
            end: 0,
        },
        folded_by_default: false,
        has_command_row: false,
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

    /// AC-3 (round-9 finding 3): a thought has no result of its own to time
    /// against, so its elapsed must come from the next indexed record's time —
    /// pinned end to end, from the index through the rendered head text.
    #[test]
    fn thought_elapsed_comes_from_the_next_indexed_records_time() {
        let text = "… thinking about it\n→ Bash  ls\n";
        let line2_start = text.find('→').expect("second line") as u64;
        let t0 = chrono::Utc::now();
        let t1 = t0 + chrono::Duration::milliseconds(800);
        let index = vec![(0u64, t0), (line2_start, t1)];
        let records = parse_log_records(text, 0, &index, &PathShortener::default(), "r", "s");
        let thought = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Thought))
            .expect("thought record");
        assert!(
            thought.elapsed.is_some(),
            "elapsed must be derived from the next record's time"
        );
        let rendered = thought.to_record();
        assert_eq!(rendered.verb, "Thought for 0.8s", "{}", rendered.verb);
    }

    /// AC-8: a thought with no index at all must never invent a time or elapsed.
    #[test]
    fn thought_with_no_index_has_no_elapsed() {
        let records = parse_log_records(
            "… thinking about it\n→ Bash  ls\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        let thought = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Thought))
            .expect("thought record");
        assert!(thought.elapsed.is_none());
        assert_eq!(thought.to_record().verb, "Thought");
    }

    /// Round-11 review: a long single-line Note/Error/Thought used to have an
    /// empty `body`, which made it permanently unexpandable — `is_record_expanded`
    /// treats an empty body as nothing to fold, so a summary the head truncated
    /// was unreachable by any key. The full text must survive into `body` so
    /// `Space` has something to reveal.
    #[test]
    fn a_long_note_error_or_thought_carries_its_full_text_into_body_for_expansion() {
        let long = "x".repeat(200);
        let records = parse_log_records(
            &format!("· {long}\n! {long}\n… {long}\n"),
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        for record in &records {
            let rendered = record.to_record();
            assert_eq!(
                rendered.body,
                vec![long.clone()],
                "{:?} must carry its full text into body",
                rendered.kind
            );
            assert!(
                rendered.folded_by_default,
                "{:?} must be foldable now that it has a body",
                rendered.kind
            );
        }
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

    /// A `Prose` record's own single line is deliberately *not* copied into its
    /// `body`: unlike Note/Error/Thought/Result, Prose never folds, so its body
    /// paints unconditionally right under the head — copying the identical short
    /// line in would show it twice, adjacent, and shift every row after it (the
    /// same defect round 9 fixed for Tool heads). A single long line with no
    /// continuation is a known, accepted gap.
    #[test]
    fn a_short_prose_record_with_no_continuation_gets_no_synthesized_body() {
        let records = parse_log_records(
            "Checking scope.\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        let rendered = records[0].to_record();
        assert!(rendered.body.is_empty(), "{:?}", rendered.body);
    }

    /// A multi-line prose paragraph, by contrast, must not drop its own first
    /// line once it has continuation lines — the same "first line missing from
    /// body" bug Note/Error had.
    #[test]
    fn a_multiline_prose_paragraph_keeps_its_first_line_in_body() {
        let record = LogRecord {
            time: None,
            direction: LogDirection::Prose,
            kind: RecordKind::Prose,
            tool: None,
            argument: String::new(),
            result: Some("first line".into()),
            ok: None,
            elapsed: None,
            body: vec!["second line".into()],
            source: SourceId::Log {
                run_id: "r".into(),
                slot_id: "s".into(),
                start: 0,
                end: 1,
            },
            raw_result: None,
        };
        let rendered = record.to_record();
        assert_eq!(rendered.body, vec!["first line", "second line"]);
        assert!(!rendered.folded_by_default);
    }

    /// Round-N review finding 1 (continued): a `Note`/`Error` whose `body` already
    /// carries continuation content used to fail the old `body.is_empty()` gate at
    /// `to_record` time, so its own first line was dropped from the expansion
    /// while only what followed it survived. `to_record` synthesis is exercised
    /// directly here (rather than via `parse_log_records`) because the parser
    /// itself never attaches continuation lines to a `Note`/`Error` (AC-8: the
    /// coalescer always emits each as one complete line, and treating a
    /// following line as automatic continuation is what folded real narration
    /// out of sight in the sibling `header_hash_lines_...` regression).
    #[test]
    fn a_note_with_continuation_lines_still_carries_its_own_first_line_into_body() {
        let record = LogRecord {
            time: None,
            direction: LogDirection::Note,
            kind: RecordKind::Note,
            tool: None,
            argument: String::new(),
            result: Some("short note".into()),
            ok: None,
            elapsed: None,
            body: vec!["continuation line".into()],
            source: SourceId::Log {
                run_id: "r".into(),
                slot_id: "s".into(),
                start: 0,
                end: 1,
            },
            raw_result: None,
        };
        let rendered = record.to_record();
        assert_eq!(rendered.body, vec!["short note", "continuation line"]);
    }

    /// Round-N review finding 1 (continued): an orphan (standalone) `←` result was
    /// in the same position as a `Note`/`Error` — its own kind was never in the
    /// old gate's kind match at all.
    #[test]
    fn a_standalone_result_carries_its_full_text_into_body() {
        let records = parse_log_records(
            "← ✓  result for an unknown call\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        assert_eq!(records.len(), 1);
        let rendered = records[0].to_record();
        assert_eq!(rendered.body, vec!["result for an unknown call"]);
        assert!(rendered.folded_by_default);
    }

    /// AC-8 review finding: reproduces the real defect found against a live run's
    /// own `logs/impl.log` — a completed tool call's result line, immediately
    /// followed by the agent's own narration for its *next* turn. Each of the
    /// three lines is its own `LogWriter::append` (one JSON stream event apiece),
    /// so the index gives three distinct offsets; the narration must land as its
    /// own visible `Prose` record, not fold invisibly into the tool call's body.
    #[test]
    fn narration_after_a_completed_tool_call_is_its_own_visible_prose_record() {
        let call = "→ Bash  git status && git log --oneline -5\n";
        let result = "← ✓  toolu_01Nj  On branch spar/3d3d6f59/impl\n";
        let narration = "No nudges. Let's dig into the five contract failures.\n";
        let text = format!("{call}{result}{narration}");
        let result_offset = call.len() as u64;
        let narration_offset = (call.len() + result.len()) as u64;
        let index = vec![
            (0u64, Utc::now()),
            (result_offset, Utc::now()),
            (narration_offset, Utc::now()),
        ];
        let records = parse_log_records(&text, 0, &index, &PathShortener::default(), "r", "s");
        assert_eq!(records.len(), 2, "{records:#?}");
        assert!(matches!(records[0].kind, RecordKind::Tool(_)));
        assert!(
            !records[0].body.iter().any(|l| l.contains("No nudges")),
            "the next turn's narration must not fold into the prior tool call's body: {:?}",
            records[0].body
        );
        assert_eq!(records[1].kind, RecordKind::Prose);
        assert_eq!(
            records[1].result.as_deref(),
            Some("No nudges. Let's dig into the five contract failures.")
        );
        let rendered = records[1].to_record();
        assert!(
            !rendered.folded_by_default,
            "prose is never folded, so the narration is visible on first paint"
        );
    }

    /// The same shape, but the tool's own multi-line output genuinely does share
    /// the `←` line's append (one write covering every row `ls -la` printed) —
    /// that must still coalesce into the tool call's body, not split apart.
    #[test]
    fn genuine_multiline_result_output_in_the_same_append_still_coalesces() {
        let text = "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\ndrwxr-xr-x 139 root\n";
        // One index entry at the start of the whole append: `←`'s line and the
        // `drwxr-xr-x` row that follows it were written together.
        let index = vec![(0u64, Utc::now())];
        let records = parse_log_records(text, 0, &index, &PathShortener::default(), "r", "s");
        assert_eq!(records.len(), 1, "{records:#?}");
        assert!(records[0].body.iter().any(|l| l == "drwxr-xr-x 139 root"));
    }

    /// Round-N review finding 2 (AC-6 regression introduced by the AC-12 fix): a
    /// merged tool result's body must carry the exact persisted bytes, not a
    /// path-shortened rewrite — shortening is a head/summary-column concession
    /// only. `Space` must reveal the real path, not an abbreviation of it.
    #[test]
    fn merged_result_body_keeps_the_real_unshortened_path() {
        let shortener = PathShortener::new(vec![PathBuf::from("/w/root")]);
        let records = parse_log_records(
            "→ Bash  ls\n← ✓  /etc/systemd/system/foo.service\n",
            0,
            &[],
            &shortener,
            "r",
            "s",
        );
        let rendered = records[0].to_record();
        assert!(
            rendered
                .body
                .iter()
                .any(|l| l == "/etc/systemd/system/foo.service"),
            "body must keep the persisted path verbatim, not shortened: {:?}",
            rendered.body
        );
        assert_eq!(
            rendered.summary, "/e/s/system/foo.service",
            "no worktree root matches, but the head still fish-abbreviates"
        );
    }

    /// Same regression as above, but through a shortener that actually rewrites
    /// the path, so a bug reusing the shortened text in `body` cannot hide behind
    /// "the shortener happened to be a no-op".
    #[test]
    fn merged_result_body_is_not_shortened_even_when_the_head_is() {
        let root = "/w/root";
        let shortener = PathShortener::new(vec![PathBuf::from(root)]);
        let text = format!("→ Bash  ls\n← ✓  {root}/src/a.rs\n");
        let records = parse_log_records(&text, 0, &[], &shortener, "r", "s");
        let rendered = records[0].to_record();
        assert_eq!(
            rendered.summary, "src/a.rs",
            "the head is allowed to shorten"
        );
        assert!(
            rendered.body.iter().any(|l| l == "/w/root/src/a.rs"),
            "the body must keep the real, unshortened path: {:?}",
            rendered.body
        );
    }

    /// Round-9 finding 1: a tool record must never paint its own command/argument
    /// twice, adjacent, on head and body — `summary` and `body[0]` must be two
    /// distinct strings (or `summary` empty), never the same text repeated.
    #[test]
    fn tool_record_never_repeats_its_argument_between_summary_and_body() {
        let records = parse_log_records(
            "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        let rendered = records[0].to_record();
        assert_eq!(rendered.body[0], "ls -la /etc | head -5");
        assert_ne!(
            rendered.summary, rendered.body[0],
            "summary must not repeat the command body verbatim: {rendered:#?}"
        );
        // Before a result merges in, there is nothing distinct to show as a
        // summary yet — it must stay empty, not fall back to the command again.
        let open = parse_log_records(
            "→ Bash  ls -la /etc | head -5\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        let open_rendered = open[0].to_record();
        assert_eq!(open_rendered.summary, "", "{open_rendered:#?}");
        assert_eq!(open_rendered.body[0], "ls -la /etc | head -5");
    }

    /// AC-6/AC-19 (round-10 review): a detail-less call (`→ {name}\n`, no
    /// argument) has nothing to insert as a command row — `body[0]` after
    /// merging is the result's own preview, which on the native Claude
    /// coalescer path always carries a provider tool id (`"tool"` literal or
    /// `toolu_…`) that `strip_tool_id` treats as opaque. A text-comparison
    /// heuristic (`body[0] != summary`) is fooled by the id substring into
    /// calling this a genuine command row; `has_command_row` is set explicitly
    /// at construction instead and must stay false here.
    #[test]
    fn detail_less_call_never_gets_a_phantom_command_row() {
        let records = parse_log_records(
            "→ Bash\n← ✓  tool  total 1184\n",
            0,
            &[],
            &PathShortener::default(),
            "r",
            "s",
        );
        let rendered = records[0].to_record();
        assert!(
            !rendered.has_command_row,
            "a detail-less call has no command row: {rendered:#?}"
        );
        let flat = flatten(std::slice::from_ref(&rendered), |_| false);
        assert_eq!(
            flat.len(),
            1,
            "folded with no command row means only the head paints: {flat:#?}"
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

    /// AC-10 (round-review, codex): an indexed append that bundles the header,
    /// the prompt dump, AND the first `→`/`←` marker pair in one atomic write must
    /// not lose the marker just because its chunk-start offset precedes
    /// `prompt_skip_end`. The per-line scan (not a whole-chunk skip decision) is
    /// what makes this work regardless of how the writer happened to batch bytes.
    #[test]
    fn indexed_prompt_suppression_does_not_swallow_a_marker_sharing_its_chunk() {
        let text = "# Role: impl\ncwd=/x\n---\n## Task\nDo the thing.\nI'll start now.\n→ Bash  ls\n← ✓  ok\n";
        let index = vec![(0u64, Utc::now())];
        let records = parse_log_records(text, 0, &index, &PathShortener::default(), "r", "s");
        assert!(
            records
                .iter()
                .any(|r| matches!(r.kind, RecordKind::Tool(_))),
            "the tool call must survive being in the same indexed chunk as the \
             suppressed prompt dump: {records:#?}"
        );
        assert!(
            records
                .iter()
                .all(|r| !r.body.iter().any(|l| l.contains("Do the thing"))
                    && !r.argument.contains("Do the thing")),
            "the headless prompt dump must still be suppressed: {records:#?}"
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
            raw_result: None,
        }
        .to_record()];
        let folded = flatten(&records, |_| false);
        assert_eq!(folded.len(), 1);
        let expanded = flatten(&records, |_| true);
        // "one" (the head text, synthesized into body[0] so it is reachable —
        // AC-6) plus the two continuation lines already on the record.
        assert_eq!(expanded.len(), 4);
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

    /// AC-7 (round-10 review): a document's final section closes its `end` at the
    /// whole document's byte length (`parse_document`), which moves every time the
    /// document grows between two snapshot rebuilds. Identity must survive that the
    /// same way `Log.end` growing does, or fold state and the cursor are lost on
    /// every rebuild of a still-being-written document (a live plan critique).
    #[test]
    fn document_source_identity_survives_the_final_section_growing_as_it_streams() {
        // The exact shape of a live plan critique being tailed between two
        // snapshot rebuilds: the last section's text (and so a naive `end`) grows,
        // but the section itself did not become a different section.
        let before = parse_document(
            "plan-critique",
            "# Critique\nLooks fine so far.\n",
            "plan-critique.md",
        );
        let after = parse_document(
            "plan-critique",
            "# Critique\nLooks fine so far.\nAnd here is more, appended later.\n",
            "plan-critique.md",
        );
        assert_eq!(before.len(), 1);
        assert_eq!(after.len(), 1);
        assert_eq!(
            before[0].source, after[0].source,
            "the same section growing must not change its identity: {before:#?} vs {after:#?}"
        );
        use std::hash::{Hash, Hasher};
        let hash_of = |id: &SourceId| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash_of(&before[0].source), hash_of(&after[0].source));
        let mut fold_open: std::collections::HashSet<SourceId> = std::collections::HashSet::new();
        fold_open.insert(before[0].source.clone());
        assert!(
            fold_open.contains(&after[0].source),
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
