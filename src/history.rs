//! `.spar/history.jsonl`: one JSON line per run, written when a run reaches a terminal
//! phase and again just before `cleanup --purge` deletes it, so trends outlive the run
//! directory. Append-only; a run can have several lines and readers keep the one with
//! the latest `written_at` per `id` (O102). Schema 2 adds the per-dispatch ledger, slot
//! and round wall times and quota parks that `spar stats` reads (O131).

use crate::events::{Event, EventKind};
use crate::paths::SparPaths;
use crate::state::{ContextSemantics, Phase, RunState, SeatSource, SlotStatus};
use crate::workflow::review_result::{Finding, Severity, Verdict};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;

pub const HISTORY_SCHEMA: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Terminal,
    Purge,
    /// Never written: `spar stats` builds this row from a run still on disk.
    Live,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRow {
    pub slot: String,
    pub role: String,
    pub provider: String,
    pub model: Option<String>,
    #[serde(default)]
    pub source: Option<SeatSource>,
    /// The slot's last dispatch failed on quota or a refused credential, the failures
    /// that park a run at `quota`.
    #[serde(default)]
    pub parked_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRow {
    pub slot: String,
    pub verdict: Option<Verdict>,
    pub synthetic: bool,
    /// Critical plus major findings in this one review, deduplicated within it.
    #[serde(default)]
    pub blocking: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRow {
    pub round: u32,
    pub verdicts: Vec<VerdictRow>,
    pub findings: SeverityCounts,
}

/// One entry of `state.usage`, with the role its slot held.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchRow {
    pub slot: String,
    pub role: String,
    pub provider: String,
    pub model: Option<String>,
    pub billed_tokens: u64,
    pub cost_usd: Option<f64>,
    pub tools: u32,
    pub context_tokens: u64,
    pub context_semantics: ContextSemantics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotTime {
    pub slot: String,
    /// `None` for events written before phase events carried the round.
    pub round: Option<u32>,
    pub wall_secs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundTime {
    pub round: u32,
    /// First slot start to last slot end in the round, so time parked at a gate is
    /// not counted.
    pub wall_secs: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timing {
    pub rounds: Vec<RoundTime>,
    pub slots: Vec<SlotTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryRow {
    pub schema_version: u32,
    pub written_at: DateTime<Utc>,
    pub trigger: Trigger,
    pub id: String,
    pub task: Option<String>,
    pub workflow: crate::cli::WorkflowKind,
    pub dry_run: bool,
    pub phase: Phase,
    pub roles: Vec<RoleRow>,
    pub rounds: u32,
    pub max_rounds: u32,
    pub review_rounds: Vec<RoundRow>,
    pub findings: SeverityCounts,
    pub billed_tokens: u64,
    pub cost_usd: Option<f64>,
    pub pr_url: Option<String>,
    /// Set once `sync-prs` saw the PR merged (O123).
    pub merged_at: Option<DateTime<Utc>>,
    pub merge_commit: Option<String>,
    pub created_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    #[serde(default)]
    pub dispatches: Vec<DispatchRow>,
    #[serde(default)]
    pub timing: Timing,
    /// Times the run entered the `quota` phase.
    #[serde(default)]
    pub quota_parks: u32,
}

/// The role a slot id names, for usage entries whose slot was since renamed (a backup
/// takes a seat under a new id).
fn role_from_slot_id(id: &str) -> &'static str {
    let prefixes = [
        ("impl", "implementer"),
        ("review-", "reviewer"),
        ("planner", "planner"),
        ("critic", "plan_critic"),
        ("test-author", "test_author"),
        ("suite-", "tester"),
        ("ranker", "ranker"),
        ("peer", "peer"),
        ("reconcile-", "reconciler"),
    ];
    prefixes
        .iter()
        .find(|(p, _)| id.starts_with(p))
        .map(|(_, r)| *r)
        .unwrap_or("unknown")
}

fn normalized(f: &Finding) -> Finding {
    Finding {
        text: f
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase(),
        ..f.clone()
    }
}

fn timing(events: &[Event]) -> Timing {
    let mut round = None;
    let mut open: HashMap<&str, (DateTime<Utc>, Option<u32>)> = HashMap::new();
    let mut slots = Vec::new();
    let mut spans: BTreeMap<u32, (DateTime<Utc>, DateTime<Utc>)> = BTreeMap::new();
    for ev in events {
        if ev.kind == EventKind::Phase && ev.round.is_some() {
            round = ev.round;
        }
        let (EventKind::Slot, Some(slot), Some(status)) = (ev.kind, ev.slot.as_deref(), ev.status)
        else {
            continue;
        };
        if status == SlotStatus::Running {
            open.insert(slot, (ev.ts, round));
            continue;
        }
        let Some((start, r)) = open.remove(slot) else {
            continue;
        };
        slots.push(SlotTime {
            slot: slot.to_string(),
            round: r,
            wall_secs: (ev.ts - start).num_seconds(),
        });
        if let Some(r) = r {
            let span = spans.entry(r).or_insert((start, ev.ts));
            span.0 = span.0.min(start);
            span.1 = span.1.max(ev.ts);
        }
    }
    Timing {
        rounds: spans
            .into_iter()
            .map(|(round, (a, b))| RoundTime {
                round,
                wall_secs: (b - a).num_seconds(),
            })
            .collect(),
        slots,
    }
}

pub fn row(state: &RunState, events: &[Event], trigger: Trigger, now: DateTime<Utc>) -> HistoryRow {
    let mut rounds: BTreeMap<u32, RoundRow> = BTreeMap::new();
    let mut seen: HashSet<(u32, Finding)> = HashSet::new();
    for entry in &state.review_history {
        let r = rounds.entry(entry.round).or_insert_with(|| RoundRow {
            round: entry.round,
            verdicts: Vec::new(),
            findings: SeverityCounts::default(),
        });
        let mut own: HashSet<Finding> = HashSet::new();
        let mut blocking = 0;
        for f in &entry.review.findings {
            let key = normalized(f);
            if !f.synthetic
                && matches!(f.severity, Severity::Critical | Severity::Major)
                && own.insert(key.clone())
            {
                blocking += 1;
            }
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
        r.verdicts.push(VerdictRow {
            slot: entry.slot_id.clone(),
            verdict: entry.review.verdict,
            synthetic: entry.review.synthetic,
            blocking,
        });
    }
    let mut findings = SeverityCounts::default();
    for r in rounds.values() {
        findings.add(r.findings);
    }
    let costs: Vec<f64> = state.usage.iter().filter_map(|u| u.cost_usd).collect();
    let dispatches = state
        .usage
        .iter()
        .map(|u| {
            let slot = state.slots.iter().find(|s| s.id == u.slot_id);
            DispatchRow {
                slot: u.slot_id.clone(),
                role: slot
                    .map(|s| s.role.as_config_key())
                    .unwrap_or_else(|| role_from_slot_id(&u.slot_id))
                    .to_string(),
                provider: u.provider.clone(),
                model: u
                    .model
                    .clone()
                    .or_else(|| slot.and_then(|s| s.model.clone())),
                billed_tokens: u.billed_tokens,
                cost_usd: u.cost_usd,
                tools: u.tools,
                context_tokens: u.context_tokens,
                context_semantics: u.context_semantics,
            }
        })
        .collect();
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
                role: s.role.as_config_key().to_string(),
                provider: s.provider.clone(),
                model: s.model.clone(),
                source: s.source,
                parked_run: s.parks_on_quota(),
            })
            .collect(),
        rounds: state.round,
        max_rounds: state.max_rounds,
        review_rounds: rounds.into_values().collect(),
        findings,
        billed_tokens: state.usage.iter().map(|u| u.billed_tokens).sum(),
        cost_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
        pr_url: state.pr_url.clone(),
        merged_at: state.merged_at,
        merge_commit: state.merge_commit.clone(),
        created_at: state.created_at,
        ended_at: state.updated_at,
        dispatches,
        timing: timing(events),
        quota_parks: events
            .iter()
            .filter(|e| e.kind == EventKind::Phase && e.phase == Some(Phase::Quota))
            .count() as u32,
    }
}

/// Rows from `.spar/history.jsonl`, one per run id: the line with the latest
/// `written_at`. Lines that do not parse are skipped.
pub fn read_latest(paths: &SparPaths) -> Result<Vec<HistoryRow>> {
    let path = paths.history_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut latest: HashMap<String, HistoryRow> = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(row) = serde_json::from_str::<HistoryRow>(line) else {
            continue;
        };
        if latest
            .get(&row.id)
            .is_some_and(|prev| prev.written_at >= row.written_at)
        {
            continue;
        }
        latest.insert(row.id.clone(), row);
    }
    let mut rows: Vec<HistoryRow> = latest.into_values().collect();
    rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    Ok(rows)
}

/// One `write` per line under `O_APPEND`, so concurrent writers never interleave
/// inside a line. Best effort: a failure warns on stderr and never blocks the caller,
/// a purge included (O102).
pub fn append(paths: &SparPaths, state: &RunState, trigger: Trigger) {
    if let Err(e) = write_line(paths, state, trigger) {
        eprintln!("warning: run {} not recorded in history: {e:#}", state.id);
    }
}

fn write_line(paths: &SparPaths, state: &RunState, trigger: Trigger) -> Result<()> {
    let events = crate::events::read_all(paths, &state.id).unwrap_or_default();
    let mut line = serde_json::to_string(&row(state, &events, trigger, Utc::now()))?;
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
        let row = row(&st, &[], Trigger::Purge, Utc::now());
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

    fn ev(secs: i64, kind: EventKind) -> Event {
        let mut e = Event::info("x");
        e.ts = DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap();
        e.kind = kind;
        e.message = None;
        e
    }

    fn phase_ev(secs: i64, phase: Phase, round: Option<u32>) -> Event {
        let mut e = ev(secs, EventKind::Phase);
        e.phase = Some(phase);
        e.round = round;
        e
    }

    fn slot_ev(secs: i64, slot: &str, status: SlotStatus) -> Event {
        let mut e = ev(secs, EventKind::Slot);
        e.slot = Some(slot.into());
        e.status = Some(status);
        e
    }

    /// Round wall time spans slot activity only, so the hour parked at a gate between
    /// rounds is not charged; a slot with no round stamp is timed but not in a round.
    #[test]
    fn timing_spans_slot_activity_per_round_and_counts_quota_parks() {
        let events = vec![
            slot_ev(0, "planner", SlotStatus::Running),
            slot_ev(30, "planner", SlotStatus::Done),
            phase_ev(40, Phase::Dispatch, Some(1)),
            slot_ev(50, "impl", SlotStatus::Running),
            slot_ev(650, "impl", SlotStatus::Done),
            slot_ev(650, "review-0-cli-codex", SlotStatus::Running),
            slot_ev(800, "review-0-cli-codex", SlotStatus::Failed),
            phase_ev(801, Phase::Quota, Some(1)),
            phase_ev(4000, Phase::Dispatch, Some(2)),
            slot_ev(4000, "impl", SlotStatus::Running),
            slot_ev(4100, "impl", SlotStatus::Done),
        ];
        let st = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/x".into());
        let row = row(&st, &events, Trigger::Live, Utc::now());
        let slots: Vec<(&str, Option<u32>, i64)> = row
            .timing
            .slots
            .iter()
            .map(|t| (t.slot.as_str(), t.round, t.wall_secs))
            .collect();
        assert_eq!(
            slots,
            vec![
                ("planner", None, 30),
                ("impl", Some(1), 600),
                ("review-0-cli-codex", Some(1), 150),
                ("impl", Some(2), 100),
            ]
        );
        let rounds: Vec<(u32, i64)> = row
            .timing
            .rounds
            .iter()
            .map(|r| (r.round, r.wall_secs))
            .collect();
        assert_eq!(rounds, vec![(1, 750), (2, 100)]);
        assert_eq!(row.quota_parks, 1);
    }

    /// A dispatch whose slot was renamed by a backup still gets a role, from its id;
    /// blocking counts one reviewer's own critical and major findings only.
    #[test]
    fn dispatches_carry_role_and_reviews_carry_blocking() {
        let mut st = RunState::new("r1", crate::cli::WorkflowKind::Loop, "/x".into());
        st.slots.push(crate::executor::init_slot(
            "impl",
            "cli:muse",
            crate::state::SlotRole::Implementer,
        ));
        let mut gone = usage(10, None);
        gone.slot_id = "review-1-cli-codex".into();
        gone.provider = "cli:codex".into();
        let mut own = usage(20, Some(0.1));
        own.slot_id = "impl".into();
        own.model = Some("muse-spark-1.3".into());
        st.usage = vec![gone, own];
        st.review_history = vec![entry(
            1,
            "review-1-cli-codex",
            "## Verdict\nrequest_changes\n\n## Findings\n- severity: critical — a\n- severity: major — b\n- severity: major — b\n- severity: minor — c\n",
        )];
        let row = row(&st, &[], Trigger::Live, Utc::now());
        assert_eq!(row.dispatches[0].role, "reviewer");
        assert_eq!(row.dispatches[1].role, "implementer");
        assert_eq!(row.dispatches[1].model.as_deref(), Some("muse-spark-1.3"));
        assert_eq!(row.review_rounds[0].verdicts[0].blocking, 2);
    }

    /// The latest `written_at` wins per id; a schema 1 line (no dispatches, timing or
    /// parks) still reads.
    #[test]
    fn read_latest_keeps_the_newest_line_per_run_and_reads_schema_1() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(&paths.root).unwrap();
        let v1 = |id: &str, at: &str, phase: &str| {
            serde_json::json!({
                "schema_version": 1, "written_at": at, "trigger": "terminal", "id": id,
                "task": null, "workflow": "loop", "dry_run": false, "phase": phase,
                "roles": [{"slot": "impl", "role": "implementer", "provider": "cli:claude", "model": null}],
                "rounds": 1, "max_rounds": 5,
                "review_rounds": [{"round": 1, "verdicts": [{"slot": "review-0", "verdict": "approve", "synthetic": false}],
                                   "findings": {"critical": 0, "major": 0, "minor": 0, "synthetic": 0}}],
                "findings": {"critical": 0, "major": 0, "minor": 0, "synthetic": 0},
                "billed_tokens": 5, "cost_usd": null, "pr_url": null,
                "created_at": "2026-10-01T00:00:00Z", "ended_at": at,
            })
            .to_string()
        };
        let lines = [
            v1("a", "2026-10-02T00:00:00Z", "failed"),
            v1("a", "2026-10-03T00:00:00Z", "done"),
            v1("a", "2026-10-01T12:00:00Z", "stuck"),
            "not json".into(),
            v1("b", "2026-10-02T00:00:00Z", "quota"),
        ];
        std::fs::write(paths.history_file(), lines.join("\n") + "\n").unwrap();
        let rows = read_latest(&paths).unwrap();
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|r| r.id == "a").unwrap();
        assert_eq!(a.phase, Phase::Done);
        assert!(a.dispatches.is_empty());
        assert_eq!(a.review_rounds[0].verdicts[0].blocking, 0);
    }
}
