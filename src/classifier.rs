//! Advisory typed classifier for slot failures (O148). The regexes and typed signals
//! decide first; this is asked only where spar would otherwise read a failure as the
//! agent's own work, and its answer is used only above a probability threshold. Any
//! error leaves spar's own verdict in place.

use crate::config::ClassifierConfig;
use crate::paths::SparPaths;
use crate::providers::failure_question::{self, Questions, QUESTION_ID};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LOG_TAIL_BYTES: usize = 4096;
/// How much of the log is read before tool output is dropped and the tail is cut: tool
/// lines are most of a log, so the 4 KB budget is filled from well before the end.
const READ_WINDOW: usize = 64 * 1024;
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
            log_tail: no_panic(|| log_tail(&read_window(log_path))),
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

/// A failure, kept apart from an answer so only answers and deterministic refusals
/// are remembered: a timeout, a 429 or a 5xx may not repeat.
struct CallError {
    message: String,
    /// The same request would fail the same way (a 4xx other than 408/409/429, an
    /// unparseable answer).
    deterministic: bool,
    /// The endpoint looks down: a transport failure, or 408/409/429/5xx after the
    /// retries ran out.
    trips: bool,
}

impl CallError {
    fn fixed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            deterministic: true,
            trips: false,
        }
    }
}

impl<E: std::fmt::Display> From<E> for CallError {
    fn from(e: E) -> Self {
        Self {
            message: e.to_string(),
            deterministic: false,
            trips: false,
        }
    }
}

/// Advisory classifier with its own memo, breakers and transport. Production runs
/// one shared instance ([`Classifier::live`]); every test builds its own, so no
/// test shares answers, breaker state or network timing with another.
pub struct Classifier {
    memo: Mutex<HashMap<u64, Result<Answer, String>>>,
    breakers: Mutex<HashMap<String, Breaker>>,
    post: PostFn,
}

/// One HTTP POST to the System One endpoint: the URL, the key, the request
/// body and the per-attempt timeout. Returns the status with the response
/// text, or a transport failure. Production posts over the network; tests
/// answer in process so no timing outside the test can add or drop a request.
type PostFn =
    Arc<dyn Fn(&str, &str, &str, Duration) -> Result<(u16, String), String> + Send + Sync>;

fn live_post(
    url: &str,
    api_key: &str,
    body: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    crate::util::retry_interrupted(|| {
        let resp = agent
            .post(url)
            .header("Authorization", &format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("X-OpenRouter-Title", "spar")
            .send(body)?;
        let status = resp.status().as_u16();
        let text = resp.into_body().read_to_string().unwrap_or_default();
        Ok((status, text))
    })
    .map_err(|e| e.to_string())
}

const BREAKER_OPEN: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy)]
enum Breaker {
    /// No calls until then.
    Open(Instant),
    /// The window lapsed and one consult is probing the endpoint; the rest skip.
    Probing(Instant),
}

fn memo_key(cfg: &ClassifierConfig, adapter: &str, state: &FailureState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        &cfg.model,
        &cfg.base_url,
        adapter,
        serde_json::to_string(state).unwrap_or_default(),
    )
        .hash(&mut h);
    h.finish()
}

fn budget(cfg: &ClassifierConfig) -> Duration {
    Duration::from_secs(cfg.timeout_secs) + Duration::from_secs(1)
}

impl Classifier {
    /// Asks the classifier, decides, and appends the `classifier` event. `None` when the
    /// classifier is off; otherwise the record, whose `action` is `None` on any error.
    #[allow(clippy::too_many_arguments)]
    pub fn consult(
        &self,
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
            self.classify(cfg, adapter, state)
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

    /// The process-wide instance production paths consult.
    pub fn live() -> &'static Self {
        static LIVE: std::sync::LazyLock<Classifier> = std::sync::LazyLock::new(|| Classifier {
            memo: Mutex::new(HashMap::new()),
            breakers: Mutex::new(HashMap::new()),
            post: Arc::new(live_post),
        });
        &LIVE
    }

    /// A classifier whose post panics, for tests that need a `&Classifier`
    /// with the classifier disabled.
    #[cfg(test)]
    pub fn inert() -> Self {
        Classifier {
            memo: Mutex::new(HashMap::new()),
            breakers: Mutex::new(HashMap::new()),
            post: Arc::new(|_, _, _, _| panic!("classifier consulted in a test")),
        }
    }

    /// Whether this consult may call `base_url`. Checked and updated under one lock, so
    /// during an outage at most one consult at a time is waiting on the endpoint. A probe
    /// that never reported back (a panic) stops blocking after one more window.
    fn breaker_admits(&self, base_url: &str, budget: Duration) -> bool {
        let Ok(mut m) = self.breakers.lock() else {
            return true;
        };
        let now = Instant::now();
        match m.get(base_url).copied() {
            None => true,
            Some(Breaker::Open(until)) if now < until => false,
            Some(Breaker::Probing(since)) if now < since + budget + BREAKER_OPEN => false,
            Some(_) => {
                m.insert(base_url.to_string(), Breaker::Probing(now));
                true
            }
        }
    }

    fn breaker_report(&self, base_url: &str, tripped: bool) {
        let Ok(mut m) = self.breakers.lock() else {
            return;
        };
        if tripped {
            m.insert(
                base_url.to_string(),
                Breaker::Open(Instant::now() + BREAKER_OPEN),
            );
        } else {
            m.remove(base_url);
        }
    }

    /// One System One call (`POST <base_url>/systemone`, pi-ai's `typesafe-system-one`
    /// shape). Retries a transport error, 408, 409, 429 or 5xx while the total budget
    /// (`timeout_secs` + 1) lasts, at most `MAX_RETRIES` times; when they run out the
    /// endpoint's breaker opens for `BREAKER_OPEN`.
    pub fn classify(
        &self,
        cfg: &ClassifierConfig,
        adapter: &str,
        state: &FailureState,
    ) -> Result<Answer> {
        let key = (!state.log_tail.is_empty()).then(|| memo_key(cfg, adapter, state));
        if let Some(hit) = key.and_then(|k| self.memo.lock().ok().and_then(|m| m.get(&k).cloned()))
        {
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
        if !self.breaker_admits(&cfg.base_url, budget(cfg)) {
            bail!("classifier skipped: endpoint failing, retried after 5 minutes");
        }
        let result = self.call(cfg, adapter, state);
        let remember = match &result {
            Ok(a) => Some(Ok(a.clone())),
            Err(e) if e.deterministic => Some(Err(e.message.clone())),
            Err(_) => None,
        };
        self.breaker_report(
            &cfg.base_url,
            result.as_ref().err().is_some_and(|e| e.trips),
        );
        if let (Some(k), Some(r)) = (key, remember) {
            if let Ok(mut m) = self.memo.lock() {
                m.insert(k, r);
            }
        }
        result.map_err(|e| anyhow::anyhow!(e.message))
    }

    fn call(
        &self,
        cfg: &ClassifierConfig,
        adapter: &str,
        state: &FailureState,
    ) -> std::result::Result<Answer, CallError> {
        let body = serde_json::to_string(&Request {
            model: &cfg.model,
            state,
            questions: failure_question::questions(adapter),
        })?;
        let api_key = read_key(cfg).map_err(|e| CallError::from(format!("{e:#}")))?;
        let url = format!("{}/systemone", cfg.base_url.trim_end_matches('/'));
        let per_attempt = Duration::from_secs(cfg.timeout_secs);
        let budget = budget(cfg);
        let started = Instant::now();
        let mut attempt = 0;
        let text = loop {
            let left = budget.saturating_sub(started.elapsed());
            let reason = match (self.post)(&url, &api_key, &body, per_attempt.min(left)) {
                Ok((status, text)) => {
                    if (200..300).contains(&status) {
                        break text;
                    }
                    if !matches!(status, 408 | 409 | 429 | 500..=599) {
                        return Err(CallError::fixed(format!(
                            "classifier returned HTTP {status}: {}",
                            truncate(&text, 200)
                        )));
                    }
                    format!("HTTP {status}")
                }
                Err(e) => e,
            };
            let backoff = Duration::from_millis(250 << attempt);
            let out_of_time = started.elapsed() + backoff + Duration::from_millis(250) >= budget;
            if attempt >= MAX_RETRIES || out_of_time {
                return Err(CallError {
                    message: format!(
                        "classifier unreachable after {} attempts: {reason}",
                        attempt + 1
                    ),
                    deterministic: false,
                    trips: true,
                });
            }
            std::thread::sleep(backoff);
            attempt += 1;
        };
        let latency_ms = started.elapsed().as_millis() as u64;
        let parsed: Response = serde_json::from_str(&text)
            .map_err(|e| CallError::fixed(format!("parse classifier response: {e}")))?;
        let wire = parsed
            .answers
            .get(QUESTION_ID)
            .ok_or_else(|| CallError::fixed("classifier returned no failure_class answer"))?;
        if wire.kind != "choice" {
            return Err(CallError::fixed(format!(
                "classifier returned a {} answer, not a choice",
                wire.kind
            )));
        }
        let choice: FailureClass = serde_json::from_value(serde_json::Value::String(
            wire.choice.clone(),
        ))
        .map_err(|_| {
            CallError::fixed(format!(
                "classifier returned unknown label {:?}",
                wire.choice
            ))
        })?;
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
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The end of the slot log, starting on a line boundary.
fn read_window(log_path: &Path) -> String {
    let text = crate::process::tail_log(log_path, READ_WINDOW);
    let whole = std::fs::metadata(log_path).is_ok_and(|m| m.len() as usize <= READ_WINDOW);
    if whole {
        text
    } else {
        drop_partial_head(&text).to_string()
    }
}

/// Only what the provider and spar said, never what the tools returned. The slot log is
/// the stream coalescer's rendering for every adapter (`process::StreamCoalescer`):
/// `← ` lines are tool results (env dumps, `.env` files, API responses) and are
/// dropped; `→ name  args` keeps the tool name and drops its arguments; spar's own spawn
/// header is dropped. Provider errors (`! `), status (`· `), assistant text and plain
/// output stay: that is the wording the evaluation scored.
pub fn provider_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut header = text.starts_with("# spawn ");
    for line in text.split_inclusive('\n') {
        if header {
            if line.trim_end() == "---" {
                header = false;
            }
            continue;
        }
        if line.starts_with("← ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("→ ") {
            let name = rest.split("  ").next().unwrap_or("").trim_end();
            out.push_str("→ ");
            out.push_str(name);
            out.push('\n');
            continue;
        }
        out.push_str(line);
    }
    out
}

/// The last `LOG_TAIL_BYTES` of the provider's own lines, redacted. `text` starts on a
/// line boundary. The cut comes before redaction and drops the line it lands in whole,
/// so no secret is cut away from the key that marks it; redaction can lengthen a line,
/// so the result is trimmed again from the front.
pub fn log_tail(text: &str) -> String {
    let lines = provider_lines(text);
    let cut = tail_bytes(&lines, LOG_TAIL_BYTES);
    let mid_line = cut.len() < lines.len() && !lines[..lines.len() - cut.len()].ends_with('\n');
    let cut = if mid_line {
        drop_partial_head(cut)
    } else {
        cut
    };
    let mut red = redact(cut);
    while red.len() > LOG_TAIL_BYTES {
        match red.find('\n') {
            Some(i) => red = red[i + 1..].to_string(),
            None => {
                red = drop_partial_head(tail_bytes(&red, LOG_TAIL_BYTES)).to_string();
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

/// Drops the partial first line, however long; with no newline at all, the first
/// whitespace-delimited word.
fn drop_partial_head(cut: &str) -> &str {
    let at = cut
        .find('\n')
        .or_else(|| cut.find(char::is_whitespace))
        .unwrap_or(cut.len());
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

/// Strips what looks like a secret: PEM blocks, the value of any key or flag whose name
/// says secret (see `secret_name`), bearer and other scheme tokens, cookies, `-u
/// user:pass`, URL credentials, JWTs, vendor-prefixed keys and long base64 or hex runs.
/// Errs toward removing too much. Works on chars, never byte offsets, so no input can
/// make it panic.
pub fn redact(text: &str) -> String {
    redact_pem(text)
        .split('\n')
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_pem(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(b) = rest.find("-----BEGIN ") {
        out.push_str(&rest[..b]);
        out.push_str(REDACTED);
        let after = &rest[b + "-----BEGIN ".len()..];
        rest = match after.find("-----END ") {
            Some(e) => {
                let tail = &after[e + "-----END ".len()..];
                match tail.find("-----") {
                    Some(close) => &tail[close + 5..],
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn redact_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let chars = redact_url_credentials(&chars);
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
    c.is_whitespace()
        || is_quote(c)
        || matches!(c, ',' | ';' | '&' | '}' | ')' | ']' | '<' | '>' | '\\')
}

/// `scheme://user:pass@host` keeps the user and host; `scheme://token@host` keeps the
/// host.
fn redact_url_credentials(chars: &[char]) -> Vec<char> {
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
            && !(chars[i].is_whitespace()
                || matches!(chars[i], '/' | '?' | '#')
                || is_quote(chars[i]))
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
            (Some(at), _) => {
                out.extend(REDACTED.chars());
                out.extend_from_slice(&authority[at..]);
            }
            _ => out.extend_from_slice(authority),
        }
    }
    out
}

/// How a name marks its value.
#[derive(PartialEq)]
enum Secret {
    No,
    /// Always a secret.
    Strong,
    /// A secret only as a quoted JSON key or a `name=value` pair (`key`, `sig`, `auth`):
    /// as prose (`auth: not logged in`) it is the provider's own wording.
    Weak,
    /// The rest of the line (`Cookie:`, `Set-Cookie:`).
    Line,
    /// `-u` / `--user`: a flag whose value is `user:password`.
    UserFlag,
}

fn secret_name(ident: &str) -> Secret {
    let flag = ident.starts_with('-');
    let k = ident.trim_start_matches('-').to_ascii_lowercase();
    let k = k.trim_end_matches(['.', '-', '_']);
    if k.is_empty() {
        return Secret::No;
    }
    if flag && (k == "u" || k == "user") {
        return Secret::UserFlag;
    }
    if k == "cookie" || k == "set-cookie" {
        return Secret::Line;
    }
    let strong = k.contains("password")
        || k.contains("passwd")
        || k.contains("passphrase")
        || k.ends_with("pass")
        || k.contains("secret")
        || k.contains("apikey")
        || k.contains("api_key")
        || k.contains("api-key")
        || k.contains("private_key")
        || k.contains("authorization")
        || k == "pwd"
        || k.ends_with("_pwd")
        || k.ends_with("token")
        || k.ends_with("_key")
        || k.ends_with("-key")
        || k == "bearer";
    if strong {
        return Secret::Strong;
    }
    if matches!(
        k,
        "key" | "sig" | "signature" | "auth" | "credentials" | "credential" | "access_key"
    ) {
        return Secret::Weak;
    }
    Secret::No
}

fn skip_while(chars: &[char], mut j: usize, f: impl Fn(char) -> bool) -> usize {
    while j < chars.len() && f(chars[j]) {
        j += 1;
    }
    j
}

/// `NAME=value`, `NAME: value`, `NAME := value`, `"name": "value"` (also JSON-escaped,
/// `\"name\": \"value\"`), `--name value`, `--name=value`, `Authorization: <scheme>
/// value`, `Bearer value`, `<value>`. A quoted value is taken to its closing quote.
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
        i = skip_while(chars, i, is_ident);
        let ident: String = chars[start..i].iter().collect();
        out.extend_from_slice(&chars[start..i]);
        let kind = secret_name(&ident);
        if kind == Secret::No {
            continue;
        }
        let flag = ident.starts_with("--") || kind == Secret::UserFlag;
        let bearer = ident.eq_ignore_ascii_case("bearer");
        let quoted_key = i < chars.len() && (is_quote(chars[i]) || chars[i] == '\\');
        let mut j = skip_while(chars, i, |c| is_quote(c) || c == '\\');
        j = skip_while(chars, j, |c| c == ' ' || c == '\t');
        let sep = j < chars.len() && matches!(chars[j], '=' | ':');
        let tight_eq = j == i && sep && chars[j] == '=';
        if sep {
            j += 1;
            if j < chars.len() && chars[j] == '=' {
                j += 1;
            }
        } else if !(flag || bearer) || j == i {
            continue;
        }
        if kind == Secret::Weak && !(quoted_key || tight_eq) {
            continue;
        }
        j = skip_while(chars, j, |c| c == ' ' || c == '\t');
        if kind == Secret::Line {
            if j < chars.len() {
                out.extend_from_slice(&chars[i..j]);
                out.extend(REDACTED.chars());
                i = chars.len();
            }
            continue;
        }
        let open = j;
        j = skip_while(chars, j, |c| c == '\\');
        let close = match chars.get(j) {
            Some(&q) if is_quote(q) => Some(q),
            Some('<') => Some('>'),
            _ => None,
        };
        if close.is_some() {
            j += 1;
        } else {
            j = open;
        }
        if !bearer && kind == Secret::Strong && ident.to_ascii_lowercase().contains("authorization")
        {
            let word_end = skip_while(chars, j, |c| c.is_ascii_alphabetic());
            let gap = skip_while(chars, word_end, |c| c == ' ');
            let more = gap < chars.len() && !ends_value(chars[gap]);
            if word_end > j && gap > word_end && more {
                j = gap;
            }
        } else if !bearer {
            let word_end = skip_while(chars, j, |c| c.is_ascii_alphabetic());
            let word: String = chars[j..word_end].iter().collect();
            let word = word.to_ascii_lowercase();
            if matches!(word.as_str(), "bearer" | "basic" | "token" | "digest")
                && word_end < chars.len()
                && chars[word_end] == ' '
            {
                j = skip_while(chars, word_end, |c| c == ' ');
            }
        }
        let vstart = j;
        j = match close {
            Some(q) => skip_while(chars, j, |c| c != q && c != '\\'),
            None => skip_while(chars, j, |c| !ends_value(c)),
        };
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
    use std::sync::{Arc, Mutex};

    /// In process, so a loaded machine cannot add or drop a request through a
    /// transport retry.
    pub struct StubSystemOne {
        pub base_url: String,
        pub urls: Arc<Mutex<Vec<String>>>,
        pub bodies: Arc<Mutex<Vec<String>>>,
        pub auth_headers: Arc<Mutex<Vec<String>>>,
    }

    pub fn stub_system_one(
        status: u16,
        body: String,
        delay: Duration,
    ) -> (Classifier, StubSystemOne) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let urls = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let auth_headers = Arc::new(Mutex::new(Vec::new()));
        let (u, b, a) = (urls.clone(), bodies.clone(), auth_headers.clone());
        let post: PostFn = Arc::new(
            move |url: &str, api_key: &str, req: &str, timeout: Duration| {
                u.lock().unwrap().push(url.to_string());
                b.lock().unwrap().push(req.to_string());
                a.lock()
                    .unwrap()
                    .push(format!("Authorization: Bearer {api_key}"));
                if delay > timeout {
                    std::thread::sleep(timeout);
                    return Err(format!("stub timed out after {timeout:?}"));
                }
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                Ok((status, body.clone()))
            },
        );
        (
            Classifier {
                memo: Mutex::new(HashMap::new()),
                breakers: Mutex::new(HashMap::new()),
                post,
            },
            StubSystemOne {
                base_url: format!("stub://classifier-{n}"),
                urls,
                bodies,
                auth_headers,
            },
        )
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
            // Generous: tests that count attempts must not see a loaded machine's
            // slow round trip as a timeout that retries or ends the budget early.
            timeout_secs: 30,
            ..Default::default()
        }
    }

    struct Loopback {
        base_url: String,
        request_lines: Arc<Mutex<Vec<String>>>,
        auth_values: Arc<Mutex<Vec<String>>>,
    }

    /// A loopback HTTP server for `live_post`: records the request line and
    /// the `authorization` value of every connection, answers each with
    /// `status`/`body` after `delay`. Assertions on it stay retry-insensitive
    /// (first values, outcomes only) so loaded-machine timing cannot fail
    /// the test.
    fn loopback(status: u16, body: &str, delay: Duration) -> Loopback {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let auth = Arc::new(Mutex::new(Vec::new()));
        let (l, a) = (lines.clone(), auth.clone());
        let body = body.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (l, a, body) = (l.clone(), a.clone(), body.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        return;
                    }
                    l.lock().unwrap().push(request.trim().to_string());
                    let mut len = 0usize;
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        let lower = line.to_ascii_lowercase();
                        if lower.starts_with("authorization:") {
                            a.lock()
                                .unwrap()
                                .push(line["authorization:".len()..].trim().to_string());
                        } else if let Some(v) = lower.strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut buf = vec![0u8; len];
                    let _ = reader.read_exact(&mut buf);
                    std::thread::sleep(delay);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        Loopback {
            base_url: format!("http://{addr}"),
            request_lines: lines,
            auth_values: auth,
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
        let (cls, fake) = stub_system_one(
            200,
            answer_body("quota_or_rate_limit", 0.91),
            Duration::ZERO,
        );
        let cfg = enabled_cfg(&format!("{}/", fake.base_url), &tmp.path().join("key"));
        let a = cls
            .classify(&cfg, "claude", &state("credits are gone for today"))
            .unwrap();
        assert_eq!(
            fake.urls.lock().unwrap()[0],
            format!("{}/systemone", fake.base_url)
        );
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
        let again = cls
            .classify(&cfg, "claude", &state("credits are gone for today"))
            .unwrap();
        assert!(again.cached);
        assert_eq!(fake.bodies.lock().unwrap().len(), 1, "memoized");
    }

    #[test]
    fn live_post_sends_auth_passes_status_through_and_honors_timeout() {
        let server = loopback(200, &answer_body("auth", 0.99), Duration::ZERO);
        let (status, text) = live_post(
            &format!("{}/systemone", server.base_url),
            "test-key",
            "{}",
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(status, 200);
        assert!(text.contains("\"auth\""), "{text}");
        assert!(
            server
                .request_lines
                .lock()
                .unwrap()
                .first()
                .is_some_and(|l| l.starts_with("POST /systemone ")),
            "posts the systemone path"
        );
        assert_eq!(
            server
                .auth_values
                .lock()
                .unwrap()
                .first()
                .map(String::as_str),
            Some("Bearer test-key"),
            "sends the bearer key"
        );

        for status in [400, 503] {
            let server = loopback(status, "{}", Duration::ZERO);
            let (got, _) = live_post(
                &format!("{}/systemone", server.base_url),
                "test-key",
                "{}",
                Duration::from_secs(30),
            )
            .unwrap();
            assert_eq!(got, status, "{status} passes through, not an error");
        }

        let server = loopback(200, &answer_body("auth", 0.99), Duration::from_secs(30));
        let started = Instant::now();
        let err = live_post(
            &format!("{}/systemone", server.base_url),
            "test-key",
            "{}",
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(err.contains("timeout"), "a slow server times out: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the per-attempt timeout cuts a slow server off"
        );
    }

    #[test]
    fn errors_timeouts_and_a_missing_key_leave_no_action() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let run = "r1";
        std::fs::create_dir_all(paths.run_dir(run)).unwrap();

        let (slow_cls, slow) =
            stub_system_one(200, answer_body("auth", 0.99), Duration::from_secs(3));
        let cfg = ClassifierConfig {
            timeout_secs: 1,
            ..enabled_cfg(&slow.base_url, &tmp.path().join("key"))
        };
        let rec = slow_cls
            .consult(
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
        let rec = slow_cls
            .consult(
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

        let (broken_cls, broken) = stub_system_one(400, "{}".into(), Duration::ZERO);
        let cfg = enabled_cfg(&broken.base_url, &tmp.path().join("key"));
        let rec = broken_cls
            .consult(
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

        let (garbage_cls, garbage) =
            stub_system_one(200, "{\"answers\":{}}".into(), Duration::ZERO);
        let cfg = enabled_cfg(&garbage.base_url, &tmp.path().join("key"));
        let rec = garbage_cls
            .consult(
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

        let (fine_cls, fine) = stub_system_one(200, answer_body("auth", 0.99), Duration::ZERO);
        let mut cfg = enabled_cfg(&fine.base_url, &tmp.path().join("key"));
        cfg.key_file = Some(tmp.path().join("missing"));
        let rec = fine_cls
            .consult(
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
        assert!(fine_cls
            .consult(
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
        let (cls, fake) = stub_system_one(200, answer_body("auth", 0.88), Duration::ZERO);
        let cfg = enabled_cfg(&fake.base_url, &tmp.path().join("key"));
        cls.consult(
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
            let a = Classifier::live().classify(&cfg, adapter, &st).unwrap();
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

    fn consult_once(
        cls: &Classifier,
        cfg: &ClassifierConfig,
        paths: &SparPaths,
        tail: &str,
    ) -> Record {
        cls.consult(
            cfg,
            paths,
            "r1",
            "s1",
            "claude",
            &state(tail),
            Site::Verdict,
            RegexVerdict::Work,
        )
        .unwrap()
    }

    #[test]
    fn breaker_trips_on_exhausted_5xx_but_not_on_4xx_and_errors_are_not_memoized() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(paths.run_dir("r1")).unwrap();

        let (down_cls, down) = stub_system_one(503, "{}".into(), Duration::ZERO);
        let cfg = enabled_cfg(&down.base_url, &tmp.path().join("key"));
        let first = consult_once(&down_cls, &cfg, &paths, "first")
            .error
            .unwrap();
        assert!(
            first.contains(&format!("after {} attempts", 1 + MAX_RETRIES)),
            "{first}"
        );
        let sent = down.bodies.lock().unwrap().len();
        let rec = consult_once(&down_cls, &cfg, &paths, "second");
        assert!(
            rec.error.unwrap().contains("skipped"),
            "5xx exhausted trips"
        );
        down_cls.breaker_report(&cfg.base_url, false);
        let again = consult_once(&down_cls, &cfg, &paths, "first")
            .error
            .unwrap();
        assert!(
            !again.contains("cached") && down.bodies.lock().unwrap().len() > sent,
            "a 5xx is never served from memory: {again}"
        );

        let (refused_cls, refused) = stub_system_one(400, "{}".into(), Duration::ZERO);
        let cfg = enabled_cfg(&refused.base_url, &tmp.path().join("key"));
        consult_once(&refused_cls, &cfg, &paths, "a");
        consult_once(&refused_cls, &cfg, &paths, "b");
        assert_eq!(refused.bodies.lock().unwrap().len(), 2, "4xx does not trip");
        let rec = consult_once(&refused_cls, &cfg, &paths, "a");
        assert!(
            rec.error.unwrap().contains("cached"),
            "a 4xx is deterministic"
        );
        assert_eq!(refused.bodies.lock().unwrap().len(), 2);

        let (ok_cls, ok) = stub_system_one(200, answer_body("auth", 0.9), Duration::ZERO);
        let cfg = enabled_cfg(&ok.base_url, &tmp.path().join("key"));
        consult_once(&ok_cls, &cfg, &paths, "");
        consult_once(&ok_cls, &cfg, &paths, "");
        assert_eq!(
            ok.bodies.lock().unwrap().len(),
            2,
            "an empty tail is not memoized"
        );
        let mut other = state("same tail");
        consult_once(&ok_cls, &cfg, &paths, "same tail");
        other.spar_error = Some("missing expected artifact".into());
        ok_cls.classify(&cfg, "claude", &other).unwrap();
        assert_eq!(
            ok.bodies.lock().unwrap().len(),
            4,
            "spar_error is part of the key"
        );
    }

    #[test]
    fn a_lapsed_breaker_lets_exactly_one_probe_through() {
        let (cls, _stub) = stub_system_one(200, "{}".into(), Duration::ZERO);
        let url = "stub://classifier-reports-probe";
        let budget = Duration::from_secs(6);
        cls.breakers.lock().unwrap().insert(
            url.into(),
            Breaker::Open(Instant::now() - Duration::from_secs(1)),
        );
        let admitted: Vec<bool> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..8)
                .map(|_| sc.spawn(|| cls.breaker_admits(url, budget)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(admitted.iter().filter(|a| **a).count(), 1);
        cls.breaker_report(url, false);
        assert!(cls.breaker_admits(url, budget));
    }

    #[test]
    fn tool_output_and_tool_arguments_never_reach_the_tail() {
        let log = "# spawn claude -p\ncwd=/w\n---\n\
            → Bash  {\"command\":\"cat .env\"}\n\
            ← ✓  t1  OPENAI_API_KEY=sk-live-abcdefghijklmnop STRIPE=rk_live_zzz\n\
            ← ✗  t2  {\"error\":\"quota exceeded\",\"token\":\"abc\"}\n\
            Let me check the config.\n\
            ! API Error: 429 You've hit your usage limit\n\
            · done  ·  2 tools\n";
        let tail = log_tail(log);
        assert_eq!(
            tail,
            "→ Bash\nLet me check the config.\n! API Error: 429 You've hit your usage limit\n· done  ·  2 tools\n"
        );
    }

    #[test]
    fn a_secret_line_cut_anywhere_never_leaks_through_failure_state() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("slot.log");
        let secret = "s3cr3tvalue";
        for pad in 0..64 {
            let filler: String = (0..LOG_TAIL_BYTES / 20)
                .map(|i| format!("line {i:05} ok here\n"))
                .collect();
            let text = format!(
                "start\nexport DATABASE_PASSWORD={secret} more words here\n{filler}{}",
                "y".repeat(pad)
            );
            std::fs::write(&log, &text).unwrap();
            let st =
                FailureState::new("cli:claude", "implementer", Some(1), None, None, None, &log);
            assert!(
                !st.log_tail.contains(secret),
                "pad {pad}: {}",
                &st.log_tail[..80]
            );
            assert!(
                !st.log_tail.contains("words here") || st.log_tail.contains("DATABASE_PASSWORD")
            );
        }

        let value = format!("hunter2 {}", "f ".repeat((LOG_TAIL_BYTES - 12) / 2));
        let value_first = format!("head password={value}\nend");
        let cut_at = value_first.len() - LOG_TAIL_BYTES;
        assert!(
            value_first[..cut_at].ends_with("password="),
            "the cut drops the key"
        );
        let tail = log_tail(&value_first);
        assert!(
            !tail.contains("hunter2"),
            "a long cut line is dropped whole"
        );
        assert_eq!(tail, "end");
    }

    #[test]
    fn redaction_covers_escaped_json_schemes_cookies_pem_and_query_params() {
        let cases: &[(&str, &str)] = &[
            (r#"{\"password\": \"hunter2\"}"#, "hunter2"),
            (
                r#"{"type":"result","result":"API_KEY=\"k3yv4lue\" set"}"#,
                "k3yv4lue",
            ),
            (r#"{\"api_key\":\"abcde\",\"user\":\"bob\"}"#, "abcde"),
            ("Authorization: token ghx123", "ghx123"),
            ("Authorization: Digest qopauth9", "qopauth9"),
            ("Proxy-Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("Cookie: session=abc123; csrftoken=zz9", "abc123"),
            ("Set-Cookie: sid=q1w2e3; Path=/; HttpOnly", "q1w2e3"),
            ("curl -u admin:pa55w0rd https://x", "pa55w0rd"),
            ("curl --user admin:pa55w0rd https://x", "pa55w0rd"),
            ("git clone https://ghtok3n@github.com/o/r", "ghtok3n"),
            ("SMTP_PASS=mailpw", "mailpw"),
            ("gpg_passphrase: horse battery", "horse"),
            ("DB_PASSWORD := pg-secret", "pg-secret"),
            ("GET /x?key=k1k2&sig=s1s2&token=t1t2&page=2", "k1k2"),
            ("GET /x?key=k1k2&sig=s1s2&token=t1t2&page=2", "s1s2"),
            ("GET /x?key=k1k2&sig=s1s2&token=t1t2&page=2", "t1t2"),
            ("https://h/p?secret=zzz&password=yyy", "zzz"),
            ("https://h/p?secret=zzz&password=yyy", "yyy"),
            (
                r#"{"key": "kval", "auth": "aval", "credentials": "cval"}"#,
                "kval",
            ),
            (
                r#"{"key": "kval", "auth": "aval", "credentials": "cval"}"#,
                "aval",
            ),
            (
                r#"{"key": "kval", "auth": "aval", "credentials": "cval"}"#,
                "cval",
            ),
            ("password=<hunter 2>", "hunter 2"),
        ];
        for (line, leak) in cases {
            let out = redact(line);
            assert!(!out.contains(leak), "leaked {leak}: {out}");
        }
        assert!(redact("Cookie: a=b").starts_with("Cookie: "));
        assert!(redact("GET /x?key=k1k2&page=2").contains("page=2"));
        assert!(redact(r#"{\"api_key\":\"abcde\",\"user\":\"bob\"}"#).contains("bob"));
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        assert_eq!(redact(pem), "before\n<redacted>\nafter");
        let cut = "x\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow";
        assert_eq!(redact(cut), "x\n<redacted>");
        for kept in [
            "auth: not logged in",
            "[auth]: credentials rejected",
            "the key: a missing semicolon",
        ] {
            assert_eq!(redact(kept), kept);
        }
    }

    /// Offline check against the evaluation corpus (no model calls): does the new tail
    /// keep the provider's own error wording the eval sent? Run with
    /// `SPAR_CLASSIFIER_EVAL_CORPUS=<corpus.jsonl> cargo test eval_corpus_tails -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn eval_corpus_tails() {
        let path = std::env::var("SPAR_CLASSIFIER_EVAL_CORPUS").expect("corpus path");
        let text = std::fs::read_to_string(path).unwrap();
        let (mut items, mut lines_total, mut lines_kept, mut whole) = (0, 0, 0, 0);
        let mut misses = Vec::new();
        for l in text.lines() {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            let Some(raw) = v["tail8k"].as_str() else {
                continue;
            };
            items += 1;
            let eval_tail = tail_bytes(raw, LOG_TAIL_BYTES);
            let new_tail = log_tail(raw);
            let provider = provider_lines(eval_tail);
            let wanted: Vec<String> = provider
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with("→ "))
                .rev()
                .take(5)
                .map(|l| redact(l).trim().to_string())
                .collect();
            let kept = wanted
                .iter()
                .filter(|w| new_tail.contains(w.as_str()))
                .count();
            lines_total += wanted.len();
            lines_kept += kept;
            if kept == wanted.len() {
                whole += 1;
            } else {
                misses.push((
                    v["id"].as_str().unwrap_or("?").to_string(),
                    wanted
                        .iter()
                        .filter(|w| !new_tail.contains(w.as_str()))
                        .map(|w| w.chars().take(100).collect::<String>())
                        .collect::<Vec<_>>(),
                ));
            }
        }
        println!(
            "{items} items; last-5 provider lines kept in full for {whole}; {lines_kept}/{lines_total} lines kept"
        );
        for (id, lost) in &misses {
            println!("  {id}: lost {lost:?}");
        }
    }
}
