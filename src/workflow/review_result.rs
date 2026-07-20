//! Parser for a reviewer's markdown artifact.
//!
//! The verdict is read as an *anchored header*: the first non-blank line under the
//! first `## Verdict` heading. An unanchored substring scan is not usable here —
//! reviewer templates spell out `approve | request_changes` in their own format
//! block, and reviewers routinely echo it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcStatus {
    Pass,
    Fail,
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcLine {
    pub id: String,
    pub status: AcStatus,
    pub evidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewResult {
    /// `None` means no parsable verdict. Callers treat that as blocking.
    pub verdict: Option<Verdict>,
    pub acceptance: Vec<AcLine>,
}

/// Same noise set as `parse_suite_result`.
const NOISE: [char; 5] = ['*', '`', '_', '-', ' '];

#[derive(PartialEq)]
enum Section {
    Other,
    Verdict,
    Acceptance,
}

pub fn parse_review(body: &str) -> ReviewResult {
    let mut out = ReviewResult::default();
    let mut section = Section::Other;
    let mut verdict_done = false;

    for raw in body.lines() {
        if let Some(rest) = raw.trim_start().strip_prefix("##") {
            let title = rest
                .trim_start_matches('#')
                .trim()
                .trim_matches(['*', '`', ':', ' '])
                .to_ascii_lowercase();
            section = match title.as_str() {
                // Latch on the heading so a later ## Verdict cannot fill an empty first section.
                "verdict" if !verdict_done => {
                    verdict_done = true;
                    Section::Verdict
                }
                "acceptance" => Section::Acceptance,
                _ => Section::Other,
            };
            continue;
        }
        match section {
            Section::Verdict => {
                if raw.trim().is_empty() {
                    continue;
                }
                out.verdict = parse_verdict_line(raw);
                section = Section::Other;
            }
            Section::Acceptance => {
                if let Some(line) = parse_ac_line(raw) {
                    out.acceptance.push(line);
                }
            }
            Section::Other => {}
        }
    }
    out
}

/// `request_changes` is tested first so a hedged `request_changes (see findings)`
/// is not mis-scored, and so `approve` never matches `approve is not warranted`.
fn parse_verdict_line(raw: &str) -> Option<Verdict> {
    let line = raw
        .trim()
        .trim_start_matches(NOISE)
        .to_ascii_lowercase()
        .replace(['*', '`'], "");
    if is_verdict_token(&line, "request_changes") {
        return Some(Verdict::RequestChanges);
    }
    if is_verdict_token(&line, "approve") {
        return Some(Verdict::Approve);
    }
    None
}

/// Token must open the line. Rest may be empty, a parenthetical hedge, or trailing
/// sentence punctuation only — not a format skeleton (`approve | request_changes`)
/// and not prose (`approve is not warranted`).
fn is_verdict_token(line: &str, token: &str) -> bool {
    let Some(rest) = line.strip_prefix(token) else {
        return false;
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return true;
    }
    if rest.starts_with('(') {
        return true;
    }
    rest.chars()
        .all(|c| matches!(c, '.' | '!' | '?' | ',' | ';' | ':'))
}

fn parse_ac_line(raw: &str) -> Option<AcLine> {
    let (id_part, rest) = raw
        .trim()
        .trim_start_matches(['-', '*', ' '])
        .split_once(':')?;
    let id = id_part
        .trim()
        .trim_matches(['*', '`', ' '])
        .to_ascii_uppercase();
    let digits = id.strip_prefix("AC-")?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let rest = rest.trim();
    let (word, evidence) = rest
        .split_once('—')
        .or_else(|| rest.split_once(" - "))
        .unwrap_or((rest, ""));
    let status = match word
        .trim()
        .trim_matches(['*', '`', ' '])
        .to_ascii_lowercase()
        .as_str()
    {
        "pass" => AcStatus::Pass,
        "fail" => AcStatus::Fail,
        "unverified" => AcStatus::Unverified,
        _ => return None,
    };
    Some(AcLine {
        id,
        status,
        evidence: evidence.trim().to_string(),
    })
}

/// Every `AC-<digits>` token in a contract body, uppercased, deduplicated,
/// in first-appearance order.
#[allow(dead_code)] // call site lands with the acceptance gate (Priority 3)
pub fn parse_contract_criteria(body: &str) -> Vec<String> {
    let upper = body.to_ascii_uppercase();
    let b = upper.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if &b[i..i + 3] == b"AC-" && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let mut j = i + 3;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 3 {
                let tok = upper[i..j].to_string();
                if !out.contains(&tok) {
                    out.push(tok);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(body: &str) -> Option<Verdict> {
        parse_review(body).verdict
    }

    #[test]
    fn verdict_approve() {
        assert_eq!(verdict("## Verdict\napprove\n"), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_request_changes() {
        assert_eq!(
            verdict("## Verdict\nrequest_changes\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn verdict_blank_line_after_header() {
        assert_eq!(verdict("## Verdict\n\n\napprove\n"), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_markup_tolerated() {
        assert_eq!(verdict("## Verdict\n**approve**\n"), Some(Verdict::Approve));
        assert_eq!(verdict("## Verdict\n- `approve`\n"), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_hedged_request_changes() {
        assert_eq!(
            verdict("## Verdict\nrequest_changes (see findings)\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn approve_body_mentioning_request_changes() {
        let body = "## Verdict\napprove\n\n## Findings\n- I considered request_changes but the fix landed.\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));
    }

    #[test]
    fn format_block_echo_does_not_flip() {
        let body = "## Verdict\napprove\n\n## Format\napprove | request_changes\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));
    }

    #[test]
    fn first_verdict_section_wins() {
        let body = "## Verdict\napprove\n\n## Verdict\nrequest_changes\n";
        assert_eq!(verdict(body), Some(Verdict::Approve));
    }

    #[test]
    fn verdict_missing_is_none() {
        assert_eq!(verdict("## Findings\n- nothing to report\n"), None);
    }

    #[test]
    fn verdict_garbage_is_none() {
        assert_eq!(verdict("## Verdict\nlgtm\n"), None);
        assert_eq!(verdict("## Verdict\napprove is not warranted\n"), None);
    }

    #[test]
    fn format_skeleton_as_verdict_is_none() {
        // templates/reviewer_adversarial.md puts this under ## Verdict as the format
        // skeleton. A reviewer that pastes it without choosing must not ship.
        assert_eq!(verdict("## Verdict\napprove | request_changes\n"), None);
        assert_eq!(verdict("## Verdict\n`approve | request_changes`\n"), None);
    }

    #[test]
    fn empty_first_verdict_section_stays_none() {
        let body = "## Verdict\n\n## Other\nnoise\n\n## Verdict\napprove\n";
        assert_eq!(verdict(body), None);
    }

    #[test]
    fn trailing_punctuation_ok() {
        assert_eq!(verdict("## Verdict\napprove.\n"), Some(Verdict::Approve));
        assert_eq!(
            verdict("## Verdict\nrequest_changes!\n"),
            Some(Verdict::RequestChanges)
        );
    }

    #[test]
    fn acceptance_parses_all_three_statuses() {
        let r = parse_review("## Acceptance\nac-1: pass\nAC-2: FAIL\nAC-3: Unverified\n");
        assert_eq!(
            r.acceptance
                .iter()
                .map(|a| (a.id.as_str(), a.status))
                .collect::<Vec<_>>(),
            vec![
                ("AC-1", AcStatus::Pass),
                ("AC-2", AcStatus::Fail),
                ("AC-3", AcStatus::Unverified),
            ]
        );
    }

    #[test]
    fn acceptance_evidence_captured() {
        let r = parse_review("## Acceptance\nAC-1: pass — cargo test output\n");
        assert_eq!(r.acceptance[0].evidence, "cargo test output");
    }

    #[test]
    fn acceptance_hyphen_separator() {
        let r = parse_review("## Acceptance\nAC-1: pass - foo\n");
        assert_eq!(r.acceptance[0].evidence, "foo");
    }

    #[test]
    fn acceptance_bulleted_lines() {
        let r = parse_review("## Acceptance\n- AC-1: pass — x\n");
        assert_eq!(r.acceptance[0].id, "AC-1");
        assert_eq!(r.acceptance[0].evidence, "x");
    }

    #[test]
    fn acceptance_malformed_line_skipped() {
        let body =
            "## Acceptance\nAC-1: pass — a\nI could not verify everything here.\nAC-2: fail — b\n";
        let r = parse_review(body);
        assert_eq!(r.acceptance.len(), 2);
        assert_eq!(r.acceptance[1].id, "AC-2");
    }

    #[test]
    fn acceptance_missing_section_is_empty() {
        assert!(parse_review("## Verdict\napprove\n").acceptance.is_empty());
    }

    #[test]
    fn contract_criteria_extracted_in_order() {
        let body = "## Scenarios\n- AC-1 foo\n- AC-3 bar\n- AC-2 baz\n";
        assert_eq!(parse_contract_criteria(body), ["AC-1", "AC-3", "AC-2"]);
    }

    #[test]
    fn contract_criteria_deduplicated() {
        let body = "## Scenarios\nac-1: thing\n\n## Notes\nAC-1 again\n";
        assert_eq!(parse_contract_criteria(body), ["AC-1"]);
    }

    #[test]
    fn contract_criteria_empty_when_absent() {
        assert!(parse_contract_criteria("## Scenarios\nnothing here\n").is_empty());
    }
}
