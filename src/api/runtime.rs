//! Thin in-tree agent runtime for api-sdk slots.
use super::openai_compat::{self, ApiProviderConfig, ChatMessage};
use crate::paths::SparPaths;
use anyhow::{bail, Context, Result};
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
    /// Override model from env default (model-select).
    pub model_override: Option<String>,
}

pub fn run_api_slot(req: &ApiSlotRequest<'_>) -> Result<(bool, Option<String>, Usage)> {
    let mut usage = Usage {
        provider: Some(format!("api:{}", req.provider_name)),
        ..Default::default()
    };

    if req.dry_run {
        append_log(
            req.log_path,
            &format!("api dry-run provider={}\n", req.provider_name),
        )?;
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

    let mut cfg = ApiProviderConfig::resolve(req.provider_name)?;
    if let Some(m) = &req.model_override {
        if !m.is_empty() {
            cfg.model = m.clone();
        }
    }
    usage.model = Some(cfg.model.clone());

    let system = format!(
        "You are a coding agent running in spar (api-sdk backend).\n\
         Working directory: {}\n\
         Use tools via a single JSON object on its own line when you need them:\n\
         {{\"tool\":\"read\",\"path\":\"relative/path\"}}\n\
         {{\"tool\":\"write\",\"path\":\"...\",\"content\":\"...\"}}\n\
         {{\"tool\":\"bash\",\"cmd\":\"...\"}}\n\
         {{\"tool\":\"list\",\"path\":\".\"}}\n\
         Paths must be relative to the worktree (no .. or absolute paths).\n\
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
            if !artifact_ok(req) {
                return Ok((
                    false,
                    Some("missing expected artifact after FINAL".into()),
                    usage,
                ));
            }
            return Ok((true, None, usage));
        }

        if let Some(tool_line) = extract_tool_json(&result.content) {
            let observation = match run_tool(req.cwd, &tool_line) {
                Ok(o) => o,
                Err(e) => format!("tool error: {e:#}"),
            };
            append_log(req.log_path, &format!("tool => {observation}\n"))?;
            messages.push(ChatMessage {
                role: "user".into(),
                content: format!("Tool result:\n{observation}"),
            });
            continue;
        }

        if step + 1 == max_steps {
            if !artifact_ok(req) {
                return Ok((
                    false,
                    Some("api slot finished without expected artifact".into()),
                    usage,
                ));
            }
            return Ok((true, None, usage));
        }
        messages.push(ChatMessage {
            role: "user".into(),
            content: "Continue. Use a tool JSON line if needed, or end with FINAL: summary."
                .into(),
        });
    }
    if !artifact_ok(req) {
        return Ok((
            false,
            Some("api slot finished without expected artifact".into()),
            usage,
        ));
    }
    Ok((true, None, usage))
}

fn artifact_ok(req: &ApiSlotRequest<'_>) -> bool {
    match req.expected_artifact {
        None => true,
        Some(art) => {
            art.is_file() && fs::metadata(art).map(|m| m.len() > 0).unwrap_or(false)
        }
    }
}

fn extract_tool_json(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('{') && t.contains("\"tool\"") {
            return Some(t.to_string());
        }
    }
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
            let p = confined_path(cwd, call.path.as_deref().unwrap_or("."))?;
            let data = fs::read_to_string(&p).unwrap_or_else(|e| format!("error: {e}"));
            Ok(truncate(&data, 8000))
        }
        "write" => {
            let p = confined_path(cwd, call.path.as_deref().unwrap_or("out.txt"))?;
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&p, call.content.as_deref().unwrap_or(""))?;
            Ok(format!("wrote {}", p.display()))
        }
        "list" => {
            let p = confined_path(cwd, call.path.as_deref().unwrap_or("."))?;
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
            if cmd_looks_dangerous(cmd) {
                bail!("bash command rejected by safety filter");
            }
            let out = Command::new("bash")
                .arg("-lc")
                .arg(cmd)
                .current_dir(cwd)
                .env_remove("LD_PRELOAD")
                .output()
                .context("bash")?;
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(truncate(&s, 8000))
        }
        other => Ok(format!("unknown tool {other}")),
    }
}

fn cmd_looks_dangerous(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    let needles = [
        "rm -rf /",
        "mkfs",
        ":(){",
        "/etc/passwd",
        "curl ",
        "wget ",
        "nc ",
        "ncat ",
        "> /dev/",
    ];
    needles.iter().any(|n| lower.contains(n))
}

/// Resolve a path that must stay inside `cwd` (no `..`, no absolute escape).
fn confined_path(cwd: &Path, path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        bail!("empty path");
    }
    let p = Path::new(path);
    if p.is_absolute() {
        bail!("absolute paths not allowed");
    }
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::ParentDir => bail!("'..' not allowed in paths"),
            Component::RootDir | Component::Prefix(_) => bail!("invalid path"),
            _ => {}
        }
    }
    let joined = cwd.join(p);
    let cwd_canon = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    // canonicalize may fail if file doesn't exist yet (write); check parent
    let check = if joined.exists() {
        fs::canonicalize(&joined).unwrap_or(joined.clone())
    } else if let Some(parent) = joined.parent() {
        let parent_c = if parent.exists() {
            fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf())
        } else {
            // ensure parent stays under cwd by components already
            parent.to_path_buf()
        };
        if !parent_c.starts_with(&cwd_canon) && parent != cwd {
            // for new nested dirs under cwd, prefix check on joined string path
            let joined_norm = normalize_dotdot(&joined);
            if !joined_norm.starts_with(&cwd_canon) && !joined.starts_with(cwd) {
                bail!("path escapes worktree");
            }
        }
        joined.clone()
    } else {
        joined.clone()
    };
    if check.exists() {
        let c = fs::canonicalize(&check).unwrap_or(check);
        if !c.starts_with(&cwd_canon) {
            bail!("path escapes worktree");
        }
        Ok(c)
    } else if !joined.starts_with(cwd) {
        bail!("path escapes worktree");
    } else {
        Ok(joined)
    }
}

fn normalize_dotdot(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        use std::path::Component;
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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
        return s.to_string();
    }
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i < n)
        .last()
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

#[allow(dead_code)]
fn _paths_touch(_: &SparPaths) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_parent_dir() {
        let tmp = tempdir().unwrap();
        assert!(confined_path(tmp.path(), "../etc/passwd").is_err());
        assert!(confined_path(tmp.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn allows_relative() {
        let tmp = tempdir().unwrap();
        let p = confined_path(tmp.path(), "src/foo.rs").unwrap();
        assert!(p.starts_with(tmp.path()));
    }

    #[test]
    fn truncate_utf8() {
        let s = "你好世界abc";
        let t = truncate(s, 5);
        assert!(!t.is_empty());
    }
}
