use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SwarmPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::util;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

pub fn run(task: String, opts: CommonOpts, paths: &SwarmPaths, cfg: &Config) -> Result<ExitCode> {
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("AGENT_SWARM_DRY_RUN", "1");
    }
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Plan,
        paths.project_root.clone(),
    );
    state.task = Some(task.clone());
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.providers = providers::pick_providers(
        &cfg.providers.order,
        2.max(cfg.max_agents.min(3) as usize),
        opts.providers.as_deref(),
        dry,
    );
    if dry && state.providers.is_empty() {
        state.providers = cfg.providers.order.clone();
        if state.providers.is_empty() {
            state.providers = vec!["claude".into(), "grok".into()];
        }
    }
    if !dry {
        match crate::quota::apply_quota_filter(paths, &state.providers) {
            Ok(p) => state.providers = p,
            Err(e) => {
                state.error = Some(e.to_string());
                state.set_phase(Phase::Quota);
                paths.ensure_run_dirs(&state.id)?;
                state.save(paths)?;
                if opts.json {
                    executor::emit_run_json(&state)?;
                } else {
                    eprintln!("error: {e}");
                }
                return Ok(ExitCode::Quota);
            }
        }
    }

    paths.ensure_run_dirs(&state.id)?;
    state.save(paths)?;

    let mut jobs = Vec::new();
    for (i, prov) in state.providers.iter().take(2).enumerate() {
        let id = format!("planner-{prov}");
        let role = if i == 0 {
            SlotRole::Planner
        } else {
            SlotRole::PlanCritic
        };
        let template = if i == 0 { "planner" } else { "plan_critic" };
        state.slots.push(executor::init_slot(&id, prov, role));
        jobs.push(SlotJob {
            slot_id: id,
            provider: prov.clone(),
            role,
            template: template.into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some("plan.md".into()),
        });
    }

    if opts.detach {
        return detach_self(&state, opts.json);
    }

    execute_plan(&mut state, paths, cfg, &jobs)?;
    if opts.json {
        executor::emit_run_json(&state)?;
    } else {
        executor::print_run_human(&state);
        println!("plan: {}", paths.artifact(&state.id, "plan.md").display());
    }
    Ok(state.exit_code())
}

pub fn execute_plan(
    state: &mut RunState,
    paths: &SwarmPaths,
    cfg: &Config,
    jobs: &[SlotJob],
) -> Result<()> {
    let slot_ids: Vec<String> = jobs.iter().map(|j| j.slot_id.clone()).collect();
    worktree::prepare_isolation(state, paths, &slot_ids)?;
    state.set_phase(Phase::SpawnSlots);
    state.save(paths)?;

    state.set_phase(Phase::Dispatch);
    state.save(paths)?;

    for job in jobs {
        if let Err(e) = executor::run_slot(state, paths, cfg, job) {
            if job.role == SlotRole::Planner {
                state.set_phase(Phase::Failed);
                state.error = Some(e.to_string());
                state.save(paths)?;
                return Err(e);
            }
            if let Some(s) = state.slot_mut(&job.slot_id) {
                s.status = crate::state::SlotStatus::Failed;
                s.error = Some(e.to_string());
            }
        }
    }

    let plan_path = paths.artifact(&state.id, "plan.md");
    if !plan_path.is_file() {
        let mut combined = String::from("# Plan\n\n");
        if let Ok(rd) = std::fs::read_dir(paths.artifacts_dir(&state.id)) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("plan-") && name.ends_with(".md") {
                    if let Ok(t) = std::fs::read_to_string(e.path()) {
                        combined.push_str(&t);
                        combined.push_str("\n\n");
                    }
                }
            }
        }
        if combined.trim() == "# Plan" {
            combined.push_str(&format!(
                "## Goal\n{}\n",
                state.task.as_deref().unwrap_or("")
            ));
        }
        std::fs::write(&plan_path, combined)?;
    }

    state.set_phase(Phase::AwaitingPlanApproval);
    state.save(paths)?;
    Ok(())
}

pub fn approve(paths: &SwarmPaths, run_id: &str, json: bool) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase != Phase::AwaitingPlanApproval && state.phase != Phase::PlanRejected {
        anyhow::bail!(
            "run {run_id} is not awaiting plan approval (phase={:?})",
            state.phase
        );
    }
    state.gates.plan_approved = true;
    state.gates.reject_reason = None;
    state.set_phase(Phase::PlanApproved);
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("approved plan for run {run_id}");
        println!("next: agent-swarm implement --run {run_id}");
    }
    Ok(ExitCode::Success)
}

pub fn reject(
    paths: &SwarmPaths,
    run_id: &str,
    reason: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase != Phase::AwaitingPlanApproval {
        anyhow::bail!(
            "run {run_id} is not awaiting plan approval (phase={:?})",
            state.phase
        );
    }
    state.gates.plan_approved = false;
    state.gates.reject_reason = reason.clone();
    state.set_phase(Phase::PlanRejected);
    state.error = reason;
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("rejected plan for run {run_id}");
    }
    Ok(ExitCode::Failure)
}

fn detach_self(state: &RunState, json: bool) -> Result<ExitCode> {
    #[cfg(unix)]
    {
        let mut child_cmd = std::process::Command::new(std::env::current_exe()?);
        child_cmd
            .arg("__internal_continue")
            .arg(&state.id)
            .env("AGENT_SWARM_INTERNAL", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = child_cmd.spawn()?;
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("detach not supported on this platform yet");
    }

    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
        println!("detached; poll with: agent-swarm wait {}", state.id);
    }
    Ok(ExitCode::Success)
}

pub fn continue_run(paths: &SwarmPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    let mut jobs = Vec::new();
    for slot in &state.slots {
        let template = match slot.role {
            SlotRole::Planner => "planner",
            SlotRole::PlanCritic => "plan_critic",
            _ => continue,
        };
        jobs.push(SlotJob {
            slot_id: slot.id.clone(),
            provider: slot.provider.clone(),
            role: slot.role,
            template: template.into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some("plan.md".into()),
        });
    }
    if jobs.is_empty() {
        for (i, prov) in state.providers.iter().take(2).enumerate() {
            let id = format!("planner-{prov}");
            let role = if i == 0 {
                SlotRole::Planner
            } else {
                SlotRole::PlanCritic
            };
            let template = if i == 0 { "planner" } else { "plan_critic" };
            if state.slots.iter().all(|s| s.id != id) {
                state.slots.push(executor::init_slot(&id, prov, role));
            }
            jobs.push(SlotJob {
                slot_id: id,
                provider: prov.clone(),
                role,
                template: template.into(),
                extra_vars: HashMap::new(),
                expected_artifact: Some("plan.md".into()),
            });
        }
        state.save(paths)?;
    }
    execute_plan(&mut state, paths, cfg, &jobs)?;
    Ok(state.exit_code())
}
