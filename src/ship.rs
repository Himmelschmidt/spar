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
/// Take the first non-blank line instead, strip its heading marker, and clamp to `max`
/// characters (`[ship] title_max`, `0` = only GitHub's own limit) on a word boundary. An
/// inline `-t` task that already fits is passed through unchanged.
fn pr_title(task: Option<&str>, max: usize) -> String {
    const FALLBACK: &str = "spar change";
    // GitHub rejects a PR title over 256 characters, whatever the setting says.
    const GITHUB_MAX: usize = 256;
    let max = if max == 0 {
        GITHUB_MAX
    } else {
        max.min(GITHUB_MAX)
    };
    let Some(line) = task.and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty())) else {
        return FALLBACK.into();
    };
    // Only the leading marker: a `#` inside the text (an issue reference, a fragment) is
    // part of the title and must survive.
    let line = line.trim_start_matches('#').trim();
    if line.is_empty() {
        return FALLBACK.into();
    }
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max.saturating_sub(1)).collect();
    let head = match cut.rsplit_once(' ') {
        // A single word longer than the budget has no boundary to cut on; the hard cut
        // is still better than a title that blows the limit.
        Some((h, _)) if !h.trim().is_empty() => h.trim_end(),
        _ => cut.as_str(),
    };
    format!("{head}…")
}

/// GitHub rejects a PR body over 65,536 characters; the margin leaves room for the note.
const PR_BODY_MAX: usize = 65_000;

/// PR body: the shipped slot's summary, the OPEN questions it lists, and each reviewer's
/// final verdict. Questions and verdicts come first so truncation only ever eats the tail
/// of the summary.
fn pr_body(paths: &SparPaths, state: &RunState, branch: &str) -> String {
    let shipped = state
        .worktrees
        .iter()
        .find(|w| w.branch == branch)
        .map(|w| format!("summary-{}.md", crate::util::sanitize_slot(&w.slot_id)));
    let summary = shipped
        .into_iter()
        .chain(["summary-impl.md".to_string()])
        .find_map(|name| {
            let text = std::fs::read_to_string(paths.artifact(&state.id, &name)).ok()?;
            (!text.trim().is_empty()).then_some((name, text))
        });

    let mut body = format!("Shipped by spar run `{}`.\n", state.id);
    if let Some((_, text)) = &summary {
        let questions = open_questions(text);
        if !questions.is_empty() {
            body.push_str("\n## Open questions\n\n");
            for q in questions {
                body.push_str(&q);
                body.push('\n');
            }
        }
    }
    let reviews = review_verdicts(paths, state);
    if !reviews.is_empty() {
        body.push_str("\n## Review\n\n");
        for line in reviews {
            body.push_str(&line);
            body.push('\n');
        }
    }
    body.push_str("\n## Implementer summary\n\n");
    match &summary {
        Some((_, text)) => body.push_str(text.trim()),
        None => body.push_str("_No implementer summary was written._"),
    }
    body.push('\n');

    if body.chars().count() <= PR_BODY_MAX {
        return body;
    }
    let source = summary.map_or_else(|| "artifacts".to_string(), |(name, _)| name);
    let note = format!(
        "\n\n_Truncated to fit GitHub's body limit. Full text: `.spar/runs/{}/artifacts/{source}`._\n",
        state.id
    );
    let mut cut: String = body
        .chars()
        .take(PR_BODY_MAX - note.chars().count())
        .collect();
    cut.push_str(&note);
    cut
}

/// The questions a summary leaves open: every non-blank line under a heading that names
/// them (`## Open questions`, `### OPEN`), plus any line elsewhere that is itself marked
/// `OPEN` (`- OPEN: …`), the way `DECISIONS.md` marks one.
fn open_questions(summary: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in summary.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix('#') {
            let heading = heading
                .trim_matches(['#', ' ', ':', '*'])
                .to_ascii_lowercase();
            in_section = heading == "open" || heading.contains("open question");
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let marked = trimmed
            .trim_start_matches(['-', '*', ' '])
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            == Some("OPEN");
        if in_section || marked {
            out.push(line.trim_end().to_string());
        }
    }
    out
}

/// One line per reviewer slot: the verdict its artifact carries now (the last round it
/// judged) and the commit it named.
fn review_verdicts(paths: &SparPaths, state: &RunState) -> Vec<String> {
    use crate::workflow::review_result::{parse_review, Verdict};
    state
        .slots
        .iter()
        .filter(|s| s.role == crate::state::SlotRole::Reviewer)
        .map(|s| {
            let name = s
                .artifact
                .clone()
                .unwrap_or_else(|| format!("review-{}.md", s.id));
            let review = std::fs::read_to_string(paths.artifact(&state.id, &name))
                .map(|b| parse_review(&b))
                .unwrap_or_default();
            let verdict = match review.verdict {
                Some(Verdict::Approve) => "approve",
                Some(Verdict::RequestChanges) => "request_changes",
                None => "no verdict",
            };
            let commit = review
                .reviewed_commit
                .map(|c| format!(" at `{}`", &c[..c.len().min(8)]))
                .unwrap_or_default();
            format!("- `{}` ({}): {verdict}{commit}", s.id, s.provider)
        })
        .collect()
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
            "cd {} && gh pr create --draft --head {branch}{base_arg} --title dry-run --body dry-run",
            cwd.display()
        );
        let commands = vec![push_cmd, pr_cmd];
        state.ship_commands = Some(commands.clone());
        std::fs::write(
            paths.artifact(run_id, "ship.md"),
            format!(
                "# Ship (dry-run — not executed)\n\nBranch: `{branch}`\nPR base: `{}`\n\n```\n{}\n```\n\n## PR title\n\n{}\n\n## PR body\n\n{}",
                pr_base.as_deref().unwrap_or("(repo default)"),
                commands.join("\n"),
                pr_title(state.task.as_deref(), cfg.ship.title_max),
                pr_body(paths, &state, &branch)
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
    let title = pr_title(state.task.as_deref(), cfg.ship.title_max);
    let body_file = paths.artifact(run_id, "pr-body.md");

    let push_cmd = format!(
        "git -C {} push --force-with-lease -u {remote} {branch}",
        cwd.display()
    );
    let pr_cmd = format!(
        "cd {} && gh pr create --draft --head {branch}{base_arg} --title {} --body-file {}",
        cwd.display(),
        shell_single_quote(&title),
        shell_single_quote(&body_file.display().to_string())
    );

    let commands = vec![push_cmd, pr_cmd];
    std::fs::write(&body_file, pr_body(paths, &state, &branch))?;
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
            // Ship opens a *draft* PR and never merges: the invariant is in
            // `skills/core.md`, `docs/PRODUCT.md` and `docs/architecture-dual-backend.md`,
            // but nothing passed the flag, so every shipped PR opened ready for review.
            "--draft".into(),
            "--head".into(),
            branch.clone(),
            "--title".into(),
            title.clone(),
            "--body-file".into(),
            body_file.display().to_string(),
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

    #[test]
    fn pr_title_clamp_is_configurable_and_zero_never_clamps() {
        let long = format!("# {}\n\nbody", "word ".repeat(30).trim_end());
        assert_eq!(pr_title(Some(&long), 0), "word ".repeat(30).trim_end());
        let huge = "word ".repeat(100);
        for max in [0, 1000] {
            let title = pr_title(Some(&huge), max);
            assert!(
                title.chars().count() <= 256,
                "{max}: {}",
                title.chars().count()
            );
            assert!(title.ends_with('\u{2026}'));
        }
        let title = pr_title(Some(&long), 20);
        assert_eq!(title, "word word word…");
    }

    fn body_state(tmp: &Path) -> (SparPaths, RunState) {
        let paths = SparPaths::new(tmp);
        let mut state = RunState::new("rb1", WorkflowKind::Loop, tmp.to_path_buf());
        paths.ensure_run_dirs(&state.id).unwrap();
        state.worktrees.push(crate::state::WorktreeRecord {
            slot_id: "impl".into(),
            path: tmp.join("wt"),
            branch: "spar/rb1/impl".into(),
        });
        for (id, provider) in [("review-1", "cli:codex"), ("review-2", "cli:claude")] {
            state.slots.push(crate::executor::init_slot(
                id,
                provider,
                crate::state::SlotRole::Reviewer,
            ));
        }
        (paths, state)
    }

    /// #183: the body carries the summary, its OPEN questions, and each reviewer's final
    /// verdict, instead of a bare run id.
    #[test]
    fn pr_body_carries_summary_open_questions_and_verdicts() {
        let tmp = tempdir().unwrap();
        let (paths, state) = body_state(tmp.path());
        std::fs::write(
            paths.artifact("rb1", "summary-impl.md"),
            "# Summary\n\nAdded the thing.\n\n## Open questions\n\n- Should X be configurable?\n\n## Files\n\n- src/a.rs\n- OPEN: is the default right?\n",
        )
        .unwrap();
        std::fs::write(
            paths.artifact("rb1", "review-review-1.md"),
            "## Verdict\napprove\n\nReviewed-Commit: 0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        std::fs::write(
            paths.artifact("rb1", "review-review-2.md"),
            "## Verdict\nrequest_changes\n",
        )
        .unwrap();

        let body = pr_body(&paths, &state, "spar/rb1/impl");
        assert!(body.starts_with("Shipped by spar run `rb1`."), "{body}");
        assert!(
            body.contains(
                "## Open questions\n\n- Should X be configurable?\n- OPEN: is the default right?\n"
            ),
            "{body}"
        );
        assert!(
            body.contains("- `review-1` (cli:codex): approve at `01234567`"),
            "{body}"
        );
        assert!(
            body.contains("- `review-2` (cli:claude): request_changes"),
            "{body}"
        );
        assert!(
            body.contains("## Implementer summary\n\n# Summary\n\nAdded the thing."),
            "{body}"
        );
        let open = body.find("## Open questions").unwrap();
        assert!(open < body.find("## Implementer summary").unwrap());
    }

    #[test]
    fn pr_body_without_a_summary_or_reviews_says_so() {
        let tmp = tempdir().unwrap();
        let (paths, mut state) = body_state(tmp.path());
        state.slots.clear();
        let body = pr_body(&paths, &state, "spar/rb1/impl");
        assert!(
            body.contains("_No implementer summary was written._"),
            "{body}"
        );
        assert!(!body.contains("## Review"), "{body}");
        assert!(!body.contains("## Open questions"), "{body}");
    }

    #[test]
    fn pr_body_is_truncated_under_githubs_limit_with_a_note() {
        let tmp = tempdir().unwrap();
        let (paths, state) = body_state(tmp.path());
        std::fs::write(
            paths.artifact("rb1", "summary-impl.md"),
            format!(
                "## Open questions\n- keep me\n\n## Detail\n{}",
                "é".repeat(100_000)
            ),
        )
        .unwrap();
        let body = pr_body(&paths, &state, "spar/rb1/impl");
        assert!(body.chars().count() <= 65_536, "{}", body.chars().count());
        assert!(body.contains("- keep me"));
        assert!(
            body.trim_end().ends_with("artifacts/summary-impl.md`._"),
            "{}",
            &body[body.len() - 200..]
        );
    }

    /// The bug this replaced: `state.task.chars().take(72)` over a `--spec` brief, which
    /// named a real PR `# Give muse slots a real presence and injection channel via its
    /// hook sys` — marker included, cut mid-word, body text already bleeding in.
    #[test]
    fn pr_title_uses_the_briefs_heading_not_its_first_72_bytes() {
        let brief = "# Give muse slots a real presence and injection channel via its hook system\n\nThree changes to spar's muse adapter: wire muse's lifecycle hooks so a muse slot\n";
        let title = pr_title(Some(brief), 72);
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
            pr_title(Some("fix the flaky quota test"), 72),
            "fix the flaky quota test"
        );
        // Leading blank lines are skipped, not rendered as an empty title.
        assert_eq!(
            pr_title(Some("\n\n  ## Retry muse 404s\n"), 72),
            "Retry muse 404s"
        );
        // A `#` inside the text is part of the title.
        assert_eq!(
            pr_title(Some("fix #123 off-by-one"), 72),
            "fix #123 off-by-one"
        );
    }

    #[test]
    fn pr_title_falls_back_when_there_is_no_usable_line() {
        assert_eq!(pr_title(None, 72), "spar change");
        assert_eq!(pr_title(Some(""), 72), "spar change");
        assert_eq!(pr_title(Some("   \n\n"), 72), "spar change");
        assert_eq!(pr_title(Some("###\n"), 72), "spar change");
    }

    /// One unbroken word longer than the budget has no boundary to cut on; it must still
    /// come back inside the limit rather than falling through whole.
    #[test]
    fn pr_title_clamps_a_single_overlong_word() {
        let title = pr_title(Some(&"x".repeat(200)), 72);
        assert!(title.chars().count() <= 72, "over budget: {title}");
        assert!(title.ends_with('\u{2026}'));
    }
}
