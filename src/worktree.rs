use crate::config::IsolationMode;
use crate::paths::SparPaths;
use crate::state::{RunState, WorktreeRecord};
use crate::util::sanitize_slot;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sibling path: `../<repo>-spar-<run>-<slot>`
pub fn worktree_path(project_root: &Path, run_id: &str, slot_id: &str) -> Result<PathBuf> {
    let repo_name = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let parent = project_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("project root has no parent"))?;
    let slot_safe = sanitize_slot(slot_id);
    Ok(parent.join(format!("{repo_name}-spar-{run_id}-{slot_safe}")))
}

pub fn branch_name(run_id: &str, slot_id: &str) -> String {
    let slot_safe = sanitize_slot(slot_id);
    format!("spar/{run_id}/{slot_safe}")
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
    paths: &SparPaths,
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
                // Idempotent: reuse existing worktree for same slot (one-run re-entry).
                let existing_path = state
                    .worktrees
                    .iter()
                    .find(|w| w.slot_id == *sid)
                    .map(|w| w.path.clone());
                if let Some(path) = existing_path {
                    if path.is_dir() {
                        if let Some(slot) = state.slot_mut(sid) {
                            slot.cwd = Some(path);
                        }
                        continue;
                    }
                }
                let expected = worktree_path(&state.project_root, &state.id, sid)?;
                if expected.is_dir() {
                    let rec = WorktreeRecord {
                        slot_id: sid.clone(),
                        path: expected.clone(),
                        branch: branch_name(&state.id, sid),
                    };
                    if let Some(slot) = state.slot_mut(sid) {
                        slot.cwd = Some(rec.path.clone());
                    }
                    if state.worktrees.iter().all(|w| w.slot_id != *sid) {
                        state.worktrees.push(rec);
                    }
                    continue;
                }
                // dry-run: never create real git worktrees / sibling dirs — only
                // ephemeral cwd under .spar/runs/<id>/ so agents are stubbed without
                // mutating the repo's worktree list.
                let rec = if state.dry_run {
                    let safe = sanitize_slot(sid);
                    let path = paths.run_dir(&state.id).join(format!("cwd-{safe}"));
                    std::fs::create_dir_all(&path)?;
                    WorktreeRecord {
                        slot_id: sid.clone(),
                        path,
                        branch: branch_name(&state.id, sid),
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
                if state.worktrees.iter().all(|w| w.slot_id != *sid) {
                    state.worktrees.push(rec);
                }
            }
        }
    }
    state.save(paths)?;
    Ok(())
}

/// What cleanup did to one worktree.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeCleanup {
    pub slot_id: String,
    pub path: PathBuf,
    /// Processes reaped because their cwd was inside the worktree.
    pub killed: Vec<u32>,
    pub removed: bool,
    /// Set when the guard refused the path (never touched).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Cleanup only ever touches a run's own worktrees. Refuse the project root itself and
/// any ancestor of it — a bad record must never take out the checkout or `$HOME`.
pub fn reapable_worktree(project_root: &Path, path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    let wt = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    wt != root && !root.starts_with(&wt)
}

/// Reap first, then remove. Agents leave dev servers and watchers running with their cwd
/// inside the worktree; those keep writing into it, which is how a `remove_dir_all` loses
/// the race and leaves a half-deleted directory (and orphaned processes) behind for days.
pub fn cleanup_run(state: &RunState) -> Result<Vec<WorktreeCleanup>> {
    let mut report = Vec::new();
    for rec in &state.worktrees {
        if !reapable_worktree(&state.project_root, &rec.path) {
            report.push(WorktreeCleanup {
                slot_id: rec.slot_id.clone(),
                path: rec.path.clone(),
                killed: Vec::new(),
                removed: false,
                skipped: Some("refusing path: not inside the run's own worktrees".into()),
            });
            continue;
        }

        let killed = crate::process::pids_with_cwd_under(&rec.path);
        if !killed.is_empty() {
            crate::process::terminate_all(&killed);
        }

        if state.dry_run || rec.path.starts_with(state.project_root.join(".spar")) {
            // dry-run cwd dirs live under .spar and git never knew about them — just rm
            let _ = std::fs::remove_dir_all(&rec.path);
        } else {
            let _ = remove_worktree(&state.project_root, rec);
        }

        report.push(WorktreeCleanup {
            slot_id: rec.slot_id.clone(),
            path: rec.path.clone(),
            removed: !rec.path.exists(),
            killed,
            skipped: None,
        });
    }
    Ok(report)
}

/// Bring pre-coding acceptance tests from the test-author worktree into the implementer cwd.
///
/// Fail closed when the author worktree is missing. Always overlays the author working tree
/// (agents often leave tests uncommitted). Live runs also try `git merge` of the author branch
/// first for committed history; failed merges are aborted before overlay.
pub fn apply_spec_tests_to_impl(
    state: &RunState,
    author_slot: &str,
    impl_cwd: &Path,
) -> Result<()> {
    let spec = state
        .worktrees
        .iter()
        .find(|w| w.slot_id == author_slot)
        .ok_or_else(|| anyhow::anyhow!("test-author worktree missing for slot {author_slot}"))?;
    if !spec.path.is_dir() {
        anyhow::bail!("test-author worktree path missing: {}", spec.path.display());
    }
    if !impl_cwd.is_dir() {
        anyhow::bail!("implementer cwd missing: {}", impl_cwd.display());
    }

    let dry_or_spar = state.dry_run || impl_cwd.starts_with(state.project_root.join(".spar"));
    if !dry_or_spar {
        try_merge_spec_branch(impl_cwd, &spec.branch)?;
    }
    // Always overlay: uncommitted author files never appear in a merge.
    copy_tree_overlay(&spec.path, impl_cwd)?;
    Ok(())
}

/// Attempt merge; on failure abort so the tree is never left in MERGING.
fn try_merge_spec_branch(impl_cwd: &Path, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .args([
            "merge",
            "--no-edit",
            "-m",
            "spar: acceptance tests from test-author",
            branch,
        ])
        .current_dir(impl_cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("git merge {branch} into {}", impl_cwd.display()))?;
    if status.success() {
        return Ok(());
    }
    let _ = Command::new("git")
        .args(["merge", "--abort"])
        .current_dir(impl_cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(())
}

fn copy_tree_overlay(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in walkdir_regular_files(src)? {
        let rel = entry.strip_prefix(src).unwrap_or(&entry);
        if rel.as_os_str().is_empty() {
            continue;
        }
        if rel.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::copy(&entry, &target)
            .with_context(|| format!("copy {} -> {}", entry.display(), target.display()))?;
    }
    Ok(())
}

/// Regular files only — never follow or copy symlinks (agent could link secrets).
fn walkdir_regular_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn rec(d: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        let rd = std::fs::read_dir(d).with_context(|| format!("read_dir {}", d.display()))?;
        for e in rd.flatten() {
            let p = e.path();
            let meta =
                std::fs::symlink_metadata(&p).with_context(|| format!("stat {}", p.display()))?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                rec(&p, out)?;
            } else if meta.is_file() {
                out.push(p);
            }
        }
        Ok(())
    }
    rec(root, &mut out)?;
    Ok(out)
}

/// After purging a run dir, drop empty parent dirs (runs/, .spar/ if empty).
pub fn prune_empty_spar_parents(paths: &SparPaths) -> Result<()> {
    let runs = paths.runs_dir();
    if runs.is_dir() && std::fs::read_dir(&runs)?.next().is_none() {
        let _ = std::fs::remove_dir(&runs);
    }
    if paths.root.is_dir() && std::fs::read_dir(&paths.root)?.next().is_none() {
        let _ = std::fs::remove_dir(&paths.root);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn overlay_copies_files_skips_symlinks() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("tests")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("tests/a.rs"), "fn t() {}\n").unwrap();
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink("/etc/passwd", src.join("evil"));
        }
        copy_tree_overlay(&src, &dst).unwrap();
        assert!(dst.join("tests/a.rs").is_file());
        #[cfg(unix)]
        {
            assert!(!dst.join("evil").exists());
        }
    }

    #[test]
    fn apply_spec_missing_worktree_errors() {
        let tmp = tempdir().unwrap();
        let mut state = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Plan,
            tmp.path().to_path_buf(),
        );
        state.dry_run = true;
        let err = apply_spec_tests_to_impl(&state, "test-author-x", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("missing"), "err={err}");
    }

    #[test]
    fn apply_spec_overlays_author_files() {
        let tmp = tempdir().unwrap();
        let project = tmp.path().join("proj");
        let author = tmp.path().join("author");
        let impl_cwd = tmp.path().join("impl");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&author).unwrap();
        std::fs::create_dir_all(&impl_cwd).unwrap();
        std::fs::write(author.join(".spar-dry-acceptance-tests"), "tests\n").unwrap();
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Plan, project);
        state.dry_run = true;
        state.worktrees.push(WorktreeRecord {
            slot_id: "test-author-x".into(),
            path: author,
            branch: "spar/r1/test-author-x".into(),
        });
        apply_spec_tests_to_impl(&state, "test-author-x", &impl_cwd).unwrap();
        assert!(impl_cwd.join(".spar-dry-acceptance-tests").is_file());
    }

    #[test]
    fn reap_guard_refuses_project_root_and_ancestors() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        assert!(!reapable_worktree(&root, &root), "never the project root");
        assert!(
            !reapable_worktree(&root, tmp.path()),
            "never a parent of the project root"
        );
        assert!(
            !reapable_worktree(&root, Path::new("/")),
            "never the filesystem root"
        );
        assert!(!reapable_worktree(&root, Path::new("")));

        let sibling = tmp.path().join("repo-spar-r1-impl");
        assert!(reapable_worktree(&root, &sibling), "sibling worktree is ok");
        assert!(
            reapable_worktree(&root, &root.join(".spar/runs/r1/cwd-impl")),
            "dry-run cwd under .spar is ok"
        );
    }

    #[test]
    fn cleanup_run_skips_guarded_path_and_removes_own_worktree() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let wt = tmp.path().join("repo-spar-r1-impl");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("file"), "x").unwrap();

        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, root.clone());
        state.dry_run = true;
        state.worktrees.push(WorktreeRecord {
            slot_id: "impl".into(),
            path: wt.clone(),
            branch: "spar/r1/impl".into(),
        });
        state.worktrees.push(WorktreeRecord {
            slot_id: "bogus".into(),
            path: root.clone(),
            branch: "spar/r1/bogus".into(),
        });

        let report = cleanup_run(&state).unwrap();
        assert!(report[0].removed);
        assert!(!wt.exists());
        assert!(report[1].skipped.is_some(), "project root must be refused");
        assert!(root.is_dir(), "project root must survive cleanup");
    }

    #[test]
    fn path_shape() {
        let p =
            worktree_path(Path::new("/home/u/projects/foo"), "abcd1234", "impl-claude").unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/u/projects/foo-spar-abcd1234-impl-claude")
        );
        assert_eq!(
            branch_name("abcd1234", "impl-claude"),
            "spar/abcd1234/impl-claude"
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
