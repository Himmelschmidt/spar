//! `spar stats`: aggregates over data spar already writes. Runs still on disk are read
//! from their state and events, purged runs from `.spar/history.jsonl` (the latest line
//! per run), and provider pauses from `.spar/quota-events.jsonl` (O131).

use crate::history::{self, HistoryRow, Trigger};
use crate::paths::SparPaths;
use crate::state::{ContextSemantics, Phase, RunState, SeatSource};
use crate::workflow::review_result::Verdict;
use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub const STATS_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Model,
    Role,
    Provider,
    Project,
}

#[derive(Debug, Default, Serialize)]
pub struct Outcomes {
    /// Reached `awaiting_ship_confirm`, shipping or done.
    pub ship_gate: u32,
    /// Parked at the round ceiling (`awaiting_round_extension`).
    pub ceiling: u32,
    pub stuck: u32,
    pub failed: u32,
    pub quota: u32,
    /// Anything else: in flight, stopped, escalated, a plan-only end.
    pub other: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct Rounds {
    pub mean_per_run: Option<f64>,
    /// Mean round of the first all-approve verdict, over the runs that reached one.
    pub mean_to_first_approve: Option<f64>,
    pub runs_with_approve: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct Reviews {
    /// Reviews a reviewer actually wrote.
    pub reviews: u32,
    /// Reviews spar wrote for a reviewer that produced nothing; never in `reviews`.
    pub synthetic_reviews: u32,
    pub blocking_findings: u32,
    pub blocking_per_review: Option<f64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Usage {
    pub billed_tokens: u64,
    /// Sum over the dispatches that reported a cost; `null` when none did.
    pub cost_usd: Option<f64>,
    pub dispatches_with_cost: u32,
    pub dispatches_without_cost: u32,
    pub tool_calls: u64,
}

/// `context_tokens` for one `context_semantics`. Values measured differently are
/// never combined (O96).
#[derive(Debug, Default, Serialize)]
pub struct Context {
    pub semantics: String,
    pub dispatches: u32,
    pub max: u64,
    pub mean: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct WallTime {
    pub rounds_timed: u32,
    pub round_secs_mean: Option<f64>,
    pub round_secs_total: i64,
    pub slots_timed: u32,
    pub slot_secs_mean: Option<f64>,
    pub slot_secs_total: i64,
}

#[derive(Debug, Default, Serialize)]
pub struct Group {
    pub key: String,
    pub runs: u32,
    pub dispatches: u32,
    pub rounds: Rounds,
    pub outcomes: Outcomes,
    pub reviews: Reviews,
    pub usage: Usage,
    pub context: Vec<Context>,
    pub wall_time: WallTime,
}

#[derive(Debug, Default, Serialize)]
pub struct QuotaRow {
    pub provider: String,
    /// Pauses on a quota signal.
    pub pauses: u32,
    pub manual_pauses: u32,
    /// Times a run entered the `quota` phase, charged to the providers whose slot hit
    /// quota (`unknown` when none was recorded).
    pub parks: u32,
    /// Seats a declared backup took over, charged to the backup.
    pub backup_engagements: u32,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub since: Option<DateTime<Utc>>,
    pub by: GroupBy,
    pub projects: Vec<PathBuf>,
    pub runs: u32,
    pub runs_from_history: u32,
    pub runs_live: u32,
    pub excluded_dry_runs: u32,
    pub groups: Vec<Group>,
    pub quota: Vec<QuotaRow>,
}

struct Run {
    project: String,
    row: HistoryRow,
}

#[derive(Default)]
struct Acc {
    runs: HashSet<(String, String)>,
    rounds: Vec<u32>,
    to_approve: Vec<u32>,
    outcomes: Outcomes,
    dispatches: u32,
    reviews: Reviews,
    usage: Usage,
    cost: f64,
    context: BTreeMap<String, (u32, u64, u64)>,
    round_secs: Vec<i64>,
    slot_secs: Vec<i64>,
}

fn mean<T: Copy + Into<f64>>(v: &[T]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().map(|x| (*x).into()).sum::<f64>() / v.len() as f64)
}

fn mean_i64(v: &[i64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<i64>() as f64 / v.len() as f64)
}

fn semantics_name(s: ContextSemantics) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn first_all_approve(row: &HistoryRow) -> Option<u32> {
    row.review_rounds
        .iter()
        .find(|r| {
            !r.verdicts.is_empty()
                && r.verdicts
                    .iter()
                    .all(|v| !v.synthetic && v.verdict == Some(Verdict::Approve))
        })
        .map(|r| r.round)
}

fn outcome(acc: &mut Outcomes, phase: Phase) {
    let bucket = match phase {
        Phase::AwaitingShipConfirm | Phase::Shipping | Phase::Done => &mut acc.ship_gate,
        Phase::AwaitingRoundExtension => &mut acc.ceiling,
        Phase::Stuck => &mut acc.stuck,
        Phase::Failed => &mut acc.failed,
        Phase::Quota => &mut acc.quota,
        _ => &mut acc.other,
    };
    *bucket += 1;
}

/// Who a slot was, as `(role, provider, model)`. The model is the one the provider
/// reported for the slot's last dispatch when there is one, else the one requested.
fn seats(row: &HistoryRow) -> HashMap<&str, (&str, &str, Option<&str>)> {
    let mut out: HashMap<&str, (&str, &str, Option<&str>)> = HashMap::new();
    for r in &row.roles {
        out.insert(
            &r.slot,
            (r.role.as_str(), r.provider.as_str(), r.model.as_deref()),
        );
    }
    for d in &row.dispatches {
        out.insert(
            &d.slot,
            (d.role.as_str(), d.provider.as_str(), d.model.as_deref()),
        );
    }
    out
}

fn key(by: GroupBy, project: &str, seat: (&str, &str, Option<&str>)) -> String {
    let (role, provider, model) = seat;
    match by {
        GroupBy::Project => project.to_string(),
        GroupBy::Role => role.to_string(),
        GroupBy::Provider => crate::quota::normalize_key(provider),
        GroupBy::Model => model.unwrap_or("unknown").to_string(),
    }
}

fn add_run(groups: &mut BTreeMap<String, Acc>, by: GroupBy, run: &Run) {
    let row = &run.row;
    let seats = seats(row);
    let seat_key = |slot: &str| {
        seats
            .get(slot)
            .map(|s| key(by, &run.project, *s))
            .unwrap_or_else(|| match by {
                GroupBy::Project => run.project.clone(),
                _ => "unknown".into(),
            })
    };
    let mut members: HashSet<String> = seats.keys().map(|s| seat_key(s)).collect();
    if members.is_empty() || by == GroupBy::Project {
        members = HashSet::from([run.project.clone()]);
    }
    let approve = first_all_approve(row);
    for m in &members {
        let acc = groups.entry(m.clone()).or_default();
        acc.runs.insert((run.project.clone(), row.id.clone()));
        acc.rounds.push(row.rounds);
        if let Some(r) = approve {
            acc.to_approve.push(r);
        }
        outcome(&mut acc.outcomes, row.phase);
    }
    for d in &row.dispatches {
        let acc = groups
            .entry(key(
                by,
                &run.project,
                (&d.role, &d.provider, d.model.as_deref()),
            ))
            .or_default();
        acc.dispatches += 1;
        acc.usage.billed_tokens += d.billed_tokens;
        acc.usage.tool_calls += d.tools as u64;
        match d.cost_usd {
            Some(c) => {
                acc.cost += c;
                acc.usage.dispatches_with_cost += 1;
            }
            None => acc.usage.dispatches_without_cost += 1,
        }
        let ctx = acc
            .context
            .entry(semantics_name(d.context_semantics))
            .or_default();
        ctx.0 += 1;
        ctx.1 = ctx.1.max(d.context_tokens);
        ctx.2 += d.context_tokens;
    }
    for round in &row.review_rounds {
        for v in &round.verdicts {
            let acc = groups.entry(seat_key(&v.slot)).or_default();
            if v.synthetic {
                acc.reviews.synthetic_reviews += 1;
                continue;
            }
            acc.reviews.reviews += 1;
            acc.reviews.blocking_findings += v.blocking;
        }
    }
    let mut round_members: HashMap<u32, HashSet<String>> = HashMap::new();
    for t in &row.timing.slots {
        let k = seat_key(&t.slot);
        groups
            .entry(k.clone())
            .or_default()
            .slot_secs
            .push(t.wall_secs);
        if let Some(r) = t.round {
            round_members.entry(r).or_default().insert(k);
        }
    }
    for rt in &row.timing.rounds {
        for k in round_members.get(&rt.round).into_iter().flatten() {
            groups
                .entry(k.clone())
                .or_default()
                .round_secs
                .push(rt.wall_secs);
        }
    }
}

fn finish(key: String, acc: Acc) -> Group {
    let mut usage = acc.usage;
    usage.cost_usd = (usage.dispatches_with_cost > 0).then_some(acc.cost);
    let mut reviews = acc.reviews;
    reviews.blocking_per_review =
        (reviews.reviews > 0).then(|| reviews.blocking_findings as f64 / reviews.reviews as f64);
    Group {
        key,
        runs: acc.runs.len() as u32,
        dispatches: acc.dispatches,
        rounds: Rounds {
            mean_per_run: mean(&acc.rounds),
            mean_to_first_approve: mean(&acc.to_approve),
            runs_with_approve: acc.to_approve.len() as u32,
        },
        outcomes: acc.outcomes,
        reviews,
        usage,
        context: acc
            .context
            .into_iter()
            .map(|(semantics, (n, max, sum))| Context {
                semantics,
                dispatches: n,
                max,
                mean: sum as f64 / n as f64,
            })
            .collect(),
        wall_time: WallTime {
            rounds_timed: acc.round_secs.len() as u32,
            round_secs_mean: mean_i64(&acc.round_secs),
            round_secs_total: acc.round_secs.iter().sum(),
            slots_timed: acc.slot_secs.len() as u32,
            slot_secs_mean: mean_i64(&acc.slot_secs),
            slot_secs_total: acc.slot_secs.iter().sum(),
        },
    }
}

fn project_name(root: &std::path::Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".")
        .to_string()
}

/// Every run of one project. A run whose directory still exists is read from it: its
/// history line is a snapshot of the same state and goes stale when a terminal phase
/// such as `plan_approved` is continued. History supplies the purged ones.
fn project_runs(paths: &SparPaths, now: DateTime<Utc>) -> Result<Vec<HistoryRow>> {
    let mut rows = Vec::new();
    let mut on_disk: HashSet<String> = HashSet::new();
    if let Ok(dir) = std::fs::read_dir(paths.runs_dir()) {
        let mut ids: Vec<String> = dir
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        for id in ids {
            let Ok(state) = RunState::load(paths, &id) else {
                continue;
            };
            let events = crate::events::read_all(paths, &id).unwrap_or_default();
            rows.push(history::row(&state, &events, Trigger::Live, now));
            on_disk.insert(id);
        }
    }
    rows.extend(
        history::read_latest(paths)?
            .into_iter()
            .filter(|r| !on_disk.contains(&r.id)),
    );
    Ok(rows)
}

fn quota_row(rows: &mut BTreeMap<String, QuotaRow>, provider: String) -> &mut QuotaRow {
    rows.entry(provider.clone()).or_insert_with(|| QuotaRow {
        provider,
        ..Default::default()
    })
}

pub fn report(
    projects: &[PathBuf],
    since: Option<DateTime<Utc>>,
    by: GroupBy,
    include_dry_run: bool,
    now: DateTime<Utc>,
) -> Result<Report> {
    let mut runs = Vec::new();
    let (mut from_history, mut live_total, mut dry) = (0, 0, 0);
    let mut quota: BTreeMap<String, QuotaRow> = BTreeMap::new();
    for root in projects {
        let paths = SparPaths::new(root);
        for row in project_runs(&paths, now)? {
            if since.is_some_and(|s| row.ended_at < s) {
                continue;
            }
            if row.dry_run && !include_dry_run {
                dry += 1;
                continue;
            }
            if row.trigger == Trigger::Live {
                live_total += 1;
            } else {
                from_history += 1;
            }
            runs.push(Run {
                project: project_name(root),
                row,
            });
        }
        for ev in crate::quota::read_events(&paths) {
            if since.is_some_and(|s| ev.ts < s) {
                continue;
            }
            let q = quota_row(&mut quota, crate::quota::normalize_key(&ev.provider));
            if ev.manual {
                q.manual_pauses += 1;
            } else {
                q.pauses += 1;
            }
        }
    }
    let mut groups: BTreeMap<String, Acc> = BTreeMap::new();
    for run in &runs {
        add_run(&mut groups, by, run);
        for r in &run.row.roles {
            if r.source == Some(SeatSource::Backup) {
                quota_row(&mut quota, crate::quota::normalize_key(&r.provider))
                    .backup_engagements += 1;
            }
        }
        if run.row.quota_parks > 0 {
            let parks = run.row.quota_parks;
            let mut hit: Vec<String> = run
                .row
                .roles
                .iter()
                .filter(|r| r.quota_hit)
                .map(|r| crate::quota::normalize_key(&r.provider))
                .collect();
            hit.sort();
            hit.dedup();
            if hit.is_empty() {
                hit.push("unknown".into());
            }
            for p in hit {
                quota_row(&mut quota, p).parks += parks;
            }
        }
    }
    Ok(Report {
        schema_version: STATS_SCHEMA,
        generated_at: now,
        since,
        by,
        projects: projects.to_vec(),
        runs: runs.len() as u32,
        runs_from_history: from_history,
        runs_live: live_total,
        excluded_dry_runs: dry,
        groups: groups.into_iter().map(|(k, a)| finish(k, a)).collect(),
        quota: quota.into_values().collect(),
    })
}

fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn secs(s: Option<f64>) -> String {
    match s {
        None => "-".into(),
        Some(s) if s >= 3600.0 => format!("{:.1}h", s / 3600.0),
        Some(s) if s >= 60.0 => format!("{:.0}m", s / 60.0),
        Some(s) => format!("{s:.0}s"),
    }
}

fn opt(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into())
}

fn pct(n: u32, of: u32) -> String {
    if of == 0 {
        return "-".into();
    }
    format!("{:.0}%", n as f64 * 100.0 / of as f64)
}

pub fn print_human(r: &Report) {
    let since = r
        .since
        .map(|s| format!(" since {}", s.format("%Y-%m-%d %H:%M")))
        .unwrap_or_default();
    println!(
        "{} runs{since} ({} from history, {} live) across {} project(s), by {}",
        r.runs,
        r.runs_from_history,
        r.runs_live,
        r.projects.len(),
        format!("{:?}", r.by).to_lowercase()
    );
    if r.excluded_dry_runs > 0 {
        println!(
            "{} dry runs excluded (--include-dry-run)",
            r.excluded_dry_runs
        );
    }
    for g in &r.groups {
        let o = &g.outcomes;
        println!("\n{}  runs {}  dispatches {}", g.key, g.runs, g.dispatches);
        println!(
            "  rounds/run {}  to first approve {} ({} runs)",
            opt(g.rounds.mean_per_run),
            opt(g.rounds.mean_to_first_approve),
            g.rounds.runs_with_approve
        );
        println!(
            "  ship gate {}  ceiling {}  stuck {}  failed {}  quota {}  other {}",
            pct(o.ship_gate, g.runs),
            pct(o.ceiling, g.runs),
            pct(o.stuck, g.runs),
            pct(o.failed, g.runs),
            pct(o.quota, g.runs),
            pct(o.other, g.runs)
        );
        if g.reviews.reviews + g.reviews.synthetic_reviews > 0 {
            println!(
                "  reviews {}  blocking/review {}  synthetic {}",
                g.reviews.reviews,
                g.reviews
                    .blocking_per_review
                    .map(|v| format!("{v:.2}"))
                    .unwrap_or_else(|| "-".into()),
                g.reviews.synthetic_reviews
            );
        }
        let cost = match g.usage.cost_usd {
            Some(c) => format!("${c:.2}"),
            None => "not reported".into(),
        };
        let missing = if g.usage.dispatches_without_cost > 0 && g.usage.cost_usd.is_some() {
            format!(
                " ({} of {} dispatches reported no cost)",
                g.usage.dispatches_without_cost, g.dispatches
            )
        } else {
            String::new()
        };
        println!(
            "  billed {}  cost {cost}{missing}  tool calls {}",
            tokens(g.usage.billed_tokens),
            g.usage.tool_calls
        );
        for c in &g.context {
            println!(
                "  context ({}) max {} mean {} over {} dispatches",
                c.semantics,
                tokens(c.max),
                tokens(c.mean as u64),
                c.dispatches
            );
        }
        let w = &g.wall_time;
        println!(
            "  wall round {} ({} timed)  slot {} ({} timed)",
            secs(w.round_secs_mean),
            w.rounds_timed,
            secs(w.slot_secs_mean),
            w.slots_timed
        );
    }
    if !r.quota.is_empty() {
        println!("\nquota by provider:");
        for q in &r.quota {
            println!(
                "  {:<14} pauses {}  manual {}  parks {}  backups {}",
                q.provider, q.pauses, q.manual_pauses, q.parks, q.backup_engagements
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dispatch(
        slot: &str,
        role: &str,
        provider: &str,
        model: &str,
        cost: Option<f64>,
        ctx: u64,
        sem: &str,
    ) -> serde_json::Value {
        json!({"slot": slot, "role": role, "provider": provider, "model": model,
               "billed_tokens": 1000, "cost_usd": cost, "tools": 3,
               "context_tokens": ctx, "context_semantics": sem})
    }

    fn history_row(
        id: &str,
        phase: &str,
        ended: &str,
        dispatches: Vec<serde_json::Value>,
        verdicts: Vec<(u32, &str, &str, bool, u32)>,
    ) -> String {
        let mut rounds: BTreeMap<u32, Vec<serde_json::Value>> = BTreeMap::new();
        for (round, slot, verdict, synthetic, blocking) in verdicts {
            rounds.entry(round).or_default().push(json!({"slot": slot, "verdict": verdict, "synthetic": synthetic, "blocking": blocking}));
        }
        let review_rounds: Vec<_> = rounds
            .into_iter()
            .map(|(round, verdicts)| {
                json!({"round": round, "verdicts": verdicts,
                "findings": {"critical": 0, "major": 0, "minor": 0, "synthetic": 0}})
            })
            .collect();
        json!({
            "schema_version": 2, "written_at": ended, "trigger": "terminal", "id": id,
            "task": "t", "workflow": "loop", "dry_run": false, "phase": phase,
            "roles": [
                {"slot": "impl", "role": "implementer", "provider": "cli:claude", "model": "opus", "source": null, "quota_hit": false},
                {"slot": "review-0-cli-codex", "role": "reviewer", "provider": "cli:codex", "model": null, "source": "backup", "quota_hit": true}
            ],
            "rounds": 2, "max_rounds": 3, "review_rounds": review_rounds,
            "findings": {"critical": 0, "major": 0, "minor": 0, "synthetic": 0},
            "billed_tokens": 0, "cost_usd": null, "pr_url": null,
            "created_at": "2026-10-01T00:00:00Z", "ended_at": ended,
            "dispatches": dispatches,
            "timing": {"rounds": [{"round": 1, "wall_secs": 600}, {"round": 2, "wall_secs": 300}],
                       "slots": [{"slot": "impl", "round": 1, "wall_secs": 500},
                                 {"slot": "review-0-cli-codex", "round": 1, "wall_secs": 100},
                                 {"slot": "impl", "round": 2, "wall_secs": 300}]},
            "quota_parks": 1
        })
        .to_string()
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        std::fs::create_dir_all(&paths.root).unwrap();
        let rows = [
            history_row(
                "a",
                "done",
                "2026-10-20T00:00:00Z",
                vec![
                    dispatch(
                        "impl",
                        "implementer",
                        "cli:claude",
                        "claude-opus-5",
                        Some(1.5),
                        200_000,
                        "peak",
                    ),
                    dispatch(
                        "review-0-cli-codex",
                        "reviewer",
                        "cli:codex",
                        "gpt-5.6",
                        None,
                        9_000_000,
                        "invocation_total",
                    ),
                    dispatch(
                        "impl",
                        "implementer",
                        "cli:claude",
                        "claude-opus-5",
                        Some(0.5),
                        100_000,
                        "peak",
                    ),
                ],
                vec![
                    (1, "review-0-cli-codex", "request_changes", false, 3),
                    (1, "review-1-cli-claude", "request_changes", true, 0),
                    (2, "review-0-cli-codex", "approve", false, 0),
                ],
            ),
            history_row(
                "b",
                "awaiting_round_extension",
                "2026-09-01T00:00:00Z",
                vec![],
                vec![],
            ),
        ];
        std::fs::write(paths.history_file(), rows.join("\n") + "\n").unwrap();
        std::fs::write(
            paths.quota_events_file(),
            [
                json!({"schema_version": 1, "ts": "2026-10-19T00:00:00Z", "provider": "codex", "manual": false}),
                json!({"schema_version": 1, "ts": "2026-10-19T00:00:00Z", "provider": "cli:codex", "manual": true}),
            ]
            .map(|v| v.to_string())
            .join("\n"),
        )
        .unwrap();
        tmp
    }

    fn group<'a>(r: &'a Report, key: &str) -> &'a Group {
        r.groups.iter().find(|g| g.key == key).unwrap()
    }

    #[test]
    fn groups_by_provider_without_mixing_context_semantics() {
        let tmp = fixture();
        let now = DateTime::parse_from_rfc3339("2026-10-30T00:00:00Z")
            .unwrap()
            .to_utc();
        let r = report(
            &[tmp.path().to_path_buf()],
            None,
            GroupBy::Provider,
            false,
            now,
        )
        .unwrap();
        assert_eq!((r.runs, r.runs_from_history, r.runs_live), (2, 2, 0));
        let claude = group(&r, "cli:claude");
        assert_eq!(claude.runs, 2);
        assert_eq!(claude.dispatches, 2);
        assert_eq!(claude.usage.cost_usd, Some(2.0));
        assert_eq!(claude.usage.dispatches_without_cost, 0);
        assert_eq!(claude.context.len(), 1);
        assert_eq!(claude.context[0].semantics, "peak");
        assert_eq!(claude.context[0].max, 200_000);
        assert_eq!(claude.outcomes.ship_gate, 1);
        assert_eq!(claude.outcomes.ceiling, 1);
        assert_eq!(claude.rounds.mean_to_first_approve, Some(2.0));
        assert_eq!(claude.rounds.runs_with_approve, 1);
        assert_eq!(claude.wall_time.slot_secs_total, 1600);
        let codex = group(&r, "cli:codex");
        assert_eq!(codex.usage.cost_usd, None);
        assert_eq!(codex.usage.dispatches_without_cost, 1);
        assert_eq!(codex.context[0].semantics, "invocation_total");
        assert_eq!(codex.reviews.reviews, 2);
        assert_eq!(codex.reviews.blocking_findings, 3);
        assert_eq!(codex.reviews.blocking_per_review, Some(1.5));
        assert_eq!(codex.wall_time.rounds_timed, 2);
        let unknown = group(&r, "unknown");
        assert_eq!(unknown.reviews.synthetic_reviews, 1);
        assert_eq!(unknown.reviews.reviews, 0);
        let q = r.quota.iter().find(|q| q.provider == "cli:codex").unwrap();
        assert_eq!(
            (q.pauses, q.manual_pauses, q.parks, q.backup_engagements),
            (1, 1, 2, 2)
        );
    }

    #[test]
    fn since_and_role_grouping() {
        let tmp = fixture();
        let now = DateTime::parse_from_rfc3339("2026-10-30T00:00:00Z")
            .unwrap()
            .to_utc();
        let since = Some(now - chrono::Duration::days(14));
        let r = report(
            &[tmp.path().to_path_buf()],
            since,
            GroupBy::Role,
            false,
            now,
        )
        .unwrap();
        assert_eq!(r.runs, 1);
        let reviewer = group(&r, "reviewer");
        assert_eq!(reviewer.dispatches, 1);
        assert_eq!(reviewer.reviews.reviews, 2);
        let model = report(
            &[tmp.path().to_path_buf()],
            since,
            GroupBy::Model,
            false,
            now,
        )
        .unwrap();
        assert!(model
            .groups
            .iter()
            .any(|g| g.key == "claude-opus-5" && g.dispatches == 2));
        let project = report(
            &[tmp.path().to_path_buf()],
            since,
            GroupBy::Project,
            false,
            now,
        )
        .unwrap();
        assert_eq!(project.groups.len(), 1);
        assert_eq!(project.groups[0].dispatches, 3);
    }
}
