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

/// The commit every slot worktree in a run is cut from, plus the ref that named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBase {
    pub reference: String,
    pub commit: String,
}

/// Resolve a run's base.
///
/// `requested` (`--base`) wins and is resolved against `project_root`, so `origin/main`,
/// a tag or a sha all work. Otherwise the base is `cwd`'s HEAD — *not* `project_root`'s.
/// `project_root` is the repo's main checkout (a linked worktree resolves to it so
/// `.spar/` stays one per repo), and taking HEAD from there hands every slot the main
/// checkout's branch when spar is driven from a worktree.
///
/// `Ok(None)` when git can't answer (not a repo, no commits) — callers keep the old
/// `HEAD`-of-project-root behaviour. An unresolvable `--base` is an error, never a
/// silent fallback.
pub fn resolve_base(
    project_root: &Path,
    cwd: &Path,
    requested: Option<&str>,
) -> Result<Option<RunBase>> {
    let in_repo = same_repo(project_root, cwd);
    if let Some(reference) = requested {
        // Resolve where the operator is standing: named refs are shared repo-wide, but
        // `HEAD` (and anything relative to it) is per-worktree, so `--base HEAD` from a
        // linked worktree must mean *that* worktree's HEAD.
        let from = if in_repo { cwd } else { project_root };
        let commit = git_out(
            from,
            &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--base {reference} does not resolve to a commit in {}",
                from.display()
            )
        })?;
        // Record what `HEAD` *is*: a run whose base_ref is the literal "HEAD" reads as
        // detached later, and `ship` then declines to target the branch it came from.
        let reference = match reference {
            "HEAD" => named_head(from).unwrap_or_else(|| reference.to_string()),
            other => other.to_string(),
        };
        return Ok(Some(RunBase { reference, commit }));
    }
    if !in_repo {
        return Ok(None);
    }
    let Some(commit) = git_out(cwd, &["rev-parse", "--verify", "HEAD"]) else {
        return Ok(None);
    };
    let reference = named_head(cwd).unwrap_or_else(|| commit.clone()); // else detached
    Ok(Some(RunBase { reference, commit }))
}

/// The branch name checked out in `dir`, or `None` when detached.
fn named_head(dir: &Path) -> Option<String> {
    let branch = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Resolve the base and record it on the run. Prints it unless `json`, because a base
/// silently taken from the wrong branch produces a healthy-looking run against the wrong
/// code — the operator has to be able to see it at launch.
pub fn apply_run_base(state: &mut RunState, requested: Option<&str>, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Some(base) = resolve_base(&state.project_root, &cwd, requested)? else {
        // Falling back to project_root's HEAD is the old behaviour and may well be the
        // wrong branch, so it is never silent.
        eprintln!(
            "note: could not resolve a base from {} — worktrees will be cut from HEAD of {}",
            cwd.display(),
            state.project_root.display()
        );
        return Ok(());
    };
    if !json {
        let short: String = base.commit.chars().take(8).collect();
        eprintln!("base: {} ({short})", base.reference);
    }
    // Always warned, `--json` and `--base` included: the base is a commit, and a driver
    // that never sees this has no other signal that the work in its tree isn't in the run.
    if dirty(&cwd) {
        eprintln!(
            "note: uncommitted changes in {} are not in the base commit",
            cwd.display()
        );
    }
    state.base_ref = Some(base.reference);
    state.base_commit = Some(base.commit);
    Ok(())
}

fn same_repo(a: &Path, b: &Path) -> bool {
    // `--git-common-dir` may come back relative to the queried dir (and `--path-format`
    // only exists in git >= 2.31), so resolve it against that dir before comparing.
    let common = |d: &Path| {
        git_out(d, &["rev-parse", "--git-common-dir"]).map(|p| {
            let p = d.join(p);
            std::fs::canonicalize(&p).unwrap_or(p)
        })
    };
    match (common(a), common(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Excluding `.spar/`: spar writes its own run store into the project, and a project
/// that hasn't gitignored it would see this warning on every single run — which is how
/// the one stderr line that matters gets trained into background noise.
fn dirty(dir: &Path) -> bool {
    git_out(dir, &["status", "--porcelain", "--", ":!.spar"]).is_some_and(|s| !s.is_empty())
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn create_worktree(
    project_root: &Path,
    run_id: &str,
    slot_id: &str,
    base: Option<&str>,
) -> Result<WorktreeRecord> {
    let path = worktree_path(project_root, run_id, slot_id)?;
    let branch = branch_name(run_id, slot_id);

    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }

    // Branch from the run's base without checking it out in primary.
    let base = base.unwrap_or("HEAD");
    let _ = git_quiet(project_root, &["branch", &branch, base])?;

    let path_s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worktree path is not valid UTF-8: {}", path.display()))?;
    let ok = git_quiet(project_root, &["worktree", "add", path_s, &branch])?;
    if !ok {
        let _ = git_quiet(
            project_root,
            &["worktree", "add", "-b", &branch, path_s, base],
        )?;
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

/// Hardlink-copy configured build-output dirs from the project checkout into a fresh
/// slot worktree, so the slot's first build reuses the dependency artifacts already on
/// disk instead of compiling them again.
///
/// `cp -al` links inodes: near-instant, no extra disk. Cargo and pnpm replace outputs
/// rather than rewriting them in place, so a stale link just becomes a new inode on the
/// next build. `incremental/` is excluded — rustc *does* rewrite there, and a shared
/// inode across concurrent worktrees is how that corrupts.
///
/// Best-effort: a failed seed costs a cold build, never the run.
pub fn seed_build_cache(project_root: &Path, worktree: &Path, dirs: &[String]) -> Result<()> {
    for name in dirs {
        let src = project_root.join(name);
        let dst = worktree.join(name);
        if !src.is_dir() || dst.exists() {
            continue;
        }
        let ok = Command::new("cp")
            .arg("-al")
            .arg(&src)
            .arg(&dst)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_dir_all(&dst);
            eprintln!(
                "note: could not seed {} from {} — slot will cold-build",
                dst.display(),
                src.display()
            );
            continue;
        }
        for incr in [
            "debug/incremental",
            "release/incremental",
            "fast/incremental",
        ] {
            let _ = std::fs::remove_dir_all(dst.join(incr));
        }
    }
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
            let seed_dirs = if state.dry_run {
                Vec::new()
            } else {
                crate::config::Config::for_run(paths, &state.id)
                    .map(|c| c.worktree.seed_dirs)
                    .unwrap_or_default()
            };
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
                    create_worktree(
                        &state.project_root,
                        &state.id,
                        sid,
                        state.base_commit.as_deref(),
                    )?
                };
                if !seed_dirs.is_empty() {
                    seed_build_cache(&state.project_root, &rec.path, &seed_dirs)?;
                }
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
    for entry in overlay_sources(src)? {
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

/// What the author worktree hands the implementer: tracked files plus untracked ones
/// git would keep, and nothing git ignores.
///
/// The unfiltered walk this replaced copied `target/` and `node_modules/` between
/// worktrees — gigabytes of build output, byte-by-byte, per implementer, while the run
/// still reported phase `prepare_isolation`. On a spinning disk that was the single
/// largest cost in a run.
///
/// Falls back to the full walk when git lists nothing: a dry-run cwd lives under an
/// ignored `.spar/`, where `ls-files` is correctly empty but the stub artifacts still
/// have to cross.
fn overlay_sources(src: &Path) -> Result<Vec<PathBuf>> {
    match git_listed_files(src) {
        Some(files) if !files.is_empty() => Ok(files),
        _ => walkdir_regular_files(src),
    }
}

/// Regular files under `dir` that git tracks or would add. `None` when git can't answer.
fn git_listed_files(dir: &Path) -> Option<Vec<PathBuf>> {
    let out = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let files = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| dir.join(String::from_utf8_lossy(s).as_ref()))
        // `ls-files` also reports symlinks, submodules and deleted-but-tracked paths.
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file()))
        .collect();
    Some(files)
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

    /// The overlay used to walk the author worktree unfiltered, so `target/` crossed into
    /// every implementer — gigabytes of build output copied byte-by-byte on a spinning
    /// disk, while the run still reported phase `prepare_isolation`.
    #[test]
    fn overlay_leaves_ignored_build_output_behind() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("author");
        let dst = tmp.path().join("impl");
        std::fs::create_dir_all(src.join("tests")).unwrap();
        std::fs::create_dir_all(src.join("target/debug")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(src.join("tests/acceptance.rs"), "fn t() {}\n").unwrap();
        std::fs::write(src.join("target/debug/huge.rlib"), vec![0u8; 4096]).unwrap();

        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&src)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }

        copy_tree_overlay(&src, &dst).unwrap();
        assert!(
            dst.join("tests/acceptance.rs").is_file(),
            "acceptance tests must still cross"
        );
        assert!(
            !dst.join("target/debug/huge.rlib").exists(),
            "gitignored build output must not cross"
        );
    }

    /// Outside a repo (and in the dry-run cwd under an ignored `.spar/`) `ls-files` is
    /// correctly empty, and the walk has to carry the files anyway.
    #[test]
    fn overlay_falls_back_to_the_walk_outside_a_repo() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("author");
        let dst = tmp.path().join("impl");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("stub.txt"), "x").unwrap();
        copy_tree_overlay(&src, &dst).unwrap();
        assert!(dst.join("stub.txt").is_file());
    }

    #[test]
    fn seed_build_cache_hardlinks_and_drops_incremental() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let wt = tmp.path().join("repo-spar-r1-impl");
        std::fs::create_dir_all(root.join("target/debug/incremental/foo")).unwrap();
        std::fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(root.join("target/debug/deps/libx.rlib"), "obj").unwrap();
        std::fs::write(root.join("target/debug/incremental/foo/dep"), "incr").unwrap();

        seed_build_cache(&root, &wt, &["target".to_string()]).unwrap();

        let seeded = wt.join("target/debug/deps/libx.rlib");
        assert!(seeded.is_file(), "dependency artifacts must be seeded");
        assert!(
            !wt.join("target/debug/incremental").exists(),
            "incremental must not be shared across worktrees"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = std::fs::metadata(root.join("target/debug/deps/libx.rlib")).unwrap();
            let b = std::fs::metadata(&seeded).unwrap();
            assert_eq!(a.ino(), b.ino(), "seed must hardlink, not copy");
        }

        // A dir already present in the worktree is never clobbered.
        std::fs::write(seeded, "mine").unwrap();
        seed_build_cache(&root, &wt, &["target".to_string()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("target/debug/deps/libx.rlib")).unwrap(),
            "mine"
        );
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

        let rec = create_worktree(&root, "runtest1", "slot-a", None).unwrap();
        assert!(rec.path.is_dir());
        remove_worktree(&root, &rec).unwrap();
        assert!(!rec.path.exists());
    }

    /// Build `<tmp>/repo` on its default branch plus a linked worktree `<tmp>/wt` on
    /// `feat` with an extra commit. Returns (main root, worktree, feat commit).
    ///
    /// Loud on purpose: a helper that swallowed git failures would leave every test in
    /// this module passing while asserting nothing.
    fn repo_with_linked_worktree(tmp: &Path) -> (PathBuf, PathBuf, String) {
        let root = tmp.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                // Never let the developer's global config (gpg signing, hooks, commit
                // templates) reach into the fixture and fail a commit.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?} in {}: {}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "t@t.com"]);
        git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("README"), "x").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);

        let wt = tmp.join("wt");
        git(
            &root,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        std::fs::write(wt.join("feature.txt"), "work\n").unwrap();
        git(&wt, &["add", "."]);
        git(&wt, &["commit", "-q", "-m", "feature work"]);
        let head = git(&wt, &["rev-parse", "HEAD"]);
        assert_ne!(head, git(&root, &["rev-parse", "HEAD"]));
        (root, wt, head)
    }

    #[test]
    fn base_defaults_to_invoking_worktree_not_project_root() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());
        let base = resolve_base(&root, &wt, None).unwrap().unwrap();
        assert_eq!(base.reference, "feat");
        assert_eq!(base.commit, feat_head);

        // Same call from the main checkout still gets the main checkout's HEAD.
        let from_root = resolve_base(&root, &root, None).unwrap().unwrap();
        assert_ne!(from_root.commit, feat_head);
    }

    #[test]
    fn explicit_base_wins_and_bad_base_errors() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());
        let base = resolve_base(&root, &root, Some("feat")).unwrap().unwrap();
        assert_eq!(base.reference, "feat");
        assert_eq!(base.commit, feat_head);
        assert!(resolve_base(&root, &wt, Some("no/such/ref")).is_err());
    }

    /// `--base HEAD` must record the branch, not the literal ref: a base_ref of "HEAD"
    /// reads as detached downstream and costs `ship` its PR target. Also pins that
    /// spar's own untracked `.spar/` never counts as a dirty tree.
    #[test]
    fn explicit_head_records_the_branch_and_spar_state_is_not_dirt() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());
        let base = resolve_base(&root, &wt, Some("HEAD")).unwrap().unwrap();
        assert_eq!(base.reference, "feat");
        assert_eq!(base.commit, feat_head);

        std::fs::create_dir_all(wt.join(".spar/runs")).unwrap();
        std::fs::write(wt.join(".spar/runs/state.json"), "{}").unwrap();
        assert!(
            !dirty(&wt),
            "spar's own run store must not read as uncommitted work"
        );
        std::fs::write(wt.join("feature.txt"), "edited\n").unwrap();
        assert!(dirty(&wt), "a real edit still counts");
    }

    #[test]
    fn base_is_none_outside_the_project_repo() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let other = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(resolve_base(&root, &other, None).unwrap(), None);
    }

    /// The whole fix in its production shape: a live run whose recorded base is the
    /// invoking worktree's HEAD must hand that commit to every slot. Guards the one
    /// line (`prepare_isolation` → `create_worktree`) that no scenario test can reach,
    /// because every scenario runs `--dry-run` and dry-run never touches git.
    #[test]
    fn prepare_isolation_cuts_live_slots_from_the_recorded_base() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());

        let mut state = RunState::new("runlive1", crate::cli::WorkflowKind::Loop, root.clone());
        state.isolation = IsolationMode::Worktree;
        state.dry_run = false;
        let base = resolve_base(&root, &wt, None).unwrap().unwrap();
        state.base_ref = Some(base.reference);
        state.base_commit = Some(base.commit);

        let paths = SparPaths::new(&root);
        paths.ensure_run_dirs(&state.id).unwrap();
        prepare_isolation(&mut state, &paths, &["impl-a".to_string()]).unwrap();

        let rec = state.worktrees.first().expect("slot worktree recorded");
        assert_eq!(
            git_out(&rec.path, &["rev-parse", "HEAD"]).unwrap(),
            feat_head,
            "live slot must be cut from the run's base, not the main checkout"
        );
        assert!(rec.path.join("feature.txt").is_file());
        cleanup_run(&state).unwrap();
    }

    #[test]
    fn worktree_is_cut_from_the_base_commit() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());
        let base = resolve_base(&root, &wt, None).unwrap().unwrap();
        let rec = create_worktree(&root, "runbase1", "slot-a", Some(&base.commit)).unwrap();
        let head = git_out(&rec.path, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head, feat_head, "slot must branch from the invoking HEAD");
        assert!(rec.path.join("feature.txt").is_file());
        remove_worktree(&root, &rec).unwrap();
    }
}
