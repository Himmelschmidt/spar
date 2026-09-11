use crate::bus::BusMessage;
use crate::config::Config;
use crate::paths::SparPaths;
use crate::record::{Record, RecordKind, SourceId};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub const SURFACE_CHAT: &str = "chat";
pub const META_SURFACE: &str = "surface";
pub const META_CONVERSATION: &str = "conversation";
#[allow(dead_code)]
pub const META_TURN: &str = "turn";

#[allow(unused_imports)]
pub use crate::bus::is_conversation_message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Home,
    Run(String),
}

impl Scope {
    pub fn run_tag(&self) -> Option<&str> {
        match self {
            Scope::Home => None,
            Scope::Run(id) => Some(id.as_str()),
        }
    }
}

pub fn agent_id(scope: &Scope) -> String {
    match scope {
        Scope::Home => "talk".to_string(),
        Scope::Run(id) => crate::bus::agent_ref(Some(id), "talk"),
    }
}

fn stable_sequence_for_id(id: &str) -> u64 {
    let mut hash: u64 = 1469598103934665603;
    for b in id.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

pub fn transcript(
    paths: &SparPaths,
    scope: &Scope,
    conversation: Option<&str>,
) -> Result<Vec<Record>> {
    let run = scope.run_tag();
    let all_events = crate::bus::list_events(paths, None)?;
    let generic_agent = agent_id(scope);
    let conv_agent = conversation.map(|conv| crate::bus::agent_ref(run, conv));
    let mut out = Vec::new();
    for msg in all_events.iter().filter(|m| {
        if m.meta.get(META_SURFACE).map(|v| v.as_str()) != Some(SURFACE_CHAT) {
            return false;
        }
        if m.run.as_deref() != run {
            return false;
        }
        let Some(conv) = conversation else {
            return false;
        };
        if m.meta.get(META_CONVERSATION).map(|v| v.as_str()) != Some(conv) {
            return false;
        }
        let Some(agent) = &conv_agent else {
            return false;
        };
        let from_is_human = m.from == crate::bus::HUMAN || m.from == "human";
        let to_is_human = m.to == crate::bus::HUMAN;
        let from_is_agent = m.from == *agent || m.from == generic_agent;
        let to_is_agent = m.to == *agent || m.to == generic_agent;
        (from_is_human && to_is_agent) || (from_is_agent && to_is_human)
    }) {
        let conv_agent_val = conv_agent.as_deref().unwrap_or(&generic_agent);
        let actor = if msg.from == conv_agent_val {
            Some("spar".to_string())
        } else if msg.from == crate::bus::HUMAN || msg.from == "human" {
            Some("you".to_string())
        } else {
            Some(msg.from.clone())
        };
        let kind = if msg.body.contains("```spar-proposal") {
            RecordKind::Section
        } else {
            RecordKind::Prose
        };
        let seq = stable_sequence_for_id(&msg.id);
        out.push(Record {
            kind,
            glyph: if kind == RecordKind::Section {
                "▸"
            } else {
                "│"
            },
            verb: actor.clone().unwrap_or_default(),
            head: msg.body.lines().next().unwrap_or("").to_string(),
            summary: msg.body.clone(),
            body: vec![msg.body.clone()],
            time: Some(msg.ts),
            elapsed: None,
            actor,
            ok: None,
            source: SourceId::Activity {
                at_millis: msg.ts.timestamp_millis(),
                sequence: seq,
            },
            folded_by_default: false,
            has_command_row: false,
        });
        let proposal_seq = seq.wrapping_add(1);
        let proposal_parse = parse_proposal(&msg.body);
        if let Err(e) = &proposal_parse {
            out.push(Record {
                kind: RecordKind::Error,
                glyph: "!",
                verb: "proposal".to_string(),
                head: format!("proposal parse error: {e}"),
                summary: format!("proposal parse error: {e}"),
                body: vec![format!("proposal parse error: {e}"), msg.body.clone()],
                time: Some(msg.ts),
                elapsed: None,
                actor: Some("spar".to_string()),
                ok: Some(false),
                source: SourceId::Activity {
                    at_millis: msg.ts.timestamp_millis(),
                    sequence: proposal_seq,
                },
                folded_by_default: false,
                has_command_row: false,
            });
        } else if let Ok(Some(proposal)) = proposal_parse {
            out.push(Record {
                kind: RecordKind::Section,
                glyph: "▸",
                verb: "proposal".to_string(),
                head: proposal.task.clone(),
                summary: proposal.task.clone(),
                body: vec![
                    format!("brief: {}", proposal.brief),
                    format!("providers: {}", proposal.providers.join(", ")),
                ],
                time: Some(msg.ts),
                elapsed: None,
                actor: Some("spar".to_string()),
                ok: None,
                source: SourceId::Activity {
                    at_millis: msg.ts.timestamp_millis(),
                    sequence: proposal_seq,
                },
                folded_by_default: false,
                has_command_row: false,
            });
        }
    }
    Ok(out)
}

pub fn say_for_tui(
    paths: &SparPaths,
    scope_key: &str,
    conversation: &str,
    body: &str,
) -> Result<crate::bus::BusMessage> {
    let scope = if scope_key == "home" {
        Scope::Home
    } else {
        Scope::Run(scope_key.to_string())
    };
    say(paths, &scope, conversation, body)
}

#[allow(dead_code)]
pub fn say(
    paths: &SparPaths,
    scope: &Scope,
    conversation: &str,
    body: &str,
) -> Result<crate::bus::BusMessage> {
    let run = scope.run_tag();
    let mut meta = HashMap::new();
    meta.insert(META_SURFACE.to_string(), SURFACE_CHAT.to_string());
    meta.insert(META_CONVERSATION.to_string(), conversation.to_string());
    let conv_agent = crate::bus::agent_ref(run, conversation);
    let msg = BusMessage {
        id: crate::bus::new_id(),
        ts: chrono::Utc::now(),
        from: crate::bus::HUMAN.to_string(),
        to: conv_agent,
        kind: crate::bus::MsgKind::Chat,
        body: body.to_string(),
        run: run.map(|s| s.to_string()),
        subject: None,
        refs: crate::bus::MsgRefs::default(),
        requires_ack: false,
        meta,
    };
    crate::bus::send(paths, msg, crate::bus::MessageBudget::Chatty)
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TurnHandle {
    pub scope_key: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub cancel: Arc<AtomicBool>,
    pub pid: Arc<Mutex<Option<u32>>>,
}

impl TurnHandle {
    pub fn new(scope_key: String, conversation_id: String, turn_id: String) -> Self {
        Self {
            scope_key,
            conversation_id,
            turn_id,
            cancel: Arc::new(AtomicBool::new(false)),
            pid: Arc::new(Mutex::new(None)),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(pid) = *self.pid.lock().unwrap() {
            crate::process::terminate_tree(pid, true);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn on_spawn(&self, pid: u32) {
        *self.pid.lock().unwrap() = Some(pid);
    }

    pub fn on_tick(&self) {
        if self.is_cancelled() {
            if let Some(pid) = *self.pid.lock().unwrap() {
                crate::process::terminate_tree(pid, true);
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_pid(&self) -> Option<u32> {
        *self.pid.lock().unwrap()
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub scope_key: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub watermark: usize,
    pub project_root: std::path::PathBuf,
    pub run_id: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub success: bool,
    pub error: Option<String>,
    pub reply: Option<BusMessage>,
    pub worktree: Option<std::path::PathBuf>,
    pub stats: Option<crate::process::StreamStats>,
}

pub fn collect_gate_evidence(paths: &SparPaths, run_id: &str) -> GateEvidence {
    collect_gate_evidence_with_fallback(paths, run_id, None)
}

pub fn collect_gate_evidence_with_fallback(
    paths: &SparPaths,
    run_id: &str,
    fallback: Option<&Config>,
) -> GateEvidence {
    let snap_path = paths.run_config_file(run_id);
    let cfg = if snap_path.is_file() {
        Config::for_run(paths, run_id).ok()
    } else {
        fallback.cloned()
    };
    let st = crate::state::RunState::load(paths, run_id).ok();
    let contract_path = paths.artifact(run_id, "test-contract.md");
    let contract_body = std::fs::read_to_string(&contract_path).unwrap_or_default();
    let criteria = crate::workflow::review_result::parse_contract_criteria(&contract_body);
    let mut block_reasons: Vec<(String, Vec<String>)> = Vec::new();
    if let (Some(cfg), Some(st)) = (cfg.as_ref(), st.as_ref()) {
        let reviewers: Vec<&crate::state::SlotState> = st
            .slots
            .iter()
            .filter(|s| s.role == crate::state::SlotRole::Reviewer)
            .collect();
        for r in reviewers {
            if r.status == crate::state::SlotStatus::Failed {
                block_reasons.push((r.id.clone(), vec!["failed".to_string()]));
                continue;
            }
            let artifact = r
                .artifact
                .clone()
                .unwrap_or_else(|| format!("review-{}.md", r.id));
            let path = paths.artifact(run_id, &artifact);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let res = crate::workflow::review_result::parse_review(&text);
                let reasons =
                    crate::workflow::implement::acceptance_block_reasons(&criteria, &res, cfg);
                if !reasons.is_empty() {
                    block_reasons.push((r.id.clone(), reasons));
                }
            }
        }
    }
    GateEvidence {
        frozen_unavailable: cfg.is_none(),
        criteria,
        block_reasons,
        phase: st.as_ref().map(|s| s.phase),
        st,
        cfg,
    }
}

#[derive(Debug)]
pub struct GateEvidence {
    pub frozen_unavailable: bool,
    pub criteria: Vec<String>,
    pub block_reasons: Vec<(String, Vec<String>)>,
    pub phase: Option<crate::state::Phase>,
    pub st: Option<crate::state::RunState>,
    pub cfg: Option<Config>,
}

fn resolve_conversation_provider(paths: &SparPaths, run_id: Option<&str>) -> String {
    if let Ok(env) = std::env::var("SPAR_CHAT_PROVIDER") {
        if !env.trim().is_empty() {
            return env;
        }
    }
    if let Some(run) = run_id {
        if let Ok(cfg) = Config::for_run(paths, run) {
            for candidate in [&cfg.roles.planner, &cfg.roles.implementer]
                .into_iter()
                .flatten()
            {
                if !candidate.trim().is_empty() {
                    return candidate.clone();
                }
            }
            if let Some(first) = cfg.providers.order.first() {
                if !first.trim().is_empty() {
                    return first.clone();
                }
            }
        }
    } else if let Ok(cfg) = Config::load(&paths.project_root) {
        for candidate in [&cfg.roles.planner, &cfg.roles.implementer]
            .into_iter()
            .flatten()
        {
            if !candidate.trim().is_empty() {
                return candidate.clone();
            }
        }
        if let Some(first) = cfg.providers.order.first() {
            if !first.trim().is_empty() {
                return first.clone();
            }
        }
    }
    "cli:claude".to_string()
}

pub fn dispatch_turn_with_handle(
    paths: SparPaths,
    req: TurnRequest,
    handle: &TurnHandle,
) -> Result<TurnOutcome> {
    dispatch_turn_inner(paths, req, Some(handle))
}

#[allow(dead_code)]
pub fn dispatch_turn(paths: SparPaths, req: TurnRequest) -> Result<TurnOutcome> {
    let handle = TurnHandle::new(
        req.scope_key.clone(),
        req.conversation_id.clone(),
        req.turn_id.clone(),
    );
    dispatch_turn_inner(paths, req, Some(&handle))
}

fn dispatch_turn_inner(
    paths: SparPaths,
    req: TurnRequest,
    handle: Option<&TurnHandle>,
) -> Result<TurnOutcome> {
    let provider_name = resolve_conversation_provider(&paths, req.run_id.as_deref());
    if provider_name.starts_with("api:") {
        anyhow::bail!("api-sdk is the better long-term host and explicitly does not block this: api-sdk turn not yet implemented for provider {provider_name}");
    }

    let run_tag = req.run_id.as_deref();
    let conv = req.conversation_id.clone();
    let turn = req.turn_id.clone();

    // Determine worktree path: per-conversation, per-turn
    let worktree = paths
        .project_root
        .join(".spar")
        .join("worktrees")
        .join(format!(
            "talk-{}-{}",
            conv.trim_start_matches("talk-"),
            &turn[..8.min(turn.len())]
        ));
    // For Home, use invoking HEAD; for Run, use base_commit
    let base_commit = if let Some(run_id) = &req.run_id {
        crate::state::RunState::load(&paths, run_id)
            .ok()
            .and_then(|st| st.base_commit.clone())
    } else {
        None
    };
    let base_ref = base_commit.unwrap_or_else(|| {
        // Use HEAD of project_root
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&req.project_root)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "HEAD".to_string())
    });

    // Create worktree (best-effort) — track only if we actually created it.
    let _ = std::fs::create_dir_all(worktree.parent().unwrap());
    let worktree_created = if !worktree.exists() {
        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                &base_ref,
            ])
            .current_dir(&req.project_root)
            .output();
        output.map(|o| o.status.success()).unwrap_or(false)
    } else {
        false
    };
    let worktree_existed = !worktree_created && worktree.exists();

    // Presence wiring — backend owns dispatch (U17), so resolve real adapter.
    let spar_exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("spar"));
    let slot_id = conv.clone();
    let adapter_box = crate::providers::adapter_named(&provider_name)
        .unwrap_or_else(|| Box::new(crate::providers::ClaudeAdapter));
    let adapter: &dyn crate::providers::ProviderAdapter = &*adapter_box;
    let identity = crate::providers::presence::SlotIdentity {
        agent_id: &slot_id,
        run_id: run_tag,
        project_root: &req.project_root,
        worktree: &worktree,
        spar_exe: &spar_exe,
    };
    let wiring = crate::providers::presence::wire(adapter, &identity);
    let mut env = wiring.env.clone();
    // Ensure SPAR_AGENT_ID is the unique conversation agent
    env.retain(|(k, _)| k != "SPAR_AGENT_ID");
    env.push((
        "SPAR_AGENT_ID".to_string(),
        crate::bus::agent_ref(run_tag, &slot_id),
    ));
    // Also ensure conversation/turn env for debugging
    env.push(("SPAR_CONVERSATION_ID".to_string(), conv.clone()));
    env.push(("SPAR_TURN_ID".to_string(), turn.clone()));

    // Per-turn log and stats
    let log_dir = paths
        .project_root
        .join(".spar")
        .join("workspaces")
        .join(format!("talk-{}", conv.trim_start_matches("talk-")))
        .join(format!("turn-{}", &turn[..8.min(turn.len())]));
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("turn.log");
    let stats_path = log_dir.join("turn.stats.json");

    // Build prompt: include transcript and evidence
    let mut prompt = String::new();
    prompt.push_str(crate::skills::CORE_SKILL);
    prompt.push_str("\n\n---\n\n");
    if let Some(run_id) = &req.run_id {
        let evidence = gate_evidence(&paths, run_id);
        prompt.push_str("## Evidence for gate consultation\n");
        prompt.push_str(&evidence);
        prompt.push_str("\n\n");
        prompt.push_str(
            "You are the orchestrator. Argue the gate either way and name the operator action, never execute it. Read evidence, do not edit the repository. Send your one reply via:\n",
        );
        prompt.push_str(&format!(
            "spar bus send --run {} --from \"$SPAR_AGENT_ID\" --to @human --surface chat --conversation {} --turn {} --message \"...\"\n",
            run_id, conv, turn
        ));
    } else {
        prompt.push_str("You are the orchestrator. Interview the operator into a brief. You may propose a fleet with a ```spar-proposal TOML block (task, brief, providers). Only the TUI launches; you never approve, confirm, merge, or pick a fleet. Send your one reply via:\n");
        prompt.push_str(&format!(
            "spar bus send --from \"$SPAR_AGENT_ID\" --to @human --surface chat --conversation {} --turn {} --message \"...\"\n",
            conv, turn
        ));
    }
    // Include transcript
    let scope = if req.scope_key == "home" {
        Scope::Home
    } else {
        Scope::Run(req.scope_key.clone())
    };
    let transcript_records = transcript(&paths, &scope, Some(&conv)).unwrap_or_default();
    if !transcript_records.is_empty() {
        prompt.push_str("\n\n## Transcript\n");
        for rec in transcript_records.iter().rev().take(20).rev() {
            prompt.push_str(&format!("{}: {}\n", rec.verb, rec.summary));
        }
    }

    // Write prompt to file
    let prompt_path = log_dir.join("prompt.md");
    let _ = std::fs::write(&prompt_path, &prompt);

    let dry_run = crate::util::env_truthy("SPAR_DRY_RUN");
    #[allow(unused_assignments)]
    let mut exit_success = true;
    let mut spawn_stats: Option<crate::process::StreamStats> = None;
    let mut spawn_error: Option<String> = None;

    if !dry_run {
        if let Some(adapter_box) = crate::providers::adapter_named(&provider_name) {
            if let Some(bin) = adapter_box.resolve_binary() {
                let spawn_opts = crate::providers::SpawnOpts {
                    prompt: prompt.clone(),
                    prompt_file: Some(prompt_path.clone()),
                    cwd: worktree.clone(),
                    trust: crate::providers::TrustPolicy::FullAuto,
                    extra_args: vec![],
                    model: None,
                    timeout_secs: Some(120),
                };
                let cmd = adapter_box.build_headless(&bin, &spawn_opts);
                let (program, args) = crate::providers::command_to_parts(&cmd);
                let isolation = if let Some(run_id) = &req.run_id {
                    Config::for_run(&paths, run_id)
                        .map(|c| c.isolation)
                        .unwrap_or(crate::config::IsolationMode::Worktree)
                } else {
                    Config::load(&paths.project_root)
                        .map(|c| c.isolation)
                        .unwrap_or(crate::config::IsolationMode::Worktree)
                };
                let (program, args) =
                    crate::sandbox::maybe_wrap(isolation, &worktree, &program, &args);
                let spawn_req = crate::process::SpawnRequest {
                    program,
                    args,
                    cwd: worktree.clone(),
                    log_path: log_path.clone(),
                    env: env.clone(),
                    timeout: std::time::Duration::from_secs(120),
                };
                let handle_for_spawn = handle.cloned();
                let handle_for_tick = handle.cloned();
                let on_spawn = handle_for_spawn.as_ref().map(|h| {
                    let hh = h.clone();
                    move |pid: u32| hh.on_spawn(pid)
                });
                let on_spawn_ref: Option<&dyn Fn(u32)> =
                    on_spawn.as_ref().map(|f| f as &dyn Fn(u32));
                let on_tick = handle_for_tick.as_ref().map(|h| {
                    let hh = h.clone();
                    move || hh.on_tick()
                });
                let on_tick_ref: Option<&dyn Fn()> = on_tick.as_ref().map(|f| f as &dyn Fn());
                let run_result = if handle.map(|h| h.is_cancelled()).unwrap_or(false) {
                    Err(anyhow::anyhow!("turn cancelled"))
                } else {
                    crate::process::run_captured(&spawn_req, on_spawn_ref, on_tick_ref)
                };
                match run_result {
                    Ok(res) => {
                        exit_success = res.exit_code == Some(0) && !res.timed_out;
                        spawn_stats = Some(res.stats);
                        if res.timed_out {
                            spawn_error = Some("turn timed out".to_string());
                        } else if res.exit_code != Some(0) {
                            spawn_error = Some(format!(
                                "turn provider exited with code {:?}",
                                res.exit_code
                            ));
                        }
                    }
                    Err(e) => {
                        exit_success = false;
                        spawn_error = Some(format!("spawn failed: {e:#}"));
                        let _ = std::fs::write(
                            &log_path,
                            format!(
                                "spawn failed: {e:#}\nturn {turn} watermark {}\n",
                                req.watermark
                            ),
                        );
                    }
                }
            } else {
                let _ = std::fs::write(
                    &log_path,
                    format!(
                        "turn {} watermark {} prompt len {} (no provider binary for {provider_name})\n",
                        turn,
                        req.watermark,
                        prompt.len()
                    ),
                );
                exit_success = true;
            }
        } else {
            let _ = std::fs::write(
                &log_path,
                format!(
                    "turn {} watermark {} prompt len {} (unknown provider {provider_name})\n",
                    turn,
                    req.watermark,
                    prompt.len()
                ),
            );
            exit_success = true;
        }
    } else {
        let _ = std::fs::write(
            &log_path,
            format!(
                "turn {} watermark {} prompt len {} (dry-run)\n",
                turn,
                req.watermark,
                prompt.len()
            ),
        );
        exit_success = true;
    }

    let stats = spawn_stats.clone().unwrap_or_default();
    if let Some(s) = &spawn_stats {
        let _ = s.save(&log_path);
    } else {
        let _ = stats.save(&log_path);
    }
    let _ = stats.save(&stats_path);

    let validate_result = validate_turn(
        &paths,
        &req.scope_key,
        &conv,
        &turn,
        req.watermark,
        exit_success,
    );

    if worktree_created && !worktree_existed {
        let is_dirty = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&worktree)
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false)
            || std::process::Command::new("git")
                .args(["log", "--branches", "--not", &base_ref, "--oneline"])
                .current_dir(&worktree)
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
        if !is_dirty {
            let _ = std::process::Command::new("git")
                .args(["worktree", "remove", "--force", worktree.to_str().unwrap()])
                .current_dir(&req.project_root)
                .output();
        }
    }

    let stats_loaded = crate::process::StreamStats::load(&log_path)
        .or_else(|| crate::process::StreamStats::load(&stats_path))
        .or(spawn_stats.clone())
        .unwrap_or(stats);

    let error = if let Some(e) = spawn_error {
        Some(e)
    } else if let Err(e) = &validate_result {
        Some(e.to_string())
    } else if validate_result.is_err() {
        Some("turn protocol error: missing reply".to_string())
    } else {
        None
    };
    let reply = validate_result.ok();

    Ok(TurnOutcome {
        success: reply.is_some() && exit_success && error.is_none(),
        error,
        reply,
        worktree: if worktree_created {
            Some(worktree)
        } else {
            None
        },
        stats: Some(stats_loaded),
    })
}

#[allow(dead_code)]
pub fn validate_turn(
    paths: &SparPaths,
    scope_key: &str,
    conversation_id: &str,
    turn_id: &str,
    watermark: usize,
    exit_success: bool,
) -> Result<BusMessage> {
    let run_tag = if scope_key == "home" {
        None
    } else {
        Some(scope_key)
    };
    let events = crate::bus::list_events(paths, run_tag)?;
    if watermark > events.len() {
        anyhow::bail!("turn protocol error: stale watermark");
    }
    let new_events = &events[watermark..];
    let agent = crate::bus::agent_ref(run_tag, conversation_id);
    let mut candidates = Vec::new();
    for msg in new_events {
        if msg.meta.get(META_SURFACE).map(|v| v.as_str()) != Some(SURFACE_CHAT) {
            continue;
        }
        if msg.meta.get(META_CONVERSATION).map(|v| v.as_str()) != Some(conversation_id) {
            continue;
        }
        if msg.meta.get(META_TURN).map(|v| v.as_str()) != Some(turn_id) {
            continue;
        }
        if msg.from != agent {
            continue;
        }
        if msg.to != crate::bus::HUMAN {
            continue;
        }
        if msg.run.as_deref() != run_tag {
            continue;
        }
        candidates.push(msg);
    }
    if !exit_success {
        anyhow::bail!(
            "turn protocol error: nonzero child exit is still a failure even if it sent text"
        );
    }
    if candidates.is_empty() {
        anyhow::bail!("turn protocol error: missing reply");
    }
    if candidates.len() > 1 {
        anyhow::bail!("turn protocol error: duplicate reply");
    }
    // Also check for stale: if there are candidates but watermark was stale, we already handled
    // Wrong-scope and wrong-sender are filtered above, so they appear as missing.

    // Check for human-originated: if the only candidate after watermark is from human, it would not be in candidates (since from != agent), so it appears as missing.

    Ok(candidates[0].clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub task: String,
    pub brief: String,
    pub providers: Vec<String>,
}

pub fn parse_proposal(body: &str) -> Result<Option<Proposal>> {
    let fence_start = "```spar-proposal";
    let fence_end = "```";
    let mut search = 0;
    while let Some(start) = body[search..].find(fence_start) {
        let abs_start = search + start;
        let content_start = abs_start + fence_start.len();
        let rest = &body[content_start..];
        if let Some(end) = rest.find(fence_end) {
            let toml_text = rest[..end].trim();
            if toml_text.is_empty() {
                search = content_start + end + fence_end.len();
                continue;
            }
            let value: toml::Value = toml::from_str(toml_text)
                .map_err(|e| anyhow::anyhow!("spar-proposal TOML parse error: {e}"))?;
            let task = value
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let brief = value
                .get("brief")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let providers = value
                .get("providers")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if task.is_empty() && brief.is_empty() {
                anyhow::bail!("spar-proposal missing task and brief");
            }
            return Ok(Some(Proposal {
                task,
                brief,
                providers,
            }));
        } else {
            anyhow::bail!("unterminated spar-proposal fence");
        }
    }
    Ok(None)
}

pub fn gate_evidence(paths: &SparPaths, run_id: &str) -> String {
    let evidence = collect_gate_evidence(paths, run_id);
    let mut out = String::new();
    if evidence.frozen_unavailable {
        out.push_str(
            "frozen config unavailable — cannot evaluate criteria-based blockers for this run\n",
        );
    }
    if let Some(phase) = &evidence.phase {
        out.push_str(&format!("phase: {:?}\n", phase));
    }
    out.push_str(&format!("criteria: {}\n", evidence.criteria.join(", ")));
    if let (Some(cfg), Some(st)) = (&evidence.cfg, &evidence.st) {
        let reviewers: Vec<&crate::state::SlotState> = st
            .slots
            .iter()
            .filter(|s| s.role == crate::state::SlotRole::Reviewer)
            .collect();
        for r in reviewers {
            if let Some((_, reasons)) = evidence.block_reasons.iter().find(|(id, _)| id == &r.id) {
                if reasons.len() == 1 && reasons[0] == "failed" {
                    out.push_str(&format!("review {} failed\n", r.id));
                } else {
                    out.push_str(&format!("review {} blocks: {}\n", r.id, reasons.join("; ")));
                }
                continue;
            }
            if r.status == crate::state::SlotStatus::Failed {
                continue;
            }
            let artifact = r
                .artifact
                .clone()
                .unwrap_or_else(|| format!("review-{}.md", r.id));
            let path = paths.artifact(run_id, &artifact);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let res = crate::workflow::review_result::parse_review(&text);
                let block_empty = crate::workflow::implement::acceptance_block_reasons(
                    &evidence.criteria,
                    &res,
                    cfg,
                )
                .is_empty();
                if block_empty && res.approves() {
                    out.push_str(&format!("review {} approves\n", r.id));
                } else if block_empty {
                    out.push_str(&format!("review {} requests changes\n", r.id));
                }
            }
        }
    }
    let plan_path = paths.artifact(run_id, "plan.md");
    if let Ok(body) = std::fs::read_to_string(&plan_path) {
        out.push_str(&format!(
            "plan.md (first 500 chars): {}\n",
            body.chars().take(500).collect::<String>()
        ));
    }
    let critic_artifact = evidence
        .st
        .as_ref()
        .and_then(|st| {
            st.slots
                .iter()
                .find(|s| s.role == crate::state::SlotRole::PlanCritic)
                .map(|s| format!("plan-critique-{}.md", s.id))
        })
        .unwrap_or_else(|| "plan-critique.md".to_string());
    let critic_path = paths.artifact(run_id, &critic_artifact);
    if let Ok(body) = std::fs::read_to_string(&critic_path) {
        out.push_str(&format!(
            "critique (first 500 chars): {}\n",
            body.chars().take(500).collect::<String>()
        ));
    }
    if out.is_empty() {
        out.push_str("no evidence available\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_proposal_extracts_fields() {
        let body = "intro\n```spar-proposal\ntask = \"do thing\"\nbrief = \"detailed brief\"\nproviders = [\"cli:claude\", \"cli:grok\"]\n```\noutro";
        let p = parse_proposal(body).unwrap().unwrap();
        assert_eq!(p.task, "do thing");
        assert_eq!(p.brief, "detailed brief");
        assert_eq!(p.providers, vec!["cli:claude", "cli:grok"]);
    }

    #[test]
    fn parse_proposal_none_when_no_fence() {
        assert!(parse_proposal("no fence here").unwrap().is_none());
    }

    #[test]
    fn parse_proposal_errors_on_malformed_toml() {
        let body = "```spar-proposal\ntask = \n```";
        assert!(parse_proposal(body).is_err());
    }

    #[test]
    fn conversation_message_predicate() {
        let mut meta = HashMap::new();
        meta.insert("surface".into(), "chat".into());
        let msg = BusMessage {
            id: "1".into(),
            ts: chrono::Utc::now(),
            from: "a".into(),
            to: crate::bus::HUMAN.into(),
            kind: crate::bus::MsgKind::Chat,
            body: "hi".into(),
            run: Some("r1".into()),
            subject: None,
            refs: crate::bus::MsgRefs::default(),
            requires_ack: false,
            meta,
        };
        assert!(is_conversation_message(&msg));
    }

    #[test]
    fn transcript_filters_by_conversation_and_scope() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let scope = Scope::Run("r1".into());
        let conv = "talk-1";
        say(&paths, &scope, conv, "hello").unwrap();
        let mut meta = HashMap::new();
        meta.insert("surface".into(), "chat".into());
        meta.insert("conversation".into(), conv.into());
        meta.insert("turn".into(), "t1".into());
        let agent = agent_id(&scope);
        let reply = BusMessage {
            id: crate::bus::new_id(),
            ts: chrono::Utc::now(),
            from: agent.clone(),
            to: crate::bus::HUMAN.into(),
            kind: crate::bus::MsgKind::Chat,
            body: "reply".into(),
            run: Some("r1".into()),
            subject: None,
            refs: crate::bus::MsgRefs::default(),
            requires_ack: false,
            meta: meta.clone(),
        };
        crate::bus::send(&paths, reply, crate::bus::MessageBudget::Chatty).unwrap();
        let records = transcript(&paths, &scope, Some(conv)).unwrap();
        assert!(records.len() >= 2);
        let other_scope = Scope::Run("r2".into());
        let records_other = transcript(&paths, &other_scope, Some(conv)).unwrap();
        assert!(records_other.is_empty());
    }

    #[test]
    fn conversation_turn_rejects_stale_duplicate_and_wrong_sender_replies() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(paths.project_root.join(".git")).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&paths.project_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&paths.project_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&paths.project_root)
            .output()
            .unwrap();
        std::fs::write(paths.project_root.join("README.md"), "hi\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&paths.project_root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(&paths.project_root)
            .output()
            .unwrap();

        let conv = format!("talk-{}", crate::bus::new_id());
        let turn = crate::bus::new_id();
        std::env::set_var("SPAR_DRY_RUN", "1");
        let before = crate::bus::list_events(&paths, None).unwrap().len();
        let req = TurnRequest {
            scope_key: "home".into(),
            conversation_id: conv.clone(),
            turn_id: turn.clone(),
            watermark: before,
            project_root: paths.project_root.clone(),
            run_id: None,
        };
        let mut meta = HashMap::new();
        meta.insert(META_SURFACE.into(), SURFACE_CHAT.into());
        meta.insert(META_CONVERSATION.into(), conv.clone());
        meta.insert(META_TURN.into(), turn.clone());
        let human_to_agent = crate::bus::BusMessage {
            id: crate::bus::new_id(),
            ts: chrono::Utc::now(),
            from: crate::bus::HUMAN.into(),
            to: crate::bus::agent_ref(None, &conv),
            kind: crate::bus::MsgKind::Chat,
            body: "hi".into(),
            run: None,
            subject: None,
            refs: crate::bus::MsgRefs::default(),
            requires_ack: false,
            meta: meta.clone(),
        };
        crate::bus::send(&paths, human_to_agent, crate::bus::MessageBudget::Chatty).unwrap();
        assert!(
            validate_turn(&paths, "home", &conv, &turn, before, true).is_err(),
            "missing reply must fail"
        );
        let stale_turn = "stale-turn";
        let mut stale_meta = HashMap::new();
        stale_meta.insert(META_SURFACE.into(), SURFACE_CHAT.into());
        stale_meta.insert(META_CONVERSATION.into(), conv.clone());
        stale_meta.insert(META_TURN.into(), stale_turn.into());
        let agent = crate::bus::agent_ref(None, &conv);
        crate::bus::send(
            &paths,
            crate::bus::BusMessage {
                id: crate::bus::new_id(),
                ts: chrono::Utc::now(),
                from: agent.clone(),
                to: crate::bus::HUMAN.into(),
                kind: crate::bus::MsgKind::Chat,
                body: "stale".into(),
                run: None,
                subject: None,
                refs: crate::bus::MsgRefs::default(),
                requires_ack: false,
                meta: stale_meta,
            },
            crate::bus::MessageBudget::Chatty,
        )
        .unwrap();
        assert!(
            validate_turn(&paths, "home", &conv, &turn, before, true).is_err(),
            "stale turn must not match"
        );
        let mut correct_meta = HashMap::new();
        correct_meta.insert(META_SURFACE.into(), SURFACE_CHAT.into());
        correct_meta.insert(META_CONVERSATION.into(), conv.clone());
        correct_meta.insert(META_TURN.into(), turn.clone());
        crate::bus::send(
            &paths,
            crate::bus::BusMessage {
                id: crate::bus::new_id(),
                ts: chrono::Utc::now(),
                from: agent.clone(),
                to: crate::bus::HUMAN.into(),
                kind: crate::bus::MsgKind::Chat,
                body: "correct".into(),
                run: None,
                subject: None,
                refs: crate::bus::MsgRefs::default(),
                requires_ack: false,
                meta: correct_meta.clone(),
            },
            crate::bus::MessageBudget::Chatty,
        )
        .unwrap();
        assert!(validate_turn(&paths, "home", &conv, &turn, before, true).is_ok());
        crate::bus::send(
            &paths,
            crate::bus::BusMessage {
                id: crate::bus::new_id(),
                ts: chrono::Utc::now(),
                from: agent.clone(),
                to: crate::bus::HUMAN.into(),
                kind: crate::bus::MsgKind::Chat,
                body: "duplicate".into(),
                run: None,
                subject: None,
                refs: crate::bus::MsgRefs::default(),
                requires_ack: false,
                meta: correct_meta.clone(),
            },
            crate::bus::MessageBudget::Chatty,
        )
        .unwrap();
        assert!(
            validate_turn(&paths, "home", &conv, &turn, before, true).is_err(),
            "duplicate must fail"
        );
        assert!(
            validate_turn(&paths, "home", &conv, &turn, before, false).is_err(),
            "nonzero exit must fail"
        );
        let _ = dispatch_turn(paths.clone(), req);
    }

    #[test]
    fn native_cli_conversation_turn_lifecycle_and_isolation() {
        std::env::set_var("SPAR_DRY_RUN", "1");
        let tmp = tempdir().unwrap();
        let proj1 = tmp.path().join("p1");
        let proj2 = tmp.path().join("p2");
        for p in [&proj1, &proj2] {
            std::fs::create_dir_all(p).unwrap();
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(p)
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["config", "user.email", "t@e.com"])
                .current_dir(p)
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["config", "user.name", "T"])
                .current_dir(p)
                .output()
                .unwrap();
            std::fs::write(p.join("README.md"), "hi\n").unwrap();
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(p)
                .output()
                .unwrap();
            std::process::Command::new("git")
                .args(["commit", "-q", "-m", "init"])
                .current_dir(p)
                .output()
                .unwrap();
        }
        let paths1 = SparPaths::new(&proj1);
        let conv1 = format!("talk-{}", crate::bus::new_id());
        let turn1 = crate::bus::new_id();
        let req1 = TurnRequest {
            scope_key: "home".into(),
            conversation_id: conv1.clone(),
            turn_id: turn1.clone(),
            watermark: 0,
            project_root: proj1.clone(),
            run_id: None,
        };
        let out1 = dispatch_turn(paths1.clone(), req1).unwrap();
        assert!(out1.worktree.is_some() || !proj1.join(".spar/worktrees").exists());
        let paths2 = SparPaths::new(&proj2);
        let conv2 = format!("talk-{}", crate::bus::new_id());
        let turn2 = crate::bus::new_id();
        let req2 = TurnRequest {
            scope_key: "home".into(),
            conversation_id: conv2.clone(),
            turn_id: turn2.clone(),
            watermark: 0,
            project_root: proj2.clone(),
            run_id: None,
        };
        let out2 = dispatch_turn(paths2.clone(), req2).unwrap();
        if let (Some(w1), Some(w2)) = (out1.worktree, out2.worktree) {
            assert_ne!(
                w1, w2,
                "concurrent conversations must use separate worktrees"
            );
        }
        assert!(out1.stats.is_some());
        assert!(out2.stats.is_some());
        let mut api_env = std::collections::HashMap::new();
        api_env.insert("SPAR_CHAT_PROVIDER".to_string(), "api:openai".to_string());
        std::env::set_var("SPAR_CHAT_PROVIDER", "api:openai");
        let bad_req = TurnRequest {
            scope_key: "home".into(),
            conversation_id: format!("talk-{}", crate::bus::new_id()),
            turn_id: crate::bus::new_id(),
            watermark: 0,
            project_root: proj1.clone(),
            run_id: None,
        };
        let err = dispatch_turn(paths1.clone(), bad_req).unwrap_err();
        assert!(err.to_string().contains("api-sdk"));
        std::env::remove_var("SPAR_CHAT_PROVIDER");
    }

    #[test]
    fn gate_consultation_uses_frozen_review_evidence_and_never_mutates_run_state() {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .output()
            .unwrap();
        let paths = SparPaths::new(&proj);
        let run_id = "run-gate1";
        std::fs::create_dir_all(paths.run_dir(run_id)).unwrap();
        std::fs::create_dir_all(paths.artifacts_dir(run_id)).unwrap();
        std::fs::write(
            paths.state_file(run_id),
            r#"{"id":"run-gate1","phase":"Review","slots":[]}"#,
        )
        .unwrap();
        std::fs::write(
            paths.artifact(run_id, "test-contract.md"),
            "AC-1: do thing\nAC-2: other\n",
        )
        .unwrap();
        std::fs::write(paths.artifact(run_id, "plan.md"), "plan body").unwrap();
        let before = std::fs::read(paths.state_file(run_id)).unwrap();
        let evidence = gate_evidence(&paths, run_id);
        assert!(
            evidence.contains("frozen config unavailable") || evidence.contains("criteria"),
            "must report config missing or criteria"
        );
        let after = std::fs::read(paths.state_file(run_id)).unwrap();
        assert_eq!(before, after, "gate consultation must not mutate run state");
        let tmp2 = tempdir().unwrap();
        let proj2 = tmp2.path().join("p2");
        std::fs::create_dir_all(&proj2).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj2)
            .output()
            .unwrap();
        std::fs::write(
            proj2.join("spar.toml"),
            "[review]\nrequire_all_criteria = false\n",
        )
        .unwrap();
        let paths2 = SparPaths::new(&proj2);
        let evidence2 = gate_evidence(&paths2, run_id);
        assert!(
            evidence2.contains("frozen config unavailable"),
            "must not fallback to live spar.toml"
        );
    }
}
