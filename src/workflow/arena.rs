use super::CommonOpts;
use crate::config::Config;
use crate::executor::{self, SlotJob};
use crate::exit_codes::ExitCode;
use crate::paths::SparPaths;
use crate::providers;
use crate::state::{Phase, RunState, SlotRole, SlotStatus};
use crate::util;
use crate::worktree;
use anyhow::Result;
use std::collections::HashMap;

pub fn run(opts: CommonOpts, paths: &SparPaths, cfg: &Config) -> Result<ExitCode> {
    let task = opts
        .task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--task required for arena"))?;
    let dry = opts.resolve_dry_run();
    if dry {
        std::env::set_var("SPAR_DRY_RUN", "1");
    }
    let n = cfg.max_agents.max(2) as usize;
    let run_id = util::short_run_id();
    let mut state = RunState::new(
        run_id,
        crate::cli::WorkflowKind::Arena,
        paths.project_root.clone(),
    );
    state.task = Some(task);
    state.backend = opts.backend;
    state.isolation = cfg.isolation;
    state.dry_run = dry;
    state.providers =
        providers::pick_providers(&cfg.providers.order, n, opts.providers.as_deref(), dry);
    if dry && state.providers.len() < n {
        let base: Vec<String> = vec!["claude".into(), "grok".into(), "agy".into()];
        state.providers = (0..n).map(|i| base[i % base.len()].clone()).collect();
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

    for (i, prov) in state.providers.iter().enumerate() {
        let id = format!("arena-{i}-{prov}");
        state
            .slots
            .push(executor::init_slot(id, prov, SlotRole::Implementer));
    }
    let rank_p = state
        .providers
        .iter()
        .next_back()
        .cloned()
        .unwrap_or_else(|| "claude".into());
    state.slots.push(executor::init_slot(
        format!("ranker-{rank_p}"),
        rank_p,
        SlotRole::Ranker,
    ));

    paths.ensure_run_dirs(&state.id)?;
    state.save(paths)?;

    if opts.detach {
        return detach(&state, opts.json);
    }

    execute(&mut state, paths, cfg)?;
    if opts.json {
        executor::emit_run_json(&state)?;
    } else {
        executor::print_run_human(&state);
        if let Some(w) = &state.winner_slot {
            println!("winner: {w} (confirm before ship)");
        }
    }
    Ok(state.exit_code())
}

pub fn execute(state: &mut RunState, paths: &SparPaths, cfg: &Config) -> Result<()> {
    let impl_ids: Vec<String> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .map(|s| s.id.clone())
        .collect();
    worktree::prepare_isolation(state, paths, &impl_ids)?;

    state.set_phase(Phase::Dispatch);
    state.save(paths)?;
    let implementers: Vec<_> = state
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Implementer)
        .cloned()
        .collect();

    // Waves of up to max_agents. Dry-run uses sequential run_slot (shared state).
    // Live mode still uses run_slot sequentially within a wave for reliable state/logging;
    // concurrency cap is enforced by wave size so we never exceed max_agents in-flight
    // once a true parallel spawn path is used. For now, wave iteration documents the cap
    // and dry-run completes all slots; live spawns one-at-a-time within the wave.
    let cap = cfg.max_agents.max(1) as usize;
    for wave in implementers.chunks(cap) {
        // Prefer parallel when dry_run: each slot's artifacts are independent.
        // run_slot mutates state, so we still serialize the call — but we process a full
        // wave before ranking, matching the arena "N implementers then rank" contract.
        if state.dry_run && wave.len() > 1 {
            // Fire-and-join via threads that only write role artifacts, then mark state.
            let task = state.task.clone().unwrap_or_default();
            let run_id = state.id.clone();
            let paths_c = paths.clone();
            std::thread::scope(|scope| {
                for slot in wave {
                    let slot = slot.clone();
                    let task = task.clone();
                    let run_id = run_id.clone();
                    let paths_c = paths_c.clone();
                    let cwd = slot
                        .cwd
                        .clone()
                        .unwrap_or_else(|| paths_c.project_root.clone());
                    scope.spawn(move || {
                        let _ = std::fs::write(
                            cwd.join(".spar-dry-implement"),
                            format!("arena dry-run {} : {task}\n", slot.id),
                        );
                        let _ = std::fs::write(
                            paths_c.artifact(&run_id, &format!("summary-{}.md", slot.id)),
                            format!("# Summary ({})\n\n{task}\n", slot.id),
                        );
                        let _ = crate::markers::write_done(&paths_c, &run_id, &slot.id);
                        let log = paths_c.log_file(&run_id, &slot.id);
                        let _ = crate::process::run_mock(
                            &crate::process::SpawnRequest {
                                program: std::path::PathBuf::from("dry-run"),
                                args: vec![],
                                cwd,
                                log_path: log,
                                env: vec![],
                                timeout: std::time::Duration::from_secs(1),
                            },
                            &format!("arena dry-run {}", slot.id),
                        );
                    });
                }
            });
            for slot in wave {
                if let Some(s) = state.slot_mut(&slot.id) {
                    s.status = SlotStatus::Done;
                    s.exit_code = Some(0);
                    s.backend = Some("dry-run".into());
                }
            }
            state.save(paths)?;
        } else {
            for slot in wave {
                let job = SlotJob {
                    slot_id: slot.id.clone(),
                    provider: slot.provider.clone(),
                    role: SlotRole::Implementer,
                    template: "implementer".into(),
                    extra_vars: HashMap::new(),
                    expected_artifact: Some(format!("summary-{}.md", slot.id)),
                };
                if let Err(e) = executor::run_slot(state, paths, cfg, &job) {
                    if let Some(s) = state.slot_mut(&slot.id) {
                        s.status = SlotStatus::Failed;
                        s.error = Some(e.to_string());
                    }
                }
            }
        }
    }

    state.set_phase(Phase::Rank);
    state.save(paths)?;
    let mut candidates = String::new();
    for slot in &implementers {
        let sum = paths.artifact(&state.id, &format!("summary-{}.md", slot.id));
        let body = std::fs::read_to_string(sum).unwrap_or_else(|_| "(missing summary)".into());
        candidates.push_str(&format!("### {}\n{body}\n\n", slot.id));
    }
    let ranker = state
        .slots
        .iter()
        .find(|s| s.role == SlotRole::Ranker)
        .cloned();
    if let Some(ranker) = ranker {
        let root = state.project_root.clone();
        if let Some(s) = state.slot_mut(&ranker.id) {
            s.cwd = Some(root);
        }
        let mut extra = HashMap::new();
        extra.insert("candidates".into(), candidates);
        let job = SlotJob {
            slot_id: ranker.id.clone(),
            provider: ranker.provider.clone(),
            role: SlotRole::Ranker,
            template: "ranker".into(),
            extra_vars: extra,
            expected_artifact: Some("ranking.md".into()),
        };
        let _ = executor::run_slot(state, paths, cfg, &job);
    }

    let winner = parse_winner(paths, &state.id, &implementers);
    state.winner_slot = winner;
    state.set_phase(Phase::AwaitingWinnerConfirm);
    state.save(paths)?;
    Ok(())
}

fn parse_winner(
    paths: &SparPaths,
    run_id: &str,
    implementers: &[crate::state::SlotState],
) -> Option<String> {
    let wpath = paths.artifact(run_id, "winner.json");
    if wpath.is_file() {
        if let Ok(text) = std::fs::read_to_string(&wpath) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(s) = v.get("winner_slot").and_then(|x| x.as_str()) {
                    return Some(s.to_string());
                }
            }
        }
    }
    implementers.first().map(|s| s.id.clone())
}

pub fn confirm_winner(
    paths: &SparPaths,
    run_id: &str,
    slot: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let mut state = RunState::load(paths, run_id)?;
    if state.phase != Phase::AwaitingWinnerConfirm {
        anyhow::bail!(
            "run {run_id} not awaiting winner confirm (phase={:?})",
            state.phase
        );
    }
    let winner = slot
        .or_else(|| state.winner_slot.clone())
        .ok_or_else(|| anyhow::anyhow!("no winner to confirm"))?;
    state.gates.winner_confirmed = Some(winner.clone());
    state.winner_slot = Some(winner.clone());
    state.set_phase(Phase::AwaitingShipConfirm);
    state.save(paths)?;
    if json {
        executor::emit_run_json(&state)?;
    } else {
        println!("confirmed winner {winner} for run {run_id}");
    }
    Ok(ExitCode::Success)
}

fn detach(state: &RunState, json: bool) -> Result<ExitCode> {
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
    if json {
        executor::emit_run_json(state)?;
    } else {
        executor::print_run_human(state);
    }
    Ok(ExitCode::Success)
}
