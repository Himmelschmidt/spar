//! Soft budgets and turn-boundary nudges (O50).
//!
//! **Nothing here kills a slot on tokens.** Of the 21 dispatches in the local corpus that
//! billed over 100M tokens, 18 exited `0`: the expensive tail is expensive work, and a
//! token cap would throw away a finished implementation and force a re-dispatch, which is
//! the costliest thing spar does. So a slot past its budget is *told*, repeatedly, to land
//! its artifact and say what it did not reach. The only wall that still kills is
//! `timeouts.hard_ceiling_multiple`, deliberately far above the soft budget.
//!
//! A nudge cannot be typed at a busy CLI agent: text sent to its TTY queues unsubmitted in
//! its input box. Every nudge therefore goes through [`crate::providers::delivery::nudge`],
//! which lands it wherever the adapter reads at a turn boundary. This module resolves the
//! strategy from the slot's adapter and never learns which one it got.

use crate::config::Config;
use crate::events::{self, Event};
use crate::paths::SparPaths;
use crate::process::StreamStats;
use crate::provider_ref::ProviderRef;
use crate::providers;
use crate::state::SlotRole;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the watcher looks at the clock and the sidecar. The wait loop polls every
/// 50ms; re-reading a growing muse session log that often would cost more than the slot.
/// The cost is granularity: a threshold is noticed at the next 30s boundary, not the
/// instant it is crossed, which is nothing against budgets measured in minutes and hours.
const POLL_SECS: u64 = 30;

/// Prefix on the `error` of a slot the hard ceiling killed. Every writer and every reader
/// of that distinction goes through [`ceiling_error`] / [`is_ceiling_kill`], so it is one
/// string in one place rather than a convention.
const CEILING_PREFIX: &str = "hard ceiling: ";

/// What a slot killed at the ceiling records, as distinct from a crash (`exit 143`,
/// `killed by signal 9`) or a soft-budget overrun (which never ends a dispatch).
pub fn ceiling_error(ceiling: Duration, soft: Duration, label: &str) -> String {
    format!(
        "{CEILING_PREFIX}killed after {}s ({label} {}s x timeouts.hard_ceiling_multiple)",
        ceiling.as_secs(),
        soft.as_secs(),
    )
}

pub fn is_ceiling_kill(error: &str) -> bool {
    error.starts_with(CEILING_PREFIX)
}

/// The artifact shape both nudges ask for. Named once so a token nudge, a time nudge and
/// the role templates cannot drift into asking for three different things.
pub const SUMMARY_SHAPE: &str = "State three things explicitly and separately: what you \
completed, what you did not reach, and what you are stuck on (\"nothing\" is a valid \
answer to the last one).";

/// Watches one live dispatch and nudges it past its soft budgets.
///
/// Driven from `run_captured`'s per-poll tick, the same seam the liveness heartbeat uses,
/// so it observes the actual process rather than provider hooks a whole adapter class
/// never installs. Interior mutability because the tick is an `Fn`, and one watcher only
/// ever belongs to one slot's supervising thread.
pub struct NudgeWatch<'a> {
    paths: &'a SparPaths,
    run_id: &'a str,
    slot_id: &'a str,
    log_path: &'a Path,
    /// Every file the slot is expected to leave behind, in the order the nudge names
    /// them. A list rather than one name because a role can owe more than one artifact
    /// (a summary plus a carry-forward brief), and a nudge that names only the first
    /// tells the slot to stop after writing it.
    artifacts: Vec<&'a str>,
    strategy: providers::DeliveryStrategy,
    dry_run: bool,
    /// Set only for `cli:muse`, whose stdout carries no token counts at all.
    muse: bool,
    started: Cell<Instant>,
    soft: Duration,
    ceiling: Duration,
    label: &'static str,
    time_step: Duration,
    budget: u64,
    token_step: u64,
    last_poll: Cell<Instant>,
    next_time: Cell<Option<Duration>>,
    next_tokens: Cell<u64>,
    live_muse: RefCell<Option<providers::muse_telemetry::LiveUsage>>,
}

/// Everything the watcher needs that the executor already has in hand.
pub struct WatchSpec<'a> {
    pub paths: &'a SparPaths,
    pub run_id: &'a str,
    pub slot_id: &'a str,
    pub provider: &'a str,
    pub role: SlotRole,
    pub log_path: &'a Path,
    pub artifacts: Vec<&'a str>,
    pub soft: Duration,
    pub ceiling: Duration,
    pub label: &'static str,
    pub dry_run: bool,
}

impl<'a> NudgeWatch<'a> {
    pub fn new(spec: WatchSpec<'a>, cfg: &Config) -> Self {
        let pref = ProviderRef::parse(spec.provider).ok();
        let cli = pref.as_ref().and_then(|p| p.cli_name());
        let strategy = cli
            .and_then(providers::adapter_named)
            .map(|a| a.delivery_strategy())
            .unwrap_or(providers::DeliveryStrategy::None);
        let time_step = Duration::from_secs(cfg.timeouts.nudge_every_secs);
        Self {
            paths: spec.paths,
            run_id: spec.run_id,
            slot_id: spec.slot_id,
            log_path: spec.log_path,
            artifacts: spec.artifacts,
            strategy,
            dry_run: spec.dry_run,
            muse: cli == Some("muse"),
            started: Cell::new(Instant::now()),
            soft: spec.soft,
            ceiling: spec.ceiling,
            label: spec.label,
            time_step,
            budget: cfg.budget.tokens_for(spec.role),
            token_step: cfg.budget.nudge_step(spec.role),
            last_poll: Cell::new(Instant::now()),
            next_time: Cell::new(Some(spec.soft)),
            next_tokens: Cell::new(cfg.budget.tokens_for(spec.role)),
            live_muse: RefCell::new(None),
        }
    }

    pub fn tick(&self) {
        if self.last_poll.get().elapsed() < Duration::from_secs(POLL_SECS) {
            return;
        }
        self.last_poll.set(Instant::now());
        self.check_time();
        self.check_tokens();
    }

    fn check_time(&self) {
        let Some(due) = self.next_time.get() else {
            return;
        };
        let elapsed = self.started.get().elapsed();
        if elapsed < due {
            return;
        }
        self.next_time.set(if self.time_step.is_zero() {
            None
        } else {
            Some(due + self.time_step)
        });
        self.send(&format!(
            "spar time nudge for slot {slot}: this dispatch has been running {ran}, past its \
             soft budget of {soft} ({label}). Passing it is allowed and is not a failure. \
             Answer one question in your next turn: are you still making progress? If you \
             are, say so in one line and carry on. If you are not, stop and write {artifact} \
             now. {shape} The hard ceiling is {ceiling}; the dispatch is killed there and \
             anything you have not written down is lost.",
            slot = self.slot_id,
            ran = human(elapsed),
            soft = human(self.soft),
            label = self.label,
            artifact = self.artifact_phrase(),
            shape = SUMMARY_SHAPE,
            ceiling = human(self.ceiling),
        ));
    }

    fn check_tokens(&self) {
        if self.budget == 0 {
            return;
        }
        let billed = self.live_billed();
        let due = self.next_tokens.get();
        if billed < due {
            return;
        }
        // Advance past what has already been spent, not by one step: a slot that jumps
        // several steps between polls gets one nudge, not a burst of identical ones.
        let step = self.token_step;
        let next = if step == u64::MAX {
            u64::MAX
        } else {
            billed.saturating_add(step)
        };
        self.next_tokens.set(next);
        self.send(&format!(
            "spar budget nudge for slot {slot}: this dispatch has billed {billed} tokens, past \
             its soft budget of {budget} for this role. Nothing is being killed and you are not \
             in trouble. Land what you have: write {artifact} before you start anything else. \
             {shape} Then finish. Do not begin work you cannot land.",
            slot = self.slot_id,
            billed = compact(billed),
            budget = compact(self.budget),
            artifact = self.artifact_phrase(),
            shape = SUMMARY_SHAPE,
        ));
    }

    fn artifact_phrase(&self) -> String {
        match self.artifacts.as_slice() {
            [] => "your artifact".into(),
            [one] => format!("your artifact ({one})"),
            many => format!("all of your artifacts ({})", many.join(", ")),
        }
    }

    /// Mid-flight spend, from the live sidecar. `state.usage[]` only gains an entry when a
    /// dispatch completes, and `slot.usage` is the last completed one, so neither can see
    /// the dispatch that is running now.
    fn live_billed(&self) -> u64 {
        let stats = StreamStats::load(self.log_path).unwrap_or_default();
        if !self.muse {
            return stats.billed_tokens;
        }
        // muse's stdout carries no token counts, so its sidecar reads zero until
        // `enrich` runs post-exit. Its session log is appended live and is the only
        // mid-dispatch source there is.
        let Some(session_id) = stats.session_id else {
            return 0;
        };
        let mut live = self.live_muse.borrow_mut();
        if live.is_none() {
            *live = providers::muse_telemetry::LiveUsage::open(&session_id);
        }
        live.as_mut().map(|l| l.billed()).unwrap_or(0)
    }

    fn send(&self, text: &str) {
        let agent = crate::bus::agent_ref(Some(self.run_id), self.slot_id);
        let outcome = providers::delivery::nudge(
            self.paths,
            Some(self.run_id),
            &agent,
            self.strategy,
            text,
            self.dry_run,
        );
        let note = match &outcome {
            Ok(d) => match &d.path {
                Some(p) => format!("nudge delivered via {:?} -> {p}", d.action),
                None => format!("nudge delivered via {:?}", d.action),
            },
            Err(e) => format!("nudge delivery failed: {e:#}"),
        };
        let _ = events::append(
            self.paths,
            self.run_id,
            &Event::slot_note(self.slot_id, format!("{text} [{note}]")),
        );
    }
}

/// The poll file a slot is told to read, as an absolute path for its prompt.
pub fn poll_file(paths: &SparPaths, run_id: &str, slot_id: &str) -> PathBuf {
    providers::delivery::poll_file(paths, Some(run_id), slot_id)
}

fn human(d: Duration) -> String {
    crate::liveness::format_duration_short(d.as_secs())
}

fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::DeliveryStrategy;
    use tempfile::tempdir;

    fn spec<'a>(
        paths: &'a SparPaths,
        log: &'a Path,
        role: SlotRole,
        provider: &'a str,
    ) -> WatchSpec<'a> {
        WatchSpec {
            paths,
            run_id: "r1",
            slot_id: "impl",
            provider,
            role,
            log_path: log,
            artifacts: vec!["summary-impl.md"],
            soft: Duration::from_secs(60),
            ceiling: Duration::from_secs(180),
            label: "timeouts.slot_secs",
            dry_run: false,
        }
    }

    fn poll_body(paths: &SparPaths) -> String {
        std::fs::read_to_string(poll_file(paths, "r1", "r1:impl")).unwrap_or_default()
    }

    /// The whole point: a slot over budget is told, not killed, and it is told again as it
    /// keeps going. Driven through the real tick, the real delivery seam and the real
    /// sidecar — the failure mode this guards is a nudge nobody ever receives.
    #[test]
    fn token_overrun_nudges_repeatedly_and_lands_in_the_poll_file() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = tmp.path().join("logs").join("impl.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.budget.implementer = 1000;
        cfg.budget.nudge_fraction = 0.2;
        // opencode reports usage live per step and has no push channel, so it exercises
        // both halves: a real mid-dispatch token reading and the poll-file drop.
        let w = NudgeWatch::new(
            spec(&paths, &log, SlotRole::Implementer, "cli:opencode"),
            &cfg,
        );
        assert_eq!(w.strategy, DeliveryStrategy::PollFile);

        let bill = |n: u64| {
            StreamStats {
                billed_tokens: n,
                ..Default::default()
            }
            .save(&log)
            .unwrap()
        };

        bill(999);
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths), "", "under budget: no nudge");

        bill(1000);
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        let first = poll_body(&paths);
        assert!(first.contains("past its soft budget"), "{first}");
        assert!(first.contains("summary-impl.md"), "{first}");
        assert!(first.contains("what you did not reach"), "{first}");

        // Still under the next step: told once, not on every poll.
        bill(1100);
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths).matches("budget nudge").count(), 1);

        // Past it: told again.
        bill(1300);
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths).matches("budget nudge").count(), 2);

        // And the run's event stream carries every one of them, slot-scoped.
        let evs = events::read_all(&paths, "r1").unwrap();
        let nudges: Vec<_> = evs
            .iter()
            .filter(|e| e.slot.as_deref() == Some("impl"))
            .collect();
        assert_eq!(nudges.len(), 2);
        let note = nudges[0].message.as_deref().unwrap();
        assert!(note.contains("PolledFile"), "{note}");
        assert!(note.contains("nudges-impl.md"), "{note}");
    }

    /// muse's sidecar reads zero tokens for a slot's entire life (`enrich` runs only
    /// after the child is waited on), so a muse token nudge must come from its live
    /// session log or not at all. Without a session id there is nothing to read yet, and
    /// the watcher must stay quiet rather than nudge on a phantom zero.
    #[test]
    fn muse_reads_its_session_log_not_the_sidecar() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = tmp.path().join("logs").join("impl.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.budget.implementer = 1000;
        let w = NudgeWatch::new(spec(&paths, &log, SlotRole::Implementer, "cli:muse"), &cfg);
        assert!(w.muse);

        // What muse's own stream produces: no tokens, a session id from the first
        // `run.model.configured` event.
        StreamStats {
            billed_tokens: 0,
            session_id: None,
            ..Default::default()
        }
        .save(&log)
        .unwrap();
        assert_eq!(w.live_billed(), 0);
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths), "", "no session yet: nothing to bill");
    }

    #[test]
    fn time_overrun_nudges_on_the_configured_cadence() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = tmp.path().join("logs").join("impl.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.timeouts.nudge_every_secs = 600;
        let mut s = spec(&paths, &log, SlotRole::Implementer, "cli:codex");
        s.soft = Duration::from_secs(60);
        let w = NudgeWatch::new(s, &cfg);

        // Not yet past the soft budget.
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths), "");

        // Past it once, then past the next 600s mark.
        w.started.set(Instant::now() - Duration::from_secs(61));
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths).matches("time nudge").count(), 1);
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(
            poll_body(&paths).matches("time nudge").count(),
            1,
            "one cadence step, not one per poll"
        );
        w.started.set(Instant::now() - Duration::from_secs(661));
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        let body = poll_body(&paths);
        assert_eq!(body.matches("time nudge").count(), 2);
        assert!(body.contains("hard ceiling is 3m"), "{body}");
    }

    /// claude has a real push channel, so its nudge must go through it and *not* to a file
    /// the Stop hook never reads.
    #[test]
    fn claude_nudges_reach_the_inbox_its_stop_hook_drains() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = tmp.path().join("logs").join("impl.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.budget.implementer = 10;
        let w = NudgeWatch::new(
            spec(&paths, &log, SlotRole::Implementer, "cli:claude"),
            &cfg,
        );
        assert_eq!(w.strategy, DeliveryStrategy::StopHookInject);
        StreamStats {
            billed_tokens: 50,
            ..Default::default()
        }
        .save(&log)
        .unwrap();
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        assert_eq!(poll_body(&paths), "", "claude has a channel; no file drop");

        // The hook's own drain is what the slot will run at its next turn boundary.
        let agent = crate::bus::agent_ref(Some("r1"), "impl");
        let d = providers::delivery::deliver(
            &paths,
            Some("r1"),
            &agent,
            DeliveryStrategy::StopHookInject,
            false,
        )
        .unwrap();
        assert_eq!(d.delivered, 1);
        let payload = d.payload.expect("stop-hook block payload");
        assert!(payload.contains("budget nudge"), "{payload}");
        assert!(payload.contains("what you are stuck on"), "{payload}");
    }

    /// The seam the whole thing hangs off: `run_captured`'s per-poll tick, with a real
    /// child alive on the other end. If this does not fire, every nudge is written to a
    /// file no dispatch ever reaches.
    #[test]
    fn the_watcher_fires_from_run_captured_while_the_child_is_alive() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = tmp.path().join("logs").join("impl.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.budget.implementer = 10;
        let mut s = spec(&paths, &log, SlotRole::Implementer, "cli:opencode");
        s.soft = Duration::from_secs(1);
        let w = NudgeWatch::new(s, &cfg);

        // A child that reports usage the way a real one does, then stays alive long
        // enough for the wait loop to poll.
        let script = format!(
            "{}\nsleep 2",
            r#"echo '{"type":"step_finish","sessionID":"ses_1","part":{"id":"p1","type":"step-finish","tokens":{"input":900,"output":100,"cache":{"read":0,"write":0}}}}'"#
        );
        let req = crate::process::SpawnRequest {
            program: "sh".into(),
            args: vec!["-c".into(), script],
            cwd: tmp.path().to_path_buf(),
            log_path: log.clone(),
            env: vec![],
            timeout: Duration::from_secs(30),
        };
        // The 30s throttle is covered above; here every wait-loop iteration is a poll, so
        // what is under test is that the tick reaches the watcher at all and that the
        // watcher can read a sidecar the streaming thread is still writing.
        let tick = || {
            w.last_poll
                .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
            w.tick();
        };
        let res = crate::process::run_captured(&req, None, Some(&tick)).unwrap();
        assert_eq!(res.exit_code, Some(0), "the child must not be killed");

        let body = poll_body(&paths);
        assert!(body.contains("budget nudge"), "{body}");
        assert!(
            body.contains("1.0k tokens"),
            "read live, not post-exit: {body}"
        );
        assert!(body.contains("time nudge"), "{body}");
    }

    /// A role can owe more than one file, and a nudge that named only the first told the
    /// slot to write it and stop, so the second was never produced in exactly the case it
    /// exists for.
    #[test]
    fn a_nudge_names_every_artifact_the_slot_owes() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let log = tmp.path().join("logs").join("impl.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mut cfg = Config::default();
        cfg.budget.implementer = 10;
        let mut s = spec(&paths, &log, SlotRole::Implementer, "cli:opencode");
        s.artifacts = vec!["summary-impl.md", "carry-forward-impl.md"];
        let w = NudgeWatch::new(s, &cfg);
        StreamStats {
            billed_tokens: 50,
            ..Default::default()
        }
        .save(&log)
        .unwrap();
        w.last_poll
            .set(Instant::now() - Duration::from_secs(POLL_SECS + 1));
        w.tick();
        let body = poll_body(&paths);
        assert!(body.contains("summary-impl.md"), "{body}");
        assert!(body.contains("carry-forward-impl.md"), "{body}");
    }

    #[test]
    fn ceiling_kill_is_distinguishable_from_a_crash() {
        let e = ceiling_error(
            Duration::from_secs(16200),
            Duration::from_secs(5400),
            "timeouts.slot_secs",
        );
        assert!(is_ceiling_kill(&e), "{e}");
        assert!(!is_ceiling_kill("killed by signal 9 (SIGKILL)"));
        assert!(!is_ceiling_kill("exit 143"));
        assert!(!is_ceiling_kill("missing expected artifact plan.md"));
    }
}
