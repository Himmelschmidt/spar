//! The question the failure classifier answers (O148). Criteria text is code: the wording
//! below is what scored in the 2026-11-01 evaluation, and a change to it is a change to
//! routing. Vendor knowledge goes in `vendor_note`, keyed by adapter name.

use serde::Serialize;

pub const QUESTION_ID: &str = "failure_class";

#[derive(Debug, Clone, Serialize)]
pub struct Questions {
    pub failure_class: Question,
}

#[derive(Debug, Clone, Serialize)]
pub struct Question {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub instructions: String,
    pub criteria: Criteria,
}

/// Field order is the order the labels are offered in; keep it.
#[derive(Debug, Clone, Serialize)]
pub struct Criteria {
    pub quota_or_rate_limit: String,
    pub auth: String,
    pub transient_infra: String,
    pub killed_by_operator_or_signal: String,
    pub genuine_failure: String,
    pub other: String,
}

const INSTRUCTIONS: &str = "A coding-agent CLI (claude, codex, grok, muse, agy or pi) was dispatched by an orchestrator and the dispatch failed. \
Classify WHY it failed, so the orchestrator can pick the right reaction: pause the provider and switch to a backup, ask for a re-login, retry the same provider, accept an external kill, or treat it as the agent's own failure. \
Judge mainly by the provider's own final error lines (often prefixed '! ', 'API Error', 'Error:' or 'ERROR'), the exit code, the signal and spar_error. \
Text the agent wrote, source code it edited, documentation it read and test output it ran are evidence about its work only, never about the provider's account state, even when they mention rate limits, quotas, 401s or 429s. \
A status line such as 'rate limit  seven_day  allowed_warning' is a warning that the request was still allowed, not a rejection.";

const QUOTA: &str = "The provider refused to serve because a usage allowance, rate limit, spending cap or credit balance is used up: HTTP 429 or 402 from the provider, 'usage limit', 'quota exhausted', 'out of credits', 'weekly/5-hour/session limit', 'resets at ...'. Waiting for the window or switching provider is the fix.";

const AUTH: &str = "The provider refused the credential: not logged in, session or token expired or revoked, invalid or disabled API key, HTTP 401/403 about authentication or account access. Logging in again is the fix.";

const TRANSIENT: &str = "A temporary provider or network fault: timeouts, connection reset, stream disconnected, 5xx, overloaded or 529, 'no response from API', service unavailable, a backend 404 for a model the account normally uses. Retrying the same provider shortly is the fix.";

const KILLED: &str = "The process was stopped from outside: operator halt ('halted by operator', spar stop), SIGTERM or SIGKILL (exit 137 or 143, or a signal is set), or the orchestrator's wall-clock ceiling ('hard ceiling', 'timed out').";

const GENUINE: &str = "The agent ran and failed at the task itself: it did not write its required artifact, asked a question instead of working, hit max turns or steps, overflowed its context, or gave up on failing code or tests.";

const OTHER: &str = "An orchestrator-side or environment problem that is none of the above: the review judged a different commit than expected, the orchestrator crashed, the orchestrator expected the wrong artifact, disk full, a bad command-line flag, or the CLI produced no output at all.";

/// What one vendor's errors mean that the generic criteria would misread.
fn vendor_note(adapter: &str) -> Option<&'static str> {
    match adapter {
        // Observed 22 times in the evaluation corpus; every one was followed by a
        // successful retry on the same account.
        "muse" => Some(
            " For muse: a final '! ' line saying a model 'does not exist or you lack access' after the agent already ran tools is muse's backend 404 for a model this account normally uses, so it is transient_infra, not auth.",
        ),
        _ => None,
    }
}

pub fn questions(adapter: &str) -> Questions {
    let transient = match vendor_note(adapter) {
        Some(note) => format!("{TRANSIENT}{note}"),
        None => TRANSIENT.to_string(),
    };
    Questions {
        failure_class: Question {
            kind: "choice",
            instructions: INSTRUCTIONS.to_string(),
            criteria: Criteria {
                quota_or_rate_limit: QUOTA.to_string(),
                auth: AUTH.to_string(),
                transient_infra: transient,
                killed_by_operator_or_signal: KILLED.to_string(),
                genuine_failure: GENUINE.to_string(),
                other: OTHER.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muse_gets_its_backend_404_note_and_others_do_not() {
        assert!(questions("muse")
            .failure_class
            .criteria
            .transient_infra
            .contains("does not exist or you lack access"));
        assert_eq!(
            questions("claude").failure_class.criteria.transient_infra,
            TRANSIENT
        );
    }

    #[test]
    fn labels_serialize_in_offer_order() {
        let v = serde_json::to_string(&questions("claude")).unwrap();
        let pos = |k: &str| v.find(&format!("\"{k}\"")).unwrap();
        assert!(pos("quota_or_rate_limit") < pos("auth"));
        assert!(pos("auth") < pos("transient_infra"));
        assert!(pos("genuine_failure") < pos("other"));
    }
}
