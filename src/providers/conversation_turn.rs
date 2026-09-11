use crate::config::IsolationMode;
use crate::process::StreamStats;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ConversationTurnRequest {
    pub provider: String,
    pub prompt: String,
    pub prompt_file: PathBuf,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
    pub isolation: IsolationMode,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ConversationTurnOutcome {
    pub exit_success: bool,
    pub stats: Option<StreamStats>,
    pub error: Option<String>,
}

pub fn dispatch_turn(
    req: ConversationTurnRequest,
    on_spawn: Option<&dyn Fn(u32)>,
    on_tick: Option<&dyn Fn()>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<ConversationTurnOutcome> {
    if req.provider.starts_with("api:") {
        return Ok(ConversationTurnOutcome {
            exit_success: false,
            stats: Some(StreamStats::default()),
            error: Some(format!(
                "api-sdk is the better long-term host and explicitly does not block this: api-sdk turn not yet implemented for provider {}",
                req.provider
            )),
        });
    }

    let Some(adapter_box) = crate::providers::adapter_named(&req.provider) else {
        let _ = std::fs::write(
            &req.log_path,
            format!(
                "turn prompt len {} (unknown provider {})\n",
                req.prompt.len(),
                req.provider
            ),
        );
        return Ok(ConversationTurnOutcome {
            exit_success: false,
            stats: None,
            error: Some(format!("unknown provider {}", req.provider)),
        });
    };

    let Some(bin) = adapter_box.resolve_binary() else {
        let _ = std::fs::write(
            &req.log_path,
            format!(
                "turn prompt len {} (no provider binary for {})\n",
                req.prompt.len(),
                req.provider
            ),
        );
        return Ok(ConversationTurnOutcome {
            exit_success: false,
            stats: None,
            error: Some(format!("no provider binary for {}", req.provider)),
        });
    };

    let spawn_opts = crate::providers::SpawnOpts {
        prompt: req.prompt.clone(),
        prompt_file: Some(req.prompt_file.clone()),
        cwd: req.cwd.clone(),
        trust: crate::providers::TrustPolicy::FullAuto,
        extra_args: vec![],
        model: None,
        timeout_secs: Some(req.timeout.as_secs()),
    };
    let cmd = adapter_box.build_headless(&bin, &spawn_opts);
    let (program, args) = crate::providers::command_to_parts(&cmd);
    let (program, args) = crate::sandbox::maybe_wrap(req.isolation, &req.cwd, &program, &args);
    let spawn_req = crate::process::SpawnRequest {
        program,
        args,
        cwd: req.cwd.clone(),
        log_path: req.log_path.clone(),
        env: req.env.clone(),
        timeout: req.timeout,
    };

    if is_cancelled() {
        return Err(anyhow::anyhow!("turn cancelled"));
    }

    let run_result = crate::process::run_captured(&spawn_req, on_spawn, on_tick);
    match run_result {
        Ok(res) => {
            let exit_success = res.exit_code == Some(0) && !res.timed_out;
            let error = if res.timed_out {
                Some("turn timed out".to_string())
            } else if res.exit_code != Some(0) {
                Some(format!(
                    "turn provider exited with code {:?}",
                    res.exit_code
                ))
            } else {
                None
            };
            Ok(ConversationTurnOutcome {
                exit_success,
                stats: Some(res.stats),
                error,
            })
        }
        Err(e) => {
            let _ = std::fs::write(&req.log_path, format!("spawn failed: {e:#}\n"));
            Ok(ConversationTurnOutcome {
                exit_success: false,
                stats: None,
                error: Some(format!("spawn failed: {e:#}")),
            })
        }
    }
}

#[allow(dead_code)]
pub fn dispatch_turn_dry_run(log_path: &Path, prompt_len: usize) -> ConversationTurnOutcome {
    let _ = std::fs::write(
        log_path,
        format!("turn prompt len {prompt_len} (dry-run)\n"),
    );
    ConversationTurnOutcome {
        exit_success: true,
        stats: None,
        error: None,
    }
}
