//! Parser for a reviewer's markdown artifact.
//!
//! The verdict is read as an *anchored header*: the first non-blank line under the
//! first `## Verdict` heading. An unanchored substring scan is not usable here —
//! reviewer templates spell out `approve | request_changes` in their own format
//! block, and reviewers routinely echo it.

use crate::paths::SparPaths;
use crate::state::RunState;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcStatus {
    Pass,
    Fail,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcLine {
    pub id: String,
    pub status: AcStatus,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    Major,
    Minor,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::Major => "major",
            Severity::Minor => "minor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// First `path.ext:line` the finding cites, when it cites one.
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    pub text: String,
    /// Written by spar in place of a review it never got, not by a reviewer.
    #[serde(default)]
    pub synthetic: bool,
    /// Listed under `## Follow-up`: work for later, never a ship blocker unless critical.
    #[serde(default)]
    pub follow_up: bool,
}

impl Finding {
    /// Whether this finding holds the ship at threshold `block_on` (O118). A critical
    /// blocks wherever the reviewer filed it: `## Follow-up` is for non-critical work.
    pub fn blocks(&self, block_on: Severity) -> bool {
        if self.synthetic {
            return false;
        }
        self.severity == Severity::Critical || (!self.follow_up && self.severity <= block_on)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewResult {
    /// `None` means no parsable verdict. Callers treat that as blocking.
    #[serde(default)]
    pub verdict: Option<Verdict>,
    /// The commit the reviewer claims to have judged (`Reviewed-Commit:`), lowercase
    /// hex as written. `None` when the line is absent or malformed; the implement
    /// loop's gate treats that exactly like a missing review (fail closed).
    #[serde(default)]
    pub reviewed_commit: Option<String>,
    #[serde(default)]
    pub acceptance: Vec<AcLine>,
    /// Only items that name a severity. A bullet without one is a note, not a finding.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// The artifact carries `SYNTHETIC_REVIEW_MARKER`: spar wrote it because the
    /// reviewer produced nothing usable.
    #[serde(default)]
    pub synthetic: bool,
}

/// First line of every review spar writes on a reviewer's behalf. An HTML comment so it
/// stays invisible in rendered markdown.
pub const SYNTHETIC_REVIEW_MARKER: &str = "<!-- spar:synthetic-review -->";

/// Version of `artifacts/review-<slot>.json`. Bump on any breaking shape change.
pub const REVIEW_JSON_SCHEMA: u32 = 1;

impl ReviewResult {
    /// The fail-closed ship contract, shared by every call site so it cannot silently
    /// drift back to a substring scan: **only an explicit parsed `approve` clears the
    /// gate.** Missing, unparseable, and `request_changes` all block.
    pub fn approves(&self) -> bool {
        self.verdict == Some(Verdict::Approve)
    }

    /// The verdict half of the implement ship gate, at severity threshold `block_on`
    /// (O118). `approve` clears and a missing verdict blocks, as `approves` has it. A
    /// `request_changes` blocks only when it carries a reviewer finding at or above
    /// `block_on`, when spar wrote it (synthetic), or when it names no finding at all:
    /// a reviewer asking for changes it never lists fails closed. One whose findings are
    /// all below the threshold or under `## Follow-up` counts as approve-with-notes.
    pub fn blocks(&self, block_on: Severity) -> bool {
        match self.verdict {
            Some(Verdict::Approve) => false,
            None => true,
            Some(Verdict::RequestChanges) => {
                if self.synthetic {
                    return true;
                }
                let mut real = self.findings.iter().filter(|f| !f.synthetic).peekable();
                real.peek().is_none() || real.any(|f| f.blocks(block_on))
            }
        }
    }
}

/// Same noise set as `parse_suite_result`.
const NOISE: [char; 5] = ['*', '`', '_', '-', ' '];

#[derive(PartialEq)]
enum Section {
    Other,
    Verdict,
    Acceptance,
}

/// First well-formed `Reviewed-Commit: <sha>` line wins. The key is
/// case-insensitive with surrounding whitespace tolerated; the value is 7..=40
/// hex, lowercase-normalized, so a short sha an agent pasted still names its
/// commit. Anything else — absent, non-hex, too short, too long, or the
/// template's non-hex `<full-sha-of-HEAD>` placeholder echoed back — parses as
/// absent, never as a coincidental match.
pub fn parse_reviewed_commit(body: &str) -> Option<String> {
    for raw in body.lines() {
        let Some((key, value)) = raw.split_once(':') else {
            continue;
        };
        // The key is normalized the same way the value below is. A reviewer that
        // bolds or bullets the line (`**Reviewed-Commit:** <sha>`, `- Reviewed-Commit:
        // <sha>`) has still named its commit, and the gate fails closed, so a
        // stricter key than value would reject a real verdict over markup.
        let key = key.trim().trim_matches(['*', '`', '-', '+', ' ', '\t']);
        if !key.eq_ignore_ascii_case("reviewed-commit") {
            continue;
        }
        let v = value.trim().trim_matches(['*', '`', ' ', '\t']);
        if (7..=40).contains(&v.len()) && v.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(v.to_ascii_lowercase());
        }
    }
    None
}

/// Pure string comparison, no git: a full-length verdict sha must equal the
/// panel head; a short (7..=39) one must prefix it. A prefix-match exists by
/// construction, so no `rev-parse --verify` existence check: anything else
/// fails closed anyway.
pub fn reviewed_commit_matches(panel_head: &str, parsed: &str) -> bool {
    let head = panel_head.trim().to_ascii_lowercase();
    let sha = parsed.trim().to_ascii_lowercase();
    if sha.is_empty() || head.is_empty() {
        return false;
    }
    if sha.len() >= 40 {
        sha == head
    } else {
        head.starts_with(&sha)
    }
}

pub fn parse_review(body: &str) -> ReviewResult {
    let mut out = ReviewResult {
        reviewed_commit: parse_reviewed_commit(body),
        ..Default::default()
    };
    let mut section = Section::Other;

    for raw in body.lines() {
        if let Some(rest) = raw.trim_start().strip_prefix("##") {
            let title = rest
                .trim_start_matches('#')
                .trim()
                .trim_matches(['*', '`', ':', ' '])
                .to_ascii_lowercase();
            section = match title.as_str() {
                // Latch on the first *parsed* verdict, not the first heading: an empty or
                // skeleton-only first section falls through to a later real one instead of
                // permanently blocking. Once a verdict parses, later sections cannot flip it.
                "verdict" if out.verdict.is_none() => Section::Verdict,
                "acceptance" => Section::Acceptance,
                _ => Section::Other,
            };
            continue;
        }
        match section {
            Section::Verdict => {
                if raw.trim().is_empty() {
                    continue;
                }
                out.verdict = parse_verdict_line(raw);
                section = Section::Other;
            }
            Section::Acceptance => {
                if let Some(line) = parse_ac_line(raw) {
                    out.acceptance.push(line);
                }
            }
            Section::Other => {}
        }
    }
    // Findings are a separate pass so nothing they parse can reach the verdict or the
    // acceptance lines the gate reads.
    out.synthetic = is_synthetic(body);
    out.findings = parse_findings(body, out.synthetic);
    out
}

/// spar writes the marker as the artifact's first line; a reviewer quoting it anywhere
/// else has not made its review spar's.
fn is_synthetic(body: &str) -> bool {
    body.lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim() == SYNTHETIC_REVIEW_MARKER)
}

fn parse_findings(body: &str, synthetic: bool) -> Vec<Finding> {
    let mut findings = FindingsParser::default();
    let mut in_findings = false;
    // A salvaged transcript quotes the template's own `## Findings` block inside a
    // fence, and that is not the reviewer's finding list.
    let mut fence: Option<FenceState> = None;
    for raw in body.lines() {
        let trimmed = raw.trim();
        match &fence {
            Some(open) if fence_closes(trimmed, open) => fence = None,
            Some(_) => {}
            None => fence = fence_open(trimmed),
        }
        if fence.is_some() || fence_open(trimmed).is_some() {
            findings.flush();
            continue;
        }
        let Some(rest) = raw.trim_start().strip_prefix("##") else {
            if in_findings {
                findings.line(raw);
            }
            continue;
        };
        let deeper = rest.starts_with('#');
        let heading = rest.trim_start_matches('#').trim();
        let title = heading
            .trim_matches(['*', '`', ':', ' '])
            .to_ascii_lowercase();
        let follow_up = ["follow-up", "follow up", "followup"]
            .iter()
            .any(|p| title.starts_with(p));
        // Reviewers group findings under `### Major` or title one `### major — ...`;
        // either way a sub-heading inside Findings is still Findings. A `### Follow-up`
        // there opens the follow-up list.
        if in_findings && deeper && !follow_up {
            findings.heading(heading);
            continue;
        }
        findings.flush();
        in_findings = follow_up || title.starts_with("findings");
        findings.follow_up = follow_up;
        // A bare bullet under Follow-up is still follow-up work; it reads as minor.
        findings.group = follow_up.then_some(Severity::Minor);
    }
    findings.flush();
    let mut out = findings.done;
    for f in &mut out {
        f.synthetic = synthetic;
    }
    out
}

#[derive(Default)]
struct FindingsParser {
    done: Vec<Finding>,
    current: Option<Finding>,
    /// Severity named by an enclosing `### Major` heading, inherited by bare bullets.
    group: Option<Severity>,
    /// `current` was opened by a `### F1 (major) — ...` heading, so paragraphs and
    /// plain bullets up to the next heading are its body.
    heading_body: bool,
    prev_blank: bool,
    /// Inside `## Follow-up` rather than `## Findings`.
    follow_up: bool,
}

impl FindingsParser {
    fn flush(&mut self) {
        self.heading_body = false;
        let Some(mut f) = self.current.take() else {
            return;
        };
        f.text = f.text.trim().to_string();
        if let Some((file, line)) = cited_location(&f.text) {
            f.file = Some(file);
            f.line = Some(line);
        }
        self.done.push(f);
    }

    fn start(&mut self, severity: Severity, text: &str) {
        self.flush();
        self.current = Some(Finding {
            severity,
            file: None,
            line: None,
            text: text.trim().to_string(),
            synthetic: false,
            follow_up: self.follow_up,
        });
    }

    fn heading(&mut self, title: &str) {
        self.flush();
        self.prev_blank = false;
        if let Some((severity, rest)) =
            labelled_severity(title.trim_start_matches(['*', '_', '`', '[']))
        {
            self.group = None;
            self.start(severity, rest);
            self.heading_body = true;
            return;
        }
        let Some((severity, rest)) = split_severity(title, true) else {
            self.group = None;
            return;
        };
        let rest_word = rest
            .trim_matches(['*', '_', '`', ' ', ':'])
            .to_ascii_lowercase();
        if rest_word.is_empty() || rest_word == "findings" || rest_word == "issues" {
            self.group = Some(severity);
            return;
        }
        self.group = None;
        self.start(severity, rest);
    }

    fn line(&mut self, raw: &str) {
        if raw.trim().is_empty() {
            self.prev_blank = true;
            return;
        }
        let indent = raw.len() - raw.trim_start().len();
        let after_blank = std::mem::replace(&mut self.prev_blank, false);
        if indent < 2 {
            if let Some(item) = list_item(raw.trim_start()) {
                let explicit = split_severity(item, false);
                if self.heading_body && explicit.is_none() {
                    self.append(raw);
                    return;
                }
                self.flush();
                if let Some((severity, text)) = explicit {
                    self.start(severity, text);
                } else if let Some(severity) = self.group {
                    let bare = item.trim_matches(['.', ' ', '*', '_']).to_ascii_lowercase();
                    if bare != "none" {
                        self.start(severity, item);
                    }
                }
                return;
            }
        }
        if indent < 2 && !self.heading_body && (after_blank || self.current.is_none()) {
            self.flush();
            if let Some((severity, text)) = split_severity(raw.trim(), false) {
                self.start(severity, text);
            }
            return;
        }
        self.append(raw);
    }

    fn append(&mut self, raw: &str) {
        if let Some(f) = self.current.as_mut() {
            f.text.push('\n');
            f.text.push_str(raw.trim());
        }
    }
}

/// The text after a top-level list marker (`-`, `*`, `+`, `1.`, `1)`), if this is one.
fn list_item(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix(['-', '*', '+']) {
        return rest.starts_with([' ', '\t']).then(|| rest.trim_start());
    }
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = s[digits..].strip_prefix(['.', ')'])?;
    rest.starts_with([' ', '\t']).then(|| rest.trim_start())
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// A leading severity and the description after it: `severity: major — text`,
/// `**minor** — text`, `[critical]: text`, `minor (derived) — text`. The template's
/// `critical|major|minor` skeleton is not a severity. Without the `severity:` key a
/// separator (or nothing) must follow the word, so prose like `Minor refactor is fine`
/// is not read as a finding. `bare_ok` admits a word followed by plain text, for
/// headings like `### Major findings`.
fn split_severity(s: &str, bare_ok: bool) -> Option<(Severity, &str)> {
    const MARKUP: [char; 5] = ['*', '_', '`', '[', ' '];
    let s = s.trim_start_matches(MARKUP);
    let (keyed, s) = match strip_prefix_ci(s, "severity") {
        Some(rest) => {
            let rest = rest
                .trim_start_matches(['*', '_', '`', ' '])
                .strip_prefix(':')?;
            (true, rest.trim_start_matches(MARKUP))
        }
        None => (false, s),
    };
    if !keyed {
        if let Some(found) = labelled_severity(s) {
            return Some(found);
        }
    }
    let (severity, rest) = SEVERITY_WORDS
        .into_iter()
        .find_map(|(word, sev)| strip_prefix_ci(s, word).map(|rest| (sev, rest)))?;
    if rest.starts_with(|c: char| c.is_alphanumeric() || c == '|' || c == '/') {
        return None;
    }
    // `[major]` and `**major**` delimit the word themselves.
    let closed = rest.starts_with([']', '*', '_', '`']);
    let mut rest = rest.trim_start_matches(['*', '_', '`', ']', ' ']);
    if rest.starts_with('(') {
        rest = &rest[rest.find(')').map_or(rest.len(), |i| i + 1)..];
        rest = rest.trim_start_matches(['*', '_', '`', ']', ' ']);
    }
    if let Some(after) = strip_separator(rest) {
        rest = after;
    } else if !rest.is_empty() && !keyed && !bare_ok && !closed {
        return None;
    }
    Some((
        severity,
        rest.trim_start_matches(['*', '_', ' ']).trim_end(),
    ))
}

/// `medium` is folded into `minor` so the levels stay three (O100).
const SEVERITY_WORDS: [(&str, Severity); 4] = [
    ("critical", Severity::Critical),
    ("major", Severity::Major),
    ("minor", Severity::Minor),
    ("medium", Severity::Minor),
];

fn strip_separator(s: &str) -> Option<&str> {
    let mut chars = s.chars();
    matches!(chars.next(), Some('—' | '–' | '-' | ':' | '.' | ',' | ';')).then(|| chars.as_str())
}

/// An id-labelled finding whose severity sits first in the parenthetical after the
/// label: `**F-1 (minor · scope)** — text`, `F4 (medium) — text`, `F3 (major, latent)`.
fn labelled_severity(s: &str) -> Option<(Severity, &str)> {
    let label_len = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(s.len());
    let label = &s[..label_len];
    if label.len() > 8
        || !label.starts_with(|c: char| c.is_ascii_alphabetic())
        || !label.contains(|c: char| c.is_ascii_digit())
    {
        return None;
    }
    let inner = s[label_len..].trim_start().strip_prefix('(')?;
    let close = inner.find(')')?;
    let word = inner[..close]
        .trim()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()?;
    let severity = SEVERITY_WORDS
        .into_iter()
        .find(|(w, _)| w.eq_ignore_ascii_case(word))
        .map(|(_, sev)| sev)?;
    let rest = inner[close + 1..].trim_start_matches(['*', '_', '`', ']', ' ']);
    let rest = strip_separator(rest).unwrap_or(rest);
    Some((
        severity,
        rest.trim_start_matches(['*', '_', ' ']).trim_end(),
    ))
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-')
}

/// First `path.ext:line` in `text`. The file name must carry an alphabetic extension,
/// so `12:30` and `AC-1:` are not locations.
fn cited_location(text: &str) -> Option<(String, u32)> {
    for (i, c) in text.char_indices() {
        if c != ':' {
            continue;
        }
        let after = &text[i + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let start = text[..i]
            .char_indices()
            .rev()
            .find(|(_, ch)| !is_path_char(*ch))
            .map_or(0, |(p, ch)| p + ch.len_utf8());
        let path = &text[start..i];
        if path.starts_with("//") {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if stem.is_empty()
            || ext.is_empty()
            || !ext.chars().all(|c| c.is_ascii_alphanumeric())
            || !ext.starts_with(|c: char| c.is_ascii_alphabetic())
        {
            continue;
        }
        let Ok(line) = after[..digits].parse() else {
            continue;
        };
        return Some((path.to_string(), line));
    }
    None
}

/// The last panel the gate read before `round`: every entry of the highest earlier
/// round in `history`. Pass `u32::MAX` for the latest panel.
pub fn panel_before(
    history: &[crate::state::ReviewHistoryEntry],
    round: u32,
) -> Vec<&crate::state::ReviewHistoryEntry> {
    let Some(last) = history.iter().map(|e| e.round).filter(|r| *r < round).max() else {
        return Vec::new();
    };
    history.iter().filter(|e| e.round == last).collect()
}

/// A panel's reviewer findings with duplicates across reviewers folded: same severity,
/// location and whitespace-normalized text, the way history counts them (O102).
/// Synthetic findings are spar's, not a reviewer's, and are left out.
pub fn panel_findings<'a>(
    panel: &[&'a crate::state::ReviewHistoryEntry],
) -> Vec<(&'a str, &'a Finding)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in panel {
        for f in e.review.findings.iter().filter(|f| !f.synthetic) {
            let text = f
                .text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            if seen.insert((f.severity, f.file.clone(), f.line, f.follow_up, text)) {
                out.push((e.slot_id.as_str(), f));
            }
        }
    }
    out
}

/// One finding as a markdown bullet: severity, follow-up mark, location, reviewer, text.
/// Continuation lines are indented under the bullet.
pub fn finding_line(slot_id: &str, f: &Finding) -> String {
    let at = match (&f.file, f.line) {
        (Some(file), Some(line)) => format!(" `{file}:{line}`"),
        _ => String::new(),
    };
    let follow = if f.follow_up { " follow-up" } else { "" };
    let text = f.text.lines().collect::<Vec<_>>().join("\n  ");
    format!("- {}{follow}{at} ({slot_id}): {text}", f.severity.as_str())
}

/// A finding from the last panel, with what has happened to its file since (O120).
#[derive(Debug, Clone, Serialize)]
pub struct OpenFinding {
    pub slot_id: String,
    /// The commit the finding's reviewer judged.
    pub reviewed_commit: Option<String>,
    #[serde(flatten)]
    pub finding: Finding,
    /// `git log --oneline <reviewed_commit>..HEAD -- <file>`, newest first. `None` when
    /// the finding cites no file, its review named no commit, or git could not answer.
    pub touched_since: Option<Vec<String>>,
    /// Its cited file changed after the review, so the finding may describe code that
    /// is gone. A hint for whoever picks it up, never a verdict.
    pub possibly_addressed: bool,
}

impl OpenFinding {
    pub fn line(&self) -> String {
        let mut out = finding_line(&self.slot_id, &self.finding);
        let (Some(commits), Some(file), Some(sha)) = (
            &self.touched_since,
            &self.finding.file,
            &self.reviewed_commit,
        ) else {
            return out;
        };
        if commits.is_empty() {
            return out;
        }
        out.push_str(&format!(
            "\n  possibly addressed: {} commit(s) since `{}` touched `{file}`:",
            commits.len(),
            &sha[..sha.len().min(8)]
        ));
        for c in commits.iter().take(5) {
            out.push_str(&format!("\n    {c}"));
        }
        if commits.len() > 5 {
            out.push_str(&format!("\n    (+{} more)", commits.len() - 5));
        }
        out
    }
}

/// The last panel's reviewer findings, each checked against the commits since its
/// review that touched its cited file, in `cwd` (the implementer's worktree). No model
/// call: one `git log` per distinct (commit, file).
pub fn open_findings(state: &RunState, cwd: &std::path::Path) -> Vec<OpenFinding> {
    let panel = panel_before(&state.review_history, u32::MAX);
    let mut cache: std::collections::HashMap<(String, String), Option<Vec<String>>> =
        std::collections::HashMap::new();
    panel_findings(&panel)
        .into_iter()
        .map(|(slot, f)| {
            let reviewed_commit = panel
                .iter()
                .find(|e| e.slot_id == slot)
                .and_then(|e| e.review.reviewed_commit.clone());
            let touched_since = match (&reviewed_commit, &f.file) {
                (Some(sha), Some(file)) => cache
                    .entry((sha.clone(), file.clone()))
                    .or_insert_with(|| commits_touching(cwd, sha, file))
                    .clone(),
                _ => None,
            };
            OpenFinding {
                slot_id: slot.to_string(),
                reviewed_commit,
                finding: f.clone(),
                possibly_addressed: touched_since.as_ref().is_some_and(|c| !c.is_empty()),
                touched_since,
            }
        })
        .collect()
}

fn commits_touching(cwd: &std::path::Path, since: &str, file: &str) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        // Findings cite repo-relative paths; `:(top)` keeps them so from any cwd.
        .args([
            "log",
            "--oneline",
            &format!("{since}..HEAD"),
            "--",
            &format!(":(top){file}"),
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// Where a run's open findings are checked: the implementer's worktree, else the
/// project root.
pub fn findings_cwd(state: &RunState) -> std::path::PathBuf {
    state
        .slots
        .iter()
        .filter(|s| s.role == crate::state::SlotRole::Implementer)
        .find_map(|s| state.worktrees.iter().find(|w| w.slot_id == s.id))
        .map_or_else(|| state.project_root.clone(), |w| w.path.clone())
}

/// Persist what the gate read from one reviewer: on the slot, in the run's per-round
/// review history, and as `artifacts/review-<slot>.json` beside the markdown.
pub fn record_review(state: &mut RunState, paths: &SparPaths, slot_id: &str, res: &ReviewResult) {
    let Some(slot) = state.slot_mut(slot_id) else {
        return;
    };
    slot.review = Some(res.clone());
    let round = slot.round;
    let provider = slot.provider.clone();
    let model = slot.model.clone();
    state
        .review_history
        .retain(|e| !(e.round == round && e.slot_id == slot_id));
    state.review_history.push(crate::state::ReviewHistoryEntry {
        round,
        slot_id: slot_id.to_string(),
        review: res.clone(),
    });
    let mut v = serde_json::json!({
        "schema_version": REVIEW_JSON_SCHEMA,
        "run_id": state.id,
        "slot_id": slot_id,
        "round": round,
        "provider": provider,
        "model": model,
    });
    if let (Some(obj), Ok(serde_json::Value::Object(review))) =
        (v.as_object_mut(), serde_json::to_value(res))
    {
        obj.extend(review);
    }
    if let Ok(text) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(
            paths.artifact(&state.id, &format!("review-{slot_id}.json")),
            text,
        );
    }
}

/// `request_changes` is tested first so a hedged `request_changes (see findings)`
/// is not mis-scored, and so `approve` never matches `approve is not warranted`.
fn parse_verdict_line(raw: &str) -> Option<Verdict> {
    let line = raw
        .trim()
        // `>` for a quoted verdict; NOISE on both ends so `_approve_` / `- approve` work.
        .trim_start_matches(['>', ' '])
        .trim_matches(NOISE)
        .to_ascii_lowercase()
        .replace(['*', '`'], "");
    if is_verdict_token(&line, "request_changes") {
        return Some(Verdict::RequestChanges);
    }
    if is_verdict_token(&line, "approve") {
        return Some(Verdict::Approve);
    }
    None
}

/// Token must open the line. The rest may be empty, a parenthetical hedge, a
/// separator-introduced remark (`approve — LGTM`, `approve - nits`, `approve: ok`), or
/// trailing sentence punctuation. It must NOT be a format skeleton
/// (`approve | request_changes`) or bare prose (`approve is not warranted`) — a reviewer
/// who never chose must fail closed rather than ship.
///
/// The separator forms matter: a verdict that false-blocks costs a whole fix round and an
/// implementer rotation, which is the very failure this parser exists to prevent.
fn is_verdict_token(line: &str, token: &str) -> bool {
    let Some(rest) = line.strip_prefix(token) else {
        return false;
    };
    let rest = rest.trim_start();
    if rest.is_empty() || rest.starts_with('(') {
        return true;
    }
    // A dash/colon separator marks commentary, not an alternative. `|` is excluded so the
    // template's `approve | request_changes` skeleton stays unparseable.
    if rest.starts_with(['—', '–', '-', ':', ';', ',']) {
        return true;
    }
    rest.chars()
        .all(|c| matches!(c, '.' | '!' | '?' | ',' | ';' | ':'))
}

fn parse_ac_line(raw: &str) -> Option<AcLine> {
    let (id_part, rest) = raw
        .trim()
        .trim_start_matches(['-', '*', ' '])
        .split_once(':')?;
    let id = id_part
        .trim()
        .trim_matches(['*', '`', ' '])
        .to_ascii_uppercase();
    let digits = id.strip_prefix("AC-")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let rest = rest.trim();
    let (word, evidence) = rest
        .split_once('—')
        .or_else(|| rest.split_once(" - "))
        .unwrap_or((rest, ""));
    let status = match word
        .trim()
        .trim_matches(['*', '`', ' '])
        .to_ascii_lowercase()
        .as_str()
    {
        "pass" => AcStatus::Pass,
        "fail" => AcStatus::Fail,
        "unverified" => AcStatus::Unverified,
        _ => return None,
    };
    Some(AcLine {
        id,
        status,
        evidence: evidence.trim().to_string(),
    })
}

/// Characters treated as markup noise around an id: emphasis and code-span delimiters.
/// Distinct from `NOISE` above, which also folds in bullets and spaces for the
/// reviewer-report parser's different grammar.
const ID_NOISE: [char; 3] = ['*', '_', '`'];

fn strip_id_noise(s: &str) -> &str {
    s.trim_start_matches(ID_NOISE)
}

/// Matches a case-insensitive `AC-<digits>` at the start of `s`. Returns the
/// canonical uppercased id and whatever follows the digits.
fn match_ac_digits(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    if !bytes[0].eq_ignore_ascii_case(&b'A')
        || !bytes[1].eq_ignore_ascii_case(&b'C')
        || bytes[2] != b'-'
    {
        return None;
    }
    let digit_len = s[3..].bytes().take_while(u8::is_ascii_digit).count();
    if digit_len == 0 {
        return None;
    }
    Some((format!("AC-{}", &s[3..3 + digit_len]), &s[3 + digit_len..]))
}

/// List/bare item form: optional bullet, optional checkbox, optional noise, the id,
/// optional noise, then a required colon (which may sit on either side of that
/// trailing noise run — `**AC-1:**` and `**AC-1**:` both declare).
fn declared_id_list_or_bare(trimmed: &str) -> Option<String> {
    let s = strip_checkbox(strip_bullet(trimmed));
    let s = strip_id_noise(s);
    let (id, rest) = match_ac_digits(s)?;
    if strip_id_noise(rest).starts_with(':') {
        Some(id)
    } else {
        None
    }
}

/// `-`, `*`, `+` followed by whitespace, or an ordered marker (`\d+[.)]`) followed by
/// whitespace. A marker char with no following whitespace is left untouched — it is
/// markup (`**bold`), not a list item.
fn strip_bullet(s: &str) -> &str {
    if let Some(c) = s.chars().next() {
        if matches!(c, '-' | '*' | '+') {
            let rest = &s[c.len_utf8()..];
            return rest
                .strip_prefix(' ')
                .map_or(s, |r| r.trim_start_matches(' '));
        }
    }
    let digit_len = s.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len > 0 {
        let after = &s[digit_len..];
        if let Some(rest) = after.strip_prefix(['.', ')']) {
            return rest
                .strip_prefix(' ')
                .map_or(s, |r| r.trim_start_matches(' '));
        }
    }
    s
}

fn strip_checkbox(s: &str) -> &str {
    for cb in ["[ ]", "[x]", "[X]"] {
        if let Some(rest) = s.strip_prefix(cb) {
            return rest.trim_start_matches(' ');
        }
    }
    s
}

/// Heading form: `#`s, whitespace, optional noise, the id, optional noise, then a
/// colon, dash, em dash, whitespace, or end of line. Anchored at the start of the
/// heading text, so a heading that merely *mentions* an id further in ("## See AC-30
/// in appendix") does not match.
fn declared_id_heading(trimmed: &str) -> Option<String> {
    let text = trimmed.trim_start_matches('#').trim_start();
    let s = strip_id_noise(text);
    let (id, rest) = match_ac_digits(s)?;
    let rest = strip_id_noise(rest);
    match rest.chars().next() {
        None => Some(id),
        Some(c) if c == ':' || c == '-' || c == '—' || c.is_whitespace() => Some(id),
        _ => None,
    }
}

fn declared_id(trimmed: &str) -> Option<String> {
    if trimmed.starts_with('#') {
        declared_id_heading(trimmed)
    } else {
        declared_id_list_or_bare(trimmed)
    }
}

struct FenceState {
    ch: u8,
    len: usize,
}

/// An opening fence: 3+ of the same backtick/tilde, any indentation. A phantom
/// criterion (a fence not recognized as one) wedges the ship gate; a criterion missed
/// inside a genuine fence only weakens it. Given that asymmetry, over-recognizing
/// fences is the safe direction, so indentation is not a disqualifier here the way it
/// is for CommonMark's top-level fences. An info string (` ```markdown `) may follow.
fn fence_open(trimmed: &str) -> Option<FenceState> {
    let ch = trimmed.as_bytes().first().copied()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = trimmed.bytes().take_while(|&b| b == ch).count();
    (len >= 3).then_some(FenceState { ch, len })
}

/// Whether `trimmed` closes the fence opened by `open`: the same char, at least as
/// long, and nothing but whitespace after the run. An info string on the line
/// (` ```rust `) makes it an opener, not a closer, so a fence nested inside a
/// differently-tagged outer fence cannot close the outer one early.
fn fence_closes(trimmed: &str, open: &FenceState) -> bool {
    if trimmed.as_bytes().first() != Some(&open.ch) {
        return false;
    }
    let len = trimmed.bytes().take_while(|&b| b == open.ch).count();
    len >= open.len && trimmed[len..].trim().is_empty()
}

/// Every `AC-<digits>` this contract *declares*: a checklist/bare-line item or a
/// heading, matched at the start of the line (after trimming and, for list items,
/// stripping bullet/checkbox/emphasis noise) and requiring the terminator the
/// declaration form calls for. A mention anywhere else — mid-sentence, inside Notes
/// or Non-goals prose, or inside a fenced code block — is not a declaration. Ids are
/// uppercased, deduplicated, in first-appearance order.
pub fn parse_contract_criteria(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<FenceState> = None;

    for raw in body.lines() {
        let trimmed = raw.trim_start();

        if let Some(open) = &fence {
            if fence_closes(trimmed, open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_open(trimmed) {
            fence = Some(open);
            continue;
        }

        if let Some(id) = declared_id(trimmed) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// Loose backstop for the vacuous-gate diagnostic: any `AC-<digits>` token anywhere in
/// the body, including mentions and fenced samples. Used only to tell "no criteria
/// because there's no contract" apart from "no criteria despite the contract talking
/// about them" — never to build the gate itself.
pub(crate) fn mentions_criterion_id(body: &str) -> bool {
    let upper = body.to_ascii_uppercase();
    let b = upper.as_bytes();
    let mut i = 0;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"AC-" && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let mut j = i + 3;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 3 {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(body: &str) -> Option<Verdict> {
        parse_review(body).verdict
    }

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn judged(body: &str) -> Option<String> {
        parse_review(body).reviewed_commit
    }

    /// Markup must not cost a reviewer its verdict: the gate fails closed, so a
    /// bolded or bulleted key is a real-world way to lose a real review.
    #[test]
    fn reviewed_commit_survives_markup_on_the_key() {
        for line in [
            format!("**Reviewed-Commit:** {HEAD}"),
            format!("- Reviewed-Commit: {HEAD}"),
            format!("`Reviewed-Commit`: {HEAD}"),
            format!("**Reviewed-Commit**: {HEAD}"),
        ] {
            let body = format!("## Verdict\napprove\n\n{line}\n");
            assert_eq!(judged(&body).as_deref(), Some(HEAD), "{line}");
        }
    }

    #[test]
    fn reviewed_commit_full_sha() {
        let body = format!("## Verdict\napprove\n\nReviewed-Commit: {HEAD}\n");
        assert_eq!(judged(&body).as_deref(), Some(HEAD));
    }

    #[test]
    fn reviewed_commit_short_sha() {
        let body = "## Verdict\napprove\n\nReviewed-Commit: 0123456\n";
        assert_eq!(judged(body).as_deref(), Some("0123456"));
    }

    #[test]
    fn reviewed_commit_key_case_and_spacing_tolerated() {
        assert_eq!(
            judged("reviewed-commit: 0123456\n").as_deref(),
            Some("0123456")
        );
        assert_eq!(
            judged("  Reviewed-Commit  :  0123456  \n").as_deref(),
            Some("0123456")
        );
        assert_eq!(
            judged("REVIEWED-COMMIT: ABCDEF1\n").as_deref(),
            Some("abcdef1")
        );
    }

    #[test]
    fn reviewed_commit_absent() {
        assert_eq!(judged("## Verdict\napprove\n"), None);
    }

    #[test]
    fn reviewed_commit_malformed_parses_as_absent() {
        for bad in [
            "Reviewed-Commit: xyz\n",
            "Reviewed-Commit: 12345\n",
            "Reviewed-Commit: 0123456789abcdef0123456789abcdef012345678\n",
            "Reviewed-Commit: <full-sha-of-HEAD>\n",
            "Reviewed-Commit:\n",
        ] {
            assert_eq!(judged(bad), None, "must parse as absent: {bad:?}");
        }
    }

    #[test]
    fn reviewed_commit_template_skeleton_echo_parses_as_absent() {
        let skeleton =
            "## Verdict\napprove | request_changes\n\nReviewed-Commit: <full-sha-of-HEAD>\n";
        let res = parse_review(skeleton);
        assert_eq!(res.verdict, None);
        assert_eq!(res.reviewed_commit, None);
    }

    #[test]
    fn reviewed_commit_first_well_formed_wins() {
        let body = "Reviewed-Commit: not-hex\nReviewed-Commit: 0123456\nReviewed-Commit: deadbee\n";
        assert_eq!(judged(body).as_deref(), Some("0123456"));
    }

    #[test]
    fn reviewed_commit_match_full_and_short() {
        assert!(reviewed_commit_matches(HEAD, HEAD));
        assert!(reviewed_commit_matches(HEAD, &HEAD[..7]));
        assert!(reviewed_commit_matches(HEAD, &HEAD.to_ascii_uppercase()));
    }

    #[test]
    fn reviewed_commit_mismatch() {
        assert!(!reviewed_commit_matches(HEAD, "deadbeef"));
        assert!(!reviewed_commit_matches(
            HEAD,
            "0123456789abcdef0123456789abcdef01234568"
        ));
        assert!(!reviewed_commit_matches(HEAD, ""));
        assert!(!reviewed_commit_matches("", "0123456"));
    }

    #[test]
    fn verdict_approve() {
        assert_eq!(verdict("## Verdict\napprove\n"), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_request_changes() {
        assert_eq!(
            verdict("## Verdict\nrequest_changes\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn verdict_blank_line_after_header() {
        assert_eq!(verdict("## Verdict\n\n\napprove\n"), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_markup_tolerated() {
        assert_eq!(verdict("## Verdict\n**approve**\n"), Some(Verdict::Approve));
        assert_eq!(verdict("## Verdict\n- `approve`\n"), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_hedged_request_changes() {
        assert_eq!(
            verdict("## Verdict\nrequest_changes (see findings)\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn approve_body_mentioning_request_changes() {
        let body = "## Verdict\napprove\n\n## Findings\n- I considered request_changes but the fix landed.\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));
    }

    #[test]
    fn format_block_echo_does_not_flip() {
        let body = "## Verdict\napprove\n\n## Format\napprove | request_changes\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));
    }

    #[test]
    fn first_verdict_section_wins() {
        let body = "## Verdict\napprove\n\n## Verdict\nrequest_changes\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_missing_is_none() {
        assert_eq!(verdict("## Findings\n- nothing to report\n"), None);
    }

    #[test]
    fn verdict_garbage_is_none() {
        assert_eq!(verdict("## Verdict\nlgtm\n"), None);
        assert_eq!(verdict("## Verdict\napprove is not warranted\n"), None);
    }

    #[test]
    fn format_skeleton_as_verdict_is_none() {
        // templates/reviewer_adversarial.md puts this under ## Verdict as the format
        // skeleton. A reviewer that pastes it without choosing must not ship.
        assert_eq!(verdict("## Verdict\napprove | request_changes\n"), None);
        assert_eq!(verdict("## Verdict\n`approve | request_changes`\n"), None);
    }

    #[test]
    fn empty_first_verdict_section_falls_through_to_the_real_one() {
        // Latching on the first *parsed* verdict (not the first heading) means an empty
        // or skeleton-only section does not permanently block a reviewer who wrote the
        // real verdict further down.
        let body = "## Verdict\n\n## Other\nnoise\n\n## Verdict\napprove\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));

        let skeleton_first =
            "## Verdict\napprove | request_changes\n\n## Verdict\nrequest_changes\n";
        assert_eq!(verdict(skeleton_first), Some(Verdict::RequestChanges));
    }

    #[test]
    fn verdict_with_separator_commentary_is_parsed() {
        // A false block costs a fix round and an implementer rotation, so common hedges
        // must parse. Regression net for the doom loop this parser exists to prevent.
        for body in [
            "## Verdict\napprove — LGTM\n",
            "## Verdict\napprove - minor nits only\n",
            "## Verdict\napprove: ship it\n",
            "## Verdict\n_approve_\n",
            "## Verdict\n> approve\n",
            "## Verdict\n- approve\n",
        ] {
            assert_eq!(verdict(body), Some(Verdict::Approve), "body: {body:?}");
        }
        assert_eq!(
            verdict("## Verdict\nrequest_changes — see findings\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn separator_loosening_does_not_admit_skeleton_or_prose() {
        // The guardrails that must survive the loosening above.
        assert_eq!(verdict("## Verdict\napprove | request_changes\n"), None);
        assert_eq!(verdict("## Verdict\napprove is not warranted\n"), None);
    }

    #[test]
    fn approves_is_fail_closed() {
        // The contract both call sites rely on: anything but a parsed approve blocks.
        // Guards against a silent revert to a substring scan at the call sites.
        assert!(parse_review("## Verdict\napprove\n").approves());
        assert!(!parse_review("## Verdict\nrequest_changes\n").approves());
        assert!(!parse_review("## Verdict\nlgtm\n").approves()); // unparseable
        assert!(!parse_review("## Findings\n- none\n").approves()); // missing
        assert!(!ReviewResult::default().approves());
    }

    #[test]
    fn trailing_punctuation_ok() {
        assert_eq!(verdict("## Verdict\napprove.\n"), Some(Verdict::Approve));
        assert_eq!(
            verdict("## Verdict\nrequest_changes!\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn acceptance_parses_all_three_statuses() {
        let r = parse_review("## Acceptance\nac-1: pass\nAC-2: FAIL\nAC-3: Unverified\n");
        assert_eq!(
            r.acceptance
                .iter()
                .map(|a| (a.id.as_str(), a.status))
                .collect::<Vec<_>>(),
            vec![
                ("AC-1", AcStatus::Pass),
                ("AC-2", AcStatus::Fail),
                ("AC-3", AcStatus::Unverified),
            ]
        );
    }

    #[test]
    fn acceptance_evidence_captured() {
        let r = parse_review("## Acceptance\nAC-1: pass — cargo test output\n");
        assert_eq!(r.acceptance[0].evidence, "cargo test output");
    }

    #[test]
    fn acceptance_hyphen_separator() {
        let r = parse_review("## Acceptance\nAC-1: pass - foo\n");
        assert_eq!(r.acceptance[0].evidence, "foo");
    }

    #[test]
    fn acceptance_bulleted_lines() {
        let r = parse_review("## Acceptance\n- AC-1: pass — x\n");
        assert_eq!(r.acceptance[0].id, "AC-1");
        assert_eq!(r.acceptance[0].evidence, "x");
    }

    #[test]
    fn acceptance_malformed_line_skipped() {
        let body =
            "## Acceptance\nAC-1: pass — a\nI could not verify everything here.\nAC-2: fail — b\n";
        let r = parse_review(body);
        assert_eq!(r.acceptance.len(), 2);
        assert_eq!(r.acceptance[1].id, "AC-2");
    }

    #[test]
    fn acceptance_missing_section_is_empty() {
        assert!(parse_review("## Verdict\napprove\n").acceptance.is_empty());
    }

    #[test]
    fn contract_criteria_extracted_in_order() {
        let body = "## Scenarios\n- [ ] AC-1: foo\n- [ ] AC-3: bar\n- [ ] AC-2: baz\n";
        assert_eq!(parse_contract_criteria(body), ["AC-1", "AC-3", "AC-2"]);
    }

    #[test]
    fn contract_criteria_deduplicated() {
        let body = "## Scenarios\nac-1: thing\n\n## Notes\nAC-1 again\n";
        assert_eq!(parse_contract_criteria(body), ["AC-1"]);
    }

    #[test]
    fn contract_criteria_empty_when_absent() {
        assert!(parse_contract_criteria("## Scenarios\nnothing here\n").is_empty());
    }

    fn ids(n: std::ops::RangeInclusive<u32>) -> Vec<String> {
        n.map(|i| format!("AC-{i}")).collect()
    }

    /// AC-1. The checklist form the house template teaches, plus the bullet and ordered
    /// markers an author reaches for without thinking about it.
    #[test]
    fn contract_checklist_forms_declare() {
        let body = "\
## Scenarios
- [ ] AC-1: unchecked — verify: `x`
- [x] AC-2: checked lower — verify: `x`
- [X] AC-3: checked upper — verify: `x`
* [ ] AC-4: star bullet — verify: `x`
+ [ ] AC-5: plus bullet — verify: `x`
1. [ ] AC-6: ordered dot — verify: `x`
2) [x] AC-7: ordered paren — verify: `x`
3. AC-8: ordered, no checkbox — verify: `x`
- AC-9: plain bullet — verify: `x`
  - [ ] AC-10: indented two — verify: `x`
    - [ ] AC-11: indented four — verify: `x`
";
        assert_eq!(parse_contract_criteria(body), ids(1..=11));
    }

    /// AC-1. Bare line-start and the markdown emphasis an author wraps the id in. The
    /// colon may sit inside or outside the delimiters.
    #[test]
    fn contract_bare_and_emphasised_forms_declare() {
        let body = "\
AC-1: bare line start
- **AC-2:** bold, colon inside
- **AC-3**: bold, colon outside
- `AC-4:` code span, colon inside
- `AC-5`: code span, colon outside
- _AC-6_: underscore
**AC-7:** bold with no bullet
";
        assert_eq!(parse_contract_criteria(body), ids(1..=7));
    }

    /// AC-1. Heading form: a separator after the id is optional, and any level declares.
    #[test]
    fn contract_heading_forms_declare() {
        let body = "\
### AC-1
#### AC-2: with colon
## AC-3 - with dash
###### AC-4 — with em dash
### `AC-5`
";
        assert_eq!(parse_contract_criteria(body), ids(1..=5));
    }

    /// AC-2. The `d995e566` regression: a Notes aside naming a future id made `AC-17` a
    /// criterion no reviewer could report, and the ship gate became unreachable.
    #[test]
    fn contract_prose_mentions_do_not_declare() {
        let body = "\
## Scenarios
- [ ] AC-1: real — verify: `x`
- [ ] AC-2: real — verify: `x`

## Non-goals
- do not implement AC-30 here

## Notes
**The 2 ids are frozen.** Later rounds append `AC-17` onward; they do not renumber.
See AC-1 for the rationale, and compare with AC-18 in the sibling run.
- AC-19 deferred to a later round
- AC-20 was dropped: see the notes above
- [ ] AC-21 — a checklist line with no colon after the id
";
        assert_eq!(parse_contract_criteria(body), ["AC-1", "AC-2"]);
    }

    /// AC-3. A contract that shows the house format in a fenced block declares nothing
    /// from inside it, whatever fence character or length it uses.
    #[test]
    fn contract_fenced_blocks_declare_nothing() {
        let body = "\
## Scenarios
- [ ] AC-1: real — verify: `x`

## How to write one
```
- [ ] AC-40: fenced sample — verify: `x`
```

~~~markdown
- [ ] AC-41: tilde fenced sample — verify: `x`
~~~

````text
```
- [ ] AC-42: nested fence sample — verify: `x`
```
````

  ```md
  - [ ] AC-43: indented fence — verify: `x`
  ```

## Scenarios (continued)
- [ ] AC-2: still real — verify: `x`
";
        assert_eq!(parse_contract_criteria(body), ["AC-1", "AC-2"]);
    }

    /// Review finding on this run: a fence indented four or more spaces is the ordinary
    /// shape for a code sample nested inside a list item, and a cap at three spaces
    /// (CommonMark's rule for top-level fences) let its contents parse as declarations
    /// again, reopening defect 1's exact wedge. Fences declare nothing regardless of
    /// indentation now.
    #[test]
    fn contract_deeply_indented_fence_declares_nothing() {
        let body = "\
## Scenarios
- [ ] AC-1: real — verify: `x`

- an example nested in a list item:
    ```markdown
    - [ ] AC-40: sample — verify: `x`
    ```

- [ ] AC-2: still real — verify: `x`
";
        assert_eq!(parse_contract_criteria(body), ["AC-1", "AC-2"]);
    }

    /// Review finding on this run: a same-length fence line that carries an info
    /// string (` ```rust `) is an opener, not a bare closing delimiter, and must not
    /// close the enclosing fence early. Only a delimiter line with nothing but the
    /// fence characters (plus trailing whitespace) closes.
    #[test]
    fn contract_info_string_line_does_not_close_the_fence() {
        let body = "\
## Scenarios
- [ ] AC-1: real — verify: `x`

```markdown
```rust
- [ ] AC-40: swallowed regardless — verify: `x`
```

- [ ] AC-2: still real — verify: `x`
";
        assert_eq!(parse_contract_criteria(body), ["AC-1", "AC-2"]);
    }

    /// AC-3. An unterminated fence swallows the rest of the file rather than reopening
    /// the token scan, and criteria declared before it survive.
    #[test]
    fn contract_unclosed_fence_keeps_earlier_criteria() {
        let body = "\
## Scenarios
- [ ] AC-1: real — verify: `x`

```
- [ ] AC-50: never closed — verify: `x`
- [ ] AC-51: also swallowed — verify: `x`
";
        assert_eq!(parse_contract_criteria(body), ["AC-1"]);
    }

    /// AC-4. Case folding, first-appearance order and dedup are unchanged by the
    /// grammar narrowing.
    #[test]
    fn contract_case_folded_and_deduplicated_in_declaration_order() {
        let body = "\
## Scenarios
- [ ] ac-2: lower — verify: `x`
- [ ] Ac-1: mixed — verify: `x`
- [ ] AC-2: declared twice — verify: `x`
";
        assert_eq!(parse_contract_criteria(body), ["AC-2", "AC-1"]);
    }

    /// AC-4. `AC-` with no digits, and an id glued to a preceding word, are not tokens.
    #[test]
    fn contract_malformed_ids_declare_nothing() {
        let body =
            "## Scenarios\n- [ ] AC-: no digits\n- [ ] XAC-1: glued\n- [ ] AC-x1: not a digit\n";
        assert!(parse_contract_criteria(body).is_empty());
    }

    /// AC-10. The overlay note appended to the *prompt copy* is declaration-shaped: the
    /// gate must never be built from that copy. Pins the shape the end-to-end guard in
    /// `tests/scenarios/contract_gate.rs` depends on.
    #[test]
    fn overlay_note_line_is_declaration_shaped() {
        let note =
            "## Not copied (git-ignored in the test-author worktree)\n- `AC-99:fixture.json`\n";
        assert_eq!(parse_contract_criteria(note), ["AC-99"]);
    }

    /// AC-5. The reviewer-report side is a different parser with a different contract:
    /// reviewers write bare `AC-n: pass` lines under `## Acceptance` and must keep
    /// working. Narrowing the *contract* grammar must not narrow this one.
    #[test]
    fn reviewer_acceptance_lines_still_parse_without_declaration_syntax() {
        let r = parse_review(
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass — evidence\nac-2: pass - evidence\n- AC-3: fail — broken\n**AC-4**: unverified — no time\n",
        );
        let got: Vec<(&str, AcStatus)> = r
            .acceptance
            .iter()
            .map(|a| (a.id.as_str(), a.status))
            .collect();
        assert_eq!(
            got,
            [
                ("AC-1", AcStatus::Pass),
                ("AC-2", AcStatus::Pass),
                ("AC-3", AcStatus::Fail),
                ("AC-4", AcStatus::Unverified),
            ]
        );
    }

    fn findings(body: &str) -> Vec<(Severity, Option<String>, Option<u32>, String)> {
        parse_review(body)
            .findings
            .into_iter()
            .map(|f| (f.severity, f.file, f.line, f.text))
            .collect()
    }

    /// The template's own shape, as reviewers actually fill it in.
    #[test]
    fn findings_template_shape_with_locations() {
        let body = "## Verdict\nrequest_changes\n\nReviewed-Commit: 0123456789abcdef0123456789abcdef01234567\n\n\
## Acceptance\nAC-1: pass — tests/scenarios/status.rs:40 asserts it\nAC-2: fail — no test\n\n\
## Findings\n\
- severity: major — `resolve_home_target` drops a legitimate Enter after 10 seconds. src/tui.rs:1556-1582, 1717-1732; src/workflow/implement.rs:139-146.\n\
- severity: minor — The ghost-target test assigns `Instant::now().checked_sub(...)` directly.\n\
  On a platform where that returns `None` the test fails spuriously (src/tui.rs:12766).\n\
- severity: critical — `cleanup_one` deletes the run dir before history is written (`main.rs:2066`).\n\n\
## Tests\nran cargo test; 812 passed\n";
        let res = parse_review(body);
        assert_eq!(res.verdict, Some(Verdict::RequestChanges));
        assert_eq!(res.acceptance.len(), 2);
        assert!(!res.synthetic);
        let f = findings(body);
        assert_eq!(f.len(), 3, "{f:?}");
        assert_eq!(f[0].0, Severity::Major);
        assert_eq!(f[0].1.as_deref(), Some("src/tui.rs"));
        assert_eq!(f[0].2, Some(1556));
        assert!(f[0].3.starts_with("`resolve_home_target` drops"));
        assert_eq!(f[1].0, Severity::Minor);
        assert!(
            f[1].3.ends_with("fails spuriously (src/tui.rs:12766)."),
            "{:?}",
            f[1].3
        );
        assert_eq!(f[1].1.as_deref(), Some("src/tui.rs"));
        assert_eq!(f[1].2, Some(12766));
        assert_eq!(f[2].0, Severity::Critical);
        assert_eq!(f[2].1.as_deref(), Some("main.rs"));
        assert_eq!(f[2].2, Some(2066));
        assert!(res.findings.iter().all(|f| !f.synthetic));
    }

    /// Spellings seen in real review corpora: bare severity, bold, qualifiers in
    /// parentheses, colon or period separators, numbered lists.
    #[test]
    fn findings_tolerate_real_world_spellings() {
        let body = "## Verdict\napprove\n\n## Findings\n\
- minor — `biddesk/src/handlers/tasks.rs:479` stamps the wrong column\n\
- **major** — PROVEN. The retry loop never backs off\n\
- **minor — `lib/automation.ts` leaks a timer\n\
- severity: minor (derived) — carried from round 2\n\
- minor (test coverage) — nothing pins the empty case\n\
- minor: the comment at foo.py:12 is stale\n\
- severity: minor. The retired flag is still parsed\n\
1. [major] numbered and bracketed\n\
- Severity: MAJOR — upper case\n";
        let f = findings(body);
        let sev: Vec<Severity> = f.iter().map(|x| x.0).collect();
        use Severity::*;
        assert_eq!(
            sev,
            vec![Minor, Major, Minor, Minor, Minor, Minor, Minor, Major, Major],
            "{f:?}"
        );
        assert_eq!(f[0].1.as_deref(), Some("biddesk/src/handlers/tasks.rs"));
        assert_eq!(f[0].2, Some(479));
        assert_eq!(f[1].3, "PROVEN. The retry loop never backs off");
        assert_eq!(f[3].3, "carried from round 2");
        assert_eq!(f[5].1.as_deref(), Some("foo.py"));
        assert_eq!(f[6].3, "The retired flag is still parsed");
        assert_eq!(f[7].3, "numbered and bracketed");
    }

    /// Notes, `none`, prose that merely starts with a severity word, and the
    /// template's skeleton are not findings.
    #[test]
    fn findings_skip_notes_skeleton_and_prose() {
        let body = "## Verdict\napprove\n\n## Findings\n\
- severity: critical|major|minor — description\n\
- none\n\
- Checked, no issue: there is no self-deadlock\n\
- resolved (was major): prose number un-refuting an address\n\
- note — the coverage warning is misattributed\n\
- Minor refactor of the loop is fine\n\
- minority report is not a severity\n\
None.\n";
        assert!(findings(body).is_empty(), "{:?}", findings(body));
    }

    /// `### Major` groups inherit their severity; a `### major — ...` heading is a
    /// finding itself; any other sub-heading ends the group.
    #[test]
    fn findings_grouped_under_severity_headings() {
        let body = "## Verdict\nrequest_changes\n\n## Findings (most severe first)\n\
### Major\n\
- `ship` never records the PR url (src/ship.rs:253)\n\
- the sweep purges before writing history\n\n\
### Minor findings\n\
- typo in skills/core.md:10\n\
### major — PR title fails the conventional-commit check\n\
### Verified sound\n\
- the lock is released on every path\n\n\
## Tests\n- severity: major — not in findings\n";
        let f = findings(body);
        assert_eq!(f.len(), 4, "{f:?}");
        assert_eq!(f[0].0, Severity::Major);
        assert_eq!(f[0].1.as_deref(), Some("src/ship.rs"));
        assert_eq!(f[1].0, Severity::Major);
        assert_eq!(f[2].0, Severity::Minor);
        assert_eq!(f[2].1.as_deref(), Some("skills/core.md"));
        assert_eq!(f[3].0, Severity::Major);
        assert_eq!(f[3].3, "PR title fails the conventional-commit check");
    }

    /// Times, AC ids and URLs are not file locations.
    #[test]
    fn findings_location_needs_a_file_with_an_extension() {
        assert_eq!(cited_location("at 12:30 the run stalls"), None);
        assert_eq!(cited_location("AC-1: fails"), None);
        assert_eq!(cited_location("see https://example.com:8080/x"), None);
        assert_eq!(
            cited_location("`src/state.rs:354` and src/tui.rs:1"),
            Some(("src/state.rs".into(), 354))
        );
        assert_eq!(
            cited_location("in (w.rs:511-522)"),
            Some(("w.rs".into(), 511))
        );
    }

    /// spar's own stand-in reviews are marked, and a transcript quoting the
    /// template's Findings block inside a fence contributes nothing.
    #[test]
    fn synthetic_review_is_marked_and_fenced_transcript_ignored() {
        let body = format!(
            "{SYNTHETIC_REVIEW_MARKER}\n## Verdict\nrequest_changes\n\n## Findings\n\
- severity: major — review slot interrupted (timeout); partial transcript salvaged below\n\n\
## Tests\nsee partial transcript\n\n## Partial transcript\n\n```\n## Findings\n\
- severity: major — the reviewer was mid-sentence\n```\n"
        );
        let res = parse_review(&body);
        assert!(res.synthetic);
        assert_eq!(res.findings.len(), 1, "{:?}", res.findings);
        assert!(res.findings[0].synthetic);
        assert_eq!(res.findings[0].severity, Severity::Major);
    }

    /// The persisted shape is the contract `status --json` and
    /// `review-<slot>.json` expose; a record from before findings existed loads.
    #[test]
    fn review_result_serializes_stably_and_old_records_load() {
        let res = parse_review(
            "## Verdict\napprove\n\nReviewed-Commit: abcdef1\n\n## Acceptance\nAC-1: pass — ok\n\n## Findings\n- severity: minor — x.rs:3 nit\n",
        );
        let v = serde_json::to_value(&res).unwrap();
        assert_eq!(v["verdict"], "approve");
        assert_eq!(v["reviewed_commit"], "abcdef1");
        assert_eq!(v["acceptance"][0]["status"], "pass");
        assert_eq!(v["findings"][0]["severity"], "minor");
        assert_eq!(v["findings"][0]["file"], "x.rs");
        assert_eq!(v["findings"][0]["line"], 3);
        assert_eq!(v["findings"][0]["synthetic"], false);
        assert_eq!(v["synthetic"], false);
        let old: ReviewResult = serde_json::from_str(r#"{"verdict":"request_changes"}"#).unwrap();
        assert_eq!(old.verdict, Some(Verdict::RequestChanges));
        assert!(old.findings.is_empty());
    }

    /// Verdict and acceptance as the gate reads them. The findings pass must never
    /// move these: each case below is pinned to what the parser returned before
    /// findings existed.
    fn gate_view(body: &str) -> (Option<Verdict>, Vec<(String, AcStatus)>) {
        let r = parse_review(body);
        (
            r.verdict,
            r.acceptance.into_iter().map(|a| (a.id, a.status)).collect(),
        )
    }

    #[test]
    fn findings_never_swallow_a_later_acceptance_subheading() {
        let body = "## Verdict\napprove\n\n## Findings\n- severity: minor — x\n### Acceptance\nAC-1: pass — ok\n";
        assert_eq!(
            gate_view(body),
            (
                Some(Verdict::Approve),
                vec![("AC-1".into(), AcStatus::Pass)]
            )
        );
        assert_eq!(parse_review(body).findings.len(), 1);
    }

    #[test]
    fn an_open_fence_in_findings_never_swallows_acceptance() {
        for fence in ["```text", "```rust and some prose after it"] {
            let body = format!(
                "## Verdict\napprove\n\n## Findings\n- severity: minor — log below\n{fence}\nerror: boom\n## Acceptance\nAC-1: pass — ok\n"
            );
            assert_eq!(
                gate_view(&body),
                (
                    Some(Verdict::Approve),
                    vec![("AC-1".into(), AcStatus::Pass)]
                ),
                "{fence}"
            );
        }
    }

    /// Fails open if broken: a `### Verdict` under Findings is still the first verdict.
    #[test]
    fn a_verdict_subheading_in_findings_still_latches_first() {
        let body = "## Findings\n- severity: major — x\n### Verdict\nrequest_changes\n\n## Verdict\napprove\n";
        assert_eq!(gate_view(body).0, Some(Verdict::RequestChanges));
    }

    /// The marker counts only where spar writes it, as the first non-blank line.
    #[test]
    fn a_quoted_marker_does_not_make_a_review_synthetic() {
        let quoted = format!(
            "## Verdict\napprove\n\n## Findings\n- severity: minor — spar writes\n```\n{SYNTHETIC_REVIEW_MARKER}\n```\n"
        );
        let r = parse_review(&quoted);
        assert!(!r.synthetic);
        assert!(r.findings.iter().all(|f| !f.synthetic));
        let late = format!("## Verdict\napprove\n{SYNTHETIC_REVIEW_MARKER}\n");
        assert!(!parse_review(&late).synthetic);
        let first = format!("\n{SYNTHETIC_REVIEW_MARKER}\n## Verdict\napprove\n");
        assert!(parse_review(&first).synthetic);
    }

    /// Id-labelled findings with the severity in a parenthetical, and `medium`
    /// folded into `minor`, as they appear in real reviews.
    #[test]
    fn findings_labelled_and_medium_shapes() {
        let body = "## Verdict\nrequest_changes\n\n## Findings\n\
- **F-1 (minor · test gap)** — nothing pins the empty case in src/a.rs:4\n\
- severity: medium — the retry is unbounded\n\n\
### F3 (major, latent) — resolver over-collects day 1\n\n\
The loop at src/resolver.rs:88 never stops.\n\
- second detail bullet\n\n\
### F4 (medium) — `{{tenant.name}}` renders empty\n\
### Verified sound\n\
- the lock is released\n";
        let f = findings(body);
        assert_eq!(f.len(), 4, "{f:?}");
        assert_eq!(f[0].0, Severity::Minor);
        assert_eq!(f[0].1.as_deref(), Some("src/a.rs"));
        assert!(f[0].3.starts_with("nothing pins"), "{:?}", f[0].3);
        assert_eq!(f[1].0, Severity::Minor);
        assert_eq!(f[2].0, Severity::Major);
        assert!(f[2].3.starts_with("resolver over-collects day 1"));
        assert!(f[2].3.contains("never stops") && f[2].3.contains("second detail"));
        assert_eq!(f[2].1.as_deref(), Some("src/resolver.rs"));
        assert_eq!(f[3].0, Severity::Minor);
        assert!(findings("## Findings\n- AC-1 (pass) — fine\n- R2 (note) — x\n").is_empty());
    }
    /// The shape real reviewers write when they ask for changes on polish alone: a
    /// test gap, a stale comment, a name. O118: at the default `block_on = major` this
    /// is approve-with-notes; at `minor` it blocks as it did before.
    const MINORS_ONLY: &str = "## Verdict\nrequest_changes\n\nReviewed-Commit: 0123456789abcdef0123456789abcdef01234567\n\n## Acceptance\nAC-1: pass — cargo test ok\n\n## Findings\n- severity: minor — src/config.rs:880 comment still says require_all_criteria is the only knob\n- **F-2 (minor · test gap)** — nothing pins the empty list in src/ship.rs:140\n- severity: medium — rename `x` to `count`\n\n## Tests\ncargo test\n";

    #[test]
    fn minors_only_request_changes_is_approve_with_notes_at_major() {
        let r = parse_review(MINORS_ONLY);
        assert_eq!(r.verdict, Some(Verdict::RequestChanges));
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(!r.blocks(Severity::Major));
        assert!(!r.blocks(Severity::Critical));
        assert!(
            r.blocks(Severity::Minor),
            "block_on = minor restores the old gate"
        );
    }

    #[test]
    fn a_major_or_critical_finding_blocks_at_its_threshold() {
        let major = MINORS_ONLY.replace(
            "- severity: medium",
            "### F3 (major, latent) — the retry at src/net.rs:40 never stops\n- severity: medium",
        );
        let r = parse_review(&major);
        assert!(r.blocks(Severity::Major));
        assert!(!r.blocks(Severity::Critical), "{:?}", r.findings);
        let critical = MINORS_ONLY.replace(
            "- severity: medium",
            "- **critical** — data loss on retry\n- severity: medium",
        );
        assert!(parse_review(&critical).blocks(Severity::Critical));
    }

    /// Fail closed: a reviewer that asks for changes it never lists, or lists only as
    /// bare notes, still blocks at any threshold.
    #[test]
    fn request_changes_with_no_parsable_finding_blocks() {
        let bare = "## Verdict\nrequest_changes\n\n## Findings\n- the error path looks off\n";
        let r = parse_review(bare);
        assert!(r.findings.is_empty());
        assert!(r.blocks(Severity::Critical));
        assert!(parse_review("## Verdict\nrequest_changes\n").blocks(Severity::Critical));
    }

    #[test]
    fn a_missing_verdict_blocks_and_approve_never_does() {
        assert!(parse_review("## Findings\n- severity: minor — x\n").blocks(Severity::Critical));
        let approve = "## Verdict\napprove\n\n## Findings\n- severity: major — x at src/a.rs:1\n";
        assert!(!parse_review(approve).blocks(Severity::Minor));
    }

    /// spar's own stand-in carries a `major` finding, but it would block even as
    /// minor: spar writing a review is not a reviewer clearing the change.
    #[test]
    fn a_synthetic_review_blocks_whatever_its_findings() {
        let body = format!(
            "{SYNTHETIC_REVIEW_MARKER}\n## Verdict\nrequest_changes\n\n## Findings\n- severity: minor — review slot failed\n"
        );
        let r = parse_review(&body);
        assert!(r.synthetic);
        assert!(r.blocks(Severity::Critical));
    }

    #[test]
    fn follow_up_findings_are_parsed_and_never_block_unless_critical() {
        let body = "## Verdict\nrequest_changes\n\n## Findings\n- severity: minor — prior F1 still open at src/a.rs:3\n\n## Follow-up\n- severity: major — the retry policy deserves its own brief\n- rename the helper\n- none\n\n## Tests\nok\n";
        let r = parse_review(body);
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(!r.findings[0].follow_up);
        assert!(r.findings[1].follow_up && r.findings[1].severity == Severity::Major);
        assert!(r.findings[2].follow_up && r.findings[2].severity == Severity::Minor);
        assert!(
            !r.blocks(Severity::Major),
            "a follow-up major is work for later"
        );

        let nested = "## Verdict\nrequest_changes\n\n## Findings\n### Follow-up\n- severity: major — later\n";
        let r = parse_review(nested);
        assert!(r.findings[0].follow_up, "{:?}", r.findings);
        assert!(!r.blocks(Severity::Minor));

        let misfiled =
            "## Verdict\nrequest_changes\n\n## Follow-up\n- severity: critical — data loss\n";
        assert!(parse_review(misfiled).blocks(Severity::Critical));
    }

    #[test]
    fn panel_findings_take_the_latest_earlier_round_and_fold_duplicates() {
        let entry = |round: u32, slot: &str, body: &str| crate::state::ReviewHistoryEntry {
            round,
            slot_id: slot.into(),
            review: parse_review(body),
        };
        let history = vec![
            entry(2, "r1", "## Verdict\nrequest_changes\n## Findings\n- severity: major — old\n"),
            entry(3, "r1", "## Verdict\nrequest_changes\n## Findings\n- severity: major — Same   thing at src/a.rs:1\n"),
            entry(3, "r2", "## Verdict\nrequest_changes\n## Findings\n- severity: major — same thing at src/a.rs:1\n- severity: minor — other\n"),
        ];
        let latest = panel_before(&history, u32::MAX);
        assert_eq!(latest.len(), 2);
        let f = panel_findings(&latest);
        assert_eq!(f.len(), 2, "{f:?}");
        assert_eq!(f[0].0, "r1");
        assert_eq!(panel_before(&history, 3)[0].round, 2);
        assert!(panel_before(&history, 2).is_empty());
        assert_eq!(
            finding_line(f[0].0, f[0].1),
            "- major `src/a.rs:1` (r1): Same   thing at src/a.rs:1"
        );
    }
    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// O120: a finding whose cited file a later commit touched is possibly addressed;
    /// one whose file did not move, or that cites no file, is not.
    #[test]
    fn open_findings_mark_the_ones_whose_file_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "-q"]);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "b\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
        let reviewed = git(dir, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("src/a.rs"), "a2\n").unwrap();
        git(dir, &["commit", "-q", "-am", "fix a"]);

        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, dir.into());
        state.review_history.push(crate::state::ReviewHistoryEntry {
            round: 2,
            slot_id: "rev".into(),
            review: parse_review(&format!(
                "## Verdict\nrequest_changes\n\nReviewed-Commit: {reviewed}\n\n## Findings\n- severity: major — off by one at src/a.rs:1\n- severity: minor — naming at src/b.rs:1\n- severity: minor — no location\n"
            )),
        });
        let open = open_findings(&state, &dir.join("src"));
        assert_eq!(open.len(), 3);
        assert!(open[0].possibly_addressed);
        assert_eq!(open[0].touched_since.as_ref().unwrap().len(), 1);
        assert!(
            open[0]
                .line()
                .contains("possibly addressed: 1 commit(s) since"),
            "{}",
            open[0].line()
        );
        assert!(open[0].line().contains("fix a"));
        assert!(!open[1].possibly_addressed);
        assert_eq!(open[1].touched_since.as_deref(), Some(&[][..]));
        assert!(!open[1].line().contains("possibly"));
        assert!(!open[2].possibly_addressed && open[2].touched_since.is_none());
    }
}
