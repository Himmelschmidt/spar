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

thread_local! {
    static HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Global spar home: a thread-local test override, else `$SPAR_HOME`, else `~/.spar`.
pub fn spar_home() -> PathBuf {
    if let Some(p) = HOME_OVERRIDE.with(|h| h.borrow().clone()) {
        return p;
    }
    if let Ok(p) = std::env::var("SPAR_HOME") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    fallback_home()
}

/// Real default home. Split out so unit tests never touch the developer's `~/.spar`.
#[cfg(not(test))]
fn fallback_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".spar")
}

/// Under `cargo test`, any in-process test that saves a run reaches `note_run` ->
/// `touch_project`, which writes `spar_home()`. Without an explicit `SPAR_HOME`/override
/// that would clobber the developer's real registry, so default to a per-process temp
/// home instead. (Scenario tests spawn the real binary and set `SPAR_HOME` themselves.)
#[cfg(test)]
fn fallback_home() -> PathBuf {
    use std::sync::OnceLock;
    static TEST_HOME: OnceLock<PathBuf> = OnceLock::new();
    TEST_HOME
        .get_or_init(|| {
            std::env::temp_dir()
                .join(format!("spar-test-home-{}", std::process::id()))
                .join(".spar")
        })
        .clone()
}

pub fn registry_path() -> PathBuf {
    spar_home().join("registry.json")
}

impl Registry {
    pub fn load() -> Result<Self> {
        let path = registry_path();
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    /// Write via temp file + rename. A concurrent reader either sees the old
    /// file or the new one, never a truncated one — a torn parse would make
    /// `load()` fall back to an empty registry and drop every known project.
    pub fn save(&self) -> Result<()> {
        let path = registry_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
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
        self.projects.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
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

/// Read-only project list. Unlike `ensure_known` this never writes, so it is
/// safe to call on a refresh tick.
pub fn projects() -> Vec<ProjectEntry> {
    Registry::load().map(|r| r.projects).unwrap_or_default()
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

    fn set_home_override(home: &Path) {
        HOME_OVERRIDE.with(|h| *h.borrow_mut() = Some(home.to_path_buf()));
    }

    #[test]
    fn touch_and_list_roundtrip() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("spar-home");
        set_home_override(&home);
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

        let reg2 = Registry::load().unwrap();
        assert!(reg2
            .projects
            .iter()
            .any(|p| p.root == canonicalize_best_effort(&proj)));
    }

    #[test]
    fn default_home_is_dot_spar_under_home() {
        let h = spar_home();
        assert!(
            h.ends_with(".spar"),
            "expected ~/.spar, got {}",
            h.display()
        );
    }

    #[test]
    fn home_override_is_thread_isolated() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let tmp = tempdir().unwrap();
        let a = tmp.path().join("home-a");
        let b = tmp.path().join("home-b");
        let barrier = Arc::new(Barrier::new(2));

        let ba = Arc::clone(&barrier);
        let aa = a.clone();
        let ta = thread::spawn(move || {
            set_home_override(&aa);
            ba.wait();
            assert_eq!(spar_home(), aa);
        });
        let bb = Arc::clone(&barrier);
        let bbp = b.clone();
        let tb = thread::spawn(move || {
            set_home_override(&bbp);
            bb.wait();
            assert_eq!(spar_home(), bbp);
        });
        ta.join().unwrap();
        tb.join().unwrap();
    }
}
