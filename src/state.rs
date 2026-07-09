use crate::bus::MessageBudget;
use crate::cli::{Backend, WorkflowKind};
use crate::config::{AutonomyLevel, IsolationMode};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::provider_ref::ExecBackend;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub workflow: WorkflowKind,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub slots: Vec<SlotState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub project_root: PathBuf,
    /// Spawn mode for native-cli: auto|headless|tmux
    #[serde(default)]
    pub backend: Backend,
    #[serde(default)]
    pub isolation: IsolationMode,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run: Option<String>,
    /// Deprecated: plan→implement now stays on one run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_run: Option<String>,
    #[serde(default)]
    pub gates: Gates,
    #[serde(default)]
    pub worktrees: Vec<WorktreeRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner_slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ship_commands: Option<Vec<String>>,
    #[serde(default)]
    pub fix_rounds: u32,
    #[serde(default)]
    pub max_fix_rounds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub rotated_implementer: bool,
    #[serde(default)]
    pub widened_reviewers: bool,
    #[serde(default)]
    pub autonomy: AutonomyLevel,
    #[serde(default)]
    pub message_budget: MessageBudget,
    #[serde(default)]
    pub big: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arena_finish: Option<ArenaFinish>,
    #[serde(default)]
    pub usage: Vec<SlotUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArenaFinish {
    Winner,
    Reconcile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotUsage {
    pub slot_id: String,
    pub provider: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub tools: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gates {
    #[serde(default)]
    pub plan_approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner_confirmed: Option<String>,
    #[serde(default)]
    pub ship_confirmed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub slot_id: String,
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Init,
    PrepareIsolation,
    SpawnSlots,
    Dispatch,
    WaitCompletion,
    PlanReady,
    AwaitingPlanApproval,
    PlanApproved,
    PlanRejected,
    Review,
    Rank,
    Fix,
    PeerRelay,
    AwaitingWinnerConfirm,
    AwaitingReconcile,
    AwaitingShipConfirm,
    Shipping,
    Done,
    Escalated,
    Failed,
    Stuck,
    /// No usable providers (maps to exit code 4).
    Quota,
}

impl Phase {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Phase::Done
                | Phase::Failed
                | Phase::Stuck
                | Phase::Escalated
                | Phase::PlanRejected
                | Phase::PlanApproved
                | Phase::Quota
        )
    }

    pub fn is_gate(&self) -> bool {
        matches!(
            self,
            Phase::AwaitingPlanApproval
                | Phase::AwaitingWinnerConfirm
                | Phase::AwaitingReconcile
                | Phase::AwaitingShipConfirm
        )
    }

    pub fn is_waitable_stop(&self) -> bool {
        self.is_terminal() || self.is_gate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotState {
    pub id: String,
    pub provider: String,
    pub role: SlotRole,
    pub status: SlotStatus,
    /// native-cli | api-sdk | dry-run | headless | tmux
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_backend: Option<ExecBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SlotUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotRole {
    Planner,
    PlanCritic,
    Implementer,
    Reviewer,
    Ranker,
    Peer,
    Reconciler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    Pending,
    Running,
    Done,
    Failed,
    Stuck,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
    pub workflow: WorkflowKind,
    pub phase: Phase,
    pub updated_at: DateTime<Utc>,
}

impl RunState {
    pub fn new(id: impl Into<String>, workflow: WorkflowKind, project_root: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            workflow,
            phase: Phase::Init,
            task: None,
            created_at: now,
            updated_at: now,
            slots: Vec::new(),
            error: None,
            project_root,
            backend: Backend::Auto,
            isolation: IsolationMode::Worktree,
            dry_run: false,
            parent_run: None,
            child_run: None,
            gates: Gates::default(),
            worktrees: Vec::new(),
            winner_slot: None,
            ship_commands: None,
            fix_rounds: 0,
            max_fix_rounds: 3,
            tmux_session: None,
            providers: Vec::new(),
            rotated_implementer: false,
            widened_reviewers: false,
            autonomy: AutonomyLevel::default(),
            message_budget: MessageBudget::default(),
            big: false,
            arena_finish: None,
            usage: Vec::new(),
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    pub fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.touch();
    }

    pub fn load(paths: &SparPaths, run_id: &str) -> Result<Self> {
        let file = paths.state_file(run_id);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("read run state {}", file.display()))?;
        let state: Self = serde_json::from_str(&text)
            .with_context(|| format!("parse run state {}", file.display()))?;
        Ok(state)
    }

    pub fn save(&self, paths: &SparPaths) -> Result<()> {
        paths.ensure_run_dirs(&self.id)?;
        let prev_phase = if paths.state_file(&self.id).is_file() {
            RunState::load(paths, &self.id).ok().map(|s| s.phase)
        } else {
            None
        };
        let file = paths.state_file(&self.id);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&file, text).with_context(|| format!("write {}", file.display()))?;

        if prev_phase != Some(self.phase) {
            let _ = crate::events::append(
                paths,
                &self.id,
                &crate::events::Event::phase(self.phase, prev_phase),
            );
            if self.phase.is_gate() {
                let _ = crate::events::append(
                    paths,
                    &self.id,
                    &crate::events::Event::gate(format!("{:?}", self.phase), self.phase),
                );
            }
        }
        Ok(())
    }

    /// Meaningful when `phase.is_waitable_stop()`; in-flight phases return Success
    /// only as a neutral placeholder — prefer `status_exit_code()` / JSON `exit_code: null`.
    pub fn exit_code(&self) -> ExitCode {
        match self.phase {
            Phase::Done | Phase::PlanApproved => ExitCode::Success,
            Phase::AwaitingPlanApproval
            | Phase::AwaitingWinnerConfirm
            | Phase::AwaitingReconcile
            | Phase::AwaitingShipConfirm => ExitCode::HumanGate,
            Phase::Stuck | Phase::Escalated => ExitCode::Stuck,
            Phase::Quota => ExitCode::Quota,
            Phase::Failed | Phase::PlanRejected => ExitCode::Failure,
            // In-flight: not a terminal success; outer agents should poll until waitable.
            _ => ExitCode::Success,
        }
    }

    /// Exit code for CLI `status` / `emit_run_json`: None while still running.
    pub fn status_exit_code(&self) -> Option<u8> {
        if self.phase.is_waitable_stop() {
            Some(self.exit_code().as_u8())
        } else {
            None
        }
    }

    pub fn slot_mut(&mut self, id: &str) -> Option<&mut SlotState> {
        self.slots.iter_mut().find(|s| s.id == id)
    }
}

pub fn list_runs(paths: &SparPaths) -> Result<Vec<RunSummary>> {
    let runs_dir = paths.runs_dir();
    if !runs_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(&runs_dir).with_context(|| format!("read {}", runs_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        match RunState::load(paths, &id) {
            Ok(state) => out.push(RunSummary {
                id: state.id,
                workflow: state.workflow,
                phase: state.phase,
                updated_at: state.updated_at,
            }),
            Err(_) => continue,
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::SparPaths;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_state() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("run1", WorkflowKind::Plan, tmp.path().to_path_buf());
        state.phase = Phase::AwaitingPlanApproval;
        state.task = Some("do the thing".into());
        state.save(&paths).unwrap();
        let loaded = RunState::load(&paths, "run1").unwrap();
        assert_eq!(loaded.phase, Phase::AwaitingPlanApproval);
        assert_eq!(loaded.exit_code(), ExitCode::HumanGate);
    }
}
