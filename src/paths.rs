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

    /// Workspace-level swarm bus root (W5). Keyed by `agent_id`, independent of any
    /// run: `.spar/bus/`. A run is now just an optional `run` tag on each message, so
    /// bare agents and run slots share one addressable bus.
    pub fn bus_dir(&self) -> PathBuf {
        self.root.join("bus")
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
    // Agents spawned by spar carry `SPAR_PROJECT_ROOT` so bus commands they run from a
    // worktree (whose own `.spar` is empty) resolve the primary checkout that owns the run.
    if let Ok(root) = std::env::var("SPAR_PROJECT_ROOT") {
        let root = PathBuf::from(root);
        if root.is_dir() {
            return Ok(root);
        }
    }
    find_project_root_from(std::env::current_dir()?)
}

pub fn find_project_root_from(start: impl AsRef<Path>) -> Result<PathBuf> {
    let mut dir = start.as_ref().to_path_buf();
    loop {
        let git = dir.join(".git");
        if git.exists() || dir.join(".spar").exists() {
            // A linked worktree has a `.git` *file* (a `gitdir:` pointer). Map it to the
            // repo's main worktree so a worktree registers under its parent project
            // rather than as its own separate entry in the project list.
            if git.is_file() {
                if let Some(main) = main_worktree_root(&dir) {
                    return Ok(main);
                }
            }
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

/// The repo's main (primary) worktree via `git worktree list` — its first entry is
/// always the main worktree. `None` if git is unavailable or `dir` isn't in a repo.
fn main_worktree_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let path = stdout.lines().next()?.strip_prefix("worktree ")?;
    let main = PathBuf::from(path.trim());
    main.is_dir().then_some(main)
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
    fn worktree_resolves_to_main_checkout() {
        let tmp = tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        let wt = tmp.path().join("wt");
        assert!(
            git(&["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()])
                .status
                .success()
        );

        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        // From the worktree root and a nested subdir, resolve to the main checkout.
        assert_eq!(
            canon(&find_project_root_from(&wt).unwrap()),
            canon(&main),
            "worktree root must resolve to the main checkout"
        );
        let nested = wt.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            canon(&find_project_root_from(&nested).unwrap()),
            canon(&main),
            "subdir of a worktree must resolve to the main checkout"
        );
        // The main checkout itself is unchanged (its `.git` is a dir, not a file).
        assert_eq!(canon(&find_project_root_from(&main).unwrap()), canon(&main));
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
