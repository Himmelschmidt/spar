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
    pub fn resolve(name: &str) -> Result<Self> {
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "openai" => Ok(Self {
                name: "openai".into(),
                base_url: env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                api_key: env::var("OPENAI_API_KEY")
                    .context("OPENAI_API_KEY required for api:openai")?,
                model: env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            }),
            "xai" => Ok(Self {
                name: "xai".into(),
                base_url: env::var("XAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.x.ai/v1".into()),
                api_key: env::var("XAI_API_KEY").context("XAI_API_KEY required for api:xai")?,
                model: env::var("XAI_MODEL").unwrap_or_else(|_| "grok-3-mini".into()),
            }),
            "anthropic" => {
                // OpenAI-compatible gateways only for v0; native Anthropic later
                Ok(Self {
                    name: "anthropic".into(),
                    base_url: env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| {
                        env::var("OPENAI_BASE_URL")
                            .unwrap_or_else(|_| "https://api.openai.com/v1".into())
                    }),
                    api_key: env::var("ANTHROPIC_API_KEY")
                        .or_else(|_| env::var("OPENAI_API_KEY"))
                        .context("ANTHROPIC_API_KEY or OPENAI_API_KEY required")?,
                    model: env::var("ANTHROPIC_MODEL")
                        .unwrap_or_else(|_| "claude-3-5-sonnet-latest".into()),
                })
            }
            "google" | "meta" => Ok(Self {
                name: name.clone(),
                base_url: env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                api_key: env::var("OPENAI_API_KEY")
                    .context("OPENAI_API_KEY required for OpenAI-compatible provider")?,
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

pub fn chat_completion(cfg: &ApiProviderConfig, messages: &[ChatMessage]) -> Result<ChatResult> {
    let url = format!(
        "{}/chat/completions",
        cfg.base_url.trim_end_matches('/')
    );
    let body = json!({
        "model": cfg.model,
        "messages": messages,
        "temperature": 0.2,
    });
    let resp = ureq::post(&url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .send_json(&body)
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp
        .into_body()
        .read_to_string()
        .context("read response body")?;
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
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
