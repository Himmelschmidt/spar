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
    /// Operator directive for the current implement round (`implement --run -t`).
    /// Never replaces `task` (the run's identity); cleared when a round runs without `-t`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amendment: Option<String>,
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
    /// Last suite-channel result. `Inconclusive` means the runner fell over and the
    /// tests never produced a clean verdict — distinct from a real `Fail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_outcome: Option<SuiteOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteOutcome {
    Pass,
    Fail,
    Inconclusive,
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
    /// Pre-coding acceptance tests (test-author slot).
    Spec,
    AwaitingPlanApproval,
    PlanApproved,
    PlanRejected,
    Review,
    /// Full test suite channel (cheap model).
    Suite,
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
    /// Halted by operator (`spar stop`). Waitable but resumable; keeps worktrees.
    Stopped,
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
        // Stopped is resumable (not terminal, not a gate) but `wait` must return.
        self.is_terminal() || self.is_gate() || matches!(self, Phase::Stopped)
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
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SlotUsage>,
    /// Selected model id (from model-select or explicit); passed to CLI/API spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotRole {
    Planner,
    PlanCritic,
    /// Writes acceptance tests before implement; coordinates with planner/critic via bus.
    TestAuthor,
    Implementer,
    /// Cheap suite runner — full test suites only; not review/impl judgment.
    Tester,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    /// In flight, but no live orchestrator owns it — computed at read time.
    #[serde(default)]
    pub abandoned: bool,
    /// Filled when listing across projects (global home).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
}

impl RunState {
    pub fn new(id: impl Into<String>, workflow: WorkflowKind, project_root: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            workflow,
            phase: Phase::Init,
            task: None,
            amendment: None,
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
            suite_outcome: None,
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

    /// Load for observation (`status`, TUI). `state.json` is only as fresh as the last
    /// orchestrator write, so an orchestrator that died mid-phase leaves slots frozen at
    /// `running` forever; their markers on disk say otherwise. Reconciles in memory only —
    /// a read-only command never rewrites the run.
    pub fn load_for_display(paths: &SparPaths, run_id: &str) -> Result<Self> {
        let mut state = Self::load(paths, run_id)?;
        state.reconcile_slots_from_markers(paths);
        Ok(state)
    }

    pub fn reconcile_slots_from_markers(&mut self, paths: &SparPaths) {
        let run_id = self.id.clone();
        for slot in &mut self.slots {
            let marker = crate::markers::terminal_marker(paths, &run_id, &slot.id);
            slot.status = reconcile_slot_status(slot.status, marker);
        }
    }

    /// True when the run is still mid-flight but nothing is driving it: the orchestrator
    /// exited without reaching a terminal phase. Computed, never persisted.
    pub fn abandoned(&self, paths: &SparPaths) -> bool {
        is_abandoned(self.phase, orchestrator_alive(paths, &self.id))
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
        // Global index so `spar` from anywhere can find this project’s runs.
        crate::registry::note_run(&self.project_root, &self.id);
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
            Phase::Failed | Phase::PlanRejected | Phase::Stopped => ExitCode::Failure,
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

/// On-disk markers beat `state.json` for a slot the orchestrator never got to update.
/// Only a `running` slot is reconciled; a slot already at rest keeps its recorded verdict.
pub fn reconcile_slot_status(
    state_status: SlotStatus,
    marker: Option<crate::markers::TerminalMarker>,
) -> SlotStatus {
    if state_status != SlotStatus::Running {
        return state_status;
    }
    match marker {
        Some(crate::markers::TerminalMarker::Done) => SlotStatus::Done,
        Some(crate::markers::TerminalMarker::Failed) => SlotStatus::Failed,
        None => SlotStatus::Running,
    }
}

/// A run is abandoned when it is still in flight but no live process owns it. Phases at
/// rest — terminal, a human gate, or `Stopped` — are *meant* to have no orchestrator.
pub fn is_abandoned(phase: Phase, orchestrator_alive: bool) -> bool {
    !phase.is_waitable_stop() && !orchestrator_alive
}

pub fn orchestrator_alive(paths: &SparPaths, run_id: &str) -> bool {
    crate::runlock::RunLock::owner(paths, run_id)
        .map(|t| t.alive())
        .unwrap_or(false)
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
                abandoned: state.abandoned(paths),
                id: state.id,
                workflow: state.workflow,
                phase: state.phase,
                updated_at: state.updated_at,
                task: state.task,
                dry_run: state.dry_run,
                project_root: None,
                project_name: None,
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

    #[test]
    fn stopped_is_waitable_and_roundtrips() {
        assert!(Phase::Stopped.is_waitable_stop());
        assert!(!Phase::Stopped.is_terminal());
        assert!(!Phase::Stopped.is_gate());
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("run-stop", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = Phase::Stopped;
        state.save(&paths).unwrap();
        let loaded = RunState::load(&paths, "run-stop").unwrap();
        assert_eq!(loaded.phase, Phase::Stopped);
        assert_eq!(loaded.exit_code(), ExitCode::Failure);
        assert_eq!(loaded.status_exit_code(), Some(1));
    }

    #[test]
    fn reconcile_running_slot_from_terminal_marker() {
        use crate::markers::TerminalMarker;
        assert_eq!(
            reconcile_slot_status(SlotStatus::Running, Some(TerminalMarker::Done)),
            SlotStatus::Done
        );
        assert_eq!(
            reconcile_slot_status(SlotStatus::Running, Some(TerminalMarker::Failed)),
            SlotStatus::Failed
        );
        assert_eq!(
            reconcile_slot_status(SlotStatus::Running, None),
            SlotStatus::Running
        );
    }

    #[test]
    fn reconcile_leaves_non_running_status_alone() {
        use crate::markers::TerminalMarker;
        for status in [
            SlotStatus::Pending,
            SlotStatus::Done,
            SlotStatus::Failed,
            SlotStatus::Stuck,
        ] {
            assert_eq!(
                reconcile_slot_status(status, Some(TerminalMarker::Done)),
                status
            );
            assert_eq!(
                reconcile_slot_status(status, Some(TerminalMarker::Failed)),
                status
            );
            assert_eq!(reconcile_slot_status(status, None), status);
        }
    }

    /// The zombie run: orchestrator died in `review`, the slot's `.done` marker is on disk,
    /// `state.json` still says `running`. Display must show `done`, and the file must not change.
    #[test]
    fn load_for_display_reconciles_without_rewriting_state() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("zombie", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = Phase::Review;
        let mut slot = crate::executor::init_slot("review-a", "cli:grok", SlotRole::Reviewer);
        slot.status = SlotStatus::Running;
        state.slots.push(slot);
        state.save(&paths).unwrap();
        crate::markers::write_done(&paths, "zombie", "review-a").unwrap();

        let shown = RunState::load_for_display(&paths, "zombie").unwrap();
        assert_eq!(shown.slots[0].status, SlotStatus::Done);

        let on_disk = RunState::load(&paths, "zombie").unwrap();
        assert_eq!(
            on_disk.slots[0].status,
            SlotStatus::Running,
            "display must not rewrite state.json"
        );
    }

    #[test]
    fn abandoned_only_when_in_flight_and_unowned() {
        assert!(is_abandoned(Phase::Review, false));
        assert!(is_abandoned(Phase::WaitCompletion, false));
        assert!(!is_abandoned(Phase::Review, true));
        // At rest by design: nobody is supposed to own these.
        for phase in [
            Phase::Done,
            Phase::Failed,
            Phase::Stuck,
            Phase::Quota,
            Phase::AwaitingPlanApproval,
            Phase::AwaitingShipConfirm,
            Phase::Stopped,
        ] {
            assert!(
                !is_abandoned(phase, false),
                "{phase:?} must not be abandoned"
            );
            assert!(
                !is_abandoned(phase, true),
                "{phase:?} must not be abandoned"
            );
        }
    }

    #[test]
    fn list_runs_flags_abandoned_run() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("zombie", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = Phase::Review;
        state.save(&paths).unwrap();

        let runs = list_runs(&paths).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].abandoned, "no lock owner ⇒ abandoned");
    }

    #[test]
    fn failed_slot_persists_exit_and_signal() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("run-sig", WorkflowKind::Loop, tmp.path().to_path_buf());
        let mut slot = crate::executor::init_slot("impl", "cli:claude", SlotRole::Implementer);
        slot.status = SlotStatus::Failed;
        slot.pid = Some(4242);
        slot.exit_code = None;
        slot.signal = Some(9);
        state.slots.push(slot);
        state.save(&paths).unwrap();

        let loaded = RunState::load(&paths, "run-sig").unwrap();
        let s = &loaded.slots[0];
        assert_eq!(s.status, SlotStatus::Failed);
        assert_eq!(s.pid, Some(4242));
        assert_eq!(s.exit_code, None);
        assert_eq!(s.signal, Some(9));
    }
}
