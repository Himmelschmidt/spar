use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "spar",
    version,
    about = "Multi-agent coding product: fleet TUI, dual backends, plan/review/arena/ship",
    long_about = "First-class multi-agent coding product.\n\
         Humans: `spar` opens the fleet TUI in the current git repo.\n\
         Outer agents: subcommands + --json + `spar skills get core`.\n\
         State lives in .spar/ under the project root."
)]
pub struct Cli {
    /// Project directory for TUI (default: cwd). Only when no subcommand.
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,

    /// Seed the TUI composer with a task (TUI mode only)
    #[arg(long = "task", global = true)]
    pub task: Option<String>,

    /// Use full mouse capture (motion/drag). Default is a minimal click+wheel mode
    /// that also works on mobile terminals like Termux, where the full set is dropped.
    #[arg(long = "full-mouse", global = true)]
    pub full_mouse: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check providers, tmux, git, and project layout
    Doctor {
        #[arg(long)]
        json: bool,
    },

    /// Multi-provider planning; ends in awaiting_plan_approval
    Plan {
        #[arg(long, short = 't')]
        task: String,
        /// Comma-separated `cli:…` or `api:…` (required unless `--select`)
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
        /// Resolve fleet from vals benchmarks + profile (`value`, `best`, `fast`, `auto`, or list)
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
        /// Urgency for `--select`: low | normal | high | critical
        #[arg(long, default_value = "normal")]
        urgency: String,
        /// Assign a provider to a role for THIS run, without touching `spar.toml`:
        /// `--role planner=cli:grok --role reviewer=cli:claude@opus`. Repeatable;
        /// repeating `reviewer` builds the panel. Overrides the file's `[roles]`.
        #[arg(long = "role", value_name = "ROLE=PROVIDER")]
        role: Vec<String>,
        /// Ref to cut every slot worktree from (branch, tag or sha).
        /// Default: the HEAD of the directory spar was invoked from.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
        /// Stub agents only; still writes `.spar/` state. Does **not** create real git worktrees.
        #[arg(long)]
        dry_run: bool,
        /// Structured plan-big task DAG under bus/tasks
        #[arg(long)]
        big: bool,
    },

    /// Approve a plan run so implement can proceed
    Approve {
        run_id: String,
        #[arg(long)]
        json: bool,
    },

    /// Reject a plan and optionally record a reason
    Reject {
        run_id: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Implement from an approved run, plan file, or direct task
    Implement {
        #[arg(long = "run")]
        run_id: Option<String>,
        #[arg(long)]
        plan: Option<std::path::PathBuf>,
        #[arg(long, short = 't')]
        task: Option<String>,
        /// Assign a provider to a role for THIS run, without touching `spar.toml`:
        /// `--role planner=cli:grok --role reviewer=cli:claude@opus`. Repeatable;
        /// repeating `reviewer` builds the panel. Overrides the file's `[roles]`.
        #[arg(long = "role", value_name = "ROLE=PROVIDER")]
        role: Vec<String>,
        /// Re-read `spar.toml` for an existing run and replace its frozen config.
        /// Without this, `--run <id>` uses the config the run was created with, whatever
        /// the project file says now.
        #[arg(long)]
        reload_config: bool,
        /// Ref to cut every slot worktree from (branch, tag or sha), for a NEW run.
        /// Default: the HEAD of the directory spar was invoked from. A run's base is
        /// fixed at creation, so `--run <id>` only accepts a base it already has.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
        /// Stub agents only; still writes `.spar/` state. Does **not** create real git worktrees.
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated `cli:…` or `api:…` (required unless `--select`)
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
        /// Resolve fleet from vals benchmarks + profile
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
        #[arg(long, default_value = "normal")]
        urgency: String,
        #[arg(long)]
        big: bool,
    },

    /// Run a named workflow
    Run {
        #[arg(long, value_enum)]
        workflow: WorkflowKind,
        #[arg(long, short = 't')]
        task: Option<String>,
        /// Assign a provider to a role for THIS run, without touching `spar.toml`:
        /// `--role planner=cli:grok --role reviewer=cli:claude@opus`. Repeatable;
        /// repeating `reviewer` builds the panel. Overrides the file's `[roles]`.
        #[arg(long = "role", value_name = "ROLE=PROVIDER")]
        role: Vec<String>,
        /// Ref to cut every slot worktree from (branch, tag or sha).
        /// Default: the HEAD of the directory spar was invoked from.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated `cli:…` or `api:…` (required unless `--select`)
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
        /// Resolve fleet from vals benchmarks + profile
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
        #[arg(long, default_value = "normal")]
        urgency: String,
        #[arg(long)]
        big: bool,
    },

    /// Show run status (or list runs)
    Status {
        run_id: Option<String>,
        #[arg(long)]
        json: bool,
        /// List runs across all registered projects (global home)
        #[arg(long)]
        all: bool,
        /// Include archived runs, which are hidden by default
        #[arg(long)]
        archived: bool,
    },

    /// Delete regenerable build output (`target/`, `node_modules`) inside finished runs'
    /// worktrees. Keeps the worktree, its branch and every commit — this is not `cleanup`
    Reclaim {
        /// Omit with `--all` to reclaim across every finished run in the project.
        run_id: Option<String>,
        /// Reclaim from every finished run in the project.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// Hide a finished run from listings. Deletes nothing; `--undo` brings it back
    Archive {
        /// Omit with `--all` to archive every finished run in the project.
        run_id: Option<String>,
        /// Archive every `done` / `plan_rejected` run in the project.
        #[arg(long)]
        all: bool,
        /// With `--all`, only runs untouched for this long, e.g. `14d`.
        #[arg(long, value_name = "DURATION")]
        older_than: Option<String>,
        /// Un-archive instead: bring the run back into listings.
        #[arg(long)]
        undo: bool,
        #[arg(long)]
        json: bool,
    },

    /// Block until a run reaches a terminal or gate phase
    Wait {
        run_id: String,
        #[arg(long, default_value = "2h")]
        timeout: String,
        #[arg(long)]
        json: bool,
        /// Block until the run reaches a terminal state or a human gate; text mode
        /// live-tails events, `--json` blocks quietly and prints final state at the stop
        #[arg(long)]
        follow: bool,
    },

    /// Show logs for a run or slot
    Logs {
        run_id: String,
        slot: Option<String>,
        /// Follow log growth
        #[arg(short = 'f', long)]
        follow: bool,
    },

    /// Attach to tmux session for a run (tmux backend)
    Attach { run_id: String },

    /// Live TUI dashboard (same as bare `spar`)
    Dashboard,

    /// Provider inventory and quota controls
    Provider {
        #[command(subcommand)]
        action: ProviderAction,
    },

    /// Dynamic model select (vals benchmarks + profiles)
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },

    /// Ship (push/PR) after human confirm
    Ship {
        run_id: String,
        #[arg(long)]
        json: bool,
        /// Branch the draft PR targets. Default: the run's base branch when it exists
        /// on origin, else the repo default.
        #[arg(long)]
        base: Option<String>,
        /// Record ship confirmation (and optionally execute)
        #[arg(long)]
        confirm: bool,
        /// Only record confirmation without pushing
        #[arg(long)]
        confirm_only: bool,
    },

    /// Confirm arena winner (or use ranked default)
    Confirm {
        run_id: String,
        #[arg(long)]
        winner: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Arena finish: merge-good-parts + multi-review → ship gate
    Reconcile {
        run_id: String,
        #[arg(long)]
        json: bool,
    },

    /// Swarm bus: send / list / presence / reserve
    Bus {
        #[command(subcommand)]
        action: BusCmd,
    },

    /// Halt a run's dispatch, keeping the branch and worktree (resumable).
    ///
    /// Writes a `stopped` marker, signals the orchestrator then the slot process
    /// groups (SIGTERM, grace, SIGKILL), and sets phase=stopped (exit code 1).
    /// Unlike `cleanup`, it never removes worktrees or the branch. Resume with
    /// `spar implement --run <id>`.
    Stop {
        /// Omit with `--abandoned` to sweep every abandoned run in the project.
        run_id: Option<String>,
        /// Stop every run that is in flight with no live orchestrator — the runs whose
        /// slots are still burning tokens after the process driving them died.
        #[arg(long)]
        abandoned: bool,
        #[arg(long)]
        json: bool,
    },

    /// Remove worktrees and optional run data
    Cleanup {
        /// Omit with `--all` to sweep the project.
        run_id: Option<String>,
        /// Sweep every finished run that nothing can resume (`done`, `plan_rejected`).
        #[arg(long)]
        all: bool,
        /// With `--all`, also sweep resumable runs at rest (stopped / failed / stuck /
        /// quota / gates) untouched for this long, e.g. `7d`. Age is the evidence that
        /// nobody is coming back to resume them.
        #[arg(long, value_name = "DURATION")]
        older_than: Option<String>,
        /// With `--all`, also sweep at-rest runs whose branches are already contained in
        /// their own base ref. Squash-merged branches are not ancestors and do not count.
        #[arg(long)]
        merged: bool,
        #[arg(long)]
        json: bool,
        /// Also delete `.spar/runs/<id>`
        #[arg(long)]
        purge: bool,
        /// Remove a worktree even if it holds uncommitted changes or unmerged commits.
        /// Only with an explicit run id — there is deliberately no project-wide force.
        #[arg(long)]
        force: bool,
    },

    /// Built-in skills for outer agents (agent-browser style)
    Skills {
        #[command(subcommand)]
        action: SkillsCmd,
    },

    /// Internal: continue a detached run (not for humans)
    #[command(name = "__internal_continue", hide = true)]
    InternalContinue { run_id: String },
}

#[derive(Debug, Subcommand)]
pub enum SkillsCmd {
    /// List available skills
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a skill document
    Get { name: String },
}

/// Swarm bus (W5): workspace-scoped, keyed by `agent_id`. `--run <id>` is an optional
/// grouping tag — pass it to scope a message/view to one run; omit it for bare
/// (run-less) Composer agents, which are addressable exactly like run slots.
#[derive(Debug, Subcommand)]
pub enum BusCmd {
    /// Send a chat message
    Send {
        #[arg(long)]
        run: Option<String>,
        #[arg(long, default_value = "human")]
        from: String,
        #[arg(long, default_value = "broadcast")]
        to: String,
        #[arg(long, short = 'm')]
        message: String,
        #[arg(long)]
        json: bool,
    },
    /// List bus events (all runs + bare traffic; `--run` filters to one run)
    Log {
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show an agent's inbox (peek by default; `--claim` drains exactly-once)
    Inbox {
        agent: String,
        /// Atomically claim (drain) messages so each is delivered exactly once
        #[arg(long)]
        claim: bool,
        /// Resolve a short role id to its unique bus id (`run:role`). Pass with a short
        /// `<agent>` (`--run $SPAR_RUN_ID`); omit when `<agent>` is already the unique id
        /// (`$SPAR_AGENT_ID`) or a bare agent. The drain keys on the unique id, not this tag.
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Presence snapshot (whole workspace; `--run` filters to one run's agents)
    Presence {
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record an agent presence transition (called by provider hooks)
    Heartbeat {
        agent: String,
        #[arg(long, default_value = "working")]
        status: String,
        #[arg(long)]
        run: Option<String>,
    },
    /// Drain an agent's inbox and dispatch it to the agent's delivery strategy.
    ///
    /// Invoked at a turn boundary (e.g. a Claude Stop hook). Bare mode emits the raw
    /// injection payload on stdout (the Stop-hook `block` JSON) and nothing else;
    /// `--json` emits an operator report instead of the hook payload.
    Deliver {
        agent: String,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Acknowledge a `requires_ack` message, stopping its redelivery
    Ack {
        /// Id of the message being acknowledged
        msg_id: String,
        #[arg(long, default_value = "human")]
        from: String,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reserve a path
    Reserve {
        path: String,
        #[arg(long, default_value = "human")]
        holder: String,
        #[arg(long)]
        run: Option<String>,
    },
    /// Release a path reserve
    Release {
        path: String,
        #[arg(long, default_value = "human")]
        holder: String,
        #[arg(long)]
        run: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProviderAction {
    List {
        #[arg(long)]
        json: bool,
    },
    Pause {
        name: String,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Resume {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove spar's agy statusline wrapper and restore the user's original statusline.
    AgyStatuslineUninstall,
}

#[derive(Debug, Subcommand)]
pub enum ModelAction {
    /// List ranked models for a profile
    List {
        #[arg(long)]
        bench: Option<String>,
        #[arg(long, default_value = "value")]
        profile: String,
        #[arg(long, default_value = "normal")]
        urgency: String,
        #[arg(long)]
        json: bool,
        /// Only models whose mapped provider is usable now
        #[arg(long)]
        usable: bool,
        /// Model catalog to list. "openrouter" lists the OpenRouter catalog (tool-capable
        /// only by default); absent keeps the default vals-bench path.
        #[arg(long)]
        provider: Option<String>,
        /// openrouter: include models that lack tool support (they silently fail as agents)
        #[arg(long)]
        all: bool,
    },
    /// Pick best model(s) for a role/profile
    Pick {
        #[arg(long, default_value = "implementer")]
        role: String,
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "normal")]
        urgency: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long)]
        json: bool,
    },
    /// Fetch vals bench data into cache
    Refresh {
        #[arg(long)]
        bench: Option<String>,
        #[arg(long)]
        json: bool,
        /// Only refresh benches whose cache is missing or older than the TTL
        #[arg(long)]
        if_stale: bool,
    },
    /// Show cache status
    Cache {
        #[arg(long)]
        json: bool,
    },
}

#[derive(
    Debug, Clone, Copy, ValueEnum, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    #[default]
    Auto,
    Headless,
    Tmux,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowKind {
    Plan,
    Loop,
    Arena,
    Roles,
    Peer,
    /// Concurrent independent multi-provider review (not split-stack peer)
    Review,
}
