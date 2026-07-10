use crate::paths::SparPaths;
use crate::state::{Phase, SlotStatus};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
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
                format!("{t} {msg}")
            }
        }
    }
}

pub fn events_file(paths: &SparPaths, run_id: &str) -> PathBuf {
    paths.run_dir(run_id).join("events.jsonl")
}

pub fn append(paths: &SparPaths, run_id: &str, event: &Event) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let path = events_file(paths, run_id);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    serde_json::to_writer(&mut f, event)?;
    f.write_all(b"\n")?;
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

/// Follow new events from `offset` byte position. Returns new offset and events.
pub fn read_from_offset(
    paths: &SparPaths,
    run_id: &str,
    offset: u64,
) -> Result<(u64, Vec<Event>)> {
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
    let mut out = Vec::new();
    let reader = BufReader::new(&file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<Event>(&line) {
            out.push(ev);
        }
    }
    Ok((len, out))
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
        assert_eq!(next[0].status, Some(SlotStatus::Done));
    }
}
