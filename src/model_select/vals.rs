//! Fetch and parse vals.ai benchmark pages.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const BENCH_URLS: &[(&str, &str)] = &[
    ("swebench", "https://www.vals.ai/benchmarks/swebench"),
    ("terminal-bench-2-1", "https://www.vals.ai/benchmarks/terminal-bench-2-1"),
    ("lcb", "https://www.vals.ai/benchmarks/lcb"),
    ("vibe-code", "https://www.vals.ai/benchmarks/vibe-code"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchSnapshot {
    pub bench: String,
    pub source: String,
    pub url: String,
    pub fetched_at: String,
    pub models: Vec<ModelScore>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelScore {
    pub id: String,
    pub accuracy: f64,
    pub latency: f64,
    /// USD per test on vals harness; 0 if missing.
    pub cost_per_test: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_label: Option<String>,
}

pub fn bench_url(bench: &str) -> Result<&'static str> {
    BENCH_URLS
        .iter()
        .find(|(k, _)| *k == bench)
        .map(|(_, u)| *u)
        .with_context(|| {
            format!(
                "unknown bench '{bench}' (supported: {})",
                BENCH_URLS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

pub fn fetch_bench(bench: &str) -> Result<BenchSnapshot> {
    let url = bench_url(bench)?;
    let resp = ureq::get(url)
        .header("User-Agent", "spar-model-select/0.0.1")
        .call()
        .with_context(|| format!("GET {url}"))?;
    let body = resp
        .into_body()
        .read_to_string()
        .with_context(|| format!("read body {url}"))?;

    let unescaped = html_unescape(&body);
    let models = parse_overall_rsc(&unescaped).with_context(|| {
        format!("parse overall scores from {url} (vals page shape may have changed)")
    })?;
    if models.is_empty() {
        bail!("parsed zero models from {url}");
    }

    Ok(BenchSnapshot {
        bench: bench.to_string(),
        source: "vals".into(),
        url: url.to_string(),
        fetched_at: chrono::Utc::now().to_rfc3339(),
        models,
        stale: false,
        stale_reason: None,
    })
}

/// Parse vals Next.js flight payload for `tasks.overall` model scores.
pub fn parse_overall_rsc(text: &str) -> Result<Vec<ModelScore>> {
    let marker = "\"overall\":[0,{";
    let start = text
        .find(marker)
        .with_context(|| "vals payload missing \"overall\" block")?;
    let rest = &text[start + marker.len()..];
    let end = find_matching_brace(rest).context("unclosed overall object in vals payload")?;
    let body = &rest[..end];

    let mut by_id = parse_model_objects(body);
    if by_id.is_empty() {
        by_id = parse_model_objects(text);
    }

    let mut models: Vec<ModelScore> = by_id.into_values().collect();
    models.sort_by(|a, b| {
        b.accuracy
            .partial_cmp(&a.accuracy)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(models)
}

fn parse_model_objects(text: &str) -> HashMap<String, ModelScore> {
    let mut by_id: HashMap<String, ModelScore> = HashMap::new();
    let key = "\":[0,{\"accuracy\":[0,";
    let mut pos = 0;
    while let Some(rel) = text[pos..].find(key) {
        let abs = pos + rel;
        // model id is "lab/name" immediately before key
        let before = &text[..abs];
        let Some(q) = before.rfind('"') else {
            pos = abs + key.len();
            continue;
        };
        let id = &before[q + 1..];
        if !valid_model_id(id) {
            pos = abs + key.len();
            continue;
        }

        let mut cursor = abs + key.len();
        let Some(accuracy) = take_number(text, &mut cursor) else {
            pos = abs + key.len();
            continue;
        };
        if !text[cursor..].starts_with("],\"latency\":[0,") {
            pos = abs + key.len();
            continue;
        }
        cursor += "],\"latency\":[0,".len();
        let Some(latency) = take_number(text, &mut cursor) else {
            pos = abs + key.len();
            continue;
        };
        // skip through stderr to cost_per_test
        let rest = &text[cursor..];
        let cost_key = "\"cost_per_test\":[0,";
        let Some(cost_rel) = rest.find(cost_key) else {
            pos = abs + key.len();
            continue;
        };
        cursor += cost_rel + cost_key.len();
        let cost_per_test = if text[cursor..].starts_with("null") {
            0.0
        } else {
            let mut c = cursor;
            match take_number(text, &mut c) {
                Some(v) => v,
                None => {
                    pos = abs + key.len();
                    continue;
                }
            }
        };

        if !by_id.contains_key(id) {
            by_id.insert(
                id.to_string(),
                ModelScore {
                    id: id.to_string(),
                    accuracy,
                    latency,
                    cost_per_test,
                    provider_label: id.split('/').next().map(|s| s.to_string()),
                },
            );
        }
        pos = abs + key.len();
    }
    by_id
}

fn valid_model_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 96 {
        return false;
    }
    if !id.contains('/') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ' ' | '(' | ')'))
}

fn take_number(text: &str, cursor: &mut usize) -> Option<f64> {
    let rest = &text[*cursor..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != '+' && c != 'e' && c != 'E')
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let s = &rest[..end];
    let v: f64 = s.parse().ok()?;
    *cursor += end;
    Some(v)
}

fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 1i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_overall() {
        let body = r#"{"tasks":[0,{"overall":[0,{"openai/gpt-x":[0,{"accuracy":[0,90.0],"latency":[0,10.0],"stderr":[0,1.0],"cost_per_test":[0,0.5],"provider":[0,"OpenAI"]}],"anthropic/claude-y":[0,{"accuracy":[0,80.0],"latency":[0,20.0],"stderr":[0,1.0],"cost_per_test":[0,null],"provider":[0,"Anthropic"]}]}]}]}"#;
        let models = parse_overall_rsc(body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openai/gpt-x");
        assert!((models[0].cost_per_test - 0.5).abs() < 1e-9);
        assert_eq!(models[1].cost_per_test, 0.0);
    }
}
