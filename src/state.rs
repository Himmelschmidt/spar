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
    /// Ref every slot worktree is cut from, as the operator named it (branch, tag, sha).
    /// Resolved once at run creation from `--base` or the invoking directory's HEAD —
    /// never from `project_root`'s HEAD, which is a different branch whenever spar is
    /// driven from a linked worktree. `None` on runs created before this existed.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Commit `base_ref` resolved to. This is what worktrees actually branch from.
    #[serde(default)]
    pub base_commit: Option<String>,
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
    /// Highest `round` this run may reach before it escalates to a human instead of
    /// re-dispatching again (O52). Frozen from `[rounds] max` when the run is created and
    /// moved only by an explicit `implement --max-rounds`, so a later `spar.toml` edit
    /// cannot silently re-ceiling a run in flight. `0` disables the ceiling.
    #[serde(default = "crate::config::default_max_rounds")]
    pub max_rounds: u32,
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
    /// Which round of work this run is on. A run is a unit of work, not an
    /// invocation (O45): continuing it — implementing an approved plan, replanning
    /// after a rejection, a fix pass — opens a new round on the same id rather than
    /// minting another run. `1` for every run created before rounds existed.
    #[serde(default = "one_round")]
    pub round: u32,
    /// When this run was archived, if it was. Archiving hides a finished run from
    /// listings; it deletes nothing and is reversible. Distinct from cleanup, which
    /// reclaims worktrees and leaves the record, and from `--purge`, which deletes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
    /// Fingerprint of `test-contract.md` as frozen when the round loop was entered.
    /// `None` only for runs from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_fingerprint: Option<String>,
    /// Set when the on-disk contract stopped matching the frozen fingerprint mid-run.
    /// The gate still judges against the frozen version; this is the loud flag that it
    /// moved. Not `skip_serializing_if`-omitted: an outer agent must see `false`
    /// explicitly rather than infer it from the field's absence.
    #[serde(default)]
    pub contract_modified: bool,
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
    /// Peak prompt footprint of a single request, for the context gauge. Never a total.
    #[serde(default)]
    pub context_tokens: u64,
    /// Cumulative billed tokens for this dispatch (input + cache read + cache write +
    /// output, reasoning folded into output). `state.usage` is the run's ledger, one
    /// entry per dispatch, so a run's billed total is the sum over it.
    #[serde(default)]
    pub billed_tokens: u64,
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
    /// Round ceiling reached: the run wants more re-dispatch than it is allowed to buy
    /// on its own (O52). A gate, not a failure — `implement --run <id> --max-rounds <N>`
    /// is the operator saying the next round is worth paying for.
    AwaitingRoundExtension,
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
                | Phase::AwaitingRoundExtension
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
    /// The run round this slot last ran in (O45). `1` for pre-rounds runs.
    #[serde(default = "one_round")]
    pub round: u32,
    /// Set when this dispatch's own failure ended with a quota/rate-limit signal in its
    /// log (the discriminator `executor::run_slot` computes, not a guess from status
    /// alone) — lets the caller route the run to `Phase::Quota` instead of `Failed`.
    /// Reset at the start of every dispatch, so a re-dispatch never carries a stale hit.
    #[serde(default)]
    pub quota_hit: bool,
}

fn one_round() -> u32 {
    1
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

impl SlotRole {
    /// Canonical config/state key — the `snake_case` serde representation. Single source
    /// of truth shared by `state.json` and the `[roles]` config block. Priority 9 keys
    /// the fleet off these.
    #[allow(dead_code)]
    pub fn as_config_key(&self) -> &'static str {
        match self {
            SlotRole::Planner => "planner",
            SlotRole::PlanCritic => "plan_critic",
            SlotRole::TestAuthor => "test_author",
            SlotRole::Implementer => "implementer",
            SlotRole::Tester => "tester",
            SlotRole::Reviewer => "reviewer",
            SlotRole::Ranker => "ranker",
            SlotRole::Peer => "peer",
            SlotRole::Reconciler => "reconciler",
        }
    }

    /// Parse a canonical config/state key back into a `SlotRole`. No aliases —
    /// `critic` is not accepted (see plan Priority 8: one vocabulary, not three).
    #[allow(dead_code)]
    pub fn from_config_key(s: &str) -> Option<SlotRole> {
        Some(match s {
            "planner" => SlotRole::Planner,
            "plan_critic" => SlotRole::PlanCritic,
            "test_author" => SlotRole::TestAuthor,
            "implementer" => SlotRole::Implementer,
            "tester" => SlotRole::Tester,
            "reviewer" => SlotRole::Reviewer,
            "ranker" => SlotRole::Ranker,
            "peer" => SlotRole::Peer,
            "reconciler" => SlotRole::Reconciler,
            _ => return None,
        })
    }
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
    /// Hidden from default listings. The record and its artifacts are untouched.
    #[serde(default)]
    pub archived: bool,
    /// Ref/commit the run's slot worktrees were cut from (see `RunState::base_ref`).
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Set when this run is a **leg** of another unit of work (O46): listing surfaces
    /// fold it into its parent's row, and `--json` keeps carrying it so outer agents
    /// can see the grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run: Option<String>,
    /// How many rounds this run has been through (O45). `1` unless it was continued.
    #[serde(default = "one_round")]
    pub round: u32,
    /// How many runs this row stands for once legs are folded in (U15). `1` normally;
    /// only a listing surface sets it higher. Never serialized: it is a property of a
    /// rendered list, not of a run, and on disk or in `--json` it could only ever be
    /// the constant 1.
    #[serde(skip)]
    pub legs: u32,
    /// How many of those legs want the operator. Only meaningful on a folded row
    /// (`legs > 1`): a unit with two runs at gates must still be counted twice, or
    /// folding becomes a way to hide a gate. Never serialized, for the same reason as
    /// `legs`.
    #[serde(skip)]
    pub wants: u32,
    /// Identity of the folded unit this row stands for (U15), stable regardless of
    /// which leg `fold_units` picks as the loudest representative from one snapshot
    /// to the next. `None` for a row that was never folded. A cursor or cache keyed
    /// on a folded row must use this, not `id` — `id` follows whichever leg is
    /// currently loudest and can change between snapshots (AC-28).
    #[serde(skip)]
    pub unit_id: Option<String>,
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
            archived_at: None,
            task: None,
            amendment: None,
            created_at: now,
            updated_at: now,
            slots: Vec::new(),
            error: None,
            project_root,
            base_ref: None,
            base_commit: None,
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
            max_rounds: crate::config::default_max_rounds(),
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
            contract_fingerprint: None,
            contract_modified: false,
            round: 1,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    /// Open a new round on this run (O45). Bumps the counter, un-archives, and hands
    /// back the new round number so the caller can stamp the slots it dispatches.
    /// Everything else about the run — id, brief, base, config, usage ledger — is
    /// deliberately untouched: it is the same unit of work.
    pub fn begin_round(&mut self) -> u32 {
        self.round = self.round.saturating_add(1);
        self.archived_at = None;
        self.touch();
        self.round
    }

    /// Whether opening another round would take this run past its ceiling (O52).
    /// Asked *before* `begin_round`, so `max_rounds` is the highest round that runs.
    pub fn round_ceiling_reached(&self) -> bool {
        self.max_rounds > 0 && self.round >= self.max_rounds
    }

    pub fn set_phase(&mut self, phase: Phase) {
        // A run stays archived only while it stays finished. Anything else — resumed,
        // re-approved, parked at a gate — is visible again.
        //
        // Keyed off the archivable set, not `at_rest`. Using `at_rest` looked equivalent
        // and was not: `spar approve` accepts a `plan_rejected` run and moves it to
        // `PlanApproved`, which is *inside* `at_rest`, so an auto-archived rejected plan
        // stayed hidden after being approved — approved, waiting for `spar implement`, and
        // in no listing. This predicate also keeps archiving independent of the sweep's
        // notion of rest, which is a separate question and is being changed separately.
        if !auto_archivable(phase) {
            self.archived_at = None;
        }
        self.phase = phase;
        self.touch();
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
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

    /// Reconcile and *persist*, for the paths that run once an orchestrator is gone:
    /// `spar stop`, a run-lock reclaim, and resume. `load_for_display` stays read-only
    /// by design, so a slot the orchestrator never got to finish would otherwise read
    /// `running` on disk forever (O49).
    ///
    /// Beyond the marker pass, a slot still `running` in a run nothing owns is demoted
    /// to `failed` with `reason`: `running` is durably saved at dispatch and a terminal
    /// status only if the orchestrator survived the wait, so with no orchestrator alive
    /// and no live slot process the record is stale by construction.
    pub fn reconcile_and_save(
        &mut self,
        paths: &SparPaths,
        owner: RunOwner,
        reason: &str,
    ) -> Result<()> {
        if !self.reconcile_dead_slots(paths, owner, reason) {
            return Ok(());
        }
        self.save(paths)
    }

    /// The in-memory half of [`RunState::reconcile_and_save`]. True when it changed something.
    pub fn reconcile_dead_slots(
        &mut self,
        paths: &SparPaths,
        owner: RunOwner,
        reason: &str,
    ) -> bool {
        let before: Vec<SlotStatus> = self.slots.iter().map(|s| s.status).collect();
        self.reconcile_slots_from_markers(paths);
        if owner == RunOwner::Nobody {
            let run_id = self.id.clone();
            for slot in &mut self.slots {
                if slot.status != SlotStatus::Running || slot_process_alive(paths, &run_id, slot) {
                    continue;
                }
                slot.status = SlotStatus::Failed;
                slot.error = Some(reason.into());
            }
        }
        self.slots.iter().zip(&before).any(|(s, b)| s.status != *b)
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
        // Dry runs are ephemeral verification fixtures (often in a temp dir), so they
        // must not register their project in ~/.spar/registry.json and clutter
        // `spar status --all` with throwaway roots.
        if !self.dry_run {
            crate::registry::note_run(&self.project_root, &self.id);
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
            | Phase::AwaitingShipConfirm
            | Phase::AwaitingRoundExtension => ExitCode::HumanGate,
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

/// What a demoted slot records. Neither says the work was bad: both say the process
/// supervising it is gone, and they differ on why, because "crashed" and "you stopped
/// it" are not the same report to leave on an operator's own run.
pub const ORPHANED_SLOT: &str = "orchestrator died mid-dispatch";
pub const STOPPED_SLOT: &str = "halted by operator (spar stop)";

/// Who is driving a run while it is reconciled.
///
/// A caller's *established fact*, never re-derived inside the reconcile, because the two
/// places that know an orchestrator died cannot ask the lock file and get the truth:
/// `RunLock::acquire` overwrites it with its own live pid before it can reconcile, and
/// `execute_loop` already holds it. Asking there answers "yes, me" and skips the very
/// demotion the reclaim exists to perform (O49).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOwner {
    Live,
    Nobody,
}

impl RunOwner {
    /// For callers that genuinely have to ask, i.e. hold no lock of their own.
    pub fn observe(paths: &SparPaths, run_id: &str) -> Self {
        if orchestrator_alive(paths, run_id) {
            Self::Live
        } else {
            Self::Nobody
        }
    }
}

/// Whether a slot's recorded process is still running.
///
/// **Deliberately stricter than `live_slot_pids`**, which reaps on a bare `alive()`.
/// The two ask the same question for opposite stakes: there, a token that cannot prove
/// identity is still worth signalling because missing an orphan leaves it burning
/// tokens; here, believing one would leave a slot recorded `running` forever, which is
/// the bug this exists to close. So a start-time-less token counts as *not* evidence of
/// life. The `.pid` marker survives re-dispatch on purpose (`markers::clear_slot`), and
/// the start-time check is what makes reading a prior dispatch's pid safe.
fn slot_process_alive(paths: &SparPaths, run_id: &str, slot: &SlotState) -> bool {
    crate::markers::read_pid(paths, run_id, &slot.id)
        .or_else(|| slot.pid.map(crate::process::PidToken::from_pid))
        .is_some_and(|t| t.starttime.is_some() && t.alive())
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

/// Whether `spar cleanup --all` may reap this run's worktrees.
///
/// Two tiers, because a worktree is the only copy of an agent's work:
/// - **Unresumable and at rest** (`done`, `plan_rejected`) is always sweepable. Nothing
///   can ever pick these up again.
/// - **Resumable at rest** (`stopped`, `failed`, `stuck`, `quota`, and human gates) only
///   when `older_than` is given and the run has been untouched that long. Age is the
///   evidence that nobody is coming back for it.
///
/// A run in flight is never swept, whether or not anyone still owns it: reaping a live
/// fleet is `spar stop`'s job, and an abandoned one is `spar stop --abandoned`'s.
pub fn sweepable(
    phase: Phase,
    idle: std::time::Duration,
    older_than: Option<std::time::Duration>,
) -> bool {
    let unresumable = matches!(phase, Phase::Done | Phase::PlanRejected);
    if unresumable {
        return older_than.is_none_or(|min| idle >= min);
    }
    match (age_sweepable(phase), older_than) {
        (true, Some(min)) => idle >= min,
        _ => false,
    }
}

/// Phases where *idle time* is evidence that nobody is coming back.
///
/// `resumable_at_rest` minus the gates, and the distinction is the whole point. A run at
/// `awaiting_plan_approval` is blocked **on a human**: its idle time measures how busy that
/// human was, not whether the run was abandoned. Age is close to anti-evidence there, and
/// sweeping on it reaps the runs most likely to still be wanted. Gates are reclaimed by
/// resolving them, by naming the run id, or by merged evidence — never by waiting.
pub fn age_sweepable(phase: Phase) -> bool {
    resumable_at_rest(phase) && !phase.is_gate()
}

/// Nobody is driving this run: it is finished, or parked. The precondition for any
/// reclamation — an in-flight run's worktrees are in use whatever else is true of it.
pub fn at_rest(phase: Phase) -> bool {
    matches!(phase, Phase::Done | Phase::PlanRejected) || resumable_at_rest(phase)
}

/// A run that is not running and not finished: parked at a gate, stopped, failed, stuck
/// or out of quota. Its worktrees are still live work — `implement --run` can pick it up.
pub fn resumable_at_rest(phase: Phase) -> bool {
    phase.is_gate()
        || matches!(
            phase,
            Phase::Stopped
                | Phase::Failed
                | Phase::Stuck
                | Phase::Quota
                | Phase::PlanApproved
                // Terminal, but nothing else claims it, and the alternative is telling
                // the operator to `spar stop` a run that already finished.
                | Phase::Escalated
        )
}

/// Why `sweepable` said no, for the sweep's report. `None` when it said yes.
pub fn sweep_skip_reason(
    phase: Phase,
    idle: std::time::Duration,
    older_than: Option<std::time::Duration>,
) -> Option<String> {
    if sweepable(phase, idle, older_than) {
        return None;
    }
    // In flight first: an in-flight run is never swept at any age, so reporting it as
    // merely too young implies it would be swept once it aged.
    if !at_rest(phase) {
        return Some(format!("{phase:?} is in flight — spar stop it first"));
    }
    // Before the age reason, because for a gate the age reason is a lie: no `--older-than`
    // will ever take it, so reporting "idle below --older-than" invites the operator to
    // raise the threshold and wonder why nothing happens.
    if phase.is_gate() {
        return Some(format!(
            "{phase:?} is waiting on you — age is not evidence here; resolve it, or reap by run id"
        ));
    }
    if older_than.is_some_and(|min| idle < min) {
        return Some(format!("idle {}s is below --older-than", idle.as_secs()));
    }
    Some(format!(
        "{phase:?} is resumable — sweep it with --older-than, or by run id"
    ))
}

/// Reap key for the built-in suite child. The orchestrator owns that process but no slot
/// does, so its pid marker is keyed by a reserved id instead of a slot id (O54).
pub const BUILTIN_SUITE_PID_ID: &str = "suite-builtin";

/// Slot processes of `state` that are still alive.
///
/// Start-time checked: a terminal slot's recorded pid may since have been recycled onto
/// an unrelated process, and reporting (or signalling) that would be worse than missing
/// an orphan.
pub fn live_slot_pids(paths: &SparPaths, state: &RunState) -> Vec<u32> {
    let mut out = Vec::new();
    // The built-in suite channel (O54) runs under the orchestrator with no slot of its
    // own, so without this its `cargo test` is invisible to `stop --abandoned` and to
    // every "orphan pids" report — the operator is told nothing is running while a suite
    // holds the worktree.
    if let Some(token) = crate::markers::read_pid(paths, &state.id, BUILTIN_SUITE_PID_ID) {
        if token.alive() {
            out.push(token.pid);
        }
    }
    for slot in &state.slots {
        if matches!(
            slot.status,
            SlotStatus::Done | SlotStatus::Failed | SlotStatus::Stuck
        ) {
            continue;
        }
        let token = crate::markers::read_pid(paths, &state.id, &slot.id)
            .or_else(|| slot.pid.map(crate::process::PidToken::from_pid));
        if let Some(token) = token {
            if token.alive() {
                out.push(token.pid);
            }
        }
    }
    out
}

/// Phases that auto-archiving may take on its own: finished, and nothing can resume them.
///
/// Deliberately narrower than `at_rest`. A run parked at a **gate** is waiting on the
/// operator — hiding those is how the one listing that matters gets lost, and payforge
/// had 12 of them buried under 28 finished runs. `stopped` / `failed` / `stuck` / `quota`
/// are ambiguous (they may be picked up again), so they stay visible until archived by
/// hand.
pub fn auto_archivable(phase: Phase) -> bool {
    matches!(phase, Phase::Done | Phase::PlanRejected)
}

/// Phases an operator may archive by hand: anything nobody is currently driving.
///
/// Deliberately its own predicate rather than `at_rest`. The sweep's notion of rest exists
/// to decide what is safe to *delete* and is under revision; archiving deletes nothing and
/// should not silently change meaning when that lands.
pub fn archivable_by_hand(phase: Phase) -> bool {
    phase.is_terminal() || phase.is_gate() || matches!(phase, Phase::Stopped)
}

/// Archive every finished run idle at least `older_than`. Returns the ids archived.
///
/// Non-destructive and reversible: it sets a timestamp, and `spar archive --undo` or any
/// resume clears it. Nothing on disk is removed — that is `cleanup --purge`.
pub fn auto_archive(
    paths: &SparPaths,
    older_than: std::time::Duration,
    now: DateTime<Utc>,
) -> Result<Vec<String>> {
    archive_sweep(paths, older_than, now, false)
}

/// The sweep behind `archive --all`. `halted` widens it from the auto-archivable set
/// to everything an operator may archive by hand — `stopped` / `failed` / `stuck` /
/// `quota`, which auto-archiving deliberately never touches (O36). Gates are excluded
/// either way: hiding the runs that want a human is the failure archiving exists to
/// prevent. Opt-in only, and `--undo` still reverses it.
pub fn archive_sweep(
    paths: &SparPaths,
    older_than: std::time::Duration,
    now: DateTime<Utc>,
    halted: bool,
) -> Result<Vec<String>> {
    // Spelled out, not derived: `archivable_by_hand` is `is_terminal() || is_gate() ||
    // Stopped`, and `is_terminal()` includes `PlanApproved` — a run the operator
    // approved and has not implemented yet, which is exactly what an unlinked-plan
    // error tells them to go continue. Hiding that is the bug O36 already fixed once.
    let reachable = |phase: Phase| {
        auto_archivable(phase)
            || (halted
                && matches!(
                    phase,
                    Phase::Stopped | Phase::Failed | Phase::Stuck | Phase::Quota
                ))
    };
    let mut archived = Vec::new();
    for summary in list_runs(paths)? {
        if summary.archived || !reachable(summary.phase) {
            continue;
        }
        let idle = (now - summary.updated_at).to_std().unwrap_or_default();
        if idle < older_than {
            continue;
        }
        // Same guard `sweep_merged` carries (O34): the phase on disk is a snapshot, and a
        // finalizing orchestrator is briefly between load and save. `archive --all` passes
        // a zero threshold, so a run that reached `Done` seconds ago is reachable here, and
        // this writes the whole file.
        if orchestrator_alive(paths, &summary.id) {
            continue;
        }
        let Ok(mut state) = RunState::load(paths, &summary.id) else {
            continue;
        };
        state.archived_at = Some(now);
        if state.save(paths).is_ok() {
            archived.push(summary.id);
        }
    }
    Ok(archived)
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
                archived: state.is_archived(),
                id: state.id,
                workflow: state.workflow,
                phase: state.phase,
                updated_at: state.updated_at,
                task: state.task,
                dry_run: state.dry_run,
                parent_run: state.parent_run,
                round: state.round,
                legs: 1,
                wants: 0,
                unit_id: None,
                base_ref: state.base_ref,
                base_commit: state.base_commit,
                project_root: None,
                project_name: None,
            }),
            Err(_) => continue,
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::SparPaths;
    use tempfile::tempdir;

    /// The built-in suite child has no slot, so this reserved marker is the only thing
    /// standing between a live two-hour `cargo test` and `stop --abandoned` reporting
    /// "reaped 0" (O54).
    #[test]
    fn live_slot_pids_sees_the_builtin_suite_child() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let state = RunState::new("r-suite", WorkflowKind::Loop, tmp.path().to_path_buf());
        std::fs::create_dir_all(paths.markers_dir(&state.id)).unwrap();
        assert!(live_slot_pids(&paths, &state).is_empty());

        let me = std::process::id();
        crate::markers::write_pid(
            &paths,
            &state.id,
            BUILTIN_SUITE_PID_ID,
            crate::process::PidToken::capture(me),
        )
        .unwrap();
        assert_eq!(live_slot_pids(&paths, &state), vec![me]);

        crate::markers::clear_pid(&paths, &state.id, BUILTIN_SUITE_PID_ID);
        assert!(live_slot_pids(&paths, &state).is_empty());
    }

    #[test]
    fn slot_role_config_key_matches_serde() {
        let all = [
            SlotRole::Planner,
            SlotRole::PlanCritic,
            SlotRole::TestAuthor,
            SlotRole::Implementer,
            SlotRole::Tester,
            SlotRole::Reviewer,
            SlotRole::Ranker,
            SlotRole::Peer,
            SlotRole::Reconciler,
        ];
        for role in all {
            let serde_key = serde_json::to_value(role).unwrap();
            let serde_key = serde_key.as_str().unwrap();
            assert_eq!(
                role.as_config_key(),
                serde_key,
                "as_config_key must match serde rename for {role:?}"
            );
            assert_eq!(
                SlotRole::from_config_key(serde_key),
                Some(role),
                "from_config_key must round-trip {role:?}"
            );
        }
        assert_eq!(SlotRole::from_config_key("critic"), None);
    }

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

    /// O52. The ceiling escalates to a **human**, so it must be exit 2 and not exit 3:
    /// nothing is broken, the run has just spent the re-dispatch budget it was allowed
    /// to spend on its own.
    #[test]
    fn round_ceiling_gate_is_a_human_gate_not_a_failure() {
        let phase = Phase::AwaitingRoundExtension;
        assert!(phase.is_gate());
        assert!(!phase.is_terminal());
        assert!(phase.is_waitable_stop());
        assert!(resumable_at_rest(phase), "the next round has to be buyable");
        assert!(
            !age_sweepable(phase),
            "age at a gate measures how busy the operator was, not abandonment"
        );
        assert!(
            !auto_archivable(phase),
            "a run waiting on you must stay visible"
        );

        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("run-ceil", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = phase;
        state.save(&paths).unwrap();
        let loaded = RunState::load(&paths, "run-ceil").unwrap();
        assert_eq!(loaded.exit_code(), ExitCode::HumanGate);
        assert_eq!(loaded.status_exit_code(), Some(2));
    }

    /// The predicate is asked before `begin_round`, so `max_rounds` is the highest round
    /// that actually runs — an off-by-one here silently buys or refuses a whole round.
    #[test]
    fn round_ceiling_is_the_last_round_that_runs() {
        let tmp = tempdir().unwrap();
        let mut state = RunState::new("r", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.max_rounds = 3;
        state.round = 2;
        assert!(!state.round_ceiling_reached());
        assert_eq!(state.begin_round(), 3);
        assert!(state.round_ceiling_reached());
        state.max_rounds = 0;
        assert!(!state.round_ceiling_reached(), "0 disables the ceiling");
    }

    /// Runs written before the ceiling existed have no `max_rounds`; they must load with
    /// the default rather than `0`, which would read as "unbounded".
    #[test]
    fn state_without_max_rounds_defaults_to_the_ceiling() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let state = RunState::new("legacy", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.save(&paths).unwrap();
        let file = paths.state_file("legacy");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("max_rounds");
        std::fs::write(&file, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let loaded = RunState::load(&paths, "legacy").unwrap();
        assert_eq!(loaded.max_rounds, crate::config::default_max_rounds());
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

    /// The 86 slots markers alone can never settle: the orchestrator was killed *inside*
    /// its dispatch, so no terminal marker was ever written and `running` is the last
    /// durable word. With nobody owning the run and no live process behind the slot, the
    /// record is stale by construction, and unlike display this one persists.
    #[test]
    fn reconcile_and_save_demotes_a_markerless_running_slot_with_no_live_process() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("orphan", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = Phase::Dispatch;
        let mut slot = crate::executor::init_slot("impl", "cli:claude", SlotRole::Implementer);
        slot.status = SlotStatus::Running;
        slot.exit_code = Some(0);
        state.slots.push(slot);
        state.save(&paths).unwrap();

        // No orchestrator lock, no `.pid` marker, no terminal marker: nothing owns it.
        assert_eq!(
            RunState::load_for_display(&paths, "orphan").unwrap().slots[0].status,
            SlotStatus::Running,
            "markers cannot settle a slot that never wrote one"
        );

        let mut state = RunState::load(&paths, "orphan").unwrap();
        assert_eq!(RunOwner::observe(&paths, "orphan"), RunOwner::Nobody);
        assert!(state
            .reconcile_and_save(&paths, RunOwner::Nobody, ORPHANED_SLOT)
            .is_ok());
        let on_disk = RunState::load(&paths, "orphan").unwrap();
        assert_eq!(on_disk.slots[0].status, SlotStatus::Failed);
        assert_eq!(on_disk.slots[0].error.as_deref(), Some(ORPHANED_SLOT));
    }

    /// A run whose orchestrator is alive is being driven: `running` is the truth there,
    /// and demoting it would fail a working slot out from under its supervisor.
    #[test]
    fn reconcile_leaves_running_slots_alone_while_an_orchestrator_owns_the_run() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("owned", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = Phase::Dispatch;
        let mut slot = crate::executor::init_slot("impl", "cli:claude", SlotRole::Implementer);
        slot.status = SlotStatus::Running;
        state.slots.push(slot);
        state.save(&paths).unwrap();

        let _held = crate::runlock::RunLock::acquire(&paths, "owned").unwrap();
        assert_eq!(
            RunOwner::observe(&paths, "owned"),
            RunOwner::Live,
            "a held lock must read as owned"
        );
        assert!(!state.reconcile_dead_slots(&paths, RunOwner::Live, ORPHANED_SLOT));
        assert_eq!(state.slots[0].status, SlotStatus::Running);
    }

    /// A slot whose own process is still up is an orphan, not a finished slot. Demoting
    /// it would hide it from `live_slot_pids`, which is what `stop --abandoned` reaps by.
    #[test]
    fn reconcile_keeps_a_running_slot_whose_process_is_still_alive() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let mut state = RunState::new("live-slot", WorkflowKind::Loop, tmp.path().to_path_buf());
        state.phase = Phase::Dispatch;
        let mut slot = crate::executor::init_slot("impl", "cli:claude", SlotRole::Implementer);
        slot.status = SlotStatus::Running;
        state.slots.push(slot);
        state.save(&paths).unwrap();
        // This test process stands in for the slot's child: live, with a start-time.
        crate::markers::write_pid(
            &paths,
            "live-slot",
            "impl",
            crate::process::PidToken::capture(std::process::id()),
        )
        .unwrap();

        assert!(!state.reconcile_dead_slots(&paths, RunOwner::Nobody, ORPHANED_SLOT));
        assert_eq!(state.slots[0].status, SlotStatus::Running);
    }

    #[test]
    fn sweep_takes_finished_runs_and_spares_resumable_ones() {
        use std::time::Duration;
        let day = Duration::from_secs(86_400);
        let week = Some(Duration::from_secs(7 * 86_400));

        // Nothing can resume these: always sweepable.
        for phase in [Phase::Done, Phase::PlanRejected] {
            assert!(sweepable(phase, Duration::ZERO, None), "{phase:?}");
        }
        // Resumable at rest: only once age says nobody is coming back.
        for phase in [
            Phase::Stopped,
            Phase::Failed,
            Phase::Stuck,
            Phase::Quota,
            Phase::PlanApproved,
        ] {
            assert!(
                !sweepable(phase, day * 30, None),
                "{phase:?} must survive a bare --all: it can still be resumed"
            );
            assert!(sweepable(phase, day * 30, week), "{phase:?} at 30d");
            assert!(!sweepable(phase, day, week), "{phase:?} at 1d");
        }
        // Gates moved out of the age path entirely: see `age_is_never_evidence_for_a_gate`.
        for phase in [Phase::AwaitingPlanApproval, Phase::AwaitingShipConfirm] {
            assert!(!sweepable(phase, day * 30, None), "{phase:?}");
            assert!(!sweepable(phase, day * 30, week), "{phase:?} at any age");
        }
        // In flight is never swept, however old the state file looks.
        for phase in [Phase::Dispatch, Phase::Review, Phase::WaitCompletion] {
            assert!(!sweepable(phase, day * 30, None), "{phase:?}");
            assert!(!sweepable(phase, day * 30, week), "{phase:?}");
        }
        // --older-than also holds back young finished runs.
        assert!(!sweepable(Phase::Done, day, week));
    }

    /// The sweep's silence was the real complaint: 124 GB of finished worktrees on disk
    /// and "nothing to sweep" on stdout reads as a refusal rather than a policy.
    #[test]
    fn spared_runs_say_why_they_were_spared() {
        use std::time::Duration;
        let day = Duration::from_secs(86_400);
        let week = Some(Duration::from_secs(7 * 86_400));

        assert_eq!(sweep_skip_reason(Phase::Done, day, None), None);

        let stopped = sweep_skip_reason(Phase::Stopped, day * 30, None).expect("spared");
        assert!(stopped.contains("resumable"), "{stopped}");
        assert!(stopped.contains("--older-than"), "{stopped}");

        let young = sweep_skip_reason(Phase::Done, day, week).expect("spared");
        assert!(young.contains("--older-than"), "{young}");

        let live = sweep_skip_reason(Phase::Review, day * 30, None).expect("spared");
        assert!(live.contains("in flight"), "{live}");

        // An in-flight run is never swept at any age, so age must not be the reason given.
        let live_young = sweep_skip_reason(Phase::Review, day, week).expect("spared");
        assert!(live_young.contains("in flight"), "{live_young}");
        assert!(!live_young.contains("--older-than"), "{live_young}");
    }

    /// `at_rest` is the precondition for merged-evidence reclamation, so an in-flight
    /// phase leaking into it would let the auto-sweep delete a live run's worktrees.
    #[test]
    fn at_rest_covers_finished_and_parked_but_never_in_flight() {
        for phase in [
            Phase::Done,
            Phase::PlanRejected,
            Phase::Stopped,
            Phase::Failed,
            Phase::Stuck,
            Phase::Quota,
            Phase::PlanApproved,
            Phase::AwaitingShipConfirm,
        ] {
            assert!(at_rest(phase), "{phase:?}");
        }
        for phase in [
            Phase::Init,
            Phase::PrepareIsolation,
            Phase::Dispatch,
            Phase::Review,
            Phase::Suite,
            Phase::Fix,
            Phase::Shipping,
            Phase::WaitCompletion,
        ] {
            assert!(!at_rest(phase), "{phase:?} is in flight");
        }
    }

    /// Gates are the listing that matters: payforge had 12 runs waiting on a human buried
    /// under 28 finished ones. Auto-archiving must never be what hides them.
    #[test]
    fn auto_archive_takes_finished_runs_and_never_gates() {
        for phase in [Phase::Done, Phase::PlanRejected] {
            assert!(auto_archivable(phase), "{phase:?}");
        }
        for phase in [
            Phase::AwaitingPlanApproval,
            Phase::AwaitingShipConfirm,
            Phase::AwaitingWinnerConfirm,
            Phase::AwaitingReconcile,
            Phase::Stopped,
            Phase::Failed,
            Phase::Stuck,
            Phase::Quota,
            Phase::Review,
            Phase::Dispatch,
        ] {
            assert!(!auto_archivable(phase), "{phase:?} must stay visible");
        }
    }

    #[test]
    fn auto_archive_respects_age_and_preserves_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let now = Utc::now();

        let mut fresh = RunState::new("fresh", WorkflowKind::Loop, tmp.path().to_path_buf());
        fresh.phase = Phase::Done;
        fresh.updated_at = now - chrono::Duration::days(1);
        fresh.save(&paths).unwrap();

        let mut old = RunState::new("old", WorkflowKind::Loop, tmp.path().to_path_buf());
        old.phase = Phase::Done;
        old.updated_at = now - chrono::Duration::days(30);
        old.save(&paths).unwrap();

        let done = auto_archive(&paths, std::time::Duration::from_secs(14 * 86_400), now).unwrap();
        assert_eq!(done, vec!["old".to_string()]);

        // Age must survive archiving, or `cleanup --older-than` would see every archived
        // run as freshly touched and spare it forever.
        let reloaded = RunState::load(&paths, "old").unwrap();
        assert!(reloaded.is_archived());
        assert_eq!(reloaded.updated_at, old.updated_at);
        assert!(!RunState::load(&paths, "fresh").unwrap().is_archived());

        // Idempotent: a second pass finds nothing new.
        assert!(
            auto_archive(&paths, std::time::Duration::from_secs(14 * 86_400), now)
                .unwrap()
                .is_empty()
        );
    }

    /// A run stays archived only while it stays finished. Anything else has to come back
    /// into view, or the operator is waiting on something no listing shows.
    ///
    /// `PlanApproved` is the case that motivated keying this off the archivable set rather
    /// than `at_rest`: `spar approve` accepts a `plan_rejected` run, and `PlanApproved` is
    /// inside `at_rest`, so an auto-archived rejected plan stayed hidden after approval —
    /// approved, waiting for `spar implement`, and in no listing.
    #[test]
    fn a_run_stays_archived_only_while_it_stays_finished() {
        let archived = |phase: Phase| {
            let mut s = RunState::new("r", WorkflowKind::Loop, PathBuf::from("/tmp/x"));
            s.archived_at = Some(Utc::now());
            s.set_phase(phase);
            s.is_archived()
        };

        assert!(archived(Phase::Done), "still finished");
        assert!(archived(Phase::PlanRejected), "still finished");

        assert!(
            !archived(Phase::PlanApproved),
            "approved runs want implement"
        );
        assert!(!archived(Phase::Dispatch), "in flight");
        assert!(
            !archived(Phase::AwaitingPlanApproval),
            "a gate wants a human"
        );
        assert!(!archived(Phase::Stopped), "parked, not finished");
        assert!(!archived(Phase::Failed), "failed runs are resumable");
    }

    /// The refusal is its own predicate, so a change to the sweep's notion of rest cannot
    /// silently start refusing runs an operator may legitimately archive.
    #[test]
    fn archivable_by_hand_covers_everything_nobody_is_driving() {
        for phase in [
            Phase::Done,
            Phase::PlanRejected,
            Phase::PlanApproved,
            Phase::Failed,
            Phase::Stuck,
            Phase::Quota,
            Phase::Escalated,
            Phase::Stopped,
            Phase::AwaitingPlanApproval,
            Phase::AwaitingShipConfirm,
        ] {
            assert!(archivable_by_hand(phase), "{phase:?}");
        }
        for phase in [
            Phase::Init,
            Phase::Dispatch,
            Phase::Review,
            Phase::Suite,
            Phase::Fix,
            Phase::Shipping,
            Phase::WaitCompletion,
        ] {
            assert!(!archivable_by_hand(phase), "{phase:?} is in flight");
        }
    }

    /// Age is evidence of abandonment for a parked run and anti-evidence for a gate: a
    /// run at `awaiting_plan_approval` is idle because the *human* was busy. Sweeping on
    /// age there reaps the runs most likely to still be wanted.
    #[test]
    fn age_is_never_evidence_for_a_gate() {
        use std::time::Duration;
        let month = Duration::from_secs(30 * 86_400);
        let week = Some(Duration::from_secs(7 * 86_400));

        for phase in [
            Phase::AwaitingPlanApproval,
            Phase::AwaitingShipConfirm,
            Phase::AwaitingWinnerConfirm,
            Phase::AwaitingReconcile,
        ] {
            assert!(!age_sweepable(phase), "{phase:?}");
            assert!(
                !sweepable(phase, month, week),
                "{phase:?} must survive any --older-than"
            );
            let why = sweep_skip_reason(phase, month, week).expect("spared");
            assert!(why.contains("waiting on you"), "{why}");
            assert!(
                !why.contains("below --older-than"),
                "a gate must not be reported as merely too young: {why}"
            );
        }

        // Parked-but-not-gated still sweeps on age, as before.
        for phase in [Phase::Stopped, Phase::Failed, Phase::Stuck, Phase::Quota] {
            assert!(age_sweepable(phase), "{phase:?}");
            assert!(sweepable(phase, month, week), "{phase:?}");
        }
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
