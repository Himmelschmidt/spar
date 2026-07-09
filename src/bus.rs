//! Run-scoped swarm bus (A2A). Replaces thin mailbox as the coordination plane.
use crate::paths::SparPaths;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsgKind {
    Chat,
    Status,
    Blocked,
    Unblocked,
    Contract,
    ReviewFinding,
    TaskClaim,
    TaskDone,
    Steer,
    Ack,
    System,
    Hello,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub from: String,
    pub to: String,
    pub kind: MsgKind,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default)]
    pub refs: MsgRefs,
    #[serde(default)]
    pub requires_ack: bool,
    #[serde(default)]
    pub meta: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MsgRefs {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub agent: String,
    pub status: String,
    pub ts: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reserve {
    pub path: String,
    pub holder: String,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReservesFile {
    #[serde(default)]
    pub claims: Vec<Reserve>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageBudget {
    None,
    #[default]
    Lean,
    Normal,
    Chatty,
}

impl MessageBudget {
    pub fn max_messages(&self) -> Option<usize> {
        match self {
            MessageBudget::None => Some(0),
            MessageBudget::Lean => Some(40),
            MessageBudget::Normal => Some(200),
            MessageBudget::Chatty => None,
        }
    }
}

pub fn bus_root(paths: &SparPaths, run_id: &str) -> PathBuf {
    paths.run_dir(run_id).join("bus")
}

pub fn ensure_bus(paths: &SparPaths, run_id: &str) -> Result<()> {
    paths.ensure_run_dirs(run_id)?;
    let root = bus_root(paths, run_id);
    for d in [
        root.clone(),
        root.join("inbox"),
        root.join("tasks"),
    ] {
        fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    }
    Ok(())
}

pub fn events_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    bus_root(paths, run_id).join("events.jsonl")
}

pub fn agents_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    bus_root(paths, run_id).join("agents.jsonl")
}

pub fn reserves_path(paths: &SparPaths, run_id: &str) -> PathBuf {
    bus_root(paths, run_id).join("reserves.json")
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub fn join(
    paths: &SparPaths,
    run_id: &str,
    agent: &str,
    provider: Option<&str>,
    backend: Option<&str>,
) -> Result<()> {
    ensure_bus(paths, run_id)?;
    let p = Presence {
        agent: agent.into(),
        status: "joined".into(),
        ts: Utc::now(),
        backend: backend.map(str::to_string),
        provider: provider.map(str::to_string),
    };
    append_jsonl(&agents_path(paths, run_id), &p)?;
    send(
        paths,
        run_id,
        BusMessage {
            id: new_id(),
            ts: Utc::now(),
            from: agent.into(),
            to: "broadcast".into(),
            kind: MsgKind::System,
            body: format!("{agent} joined"),
            subject: Some("join".into()),
            refs: MsgRefs::default(),
            requires_ack: false,
            meta: HashMap::new(),
        },
        MessageBudget::Chatty,
    )?;
    Ok(())
}

pub fn heartbeat(paths: &SparPaths, run_id: &str, agent: &str, status: &str) -> Result<()> {
    ensure_bus(paths, run_id)?;
    let p = Presence {
        agent: agent.into(),
        status: status.into(),
        ts: Utc::now(),
        backend: None,
        provider: None,
    };
    append_jsonl(&agents_path(paths, run_id), &p)
}

pub fn send(
    paths: &SparPaths,
    run_id: &str,
    msg: BusMessage,
    budget: MessageBudget,
) -> Result<BusMessage> {
    ensure_bus(paths, run_id)?;
    if let Some(max) = budget.max_messages() {
        let n = count_events(paths, run_id)?;
        if n >= max {
            bail!("message budget exhausted ({max} messages)");
        }
    }
    append_jsonl(&events_path(paths, run_id), &msg)?;
    deliver_inbox(paths, run_id, &msg)?;
    // also mirror to legacy mailbox for tools that still read it
    let _ = crate::mailbox::send(
        paths,
        run_id,
        &crate::mailbox::Message {
            id: msg.id.clone(),
            from: msg.from.clone(),
            to: msg.to.clone(),
            subject: msg.subject.clone().unwrap_or_else(|| format!("{:?}", msg.kind)),
            body: msg.body.clone(),
            created_at: msg.ts,
        },
    );
    Ok(msg)
}

pub fn chat(
    paths: &SparPaths,
    run_id: &str,
    from: &str,
    to: &str,
    body: impl Into<String>,
    budget: MessageBudget,
) -> Result<BusMessage> {
    send(
        paths,
        run_id,
        BusMessage {
            id: new_id(),
            ts: Utc::now(),
            from: from.into(),
            to: to.into(),
            kind: MsgKind::Chat,
            body: body.into(),
            subject: None,
            refs: MsgRefs::default(),
            requires_ack: false,
            meta: HashMap::new(),
        },
        budget,
    )
}

pub fn broadcast(
    paths: &SparPaths,
    run_id: &str,
    from: &str,
    body: impl Into<String>,
    budget: MessageBudget,
) -> Result<BusMessage> {
    chat(paths, run_id, from, "broadcast", body, budget)
}

fn deliver_inbox(paths: &SparPaths, run_id: &str, msg: &BusMessage) -> Result<()> {
    let targets: Vec<String> = if msg.to == "broadcast" || msg.to == "*" {
        list_presence(paths, run_id)?
            .into_iter()
            .map(|p| p.agent)
            .filter(|a| a != &msg.from)
            .collect()
    } else {
        vec![msg.to.clone()]
    };
    for t in targets {
        let dir = bus_root(paths, run_id).join("inbox").join(&t);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}-{}.json", msg.ts.timestamp_millis(), msg.id));
        fs::write(&path, serde_json::to_string_pretty(msg)?)?;
    }
    Ok(())
}

pub fn list_events(paths: &SparPaths, run_id: &str) -> Result<Vec<BusMessage>> {
    read_jsonl(&events_path(paths, run_id))
}

pub fn list_presence(paths: &SparPaths, run_id: &str) -> Result<Vec<Presence>> {
    let rows: Vec<Presence> = read_jsonl(&agents_path(paths, run_id))?;
    // last status per agent
    let mut map: HashMap<String, Presence> = HashMap::new();
    for p in rows {
        map.insert(p.agent.clone(), p);
    }
    let mut out: Vec<_> = map.into_values().collect();
    out.sort_by(|a, b| a.agent.cmp(&b.agent));
    Ok(out)
}

#[allow(dead_code)]
pub fn inbox(paths: &SparPaths, run_id: &str, agent: &str) -> Result<Vec<BusMessage>> {
    let dir = bus_root(paths, run_id).join("inbox").join(agent);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<BusMessage> = Vec::new();
    for e in fs::read_dir(&dir)? {
        let e = e?;
        if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(m) = serde_json::from_str(&fs::read_to_string(e.path())?) {
            out.push(m);
        }
    }
    out.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(out)
}

pub fn reserve(paths: &SparPaths, run_id: &str, path: &str, holder: &str) -> Result<()> {
    ensure_bus(paths, run_id)?;
    let mut file = load_reserves(paths, run_id)?;
    if let Some(c) = file.claims.iter().find(|c| c.path == path && c.holder != holder) {
        bail!("path {path} already reserved by {}", c.holder);
    }
    file.claims.retain(|c| c.path != path);
    file.claims.push(Reserve {
        path: path.into(),
        holder: holder.into(),
        ts: Utc::now(),
    });
    save_reserves(paths, run_id, &file)
}

pub fn release(paths: &SparPaths, run_id: &str, path: &str, holder: &str) -> Result<()> {
    let mut file = load_reserves(paths, run_id)?;
    file.claims
        .retain(|c| !(c.path == path && c.holder == holder));
    save_reserves(paths, run_id, &file)
}

#[allow(dead_code)]
pub fn list_reserves(paths: &SparPaths, run_id: &str) -> Result<Vec<Reserve>> {
    Ok(load_reserves(paths, run_id)?.claims)
}

fn load_reserves(paths: &SparPaths, run_id: &str) -> Result<ReservesFile> {
    let p = reserves_path(paths, run_id);
    if !p.is_file() {
        return Ok(ReservesFile::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(p)?)?)
}

fn save_reserves(paths: &SparPaths, run_id: &str, file: &ReservesFile) -> Result<()> {
    ensure_bus(paths, run_id)?;
    fs::write(
        reserves_path(paths, run_id),
        serde_json::to_string_pretty(file)?,
    )?;
    Ok(())
}

fn count_events(paths: &SparPaths, run_id: &str) -> Result<usize> {
    let p = events_path(paths, run_id);
    if !p.is_file() {
        return Ok(0);
    }
    Ok(fs::read_to_string(p)?.lines().filter(|l| !l.trim().is_empty()).count())
}

fn append_jsonl<T: Serialize>(path: &PathBuf, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    serde_json::to_writer(&mut f, value)?;
    f.write_all(b"\n")?;
    Ok(())
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<Vec<T>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str(line) {
            out.push(v);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn send_inbox_reserve() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        join(&paths, "r1", "a", Some("cli:claude"), Some("native-cli")).unwrap();
        join(&paths, "r1", "b", Some("cli:grok"), Some("native-cli")).unwrap();
        chat(&paths, "r1", "a", "b", "hello", MessageBudget::Normal).unwrap();
        let inbox_b = inbox(&paths, "r1", "b").unwrap();
        assert!(!inbox_b.is_empty());
        reserve(&paths, "r1", "src/foo.rs", "a").unwrap();
        assert!(reserve(&paths, "r1", "src/foo.rs", "b").is_err());
        release(&paths, "r1", "src/foo.rs", "a").unwrap();
        reserve(&paths, "r1", "src/foo.rs", "b").unwrap();
    }
}
