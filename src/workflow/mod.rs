pub mod arena;
pub mod implement;
pub mod peer;
pub mod plan;
pub mod review;
pub mod review_result;
pub mod roles;
pub mod roles_resolve;

use crate::cli::{Backend, WorkflowKind};
use crate::config::Config;
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::util;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct CommonOpts {
    pub task: Option<String>,
    /// Explicit provider list (required unless `select` is set).
    pub providers: Vec<String>,
    /// vals-backed profile list (`value` / `best` / `fast` / `auto` / multi).
    pub select: Vec<String>,
    pub urgency: String,
    /// `--base`: ref every slot worktree is cut from. `None` = the invoking directory's HEAD.
    pub base: Option<String>,
    pub detach: bool,
    pub json: bool,
    pub backend: Backend,
    pub dry_run: bool,
    pub big: bool,
    /// `--max-rounds`: raise this run's round ceiling (O52). `None` leaves it where the
    /// run froze it. Only `implement` offers it; every other entry point passes `None`.
    pub max_rounds: Option<u32>,
    /// `--accept-contract`: adopt a `test-contract.md` that drifted under the previous
    /// round. Without it, a re-entry that would re-freeze a tampered contract refuses.
    pub accept_contract: bool,
}

impl Default for CommonOpts {
    fn default() -> Self {
        Self {
            task: None,
            providers: Vec::new(),
            select: Vec::new(),
            urgency: "normal".into(),
            base: None,
            detach: false,
            json: false,
            backend: Backend::Auto,
            dry_run: false,
            big: false,
            max_rounds: None,
            accept_contract: false,
        }
    }
}

impl CommonOpts {
    pub fn resolve_dry_run(&self) -> bool {
        self.dry_run || util::env_truthy("SPAR_DRY_RUN")
    }

    /// Explicit providers or `--select` resolution. Writes `model-select.json` when selecting.
    pub fn resolve_fleet(
        &self,
        n: usize,
        roles: &[&str],
        paths: &SparPaths,
        cfg: &Config,
        run_id: &str,
    ) -> Result<Vec<String>> {
        let dry = self.resolve_dry_run();
        let select = if self.select.is_empty() {
            None
        } else {
            Some(self.select.as_slice())
        };
        let urgency = crate::model_select::Urgency::parse(&self.urgency)?;
        let resolved = crate::model_select::resolve_providers(
            &self.providers,
            select,
            urgency,
            n,
            roles,
            cfg,
            dry,
        )?;
        if let Some(art) = &resolved.artifact {
            crate::model_select::write_select_artifact(paths, run_id, art)?;
            let _ = crate::events::append(
                paths,
                run_id,
                &crate::events::Event {
                    ts: chrono::Utc::now(),
                    kind: crate::events::EventKind::Info,
                    phase: None,
                    prev_phase: None,
                    slot: None,
                    status: None,
                    message: Some(format!(
                        "model-select: {}",
                        art.choices
                            .iter()
                            .map(|c| format!("{}→{} ({})", c.vals_id, c.provider, c.profile))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                },
            );
            if !self.json {
                eprintln!(
                    "model-select: {}",
                    art.choices
                        .iter()
                        .map(|c| format!("{}→{}", c.vals_id, c.provider))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Ok(resolved.providers)
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
        WorkflowKind::Review => review::run(opts, paths, cfg),
    }
}
