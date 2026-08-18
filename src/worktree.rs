use crate::config::IsolationMode;
use crate::paths::SparPaths;
use crate::state::{RunState, WorktreeRecord};
use crate::util::sanitize_slot;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Expand a leading `~`, so `worktree.root` can be written the way a human types it.
fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

/// The configured `worktree.root`, expanded.
///
/// Takes the config rather than loading it so `worktree_path` stays a pure function of
/// its arguments: callers already hold the run's config, and a path helper that reads
/// ambient user config would make its own tests depend on the machine they run on.
pub fn worktree_root(cfg: &crate::config::Config) -> Option<PathBuf> {
    cfg.worktree.root.as_deref().map(expand_tilde)
}

/// Where a slot's worktree lives.
///
/// Default is the historical sibling path, `../<repo>-spar-<run>-<slot>`. With
/// `worktree.root` set, runs are collected under it as `<root>/<repo>/<run>-<slot>`
/// instead, which keeps the repo's parent directory clean and puts every worktree
/// spar owns in one sweepable place.
pub fn worktree_path(
    project_root: &Path,
    run_id: &str,
    slot_id: &str,
    root: Option<&Path>,
) -> Result<PathBuf> {
    let repo_name = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let slot_safe = sanitize_slot(slot_id);

    if let Some(root) = root {
        return Ok(root.join(repo_name).join(format!("{run_id}-{slot_safe}")));
    }

    let parent = project_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("project root has no parent"))?;
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
    root: Option<&Path>,
) -> Result<WorktreeRecord> {
    let path = worktree_path(project_root, run_id, slot_id, root)?;
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
            let cfg = if state.dry_run {
                None
            } else {
                crate::config::Config::for_run(paths, &state.id).ok()
            };
            // Resolved once here, from the run's own config snapshot, so recovery below
            // looks for worktrees where this run actually put them.
            let wt_root = cfg.as_ref().and_then(worktree_root);
            // Reclaim before cutting, and only when actually cutting: the moment new
            // worktrees appear is the moment landed ones stop being worth their disk.
            let cutting_new = slot_ids.iter().any(|sid| {
                !state
                    .worktrees
                    .iter()
                    .any(|w| w.slot_id == *sid && w.path.is_dir())
            });
            if cutting_new && cfg.as_ref().is_some_and(|c| c.worktree.auto_cleanup_merged) {
                match sweep_merged(paths, Some(&state.id)) {
                    Ok(reaped) if !reaped.is_empty() => {
                        let total: usize = reaped.iter().map(|(_, n)| n).sum();
                        eprintln!(
                            "reclaimed {total} merged worktree(s) from {} run(s): {}",
                            reaped.len(),
                            reaped
                                .iter()
                                .map(|(id, _)| id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    // Never fails a run: this is housekeeping, not the work.
                    Ok(_) => {}
                    Err(e) => eprintln!("note: merged-worktree sweep skipped: {e}"),
                }
            }

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
                let expected = worktree_path(&state.project_root, &state.id, sid, wt_root.as_deref())?;
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
                        wt_root.as_deref(),
                    )?
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

/// Whether every branch this run cut is already contained in its own `base_ref`.
///
/// `None` when the question cannot be answered — no recorded base (pre-O26), a base that
/// no longer resolves, or a run with no worktrees. Only `Some(true)` is evidence, and
/// only evidence reaps.
///
/// Ancestry, not patch equivalence. A **squash-merged** branch is not an ancestor of its
/// base and reads as unmerged here, so a squash-merged run still has to be swept by age
/// or by run id. That is the safe direction to be wrong in: it keeps a worktree the
/// operator might still want instead of deleting work that only looks landed.
pub fn merged_into_base(state: &RunState) -> Option<bool> {
    if state.worktrees.is_empty() {
        return None;
    }
    let root = &state.project_root;
    let base = resolve_commit(root, state.base_ref.as_deref()?)?;
    let mut checked = 0usize;
    for rec in &state.worktrees {
        // Uncommitted work first, and it is the check that matters. Branch ancestry says
        // nothing about a dirty tree: a slot that wrote code and never committed leaves
        // its branch sitting exactly on the base, which is trivially an ancestor, so
        // ancestry alone reads "fully landed" for the work most at risk. `cleanup_run`
        // then removes the worktree with `--force`. The phases this sweep can reach are
        // `stopped` / `failed` / `stuck` / `quota` / gates — precisely the runs whose
        // agents were interrupted mid-turn, and the ones `implement --run` exists to
        // resume.
        if rec.path.is_dir() && worktree_holds_uncommitted(&rec.path) {
            return Some(false);
        }
        // A branch that is already gone cannot be holding unmerged work.
        if resolve_commit(root, &rec.branch).is_none() {
            continue;
        }
        checked += 1;
        let contained =
            git_quiet(root, &["merge-base", "--is-ancestor", &rec.branch, &base]).unwrap_or(false);
        if !contained {
            return Some(false);
        }
    }
    // Vacuous truth is not evidence. With every branch unresolvable the loop above skips
    // them all and falls out here, turning "git could not answer for any of these" into
    // "all of them are merged" — and a worktree can still hold committed work its branch
    // no longer points at.
    if checked == 0 {
        return None;
    }
    Some(true)
}

/// Uncommitted work in a slot worktree. Fails **closed**: a git that cannot answer means
/// the tree is treated as holding work, because the caller's only use for this is deciding
/// whether it is safe to delete.
fn worktree_holds_uncommitted(path: &Path) -> bool {
    match git_out(path, &["status", "--porcelain"]) {
        Some(s) => !s.is_empty(),
        None => true,
    }
}

/// A ref as it resolves today: the local ref, else its `origin/` counterpart.
fn resolve_commit(root: &Path, reference: &str) -> Option<String> {
    git_out(
        root,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .or_else(|| {
        git_out(
            root,
            &[
                "rev-parse",
                "--verify",
                &format!("origin/{reference}^{{commit}}"),
            ],
        )
    })
}

/// Reap the worktrees of every at-rest run whose branches are already in their base.
///
/// The disk-pollution backstop: a run that shipped and landed has nothing left to resume,
/// but its phase (`awaiting_ship_confirm`, `stopped`) says otherwise, so age was the only
/// evidence the sweep accepted and worktrees accumulated for as long as the operator went
/// without running one. Merged is stronger evidence than age — the work is in the base
/// branch — which is what makes this safe to do without being asked. `skip_run` is the
/// run being launched right now.
pub fn sweep_merged(paths: &SparPaths, skip_run: Option<&str>) -> Result<Vec<(String, usize)>> {
    let mut reaped = Vec::new();
    for summary in crate::state::list_runs(paths)? {
        if skip_run == Some(summary.id.as_str()) {
            continue;
        }
        let Ok(state) = RunState::load(paths, &summary.id) else {
            continue;
        };
        if !crate::state::at_rest(state.phase) {
            continue;
        }
        // The phase on disk is a snapshot, and a resuming orchestrator is between loading
        // its state and saving the new one for a window this sweep can land in. Reaping
        // then would not merely delete a live run's worktrees — `cleanup_run` also
        // terminates every process whose cwd is inside them, i.e. that run's agents.
        if crate::state::orchestrator_alive(paths, &summary.id) {
            continue;
        }
        // Nothing on disk to reclaim: skip before touching git. Records outlive their
        // worktrees (cleanup does not clear them), so without this every later sweep
        // re-runs `worktree remove` / `branch -D` / `prune` per stale record and reports
        // reclaiming what it already reclaimed.
        if !state.worktrees.iter().any(|r| r.path.is_dir()) {
            continue;
        }
        if merged_into_base(&state) != Some(true) {
            continue;
        }
        let cleaned = cleanup_run(&state, false)?;
        let removed = cleaned.iter().filter(|c| c.removed).count();
        if removed > 0 {
            reaped.push((summary.id, removed));
        }
    }
    Ok(reaped)
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
/// Reap build output from every run in the project that has finished.
///
/// `is_terminal()` plus `Stopped`: a stopped run is parked, and parking it is exactly when
/// its 72 GB of build output stops earning its disk. Nothing here is unrecoverable, so
/// unlike the worktree sweep this needs no age threshold and no evidence — only that the
/// run is not currently running and nothing is working in the tree.
pub fn reap_finished_caches(paths: &SparPaths, skip_run: Option<&str>) -> Result<Vec<CacheReap>> {
    let live = LiveCwds::snapshot();
    let mut out = Vec::new();
    for summary in crate::state::list_runs(paths)? {
        if skip_run == Some(summary.id.as_str()) {
            continue;
        }
        if !(summary.phase.is_terminal() || summary.phase == crate::state::Phase::Stopped) {
            continue;
        }
        let Ok(state) = RunState::load(paths, &summary.id) else {
            continue;
        };
        // The phase on disk is a snapshot; a resuming orchestrator is briefly between load
        // and save. Same guard the worktree sweep carries (O34).
        if crate::state::orchestrator_alive(paths, &summary.id) {
            continue;
        }
        let reap = reap_build_cache(&state, &live);
        if reap.freed_bytes > 0 || !reap.skipped.is_empty() {
            out.push(reap);
        }
    }
    Ok(out)
}

/// Build-output directory names reaped from a finished run's worktrees.
const CACHE_DIRS: [&str; 2] = ["target", "node_modules"];

/// True when `dir` looks like a build cache rather than someone's source directory.
///
/// A **disjunction** on purpose. `CACHEDIR.TAG` alone is not enough: cargo writes it late,
/// so a build interrupted before that point leaves a multi-GB tree with `.rustc_info.json`
/// and `debug/` and no tag, which a tag-only test strands forever. Measured: one such tree
/// held 22 GB.
///
/// `node_modules` is taken on the name alone — it has no marker file and is regenerable by
/// definition.
fn is_build_cache(dir: &Path, name: &str) -> bool {
    if name == "node_modules" {
        return dir.is_dir();
    }
    dir.join("CACHEDIR.TAG").is_file()
        || dir.join(".rustc_info.json").is_file()
        || dir.join("debug").is_dir()
        || dir.join("release").is_dir()
}

/// What a cache reap did to one run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheReap {
    pub run_id: String,
    pub freed_bytes: u64,
    pub dirs: Vec<PathBuf>,
    /// Paths left alone, with why. Never silent: a skipped 70 GB tree is the whole point.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
}

/// Delete regenerable build output inside a finished run's own worktrees.
///
/// Deliberately **not** cleanup. Cleanup decides whether a *worktree* may be removed and
/// carries every question that comes with that — unsaved work, merged evidence, operator
/// confirmation. This decides only whether regenerable bytes *inside a surviving worktree*
/// may go, and the answer needs no evidence at all: the worktree, its branch, its commits
/// and its uncommitted changes are all untouched. That is why it has its own command and
/// must never become a route into a sweep.
///
/// Measured motivation: 457 GB of 587 GB under one projects dir was `target/` and
/// `node_modules`, and the single largest object on the machine was a **stopped** run's
/// target dir at 72.7 GB.
pub fn reap_build_cache(state: &RunState, live: &LiveCwds) -> CacheReap {
    let mut reap = CacheReap {
        run_id: state.id.clone(),
        freed_bytes: 0,
        dirs: Vec::new(),
        skipped: Vec::new(),
    };
    for rec in &state.worktrees {
        if !rec.path.is_dir() || !reapable_worktree(&state.project_root, &rec.path) {
            continue;
        }
        if live.inside(&rec.path) {
            reap.skipped.push(format!(
                "{}: a live process is working here",
                rec.path.display()
            ));
            continue;
        }
        for name in CACHE_DIRS {
            let dir = rec.path.join(name);
            if !dir.is_dir() || !is_build_cache(&dir, name) {
                continue;
            }
            // A symlinked cache points somewhere the operator chose — another disk, a
            // shared cache. `remove_dir_all` would unlink it (it does not follow, so the
            // real tree survives), which silently breaks their setup and reports bytes as
            // freed that never were.
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
                reap.skipped
                    .push(format!("{}: symlinked elsewhere", dir.display()));
                continue;
            }
            let bytes = dir_size(&dir);
            if std::fs::remove_dir_all(&dir).is_ok() {
                reap.freed_bytes += bytes;
                reap.dirs.push(dir);
            }
        }
    }
    reap
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                stack.push(e.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// Every live process cwd, read once.
///
/// `process::pids_with_cwd_under` re-reads all of `/proc` and re-canonicalises per call.
/// That is fine for the one worktree `cleanup` looks at and pathological across a project's
/// worth of candidates on 7200rpm disks, which is where this runs.
pub struct LiveCwds(Vec<PathBuf>);

impl LiveCwds {
    pub fn snapshot() -> Self {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
                    continue;
                }
                if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) {
                    out.push(cwd);
                }
            }
        }
        Self(out)
    }

    pub fn inside(&self, dir: &Path) -> bool {
        let root = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.0.iter().any(|c| c.starts_with(&root))
    }
}

/// Work a worktree holds that removing it would destroy: uncommitted changes, or commits
/// the run's base does not contain. `None` when there is nothing to lose.
///
/// Both matter because `remove_worktree` runs `git worktree remove --force` *and*
/// `git branch -D`, so an unmerged commit is as gone as an unsaved edit.
///
/// Fails **closed** throughout: an unresolvable base, an unanswerable git, or an
/// unreadable count all count as holding work, because the only
/// caller is deciding whether deletion is safe.
fn unsaved_work(state: &RunState, rec: &WorktreeRecord) -> Option<String> {
    if !rec.path.is_dir() {
        return None;
    }
    // Dry-run cwds under `.spar/` are scratch dirs, not git worktrees. git cannot answer
    // for them, and fail-closed on an unanswerable question would make the veto refuse
    // every dry-run cleanup forever.
    if state.dry_run || rec.path.starts_with(state.project_root.join(".spar")) {
        return None;
    }
    match git_out(&rec.path, &["status", "--porcelain"]) {
        None => return Some("git could not report status".into()),
        Some(s) if !s.is_empty() => {
            return Some(format!("{} uncommitted change(s)", s.lines().count()))
        }
        Some(_) => {}
    }
    // `base_commit` first: it is what the worktree was actually cut from (O26). `base_ref`
    // is a *label*, and a label can be deleted or moved out from under a run — a feature
    // branch merged and deleted last week is the common case, and pre-O26 runs have no ref
    // at all. Both were reachable on the machine this fix came from.
    let base = state.base_commit.clone().or_else(|| {
        state
            .base_ref
            .as_deref()
            .and_then(|r| resolve_commit(&state.project_root, r))
    });
    let Some(base) = base else {
        // Unknowable, so refuse. Reading "I cannot determine the base" as "there is
        // nothing to lose" is the same inversion this whole change exists to remove.
        return Some("base cannot be resolved — cannot tell what is unmerged".into());
    };
    // HEAD *and* the recorded branch. The tree's HEAD is what an agent left checked out;
    // `remove_worktree` deletes `rec.branch`. An agent that detached HEAD to compare
    // against the base leaves HEAD clean and level while the branch still carries every
    // commit it wrote.
    for reference in ["HEAD", rec.branch.as_str()] {
        if reference != "HEAD" && resolve_commit(&rec.path, reference).is_none() {
            continue; // branch already gone; `branch -D` has nothing to destroy
        }
        let Some(count) = git_out(
            &rec.path,
            &["rev-list", "--count", &format!("{base}..{reference}")],
        ) else {
            return Some(format!(
                "git could not compare {reference} against the base"
            ));
        };
        match count.parse::<u32>() {
            Ok(0) => {}
            Ok(n) => return Some(format!("{n} commit(s) on {reference} not in the base")),
            Err(_) => {
                return Some(format!("unreadable commit count for {reference}"));
            }
        }
    }
    None
}

/// Reap first, then remove — unless the worktree still holds work.
///
/// The veto lives here, at the destructive operation, rather than in any one caller. It
/// was originally written into `merged_into_base`, which guards only the `--merged`
/// evidence path, so `--older-than` walked straight past it and force-removed worktrees
/// holding uncommitted agent work. Guarding an evidence path leaves the next evidence path
/// unguarded; guarding the deletion covers all of them, including ones not written yet.
///
/// `force` is the operator naming a run and meaning it.
pub fn cleanup_run(state: &RunState, force: bool) -> Result<Vec<WorktreeCleanup>> {
    let mut report = Vec::new();
    for rec in &state.worktrees {
        if !force {
            if let Some(why) = unsaved_work(state, rec) {
                report.push(WorktreeCleanup {
                    slot_id: rec.slot_id.clone(),
                    path: rec.path.clone(),
                    killed: Vec::new(),
                    removed: false,
                    skipped: Some(format!("holds {why} — reap by run id with --force")),
                });
                continue;
            }
        }
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

/// What the overlay left behind: paths git ignores in the author worktree, and where
/// they still are. Directories are collapsed (`target/`), so this stays a short list.
#[derive(Debug, Clone)]
pub struct SpecOverlay {
    pub author_path: PathBuf,
    pub ignored: Vec<String>,
}

/// Bring pre-coding acceptance tests from the test-author worktree into the implementer cwd.
///
/// Fail closed when the author worktree is missing. Always overlays the author working tree
/// (agents often leave tests uncommitted). Live runs also try `git merge` of the author branch
/// first for committed history; failed merges are aborted before overlay.
///
/// Returns what git ignored, because the caller has to say so out loud: a fixture the
/// test-author wrote into an ignored path (`.env.test`, an ignored `tests/data/`) does not
/// cross, and the implementer then meets a test that compiles and fails at runtime with
/// nothing pointing at the plumbing.
pub fn apply_spec_tests_to_impl(
    state: &RunState,
    author_slot: &str,
    impl_cwd: &Path,
) -> Result<SpecOverlay> {
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
    Ok(SpecOverlay {
        author_path: spec.path.clone(),
        ignored: ignored_entries(&spec.path),
    })
}

/// Build output and dependency trees. Not policy — a noise filter, so the "these did not
/// come across" notice carries fixtures the implementer might actually need instead of
/// `target/` on every single run, which is how a notice gets tuned out.
const BUILD_OUTPUT_DIRS: [&str; 12] = [
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".gradle",
    ".spar",
];

/// Paths git ignores in `dir` and worth telling somebody about, with fully-ignored
/// directories collapsed to one entry.
///
/// `--directory` is what keeps this readable: without it a Rust worktree reports every
/// file under `target/` individually.
fn ignored_entries(dir: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ])
        .current_dir(dir)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    out.stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|p| !is_build_output(p))
        .collect()
}

fn is_build_output(entry: &str) -> bool {
    entry.split('/').any(|c| BUILD_OUTPUT_DIRS.contains(&c))
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
/// have to cross. A git that *errored* (broken worktree admin dir, dubious ownership,
/// git missing) falls back too, but says so — the walk is the gigabyte copy this change
/// exists to stop, and it must never resume silently.
fn overlay_sources(src: &Path) -> Result<Vec<PathBuf>> {
    match git_listed_files(src) {
        Ok(files) if !files.is_empty() => Ok(files),
        Ok(_) => walkdir_regular_files(src),
        Err(why) => {
            eprintln!(
                "note: git could not list {} ({why}) — falling back to a full tree copy, \
                 which will include build output",
                src.display()
            );
            walkdir_regular_files(src)
        }
    }
}

/// Regular files under `dir` that git tracks or would add. `Err` when git can't answer.
fn git_listed_files(dir: &Path) -> std::result::Result<Vec<PathBuf>, String> {
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
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let files = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        // Raw bytes, not `from_utf8_lossy`: a replacement char would name a path that does
        // not exist, and the file would be dropped instead of copied.
        .map(|s| dir.join(bytes_to_path(s)))
        // `ls-files` also reports symlinks, submodules and deleted-but-tracked paths.
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file()))
        .collect();
    Ok(files)
}

#[cfg(unix)]
fn bytes_to_path(b: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(not(unix))]
fn bytes_to_path(b: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(b).as_ref())
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

    /// The overlay carrying git-visible files only is the fix; doing it *silently* is the
    /// new failure. An ignored fixture must come back named, so the caller can say where
    /// it still is.
    #[test]
    fn overlay_reports_what_git_ignored() {
        let tmp = tempdir().unwrap();
        let project = tmp.path().join("proj");
        let author = tmp.path().join("author");
        let impl_cwd = tmp.path().join("impl");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(author.join("tests/data")).unwrap();
        std::fs::create_dir_all(author.join("target/debug")).unwrap();
        std::fs::create_dir_all(&impl_cwd).unwrap();
        std::fs::write(
            author.join(".gitignore"),
            "target/\n.env.test\ntests/data/\n",
        )
        .unwrap();
        std::fs::write(author.join("tests/acceptance.rs"), "fn t() {}\n").unwrap();
        std::fs::write(author.join(".env.test"), "DB=x\n").unwrap();
        std::fs::write(author.join("tests/data/golden.json"), "{}\n").unwrap();
        std::fs::write(author.join("target/debug/huge.rlib"), vec![0u8; 512]).unwrap();

        let git_ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&author)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !git_ok {
            return;
        }

        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Plan, project);
        state.dry_run = true;
        state.worktrees.push(WorktreeRecord {
            slot_id: "test-author-x".into(),
            path: author.clone(),
            branch: "spar/r1/test-author-x".into(),
        });

        let overlay = apply_spec_tests_to_impl(&state, "test-author-x", &impl_cwd).unwrap();
        assert!(impl_cwd.join("tests/acceptance.rs").is_file());
        assert!(!impl_cwd.join(".env.test").exists());
        assert!(!impl_cwd.join("target/debug/huge.rlib").exists());

        assert_eq!(overlay.author_path, author);
        assert!(
            overlay.ignored.iter().any(|p| p == ".env.test"),
            "an ignored fixture must be named, not silently dropped: {:?}",
            overlay.ignored
        );
        assert!(
            overlay.ignored.iter().any(|p| p.starts_with("tests/data")),
            "{:?}",
            overlay.ignored
        );
        // Build output is filtered out: every Rust run has a `target/`, and a notice that
        // fires every run with nothing actionable in it is a notice nobody reads.
        assert!(
            !overlay.ignored.iter().any(|p| p.starts_with("target")),
            "build output must not be reported as a missing fixture: {:?}",
            overlay.ignored
        );
        assert!(
            overlay.ignored.len() < 5,
            "ignored dirs must collapse to one entry each: {:?}",
            overlay.ignored
        );
    }

    /// Merged evidence is what lets the sweep reclaim without being asked, so it has to be
    /// exact: contained in the base ⇒ reap, one unmerged commit ⇒ keep.
    #[test]
    fn merged_into_base_tracks_containment() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let base_ref = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let base_commit = git(&root, &["rev-parse", "HEAD"]);

        let mut state = RunState::new("runmerge", crate::cli::WorkflowKind::Loop, root.clone());
        state.base_ref = Some(base_ref.clone());
        state.base_commit = Some(base_commit);

        assert_eq!(
            merged_into_base(&state),
            None,
            "a run with no worktrees offers no evidence either way"
        );

        let rec = create_worktree(&root, "runmerge", "impl", state.base_commit.as_deref(), None).unwrap();
        state.worktrees.push(rec.clone());
        assert_eq!(
            merged_into_base(&state),
            Some(true),
            "a clean worktree on an unmoved branch holds nothing to lose"
        );

        // The data-loss case: an agent wrote code and never committed, so the branch is
        // still on the base and reads as trivially contained. Ancestry must not decide it.
        std::fs::write(rec.path.join("uncommitted.rs"), "fn work() {}\n").unwrap();
        assert_eq!(
            merged_into_base(&state),
            Some(false),
            "uncommitted work must veto the merged verdict — cleanup removes with --force"
        );
        std::fs::remove_file(rec.path.join("uncommitted.rs")).unwrap();

        std::fs::write(rec.path.join("work.txt"), "slot work\n").unwrap();
        git(&rec.path, &["add", "."]);
        git(&rec.path, &["commit", "-q", "-m", "slot work"]);
        assert_eq!(
            merged_into_base(&state),
            Some(false),
            "unmerged work must never read as reclaimable"
        );

        git(
            &root,
            &["merge", "--no-ff", "-q", "-m", "merge", &rec.branch],
        );
        assert_eq!(merged_into_base(&state), Some(true), "merged ⇒ reclaimable");

        // Documented limitation: containment is ancestry, so a squash merge reads unmerged.
        state.base_ref = None;
        assert_eq!(
            merged_into_base(&state),
            None,
            "no recorded base ⇒ no verdict, never a guess"
        );

        cleanup_run(&state, false).unwrap();
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

    /// The veto belongs at the deletion, not at one evidence path. It was written into
    /// `merged_into_base`, so `--older-than` walked past it and force-removed worktrees
    /// holding uncommitted agent work.
    #[test]
    fn cleanup_refuses_a_worktree_holding_work_unless_forced() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let base = git_out(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();

        let mut state = RunState::new("runveto", crate::cli::WorkflowKind::Loop, root.clone());
        state.base_ref = Some(base);
        state.base_commit = git_out(&root, &["rev-parse", "HEAD"]);
        let rec = create_worktree(&root, "runveto", "impl", state.base_commit.as_deref(), None).unwrap();
        state.worktrees.push(rec.clone());

        // Clean tree, nothing ahead: ordinary cleanup takes it.
        assert!(unsaved_work(&state, &rec).is_none());

        std::fs::write(rec.path.join("precious.rs"), "fn work() {}\n").unwrap();
        assert!(
            unsaved_work(&state, &rec).is_some(),
            "uncommitted work counts"
        );

        let report = cleanup_run(&state, false).unwrap();
        assert!(report[0].skipped.is_some(), "must refuse: {report:?}");
        assert!(!report[0].removed);
        assert!(rec.path.join("precious.rs").is_file(), "work survives");

        // Committed-but-unmerged counts too: remove_worktree also runs `git branch -D`.
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&rec.path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
        };
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "slot work"]);
        let why = unsaved_work(&state, &rec).expect("unmerged commits count");
        assert!(why.contains("commit"), "{why}");
        assert!(cleanup_run(&state, false).unwrap()[0].skipped.is_some());

        // --force is the operator naming the run and meaning it.
        let report = cleanup_run(&state, true).unwrap();
        assert!(report[0].skipped.is_none(), "{report:?}");
        assert!(!rec.path.exists());
    }

    /// "I cannot determine the base" must never read as "there is nothing to lose". Both
    /// triggers were live on the machine this came from: pre-O26 runs carry no `base_ref`,
    /// and a run based on a feature branch that has since been merged and deleted cannot
    /// resolve one.
    #[test]
    fn an_unresolvable_base_vetoes_rather_than_permits() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let mut state = RunState::new("runbase", crate::cli::WorkflowKind::Loop, root.clone());
        let rec = create_worktree(&root, "runbase", "impl", None, None).unwrap();
        state.worktrees.push(rec.clone());

        // Pre-O26: neither field recorded.
        state.base_ref = None;
        state.base_commit = None;
        let why = unsaved_work(&state, &rec).expect("must refuse");
        assert!(why.contains("base cannot be resolved"), "{why}");
        assert!(cleanup_run(&state, false).unwrap()[0].skipped.is_some());

        // A ref that no longer exists, with no commit recorded either.
        state.base_ref = Some("feat/deleted-last-week".into());
        assert!(unsaved_work(&state, &rec).is_some(), "dead ref must refuse");

        // base_commit is the ground truth and survives the ref going away.
        state.base_commit = git_out(&root, &["rev-parse", "HEAD"]);
        assert!(
            unsaved_work(&state, &rec).is_none(),
            "a resolvable base_commit is enough"
        );
        cleanup_run(&state, true).unwrap();
    }

    /// The veto reads the tree; `remove_worktree` deletes `rec.branch`. An agent that
    /// detached HEAD leaves HEAD level with the base while the branch still carries every
    /// commit it wrote.
    #[test]
    fn a_detached_head_does_not_hide_the_branchs_commits() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let mut state = RunState::new("rundetach", crate::cli::WorkflowKind::Loop, root.clone());
        state.base_commit = git_out(&root, &["rev-parse", "HEAD"]);
        let rec =
            create_worktree(&root, "rundetach", "impl", state.base_commit.as_deref(), None).unwrap();
        state.worktrees.push(rec.clone());

        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&rec.path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        std::fs::write(rec.path.join("work.rs"), "fn work() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "slot work"]);
        // Agent detaches to look at the original code and leaves it that way.
        git(&[
            "checkout",
            "-q",
            "--detach",
            state.base_commit.as_deref().unwrap(),
        ]);

        let why = unsaved_work(&state, &rec).expect("branch commits must still count");
        assert!(why.contains(&rec.branch), "{why}");
        assert!(cleanup_run(&state, false).unwrap()[0].skipped.is_some());
        cleanup_run(&state, true).unwrap();
    }

    #[test]
    fn build_cache_detection_survives_an_interrupted_build() {
        let tmp = tempdir().unwrap();
        let t = tmp.path();

        // Each marker alone is enough. The 22 GB tree that motivated this had no
        // CACHEDIR.TAG because the build died before cargo wrote it.
        for marker in ["CACHEDIR.TAG", ".rustc_info.json"] {
            let d = t.join(marker.replace('.', "_"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(marker), "x").unwrap();
            assert!(is_build_cache(&d, "target"), "{marker}");
        }
        for sub in ["debug", "release"] {
            let d = t.join(format!("by-{sub}"));
            std::fs::create_dir_all(d.join(sub)).unwrap();
            assert!(is_build_cache(&d, "target"), "{sub}");
        }

        // A source directory that happens to be called `target` is not a cache.
        let src = t.join("innocent");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(src.join("src/main.rs"), "fn main() {}").unwrap();
        assert!(!is_build_cache(&src, "target"));

        // node_modules needs no marker; it is regenerable by definition.
        let nm = t.join("nm");
        std::fs::create_dir_all(&nm).unwrap();
        assert!(is_build_cache(&nm, "node_modules"));
    }

    /// Reclaiming keeps everything a build cannot regenerate — that is what lets it run
    /// without the evidence `cleanup` demands.
    #[test]
    fn reclaim_takes_only_build_output() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let wt = tmp.path().join("repo-spar-r1-impl");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::create_dir_all(wt.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("target/debug/huge.rlib"), vec![0u8; 4096]).unwrap();
        std::fs::write(wt.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::write(wt.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(wt.join("uncommitted.txt"), "precious").unwrap();

        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, root);
        state.worktrees.push(WorktreeRecord {
            slot_id: "impl".into(),
            path: wt.clone(),
            branch: "spar/r1/impl".into(),
        });

        let reap = reap_build_cache(&state, &LiveCwds(Vec::new()));
        assert!(reap.freed_bytes >= 4096, "{reap:?}");
        assert!(!wt.join("target").exists());
        assert!(!wt.join("node_modules").exists());
        assert!(wt.join("src/main.rs").is_file(), "source survives");
        assert!(
            wt.join("uncommitted.txt").is_file(),
            "uncommitted work survives -- reclaim is not cleanup"
        );
        assert!(wt.is_dir(), "the worktree itself survives");
    }

    /// A symlinked `target/` belongs to whatever the operator pointed it at. Removing it
    /// unlinks their setup and reports space that was never freed — `remove_dir_all` does
    /// not follow, so the real tree survives and the byte count would be a lie.
    #[test]
    fn reclaim_leaves_a_symlinked_cache_alone() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let wt = tmp.path().join("repo-spar-r1-impl");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(elsewhere.join("debug")).unwrap();
        std::fs::write(elsewhere.join("debug/x.rlib"), vec![0u8; 2048]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, wt.join("target")).unwrap();

        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, root);
        state.worktrees.push(WorktreeRecord {
            slot_id: "impl".into(),
            path: wt.clone(),
            branch: "spar/r1/impl".into(),
        });

        let reap = reap_build_cache(&state, &LiveCwds(Vec::new()));
        assert_eq!(reap.freed_bytes, 0, "nothing was actually freed");
        assert!(wt.join("target").exists(), "the symlink survives");
        assert!(elsewhere.join("debug/x.rlib").is_file());
        assert!(
            reap.skipped.iter().any(|s| s.contains("symlinked")),
            "{reap:?}"
        );
    }

    #[test]
    fn reclaim_skips_a_tree_someone_is_working_in() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        let wt = tmp.path().join("repo-spar-r1-impl");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join("target/debug/x.rlib"), "obj").unwrap();

        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, root);
        state.worktrees.push(WorktreeRecord {
            slot_id: "impl".into(),
            path: wt.clone(),
            branch: "spar/r1/impl".into(),
        });

        let live = LiveCwds(vec![wt.join("src")]);
        let reap = reap_build_cache(&state, &live);
        assert_eq!(reap.freed_bytes, 0);
        assert_eq!(reap.skipped.len(), 1, "{reap:?}");
        assert!(wt.join("target/debug/x.rlib").is_file());
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

        let report = cleanup_run(&state, false).unwrap();
        assert!(report[0].removed);
        assert!(!wt.exists());
        assert!(report[1].skipped.is_some(), "project root must be refused");
        assert!(root.is_dir(), "project root must survive cleanup");
    }

    #[test]
    fn path_shape() {
        let p =
            worktree_path(Path::new("/home/u/projects/foo"), "abcd1234", "impl-claude", None).unwrap();
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
    fn configured_root_collects_runs_instead_of_scattering_siblings() {
        let root = PathBuf::from("/srv/spar-worktrees");
        let p = worktree_path(
            Path::new("/home/u/projects/foo"),
            "abcd1234",
            "impl-claude",
            Some(&root),
        )
        .unwrap();
        assert_eq!(
            p,
            PathBuf::from("/srv/spar-worktrees/foo/abcd1234-impl-claude")
        );
        // The point of the setting: nothing lands beside the repo any more.
        assert!(!p.starts_with("/home/u/projects/foo-spar"));
        assert!(p.starts_with(&root));
    }

    #[test]
    fn unset_root_keeps_the_sibling_layout() {
        let cfg = crate::config::Config::default();
        assert!(
            worktree_root(&cfg).is_none(),
            "default must stay the historical sibling path"
        );
    }

    #[test]
    fn configured_root_expands_a_leading_tilde() {
        let mut cfg = crate::config::Config::default();
        cfg.worktree.root = Some("~/projects/spar/worktrees".into());
        let expanded = worktree_root(&cfg).expect("root is set");
        assert!(expanded.is_absolute(), "~ must not survive into a git path");
        assert!(expanded.ends_with("projects/spar/worktrees"));
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

        let rec = create_worktree(&root, "runtest1", "slot-a", None, None).unwrap();
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
        cleanup_run(&state, false).unwrap();
    }

    #[test]
    fn worktree_is_cut_from_the_base_commit() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());
        let base = resolve_base(&root, &wt, None).unwrap().unwrap();
        let rec = create_worktree(&root, "runbase1", "slot-a", Some(&base.commit), None).unwrap();
        let head = git_out(&rec.path, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head, feat_head, "slot must branch from the invoking HEAD");
        assert!(rec.path.join("feature.txt").is_file());
        remove_worktree(&root, &rec).unwrap();
    }
}
