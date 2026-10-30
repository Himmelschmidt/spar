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
        let verify =
            |r: &str| git_out(from, &["rev-parse", "--verify", &format!("{r}^{{commit}}")]);
        if let Some(commit) = verify(reference) {
            // Record what `HEAD` *is*: a run whose base_ref is the literal "HEAD" reads as
            // detached later, and `ship` then declines to target the branch it came from.
            let reference = match reference {
                "HEAD" => named_head(from).unwrap_or_else(|| reference.to_string()),
                other => checkpoint_ref_name(other).unwrap_or_else(|| other.to_string()),
            };
            return Ok(Some(RunBase { reference, commit }));
        }
        // The local checkpoint first; then origin, which has it only when the run pushed
        // checkpoints and which default refspecs never fetch, so it is asked by name (O112).
        if let Some(full) = checkpoint_ref_name(reference) {
            if verify(&full).is_none() {
                let _ = crate::checkpoint::git_unattended(
                    from,
                    &["fetch", "--quiet", "origin", &format!("+{full}:{full}")],
                    std::time::Duration::from_secs(60),
                );
            }
            if let Some(commit) = verify(&full) {
                return Ok(Some(RunBase {
                    reference: full,
                    commit,
                }));
            }
        }
        let hint = slot_branch_checkpoint(reference)
            .map(|c| format!("; if that run died before ship, its work may be at --base {c}"))
            .unwrap_or_default();
        bail!(
            "--base {reference} does not resolve to a commit in {}{hint}",
            from.display()
        );
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

/// `refs/spar-checkpoint/<run>/<slot>` for any spelling of it an operator might type:
/// full, without `refs/`, or as if it were a remote-tracking ref under `origin/`.
fn checkpoint_ref_name(reference: &str) -> Option<String> {
    let bare = reference.strip_prefix("refs/").unwrap_or(reference);
    let bare = bare.strip_prefix("origin/").unwrap_or(bare);
    bare.starts_with("spar-checkpoint/")
        .then(|| format!("refs/{bare}"))
}

/// The checkpoint ref matching a slot branch (`[origin/]spar/<run>/<slot>`), the ref an
/// operator reaches for when the branch itself was never pushed.
fn slot_branch_checkpoint(reference: &str) -> Option<String> {
    let bare = reference.strip_prefix("origin/").unwrap_or(reference);
    let rest = bare.strip_prefix("spar/")?;
    let (run, slot) = rest.split_once('/')?;
    Some(crate::checkpoint::checkpoint_ref(run, slot))
}

/// The branch name checked out in `dir`, or `None` when detached.
pub(crate) fn named_head(dir: &Path) -> Option<String> {
    let branch = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

/// Puts `dir` back on `branch` when a slot left it on another one. Reviewers judge from
/// detached checkouts of their own (O138), so this is the safety net, not the mechanism. Repairs only a fast-forward, so no commit is ever dropped,
/// and never when `branch` still sits at the run's `base` (HEAD may then be any branch
/// that happens to be ahead of base, such as main). `None` when there is nothing to report.
pub fn restore_branch(dir: &Path, branch: &str, base: Option<&str>) -> Option<String> {
    let head = git_out(dir, &["rev-parse", "--verify", "HEAD"])?;
    let current = named_head(dir);
    if current.as_deref() == Some(branch) {
        return None;
    }
    let shown = current.unwrap_or_else(|| "a detached HEAD".into());
    let at = format!("worktree {} is on {shown}", dir.display());
    let branch_ref = format!("refs/heads/{branch}");
    let Some(old) = git_out(dir, &["rev-parse", "--verify", &branch_ref]) else {
        return Some(format!("{at} and {branch} does not exist; left as is"));
    };
    if base.is_some_and(|b| b == old) {
        return Some(format!(
            "{at} and {branch} never advanced past the run base; left as is"
        ));
    }
    if git_out(dir, &["merge-base", "--is-ancestor", &old, &head]).is_none() {
        return Some(format!(
            "{at}, which does not contain {branch}; left as is, \
             ship will refuse until {branch} is an ancestor of HEAD"
        ));
    }
    if let Some(other) = checked_out_elsewhere(dir, &branch_ref) {
        return Some(format!(
            "{at} and {branch} is checked out in {}; left as is",
            other.display()
        ));
    }
    if git_out(dir, &["update-ref", &branch_ref, &head, &old]).is_none() {
        return Some(format!(
            "{at}; could not fast-forward {branch} to its HEAD; left as is"
        ));
    }
    if git_out(dir, &["checkout", "--quiet", branch]).is_none() {
        return Some(format!(
            "{at}; fast-forwarded {branch} to its HEAD but could not check it out"
        ));
    }
    Some(format!(
        "worktree {} was on {shown}; fast-forwarded {branch} to its HEAD and checked it out",
        dir.display()
    ))
}

/// The other worktree that has `branch_ref` checked out, if any.
fn checked_out_elsewhere(dir: &Path, branch_ref: &str) -> Option<PathBuf> {
    let list = git_out(dir, &["worktree", "list", "--porcelain"])?;
    let here = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    list.split("\n\n").find_map(|entry| {
        let path = entry.lines().find_map(|l| l.strip_prefix("worktree "))?;
        entry
            .lines()
            .any(|l| l.strip_prefix("branch ") == Some(branch_ref))
            .then(|| PathBuf::from(path))
            .filter(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) != here)
    })
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
    // An explicit `--base` is the operator's choice of commit; only a base taken
    // implicitly from the invoking HEAD can be silently stale (O110).
    state.base_behind = requested.is_none().then(|| behind_upstream(&cwd)).flatten();
    if let Some(n) = state.base_behind.filter(|n| *n > 0) {
        let upstream = git_out(&cwd, &["rev-parse", "--abbrev-ref", "@{u}"])
            .unwrap_or_else(|| "its upstream".into());
        eprintln!(
            "warning: base {} is {n} commit(s) behind {upstream}; pass --base {upstream} to build on it",
            base.reference
        );
    }
    state.base_ref = Some(base.reference);
    state.base_commit = Some(base.commit);
    Ok(())
}

/// Commits `dir`'s HEAD is behind its upstream tracking branch, as of the last fetch.
/// `None` with no upstream (detached HEAD, local-only branch).
fn behind_upstream(dir: &Path) -> Option<u32> {
    git_out(dir, &["rev-list", "--count", "HEAD..@{u}"])?
        .parse()
        .ok()
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

pub(crate) fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
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
    exclude_run_outputs(&path);

    Ok(WorktreeRecord {
        slot_id: slot_id.into(),
        path,
        branch,
    })
}

/// Where a run's reviewers judge from (O138): `../<repo>-spar-<run>-reviewers/`, one
/// detached checkout per seat plus the `target/` they all build into.
pub fn reviewer_area(project_root: &Path, run_id: &str) -> Result<PathBuf> {
    worktree_path(project_root, run_id, "reviewers")
}

/// A detached checkout of `head` for reviewer `seat`, so nothing a reviewer runs can move
/// the implementer's branch (#158). Reused across rounds and forced onto each round's
/// panel head; whatever the reviewer left in it last round is discarded.
pub fn reviewer_checkout(
    state: &RunState,
    seat: &str,
    head: &str,
    copy: &[String],
) -> Result<PathBuf> {
    let root = &state.project_root;
    let area = reviewer_area(root, &state.id)?;
    let path = area.join(sanitize_slot(seat));
    if path.join(".git").exists() {
        if git_out(&path, &["checkout", "--quiet", "--force", "--detach", head]).is_none() {
            bail!(
                "could not move reviewer checkout {} to {head}",
                path.display()
            );
        }
        let _ = git_out(&path, &["clean", "-fdq"]);
        return Ok(path);
    }
    std::fs::create_dir_all(&area)?;
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    let _ = git_quiet(root, &["worktree", "prune"]);
    let path_s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))?;
    if !git_quiet(
        root,
        &["worktree", "add", "--quiet", "--detach", path_s, head],
    )? {
        bail!("git worktree add --detach failed for {}", path.display());
    }
    seed_env_files(root, &path, copy)?;
    Ok(path)
}

/// `CARGO_TARGET_DIR` for a reviewer working in one of the run's reviewer checkouts: one
/// `target/` per run shared by every seat, rather than one per checkout. Not under bwrap,
/// which leaves only the slot's own cwd writable.
pub fn reviewer_build_env(state: &RunState, cwd: &Path) -> Vec<(String, String)> {
    if matches!(state.isolation, IsolationMode::WorktreeBwrap) {
        return Vec::new();
    }
    let Ok(area) = reviewer_area(&state.project_root, &state.id) else {
        return Vec::new();
    };
    if cwd == area || !cwd.starts_with(&area) {
        return Vec::new();
    }
    vec![(
        "CARGO_TARGET_DIR".to_string(),
        area.join("target").display().to_string(),
    )]
}

/// Remove the run's reviewer checkouts and their shared build dir. They hold nothing a
/// reviewer is meant to keep, so there is no unsaved-work veto here.
fn remove_reviewer_area(state: &RunState) -> Option<WorktreeCleanup> {
    let root = &state.project_root;
    let area = reviewer_area(root, &state.id).ok()?;
    if !area.is_dir() || !reapable_worktree(root, &area) {
        return None;
    }
    let killed = crate::process::pids_with_cwd_under(&area);
    if !killed.is_empty() {
        crate::process::terminate_all(&killed);
    }
    if let Ok(rd) = std::fs::read_dir(&area) {
        for e in rd.flatten() {
            let p = e.path();
            if p.join(".git").is_file() {
                let _ = git_quiet(
                    root,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        p.to_str().unwrap_or_default(),
                    ],
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&area);
    let _ = git_quiet(root, &["worktree", "prune"]);
    Some(WorktreeCleanup {
        slot_id: "reviewers".into(),
        path: area.clone(),
        killed,
        removed: !area.exists(),
        skipped: None,
        archived: None,
    })
}

/// Keep spar's run store out of a slot's commits: an agent running `spar` in its worktree
/// can create `.spar/` there, and `git add -A` then puts it in the PR. git reads only the
/// common dir's `info/exclude` for a linked worktree (its own copy is ignored), so the
/// line applies to the whole repo, primary checkout included. Skipped when the repo
/// tracks files under `.spar/`.
fn exclude_run_outputs(worktree: &Path) {
    if git_out(worktree, &["ls-files", "--", ".spar"]).is_none_or(|s| !s.is_empty()) {
        return;
    }
    match crate::providers::presence::exclude_from_index(worktree, "/.spar/") {
        Ok(true) => eprintln!(
            "note: added /.spar/ to this repo's .git/info/exclude so spar's run store is never committed"
        ),
        Ok(false) => {}
        Err(e) => eprintln!("note: could not exclude .spar/ in {}: {e:#}", worktree.display()),
    }
}

/// Warn when the primary checkout changed while a run leg was driving slots (#178).
///
/// Called on every phase change. A leg starts when the run enters an active phase from a
/// waitable stop, from creation, or from `Init` (where a queued run waits for admission,
/// doing nothing): that is when `git status --porcelain` is captured. It ends at the next
/// waitable stop, which is when it is compared and the snapshot dropped, so a later leg
/// never compares against an earlier one. Time parked at a gate is the operator's, so
/// their own edits then are never reported. `.spar/` is outside the comparison, so the
/// `/.spar/` exclude spar adds mid-leg cannot show up as a change. `isolation = "none"`
/// runs slots in the primary checkout on purpose and is skipped.
pub fn watch_primary_checkout(
    paths: &SparPaths,
    state: &RunState,
    prev: Option<crate::state::Phase>,
) {
    use crate::state::Phase;
    if state.isolation == IsolationMode::None {
        return;
    }
    let working = |p: Phase| !p.is_waitable_stop() && p != Phase::Init;
    let starting = working(state.phase) && !prev.is_some_and(working);
    let stopping = state.phase.is_waitable_stop() && prev.is_some_and(working);
    if !starting && !stopping {
        return;
    }
    let file = paths.primary_status_file(&state.id);
    let Some(now) = porcelain(&state.project_root) else {
        return;
    };
    if starting {
        let _ = std::fs::write(&file, &now);
        return;
    }
    let Ok(before) = std::fs::read_to_string(&file) else {
        return;
    };
    let _ = std::fs::remove_file(&file);
    if before == now {
        return;
    }
    let changed: Vec<&str> = now
        .lines()
        .filter(|l| !before.lines().any(|b| b == *l))
        .chain(before.lines().filter(|b| !now.lines().any(|l| l == *b)))
        .collect();
    let shown = changed
        .iter()
        .take(5)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let more = changed.len().saturating_sub(5);
    let more = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    let msg = format!(
        "primary checkout {} changed while run {} was active ({shown}{more}); a slot may have written outside its worktree",
        state.project_root.display(),
        state.id
    );
    eprintln!("warning: {msg}");
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(msg));
}

/// `git status --porcelain` minus `.spar/`, untrimmed at the front: the leading space of
/// ` M path` is part of the status code, and `git_out`'s trim would strip it from the
/// first line only.
fn porcelain(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "--", ":!.spar"])
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
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

/// Copy `[worktree].copy` from the primary checkout into a fresh worktree. A file that is
/// missing, or already present in the worktree (tracked there), is left alone.
pub fn seed_env_files(project_root: &Path, worktree: &Path, names: &[String]) -> Result<()> {
    for name in names {
        let src = project_root.join(name);
        let dst = worktree.join(name);
        if !src.is_file() || dst.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)
            .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
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
            let cfg = if state.dry_run {
                None
            } else {
                crate::config::Config::for_run(paths, &state.id).ok()
            };
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
                let copy = cfg.as_ref().map_or_else(
                    || crate::config::WorktreeConfig::default().copy,
                    |c| c.worktree.copy.clone(),
                );
                seed_env_files(&state.project_root, &rec.path, &copy)?;
                if matches!(state.isolation, IsolationMode::WorktreeDb) {
                    maybe_dbiso(&state.project_root, &rec.path)?;
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

/// Squash-aware merge evidence for `cleanup --if-merged` (O113). `Err` says why the run
/// does not count as landed.
///
/// Every worktree must be clean, and every commit it can reach (its branch and its HEAD)
/// must be in the base by one of: ancestry; a merged PR for the branch whose head contains
/// that commit (`gh`, skipped when absent or failing); patch-id containment in the base,
/// commit by commit (rebase merge) or as one combined diff (squash merge). A branch that is
/// an ancestor of another branch that landed (the test author's, merged into the
/// implementer's) landed with it.
pub fn landed_in_base(state: &RunState) -> std::result::Result<(), String> {
    let root = &state.project_root;
    let base_ref = state
        .base_ref
        .as_deref()
        .ok_or("the run has no recorded base branch")?;
    let mut tips: Vec<String> = Vec::new();
    for r in [base_ref.to_string(), format!("origin/{base_ref}")] {
        if let Some(c) = git_out(root, &["rev-parse", "--verify", &format!("{r}^{{commit}}")]) {
            if !tips.contains(&c) {
                tips.push(c);
            }
        }
    }
    if tips.is_empty() {
        return Err(format!("base {base_ref} does not resolve"));
    }
    let mut landed: Vec<String> = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new();
    // `sync-prs` saw the PR holding this commit merged (O123): it and its ancestors landed.
    let merged_head = state
        .merged_at
        .and(state.shipped_head.as_deref())
        .filter(|_| state.shipped_round == Some(state.round));
    for rec in &state.worktrees {
        let mut commits = Vec::new();
        if rec.path.is_dir() {
            if worktree_holds_uncommitted(&rec.path) {
                return Err(format!("{} has uncommitted changes", rec.path.display()));
            }
            if let Some(head) = git_out(&rec.path, &["rev-parse", "--verify", "HEAD"]) {
                commits.push((format!("HEAD of {}", rec.path.display()), head));
            }
        }
        if let Some(tip) = resolve_commit(root, &rec.branch) {
            commits.retain(|(_, c)| *c != tip);
            commits.push((rec.branch.clone(), tip));
        }
        for (label, commit) in commits {
            if merged_head.is_some_and(|h| h == commit || is_ancestor(root, &commit, h)) {
                landed.push(commit);
                continue;
            }
            if commit_landed(
                root,
                &rec.branch,
                &commit,
                state.base_commit.as_deref(),
                &tips,
            ) {
                landed.push(commit);
            } else {
                pending.push((label, commit));
            }
        }
    }
    pending.retain(|(_, c)| !landed.iter().any(|l| is_ancestor(root, c, l)));
    match pending.first() {
        None => Ok(()),
        Some((label, _)) => Err(format!(
            "{label} is not in {base_ref}: not an ancestor, no merged PR containing it, \
             and no matching patch"
        )),
    }
}

fn is_ancestor(root: &Path, commit: &str, of: &str) -> bool {
    git_quiet(root, &["merge-base", "--is-ancestor", commit, of]).unwrap_or(false)
}

fn commit_landed(
    root: &Path,
    branch: &str,
    commit: &str,
    fork: Option<&str>,
    tips: &[String],
) -> bool {
    if tips.iter().any(|t| is_ancestor(root, commit, t)) {
        return true;
    }
    if merged_pr_head(root, branch)
        .is_some_and(|head| head == commit || is_ancestor(root, commit, &head))
    {
        return true;
    }
    tips.iter().any(|tip| {
        let fork = fork
            .map(str::to_string)
            .or_else(|| git_out(root, &["merge-base", commit, tip]));
        fork.is_some_and(|f| patches_in(root, &f, commit, tip))
    })
}

/// The head commit of `branch`'s merged PR, by `gh`. `None` when gh is missing, fails, or
/// the PR is not merged: the caller falls through to patch evidence.
fn merged_pr_head(root: &Path, branch: &str) -> Option<String> {
    let out = Command::new("gh")
        .args(["pr", "view", branch, "--json", "state,headRefOid"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    merged_head_from_json(&String::from_utf8_lossy(&out.stdout))
}

fn merged_head_from_json(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v["state"].as_str() != Some("MERGED") {
        return None;
    }
    v["headRefOid"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `fork..commit` is in `fork..tip` by patch-id: every commit has a twin (a rebase
/// merge), or the whole range's combined diff matches one commit (a squash merge).
fn patches_in(root: &Path, fork: &str, commit: &str, tip: &str) -> bool {
    if fork == commit {
        return true;
    }
    let upstream: Vec<String> = patch_ids(
        root,
        &[
            "log",
            "-p",
            "--no-merges",
            "--no-color",
            &format!("{fork}..{tip}"),
        ],
    );
    if upstream.is_empty() {
        return false;
    }
    let combined = patch_ids(root, &["diff", "--no-color", fork, commit]);
    if combined.first().is_some_and(|id| upstream.contains(id)) {
        return true;
    }
    let own = patch_ids(
        root,
        &[
            "log",
            "-p",
            "--no-merges",
            "--no-color",
            &format!("{fork}..{commit}"),
        ],
    );
    !own.is_empty() && own.iter().all(|id| upstream.contains(id))
}

/// Stable patch ids of the patches `git <args>` prints.
fn patch_ids(root: &Path, args: &[&str]) -> Vec<String> {
    let Some(patch) = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Vec::new();
    };
    let Ok(mut child) = Command::new("git")
        .args(["patch-id", "--stable"])
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    else {
        return Vec::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(&patch.stdout);
    }
    let Ok(out) = child.wait_with_output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect()
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
    /// The ref `--force` archived this worktree's work to before deleting it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<String>,
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
///
/// `halted` widens it to every run nothing is driving: parked at a human gate, or in
/// flight with no live orchestrator (`cache_reclaimable`).
pub fn reap_finished_caches(
    paths: &SparPaths,
    skip_run: Option<&str>,
    halted: bool,
) -> Result<Vec<CacheReap>> {
    let live = LiveCwds::snapshot();
    let mut out = Vec::new();
    for summary in crate::state::list_runs(paths)? {
        if skip_run == Some(summary.id.as_str()) {
            continue;
        }
        let Ok(state) = RunState::load(paths, &summary.id) else {
            continue;
        };
        if !cache_reclaimable(paths, &state, halted) {
            continue;
        }
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

/// Whether a run's build caches may be reclaimed. Finished and stopped runs always. With
/// `halted` (O116), also a run parked at a human gate, whose orchestrator has exited and
/// whose next step waits on a person, and an in-flight run no live orchestrator drives:
/// both can sit for days holding tens of gigabytes a build regenerates. The live-process
/// check in `reap_build_cache` still spares any tree something is working in.
pub fn cache_reclaimable(paths: &SparPaths, state: &RunState, halted: bool) -> bool {
    if state.phase.is_terminal() || state.phase == crate::state::Phase::Stopped {
        return true;
    }
    halted && (state.phase.is_gate() || state.abandoned(paths))
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
    let trees = state
        .worktrees
        .iter()
        .map(|r| r.path.clone())
        .chain(reviewer_area(&state.project_root, &state.id).ok());
    for tree in trees {
        if !tree.is_dir() || !reapable_worktree(&state.project_root, &tree) {
            continue;
        }
        if live.inside(&tree) {
            reap.skipped.push(format!(
                "{}: a live process is working here",
                tree.display()
            ));
            continue;
        }
        for name in CACHE_DIRS {
            let dir = tree.join(name);
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
///
/// `force` never deletes work outright: whatever the veto would have protected is first
/// archived to `refs/spar-archive/<run>/<slot>` (O113), and a worktree that cannot be
/// archived is kept.
pub fn cleanup_run(state: &RunState, force: bool) -> Result<Vec<WorktreeCleanup>> {
    let mut report = Vec::new();
    for rec in &state.worktrees {
        let mut archived = None;
        if let Some(why) = unsaved_work(state, rec) {
            let refused = if force {
                match archive_worktree(state, rec) {
                    Ok(reference) => {
                        archived = Some(reference);
                        None
                    }
                    Err(e) => Some(format!("holds {why} and could not be archived ({e})")),
                }
            } else {
                Some(format!("holds {why} — reap by run id with --force"))
            };
            if let Some(reason) = refused {
                report.push(WorktreeCleanup {
                    slot_id: rec.slot_id.clone(),
                    path: rec.path.clone(),
                    killed: Vec::new(),
                    removed: false,
                    skipped: Some(reason),
                    archived: None,
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
                archived,
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
            archived,
        });
    }
    report.extend(remove_reviewer_area(state));
    Ok(report)
}

/// Snapshot a worktree's dirty tree and every commit its HEAD and branch reach into one
/// commit under `refs/spar-archive/<run>/<slot>` in the project repo, which outlives both
/// the worktree and `git branch -D`.
fn archive_worktree(state: &RunState, rec: &WorktreeRecord) -> std::result::Result<String, String> {
    let root = &state.project_root;
    let branch = resolve_commit(root, &rec.branch);
    let commit = if rec.path.is_dir() {
        let extra: Vec<&str> = branch.iter().map(String::as_str).collect();
        crate::checkpoint::snapshot(
            &rec.path,
            &format!(
                "spar archive: run {} slot {} before cleanup --force",
                state.id, rec.slot_id
            ),
            &extra,
        )
        .ok_or("git could not snapshot the worktree")?
    } else {
        branch.ok_or("its branch does not resolve")?
    };
    let reference = crate::checkpoint::archive_ref(&state.id, &rec.slot_id);
    if !crate::checkpoint::record(root, &reference, &commit) {
        return Err(format!("could not write {reference}"));
    }
    Ok(reference)
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
/// Fail closed when the author worktree is missing. Committed author work crosses by
/// `git merge` of the author branch, and the overlay adds only what a merge cannot carry:
/// the files the author left uncommitted. The overlay used to copy every git-visible file
/// in the author worktree — 3202 of them on a live BidDesk run — which reverted the
/// implementer's own source to the author branch's revision on every dispatch after the
/// first (O53).
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
    let scope = if dry_or_spar {
        OverlayScope::WholeTree
    } else {
        merge_spec_branch(impl_cwd, &spec.branch)?;
        OverlayScope::Uncommitted
    };
    copy_tree_overlay(&spec.path, impl_cwd, scope)?;
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

/// Merge the author branch into the implementer worktree. Abort on failure so the tree is
/// never left in MERGING, then fail the dispatch.
///
/// This used to swallow a failed merge, because the overlay copied the author's committed
/// files too and would deliver the acceptance tests anyway. It no longer does, so the merge
/// is the only path committed author work takes: a swallowed failure here means the
/// implementer is told to satisfy acceptance tests that are not in its worktree (O53).
fn merge_spec_branch(impl_cwd: &Path, branch: &str) -> Result<()> {
    let out = Command::new("git")
        .args([
            "merge",
            "--no-edit",
            "-m",
            "spar: acceptance tests from test-author",
            branch,
        ])
        .current_dir(impl_cwd)
        .output()
        .with_context(|| format!("git merge {branch} into {}", impl_cwd.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let _ = Command::new("git")
        .args(["merge", "--abort"])
        .current_dir(impl_cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // Conflict detail lands on stdout ("CONFLICT (content): …"); refusals to start
    // ("Your local changes … would be overwritten") land on stderr. Both are the answer.
    let why = [&out.stderr, &out.stdout]
        .into_iter()
        .map(|s| String::from_utf8_lossy(s).trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "no output".into());
    bail!(
        "git merge {branch} into {} failed (aborted, worktree left clean): {why}",
        impl_cwd.display()
    );
}

/// Which of the author's files the overlay has to carry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayScope {
    /// A merge already delivered every committed author file, so the overlay adds only
    /// what a merge cannot: the author's uncommitted work. Anything wider reverts the
    /// implementer's own source to the author branch's revision.
    Uncommitted,
    /// No merge ran (dry runs, `.spar/`-hosted cwds have no shared history), so nothing
    /// else delivers the author's files and the overlay carries the visible tree.
    WholeTree,
}

fn copy_tree_overlay(src: &Path, dst: &Path, scope: OverlayScope) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in overlay_sources(src, scope)? {
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

/// What the author worktree hands the implementer, per `scope`.
///
/// `Uncommitted` is the live-run set and it never falls back to a wider one. An empty
/// answer is the *normal* answer there — an author who committed everything has nothing a
/// merge did not already deliver — so emptiness cannot mean "list more". A git that
/// errored cannot mean it either: the wide sets below copy the author's revision of every
/// production file over the implementer's, which is exactly the data loss this scope
/// exists to prevent, so a broken git is a hard error instead.
///
/// `WholeTree` keeps the pre-existing behaviour, git-visible files with a walk behind
/// them: a dry-run cwd lives under an ignored `.spar/` where `ls-files` is correctly empty
/// but the stub artifacts still have to cross. The walk copies build output (`target/`,
/// `node_modules/`), so a git that *errored* into it says so out loud.
fn overlay_sources(src: &Path, scope: OverlayScope) -> Result<Vec<PathBuf>> {
    if scope == OverlayScope::Uncommitted {
        return uncommitted_files(src).map_err(|why| {
            anyhow::anyhow!(
                "git could not list uncommitted files in {} ({why}); refusing to overlay \
                 the whole author worktree, which would revert the implementer's work",
                src.display()
            )
        });
    }
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

/// Regular files in `dir` that a merge of its branch cannot deliver: tracked files with
/// uncommitted modifications (staged or not), plus untracked files git would keep.
fn uncommitted_files(dir: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let mut out = git_paths(dir, &["diff", "-z", "--name-only", "HEAD"])?;
    for p in git_paths(dir, &["ls-files", "-z", "--others", "--exclude-standard"])? {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// Regular files under `dir` that git tracks or would add. `Err` when git can't answer.
fn git_listed_files(dir: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    git_paths(
        dir,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
}

/// Run a NUL-separated path-listing git command in `dir` and resolve what it names.
fn git_paths(dir: &Path, args: &[&str]) -> std::result::Result<Vec<PathBuf>, String> {
    let out = Command::new("git")
        .args(args)
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
        // These also report symlinks, submodules, and paths deleted in the working tree.
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

    /// git in a fixture, never reading the developer's global config (gpg signing, hooks,
    /// commit templates), which would otherwise fail a commit here.
    fn fixture_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
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
    }

    /// #158: a reviewer works in a detached checkout of its own, reused and forced onto
    /// each round's panel head, so nothing it does moves the implementer's branch.
    #[test]
    fn reviewer_checkouts_are_detached_reused_and_never_move_the_impl_branch() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        fixture_git(&root, &["init", "-q"]);
        fixture_git(&root, &["config", "user.email", "t@t.com"]);
        fixture_git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("README"), "x").unwrap();
        fixture_git(&root, &["add", "."]);
        fixture_git(&root, &["commit", "-q", "-m", "init"]);
        let mut state = RunState::new("r1", crate::cli::WorkflowKind::Loop, root.clone());
        let rec = create_worktree(&root, "r1", "impl", None).unwrap();
        state.worktrees.push(rec.clone());
        let commit = |msg: &str| {
            std::fs::write(rec.path.join(format!("{msg}.txt")), msg).unwrap();
            fixture_git(&rec.path, &["add", "."]);
            fixture_git(&rec.path, &["commit", "-q", "-m", msg]);
            fixture_git(&rec.path, &["rev-parse", "HEAD"])
        };
        let head1 = commit("one");

        let rc = reviewer_checkout(&state, "review-0-cli-codex", &head1, &[]).unwrap();
        let area = reviewer_area(&root, "r1").unwrap();
        assert_eq!(rc, area.join("review-0-cli-codex"));
        assert_eq!(fixture_git(&rc, &["rev-parse", "HEAD"]), head1);
        assert_eq!(named_head(&rc), None, "detached");

        fixture_git(&rc, &["checkout", "-q", "-b", "review-x"]);
        std::fs::write(rc.join("evil.txt"), "evil").unwrap();
        fixture_git(&rc, &["add", "."]);
        fixture_git(&rc, &["commit", "-q", "-m", "evil"]);
        std::fs::write(rc.join("scratch.txt"), "left behind").unwrap();
        assert_eq!(named_head(&rec.path).as_deref(), Some("spar/r1/impl"));
        assert_eq!(fixture_git(&rec.path, &["rev-parse", "HEAD"]), head1);

        let head2 = commit("two");
        let again = reviewer_checkout(&state, "review-0-cli-codex", &head2, &[]).unwrap();
        assert_eq!(again, rc, "reused across rounds");
        assert_eq!(fixture_git(&rc, &["rev-parse", "HEAD"]), head2);
        assert_eq!(named_head(&rc), None);
        assert!(!rc.join("evil.txt").exists());
        assert!(!rc.join("scratch.txt").exists());

        assert_eq!(
            reviewer_build_env(&state, &rc),
            vec![(
                "CARGO_TARGET_DIR".to_string(),
                area.join("target").display().to_string()
            )]
        );
        assert!(reviewer_build_env(&state, &rec.path).is_empty());
        state.isolation = IsolationMode::WorktreeBwrap;
        assert!(reviewer_build_env(&state, &rc).is_empty());

        let report = cleanup_run(&state, false).unwrap();
        assert!(report.iter().any(|r| r.slot_id == "reviewers" && r.removed));
        assert!(!area.exists());
        assert!(!fixture_git(&root, &["worktree", "list"]).contains("reviewers"));
    }

    /// A repo with production source, plus a test-author and an implementer worktree cut
    /// from one base, wired into a live (non-dry-run) run state.
    fn author_and_impl_worktrees(tmp: &Path) -> (RunState, PathBuf, PathBuf) {
        let root = tmp.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        fixture_git(&root, &["init", "-q"]);
        fixture_git(&root, &["config", "user.email", "t@t.com"]);
        fixture_git(&root, &["config", "user.name", "t"]);
        // Local config, so the merge spar itself runs (which does not scrub global config,
        // because production must not) still commits on a machine that signs by default.
        fixture_git(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("src/task.rs"), "fn base() {}\n").unwrap();
        std::fs::write(root.join("src/download.rs"), "fn download() {}\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "mod task;\nmod download;\n").unwrap();
        fixture_git(&root, &["add", "."]);
        fixture_git(&root, &["commit", "-q", "-m", "init"]);
        let base = fixture_git(&root, &["rev-parse", "HEAD"]);

        let mut state = RunState::new("ovl", crate::cli::WorkflowKind::Loop, root.clone());
        state.base_commit = Some(base.clone());
        let author = create_worktree(&root, "ovl", "test-author-x", Some(&base)).unwrap();
        let implementer = create_worktree(&root, "ovl", "impl", Some(&base)).unwrap();
        let paths = (author.path.clone(), implementer.path.clone());
        state.worktrees.push(author);
        state.worktrees.push(implementer);
        (state, paths.0, paths.1)
    }

    /// The regression: the overlay listed `--cached` files, so it copied the author
    /// branch's revision of every tracked file over the implementer's worktree. On a live
    /// BidDesk run that was 3202 files, 486 of them production source, to deliver zero
    /// uncommitted ones — and `models/task.rs` came back byte-identical to the author tip
    /// with the implementer's work gone (O53).
    #[test]
    fn overlay_carries_only_the_authors_uncommitted_work() {
        let tmp = tempdir().unwrap();
        let (state, author, impl_cwd) = author_and_impl_worktrees(tmp.path());

        // What a test author leaves behind: committed acceptance tests, plus a tracked
        // edit and a new file it never got round to committing.
        std::fs::create_dir_all(author.join("tests")).unwrap();
        std::fs::write(author.join("tests/committed_acc.rs"), "fn committed() {}\n").unwrap();
        fixture_git(&author, &["add", "."]);
        fixture_git(&author, &["commit", "-q", "-m", "acceptance tests"]);
        std::fs::write(
            author.join("src/lib.rs"),
            "mod task;\nmod download;\n#[cfg(test)]\nmod acc;\n",
        )
        .unwrap();
        std::fs::write(author.join("tests/uncommitted_acc.rs"), "fn pending() {}\n").unwrap();

        // The implementer has written the feature into a file the author never touched.
        let implemented = "pub struct PipelineSuite;\n";
        std::fs::write(impl_cwd.join("src/task.rs"), implemented).unwrap();

        apply_spec_tests_to_impl(&state, "test-author-x", &impl_cwd).unwrap();

        assert!(
            impl_cwd.join("tests/committed_acc.rs").is_file(),
            "committed author tests arrive by merge"
        );
        assert_eq!(
            std::fs::read_to_string(impl_cwd.join("tests/uncommitted_acc.rs")).unwrap(),
            "fn pending() {}\n",
            "an untracked author file is what the overlay exists for"
        );
        assert!(
            std::fs::read_to_string(impl_cwd.join("src/lib.rs"))
                .unwrap()
                .contains("mod acc"),
            "a tracked-but-modified author file crosses too"
        );
        assert_eq!(
            std::fs::read_to_string(impl_cwd.join("src/task.rs")).unwrap(),
            implemented,
            "a production file the author never touched must survive byte-for-byte"
        );
        assert_eq!(
            std::fs::read_to_string(impl_cwd.join("src/download.rs")).unwrap(),
            "fn download() {}\n",
            "and so must every other file the author never touched"
        );
    }

    /// The shape that lost a whole fix round in run `e663edaf`: dispatch two, into a
    /// worktree where the implementer has already committed.
    #[test]
    fn a_second_dispatch_leaves_committed_implementer_work_intact() {
        let tmp = tempdir().unwrap();
        let (state, author, impl_cwd) = author_and_impl_worktrees(tmp.path());

        std::fs::create_dir_all(author.join("tests")).unwrap();
        std::fs::write(author.join("tests/round1.rs"), "fn r1() {}\n").unwrap();
        fixture_git(&author, &["add", "."]);
        fixture_git(&author, &["commit", "-q", "-m", "round 1 tests"]);
        apply_spec_tests_to_impl(&state, "test-author-x", &impl_cwd).unwrap();

        let feature = "pub struct PipelineSuite;\npub fn run() {}\n";
        std::fs::write(impl_cwd.join("src/task.rs"), feature).unwrap();
        std::fs::write(
            impl_cwd.join("src/download.rs"),
            "fn download() { /* long */ }\n",
        )
        .unwrap();
        fixture_git(&impl_cwd, &["add", "."]);
        fixture_git(
            &impl_cwd,
            &["commit", "-q", "-m", "implement pipeline suite"],
        );

        std::fs::write(author.join("tests/round2.rs"), "fn r2() {}\n").unwrap();
        fixture_git(&author, &["add", "."]);
        fixture_git(&author, &["commit", "-q", "-m", "round 2 tests"]);
        apply_spec_tests_to_impl(&state, "test-author-x", &impl_cwd).unwrap();

        assert!(
            impl_cwd.join("tests/round2.rs").is_file(),
            "the new round's tests still have to arrive"
        );
        assert_eq!(
            std::fs::read_to_string(impl_cwd.join("src/task.rs")).unwrap(),
            feature,
            "round-1 work must survive the round-2 dispatch"
        );
        assert_eq!(
            std::fs::read_to_string(impl_cwd.join("src/download.rs")).unwrap(),
            "fn download() { /* long */ }\n"
        );
    }

    /// The wide walk is the clobber with a different list behind it, so the narrow scope
    /// never falls back to it. The wide scope keeps its fallback: dry-run cwds live under
    /// an ignored `.spar/` where `ls-files` is correctly empty.
    #[test]
    fn the_narrow_overlay_refuses_rather_than_widening_when_git_cannot_answer() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("not-a-repo");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("prod.rs"), "fn prod() {}\n").unwrap();

        let err = overlay_sources(&src, OverlayScope::Uncommitted).unwrap_err();
        assert!(err.to_string().contains("refusing"), "err={err}");
        assert!(!overlay_sources(&src, OverlayScope::WholeTree)
            .unwrap()
            .is_empty());
    }

    /// The merge is now the only path committed author work takes, so swallowing a failed
    /// one hands the implementer acceptance tests that are not in its worktree.
    #[test]
    fn a_failed_merge_fails_the_dispatch_instead_of_dropping_the_tests() {
        let tmp = tempdir().unwrap();
        let (state, author, impl_cwd) = author_and_impl_worktrees(tmp.path());

        std::fs::write(author.join("src/task.rs"), "fn author_side() {}\n").unwrap();
        fixture_git(&author, &["add", "."]);
        fixture_git(&author, &["commit", "-q", "-m", "author edit"]);
        std::fs::write(impl_cwd.join("src/task.rs"), "fn impl_side() {}\n").unwrap();
        fixture_git(&impl_cwd, &["add", "."]);
        fixture_git(&impl_cwd, &["commit", "-q", "-m", "impl edit"]);

        let err = apply_spec_tests_to_impl(&state, "test-author-x", &impl_cwd).unwrap_err();
        assert!(err.to_string().contains("git merge"), "err={err}");
        assert_eq!(
            std::fs::read_to_string(impl_cwd.join("src/task.rs")).unwrap(),
            "fn impl_side() {}\n",
            "the aborted merge leaves the implementer's committed work alone"
        );
        assert!(
            fixture_git(&impl_cwd, &["status", "--porcelain"]).is_empty(),
            "the worktree must never be left MERGING"
        );
    }

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
        copy_tree_overlay(&src, &dst, OverlayScope::WholeTree).unwrap();
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

        copy_tree_overlay(&src, &dst, OverlayScope::WholeTree).unwrap();
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
        copy_tree_overlay(&src, &dst, OverlayScope::WholeTree).unwrap();
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

        let rec = create_worktree(&root, "runmerge", "impl", state.base_commit.as_deref()).unwrap();
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
        let rec = create_worktree(&root, "runveto", "impl", state.base_commit.as_deref()).unwrap();
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

    /// #157: `--force` used to delete uncommitted edits and unmerged commits outright. Now
    /// both land in one archive commit that outlives the worktree and `branch -D`.
    #[test]
    fn force_archives_dirty_and_unmerged_work_before_deleting() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let mut state = RunState::new("runarch", crate::cli::WorkflowKind::Loop, root.clone());
        state.base_commit = git_out(&root, &["rev-parse", "HEAD"]);
        let rec = create_worktree(&root, "runarch", "impl", state.base_commit.as_deref()).unwrap();
        state.worktrees.push(rec.clone());
        std::fs::write(rec.path.join("committed.rs"), "c\n").unwrap();
        fixture_git(&rec.path, &["add", "."]);
        fixture_git(
            &rec.path,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "c",
            ],
        );
        std::fs::write(rec.path.join("dirty.rs"), "d\n").unwrap();

        let report = cleanup_run(&state, true).unwrap();
        assert!(report[0].removed, "{report:?}");
        assert_eq!(
            report[0].archived.as_deref(),
            Some("refs/spar-archive/runarch/impl")
        );
        let archived = fixture_git(&root, &["rev-parse", "refs/spar-archive/runarch/impl"]);
        assert_eq!(
            fixture_git(&root, &["show", &format!("{archived}:committed.rs")]),
            "c"
        );
        assert_eq!(
            fixture_git(&root, &["show", &format!("{archived}:dirty.rs")]),
            "d"
        );
        assert!(
            resolve_commit(&root, &rec.branch).is_none(),
            "branch deleted"
        );
    }

    fn landed_fixture(tmp: &Path) -> (RunState, WorktreeRecord) {
        let (root, _wt, _) = repo_with_linked_worktree(tmp);
        let base = fixture_git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let mut state = RunState::new("runland", crate::cli::WorkflowKind::Loop, root.clone());
        state.base_ref = Some(base);
        state.base_commit = git_out(&root, &["rev-parse", "HEAD"]);
        let rec = create_worktree(&root, "runland", "impl", state.base_commit.as_deref()).unwrap();
        state.worktrees.push(rec.clone());
        for name in ["one", "two"] {
            std::fs::write(rec.path.join(name), format!("{name}\n")).unwrap();
            fixture_git(&rec.path, &["add", "."]);
            fixture_git(
                &rec.path,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    name,
                ],
            );
        }
        (state, rec)
    }

    /// #157: a squash merge is not an ancestor, so ancestry alone called every landed
    /// branch unmerged and made `--force` routine. Patch-id containment sees it.
    /// O123: `sync-prs` recorded a merge of the PR holding the shipped commit; that
    /// counts as landed for `cleanup --if-merged`, but only for this round's ship and
    /// only for commits the merged head contains.
    #[test]
    fn landed_in_base_accepts_a_recorded_merge_of_the_shipped_head() {
        let tmp = tempdir().unwrap();
        let (mut state, rec) = landed_fixture(tmp.path());
        let head = git_out(&rec.path, &["rev-parse", "HEAD"]).unwrap();
        assert!(landed_in_base(&state).is_err());

        state.merged_at = Some(chrono::Utc::now());
        state.shipped_head = Some(head);
        state.shipped_round = Some(state.round);
        assert_eq!(landed_in_base(&state), Ok(()));

        state.shipped_round = Some(state.round - 1);
        assert!(landed_in_base(&state).is_err(), "an earlier round's merge");
        state.shipped_round = Some(state.round);

        std::fs::write(rec.path.join("after"), "after\n").unwrap();
        fixture_git(&rec.path, &["add", "."]);
        fixture_git(
            &rec.path,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "after",
            ],
        );
        assert!(
            landed_in_base(&state).is_err(),
            "work after the merged head"
        );
    }

    #[test]
    fn landed_in_base_recognizes_a_squash_merge_and_refuses_unmerged_work() {
        let tmp = tempdir().unwrap();
        let (state, rec) = landed_fixture(tmp.path());
        let root = state.project_root.clone();
        let err = landed_in_base(&state).unwrap_err();
        assert!(err.contains(&rec.branch), "{err}");

        fixture_git(&root, &["merge", "--squash", "-q", &rec.branch]);
        fixture_git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "squashed",
            ],
        );
        assert_eq!(landed_in_base(&state), Ok(()));

        std::fs::write(rec.path.join("late.rs"), "x\n").unwrap();
        let err = landed_in_base(&state).unwrap_err();
        assert!(err.contains("uncommitted"), "{err}");
        std::fs::remove_file(rec.path.join("late.rs")).unwrap();

        std::fs::write(rec.path.join("after"), "after\n").unwrap();
        fixture_git(&rec.path, &["add", "."]);
        fixture_git(
            &rec.path,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "after",
            ],
        );
        assert!(
            landed_in_base(&state).is_err(),
            "a commit made after the squash is not in the base"
        );
    }

    #[test]
    fn landed_in_base_recognizes_a_rebase_merge() {
        let tmp = tempdir().unwrap();
        let (state, rec) = landed_fixture(tmp.path());
        let root = state.project_root.clone();
        fixture_git(&root, &["commit", "-q", "--allow-empty", "-m", "moved on"]);
        let commits = fixture_git(
            &root,
            &["rev-list", "--reverse", &format!("HEAD~1..{}", rec.branch)],
        );
        for c in commits.lines() {
            fixture_git(
                &root,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "cherry-pick",
                    c,
                ],
            );
        }
        assert_eq!(landed_in_base(&state), Ok(()));
    }

    #[test]
    fn a_merged_pr_counts_only_when_its_head_contains_the_commit() {
        assert_eq!(
            merged_head_from_json(r#"{"state":"MERGED","headRefOid":"abc"}"#).as_deref(),
            Some("abc")
        );
        assert_eq!(
            merged_head_from_json(r#"{"state":"OPEN","headRefOid":"abc"}"#),
            None
        );
        assert_eq!(
            merged_head_from_json(r#"{"state":"CLOSED","headRefOid":"abc"}"#),
            None
        );
        assert_eq!(merged_head_from_json("not json"), None);
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
        let rec = create_worktree(&root, "runbase", "impl", None).unwrap();
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
            create_worktree(&root, "rundetach", "impl", state.base_commit.as_deref()).unwrap();
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

    /// #156: a replacement run on another box bases on the dead run's checkpoint. It was
    /// pushed to origin and never fetched here, so `--base` fetches it by name, in any
    /// spelling, and a missing slot branch names the checkpoint to use instead.
    #[test]
    fn base_resolves_a_checkpoint_ref_only_origin_has() {
        let tmp = tempdir().unwrap();
        let (root, wt, feat_head) = repo_with_linked_worktree(tmp.path());
        let origin = tmp.path().join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        fixture_git(&origin, &["init", "-q", "--bare"]);
        fixture_git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        fixture_git(&root, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
        fixture_git(
            &wt,
            &["push", "-q", "origin", "HEAD:refs/spar-checkpoint/r9/impl"],
        );

        let clone = tmp.path().join("clone");
        fixture_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        for spelling in [
            "refs/spar-checkpoint/r9/impl",
            "origin/spar-checkpoint/r9/impl",
            "spar-checkpoint/r9/impl",
        ] {
            let base = resolve_base(&clone, &clone, Some(spelling))
                .unwrap()
                .unwrap();
            assert_eq!(base.commit, feat_head, "{spelling}");
            assert_eq!(base.reference, "refs/spar-checkpoint/r9/impl");
        }
        let err = resolve_base(&clone, &clone, Some("origin/spar/r9/impl"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refs/spar-checkpoint/r9/impl"), "{err}");
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

    /// #176: gitignored local config reaches every slot worktree, in plain `worktree`
    /// isolation too, from the list `[worktree].copy` names. Missing entries are skipped.
    #[test]
    fn prepare_isolation_copies_configured_files_into_slot_worktrees() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        std::fs::write(
            root.join("spar.toml"),
            "[worktree]\ncopy = [\".db.env\", \"conf/local.env\", \"missing.env\"]\n",
        )
        .unwrap();
        std::fs::write(root.join(".db.env"), "DATABASE_URL=x\n").unwrap();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::write(root.join("conf/local.env"), "K=v\n").unwrap();
        std::fs::write(root.join(".envrc"), "not listed\n").unwrap();

        let mut state = RunState::new("runcopy1", crate::cli::WorkflowKind::Loop, root.clone());
        state.isolation = IsolationMode::Worktree;
        state.dry_run = false;
        state.base_commit = git_out(&root, &["rev-parse", "HEAD"]);
        let paths = SparPaths::new(&root);
        paths.ensure_run_dirs(&state.id).unwrap();
        prepare_isolation(&mut state, &paths, &["impl".to_string()]).unwrap();

        let wt = &state.worktrees.first().expect("slot worktree").path;
        assert_eq!(
            std::fs::read_to_string(wt.join(".db.env")).unwrap(),
            "DATABASE_URL=x\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("conf/local.env")).unwrap(),
            "K=v\n"
        );
        assert!(!wt.join("missing.env").exists());
        assert!(
            !wt.join(".envrc").exists(),
            "only the configured list is copied"
        );
        cleanup_run(&state, false).unwrap();
    }

    /// #178: `.spar/` an agent creates in its worktree never shows up as a change, while
    /// `artifacts/` stays visible (it may be the project's own), and a repo that tracks
    /// `.spar/` is left alone.
    #[test]
    fn slot_worktrees_exclude_spar_only() {
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let rec = create_worktree(&root, "runexcl", "impl", None).unwrap();
        for dir in ["artifacts", ".spar"] {
            std::fs::create_dir_all(rec.path.join(dir)).unwrap();
            std::fs::write(rec.path.join(dir).join("summary-impl.md"), "x").unwrap();
        }
        assert_eq!(
            git_out(&rec.path, &["status", "--porcelain"]).unwrap(),
            "?? artifacts/"
        );
        let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == "/.spar/").count(), 1);
        assert!(!exclude.contains("artifacts"), "{exclude}");
        let again = create_worktree(&root, "runexcl", "impl-2", None).unwrap();
        let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(
            exclude.lines().filter(|l| *l == "/.spar/").count(),
            1,
            "idempotent"
        );
        remove_worktree(&root, &rec).unwrap();
        remove_worktree(&root, &again).unwrap();

        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        std::fs::create_dir_all(root.join(".spar")).unwrap();
        std::fs::write(root.join(".spar/keep.txt"), "tracked").unwrap();
        fixture_git(&root, &["add", "-f", ".spar/keep.txt"]);
        fixture_git(&root, &["commit", "-q", "-m", "track .spar"]);
        let rec = create_worktree(&root, "runexcl2", "impl", None).unwrap();
        std::fs::write(rec.path.join(".spar/new.txt"), "x").unwrap();
        assert_eq!(
            git_out(&rec.path, &["status", "--porcelain"]).unwrap(),
            "?? .spar/new.txt"
        );
        remove_worktree(&root, &rec).unwrap();
    }

    fn drift(paths: &SparPaths, run_id: &str) -> Option<String> {
        crate::events::read_all(paths, run_id)
            .unwrap()
            .into_iter()
            .filter_map(|e| e.message)
            .find(|m| m.contains("primary checkout"))
    }

    /// #178: a primary checkout that changes while slots run is warned once the leg
    /// stops; edits made while the run is parked at a gate are the operator's and are not.
    #[test]
    fn a_primary_checkout_changed_during_a_leg_is_reported() {
        use crate::state::Phase;
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let paths = SparPaths::new(&root);
        let mut state = RunState::new("runwatch", crate::cli::WorkflowKind::Loop, root.clone());
        state.save(&paths).unwrap();
        state.set_phase(Phase::Dispatch);
        state.save(&paths).unwrap();
        state.set_phase(Phase::AwaitingPlanApproval);
        state.save(&paths).unwrap();
        assert_eq!(drift(&paths, "runwatch"), None);

        std::fs::write(root.join("README"), "operator edit at the gate").unwrap();
        state.set_phase(Phase::PlanApproved);
        state.save(&paths).unwrap();
        state.set_phase(Phase::Dispatch);
        state.save(&paths).unwrap();
        state.set_phase(Phase::AwaitingShipConfirm);
        state.save(&paths).unwrap();
        assert_eq!(
            drift(&paths, "runwatch"),
            None,
            "edits made at a gate are not the run's"
        );

        state.set_phase(Phase::Shipping);
        state.save(&paths).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        state.set_phase(Phase::Failed);
        state.save(&paths).unwrap();
        let hit = drift(&paths, "runwatch").expect("drift reported");
        assert!(hit.contains("?? Cargo.toml"), "{hit}");
        assert!(
            !hit.contains("README"),
            "pre-existing changes are not news: {hit}"
        );
        assert!(
            !paths.primary_status_file("runwatch").exists(),
            "a leg's snapshot is dropped when it is compared"
        );
    }

    /// A queued run sits at `Init` until admitted: the leg starts when work does, and
    /// the `/.spar/` exclude added when slots are cut mid-leg is not a change.
    #[test]
    fn the_drift_leg_starts_at_work_and_ignores_spars_own_exclude() {
        use crate::state::Phase;
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        let paths = SparPaths::new(&root);
        let mut state = RunState::new("runqueue", crate::cli::WorkflowKind::Loop, root.clone());
        state.save(&paths).unwrap();
        assert!(
            !paths.primary_status_file("runqueue").exists(),
            "queued: no leg yet"
        );

        std::fs::write(root.join("README"), "edited while queued").unwrap();
        state.set_phase(Phase::PrepareIsolation);
        state.save(&paths).unwrap();
        let rec = create_worktree(&root, "runqueue", "impl", None).unwrap();
        state.set_phase(Phase::AwaitingShipConfirm);
        state.save(&paths).unwrap();
        assert_eq!(drift(&paths, "runqueue"), None);
        remove_worktree(&root, &rec).unwrap();
    }

    /// Porcelain's leading space is part of the status code. Trimmed off the first line
    /// only, an unchanged ` M` entry read as both removed and added.
    #[test]
    fn drift_lists_only_what_changed() {
        use crate::state::Phase;
        let tmp = tempdir().unwrap();
        let (root, _wt, _) = repo_with_linked_worktree(tmp.path());
        std::fs::write(root.join("A.txt"), "a").unwrap();
        fixture_git(&root, &["add", "A.txt"]);
        fixture_git(&root, &["commit", "-q", "-m", "a"]);
        std::fs::write(root.join("README"), "dirty before the run").unwrap();
        let paths = SparPaths::new(&root);
        let mut state = RunState::new("runlist", crate::cli::WorkflowKind::Loop, root.clone());
        state.set_phase(Phase::Dispatch);
        state.save(&paths).unwrap();
        std::fs::write(root.join("A.txt"), "changed by a slot").unwrap();
        state.set_phase(Phase::Failed);
        state.save(&paths).unwrap();
        let hit = drift(&paths, "runlist").expect("drift reported");
        assert!(hit.contains("( M A.txt)"), "{hit}");
        assert!(!hit.contains("README"), "{hit}");
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

    fn restore_fixture(tmp: &Path) -> (PathBuf, String) {
        let dir = tmp.join("wt");
        std::fs::create_dir_all(&dir).unwrap();
        fixture_git(&dir, &["init", "-q"]);
        fixture_git(&dir, &["config", "user.email", "t@t.com"]);
        fixture_git(&dir, &["config", "user.name", "t"]);
        fixture_git(&dir, &["checkout", "-q", "-b", "spar/r1/impl"]);
        std::fs::write(dir.join("a"), "a").unwrap();
        fixture_git(&dir, &["add", "a"]);
        fixture_git(&dir, &["commit", "-q", "-m", "a"]);
        let base = fixture_git(&dir, &["rev-parse", "HEAD"]);
        (dir, base)
    }

    fn commit_file(dir: &Path, name: &str) -> String {
        std::fs::write(dir.join(name), name).unwrap();
        fixture_git(dir, &["add", name]);
        fixture_git(dir, &["commit", "-q", "-m", name]);
        fixture_git(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn restore_branch_is_silent_when_already_on_the_branch() {
        let tmp = tempdir().unwrap();
        let (dir, _) = restore_fixture(tmp.path());
        assert_eq!(restore_branch(&dir, "spar/r1/impl", None), None);
    }

    /// #158: a reviewer committed on its own branch in the implementer's worktree. The
    /// run's branch is fast-forwarded to that HEAD and checked out again.
    #[test]
    fn restore_branch_fast_forwards_and_checks_out_the_branch() {
        let tmp = tempdir().unwrap();
        let (dir, _) = restore_fixture(tmp.path());
        fixture_git(&dir, &["checkout", "-q", "-b", "review-x"]);
        let head = commit_file(&dir, "reviewed");

        let note = restore_branch(&dir, "spar/r1/impl", None).expect("must report the repair");
        assert!(note.contains("review-x"), "{note}");
        assert_eq!(named_head(&dir).as_deref(), Some("spar/r1/impl"));
        assert_eq!(fixture_git(&dir, &["rev-parse", "spar/r1/impl"]), head);
    }

    /// Diverged: repairing would drop the branch's own commits, so it is only reported.
    #[test]
    fn restore_branch_leaves_a_diverged_worktree_alone() {
        let tmp = tempdir().unwrap();
        let (dir, base) = restore_fixture(tmp.path());
        let impl_tip = commit_file(&dir, "impl-work");
        fixture_git(&dir, &["checkout", "-q", "-b", "review-x", &base]);
        let side = commit_file(&dir, "side");

        let note = restore_branch(&dir, "spar/r1/impl", None).expect("must warn");
        assert!(note.contains("does not contain"), "{note}");
        assert_eq!(named_head(&dir).as_deref(), Some("review-x"));
        assert_eq!(fixture_git(&dir, &["rev-parse", "HEAD"]), side);
        assert_eq!(fixture_git(&dir, &["rev-parse", "spar/r1/impl"]), impl_tip);
    }

    #[test]
    fn restore_branch_fast_forwards_from_a_detached_head() {
        let tmp = tempdir().unwrap();
        let (dir, _) = restore_fixture(tmp.path());
        fixture_git(&dir, &["checkout", "-q", "--detach"]);
        let head = commit_file(&dir, "detached-work");

        let note = restore_branch(&dir, "spar/r1/impl", None).expect("must report the repair");
        assert!(note.contains("detached HEAD"), "{note}");
        assert_eq!(named_head(&dir).as_deref(), Some("spar/r1/impl"));
        assert_eq!(fixture_git(&dir, &["rev-parse", "spar/r1/impl"]), head);
    }

    /// Moving a ref another worktree has checked out would rewrite that worktree's HEAD
    /// under it, and the checkout here would then fail anyway.
    #[test]
    fn restore_branch_leaves_a_branch_checked_out_elsewhere_alone() {
        let tmp = tempdir().unwrap();
        let (dir, base) = restore_fixture(tmp.path());
        fixture_git(&dir, &["checkout", "-q", "-b", "review-x"]);
        commit_file(&dir, "reviewed");
        let other = tmp.path().join("other");
        fixture_git(
            &dir,
            &[
                "worktree",
                "add",
                "-q",
                other.to_str().unwrap(),
                "spar/r1/impl",
            ],
        );

        let note = restore_branch(&dir, "spar/r1/impl", None).expect("must warn");
        assert!(note.contains("is checked out in"), "{note}");
        assert!(!note.contains("fast-forwarded"), "{note}");
        assert_eq!(fixture_git(&dir, &["rev-parse", "spar/r1/impl"]), base);
        assert_eq!(named_head(&dir).as_deref(), Some("review-x"));
    }

    /// The branch still at the run base: HEAD on main (ahead of base) is not the run's
    /// work, so the branch must not be fast-forwarded onto it.
    #[test]
    fn restore_branch_leaves_a_branch_still_at_base_alone() {
        let tmp = tempdir().unwrap();
        let (dir, base) = restore_fixture(tmp.path());
        fixture_git(&dir, &["checkout", "-q", "-b", "main"]);
        commit_file(&dir, "unrelated");

        let note = restore_branch(&dir, "spar/r1/impl", Some(&base)).expect("must warn");
        assert!(note.contains("never advanced past the run base"), "{note}");
        assert_eq!(fixture_git(&dir, &["rev-parse", "spar/r1/impl"]), base);
        assert_eq!(named_head(&dir).as_deref(), Some("main"));
    }
}
