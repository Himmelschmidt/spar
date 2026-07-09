use crate::config::IsolationMode;
use crate::paths::SwarmPaths;
use crate::state::{RunState, WorktreeRecord};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sibling path: `../<repo>-swarm-<run>-<slot>`
pub fn worktree_path(project_root: &Path, run_id: &str, slot_id: &str) -> Result<PathBuf> {
    let repo_name = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let parent = project_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("project root has no parent"))?;
    let slot_safe = slot_id.replace('/', "-");
    Ok(parent.join(format!("{repo_name}-swarm-{run_id}-{slot_safe}")))
}

pub fn branch_name(run_id: &str, slot_id: &str) -> String {
    let slot_safe = slot_id.replace('/', "-");
    format!("swarm/{run_id}/{slot_safe}")
}

pub fn create_worktree(project_root: &Path, run_id: &str, slot_id: &str) -> Result<WorktreeRecord> {
    let path = worktree_path(project_root, run_id, slot_id)?;
    let branch = branch_name(run_id, slot_id);

    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }

    // Create branch from HEAD without checking it out in primary.
    let _ = git_quiet(project_root, &["branch", &branch, "HEAD"])?;

    let path_s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worktree path is not valid UTF-8: {}", path.display()))?;
    let ok = git_quiet(project_root, &["worktree", "add", path_s, &branch])?;
    if !ok {
        let _ = git_quiet(project_root, &["worktree", "add", "-b", &branch, path_s])?;
        if !path.is_dir() {
            bail!("git worktree add failed for {}", path.display());
        }
    }

    Ok(WorktreeRecord {
        slot_id: slot_id.into(),
        path,
        branch,
    })
}

pub fn remove_worktree(project_root: &Path, record: &WorktreeRecord) -> Result<()> {
    let _ = git_quiet(
        project_root,
        &[
            "worktree",
            "remove",
            "--force",
            record.path.to_str().unwrap_or_default(),
        ],
    );
    if record.path.exists() {
        let _ = std::fs::remove_dir_all(&record.path);
    }
    let _ = git_quiet(project_root, &["branch", "-D", &record.branch]);
    let _ = git_quiet(project_root, &["worktree", "prune"]);
    Ok(())
}

fn git_quiet(project_root: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("git {}", args.join(" ")))?;
    Ok(status.success())
}

/// Copy optional env seed files into a worktree when present.
pub fn seed_env_files(project_root: &Path, worktree: &Path) -> Result<()> {
    for name in [".dbiso.env", ".envrc", ".env.example"] {
        let src = project_root.join(name);
        if src.is_file() {
            let dst = worktree.join(name);
            if !dst.exists() {
                std::fs::copy(&src, &dst)
                    .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
            }
        }
    }
    // optional: run dbiso up if present and mode wants db
    Ok(())
}

pub fn maybe_dbiso(project_root: &Path, worktree: &Path) -> Result<()> {
    if !project_root.join(".dbiso.env").is_file() {
        return Ok(());
    }
    if which::which("dbiso").is_err() {
        return Ok(());
    }
    let _ = Command::new("dbiso")
        .arg("up")
        .current_dir(worktree)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

pub fn prepare_isolation(
    state: &mut RunState,
    paths: &SwarmPaths,
    slot_ids: &[String],
) -> Result<()> {
    state.set_phase(crate::state::Phase::PrepareIsolation);
    match state.isolation {
        IsolationMode::None => {
            let root = state.project_root.clone();
            for sid in slot_ids {
                if let Some(slot) = state.slot_mut(sid) {
                    slot.cwd = Some(root.clone());
                }
            }
        }
        IsolationMode::Worktree | IsolationMode::WorktreeDb | IsolationMode::WorktreeBwrap => {
            for sid in slot_ids {
                let rec = if state.dry_run {
                    // still create real worktrees when git available; fall back to temp dir
                    match create_worktree(&state.project_root, &state.id, sid) {
                        Ok(r) => r,
                        Err(_) => {
                            let path = paths.run_dir(&state.id).join(format!("cwd-{sid}"));
                            std::fs::create_dir_all(&path)?;
                            WorktreeRecord {
                                slot_id: sid.clone(),
                                path,
                                branch: branch_name(&state.id, sid),
                            }
                        }
                    }
                } else {
                    create_worktree(&state.project_root, &state.id, sid)?
                };
                if matches!(
                    state.isolation,
                    IsolationMode::WorktreeDb | IsolationMode::WorktreeBwrap
                ) {
                    seed_env_files(&state.project_root, &rec.path)?;
                    if matches!(state.isolation, IsolationMode::WorktreeDb) {
                        maybe_dbiso(&state.project_root, &rec.path)?;
                    }
                }
                if let Some(slot) = state.slot_mut(sid) {
                    slot.cwd = Some(rec.path.clone());
                }
                state.worktrees.push(rec);
            }
        }
    }
    state.save(paths)?;
    Ok(())
}

pub fn cleanup_run(state: &RunState) -> Result<()> {
    for rec in &state.worktrees {
        let _ = remove_worktree(&state.project_root, rec);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn path_shape() {
        let p =
            worktree_path(Path::new("/home/u/projects/foo"), "abcd1234", "impl-claude").unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/u/projects/foo-swarm-abcd1234-impl-claude")
        );
        assert_eq!(
            branch_name("abcd1234", "impl-claude"),
            "swarm/abcd1234/impl-claude"
        );
    }

    #[test]
    fn create_and_remove_when_git() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let git_ok = Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !git_ok {
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&root)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(&root)
            .status();
        std::fs::write(root.join("README"), "x").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status();
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .status();

        let rec = create_worktree(&root, "runtest1", "slot-a").unwrap();
        assert!(rec.path.is_dir());
        remove_worktree(&root, &rec).unwrap();
        assert!(!rec.path.exists());
    }
}
