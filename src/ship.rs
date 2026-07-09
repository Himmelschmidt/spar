use crate::config::Config;
use crate::executor;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::state::{Phase, RunState};
use anyhow::{bail, Result};
use std::process::Command;

pub fn confirm_ship(paths: &SparPaths, run_id: &str, json: bool) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    state.gates.ship_confirmed = true;
    if state.phase == Phase::AwaitingShipConfirm || state.phase == Phase::AwaitingWinnerConfirm {
        // allow confirm from winner gate only if winner already set
        if state.phase == Phase::AwaitingWinnerConfirm {
            if state.winner_slot.is_none() {
                bail!("confirm a winner first");
            }
            state.gates.winner_confirmed = state.winner_slot.clone();
        }
        state.set_phase(Phase::AwaitingShipConfirm);
    }
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("ship confirmed for {run_id}; run: spar ship {run_id}");
    }
    Ok(ExitCode::Success)
}

pub fn ship(paths: &SparPaths, cfg: &Config, run_id: &str, json: bool) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if !state.gates.ship_confirmed && !cfg.ship.auto_confirm {
        if state.phase == Phase::AwaitingShipConfirm || state.phase == Phase::AwaitingWinnerConfirm
        {
            // set gate and refuse to ship
            state.set_phase(Phase::AwaitingShipConfirm);
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

    // Determine branch/worktree to push
    let (branch, cwd) = select_branch_cwd(&state)?;
    if state.dry_run {
        let push_cmd = format!(
            "git -C {} push --force-with-lease -u origin {branch}",
            cwd.display()
        );
        let pr_cmd = format!(
            "cd {} && gh pr create --head {branch} --title dry-run --body dry-run",
            cwd.display()
        );
        let commands = vec![push_cmd, pr_cmd];
        state.ship_commands = Some(commands.clone());
        std::fs::write(
            paths.artifact(run_id, "ship.md"),
            format!(
                "# Ship (dry-run — not executed)\n\nBranch: `{branch}`\n\n```\n{}\n```\n",
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
        "cd {} && gh pr create --head {branch} --title {} --body {}",
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
        let pr = Command::new("gh")
            .args([
                "pr",
                "create",
                "--head",
                &branch,
                "--title",
                &title,
                "--body",
                &format!("Shipped by spar run {}", state.id),
            ])
            .current_dir(&cwd)
            .output();
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
            "# Ship\n\nBranch: `{branch}`\nCwd: `{}`\n\n## Commands\n```\n{}\n```\n\n## Result\n{}\n",
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
