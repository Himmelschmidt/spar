//! PR lookups through `gh`, and `sync-prs`: settling shipped runs from their PR's state.
use crate::paths::SparPaths;
use crate::state::{Phase, RunState};
use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct GhPr {
    pub number: u64,
    pub url: String,
    #[serde(default)]
    state: String,
    #[serde(default, rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(default, rename = "mergeCommit")]
    merge_commit: Option<GhCommit>,
}

#[derive(Debug, serde::Deserialize)]
struct GhCommit {
    oid: String,
}

/// stdout of `gh <args>`, or an error that says whether `gh` is missing or what it said.
pub(crate) fn gh(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = match Command::new("gh").args(args).current_dir(cwd).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("gh not found on PATH; install the GitHub CLI and run `gh auth login`")
        }
        Err(e) => bail!("could not run gh: {e}"),
    };
    if !out.status.success() {
        bail!(
            "gh {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The open PR whose head is `branch`, if one exists: an implementer that opened the PR
/// itself, or an earlier ship that got this far (#164).
pub(crate) fn open_pr_for(cwd: &Path, branch: &str) -> Result<Option<GhPr>> {
    let out = gh(
        cwd,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number,url,state,isDraft",
        ],
    )?;
    let prs: Vec<GhPr> = serde_json::from_str(&out)
        .map_err(|e| anyhow::anyhow!("unreadable gh pr list output: {e}"))?;
    Ok(prs.into_iter().next())
}

/// Which runs `sync-prs` looks at.
pub enum SyncScope<'a> {
    Run(&'a str),
    All,
}

#[derive(Debug, serde::Serialize)]
pub struct SyncRow {
    pub run_id: String,
    pub phase: Phase,
    /// Phase the run moved to, or would move to under `--dry-run`. `None`: left alone.
    pub to: Option<Phase>,
    pub pr_url: Option<String>,
    pub pr_state: Option<String>,
    pub note: Option<String>,
    pub error: Option<String>,
}

/// A run spar shipped or tried to: it recorded a PR, or ran the ship commands. Dry runs
/// never pushed anything.
fn shipped(state: &RunState) -> bool {
    !state.dry_run && (state.pr_url.is_some() || state.ship_commands.is_some())
}

/// `--all` does not ask GitHub again about a run it already settled.
fn settled(state: &RunState) -> bool {
    (state.phase == Phase::Done && state.merged_at.is_some())
        || (state.phase == Phase::Stopped && state.pr_state.as_deref() == Some("CLOSED"))
}

/// Settle shipped runs from their PR's state (O123): merged moves a run to `done` and
/// records when and as what, closed unmerged moves it to `stopped`, and an open PR (or
/// none) leaves it alone. `apply = false` reports without writing.
pub fn sync_prs(paths: &SparPaths, scope: SyncScope, apply: bool) -> Result<Vec<SyncRow>> {
    gh(&paths.project_root, &["--version"])?;
    let explicit = matches!(scope, SyncScope::Run(_));
    let ids = match scope {
        SyncScope::Run(id) => vec![id.to_string()],
        SyncScope::All => crate::state::list_runs(paths)?
            .into_iter()
            .map(|s| s.id)
            .collect(),
    };
    let mut rows = Vec::new();
    for id in ids {
        let mut state = match RunState::load(paths, &id) {
            Ok(s) => s,
            Err(e) if explicit => return Err(e),
            Err(_) => continue,
        };
        if !shipped(&state) {
            if explicit {
                rows.push(SyncRow::note(&state, "no PR or shipped branch recorded"));
            }
            continue;
        }
        if !explicit && settled(&state) {
            continue;
        }
        if crate::state::orchestrator_alive(paths, &id) {
            rows.push(SyncRow::note(&state, "orchestrator alive; left alone"));
            continue;
        }
        rows.push(sync_one(paths, &mut state, apply));
    }
    Ok(rows)
}

impl SyncRow {
    fn note(state: &RunState, note: &str) -> Self {
        Self {
            run_id: state.id.clone(),
            phase: state.phase,
            to: None,
            pr_url: state.pr_url.clone(),
            pr_state: None,
            note: Some(note.into()),
            error: None,
        }
    }
}

fn sync_one(paths: &SparPaths, state: &mut RunState, apply: bool) -> SyncRow {
    let mut row = SyncRow::note(state, "");
    row.note = None;
    let pr = match find_pr(&paths.project_root, state) {
        Ok(Some(pr)) => pr,
        Ok(None) => {
            row.note = Some("no PR found for the run's branch".into());
            return row;
        }
        Err(e) => {
            row.error = Some(format!("{e:#}"));
            return row;
        }
    };
    row.pr_url = Some(pr.url.clone());
    row.pr_state = Some(pr.state.clone());
    let to = match pr.state.as_str() {
        "MERGED" if !(state.phase == Phase::Done && state.merged_at.is_some()) => Phase::Done,
        "CLOSED" if state.phase != Phase::Stopped => Phase::Stopped,
        _ => {
            row.note = Some("nothing to settle".into());
            return row;
        }
    };
    row.to = Some(to);
    if !apply {
        return row;
    }
    state.pr_url = Some(pr.url.clone());
    state.pr_state = Some(pr.state.clone());
    let message = if to == Phase::Done {
        state.merged_at = pr
            .merged_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        state.merge_commit = pr.merge_commit.map(|c| c.oid);
        state.error = None;
        format!(
            "sync-prs: PR {} merged{}",
            pr.url,
            state
                .merge_commit
                .as_deref()
                .map(|c| format!(" as {c}"))
                .unwrap_or_default()
        )
    } else {
        format!("sync-prs: PR {} closed without merging", pr.url)
    };
    state.set_phase(to);
    if let Err(e) = state.save(paths) {
        row.error = Some(format!("{e:#}"));
        return row;
    }
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(message));
    row
}

/// The run's PR: by its recorded URL, else the newest PR of any state for its branch.
fn find_pr(cwd: &Path, state: &RunState) -> Result<Option<GhPr>> {
    const FIELDS: &str = "number,url,state,mergedAt,mergeCommit";
    if let Some(url) = &state.pr_url {
        let out = gh(cwd, &["pr", "view", url, "--json", FIELDS])?;
        let pr: GhPr = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("unreadable gh pr view output: {e}"))?;
        return Ok(Some(pr));
    }
    let (branch, _) = crate::ship::select_branch_cwd(state)?;
    let out = gh(
        cwd,
        &[
            "pr", "list", "--head", &branch, "--state", "all", "--limit", "1", "--json", FIELDS,
        ],
    )?;
    let prs: Vec<GhPr> = serde_json::from_str(&out)
        .map_err(|e| anyhow::anyhow!("unreadable gh pr list output: {e}"))?;
    Ok(prs.into_iter().next())
}
