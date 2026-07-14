//! Product shell — clear fleet dashboard for multi-agent runs.
use crate::cli::WorkflowKind;
use crate::config::Config;
use crate::events;
use crate::liveness::SlotActivity;
use crate::paths::{self, SparPaths};
use crate::process;
use crate::quota::QuotaStore;
use crate::registry;
use crate::state::{self, Phase, RunState, SlotState, SlotStatus};
use crate::tmux;
use crate::workflow;
use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Widget, Wrap,
};
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tui_term::widget::PseudoTerminal;

// ── palette ─────────────────────────────────────────────────────────────────

const BG: Color = Color::Rgb(12, 14, 18);
const BG_PANEL: Color = Color::Rgb(18, 21, 28);
const BG_RAISED: Color = Color::Rgb(24, 28, 36);
const BORDER: Color = Color::Rgb(42, 48, 60);
const BORDER_FOCUS: Color = Color::Rgb(88, 166, 255);
const FG: Color = Color::Rgb(220, 224, 232);
const FG_DIM: Color = Color::Rgb(110, 118, 132);
const FG_MUTED: Color = Color::Rgb(72, 80, 96);
const ACCENT: Color = Color::Rgb(88, 166, 255);
const ACCENT_SOFT: Color = Color::Rgb(56, 110, 180);
const GREEN: Color = Color::Rgb(63, 185, 80);
const YELLOW: Color = Color::Rgb(210, 168, 70);
const RED: Color = Color::Rgb(248, 81, 73);
const MAGENTA: Color = Color::Rgb(188, 120, 240);
const CYAN: Color = Color::Rgb(57, 190, 200);

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Runs,
    Agents,
    Log,
    Activity,
    Terminal,
    Composer,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Runs => Focus::Agents,
            Focus::Agents => Focus::Log,
            Focus::Log => Focus::Activity,
            Focus::Activity => Focus::Terminal,
            Focus::Terminal => Focus::Composer,
            Focus::Composer => Focus::Runs,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::Runs => Focus::Composer,
            Focus::Agents => Focus::Runs,
            Focus::Log => Focus::Agents,
            Focus::Activity => Focus::Log,
            Focus::Terminal => Focus::Activity,
            Focus::Composer => Focus::Terminal,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Focus::Runs => "Runs",
            Focus::Agents => "Agents",
            Focus::Log => "Live log",
            Focus::Activity => "Activity",
            Focus::Terminal => "Terminal",
            Focus::Composer => "Command",
        }
    }
}

pub struct TuiOpts {
    pub task_seed: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Opt into crossterm's full mouse capture; default is the mobile-safe subset.
    pub full_mouse: bool,
}

pub fn run_with(opts: TuiOpts) -> Result<crate::exit_codes::ExitCode> {
    if let Some(cwd) = &opts.cwd {
        std::env::set_current_dir(cwd)?;
    }
    // Optional: cwd may not be a git project — global home still works.
    let local_root = paths::find_project_root().ok();
    if let Some(root) = &local_root {
        let _ = registry::ensure_known(Some(root));
    } else {
        let _ = registry::ensure_known(None);
    }
    let stall_warn_secs = local_root
        .as_ref()
        .and_then(|r| Config::load(r).ok())
        .map(|c| c.timeouts.stall_warn_secs)
        .unwrap_or(300);

    enable_raw_mode()?;
    // Install immediately so partial setup / panic still restores the terminal.
    let _guard = TerminalGuard;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    // Default to a minimal mouse mode: basic tracking (1000) + SGR encoding (1006).
    // crossterm's EnableMouseCapture also sets button/any-motion tracking
    // (1002/1003), which Termux silently drops — leaving the app with no mouse
    // events at all. 1000 still reports clicks and wheel, all this UI needs.
    // `--full-mouse` opts into the full capture for desktop terminals that want it.
    if opts.full_mouse {
        out.execute(EnableMouseCapture)?;
    } else {
        out.write_all(MOUSE_ENABLE)?;
        out.flush()?;
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.clear()?;

    run_loop(&mut terminal, local_root, opts.task_seed, stall_warn_secs)
}

/// Narrow/mobile SGR mouse: basic tracking + SGR encoding only (Termux-compatible;
/// see run_with). `DisableMouseCapture` on teardown disables this superset too.
const MOUSE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1006h";

/// Best-effort teardown of raw mode / mouse / alt-screen (safe if only partially entered).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut out = stdout();
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(LeaveAlternateScreen);
    }
}

/// Bytes of slot log kept in the live-log viewport (tail window).
const LOG_TAIL_BYTES: usize = 256_000;

/// Left-rail navigation: projects first (general), then runs for one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseLevel {
    /// General view — registered projects only (not a wall of runs).
    Projects,
    /// Per-project view — runs for `active_root` only.
    Runs,
}

struct App {
    selected_run: usize,
    selected_project: usize,
    selected_slot: usize,
    focus: Focus,
    browse: BrowseLevel,
    composer: String,
    status_line: String,
    stream_scroll: u16,
    bus_scroll: u16,
    /// When true, keep the live log pinned to the newest line as content grows.
    stream_follow: bool,
    bus_follow: bool,
    /// Last known max scroll offsets (from the most recent paint).
    stream_max: u16,
    bus_max: u16,
    /// Log viewport height in rows (for PageUp/PageDown).
    stream_view_h: u16,
    bus_view_h: u16,
    tick: u64,
    /// (started, message, color, how long to show)
    flash: Option<(Instant, String, Color, Duration)>,
    stall_warn_secs: u64,
    /// When false (default), long log lines truncate with …; `w` toggles wrap.
    log_expand: bool,
    /// First Ctrl+C timestamp; second within 2s exits (Esc/q never quit).
    last_ctrl_c: Option<Instant>,
    last_click: Option<(u16, u16, Instant)>,
    show_help: bool,
    /// Whether the current frame is part of an animation; drives the spinner so
    /// it shows a static glyph when idle instead of a frame frozen mid-spin.
    animated: bool,
    rect_header: Rect,
    rect_action: Rect,
    rect_fleet: Rect,
    rect_stream: Rect,
    rect_bus: Rect,
    rect_terminal: Rect,
    rect_composer: Rect,
    rect_runs: Rect,
    /// Narrow-mode tab strip (zero-sized when the wide layout is active).
    rect_tabs: Rect,
    /// One-shot: on first narrow render with an active run, jump focus to the log.
    narrow_autofocus_done: bool,
    /// Tappable gate buttons painted this frame, for touch/mouse hit-testing.
    gate_buttons: Vec<(Rect, GateAction)>,
    /// Tappable footer tokens.
    rect_help: Rect,
    rect_projects: Rect,
    /// Debounce for spawning the detached reconcile process (run id + when).
    reconcile_spawn: Option<(String, Instant)>,
    /// Count of unresolved `@human`/`Blocked` bus alerts for the selected run; drives
    /// the header badge. Refreshed from the snapshot each frame.
    human_alerts_n: usize,
    /// Embedded terminal (W3): a live vt100 view of the selected run's tmux pane.
    /// Lazily attached when the Terminal focus is opened over a live session.
    terminal_pane: Option<crate::terminal::TerminalPane>,
    /// Sender for background tasks (e.g. deferred `/spawn`) to flash a result back
    /// onto the render loop. Set once the message channel exists.
    bg_tx: Option<mpsc::Sender<Msg>>,
}

/// A gate action reachable by both a key and a tappable button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateAction {
    Approve,
    Reject,
    Ship,
    ConfirmWinner,
    Reconcile,
}

/// Avoid re-reading the slot log on every frame when the file is unchanged.
struct LogCache {
    path: Option<PathBuf>,
    len: u64,
    mtime: Option<SystemTime>,
    text: String,
    truncated: bool,
}

impl LogCache {
    fn empty() -> Self {
        Self {
            path: None,
            len: 0,
            mtime: None,
            text: String::new(),
            truncated: false,
        }
    }

    fn load(&mut self, path: &Path, max_bytes: usize) -> (&str, bool) {
        let meta = std::fs::metadata(path).ok();
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta.and_then(|m| m.modified().ok());
        let same = self.path.as_deref() == Some(path) && self.len == len && self.mtime == mtime;
        if !same {
            let tail = process::tail_log_info(path, max_bytes);
            if tail.io_error {
                // Do not cache a failed read as an empty successful snapshot.
                return ("", false);
            }
            self.path = Some(path.to_path_buf());
            self.len = len;
            self.mtime = mtime;
            self.text = tail.text;
            self.truncated = tail.truncated;
        }
        (&self.text, self.truncated)
    }

    fn clear(&mut self) {
        self.path = None;
        self.len = 0;
        self.mtime = None;
        self.text.clear();
        self.truncated = false;
    }
}

impl App {
    fn new(task_seed: Option<String>, stall_warn_secs: u64, start_in_project: bool) -> Self {
        Self {
            selected_run: 0,
            selected_project: 0,
            selected_slot: 0,
            focus: Focus::Runs,
            // Inside a project → that project's runs. Outside → project picker.
            browse: if start_in_project {
                BrowseLevel::Runs
            } else {
                BrowseLevel::Projects
            },
            composer: task_seed.unwrap_or_default(),
            status_line: String::new(),
            stream_scroll: 0,
            bus_scroll: 0,
            // Default: follow live output (newest lines).
            stream_follow: true,
            bus_follow: true,
            stream_max: 0,
            bus_max: 0,
            stream_view_h: 12,
            bus_view_h: 12,
            tick: 0,
            flash: None,
            stall_warn_secs,
            log_expand: false,
            last_ctrl_c: None,
            last_click: None,
            show_help: false,
            animated: false,
            rect_header: Rect::default(),
            rect_action: Rect::default(),
            rect_fleet: Rect::default(),
            rect_stream: Rect::default(),
            rect_bus: Rect::default(),
            rect_terminal: Rect::default(),
            rect_composer: Rect::default(),
            rect_runs: Rect::default(),
            rect_tabs: Rect::default(),
            narrow_autofocus_done: false,
            gate_buttons: Vec::new(),
            rect_help: Rect::default(),
            rect_projects: Rect::default(),
            reconcile_spawn: None,
            human_alerts_n: 0,
            terminal_pane: None,
            bg_tx: None,
        }
    }

    fn flash(&mut self, msg: impl Into<String>, color: Color) {
        self.flash_for(msg, color, Duration::from_secs(3));
    }

    fn flash_for(&mut self, msg: impl Into<String>, color: Color, for_ms: Duration) {
        self.flash = Some((Instant::now(), msg.into(), color, for_ms));
        self.status_line.clear();
        self.show_help = false;
    }

    fn spinner(&self) -> &'static str {
        if self.animated {
            SPINNER[(self.tick as usize) % SPINNER.len()]
        } else {
            "·"
        }
    }

    fn reset_stream_view(&mut self) {
        self.stream_scroll = 0;
        self.stream_follow = true;
    }

    fn reset_bus_view(&mut self) {
        self.bus_scroll = 0;
        self.bus_follow = true;
    }

    fn select_run(&mut self, idx: usize, n: usize) {
        if n == 0 {
            return;
        }
        self.selected_run = idx.min(n - 1);
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
    }

    fn select_project(&mut self, idx: usize, n: usize) {
        if n == 0 {
            return;
        }
        self.selected_project = idx.min(n - 1);
        self.selected_run = 0;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
    }

    fn select_slot(&mut self, idx: usize, n: usize) {
        if n == 0 {
            return;
        }
        self.selected_slot = idx.min(n - 1);
        self.reset_stream_view();
    }

    fn open_project_runs(&mut self) {
        self.browse = BrowseLevel::Runs;
        self.selected_run = 0;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Runs;
    }

    fn open_projects_view(&mut self) {
        self.browse = BrowseLevel::Projects;
        self.selected_run = 0;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Runs;
    }

    fn stream_page(&self) -> u16 {
        self.stream_view_h.saturating_sub(1).max(3)
    }

    fn bus_page(&self) -> u16 {
        self.bus_view_h.saturating_sub(1).max(3)
    }

    fn scroll_stream_by(&mut self, delta: i32) {
        apply_scroll_delta(
            &mut self.stream_scroll,
            &mut self.stream_follow,
            self.stream_max,
            delta,
        );
    }

    fn scroll_bus_by(&mut self, delta: i32) {
        apply_scroll_delta(
            &mut self.bus_scroll,
            &mut self.bus_follow,
            self.bus_max,
            delta,
        );
    }

    fn home_for_focus(&mut self) {
        match self.focus {
            Focus::Activity => {
                self.bus_follow = false;
                self.bus_scroll = 0;
            }
            _ => {
                self.stream_follow = false;
                self.stream_scroll = 0;
            }
        }
    }

    fn end_for_focus(&mut self) {
        match self.focus {
            Focus::Activity => {
                self.bus_follow = true;
                self.bus_scroll = self.bus_max;
            }
            _ => {
                self.stream_follow = true;
                self.stream_scroll = self.stream_max;
            }
        }
    }

    /// Forward a key event to the focused embedded terminal pane, if one is
    /// attached. No-op otherwise; the caller still consumes the key.
    fn forward_key_to_terminal(&self, code: KeyCode, mods: KeyModifiers) {
        if let Some(pane) = self.terminal_pane.as_ref() {
            pane.send_key(code, mods);
        }
    }
}

/// Apply a scroll delta and update follow-tail. Positive = toward newer lines.
fn apply_scroll_delta(scroll: &mut u16, follow: &mut bool, max: u16, delta: i32) {
    if delta == 0 {
        return;
    }
    if delta > 0 {
        let next = (*scroll as u32).saturating_add(delta as u32);
        *scroll = next.min(u32::from(max)) as u16;
    } else {
        let sub = (-delta) as u32;
        *scroll = (*scroll as u32).saturating_sub(sub) as u16;
    }
    // When content fits (max==0) or we remain at the end, keep follow so growth
    // does not leave the viewport stuck at the top of a short log.
    *follow = *scroll >= max;
}

/// Clamp scroll into `[0, max]`; when `follow`, pin to max.
fn clamp_scroll(scroll: &mut u16, follow: &mut bool, max: u16) {
    if *follow {
        *scroll = max;
    } else {
        *scroll = (*scroll).min(max);
        if *scroll >= max {
            *follow = true;
        }
    }
}

/// How often the background thread re-reads the run state from disk.
const REFRESH: Duration = Duration::from_millis(200);
/// Upper bound on how long the render thread sleeps; also the animation rate.
const FRAME: Duration = Duration::from_millis(100);

/// What the refresher needs in order to know which run/slot to read.
#[derive(Clone, PartialEq, Eq)]
struct Selection {
    browse: BrowseLevel,
    root: PathBuf,
    run_id: Option<String>,
    slot_idx: usize,
    project_idx: usize,
}

/// An immutable view of the world, produced off-thread and rendered as-is.
struct Snapshot {
    swarm: SparPaths,
    projects: Vec<registry::ProjectEntry>,
    runs: Vec<state::RunSummary>,
    full: Option<RunState>,
    stream_text: String,
    activity: Vec<String>,
    /// Unresolved `@human`/`Blocked` alerts for the selected run (header badge count).
    human_alerts: usize,
}

enum Msg {
    Input(Event),
    Data,
    /// A status line pushed from a background task (e.g. `/spawn`'s deferred
    /// spawn+deliver), flashed on the next render tick.
    Flash(String, Color),
}

/// Size+mtime of everything a snapshot is derived from. Comparing these is a
/// handful of `stat` calls, versus re-parsing the event log and every run state.
type Marks = Vec<Option<(u64, SystemTime)>>;

fn stamp(p: &Path) -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()?))
}

fn marks_for(sel: &Selection, prev: Option<&Snapshot>) -> Marks {
    let mut out = vec![stamp(&registry::registry_path())];
    if sel.browse == BrowseLevel::Runs {
        let swarm = SparPaths::new(&sel.root);
        let runs_dir = swarm.runs_dir();
        out.push(stamp(&runs_dir));
        out.push(stamp(&swarm.quota_file()));
        // The rail lists every run's phase/age from its state.json, which
        // RunState::save rewrites in place — so the dir mtime above misses it.
        // Stamp each state file; sort for a stable order across readdirs.
        if let Ok(entries) = std::fs::read_dir(&runs_dir) {
            let mut ids: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            ids.sort();
            for id in ids {
                out.push(stamp(&swarm.state_file(&id)));
            }
        }
        if let Some(id) = sel.run_id.as_deref() {
            out.push(stamp(&swarm.state_file(id)));
            out.push(stamp(&events::events_file(&swarm, id)));
            out.push(stamp(&crate::bus::run_events_path(&swarm, id)));
            // The live log grows without the run state changing.
            let slot = prev
                .and_then(|s| s.full.as_ref())
                .and_then(|st| st.slots.get(sel.slot_idx));
            if let Some(sl) = slot {
                let p = sl
                    .log_path
                    .clone()
                    .unwrap_or_else(|| swarm.log_file(id, &sl.id));
                out.push(stamp(&p));
            }
        }
    }
    out
}

/// All blocking filesystem work lives here, never on the render thread.
fn build_snapshot(sel: &Selection, cache: &mut LogCache) -> Snapshot {
    let swarm = SparPaths::new(&sel.root);
    let projects = registry::projects();
    let runs = if sel.browse == BrowseLevel::Runs {
        registry::list_project_runs(&sel.root).unwrap_or_default()
    } else {
        Vec::new()
    };
    let full = if sel.browse == BrowseLevel::Runs {
        sel.run_id
            .as_ref()
            .and_then(|id| RunState::load(&swarm, id).ok())
    } else {
        None
    };
    let quota = QuotaStore::load(&swarm).unwrap_or_default();
    let stream_text = match sel.browse {
        BrowseLevel::Projects => {
            cache.clear();
            project_overview(&projects, sel.project_idx)
        }
        BrowseLevel::Runs => stream_content(&swarm, full.as_ref(), sel.slot_idx, cache),
    };
    // The TUI refresh is a provider-agnostic delivery pulse for the selected run:
    // advance unacked-message redelivery/escalation before reading alerts, so
    // requires_ack works even when no Claude slot's Stop hook is ticking acks.
    if full.is_some() {
        let _ = crate::bus::tick_acks(&swarm, &crate::bus::AckPolicy::default(), Utc::now());
    }
    let alerts = full
        .as_ref()
        .map(|st| crate::bus::unresolved_alerts(&swarm, Some(&st.id)).unwrap_or_default())
        .unwrap_or_default();
    let activity = activity_feed(&swarm, full.as_ref(), &quota, &alerts);
    Snapshot {
        swarm,
        projects,
        runs,
        full,
        stream_text,
        activity,
        human_alerts: alerts.len(),
    }
}

/// Redraw is only worth it while something is moving on screen: a flash timer,
/// the composer cursor, or a run that is actively working (active phase or a
/// running slot). An active phase with no running slot — Suite, Review,
/// Shipping — still animates so the header spinner keeps turning.
fn animating(app: &App, snap: &Snapshot) -> bool {
    app.flash.is_some()
        || app.focus == Focus::Composer
        // A live terminal streams between disk snapshots; keep repainting it.
        || (app.focus == Focus::Terminal && app.terminal_pane.is_some())
        || snap.full.as_ref().is_some_and(|st| {
            is_active_phase(st.phase) || st.slots.iter().any(|s| s.status == SlotStatus::Running)
        })
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    local_root: Option<PathBuf>,
    task_seed: Option<String>,
    stall_warn_secs: u64,
) -> Result<crate::exit_codes::ExitCode> {
    let mut app = App::new(task_seed, stall_warn_secs, local_root.is_some());
    let mut fleet_state = ListState::default();
    let mut rail_state = ListState::default();
    let mut active_root: PathBuf = local_root.clone().unwrap_or_else(|| {
        registry::projects()
            .into_iter()
            .next()
            .map(|p| p.root)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    });

    let mut sel = Selection {
        browse: app.browse,
        root: active_root.clone(),
        run_id: None,
        slot_idx: 0,
        project_idx: 0,
    };

    // First paint needs data, so build one snapshot synchronously.
    let mut cache = LogCache::empty();
    let snapshot = Arc::new(Mutex::new(Arc::new(build_snapshot(&sel, &mut cache))));

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (sel_tx, sel_rx) = mpsc::channel::<Selection>();
    app.bg_tx = Some(msg_tx.clone());

    {
        let tx = msg_tx.clone();
        thread::spawn(move || {
            while let Ok(ev) = event::read() {
                if tx.send(Msg::Input(ev)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let tx = msg_tx;
        let slot = Arc::clone(&snapshot);
        let mut sel = sel.clone();
        let mut marks = Marks::new();
        thread::spawn(move || loop {
            let mut forced = false;
            match sel_rx.recv_timeout(REFRESH) {
                Ok(s) => {
                    sel = s;
                    while let Ok(newer) = sel_rx.try_recv() {
                        sel = newer;
                    }
                    forced = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            let prev = Arc::clone(&*slot.lock().unwrap());
            let next_marks = marks_for(&sel, Some(&prev));
            if !forced && next_marks == marks {
                continue; // nothing on disk moved; don't rebuild, don't repaint
            }
            marks = next_marks;

            let next = Arc::new(build_snapshot(&sel, &mut cache));
            *slot.lock().unwrap() = next;
            if tx.send(Msg::Data).is_err() {
                break;
            }
        });
    }

    let mut dirty = true;
    loop {
        let snap = Arc::clone(&*snapshot.lock().unwrap());

        if let Some((t, _, _, dur)) = &app.flash {
            if t.elapsed() > *dur {
                app.flash = None;
                dirty = true;
            }
        }

        // Clamp selections against the snapshot we are about to paint.
        if snap.projects.is_empty() {
            app.selected_project = 0;
        } else {
            app.selected_project = app.selected_project.min(snap.projects.len() - 1);
            if app.browse == BrowseLevel::Projects {
                active_root = snap.projects[app.selected_project].root.clone();
            }
        }
        if snap.runs.is_empty() {
            app.selected_run = 0;
        } else {
            app.selected_run = app.selected_run.min(snap.runs.len() - 1);
        }
        let n_slots = snap.full.as_ref().map(|s| s.slots.len()).unwrap_or(0);
        app.selected_slot = if n_slots == 0 {
            0
        } else {
            app.selected_slot.min(n_slots - 1)
        };

        rail_state.select(match app.browse {
            BrowseLevel::Projects if !snap.projects.is_empty() => Some(app.selected_project),
            BrowseLevel::Runs if !snap.runs.is_empty() => Some(app.selected_run),
            _ => None,
        });
        fleet_state.select((n_slots > 0).then_some(app.selected_slot));

        manage_terminal(&mut app, snap.full.as_ref());
        app.animated = animating(&app, &snap);
        app.human_alerts_n = snap.human_alerts;

        if dirty {
            app.tick = app.tick.wrapping_add(1);
            terminal.draw(|f| {
                draw(
                    f,
                    &snap.swarm,
                    &snap.projects,
                    &snap.runs,
                    snap.full.as_ref(),
                    &snap.stream_text,
                    &snap.activity,
                    &mut app,
                    &mut fleet_state,
                    &mut rail_state,
                );
            })?;
            dirty = false;
        }

        match msg_rx.recv_timeout(FRAME) {
            Ok(Msg::Data) => dirty = true,
            Ok(Msg::Flash(msg, color)) => {
                app.flash(msg, color);
                dirty = true;
            }
            Ok(Msg::Input(ev)) => {
                dirty = true;
                let mut ev = Some(ev);
                // Drain the burst so wheel/key spam cannot outpace the redraw.
                while let Some(e) = ev {
                    match e {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            if handle_key(
                                &mut app,
                                key.code,
                                key.modifiers,
                                &snap.swarm,
                                &snap.projects,
                                &snap.runs,
                                snap.full.as_ref(),
                                &mut active_root,
                                local_root.as_deref(),
                            )? {
                                return Ok(crate::exit_codes::ExitCode::Success);
                            }
                        }
                        Event::Mouse(m) => handle_mouse(
                            &mut app,
                            m,
                            &snap.swarm,
                            &snap.projects,
                            &snap.runs,
                            snap.full.as_ref(),
                            &mut active_root,
                            local_root.as_deref(),
                            rail_state.offset(),
                            fleet_state.offset(),
                        ),
                        _ => {}
                    }
                    ev = match msg_rx.try_recv() {
                        Ok(Msg::Input(next)) => Some(next),
                        Ok(Msg::Flash(msg, color)) => {
                            app.flash(msg, color);
                            None
                        }
                        Ok(Msg::Data) => None,
                        Err(_) => None,
                    };
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if app.animated {
                    dirty = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(crate::exit_codes::ExitCode::Success)
            }
        }

        let next_sel = Selection {
            browse: app.browse,
            root: active_root.clone(),
            run_id: snap.runs.get(app.selected_run).map(|r| r.id.clone()),
            slot_idx: app.selected_slot,
            project_idx: app.selected_project,
        };
        if next_sel != sel {
            sel = next_sel.clone();
            let _ = sel_tx.send(next_sel);
        }
    }
}

fn project_overview(projects: &[registry::ProjectEntry], idx: usize) -> String {
    if projects.is_empty() {
        return format!(
            "\n  No projects registered yet.\n\n  cd into a repo and run spar (or start a plan).\n  Registry: {}\n",
            registry::spar_home().display()
        );
    }
    let p = &projects[idx.min(projects.len() - 1)];
    let n_runs = registry::list_project_runs(&p.root)
        .map(|r| r.len())
        .unwrap_or(0);
    format!(
        "\n  Project: {}\n  Path:    {}\n  Runs:    {}\n  Last:    {}\n\n  Enter / click  → open this project's runs\n  p              → stay on projects list\n",
        p.name.as_deref().unwrap_or("·"),
        p.root.display(),
        n_runs,
        relative_age(p.last_seen),
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
    local_root: Option<&std::path::Path>,
) -> Result<bool> {
    let selected_id = runs.get(app.selected_run).map(|r| r.id.as_str());
    let n_slots = full.map(|s| s.slots.len()).unwrap_or(0);

    // Ctrl+C twice (within 2s) is the only quit path — never Esc or q. While the
    // Terminal panel is focused, Ctrl+C belongs to the agent pane (SIGINT), so it
    // falls through to forwarding; Tab out first to reach the quit arm.
    if code == KeyCode::Char('c')
        && mods.contains(KeyModifiers::CONTROL)
        && app.focus != Focus::Terminal
    {
        if let Some(t) = app.last_ctrl_c {
            if t.elapsed() < Duration::from_secs(2) {
                return Ok(true);
            }
        }
        app.last_ctrl_c = Some(Instant::now());
        // Match the 2s double-press window so the hint doesn't outlive the arm.
        app.flash_for("Ctrl+C again to exit", YELLOW, Duration::from_secs(2));
        return Ok(false);
    }
    // Any other key clears the first Ctrl+C arm.
    if !mods.contains(KeyModifiers::CONTROL) {
        app.last_ctrl_c = None;
    }

    if app.show_help {
        match code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter => {
                app.show_help = false;
            }
            _ => {}
        }
        return Ok(false);
    }

    // Terminal panel focused: keystrokes drive the live agent pane, not TUI nav.
    // Tab / BackTab stay the focus-cycle escape hatch so the operator is never
    // trapped; everything else (printable text, Enter, Ctrl-combos, arrows, Esc,
    // Backspace, …) is forwarded to the pane and consumed here.
    if app.focus == Focus::Terminal {
        match code {
            KeyCode::Tab => app.focus = app.focus.next(),
            KeyCode::BackTab => app.focus = app.focus.prev(),
            _ => app.forward_key_to_terminal(code, mods),
        }
        return Ok(false);
    }

    if app.focus == Focus::Composer {
        match code {
            KeyCode::Esc => app.focus = Focus::Runs,
            KeyCode::Enter => {
                let line = app.composer.trim().to_string();
                if !line.is_empty() {
                    let bg = app.bg_tx.clone();
                    match handle_composer(swarm, runs, app.selected_run, &line, bg) {
                        Ok(msg) => {
                            app.flash(msg, GREEN);
                            app.composer.clear();
                        }
                        Err(e) => app.flash(format!("{e:#}"), RED),
                    }
                }
            }
            KeyCode::Backspace => {
                app.composer.pop();
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                app.composer.push(c);
            }
            KeyCode::Tab => app.focus = app.focus.next(),
            KeyCode::BackTab => app.focus = app.focus.prev(),
            _ => {}
        }
        return Ok(false);
    }

    match code {
        KeyCode::Esc => {
            if app.focus != Focus::Runs {
                app.focus = Focus::Runs;
            } else if app.browse == BrowseLevel::Runs {
                app.open_projects_view();
                app.flash("Projects — Enter to open · p always returns here", ACCENT);
            }
            // Never exit on Esc (including Projects view).
        }
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('/') => {
            app.focus = Focus::Composer;
            if !app.composer.starts_with('/') {
                app.composer = "/".into();
            }
        }
        KeyCode::Char('i') => app.focus = Focus::Composer,
        KeyCode::Enter => {
            if app.focus == Focus::Runs && app.browse == BrowseLevel::Projects {
                if let Some(p) = projects.get(app.selected_project) {
                    *active_root = p.root.clone();
                    app.open_project_runs();
                    app.flash(
                        format!("Opened {}", p.name.as_deref().unwrap_or("project")),
                        GREEN,
                    );
                }
            }
        }
        KeyCode::Char('p') => {
            app.open_projects_view();
            // Highlight local project if present
            if let Some(root) = local_root {
                if let Some(i) = projects.iter().position(|p| p.root == root) {
                    app.selected_project = i;
                }
            }
            app.flash("Projects (general view)", ACCENT);
        }
        KeyCode::Char('j') | KeyCode::Down => match app.focus {
            Focus::Runs => match app.browse {
                BrowseLevel::Projects => {
                    if !projects.is_empty() {
                        app.select_project(app.selected_project + 1, projects.len());
                    }
                }
                BrowseLevel::Runs => {
                    if !runs.is_empty() {
                        app.select_run(app.selected_run + 1, runs.len());
                    }
                }
            },
            Focus::Agents => {
                if n_slots > 0 {
                    app.select_slot(app.selected_slot + 1, n_slots);
                }
            }
            Focus::Log => app.scroll_stream_by(3),
            Focus::Activity => app.scroll_bus_by(1),
            Focus::Terminal | Focus::Composer => {}
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::Runs => match app.browse {
                BrowseLevel::Projects => {
                    app.select_project(
                        app.selected_project.saturating_sub(1),
                        projects.len().max(1),
                    );
                }
                BrowseLevel::Runs => {
                    app.select_run(app.selected_run.saturating_sub(1), runs.len().max(1));
                }
            },
            Focus::Agents => {
                app.select_slot(app.selected_slot.saturating_sub(1), n_slots.max(1));
            }
            Focus::Log => app.scroll_stream_by(-3),
            Focus::Activity => app.scroll_bus_by(-1),
            Focus::Terminal | Focus::Composer => {}
        },
        KeyCode::PageDown => match app.focus {
            Focus::Log => app.scroll_stream_by(i32::from(app.stream_page())),
            Focus::Activity => app.scroll_bus_by(i32::from(app.bus_page())),
            Focus::Runs => match app.browse {
                BrowseLevel::Projects if !projects.is_empty() => {
                    app.select_project(app.selected_project + 5, projects.len());
                }
                BrowseLevel::Runs if !runs.is_empty() => {
                    app.select_run(app.selected_run + 5, runs.len());
                }
                _ => {}
            },
            Focus::Agents if n_slots > 0 => app.select_slot(app.selected_slot + 5, n_slots),
            _ => {}
        },
        KeyCode::PageUp => match app.focus {
            Focus::Log => app.scroll_stream_by(-i32::from(app.stream_page())),
            Focus::Activity => app.scroll_bus_by(-i32::from(app.bus_page())),
            Focus::Runs => match app.browse {
                BrowseLevel::Projects => {
                    app.select_project(
                        app.selected_project.saturating_sub(5),
                        projects.len().max(1),
                    );
                }
                BrowseLevel::Runs => {
                    app.select_run(app.selected_run.saturating_sub(5), runs.len().max(1));
                }
            },
            Focus::Agents => {
                app.select_slot(app.selected_slot.saturating_sub(5), n_slots.max(1));
            }
            _ => {}
        },
        KeyCode::Char('J') | KeyCode::Char(']') => {
            app.focus = Focus::Runs;
            match app.browse {
                BrowseLevel::Projects if !projects.is_empty() => {
                    app.select_project(app.selected_project + 1, projects.len());
                }
                BrowseLevel::Runs if !runs.is_empty() => {
                    app.select_run(app.selected_run + 1, runs.len());
                }
                _ => {}
            }
        }
        KeyCode::Char('K') | KeyCode::Char('[') => {
            app.focus = Focus::Runs;
            match app.browse {
                BrowseLevel::Projects => {
                    app.select_project(
                        app.selected_project.saturating_sub(1),
                        projects.len().max(1),
                    );
                }
                BrowseLevel::Runs => {
                    app.select_run(app.selected_run.saturating_sub(1), runs.len().max(1));
                }
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as u8 - b'1') as usize;
            if n_slots > 0 && idx < n_slots {
                app.select_slot(idx, n_slots);
                app.focus = Focus::Agents;
            }
        }
        KeyCode::Char('a') => {
            if let Some(id) = selected_id {
                run_gate_action(app, swarm, id, GateAction::Approve);
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = selected_id {
                run_gate_action(app, swarm, id, GateAction::Reject);
            }
        }
        KeyCode::Char('s') => {
            if let Some(id) = selected_id {
                run_gate_action(app, swarm, id, GateAction::Ship);
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            app.home_for_focus();
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.end_for_focus();
        }
        KeyCode::Char('?') => {
            app.show_help = true;
        }
        KeyCode::Char('w') => {
            app.log_expand = !app.log_expand;
            // Row count changes with wrap; keep follow semantics, clamp on next paint.
            app.flash(
                if app.log_expand {
                    "Log: wrap long lines"
                } else {
                    "Log: truncate long lines (w toggles)"
                },
                ACCENT,
            );
        }
        _ => {}
    }
    Ok(false)
}

/// Run a gate action from a key or a tapped button — one path for both.
fn run_gate_action(app: &mut App, swarm: &SparPaths, id: &str, action: GateAction) {
    let res = match action {
        GateAction::Approve => workflow::plan::approve(swarm, id, false)
            .map(|_| (format!("Approved plan {id}"), GREEN)),
        GateAction::Reject => workflow::plan::reject(swarm, id, None, false)
            .map(|_| (format!("Rejected plan {id}"), YELLOW)),
        GateAction::Ship => crate::ship::confirm_ship(swarm, id, false)
            .map(|_| (format!("Ship confirmed {id}"), GREEN)),
        GateAction::ConfirmWinner => workflow::arena::confirm_winner(swarm, id, None, false)
            .map(|_| (format!("Confirmed winner for {id}"), GREEN)),
        // Reconcile runs agents (minutes) — never on the render thread.
        GateAction::Reconcile => return spawn_reconcile(app, swarm, id),
    };
    match res {
        Ok((msg, color)) => app.flash(msg, color),
        Err(e) => app.flash(format!("{} failed: {e:#}", action.verb()), RED),
    }
}

/// Kick off arena reconcile as a detached `spar reconcile` process so it survives
/// the TUI and keeps agent work off the render loop. Progress shows via the log.
fn spawn_reconcile(app: &mut App, swarm: &SparPaths, id: &str) {
    if let Some((rid, t)) = &app.reconcile_spawn {
        if rid == id && t.elapsed() < Duration::from_secs(15) {
            app.flash("Reconcile already starting…", YELLOW);
            return;
        }
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            app.flash(format!("Reconcile failed to start: {e}"), RED);
            return;
        }
    };
    let spawned = std::process::Command::new(exe)
        .arg("reconcile")
        .arg(id)
        .arg("--json")
        .current_dir(&swarm.project_root)
        .env("SPAR_INTERNAL", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => {
            app.reconcile_spawn = Some((id.to_string(), Instant::now()));
            app.flash(
                format!("Reconcile started for {id} — watch Live log"),
                ACCENT,
            );
        }
        Err(e) => app.flash(format!("Reconcile failed to start: {e}"), RED),
    }
}

/// Gate buttons for the current phase, in display order (label, action).
fn gate_buttons_for(full: Option<&RunState>) -> Vec<(&'static str, GateAction)> {
    match full.map(|s| s.phase) {
        Some(Phase::AwaitingPlanApproval) => vec![
            ("Approve", GateAction::Approve),
            ("Reject", GateAction::Reject),
        ],
        Some(Phase::AwaitingShipConfirm) => vec![("Ship", GateAction::Ship)],
        Some(Phase::AwaitingWinnerConfirm) => vec![
            ("Confirm", GateAction::ConfirmWinner),
            ("Reconcile", GateAction::Reconcile),
        ],
        Some(Phase::AwaitingReconcile) => vec![("Reconcile", GateAction::Reconcile)],
        _ => Vec::new(),
    }
}

impl GateAction {
    fn verb(self) -> &'static str {
        match self {
            GateAction::Approve => "Approve",
            GateAction::Reject => "Reject",
            GateAction::Ship => "Ship",
            GateAction::ConfirmWinner => "Confirm winner",
            GateAction::Reconcile => "Reconcile",
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
    local_root: Option<&Path>,
    rail_offset: usize,
    fleet_offset: usize,
) {
    let (x, y) = (m.column, m.row);
    let n_slots = full.map(|s| s.slots.len()).unwrap_or(0);

    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();
            let dbl = app
                .last_click
                .map(|(lx, ly, t)| lx == x && ly == y && t.elapsed() < Duration::from_millis(400))
                .unwrap_or(false);
            app.last_click = Some((x, y, now));

            // A tap anywhere dismisses the help overlay first.
            if app.show_help {
                app.show_help = false;
                return;
            }
            // Tappable gate buttons take priority — they sit on the status/action bar.
            // Target the run the buttons were painted from (`full`), not the rail
            // selection, which can lag by a snapshot cycle.
            if let Some(&(_, action)) = app.gate_buttons.iter().find(|(r, _)| contains(*r, x, y)) {
                if let Some(id) = full.map(|s| s.id.as_str()) {
                    run_gate_action(app, swarm, id, action);
                }
                return;
            }
            if contains(app.rect_help, x, y) {
                app.show_help = true;
                return;
            }
            if contains(app.rect_projects, x, y) {
                app.open_projects_view();
                if let Some(root) = local_root {
                    if let Some(i) = projects.iter().position(|p| p.root == root) {
                        app.selected_project = i;
                    }
                }
                return;
            }

            if contains(app.rect_tabs, x, y) {
                let n = NARROW_TABS.len() as u16;
                let cell = (app.rect_tabs.width / n).max(1);
                let idx = ((x - app.rect_tabs.x) / cell).min(n - 1) as usize;
                app.focus = NARROW_TABS[idx].0;
            } else if contains(app.rect_composer, x, y) {
                app.focus = Focus::Composer;
            } else if contains(app.rect_stream, x, y) {
                app.focus = Focus::Log;
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Activity;
            } else if contains(app.rect_fleet, x, y) {
                app.focus = Focus::Agents;
                if let Some(st) = full {
                    if let Some(row) = list_row_at(app.rect_fleet, y, st.slots.len(), fleet_offset)
                    {
                        app.select_slot(row, st.slots.len());
                    }
                }
            } else if contains(app.rect_runs, x, y) {
                app.focus = Focus::Runs;
                match app.browse {
                    BrowseLevel::Projects => {
                        if let Some(row) =
                            list_row_at(app.rect_runs, y, projects.len(), rail_offset)
                        {
                            app.select_project(row, projects.len());
                            if dbl {
                                if let Some(p) = projects.get(row) {
                                    *active_root = p.root.clone();
                                    app.open_project_runs();
                                }
                            }
                        }
                    }
                    BrowseLevel::Runs => {
                        if let Some(row) = list_row_at(app.rect_runs, y, runs.len(), rail_offset) {
                            app.select_run(row, runs.len());
                        }
                    }
                }
            } else if contains(app.rect_action, x, y) {
                app.focus = Focus::Runs;
            }
        }
        MouseEventKind::ScrollDown => {
            if contains(app.rect_stream, x, y) {
                app.focus = Focus::Log;
                app.scroll_stream_by(3);
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Activity;
                app.scroll_bus_by(2);
            } else if contains(app.rect_fleet, x, y) {
                app.focus = Focus::Agents;
                if n_slots > 0 {
                    app.select_slot(app.selected_slot + 1, n_slots);
                }
            } else if contains(app.rect_runs, x, y) {
                app.focus = Focus::Runs;
                match app.browse {
                    BrowseLevel::Projects if !projects.is_empty() => {
                        app.select_project(app.selected_project + 1, projects.len());
                    }
                    BrowseLevel::Runs if !runs.is_empty() => {
                        app.select_run(app.selected_run + 1, runs.len());
                    }
                    _ => {}
                }
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(app.rect_stream, x, y) {
                app.focus = Focus::Log;
                app.scroll_stream_by(-3);
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Activity;
                app.scroll_bus_by(-2);
            } else if contains(app.rect_fleet, x, y) {
                app.focus = Focus::Agents;
                if n_slots > 0 {
                    app.select_slot(app.selected_slot.saturating_sub(1), n_slots);
                }
            } else if contains(app.rect_runs, x, y) {
                app.focus = Focus::Runs;
                match app.browse {
                    BrowseLevel::Projects => {
                        app.select_project(
                            app.selected_project.saturating_sub(1),
                            projects.len().max(1),
                        );
                    }
                    BrowseLevel::Runs => {
                        app.select_run(app.selected_run.saturating_sub(1), runs.len().max(1));
                    }
                }
            }
        }
        _ => {}
    }
}

/// Map a mouse Y to a list row inside a bordered panel (title row skipped).
/// `offset` is the ListState scroll offset so clicks track the visible window.
fn list_row_at(panel: Rect, y: u16, n_items: usize, offset: usize) -> Option<usize> {
    if n_items == 0 || panel.height < 3 {
        return None;
    }
    // border top + title uses y = panel.y; first visible item at panel.y + 1
    let inner_y = y.saturating_sub(panel.y.saturating_add(1));
    let visible = panel.height.saturating_sub(2) as usize;
    if inner_y as usize >= visible {
        return None;
    }
    let row = offset.saturating_add(inner_y as usize);
    if row < n_items {
        Some(row)
    } else {
        None
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

struct LayoutRects {
    header: Rect,
    action: Rect,
    runs: Rect,
    fleet: Rect,
    stream: Rect,
    bus: Rect,
    /// Embedded terminal stage; non-zero only when the Terminal focus is active
    /// (it shares the main stage with the live log).
    terminal: Rect,
    composer: Rect,
    footer: Rect,
    /// Narrow-mode tab strip; zero-sized in the wide layout.
    tabs: Rect,
    /// True when the single-panel phone layout is active.
    narrow: bool,
}

/// Below this terminal width the 3-column layout collapses to one focused
/// panel at a time with a tab strip — usable over a phone/Termux SSH session.
const NARROW_WIDTH: u16 = 90;

fn layout_rects(area: Rect, focus: Focus) -> LayoutRects {
    if area.width < NARROW_WIDTH {
        // Header + action collapse into a single status row; one panel fills the
        // stage, a tab strip picks which. Off-screen panels get zero rects so
        // both painting and mouse hit-testing skip them.
        let nroot = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // status bar (header + action merged)
                Constraint::Length(1), // tab strip
                Constraint::Min(4),    // focused panel
                Constraint::Length(4), // command
                Constraint::Length(1), // footer
            ])
            .split(area);
        let z = Rect::default();
        let mut lay = LayoutRects {
            header: nroot[0],
            action: z,
            runs: z,
            fleet: z,
            stream: z,
            bus: z,
            terminal: z,
            composer: nroot[3],
            footer: nroot[4],
            tabs: nroot[1],
            narrow: true,
        };
        // Composer focus keeps the live log on stage so you can watch while typing.
        match focus {
            Focus::Runs => lay.runs = nroot[2],
            Focus::Agents => lay.fleet = nroot[2],
            Focus::Log | Focus::Composer => lay.stream = nroot[2],
            Focus::Activity => lay.bus = nroot[2],
            Focus::Terminal => lay.terminal = nroot[2],
        }
        return lay;
    }

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(2), // action / context
            Constraint::Min(8),    // main
            Constraint::Length(4), // command
            Constraint::Length(1), // footer
        ])
        .split(area);

    // Wide live log is the main stage; rail + activity are supporting columns.
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(22),
            Constraint::Percentage(58),
            Constraint::Percentage(20),
        ])
        .split(root[2]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(mid[0]);

    // The terminal shares the main stage with the live log; only one paints,
    // chosen by focus, so a zero rect suppresses the other.
    let (stream, terminal) = if focus == Focus::Terminal {
        (Rect::default(), mid[1])
    } else {
        (mid[1], Rect::default())
    };

    LayoutRects {
        header: root[0],
        action: root[1],
        runs: left[0],
        fleet: left[1],
        stream,
        bus: mid[2],
        terminal,
        composer: root[3],
        footer: root[4],
        tabs: Rect::default(),
        narrow: false,
    }
}

/// Narrow-mode tabs, in cycle order, each mapped to the focus it selects.
const NARROW_TABS: [(Focus, &str); 5] = [
    (Focus::Runs, "Runs"),
    (Focus::Agents, "Agents"),
    (Focus::Log, "Log"),
    (Focus::Activity, "Activity"),
    (Focus::Terminal, "Term"),
];

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut Frame,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    stream_text: &str,
    activity: &[String],
    app: &mut App,
    fleet_state: &mut ListState,
    rail_state: &mut ListState,
) {
    let area = f.area();
    // Full clear each frame — prevents styled-cell ghosting across the whole UI.
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    // On the first narrow render with an active run, land on the live log so a
    // phone glance shows progress — but only once, and never over a manual Tab.
    if area.width < NARROW_WIDTH && !app.narrow_autofocus_done {
        let active = full.map(|s| {
            is_active_phase(s.phase) || s.slots.iter().any(|sl| sl.status == SlotStatus::Running)
        });
        if active == Some(true) {
            if app.focus == Focus::Runs {
                app.focus = Focus::Log;
            }
            app.narrow_autofocus_done = true;
        }
    }

    let lay = layout_rects(area, app.focus);
    // Keep mouse hit regions aligned with the frame actually painted.
    app.rect_header = lay.header;
    app.rect_action = lay.action;
    app.rect_runs = lay.runs;
    app.rect_fleet = lay.fleet;
    app.rect_stream = lay.stream;
    app.rect_bus = lay.bus;
    app.rect_terminal = lay.terminal;
    app.rect_composer = lay.composer;
    app.rect_tabs = lay.tabs;
    // Rebuilt below by whichever bar/footer paints this frame.
    app.gate_buttons.clear();

    if lay.narrow {
        draw_status_bar(f, lay.header, swarm, full, app, projects, runs);
        draw_tabs(f, lay.tabs, app);
        // Only the focused panel is on stage; its rect is non-zero, the rest zero.
        match app.focus {
            Focus::Runs => draw_rail(f, lay.runs, projects, runs, app, rail_state),
            Focus::Agents => draw_agents(f, lay.fleet, full, app, fleet_state),
            Focus::Log | Focus::Composer => draw_stream(f, lay.stream, full, stream_text, app),
            Focus::Activity => draw_activity(f, lay.bus, activity, app),
            Focus::Terminal => draw_terminal(f, lay.terminal, app),
        }
    } else {
        draw_header(f, lay.header, swarm, full, app);
        draw_action(f, lay.action, projects, runs, full, app);
        draw_rail(f, lay.runs, projects, runs, app, rail_state);
        draw_agents(f, lay.fleet, full, app, fleet_state);
        if app.focus == Focus::Terminal {
            draw_terminal(f, lay.terminal, app);
        } else {
            draw_stream(f, lay.stream, full, stream_text, app);
        }
        draw_activity(f, lay.bus, activity, app);
    }
    draw_composer(f, lay.composer, app);
    draw_footer(f, lay.footer, app, full);

    if app.show_help {
        draw_help_overlay(f, area);
    }
}

/// One-row tab strip for the narrow layout: four equal cells, focused one lit.
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let n = NARROW_TABS.len() as u16;
    let cell = area.width / n;
    let mut spans: Vec<Span> = Vec::with_capacity(NARROW_TABS.len());
    for (i, (focus, label)) in NARROW_TABS.iter().enumerate() {
        // Composer focus visually maps to the Log tab (that's what's on stage).
        let on = app.focus == *focus || (app.focus == Focus::Composer && *focus == Focus::Log);
        let w = if i as u16 == n - 1 {
            area.width.saturating_sub(cell * (n - 1))
        } else {
            cell
        } as usize;
        let text = format!("{label:^w$}");
        let style = if on {
            Style::default().fg(BG).bg(ACCENT).bold()
        } else {
            Style::default().fg(FG_MUTED).bg(BG_RAISED)
        };
        spans.push(Span::styled(text, style));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_RAISED)),
        area,
    );
}

fn draw_header(f: &mut Frame, area: Rect, swarm: &SparPaths, full: Option<&RunState>, app: &App) {
    let project = swarm
        .project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".");
    let (run, phase_label, task) = match full {
        Some(st) => (
            st.id.clone(),
            phase_label(st.phase),
            st.task
                .as_deref()
                .map(|t| truncate(t, 36))
                .unwrap_or_default(),
        ),
        None => ("—".into(), "No run selected".into(), String::new()),
    };

    let phase_color = full.map(|s| phase_color(s.phase)).unwrap_or(FG_DIM);
    let dry = full
        .filter(|s| s.dry_run)
        .map(|_| " dry-run ")
        .unwrap_or("");

    let scope = match app.browse {
        BrowseLevel::Projects => " projects ",
        BrowseLevel::Runs => " runs ",
    };
    let alert_badge = if app.human_alerts_n > 0 {
        format!(" ⚠ {} needs you ", app.human_alerts_n)
    } else {
        String::new()
    };
    let left = Line::from(vec![
        Span::styled(" spar ", Style::default().fg(BG).bg(ACCENT).bold()),
        Span::raw(" "),
        Span::styled(project, Style::default().fg(FG).bold()),
        Span::styled(scope, Style::default().fg(BG).bg(ACCENT_SOFT)),
        Span::styled("  ·  ", Style::default().fg(FG_MUTED)),
        Span::styled(run, Style::default().fg(CYAN)),
        Span::styled(dry, Style::default().fg(BG).bg(YELLOW).bold()),
        Span::styled(alert_badge, Style::default().fg(BG).bg(RED).bold()),
        Span::raw("  "),
        Span::styled(
            if full.map(|s| is_active_phase(s.phase)).unwrap_or(false) {
                format!("{} ", app.spinner())
            } else {
                String::new()
            },
            Style::default().fg(phase_color),
        ),
        Span::styled(phase_label, Style::default().fg(phase_color).bold()),
    ]);
    let right = Line::from(vec![
        Span::styled(task, Style::default().fg(FG_DIM)),
        Span::raw("  "),
        Span::styled(
            format!("focus: {}  ", app.focus.label()),
            Style::default().fg(ACCENT_SOFT),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(BG_RAISED));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(42)])
        .split(inner);
    f.render_widget(Paragraph::new(left), chunks[0]);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), chunks[1]);
}

/// The action/context banner text and its colors. `bg != BG_RAISED` means an
/// alert state (gate, quota, failure) worth surfacing loudly.
fn action_content(
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    app: &App,
) -> (String, Color, Color) {
    if app.browse == BrowseLevel::Projects {
        if projects.is_empty() {
            (
                format!(
                    "  No projects yet — cd into a repo and run spar · index {}  ",
                    registry::spar_home().display()
                ),
                FG_DIM,
                BG_RAISED,
            )
        } else {
            (
                "  Projects (general) — j/k select · Enter / double-click open runs · p stays here  "
                    .into(),
                FG_MUTED,
                BG_RAISED,
            )
        }
    } else if let Some(st) = full {
        match st.phase {
            Phase::AwaitingPlanApproval => (
                "  Plan + tests ready — press  a  to approve ·  r  to reject · p = all projects  "
                    .into(),
                BG,
                YELLOW,
            ),
            Phase::AwaitingWinnerConfirm => (
                "  Arena winner ready — confirm with spar confirm · p = projects  ".into(),
                BG,
                YELLOW,
            ),
            Phase::AwaitingShipConfirm => (
                "  Ready to ship — press  s  (draft PR) · p = projects  ".into(),
                BG,
                YELLOW,
            ),
            Phase::AwaitingReconcile => (
                "  Arena reconcile ready — spar reconcile · p = projects  ".into(),
                BG,
                YELLOW,
            ),
            Phase::Quota => (
                "  All providers paused (quota) — spar provider resume  ".into(),
                BG,
                RED,
            ),
            Phase::Failed | Phase::Stuck | Phase::Escalated => (
                format!(
                    "  {} — check Live log · spar cleanup {} · p = projects  ",
                    phase_label(st.phase),
                    st.id
                ),
                FG,
                Color::Rgb(48, 24, 24),
            ),
            _ if st.dry_run => (
                "  Dry-run — synthetic agents only · p = projects  ".into(),
                FG_DIM,
                BG_RAISED,
            ),
            _ if is_active_phase(st.phase) => (
                format!(
                    "  Working…  j/k runs · scroll Live log · p = projects · Tab = {}  ",
                    app.focus.label()
                ),
                FG_DIM,
                BG_RAISED,
            ),
            _ => (
                format!(
                    "  {}  ·  p projects · Tab panes · j/k · ? help  ",
                    phase_label(st.phase)
                ),
                FG_MUTED,
                BG_RAISED,
            ),
        }
    } else if runs.is_empty() {
        (
            "  No runs in this project — spar plan -t \"…\" --providers cli:claude  ·  Esc/p = projects  ".into(),
            FG_DIM,
            BG_RAISED,
        )
    } else {
        (
            "  This project's runs — j/k select · Esc or p = all projects · Tab panes  ".into(),
            FG_MUTED,
            BG_RAISED,
        )
    }
}

fn button_style(action: GateAction) -> Style {
    let bg = match action {
        GateAction::Approve | GateAction::Ship | GateAction::ConfirmWinner => GREEN,
        GateAction::Reject => RED,
        GateAction::Reconcile => ACCENT,
    };
    Style::default().fg(BG).bg(bg).bold()
}

/// Paint right-aligned tappable gate buttons filling every row of `area` and
/// record their hit-rects. Buttons overpaint whatever text sits beneath them.
fn render_gate_buttons(f: &mut Frame, area: Rect, app: &mut App, buttons: &[(&str, GateAction)]) {
    if buttons.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }
    let labels: Vec<String> = buttons.iter().map(|(l, _)| format!(" {l} ")).collect();
    let gap: u16 = 1;
    let widths: Vec<u16> = labels.iter().map(|s| s.chars().count() as u16).collect();
    let total: u16 = widths.iter().sum::<u16>() + gap * (buttons.len() as u16 - 1);
    let mut cx = area.x + area.width.saturating_sub(total + 1); // 1-col right margin
    cx = cx.max(area.x);
    for (i, ((_, action), w)) in buttons.iter().zip(widths.iter()).enumerate() {
        let r = Rect {
            x: cx,
            y: area.y,
            width: *w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(labels[i].clone(), button_style(*action))),
            r,
        );
        app.gate_buttons.push((r, *action));
        cx = cx.saturating_add(*w + gap);
    }
}

fn draw_action(
    f: &mut Frame,
    area: Rect,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    app: &mut App,
) {
    let (text, fg, bg) = action_content(projects, runs, full, app);
    let buttons = gate_buttons_for(full);
    // Buttons carry the action, so keep the prompt short and let them sit on the right.
    let text = if buttons.is_empty() {
        text
    } else {
        format!(
            "  {}  ",
            full.map(|s| phase_label(s.phase)).unwrap_or_default()
        )
    };
    f.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(fg).bg(bg).bold()))
            .style(Style::default().bg(bg)),
        area,
    );
    render_gate_buttons(f, area, app, &buttons);
}

/// Narrow-mode one-row status bar: header essence (project · run · phase) merged
/// with the action banner. Turns the whole row into the alert color on a gate.
fn draw_status_bar(
    f: &mut Frame,
    area: Rect,
    swarm: &SparPaths,
    full: Option<&RunState>,
    app: &mut App,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
) {
    let (act_text, act_fg, act_bg) = action_content(projects, runs, full, app);
    let buttons = gate_buttons_for(full);
    let alert = act_bg != BG_RAISED;
    let bg = if alert { act_bg } else { BG_RAISED };

    let project = swarm
        .project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".");
    let (run, phase, phase_col, active) = match full {
        Some(st) => (
            st.id.as_str(),
            phase_label(st.phase),
            phase_color(st.phase),
            is_active_phase(st.phase),
        ),
        None => ("—", "No run".into(), FG_DIM, false),
    };

    let mut spans = Vec::new();
    if alert {
        // The whole bar is the alert; lead with the run id, then the cue. When
        // tappable buttons are shown they carry the action, so keep the cue short.
        spans.push(Span::styled(
            format!(" {run}  "),
            Style::default().fg(BG).bg(bg).bold(),
        ));
        let cue = if buttons.is_empty() {
            act_text.trim().to_string()
        } else {
            phase.clone()
        };
        spans.push(Span::styled(cue, Style::default().fg(act_fg).bg(bg).bold()));
    } else {
        spans.push(Span::styled(
            format!(" {} ", truncate(project, 12)),
            Style::default().fg(FG).bg(bg).bold(),
        ));
        spans.push(Span::styled(
            format!("· {run} "),
            Style::default().fg(CYAN).bg(bg),
        ));
        if active {
            spans.push(Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(phase_col).bg(bg),
            ));
        }
        spans.push(Span::styled(
            phase,
            Style::default().fg(phase_col).bg(bg).bold(),
        ));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        area,
    );
    render_gate_buttons(f, area, app, &buttons);
}

fn draw_rail(
    f: &mut Frame,
    area: Rect,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    app: &App,
    state: &mut ListState,
) {
    let focused = app.focus == Focus::Runs;
    match app.browse {
        BrowseLevel::Projects => {
            let items: Vec<ListItem> = if projects.is_empty() {
                vec![ListItem::new(Span::styled(
                    "  (no projects)",
                    Style::default().fg(FG_MUTED).italic(),
                ))]
            } else {
                projects
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let sel = i == app.selected_project;
                        let mark = if sel { "›" } else { " " };
                        let name = p.name.as_deref().unwrap_or("·");
                        let n = registry::list_project_runs(&p.root)
                            .map(|r| r.len())
                            .unwrap_or(0);
                        let line = Line::from(vec![
                            Span::styled(mark.to_string(), Style::default().fg(ACCENT)),
                            Span::styled(
                                format!(" {:<14}", truncate(name, 14)),
                                Style::default()
                                    .fg(if sel { FG } else { CYAN })
                                    .add_modifier(if sel {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ),
                            Span::styled(format!(" {n} runs "), Style::default().fg(FG_MUTED)),
                            Span::styled(relative_age(p.last_seen), Style::default().fg(FG_MUTED)),
                        ]);
                        ListItem::new(line).style(if sel {
                            Style::default().bg(BG_RAISED)
                        } else {
                            Style::default()
                        })
                    })
                    .collect()
            };
            let title = if focused {
                format!(" Projects  ({}) · Enter open · j/k ", projects.len())
            } else {
                format!(" Projects  ({}) ", projects.len())
            };
            let list = List::new(items).block(panel(&title, focused));
            f.render_stateful_widget(list, area, state);
        }
        BrowseLevel::Runs => {
            let items: Vec<ListItem> = if runs.is_empty() {
                vec![ListItem::new(Span::styled(
                    "  (no runs)",
                    Style::default().fg(FG_MUTED).italic(),
                ))]
            } else {
                runs.iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let sel = i == app.selected_run;
                        let mark = if sel { "›" } else { " " };
                        let dry = if r.dry_run { "dry " } else { "" };
                        let task = r
                            .task
                            .as_deref()
                            .map(|t| truncate(t, 16))
                            .unwrap_or_else(|| workflow_label(r.workflow).into());
                        let age = relative_age(r.updated_at);
                        let line = Line::from(vec![
                            Span::styled(mark.to_string(), Style::default().fg(ACCENT)),
                            Span::styled(
                                format!(" {:<8}", truncate(&r.id, 8)),
                                Style::default()
                                    .fg(if sel { FG } else { FG_DIM })
                                    .add_modifier(if sel {
                                        Modifier::BOLD
                                    } else {
                                        Modifier::empty()
                                    }),
                            ),
                            Span::styled(
                                format!(" {:<12}", truncate(&phase_label(r.phase), 12)),
                                Style::default().fg(phase_color(r.phase)),
                            ),
                            Span::styled(format!(" {dry}"), Style::default().fg(YELLOW)),
                            Span::styled(format!("{task} "), Style::default().fg(FG_MUTED)),
                            Span::styled(age, Style::default().fg(FG_MUTED)),
                        ]);
                        ListItem::new(line).style(if sel {
                            Style::default().bg(BG_RAISED)
                        } else {
                            Style::default()
                        })
                    })
                    .collect()
            };
            let title = if focused {
                format!(" Runs  ({}) · Esc/p projects · j/k ", runs.len())
            } else {
                format!(" Runs  ({}) ", runs.len())
            };
            let list = List::new(items).block(panel(&title, focused));
            f.render_stateful_widget(list, area, state);
        }
    }
}

fn draw_agents(
    f: &mut Frame,
    area: Rect,
    full: Option<&RunState>,
    app: &App,
    state: &mut ListState,
) {
    let focused = app.focus == Focus::Agents;
    let items: Vec<ListItem> = match full {
        None => vec![ListItem::new(Span::styled(
            "  select a run first",
            Style::default().fg(FG_MUTED).italic(),
        ))],
        Some(st) if st.slots.is_empty() => vec![ListItem::new(Span::styled(
            "  no agents yet",
            Style::default().fg(FG_MUTED).italic(),
        ))],
        Some(st) => st
            .slots
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let sel = i == app.selected_slot;
                let icon = slot_icon(s, app);
                let role = role_label(s.role);
                let status = slot_status_label(s.status);
                let act = SlotActivity::observe(s, app.stall_warn_secs);
                let (tail, tail_c) = if s.status == SlotStatus::Running {
                    if act.stalled {
                        (format!(" STALL {}", act.human_silent()), RED)
                    } else {
                        (format!(" quiet {}", act.human_silent()), FG_MUTED)
                    }
                } else {
                    (String::new(), FG_MUTED)
                };
                let color = if act.stalled { RED } else { slot_color(s) };
                let line = Line::from(vec![
                    Span::styled(format!(" {icon} "), Style::default().fg(color)),
                    Span::styled(
                        format!("{:<10}", truncate(&s.id, 10)),
                        Style::default()
                            .fg(if sel { FG } else { FG_DIM })
                            .add_modifier(if sel {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(format!(" {role:<8}"), Style::default().fg(ACCENT_SOFT)),
                    Span::styled(format!(" {status}"), Style::default().fg(color)),
                    Span::styled(
                        format!(" {}", truncate(&s.provider, 10)),
                        Style::default().fg(FG_MUTED),
                    ),
                    Span::styled(tail, Style::default().fg(tail_c)),
                ]);
                ListItem::new(line).style(if sel {
                    Style::default().bg(BG_RAISED)
                } else {
                    Style::default()
                })
            })
            .collect(),
    };

    let title = if let Some(st) = full {
        let n = st.slots.len();
        let running = st
            .slots
            .iter()
            .filter(|s| s.status == SlotStatus::Running)
            .count();
        let stalled = st
            .slots
            .iter()
            .filter(|s| SlotActivity::observe(s, app.stall_warn_secs).stalled)
            .count();
        if focused {
            if stalled > 0 {
                format!(" Agents  {running}/{n} live · {stalled} quiet too long  · j/k ")
            } else {
                format!(" Agents  {running}/{n} live  · j/k or click ")
            }
        } else if stalled > 0 {
            format!(" Agents  {running}/{n} live · {stalled} stall ")
        } else {
            format!(" Agents  {running}/{n} live ")
        }
    } else if focused {
        " Agents  · select a run first ".into()
    } else {
        " Agents ".into()
    };

    let list = List::new(items).block(panel(&title, focused));
    f.render_stateful_widget(list, area, state);

    if let Some(st) = full {
        if !st.slots.is_empty() && area.height > 6 {
            let done = st
                .slots
                .iter()
                .filter(|s| s.status == SlotStatus::Done)
                .count() as f64;
            let ratio = done / st.slots.len() as f64;
            let gauge_area = Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(2),
                width: area.width.saturating_sub(2),
                height: 1,
            };
            let g = Gauge::default()
                .gauge_style(Style::default().fg(ACCENT).bg(BG_PANEL))
                .ratio(ratio)
                .label(format!("{:.0}% done", ratio * 100.0));
            f.render_widget(g, gauge_area);
        }
    }
}

fn draw_stream(
    f: &mut Frame,
    area: Rect,
    full: Option<&RunState>,
    stream_text: &str,
    app: &mut App,
) {
    let focused = app.focus == Focus::Log;
    let slot = full.and_then(|st| st.slots.get(app.selected_slot));
    let slot_id = slot.map(|s| s.id.as_str()).unwrap_or("—");
    let silent_hint = slot
        .map(|s| {
            let act = SlotActivity::observe(s, app.stall_warn_secs);
            if act.stalled {
                format!(" · STALL {}", act.human_silent())
            } else if s.status == SlotStatus::Running {
                format!(" · quiet {}", act.human_silent())
            } else {
                String::new()
            }
        })
        .unwrap_or_default();
    let mode = if app.log_expand { "wrap" } else { "trim" };
    let follow = if app.stream_follow { " · live" } else { "" };
    let title = if focused {
        format!(" Live log  · {slot_id}{silent_hint}  · {mode}{follow} · w · scroll ")
    } else {
        format!(" Live log  · {slot_id}{silent_hint}{follow} ")
    };

    let block = panel(&title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let stats = slot.and_then(|s| {
        s.log_path
            .as_ref()
            .and_then(|p| process::StreamStats::load(p))
            .or_else(|| {
                s.usage.as_ref().map(|u| process::StreamStats {
                    tools: u.tools,
                    tool_errors: 0,
                    input_tokens: u.input_tokens,
                    output_tokens: u.output_tokens,
                    cache_read_tokens: u.cache_read_tokens,
                    cache_write_tokens: 0,
                    context_tokens: u.context_tokens,
                    model: u.model.clone(),
                    lines_in: 0,
                    chars_out: 0,
                    last_log_at: None,
                })
            })
    });
    draw_stream_stats(f, chunks[0], stats.as_ref(), slot.map(|s| s.status));

    app.stream_view_h = chunks[1].height;
    app.stream_max = render_scrollable_log(
        f,
        chunks[1],
        stream_text,
        &mut app.stream_scroll,
        &mut app.stream_follow,
        true,
        app.log_expand,
    );
}

fn draw_stream_stats(
    f: &mut Frame,
    area: Rect,
    stats: Option<&process::StreamStats>,
    status: Option<SlotStatus>,
) {
    let Some(s) = stats else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "  waiting for agent output…",
                Style::default().fg(FG_MUTED).bg(BG_RAISED),
            )),
            area,
        );
        return;
    };
    let ctx = s.context_tokens;
    let ctx_color = if ctx > 150_000 {
        RED
    } else if ctx > 80_000 {
        YELLOW
    } else if ctx > 0 {
        GREEN
    } else {
        FG_MUTED
    };
    let tools_color = if s.tool_errors > 0 {
        RED
    } else if s.tools > 0 {
        CYAN
    } else {
        FG_MUTED
    };
    let status_span = match status {
        Some(SlotStatus::Running) => {
            Span::styled(" LIVE ", Style::default().fg(BG).bg(CYAN).bold())
        }
        Some(SlotStatus::Done) => Span::styled(" DONE ", Style::default().fg(BG).bg(GREEN).bold()),
        Some(SlotStatus::Failed) => Span::styled(" FAIL ", Style::default().fg(BG).bg(RED).bold()),
        _ => Span::styled(" … ", Style::default().fg(FG_MUTED).bg(BG_RAISED)),
    };
    let line = Line::from(vec![
        status_span,
        Span::raw(" "),
        Span::styled(
            format!(" context {} ", compact_u64(ctx)),
            Style::default().fg(ctx_color).bg(BG_RAISED).bold(),
        ),
        Span::raw(" "),
        Span::styled(
            format!(" {} tools ", s.tools),
            Style::default().fg(tools_color).bg(BG_RAISED),
        ),
        Span::raw(" "),
        Span::styled(
            format!(" in {} ", compact_u64(s.input_tokens)),
            Style::default().fg(ACCENT).bg(BG_RAISED),
        ),
        Span::styled(
            format!(" out {} ", compact_u64(s.output_tokens)),
            Style::default().fg(MAGENTA).bg(BG_RAISED),
        ),
        if s.cache_read_tokens > 0 {
            Span::styled(
                format!(" cache {} ", compact_u64(s.cache_read_tokens)),
                Style::default().fg(YELLOW).bg(BG_RAISED),
            )
        } else {
            Span::raw("")
        },
        Span::raw(" "),
        Span::styled(
            s.model.as_deref().unwrap_or(""),
            Style::default().fg(FG_MUTED).bg(BG_RAISED),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(BG_RAISED)),
        area,
    );
}

/// Paint a log viewport by writing cells directly (no Paragraph wrap/scroll).
/// Clamps `scroll` into range and pins to bottom when `follow` is set.
/// Returns the max valid scroll offset for this paint.
fn render_scrollable_log(
    f: &mut Frame,
    area: Rect,
    text: &str,
    scroll: &mut u16,
    follow: &mut bool,
    colorize: bool,
    expand: bool,
) -> u16 {
    if area.width == 0 || area.height == 0 {
        clamp_scroll(scroll, follow, 0);
        return 0;
    }

    let sb_w = 1u16;
    let text_w = area.width.saturating_sub(sb_w).max(1) as usize;
    let height = area.height as usize;
    let total = log_row_count(text, text_w, expand).max(1);
    // Cap at u16::MAX so dense tails cannot wrap the scroll type.
    let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    clamp_scroll(scroll, follow, max_scroll);
    let start = *scroll as usize;
    // Materialise only the rows we are about to paint, not the whole tail.
    let visible = log_rows_window(text, text_w, colorize, expand, start, height);

    let text_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(sb_w).max(1),
        height: area.height,
    };
    f.render_widget(Clear, text_area);
    f.render_widget(
        CellLog {
            lines: visible,
            fill: Style::default().bg(BG_PANEL).fg(FG),
        },
        text_area,
    );

    // Map our tail-scroll model (position in [0, max_scroll], last screenful
    // pinned to the bottom) onto ratatui's scrollbar, whose thumb only reaches
    // the track bottom when position == content_length - 1. content_length is
    // the number of scroll positions, not content rows, so the thumb lands flush
    // at the bottom when start == max_scroll and its length stays height/total.
    let mut sb = ScrollbarState::new(max_scroll as usize + 1)
        .position(start)
        .viewport_content_length(height);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(FG_MUTED).bg(BG_PANEL))
            .thumb_style(Style::default().fg(ACCENT_SOFT).bg(BG_PANEL)),
        area,
        &mut sb,
    );
    max_scroll
}

/// Fills every cell, then paints plain strings — no span leftovers across frames.
struct CellLog {
    lines: Vec<(String, Style)>,
    fill: Style,
}

impl Widget for CellLog {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_symbol(" ");
                    cell.set_style(self.fill);
                    cell.set_skip(false);
                }
            }
        }
        for (i, (text, style)) in self.lines.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let y = area.top() + i as u16;
            let mut col = 0u16;
            for ch in text.chars() {
                if col >= area.width {
                    break;
                }
                let x = area.left() + col;
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_style(*style);
                    cell.set_skip(false);
                }
                col = col.saturating_add(1);
            }
        }
    }
}

fn log_line_style(line: &str, colorize: bool) -> Style {
    let base = Style::default().bg(BG_PANEL);
    if !colorize {
        return base.fg(FG_DIM);
    }
    let t = line.trim_start();
    if t.starts_with('▸') || t.starts_with('→') {
        base.fg(CYAN)
    } else if t.starts_with('◂') || t.starts_with('←') {
        if t.contains('✗') || t.contains("err") {
            base.fg(RED)
        } else {
            base.fg(GREEN)
        }
    } else if t.starts_with('·') || t.starts_with('…') || t.starts_with('│') {
        base.fg(FG_MUTED).italic()
    } else if t.starts_with('!') {
        base.fg(RED).bold()
    } else if t.starts_with('#') {
        base.fg(FG_MUTED)
    } else {
        base.fg(FG)
    }
}

/// Rows the log occupies, without building any of them. In trim mode this is
/// just the line count; wrapping has to measure each line. Matches
/// `log_rows_window`'s empty-output fallback so the two always agree.
fn log_row_count(text: &str, width: usize, expand: bool) -> usize {
    let width = width.max(1);
    let n: usize = if !expand {
        text.lines().count()
    } else {
        text.lines()
            .map(|raw| {
                let line = compact_log_line(raw);
                if line.is_empty() {
                    1
                } else {
                    soft_wrap(&line, width).len()
                }
            })
            .sum()
    };
    // Empty text still renders one blank row (see log_rows_window fallback).
    n.max(1)
}

/// Build only the rows in `[start, start + height)`.
fn log_rows_window(
    text: &str,
    width: usize,
    colorize: bool,
    expand: bool,
    start: usize,
    height: usize,
) -> Vec<(String, Style)> {
    let width = width.max(1);
    let end = start.saturating_add(height);
    let mut out = Vec::new();
    let mut row = 0usize;
    for raw in text.lines() {
        if row >= end {
            break;
        }
        let line = compact_log_line(raw);
        let style = log_line_style(&line, colorize);
        if line.is_empty() {
            if row >= start {
                out.push((String::new(), style));
            }
            row += 1;
            continue;
        }
        if expand {
            for chunk in soft_wrap(&line, width) {
                if row >= end {
                    break;
                }
                if row >= start {
                    out.push((chunk, style));
                }
                row += 1;
            }
        } else {
            if row >= start {
                out.push((truncate_display(&line, width), style));
            }
            row += 1;
        }
    }
    if out.is_empty() && start == 0 {
        out.push((String::new(), log_line_style("", colorize)));
    }
    out
}

#[cfg(test)]
mod window_eq {
    use super::*;
    fn old_full(text: &str, width: usize, colorize: bool, expand: bool) -> Vec<(String, Style)> {
        let width = width.max(1);
        let mut out = Vec::new();
        for raw in text.lines() {
            let line = compact_log_line(raw);
            let style = log_line_style(&line, colorize);
            if line.is_empty() {
                out.push((String::new(), style));
                continue;
            }
            if expand {
                for chunk in soft_wrap(&line, width) {
                    out.push((chunk, style));
                }
            } else {
                out.push((truncate_display(&line, width), style));
            }
        }
        if out.is_empty() {
            out.push((String::new(), log_line_style("", colorize)));
        }
        out
    }
    #[test]
    fn windows_match_full_layout() {
        let cases = [
            "", "\n", "\n\n\n", "one line",
            "→ tool call\n← result ok\n· thinking about a very long line that definitely exceeds any reasonable terminal width and must wrap or truncate depending on mode yes indeed\n\n! error here\n# comment",
            &"word ".repeat(200),
        ];
        for text in cases {
            for &w in &[1usize, 5, 20, 80, 200] {
                for &exp in &[false, true] {
                    for &col in &[false, true] {
                        let full = old_full(text, w, col, exp);
                        let total_fn = log_row_count(text, w, exp);
                        assert_eq!(
                            full.len(),
                            total_fn,
                            "row count mismatch text={:?} w={} exp={}",
                            text,
                            w,
                            exp
                        );
                        for &(start, height) in &[
                            (0usize, 1usize),
                            (0, 3),
                            (1, 2),
                            (2, 5),
                            (5, 10),
                            (0, 1000),
                            (full.len(), 3),
                            (full.len().saturating_sub(1), 2),
                        ] {
                            let win = log_rows_window(text, w, col, exp, start, height);
                            let expected: Vec<_> =
                                full.iter().skip(start).take(height).cloned().collect();
                            // old fallback: when full has the single empty row and we skip past it, old yields []
                            assert_eq!(
                                win.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
                                expected.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
                                "window text mismatch text={:?} w={} exp={} start={} h={}",
                                text,
                                w,
                                exp,
                                start,
                                height
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
fn layout_log_rows(text: &str, width: usize, colorize: bool, expand: bool) -> Vec<(String, Style)> {
    log_rows_window(text, width, colorize, expand, 0, usize::MAX)
}

fn compact_log_line(raw: &str) -> String {
    let s = raw.trim_end();
    if s.is_empty() {
        return String::new();
    }
    // Tool call / result markers from stream coalescer
    if let Some(rest) = s.strip_prefix('→') {
        let rest = rest.trim();
        // "Bash  Fetch PR diff" → keep short tool + summary
        return format!("▸ {}", collapse_ws(rest));
    }
    if let Some(rest) = s.strip_prefix('←') {
        let rest = rest.trim();
        return format!("◂ {}", collapse_ws(rest));
    }
    if let Some(rest) = s.strip_prefix('·') {
        return format!("  {}", collapse_ws(rest.trim()));
    }
    if s.starts_with('…') {
        return format!("  {}", collapse_ws(s.trim_start_matches('…').trim()));
    }
    collapse_ws(s)
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn truncate_display(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let n = s.chars().count();
    if n <= width {
        return s.to_string();
    }
    if width == 1 {
        return "…".into();
    }
    let keep: String = s.chars().take(width - 1).collect();
    format!("{keep}…")
}

fn soft_wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if word.chars().count() > width {
            if !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = word.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let end = (i + width).min(chars.len());
                rows.push(chars[i..end].iter().collect());
                i = end;
            }
            continue;
        }
        let next_len = if cur.is_empty() {
            word.chars().count()
        } else {
            cur.chars().count() + 1 + word.chars().count()
        };
        if next_len > width && !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || rows.is_empty() {
        rows.push(cur);
    }
    rows
}

fn compact_u64(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn draw_activity(f: &mut Frame, area: Rect, activity: &[String], app: &mut App) {
    let focused = app.focus == Focus::Activity;
    let text = if activity.is_empty() {
        "No activity yet.\n\nRun timeline: phases,\nagents, gates, bus.".into()
    } else {
        activity.join("\n")
    };
    let title = if focused {
        " Activity  · timeline · j/k "
    } else {
        " Activity "
    };
    let block = panel(title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.bus_view_h = inner.height;
    app.bus_max = render_scrollable_log(
        f,
        inner,
        &text,
        &mut app.bus_scroll,
        &mut app.bus_follow,
        false,
        true,
    );
}

fn draw_composer(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Composer;
    let cursor_blink = if focused && (app.tick / 6).is_multiple_of(2) {
        "▌"
    } else if focused {
        " "
    } else {
        ""
    };
    let prompt = if focused {
        Span::styled(" › ", Style::default().fg(ACCENT).bold())
    } else {
        Span::styled("   ", Style::default().fg(FG_MUTED))
    };
    let line = Line::from(vec![
        prompt,
        Span::styled(&app.composer, Style::default().fg(FG)),
        Span::styled(cursor_blink, Style::default().fg(ACCENT)),
    ]);
    let hint = if app.composer.is_empty() {
        "  Type a command:  /approve  /reject  /ship  /help   ·  click here or press i /"
    } else {
        ""
    };
    let title = if focused {
        " Command  · Enter run · Esc leave "
    } else {
        " Command  · i or / to type "
    };
    let p = Paragraph::new(vec![
        line,
        Line::from(Span::styled(hint, Style::default().fg(FG_MUTED).italic())),
    ])
    .block(panel(title, focused));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &mut App, full: Option<&RunState>) {
    app.rect_help = Rect::default();
    app.rect_projects = Rect::default();

    let (msg, color) = if let Some((_, m, c, _)) = &app.flash {
        (m.as_str(), *c)
    } else if !app.status_line.is_empty() {
        (app.status_line.as_str(), YELLOW)
    } else {
        (situational_footer(full, app.focus, app.browse), FG_MUTED)
    };

    let gate = full.map(|s| s.phase.is_gate()).unwrap_or(false);
    let bg = if gate {
        Color::Rgb(40, 30, 12)
    } else {
        BG_RAISED
    };

    if gate {
        // At a gate the tappable buttons live on the status/action bar above.
        let left_text = format!(" {msg} ");
        let right_text = "  YOUR MOVE  ";
        let left = Span::styled(left_text.clone(), Style::default().fg(color).bg(bg));
        let right = Span::styled(right_text, Style::default().fg(BG).bg(YELLOW).bold());
        let used = (left_text.chars().count() + right_text.chars().count()) as u16;
        let pad = area.width.saturating_sub(used).max(1) as usize;
        let line = Line::from(vec![
            left,
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            right,
        ]);
        f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
        return;
    }

    // Right cluster: spinner + tappable Projects/Help tokens + exit hint.
    let sp = format!(" {} ", app.spinner());
    let proj = " Projects ";
    let help = " Help ";
    let tail = " Ctrl+C×2 exit ";
    // Display columns, not bytes — the × in `tail` is 2 bytes but 1 column.
    let right_w =
        (sp.chars().count() + proj.chars().count() + help.chars().count() + tail.chars().count())
            as u16;

    // Keep the tokens on screen by truncating the left hint to what's left.
    let avail_left = area.width.saturating_sub(right_w + 1).max(1) as usize;
    let left_text = truncate(&format!(" {msg} "), avail_left);
    let used = left_text.chars().count() as u16 + right_w;
    let pad = area.width.saturating_sub(used).max(1) as usize;

    let tok_x = area.x + area.width.saturating_sub(right_w);
    let proj_x = tok_x + sp.chars().count() as u16;
    let help_x = proj_x + proj.chars().count() as u16;
    app.rect_projects = Rect {
        x: proj_x,
        y: area.y,
        width: proj.chars().count() as u16,
        height: 1,
    };
    app.rect_help = Rect {
        x: help_x,
        y: area.y,
        width: help.chars().count() as u16,
        height: 1,
    };

    let tok = Style::default().fg(BG).bg(ACCENT_SOFT).bold();
    let line = Line::from(vec![
        Span::styled(left_text, Style::default().fg(color).bg(bg)),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        Span::styled(sp, Style::default().fg(FG_MUTED).bg(bg)),
        Span::styled(proj, tok),
        Span::styled(help, tok),
        Span::styled(tail, Style::default().fg(FG_MUTED).bg(bg)),
    ]);
    f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
}

fn situational_footer(full: Option<&RunState>, focus: Focus, browse: BrowseLevel) -> &'static str {
    if browse == BrowseLevel::Projects {
        return "j/k projects · Enter open · double-click open · ? help";
    }
    if let Some(st) = full {
        if st.phase == Phase::AwaitingPlanApproval {
            return "a approve · r reject · p projects · Tab panes";
        }
        if st.phase == Phase::AwaitingShipConfirm {
            return "s confirm ship · p projects · ? help";
        }
        if st.phase == Phase::AwaitingWinnerConfirm {
            return "tap Confirm / Reconcile above · p projects";
        }
        if st.phase == Phase::AwaitingReconcile {
            return "tap Reconcile above · watch log · p projects";
        }
        if st.phase.is_gate() {
            return "gate waiting · yellow bar above · p projects";
        }
    }
    match focus {
        Focus::Runs => "j/k runs · Esc/p projects · Tab → Agents · ? help",
        Focus::Agents => "j/k agent · log follows · Tab → Live log",
        Focus::Log => "scroll · w wrap · g/G top/end · up unfollows live · Tab",
        Focus::Activity => "run timeline · scroll · Tab → Terminal",
        Focus::Terminal => "live agent pane · Tab → Command",
        Focus::Composer => "type /approve /reject /ship /help · Enter · Esc",
    }
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let w = area.width.clamp(40, 72);
    let h = area.height.clamp(14, 32);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let body = "\
 spar — keyboard & mouse\n\
 \n\
  Mouse / touch\n\
    tap panel / tab      focus it (border lights up)\n\
    tap a run/agent      select that row\n\
    tap a gate button    Approve/Reject/Ship/Confirm/Reconcile\n\
    tap Projects / Help  footer shortcuts · tap anywhere closes help\n\
    scroll / swipe       scroll log/messages · step runs/agents\n\
 \n\
  Keyboard\n\
    Tab / Shift-Tab      next / previous panel\n\
    j k  or  ↑ ↓         move in focused panel\n\
    Enter                open project (from Projects list)\n\
    p  or  Esc           back to Projects (general view)\n\
    [ ]  or  J K         previous / next item\n\
    1-9                  jump to agent slot\n\
    a / r / s            approve · reject · ship (when gated)\n\
    i  or  /             command bar\n\
    w                    log wrap ↔ truncate long lines\n\
    g / G                log top / bottom\n\
    ?                    this help · Esc closes help\n\
    Ctrl+C twice         exit (only quit path)\n\
 \n\
  Default: this project's runs (or Projects if outside a repo).\n\
  Activity: phase timeline + agent status (not chat).\n\
  Runs: <project>/.spar/runs/ · Index: ~/.spar/registry.json\n\
 \n\
  Esc, ?, or tap to close help";
    let p = Paragraph::new(body)
        .style(Style::default().fg(FG).bg(BG_RAISED))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER_FOCUS))
                .title(Span::styled(" Help ", Style::default().fg(ACCENT).bold()))
                .style(Style::default().bg(BG_RAISED)),
        );
    f.render_widget(p, rect);
}

/// Rows/cols available inside the terminal panel's border, falling back to a
/// standard 80x24 when the panel hasn't been laid out yet.
fn terminal_dims(rect: Rect) -> (u16, u16) {
    let rows = rect.height.saturating_sub(2);
    let cols = rect.width.saturating_sub(2);
    (
        if rows == 0 { 24 } else { rows },
        if cols == 0 { 80 } else { cols },
    )
}

/// Lifecycle for the embedded terminal: attach to the selected run's tmux pane on
/// the spar socket when the Terminal panel is focused, drop a stale attachment,
/// and pump live output into the vt100 buffer every frame.
fn manage_terminal(app: &mut App, full: Option<&RunState>) {
    // Nothing to do until the panel is opened; avoids forking `tmux has-session`
    // every frame while the operator is on another tab.
    if app.focus != Focus::Terminal && app.terminal_pane.is_none() {
        return;
    }
    let session = full.and_then(|st| {
        let name = st
            .tmux_session
            .clone()
            .unwrap_or_else(|| tmux::session_name(&st.id));
        (tmux::available() && tmux::has_session(&name)).then_some(name)
    });

    // Selection moved to a different (or no) session — release the old client.
    if let Some(pane) = app.terminal_pane.as_ref() {
        if pane.session() != session.as_deref() {
            app.terminal_pane = None;
        }
    }

    // Attach lazily, only while the panel is focused, to avoid spawning a control
    // client for runs the operator never looks at.
    if app.focus == Focus::Terminal && app.terminal_pane.is_none() {
        if let Some(name) = &session {
            let (rows, cols) = terminal_dims(app.rect_terminal);
            let mut pane = crate::terminal::TerminalPane::new(rows, cols);
            if pane.attach(name).is_ok() {
                app.terminal_pane = Some(pane);
            }
        }
    }

    if let Some(pane) = app.terminal_pane.as_mut() {
        pane.pump();
    }
}

fn draw_terminal(f: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::Terminal;
    let title = match app.terminal_pane.as_ref() {
        Some(pane) => {
            let (rows, cols) = pane.dims();
            format!(" Terminal · live agent pane · {cols}x{rows} ")
        }
        None => " Terminal · no live tmux pane ".to_string(),
    };
    let block = panel(&title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(pane) = app.terminal_pane.as_mut() else {
        let hint = Paragraph::new(
            "No live tmux session for the selected run.\n\nRuns spawned on the spar socket show their agent pane here — \
             switch to a run with a live session, or spawn one, to attach.",
        )
        .style(Style::default().fg(FG_DIM))
        .wrap(Wrap { trim: true });
        f.render_widget(hint, inner);
        return;
    };

    // Keep the vt100 buffer (and the tmux pane) matched to the visible area.
    pane.resize(inner.height, inner.width);
    let term = PseudoTerminal::new(pane.screen());
    f.render_widget(term, inner);
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    let border = if focused { BORDER_FOCUS } else { BORDER };
    let title_style = if focused {
        Style::default().fg(ACCENT).bold()
    } else {
        Style::default().fg(FG_DIM)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(title.to_string(), title_style))
        .style(Style::default().bg(BG_PANEL))
}

fn handle_composer(
    swarm: &SparPaths,
    runs: &[state::RunSummary],
    selected: usize,
    line: &str,
    bg: Option<mpsc::Sender<Msg>>,
) -> Result<String> {
    let run_id = runs.get(selected).map(|r| r.id.as_str());
    let cmd = line.trim();
    if let Some(rest) = cmd.strip_prefix('/') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("").to_ascii_lowercase();
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
        return match head.as_str() {
            "q" | "quit" => Ok("Use Ctrl+C twice to exit (q does not quit)".into()),
            "help" | "h" | "?" => Ok(
                "Commands: /approve /reject [reason] /ship · press ? for full help · Ctrl+C×2 exit"
                    .into(),
            ),
            "approve" => {
                let id = arg
                    .or(run_id)
                    .ok_or_else(|| anyhow::anyhow!("no run selected"))?;
                workflow::plan::approve(swarm, id, false)?;
                Ok(format!("Approved plan {id}"))
            }
            "reject" => {
                let id = run_id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
                workflow::plan::reject(swarm, id, arg.map(|s| s.to_string()), false)?;
                Ok(format!("Rejected plan {id}"))
            }
            "ship" => {
                let id = arg
                    .or(run_id)
                    .ok_or_else(|| anyhow::anyhow!("no run selected"))?;
                crate::ship::confirm_ship(swarm, id, false)?;
                Ok(format!("Ship confirmed {id}"))
            }
            "spawn" => spawn_agent_command(runs, selected, arg, bg),
            other => Ok(format!("Unknown /{other} — try /help")),
        };
    }
    if let Some(rest) = cmd.strip_prefix('@') {
        return send_mention(swarm, run_id, rest);
    }
    Ok(format!("Noted (chat later): {}", truncate(cmd, 48)))
}

/// Composer `@<agent> <message>` — send a directed bus chat from the human to a slot or
/// bare agent, resolving the mention to its unique bus id via [`resolve_mention`].
fn send_mention(swarm: &SparPaths, run_id: Option<&str>, rest: &str) -> Result<String> {
    let mut it = rest.splitn(2, char::is_whitespace);
    let target = it.next().unwrap_or("").trim();
    let body = it.next().map(str::trim).unwrap_or("");
    if target.is_empty() || body.is_empty() {
        anyhow::bail!("usage: @<agent> <message>");
    }
    let to = resolve_mention(swarm, run_id, target)?;
    // Tag the message with the target's run scope (a run slot, or a reserved sink for the
    // selected run) so it shows in that run's bus view; delivery keys on the unique id,
    // not the tag.
    let tag = if crate::bus::is_reserved_sink(&to) {
        run_id
    } else {
        run_id.filter(|r| to.starts_with(&format!("{r}:")))
    };
    crate::bus::chat(
        swarm,
        tag,
        "human",
        &to,
        body,
        crate::bus::MessageBudget::Normal,
    )?;
    Ok(format!("sent to {to}"))
}

/// Resolve a composer mention to a unique bus id. An already-qualified id (`run:slot`)
/// or reserved sink (`broadcast`/`@human`) passes through. A short id resolves against
/// the workspace roster: the selected run's slot (`run:slot`) and any bare agent of that
/// id are candidates — exactly one resolves, several error (listing them), and none
/// falls back to the selected run's slot (or the bare id as typed).
fn resolve_mention(swarm: &SparPaths, run_id: Option<&str>, target: &str) -> Result<String> {
    if target.contains(':') || crate::bus::is_reserved_sink(target) {
        return Ok(target.to_string());
    }
    let qualified = run_id.map(|r| crate::bus::agent_ref(Some(r), target));
    let mut candidates: Vec<String> = crate::bus::list_presence(swarm, None)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.agent)
        .filter(|a| Some(a.as_str()) == qualified.as_deref() || a == target)
        .collect();
    candidates.sort();
    candidates.dedup();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Ok(qualified.unwrap_or_else(|| target.to_string())),
        _ => anyhow::bail!(
            "ambiguous mention @{target}: candidates {}",
            candidates.join(", ")
        ),
    }
}

/// How long to let a freshly launched CLI paint its input box before typing the
/// prompt. Generous: a cold CLI start can take a few seconds, and delivering early
/// drops the prompt into an unbooted TUI.
const SPAWN_READY_TIMEOUT: Duration = Duration::from_secs(12);

/// `/spawn <cli:provider> <prompt>` — launch a fresh agent into a pane on the spar
/// tmux socket, joined to the selected run's bus, and hand it the prompt. The whole
/// spawn → prompt loop runs without leaving spar (Stage 11 / A4).
///
/// Two correctness guards live here:
///  - The poke agent gets its **own worktree**, never the primary checkout: a
///    FullAuto agent must not run in the primary tree, and presence hooks refuse to
///    install there (`same_dir` guard), so cwd == project_root would leave the agent
///    with no working/idle signal at all.
///  - Spawn + delivery run on a **background thread** with a bounded readiness gate,
///    so the render loop never blocks and the prompt is only typed once the CLI has
///    painted its input box. The final flash reflects actual delivery, not a guess.
fn spawn_agent_command(
    runs: &[state::RunSummary],
    selected: usize,
    arg: Option<&str>,
    bg: Option<mpsc::Sender<Msg>>,
) -> Result<String> {
    let run = runs
        .get(selected)
        .ok_or_else(|| anyhow::anyhow!("select a run first — /spawn joins its bus"))?;
    let spec = arg.ok_or_else(|| anyhow::anyhow!("usage: /spawn <cli:provider> <prompt>"))?;
    let mut parts = spec.splitn(2, char::is_whitespace);
    let provider = parts.next().unwrap_or("").trim();
    let prompt = parts.next().map(str::trim).unwrap_or("");
    if provider.is_empty() || prompt.is_empty() {
        anyhow::bail!("usage: /spawn <cli:provider> <prompt>");
    }
    let project_root = run
        .project_root
        .clone()
        .ok_or_else(|| anyhow::anyhow!("run has no known project root"))?;
    let uid = uuid::Uuid::new_v4().simple().to_string();
    let agent_id = format!("poke-{}", &uid[..8]);

    // Give the agent its own worktree (never the primary checkout) so presence hooks
    // install and it can run FullAuto safely. Done on this thread so a git failure
    // surfaces synchronously as a composer error rather than a silent background drop.
    let record = crate::worktree::create_worktree(&project_root, &run.id, &agent_id)?;

    let paths = SparPaths::new(&project_root);
    let run_id = run.id.clone();
    let provider_s = provider.to_string();
    let prompt_s = prompt.to_string();
    let cwd = record.path;
    let label = format!("{agent_id} ({provider})");
    let pending = format!("Spawning {label}… delivering prompt when the pane is ready");

    let work = move || -> Result<String> {
        let req = crate::workspace::SpawnRequest {
            paths: &paths,
            run: Some(&run_id),
            agent_id: &agent_id,
            provider: &provider_s,
            cwd: &cwd,
            project_root: &project_root,
        };
        let (session, window) = crate::workspace::spawn_agent(&req)?;
        let ready = crate::workspace::wait_pane_ready(
            &session,
            &window,
            SPAWN_READY_TIMEOUT,
            Duration::from_millis(200),
        )?;
        crate::workspace::deliver_prompt(&session, &window, &prompt_s)?;
        Ok(if ready {
            format!("Spawned {label} — prompt delivered · Terminal tab to watch")
        } else {
            format!("Spawned {label} — pane slow to boot; prompt sent, confirm in Terminal")
        })
    };

    // Real TUI path: hand the spawn+deliver to a background thread and flash the true
    // outcome when it lands. No channel (defensive/tests) → run inline.
    match bg {
        Some(tx) => {
            std::thread::spawn(move || {
                let (msg, color) = match work() {
                    Ok(m) => (m, GREEN),
                    Err(e) => (format!("spawn failed: {e:#}"), RED),
                };
                let _ = tx.send(Msg::Flash(msg, color));
            });
            Ok(pending)
        }
        None => work(),
    }
}

fn stream_content(
    swarm: &SparPaths,
    full: Option<&RunState>,
    slot_idx: usize,
    cache: &mut LogCache,
) -> String {
    let Some(st) = full else {
        cache.clear();
        return "\n  Select a run on the left.\n\n  New work:\n    spar plan -t \"describe the change\" --providers cli:claude\n".into();
    };
    if st.slots.is_empty() {
        cache.clear();
        return "\n  This run has no agents yet.".into();
    }
    let slot = &st.slots[slot_idx.min(st.slots.len() - 1)];
    let path = slot
        .log_path
        .clone()
        .unwrap_or_else(|| swarm.log_file(&st.id, &slot.id));
    if path.is_file() {
        let (raw, truncated) = cache.load(&path, LOG_TAIL_BYTES);
        let body: Vec<&str> = raw
            .lines()
            .skip_while(|l| {
                l.starts_with('#')
                    || *l == "---"
                    || l.starts_with("cwd=")
                    || l.is_empty()
                    || l.starts_with("# Role:")
            })
            // Drop the huge prompt dump often pasted as first "user" blob in headless spawn
            .filter(|l| !l.starts_with("# Role:") && !l.starts_with("## Task"))
            .collect();
        // Skip until first real stream marker if present
        let start = body
            .iter()
            .position(|l| {
                l.starts_with('→')
                    || l.starts_with('←')
                    || l.starts_with('·')
                    || l.starts_with('…')
                    || l.starts_with('!')
                    || l.starts_with("I'll ")
                    || l.starts_with("I ")
            })
            .unwrap_or(0);
        let body = body[start..].join("\n");
        if body.trim().is_empty() {
            format!(
                "\n  {} is running — waiting for stream…\n  Quiet time is on Agents; Activity shows phase timeline.",
                slot.id
            )
        } else if truncated {
            format!(
                "… earlier log truncated (showing last ~{} KB)\n{body}",
                LOG_TAIL_BYTES / 1024
            )
        } else {
            body
        }
    } else {
        cache.clear();
        format!(
            "\n  No log yet for {}\n  {} · {}",
            slot.id,
            slot.provider,
            slot_status_label(slot.status)
        )
    }
}

/// Right-rail feed: human run timeline (not a raw bus dump).
fn activity_feed(
    swarm: &SparPaths,
    full: Option<&RunState>,
    quota: &QuotaStore,
    alerts: &[crate::bus::BusMessage],
) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(st) = full else {
        lines.push("No run selected.".into());
        lines.push(String::new());
        lines.push("Open a project, pick a run.".into());
        return lines;
    };

    // Loudest first: anything waiting on a human sits at the top of the rail.
    if !alerts.is_empty() {
        lines.push(format!("⚠ Needs you ({})", alerts.len()));
        for m in alerts.iter().rev().take(6).rev() {
            lines.push(format!(
                " {} {}",
                short_agent(short_in_run(&m.from, &st.id)),
                truncate(&m.body, 30)
            ));
        }
        lines.push(String::new());
    }

    lines.push(format!("Run  {}", st.id));
    lines.push(format!("  {}", phase_label(st.phase)));
    if st.dry_run {
        lines.push("  dry-run".into());
    }
    if let Some(t) = st.task.as_deref() {
        lines.push(format!("  {}", truncate(t, 36)));
    }
    lines.push(String::new());

    // Compact agent status
    lines.push("Agents".into());
    for s in &st.slots {
        let act = SlotActivity::observe(s, 300);
        let mark = match s.status {
            SlotStatus::Running if act.stalled => "!",
            SlotStatus::Running => "●",
            SlotStatus::Done => "✓",
            SlotStatus::Failed => "✗",
            SlotStatus::Stuck => "!",
            SlotStatus::Pending => "·",
        };
        let quiet = if s.status == SlotStatus::Running {
            format!(" {}", act.human_silent())
        } else {
            String::new()
        };
        lines.push(format!(
            " {mark} {} {}{quiet}",
            role_label(s.role),
            slot_status_label(s.status),
        ));
    }

    // Orchestrator event timeline (human)
    let evs = events::read_all(swarm, &st.id).unwrap_or_default();
    if !evs.is_empty() {
        lines.push(String::new());
        lines.push("Timeline".into());
        for e in evs.iter().rev().take(14).rev() {
            lines.push(format!(" {}", activity_event_line(e)));
        }
    }

    // Bus chat only if real agent chat exists
    if let Ok(bus) = crate::bus::list_events(swarm, Some(&st.id)) {
        let chat: Vec<_> = bus
            .iter()
            .filter(|m| {
                !matches!(
                    m.kind,
                    crate::bus::MsgKind::Hello | crate::bus::MsgKind::System
                )
            })
            .collect();
        if !chat.is_empty() {
            lines.push(String::new());
            lines.push("Bus".into());
            for m in chat.iter().rev().take(8).rev() {
                lines.push(format!(
                    " {}→{} {}",
                    short_agent(short_in_run(&m.from, &st.id)),
                    short_agent(short_in_run(&m.to, &st.id)),
                    truncate(&m.body, 28)
                ));
            }
        }
    }

    let paused: Vec<_> = quota
        .providers
        .iter()
        .filter(|(_, q)| {
            format!("{:?}", q.status)
                .to_ascii_lowercase()
                .contains("pause")
        })
        .collect();
    if !paused.is_empty() {
        lines.push(String::new());
        lines.push("Quota".into());
        for (name, q) in paused {
            lines.push(format!(" {} {:?}", name, q.status));
        }
    }

    lines
}

fn short_agent(s: &str) -> &str {
    s.rsplit(['-', '/']).next().unwrap_or(s)
}

/// Render a bus agent id inside run `run`'s view: drop a leading `run:` qualifier so a
/// run slot shows as its short role id. Bare ids (no `run:` prefix) are left intact.
fn short_in_run<'a>(id: &'a str, run: &str) -> &'a str {
    id.strip_prefix(run)
        .and_then(|rest| rest.strip_prefix(':'))
        .unwrap_or(id)
}

fn activity_event_line(e: &events::Event) -> String {
    let t = e.ts.format("%H:%M");
    match e.kind {
        events::EventKind::Phase => {
            let phase = e.phase.map(phase_label).unwrap_or_else(|| "?".into());
            format!("{t} → {phase}")
        }
        events::EventKind::Slot => {
            let slot = e.slot.as_deref().unwrap_or("agent");
            let st = e.status.map(slot_status_label).unwrap_or("?");
            format!("{t} {slot} {st}")
        }
        events::EventKind::Gate => {
            let msg = e.message.as_deref().unwrap_or("waiting on you");
            format!("{t} gate · {msg}")
        }
        events::EventKind::Info => {
            let msg = e.message.as_deref().unwrap_or("");
            format!("{t} {msg}")
        }
    }
}

// ── human labels ────────────────────────────────────────────────────────────

fn phase_label(phase: Phase) -> String {
    match phase {
        Phase::Init => "Starting".into(),
        Phase::PrepareIsolation => "Preparing worktrees".into(),
        Phase::SpawnSlots => "Spawning agents".into(),
        Phase::Dispatch => "Dispatching".into(),
        Phase::WaitCompletion => "Waiting on agents".into(),
        Phase::PlanReady => "Plan ready".into(),
        Phase::Spec => "Writing acceptance tests".into(),
        Phase::AwaitingPlanApproval => "Needs plan approval".into(),
        Phase::PlanApproved => "Plan approved".into(),
        Phase::PlanRejected => "Plan rejected".into(),
        Phase::Review => "Under review".into(),
        Phase::Suite => "Running tests".into(),
        Phase::Rank => "Ranking candidates".into(),
        Phase::Fix => "Fixing issues".into(),
        Phase::PeerRelay => "Peer collaboration".into(),
        Phase::AwaitingWinnerConfirm => "Needs winner pick".into(),
        Phase::AwaitingReconcile => "Needs reconcile".into(),
        Phase::AwaitingShipConfirm => "Ready to ship".into(),
        Phase::Shipping => "Shipping".into(),
        Phase::Done => "Done".into(),
        Phase::Escalated => "Escalated".into(),
        Phase::Failed => "Failed".into(),
        Phase::Stuck => "Stuck".into(),
        Phase::Quota => "Quota blocked".into(),
        Phase::Stopped => "Stopped".into(),
    }
}

fn workflow_label(w: WorkflowKind) -> &'static str {
    match w {
        WorkflowKind::Plan => "plan",
        WorkflowKind::Loop => "build",
        WorkflowKind::Arena => "arena",
        WorkflowKind::Roles => "roles",
        WorkflowKind::Peer => "peer",
        WorkflowKind::Review => "review",
    }
}

fn role_label(r: crate::state::SlotRole) -> &'static str {
    use crate::state::SlotRole::*;
    match r {
        Planner => "planner",
        PlanCritic => "critic",
        TestAuthor => "spec",
        Implementer => "builder",
        Tester => "tests",
        Reviewer => "review",
        Ranker => "ranker",
        Peer => "peer",
        Reconciler => "merge",
    }
}

fn slot_status_label(s: SlotStatus) -> &'static str {
    match s {
        SlotStatus::Pending => "wait",
        SlotStatus::Running => "run",
        SlotStatus::Done => "done",
        SlotStatus::Failed => "fail",
        SlotStatus::Stuck => "stuck",
    }
}

fn relative_age(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn slot_icon(s: &SlotState, app: &App) -> String {
    match s.status {
        SlotStatus::Running => app.spinner().to_string(),
        SlotStatus::Done => "✓".into(),
        SlotStatus::Failed => "✗".into(),
        SlotStatus::Stuck => "!".into(),
        SlotStatus::Pending => "·".into(),
    }
}

fn slot_color(s: &SlotState) -> Color {
    match s.status {
        SlotStatus::Done => GREEN,
        SlotStatus::Failed | SlotStatus::Stuck => RED,
        SlotStatus::Running => CYAN,
        SlotStatus::Pending => FG_MUTED,
    }
}

fn phase_color(phase: Phase) -> Color {
    match phase {
        Phase::Done | Phase::PlanApproved => GREEN,
        Phase::Failed | Phase::PlanRejected | Phase::Quota => RED,
        Phase::Stuck | Phase::Escalated => MAGENTA,
        Phase::AwaitingPlanApproval
        | Phase::AwaitingWinnerConfirm
        | Phase::AwaitingShipConfirm
        | Phase::AwaitingReconcile => YELLOW,
        _ => ACCENT,
    }
}

fn is_active_phase(phase: Phase) -> bool {
    !phase.is_waitable_stop()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod labels {
    use super::*;

    #[test]
    fn phase_labels_are_human() {
        assert_eq!(
            phase_label(Phase::AwaitingPlanApproval),
            "Needs plan approval"
        );
        assert_eq!(phase_label(Phase::AwaitingShipConfirm), "Ready to ship");
        assert!(!phase_label(Phase::Suite).contains('_'));
    }

    #[test]
    fn wide_layout_shows_all_panels() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let lay = layout_rects(area, Focus::Log);
        assert!(!lay.narrow);
        assert_eq!(lay.tabs, Rect::default());
        for r in [lay.runs, lay.fleet, lay.stream, lay.bus] {
            assert!(r.width > 0 && r.height > 0);
        }
    }

    #[test]
    fn narrow_layout_shows_only_focused_panel() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 40,
        };
        let lay = layout_rects(area, Focus::Log);
        assert!(lay.narrow);
        assert!(lay.tabs.width > 0);
        assert!(lay.stream.width > 0, "focused log panel is on stage");
        for hidden in [lay.runs, lay.fleet, lay.bus] {
            assert_eq!(hidden, Rect::default(), "unfocused panels are zero-sized");
        }
        // Composer focus keeps the live log on stage.
        let composer = layout_rects(area, Focus::Composer);
        assert!(composer.stream.width > 0);
        // Runs focus swaps the stage to the rail.
        let runs = layout_rects(area, Focus::Runs);
        assert!(runs.runs.width > 0);
        assert_eq!(runs.stream, Rect::default());
    }

    #[test]
    fn gate_phases_map_to_buttons() {
        use crate::cli::WorkflowKind;
        let mut st = RunState::new("r1", WorkflowKind::Plan, std::path::PathBuf::from("/x"));
        assert!(gate_buttons_for(Some(&st)).is_empty());
        st.phase = Phase::AwaitingPlanApproval;
        let b = gate_buttons_for(Some(&st));
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].1, GateAction::Approve);
        assert_eq!(b[1].1, GateAction::Reject);
        st.phase = Phase::AwaitingShipConfirm;
        let b = gate_buttons_for(Some(&st));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].1, GateAction::Ship);
        st.phase = Phase::AwaitingWinnerConfirm;
        let b = gate_buttons_for(Some(&st));
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].1, GateAction::ConfirmWinner);
        assert_eq!(b[1].1, GateAction::Reconcile);
        st.phase = Phase::AwaitingReconcile;
        let b = gate_buttons_for(Some(&st));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].1, GateAction::Reconcile);
    }

    #[test]
    fn gate_buttons_render_and_record_hit_rects() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(90, 3)).unwrap();
        let mut app = App::new(None, 300, true);
        let buttons = vec![
            ("Approve", GateAction::Approve),
            ("Reject", GateAction::Reject),
        ];
        let area = Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 2,
        };
        term.draw(|f| render_gate_buttons(f, area, &mut app, &buttons))
            .unwrap();
        assert_eq!(app.gate_buttons.len(), 2);
        // Both buttons sit on the top row, in order, inside the area, right-aligned.
        assert!(app
            .gate_buttons
            .iter()
            .all(|(r, _)| r.y == 0 && r.right() <= 90));
        assert!(app.gate_buttons[0].0.x < app.gate_buttons[1].0.x);
        assert_eq!(app.gate_buttons[1].1, GateAction::Reject);
    }

    #[test]
    fn winner_confirm_bar_paints_both_buttons() {
        use crate::cli::WorkflowKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut st = RunState::new("run1", WorkflowKind::Arena, std::path::PathBuf::from("/x"));
        st.phase = Phase::AwaitingWinnerConfirm;
        let swarm = SparPaths::new("/x");
        let mut term = Terminal::new(TestBackend::new(70, 1)).unwrap();
        let mut app = App::new(None, 300, true);
        term.draw(|f| {
            let area = f.area();
            draw_status_bar(f, area, &swarm, Some(&st), &mut app, &[], &[]);
        })
        .unwrap();
        let row: String = {
            let buf = term.backend().buffer();
            (0..70).map(|x| buf[(x, 0)].symbol()).collect()
        };
        assert!(row.contains("Confirm"), "row was: {row:?}");
        assert!(row.contains("Reconcile"), "row was: {row:?}");
        assert_eq!(app.gate_buttons.len(), 2);
    }

    #[test]
    fn list_row_hit() {
        let r = Rect {
            x: 0,
            y: 10,
            width: 20,
            height: 8,
        };
        assert_eq!(list_row_at(r, 11, 3, 0), Some(0));
        assert_eq!(list_row_at(r, 12, 3, 0), Some(1));
        assert_eq!(list_row_at(r, 20, 3, 0), None);
        // Scrolled list: first visible row is index 2
        assert_eq!(list_row_at(r, 11, 10, 2), Some(2));
        assert_eq!(list_row_at(r, 12, 10, 2), Some(3));
    }

    #[test]
    fn truncate_log_default_one_row() {
        let long = format!("→ {}", "abcdefghij".repeat(8));
        let rows = layout_log_rows(&long, 24, true, false);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].0.ends_with('…'));
        assert!(rows[0].0.chars().count() <= 24);
    }

    #[test]
    fn expand_log_soft_wraps() {
        let long = format!("→ tool {}", "word ".repeat(20));
        let rows = layout_log_rows(&long, 20, true, true);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|(s, _)| s.chars().count() <= 20));
    }

    #[test]
    fn scroll_delta_clamps_and_sets_follow() {
        let mut scroll = 0u16;
        let mut follow = false;
        let max = 100u16;
        apply_scroll_delta(&mut scroll, &mut follow, max, 3);
        assert_eq!(scroll, 3);
        assert!(!follow);
        apply_scroll_delta(&mut scroll, &mut follow, max, 1000);
        assert_eq!(scroll, 100);
        assert!(follow);
        apply_scroll_delta(&mut scroll, &mut follow, max, -5);
        assert_eq!(scroll, 95);
        assert!(!follow);
    }

    #[test]
    fn scroll_up_when_content_fits_keeps_follow() {
        let mut scroll = 0u16;
        let mut follow = true;
        apply_scroll_delta(&mut scroll, &mut follow, 0, -3);
        assert_eq!(scroll, 0);
        assert!(
            follow,
            "short log must stay following so growth stays visible"
        );
    }

    #[test]
    fn clamp_scroll_pins_when_following() {
        let mut scroll = 9999u16;
        let mut follow = true;
        clamp_scroll(&mut scroll, &mut follow, 40);
        assert_eq!(scroll, 40);
        assert!(follow);

        scroll = 9999;
        follow = false;
        clamp_scroll(&mut scroll, &mut follow, 40);
        assert_eq!(scroll, 40);
        assert!(follow);

        scroll = 10;
        follow = false;
        clamp_scroll(&mut scroll, &mut follow, 40);
        assert_eq!(scroll, 10);
        assert!(!follow);
    }

    #[test]
    fn overscroll_then_up_moves_immediately() {
        let mut scroll = 9999u16;
        let mut follow = false;
        let max = 50u16;
        clamp_scroll(&mut scroll, &mut follow, max);
        assert_eq!(scroll, 50);
        apply_scroll_delta(&mut scroll, &mut follow, max, -3);
        assert_eq!(scroll, 47);
        assert!(!follow);
    }

    #[test]
    fn follow_pins_when_max_grows() {
        let mut scroll = 10u16;
        let mut follow = true;
        clamp_scroll(&mut scroll, &mut follow, 10);
        assert_eq!(scroll, 10);
        clamp_scroll(&mut scroll, &mut follow, 40);
        assert_eq!(scroll, 40);
        assert!(follow);
    }
}
