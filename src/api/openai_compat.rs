//! OpenAI-compatible Chat Completions (OpenAI, xAI, many proxies).
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;

#[derive(Debug, Clone)]
pub struct ApiProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl ApiProviderConfig {
    /// Credentials and endpoints resolve from the process environment unless
    /// the call carries its own: an explicit key or base URL wins over the env,
    /// so a test injects credentials through the request, never process env.
    pub fn resolve_with(name: &str, api_key: Option<&str>, base_url: Option<&str>) -> Result<Self> {
        let key = |env_name: &str| {
            api_key
                .map(str::to_string)
                .or_else(|| env::var(env_name).ok())
                .context(format!("{env_name} required"))
        };
        let base = |env_name: &str, fallback: &str| {
            base_url
                .map(str::to_string)
                .or_else(|| env::var(env_name).ok())
                .unwrap_or_else(|| fallback.into())
        };
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "openai" => Ok(Self {
                name: "openai".into(),
                base_url: base("OPENAI_BASE_URL", "https://api.openai.com/v1"),
                api_key: key("OPENAI_API_KEY")?,
                model: env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            }),
            "xai" => Ok(Self {
                name: "xai".into(),
                base_url: base("XAI_BASE_URL", "https://api.x.ai/v1"),
                api_key: key("XAI_API_KEY")?,
                model: env::var("XAI_MODEL").unwrap_or_else(|_| "grok-3-mini".into()),
            }),
            "anthropic" => {
                // OpenAI-compatible gateways only for v0; native Anthropic later
                Ok(Self {
                    name: "anthropic".into(),
                    base_url: base_url
                        .map(str::to_string)
                        .or_else(|| {
                            env::var("ANTHROPIC_BASE_URL")
                                .or_else(|_| env::var("OPENAI_BASE_URL"))
                                .ok()
                        })
                        .unwrap_or_else(|| "https://api.openai.com/v1".into()),
                    api_key: api_key
                        .map(str::to_string)
                        .or_else(|| {
                            env::var("ANTHROPIC_API_KEY")
                                .or_else(|_| env::var("OPENAI_API_KEY"))
                                .ok()
                        })
                        .context("ANTHROPIC_API_KEY or OPENAI_API_KEY required")?,
                    model: env::var("ANTHROPIC_MODEL")
                        .unwrap_or_else(|_| "claude-3-5-sonnet-latest".into()),
                })
            }
            "google" | "meta" => Ok(Self {
                name: name.clone(),
                base_url: base("OPENAI_BASE_URL", "https://api.openai.com/v1"),
                api_key: key("OPENAI_API_KEY")?,
                model: env::var("SPAR_API_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            }),
            other => bail!("unknown api provider '{other}' (openai|xai|anthropic|google|meta)"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct ChatResult {
    pub content: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageJson>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Msg,
}

#[derive(Debug, Deserialize)]
struct Msg {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageJson {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

pub fn chat_completion(
    cfg: &ApiProviderConfig,
    messages: &[ChatMessage],
    timeout: std::time::Duration,
) -> Result<ChatResult> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "messages": messages,
        "temperature": 0.2,
    });
    // Bound the whole request by the slot's remaining ceiling: the step loop
    // only checks between calls, so one hung call would otherwise overrun it.
    // One attempt, never re-sent: an interrupted read may already have billed
    // a completion server-side. The mask keeps child churn from failing it.
    let _mask = crate::util::block_sigchld();
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build(),
    );
    let resp = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .send_json(&body)
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp
        .into_body()
        .read_to_string()
        .with_context(|| format!("read POST {url} body"))?;
    if !status.is_success() {
        bail!("api {} status {status}: {}", cfg.name, truncate(&text, 400));
    }
    let parsed: ChatResponse = serde_json::from_str(&text).context("parse chat response")?;
    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.clone())
        .unwrap_or_default();
    let (input_tokens, output_tokens) = parsed
        .usage
        .map(|u| (u.prompt_tokens, u.completion_tokens))
        .unwrap_or((0, 0));
    Ok(ChatResult {
        content,
        input_tokens,
        output_tokens,
    })
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
