pub mod arena;
pub mod implement;
pub mod peer;
pub mod plan;
pub mod review;
pub mod review_result;
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
    /// Explicit provider list (required unless `select` is set).
    pub providers: Vec<String>,
    /// vals-backed profile list (`value` / `best` / `fast` / `auto` / multi).
    pub select: Vec<String>,
    pub urgency: String,
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
            providers: Vec::new(),
            select: Vec::new(),
            urgency: "normal".into(),
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

    #[allow(dead_code)]
    pub fn require_providers(&self) -> Result<&[String]> {
        if self.providers.is_empty() {
            anyhow::bail!(
                "--providers is required (e.g. --providers cli:claude), or use --select <profile>"
            );
        }
        for p in &self.providers {
            crate::provider_ref::ProviderRef::parse(p)?;
        }
        Ok(&self.providers)
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
