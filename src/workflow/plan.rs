use super::CommonOpts;
use crate::bus::{self, MessageBudget, MsgKind, MsgRefs};
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole};
use crate::util::{self, sanitize_slot};
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
    let n_slots = if cfg.spec.enabled { 3 } else { 2 };
    let roles: &[&str] = if cfg.spec.enabled {
        &["planner", "critic", "tester"]
    } else {
        &["planner", "critic"]
    };
    let requested = opts.resolve_fleet(n_slots, roles, paths, cfg, &state.id)?;
    state.providers = providers::pick_providers(&requested, n_slots, Some(&requested), dry);
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
    let _ = bus::ensure_bus(paths);
    let _ = bus::join(paths, Some(&state.id), "orchestrator", None, None);
    state.save(paths)?;

    let art = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten();
    let mut jobs = Vec::new();
    for (i, prov) in state.providers.iter().take(2).enumerate() {
        let safe = sanitize_slot(prov);
        let role_name = if i == 0 { "planner" } else { "critic" };
        let model = art.as_ref().and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.role.as_deref() == Some(role_name) || c.slot == i)
                .and_then(|c| c.model.clone())
        });
        let (id, role, template) = if i == 0 {
            (format!("planner-{safe}"), SlotRole::Planner, "planner")
        } else {
            (
                format!("critic-{safe}"),
                SlotRole::PlanCritic,
                "plan_critic",
            )
        };
        state
            .slots
            .push(executor::init_slot_model(&id, prov, role, model.clone()));
        jobs.push(SlotJob {
            slot_id: id,
            provider: prov.clone(),
            role,
            template: template.into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some("plan.md".into()),
            model,
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
        let contract = paths.artifact(&state.id, "test-contract.md");
        if contract.is_file() {
            println!("tests: {}", contract.display());
        }
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

    if cfg.spec.enabled {
        if let Err(e) = run_test_author(state, paths, cfg) {
            if state.phase != Phase::Failed {
                state.set_phase(Phase::Failed);
                state.error = Some(e.to_string());
                let _ = state.save(paths);
            }
            return Err(e);
        }
    }

    if cfg.auto_plan() {
        state.gates.plan_approved = true;
        state.set_phase(Phase::PlanApproved);
        let _ = bus::broadcast(
            paths,
            Some(&state.id),
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

fn run_test_author(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    let planner_slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Planner)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| "planner".into());
    let critic_slot = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::PlanCritic)
        .map(|s| s.id.clone())
        .unwrap_or_else(|| "critic".into());

    let used: Vec<String> = state
        .slots
        .iter()
        .filter(|s| matches!(s.role, SlotRole::Planner | SlotRole::PlanCritic))
        .map(|s| s.provider.clone())
        .collect();
    let provider = resolve_spec_provider(cfg, state.dry_run, &state.providers, &used)?;
    let model = crate::model_select::load_select_artifact(paths, &state.id)
        .ok()
        .flatten()
        .and_then(|a| {
            a.choices
                .iter()
                .find(|c| c.role.as_deref() == Some("tester") || c.slot == 2)
                .and_then(|c| c.model.clone())
        });
    let safe = sanitize_slot(&provider);
    let id = format!("test-author-{safe}");

    if state.slots.iter().all(|s| s.id != id) {
        state.slots.push(executor::init_slot_model(
            &id,
            &provider,
            SlotRole::TestAuthor,
            model.clone(),
        ));
    }
    worktree::prepare_isolation(state, paths, std::slice::from_ref(&id))?;
    // After isolation so status/TUI show Spec for the author wall-clock, not PrepareIsolation.
    state.set_phase(Phase::Spec);
    state.save(paths)?;

    let _ = bus::join(paths, Some(&state.id), &id, Some(&provider), None);
    seed_spec_bus(state, paths, &id, &planner_slot, &critic_slot)?;

    let mut extra = HashMap::new();
    extra.insert("planner_slot".into(), planner_slot);
    extra.insert("critic_slot".into(), critic_slot);
    let job = SlotJob {
        slot_id: id.clone(),
        provider,
        role: SlotRole::TestAuthor,
        template: "test_author".into(),
        extra_vars: extra,
        expected_artifact: Some("test-contract.md".into()),
        model,
    };

    if let Err(e) = executor::run_slot(state, paths, cfg, &job) {
        state.set_phase(Phase::Failed);
        state.error = Some(format!("test-author failed: {e}"));
        state.save(paths)?;
        return Err(e);
    }

    let contract = paths.artifact(&state.id, "test-contract.md");
    if !contract.is_file()
        || std::fs::metadata(&contract)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
    {
        let msg = "test-author finished without test-contract.md";
        state.set_phase(Phase::Failed);
        state.error = Some(msg.into());
        state.save(paths)?;
        anyhow::bail!("{msg}");
    }

    let _ = bus::broadcast(
        paths,
        Some(&state.id),
        "orchestrator",
        "test-author finished; acceptance contract ready for plan approval",
        state.message_budget,
    );
    Ok(())
}

fn seed_spec_bus(
    state: &RunState,
    paths: &SparPaths,
    author_id: &str,
    planner_slot: &str,
    critic_slot: &str,
) -> Result<()> {
    let budget = state.message_budget;
    let body = format!(
        "Spec phase: `{author_id}` will freeze acceptance tests from plan.md. \
         Planner `{planner_slot}` and critic `{critic_slot}`: reply on bus if still available; \
         otherwise the author uses plan + critique artifacts."
    );
    let _ = bus::broadcast(paths, Some(&state.id), "orchestrator", &body, budget);

    for (to, note) in [
        (
            author_id,
            format!(
                "You are the test author. Coordinate with `{planner_slot}` and `{critic_slot}` via bus, then write tests + test-contract.md."
            ),
        ),
        (
            planner_slot,
            format!("Test author `{author_id}` is writing acceptance tests. Answer bus questions if you can."),
        ),
        (
            critic_slot,
            format!("Test author `{author_id}` is freezing the test bar. Challenge weak scenarios on the bus if you can."),
        ),
    ] {
        let _ = bus::send(
            paths,
            bus::BusMessage {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                ts: chrono::Utc::now(),
                from: "orchestrator".into(),
                to: to.into(),
                kind: MsgKind::Hello,
                body: note,
                run: Some(state.id.clone()),
                subject: Some("spec".into()),
                refs: MsgRefs {
                    artifact: Some("plan.md".into()),
                    ..Default::default()
                },
                requires_ack: false,
                meta: HashMap::new(),
            },
            budget,
        );
    }
    Ok(())
}

/// Spec provider: config override, then fleet provider not used by planner/critic, then cycle.
fn resolve_spec_provider(
    cfg: &Config,
    dry: bool,
    fleet: &[String],
    used: &[String],
) -> Result<String> {
    if let Some(p) = &cfg.spec.provider {
        crate::provider_ref::ProviderRef::parse(p)
            .map_err(|e| anyhow::anyhow!("invalid spec.provider {p:?}: {e}"))?;
        if dry || providers::is_provider_usable(p, false) {
            return Ok(p.clone());
        }
        // Fall through to fleet if override is unusable (missing CLI / paused).
    }
    if dry {
        if let Some(p) = fleet.iter().find(|p| !used.contains(p)) {
            return Ok(p.clone());
        }
        if let Some(p) = fleet.get(2) {
            return Ok(p.clone());
        }
        if let Some(p) = fleet.last() {
            return Ok(p.clone());
        }
        return Ok("cli:claude".into());
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| !used.contains(p) && providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok(p);
    }
    if let Some(p) = fleet
        .iter()
        .find(|p| providers::is_provider_usable(p, false))
        .cloned()
    {
        return Ok(p);
    }
    anyhow::bail!(
        "spec.enabled but no usable test-author provider (set [spec].provider or pass more --providers)"
    )
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
        println!("approved plan (+ acceptance contract if present) for run {run_id}");
        println!("next: spar implement --run {run_id}  (same run id)");
    }
    let _ = bus::broadcast(
        paths,
        Some(run_id),
        "human",
        "plan approved",
        MessageBudget::Normal,
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
    let _lock = crate::runlock::RunLock::acquire(paths, run_id)?;
    let mut state = RunState::load(paths, run_id)?;
    let mut jobs = Vec::new();
    for slot in &state.slots {
        let template = match slot.role {
            SlotRole::Planner => "planner",
            SlotRole::PlanCritic => "plan_critic",
            // Test author is spawned after plan draft inside execute_plan.
            SlotRole::TestAuthor => continue,
            _ => continue,
        };
        jobs.push(SlotJob {
            slot_id: slot.id.clone(),
            provider: slot.provider.clone(),
            role: slot.role,
            template: template.into(),
            extra_vars: HashMap::new(),
            expected_artifact: Some("plan.md".into()),
            model: None,
        });
    }
    if jobs.is_empty() {
        for (i, prov) in state.providers.iter().take(2).enumerate() {
            let safe = sanitize_slot(prov);
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
                model: None,
            });
        }
        state.save(paths)?;
    }
    execute_plan(&mut state, paths, cfg, &jobs)?;
    Ok(state.exit_code())
}
