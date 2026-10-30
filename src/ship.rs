use crate::config::Config;
use crate::executor;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::state::{Phase, RunState};
use crate::worktree;
use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;

pub fn confirm_ship(paths: &SparPaths, run_id: &str, json: bool) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if !matches!(
        state.phase,
        Phase::AwaitingShipConfirm | Phase::AwaitingWinnerConfirm
    ) {
        bail!(
            "run {run_id} not ready for ship confirm (phase={:?})",
            state.phase
        );
    }
    if state.phase == Phase::AwaitingWinnerConfirm {
        if state.winner_slot.is_none() {
            bail!("confirm a winner first");
        }
        state.gates.winner_confirmed = state.winner_slot.clone();
    }
    state.gates.ship_confirmed = true;
    state.set_phase(Phase::AwaitingShipConfirm);
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("ship confirmed for {run_id}; run: spar ship {run_id}");
    }
    Ok(ExitCode::Success)
}

/// PR target branch: `--base` wins outright (the operator's call, passed through even if
/// `gh` will reject it). Otherwise the run's own base, but only when it names a branch
/// origin actually has — a sha, a detached HEAD, a tag, or an unpushed branch is not a
/// PR target, and falling through to `gh`'s default is better than a failed ship.
fn pr_base(state: &RunState, cwd: &std::path::Path, override_base: Option<&str>) -> Option<String> {
    if let Some(b) = override_base {
        return Some(b.to_string());
    }
    let reference = state.base_ref.as_deref()?;
    let branch = reference.strip_prefix("origin/").unwrap_or(reference);
    if branch == "HEAD" || Some(branch) == state.base_commit.as_deref() {
        return None;
    }
    // `--base v1.2` resolves through the tag when the run is created (git prefers
    // refs/tags), and a same-named branch on origin is a different commit. Veto only when
    // the name is a tag and *not* also a local branch: a branch base that merely shares a
    // name with a tag is still a fine PR target, whatever the branch tip has moved to.
    if exact_ref(cwd, &format!("refs/tags/{branch}"))
        && !exact_ref(cwd, &format!("refs/heads/{branch}"))
    {
        return None;
    }
    remote_has_branch(cwd, branch).then(|| branch.to_string())
}

/// Local remote-tracking refs only — deliberately no `ls-remote`. Ship must not block on
/// the network here: it runs on the `--dry-run` path too, and a credential prompt or an
/// unreachable host would hang the command with its output already nulled. A stale
/// mirror is the cost; `git fetch --prune` is the cure.
fn remote_has_branch(cwd: &std::path::Path, branch: &str) -> bool {
    exact_ref(cwd, &format!("refs/remotes/origin/{branch}"))
}

/// `show-ref`, not `rev-parse`: `rev-parse refs/remotes/origin/main~1` happily resolves,
/// which would turn a run based on `--base main~1` into `gh pr create --base main~1`.
fn exact_ref(cwd: &std::path::Path, git_ref: &str) -> bool {
    git_ok(cwd, &["show-ref", "--verify", "--quiet", git_ref])
}

/// PR title from the run's task. A `--spec` / `--brief` task is a whole markdown
/// document, so the first 72 characters of it are the heading marker, the heading, and
/// the start of the first paragraph, cut mid-word — which is what ends up naming the PR.
/// Take the first non-blank line instead, strip its heading marker, and clamp on a word
/// boundary. An inline `-t` task that already fits is passed through unchanged.
fn pr_title(task: Option<&str>) -> String {
    const FALLBACK: &str = "spar change";
    const MAX: usize = 72;
    let Some(line) = task.and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty())) else {
        return FALLBACK.into();
    };
    // Only the leading marker: a `#` inside the text (an issue reference, a fragment) is
    // part of the title and must survive.
    let line = line.trim_start_matches('#').trim();
    if line.is_empty() {
        return FALLBACK.into();
    }
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX - 1).collect();
    let head = match cut.rsplit_once(' ') {
        // A single word longer than the budget has no boundary to cut on; the hard cut
        // is still better than a title that blows the limit.
        Some((h, _)) if !h.trim().is_empty() => h.trim_end(),
        _ => cut.as_str(),
    };
    format!("{head}…")
}

fn git_ok(cwd: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn ship(
    paths: &SparPaths,
    cfg: &Config,
    run_id: &str,
    json: bool,
    base: Option<&str>,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if !matches!(
        state.phase,
        Phase::AwaitingShipConfirm | Phase::Shipping | Phase::Done
    ) && !state.gates.ship_confirmed
    {
        bail!(
            "run {run_id} is not ready to ship (phase={:?})",
            state.phase
        );
    }
    if !state.gates.ship_confirmed && !cfg.ship.auto_confirm && !cfg.auto_ship() {
        if state.phase == Phase::AwaitingShipConfirm {
            state.save(paths)?;
            if json {
                executor::emit_run_json(&state)?;
            } else {
                eprintln!("ship requires confirm: spar ship {run_id} --confirm");
            }
            return Ok(ExitCode::HumanGate);
        }
        bail!(
            "run {run_id} is not ready to ship (phase={:?}); approve/confirm first",
            state.phase
        );
    }
    if cfg.auto_ship() || cfg.ship.auto_confirm {
        state.gates.ship_confirmed = true;
    }

    // Determine branch/worktree to push
    let (branch, cwd) = select_branch_cwd(&state)?;
    let pr_base = pr_base(&state, &cwd, base);
    let base_arg = match &pr_base {
        Some(b) => format!(" --base {}", shell_single_quote(b)),
        None => String::new(),
    };
    if pr_base.is_none() {
        if let Some(r) = &state.base_ref {
            eprintln!("note: run base {r} is not a branch on origin; PR targets the repo default");
        }
    }
    if state.dry_run {
        let push_cmd = format!(
            "git -C {} push --force-with-lease=refs/heads/{branch}:<fetched origin tip> origin HEAD:refs/heads/{branch}",
            cwd.display()
        );
        let pr_cmd = format!(
            "cd {} && gh pr create --draft --head {branch}{base_arg} --title dry-run --body dry-run",
            cwd.display()
        );
        let commands = vec![push_cmd, pr_cmd];
        state.ship_commands = Some(commands.clone());
        std::fs::write(
            paths.artifact(run_id, "ship.md"),
            format!(
                "# Ship (dry-run — not executed)\n\nBranch: `{branch}`\nPR base: `{}`\n\n```\n{}\n```\n",
                pr_base.as_deref().unwrap_or("(repo default)"),
                commands.join("\n")
            ),
        )?;
        state.set_phase(Phase::Done);
        state.save(paths)?;
        if json {
            executor::emit_run_json(&state)?;
        } else {
            println!("dry-run ship: wrote commands to artifacts/ship.md (no push)");
        }
        return Ok(ExitCode::Success);
    }
    let remote = "origin";
    let title = pr_title(state.task.as_deref());

    let push_args = match push_args(&cwd, remote, &branch, state.base_commit.as_deref()) {
        Ok(args) => args,
        Err(e) => {
            state.error = Some(format!("ship refused: {e}"));
            state.save(paths)?;
            if json {
                executor::emit_run_json(&state)?;
            } else {
                eprintln!("ship refused: {e}");
            }
            return Ok(ExitCode::Failure);
        }
    };
    let push_cmd = format!("git -C {} {}", cwd.display(), push_args.join(" "));
    let pr_cmd = format!(
        "cd {} && gh pr create --draft --head {branch}{base_arg} --title {} --body {}",
        cwd.display(),
        shell_single_quote(&title),
        shell_single_quote(&format!("spar run `{}`", state.id))
    );

    let commands = vec![push_cmd, pr_cmd];
    state.ship_commands = Some(commands.clone());
    state.set_phase(Phase::Shipping);
    state.save(paths)?;

    // Prefer printing if gh/git might fail; try execute
    let mut executed = Vec::new();
    let mut failed = false;

    let push_status = Command::new("git")
        .args(&push_args)
        .current_dir(&cwd)
        .status();
    match push_status {
        Ok(s) if s.success() => executed.push(format!("pushed {branch}")),
        Ok(s) => {
            failed = true;
            executed.push(format!("push failed (exit {:?})", s.code()));
        }
        Err(e) => {
            failed = true;
            executed.push(format!("push error: {e}"));
        }
    }

    if !failed {
        let mut args: Vec<String> = vec![
            "pr".into(),
            "create".into(),
            // Ship opens a *draft* PR and never merges: the invariant is in
            // `skills/core.md`, `docs/PRODUCT.md` and `docs/architecture-dual-backend.md`,
            // but nothing passed the flag, so every shipped PR opened ready for review.
            "--draft".into(),
            "--head".into(),
            branch.clone(),
            "--title".into(),
            title.clone(),
            "--body".into(),
            format!("Shipped by spar run {}", state.id),
        ];
        if let Some(b) = &pr_base {
            args.push("--base".into());
            args.push(b.clone());
        }
        let pr = Command::new("gh").args(&args).current_dir(&cwd).output();
        match pr {
            Ok(o) if o.status.success() => {
                executed.push(String::from_utf8_lossy(&o.stdout).trim().to_string());
            }
            Ok(o) => {
                // print commands instead of hard-fail if pr exists
                let err = String::from_utf8_lossy(&o.stderr);
                executed.push(format!("gh pr create: {err}"));
                failed = true;
            }
            Err(e) => {
                executed.push(format!("gh missing or failed: {e}"));
                failed = true;
            }
        }
    }

    std::fs::write(
        paths.artifact(run_id, "ship.md"),
        format!(
            "# Ship\n\nBranch: `{branch}`\nPR base: `{}`\nCwd: `{}`\n\n## Commands\n```\n{}\n```\n\n## Result\n{}\n",
            pr_base.as_deref().unwrap_or("(repo default)"),
            cwd.display(),
            commands.join("\n"),
            executed.join("\n")
        ),
    )?;

    if failed {
        // leave commands for human; still not merge
        state.set_phase(Phase::AwaitingShipConfirm);
        state.error = Some("ship partial failure; see artifacts/ship.md".into());
        state.save(paths)?;
        if json {
            executor::emit_run_json(&state)?;
        } else {
            println!("ship did not fully succeed; commands:");
            for c in &commands {
                println!("  {c}");
            }
        }
        return Ok(ExitCode::Failure);
    }

    state.error = None;
    state.set_phase(Phase::Done);
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("shipped branch {branch} (PR created or updated). Never merged.");
    }
    Ok(ExitCode::Success)
}

/// `git push` arguments that ship the worktree's HEAD (not whatever the branch name
/// points at) to `refs/heads/<branch>`, leased on the remote tip just fetched. Refuses
/// rather than rewind a remote branch (O10): a lease against the local tracking ref alone
/// passes after any fetch, which is how a hand-pushed branch got reset.
fn push_args(cwd: &Path, remote: &str, branch: &str, base: Option<&str>) -> Result<Vec<String>> {
    let on_branch = check_head_branch(cwd, branch, base)?;
    let lease = remote_tip(cwd, remote, branch)?;
    let mut args = vec![
        "push".to_string(),
        format!(
            "--force-with-lease=refs/heads/{branch}:{}",
            lease.unwrap_or_default()
        ),
    ];
    // `-u` binds the *checked-out* branch, which is not ours when HEAD is elsewhere.
    if on_branch {
        args.push("-u".into());
    }
    args.push(remote.into());
    args.push(format!("HEAD:refs/heads/{branch}"));
    Ok(args)
}

/// `true` when HEAD is checked out on `branch`. A worktree left on another branch (a
/// reviewer's, or detached) is shipped from HEAD only when HEAD contains `branch` and
/// `branch` has moved past the run's base; otherwise any branch ahead of base would do.
fn check_head_branch(cwd: &Path, branch: &str, base: Option<&str>) -> Result<bool> {
    let current = worktree::named_head(cwd);
    if current.as_deref() == Some(branch) {
        return Ok(true);
    }
    let shown = current.unwrap_or_else(|| "a detached HEAD".into());
    let tip = worktree::git_out(
        cwd,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    );
    if tip.is_some() && tip.as_deref() == base {
        bail!(
            "worktree {} is on {shown} and {branch} never advanced past the run base; check out the work to ship in {branch} and retry",
            cwd.display()
        );
    }
    if !git_ok(
        cwd,
        &[
            "merge-base",
            "--is-ancestor",
            &format!("refs/heads/{branch}"),
            "HEAD",
        ],
    ) {
        bail!(
            "worktree {} is on {shown}, which does not contain {branch}; check out the work to ship in {branch} and retry",
            cwd.display()
        );
    }
    eprintln!(
        "note: worktree {} is on {shown}, ahead of {branch}; shipping its HEAD",
        cwd.display()
    );
    Ok(false)
}

/// The remote's current tip of `branch`, fetched, or `None` when the branch does not
/// exist there. Errors when that tip is not contained in HEAD.
fn remote_tip(cwd: &Path, remote: &str, branch: &str) -> Result<Option<String>> {
    let out = Command::new("git")
        .args([
            "ls-remote",
            "--exit-code",
            remote,
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(cwd)
        .output()?;
    match out.status.code() {
        Some(0) => {}
        Some(2) => return Ok(None),
        _ => bail!(
            "could not read {remote}/{branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
    let tracking = format!("refs/remotes/{remote}/{branch}");
    if !git_ok(
        cwd,
        &[
            "fetch",
            "--quiet",
            remote,
            &format!("+refs/heads/{branch}:{tracking}"),
        ],
    ) {
        bail!("could not fetch {remote}/{branch}");
    }
    let Some(tip) = worktree::git_out(cwd, &["rev-parse", "--verify", &tracking]) else {
        bail!("could not resolve {tracking} after fetch");
    };
    if !git_ok(cwd, &["merge-base", "--is-ancestor", &tip, "HEAD"]) {
        bail!(
            "remote {branch} has commits the run lacks; fast-forward the worktree by hand \
             (git -C {} merge --ff-only {remote}/{branch}) and retry ship",
            cwd.display()
        );
    }
    Ok(Some(tip))
}

fn select_branch_cwd(state: &RunState) -> Result<(String, std::path::PathBuf)> {
    if let Some(winner) = state
        .gates
        .winner_confirmed
        .as_ref()
        .or(state.winner_slot.as_ref())
    {
        if let Some(wt) = state.worktrees.iter().find(|w| w.slot_id == *winner) {
            return Ok((wt.branch.clone(), wt.path.clone()));
        }
    }
    // implementer worktree
    if let Some(imp) = state.slots.iter().find(|s| {
        matches!(
            s.role,
            crate::state::SlotRole::Implementer | crate::state::SlotRole::Peer
        )
    }) {
        if let Some(wt) = state.worktrees.iter().find(|w| w.slot_id == imp.id) {
            return Ok((wt.branch.clone(), wt.path.clone()));
        }
        if let Some(cwd) = &imp.cwd {
            let branch = state
                .worktrees
                .iter()
                .find(|w| w.slot_id == imp.id)
                .map(|w| w.branch.clone())
                .unwrap_or_else(|| format!("spar/{}/{}", state.id, imp.id));
            return Ok((branch, cwd.clone()));
        }
    }
    if let Some(wt) = state.worktrees.first() {
        return Ok((wt.branch.clone(), wt.path.clone()));
    }
    bail!("no worktree/branch available to ship for run {}", state.id);
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::WorkflowKind;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    /// Repo with one commit and a fake `origin/feat` remote-tracking ref (no network).
    /// Loud on failure: a fixture that skipped would let `pr_base` regressions pass.
    fn repo_with_origin_feat(dir: &Path) -> String {
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("README"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/feat", &head]);
        head
    }

    fn state_with_base(reference: &str, commit: &str) -> RunState {
        let mut s = RunState::new("r1", WorkflowKind::Loop, PathBuf::from("/x"));
        s.base_ref = Some(reference.into());
        s.base_commit = Some(commit.into());
        s
    }

    #[test]
    fn pr_base_prefers_override_then_remote_branch() {
        let tmp = tempdir().unwrap();
        let head = repo_with_origin_feat(tmp.path());

        let state = state_with_base("feat", &head);
        assert_eq!(
            pr_base(&state, tmp.path(), Some("release/1")),
            Some("release/1".into()),
            "explicit --base is passed through untouched"
        );
        assert_eq!(pr_base(&state, tmp.path(), None), Some("feat".into()));

        let remote_form = state_with_base("origin/feat", &head);
        assert_eq!(pr_base(&remote_form, tmp.path(), None), Some("feat".into()));
    }

    /// `--base main~1` is documented as supported, and `refs/remotes/origin/main~1`
    /// resolves through `rev-parse` — targeting a PR at a rev-expression `gh` rejects.
    #[test]
    fn pr_base_none_for_a_rev_expression_base() {
        let tmp = tempdir().unwrap();
        repo_with_origin_feat(tmp.path());
        // A second commit, so `refs/remotes/origin/feat~1` is a *resolvable* rev-spec —
        // otherwise the probe rejects it for the wrong reason and proves nothing.
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(tmp.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        std::fs::write(tmp.path().join("second"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "second"]);
        let head = git(&["rev-parse", "HEAD"]);
        git(&["update-ref", "refs/remotes/origin/feat", &head]);
        assert!(
            !git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/remotes/origin/feat~1"
            ])
            .is_empty(),
            "fixture must make the rev-spec resolvable"
        );

        let state = state_with_base("feat~1", &head);
        assert_eq!(pr_base(&state, tmp.path(), None), None);
    }

    #[test]
    fn pr_base_none_for_sha_or_local_only_branch() {
        let tmp = tempdir().unwrap();
        let head = repo_with_origin_feat(tmp.path());
        assert_eq!(
            pr_base(&state_with_base(&head, &head), tmp.path(), None),
            None,
            "a detached base is not a PR target"
        );
        assert_eq!(
            pr_base(&state_with_base("local-only", &head), tmp.path(), None),
            None,
            "unpushed base falls through to the repo default"
        );
        let no_base = RunState::new("r1", WorkflowKind::Loop, PathBuf::from("/x"));
        assert_eq!(pr_base(&no_base, tmp.path(), None), None);
    }

    /// The bug this replaced: `state.task.chars().take(72)` over a `--spec` brief, which
    /// named a real PR `# Give muse slots a real presence and injection channel via its
    /// hook sys` — marker included, cut mid-word, body text already bleeding in.
    #[test]
    fn pr_title_uses_the_briefs_heading_not_its_first_72_bytes() {
        let brief = "# Give muse slots a real presence and injection channel via its hook system\n\nThree changes to spar's muse adapter: wire muse's lifecycle hooks so a muse slot\n";
        let title = pr_title(Some(brief));
        assert_eq!(
            title,
            "Give muse slots a real presence and injection channel via its hook…"
        );
        assert!(!title.starts_with('#'), "heading marker survived: {title}");
        assert!(!title.contains("Three changes"), "body bled in: {title}");
        assert!(title.chars().count() <= 72, "over budget: {title}");
    }

    #[test]
    fn pr_title_passes_a_short_inline_task_through_untouched() {
        assert_eq!(
            pr_title(Some("fix the flaky quota test")),
            "fix the flaky quota test"
        );
        // Leading blank lines are skipped, not rendered as an empty title.
        assert_eq!(
            pr_title(Some("\n\n  ## Retry muse 404s\n")),
            "Retry muse 404s"
        );
        // A `#` inside the text is part of the title.
        assert_eq!(pr_title(Some("fix #123 off-by-one")), "fix #123 off-by-one");
    }

    #[test]
    fn pr_title_falls_back_when_there_is_no_usable_line() {
        assert_eq!(pr_title(None), "spar change");
        assert_eq!(pr_title(Some("")), "spar change");
        assert_eq!(pr_title(Some("   \n\n")), "spar change");
        assert_eq!(pr_title(Some("###\n")), "spar change");
    }

    /// One unbroken word longer than the budget has no boundary to cut on; it must still
    /// come back inside the limit rather than falling through whole.
    #[test]
    fn pr_title_clamps_a_single_overlong_word() {
        let title = pr_title(Some(&"x".repeat(200)));
        assert!(title.chars().count() <= 72, "over budget: {title}");
        assert!(title.ends_with('\u{2026}'));
    }

    const BRANCH: &str = "spar/r1/impl";

    fn sh(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"))
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = sh(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, name: &str) -> String {
        std::fs::write(dir.join(name), name).unwrap();
        git_in(dir, &["add", name]);
        git_in(dir, &["commit", "-q", "-m", name]);
        git_in(dir, &["rev-parse", "HEAD"])
    }

    /// A bare `origin` and a work repo on `BRANCH` with one commit, nothing pushed yet.
    fn origin_and_work(tmp: &Path) -> (PathBuf, PathBuf) {
        let origin = tmp.join("origin.git");
        let work = tmp.join("work");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git_in(&origin, &["init", "-q", "--bare"]);
        git_in(&work, &["init", "-q"]);
        git_in(&work, &["config", "user.email", "t@t.com"]);
        git_in(&work, &["config", "user.name", "t"]);
        git_in(&work, &["checkout", "-q", "-b", BRANCH]);
        commit(&work, "base");
        git_in(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        (origin, work)
    }

    fn remote_tip_of(origin: &Path) -> String {
        git_in(origin, &["rev-parse", &format!("refs/heads/{BRANCH}")])
    }

    fn run_push(work: &Path, args: &[String]) -> bool {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        sh(work, &args).status.success()
    }

    /// #163: a hand-push to the run's branch must never be rewound by ship.
    #[test]
    fn push_refuses_when_remote_has_commits_the_run_lacks() {
        let tmp = tempdir().unwrap();
        let (origin, work) = origin_and_work(tmp.path());
        git_in(&work, &["push", "-q", "origin", BRANCH]);
        let hand = tmp.path().join("hand");
        git_in(
            tmp.path(),
            &[
                "clone",
                "-q",
                "-b",
                BRANCH,
                origin.to_str().unwrap(),
                "hand",
            ],
        );
        git_in(&hand, &["config", "user.email", "h@h.com"]);
        git_in(&hand, &["config", "user.name", "h"]);
        let hand_tip = commit(&hand, "by-hand");
        git_in(&hand, &["push", "-q", "origin", BRANCH]);
        // The stale local tracking ref is exactly what the old bare lease checked.
        commit(&work, "run-work");

        let err = push_args(&work, "origin", BRANCH, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has commits the run lacks"), "{err}");
        assert_eq!(remote_tip_of(&origin), hand_tip, "remote must be untouched");
    }

    #[test]
    fn push_fast_forwards_a_remote_tip_the_run_contains() {
        let tmp = tempdir().unwrap();
        let (origin, work) = origin_and_work(tmp.path());
        git_in(&work, &["push", "-q", "origin", BRANCH]);
        let pushed = remote_tip_of(&origin);
        let head = commit(&work, "more");

        let args = push_args(&work, "origin", BRANCH, None).unwrap();
        assert!(
            args.contains(&format!("--force-with-lease=refs/heads/{BRANCH}:{pushed}")),
            "lease must name the observed tip: {args:?}"
        );
        assert!(args.contains(&"-u".to_string()));
        assert!(run_push(&work, &args));
        assert_eq!(remote_tip_of(&origin), head);
    }

    /// The lease is explicit, so a push landing between ship's fetch and its push still
    /// fails, and an absent branch must still be absent at push time.
    #[test]
    fn push_lease_fails_when_the_remote_moves_after_the_check() {
        let tmp = tempdir().unwrap();
        let (origin, work) = origin_and_work(tmp.path());
        commit(&work, "run-work");
        let args = push_args(&work, "origin", BRANCH, None).unwrap();
        assert!(args.contains(&format!("--force-with-lease=refs/heads/{BRANCH}:")));

        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        git_in(&other, &["init", "-q"]);
        git_in(&other, &["config", "user.email", "o@o.com"]);
        git_in(&other, &["config", "user.name", "o"]);
        let racer = commit(&other, "racer");
        git_in(
            &other,
            &[
                "push",
                "-q",
                origin.to_str().unwrap(),
                &format!("HEAD:refs/heads/{BRANCH}"),
            ],
        );

        assert!(!run_push(&work, &args), "stale lease must reject the push");
        assert_eq!(remote_tip_of(&origin), racer);
    }

    /// #158: a reviewer left the worktree on its own branch, ahead of the run's branch.
    /// Ship pushes HEAD, not the stale branch ref, and does not rebind HEAD's upstream.
    #[test]
    fn push_ships_head_when_the_worktree_is_ahead_on_another_branch() {
        let tmp = tempdir().unwrap();
        let (origin, work) = origin_and_work(tmp.path());
        git_in(&work, &["checkout", "-q", "-b", "review-x"]);
        let head = commit(&work, "reviewed");

        let args = push_args(&work, "origin", BRANCH, None).unwrap();
        assert!(!args.contains(&"-u".to_string()), "{args:?}");
        assert!(args.contains(&format!("HEAD:refs/heads/{BRANCH}")));
        assert!(run_push(&work, &args));
        assert_eq!(remote_tip_of(&origin), head);
    }

    #[test]
    fn push_refuses_when_head_does_not_contain_the_branch() {
        let tmp = tempdir().unwrap();
        let (_origin, work) = origin_and_work(tmp.path());
        let base = git_in(&work, &["rev-parse", "HEAD"]);
        commit(&work, "impl-work");
        git_in(&work, &["checkout", "-q", "-b", "review-x", &base]);

        let err = push_args(&work, "origin", BRANCH, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("review-x"), "{err}");
        assert!(err.contains(BRANCH), "{err}");
    }

    /// The run branch never moved, so "HEAD contains it" holds for any branch ahead of
    /// base: shipping main's tip as the run's work must be refused.
    #[test]
    fn push_refuses_when_the_branch_never_advanced_past_base() {
        let tmp = tempdir().unwrap();
        let (_origin, work) = origin_and_work(tmp.path());
        let base = git_in(&work, &["rev-parse", "HEAD"]);
        git_in(&work, &["checkout", "-q", "-b", "main"]);
        commit(&work, "unrelated");

        let err = push_args(&work, "origin", BRANCH, Some(&base))
            .unwrap_err()
            .to_string();
        assert!(err.contains("never advanced past the run base"), "{err}");
    }

    #[test]
    fn refused_ship_records_the_error_and_keeps_the_phase() {
        let tmp = tempdir().unwrap();
        let (_origin, work) = origin_and_work(tmp.path());
        let base = git_in(&work, &["rev-parse", "HEAD"]);
        commit(&work, "impl-work");
        git_in(&work, &["checkout", "-q", "-b", "review-x", &base]);

        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let paths = SparPaths::new(&root);
        let mut state = RunState::new("r1", WorkflowKind::Loop, root.clone());
        state.phase = Phase::AwaitingShipConfirm;
        state.gates.ship_confirmed = true;
        state.worktrees.push(crate::state::WorktreeRecord {
            slot_id: "impl".into(),
            path: work.clone(),
            branch: BRANCH.into(),
        });
        state.save(&paths).unwrap();

        let code = ship(&paths, &Config::default(), "r1", true, None).unwrap();
        assert_eq!(code, ExitCode::Failure);
        let after = RunState::load(&paths, "r1").unwrap();
        assert_eq!(after.phase, Phase::AwaitingShipConfirm);
        let err = after.error.unwrap_or_default();
        assert!(err.starts_with("ship refused:"), "{err}");
        assert!(err.contains("review-x"), "{err}");
    }
}
