//! Product shell — clear fleet dashboard for multi-agent runs.
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
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Widget, Wrap,
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

/// Three focus targets, not an N-way ring: the drill-down rail, the one main
/// area, and the composer. `1` / `2` / `3` jump straight to one (see U1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Rail,
    Main,
    Composer,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Rail => Focus::Main,
            Focus::Main => Focus::Composer,
            Focus::Composer => Focus::Rail,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::Rail => Focus::Composer,
            Focus::Main => Focus::Rail,
            Focus::Composer => Focus::Main,
        }
    }
}

/// Main is one area whose content is `f(rail selection, tab)`. `[` / `]` (or a
/// click on the tab strip) switches tabs; nothing else moves on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainTab {
    Log,
    Activity,
    Diff,
    Shell,
}

/// Tab strip order — also the `[` / `]` cycle order and the narrow strip order.
const MAIN_TABS: [MainTab; 4] = [
    MainTab::Log,
    MainTab::Activity,
    MainTab::Diff,
    MainTab::Shell,
];

impl MainTab {
    fn label(self) -> &'static str {
        match self {
            MainTab::Log => "Log",
            MainTab::Activity => "Activity",
            MainTab::Diff => "Diff",
            MainTab::Shell => "Shell",
        }
    }
    fn idx(self) -> usize {
        MAIN_TABS.iter().position(|t| *t == self).unwrap_or(0)
    }
    fn next(self) -> Self {
        MAIN_TABS[(self.idx() + 1) % MAIN_TABS.len()]
    }
    fn prev(self) -> Self {
        MAIN_TABS[(self.idx() + MAIN_TABS.len() - 1) % MAIN_TABS.len()]
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
    // Bracketed paste so the embedded tmux client receives pastes as one framed
    // chunk (Event::Paste) rather than a storm of synthetic keystrokes.
    out.execute(EnableBracketedPaste)?;
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
        let _ = out.execute(DisableBracketedPaste);
        let _ = out.execute(DisableMouseCapture);
        let _ = out.execute(LeaveAlternateScreen);
    }
}

/// Bytes of slot log kept in the live-log viewport (tail window).
const LOG_TAIL_BYTES: usize = 256_000;

/// The rail is one drill-down tree: `projects ▸ runs ▸ agents`. `Enter` pushes a
/// level, `Esc` pops one (and never exits the app at the root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseLevel {
    /// General view — registered projects only (not a wall of runs).
    Projects,
    /// Per-project view — runs for `active_root` only.
    Runs,
    /// Per-run view — the selected run's slots.
    Agents,
}

impl BrowseLevel {
    /// Levels that need this project's runs (and the selected run) loaded.
    fn in_project(self) -> bool {
        !matches!(self, BrowseLevel::Projects)
    }
    fn pop(self) -> Self {
        match self {
            BrowseLevel::Agents => BrowseLevel::Runs,
            _ => BrowseLevel::Projects,
        }
    }
}

struct App {
    selected_run: usize,
    selected_project: usize,
    selected_slot: usize,
    focus: Focus,
    browse: BrowseLevel,
    /// Which view Main is showing. Content is a function of (rail selection × tab).
    main_tab: MainTab,
    /// Main is zoomed to the full body (rail hidden); `+` / `_`.
    zoom: bool,
    composer: String,
    status_line: String,
    stream_scroll: u16,
    bus_scroll: u16,
    diff_scroll: u16,
    /// When true, keep the live log pinned to the newest line as content grows.
    stream_follow: bool,
    bus_follow: bool,
    diff_follow: bool,
    /// Last known max scroll offsets (from the most recent paint).
    stream_max: u16,
    bus_max: u16,
    diff_max: u16,
    /// Log viewport height in rows (for PageUp/PageDown).
    stream_view_h: u16,
    bus_view_h: u16,
    diff_view_h: u16,
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
    /// One status line carrying the breadcrumb; tapping it returns focus to the rail.
    rect_status: Rect,
    /// The drill-down rail (zero-sized when zoomed, or in narrow while Main is focused).
    rect_rail: Rect,
    /// The one main area, borders included.
    rect_main: Rect,
    rect_composer: Rect,
    /// Per-tab hit rects for the Main tab strip (wide: in Main's top border; narrow: its own row).
    main_tabs: Vec<(Rect, MainTab)>,
    /// One-shot: on first narrow render with an active run, jump to Main's Log tab.
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
    /// Selected run is in flight with no live orchestrator. Refreshed from the snapshot
    /// each frame; a slot that still says `running` under this is not actually working.
    abandoned: bool,
    /// Embedded terminal (W3/W7/W8): a real `tmux -L spar attach` client in a PTY,
    /// rendered from its output bytes with raw keys/mouse/paste forwarded in. Lazily
    /// attached to the project's workspace shell when Main's Shell tab is opened.
    terminal_pane: Option<crate::terminal::TerminalPane>,
    /// Which tmux session the Shell tab should attach to. `None` = the project
    /// workspace shell; `Some(spar-<run_id>)` = an agent takeover selected from the
    /// rail's Agents level. Cleared back to `None` when the client detaches or the
    /// session ends.
    takeover_target: Option<String>,
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
            focus: Focus::Rail,
            // Inside a project → that project's runs. Outside → project picker.
            browse: if start_in_project {
                BrowseLevel::Runs
            } else {
                BrowseLevel::Projects
            },
            main_tab: MainTab::Log,
            zoom: false,
            composer: task_seed.unwrap_or_default(),
            status_line: String::new(),
            stream_scroll: 0,
            bus_scroll: 0,
            diff_scroll: 0,
            // Default: follow live output (newest lines).
            stream_follow: true,
            bus_follow: true,
            diff_follow: false,
            stream_max: 0,
            bus_max: 0,
            diff_max: 0,
            stream_view_h: 12,
            bus_view_h: 12,
            diff_view_h: 12,
            tick: 0,
            flash: None,
            stall_warn_secs,
            log_expand: false,
            last_ctrl_c: None,
            last_click: None,
            show_help: false,
            animated: false,
            rect_status: Rect::default(),
            rect_rail: Rect::default(),
            rect_main: Rect::default(),
            rect_composer: Rect::default(),
            main_tabs: Vec::new(),
            narrow_autofocus_done: false,
            gate_buttons: Vec::new(),
            rect_help: Rect::default(),
            rect_projects: Rect::default(),
            reconcile_spawn: None,
            human_alerts_n: 0,
            abandoned: false,
            terminal_pane: None,
            takeover_target: None,
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
        self.diff_scroll = 0;
        self.diff_follow = false;
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
        self.focus = Focus::Rail;
    }

    fn open_projects_view(&mut self) {
        self.browse = BrowseLevel::Projects;
        self.selected_run = 0;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Rail;
    }

    /// `Esc` in the rail: pop one level. At `Projects` this is a no-op — the rail
    /// root is never an exit.
    fn rail_pop(&mut self) {
        if self.browse == BrowseLevel::Projects {
            return;
        }
        let next = self.browse.pop();
        if next == BrowseLevel::Projects {
            self.open_projects_view();
        } else {
            self.browse = next;
            self.selected_slot = 0;
            self.reset_stream_view();
        }
    }

    /// Focus Main on `tab` — the one path used by clicks, `2`, and takeover.
    fn open_main(&mut self, tab: MainTab) {
        self.main_tab = tab;
        self.focus = Focus::Main;
    }

    fn stream_page(&self) -> u16 {
        self.stream_view_h.saturating_sub(1).max(3)
    }

    fn bus_page(&self) -> u16 {
        self.bus_view_h.saturating_sub(1).max(3)
    }

    fn diff_page(&self) -> u16 {
        self.diff_view_h.saturating_sub(1).max(3)
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

    fn scroll_diff_by(&mut self, delta: i32) {
        apply_scroll_delta(
            &mut self.diff_scroll,
            &mut self.diff_follow,
            self.diff_max,
            delta,
        );
    }

    /// Scroll whichever view Main is showing. The Shell tab is a live tmux client:
    /// it never scrolls from here (its input is forwarded raw).
    fn scroll_main_by(&mut self, delta: i32) {
        match self.main_tab {
            MainTab::Log => self.scroll_stream_by(delta),
            MainTab::Activity => self.scroll_bus_by(delta),
            MainTab::Diff => self.scroll_diff_by(delta),
            MainTab::Shell => {}
        }
    }

    fn main_page(&self) -> u16 {
        match self.main_tab {
            MainTab::Activity => self.bus_page(),
            MainTab::Diff => self.diff_page(),
            _ => self.stream_page(),
        }
    }

    fn home_for_main(&mut self) {
        match self.main_tab {
            MainTab::Activity => {
                self.bus_follow = false;
                self.bus_scroll = 0;
            }
            MainTab::Diff => {
                self.diff_follow = false;
                self.diff_scroll = 0;
            }
            _ => {
                self.stream_follow = false;
                self.stream_scroll = 0;
            }
        }
    }

    fn end_for_main(&mut self) {
        match self.main_tab {
            MainTab::Activity => {
                self.bus_follow = true;
                self.bus_scroll = self.bus_max;
            }
            MainTab::Diff => {
                self.diff_follow = true;
                self.diff_scroll = self.diff_max;
            }
            _ => {
                self.stream_follow = true;
                self.stream_scroll = self.stream_max;
            }
        }
    }

    /// True when keys/mouse belong to the embedded tmux client rather than spar.
    fn shell_active(&self) -> bool {
        self.focus == Focus::Main && self.main_tab == MainTab::Shell
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
    /// Main's Diff tab: the run's plan/artifacts, or a placeholder.
    diff_text: String,
    /// Unresolved `@human`/`Blocked` alerts for the selected run (status-line badge count).
    human_alerts: usize,
    /// Selected run is in flight with no live orchestrator.
    abandoned: bool,
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
    if sel.browse.in_project() {
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
            out.push(stamp(&swarm.artifacts_dir(id)));
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
    let runs = if sel.browse.in_project() {
        registry::list_project_runs(&sel.root).unwrap_or_default()
    } else {
        Vec::new()
    };
    // Display path: markers, not state.json, decide whether a slot is still running.
    let full = if sel.browse.in_project() {
        sel.run_id
            .as_ref()
            .and_then(|id| RunState::load_for_display(&swarm, id).ok())
    } else {
        None
    };
    let abandoned = full
        .as_ref()
        .map(|st| st.abandoned(&swarm))
        .unwrap_or(false);
    let quota = QuotaStore::load(&swarm).unwrap_or_default();
    let stream_text = if sel.browse.in_project() {
        stream_content(&swarm, full.as_ref(), sel.slot_idx, cache)
    } else {
        cache.clear();
        project_overview(&projects, sel.project_idx)
    };
    let diff_text = diff_content(&swarm, full.as_ref(), sel.slot_idx);
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
        diff_text,
        human_alerts: alerts.len(),
        abandoned,
    }
}

/// Main's Diff tab. No new plumbing (Stage A): show the run's artifacts — the
/// selected slot's own artifact when it has one, else the plan — and say so
/// plainly when there is nothing to show yet.
fn diff_content(swarm: &SparPaths, full: Option<&RunState>, slot_idx: usize) -> String {
    let Some(st) = full else {
        return "\n  No run selected.\n\n  Diff shows the run's artifacts (plan, review, suite)."
            .into();
    };
    let dir = swarm.artifacts_dir(&st.id);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        return format!(
            "\n  No artifacts yet for {}.\n\n  Diff will render the worktree diff in a later stage;\n  for now it shows this run's artifacts as they land:\n    {}\n",
            st.id,
            dir.display()
        );
    }

    // Prefer the selected slot's artifact, then a plan, then the first file.
    let slot_artifact = st
        .slots
        .get(slot_idx)
        .and_then(|s| s.artifact.as_deref())
        .map(|a| {
            Path::new(a)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| a.to_string())
        })
        .filter(|a| names.contains(a));
    let pick = slot_artifact
        .or_else(|| names.iter().find(|n| n.starts_with("plan")).cloned())
        .unwrap_or_else(|| names[0].clone());

    let body = process::tail_log_info(&dir.join(&pick), LOG_TAIL_BYTES).text;
    format!(
        "  artifacts: {}\n  showing: {pick}\n\n{body}",
        names.join(" · ")
    )
}

/// Redraw is only worth it while something is moving on screen: a flash timer,
/// the composer cursor, or a run that is actively working (active phase or a
/// running slot). An active phase with no running slot — Suite, Review,
/// Shipping — still animates so the header spinner keeps turning.
fn animating(app: &App, snap: &Snapshot) -> bool {
    app.flash.is_some()
        || app.focus == Focus::Composer
        // A live terminal streams between disk snapshots; keep repainting it.
        || (app.main_tab == MainTab::Shell && app.terminal_pane.is_some())
        // An abandoned run is going nowhere: never spin for it.
        || (!snap.abandoned
            && snap.full.as_ref().is_some_and(|st| {
                is_active_phase(st.phase)
                    || st.slots.iter().any(|s| s.status == SlotStatus::Running)
            }))
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    local_root: Option<PathBuf>,
    task_seed: Option<String>,
    stall_warn_secs: u64,
) -> Result<crate::exit_codes::ExitCode> {
    let mut app = App::new(task_seed, stall_warn_secs, local_root.is_some());
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
            BrowseLevel::Agents if n_slots > 0 => Some(app.selected_slot),
            _ => None,
        });

        manage_terminal(&mut app, &active_root);
        app.animated = animating(&app, &snap);
        app.human_alerts_n = snap.human_alerts;
        app.abandoned = snap.abandoned;

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
                    &snap.diff_text,
                    &mut app,
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
                        ),
                        Event::Paste(text) => {
                            // Forward a paste to the tmux client as bracketed paste.
                            if app.shell_active() {
                                if let Some(pane) = app.terminal_pane.as_ref() {
                                    let mut buf = Vec::with_capacity(text.len() + 12);
                                    buf.extend_from_slice(b"\x1b[200~");
                                    buf.extend_from_slice(text.as_bytes());
                                    buf.extend_from_slice(b"\x1b[201~");
                                    pane.write_input(&buf);
                                }
                            }
                        }
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

    // Ctrl+C twice (within 2s) is the only quit path — never Esc or q. On Main's
    // Shell tab, Ctrl+C belongs to the agent pane (SIGINT), so it falls through to
    // forwarding; F12 out first to reach the quit arm.
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) && !app.shell_active() {
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

    // Main's Shell tab IS a real tmux client, so every key is forwarded raw into its
    // PTY — prefix (C-a), copy-mode, splits, session switch are all tmux's own. F12 is
    // the ONLY escape back to spar (Esc/Tab belong to the agent). With no pane attached
    // we deliberately fall through to the normal handler so an unattachable Shell tab
    // can never trap the operator.
    if app.shell_active() {
        if code == KeyCode::F(12) {
            app.focus = Focus::Rail;
            return Ok(false);
        }
        if let Some(pane) = app.terminal_pane.as_ref() {
            if let Some(bytes) = crate::terminal::encode_key(code, mods) {
                pane.write_input(&bytes);
            }
            return Ok(false);
        }
    }

    if app.focus == Focus::Composer {
        match code {
            KeyCode::Esc => app.focus = Focus::Rail,
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
        // Esc pops one rail level; from Main/Composer it returns to the rail. It
        // never exits the app (at Projects it does nothing).
        KeyCode::Esc => {
            if app.focus != Focus::Rail {
                app.focus = Focus::Rail;
            } else {
                app.rail_pop();
            }
        }
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('1') => app.focus = Focus::Rail,
        KeyCode::Char('2') => app.focus = Focus::Main,
        KeyCode::Char('3') => app.focus = Focus::Composer,
        KeyCode::Char('/') => {
            app.focus = Focus::Composer;
            if !app.composer.starts_with('/') {
                app.composer = "/".into();
            }
        }
        KeyCode::Char('i') => app.focus = Focus::Composer,
        // ] / [ move between Main's tabs — the only thing that changes on screen.
        KeyCode::Char(']') => {
            app.main_tab = app.main_tab.next();
        }
        KeyCode::Char('[') => {
            app.main_tab = app.main_tab.prev();
        }
        KeyCode::Char('+') => app.zoom = true,
        KeyCode::Char('_') => app.zoom = false,
        KeyCode::Enter => {
            if app.focus == Focus::Rail {
                rail_enter(app, projects, runs, full, active_root);
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
            Focus::Rail => rail_move(app, projects, runs, n_slots, 1),
            Focus::Main => app.scroll_main_by(3),
            Focus::Composer => {}
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::Rail => rail_move(app, projects, runs, n_slots, -1),
            Focus::Main => app.scroll_main_by(-3),
            Focus::Composer => {}
        },
        KeyCode::PageDown => match app.focus {
            Focus::Rail => rail_move(app, projects, runs, n_slots, 5),
            Focus::Main => app.scroll_main_by(i32::from(app.main_page())),
            Focus::Composer => {}
        },
        KeyCode::PageUp => match app.focus {
            Focus::Rail => rail_move(app, projects, runs, n_slots, -5),
            Focus::Main => app.scroll_main_by(-i32::from(app.main_page())),
            Focus::Composer => {}
        },
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
            app.home_for_main();
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.end_for_main();
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

/// Move the rail selection by `delta` rows at whatever level it is on.
fn rail_move(
    app: &mut App,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    n_slots: usize,
    delta: i32,
) {
    let step = |cur: usize, n: usize| -> usize {
        if delta < 0 {
            cur.saturating_sub((-delta) as usize)
        } else {
            (cur + delta as usize).min(n.saturating_sub(1))
        }
    };
    match app.browse {
        BrowseLevel::Projects if !projects.is_empty() => {
            app.select_project(step(app.selected_project, projects.len()), projects.len());
        }
        BrowseLevel::Runs if !runs.is_empty() => {
            app.select_run(step(app.selected_run, runs.len()), runs.len());
        }
        BrowseLevel::Agents if n_slots > 0 => {
            app.select_slot(step(app.selected_slot, n_slots), n_slots);
        }
        _ => {}
    }
}

/// `Enter` in the rail: push one level. On a slot (the deepest level) there is
/// nothing left to push into, so it takes the agent over — point the passthrough
/// terminal at that run's tmux pane and open it in Main's Shell tab. Only runs
/// launched with `--backend tmux` have a `spar-<run_id>` session; headless runs
/// have no pane to attach to.
fn rail_enter(
    app: &mut App,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
) {
    match app.browse {
        BrowseLevel::Projects => {
            if let Some(p) = projects.get(app.selected_project) {
                *active_root = p.root.clone();
                app.open_project_runs();
                app.flash(
                    format!("Opened {}", p.name.as_deref().unwrap_or("project")),
                    GREEN,
                );
            }
        }
        BrowseLevel::Runs => {
            if runs.get(app.selected_run).is_some() {
                app.browse = BrowseLevel::Agents;
                app.selected_slot = 0;
                app.reset_stream_view();
            }
        }
        BrowseLevel::Agents => {
            let Some(st) = full else { return };
            let Some(slot) = st.slots.get(app.selected_slot) else {
                return;
            };
            let session = tmux::session_name(&st.id);
            let slot_id = slot.id.clone();
            if tmux::has_session(&session) {
                app.takeover_target = Some(session.clone());
                let _ = tmux::select_window(&session, &slot_id);
                app.open_main(MainTab::Shell);
                app.flash(
                    format!("Took over {slot_id} — F12/Ctrl+a d to hand back"),
                    GREEN,
                );
            } else {
                app.flash(
                    "headless run — rerun with --backend tmux to take over",
                    YELLOW,
                );
            }
        }
    }
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
) {
    let (x, y) = (m.column, m.row);
    let n_slots = full.map(|s| s.slots.len()).unwrap_or(0);
    let n_rail = rail_len(app.browse, projects.len(), runs.len(), n_slots);

    // The tab strip is chrome, never the agent's — it is the escape hatch out of the
    // Shell tab on a touch screen, so it is hit-tested BEFORE the terminal forward.
    if let Some(&(_, tab)) = app.main_tabs.iter().find(|(r, _)| contains(*r, x, y)) {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            app.open_main(tab);
        }
        return;
    }

    // Shell tab with a live pane: mouse over the terminal body is tmux's (wheel scroll
    // into copy-mode, click-select). Translate to pane-relative coords inside the border
    // and forward as SGR mouse. Events outside it fall through so clicking the rail or
    // the composer still changes focus.
    if app.shell_active() {
        if let Some(pane) = app.terminal_pane.as_ref() {
            let r = app.rect_main;
            if contains(r, x, y) && r.width > 2 && r.height > 2 {
                let inner_x = r.x + 1;
                let inner_y = r.y + 1;
                let max_x = r.x + r.width - 2;
                let max_y = r.y + r.height - 2;
                let cx = x.clamp(inner_x, max_x) - inner_x;
                let cy = y.clamp(inner_y, max_y) - inner_y;
                if let Some(bytes) = crate::terminal::encode_mouse(m.kind, cx, cy, m.modifiers) {
                    pane.write_input(&bytes);
                }
                return;
            }
        }
    }

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
            // Tappable gate buttons take priority — they sit on the status line.
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

            if contains(app.rect_composer, x, y) {
                app.focus = Focus::Composer;
            } else if contains(app.rect_rail, x, y) {
                app.focus = Focus::Rail;
                if let Some(row) = list_row_at(app.rect_rail, y, n_rail, rail_offset) {
                    rail_select(app, row, projects.len(), runs.len(), n_slots);
                    // Double-click = Enter: drill one level (and take over on a slot).
                    if dbl {
                        rail_enter(app, projects, runs, full, active_root);
                    }
                }
            } else if contains(app.rect_main, x, y) {
                app.focus = Focus::Main;
            } else if contains(app.rect_status, x, y) {
                // The breadcrumb is the way back to the rail on a touch screen.
                app.focus = Focus::Rail;
            }
        }
        MouseEventKind::ScrollDown => {
            if contains(app.rect_main, x, y) {
                app.focus = Focus::Main;
                app.scroll_main_by(3);
            } else if contains(app.rect_rail, x, y) {
                app.focus = Focus::Rail;
                rail_move(app, projects, runs, n_slots, 1);
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(app.rect_main, x, y) {
                app.focus = Focus::Main;
                app.scroll_main_by(-3);
            } else if contains(app.rect_rail, x, y) {
                app.focus = Focus::Rail;
                rail_move(app, projects, runs, n_slots, -1);
            }
        }
        _ => {}
    }
}

/// Row count of the rail at its current level — the list the mouse hit-tests against.
fn rail_len(browse: BrowseLevel, n_projects: usize, n_runs: usize, n_slots: usize) -> usize {
    match browse {
        BrowseLevel::Projects => n_projects,
        BrowseLevel::Runs => n_runs,
        BrowseLevel::Agents => n_slots,
    }
}

/// Select rail row `row` at whatever level the rail is on.
fn rail_select(app: &mut App, row: usize, n_projects: usize, n_runs: usize, n_slots: usize) {
    match app.browse {
        BrowseLevel::Projects => app.select_project(row, n_projects),
        BrowseLevel::Runs => app.select_run(row, n_runs),
        BrowseLevel::Agents => app.select_slot(row, n_slots),
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
    /// One status line: breadcrumb + run context + gate cues/buttons.
    status: Rect,
    /// The drill-down rail. Zero-sized when zoomed, or in narrow while Main is focused.
    rail: Rect,
    /// The one main area (tab strip lives in its top border in the wide layout).
    main: Rect,
    composer: Rect,
    footer: Rect,
    /// Narrow-mode MainTab strip; zero-sized in the wide layout (the strip is drawn
    /// inside Main's top border there).
    tabs: Rect,
    /// True when the single-column phone layout is active.
    narrow: bool,
}

/// Below this terminal width the rail folds away: Main only, with the MainTab strip
/// on its own row — usable over a phone/Termux SSH session.
const NARROW_WIDTH: u16 = 90;

/// Rail width in the wide layout. Enough for `run id · phase · age`, no more.
const RAIL_WIDTH: u16 = 24;

/// Chrome budget: 1 status row + 3 composer rows + 1 footer row. Everything else
/// on screen is content (rail + main).
fn layout_rects(area: Rect, focus: Focus, zoom: bool) -> LayoutRects {
    let narrow = area.width < NARROW_WIDTH;
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status line (breadcrumb + gate)
            Constraint::Length(if narrow { 1 } else { 0 }), // narrow MainTab strip
            Constraint::Min(4),    // body: rail + main
            Constraint::Length(3), // command
            Constraint::Length(1), // footer
        ])
        .split(area);

    let z = Rect::default();
    if narrow {
        // One column. The rail takes the stage while it is focused; otherwise Main
        // has it. Tapping a tab (or the breadcrumb) moves between the two.
        let (rail, main) = if focus == Focus::Rail {
            (root[2], z)
        } else {
            (z, root[2])
        };
        return LayoutRects {
            status: root[0],
            rail,
            main,
            composer: root[3],
            footer: root[4],
            tabs: root[1],
            narrow: true,
        };
    }

    // Wide: rail + one main area. Zoom hides the rail in place; nothing else moves.
    let (rail, main) = if zoom {
        (z, root[2])
    } else {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(RAIL_WIDTH), Constraint::Min(20)])
            .split(root[2]);
        (body[0], body[1])
    };

    LayoutRects {
        status: root[0],
        rail,
        main,
        composer: root[3],
        footer: root[4],
        tabs: z,
        narrow: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut Frame,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    stream_text: &str,
    activity: &[String],
    diff_text: &str,
    app: &mut App,
    rail_state: &mut ListState,
) {
    let area = f.area();
    // Full clear each frame — prevents styled-cell ghosting across the whole UI.
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    // On the first narrow render with an active run, land on the live log so a
    // phone glance shows progress — but only once, and never over a manual move.
    if area.width < NARROW_WIDTH && !app.narrow_autofocus_done {
        let active = full.map(|s| {
            is_active_phase(s.phase) || s.slots.iter().any(|sl| sl.status == SlotStatus::Running)
        });
        if active == Some(true) {
            if app.focus == Focus::Rail {
                app.open_main(MainTab::Log);
            }
            app.narrow_autofocus_done = true;
        }
    }

    let lay = layout_rects(area, app.focus, app.zoom);
    // Keep mouse hit regions aligned with the frame actually painted.
    app.rect_status = lay.status;
    app.rect_rail = lay.rail;
    app.rect_main = lay.main;
    app.rect_composer = lay.composer;
    // Rebuilt below by whatever paints this frame.
    app.gate_buttons.clear();
    app.main_tabs.clear();

    draw_status(f, lay.status, swarm, projects, runs, full, app);
    if lay.narrow {
        draw_narrow_tabs(f, lay.tabs, app);
    }
    if lay.rail.width > 0 {
        draw_rail(f, lay.rail, projects, runs, full, app, rail_state);
    }
    if lay.main.width > 0 {
        draw_main(
            f,
            lay.main,
            swarm,
            full,
            stream_text,
            activity,
            diff_text,
            app,
            !lay.narrow,
        );
    }
    draw_composer(f, lay.composer, app);
    draw_footer(f, lay.footer, app, full);

    if app.show_help {
        draw_help_overlay(f, area);
    }
}

/// The Main tab strip. Labels + the Activity alert badge, active tab lit. Records a
/// hit rect per tab so a click (or a phone tap) switches tabs.
fn main_tab_spans(app: &App) -> Vec<(MainTab, String, Style)> {
    MAIN_TABS
        .iter()
        .map(|t| {
            let badge = if *t == MainTab::Activity && app.human_alerts_n > 0 {
                format!(" ⚠{}", app.human_alerts_n)
            } else {
                String::new()
            };
            let text = format!(" {}{badge} ", t.label());
            let style = if *t == app.main_tab {
                Style::default().fg(BG).bg(ACCENT).bold()
            } else if *t == MainTab::Activity && app.human_alerts_n > 0 {
                Style::default().fg(BG).bg(RED).bold()
            } else {
                Style::default().fg(FG_DIM).bg(BG_RAISED)
            };
            (*t, text, style)
        })
        .collect()
}

/// Narrow layout: the same MainTab strip on its own row, equal cells, tappable —
/// the only escape from the Shell tab on a phone.
fn draw_narrow_tabs(f: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let tabs = main_tab_spans(app);
    let n = tabs.len() as u16;
    let cell = (area.width / n).max(1);
    let mut spans: Vec<Span> = Vec::with_capacity(tabs.len());
    let mut x = area.x;
    for (i, (tab, text, style)) in tabs.into_iter().enumerate() {
        let w = if i as u16 == n - 1 {
            area.width.saturating_sub(cell * (n - 1))
        } else {
            cell
        };
        let label = truncate(text.trim(), w as usize);
        spans.push(Span::styled(format!("{label:^w$}", w = w as usize), style));
        app.main_tabs.push((
            Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            tab,
        ));
        x = x.saturating_add(w);
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_RAISED)),
        area,
    );
}

/// The status line's cue and its colors. A background other than `BG_RAISED` means an
/// alert state (gate, quota, failure, abandoned) worth surfacing loudly.
fn status_cue(
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    app: &App,
) -> (String, Color, Color) {
    if app.browse == BrowseLevel::Projects {
        if projects.is_empty() {
            return (
                format!(
                    "no projects yet — run spar in a repo · {}",
                    registry::spar_home().display()
                ),
                FG_DIM,
                BG_RAISED,
            );
        }
        return ("Enter opens a project".into(), FG_MUTED, BG_RAISED);
    }
    let Some(st) = full else {
        return if runs.is_empty() {
            (
                "no runs — spar plan -t \"…\" --providers cli:claude".into(),
                FG_DIM,
                BG_RAISED,
            )
        } else {
            ("select a run".into(), FG_MUTED, BG_RAISED)
        };
    };
    if app.abandoned {
        return (
            format!(
                "ABANDONED — no orchestrator · spar implement --run {}",
                st.id
            ),
            FG,
            Color::Rgb(48, 24, 24),
        );
    }
    match st.phase {
        Phase::AwaitingPlanApproval => ("plan ready — a approve · r reject".into(), BG, YELLOW),
        Phase::AwaitingWinnerConfirm => ("winner ready — confirm or reconcile".into(), BG, YELLOW),
        Phase::AwaitingShipConfirm => ("ready to ship — s (draft PR)".into(), BG, YELLOW),
        Phase::AwaitingReconcile => ("reconcile ready".into(), BG, YELLOW),
        Phase::Quota => (
            "all providers paused — spar provider resume".into(),
            BG,
            RED,
        ),
        Phase::Failed | Phase::Stuck | Phase::Escalated => (
            format!("{} — check the Log tab", phase_label(st.phase)),
            FG,
            Color::Rgb(48, 24, 24),
        ),
        _ if st.dry_run => ("dry-run".into(), FG_DIM, BG_RAISED),
        _ => (String::new(), FG_MUTED, BG_RAISED),
    }
}

/// The whole top chrome: one line.
///
/// `spar · acme/api ▸ run 3f2a ▸ impl#2 · implement (2/3) · ⚠2 · ABANDONED`
///
/// Breadcrumb (rail path) + phase + slot counts + alert/abandoned badges + the gate
/// cue, with tappable gate buttons right-aligned on the same row.
fn draw_status(
    f: &mut Frame,
    area: Rect,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    app: &mut App,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (cue, cue_fg, cue_bg) = status_cue(projects, runs, full, app);
    let buttons = gate_buttons_for(full);
    let alert = cue_bg != BG_RAISED;
    let bg = if alert { cue_bg } else { BG_RAISED };

    let project = swarm
        .project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".");

    let mut spans = vec![
        Span::styled(" spar ", Style::default().fg(BG).bg(ACCENT).bold()),
        Span::styled(
            format!(" {} ", truncate(project, 20)),
            Style::default().fg(FG).bg(bg).bold(),
        ),
    ];
    if app.browse.in_project() {
        let run = full
            .map(|s| s.id.clone())
            .or_else(|| runs.get(app.selected_run).map(|r| r.id.clone()))
            .unwrap_or_else(|| "—".into());
        spans.push(Span::styled("▸ ", Style::default().fg(FG_MUTED).bg(bg)));
        spans.push(Span::styled(
            format!("run {run} "),
            Style::default().fg(CYAN).bg(bg),
        ));
    }
    if app.browse == BrowseLevel::Agents {
        let slot = full
            .and_then(|s| s.slots.get(app.selected_slot))
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "—".into());
        spans.push(Span::styled("▸ ", Style::default().fg(FG_MUTED).bg(bg)));
        spans.push(Span::styled(
            format!("{slot} "),
            Style::default().fg(MAGENTA).bg(bg),
        ));
    }

    if let Some(st) = full {
        let pc = if alert { cue_fg } else { phase_color(st.phase) };
        spans.push(Span::styled("· ", Style::default().fg(FG_MUTED).bg(bg)));
        if !app.abandoned && is_active_phase(st.phase) {
            spans.push(Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(pc).bg(bg),
            ));
        }
        spans.push(Span::styled(
            phase_label(st.phase),
            Style::default().fg(pc).bg(bg).bold(),
        ));
        if !st.slots.is_empty() {
            let done = st
                .slots
                .iter()
                .filter(|s| s.status == SlotStatus::Done)
                .count();
            spans.push(Span::styled(
                format!(" ({done}/{}) ", st.slots.len()),
                Style::default().fg(FG_DIM).bg(bg),
            ));
        }
        if st.dry_run {
            spans.push(Span::styled(
                " dry-run ",
                Style::default().fg(BG).bg(YELLOW).bold(),
            ));
        }
    }
    if app.human_alerts_n > 0 {
        spans.push(Span::styled(
            format!(" ⚠{} ", app.human_alerts_n),
            Style::default().fg(BG).bg(RED).bold(),
        ));
    }
    if app.abandoned {
        spans.push(Span::styled(
            " ABANDONED ",
            Style::default().fg(BG).bg(RED).bold(),
        ));
    }
    if !cue.is_empty() {
        spans.push(Span::styled(" · ", Style::default().fg(FG_MUTED).bg(bg)));
        spans.push(Span::styled(
            cue,
            Style::default().fg(cue_fg).bg(bg).add_modifier(if alert {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        area,
    );
    render_gate_buttons(f, area, app, &buttons);
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

/// The rail: one drill-down tree (`projects ▸ runs ▸ agents`), never a stack of
/// co-equal panels. `Enter` pushes a level, `Esc` pops one.
fn draw_rail(
    f: &mut Frame,
    area: Rect,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    app: &App,
    state: &mut ListState,
) {
    let focused = app.focus == Focus::Rail;
    let (items, title) = match app.browse {
        BrowseLevel::Projects => (
            rail_project_items(projects, app),
            format!(" Projects ({}) ", projects.len()),
        ),
        BrowseLevel::Runs => (
            rail_run_items(runs, app),
            format!(" Runs ({}) ", runs.len()),
        ),
        BrowseLevel::Agents => {
            let slots = full.map(|s| s.slots.as_slice()).unwrap_or(&[]);
            let running = slots
                .iter()
                .filter(|s| s.status == SlotStatus::Running)
                .count();
            let title = if app.abandoned {
                format!(" Agents {running}/{} orphaned ", slots.len())
            } else {
                format!(" Agents {running}/{} live ", slots.len())
            };
            (rail_slot_items(slots, app), title)
        }
    };
    let list = List::new(items).block(panel(&title, focused));
    f.render_stateful_widget(list, area, state);
}

fn rail_project_items<'a>(projects: &'a [registry::ProjectEntry], app: &App) -> Vec<ListItem<'a>> {
    if projects.is_empty() {
        return vec![ListItem::new(Span::styled(
            "  (no projects)",
            Style::default().fg(FG_MUTED).italic(),
        ))];
    }
    projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let sel = i == app.selected_project;
            let name = p.name.as_deref().unwrap_or("·");
            let n = registry::list_project_runs(&p.root)
                .map(|r| r.len())
                .unwrap_or(0);
            let line = Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{:<12}", truncate(name, 12)),
                    Style::default()
                        .fg(if sel { FG } else { CYAN })
                        .add_modifier(if sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(format!(" {n}r "), Style::default().fg(FG_MUTED)),
                Span::styled(relative_age(p.last_seen), Style::default().fg(FG_MUTED)),
            ]);
            ListItem::new(line).style(if sel {
                Style::default().bg(BG_RAISED)
            } else {
                Style::default()
            })
        })
        .collect()
}

fn rail_run_items<'a>(runs: &'a [state::RunSummary], app: &App) -> Vec<ListItem<'a>> {
    if runs.is_empty() {
        return vec![ListItem::new(Span::styled(
            "  (no runs)",
            Style::default().fg(FG_MUTED).italic(),
        ))];
    }
    runs.iter()
        .enumerate()
        .map(|(i, r)| {
            let sel = i == app.selected_run;
            // Phase reads "review" forever on a run nobody is driving; say so.
            let (phase_text, phase_c) = if r.abandoned {
                (format!("{} ✗", truncate(&phase_label(r.phase), 8)), RED)
            } else {
                (truncate(&phase_label(r.phase), 10), phase_color(r.phase))
            };
            let line = Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{:<8}", truncate(&r.id, 8)),
                    Style::default()
                        .fg(if sel { FG } else { FG_DIM })
                        .add_modifier(if sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(format!(" {phase_text:<10}"), Style::default().fg(phase_c)),
                Span::styled(
                    format!(" {}", relative_age(r.updated_at)),
                    Style::default().fg(FG_MUTED),
                ),
            ]);
            ListItem::new(line).style(if sel {
                Style::default().bg(BG_RAISED)
            } else {
                Style::default()
            })
        })
        .collect()
}

fn rail_slot_items<'a>(slots: &'a [SlotState], app: &App) -> Vec<ListItem<'a>> {
    if slots.is_empty() {
        return vec![ListItem::new(Span::styled(
            "  (no agents yet)",
            Style::default().fg(FG_MUTED).italic(),
        ))];
    }
    slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let sel = i == app.selected_slot;
            let act = SlotActivity::observe(s, app.stall_warn_secs);
            let orphaned = app.abandoned && s.status == SlotStatus::Running;
            let color = if act.stalled || orphaned {
                RED
            } else {
                slot_color(s)
            };
            let tail = if s.status != SlotStatus::Running {
                slot_status_label(s.status).to_string()
            } else if orphaned {
                format!("ORPHAN {}", act.human_silent())
            } else if act.stalled {
                format!("STALL {}", act.human_silent())
            } else {
                act.human_silent()
            };
            let line = Line::from(vec![
                Span::styled(if sel { "› " } else { "  " }, Style::default().fg(ACCENT)),
                Span::styled(
                    format!("{} ", slot_icon(s, app)),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{:<8}", truncate(&s.id, 8)),
                    Style::default()
                        .fg(if sel { FG } else { FG_DIM })
                        .add_modifier(if sel {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::styled(format!(" {tail}"), Style::default().fg(color)),
            ]);
            ListItem::new(line).style(if sel {
                Style::default().bg(BG_RAISED)
            } else {
                Style::default()
            })
        })
        .collect()
}

/// Main: ONE area, content = f(rail selection × tab). The tab strip is painted into
/// its top border (wide) or on its own row (narrow); nothing relocates when the tab
/// changes.
#[allow(clippy::too_many_arguments)]
fn draw_main(
    f: &mut Frame,
    area: Rect,
    swarm: &SparPaths,
    full: Option<&RunState>,
    stream_text: &str,
    activity: &[String],
    diff_text: &str,
    app: &mut App,
    wide: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::Main;
    let border = if focused { BORDER_FOCUS } else { BORDER };

    let mut title: Vec<Span> = Vec::new();
    if wide {
        // The strip lives in the border, so its hit rects start one cell in.
        let mut x = area.x.saturating_add(1);
        for (tab, text, style) in main_tab_spans(app) {
            let w = text.chars().count() as u16;
            if x.saturating_add(w) < area.right() {
                app.main_tabs.push((
                    Rect {
                        x,
                        y: area.y,
                        width: w,
                        height: 1,
                    },
                    tab,
                ));
            }
            x = x.saturating_add(w);
            title.push(Span::styled(text, style));
        }
    }
    let ctx = main_context(swarm, full, app);
    if !ctx.is_empty() {
        title.push(Span::styled(
            format!(" {ctx} "),
            Style::default().fg(FG_DIM),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(title))
        .style(Style::default().bg(BG_PANEL));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match app.main_tab {
        MainTab::Log => draw_log_body(f, inner, full, stream_text, app),
        MainTab::Activity => draw_activity_body(f, inner, activity, app),
        MainTab::Diff => draw_diff_body(f, inner, diff_text, app),
        MainTab::Shell => draw_shell_body(f, inner, app),
    }
}

/// The subtitle that rides after the tab strip: what the active tab is showing.
fn main_context(swarm: &SparPaths, full: Option<&RunState>, app: &App) -> String {
    match app.main_tab {
        MainTab::Log => {
            let slot = full
                .and_then(|st| st.slots.get(app.selected_slot))
                .map(|s| s.id.as_str())
                .unwrap_or("—");
            let mode = if app.log_expand { "wrap" } else { "trim" };
            let follow = if app.stream_follow { " · live" } else { "" };
            format!("· {slot} · {mode}{follow}")
        }
        MainTab::Activity => "· run timeline + bus".into(),
        MainTab::Diff => "· artifacts".into(),
        MainTab::Shell => match app.takeover_target.as_deref() {
            Some(session) => format!("· agent · {session}"),
            None => {
                let base = swarm
                    .project_root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project");
                format!("· shell · {base}")
            }
        },
    }
}

/// Main's Log tab: the live stream for the selected slot (or the run), with the
/// slot's stall/quiet state and token stats on a one-row band.
fn draw_log_body(
    f: &mut Frame,
    inner: Rect,
    full: Option<&RunState>,
    stream_text: &str,
    app: &mut App,
) {
    // No run selected (Projects level): the body is an overview, not a stream — no
    // stats band for it.
    if full.is_none() {
        app.stream_view_h = inner.height;
        app.stream_max = render_scrollable_log(
            f,
            inner,
            stream_text,
            &mut app.stream_scroll,
            &mut app.stream_follow,
            false,
            app.log_expand,
        );
        return;
    }
    let slot = full.and_then(|st| st.slots.get(app.selected_slot));
    let silent_hint = slot
        .map(|s| {
            let act = SlotActivity::observe(s, app.stall_warn_secs);
            if app.abandoned && s.status == SlotStatus::Running {
                format!(" ORPHAN {} ", act.human_silent())
            } else if act.stalled {
                format!(" STALL {} ", act.human_silent())
            } else if s.status == SlotStatus::Running {
                format!(" quiet {} ", act.human_silent())
            } else {
                String::new()
            }
        })
        .unwrap_or_default();

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
    draw_stream_stats(
        f,
        chunks[0],
        stats.as_ref(),
        slot.map(|s| s.status),
        &silent_hint,
        app.abandoned,
    );

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

/// Main's Activity tab: the run timeline + bus feed + human alerts (was a column).
fn draw_activity_body(f: &mut Frame, inner: Rect, activity: &[String], app: &mut App) {
    let text = if activity.is_empty() {
        "No activity yet.\n\nRun timeline: phases, agents, gates, bus.".into()
    } else {
        activity.join("\n")
    };
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

/// Main's Diff tab: the run's artifacts for now (no new plumbing in Stage A).
fn draw_diff_body(f: &mut Frame, inner: Rect, diff_text: &str, app: &mut App) {
    app.diff_view_h = inner.height;
    app.diff_max = render_scrollable_log(
        f,
        inner,
        diff_text,
        &mut app.diff_scroll,
        &mut app.diff_follow,
        false,
        app.log_expand,
    );
}

fn draw_stream_stats(
    f: &mut Frame,
    area: Rect,
    stats: Option<&process::StreamStats>,
    status: Option<SlotStatus>,
    silent_hint: &str,
    abandoned: bool,
) {
    let quiet = if silent_hint.is_empty() {
        Span::raw("")
    } else {
        let c = if abandoned || silent_hint.contains("STALL") || silent_hint.contains("ORPHAN") {
            RED
        } else {
            FG_MUTED
        };
        Span::styled(
            silent_hint.to_string(),
            Style::default().fg(c).bg(BG_RAISED),
        )
    };
    let Some(s) = stats else {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "  waiting for agent output…",
                    Style::default().fg(FG_MUTED).bg(BG_RAISED),
                ),
                quiet,
            ]))
            .style(Style::default().bg(BG_RAISED)),
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
        quiet,
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

/// The composer: one input row inside its border (3 rows total). Stage B replaces it
/// with a `:` palette.
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
    let body = if app.composer.is_empty() && !focused {
        Line::from(vec![
            prompt,
            Span::styled(
                "/approve  /reject  /ship  /spawn  @agent …",
                Style::default().fg(FG_MUTED).italic(),
            ),
        ])
    } else {
        Line::from(vec![
            prompt,
            Span::styled(&app.composer, Style::default().fg(FG)),
            Span::styled(cursor_blink, Style::default().fg(ACCENT)),
        ])
    };
    let title = if focused {
        " Command · Enter run · Esc leave "
    } else {
        " Command · 3 or i or / "
    };
    f.render_widget(Paragraph::new(body).block(panel(title, focused)), area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &mut App, full: Option<&RunState>) {
    app.rect_help = Rect::default();
    app.rect_projects = Rect::default();

    let (msg, color) = if let Some((_, m, c, _)) = &app.flash {
        (m.as_str(), *c)
    } else if !app.status_line.is_empty() {
        (app.status_line.as_str(), YELLOW)
    } else {
        (
            situational_footer(full, app.focus, app.browse, app.main_tab),
            FG_MUTED,
        )
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

/// One row of keys that are valid *right now* — nothing else.
fn situational_footer(
    full: Option<&RunState>,
    focus: Focus,
    browse: BrowseLevel,
    tab: MainTab,
) -> &'static str {
    if let Some(st) = full {
        if st.phase == Phase::AwaitingPlanApproval {
            return "a approve · r reject · or tap the buttons above";
        }
        if st.phase == Phase::AwaitingShipConfirm {
            return "s confirm ship (draft PR) · or tap Ship above";
        }
        if st.phase == Phase::AwaitingWinnerConfirm || st.phase == Phase::AwaitingReconcile {
            return "tap Confirm / Reconcile above · ] Log";
        }
    }
    match focus {
        Focus::Rail => match browse {
            BrowseLevel::Projects => "j/k · Enter open project · 2 main · 3 cmd · ? help",
            BrowseLevel::Runs => "j/k · Enter agents · Esc projects · 2 main · ? help",
            BrowseLevel::Agents => "j/k · Enter take over agent · Esc runs · 2 main",
        },
        Focus::Main => match tab {
            MainTab::Log => "scroll · [ ] tabs · w wrap · g/G top/end · + zoom · 1 rail",
            MainTab::Activity => "scroll · [ ] tabs · g/G top/end · 1 rail",
            MainTab::Diff => "scroll · [ ] tabs · 1 rail",
            MainTab::Shell => "tmux passthrough · prefix C-a · Ctrl+a d / F12 → spar",
        },
        Focus::Composer => "/approve /reject /ship /spawn · @agent msg · Enter · Esc",
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
 spar — rail + one main area\n\
 \n\
  Shape\n\
    Rail   projects ▸ runs ▸ agents  (Enter pushes, Esc pops)\n\
    Main   one area · tabs: Log · Activity · Diff · Shell\n\
    Main always shows the rail's selection — nothing else moves.\n\
 \n\
  Keyboard\n\
    1 / 2 / 3            focus Rail · Main · Command\n\
    Tab / Shift-Tab      cycle those three\n\
    j k  or  ↑ ↓         move in the rail · scroll Main\n\
    Enter                push a rail level (on an agent: take it over)\n\
    Esc                  pop a rail level (never quits)\n\
    [ ]                  previous / next Main tab\n\
    + / _                zoom Main fullscreen / restore\n\
    p                    jump to Projects\n\
    a / r / s            approve · reject · ship (when gated)\n\
    i  or  /             command bar\n\
    w                    log wrap ↔ truncate long lines\n\
    g / G                top / bottom of Main\n\
    ?                    this help · Esc closes help\n\
    Ctrl+C twice         exit (only quit path)\n\
 \n\
  Shell tab = a real tmux client: every key goes to the agent.\n\
    prefix C-a · Ctrl+a d or F12 hands focus back to spar.\n\
 \n\
  Mouse / touch: tap a tab, a rail row (double-tap = Enter), a gate\n\
  button, or the breadcrumb (back to the rail). Scroll to scroll.\n\
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

/// Lifecycle for the embedded terminal (W7), now hosted in Main's Shell tab:
/// resolve the desired session on the spar socket, drop a stale attachment, attach
/// lazily while the Shell tab is up, and pump live output into the vt100 buffer every
/// frame. The pane is project-scoped, not run-scoped: by default it shows the
/// project's persistent workspace shell.
fn manage_terminal(app: &mut App, project_root: &Path) {
    // Nothing to do until the Shell tab is opened; avoids forking tmux every frame
    // while the operator is on another tab.
    if app.main_tab != MainTab::Shell && app.terminal_pane.is_none() {
        return;
    }
    if !tmux::available() {
        app.terminal_pane = None;
        return;
    }

    // Dead client (Ctrl+a d detach, or the takeover session ended): the `attach`
    // child exited. Drop the pane, revert to the workspace shell, and hand focus back
    // to spar so the operator isn't stranded on a dead tab. The tmux SESSION is
    // untouched — only our transient client went away.
    if let Some(pane) = app.terminal_pane.as_mut() {
        if !pane.is_alive() {
            app.terminal_pane = None;
            app.takeover_target = None;
            if app.shell_active() {
                app.focus = Focus::Rail;
            }
            return;
        }
    }

    // Resolve the session to attach to: an agent takeover if one is set and its
    // session still exists, otherwise the project workspace shell. A takeover whose
    // session has since died silently reverts to the shell. The workspace shell is
    // detached and deliberately OUTLIVES the TUI, so a dev server in it survives restarts.
    let desired = match app.takeover_target.as_ref() {
        Some(s) if tmux::has_session(s) => s.clone(),
        _ => {
            app.takeover_target = None;
            match tmux::ensure_workspace_shell(project_root) {
                Ok(name) => name,
                Err(_) => {
                    app.terminal_pane = None;
                    return;
                }
            }
        }
    };

    // Bound to a different session — release the old client so we rebind below.
    if let Some(pane) = app.terminal_pane.as_ref() {
        if pane.session() != Some(desired.as_str()) {
            app.terminal_pane = None;
        }
    }

    // Attach lazily, only once the Shell tab is up.
    if app.main_tab == MainTab::Shell && app.terminal_pane.is_none() {
        // Enable tmux mouse so our forwarded SGR mouse is interpreted by the client.
        tmux::ensure_server_config();
        let (rows, cols) = terminal_dims(app.rect_main);
        let mut pane = crate::terminal::TerminalPane::new(rows, cols);
        if pane.attach(&desired).is_ok() {
            app.terminal_pane = Some(pane);
        }
    }

    if let Some(pane) = app.terminal_pane.as_mut() {
        pane.pump();
    }
}

/// Main's Shell tab: the real tmux client. Keys/mouse are forwarded raw whenever this
/// tab is focused and a pane is attached (see `App::shell_active`).
fn draw_shell_body(f: &mut Frame, inner: Rect, app: &mut App) {
    let Some(pane) = app.terminal_pane.as_mut() else {
        let hint = Paragraph::new(
            "Opening a real tmux client for the project's workspace shell — \
             run a dev server, cargo, poke around; the session stays alive across TUI restarts.\n\n\
             Or select an agent in the rail (Enter on a run, then Enter on a slot) to take over its live pane.\n\n\
             Full tmux: prefix C-a, copy-mode/scroll, splits, session switch. Ctrl+a d / F12 → spar.\n\n\
             (No tmux on PATH? The Shell tab needs it.)",
        )
        .style(Style::default().fg(FG_DIM))
        .wrap(Wrap { trim: true });
        f.render_widget(hint, inner);
        return;
    };

    // Reserve a one-line in-panel hint footer when there's room for it.
    let footer_h: u16 = if inner.height >= 3 { 1 } else { 0 };
    let term_area = Rect {
        height: inner.height - footer_h,
        ..inner
    };
    // Keep the vt100 buffer (and the tmux pane) matched to the visible area.
    pane.resize(term_area.height, term_area.width);
    let term = PseudoTerminal::new(pane.screen());
    f.render_widget(term, term_area);

    if footer_h == 1 {
        let footer = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        let hint = Paragraph::new(
            "Ctrl+a d / F12 / tap a tab → spar · C-a [ scroll/copy · ] paste · % / \" split · s session",
        )
        .style(Style::default().fg(FG_DIM));
        f.render_widget(hint, footer);
    }
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
    if crate::bus::is_reserved_sink(target) {
        // Canonicalize a `human` alias to the HUMAN sink (`@human`) so it routes to the
        // notifier and alert panel (which key on `@human`), not a literal `inbox/human`.
        return Ok(if target == "human" {
            crate::bus::HUMAN.to_string()
        } else {
            target.to_string()
        });
    }
    if target.contains(':') {
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
    fn wide_layout_is_rail_plus_one_main() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let lay = layout_rects(area, Focus::Main, false);
        assert!(!lay.narrow);
        // The tab strip rides in Main's top border, not on its own row.
        assert_eq!(lay.tabs, Rect::default());
        assert_eq!(lay.rail.width, RAIL_WIDTH);
        assert!(lay.main.width > 0);
        // Fixed chrome is exactly 2 rows (status + footer); composer is 3.
        assert_eq!(lay.status.height, 1);
        assert_eq!(lay.footer.height, 1);
        assert_eq!(lay.composer.height, 3);
        assert_eq!(lay.rail.height + 5, area.height);
        // Rail and Main are side by side, and together they fill the width.
        assert_eq!(lay.rail.right(), lay.main.x);
        assert_eq!(lay.main.right(), area.width);
    }

    #[test]
    fn zoom_hides_the_rail_in_place() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let plain = layout_rects(area, Focus::Main, false);
        let zoomed = layout_rects(area, Focus::Main, true);
        assert_eq!(zoomed.rail, Rect::default());
        assert_eq!(zoomed.main.x, area.x);
        assert_eq!(zoomed.main.width, area.width);
        // Nothing else relocates.
        assert_eq!(zoomed.status, plain.status);
        assert_eq!(zoomed.composer, plain.composer);
        assert_eq!(zoomed.footer, plain.footer);
        assert_eq!(zoomed.main.y, plain.main.y);
    }

    #[test]
    fn narrow_layout_is_main_only_with_a_tab_strip() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 40,
        };
        let lay = layout_rects(area, Focus::Main, false);
        assert!(lay.narrow);
        assert!(lay.tabs.width > 0, "MainTab strip is tappable on a phone");
        assert!(lay.main.width > 0);
        assert_eq!(lay.rail, Rect::default(), "no rail in narrow");
        // Composer focus keeps Main on stage so you can watch while typing.
        let composer = layout_rects(area, Focus::Composer, false);
        assert!(composer.main.width > 0);
        assert_eq!(composer.rail, Rect::default());
        // Rail focus swaps the single stage to the rail; the tab strip stays.
        let rail = layout_rects(area, Focus::Rail, false);
        assert!(rail.rail.width > 0);
        assert_eq!(rail.main, Rect::default());
        assert!(rail.tabs.width > 0);
    }

    #[test]
    fn focus_ring_is_three_wide() {
        assert_eq!(Focus::Rail.next(), Focus::Main);
        assert_eq!(Focus::Main.next(), Focus::Composer);
        assert_eq!(Focus::Composer.next(), Focus::Rail);
        assert_eq!(Focus::Rail.prev(), Focus::Composer);
    }

    #[test]
    fn main_tabs_cycle_both_ways() {
        assert_eq!(MainTab::Log.next(), MainTab::Activity);
        assert_eq!(MainTab::Shell.next(), MainTab::Log);
        assert_eq!(MainTab::Log.prev(), MainTab::Shell);
        assert_eq!(MainTab::Diff.prev(), MainTab::Activity);
    }

    /// The tab strip must out-rank the terminal's mouse forwarding: on a phone it is
    /// the only way out of the Shell tab.
    #[test]
    fn clicking_a_tab_escapes_the_shell() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let swarm = SparPaths::new("/x");
        let mut app = App::new(None, 300, true);
        app.open_main(MainTab::Shell);
        app.main_tabs = vec![
            (
                Rect {
                    x: 1,
                    y: 0,
                    width: 5,
                    height: 1,
                },
                MainTab::Log,
            ),
            (
                Rect {
                    x: 6,
                    y: 0,
                    width: 10,
                    height: 1,
                },
                MainTab::Activity,
            ),
        ];
        app.rect_main = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let mut root = PathBuf::from("/x");
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 7,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            &swarm,
            &[],
            &[],
            None,
            &mut root,
            None,
            0,
        );
        assert_eq!(app.main_tab, MainTab::Activity);
        assert_eq!(app.focus, Focus::Main);
        assert!(!app.shell_active());
    }

    #[test]
    fn rail_pop_never_leaves_projects() {
        let mut app = App::new(None, 300, true);
        app.browse = BrowseLevel::Agents;
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Runs);
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Projects);
        // Root: Esc is a no-op, never an exit.
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Projects);
        assert_eq!(app.focus, Focus::Rail);
    }

    #[test]
    fn shell_active_only_on_focused_main_shell_tab() {
        let mut app = App::new(None, 300, true);
        assert!(!app.shell_active());
        app.main_tab = MainTab::Shell;
        assert!(!app.shell_active(), "rail focus keeps keys in spar");
        app.focus = Focus::Main;
        assert!(app.shell_active());
        app.main_tab = MainTab::Log;
        assert!(!app.shell_active(), "another tab keeps keys in spar");
    }

    #[test]
    fn takeover_opens_the_shell_tab() {
        use crate::cli::WorkflowKind;
        let mut app = App::new(None, 300, true);
        app.open_main(MainTab::Shell);
        assert_eq!(app.focus, Focus::Main);
        assert_eq!(app.main_tab, MainTab::Shell);
        // No tmux session for a headless run: rail_enter must not attach or focus.
        let mut st = RunState::new("r1", WorkflowKind::Loop, std::path::PathBuf::from("/x"));
        st.slots.push(crate::executor::init_slot(
            "impl-1",
            "cli:claude",
            crate::state::SlotRole::Implementer,
        ));
        let mut app = App::new(None, 300, true);
        app.browse = BrowseLevel::Agents;
        let mut root = PathBuf::from("/x");
        rail_enter(&mut app, &[], &[], Some(&st), &mut root);
        assert!(app.takeover_target.is_none());
        assert_eq!(app.focus, Focus::Rail, "headless run: nothing to take over");
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
    fn status_line_carries_breadcrumb_and_gate_buttons() {
        use crate::cli::WorkflowKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut st = RunState::new("run1", WorkflowKind::Arena, std::path::PathBuf::from("/x"));
        st.phase = Phase::AwaitingWinnerConfirm;
        let swarm = SparPaths::new("/x");
        let mut term = Terminal::new(TestBackend::new(90, 1)).unwrap();
        let mut app = App::new(None, 300, true);
        app.human_alerts_n = 2;
        term.draw(|f| {
            let area = f.area();
            draw_status(f, area, &swarm, &[], &[], Some(&st), &mut app);
        })
        .unwrap();
        let row: String = {
            let buf = term.backend().buffer();
            (0..90).map(|x| buf[(x, 0)].symbol()).collect()
        };
        assert!(row.contains("spar"), "row was: {row:?}");
        assert!(row.contains("run run1"), "breadcrumb · row was: {row:?}");
        assert!(row.contains("⚠2"), "alert badge · row was: {row:?}");
        assert!(row.contains("Confirm"), "row was: {row:?}");
        assert!(row.contains("Reconcile"), "row was: {row:?}");
        assert_eq!(app.gate_buttons.len(), 2);
    }

    #[test]
    fn main_tab_strip_is_hit_testable_and_badges_alerts() {
        use crate::cli::WorkflowKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let st = RunState::new("run1", WorkflowKind::Loop, std::path::PathBuf::from("/x"));
        let swarm = SparPaths::new("/x");
        let mut term = Terminal::new(TestBackend::new(90, 12)).unwrap();
        let mut app = App::new(None, 300, true);
        app.human_alerts_n = 3;
        let area = Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 12,
        };
        term.draw(|f| {
            draw_main(
                f,
                area,
                &swarm,
                Some(&st),
                "log",
                &[],
                "diff",
                &mut app,
                true,
            );
        })
        .unwrap();
        assert_eq!(app.main_tabs.len(), MAIN_TABS.len());
        // The strip sits in Main's top border row, left to right, inside the frame.
        assert!(app.main_tabs.iter().all(|(r, _)| r.y == area.y));
        assert!(app.main_tabs[0].0.x > area.x);
        assert!(app.main_tabs.windows(2).all(|w| w[0].0.x < w[1].0.x));
        assert_eq!(app.main_tabs[3].1, MainTab::Shell);
        let border: String = {
            let buf = term.backend().buffer();
            (0..90).map(|x| buf[(x, 0)].symbol()).collect()
        };
        assert!(border.contains("Activity ⚠3"), "border was: {border:?}");
        assert!(border.contains("Shell"), "border was: {border:?}");
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
