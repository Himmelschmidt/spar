//! Advisory typed classifier for slot failures (O148). The regexes and typed signals
//! decide first; this is asked only where spar would otherwise read a failure as the
//! agent's own work, and its answer is used only above a probability threshold. Any
//! error leaves spar's own verdict in place.

use crate::config::ClassifierConfig;
use crate::paths::SparPaths;
use crate::providers::failure_question::{self, Questions, QUESTION_ID};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

const LOG_TAIL_BYTES: usize = 4096;
const MAX_RETRIES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    QuotaOrRateLimit,
    Auth,
    TransientInfra,
    KilledByOperatorOrSignal,
    GenuineFailure,
    Other,
}

/// What spar's own rules concluded before the classifier was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegexVerdict {
    Work,
    /// Quota read from log or error text, not from a typed event.
    Quota,
    /// Auth read from log or error text.
    Auth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    None,
    /// Re-dispatch through the transient retry loop.
    Retry,
    /// Strong quota pause, then the backup path.
    PauseQuota,
    /// The O104 auth path.
    Auth,
    /// Veto: a text-read quota or auth verdict becomes a weak pause.
    WeakPause,
}

/// The failure as the classifier sees it. Field order is the wire order.
#[derive(Debug, Clone, Serialize)]
pub struct FailureState {
    pub provider: String,
    pub role: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub tool_calls: Option<u32>,
    pub spar_error: Option<String>,
    pub log_tail: String,
}

impl FailureState {
    pub fn new(
        provider: &str,
        role: &str,
        exit_code: Option<i32>,
        signal: Option<i32>,
        tool_calls: Option<u32>,
        spar_error: Option<&str>,
        log_path: &Path,
    ) -> Self {
        Self {
            provider: provider.to_string(),
            role: role.to_string(),
            exit_code,
            signal,
            tool_calls,
            spar_error: spar_error.map(|e| no_panic(|| redact(e))),
            log_tail: no_panic(|| log_tail(&crate::process::tail_log(log_path, LOG_TAIL_BYTES))),
        }
    }
}

/// Redaction runs on untrusted text in the orchestrator thread: a bug in it must cost
/// the classifier its input, never the run.
fn no_panic(f: impl FnOnce() -> String) -> String {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub choice: FailureClass,
    pub probability: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
    pub cached: bool,
}

/// One consultation, as written to `events.jsonl` and the slot's `classifier` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probabilities: BTreeMap<String, f64>,
    pub acted: bool,
    pub action: Action,
    pub regex_verdict: RegexVerdict,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where the classifier is asked, which bounds what it may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// Inside the dispatch, before the retries: a transient answer may take the retry
    /// loop (only when its own gate of at least one tool call holds) and an auth
    /// answer the O104 short-wait retry.
    Dispatch { tool_calls: bool },
    /// The failed dispatch's final routing: quota, auth and the veto.
    Verdict,
}

/// The rule from the evaluation: fill a "work" gap at `threshold`, veto a text-read
/// quota or auth verdict at `veto_threshold`.
pub fn decide(cfg: &ClassifierConfig, site: Site, regex: RegexVerdict, answer: &Answer) -> Action {
    if regex != RegexVerdict::Work {
        let veto = site == Site::Verdict
            && answer.choice == FailureClass::GenuineFailure
            && answer.probability >= cfg.veto_threshold;
        return if veto {
            Action::WeakPause
        } else {
            Action::None
        };
    }
    if answer.probability < cfg.threshold {
        return Action::None;
    }
    match (site, answer.choice) {
        (Site::Dispatch { tool_calls: true }, FailureClass::TransientInfra) => Action::Retry,
        (Site::Verdict, FailureClass::QuotaOrRateLimit) => Action::PauseQuota,
        (_, FailureClass::Auth) => Action::Auth,
        _ => Action::None,
    }
}

/// Asks the classifier, decides, and appends the `classifier` event. `None` when the
/// classifier is off; otherwise the record, whose `action` is `None` on any error.
#[allow(clippy::too_many_arguments)]
pub fn consult(
    cfg: &ClassifierConfig,
    paths: &SparPaths,
    run_id: &str,
    slot_id: &str,
    adapter: &str,
    state: &FailureState,
    site: Site,
    regex: RegexVerdict,
) -> Option<Record> {
    if !cfg.enabled {
        return None;
    }
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        classify(cfg, adapter, state)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("classifier panicked")));
    let record = match outcome {
        Ok(answer) => {
            let action = decide(cfg, site, regex, &answer);
            Record {
                model: cfg.model.clone(),
                choice: Some(answer.choice),
                probability: Some(answer.probability),
                probabilities: answer.probabilities,
                acted: action != Action::None,
                action,
                regex_verdict: regex,
                latency_ms: answer.latency_ms,
                cost_usd: answer.cost_usd,
                cached: answer.cached,
                error: None,
            }
        }
        Err(e) => Record {
            model: cfg.model.clone(),
            choice: None,
            probability: None,
            probabilities: BTreeMap::new(),
            acted: false,
            action: Action::None,
            regex_verdict: regex,
            latency_ms: started.elapsed().as_millis() as u64,
            cost_usd: None,
            cached: false,
            error: Some(format!("{e:#}")),
        },
    };
    let _ = crate::events::append(
        paths,
        run_id,
        &crate::events::Event::classifier(slot_id, record.clone()),
    );
    Some(record)
}

/// Where the key comes from, for `doctor`. Never returns the key.
pub fn key_source(cfg: &ClassifierConfig) -> Result<String> {
    read_key(cfg).map(|_| match &cfg.key_file {
        Some(p) => p.display().to_string(),
        None => "OPENROUTER_API_KEY".to_string(),
    })
}

fn read_key(cfg: &ClassifierConfig) -> Result<String> {
    let key = match &cfg.key_file {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("read classifier key_file {}", p.display()))?,
        None => std::env::var("OPENROUTER_API_KEY")
            .context("no classifier key: set [classifier].key_file or OPENROUTER_API_KEY")?,
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("classifier key is empty");
    }
    Ok(key)
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    state: &'a FailureState,
    questions: Questions,
}

#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, WireAnswer>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireAnswer {
    #[serde(rename = "type")]
    kind: String,
    choice: String,
    probabilities: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    cost: Option<f64>,
}

type Memo = std::sync::Mutex<std::collections::HashMap<u64, std::result::Result<Answer, String>>>;

/// Answers (and failures) by request, so the dispatch and verdict sites asking about
/// the same log pay for one call. Keyed on what the answer depends on: the model, the
/// endpoint, the vendor's criteria, the exit, the signal and the log tail.
fn memo() -> &'static Memo {
    static MEMO: std::sync::OnceLock<Memo> = std::sync::OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// While open, no call is made to that endpoint: an outage costs one failed consult
/// per `BREAKER_OPEN`, not one per failed dispatch.
const BREAKER_OPEN: Duration = Duration::from_secs(300);

type Breaker = std::sync::Mutex<std::collections::HashMap<String, Instant>>;

fn breaker() -> &'static Breaker {
    static BREAKER: std::sync::OnceLock<Breaker> = std::sync::OnceLock::new();
    BREAKER.get_or_init(Default::default)
}

fn breaker_open(base_url: &str) -> bool {
    breaker()
        .lock()
        .ok()
        .and_then(|m| m.get(base_url).copied())
        .is_some_and(|until| Instant::now() < until)
}

fn trip_breaker(base_url: &str) {
    if let Ok(mut m) = breaker().lock() {
        m.insert(base_url.to_string(), Instant::now() + BREAKER_OPEN);
    }
}

fn memo_key(cfg: &ClassifierConfig, adapter: &str, state: &FailureState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        &cfg.model,
        &cfg.base_url,
        adapter,
        state.exit_code,
        state.signal,
        &state.log_tail,
    )
        .hash(&mut h);
    h.finish()
}

/// One System One call (`POST <base_url>/systemone`, pi-ai's `typesafe-system-one`
/// shape). Retries a transport error, 408, 409, 429 or 5xx while the total budget
/// (`timeout_secs` + 1) lasts, at most `MAX_RETRIES` times. A transport failure opens
/// the breaker for this endpoint.
pub fn classify(cfg: &ClassifierConfig, adapter: &str, state: &FailureState) -> Result<Answer> {
    let key = memo_key(cfg, adapter, state);
    if let Some(hit) = memo().lock().ok().and_then(|m| m.get(&key).cloned()) {
        return match hit {
            Ok(a) => Ok(Answer {
                cached: true,
                latency_ms: 0,
                cost_usd: None,
                ..a
            }),
            Err(e) => bail!("{e} (cached)"),
        };
    }
    if breaker_open(&cfg.base_url) {
        bail!("classifier skipped: endpoint failed within the last 5 minutes");
    }
    let result = call(cfg, adapter, state);
    if let Ok(mut m) = memo().lock() {
        m.insert(
            key,
            result
                .as_ref()
                .map(Clone::clone)
                .map_err(|e| format!("{e:#}")),
        );
    }
    result
}

fn call(cfg: &ClassifierConfig, adapter: &str, state: &FailureState) -> Result<Answer> {
    let body = serde_json::to_string(&Request {
        model: &cfg.model,
        state,
        questions: failure_question::questions(adapter),
    })?;
    let api_key = read_key(cfg)?;
    let url = format!("{}/systemone", cfg.base_url.trim_end_matches('/'));
    let per_attempt = Duration::from_secs(cfg.timeout_secs);
    let budget = per_attempt + Duration::from_secs(1);
    let started = Instant::now();
    let mut attempt = 0;
    let text = loop {
        let left = budget.saturating_sub(started.elapsed());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(per_attempt.min(left)))
            .http_status_as_error(false)
            .build()
            .into();
        let sent = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("X-OpenRouter-Title", "spar")
            .send(&body);
        let (reason, transport) = match sent {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.into_body().read_to_string().unwrap_or_default();
                if (200..300).contains(&status) {
                    break text;
                }
                if !matches!(status, 408 | 409 | 429 | 500..=599) {
                    bail!(
                        "classifier returned HTTP {status}: {}",
                        truncate(&text, 200)
                    );
                }
                (format!("HTTP {status}"), false)
            }
            Err(e) => (e.to_string(), true),
        };
        let backoff = Duration::from_millis(250 << attempt);
        let out_of_time = started.elapsed() + backoff + Duration::from_millis(250) >= budget;
        if attempt >= MAX_RETRIES || out_of_time {
            if transport {
                trip_breaker(&cfg.base_url);
            }
            bail!(
                "classifier unreachable after {} attempts: {reason}",
                attempt + 1
            );
        }
        std::thread::sleep(backoff);
        attempt += 1;
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    let parsed: Response = serde_json::from_str(&text).context("parse classifier response")?;
    let wire = parsed
        .answers
        .get(QUESTION_ID)
        .context("classifier returned no failure_class answer")?;
    if wire.kind != "choice" {
        bail!("classifier returned a {} answer, not a choice", wire.kind);
    }
    let choice: FailureClass =
        serde_json::from_value(serde_json::Value::String(wire.choice.clone()))
            .with_context(|| format!("classifier returned unknown label {:?}", wire.choice))?;
    let probability = wire.probabilities.get(&wire.choice).copied().unwrap_or(0.0);
    Ok(Answer {
        choice,
        probability,
        probabilities: wire.probabilities.clone(),
        cost_usd: parsed.usage.and_then(|u| u.cost),
        latency_ms,
        cached: false,
    })
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The last `LOG_TAIL_BYTES` of `text`, redacted. The cut comes first and drops the
/// partial line (or word) it lands in, so no secret is redacted whole and then cut, or
/// cut so its recognizable start is lost; redaction can only lengthen a line slightly,
/// so the result is trimmed again from the front on line boundaries.
pub fn log_tail(text: &str) -> String {
    let cut = tail_bytes(text, LOG_TAIL_BYTES);
    let cut = if cut.len() < text.len() {
        drop_partial_head(cut)
    } else {
        cut
    };
    let mut red = redact(cut);
    while red.len() > LOG_TAIL_BYTES {
        match red.find('\n') {
            Some(i) => red = red[i + 1..].to_string(),
            None => {
                red = tail_bytes(&red, LOG_TAIL_BYTES).to_string();
                break;
            }
        }
    }
    red
}

fn tail_bytes(text: &str, n: usize) -> &str {
    let mut start = text.len().saturating_sub(n);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn drop_partial_head(cut: &str) -> &str {
    let at = match cut.find('\n') {
        Some(i) if i < 1024 => i,
        _ => cut.find(char::is_whitespace).unwrap_or(cut.len()),
    };
    let rest = &cut[at..];
    &rest[rest.chars().next().map_or(0, char::len_utf8)..]
}

const REDACTED: &str = "<redacted>";

const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_live_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AKIA",
    "ASIA",
    "AIza",
    "ya29.",
    "hf_",
    "npm_",
];

/// Strips what looks like a secret: the value of any key or flag whose name says
/// secret (`*password*`, `*passwd`, `*pwd`, `*token`, `*secret*`, `*_key`, `*-key`,
/// `*apikey`, `authorization`), bearer tokens, URL passwords (database
/// URLs), JWTs, vendor-prefixed keys and long base64 or hex runs. Errs toward removing
/// too much. Works on chars, never byte offsets, so no input can make it panic.
pub fn redact(text: &str) -> String {
    text.split('\n')
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let chars = redact_url_passwords(&chars);
    let chars = redact_keyed_values(&chars);
    redact_tokens(&chars)
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '`')
}

fn ends_value(c: char) -> bool {
    c.is_whitespace() || is_quote(c) || matches!(c, ',' | ';' | '&' | '}' | ')' | ']' | '<' | '>')
}

/// `scheme://user:pass@host` keeps the user and host.
fn redact_url_passwords(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let scheme_end = i + 3 <= chars.len() && chars[i..i + 3] == [':', '/', '/'];
        if !scheme_end {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        out.extend_from_slice(&chars[i..i + 3]);
        i += 3;
        let start = i;
        while i < chars.len()
            && !(chars[i].is_whitespace() || chars[i] == '/' || is_quote(chars[i]))
        {
            i += 1;
        }
        let authority = &chars[start..i];
        let at = authority.iter().rposition(|&c| c == '@');
        let colon = authority.iter().position(|&c| c == ':');
        match (at, colon) {
            (Some(at), Some(colon)) if colon < at => {
                out.extend_from_slice(&authority[..=colon]);
                out.extend(REDACTED.chars());
                out.extend_from_slice(&authority[at..]);
            }
            _ => out.extend_from_slice(authority),
        }
    }
    out
}

fn secret_name(ident: &str) -> bool {
    let k = ident.trim_start_matches('-').to_ascii_lowercase();
    let k = k.trim_end_matches(['.', '-', '_']);
    if k.is_empty() {
        return false;
    }
    k.contains("password")
        || k.contains("passwd")
        || k.contains("secret")
        || k.contains("apikey")
        || k.contains("api_key")
        || k.contains("api-key")
        || k.contains("private_key")
        || k == "pwd"
        || k.ends_with("_pwd")
        || k == "token"
        || k.ends_with("token")
        || k.ends_with("_key")
        || k.ends_with("-key")
        || k == "authorization"
        || k == "bearer"
}

/// `NAME=value`, `NAME: value`, `"name": "value"`, `--name value`, `--name=value` and
/// `Bearer value`, for a secret-sounding name. A quoted value is taken to its closing
/// quote.
fn redact_keyed_values(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if !is_ident(chars[i]) || (i > 0 && is_ident(chars[i - 1])) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_ident(chars[i]) {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();
        out.extend_from_slice(&chars[start..i]);
        if !secret_name(&ident) {
            continue;
        }
        let flag = ident.starts_with("--");
        let bearer = ident.eq_ignore_ascii_case("bearer");
        let mut j = i;
        while j < chars.len() && is_quote(chars[j]) {
            j += 1;
        }
        while j < chars.len() && matches!(chars[j], ' ' | '\t') {
            j += 1;
        }
        let sep = j < chars.len() && matches!(chars[j], '=' | ':');
        if sep {
            j += 1;
        } else if !(flag || bearer) || j == i {
            continue;
        }
        while j < chars.len() && matches!(chars[j], ' ' | '\t') {
            j += 1;
        }
        let quote = (j < chars.len() && is_quote(chars[j])).then(|| chars[j]);
        if quote.is_some() {
            j += 1;
        }
        let scheme: String = chars[j..chars.len().min(j + 7)].iter().collect();
        let scheme = scheme.to_ascii_lowercase();
        if !bearer && (scheme.starts_with("bearer ") || scheme.starts_with("basic ")) {
            j += scheme.find(' ').unwrap_or(0) + 1;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
        }
        let vstart = j;
        match quote {
            Some(q) => {
                while j < chars.len() && chars[j] != q {
                    j += 1;
                }
            }
            None => {
                while j < chars.len() && !ends_value(chars[j]) {
                    j += 1;
                }
            }
        }
        if j == vstart {
            continue;
        }
        out.extend_from_slice(&chars[i..vstart]);
        out.extend(REDACTED.chars());
        i = j;
    }
    out
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '=')
}

fn looks_secret(word: &str) -> bool {
    if TOKEN_PREFIXES.iter().any(|p| word.starts_with(p)) && word.len() >= 16 {
        return true;
    }
    if word.starts_with("eyJ") && word.matches('.').count() == 2 && word.len() >= 30 {
        return true;
    }
    let core = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let run = core
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'));
    let hex = core.chars().all(|c| c.is_ascii_hexdigit());
    let digits = core.chars().filter(char::is_ascii_digit).count();
    let mixed = digits > 0 && core.chars().any(|c| c.is_ascii_alphabetic());
    let cased = core.chars().any(|c| c.is_ascii_uppercase())
        && core.chars().any(|c| c.is_ascii_lowercase());
    core.len() >= 32 && mixed && (hex || (run && (cased || digits * 10 >= core.len())))
}

fn redact_tokens(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if looks_secret(word) {
            out.push_str(REDACTED);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for &c in chars {
        if is_token_char(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A one-endpoint HTTP server: answers every request with `status` and `body`,
    /// after `delay`, and records request bodies. Never touches the network.
    pub struct FakeSystemOne {
        pub base_url: String,
        pub bodies: Arc<Mutex<Vec<String>>>,
        pub auth_headers: Arc<Mutex<Vec<String>>>,
    }

    pub fn fake_system_one(status: u16, body: String, delay: Duration) -> FakeSystemOne {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let auth_headers = Arc::new(Mutex::new(Vec::new()));
        let (b, a) = (bodies.clone(), auth_headers.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (b, a, body) = (b.clone(), a.clone(), body.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut len = 0usize;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if let Some(v) = lower.strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        if lower.starts_with("authorization:") {
                            a.lock().unwrap().push(line.trim().to_string());
                        }
                    }
                    let mut buf = vec![0u8; len];
                    let _ = reader.read_exact(&mut buf);
                    b.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf).into_owned());
                    std::thread::sleep(delay);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        FakeSystemOne {
            base_url: format!("http://{addr}/api/v1"),
            bodies,
            auth_headers,
        }
    }

    pub fn answer_body(choice: &str, p: f64) -> String {
        let rest = (1.0 - p) / 5.0;
        let labels = [
            "quota_or_rate_limit",
            "auth",
            "transient_infra",
            "killed_by_operator_or_signal",
            "genuine_failure",
            "other",
        ];
        let probs: serde_json::Map<String, serde_json::Value> = labels
            .iter()
            .map(|l| {
                let v = if *l == choice { p } else { rest };
                (l.to_string(), serde_json::json!(v))
            })
            .collect();
        serde_json::json!({
            "answers": {"failure_class": {"type": "choice", "choice": choice, "probabilities": probs, "confidence": p}},
            "usage": {"input_tokens": 1200, "output_tokens": 0, "cost": 0.00005}
        })
        .to_string()
    }

    pub fn enabled_cfg(base_url: &str, key_file: &Path) -> ClassifierConfig {
        std::fs::write(key_file, "sk-or-v1-test-key-not-real\n").unwrap();
        ClassifierConfig {
            enabled: true,
            base_url: base_url.to_string(),
            key_file: Some(key_file.to_path_buf()),
            timeout_secs: 1,
            ..Default::default()
        }
    }

    fn state(tail: &str) -> FailureState {
        FailureState {
            provider: "cli:claude".into(),
            role: "implementer".into(),
            exit_code: Some(1),
            signal: None,
            tool_calls: Some(3),
            spar_error: Some("exit 1".into()),
            log_tail: tail.into(),
        }
    }

    fn answer(choice: FailureClass, p: f64) -> Answer {
        Answer {
            choice,
            probability: p,
            probabilities: BTreeMap::new(),
            cost_usd: None,
            latency_ms: 1,
            cached: false,
        }
    }

    #[test]
    fn gap_fill_acts_at_threshold_and_not_below() {
        let cfg = ClassifierConfig::default();
        let w = RegexVerdict::Work;
        let v = Site::Verdict;
        let d = Site::Dispatch { tool_calls: true };
        let q = FailureClass::QuotaOrRateLimit;
        assert_eq!(decide(&cfg, v, w, &answer(q, 0.7)), Action::PauseQuota);
        assert_eq!(decide(&cfg, v, w, &answer(q, 0.69)), Action::None);
        assert_eq!(
            decide(&cfg, d, w, &answer(q, 0.99)),
            Action::None,
            "quota is the verdict's to act on"
        );
        assert_eq!(
            decide(&cfg, v, w, &answer(FailureClass::Auth, 0.8)),
            Action::Auth
        );
        assert_eq!(
            decide(&cfg, d, w, &answer(FailureClass::Auth, 0.8)),
            Action::Auth
        );
        let t = answer(FailureClass::TransientInfra, 0.95);
        assert_eq!(decide(&cfg, d, w, &t), Action::Retry);
        assert_eq!(
            decide(&cfg, Site::Dispatch { tool_calls: false }, w, &t),
            Action::None,
            "transient keeps the retry loop's tool-call gate"
        );
        assert_eq!(decide(&cfg, v, w, &t), Action::None);
        assert_eq!(
            decide(&cfg, v, w, &answer(FailureClass::GenuineFailure, 0.99)),
            Action::None
        );
    }

    #[test]
    fn veto_downgrades_text_verdicts_only_at_its_own_threshold() {
        let cfg = ClassifierConfig::default();
        let v = Site::Verdict;
        let work = |p| answer(FailureClass::GenuineFailure, p);
        assert_eq!(
            decide(&cfg, v, RegexVerdict::Quota, &work(0.9)),
            Action::WeakPause
        );
        assert_eq!(
            decide(&cfg, v, RegexVerdict::Auth, &work(0.95)),
            Action::WeakPause
        );
        assert_eq!(
            decide(&cfg, v, RegexVerdict::Quota, &work(0.89)),
            Action::None
        );
        assert_eq!(
            decide(
                &cfg,
                v,
                RegexVerdict::Quota,
                &answer(FailureClass::Other, 0.99)
            ),
            Action::None
        );
        assert_eq!(
            decide(
                &cfg,
                v,
                RegexVerdict::Quota,
                &answer(FailureClass::Auth, 0.99)
            ),
            Action::None,
            "the veto only removes, it never re-routes"
        );
    }

    #[test]
    fn classify_posts_the_system_one_shape_and_parses_the_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = fake_system_one(
            200,
            answer_body("quota_or_rate_limit", 0.91),
            Duration::ZERO,
        );
        let cfg = enabled_cfg(&fake.base_url, &tmp.path().join("key"));
        let a = classify(&cfg, "claude", &state("credits are gone for today")).unwrap();
        assert_eq!(a.choice, FailureClass::QuotaOrRateLimit);
        assert!((a.probability - 0.91).abs() < 1e-9);
        assert_eq!(a.cost_usd, Some(0.00005));
        let body: serde_json::Value =
            serde_json::from_str(&fake.bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(body["model"], "typesafe/jev-1.14");
        assert_eq!(body["state"]["log_tail"], "credits are gone for today");
        assert_eq!(body["state"]["tool_calls"], 3);
        assert_eq!(body["questions"]["failure_class"]["type"], "choice");
        assert!(body["questions"]["failure_class"]["criteria"]["auth"].is_string());
        assert_eq!(
            fake.auth_headers.lock().unwrap()[0].to_ascii_lowercase(),
            "authorization: bearer sk-or-v1-test-key-not-real"
        );
        let again = classify(&cfg, "claude", &state("credits are gone for today")).unwrap();
        assert!(again.cached);
        assert_eq!(fake.bodies.lock().unwrap().len(), 1, "memoized");
    }

    #[test]
    fn errors_timeouts_and_a_missing_key_leave_no_action() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let run = "r1";
        std::fs::create_dir_all(paths.run_dir(run)).unwrap();

        let slow = fake_system_one(200, answer_body("auth", 0.99), Duration::from_secs(3));
        let cfg = enabled_cfg(&slow.base_url, &tmp.path().join("key"));
        let rec = consult(
            &cfg,
            &paths,
            run,
            "s1",
            "claude",
            &state("slow"),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap();
        assert_eq!(rec.action, Action::None);
        assert!(rec.error.is_some());
        let sent = slow.bodies.lock().unwrap().len();
        assert!((1..=1 + MAX_RETRIES).contains(&sent), "{sent} attempts");
        assert!(
            rec.latency_ms <= 2500,
            "the whole consult stays inside timeout + 1s: {}ms",
            rec.latency_ms
        );
        let started = Instant::now();
        let rec = consult(
            &cfg,
            &paths,
            run,
            "s1",
            "claude",
            &state("another failure, same dead endpoint"),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap();
        assert!(
            rec.error.unwrap().contains("skipped"),
            "the breaker is open"
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(slow.bodies.lock().unwrap().len(), sent, "no new request");

        let broken = fake_system_one(400, "{}".into(), Duration::ZERO);
        let cfg = enabled_cfg(&broken.base_url, &tmp.path().join("key"));
        let rec = consult(
            &cfg,
            &paths,
            run,
            "s1",
            "claude",
            &state("bad request"),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap();
        assert_eq!(rec.action, Action::None);
        assert_eq!(broken.bodies.lock().unwrap().len(), 1, "4xx is not retried");

        let garbage = fake_system_one(200, "{\"answers\":{}}".into(), Duration::ZERO);
        let cfg = enabled_cfg(&garbage.base_url, &tmp.path().join("key"));
        let rec = consult(
            &cfg,
            &paths,
            run,
            "s1",
            "claude",
            &state("garbage"),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap();
        assert_eq!(rec.action, Action::None);

        let fine = fake_system_one(200, answer_body("auth", 0.99), Duration::ZERO);
        let mut cfg = enabled_cfg(&fine.base_url, &tmp.path().join("key"));
        cfg.key_file = Some(tmp.path().join("missing"));
        let rec = consult(
            &cfg,
            &paths,
            run,
            "s1",
            "claude",
            &state("no key"),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap();
        assert_eq!(rec.action, Action::None);
        assert!(fine.bodies.lock().unwrap().is_empty(), "no key, no request");

        cfg.enabled = false;
        assert!(consult(
            &cfg,
            &paths,
            run,
            "s1",
            "claude",
            &state("off"),
            Site::Verdict,
            RegexVerdict::Work
        )
        .is_none());
    }

    #[test]
    fn consult_appends_a_classifier_event_with_its_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(paths.run_dir("r1")).unwrap();
        let fake = fake_system_one(200, answer_body("auth", 0.88), Duration::ZERO);
        let cfg = enabled_cfg(&fake.base_url, &tmp.path().join("key"));
        consult(
            &cfg,
            &paths,
            "r1",
            "impl-1",
            "claude",
            &state("your session is no longer valid"),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap();
        let text = std::fs::read_to_string(paths.run_dir("r1").join("events.jsonl")).unwrap();
        let ev: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(ev["type"], "classifier");
        assert_eq!(ev["slot"], "impl-1");
        let c = &ev["classifier"];
        assert_eq!(c["model"], "typesafe/jev-1.14");
        assert_eq!(c["choice"], "auth");
        assert_eq!(c["acted"], true);
        assert_eq!(c["action"], "auth");
        assert_eq!(c["regex_verdict"], "work");
        assert!(c["latency_ms"].is_u64());
        assert_eq!(c["cost_usd"], 0.00005);
        assert!(c["probabilities"]["auth"].is_f64());
        assert!(!text.contains("sk-or-v1-test-key-not-real"));
    }

    /// Live smoke against the real endpoint; costs a fraction of a cent. Run with
    /// `SPAR_CLASSIFIER_LIVE_KEY_FILE=<path> cargo test live_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_smoke() {
        let key_file = std::env::var("SPAR_CLASSIFIER_LIVE_KEY_FILE").expect("key file");
        let cfg = ClassifierConfig {
            enabled: true,
            key_file: Some(key_file.into()),
            ..Default::default()
        };
        let cases = [
            (
                "codex",
                "ERROR: stream error: You have exhausted the prepaid balance for this organization. Top up to continue.",
            ),
            (
                "muse",
                "tool_call edit src/lib.rs\n! model `muse-spark-1.3` does not exist or you lack access [request_id=abc]",
            ),
            (
                "claude",
                "running cargo test\ntest quota::tests::rate_limit_exceeded_returns_retry_after ... FAILED\nerror: test failed, to rerun pass `--lib`",
            ),
        ];
        for (adapter, tail) in cases {
            let st = FailureState {
                provider: format!("cli:{adapter}"),
                role: "implementer".into(),
                exit_code: Some(1),
                signal: None,
                tool_calls: Some(4),
                spar_error: Some("exit 1".into()),
                log_tail: log_tail(tail),
            };
            let a = classify(&cfg, adapter, &st).unwrap();
            println!(
                "{adapter}: {:?} p={:.3} latency={}ms cost={:?}",
                a.choice, a.probability, a.latency_ms, a.cost_usd
            );
        }
    }

    #[test]
    fn redaction_strips_keys_tokens_passwords_and_db_urls() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let cases: &[(&str, &str)] = &[
            (
                "Authorization: Bearer abc123def456ghi789",
                "abc123def456ghi789",
            ),
            (
                "curl -H 'authorization: bearer zzTopSecret9'",
                "zzTopSecret9",
            ),
            ("bearer qwertyuiop12", "qwertyuiop12"),
            (
                "export OPENAI_API_KEY=sk-proj-AbCdEf0123456789xyzXYZ",
                "sk-proj-AbCdEf0123456789xyzXYZ",
            ),
            ("postgres://app:hunter2@db.internal:5432/main", "hunter2"),
            (
                "{\"password\": \"correct horse\", \"user\": \"bob\"}",
                "correct horse",
            ),
            ("secret=s3cr3t&next=1", "s3cr3t"),
            ("DATABASE_PASSWORD=pa55word", "pa55word"),
            ("export PGPASSWORD=letmein", "letmein"),
            ("GITHUB_TOKEN=abc", "abc"),
            ("MY_TOKEN: shorty", "shorty"),
            (
                "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "wJalrXUtnFEMI",
            ),
            (
                "x-api-key: 9f86d081884c7d659a2feaa0c55ad015",
                "9f86d081884c7d659a2feaa0c55ad015",
            ),
            ("psql --password hunter3", "hunter3"),
            ("tool --api-key=k3y-value", "k3y-value"),
            ("STRIPE_KEY: rk_test_x", "rk_test_x"),
            (
                "key ghp_0123456789abcdefABCDEF0123456789abcd",
                "ghp_0123456789abcdefABCDEF0123456789abcd",
            ),
            (
                "blob ZXhhbXBsZTEyMzQ1Njc4OTBleGFtcGxlMTIzNDU2Nzg5MA==",
                "ZXhhbXBsZTEyMzQ1Njc4OTBleGFtcGxlMTIzNDU2Nzg5MA",
            ),
        ];
        for (line, leak) in cases {
            let out = redact(line);
            assert!(!out.contains(leak), "leaked {leak}: {out}");
            assert!(out.contains(REDACTED), "{out}");
        }
        let out = redact(&format!("token {jwt} expired"));
        assert!(!out.contains(jwt), "{out}");

        let kept = [
            "API Error: 401 OAuth access token has been revoked",
            "[auth]: credentials rejected",
            "not logged in: run `muse login` first",
            "You've hit your weekly limit · resets 12am (America/New_York)",
            "test quota::tests::rate_limit_exceeded_returns_retry_after ... FAILED",
            "input_tokens: 1200, max_tokens: 4096",
        ];
        for line in kept {
            assert_eq!(redact(line), line);
        }
        assert_eq!(
            redact("postgres://app:hunter2@db.internal:5432/main"),
            "postgres://app:<redacted>@db.internal:5432/main"
        );
        assert!(redact("{\"password\": \"x\", \"user\": \"bob\"}").contains("\"user\": \"bob\""));
    }

    #[test]
    fn redaction_never_panics_on_non_ascii() {
        for line in [
            "password=voilà ok",
            "secret: x\u{a0}y",
            "token=\u{85}abc",
            "Authorization: Bearer é",
            "--password\u{a0}hunter2",
            "postgres://ü:ß@höst/db",
            "ключ api_key: значение",
            "password=",
            "bearer",
            "x://",
            "😀 secret=😀😀 eyJ😀.a.b",
        ] {
            let _ = redact(line);
        }
        assert!(!redact("password=voilà ok").contains("voilà"));
        assert!(redact("secret: x\u{a0}y").starts_with("secret: <redacted>"));
    }

    #[test]
    fn log_tail_cuts_first_then_redacts_and_drops_the_partial_line() {
        let text = format!("{}\nend", "é".repeat(3000));
        let tail = log_tail(&text);
        assert_eq!(tail, "end", "the cut line is dropped whole");

        let secret = "sk-proj-AbCdEf0123456789xyzXYZ";
        let filler = "x".repeat(LOG_TAIL_BYTES - 10);
        let text = format!("OPENAI_API_KEY={secret}\n{filler}\nlast line");
        let tail = log_tail(&text);
        assert!(
            !tail.contains("AbCdEf0123456789"),
            "no half-token survives the cut"
        );
        assert!(tail.ends_with("last line"));

        let many = "password=a\n".repeat(LOG_TAIL_BYTES / 11);
        let tail = log_tail(&many);
        assert!(tail.len() <= LOG_TAIL_BYTES);
        assert!(!tail.contains("password=a\n"));
    }
}
