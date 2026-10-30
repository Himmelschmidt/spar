//! Refs outside `refs/heads` that keep a slot's work past its worktree (O112, O113).
//!
//! `refs/spar-checkpoint/<run>/<slot>` holds the implementer's end state after every
//! dispatch, uncommitted edits included, in the local repo, and on origin only with
//! `[checkpoint] push = true` (anyone who can read origin can then fetch it).
//! `refs/spar-archive/<run>/<slot>` holds what `cleanup --force` was about to delete. Neither is a branch, so no PR targets
//! them and `spar/<run>/<slot>` short names never resolve to them.

use crate::paths::SparPaths;
use crate::util::sanitize_slot;
use crate::worktree::git_out;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A push that cannot reach its remote (no network, a hung ssh) must not hold a round
/// hostage. Long enough for a slow link, short enough that an offline box loses a
/// minute per round rather than the TCP timeout.
const PUSH_TIMEOUT: Duration = Duration::from_secs(60);

const SNAPSHOT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "spar"),
    ("GIT_AUTHOR_EMAIL", "spar@localhost"),
    ("GIT_COMMITTER_NAME", "spar"),
    ("GIT_COMMITTER_EMAIL", "spar@localhost"),
];

pub fn checkpoint_ref(run_id: &str, slot_id: &str) -> String {
    format!("refs/spar-checkpoint/{run_id}/{}", sanitize_slot(slot_id))
}

pub fn archive_ref(run_id: &str, slot_id: &str) -> String {
    format!("refs/spar-archive/{run_id}/{}", sanitize_slot(slot_id))
}

/// A commit holding `dir`'s whole working tree as `git add -A` sees it, parented on HEAD
/// and on each of `extra_parents` that differs from it. Built through a throwaway index
/// and `commit-tree`, so the worktree's own index, HEAD and branch never move.
///
/// HEAD itself when the tree is clean and there is no other parent to carry.
pub fn snapshot(dir: &Path, message: &str, extra_parents: &[&str]) -> Option<String> {
    let head = git_out(dir, &["rev-parse", "--verify", "HEAD"])?;
    let git_dir = git_out(dir, &["rev-parse", "--absolute-git-dir"])?;
    let index = Path::new(&git_dir).join(format!("spar-snapshot.{}.index", std::process::id()));
    let tree = git_with_index(dir, &index, &["read-tree", "HEAD"])
        .and_then(|_| git_with_index(dir, &index, &["add", "-A"]))
        .and_then(|_| git_with_index(dir, &index, &["write-tree"]));
    let _ = std::fs::remove_file(&index);
    let tree = tree?;
    let mut parents = vec![head.clone()];
    for p in extra_parents {
        if !parents.iter().any(|q| q == p) {
            parents.push((*p).to_string());
        }
    }
    let head_tree = git_out(dir, &["rev-parse", "HEAD^{tree}"])?;
    if tree == head_tree && parents.len() == 1 {
        return Some(head);
    }
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(["commit-tree", &tree]);
    for p in &parents {
        cmd.args(["-p", p]);
    }
    cmd.args(["-m", message]).envs(SNAPSHOT_IDENTITY);
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!commit.is_empty()).then_some(commit)
}

fn git_with_index(dir: &Path, index: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_INDEX_FILE", index)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Point `reference` at `commit` in the local repo. Refs outside `refs/heads` are shared
/// by every linked worktree, so this survives the worktree's removal.
pub fn record(dir: &Path, reference: &str, commit: &str) -> bool {
    git_out(dir, &["update-ref", reference, commit]).is_some()
}

/// Push `commit` to `reference` on origin. Forced: the ref is spar's own and a WIP
/// snapshot is never on the branch the next round builds on, so successive checkpoints
/// do not fast-forward. `Err` names why, for a warning; it never fails a run.
pub fn push(dir: &Path, reference: &str, commit: &str) -> Result<(), String> {
    if git_out(dir, &["remote", "get-url", "origin"]).is_none() {
        return Err("no origin remote".into());
    }
    git_unattended(
        dir,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("+{commit}:{reference}"),
        ],
        PUSH_TIMEOUT,
    )
}

/// Run a git command that talks to a remote without ever waiting on a person or a dead
/// network: no stdin, no terminal or askpass prompt, ssh in batch mode (appended to the
/// operator's own ssh command), and the whole process group (pre-push hooks included,
/// which are kept) killed at `timeout`.
pub fn git_unattended(dir: &Path, args: &[&str], timeout: Duration) -> Result<(), String> {
    let ssh = std::env::var("GIT_SSH_COMMAND")
        .ok()
        .filter(|c| !c.trim().is_empty())
        .or_else(|| git_out(dir, &["config", "core.sshCommand"]).filter(|c| !c.is_empty()))
        .unwrap_or_else(|| "ssh".into());
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", format!("{ssh} -o BatchMode=yes"))
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("git {}: {e}", args[0]))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => return Err(format!("git {} exited {s}", args[0])),
            Ok(None) if started.elapsed() >= timeout => {
                crate::process::terminate_tree(child.id(), true);
                let _ = child.wait();
                return Err(format!(
                    "git {} timed out after {}s",
                    args[0],
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("git {}: {e}", args[0])),
        }
    }
}

/// Checkpoint an implementer's worktree after a dispatch: every commit plus, when the
/// slot left a dirty tree, a WIP commit on top of it. `died` carries the reason when the
/// slot is about to be marked failed. Best-effort throughout: a checkpoint that cannot be
/// taken or pushed is a warning and an event, never a failed run.
pub fn save_slot(
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    cwd: &Path,
    died: Option<&str>,
) -> Option<String> {
    if !is_slot_worktree(paths, cwd) {
        return None;
    }
    let message = match died {
        Some(why) => format!(
            "spar WIP checkpoint: run {run_id} slot {slot_id} died\n\n{}",
            first_line(why)
        ),
        None => format!("spar checkpoint: run {run_id} slot {slot_id}"),
    };
    let reference = checkpoint_ref(run_id, slot_id);
    let note = match snapshot(cwd, &message, &[]) {
        None => format!("checkpoint {reference} not taken: git could not snapshot the worktree"),
        Some(commit) if !record(cwd, &reference, &commit) => {
            format!("checkpoint {reference} not taken: could not update the local ref")
        }
        Some(commit) => {
            let short: String = commit.chars().take(8).collect();
            let publish = crate::config::Config::for_run(paths, run_id)
                .map(|c| c.checkpoint.push)
                .unwrap_or(false);
            if !publish {
                format!("checkpoint {short} kept locally at {reference}")
            } else {
                match push(cwd, &reference, &commit) {
                    Ok(()) => format!("checkpoint {short} pushed to origin {reference}"),
                    Err(e) => {
                        eprintln!("warning: checkpoint {reference} kept locally only: {e}");
                        format!("checkpoint {short} kept locally at {reference}; not pushed: {e}")
                    }
                }
            }
        }
    };
    let _ = crate::events::append(
        paths,
        run_id,
        &crate::events::Event::slot_note(slot_id, &note),
    );
    Some(note)
}

/// Only a slot's own linked worktree is checkpointed. A dry-run scratch dir under `.spar/`
/// sits inside the project repo, so git would answer for the operator's checkout, and an
/// un-isolated slot running in the project root shares that checkout's unrelated edits.
fn is_slot_worktree(paths: &SparPaths, cwd: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let here = canon(cwd);
    if here.starts_with(canon(&paths.root)) || here == canon(&paths.project_root) {
        return false;
    }
    git_out(cwd, &["rev-parse", "--show-toplevel"]).is_some_and(|t| canon(Path::new(&t)) == here)
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or_default();
    line.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A project with a bare origin and one slot worktree on `spar/r1/impl`.
    fn project_with_origin(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let origin = tmp.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare"]);
        let proj = tmp.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        git(&proj, &["init", "-q"]);
        git(&proj, &["config", "user.email", "t@t"]);
        git(&proj, &["config", "user.name", "t"]);
        std::fs::write(proj.join("a.txt"), "a\n").unwrap();
        git(&proj, &["add", "."]);
        git(&proj, &["commit", "-qm", "init"]);
        git(
            &proj,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        let wt = tmp.join("proj-spar-r1-impl");
        git(
            &proj,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "spar/r1/impl",
                wt.to_str().unwrap(),
            ],
        );
        (proj, wt, origin)
    }

    #[test]
    fn snapshot_of_a_clean_tree_is_head() {
        let tmp = tempdir().unwrap();
        let (_, wt, _) = project_with_origin(tmp.path());
        let head = git(&wt, &["rev-parse", "HEAD"]);
        assert_eq!(snapshot(&wt, "x", &[]).as_deref(), Some(head.as_str()));
    }

    /// #156: the dirty tree, untracked files included, lands in a commit on top of HEAD
    /// while the worktree's branch, index and files stay exactly as the agent left them.
    #[test]
    fn snapshot_captures_a_dirty_tree_without_touching_branch_or_index() {
        let tmp = tempdir().unwrap();
        let (_, wt, _) = project_with_origin(tmp.path());
        let head = git(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        git(&wt, &["add", "a.txt"]);
        let status_before = git(&wt, &["status", "--porcelain"]);

        let snap = snapshot(&wt, "wip", &[]).expect("snapshot");
        assert_ne!(snap, head);
        assert_eq!(git(&wt, &["rev-parse", &format!("{snap}^")]), head);
        assert_eq!(git(&wt, &["show", &format!("{snap}:a.txt")]), "edited");
        assert_eq!(git(&wt, &["show", &format!("{snap}:new.txt")]), "new");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]), head, "HEAD moved");
        assert_eq!(
            git(&wt, &["rev-parse", "spar/r1/impl"]),
            head,
            "branch moved"
        );
        assert_eq!(
            git(&wt, &["status", "--porcelain"]),
            status_before,
            "index or tree changed"
        );
    }

    /// Freeze `[checkpoint] push = true` into the run's config, as a run created with it.
    fn run_with_push(proj: &Path) -> SparPaths {
        let paths = SparPaths::new(proj);
        paths.ensure_run_dirs("r1").unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.checkpoint.push = true;
        cfg.save_snapshot(&paths, "r1").unwrap();
        paths
    }

    /// Review of #156: spar's own origin is public, so publishing work in progress is the
    /// operator's call. By default the checkpoint is local and origin gets nothing.
    #[test]
    fn save_slot_keeps_the_checkpoint_local_unless_push_is_enabled() {
        let tmp = tempdir().unwrap();
        let (proj, wt, origin) = project_with_origin(tmp.path());
        std::fs::write(wt.join("half.rs"), "fn half() {}\n").unwrap();
        let paths = SparPaths::new(&proj);
        paths.ensure_run_dirs("r1").unwrap();

        let note = save_slot(&paths, "r1", "impl", &wt, Some("exit 137")).unwrap();
        assert!(note.contains("kept locally"), "{note}");
        let local = git(&proj, &["rev-parse", "refs/spar-checkpoint/r1/impl"]);
        assert_eq!(
            git(&proj, &["show", &format!("{local}:half.rs")]),
            "fn half() {}"
        );
        assert!(
            git(&origin, &["for-each-ref"]).is_empty(),
            "nothing published"
        );
    }

    #[test]
    fn save_slot_pushes_the_wip_checkpoint_to_origin_when_enabled() {
        let tmp = tempdir().unwrap();
        let (proj, wt, origin) = project_with_origin(tmp.path());
        std::fs::write(wt.join("half.rs"), "fn half() {}\n").unwrap();
        let paths = run_with_push(&proj);

        let note = save_slot(&paths, "r1", "impl", &wt, Some("exit 137\nmore")).unwrap();
        assert!(note.contains("pushed to origin"), "{note}");
        let remote = git(&origin, &["rev-parse", "refs/spar-checkpoint/r1/impl"]);
        assert_eq!(
            git(&origin, &["show", &format!("{remote}:half.rs")]),
            "fn half() {}"
        );
        assert_eq!(
            git(&proj, &["rev-parse", "refs/spar-checkpoint/r1/impl"]),
            remote
        );
        assert!(git(&origin, &["log", "-1", "--format=%B", &remote]).contains("exit 137"));
        assert!(
            git(&origin, &["branch", "--list"]).is_empty(),
            "a checkpoint must never create a branch on origin"
        );
    }

    /// No origin is a warning, never a failure: the checkpoint still lands locally.
    #[test]
    fn save_slot_without_origin_keeps_the_checkpoint_locally() {
        let tmp = tempdir().unwrap();
        let (proj, wt, _) = project_with_origin(tmp.path());
        git(&proj, &["remote", "remove", "origin"]);
        std::fs::write(wt.join("half.rs"), "x\n").unwrap();
        let paths = run_with_push(&proj);

        let note = save_slot(&paths, "r1", "impl", &wt, None).unwrap();
        assert!(note.contains("kept locally"), "{note}");
        git(
            &proj,
            &["rev-parse", "--verify", "refs/spar-checkpoint/r1/impl"],
        );
    }

    /// An unreachable origin is the no-network case: same outcome as no origin.
    #[test]
    fn save_slot_with_an_unreachable_origin_keeps_the_checkpoint_locally() {
        let tmp = tempdir().unwrap();
        let (proj, wt, _) = project_with_origin(tmp.path());
        let gone = tmp.path().join("nowhere.git");
        git(
            &proj,
            &["remote", "set-url", "origin", gone.to_str().unwrap()],
        );
        let paths = run_with_push(&proj);

        let note = save_slot(&paths, "r1", "impl", &wt, None).unwrap();
        assert!(note.contains("not pushed"), "{note}");
    }

    /// A pre-push hook that outlives the timeout (here it forks a sleeper) must die with
    /// git: the whole process group goes, not just the git pid.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_timed_out_push_kills_its_hooks_too() {
        let tmp = tempdir().unwrap();
        let (proj, wt, _) = project_with_origin(tmp.path());
        let pidfile = tmp.path().join("hook.pid");
        let hooks = proj.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-push");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\nsleep 60 &\necho $! > {}\nwait\n",
                pidfile.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let head = git(&wt, &["rev-parse", "HEAD"]);

        let err = git_unattended(
            &wt,
            &[
                "push",
                "--quiet",
                "origin",
                &format!("+{head}:refs/spar-checkpoint/r1/impl"),
            ],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        let gone = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| s.split(") ").nth(1).is_some_and(|r| r.starts_with('Z')))
                .unwrap_or(true)
        });
        assert!(gone, "the hook's child {pid} survived the timeout");
    }

    #[test]
    fn save_slot_never_checkpoints_the_project_root_or_a_scratch_dir() {
        let tmp = tempdir().unwrap();
        let (proj, _, origin) = project_with_origin(tmp.path());
        let paths = SparPaths::new(&proj);
        let scratch = paths.root.join("runs/r1/cwd");
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(save_slot(&paths, "r1", "impl", &proj, None).is_none());
        assert!(save_slot(&paths, "r1", "impl", &scratch, None).is_none());
        assert!(git(&origin, &["for-each-ref"]).is_empty());
    }

    /// The checkpoint ref must not shadow the slot branch: `spar/r1/impl` would resolve
    /// to `refs/spar/r1/impl` before `refs/heads/spar/r1/impl`.
    #[test]
    fn the_checkpoint_ref_never_shadows_the_slot_branch() {
        let tmp = tempdir().unwrap();
        let (_, wt, _) = project_with_origin(tmp.path());
        let head = git(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("x"), "x").unwrap();
        let snap = snapshot(&wt, "wip", &[]).unwrap();
        assert!(record(&wt, &checkpoint_ref("r1", "impl"), &snap));
        assert_eq!(git(&wt, &["rev-parse", "spar/r1/impl"]), head);
    }
}
