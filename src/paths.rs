use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Layout under `<project>/.spar/`.
#[derive(Debug, Clone)]
pub struct SparPaths {
    pub project_root: PathBuf,
    pub root: PathBuf,
}

impl SparPaths {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let project_root = project_root.into();
        let root = project_root.join(".spar");
        Self { project_root, root }
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.runs_dir().join(run_id)
    }

    pub fn state_file(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("state.json")
    }

    pub fn artifacts_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("artifacts")
    }

    pub fn mailbox_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("mailbox")
    }

    pub fn markers_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("markers")
    }

    pub fn logs_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("logs")
    }

    pub fn quota_file(&self) -> PathBuf {
        self.root.join("quota.json")
    }

    pub fn log_file(&self, run_id: &str, slot_id: &str) -> PathBuf {
        self.logs_dir(run_id).join(format!("{slot_id}.log"))
    }

    pub fn artifact(&self, run_id: &str, name: &str) -> PathBuf {
        self.artifacts_dir(run_id).join(name)
    }

    pub fn marker(&self, run_id: &str, name: &str) -> PathBuf {
        self.markers_dir(run_id).join(name)
    }

    pub fn ensure_run_dirs(&self, run_id: &str) -> Result<()> {
        for dir in [
            self.run_dir(run_id),
            self.artifacts_dir(run_id),
            self.mailbox_dir(run_id),
            self.markers_dir(run_id),
            self.logs_dir(run_id),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn ensure_swarm_root(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        Ok(())
    }
}

/// Walk up from cwd looking for `.git` or existing `.spar`.
pub fn find_project_root() -> Result<PathBuf> {
    find_project_root_from(std::env::current_dir()?)
}

pub fn find_project_root_from(start: impl AsRef<Path>) -> Result<PathBuf> {
    let mut dir = start.as_ref().to_path_buf();
    loop {
        if dir.join(".git").exists() || dir.join(".spar").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!(
                "not inside a project (no .git or .spar above {})",
                start.as_ref().display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finds_git_root() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let root = find_project_root_from(&nested).unwrap();
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn run_layout() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        paths.ensure_run_dirs("abc").unwrap();
        assert!(paths.artifacts_dir("abc").is_dir());
        assert!(paths.markers_dir("abc").is_dir());
    }
}
