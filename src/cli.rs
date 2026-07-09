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
        #[arg(long, value_delimiter = ',')]
        providers: Option<Vec<String>>,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
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
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_delimiter = ',')]
        providers: Option<Vec<String>>,
        #[arg(long)]
        big: bool,
    },

    /// Run a named workflow
    Run {
        #[arg(long, value_enum)]
        workflow: WorkflowKind,
        #[arg(long, short = 't')]
        task: Option<String>,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value_t = Backend::Auto)]
        backend: Backend,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_delimiter = ',')]
        providers: Option<Vec<String>>,
        #[arg(long)]
        big: bool,
    },

    /// Show run status (or list runs)
    Status {
        run_id: Option<String>,
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
        /// Stream events until stop
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

    /// Ship (push/PR) after human confirm
    Ship {
        run_id: String,
        #[arg(long)]
        json: bool,
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

    /// Remove worktrees and optional run data
    Cleanup {
        run_id: String,
        #[arg(long)]
        json: bool,
        /// Also delete `.spar/runs/<id>`
        #[arg(long)]
        purge: bool,
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
    Get {
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum BusCmd {
    /// Send a chat message
    Send {
        run_id: String,
        #[arg(long, default_value = "human")]
        from: String,
        #[arg(long, default_value = "broadcast")]
        to: String,
        #[arg(long, short = 'm')]
        message: String,
        #[arg(long)]
        json: bool,
    },
    /// List bus events
    Log {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Presence snapshot
    Presence {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Reserve a path
    Reserve {
        run_id: String,
        path: String,
        #[arg(long, default_value = "human")]
        holder: String,
    },
    /// Release a path reserve
    Release {
        run_id: String,
        path: String,
        #[arg(long, default_value = "human")]
        holder: String,
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
}
