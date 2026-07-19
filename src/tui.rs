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
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Rail => Focus::Main,
            Focus::Main => Focus::Rail,
        }
    }
    fn prev(self) -> Self {
        self.next()
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

/// One entry in the `:` command palette. `needs_run` commands complete the run id
/// from the workspace roster; `arg_hint` is the ghost text shown after the verb.
struct PaletteCmd {
    name: &'static str,
    arg_hint: &'static str,
    help: &'static str,
    needs_run: bool,
}

/// The `:` palette verb table — the run-lifecycle actions the orchestrator brokers.
/// This is the whole command surface; there is no hidden syntax.
const PALETTE_CMDS: &[PaletteCmd] = &[
    PaletteCmd {
        name: "approve",
        arg_hint: "[run]",
        help: "approve the plan gate",
        needs_run: true,
    },
    PaletteCmd {
        name: "reject",
        arg_hint: "[run] [reason]",
        help: "reject the plan gate",
        needs_run: true,
    },
    PaletteCmd {
        name: "ship",
        arg_hint: "[run]",
        help: "confirm ship (draft PR)",
        needs_run: true,
    },
    PaletteCmd {
        name: "confirm",
        arg_hint: "[run]",
        help: "confirm the arena winner",
        needs_run: true,
    },
    PaletteCmd {
        name: "reconcile",
        arg_hint: "[run]",
        help: "start reconcile",
        needs_run: true,
    },
    PaletteCmd {
        name: "takeover",
        arg_hint: "[run]",
        help: "attach the run's tmux pane",
        needs_run: true,
    },
    PaletteCmd {
        name: "implement",
        arg_hint: "[run]",
        help: "advance a planned run into implement",
        needs_run: true,
    },
    PaletteCmd {
        name: "plan",
        arg_hint: "<task>",
        help: "start a plan (reuses the selected run's fleet)",
        needs_run: false,
    },
    PaletteCmd {
        name: "spawn",
        arg_hint: "<provider> [task]",
        help: "spawn a bare agent",
        needs_run: false,
    },
    PaletteCmd {
        name: "chat",
        arg_hint: "@agent <msg>",
        help: "send a bus message",
        needs_run: false,
    },
    PaletteCmd {
        name: "help",
        arg_hint: "",
        help: "open the keymap",
        needs_run: false,
    },
    PaletteCmd {
        name: "quit",
        arg_hint: "",
        help: "exit spar",
        needs_run: false,
    },
];

/// State for the open `:` palette: the typed line and the highlighted completion.
#[derive(Default)]
struct Palette {
    input: String,
    /// Index into the current completion list (commands, or run ids for the arg).
    sel: usize,
}

impl Palette {
    /// The verb word typed so far (everything before the first space), lowercased.
    fn head(&self) -> String {
        self.input
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// True once the operator has typed a space — i.e. is on the argument, so
    /// completion switches from verbs to run ids.
    fn on_arg(&self) -> bool {
        self.input.contains(char::is_whitespace)
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
    let cfg = local_root
        .as_ref()
        .and_then(|r| Config::load(r).ok())
        .unwrap_or_default();

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

    run_loop(&mut terminal, local_root, opts.task_seed, cfg)
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
    /// The `:` command palette. `Some` = open and capturing keys.
    palette: Option<Palette>,
    /// Incremental `/` rail filter. `Some` = editing it; the string also persists as
    /// the active filter while navigating (empty string = filter shown but matches all).
    filter: Option<String>,
    /// True once `/` has committed (Enter): the filter still narrows the rail but keys
    /// have returned to normal rail navigation. Cleared when the filter is dropped.
    filter_committed: bool,
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
    /// Loaded once at startup; supplies `stall_warn_secs` and per-role stall hard caps.
    cfg: Config,
    /// Freshest process heartbeat per slot id, refreshed from the snapshot each frame.
    /// Feeds stall detection so a busy-but-log-quiet slot isn't flagged as stalled.
    heartbeats: std::collections::HashMap<String, DateTime<Utc>>,
    /// When false (default), long log lines truncate with …; `w` toggles wrap.
    log_expand: bool,
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
    /// The `:` palette overlay rect (for click-to-dismiss); zero-sized when closed.
    rect_palette: Rect,
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
    /// Per-run attention level from the previous snapshot, for toast edge-detection.
    /// `None` until the first snapshot primes it (so we never toast the initial fleet).
    prev_attention: Option<Vec<(String, Attention)>>,
    /// Hit rect of the fleet roll-up token on the status line; a tap jumps to the next
    /// run that needs you (same as `a`). Zero-sized when nothing needs attention.
    rect_attention: Rect,
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
    fn new(task_seed: Option<String>, cfg: Config, start_in_project: bool) -> Self {
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
            // A launch task seed opens the palette pre-filled with a `plan` command.
            palette: task_seed.map(|t| Palette {
                input: format!("plan {t}"),
                sel: 0,
            }),
            filter: None,
            filter_committed: false,
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
            cfg,
            heartbeats: std::collections::HashMap::new(),
            log_expand: false,
            last_click: None,
            show_help: false,
            animated: false,
            rect_status: Rect::default(),
            rect_rail: Rect::default(),
            rect_main: Rect::default(),
            rect_palette: Rect::default(),
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
            prev_attention: None,
            rect_attention: Rect::default(),
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

    /// Driving mode: the Shell tab is focused with a live pane attached, so spar goes
    /// full-screen for the agent. This is a *structural* mode — the rail collapses and
    /// the chrome recolors (a text label alone is proven insufficient signalling).
    fn driving(&self) -> bool {
        self.shell_active() && self.terminal_pane.is_some()
    }

    /// True while a text field (palette or rail filter) owns keystrokes.
    fn editing_text(&self) -> bool {
        self.palette.is_some() || self.filter.is_some()
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
    /// Freshest process heartbeat per slot id for the selected run.
    heartbeats: std::collections::HashMap<String, DateTime<Utc>>,
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
            // Heartbeats append to the workspace roster without touching state/events, so
            // stamp it too — else a log-quiet-but-heartbeating slot never triggers a
            // snapshot rebuild and its heartbeat (and stall status) goes stale in the TUI.
            out.push(stamp(&crate::bus::agents_path(&swarm)));
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
fn build_snapshot(sel: &Selection, cache: &mut LogCache, cfg: &Config) -> Snapshot {
    let swarm = SparPaths::new(&sel.root);
    let projects = registry::projects();
    let runs = if sel.browse.in_project() {
        let mut runs = registry::list_project_runs(&sel.root).unwrap_or_default();
        // Attention-sorted rail: gates and broken runs float to the top (Stage C).
        sort_runs_by_attention(&mut runs);
        runs
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
    // One roster read per tick; slot id → freshest heartbeat. Process liveness
    // independent of log output, so a quiet-but-working slot isn't shown as stalled.
    let heartbeats = full
        .as_ref()
        .map(|st| {
            let by_addr = crate::bus::heartbeat_map(&swarm, Some(&st.id));
            st.slots
                .iter()
                .filter_map(|s| {
                    by_addr
                        .get(&crate::bus::resolve_addr(Some(&st.id), &s.id))
                        .map(|ts| (s.id.clone(), *ts))
                })
                .collect()
        })
        .unwrap_or_default();
    let activity = activity_feed(&swarm, full.as_ref(), &quota, &alerts, &heartbeats, cfg);
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
        heartbeats,
    }
}

/// The real `git diff` of a slot's worktree against HEAD (Stage B): staged + unstaged,
/// capped so a huge diff never blows the log buffer. `git -C` keeps us out of the
/// primary checkout.
fn worktree_diff(path: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["--no-pager", "diff", "HEAD", "--stat"])
        .output()?;
    let stat = String::from_utf8_lossy(&out.stdout).into_owned();
    let patch = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["--no-pager", "diff", "HEAD"])
        .output()?;
    if !patch.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&patch.stderr).trim());
    }
    let body = String::from_utf8_lossy(&patch.stdout);
    let capped: String = body.chars().take(DIFF_MAX_BYTES).collect();
    let trailer = if body.len() > DIFF_MAX_BYTES {
        "\n\n  … diff truncated (open the worktree to see the rest)"
    } else {
        ""
    };
    Ok(format!("{stat}\n{capped}{trailer}"))
}

/// Cap for the rendered worktree diff, in chars.
const DIFF_MAX_BYTES: usize = 200_000;

/// Main's Diff tab (Stage B): the selected slot's worktree diff against HEAD, falling
/// back to the run's artifacts when the slot has no worktree (plan/review slots,
/// headless runs) so the tab is never blank.
fn diff_content(swarm: &SparPaths, full: Option<&RunState>, slot_idx: usize) -> String {
    let Some(st) = full else {
        return "\n  No run selected.".into();
    };

    // Prefer the real worktree diff for the selected slot (Stage B). Coding slots each
    // get a worktree; map the selection to its record and diff it against HEAD.
    if let Some(slot) = st.slots.get(slot_idx) {
        if let Some(wt) = st.worktrees.iter().find(|w| w.slot_id == slot.id) {
            match worktree_diff(&wt.path) {
                Ok(text) if !text.trim().is_empty() => {
                    return format!(
                        "  {} · {}\n  {}\n\n{text}",
                        slot.id,
                        wt.branch,
                        wt.path.display()
                    );
                }
                Ok(_) => {
                    return format!(
                        "  {} · {}\n  {}\n\n  No changes in the worktree yet.",
                        slot.id,
                        wt.branch,
                        wt.path.display()
                    );
                }
                Err(e) => {
                    return format!("  {} · {}\n\n  git diff failed: {e:#}", slot.id, wt.branch);
                }
            }
        }
    }

    // No worktree for this slot (e.g. plan/review slot, or headless) — fall back to the
    // run's artifacts so the tab is never blank.
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
            "\n  No worktree diff and no artifacts yet for {}.\n\n  The Diff tab shows the selected slot's worktree changes once it has one;\n  until then it falls back to this run's artifacts:\n    {}\n",
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
        || app.editing_text()
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
    cfg: Config,
) -> Result<crate::exit_codes::ExitCode> {
    let mut app = App::new(task_seed, cfg.clone(), local_root.is_some());
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
    let snapshot = Arc::new(Mutex::new(Arc::new(build_snapshot(&sel, &mut cache, &cfg))));

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
        let cfg = cfg.clone();
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

            let next = Arc::new(build_snapshot(&sel, &mut cache, &cfg));
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
            // The attention sort reorders the rail as runs change state; keep the
            // cursor glued to the same run id rather than the same row.
            if let Some(prev) = sel.run_id.as_deref() {
                if let Some(pos) = snap.runs.iter().position(|r| r.id == prev) {
                    app.selected_run = pos;
                }
            }
            app.selected_run = app.selected_run.min(snap.runs.len() - 1);
        }
        // Toast a run the moment it starts wanting the operator (gate/broken), so a
        // fleet transition is noticed even while looking at another run.
        emit_attention_toasts(&mut app, &snap.runs);
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
        app.heartbeats = snap.heartbeats.clone();

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

    // The `:` palette owns every key while it is open — including Enter (run), Tab
    // (complete), and Esc (close). It can only open when not in the Shell tab, so it
    // never contends with the agent pane.
    if app.palette.is_some() {
        return handle_palette_key(app, code, mods, swarm, runs, full);
    }

    // The `/` rail filter captures keys while it is being edited.
    if app.filter.is_some() && !app.filter_committed {
        handle_filter_key(app, code, projects, runs, n_slots);
        return Ok(false);
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
    // PTY — prefix (C-a), copy-mode, splits, session switch are all tmux's own, and
    // Ctrl+C is the agent's SIGINT. F12 is the ONLY escape back to spar (Esc/Tab belong
    // to the agent). With no pane attached we deliberately fall through to the normal
    // handler so an unattachable Shell tab can never trap the operator.
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

    match code {
        // q exits from any non-text context (Shell forwards it to the agent above, and
        // the palette/filter capture it while editing). Ctrl+C is no longer a quit path
        // — it belongs to the agent pane.
        KeyCode::Char('q') => return Ok(true),
        // Esc pops one rail level; from Main it returns to the rail. It never exits the
        // app (at Projects it does nothing).
        KeyCode::Esc => {
            if app.filter.is_some() {
                app.filter = None;
                app.filter_committed = false;
            } else if app.focus != Focus::Rail {
                app.focus = Focus::Rail;
            } else {
                app.rail_pop();
            }
        }
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('1') => app.focus = Focus::Rail,
        KeyCode::Char('2') => app.focus = Focus::Main,
        // : opens the command palette; / opens the rail filter.
        KeyCode::Char(':') => app.palette = Some(Palette::default()),
        KeyCode::Char('/') => {
            app.focus = Focus::Rail;
            app.filter = Some(String::new());
            app.filter_committed = false;
        }
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
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::Rail => rail_move(app, projects, runs, n_slots, -1),
            Focus::Main => app.scroll_main_by(-3),
        },
        KeyCode::PageDown => match app.focus {
            Focus::Rail => rail_move(app, projects, runs, n_slots, 5),
            Focus::Main => app.scroll_main_by(i32::from(app.main_page())),
        },
        KeyCode::PageUp => match app.focus {
            Focus::Rail => rail_move(app, projects, runs, n_slots, -5),
            Focus::Main => app.scroll_main_by(-i32::from(app.main_page())),
        },
        // a jumps to the next run that wants you (Stage C). Approve moved to the gate
        // button / `:approve` when `a` became the fleet-wide attention binding.
        KeyCode::Char('a') => jump_to_attention(app, runs),
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

/// What running a palette command produced.
enum PaletteResult {
    Flash(String, Color),
    Quit,
    Help,
}

/// The completion candidates for the palette right now: verb names while typing the
/// command, or matching run ids once on the argument of a run-scoped verb.
fn palette_completions(pal: &Palette, runs: &[state::RunSummary]) -> Vec<String> {
    if !pal.on_arg() {
        let head = pal.head();
        return PALETTE_CMDS
            .iter()
            .filter(|c| c.name.starts_with(&head))
            .map(|c| c.name.to_string())
            .collect();
    }
    let cmd = PALETTE_CMDS.iter().find(|c| c.name == pal.head());
    if cmd.map(|c| c.needs_run).unwrap_or(false) {
        let arg = pal.input.split_whitespace().nth(1).unwrap_or("");
        return runs
            .iter()
            .filter(|r| r.id.starts_with(arg))
            .map(|r| r.id.clone())
            .collect();
    }
    Vec::new()
}

/// Keys while the `:` palette is open. Returns `Ok(true)` only when a command quits.
fn handle_palette_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    swarm: &SparPaths,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
) -> Result<bool> {
    match code {
        KeyCode::Esc => {
            app.palette = None;
        }
        KeyCode::Enter => {
            let input = app
                .palette
                .as_ref()
                .map(|p| p.input.clone())
                .unwrap_or_default();
            if input.trim().is_empty() {
                app.palette = None;
                return Ok(false);
            }
            match run_palette(app, swarm, runs, full, &input) {
                Ok(PaletteResult::Quit) => return Ok(true),
                Ok(PaletteResult::Help) => {
                    app.palette = None;
                    app.show_help = true;
                }
                Ok(PaletteResult::Flash(msg, color)) => {
                    app.palette = None;
                    app.flash(msg, color);
                }
                Err(e) => {
                    // Keep the palette open so the operator can fix the line.
                    app.flash(format!("{e:#}"), RED);
                }
            }
        }
        KeyCode::Tab => {
            let comps = app
                .palette
                .as_ref()
                .map(|p| palette_completions(p, runs))
                .unwrap_or_default();
            if let Some(pal) = app.palette.as_mut() {
                if let Some(pick) = comps.get(pal.sel).or_else(|| comps.first()) {
                    if pal.on_arg() {
                        let head = pal
                            .input
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        pal.input = format!("{head} {pick}");
                    } else {
                        pal.input = format!("{pick} ");
                    }
                    pal.sel = 0;
                }
            }
        }
        KeyCode::Up => {
            if let Some(pal) = app.palette.as_mut() {
                pal.sel = pal.sel.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            let n = app
                .palette
                .as_ref()
                .map(|p| palette_completions(p, runs).len())
                .unwrap_or(0);
            if let Some(pal) = app.palette.as_mut() {
                if pal.sel + 1 < n {
                    pal.sel += 1;
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(pal) = app.palette.as_mut() {
                pal.input.pop();
                pal.sel = 0;
            }
        }
        KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
            if let Some(pal) = app.palette.as_mut() {
                pal.input.push(c);
                pal.sel = 0;
            }
        }
        _ => {}
    }
    Ok(false)
}

/// Split a run-scoped verb's argument into `(run_id, rest)`. A first token that
/// matches a known run id (or unique prefix) is consumed as the id; otherwise the
/// selected run is used and the whole argument is the remainder (e.g. a reject reason).
fn split_run_arg<'a>(
    runs: &[state::RunSummary],
    selected: Option<&'a str>,
    arg: &'a str,
) -> (Option<String>, String) {
    let mut it = arg.splitn(2, char::is_whitespace);
    let first = it.next().unwrap_or("").trim();
    let rest = it.next().map(str::trim).unwrap_or("").to_string();
    if !first.is_empty() {
        let matches: Vec<&state::RunSummary> =
            runs.iter().filter(|r| r.id.starts_with(first)).collect();
        if matches.len() == 1 {
            return (Some(matches[0].id.clone()), rest);
        }
        if runs.iter().any(|r| r.id == first) {
            return (Some(first.to_string()), rest);
        }
    }
    (selected.map(str::to_string), arg.trim().to_string())
}

/// Execute one palette line. The verb table is the whole surface; `@…` is chat.
fn run_palette(
    app: &mut App,
    swarm: &SparPaths,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    input: &str,
) -> Result<PaletteResult> {
    let line = input.trim();
    if let Some(rest) = line.strip_prefix('@') {
        let run_id = runs.get(app.selected_run).map(|r| r.id.as_str());
        return send_mention(swarm, run_id, rest).map(|m| PaletteResult::Flash(m, GREEN));
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next().map(str::trim).unwrap_or("");
    let selected = runs.get(app.selected_run).map(|r| r.id.as_str());

    match head.as_str() {
        "help" | "?" | "h" => Ok(PaletteResult::Help),
        "quit" | "q" | "exit" => Ok(PaletteResult::Quit),
        "approve" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            workflow::plan::approve(swarm, &id, false)?;
            Ok(PaletteResult::Flash(format!("Approved plan {id}"), GREEN))
        }
        "reject" => {
            let (id, reason) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            let reason = (!reason.is_empty()).then_some(reason);
            workflow::plan::reject(swarm, &id, reason, false)?;
            Ok(PaletteResult::Flash(format!("Rejected plan {id}"), GREEN))
        }
        "ship" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            crate::ship::confirm_ship(swarm, &id, false)?;
            Ok(PaletteResult::Flash(format!("Ship confirmed {id}"), GREEN))
        }
        "confirm" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            run_gate_action(app, swarm, &id, GateAction::ConfirmWinner);
            Ok(PaletteResult::Flash(
                format!("Confirmed winner {id}"),
                GREEN,
            ))
        }
        "reconcile" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            spawn_reconcile(app, swarm, &id);
            Ok(PaletteResult::Flash(
                format!("Reconcile started {id}"),
                ACCENT,
            ))
        }
        "takeover" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            takeover_run(app, &id)
        }
        "implement" => {
            let st = full.ok_or_else(|| anyhow::anyhow!("select a planned run first"))?;
            if st.providers.is_empty() {
                anyhow::bail!("run has no recorded providers — use the CLI");
            }
            let args = [
                "implement".to_string(),
                "--run".to_string(),
                st.id.clone(),
                "--providers".to_string(),
                st.providers.join(","),
            ];
            spawn_detached_workflow(swarm, &args, &format!("Implement started {}", st.id))
        }
        "plan" => {
            if arg.is_empty() {
                anyhow::bail!("usage: plan <task>");
            }
            let st = full.ok_or_else(|| {
                anyhow::anyhow!("select a run to reuse its fleet, or use the CLI")
            })?;
            if st.providers.is_empty() {
                anyhow::bail!("selected run has no providers — use the CLI");
            }
            let args = [
                "plan".to_string(),
                "-t".to_string(),
                arg.to_string(),
                "--providers".to_string(),
                st.providers.join(","),
            ];
            spawn_detached_workflow(swarm, &args, "Plan started")
        }
        "spawn" => {
            let arg = (!arg.is_empty()).then_some(arg);
            let bg = app.bg_tx.clone();
            spawn_agent_command(runs, app.selected_run, arg, bg)
                .map(|m| PaletteResult::Flash(m, GREEN))
        }
        "chat" => {
            let run_id = selected;
            send_mention(swarm, run_id, arg).map(|m| PaletteResult::Flash(m, GREEN))
        }
        other => anyhow::bail!("unknown command: {other} — Tab lists commands"),
    }
}

/// Attach the Shell tab to a run's tmux session (palette `takeover`). Mirrors the
/// rail's Enter-on-agent path but keyed only by run id.
fn takeover_run(app: &mut App, id: &str) -> Result<PaletteResult> {
    let session = tmux::session_name(id);
    if tmux::has_session(&session) {
        app.takeover_target = Some(session);
        app.open_main(MainTab::Shell);
        Ok(PaletteResult::Flash(
            format!("Took over {id} — F12/Ctrl+a d to hand back"),
            GREEN,
        ))
    } else {
        anyhow::bail!("headless run — rerun with --backend tmux to take over")
    }
}

/// Spawn a detached `spar <args>` for a lifecycle command the palette dispatches
/// (plan / implement). Mirrors [`spawn_reconcile`]: null stdio, `SPAR_INTERNAL`.
fn spawn_detached_workflow(
    swarm: &SparPaths,
    args: &[String],
    ok_msg: &str,
) -> Result<PaletteResult> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args(args)
        .arg("--json")
        .current_dir(&swarm.project_root)
        .env("SPAR_INTERNAL", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(PaletteResult::Flash(ok_msg.to_string(), ACCENT))
}

/// Keys while the `/` rail filter is being edited. Enter commits (keeps the filter,
/// hands keys back to rail navigation); Esc clears it; typing narrows live.
fn handle_filter_key(
    app: &mut App,
    code: KeyCode,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    n_slots: usize,
) {
    match code {
        KeyCode::Esc => {
            app.filter = None;
            app.filter_committed = false;
        }
        KeyCode::Enter => {
            if app.filter.as_deref().unwrap_or("").is_empty() {
                app.filter = None;
            }
            app.filter_committed = true;
        }
        KeyCode::Backspace => {
            if let Some(f) = app.filter.as_mut() {
                f.pop();
            }
            snap_selection_to_filter(app, projects, runs, n_slots);
        }
        KeyCode::Char(c) => {
            if let Some(f) = app.filter.as_mut() {
                f.push(c);
            }
            snap_selection_to_filter(app, projects, runs, n_slots);
        }
        _ => {}
    }
}

/// After the filter text changes, move the rail selection onto the first row that
/// still matches so Main never shows a filtered-out run.
fn snap_selection_to_filter(
    app: &mut App,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    n_slots: usize,
) {
    let Some(f) = app.filter.as_deref() else {
        return;
    };
    if f.is_empty() {
        return;
    }
    match app.browse {
        BrowseLevel::Projects => {
            let cur = app.selected_project;
            if let Some(i) = first_project_match(projects, f, cur) {
                app.select_project(i, projects.len());
            }
        }
        BrowseLevel::Runs | BrowseLevel::Agents => {
            let cur = app.selected_run;
            if !run_matches_filter(runs, cur, f) {
                if let Some(i) = (0..runs.len()).find(|i| run_matches_filter(runs, *i, f)) {
                    app.select_run(i, runs.len());
                }
            }
            let _ = n_slots;
        }
    }
}

/// Case-insensitive match of a rail filter against a run's id / task / phase.
fn run_matches_filter(runs: &[state::RunSummary], i: usize, f: &str) -> bool {
    let Some(r) = runs.get(i) else { return false };
    if f.is_empty() {
        return true;
    }
    let f = f.to_ascii_lowercase();
    r.id.to_ascii_lowercase().contains(&f)
        || r.task
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains(&f)
        || format!("{:?}", r.phase).to_ascii_lowercase().contains(&f)
        || r.project_name
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains(&f)
}

/// Case-insensitive match against a project's name / root path.
fn project_matches_filter(projects: &[registry::ProjectEntry], i: usize, f: &str) -> bool {
    let Some(p) = projects.get(i) else {
        return false;
    };
    if f.is_empty() {
        return true;
    }
    let f = f.to_ascii_lowercase();
    p.name
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase()
        .contains(&f)
        || p.root.to_string_lossy().to_ascii_lowercase().contains(&f)
}

/// First project index matching the filter, preferring the current selection.
fn first_project_match(projects: &[registry::ProjectEntry], f: &str, cur: usize) -> Option<usize> {
    if project_matches_filter(projects, cur, f) {
        return Some(cur);
    }
    (0..projects.len()).find(|i| project_matches_filter(projects, *i, f))
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
    // With a filter active the rail hides non-matching rows, so navigation walks the
    // matching indices only. `stepv` maps a source index to its next matching one.
    let filter = app.filter.clone().filter(|f| !f.is_empty());
    match app.browse {
        BrowseLevel::Projects if !projects.is_empty() => {
            let next = match &filter {
                Some(f) => {
                    let m: Vec<usize> = (0..projects.len())
                        .filter(|i| project_matches_filter(projects, *i, f))
                        .collect();
                    step_matched(&m, app.selected_project, delta)
                }
                None => step(app.selected_project, projects.len()),
            };
            app.select_project(next, projects.len());
        }
        BrowseLevel::Runs if !runs.is_empty() => {
            let next = match &filter {
                Some(f) => {
                    let m: Vec<usize> = (0..runs.len())
                        .filter(|i| run_matches_filter(runs, *i, f))
                        .collect();
                    step_matched(&m, app.selected_run, delta)
                }
                None => step(app.selected_run, runs.len()),
            };
            app.select_run(next, runs.len());
        }
        BrowseLevel::Agents if n_slots > 0 => {
            app.select_slot(step(app.selected_slot, n_slots), n_slots);
        }
        _ => {}
    }
}

/// Step within a list of matching indices by `delta`, staying on a match. Falls back
/// to `cur` when nothing matches.
fn step_matched(matched: &[usize], cur: usize, delta: i32) -> usize {
    if matched.is_empty() {
        return cur;
    }
    let pos = matched.iter().position(|&i| i == cur).unwrap_or(0);
    let next = if delta < 0 {
        pos.saturating_sub((-delta) as usize)
    } else {
        (pos + delta as usize).min(matched.len() - 1)
    };
    matched[next]
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
            // With the palette open, a tap outside it closes it; inside is swallowed.
            if app.palette.is_some() {
                if !contains(app.rect_palette, x, y) {
                    app.palette = None;
                }
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
            // Tapping the fleet roll-up token jumps to the next run that needs you.
            if contains(app.rect_attention, x, y) {
                jump_to_attention(app, runs);
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

            if contains(app.rect_rail, x, y) {
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
    /// One status line: breadcrumb + run context + gate cues/buttons (the Driving-mode
    /// banner in driving mode).
    status: Rect,
    /// The drill-down rail. Zero-sized when zoomed, driving, or in narrow while Main
    /// is focused.
    rail: Rect,
    /// The one main area (tab strip lives in its top border in the wide layout).
    main: Rect,
    footer: Rect,
    /// Narrow-mode MainTab strip; zero-sized in the wide layout (the strip is drawn
    /// inside Main's top border there).
    tabs: Rect,
    /// True when the single-column phone layout is active.
    narrow: bool,
}

/// Width breakpoints (Stage C): `<80` Main only (phone/SSH — rail folds away, tab strip
/// on its own row); `80–119` rail + Main; `>=120` rail + a **wider Main** (the primary
/// object gets the extra columns — we never add a fourth box).
const NARROW_WIDTH: u16 = 80;

/// Rail width in the wide layout. Enough for `run id · phase · age`, no more. Fixed
/// across the wide bands so the extra width at `>=120` all lands on Main.
const RAIL_WIDTH: u16 = 24;

/// Chrome budget: 1 status row + 1 footer row. Everything else on screen is content
/// (rail + main). The `:` palette and `/` filter are overlays, not reserved rows.
fn layout_rects(area: Rect, focus: Focus, zoom: bool, driving: bool) -> LayoutRects {
    let narrow = area.width < NARROW_WIDTH;
    // Driving mode drops the narrow tab strip too — the banner + F12 is the whole chrome.
    let strip = if narrow && !driving { 1 } else { 0 };
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),     // status line / driving banner
            Constraint::Length(strip), // narrow MainTab strip
            Constraint::Min(4),        // body: rail + main
            Constraint::Length(1),     // footer
        ])
        .split(area);

    let z = Rect::default();
    // Zoom or driving both hide the rail in place; nothing else on screen moves.
    let hide_rail = zoom || driving;

    if narrow {
        // One column. The rail takes the stage while it is focused; otherwise Main
        // has it. Tapping a tab (or the breadcrumb) moves between the two.
        let (rail, main) = if focus == Focus::Rail && !hide_rail {
            (root[2], z)
        } else {
            (z, root[2])
        };
        return LayoutRects {
            status: root[0],
            rail,
            main,
            footer: root[3],
            tabs: root[1],
            narrow: true,
        };
    }

    let (rail, main) = if hide_rail {
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
        footer: root[3],
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

    let driving = app.driving();
    let lay = layout_rects(area, app.focus, app.zoom, driving);
    // Keep mouse hit regions aligned with the frame actually painted.
    app.rect_status = lay.status;
    app.rect_rail = lay.rail;
    app.rect_main = lay.main;
    app.rect_palette = Rect::default();
    // Rebuilt below by whatever paints this frame.
    app.gate_buttons.clear();
    app.main_tabs.clear();

    if driving {
        draw_driving_banner(f, lay.status, app);
    } else {
        draw_status(f, lay.status, swarm, projects, runs, full, app);
    }
    if lay.narrow && lay.tabs.height > 0 {
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
    draw_footer(f, lay.footer, app, full);

    // The `:` palette floats above the footer; the `/` filter shows inline in the rail.
    if app.palette.is_some() {
        draw_palette(f, area, runs, app);
    }

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
        Phase::AwaitingPlanApproval => ("plan ready — tap Approve · r reject".into(), BG, YELLOW),
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
    // Fleet roll-up: how many runs anywhere want the operator, with the `a` jump hint.
    // This is the "what needs me?" answer that does not depend on which run is selected.
    app.rect_attention = Rect::default();
    if app.browse.in_project() {
        let need = runs_needing_attention(runs);
        if need > 0 {
            let token = format!(" ⚑{need} need you · a ");
            let col: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let x = area.x.saturating_add(col as u16);
            if x < area.right() {
                app.rect_attention = Rect {
                    x,
                    y: area.y,
                    width: (token.chars().count() as u16).min(area.right() - x),
                    height: 1,
                };
            }
            spans.push(Span::styled(
                token,
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
    // While the `/` filter is active, the title becomes the live filter field so the
    // operator can see (and edit) what they are narrowing by.
    let filt = |base: String| -> String {
        match app.filter.as_deref() {
            Some(f) if !app.filter_committed => format!(" /{f}▌ "),
            Some(f) if !f.is_empty() => format!("{base}/{f} "),
            _ => base,
        }
    };
    let (items, title) = match app.browse {
        BrowseLevel::Projects => (
            rail_project_items(projects, app),
            filt(format!(" Projects ({}) ", projects.len())),
        ),
        BrowseLevel::Runs => (
            rail_run_items(runs, app),
            filt(format!(" Runs ({}) ", runs.len())),
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
    let filter = app.filter.as_deref().filter(|f| !f.is_empty());
    projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let sel = i == app.selected_project;
            if let Some(f) = filter {
                if !project_matches_filter(projects, i, f) {
                    return ListItem::new(Line::from(Span::styled(
                        format!("  {}", truncate(p.name.as_deref().unwrap_or("·"), 12)),
                        Style::default().fg(FG_MUTED).dim(),
                    )));
                }
            }
            let name = p.name.as_deref().unwrap_or("·");
            let project_runs = registry::list_project_runs(&p.root).unwrap_or_default();
            let n = project_runs.len();
            // Roll-up: a run that wants the operator makes its whole project fly a ⚑.
            let need = runs_needing_attention(&project_runs);
            let lead = if need > 0 {
                Span::styled("⚑ ", Style::default().fg(YELLOW).bold())
            } else {
                Span::styled(if sel { "› " } else { "  " }, Style::default().fg(ACCENT))
            };
            let mut spans = vec![
                lead,
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
            ];
            if need > 0 {
                spans.push(Span::styled(
                    format!("⚑{need} "),
                    Style::default().fg(YELLOW).bold(),
                ));
            }
            spans.push(Span::styled(
                relative_age(p.last_seen),
                Style::default().fg(FG_MUTED),
            ));
            let line = Line::from(spans);
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
    let filter = app.filter.as_deref().filter(|f| !f.is_empty());
    runs.iter()
        .enumerate()
        .map(|(i, r)| {
            let sel = i == app.selected_run;
            if let Some(f) = filter {
                if !run_matches_filter(runs, i, f) {
                    return ListItem::new(Line::from(Span::styled(
                        format!("  {}", truncate(&r.id, 8)),
                        Style::default().fg(FG_MUTED).dim(),
                    )));
                }
            }
            // Phase reads "review" forever on a run nobody is driving; say so.
            let (phase_text, phase_c) = if r.abandoned {
                (format!("{} ✗", truncate(&phase_label(r.phase), 8)), RED)
            } else {
                (truncate(&phase_label(r.phase), 10), phase_color(r.phase))
            };
            // The lead glyph doubles as the attention marker: a run that wants you flies
            // a ⚑ (yellow gate, red broken); otherwise it is the plain selection caret.
            let lead = match run_attention(r) {
                Attention::Gate => Span::styled("⚑ ", Style::default().fg(YELLOW).bold()),
                Attention::Broken => Span::styled("⚑ ", Style::default().fg(RED).bold()),
                _ => Span::styled(if sel { "› " } else { "  " }, Style::default().fg(ACCENT)),
            };
            let line = Line::from(vec![
                lead,
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
            let act = SlotActivity::observe(
                s,
                app.cfg.timeouts.stall_warn_secs,
                crate::executor::timeout_for_role(&app.cfg, s.role).as_secs(),
                app.heartbeats.get(&s.id).copied(),
            );
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
    // Driving mode recolors the pane border green — structural, not just a label.
    let border = if app.driving() {
        GREEN
    } else if focused {
        BORDER_FOCUS
    } else {
        BORDER
    };

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
            let act = SlotActivity::observe(
                s,
                app.cfg.timeouts.stall_warn_secs,
                crate::executor::timeout_for_role(&app.cfg, s.role).as_secs(),
                app.heartbeats.get(&s.id).copied(),
            );
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

/// The `:` command palette: a floating input line + a live completion menu (verbs, or
/// run ids once on the argument). Anchored to the bottom, above the footer.
fn draw_palette(f: &mut Frame, area: Rect, runs: &[state::RunSummary], app: &mut App) {
    let Some(pal) = app.palette.as_ref() else {
        return;
    };
    let comps = palette_completions(pal, runs);
    // Show up to 8 completions, plus the input row and borders.
    let menu_n = comps.len().min(8) as u16;
    let h = menu_n + 3; // input row + top/bottom border + a hint row
    let w = area.width.clamp(30, 76);
    let x = area.x + 2;
    let y = area.bottom().saturating_sub(h + 1);
    let rect = Rect {
        x,
        y,
        width: w.min(area.width.saturating_sub(4)),
        height: h.min(area.height),
    };
    app.rect_palette = rect;
    f.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " : command ",
            Style::default().fg(ACCENT).bold(),
        ))
        .style(Style::default().bg(BG_RAISED));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }

    let cursor = if (app.tick / 6).is_multiple_of(2) {
        "▌"
    } else {
        " "
    };
    let input_line = Line::from(vec![
        Span::styled(" : ", Style::default().fg(ACCENT).bold()),
        Span::styled(&pal.input, Style::default().fg(FG)),
        Span::styled(cursor, Style::default().fg(ACCENT)),
    ]);

    // The completion menu: verb + hint/help when on the command, run id list on the arg.
    let on_arg = pal.on_arg();
    let mut rows: Vec<Line> = vec![input_line];
    for (i, c) in comps.iter().take(menu_n as usize).enumerate() {
        let selected = i == pal.sel;
        let mark = if selected { "▸ " } else { "  " };
        let base = if selected {
            Style::default().fg(BG).bg(ACCENT_SOFT).bold()
        } else {
            Style::default().fg(FG_DIM)
        };
        let tail = if on_arg {
            String::new()
        } else {
            PALETTE_CMDS
                .iter()
                .find(|pc| pc.name == c)
                .map(|pc| format!("  {} — {}", pc.arg_hint, pc.help))
                .unwrap_or_default()
        };
        rows.push(Line::from(vec![
            Span::styled(format!("{mark}{c}"), base),
            Span::styled(tail, Style::default().fg(FG_MUTED)),
        ]));
    }
    let hint = if on_arg {
        "Tab complete run · Enter run · Esc close"
    } else {
        "Tab complete · ↑↓ pick · Enter run · Esc close"
    };
    rows.push(Line::from(Span::styled(
        hint,
        Style::default().fg(FG_MUTED).italic(),
    )));
    f.render_widget(
        Paragraph::new(rows).style(Style::default().bg(BG_RAISED)),
        inner,
    );
}

/// Driving mode's one-line banner replaces the status line: a loud recolored bar that
/// (with the collapsed rail and recolored pane border) makes the mode structurally
/// obvious — a text label alone is proven insufficient (Raskin).
fn draw_driving_banner(f: &mut Frame, area: Rect, app: &App) {
    let target = app
        .takeover_target
        .as_deref()
        .map(|s| s.strip_prefix("spar-").unwrap_or(s))
        .unwrap_or("workspace shell");
    let left = format!("  ▶ DRIVING · {target} ");
    let right = " keys → agent · F12 / C-a d → spar ";
    let bg = Color::Rgb(20, 60, 40);
    let used = (left.chars().count() + right.chars().count()) as u16;
    let pad = area.width.saturating_sub(used).max(1) as usize;
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(BG).bg(GREEN).bold()),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        Span::styled(right, Style::default().fg(FG).bg(bg)),
    ]);
    f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
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
    let tail = " : cmd · q exit ";
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
            return "tap Approve · r reject · :approve · a next alert";
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
            BrowseLevel::Projects => "j/k · Enter open · / filter · : cmd · 2 main · ? help",
            BrowseLevel::Runs => "j/k · Enter agents · a next-alert · / filter · : cmd · ? help",
            BrowseLevel::Agents => "j/k · Enter take over · a next-alert · Esc runs · : cmd",
        },
        Focus::Main => match tab {
            MainTab::Log => "scroll · [ ] tabs · w wrap · g/G top/end · + zoom · 1 rail",
            MainTab::Activity => "scroll · [ ] tabs · g/G top/end · 1 rail",
            MainTab::Diff => "scroll · [ ] tabs · 1 rail",
            MainTab::Shell => "tmux passthrough · prefix C-a · Ctrl+a d / F12 → spar",
        },
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
           attention-sorted: gates and broken runs float to the top.\n\
    Main   one area · tabs: Log · Activity · Diff · Shell\n\
    Main always shows the rail's selection — nothing else moves.\n\
 \n\
  Keyboard\n\
    1 / 2                focus Rail · Main\n\
    Tab / Shift-Tab      cycle Rail ↔ Main\n\
    j k  or  ↑ ↓         move in the rail · scroll Main\n\
    Enter                push a rail level (on an agent: take it over)\n\
    Esc                  pop a rail level · clear filter (never quits)\n\
    [ ]                  previous / next Main tab\n\
    + / _                zoom Main fullscreen / restore\n\
    p                    jump to Projects\n\
    a                    jump to the next run that needs you\n\
    r / s                reject · ship (when gated; approve = tap / :approve)\n\
    :                    command palette (approve/ship/takeover/…)\n\
    /                    filter the rail\n\
    w                    log wrap ↔ truncate long lines\n\
    g / G                top / bottom of Main\n\
    ?                    this help · Esc closes help\n\
    q                    quit\n\
 \n\
  Shell tab = a real tmux client: every key goes to the agent (incl.\n\
    Ctrl+C). prefix C-a · Ctrl+a d or F12 hands focus back to spar.\n\
    Focusing it full-screen is Driving mode (green banner + border).\n\
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

/// Palette `chat`/`@<agent> <message>` — send a directed bus chat from the human to a
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
    heartbeats: &std::collections::HashMap<String, DateTime<Utc>>,
    cfg: &Config,
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
        let act = SlotActivity::observe(
            s,
            cfg.timeouts.stall_warn_secs,
            crate::executor::timeout_for_role(cfg, s.role).as_secs(),
            heartbeats.get(&s.id).copied(),
        );
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

/// How loudly a run wants the operator's eyes. Derived from the run summary alone
/// (cheap — no per-run full-state load), it drives the attention-sorted rail, the
/// status roll-up, and the `a` jump. Ordering matters: higher = louder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Attention {
    Idle = 0,    // Done / Stopped — nothing to do
    Working = 1, // actively running
    Broken = 2,  // abandoned / failed / stuck / escalated / quota
    Gate = 3,    // a human decision is blocking the run right now
}

impl Attention {
    /// A run at or above this wants the operator; below it is just progress.
    fn needs_you(self) -> bool {
        self >= Attention::Broken
    }
}

/// Attention level for one run, from its summary.
fn run_attention(r: &state::RunSummary) -> Attention {
    if r.phase.is_gate() {
        return Attention::Gate;
    }
    if r.abandoned
        || matches!(
            r.phase,
            Phase::Failed | Phase::Stuck | Phase::Escalated | Phase::Quota
        )
    {
        return Attention::Broken;
    }
    if is_active_phase(r.phase) {
        Attention::Working
    } else {
        Attention::Idle
    }
}

/// Order runs for the rail: loudest attention first, then most-recently updated. The
/// sort is applied at the data layer (in the snapshot) so navigation, selection, and
/// rendering all see one order.
fn sort_runs_by_attention(runs: &mut [state::RunSummary]) {
    runs.sort_by(|a, b| {
        run_attention(b)
            .cmp(&run_attention(a))
            .then(b.updated_at.cmp(&a.updated_at))
    });
}

/// How many runs currently want the operator (gate or broken) — the fleet roll-up.
fn runs_needing_attention(runs: &[state::RunSummary]) -> usize {
    runs.iter().filter(|r| run_attention(r).needs_you()).count()
}

/// Flash a toast when a run first crosses into wanting the operator (Working/Idle →
/// Gate/Broken) since the last snapshot. The first snapshot only primes the baseline
/// so the existing fleet is never announced.
fn emit_attention_toasts(app: &mut App, runs: &[state::RunSummary]) {
    let now: Vec<(String, Attention)> = runs
        .iter()
        .map(|r| (r.id.clone(), run_attention(r)))
        .collect();
    if let Some(prev) = app.prev_attention.take() {
        for (id, att) in &now {
            let was = prev
                .iter()
                .find(|(pid, _)| pid == id)
                .map(|(_, a)| *a)
                .unwrap_or(Attention::Idle);
            if att.needs_you() && !was.needs_you() {
                let (what, color) = match att {
                    Attention::Gate => ("needs your decision", YELLOW),
                    _ => ("needs attention", RED),
                };
                app.flash_for(
                    format!("⚠ {} {what} — a to jump", truncate(id, 8)),
                    color,
                    Duration::from_secs(6),
                );
            }
        }
    }
    app.prev_attention = Some(now);
}

/// `a`: jump the rail selection to the next run that wants the operator, cycling from
/// just after the current selection. Lands on the run (rail at the Runs level) so the
/// status line shows its gate/breakage.
fn jump_to_attention(app: &mut App, runs: &[state::RunSummary]) {
    if !app.browse.in_project() {
        app.flash("open a project first", FG_MUTED);
        return;
    }
    let n = runs.len();
    let next = (1..=n).map(|off| (app.selected_run + off) % n).find(|&i| {
        runs.get(i)
            .map(|r| run_attention(r).needs_you())
            .unwrap_or(false)
    });
    match next {
        Some(i) => {
            app.selected_run = i;
            app.browse = BrowseLevel::Runs;
            app.focus = Focus::Rail;
            app.reset_stream_view();
            let id = runs.get(i).map(|r| r.id.as_str()).unwrap_or("");
            app.flash(format!("→ {} needs you", truncate(id, 8)), YELLOW);
        }
        None => app.flash("nothing needs you", GREEN),
    }
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
        let lay = layout_rects(area, Focus::Main, false, false);
        assert!(!lay.narrow);
        // The tab strip rides in Main's top border, not on its own row.
        assert_eq!(lay.tabs, Rect::default());
        assert_eq!(lay.rail.width, RAIL_WIDTH);
        assert!(lay.main.width > 0);
        // Fixed chrome is exactly 2 rows (status + footer) — the palette is an overlay.
        assert_eq!(lay.status.height, 1);
        assert_eq!(lay.footer.height, 1);
        assert_eq!(lay.rail.height + 2, area.height);
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
        let plain = layout_rects(area, Focus::Main, false, false);
        let zoomed = layout_rects(area, Focus::Main, true, false);
        assert_eq!(zoomed.rail, Rect::default());
        assert_eq!(zoomed.main.x, area.x);
        assert_eq!(zoomed.main.width, area.width);
        // Nothing else relocates.
        assert_eq!(zoomed.status, plain.status);
        assert_eq!(zoomed.footer, plain.footer);
        assert_eq!(zoomed.main.y, plain.main.y);
    }

    #[test]
    fn driving_mode_collapses_the_rail_and_chrome() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let driving = layout_rects(area, Focus::Main, false, true);
        assert_eq!(driving.rail, Rect::default(), "rail collapses when driving");
        assert_eq!(driving.main.width, area.width);
        // Narrow driving drops the tab-strip row too (zero-height).
        let narrow = Rect { width: 60, ..area };
        let nd = layout_rects(narrow, Focus::Main, false, true);
        assert_eq!(nd.tabs.height, 0);
    }

    #[test]
    fn narrow_layout_is_main_only_with_a_tab_strip() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 40,
        };
        let lay = layout_rects(area, Focus::Main, false, false);
        assert!(lay.narrow);
        assert!(lay.tabs.width > 0, "MainTab strip is tappable on a phone");
        assert!(lay.main.width > 0);
        assert_eq!(lay.rail, Rect::default(), "no rail in narrow");
        // Rail focus swaps the single stage to the rail; the tab strip stays.
        let rail = layout_rects(area, Focus::Rail, false, false);
        assert!(rail.rail.width > 0);
        assert_eq!(rail.main, Rect::default());
        assert!(rail.tabs.width > 0);
    }

    #[test]
    fn focus_ring_is_two_wide() {
        assert_eq!(Focus::Rail.next(), Focus::Main);
        assert_eq!(Focus::Main.next(), Focus::Rail);
        assert_eq!(Focus::Rail.prev(), Focus::Main);
        assert_eq!(Focus::Main.prev(), Focus::Rail);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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
        let mut app = App::new(None, Config::default(), true);
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

    fn summary(id: &str, task: Option<&str>) -> state::RunSummary {
        use crate::cli::WorkflowKind;
        state::RunSummary {
            id: id.to_string(),
            workflow: WorkflowKind::Loop,
            phase: Phase::Review,
            updated_at: Utc::now(),
            task: task.map(str::to_string),
            dry_run: false,
            abandoned: false,
            project_root: None,
            project_name: None,
        }
    }

    #[test]
    fn palette_completes_verbs_then_run_ids() {
        let runs = [summary("3f2a", None), summary("9c11", None)];
        // On the verb: prefix-filtered command names.
        let mut pal = Palette {
            input: "app".into(),
            sel: 0,
        };
        assert_eq!(
            palette_completions(&pal, &runs),
            vec!["approve".to_string()]
        );
        // Past the space on a run-scoped verb: run ids matching the arg.
        pal.input = "approve 9".into();
        assert_eq!(palette_completions(&pal, &runs), vec!["9c11".to_string()]);
        // A verb that takes no run offers no id completions.
        pal.input = "help ".into();
        assert!(palette_completions(&pal, &runs).is_empty());
    }

    #[test]
    fn split_run_arg_picks_known_id_else_selected() {
        let runs = [summary("3f2a", None), summary("9c11", None)];
        // A leading token that is a known id is consumed; the rest is the reason.
        let (id, rest) = split_run_arg(&runs, Some("3f2a"), "9c11 too risky");
        assert_eq!(id.as_deref(), Some("9c11"));
        assert_eq!(rest, "too risky");
        // A leading token that is NOT an id falls back to the selected run.
        let (id, rest) = split_run_arg(&runs, Some("3f2a"), "too risky");
        assert_eq!(id.as_deref(), Some("3f2a"));
        assert_eq!(rest, "too risky");
        // Empty arg → selected run, empty reason.
        let (id, rest) = split_run_arg(&runs, Some("3f2a"), "");
        assert_eq!(id.as_deref(), Some("3f2a"));
        assert_eq!(rest, "");
    }

    #[test]
    fn run_filter_matches_id_and_task() {
        let runs = [summary("3f2a", Some("wire up auth")), summary("9c11", None)];
        assert!(run_matches_filter(&runs, 0, "auth"));
        assert!(run_matches_filter(&runs, 0, "3f"));
        assert!(!run_matches_filter(&runs, 1, "auth"));
        // Empty filter matches everything.
        assert!(run_matches_filter(&runs, 1, ""));
    }

    #[test]
    fn step_matched_walks_only_matches() {
        // matches at source indices 1 and 3; stepping from 1 forward lands on 3.
        let matched = [1usize, 3];
        assert_eq!(step_matched(&matched, 1, 1), 3);
        assert_eq!(step_matched(&matched, 3, -1), 1);
        // Clamps at the ends.
        assert_eq!(step_matched(&matched, 3, 1), 3);
        assert_eq!(step_matched(&matched, 1, -1), 1);
        // Selection not in the matched set starts at the first match.
        assert_eq!(step_matched(&matched, 0, 1), 3);
    }

    #[test]
    fn slash_opens_filter_and_esc_clears_it() {
        let mut app = App::new(None, Config::default(), true);
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");
        // `/` opens the filter editor with focus on the rail.
        handle_key(
            &mut app,
            KeyCode::Char('/'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            None,
            &mut root,
            None,
        )
        .unwrap();
        assert_eq!(app.filter.as_deref(), Some(""));
        assert!(!app.filter_committed);
        assert!(app.editing_text());
        // Typing narrows; Esc drops the filter entirely.
        handle_key(
            &mut app,
            KeyCode::Char('a'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            None,
            &mut root,
            None,
        )
        .unwrap();
        assert_eq!(app.filter.as_deref(), Some("a"));
        handle_key(
            &mut app,
            KeyCode::Esc,
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            None,
            &mut root,
            None,
        )
        .unwrap();
        assert!(app.filter.is_none());
    }

    #[test]
    fn colon_opens_palette_and_q_quits() {
        let mut app = App::new(None, Config::default(), true);
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");
        // q quits from a normal context.
        let quit = handle_key(
            &mut app,
            KeyCode::Char('q'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            None,
            &mut root,
            None,
        )
        .unwrap();
        assert!(quit, "q is the quit path");
        // `:` opens the palette; then keys route to it (q types, does not quit).
        handle_key(
            &mut app,
            KeyCode::Char(':'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            None,
            &mut root,
            None,
        )
        .unwrap();
        assert!(app.palette.is_some());
        let quit = handle_key(
            &mut app,
            KeyCode::Char('q'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            None,
            &mut root,
            None,
        )
        .unwrap();
        assert!(!quit, "q inside the palette types, never quits");
        assert_eq!(app.palette.as_ref().map(|p| p.input.as_str()), Some("q"));
    }

    fn summary_phase(id: &str, phase: Phase) -> state::RunSummary {
        state::RunSummary {
            phase,
            ..summary(id, None)
        }
    }

    #[test]
    fn attention_ranks_gate_over_broken_over_working() {
        assert_eq!(
            run_attention(&summary_phase("a", Phase::AwaitingPlanApproval)),
            Attention::Gate
        );
        assert_eq!(
            run_attention(&summary_phase("a", Phase::Failed)),
            Attention::Broken
        );
        assert_eq!(
            run_attention(&summary_phase("a", Phase::Review)),
            Attention::Working
        );
        assert_eq!(
            run_attention(&summary_phase("a", Phase::Done)),
            Attention::Idle
        );
        // An abandoned running run reads as Broken, not Working.
        let mut ab = summary_phase("a", Phase::Review);
        ab.abandoned = true;
        assert_eq!(run_attention(&ab), Attention::Broken);
        assert!(Attention::Gate > Attention::Broken);
        assert!(Attention::Broken.needs_you() && !Attention::Working.needs_you());
    }

    #[test]
    fn sort_floats_gates_and_broken_to_the_top() {
        let mut runs = vec![
            summary_phase("work", Phase::Review),
            summary_phase("gate", Phase::AwaitingShipConfirm),
            summary_phase("idle", Phase::Done),
            summary_phase("brok", Phase::Stuck),
        ];
        sort_runs_by_attention(&mut runs);
        let order: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(order, vec!["gate", "brok", "work", "idle"]);
        assert_eq!(runs_needing_attention(&runs), 2);
    }

    #[test]
    fn a_jumps_to_next_run_that_needs_you() {
        let runs = vec![
            summary_phase("r0", Phase::Review),
            summary_phase("r1", Phase::Review),
            summary_phase("r2", Phase::AwaitingPlanApproval),
        ];
        let mut app = App::new(None, Config::default(), true);
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        jump_to_attention(&mut app, &runs);
        assert_eq!(app.selected_run, 2, "lands on the gated run");
        // From the gate it wraps and, finding no other, stays put.
        jump_to_attention(&mut app, &runs);
        assert_eq!(app.selected_run, 2);
    }

    #[test]
    fn toasts_prime_silently_then_fire_on_transition() {
        let mut app = App::new(None, Config::default(), true);
        // First snapshot only primes: an existing gate is NOT toasted.
        let runs = vec![summary_phase("r0", Phase::AwaitingPlanApproval)];
        emit_attention_toasts(&mut app, &runs);
        assert!(app.flash.is_none(), "initial fleet is never toasted");
        // A run that was working and is now working: still silent.
        let runs = vec![summary_phase("r0", Phase::Review)];
        emit_attention_toasts(&mut app, &runs);
        assert!(app.flash.is_none());
        // Now it crosses into a gate: toast fires.
        let runs = vec![summary_phase("r0", Phase::AwaitingPlanApproval)];
        emit_attention_toasts(&mut app, &runs);
        assert!(app.flash.is_some(), "gate transition toasts");
    }
}
