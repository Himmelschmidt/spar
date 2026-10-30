//! `.spar/history.jsonl`: one JSON line per run, written when a run reaches a terminal
//! phase and again just before `cleanup --purge` deletes it, so trends outlive the run
//! directory. Append-only; a run can have several lines and readers keep the one with
//! the latest `written_at` per `id` (O102).

use crate::paths::SparPaths;
use crate::state::RunState;
use crate::workflow::review_result::{Finding, Severity};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;

pub const HISTORY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Terminal,
    Purge,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SeverityCounts {
    pub critical: u32,
    pub major: u32,
    pub minor: u32,
    /// Findings spar wrote for a reviewer that produced nothing; never in the
    /// severity buckets.
    pub synthetic: u32,
}

impl SeverityCounts {
    fn add(&mut self, other: SeverityCounts) {
        self.critical += other.critical;
        self.major += other.major;
        self.minor += other.minor;
        self.synthetic += other.synthetic;
    }
}

#[derive(Debug, Serialize)]
pub struct RoleRow {
    pub slot: String,
    pub role: &'static str,
    pub provider: String,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerdictRow {
    pub slot: String,
    pub verdict: Option<crate::workflow::review_result::Verdict>,
    pub synthetic: bool,
}

#[derive(Debug, Serialize)]
pub struct RoundRow {
    pub round: u32,
    pub verdicts: Vec<VerdictRow>,
    pub findings: SeverityCounts,
}

#[derive(Debug, Serialize)]
pub struct HistoryRow {
    pub schema_version: u32,
    pub written_at: DateTime<Utc>,
    pub trigger: Trigger,
    pub id: String,
    pub task: Option<String>,
    pub workflow: crate::cli::WorkflowKind,
    pub dry_run: bool,
    pub phase: crate::state::Phase,
    pub roles: Vec<RoleRow>,
    pub rounds: u32,
    pub max_rounds: u32,
    pub review_rounds: Vec<RoundRow>,
    pub findings: SeverityCounts,
    pub billed_tokens: u64,
    pub cost_usd: Option<f64>,
    pub pr_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

pub fn row(state: &RunState, trigger: Trigger, now: DateTime<Utc>) -> HistoryRow {
    let mut rounds: BTreeMap<u32, RoundRow> = BTreeMap::new();
    let mut seen: HashSet<(u32, Finding)> = HashSet::new();
    for entry in &state.review_history {
        let r = rounds.entry(entry.round).or_insert_with(|| RoundRow {
            round: entry.round,
            verdicts: Vec::new(),
            findings: SeverityCounts::default(),
        });
        r.verdicts.push(VerdictRow {
            slot: entry.slot_id.clone(),
            verdict: entry.review.verdict,
            synthetic: entry.review.synthetic,
        });
        for f in &entry.review.findings {
            let key = Finding {
                text: f
                    .text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase(),
                ..f.clone()
            };
            if !seen.insert((entry.round, key)) {
                continue;
            }
            let bucket = if f.synthetic {
                &mut r.findings.synthetic
            } else {
                match f.severity {
                    Severity::Critical => &mut r.findings.critical,
                    Severity::Major => &mut r.findings.major,
                    Severity::Minor => &mut r.findings.minor,
                }
            };
            *bucket += 1;
        }
    }
    let mut findings = SeverityCounts::default();
    for r in rounds.values() {
        findings.add(r.findings);
    }
    let costs: Vec<f64> = state.usage.iter().filter_map(|u| u.cost_usd).collect();
    HistoryRow {
        schema_version: HISTORY_SCHEMA,
        written_at: now,
        trigger,
        id: state.id.clone(),
        task: state.task.as_deref().and_then(|t| {
            t.lines()
                .map(|l| l.trim_start_matches('#').trim())
                .find(|l| !l.is_empty())
                .map(str::to_string)
        }),
        workflow: state.workflow,
        dry_run: state.dry_run,
        phase: state.phase,
        roles: state
            .slots
            .iter()
            .map(|s| RoleRow {
                slot: s.id.clone(),
                role: s.role.as_config_key(),
                provider: s.provider.clone(),
                model: s.model.clone(),
            })
            .collect(),
        rounds: state.round,
        max_rounds: state.max_rounds,
        review_rounds: rounds.into_values().collect(),
        findings,
        billed_tokens: state.usage.iter().map(|u| u.billed_tokens).sum(),
        cost_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
        pr_url: state.pr_url.clone(),
        created_at: state.created_at,
        ended_at: state.updated_at,
    }
}

/// One `write` per line under `O_APPEND`, so concurrent writers never interleave
/// inside a line.
pub fn append(paths: &SparPaths, state: &RunState, trigger: Trigger) -> Result<()> {
    let mut line = serde_json::to_string(&row(state, trigger, Utc::now()))?;
    line.push('\n');
    std::fs::create_dir_all(&paths.root)?;
    let path = paths.history_file();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ReviewHistoryEntry, SlotUsage};
    use crate::workflow::review_result::parse_review;

    fn entry(round: u32, slot: &str, body: &str) -> ReviewHistoryEntry {
        ReviewHistoryEntry {
            round,
            slot_id: slot.into(),
            review: parse_review(body),
        }
    }

    fn usage(billed: u64, cost: Option<f64>) -> SlotUsage {
        serde_json::from_value(serde_json::json!({
            "slot_id": "s", "provider": "cli:claude", "input_tokens": 0,
            "output_tokens": 0, "billed_tokens": billed, "cost_usd": cost,
        }))
        .unwrap()
    }

    /// Two reviewers reporting the same finding in one round count it once; the same
    /// finding again next round counts again; spar's stand-in finding is kept out of
    /// the severity buckets.
    #[test]
    fn row_dedupes_findings_per_round_and_sums_usage() {
        let mut st = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/x".into());
        st.task = Some("# Add history\n\nbody".into());
        st.round = 2;
        st.phase = crate::state::Phase::Done;
        let shared = "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — leaks at a.rs:1\n- severity: minor — nit\n";
        st.review_history = vec![
            entry(1, "review-0", shared),
            entry(1, "review-1", "## Verdict\nrequest_changes\n\n## Findings\n- severity: major — leaks  at a.rs:1\n"),
            entry(
                1,
                "review-2",
                &format!(
                    "{}\n## Verdict\nrequest_changes\n\n## Findings\n- severity: major — review slot `review-2` failed or produced no artifact\n",
                    crate::workflow::review_result::SYNTHETIC_REVIEW_MARKER
                ),
            ),
            entry(2, "review-0", shared),
            entry(2, "review-1", "## Verdict\napprove\n"),
        ];
        st.usage = vec![
            usage(100, Some(0.5)),
            usage(50, None),
            usage(25, Some(0.25)),
        ];
        let row = row(&st, Trigger::Purge, Utc::now());
        assert_eq!(row.task.as_deref(), Some("Add history"));
        assert_eq!(row.review_rounds.len(), 2);
        let r1 = &row.review_rounds[0];
        assert_eq!(r1.verdicts.len(), 3);
        assert_eq!(
            r1.findings,
            SeverityCounts {
                critical: 0,
                major: 1,
                minor: 1,
                synthetic: 1
            }
        );
        assert!(r1.verdicts[2].synthetic);
        assert_eq!(
            row.findings,
            SeverityCounts {
                critical: 0,
                major: 2,
                minor: 2,
                synthetic: 1
            }
        );
        assert_eq!(row.billed_tokens, 175);
        assert_eq!(row.cost_usd, Some(0.75));
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["trigger"], "purge");
        assert_eq!(v["schema_version"], HISTORY_SCHEMA);
        assert_eq!(v["review_rounds"][1]["verdicts"][1]["verdict"], "approve");
        assert_eq!(v["phase"], "done");
    }
}
