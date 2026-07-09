//! Optional task DAG + waves for --big / structured plans.
use crate::bus;
use crate::paths::SparPaths;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    Ready,
    Running,
    Done,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wave: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskGraph {
    pub run_id: String,
    #[serde(default)]
    pub tasks: Vec<TaskNode>,
    pub updated_at: DateTime<Utc>,
}

impl TaskGraph {
    pub fn path(paths: &SparPaths, run_id: &str) -> PathBuf {
        bus::bus_root(paths, run_id).join("tasks").join("graph.json")
    }

    pub fn load(paths: &SparPaths, run_id: &str) -> Result<Self> {
        let p = Self::path(paths, run_id);
        if !p.is_file() {
            return Ok(Self {
                run_id: run_id.into(),
                tasks: Vec::new(),
                updated_at: Utc::now(),
            });
        }
        let g: Self = serde_json::from_str(&fs::read_to_string(&p)?)
            .with_context(|| format!("parse {}", p.display()))?;
        Ok(g)
    }

    pub fn save(&self, paths: &SparPaths) -> Result<()> {
        bus::ensure_bus(paths, &self.run_id)?;
        let p = Self::path(paths, &self.run_id);
        fs::write(&p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Seed a linear wave DAG from plan headings or numbered items.
    pub fn from_plan_outline(run_id: &str, plan_md: &str) -> Self {
        let mut tasks = Vec::new();
        let mut prev: Option<String> = None;
        let mut wave = 0u32;
        for line in plan_md.lines() {
            let t = line.trim();
            let title = t
                .strip_prefix("### ")
                .or_else(|| t.strip_prefix("- [ ] "))
                .map(|rest| rest.trim().to_string())
                .or_else(|| {
                    t.strip_prefix("1. ")
                        .or_else(|| t.strip_prefix("2. "))
                        .or_else(|| t.strip_prefix("3. "))
                        .or_else(|| t.strip_prefix("4. "))
                        .or_else(|| t.strip_prefix("5. "))
                        .map(|rest| rest.trim().to_string())
                });
            let Some(title) = title else { continue };
            if title.len() < 3 {
                continue;
            }
            let id = format!("t{}", tasks.len() + 1);
            let mut depends_on = Vec::new();
            if let Some(p) = &prev {
                depends_on.push(p.clone());
            }
            tasks.push(TaskNode {
                id: id.clone(),
                title,
                depends_on,
                status: if prev.is_none() {
                    TaskStatus::Ready
                } else {
                    TaskStatus::Pending
                },
                assignee: None,
                wave: Some(wave),
            });
            prev = Some(id);
            wave += 1;
        }
        if tasks.is_empty() {
            tasks.push(TaskNode {
                id: "t1".into(),
                title: "Implement plan".into(),
                depends_on: vec![],
                status: TaskStatus::Ready,
                assignee: None,
                wave: Some(0),
            });
        }
        Self {
            run_id: run_id.into(),
            tasks,
            updated_at: Utc::now(),
        }
    }

    pub fn ready_wave(&self) -> Vec<&TaskNode> {
        let done: HashSet<&str> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| t.id.as_str())
            .collect();
        self.tasks
            .iter()
            .filter(|t| {
                matches!(t.status, TaskStatus::Pending | TaskStatus::Ready)
                    && t.depends_on.iter().all(|d| done.contains(d.as_str()))
            })
            .collect()
    }

    pub fn mark_done(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.status = TaskStatus::Done;
        }
        self.updated_at = Utc::now();
        // promote dependents
        let done: HashSet<String> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Done)
            .map(|t| t.id.clone())
            .collect();
        for t in &mut self.tasks {
            if t.status == TaskStatus::Pending
                && t.depends_on.iter().all(|d| done.contains(d))
            {
                t.status = TaskStatus::Ready;
            }
        }
    }

    #[allow(dead_code)]
    pub fn all_done(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.iter().all(|t| t.status == TaskStatus::Done)
    }

    #[allow(dead_code)]
    pub fn summary_lines(&self) -> Vec<String> {
        self.tasks
            .iter()
            .map(|t| format!("[{:?}] {} — {}", t.status, t.id, t.title))
            .collect()
    }
}

/// Write graph + task files; used by plan --big and implement.
pub fn seed_from_plan(paths: &SparPaths, run_id: &str, plan_md: &str) -> Result<TaskGraph> {
    let g = TaskGraph::from_plan_outline(run_id, plan_md);
    g.save(paths)?;
    for t in &g.tasks {
        let p = bus::bus_root(paths, run_id)
            .join("tasks")
            .join(format!("task-{}.json", t.id));
        fs::write(&p, serde_json::to_string_pretty(t)?)?;
    }
    let _ = bus::broadcast(
        paths,
        run_id,
        "orchestrator",
        format!("task graph seeded: {} tasks", g.tasks.len()),
        bus::MessageBudget::Normal,
    );
    Ok(g)
}

#[allow(dead_code)]
pub fn claim(paths: &SparPaths, run_id: &str, task_id: &str, agent: &str) -> Result<()> {
    let mut g = TaskGraph::load(paths, run_id)?;
    if let Some(t) = g.tasks.iter_mut().find(|t| t.id == task_id) {
        t.assignee = Some(agent.into());
        t.status = TaskStatus::Running;
    }
    g.save(paths)?;
    let mut meta = HashMap::new();
    meta.insert("task_id".into(), task_id.into());
    bus::send(
        paths,
        run_id,
        bus::BusMessage {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            ts: Utc::now(),
            from: agent.into(),
            to: "orchestrator".into(),
            kind: bus::MsgKind::TaskClaim,
            body: format!("claimed {task_id}"),
            subject: Some(task_id.into()),
            refs: bus::MsgRefs {
                task_id: Some(task_id.into()),
                ..Default::default()
            },
            requires_ack: false,
            meta,
        },
        bus::MessageBudget::Normal,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn outline_waves() {
        let plan = "# Plan\n\n### Setup\n### Implement\n### Test\n";
        let g = TaskGraph::from_plan_outline("r1", plan);
        assert!(g.tasks.len() >= 3);
        assert_eq!(g.ready_wave().len(), 1);
        let mut g = g;
        let id = g.ready_wave()[0].id.clone();
        g.mark_done(&id);
        assert_eq!(g.ready_wave().len(), 1);
    }

    #[test]
    fn persist() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let g = seed_from_plan(&paths, "r1", "### A\n### B\n").unwrap();
        assert!(TaskGraph::path(&paths, "r1").is_file());
        assert_eq!(TaskGraph::load(&paths, "r1").unwrap().tasks.len(), g.tasks.len());
    }
}
