use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::util;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

pub fn run(task: String, opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
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
    state.autonomy = cfg.autonomy;
    state.message_budget = cfg.message_budget;
    state.big = opts.big;
    let requested = opts.require_providers()?;
    state.providers = providers::pick_providers(
        requested,
        2.max(cfg.max_agents.min(3) as usize),
        Some(requested),
        dry,
    );
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

    if state.providers.is_empty() {
        state.error = Some("no usable providers".into());
        state.set_phase(Phase::Failed);
        paths.ensure_run_dirs(&state.id)?;
        state.save(paths)?;
        if opts.json {
            executor::emit_run_json(&state)?;
        } else {
            eprintln!("error: no usable providers");
        }
        return Ok(ExitCode::Failure);
    }
    if !opts.json {
        eprintln!(
            "providers: {}{}",
            state.providers.join(", "),
            if dry {
                " (dry-run: no git worktrees; agents stubbed)"
            } else {
                ""
            }
        );
    }

    paths.ensure_run_dirs(&state.id)?;
    let _ = crate::bus::ensure_bus(paths, &state.id);
    let _ = crate::bus::join(paths, &state.id, "orchestrator", None, None);
    state.save(paths)?;

    let mut jobs = Vec::new();
    for (i, prov) in state.providers.iter().take(2).enumerate() {
        let safe = prov.replace(['/', ':'], "-");
        let (id, role, template) = if i == 0 {
            (format!("planner-{safe}"), SlotRole::Planner, "planner")
        } else {
            (
                format!("critic-{safe}"),
                SlotRole::PlanCritic,
                "plan_critic",
            )
        };
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
    paths: &SparPaths,
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

    if state.big {
        if let Ok(body) = std::fs::read_to_string(&plan_path) {
            let _ = crate::tasks::seed_from_plan(paths, &state.id, &body);
        }
    }

    if cfg.auto_plan() {
        state.gates.plan_approved = true;
        state.set_phase(Phase::PlanApproved);
        let _ = crate::bus::broadcast(
            paths,
            &state.id,
            "orchestrator",
            "plan auto-approved (autonomy)",
            state.message_budget,
        );
    } else {
        state.set_phase(Phase::AwaitingPlanApproval);
    }
    state.save(paths)?;
    Ok(())
}

pub fn approve(paths: &SparPaths, run_id: &str, json: bool) -> Result<ExitCode> {
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
        println!("next: spar implement --run {run_id}  (same run id)");
    }
    let _ = crate::bus::broadcast(
        paths,
        run_id,
        "human",
        "plan approved",
        crate::bus::MessageBudget::Normal,
    );
    Ok(ExitCode::Success)
}

pub fn reject(
    paths: &SparPaths,
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
            .env("SPAR_INTERNAL", "1")
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
        println!("detached; poll with: spar wait {}", state.id);
    }
    Ok(ExitCode::Success)
}

pub fn continue_run(paths: &SparPaths, cfg: &Config, run_id: &str) -> Result<ExitCode> {
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
            let safe = prov.replace(['/', ':'], "-");
            let (id, role, template) = if i == 0 {
                (format!("planner-{safe}"), SlotRole::Planner, "planner")
            } else {
                (
                    format!("critic-{safe}"),
                    SlotRole::PlanCritic,
                    "plan_critic",
                )
            };
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
