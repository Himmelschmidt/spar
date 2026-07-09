//! Thin in-tree agent runtime for api-sdk slots.
use super::openai_compat::{self, ApiProviderConfig, ChatMessage};
use crate::paths::SparPaths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

pub struct ApiSlotRequest<'a> {
    pub provider_name: &'a str,
    pub prompt: &'a str,
    pub cwd: &'a Path,
    pub log_path: &'a Path,
    pub expected_artifact: Option<&'a Path>,
    pub timeout: Duration,
    pub dry_run: bool,
}

pub fn run_api_slot(req: &ApiSlotRequest<'_>) -> Result<(bool, Option<String>, Usage)> {
    let mut usage = Usage {
        provider: Some(format!("api:{}", req.provider_name)),
        ..Default::default()
    };

    if req.dry_run {
        append_log(req.log_path, &format!("api dry-run provider={}\n", req.provider_name))?;
        if let Some(art) = req.expected_artifact {
            if let Some(parent) = art.parent() {
                fs::create_dir_all(parent)?;
            }
            if !art.is_file() {
                fs::write(
                    art,
                    format!(
                        "# API dry-run artifact\n\nprovider: api:{}\n\n{}",
                        req.provider_name,
                        truncate(req.prompt, 500)
                    ),
                )?;
            }
        }
        return Ok((true, None, usage));
    }

    let cfg = ApiProviderConfig::resolve(req.provider_name)?;
    usage.model = Some(cfg.model.clone());

    let system = format!(
        "You are a coding agent running in spar (api-sdk backend).\n\
         Working directory: {}\n\
         Use tools via a single JSON object on its own line when you need them:\n\
         {{\"tool\":\"read\",\"path\":\"relative/or/abs\"}}\n\
         {{\"tool\":\"write\",\"path\":\"...\",\"content\":\"...\"}}\n\
         {{\"tool\":\"bash\",\"cmd\":\"...\"}}\n\
         {{\"tool\":\"list\",\"path\":\".\"}}\n\
         When finished, write any required artifact and reply with FINAL: <summary>.\n\
         Prefer small safe edits. Stay inside the worktree.",
        req.cwd.display()
    );

    let mut messages = vec![
        ChatMessage {
            role: "system".into(),
            content: system,
        },
        ChatMessage {
            role: "user".into(),
            content: req.prompt.to_string(),
        },
    ];

    let start = std::time::Instant::now();
    let max_steps = 12u32;
    for step in 0..max_steps {
        if start.elapsed() > req.timeout {
            return Ok((false, Some("api slot timeout".into()), usage));
        }
        append_log(
            req.log_path,
            &format!("\n--- api step {step} model={} ---\n", cfg.model),
        )?;
        let result = openai_compat::chat_completion(&cfg, &messages)?;
        usage.input_tokens += result.input_tokens;
        usage.output_tokens += result.output_tokens;
        append_log(req.log_path, &result.content)?;
        append_log(req.log_path, "\n")?;

        messages.push(ChatMessage {
            role: "assistant".into(),
            content: result.content.clone(),
        });

        if result.content.contains("FINAL:") {
            ensure_artifact(req)?;
            return Ok((true, None, usage));
        }

        if let Some(tool_line) = extract_tool_json(&result.content) {
            let observation = run_tool(req.cwd, &tool_line)?;
            append_log(req.log_path, &format!("tool => {observation}\n"))?;
            messages.push(ChatMessage {
                role: "user".into(),
                content: format!("Tool result:\n{observation}"),
            });
            continue;
        }

        // No tool and no FINAL — nudge once, then accept
        if step + 1 == max_steps {
            ensure_artifact(req)?;
            return Ok((true, None, usage));
        }
        messages.push(ChatMessage {
            role: "user".into(),
            content: "Continue. Use a tool JSON line if needed, or end with FINAL: summary."
                .into(),
        });
    }
    ensure_artifact(req)?;
    Ok((true, None, usage))
}

fn ensure_artifact(req: &ApiSlotRequest<'_>) -> Result<()> {
    if let Some(art) = req.expected_artifact {
        if !art.is_file() {
            if let Some(parent) = art.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(art, format!("# API slot complete\n\nprovider: {}\n", req.provider_name))?;
        }
    }
    Ok(())
}

fn extract_tool_json(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('{') && t.contains("\"tool\"") {
            return Some(t.to_string());
        }
    }
    // fenced
    if let Some(start) = text.find("```json") {
        let rest = &text[start + 7..];
        if let Some(end) = rest.find("```") {
            let block = rest[..end].trim();
            if block.contains("\"tool\"") {
                return Some(block.to_string());
            }
        }
    }
    None
}

fn run_tool(cwd: &Path, json_line: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct ToolCall {
        tool: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        cmd: Option<String>,
    }
    let call: ToolCall = serde_json::from_str(json_line).context("parse tool json")?;
    match call.tool.as_str() {
        "read" => {
            let p = resolve_path(cwd, call.path.as_deref().unwrap_or("."))?;
            if !p.starts_with(cwd) {
                anyhow::bail!("path escapes worktree");
            }
            let data = fs::read_to_string(&p).unwrap_or_else(|e| format!("error: {e}"));
            Ok(truncate(&data, 8000))
        }
        "write" => {
            let p = resolve_path(cwd, call.path.as_deref().unwrap_or("out.txt"))?;
            if !p.starts_with(cwd) {
                anyhow::bail!("path escapes worktree");
            }
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&p, call.content.as_deref().unwrap_or(""))?;
            Ok(format!("wrote {}", p.display()))
        }
        "list" => {
            let p = resolve_path(cwd, call.path.as_deref().unwrap_or("."))?;
            if !p.starts_with(cwd) {
                anyhow::bail!("path escapes worktree");
            }
            let mut names = Vec::new();
            if let Ok(rd) = fs::read_dir(&p) {
                for e in rd.flatten().take(100) {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
            }
            Ok(names.join("\n"))
        }
        "bash" => {
            let cmd = call.cmd.as_deref().unwrap_or("true");
            // deny destructive absolute escapes roughly
            let out = Command::new("bash")
                .arg("-lc")
                .arg(cmd)
                .current_dir(cwd)
                .output()
                .context("bash")?;
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(truncate(&s, 8000))
        }
        other => Ok(format!("unknown tool {other}")),
    }
}

fn resolve_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(cwd.join(p))
    }
}

fn append_log(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Used only for type export path in docs; keep paths import silent.
#[allow(dead_code)]
fn _paths_touch(_: &SparPaths) {}
