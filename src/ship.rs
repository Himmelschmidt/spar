use crate::config::Config;
use crate::executor;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::state::{Phase, RunState};
use anyhow::{bail, Result};
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
    // refs/tags), and a same-named branch on origin is then a different commit. Only
    // veto when the name really came from the tag: a branch base that merely shares its
    // name with a local tag is still a fine PR target.
    let tagged = git_ok(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/tags/{branch}"),
        ],
    );
    if tagged
        && !branch_points_at(
            cwd,
            &format!("refs/heads/{branch}"),
            state.base_commit.as_deref(),
        )
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
    git_ok(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ],
    )
}

fn branch_points_at(cwd: &std::path::Path, git_ref: &str, commit: Option<&str>) -> bool {
    let Some(commit) = commit else {
        return false;
    };
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", git_ref])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == commit)
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
            "git -C {} push --force-with-lease -u origin {branch}",
            cwd.display()
        );
        let pr_cmd = format!(
            "cd {} && gh pr create --head {branch}{base_arg} --title dry-run --body dry-run",
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
    let title = state
        .task
        .as_deref()
        .unwrap_or("spar change")
        .chars()
        .take(72)
        .collect::<String>();

    let push_cmd = format!(
        "git -C {} push --force-with-lease -u {remote} {branch}",
        cwd.display()
    );
    let pr_cmd = format!(
        "cd {} && gh pr create --head {branch}{base_arg} --title {} --body {}",
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

    // Never bare force-push — only --force-with-lease
    let push_status = Command::new("git")
        .args(["push", "--force-with-lease", "-u", remote, &branch])
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

    state.set_phase(Phase::Done);
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("shipped branch {branch} (PR created or updated). Never merged.");
    }
    Ok(ExitCode::Success)
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
}
