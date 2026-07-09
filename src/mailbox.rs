use crate::paths::SparPaths;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

impl Message {
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            from: from.into(),
            to: to.into(),
            subject: subject.into(),
            body: body.into(),
            created_at: Utc::now(),
        }
    }
}

fn message_path(paths: &SparPaths, run_id: &str, msg: &Message) -> PathBuf {
    paths.mailbox_dir(run_id).join(format!(
        "{}-{}-{}.json",
        msg.created_at.timestamp_millis(),
        msg.from,
        msg.id
    ))
}

pub fn send(paths: &SparPaths, run_id: &str, msg: &Message) -> Result<PathBuf> {
    paths.ensure_run_dirs(run_id)?;
    let path = message_path(paths, run_id, msg);
    let text = serde_json::to_string_pretty(msg)?;
    fs::write(&path, text).with_context(|| format!("write mailbox {}", path.display()))?;
    Ok(path)
}

pub fn list(paths: &SparPaths, run_id: &str) -> Result<Vec<Message>> {
    let dir = paths.mailbox_dir(run_id);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(entry.path())?;
        if let Ok(msg) = serde_json::from_str::<Message>(&text) {
            out.push(msg);
        }
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn send_and_list() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let msg = Message::new("peer-a", "peer-b", "hello", "world");
        send(&paths, "r1", &msg).unwrap();
        let all = list(&paths, "r1").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].body, "world");
    }
}
