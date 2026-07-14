//! Workspace spawn + dispatch (Stage 11 / A4).
//!
//! Lets the Composer launch a fresh agent into a pane on spar's private tmux
//! socket, join it to the bus with a stable `SPAR_AGENT_ID`, and hand it a
//! prompt — the whole loop without leaving spar.
//!
//! Two gotchas from the herdr/thurbox findings are baked in here:
//!  - Delivering a prompt to a CLI agent's TUI is **two** steps: the text is
//!    typed, then Enter is a *separate* key. Fusing them submits a half-typed
//!    line, so [`deliver_prompt`] always sends Enter on its own.
//!  - Waiting on idle races the agent's start: an agent read as `idle` *before*
//!    it has begun its turn is a false positive. [`wait_working_then_idle`]
//!    waits for `working` first and only then for `idle`, so a leading idle can
//!    never be mistaken for a finished turn.

use crate::bus;
use crate::paths::SparPaths;
use crate::provider_ref::ProviderRef;
use crate::providers::{self, SpawnOpts, TrustPolicy};
use crate::tmux;
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Presence status strings the bus records, mirroring the Claude-hook mapping in
/// `providers::presence` (`working` on prompt/tool use, `idle` on Stop).
pub const STATUS_WORKING: &str = "working";
pub const STATUS_IDLE: &str = "idle";

/// A request to launch a fresh agent into a pane on spar's private tmux socket.
pub struct SpawnRequest<'a> {
    pub paths: &'a SparPaths,
    /// Run this agent is tagged with on the bus, or `None` for a bare Composer agent.
    /// Either way the agent is addressable on the one workspace bus by its `agent_id`
    /// (W5) — a run tag only scopes run-filtered views.
    pub run: Option<&'a str>,
    /// Stable agent id, exported as `SPAR_AGENT_ID`.
    pub agent_id: &'a str,
    /// `cli:name` provider ref. API providers have no pane, so they are rejected.
    pub provider: &'a str,
    /// Directory the agent runs in — a worktree for a coding slot, or any chosen
    /// cwd for a bare poke-at-something agent.
    pub cwd: &'a Path,
    /// Checkout that owns `.spar/runs/<id>`; heartbeat hooks resolve against it.
    pub project_root: &'a Path,
}

/// Launch `req`'s agent into a spar-socket pane: wire its presence hooks + identity
/// env via the Stage 3 adapter seam, join it to the bus, and start the interactive
/// CLI (no prompt baked in — the prompt is typed into the live pane afterwards).
///
/// Returns `(session, window)` so the caller can [`deliver_prompt`] to the pane.
pub fn spawn_agent(req: &SpawnRequest) -> Result<(String, String)> {
    if !tmux::available() {
        bail!("tmux not available — the spar-socket workspace needs tmux");
    }
    let pref = ProviderRef::parse(req.provider)?;
    let cli_name = pref.cli_name().ok_or_else(|| {
        anyhow!(
            "{} is an api provider; only cli: agents run in a pane",
            req.provider
        )
    })?;
    let adapter =
        providers::adapter_named(cli_name).ok_or_else(|| anyhow!("unknown provider {cli_name}"))?;
    let bin = adapter
        .resolve_binary()
        .ok_or_else(|| anyhow!("provider {} not on PATH", req.provider))?;

    // Stage 3 seam: install the adapter's presence hooks (best-effort) and get the
    // identity env every agent carries. Degraded-mode notes are surfaced to the
    // caller-visible bus rather than failing the spawn.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("spar"));
    let identity = providers::presence::SlotIdentity {
        agent_id: req.agent_id,
        run_id: req.run,
        project_root: req.project_root,
        worktree: req.cwd,
        spar_exe: &exe,
    };
    let wiring = providers::presence::wire(adapter.as_ref(), &identity);

    // Register on the workspace bus so the agent is addressable by id like any run slot,
    // bare or not (W5).
    let storage = pref.storage_key();
    bus::join(
        req.paths,
        req.run,
        req.agent_id,
        Some(storage.as_str()),
        Some(pref.backend.as_str()),
    )?;
    if let Some(note) = wiring.note {
        let _ = bus::broadcast(
            req.paths,
            req.run,
            req.agent_id,
            note,
            bus::MessageBudget::Chatty,
        );
    }

    let opts = SpawnOpts {
        prompt: String::new(),
        prompt_file: None,
        cwd: req.cwd.to_path_buf(),
        trust: TrustPolicy::FullAuto,
        extra_args: vec![],
        model: None,
    };
    let cmd = adapter.build_interactive(&bin, &opts);
    let (program, args) = providers::command_to_parts(&cmd);
    let shell = tmux::shell_command(&program, &args);

    // Bare agents have no run, so key the tmux session on the agent id instead.
    let session = tmux::session_name(req.run.unwrap_or(req.agent_id));
    tmux::new_session(&session, req.cwd)?;
    tmux::spawn_window(&session, req.agent_id, req.cwd, &shell, &wiring.env)?;

    Ok((session, req.agent_id.to_string()))
}

/// A freshly launched CLI agent needs a beat to paint its input box before a
/// prompt can be typed. Text sent into an unbooted TUI lands in an uninitialised
/// buffer and is silently dropped (herdr/thurbox finding: readiness must be gated,
/// not assumed). Polls the rendered pane until the CLI has drawn something or
/// `timeout` elapses, returning whether the pane looked ready before giving up.
pub fn wait_pane_ready(
    session: &str,
    window: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<bool> {
    let start = Instant::now();
    loop {
        // A pane that isn't attached yet errors; treat that as "not ready" and retry.
        let capture = tmux::capture_pane(session, window).unwrap_or_default();
        if pane_looks_ready(&capture) {
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

/// Pure readiness predicate: the CLI has painted its UI once the pane holds any
/// non-whitespace content. Factored out so the gate can be unit-tested without tmux.
pub fn pane_looks_ready(capture: &str) -> bool {
    capture.chars().any(|c| !c.is_whitespace())
}

/// Deliver a prompt to a live agent pane as **two** steps: type the text, then a
/// separate Enter. Fusing them would submit a half-typed line, so the submit key
/// is always its own send (the write-then-Enter gotcha).
pub fn deliver_prompt(session: &str, window: &str, prompt: &str) -> Result<()> {
    let target = format!("{session}:{window}");
    tmux::send_key(&target, &tmux::SendKey::Literal(prompt.to_string()))?;
    tmux::send_key(&target, &tmux::SendKey::Named("Enter".to_string()))?;
    Ok(())
}

/// Where a [`WaitWorkingThenIdle`] observer sits in the turn lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPhase {
    /// Not yet seen the agent enter `working`. A leading `idle` here is the
    /// pre-start false idle and is deliberately ignored.
    AwaitingWorking,
    /// Observed `working`; now waiting for the return to `idle`.
    AwaitingIdle,
    /// Observed `working` then `idle` — the turn is complete.
    Done,
}

/// Pure state machine behind [`wait_working_then_idle`]. Factored out so the
/// false-idle race guard can be unit-tested against a scripted presence sequence
/// with no timing at all.
///
/// Reusable across call sites that steer a spawned agent (the CLI/outer-agent
/// path and later workspace stages); the TUI composer can't block on it inline
/// without stalling its render loop, so it isn't driven from the composer yet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct WaitWorkingThenIdle {
    phase: WaitPhase,
}

impl Default for WaitWorkingThenIdle {
    fn default() -> Self {
        Self {
            phase: WaitPhase::AwaitingWorking,
        }
    }
}

#[allow(dead_code)]
impl WaitWorkingThenIdle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> WaitPhase {
        self.phase
    }

    pub fn is_done(&self) -> bool {
        self.phase == WaitPhase::Done
    }

    /// Feed one presence status observation. Only a `working` advances past the
    /// leading phase, so an `idle` seen before any `working` can never complete
    /// the wait. Returns the phase after applying the observation.
    pub fn observe(&mut self, status: &str) -> WaitPhase {
        self.phase = match self.phase {
            WaitPhase::AwaitingWorking if status == STATUS_WORKING => WaitPhase::AwaitingIdle,
            WaitPhase::AwaitingIdle if status == STATUS_IDLE => WaitPhase::Done,
            other => other,
        };
        self.phase
    }
}

/// Block until `agent` has gone `working` and *then* `idle` on the bus, or until
/// `timeout` elapses. Returns `Ok(true)` on a completed turn, `Ok(false)` on
/// timeout. Guards the false-idle race: an `idle` observed before the agent has
/// started its turn is ignored, so this never returns on the leading idle.
#[allow(dead_code)]
pub fn wait_working_then_idle(
    paths: &SparPaths,
    run: Option<&str>,
    agent: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<bool> {
    let mut w = WaitWorkingThenIdle::new();
    // Presence is keyed by the unique id (`run:slot`), so qualify the short `agent` before
    // matching — a raw short id would never match a run slot's presence row.
    let want = bus::resolve_addr(run, agent);
    let start = Instant::now();
    loop {
        let status = bus::list_presence(paths, run)?
            .into_iter()
            .find(|p| p.agent == want)
            .map(|p| p.status);
        if let Some(status) = status {
            if w.observe(&status) == WaitPhase::Done {
                return Ok(true);
            }
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A blank pane (CLI still booting) is not ready; any painted glyph makes it ready.
    #[test]
    fn pane_readiness_tracks_painted_content() {
        assert!(!pane_looks_ready(""));
        assert!(!pane_looks_ready("   \n\t \n  "));
        assert!(pane_looks_ready("> "));
        assert!(pane_looks_ready("\n  Claude Code\n"));
    }

    /// With a pane that never paints, the gate must time out rather than deliver
    /// into an unbooted TUI.
    #[test]
    fn wait_pane_ready_times_out_on_missing_session() {
        let ready = wait_pane_ready(
            "spar-no-such-session",
            "nope",
            Duration::from_millis(60),
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(!ready, "a pane that never paints must not report ready");
    }

    /// The leading idle (pre-start false positive) must not complete the wait.
    #[test]
    fn leading_idle_does_not_complete() {
        let mut w = WaitWorkingThenIdle::new();
        // Any number of idles before the first `working` are ignored.
        assert_eq!(w.observe("idle"), WaitPhase::AwaitingWorking);
        assert_eq!(w.observe("joined"), WaitPhase::AwaitingWorking);
        assert_eq!(w.observe("idle"), WaitPhase::AwaitingWorking);
        assert!(!w.is_done());
    }

    /// idle -> working -> idle completes only on the trailing idle, never the lead.
    #[test]
    fn working_then_idle_completes_on_trailing_idle() {
        let mut w = WaitWorkingThenIdle::new();
        assert_eq!(w.observe("idle"), WaitPhase::AwaitingWorking); // leading idle ignored
        assert_eq!(w.observe("working"), WaitPhase::AwaitingIdle); // turn started
        assert_eq!(w.observe("blocked"), WaitPhase::AwaitingIdle); // mid-turn noise
        assert_eq!(w.observe("idle"), WaitPhase::Done); // trailing idle -> done
        assert!(w.is_done());
    }

    /// Once done, further observations are inert (idempotent terminal state).
    #[test]
    fn done_is_sticky() {
        let mut w = WaitWorkingThenIdle::new();
        w.observe("working");
        w.observe("idle");
        assert!(w.is_done());
        assert_eq!(w.observe("working"), WaitPhase::Done);
    }

    /// Live driver: with only a stale `idle` on the bus and no `working`, the wait
    /// must time out (`Ok(false)`) rather than falsely report a finished turn.
    #[test]
    fn live_driver_times_out_on_leading_idle_only() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        bus::ensure_bus(&paths).unwrap();
        bus::heartbeat(&paths, Some("r1"), "poke-1", "idle").unwrap();

        let done = wait_working_then_idle(
            &paths,
            Some("r1"),
            "poke-1",
            Duration::from_millis(120),
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(
            !done,
            "leading idle must not early-return as a completed turn"
        );
    }

    /// Live driver: a working->idle transition delivered over time completes.
    #[test]
    fn live_driver_completes_on_working_then_idle() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        bus::ensure_bus(&paths).unwrap();
        // Bare (run-less) agent: presence tracks it by id on the workspace bus.
        bus::heartbeat(&paths, None, "poke-2", "idle").unwrap(); // stale leading idle

        let feeder = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(40));
                bus::heartbeat(&paths, None, "poke-2", "working").unwrap();
                std::thread::sleep(Duration::from_millis(60));
                bus::heartbeat(&paths, None, "poke-2", "idle").unwrap();
            })
        };

        let done = wait_working_then_idle(
            &paths,
            None,
            "poke-2",
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .unwrap();
        feeder.join().unwrap();
        assert!(done, "should complete after observing working then idle");
    }

    #[test]
    fn live_driver_completes_for_run_slot_working_then_idle() {
        // Regression guard: presence is keyed by the qualified id (`run:slot`), so the
        // wait lookup must qualify too — a run slot's working->idle turn must be observed,
        // not silently missed (which would make this path always time out).
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        bus::ensure_bus(&paths).unwrap();
        bus::heartbeat(&paths, Some("r1"), "poke-3", "idle").unwrap();
        let feeder = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(40));
                bus::heartbeat(&paths, Some("r1"), "poke-3", "working").unwrap();
                std::thread::sleep(Duration::from_millis(60));
                bus::heartbeat(&paths, Some("r1"), "poke-3", "idle").unwrap();
            })
        };
        let done = wait_working_then_idle(
            &paths,
            Some("r1"),
            "poke-3",
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .unwrap();
        feeder.join().unwrap();
        assert!(
            done,
            "run slot working->idle must be observed via the qualified lookup"
        );
    }
}
