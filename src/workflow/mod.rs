pub mod arena;
pub mod implement;
pub mod peer;
pub mod plan;
pub mod roles;

use crate::cli::{Backend, WorkflowKind};
use crate::config::Config;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::util;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct CommonOpts {
    pub task: Option<String>,
    pub providers: Option<Vec<String>>,
    pub detach: bool,
    pub json: bool,
    pub backend: Backend,
    pub dry_run: bool,
    pub big: bool,
}

impl Default for CommonOpts {
    fn default() -> Self {
        Self {
            task: None,
            providers: None,
            detach: false,
            json: false,
            backend: Backend::Auto,
            dry_run: false,
            big: false,
        }
    }
}

impl CommonOpts {
    pub fn resolve_dry_run(&self) -> bool {
        self.dry_run || util::env_truthy("SPAR_DRY_RUN")
    }
}

pub fn run_named(
    kind: WorkflowKind,
    opts: CommonOpts,
    paths: &SparPaths,
    cfg: &Config,
) -> Result<ExitCode> {
    match kind {
        WorkflowKind::Plan => {
            let task = opts
                .task
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--task required for plan"))?;
            plan::run(task, opts, paths, cfg)
        }
        WorkflowKind::Loop => implement::run_loop(opts, paths, cfg),
        WorkflowKind::Arena => arena::run(opts, paths, cfg),
        WorkflowKind::Roles => roles::run(opts, paths, cfg),
        WorkflowKind::Peer => peer::run(opts, paths, cfg),
    }
}
