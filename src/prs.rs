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
    #[serde(default, rename = "headRefOid")]
    head_ref_oid: Option<String>,
    #[serde(default, rename = "baseRefName")]
    base_ref_name: Option<String>,
    /// `true` when the head lives in a fork. Spar pushes to the repo the PR targets, so
    /// a fork's PR with the same branch name is never the run's.
    #[serde(default, rename = "isCrossRepository")]
    is_cross_repository: bool,
    #[serde(default)]
    pub body: Option<String>,
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

fn parse<T: serde::de::DeserializeOwned>(out: &str, what: &str) -> Result<T> {
    serde_json::from_str(out).map_err(|e| anyhow::anyhow!("unreadable gh {what} output: {e}"))
}

/// Same repo, and the base spar targets when it named one (`None` is the repo default,
/// which `gh` does not report, so any base passes).
fn ours(pr: &GhPr, base: Option<&str>) -> bool {
    !pr.is_cross_repository && base.is_none_or(|b| pr.base_ref_name.as_deref() == Some(b))
}

/// The open PR for `branch` that spar would have opened itself: an implementer that
/// opened it, or an earlier ship that got this far (#164).
pub(crate) fn open_pr_for(cwd: &Path, branch: &str, base: Option<&str>) -> Result<Option<GhPr>> {
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
            "number,url,state,isDraft,baseRefName,isCrossRepository,headRefOid,body",
        ],
    )?;
    let prs: Vec<GhPr> = parse(&out, "pr list")?;
    Ok(prs.into_iter().find(|pr| ours(pr, base)))
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

    pub fn rate_limited(&self) -> bool {
        self.error
            .as_deref()
            .is_some_and(|e| e.to_ascii_lowercase().contains("rate limit"))
    }
}

/// Why a run is not one `sync-prs` may settle, or `None` when it is: a real run whose
/// current round shipped, sitting at the ship gate or done.
fn ineligible(state: &RunState) -> Option<&'static str> {
    if state.dry_run {
        return Some("dry run");
    }
    if state.shipped_round != Some(state.round) || state.shipped_head.is_none() {
        return Some("has not shipped its current round");
    }
    if !matches!(state.phase, Phase::Done | Phase::AwaitingShipConfirm) {
        return Some("not done or at the ship gate");
    }
    None
}

/// Settle shipped runs from their PR's state (O123): merged moves a run to `done` and
/// records when and as what, closed unmerged moves it to `stopped`, and an open PR (or
/// none) leaves it alone. `apply = false` reports without writing. A rate-limited `gh`
/// ends the pass; the rest wait for the next one.
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
        if let Some(why) = ineligible(&state) {
            if explicit {
                rows.push(SyncRow::note(&state, why));
            }
            continue;
        }
        if !explicit && state.phase == Phase::Done && state.merged_at.is_some() {
            continue;
        }
        if crate::state::orchestrator_alive(paths, &id) {
            rows.push(SyncRow::note(&state, "orchestrator alive; left alone"));
            continue;
        }
        let row = sync_one(paths, &mut state, apply);
        let stop = row.rate_limited();
        rows.push(row);
        if stop {
            break;
        }
    }
    Ok(rows)
}

fn sync_one(paths: &SparPaths, state: &mut RunState, apply: bool) -> SyncRow {
    let mut row = SyncRow::note(state, "");
    row.note = None;
    let pr = match find_pr(&paths.project_root, state) {
        Ok(Some(pr)) => pr,
        Ok(None) => {
            row.note = Some("no PR whose head is the shipped commit".into());
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
        "MERGED" if state.merged_at.is_none() => Phase::Done,
        "CLOSED" => Phase::Stopped,
        _ => {
            row.note = Some("nothing to settle".into());
            return row;
        }
    };
    row.to = Some(to);
    if !apply {
        return row;
    }
    let was = state.phase;
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
    // `save` writes the terminal history row only on a phase change; a run already
    // `done` gets its merge recorded here (O102).
    if was == Phase::Done && to == Phase::Done {
        crate::history::append(paths, state, crate::history::Trigger::Terminal);
    }
    let _ = crate::events::append(paths, &state.id, &crate::events::Event::info(message));
    row
}

/// The PR's head is the commit ship pushed, or descends from it.
fn holds_shipped(root: &Path, state: &RunState, pr: &GhPr) -> bool {
    let (Some(shipped), Some(head)) = (state.shipped_head.as_deref(), pr.head_ref_oid.as_deref())
    else {
        return false;
    };
    shipped == head
        || Command::new("git")
            .args(["merge-base", "--is-ancestor", shipped, head])
            .current_dir(root)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// The run's PR: by its recorded URL, else the newest PR for its branch that targets the
/// shipped base from this repo. Either way only a PR holding the shipped commit counts,
/// so an earlier round's PR, or a stranger's PR on a reused branch name, never settles it.
fn find_pr(root: &Path, state: &RunState) -> Result<Option<GhPr>> {
    const FIELDS: &str =
        "number,url,state,mergedAt,mergeCommit,headRefOid,baseRefName,isCrossRepository";
    if let Some(url) = &state.pr_url {
        let out = gh(root, &["pr", "view", url, "--json", FIELDS])?;
        let pr: GhPr = parse(&out, "pr view")?;
        return Ok(holds_shipped(root, state, &pr).then_some(pr));
    }
    let (branch, _) = crate::ship::select_branch_cwd(state)?;
    let out = gh(
        root,
        &[
            "pr", "list", "--head", &branch, "--state", "all", "--limit", "20", "--json", FIELDS,
        ],
    )?;
    let prs: Vec<GhPr> = parse(&out, "pr list")?;
    Ok(prs
        .into_iter()
        .find(|pr| ours(pr, state.shipped_base.as_deref()) && holds_shipped(root, state, pr)))
}
