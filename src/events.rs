use crate::paths::SparPaths;
use crate::state::{Phase, SlotStatus};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SlotStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The run's round when a phase event was written. Slot events carry none; they
    /// belong to the round of the phase event before them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Phase,
    Slot,
    Gate,
    Info,
}

impl Event {
    pub fn phase(phase: Phase, prev: Option<Phase>) -> Self {
        Self {
            ts: Utc::now(),
            kind: EventKind::Phase,
            phase: Some(phase),
            prev_phase: prev,
            slot: None,
            status: None,
            message: None,
            round: None,
        }
    }

    pub fn slot(slot: impl Into<String>, status: SlotStatus) -> Self {
        Self {
            ts: Utc::now(),
            kind: EventKind::Slot,
            phase: None,
            prev_phase: None,
            slot: Some(slot.into()),
            status: Some(status),
            message: None,
            round: None,
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            kind: EventKind::Info,
            phase: None,
            prev_phase: None,
            slot: None,
            status: None,
            message: Some(message.into()),
            round: None,
        }
    }

    /// An informational event about one slot: same `Info` kind, with the slot named so
    /// the TUI and `wait --follow --json` can attribute it. Used for nudges and for the
    /// hard-ceiling kill.
    pub fn slot_note(slot: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ts: Utc::now(),
            kind: EventKind::Info,
            phase: None,
            prev_phase: None,
            slot: Some(slot.into()),
            status: None,
            message: Some(message.into()),
            round: None,
        }
    }

    pub fn gate(message: impl Into<String>, phase: Phase) -> Self {
        Self {
            ts: Utc::now(),
            kind: EventKind::Gate,
            phase: Some(phase),
            prev_phase: None,
            slot: None,
            status: None,
            message: Some(message.into()),
            round: None,
        }
    }

    pub fn display_line(&self) -> String {
        let t = self.ts.format("%H:%M:%S");
        match self.kind {
            EventKind::Phase => {
                let phase = self.phase.map(|p| format!("{p:?}")).unwrap_or_default();
                if let Some(prev) = self.prev_phase {
                    format!("{t} phase {prev:?} -> {phase}")
                } else {
                    format!("{t} phase {phase}")
                }
            }
            EventKind::Slot => {
                let slot = self.slot.as_deref().unwrap_or("?");
                let st = self.status.map(|s| format!("{s:?}")).unwrap_or_default();
                format!("{t} slot {slot} {st}")
            }
            EventKind::Gate => {
                let msg = self.message.as_deref().unwrap_or("gate");
                format!("{t} gate {msg}")
            }
            EventKind::Info => {
                let msg = self.message.as_deref().unwrap_or("");
                match self.slot.as_deref() {
                    Some(slot) => format!("{t} {slot} {msg}"),
                    None => format!("{t} {msg}"),
                }
            }
        }
    }
}

pub fn events_file(paths: &SparPaths, run_id: &str) -> PathBuf {
    paths.run_dir(run_id).join("events.jsonl")
}

/// Append one event as a single locked `write_all`.
///
/// `serde_json::to_writer` straight at the `File` issues a syscall per fragment, so two
/// appenders can interleave inside one record. That used to be theoretical here (only the
/// orchestrator's own thread appended); slot dispatch and the nudge watcher now append
/// from per-slot threads, and every reader `continue`s past a line it cannot parse while
/// `read_from_offset` advances beyond it, so a torn record is lost silently rather than
/// loudly. Mirrors `bus::append_jsonl`, which locks for exactly this reason.
pub fn append(paths: &SparPaths, run_id: &str, event: &Event) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let path = events_file(paths, run_id);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)
        .with_context(|| format!("lock {}", path.display()))?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    f.write_all(&line)?;
    Ok(())
}

pub fn read_all(paths: &SparPaths, run_id: &str) -> Result<Vec<Event>> {
    let path = events_file(paths, run_id);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(ev) => out.push(ev),
            Err(_) => continue,
        }
    }
    Ok(out)
}

/// Follow new events from `offset` byte position. Returns the offset just past the last
/// complete line, and each event paired with the offset just past its own line.
///
/// Only newline-terminated lines are consumed: a record still being appended is left for
/// the next read instead of failing to parse and being stepped over for good. A complete
/// line that does not parse is skipped.
pub fn read_from_offset(
    paths: &SparPaths,
    run_id: &str,
    offset: u64,
) -> Result<(u64, Vec<(u64, Event)>)> {
    let path = events_file(paths, run_id);
    if !path.is_file() {
        return Ok((offset, Vec::new()));
    }
    let mut file = std::fs::File::open(&path)?;
    let len = file.metadata()?.len();
    if offset > len {
        return Ok((len, Vec::new()));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let mut out = Vec::new();
    let mut start = 0usize;
    while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
        let end = start + nl + 1;
        let line = &buf[start..end];
        start = end;
        if line.trim_ascii().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_slice::<Event>(line) {
            out.push((offset + end as u64, ev));
        }
    }
    Ok((offset + start as u64, out))
}

/// One line of `spar events`: the event, which run it came from, and the cursor that
/// resumes the feed just after it.
#[derive(Debug, Clone, Serialize)]
pub struct FeedEvent {
    pub run_id: String,
    pub cursor: String,
    #[serde(flatten)]
    pub event: Event,
}

const CURSOR_VERSION: &str = "1";

/// The project-wide event feed (O146): every run's `events.jsonl`, multiplexed.
///
/// Position is a byte offset per run, held for every run the feed has read past the start
/// of. A run with no entry is read from the beginning, which is what makes a run created
/// after a cursor was issued replay in full on resume.
pub struct Feed {
    offsets: std::collections::BTreeMap<String, u64>,
    only: Option<String>,
}

impl Feed {
    /// A feed positioned at `cursor` (an empty cursor is the start of every run).
    pub fn from_cursor(cursor: &str, only: Option<String>) -> Result<Self> {
        let mut offsets = std::collections::BTreeMap::new();
        let mut parts = cursor.split(',');
        let version = parts.next().unwrap_or_default();
        if !cursor.is_empty() && version != CURSOR_VERSION {
            anyhow::bail!("invalid cursor: {cursor}");
        }
        for part in parts {
            let parsed = part
                .rsplit_once(':')
                .and_then(|(id, off)| Some((id, off.parse::<u64>().ok()?)));
            let Some((id, off)) = parsed.filter(|(id, _)| !id.is_empty()) else {
                anyhow::bail!("invalid cursor: {cursor}");
            };
            offsets.insert(id.to_string(), off);
        }
        Ok(Self { offsets, only })
    }

    /// A feed positioned at the end of every complete line already written.
    pub fn at_tail(paths: &SparPaths, only: Option<String>) -> Result<Self> {
        let mut feed = Self::from_cursor("", only)?;
        for id in feed.run_ids(paths)? {
            let Ok(bytes) = std::fs::read(events_file(paths, &id)) else {
                continue;
            };
            let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            if end > 0 {
                feed.offsets.insert(id, end as u64);
            }
        }
        Ok(feed)
    }

    pub fn cursor(&self) -> String {
        let mut out = CURSOR_VERSION.to_string();
        for (id, off) in self.offsets.iter().filter(|(_, off)| **off > 0) {
            out.push_str(&format!(",{id}:{off}"));
        }
        out
    }

    fn run_ids(&self, paths: &SparPaths) -> Result<Vec<String>> {
        if let Some(id) = &self.only {
            return Ok(vec![id.clone()]);
        }
        let dir = paths.runs_dir();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(ids)
    }

    /// Everything appended since the last poll, merged across runs by timestamp. Each
    /// run's own lines keep their file order, so a cursor never jumps past an unread line.
    ///
    /// Archived runs are read past without being emitted (unless the feed was asked for
    /// that run by name). Bytes are read before the archive check: `RunState::save`
    /// writes `state.json` before it appends the event, so the phase event that brings a
    /// run back out of the archive is never mistaken for one written while archived.
    pub fn poll(&mut self, paths: &SparPaths) -> Result<Vec<FeedEvent>> {
        self.offsets.retain(|id, _| paths.run_dir(id).is_dir());
        let mut pending = Vec::new();
        for id in self.run_ids(paths)? {
            let off = self.offsets.get(&id).copied().unwrap_or(0);
            let (next, evs) = read_from_offset(paths, &id, off)?;
            if next == off {
                continue;
            }
            let archived = self.only.is_none()
                && crate::state::RunState::load(paths, &id).is_ok_and(|s| s.is_archived());
            if archived || evs.is_empty() {
                self.offsets.insert(id, next);
                continue;
            }
            pending.push((id, std::collections::VecDeque::from(evs), next));
        }
        let mut out = Vec::new();
        while let Some(i) = pending
            .iter()
            .enumerate()
            .filter_map(|(i, (id, q, _))| q.front().map(|(_, ev)| (i, ev.ts, id)))
            .min_by(|a, b| (a.1, a.2).cmp(&(b.1, b.2)))
            .map(|(i, _, _)| i)
        {
            let (id, queue, next) = &mut pending[i];
            let (end, event) = queue.pop_front().expect("front checked");
            let end = if queue.is_empty() { *next } else { end };
            self.offsets.insert(id.clone(), end);
            out.push(FeedEvent {
                run_id: id.clone(),
                cursor: self.cursor(),
                event,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_and_read() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        append(&paths, "r1", &Event::phase(Phase::Init, None)).unwrap();
        append(
            &paths,
            "r1",
            &Event::phase(Phase::AwaitingPlanApproval, Some(Phase::Init)),
        )
        .unwrap();
        let evs = read_all(&paths, "r1").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].kind, EventKind::Phase);
        assert_eq!(evs[1].phase, Some(Phase::AwaitingPlanApproval));
    }

    #[test]
    fn offset_follow() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        append(&paths, "r1", &Event::slot("s1", SlotStatus::Running)).unwrap();
        let (off, first) = read_from_offset(&paths, "r1", 0).unwrap();
        assert_eq!(first.len(), 1);
        append(&paths, "r1", &Event::slot("s1", SlotStatus::Done)).unwrap();
        let (_, next) = read_from_offset(&paths, "r1", off).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].1.status, Some(SlotStatus::Done));
    }

    fn at(secs: i64, msg: &str) -> Event {
        let mut ev = Event::info(msg);
        ev.ts = DateTime::<Utc>::from_timestamp(1_800_000_000 + secs, 0).unwrap();
        ev
    }

    fn messages(lines: &[FeedEvent]) -> Vec<String> {
        lines
            .iter()
            .map(|l| format!("{}/{}", l.run_id, l.event.message.as_deref().unwrap()))
            .collect()
    }

    #[test]
    fn torn_last_line_waits_for_its_newline() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        append(&paths, "r1", &Event::info("one")).unwrap();
        let mut line = serde_json::to_vec(&Event::info("two")).unwrap();
        let path = events_file(&paths, "r1");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&line[..10]).unwrap();

        let (off, evs) = read_from_offset(&paths, "r1", 0).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(off, evs[0].0, "stops at the last complete line");

        line.push(b'\n');
        f.write_all(&line[10..]).unwrap();
        let (_, evs) = read_from_offset(&paths, "r1", off).unwrap();
        assert_eq!(evs.len(), 1, "the finished line is read whole");
        assert_eq!(evs[0].1.message.as_deref(), Some("two"));
    }

    #[test]
    fn feed_merges_runs_by_time_keeping_each_runs_file_order() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        append(&paths, "a", &at(1, "a1")).unwrap();
        append(&paths, "a", &at(5, "a2")).unwrap();
        append(&paths, "b", &at(3, "b1")).unwrap();
        // Out of time order inside one run: file order still wins for that run.
        append(&paths, "b", &at(2, "b2")).unwrap();
        let mut feed = Feed::from_cursor("", None).unwrap();
        let lines = feed.poll(&paths).unwrap();
        assert_eq!(messages(&lines), ["a/a1", "b/b1", "b/b2", "a/a2"]);
        assert!(feed.poll(&paths).unwrap().is_empty());
    }

    #[test]
    fn resume_from_any_cursor_has_no_loss_or_duplicates() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        append(&paths, "a", &at(10, "a1")).unwrap();
        append(&paths, "b", &at(11, "b1")).unwrap();
        let mut live = Feed::from_cursor("", None).unwrap();
        let mut seen = live.poll(&paths).unwrap();
        let disconnected_at = seen[0].cursor.clone();

        // A late append carries an older timestamp than events already emitted, and a
        // run appears that did not exist when the cursor was issued.
        append(&paths, "a", &at(5, "a2")).unwrap();
        append(&paths, "c", &at(1, "c1")).unwrap();
        seen.extend(live.poll(&paths).unwrap());
        append(&paths, "b", &at(12, "b2")).unwrap();
        seen.extend(live.poll(&paths).unwrap());
        let all = messages(&seen);
        assert_eq!(all.len(), 5);

        for (i, line) in seen.iter().enumerate() {
            let mut resumed = Feed::from_cursor(&line.cursor, None).unwrap();
            let rest = messages(&resumed.poll(&paths).unwrap());
            let mut expected = all[i + 1..].to_vec();
            let mut got = rest.clone();
            expected.sort();
            got.sort();
            assert_eq!(got, expected, "resume after line {i}");
        }
        let mut resumed = Feed::from_cursor(&disconnected_at, None).unwrap();
        assert_eq!(resumed.poll(&paths).unwrap().len(), 4);
    }

    #[test]
    fn tail_starts_after_complete_lines_only() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        append(&paths, "a", &at(1, "old")).unwrap();
        let mut line = serde_json::to_vec(&at(2, "new")).unwrap();
        let path = events_file(&paths, "a");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&line[..5]).unwrap();
        let mut feed = Feed::at_tail(&paths, None).unwrap();
        line.push(b'\n');
        f.write_all(&line[5..]).unwrap();
        assert_eq!(messages(&feed.poll(&paths).unwrap()), ["a/new"]);
    }

    #[test]
    fn malformed_cursor_is_refused() {
        assert!(Feed::from_cursor("2,a:1", None).is_err());
        assert!(Feed::from_cursor("1,a", None).is_err());
        assert!(Feed::from_cursor("1,a:x", None).is_err());
        assert_eq!(
            Feed::from_cursor("1,a:3,b:0", None).unwrap().cursor(),
            "1,a:3"
        );
    }
}
