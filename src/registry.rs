//! Global project registry so `spar` can list runs from anywhere.
//!
//! Layout (override with `SPAR_HOME`):
//!   ~/.spar/registry.json
//!
//! Runs still live under each project’s `.spar/runs/` (worktrees, isolation).
//! The registry only tracks project roots we’ve seen — no hardcoded scan paths.
//! Projects appear when you run spar there or a run is saved.
use crate::paths::SparPaths;
use crate::state::{self, RunSummary};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "registry_version")]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

fn registry_version() -> u32 {
    REGISTRY_VERSION
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub last_seen: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
}

/// Global spar home: `$SPAR_HOME` or `~/.spar`.
pub fn spar_home() -> PathBuf {
    if let Ok(p) = std::env::var("SPAR_HOME") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".spar")
}

pub fn registry_path() -> PathBuf {
    spar_home().join("registry.json")
}

/// One-time copy from the short-lived XDG path if `~/.spar/registry.json` is missing.
fn maybe_migrate_from_xdg() {
    let dest = registry_path();
    if dest.is_file() {
        return;
    }
    let Some(data) = dirs::data_local_dir() else {
        return;
    };
    let legacy = data.join("spar").join("registry.json");
    if !legacy.is_file() {
        return;
    }
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(&legacy, &dest);
}

impl Registry {
    pub fn load() -> Result<Self> {
        maybe_migrate_from_xdg();
        let path = registry_path();
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// Remember a project root (idempotent).
    pub fn touch_project(&mut self, root: &Path, last_run_id: Option<&str>) -> Result<()> {
        let root = canonicalize_best_effort(root);
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let now = Utc::now();
        if let Some(p) = self.projects.iter_mut().find(|p| p.root == root) {
            p.last_seen = now;
            if let Some(id) = last_run_id {
                p.last_run_id = Some(id.to_string());
            }
            if p.name.is_none() {
                p.name = name;
            }
        } else {
            self.projects.push(ProjectEntry {
                root,
                name,
                last_seen: now,
                last_run_id: last_run_id.map(|s| s.to_string()),
            });
        }
        self.projects
            .sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        self.save()
    }

    /// Drop projects whose root no longer exists.
    pub fn prune_missing(&mut self) -> Result<usize> {
        let before = self.projects.len();
        self.projects.retain(|p| p.root.is_dir());
        let n = before - self.projects.len();
        if n > 0 {
            self.save()?;
        }
        Ok(n)
    }
}

/// Register project when a run is written (best-effort; never fail the run).
pub fn note_run(project_root: &Path, run_id: &str) {
    let mut reg = Registry::load().unwrap_or_default();
    let _ = reg.touch_project(project_root, Some(run_id));
}

fn canonicalize_best_effort(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".")
        .to_string()
}

/// Load registry; optionally register the cwd project. No filesystem path scans.
pub fn ensure_known(current: Option<&Path>) -> Registry {
    let mut reg = Registry::load().unwrap_or_default();
    let _ = reg.prune_missing();
    if let Some(root) = current {
        let _ = reg.touch_project(root, None);
    }
    Registry::load().unwrap_or(reg)
}

/// All runs across registered projects, newest first.
pub fn list_all_runs() -> Result<Vec<RunSummary>> {
    let reg = ensure_known(None);
    let mut out = Vec::new();
    for proj in &reg.projects {
        if !proj.root.is_dir() {
            continue;
        }
        let paths = SparPaths::new(&proj.root);
        let Ok(runs) = state::list_runs(&paths) else {
            continue;
        };
        let name = proj
            .name
            .clone()
            .unwrap_or_else(|| project_name(&proj.root));
        for mut r in runs {
            r.project_root = Some(proj.root.clone());
            r.project_name = Some(name.clone());
            out.push(r);
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(out)
}

/// Runs for one project, annotated with project fields.
pub fn list_project_runs(project_root: &Path) -> Result<Vec<RunSummary>> {
    let paths = SparPaths::new(project_root);
    let name = project_name(project_root);
    let mut runs = state::list_runs(&paths)?;
    for r in &mut runs {
        r.project_root = Some(project_root.to_path_buf());
        r.project_name = Some(name.clone());
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn touch_and_list_roundtrip() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("spar-home");
        // Serialize env mutation across tests in this process.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SPAR_HOME", &home);
        assert_eq!(spar_home(), home);

        let proj = tmp.path().join("myproj");
        std::fs::create_dir_all(proj.join(".spar/runs")).unwrap();
        let paths = SparPaths::new(&proj);
        paths.ensure_run_dirs("abcd1234").unwrap();
        let state = state::RunState::new("abcd1234", crate::cli::WorkflowKind::Plan, proj.clone());
        state.save(&paths).unwrap();

        let mut reg = Registry::default();
        reg.touch_project(&proj, Some("abcd1234")).unwrap();
        assert_eq!(reg.projects.len(), 1);
        assert_eq!(registry_path(), home.join("registry.json"));

        let local = list_project_runs(&proj).unwrap();
        assert!(
            local.iter().any(|r| r.id == "abcd1234"),
            "expected run on disk under project"
        );

        // Reload via SPAR_HOME path — same process as list_all_runs uses.
        let reg2 = Registry::load().unwrap();
        assert!(reg2.projects.iter().any(|p| p.root == canonicalize_best_effort(&proj)));

        std::env::remove_var("SPAR_HOME");
    }

    #[test]
    fn default_home_is_dot_spar_under_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SPAR_HOME");
        let h = spar_home();
        assert!(
            h.ends_with(".spar"),
            "expected ~/.spar, got {}",
            h.display()
        );
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
