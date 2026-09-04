//! Product shell — clear fleet dashboard for multi-agent runs.
use crate::config::Config;
use crate::events;
use crate::liveness::SlotActivity;
use crate::paths::{self, SparPaths};
use crate::process;
use crate::quota::QuotaStore;
use crate::registry;
use crate::state::{self, Phase, RunState, SlotRole, SlotState, SlotStatus};
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
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Widget, Wrap,
};
use std::collections::HashMap;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use tui_term::widget::PseudoTerminal;

use crate::theme::{
    chip, dim, muted, rule, selected, ACCENT, ACCENT_SOFT, ALERT, ALERT_WASH, DRIVE_WASH, FG,
    FG_DIM, FG_MUTED, GATE_WASH, HINT, INFO, INK, OK, RULE, WARN,
};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Chrome glyphs. One border language: a thin rule under the chrome bands, a thin
/// seam between rail and Main, and a heavy underline marking the active tab.
const RULE_H: &str = "─";
const RULE_SEAM: &str = "│";
const RULE_TEE: &str = "┬";
const TAB_MARK: &str = "━";
/// The rail's selection bar. Replaces the old raised-background row highlight, which
/// needed a page background to sit on.
const SEL_BAR: &str = "▌";

/// Two focus targets, not an N-way ring: the drill-down rail and the one main
/// area. `1` / `2` jump straight to one; `Tab` / `BackTab` cycles between them
/// (see U1). The `:` palette is a transient overlay, not a third ring member.
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
        help: "start a plan (fleet picker, or the selected run's fleet)",
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
    // Canonicalized to match how the registry stores project roots
    // (`registry::register`): `find_project_root` returns `SPAR_PROJECT_ROOT`
    // verbatim when set, which spar exports to every agent it spawns, so an agent
    // running the TUI is the likely path that would otherwise mismatch and leave
    // `HomeScope::Project` filtering every row out (Home renders empty with no
    // explanation why).
    let local_root = paths::find_project_root()
        .ok()
        .map(|r| registry::canonicalize_best_effort(&r));
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

/// The rail is one drill-down tree, rooted at Home: `Home ▸ runs ▸ agents`, with
/// `Projects` reachable as navigation (`p`, or a Home row) rather than the root.
/// `Enter` pushes a level, `Esc` pops one (and never exits the app at the root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseLevel {
    /// The cross-project landing view — the rail root (U7/U18).
    Home,
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
        matches!(self, BrowseLevel::Runs | BrowseLevel::Agents)
    }
    fn pop(self) -> Self {
        match self {
            BrowseLevel::Agents => BrowseLevel::Runs,
            BrowseLevel::Runs => BrowseLevel::Home,
            BrowseLevel::Projects => BrowseLevel::Home,
            BrowseLevel::Home => BrowseLevel::Home,
        }
    }
}

/// Home's four bands, in fixed display order (U7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeBand {
    NeedsMe,
    Running,
    Finished,
    StartNew,
}

/// Home's four bands flattened into one list, headers included so they are always
/// present even when a band is empty (U14: reserve space from the layout, never the
/// content). Built off-thread in `build_snapshot` — `draw` only ever consumes this.
// `run: state::RunSummary` makes `Run` the largest variant by a wide margin, but
// boxing it would break the test contract's direct-construction call sites — Home
// rows never number in the thousands per frame (`HOME_BAND_CAP`), so the copy cost
// is not worth that.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
enum HomeRow {
    Header(HomeBand),
    Run {
        band: HomeBand,
        run: state::RunSummary,
        waited: Duration,
    },
    /// A capped band's tail: how many more rows it is not showing.
    More {
        band: HomeBand,
        n: usize,
    },
    /// Index into the snapshot's `projects` — band 4's project switcher.
    Project(usize),
    /// Band 4's action row — opens the Phase D new-run surface.
    NewRun,
}

/// Home's scope: everything registered, or one project. Filters which rows land in
/// the bands; it never changes the bands themselves (U20).
#[derive(Debug, Clone, PartialEq, Eq)]
enum HomeScope {
    All,
    Project(PathBuf),
}

/// Per-project roll-up for the rail's Projects level and Home's project rows,
/// index-aligned with `Snapshot::projects`. Computed off-thread because `draw`
/// never scans (U13).
#[derive(Debug, Clone, Copy, Default)]
struct ProjectStat {
    n_runs: usize,
    needs_you: usize,
}

/// Home's off-thread roll-up: the flattened band rows plus the per-project stats the
/// Projects level and Home's project rows both need. One `Snapshot` field.
#[derive(Default)]
struct HomeData {
    rows: Vec<HomeRow>,
    project_stats: Vec<ProjectStat>,
}

/// Per-band cap on rendered rows, so a thousand-run workspace does not build a
/// thousand `ListItem`s a frame. Band 1 (`NeedsMe`) is exempt — the cap must never
/// hide something that wants the operator.
const HOME_BAND_CAP: usize = 50;

/// Field the Phase D new-run modal is currently editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewRunField {
    Project,
    Task,
    Fleet,
}

/// Where a roster entry came from — shown so the operator knows why it is (or is
/// not) selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterSource {
    /// Listed in `spar.toml`'s `[providers] order`.
    Configured,
    /// Found on `PATH` by `providers::detect_all()` but not configured.
    Detected,
    /// The most recently run's recorded fleet, offered as one row.
    RecentFleet,
}

/// What picking a roster row adds to the fleet: one provider, or (for a recent-fleet
/// row) several at once.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RosterChoice {
    Provider(String),
    Fleet(Vec<String>),
}

/// One row in the fleet picker.
#[derive(Debug, Clone)]
struct RosterEntry {
    choice: RosterChoice,
    label: String,
    available: bool,
    reason: Option<String>,
    source: RosterSource,
}

/// Phase D's new-run modal: a one-line task (U16's manual seam — the brief file is
/// 008's half) plus a fleet picker over the provider roster (U8/U21/U22).
struct NewRun {
    /// The target project. `None` means no launchable target (R5/AC-32).
    project: Option<PathBuf>,
    /// Cycleable project choices when Home's scope is `All`.
    projects: Vec<PathBuf>,
    task: String,
    /// Snapshotted once, when the modal opens (U22) — never touched again in `draw`.
    roster: Vec<RosterEntry>,
    /// Indices into `roster`, in the order they were picked.
    picked: Vec<usize>,
    field: NewRunField,
    /// Cursor row within the roster list.
    sel: usize,
    /// True while the background roster probe (`detect_all`, a registry read) is
    /// still in flight — `draw` shows "checking roster" instead of an empty list.
    loading: bool,
    /// Tags which `open_new_run`/`begin_new_run` call this modal belongs to, so a
    /// slow probe from a cancelled or reopened modal cannot clobber a later one
    /// (D2 — the `Msg::RosterReady` guard).
    gen: u64,
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
    /// Loaded once at startup; supplies `stall_warn_secs` and each role's soft budget,
    /// which is the stall arm's second threshold.
    cfg: Config,
    /// Freshest process heartbeat per slot id, refreshed from the snapshot each frame.
    /// Feeds stall detection so a busy-but-log-quiet slot isn't flagged as stalled.
    heartbeats: std::collections::HashMap<String, DateTime<Utc>>,
    /// When false (default), long log lines truncate with …; `w` toggles wrap.
    log_expand: bool,
    last_click: Option<(u16, u16, Instant)>,
    show_help: bool,
    /// Scroll offset into the help overlay; reset whenever help is (re)opened.
    help_scroll: u16,
    /// Whether the current frame is part of an animation; drives the spinner so
    /// it shows a static glyph when idle instead of a frame frozen mid-spin.
    animated: bool,
    /// One status line carrying the breadcrumb; tapping it returns focus to the rail.
    rect_status: Rect,
    /// The drill-down rail (zero-sized when zoomed, or in narrow while Main is focused).
    rect_rail: Rect,
    /// The one main area.
    rect_main: Rect,
    /// Main's content rect: `rect_main` minus its left padding. What the embedded
    /// terminal is sized to and what mouse forwarding is measured against.
    rect_main_inner: Rect,
    /// The `:` palette overlay rect (for click-to-dismiss); zero-sized when closed.
    rect_palette: Rect,
    /// Per-tab hit rects for the Main tab strip (wide: in Main's top border; narrow: its own row).
    /// Padded for touch in narrow (half of each neighboring gap), so not the same as
    /// the painted glyph span — `main_tab_glyphs` below is the one to underline.
    main_tabs: Vec<(Rect, MainTab)>,
    /// Per-tab painted glyph rects for the Main tab strip, unpadded. `draw_rule` reads
    /// this for the active-tab underline; using `main_tabs` there would stretch the
    /// accent into the touch-target padding on either side of the label (narrow band).
    main_tab_glyphs: Vec<(Rect, MainTab)>,
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
    /// Rail cursor at the Home level. Rebuilt every snapshot; see `home_key`.
    selected_home: usize,
    /// Whether Home shows every registered project's rows or one project's. `P` toggles.
    home_scope: HomeScope,
    /// "Finished since last look" boundary — read once at startup, held for the whole
    /// session so band 3 does not empty under the operator while they are looking (U19).
    home_watermark: DateTime<Utc>,
    /// Identity of the row `selected_home` points at (not its index — Home re-ranks
    /// every snapshot). `resync_home_selection` uses this to follow the row across
    /// rebuilds instead of a position that can slide out from under the cursor (R3).
    home_key: Option<String>,
    /// Set by `rail_enter` on a Home run row so the one-tick snapshot handoff into
    /// `Agents` selects the row's own run, not whatever the rail happened to have.
    home_target_run: Option<String>,
    /// Phase D's new-run modal. `Some` = open and capturing keys.
    new_run: Option<NewRun>,
    /// Bumped every time the new-run modal opens; tags `NewRun::gen` and
    /// `Msg::RosterReady` so a stale background probe cannot land in a later modal.
    new_run_gen: u64,
    /// The new-run overlay's outer rect (for click-outside-to-cancel); zero-sized when
    /// closed. Mirrors `rect_palette`.
    rect_new_run: Rect,
    /// Painted roster row rects this frame as `(roster_index, rect)`, for
    /// click-to-toggle. Only as many entries as were actually rendered (post-cap,
    /// post-scroll).
    rect_new_run_roster: Vec<(usize, Rect)>,
}

/// A gate action reachable by both a key and a tappable button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateAction {
    Approve,
    Reject,
    Ship,
    ConfirmWinner,
    Reconcile,
    /// Lift the round ceiling and re-dispatch (O52). The only gate whose resolution is
    /// a *number*, which a button cannot ask for — it buys a fixed four more rounds,
    /// and the CLI's `--max-rounds N` stays the way to name an exact one.
    MoreRounds,
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
    fn new(task_seed: Option<String>, cfg: Config, local_root: Option<&Path>) -> Self {
        let home_scope = match local_root {
            Some(root) => HomeScope::Project(root.to_path_buf()),
            None => HomeScope::All,
        };
        // A launch task seed opens the new-run surface pre-filled (U3/U21) — it no
        // longer opens the palette, which had no way to offer a fresh fleet. The
        // roster itself is *not* built here: `bg_tx` doesn't exist yet at this point
        // in startup, and `detect_all()` spawns every provider with `--version` (D2)
        // — that cannot sit on the startup path. `run_loop` kicks off the probe once
        // the channel is wired, or a `spar --task` launch opens a surface with
        // nothing to pick and no key that can populate it (AC-33).
        let new_run = task_seed.map(|t| {
            let all_projects: Vec<PathBuf> =
                registry::projects().into_iter().map(|p| p.root).collect();
            let project = local_root
                .map(Path::to_path_buf)
                .or_else(|| all_projects.first().cloned());
            pending_new_run(project, all_projects, t, NewRunField::Task, 1)
        });
        let new_run_gen = if new_run.is_some() { 1 } else { 0 };
        Self {
            selected_run: 0,
            selected_project: 0,
            selected_slot: 0,
            focus: Focus::Rail,
            // Home is always the landing view (U7/U18); Projects survives as
            // navigation, reachable with `p` or a Home row.
            browse: BrowseLevel::Home,
            main_tab: MainTab::Log,
            zoom: false,
            palette: None,
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
            help_scroll: 0,
            animated: false,
            rect_status: Rect::default(),
            rect_rail: Rect::default(),
            rect_main: Rect::default(),
            rect_main_inner: Rect::default(),
            rect_palette: Rect::default(),
            main_tabs: Vec::new(),
            main_tab_glyphs: Vec::new(),
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
            selected_home: 0,
            home_scope,
            home_watermark: read_watermark(&watermark_path()),
            home_key: None,
            home_target_run: None,
            new_run,
            new_run_gen,
            rect_new_run: Rect::default(),
            rect_new_run_roster: Vec::new(),
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

    /// Back to the landing view. `Projects`/`Runs`/`Agents` all pop here eventually.
    fn open_home(&mut self) {
        self.browse = BrowseLevel::Home;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Rail;
    }

    /// `Esc` in the rail: pop one level. At `Home` this is a no-op — the rail root
    /// is never an exit (U18).
    fn rail_pop(&mut self) {
        if self.browse == BrowseLevel::Home {
            return;
        }
        let next = self.browse.pop();
        if next == BrowseLevel::Home {
            self.open_home();
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
    /// it never scrolls from here (its input is forwarded raw). Without a run
    /// selected, Activity and Diff fall back to the same overview body Log uses
    /// (`draw_log_body`), so scrolling must follow that body — `stream_*` — rather
    /// than the run-scoped `bus_*`/`diff_*` state those tabs normally own.
    fn scroll_main_by(&mut self, delta: i32, has_full: bool) {
        match self.main_tab {
            MainTab::Log => self.scroll_stream_by(delta),
            MainTab::Activity if has_full => self.scroll_bus_by(delta),
            MainTab::Activity => self.scroll_stream_by(delta),
            MainTab::Diff if has_full => self.scroll_diff_by(delta),
            MainTab::Diff => self.scroll_stream_by(delta),
            MainTab::Shell => {}
        }
    }

    fn main_page(&self, has_full: bool) -> u16 {
        match self.main_tab {
            MainTab::Activity if has_full => self.bus_page(),
            MainTab::Diff if has_full => self.diff_page(),
            _ => self.stream_page(),
        }
    }

    fn home_for_main(&mut self, has_full: bool) {
        match self.main_tab {
            MainTab::Activity if has_full => {
                self.bus_follow = false;
                self.bus_scroll = 0;
            }
            MainTab::Diff if has_full => {
                self.diff_follow = false;
                self.diff_scroll = 0;
            }
            _ => {
                self.stream_follow = false;
                self.stream_scroll = 0;
            }
        }
    }

    fn end_for_main(&mut self, has_full: bool) {
        match self.main_tab {
            MainTab::Activity if has_full => {
                self.bus_follow = true;
                self.bus_scroll = self.bus_max;
            }
            MainTab::Diff if has_full => {
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

/// Test fixture: an `App` pinned to the **Runs** level, which is where every
/// pre-Home render test means to be. `App::new` now always lands on `Home`
/// (feature 004 Phase C), so a test that wants a project's run list has to say so.
#[cfg(test)]
fn test_app() -> App {
    let mut app = App::new(None, Config::default(), Some(Path::new("/x")));
    app.browse = BrowseLevel::Runs;
    app
}

/// Test fixture: the Phase D new-run modal, open on a real target project with a
/// two-entry roster and one provider picked. Shared by `render_stability` (overlay
/// sweep) and `home_ia` (picker semantics).
#[cfg(test)]
fn new_run_fixture() -> NewRun {
    NewRun {
        project: Some(PathBuf::from("/nonexistent/spar")),
        projects: vec![
            PathBuf::from("/nonexistent/spar"),
            PathBuf::from("/nonexistent/acme-api"),
        ],
        task: "stop prose mentions creating phantom criteria".into(),
        roster: vec![
            RosterEntry {
                choice: RosterChoice::Provider("cli:claude@opus".into()),
                label: "cli:claude@opus".into(),
                available: true,
                reason: None,
                source: RosterSource::Configured,
            },
            RosterEntry {
                choice: RosterChoice::Provider("cli:codex".into()),
                label: "cli:codex".into(),
                available: true,
                reason: None,
                source: RosterSource::Detected,
            },
        ],
        picked: vec![0],
        field: NewRunField::Fleet,
        sel: 0,
        loading: false,
        gen: 1,
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
    home_scope: HomeScope,
    home_watermark: DateTime<Utc>,
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
    /// Home's bands and the per-project roll-up, built off-thread (U13/B).
    home: HomeData,
}

enum Msg {
    Input(Event),
    Data,
    /// A status line pushed from a background task (e.g. `/spawn`'s deferred
    /// spawn+deliver), flashed on the next render tick.
    Flash(String, Color),
    /// The new-run modal's background roster probe landed (D2). The `u64` is the
    /// `NewRun::gen` it was built for — applied only if the open modal is still on
    /// that generation, so a cancelled or reopened modal can't be clobbered.
    RosterReady(u64, Vec<RosterEntry>),
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

/// How often the refresher is allowed to sweep every registered project's run dirs
/// for Home/Projects. Far coarser than `REFRESH`: at "thousands of run dirs" scale
/// (the scale that has already bitten once, per the IA doc) a per-200ms sweep across
/// every project is a permanent stat storm (U23).
const CROSS_PROJECT_REFRESH: Duration = Duration::from_secs(2);

/// Whether the refresher should re-sweep every registered project's marks right now.
/// Only Home and Projects are cross-project; `Runs`/`Agents` are scoped to one
/// project and must never trigger the sweep, no matter how long it has been.
fn cross_project_due(browse: BrowseLevel, since_last: Duration, forced: bool) -> bool {
    if !matches!(browse, BrowseLevel::Home | BrowseLevel::Projects) {
        return false;
    }
    forced || since_last >= CROSS_PROJECT_REFRESH
}

/// Marks for every registered project's run listing — the cross-project half of
/// `marks_for`, called at `CROSS_PROJECT_REFRESH` cadence rather than every tick.
fn cross_project_marks(projects: &[registry::ProjectEntry]) -> Marks {
    let mut out = Vec::new();
    for p in projects {
        let swarm = SparPaths::new(&p.root);
        let runs_dir = swarm.runs_dir();
        out.push(stamp(&runs_dir));
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
    }
    out
}

/// One row per unit of work (U15). A run with `parent_run` set is a **leg**: it folds
/// into its root's row. The row keeps the root's brief — that is the human-readable
/// identity of the work — but takes the id, phase and age of the group's *active*
/// member, so gate buttons, `:approve` and drill-down all act on the run that actually
/// holds the state. Attention rolls up loudest-first: folding must never hide a gate.
///
/// Returns the folded rows and, for each row, every run id it stands for.
fn fold_units(
    runs: Vec<state::RunSummary>,
) -> (Vec<state::RunSummary>, HashMap<String, Vec<String>>) {
    let parents: HashMap<String, Option<String>> = runs
        .iter()
        .map(|r| (r.id.clone(), r.parent_run.clone()))
        .collect();
    // Resolve to a root, tolerating a parent that is archived or gone: an orphan leg
    // stands on its own rather than vanishing from the list.
    let root_of = |id: &str| -> String {
        let mut cur = id.to_string();
        for _ in 0..16 {
            match parents.get(&cur).and_then(|p| p.clone()) {
                Some(p) if parents.contains_key(&p) => cur = p,
                _ => break,
            }
        }
        cur
    };

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<state::RunSummary>> = HashMap::new();
    for r in runs {
        let root = root_of(&r.id);
        if !groups.contains_key(&root) {
            order.push(root.clone());
        }
        groups.entry(root).or_default().push(r);
    }

    let mut out = Vec::with_capacity(order.len());
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for root in order {
        let mut group = groups.remove(&root).unwrap_or_default();
        if group.len() == 1 {
            let r = group.pop().expect("len 1");
            members.insert(r.id.clone(), vec![r.id.clone()]);
            out.push(r);
            continue;
        }
        // Loudest attention first, then most recent: the active member is whatever the
        // operator would want the row to be about.
        group.sort_by(|a, b| {
            run_attention(b)
                .cmp(&run_attention(a))
                .then(b.updated_at.cmp(&a.updated_at))
        });
        let ids: Vec<String> = group.iter().map(|r| r.id.clone()).collect();
        // Every leg that wants the operator still counts. Two gates folded into one
        // row must read as two in the roll-up, or folding hides one of them.
        let wants = group
            .iter()
            .filter(|r| run_attention(r).needs_you())
            .count() as u32;
        let brief = group
            .iter()
            .find(|r| r.id == root)
            .and_then(|r| r.task.clone());
        let newest = group.iter().map(|r| r.updated_at).max();
        let mut row = group.swap_remove(0);
        if brief.is_some() {
            row.task = brief;
        }
        if let Some(t) = newest {
            row.updated_at = t;
        }
        row.abandoned = group.iter().any(|r| r.abandoned) || row.abandoned;
        row.legs = ids.len() as u32;
        row.wants = wants;
        members.insert(row.id.clone(), ids);
        out.push(row);
    }
    (out, members)
}

/// All blocking filesystem work lives here, never on the render thread.
fn build_snapshot(sel: &Selection, cache: &mut LogCache, cfg: &Config) -> Snapshot {
    let swarm = SparPaths::new(&sel.root);
    let _ = PROJECT_PREFIX.set(sel.root.to_string_lossy().into_owned());
    let projects = registry::projects();
    let runs = if sel.browse.in_project() {
        let listed = registry::list_visible_project_runs(&sel.root).unwrap_or_default();
        // One row per unit of work, then attention-sorted: gates and broken runs float
        // to the top (Stage C, U15). Drilling in stays scoped to the leg the row acts
        // on — merging the other legs' slots into the view put agents, worktrees and
        // tmux windows from one run under another run's id, which is how a takeover
        // types into the wrong pane.
        let (mut runs, _) = fold_units(listed);
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
    // Home and Projects both need the per-project roll-up (U13/B); only Home needs
    // the flattened band rows, which Projects has no use for.
    let home = if matches!(sel.browse, BrowseLevel::Home | BrowseLevel::Projects) {
        let folded = gather_home(&projects);
        let project_stats = project_stats_of(&folded);
        let rows = if sel.browse == BrowseLevel::Home {
            build_home_rows(
                &projects,
                &folded,
                &sel.home_scope,
                sel.home_watermark,
                Utc::now(),
            )
        } else {
            Vec::new()
        };
        HomeData {
            rows,
            project_stats,
        }
    } else {
        HomeData::default()
    };
    let stream_text = if sel.browse.in_project() {
        stream_content(&swarm, full.as_ref(), sel.slot_idx, cache, !runs.is_empty())
    } else if sel.browse == BrowseLevel::Home {
        cache.clear();
        home_overview(&home.rows, &sel.home_scope, sel.home_watermark)
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
        home,
    }
}

/// Disk half of U13/B: one folded, archived-filtered run listing per registered
/// project, index-aligned with `projects`. A missing project root degrades to an
/// empty listing rather than panicking. Called off-thread only.
fn gather_home(projects: &[registry::ProjectEntry]) -> Vec<Vec<state::RunSummary>> {
    projects
        .iter()
        .map(|p| {
            let listed = registry::list_visible_project_runs(&p.root).unwrap_or_default();
            let (folded, _) = fold_units(listed);
            folded
        })
        .collect()
}

/// Pure roll-up over an already-folded, per-project listing: run count and how many
/// legs want the operator (U15's `wants`, not the row count).
fn project_stats_of(folded: &[Vec<state::RunSummary>]) -> Vec<ProjectStat> {
    folded
        .iter()
        .map(|runs| ProjectStat {
            n_runs: runs.len(),
            needs_you: runs_needing_attention(runs),
        })
        .collect()
}

/// A run's wait — how long it has sat since its last update, clamped to zero for a
/// clock that ran backwards rather than sorting a skewed timestamp to the top forever.
fn home_wait(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (now - updated_at).to_std().unwrap_or(Duration::ZERO)
}

fn push_home_band(
    rows: &mut Vec<HomeRow>,
    band: HomeBand,
    runs: &[state::RunSummary],
    now: DateTime<Utc>,
    cap: Option<usize>,
) {
    let shown = match cap {
        Some(c) => runs.len().min(c),
        None => runs.len(),
    };
    for r in &runs[..shown] {
        rows.push(HomeRow::Run {
            band,
            run: r.clone(),
            waited: home_wait(r.updated_at, now),
        });
    }
    if let Some(c) = cap {
        if runs.len() > c {
            rows.push(HomeRow::More {
                band,
                n: runs.len() - c,
            });
        }
    }
}

/// Pure banding/ranking/capping (C): first match wins, so a run lands in exactly one
/// band. `NeedsMe` is never capped (U5/AC-21); `Finished` is bounded by `watermark`
/// (U19); band membership is declared per phase, not a fallthrough (AC-18).
fn build_home_rows(
    projects: &[registry::ProjectEntry],
    folded: &[Vec<state::RunSummary>],
    scope: &HomeScope,
    watermark: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<HomeRow> {
    let mut needs_me: Vec<state::RunSummary> = Vec::new();
    let mut running: Vec<state::RunSummary> = Vec::new();
    let mut finished: Vec<state::RunSummary> = Vec::new();

    for (i, runs) in folded.iter().enumerate() {
        let Some(proj) = projects.get(i) else {
            continue;
        };
        if let HomeScope::Project(root) = scope {
            if &proj.root != root {
                continue;
            }
        }
        for r in runs {
            // `run_attention` already folds `r.abandoned` into `Broken` (needs_you).
            if run_attention(r).needs_you() {
                needs_me.push(r.clone());
            } else if is_active_phase(r.phase) {
                running.push(r.clone());
            } else if matches!(r.phase, Phase::Done | Phase::Stopped | Phase::PlanRejected)
                && r.updated_at > watermark
            {
                finished.push(r.clone());
            }
        }
    }

    // Band 1: longest wait first. Bands 2/3: most recently updated first.
    needs_me.sort_by_key(|r| std::cmp::Reverse(home_wait(r.updated_at, now)));
    running.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    finished.sort_by_key(|r| std::cmp::Reverse(r.updated_at));

    let mut rows = Vec::new();
    rows.push(HomeRow::Header(HomeBand::NeedsMe));
    push_home_band(&mut rows, HomeBand::NeedsMe, &needs_me, now, None);
    rows.push(HomeRow::Header(HomeBand::Running));
    push_home_band(
        &mut rows,
        HomeBand::Running,
        &running,
        now,
        Some(HOME_BAND_CAP),
    );
    rows.push(HomeRow::Header(HomeBand::Finished));
    push_home_band(
        &mut rows,
        HomeBand::Finished,
        &finished,
        now,
        Some(HOME_BAND_CAP),
    );
    rows.push(HomeRow::Header(HomeBand::StartNew));
    rows.push(HomeRow::NewRun);
    for (i, proj) in projects.iter().enumerate() {
        if let HomeScope::Project(root) = scope {
            if &proj.root != root {
                continue;
            }
        }
        rows.push(HomeRow::Project(i));
    }
    rows
}

fn home_band_label(b: HomeBand) -> &'static str {
    match b {
        HomeBand::NeedsMe => "NEEDS YOU",
        HomeBand::Running => "RUNNING",
        HomeBand::Finished => "FINISHED SINCE YOUR LAST LOOK",
        HomeBand::StartNew => "START SOMETHING NEW",
    }
}

fn home_band_empty_text(b: HomeBand) -> &'static str {
    match b {
        HomeBand::NeedsMe => "nothing needs you",
        HomeBand::Running => "nothing running",
        HomeBand::Finished => "nothing finished since your last look",
        HomeBand::StartNew => "",
    }
}

fn home_scope_label(scope: &HomeScope) -> String {
    match scope {
        HomeScope::All => "all projects".to_string(),
        HomeScope::Project(p) => p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string()),
    }
}

/// How many runs need the operator at Home — a folded row with `legs > 1` still
/// counts every leg that wants attention (U15), matching `runs_needing_attention`.
fn home_needs_you(rows: &[HomeRow]) -> usize {
    rows.iter()
        .filter_map(|r| match r {
            HomeRow::Run {
                band: HomeBand::NeedsMe,
                run,
                ..
            } => Some(if run.legs > 1 { run.wants as usize } else { 1 }),
            _ => None,
        })
        .sum()
}

/// Total rows in `band`, including what a per-band cap trimmed (`HomeRow::More`'s
/// `n`) — otherwise the context band's count undercounts a capped band while
/// `home_needs_you` (uncapped) reads exact, mixing exact and truncated numbers on
/// the same line.
fn home_band_count(rows: &[HomeRow], band: HomeBand) -> usize {
    rows.iter()
        .filter_map(|r| match r {
            HomeRow::Run { band: b, .. } if *b == band => Some(1),
            HomeRow::More { band: b, n } if *b == band => Some(*n),
            _ => None,
        })
        .sum()
}

/// Main's Home body: the four bands, headers always present, each empty band saying
/// so on its own line (U14's reserved-space rule applied to Home).
fn home_overview(rows: &[HomeRow], scope: &HomeScope, watermark: DateTime<Utc>) -> String {
    let mut out = format!("\n  Home · {}\n\n", home_scope_label(scope));
    let mut i = 0;
    while i < rows.len() {
        let HomeRow::Header(band) = rows[i] else {
            i += 1;
            continue;
        };
        // The Finished band's header names the watermark it is bounded by, so the
        // operator knows what "last look" means without opening a project (AC-25).
        let suffix = if band == HomeBand::Finished {
            format!(" (since {})", relative_age(watermark))
        } else {
            String::new()
        };
        out.push_str(&format!("  {}{suffix}\n", home_band_label(band)));
        let mut j = i + 1;
        let mut any = false;
        while j < rows.len() && !matches!(rows[j], HomeRow::Header(_)) {
            match &rows[j] {
                HomeRow::Run { run, waited, .. } => {
                    any = true;
                    let flag = if run.wants > 1 {
                        format!(" ⚑{}", run.wants)
                    } else {
                        String::new()
                    };
                    out.push_str(&format!(
                        "    {} · {} · {} · waited {}{flag}\n",
                        run.project_name.as_deref().unwrap_or("?"),
                        truncate(&run.id, 8),
                        rail_phase(run.phase),
                        relative_wait(*waited),
                    ));
                }
                HomeRow::More { n, .. } => {
                    any = true;
                    out.push_str(&format!("    … {n} more\n"));
                }
                HomeRow::NewRun => {
                    any = true;
                    out.push_str("    n — start something new\n");
                }
                HomeRow::Project(_) => {}
                HomeRow::Header(_) => unreachable!(),
            }
            j += 1;
        }
        if !any {
            let t = home_band_empty_text(band);
            if !t.is_empty() {
                out.push_str(&format!("    {t}\n"));
            }
        }
        out.push('\n');
        i = j;
    }
    out
}

fn relative_wait(d: Duration) -> String {
    let secs = d.as_secs();
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

/// Identity of a Home row, for `resync_home_selection` to follow the cursor across a
/// re-ranked snapshot instead of a position (R3).
fn home_row_key(row: &HomeRow) -> String {
    match row {
        HomeRow::Header(b) => format!("hdr:{b:?}"),
        HomeRow::Run { run, .. } => format!("run:{}", run.id),
        HomeRow::More { band, .. } => format!("more:{band:?}"),
        HomeRow::Project(i) => format!("proj:{i}"),
        HomeRow::NewRun => "newrun".to_string(),
    }
}

/// Re-glue the Home cursor to the row it was on, by identity, after a rebuild (R3).
/// A row that vanished clamps to the nearest non-header row rather than indexing out
/// of bounds or landing on a header.
fn resync_home_selection(app: &mut App, rows: &[HomeRow]) {
    if rows.is_empty() {
        app.selected_home = 0;
        app.home_key = None;
        return;
    }
    if let Some(key) = app.home_key.clone() {
        if let Some(i) = rows.iter().position(|r| home_row_key(r) == key) {
            app.selected_home = i;
            return;
        }
    }
    let mut i = app.selected_home.min(rows.len() - 1);
    if matches!(rows[i], HomeRow::Header(_)) {
        if let Some(f) = (i..rows.len()).find(|&j| !matches!(rows[j], HomeRow::Header(_))) {
            i = f;
        } else if let Some(b) = (0..i)
            .rev()
            .find(|&j| !matches!(rows[j], HomeRow::Header(_)))
        {
            i = b;
        }
    }
    app.selected_home = i;
    app.home_key = rows.get(i).map(home_row_key);
}

/// `P`: toggle Home's scope between "everything" and the local project.
fn toggle_home_scope(app: &mut App, local_root: Option<&Path>) {
    app.home_scope = match &app.home_scope {
        HomeScope::All => match local_root {
            Some(root) => HomeScope::Project(root.to_path_buf()),
            None => HomeScope::All,
        },
        HomeScope::Project(_) => HomeScope::All,
    };
}

/// Where the "finished since last look" watermark lives. Cross-project state cannot
/// live in a per-project `.spar/` (U19).
fn watermark_path() -> PathBuf {
    registry::spar_home().join("home_watermark.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WatermarkFile {
    seen_at: DateTime<Utc>,
}

/// A missing or corrupt watermark reads as a day ago — nonfatal, and it only ever
/// makes band 3 show *more*, never fewer.
fn read_watermark(path: &Path) -> DateTime<Utc> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<WatermarkFile>(&s).ok())
        .map(|w| w.seen_at)
        .unwrap_or_else(|| Utc::now() - chrono::Duration::hours(24))
}

fn write_watermark(path: &Path, at: DateTime<Utc>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(&WatermarkFile { seen_at: at })?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Build the fleet picker's roster (D2). Pure: `detected` is the result of
/// `providers::detect_all()` already resolved by the caller — this never touches
/// `PATH` itself (U22, U13). Parse failures are listed first (so a broken config
/// entry is not lost among usable ones), then configured entries in their
/// `spar.toml` order, then whatever `detect_all` found that config did not already
/// list, then the most recent run's fleet as one row.
fn build_roster(
    cfg: &Config,
    detected: &[(String, bool)],
    recent: Option<(&str, &[String])>,
) -> Vec<RosterEntry> {
    let mut invalid = Vec::new();
    let mut valid: Vec<crate::provider_ref::ProviderRef> = Vec::new();
    for raw in &cfg.providers.order {
        match crate::provider_ref::ProviderRef::parse(raw) {
            Ok(r) => valid.push(r),
            Err(e) => invalid.push(RosterEntry {
                choice: RosterChoice::Provider(raw.clone()),
                label: raw.clone(),
                available: false,
                reason: Some(e.to_string()),
                source: RosterSource::Configured,
            }),
        }
    }
    let mut roster = invalid;
    let mut configured_native: Vec<String> = Vec::new();
    for r in &valid {
        let (available, reason) = if r.is_api() {
            (true, None)
        } else {
            match detected.iter().find(|(n, _)| n == &r.name) {
                Some((_, true)) => (true, None),
                _ => (false, Some("not on PATH".to_string())),
            }
        };
        if let Some(name) = r.cli_name() {
            configured_native.push(name.to_string());
        }
        roster.push(RosterEntry {
            choice: RosterChoice::Provider(r.display()),
            label: r.display(),
            available,
            reason,
            source: RosterSource::Configured,
        });
    }
    for (name, avail) in detected {
        if configured_native.iter().any(|n| n == name) {
            continue;
        }
        roster.push(RosterEntry {
            choice: RosterChoice::Provider(format!("cli:{name}")),
            label: format!("cli:{name}"),
            available: *avail,
            reason: (!avail).then(|| "not on PATH".to_string()),
            source: RosterSource::Detected,
        });
    }
    if let Some((run_id, providers)) = recent {
        // A recent fleet is only as launchable as its least available member — the
        // same rule a directly-picked ref already gets (AC-30); otherwise picking
        // this row could build a `--providers` argv with a ref that isn't on PATH.
        let missing = providers
            .iter()
            .find(|p| !provider_ref_available(p, detected));
        let (available, reason) = match missing {
            None => (true, None),
            Some(p) => (false, Some(format!("{p} not on PATH"))),
        };
        roster.push(RosterEntry {
            choice: RosterChoice::Fleet(providers.to_vec()),
            label: format!("reuse {}'s fleet", truncate(run_id, 8)),
            available,
            reason,
            source: RosterSource::RecentFleet,
        });
    }
    roster
}

/// Whether a provider ref (as recorded in a run's `providers` list) is currently
/// usable, by the same rule a configured roster entry gets: `api:` refs need no
/// PATH lookup, native refs need to be in `detected` and available.
fn provider_ref_available(raw: &str, detected: &[(String, bool)]) -> bool {
    match crate::provider_ref::ProviderRef::parse(raw) {
        Ok(r) if r.is_api() => true,
        Ok(r) => detected.iter().any(|(n, avail)| *n == r.name && *avail),
        Err(_) => false,
    }
}

/// Expand `picked` roster indices into provider refs, in pick order, deduplicated
/// keeping the first occurrence — a `Fleet` choice can repeat a provider a `Provider`
/// choice already picked.
fn new_run_providers(nr: &NewRun) -> Vec<String> {
    let mut out = Vec::new();
    for &idx in &nr.picked {
        let Some(entry) = nr.roster.get(idx) else {
            continue;
        };
        match &entry.choice {
            RosterChoice::Provider(p) => out.push(p.clone()),
            RosterChoice::Fleet(v) => out.extend(v.iter().cloned()),
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// Validate the new-run surface, then build the argv `run_palette`'s `plan` arm
/// already sends. The `--base` resolution stays at the call site (it needs the
/// operator's actual cwd, which this pure function does not have).
fn new_run_launch(nr: &NewRun) -> std::result::Result<(PathBuf, Vec<String>), String> {
    let target = nr.project.clone().ok_or_else(|| {
        "no project to launch into — open spar in a project or choose a registered project"
            .to_string()
    })?;
    if nr.task.trim().is_empty() {
        return Err("task cannot be empty".to_string());
    }
    if nr.picked.is_empty() {
        return Err("pick at least one provider".to_string());
    }
    for &idx in &nr.picked {
        match nr.roster.get(idx) {
            Some(e) if e.available => {}
            Some(e) => {
                return Err(format!(
                    "{} is not available: {}",
                    e.label,
                    e.reason.as_deref().unwrap_or("unavailable")
                ))
            }
            None => return Err("invalid roster selection".to_string()),
        }
    }
    let providers = new_run_providers(nr);
    if providers.is_empty() {
        return Err("no providers resolved".to_string());
    }
    let argv = vec![
        "plan".to_string(),
        "-t".to_string(),
        nr.task.trim().to_string(),
        "--providers".to_string(),
        providers.join(","),
    ];
    Ok((target, argv))
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
/// the palette/filter cursor, or a run that is actively working (active phase or a
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
    let mut app = App::new(task_seed, cfg.clone(), local_root.as_deref());
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
        home_scope: app.home_scope.clone(),
        home_watermark: app.home_watermark,
    };

    // First paint needs data, so build one snapshot synchronously.
    let mut cache = LogCache::empty();
    let snapshot = Arc::new(Mutex::new(Arc::new(build_snapshot(&sel, &mut cache, &cfg))));

    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let (sel_tx, sel_rx) = mpsc::channel::<Selection>();
    app.bg_tx = Some(msg_tx.clone());

    // `App::new`'s `--task` seed opened the modal before `bg_tx` existed, so its
    // roster probe was deferred (D2) — kick it off now that the channel is wired.
    if let Some(nr) = app.new_run.as_ref() {
        if nr.loading {
            let gen = nr.gen;
            let cfg = app.cfg.clone();
            let tx = msg_tx.clone();
            thread::spawn(move || {
                let roster = compute_new_run_roster(&cfg);
                let _ = tx.send(Msg::RosterReady(gen, roster));
            });
        }
    }

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
        let mut cross_marks = Marks::new();
        // Fire the cross-project sweep on the very first loop iteration.
        let mut last_cross = Instant::now() - CROSS_PROJECT_REFRESH;
        let mut prev_cross_key = (sel.browse, sel.home_scope.clone());
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

            let cross_key = (sel.browse, sel.home_scope.clone());
            let forced_cross = forced && cross_key != prev_cross_key;
            prev_cross_key = cross_key;

            let prev = Arc::clone(&*slot.lock().unwrap());
            let next_marks = marks_for(&sel, Some(&prev));
            let mut rebuild = forced || next_marks != marks;
            marks = next_marks;

            if cross_project_due(sel.browse, last_cross.elapsed(), forced_cross) {
                let next_cross = cross_project_marks(&registry::projects());
                if forced_cross || next_cross != cross_marks {
                    rebuild = true;
                }
                cross_marks = next_cross;
                last_cross = Instant::now();
            }

            if !rebuild {
                continue; // nothing on disk moved; don't rebuild, don't repaint
            }

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
        } else if let Some(target) = app.home_target_run.as_deref() {
            // A Home Enter carries a run id ahead of the snapshot that actually
            // contains it (R2/AC-27): the snapshot in hand may still be Home's
            // (cross-project, `snap.runs` empty) or a stale project's. Hold the
            // target and only clear it once a snapshot arrives whose `runs`
            // actually contains it; until then the id-glue clamp below is
            // skipped so a stale `sel.run_id` cannot steal the selection.
            if let Some(pos) = snap.runs.iter().position(|r| r.id == target) {
                app.selected_run = pos;
                app.home_target_run = None;
            }
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
        // fleet transition is noticed even while looking at another run. At Home
        // there is no `snap.runs` (it is cross-project) — feed it Home's rows instead
        // so the toast still fires there.
        if app.browse == BrowseLevel::Home {
            let home_runs: Vec<state::RunSummary> = snap
                .home
                .rows
                .iter()
                .filter_map(|r| match r {
                    HomeRow::Run { run, .. } => Some(run.clone()),
                    _ => None,
                })
                .collect();
            emit_attention_toasts(&mut app, &home_runs);
        } else {
            emit_attention_toasts(&mut app, &snap.runs);
        }
        let n_slots = snap.full.as_ref().map(|s| s.slots.len()).unwrap_or(0);
        app.selected_slot = if n_slots == 0 {
            0
        } else {
            app.selected_slot.min(n_slots - 1)
        };
        if app.browse == BrowseLevel::Home {
            resync_home_selection(&mut app, &snap.home.rows);
        }

        rail_state.select(match app.browse {
            BrowseLevel::Home if !snap.home.rows.is_empty() => Some(app.selected_home),
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
                    &snap.home,
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
            Ok(Msg::RosterReady(gen, roster)) => {
                apply_roster_ready(&mut app, gen, roster);
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
                                &snap.home.rows,
                                &snap.runs,
                                snap.full.as_ref(),
                                &mut active_root,
                                local_root.as_deref(),
                            )? {
                                let _ = write_watermark(&watermark_path(), Utc::now());
                                return Ok(crate::exit_codes::ExitCode::Success);
                            }
                        }
                        Event::Mouse(m) => handle_mouse(
                            &mut app,
                            m,
                            &snap.swarm,
                            &snap.projects,
                            &snap.home.rows,
                            &snap.runs,
                            snap.full.as_ref(),
                            &mut active_root,
                            local_root.as_deref(),
                            rail_state.offset(),
                        ),
                        // Forward a paste to the tmux client as bracketed paste.
                        Event::Paste(text) if app.shell_active() => {
                            if let Some(pane) = app.terminal_pane.as_ref() {
                                let mut buf = Vec::with_capacity(text.len() + 12);
                                buf.extend_from_slice(b"\x1b[200~");
                                buf.extend_from_slice(text.as_bytes());
                                buf.extend_from_slice(b"\x1b[201~");
                                pane.write_input(&buf);
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
                        Ok(Msg::RosterReady(gen, roster)) => {
                            apply_roster_ready(&mut app, gen, roster);
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
                let _ = write_watermark(&watermark_path(), Utc::now());
                return Ok(crate::exit_codes::ExitCode::Success);
            }
        }

        let next_sel = Selection {
            browse: app.browse,
            root: active_root.clone(),
            // A Home Enter carries the run by id (`home_target_run`), because the
            // outgoing snapshot's `snap.runs`/`app.selected_run` still describe the
            // *previous* project. Keep resending it (not `take()`) until the clamp
            // above observes a snapshot that actually contains it and clears it —
            // otherwise this send races that clamp and can overwrite the target
            // with `None` a tick early (R2/AC-27).
            run_id: app
                .home_target_run
                .clone()
                .or_else(|| snap.runs.get(app.selected_run).map(|r| r.id.clone())),
            slot_idx: app.selected_slot,
            project_idx: app.selected_project,
            home_scope: app.home_scope.clone(),
            home_watermark: app.home_watermark,
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
    let n_runs = registry::list_visible_project_runs(&p.root)
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
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
    local_root: Option<&std::path::Path>,
) -> Result<bool> {
    let selected_id = runs.get(app.selected_run).map(|r| r.id.as_str());
    let n_slots = full.map(|s| s.slots.len()).unwrap_or(0);

    // The Phase D new-run modal owns every key while it is open, same precedence as
    // the `:` palette.
    if app.new_run.is_some() {
        handle_new_run_key(app, code, mods);
        return Ok(false);
    }

    // The `:` palette owns every key while it is open — including Enter (run), Tab
    // (complete), and Esc (close). It can only open when not in the Shell tab, so it
    // never contends with the agent pane.
    if app.palette.is_some() {
        return handle_palette_key(app, code, mods, swarm, projects, local_root, runs, full);
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
            KeyCode::Char('j') | KeyCode::Down => {
                app.help_scroll = app.help_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.help_scroll = app.help_scroll.saturating_sub(1);
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
        // app (at Home, the root, it does nothing).
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
                rail_enter(
                    app,
                    projects,
                    home_rows,
                    runs,
                    full,
                    active_root,
                    local_root,
                );
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
        KeyCode::Char('n') => {
            open_new_run(app, projects, home_rows, local_root);
        }
        KeyCode::Char('P') => {
            toggle_home_scope(app, local_root);
            app.flash(
                format!("Home scope: {}", home_scope_label(&app.home_scope)),
                ACCENT,
            );
        }
        KeyCode::Char('j') | KeyCode::Down => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, 1),
            Focus::Main => app.scroll_main_by(3, full.is_some()),
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, -1),
            Focus::Main => app.scroll_main_by(-3, full.is_some()),
        },
        KeyCode::PageDown => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, 5),
            Focus::Main => {
                app.scroll_main_by(i32::from(app.main_page(full.is_some())), full.is_some())
            }
        },
        KeyCode::PageUp => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, -5),
            Focus::Main => {
                app.scroll_main_by(-i32::from(app.main_page(full.is_some())), full.is_some())
            }
        },
        // a jumps to the next run that wants you (Stage C). Approve moved to the gate
        // button / `:approve` when `a` became the fleet-wide attention binding.
        KeyCode::Char('a') => jump_to_attention(app, runs, home_rows),
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
            app.home_for_main(full.is_some());
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.end_for_main(full.is_some());
        }
        KeyCode::Char('?') => {
            app.show_help = true;
            app.help_scroll = 0;
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

/// `n`: open the Phase D new-run surface. The target project follows the scope
/// (D2): a scoped Home defaults to its project, an all-project Home defaults to the
/// selected row's project (falling back to the local repo), and the picker offers
/// every registered project to cycle through.
fn open_new_run(
    app: &mut App,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    local_root: Option<&Path>,
) {
    let target = match &app.home_scope {
        HomeScope::Project(root) => Some(root.clone()),
        HomeScope::All => home_rows
            .get(app.selected_home)
            .and_then(|r| match r {
                HomeRow::Run { run, .. } => run.project_root.clone(),
                HomeRow::Project(i) => projects.get(*i).map(|p| p.root.clone()),
                _ => None,
            })
            .or_else(|| local_root.map(Path::to_path_buf)),
    };
    let all_projects: Vec<PathBuf> = projects.iter().map(|p| p.root.clone()).collect();
    let project = target.or_else(|| all_projects.first().cloned());
    begin_new_run(app, project, all_projects, String::new(), NewRunField::Task);
}

/// A `NewRun` in its initial "checking roster" state — no disk or process I/O, so
/// this is safe to call before the background channel exists (`App::new`'s task
/// seed, before `run_loop` wires `bg_tx`).
fn pending_new_run(
    project: Option<PathBuf>,
    projects: Vec<PathBuf>,
    task: String,
    field: NewRunField,
    gen: u64,
) -> NewRun {
    NewRun {
        project,
        projects,
        task,
        roster: Vec::new(),
        picked: Vec::new(),
        field,
        sel: 0,
        loading: true,
        gen,
    }
}

/// Open the modal in its "checking roster" state, then hand the disk/process work
/// (`detect_all`, a registry read) to a background thread so `n`, `:plan` and startup
/// never block on a slow or stuck provider probe (D2). The probe is tagged with a
/// fresh `gen`; `Msg::RosterReady` only applies if the modal it was built for is still
/// open and still on that generation, so a cancelled or reopened modal can't be
/// clobbered by a stale result. With no `bg_tx` (before the channel is wired, or in a
/// test) the probe runs inline instead of being silently lost.
fn begin_new_run(
    app: &mut App,
    project: Option<PathBuf>,
    all_projects: Vec<PathBuf>,
    task: String,
    field: NewRunField,
) {
    app.new_run_gen += 1;
    let gen = app.new_run_gen;
    app.new_run = Some(pending_new_run(project, all_projects, task, field, gen));
    match app.bg_tx.clone() {
        Some(tx) => {
            let cfg = app.cfg.clone();
            thread::spawn(move || {
                let roster = compute_new_run_roster(&cfg);
                let _ = tx.send(Msg::RosterReady(gen, roster));
            });
        }
        None => {
            let roster = compute_new_run_roster(&app.cfg);
            if let Some(nr) = app.new_run.as_mut() {
                nr.roster = roster;
                nr.loading = false;
            }
        }
    }
}

/// The roster and recent-fleet lookup: disk/process work (`detect_all`, a registry
/// read) that must never run on the input thread (U13/D2) — see `begin_new_run`.
fn compute_new_run_roster(cfg: &Config) -> Vec<RosterEntry> {
    let detected: Vec<(String, bool)> = crate::providers::detect_all()
        .into_iter()
        .map(|r| (r.name, r.available))
        .collect();
    let recent_fleet: Option<(String, Vec<String>)> = registry::projects()
        .into_iter()
        .find_map(|p| p.last_run_id.map(|id| (p.root, id)))
        .and_then(|(root, id)| {
            let paths = SparPaths::new(&root);
            RunState::load(&paths, &id)
                .ok()
                .map(|st| (id, st.providers))
        });
    let recent_ref = recent_fleet
        .as_ref()
        .map(|(id, v)| (id.as_str(), v.as_slice()));
    build_roster(cfg, &detected, recent_ref)
}

/// Apply a background roster probe's result (D2) if the modal it was built for is
/// still open on the same generation; a stale result from a cancelled or reopened
/// modal is dropped.
fn apply_roster_ready(app: &mut App, gen: u64, roster: Vec<RosterEntry>) {
    if let Some(nr) = app.new_run.as_mut() {
        if nr.gen == gen {
            nr.roster = roster;
            nr.loading = false;
        }
    }
}

/// Toggle roster entry `i`'s pick, same rule for the `space` key and a roster-row
/// click: picked → unpicked, unavailable → no-op, else picked.
fn toggle_roster_pick(nr: &mut NewRun, i: usize) {
    if let Some(pos) = nr.picked.iter().position(|&p| p == i) {
        nr.picked.remove(pos);
    } else if nr.roster.get(i).map(|e| e.available).unwrap_or(false) {
        nr.picked.push(i);
    }
}

/// Keys while the Phase D new-run modal is open.
fn handle_new_run_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    if code == KeyCode::Esc {
        app.new_run = None;
        return;
    }
    if code == KeyCode::Enter {
        let Some(nr) = app.new_run.as_ref() else {
            return;
        };
        match new_run_launch(nr) {
            Ok((target, mut args)) => {
                let target_swarm = SparPaths::new(&target);
                if let Ok(cwd) = std::env::current_dir() {
                    if let Ok(Some(base)) =
                        crate::worktree::resolve_base(&target_swarm.project_root, &cwd, None)
                    {
                        args.push("--base".to_string());
                        args.push(base.reference);
                    }
                }
                match spawn_detached_workflow(&target_swarm, &args, "Plan started") {
                    Ok(PaletteResult::Flash(msg, color)) => app.flash(msg, color),
                    Ok(_) => {}
                    Err(e) => app.flash(format!("Plan failed to start: {e:#}"), ALERT),
                }
                app.new_run = None;
            }
            Err(e) => app.flash(e, ALERT),
        }
        return;
    }
    let Some(nr) = app.new_run.as_mut() else {
        return;
    };
    match code {
        KeyCode::Tab => {
            nr.field = match nr.field {
                NewRunField::Project => NewRunField::Task,
                NewRunField::Task => NewRunField::Fleet,
                NewRunField::Fleet => NewRunField::Project,
            };
        }
        KeyCode::BackTab => {
            nr.field = match nr.field {
                NewRunField::Project => NewRunField::Fleet,
                NewRunField::Task => NewRunField::Project,
                NewRunField::Fleet => NewRunField::Task,
            };
        }
        KeyCode::Left if nr.field == NewRunField::Project && !nr.projects.is_empty() => {
            let i = nr
                .project
                .as_ref()
                .and_then(|p| nr.projects.iter().position(|q| q == p))
                .unwrap_or(0);
            let i = if i == 0 { nr.projects.len() - 1 } else { i - 1 };
            nr.project = nr.projects.get(i).cloned();
        }
        KeyCode::Right if nr.field == NewRunField::Project && !nr.projects.is_empty() => {
            let i = nr
                .project
                .as_ref()
                .and_then(|p| nr.projects.iter().position(|q| q == p))
                .unwrap_or(0);
            let i = (i + 1) % nr.projects.len();
            nr.project = nr.projects.get(i).cloned();
        }
        KeyCode::Char(' ') if nr.field == NewRunField::Fleet => toggle_roster_pick(nr, nr.sel),
        KeyCode::Char('j') | KeyCode::Down if nr.field == NewRunField::Fleet => {
            if !nr.roster.is_empty() {
                nr.sel = (nr.sel + 1).min(nr.roster.len() - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up if nr.field == NewRunField::Fleet => {
            nr.sel = nr.sel.saturating_sub(1);
        }
        KeyCode::Backspace if nr.field == NewRunField::Task => {
            nr.task.pop();
        }
        KeyCode::Char(c)
            if nr.field == NewRunField::Task && !mods.contains(KeyModifiers::CONTROL) =>
        {
            nr.task.push(c);
        }
        _ => {}
    }
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
#[allow(clippy::too_many_arguments)]
fn handle_palette_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    local_root: Option<&Path>,
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
            match run_palette(app, swarm, projects, local_root, runs, full, &input) {
                Ok(PaletteResult::Quit) => return Ok(true),
                Ok(PaletteResult::Help) => {
                    app.palette = None;
                    app.show_help = true;
                    app.help_scroll = 0;
                }
                Ok(PaletteResult::Flash(msg, color)) => {
                    app.palette = None;
                    app.flash(msg, color);
                }
                Err(e) => {
                    // Keep the palette open so the operator can fix the line.
                    app.flash(format!("{e:#}"), ALERT);
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
#[allow(clippy::too_many_arguments)]
fn run_palette(
    app: &mut App,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    local_root: Option<&Path>,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    input: &str,
) -> Result<PaletteResult> {
    let line = input.trim();
    if let Some(rest) = line.strip_prefix('@') {
        let run_id = runs.get(app.selected_run).map(|r| r.id.as_str());
        return send_mention(swarm, run_id, rest).map(|m| PaletteResult::Flash(m, OK));
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
            Ok(PaletteResult::Flash(format!("Approved plan {id}"), OK))
        }
        "reject" => {
            let (id, reason) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            let reason = (!reason.is_empty()).then_some(reason);
            workflow::plan::reject(swarm, &id, reason, false)?;
            Ok(PaletteResult::Flash(format!("Rejected plan {id}"), OK))
        }
        "ship" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            crate::ship::confirm_ship(swarm, &id, false)?;
            Ok(PaletteResult::Flash(format!("Ship confirmed {id}"), OK))
        }
        "confirm" => {
            let (id, _) = split_run_arg(runs, selected, arg);
            let id = id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
            run_gate_action(app, swarm, &id, GateAction::ConfirmWinner);
            Ok(PaletteResult::Flash(format!("Confirmed winner {id}"), OK))
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
            let msg = if st.phase == Phase::AwaitingRoundExtension {
                format!("Bought {ROUND_GRANT} rounds for {}", st.id)
            } else {
                format!("Implement started {}", st.id)
            };
            spawn_detached_workflow(swarm, &implement_argv(st), &msg)
        }
        "plan" => {
            if arg.is_empty() {
                anyhow::bail!("usage: plan <task>");
            }
            let Some(st) = full.filter(|st| !st.providers.is_empty()) else {
                // No run to reuse a fleet from — U3's punt is retired: open the
                // new-run surface pre-filled with the typed task instead of erroring
                // to the CLI (U21). Same background probe as `n` (D2), not a
                // hand-rolled roster build, so this path also gets the recent-fleet
                // row `open_new_run` offers.
                //
                // `swarm.project_root` is `active_root`, which falls back to an
                // arbitrary cwd when there is no local repo and the registry is
                // empty (`run_loop`'s init). Only offer it as the target when it is
                // actually a known project — the local repo or a registered one —
                // so the no-target refusal in `new_run_launch` cannot be bypassed by
                // an unregistered directory.
                let all_projects: Vec<PathBuf> = projects.iter().map(|p| p.root.clone()).collect();
                let is_known_project = local_root == Some(swarm.project_root.as_path())
                    || all_projects.iter().any(|r| r == &swarm.project_root);
                let target = is_known_project
                    .then(|| swarm.project_root.clone())
                    .or_else(|| all_projects.first().cloned());
                begin_new_run(
                    app,
                    target,
                    all_projects,
                    arg.to_string(),
                    NewRunField::Fleet,
                );
                return Ok(PaletteResult::Flash(
                    "Pick a fleet to start".to_string(),
                    ACCENT,
                ));
            };
            let mut args = vec![
                "plan".to_string(),
                "-t".to_string(),
                arg.to_string(),
                "--providers".to_string(),
                st.providers.join(","),
            ];
            // The child runs in the project root (the TUI can act on another project's
            // run), so the branch the operator actually started spar in has to be
            // handed over explicitly or the plan is cut from the main checkout.
            if let Ok(cwd) = std::env::current_dir() {
                if let Ok(Some(base)) =
                    crate::worktree::resolve_base(&swarm.project_root, &cwd, None)
                {
                    // The ref, not the sha: a run whose base_ref is its own commit reads
                    // as detached, and `ship` then declines to target the branch.
                    args.push("--base".to_string());
                    args.push(base.reference);
                }
            }
            spawn_detached_workflow(swarm, &args, "Plan started")
        }
        "spawn" => {
            let arg = (!arg.is_empty()).then_some(arg);
            let bg = app.bg_tx.clone();
            spawn_agent_command(runs, app.selected_run, arg, bg)
                .map(|m| PaletteResult::Flash(m, OK))
        }
        "chat" => {
            let run_id = selected;
            send_mention(swarm, run_id, arg).map(|m| PaletteResult::Flash(m, OK))
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
            OK,
        ))
    } else {
        anyhow::bail!("headless run — rerun with --backend tmux to take over")
    }
}

/// Spawn a detached `spar <args>` for a lifecycle command the palette dispatches
/// (plan / implement). Mirrors [`spawn_reconcile`]: null stdio, `SPAR_INTERNAL`.
/// `spar implement --run <id>` argv reusing the run's recorded fleet.
///
/// Carries `--max-rounds` when the run is parked at the round ceiling: without it the
/// detached process gates again the instant it starts and the TUI reports "Implement
/// started" over a run that never moved.
fn implement_argv(st: &RunState) -> Vec<String> {
    let mut args = vec![
        "implement".to_string(),
        "--run".to_string(),
        st.id.clone(),
        "--providers".to_string(),
        st.providers.join(","),
    ];
    if st.phase == Phase::AwaitingRoundExtension {
        args.push("--max-rounds".to_string());
        args.push((st.max_rounds + ROUND_GRANT).to_string());
    }
    args
}

/// How many rounds the TUI's one-tap lift buys. A button cannot ask for a number, and
/// the point of the gate is that each round is expensive — so it grants a few, not a
/// blank cheque. `--max-rounds N` on the CLI is how you name an exact ceiling.
const ROUND_GRANT: u32 = 4;

/// Lift the round ceiling as a detached `spar implement`, so the re-dispatched fleet
/// survives the TUI and never runs on the render thread.
fn spawn_more_rounds(app: &mut App, swarm: &SparPaths, id: &str) {
    let st = match RunState::load(swarm, id) {
        Ok(st) => st,
        Err(e) => {
            app.flash(format!("Buy rounds failed: {e:#}"), ALERT);
            return;
        }
    };
    if st.providers.is_empty() {
        app.flash("run has no recorded providers — use the CLI", ALERT);
        return;
    }
    match spawn_detached_workflow(
        swarm,
        &implement_argv(&st),
        &format!("Bought {ROUND_GRANT} rounds for {id}"),
    ) {
        Ok(PaletteResult::Flash(msg, color)) => app.flash(msg, color),
        Ok(_) => {}
        Err(e) => app.flash(format!("Buy rounds failed: {e:#}"), ALERT),
    }
}

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
        // Home has no `/` filter of its own — the rail filter is a Projects/Runs
        // concept and does not reach the landing view.
        BrowseLevel::Home => {}
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
/// Step the Home rail by `delta`, skipping header rows entirely — the cursor must
/// never land on one, at either end of the list (AC-26).
fn step_home(rows: &[HomeRow], cur: usize, delta: i32) -> usize {
    let selectable: Vec<usize> = (0..rows.len())
        .filter(|&i| !matches!(rows[i], HomeRow::Header(_)))
        .collect();
    if selectable.is_empty() {
        return cur;
    }
    let pos = selectable.iter().position(|&i| i == cur).unwrap_or(0);
    let next = if delta < 0 {
        pos.saturating_sub((-delta) as usize)
    } else {
        (pos + delta as usize).min(selectable.len() - 1)
    };
    selectable[next]
}

fn rail_move(
    app: &mut App,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
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
        BrowseLevel::Home if !home_rows.is_empty() => {
            let next = step_home(home_rows, app.selected_home, delta);
            app.selected_home = next;
            app.home_key = home_rows.get(next).map(home_row_key);
        }
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
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
    local_root: Option<&Path>,
) {
    match app.browse {
        BrowseLevel::Home => match home_rows.get(app.selected_home) {
            Some(HomeRow::Run { run, .. }) => {
                if let Some(root) = run.project_root.clone() {
                    *active_root = root;
                }
                app.home_target_run = Some(run.id.clone());
                app.browse = BrowseLevel::Agents;
                app.selected_slot = 0;
                app.reset_stream_view();
            }
            Some(HomeRow::Project(i)) => {
                if let Some(p) = projects.get(*i) {
                    *active_root = p.root.clone();
                    app.open_project_runs();
                }
            }
            Some(HomeRow::NewRun) => {
                // `active_root` is the rail's current browsing root, not a verified
                // project — outside a repo with an empty registry it degrades to an
                // arbitrary cwd (`run_loop`'s `active_root` init). Route through the
                // same verified `local_root` the `n` key and `open_new_run`'s own
                // `HomeScope::All` fallback use, or the no-target refusal never fires
                // (AC-32).
                open_new_run(app, projects, home_rows, local_root);
            }
            Some(HomeRow::Header(_)) | Some(HomeRow::More { .. }) | None => {}
        },
        BrowseLevel::Projects => {
            if let Some(p) = projects.get(app.selected_project) {
                *active_root = p.root.clone();
                app.open_project_runs();
                app.flash(
                    format!("Opened {}", p.name.as_deref().unwrap_or("project")),
                    OK,
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
                    OK,
                );
            } else {
                app.flash(
                    "headless run — rerun with --backend tmux to take over",
                    WARN,
                );
            }
        }
    }
}

/// Run a gate action from a key or a tapped button — one path for both.
fn run_gate_action(app: &mut App, swarm: &SparPaths, id: &str, action: GateAction) {
    let res = match action {
        GateAction::Approve => {
            workflow::plan::approve(swarm, id, false).map(|_| (format!("Approved plan {id}"), OK))
        }
        GateAction::Reject => workflow::plan::reject(swarm, id, None, false)
            .map(|_| (format!("Rejected plan {id}"), WARN)),
        GateAction::Ship => crate::ship::confirm_ship(swarm, id, false)
            .map(|_| (format!("Ship confirmed {id}"), OK)),
        GateAction::ConfirmWinner => workflow::arena::confirm_winner(swarm, id, None, false)
            .map(|_| (format!("Confirmed winner for {id}"), OK)),
        // Reconcile runs agents (minutes) — never on the render thread.
        GateAction::Reconcile => return spawn_reconcile(app, swarm, id),
        GateAction::MoreRounds => return spawn_more_rounds(app, swarm, id),
    };
    match res {
        Ok((msg, color)) => app.flash(msg, color),
        Err(e) => app.flash(format!("{} failed: {e:#}", action.verb()), ALERT),
    }
}

/// Kick off arena reconcile as a detached `spar reconcile` process so it survives
/// the TUI and keeps agent work off the render loop. Progress shows via the log.
fn spawn_reconcile(app: &mut App, swarm: &SparPaths, id: &str) {
    if let Some((rid, t)) = &app.reconcile_spawn {
        if rid == id && t.elapsed() < Duration::from_secs(15) {
            app.flash("Reconcile already starting…", WARN);
            return;
        }
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            app.flash(format!("Reconcile failed to start: {e}"), ALERT);
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
        Err(e) => app.flash(format!("Reconcile failed to start: {e}"), ALERT),
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
        Some(Phase::AwaitingRoundExtension) => vec![("+4 rounds", GateAction::MoreRounds)],
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
            GateAction::MoreRounds => "Buy rounds",
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
    local_root: Option<&Path>,
    rail_offset: usize,
) {
    let (x, y) = (m.column, m.row);
    let n_slots = full.map(|s| s.slots.len()).unwrap_or(0);
    let n_rail = rail_len(
        app.browse,
        projects.len(),
        home_rows.len(),
        runs.len(),
        n_slots,
    );

    // The help overlay can grow tall enough to sit on top of the tab strip (it sizes
    // to its content, not a fixed box), so it must be hit-tested before the strip or
    // a tap meant to dismiss help silently changes the tab underneath instead.
    if app.show_help {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => app.show_help = false,
            MouseEventKind::ScrollDown => {
                app.help_scroll = app.help_scroll.saturating_add(1);
            }
            MouseEventKind::ScrollUp => {
                app.help_scroll = app.help_scroll.saturating_sub(1);
            }
            _ => {}
        }
        return;
    }

    // The Phase D new-run modal owns every click while it is open, same precedence as
    // the `:` palette (D4): a click on a roster row toggles it, a click outside cancels,
    // and everything else underneath (rail, tabs, gate buttons) is swallowed rather
    // than reached through the overlay.
    if app.new_run.is_some() {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if !contains(app.rect_new_run, x, y) {
                app.new_run = None;
            } else if let Some(i) = app
                .rect_new_run_roster
                .iter()
                .find(|(_, r)| contains(*r, x, y))
                .map(|(i, _)| *i)
            {
                if let Some(nr) = app.new_run.as_mut() {
                    nr.field = NewRunField::Fleet;
                    nr.sel = i;
                    toggle_roster_pick(nr, i);
                }
            }
        }
        return;
    }

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
    // Main still changes focus.
    if app.shell_active() {
        if let Some(pane) = app.terminal_pane.as_ref() {
            let r = app.rect_main_inner;
            if contains(r, x, y) && r.width > 0 && r.height > 0 {
                let max_x = r.right() - 1;
                let max_y = r.bottom() - 1;
                let cx = x.clamp(r.x, max_x) - r.x;
                let cy = y.clamp(r.y, max_y) - r.y;
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
                jump_to_attention(app, runs, home_rows);
                return;
            }
            if contains(app.rect_help, x, y) {
                app.show_help = true;
                app.help_scroll = 0;
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
                    rail_select(app, row, projects.len(), home_rows, runs.len(), n_slots);
                    // Double-click = Enter: drill one level (and take over on a slot).
                    if dbl {
                        rail_enter(
                            app,
                            projects,
                            home_rows,
                            runs,
                            full,
                            active_root,
                            local_root,
                        );
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
                app.scroll_main_by(3, full.is_some());
            } else if contains(app.rect_rail, x, y) {
                app.focus = Focus::Rail;
                rail_move(app, projects, home_rows, runs, n_slots, 1);
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(app.rect_main, x, y) {
                app.focus = Focus::Main;
                app.scroll_main_by(-3, full.is_some());
            } else if contains(app.rect_rail, x, y) {
                app.focus = Focus::Rail;
                rail_move(app, projects, home_rows, runs, n_slots, -1);
            }
        }
        _ => {}
    }
}

/// Row count of the rail at its current level — the list the mouse hit-tests against.
fn rail_len(
    browse: BrowseLevel,
    n_projects: usize,
    n_home: usize,
    n_runs: usize,
    n_slots: usize,
) -> usize {
    match browse {
        BrowseLevel::Home => n_home,
        BrowseLevel::Projects => n_projects,
        BrowseLevel::Runs => n_runs,
        BrowseLevel::Agents => n_slots,
    }
}

/// Select rail row `row` at whatever level the rail is on. At Home a click on any
/// header row (not just row 0) is ignored — a click is a pointer at content, and a
/// header is not content — and a landed selection glues `home_key` to the row's
/// identity, the same as every other Home cursor mover (`rail_move`,
/// `jump_to_attention`), or the very next snapshot yanks the cursor back (AC-28).
fn rail_select(
    app: &mut App,
    row: usize,
    n_projects: usize,
    home_rows: &[HomeRow],
    n_runs: usize,
    n_slots: usize,
) {
    match app.browse {
        BrowseLevel::Home => {
            if matches!(home_rows.get(row), Some(HomeRow::Header(_))) {
                return;
            }
            if row < home_rows.len() {
                app.selected_home = row;
                app.home_key = home_rows.get(row).map(home_row_key);
            }
        }
        BrowseLevel::Projects => app.select_project(row, n_projects),
        BrowseLevel::Runs => app.select_run(row, n_runs),
        BrowseLevel::Agents => app.select_slot(row, n_slots),
    }
}

/// Map a mouse Y to a list row.
/// `offset` is the ListState scroll offset so clicks track the visible window.
fn list_row_at(panel: Rect, y: u16, n_items: usize, offset: usize) -> Option<usize> {
    if n_items == 0 || panel.height == 0 || y < panel.y {
        return None;
    }
    // The rail is borderless: its first row is the first item (the title rides the
    // labels row above), so every row of `panel` is content.
    let inner_y = y - panel.y;
    if inner_y >= panel.height {
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
    /// One header line: breadcrumb + run context + gate cues/buttons (the Driving-mode
    /// banner in driving mode).
    header: Rect,
    /// The run stepper (or, with no run, the project roll-up). Zero-height on short
    /// terminals and in Driving mode.
    context: Rect,
    /// One row carrying the rail's title on the left and the MainTab labels on the
    /// right. Full width; the drawer slices it against `rail` / `main`.
    labels: Rect,
    /// The rule under `labels`: the chrome/content divider, doubling as the active
    /// tab's underline.
    rule: Rect,
    /// The drill-down rail. Zero-sized when zoomed, driving, or in narrow while Main
    /// is focused.
    rail: Rect,
    /// One-column seam between rail and Main. Zero-sized whenever the rail is.
    seam: Rect,
    /// The one main area — content only; its tabs live in `labels`.
    main: Rect,
    footer: Rect,
    /// True when the single-column phone layout is active.
    narrow: bool,
}

/// Width breakpoints (Stage C): `<80` Main only (phone/SSH — rail folds away, tab strip
/// on its own row); `80–119` rail + Main; `>=120` rail + a **wider Main** (the primary
/// object gets the extra columns — we never add a fourth box).
const NARROW_WIDTH: u16 = 80;

/// Minimum height for the labels + rule rows, then for the context band on top. Below
/// each, that band folds away rather than eating the content it describes.
const LABELS_MIN_H: u16 = 9;
const CONTEXT_MIN_H: u16 = 14;

/// Rail width, derived from the terminal width alone — never from the data, so rows
/// cannot slide sideways as runs and agents arrive (U11). Wide enough for
/// `role · model · age` at both bands; the `>=120` band spends 6 of its extra columns
/// on making agent identity legible and the rest on Main.
fn rail_width(total: u16) -> u16 {
    if total >= 120 {
        32
    } else {
        26
    }
}

/// Chrome budget: header + context + labels + rule + footer, each foldable except the
/// header and footer. Everything else is content. The `:` palette, `/` filter and help
/// are overlays, not reserved rows.
fn layout_rects(area: Rect, focus: Focus, zoom: bool, driving: bool) -> LayoutRects {
    let narrow = area.width < NARROW_WIDTH;
    // Driving mode drops every band but the banner — it plus F12 is the whole chrome.
    let labels_h = if !driving && area.height >= LABELS_MIN_H {
        1
    } else {
        0
    };
    let ctx_h = if !driving && area.height >= CONTEXT_MIN_H {
        1
    } else {
        0
    };
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // header / driving banner
            Constraint::Length(ctx_h),    // stepper or project roll-up
            Constraint::Length(labels_h), // rail title + tab labels
            Constraint::Length(labels_h), // rule / active-tab underline
            Constraint::Min(2),           // body: rail + seam + main
            Constraint::Length(1),        // footer
        ])
        .split(area);

    let z = Rect::default();
    // Zoom or driving both hide the rail in place; nothing else on screen moves.
    let hide_rail = zoom || driving;
    let body = root[4];

    if narrow {
        // One column. The rail takes the stage while it is focused; otherwise Main
        // has it. Tapping a tab (or the breadcrumb) moves between the two.
        let (rail, main) = if focus == Focus::Rail && !hide_rail {
            (body, z)
        } else {
            (z, body)
        };
        return LayoutRects {
            header: root[0],
            context: root[1],
            labels: root[2],
            rule: root[3],
            rail,
            seam: z,
            main,
            footer: root[5],
            narrow: true,
        };
    }

    let (rail, seam, main) = if hide_rail {
        (z, z, body)
    } else {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(rail_width(area.width)),
                Constraint::Length(1),
                Constraint::Min(20),
            ])
            .split(body);
        (cols[0], cols[1], cols[2])
    };

    LayoutRects {
        header: root[0],
        context: root[1],
        labels: root[2],
        rule: root[3],
        rail,
        seam,
        main,
        footer: root[5],
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
    home: &HomeData,
    app: &mut App,
    rail_state: &mut ListState,
) {
    let area = f.area();
    // Full clear each frame — prevents styled-cell ghosting across the whole UI, and
    // leaves every cell on the terminal's own background (no page fill: the host
    // theme, and its transparency, show through).
    f.render_widget(Clear, area);

    // On the first narrow render with an active run, land on the live log so a
    // phone glance shows progress — but only once, and never over a manual move.
    // Zero runs gets the same treatment: the rail's "(no runs)" row has no CTA of its
    // own, so leaving focus on it would strand the phone view on a blank pane with
    // the coherent empty-state message (Main) never shown (AC-5). An empty Home (no
    // actual Run/Project rows, only its headers and the action row) gets the same
    // treatment, or the phone view strands the operator on a rail with no CTA (R9).
    if area.width < NARROW_WIDTH && !app.narrow_autofocus_done {
        let active = full.map(|s| {
            is_active_phase(s.phase) || s.slots.iter().any(|sl| sl.status == SlotStatus::Running)
        });
        let no_runs = app.browse == BrowseLevel::Runs && full.is_none() && runs.is_empty();
        let empty_home = app.browse == BrowseLevel::Home
            && !home
                .rows
                .iter()
                .any(|r| matches!(r, HomeRow::Run { .. } | HomeRow::Project(_)));
        if active == Some(true) || no_runs || empty_home {
            if app.focus == Focus::Rail {
                app.open_main(MainTab::Log);
            }
            app.narrow_autofocus_done = true;
        }
    }

    let driving = app.driving();
    let lay = layout_rects(area, app.focus, app.zoom, driving);
    // Keep mouse hit regions aligned with the frame actually painted.
    app.rect_status = lay.header;
    app.rect_rail = lay.rail;
    app.rect_main = lay.main;
    app.rect_palette = Rect::default();
    // Rebuilt below by whatever paints this frame. `rect_attention` is cleared here,
    // not in `draw_header`: driving mode skips the header entirely, and a stale chip
    // rect would swallow a click meant for the agent's terminal.
    app.rect_attention = Rect::default();
    app.gate_buttons.clear();
    app.main_tabs.clear();
    app.main_tab_glyphs.clear();

    if driving {
        draw_driving_banner(f, lay.header, app);
    } else {
        draw_header(f, lay.header, swarm, projects, runs, full, home, app);
        if lay.context.height > 0 {
            draw_context_band(f, lay.context, projects, runs, full, home, app);
        }
        if lay.labels.height > 0 {
            draw_labels(f, &lay, swarm, projects, runs, full, home, app);
            draw_rule(f, &lay, app);
        }
    }
    if lay.seam.width > 0 {
        draw_seam(f, lay.seam);
    }
    if lay.rail.width > 0 {
        draw_rail(f, lay.rail, projects, runs, full, home, app, rail_state);
    }
    if lay.main.width > 0 {
        draw_main(
            f,
            lay.main,
            full,
            stream_text,
            activity,
            diff_text,
            home,
            app,
        );
    }
    draw_footer(f, lay.footer, app, full);

    // The `:` palette floats above the footer; the `/` filter shows inline in the rail.
    if app.palette.is_some() {
        draw_palette(f, area, runs, app);
    }

    if app.show_help {
        draw_help_overlay(f, area, app);
    }
    if app.new_run.is_some() {
        draw_new_run(f, area, projects, app);
    }
}

/// The Main tab strip. Labels + the Activity alert badge; the active tab is lit by
/// weight and by the accent underline on the rule below it, never by a filled block.
fn main_tab_spans(app: &App) -> Vec<(MainTab, String, Style)> {
    MAIN_TABS
        .iter()
        .map(|t| {
            // Every tab reserves the same 4-column badge slot, blank unless it is
            // Activity with something to say: Activity is second of four, so a badge
            // that changed width would shift Diff and Shell out from under a click
            // (U11) — and a slot reserved on one tab only would make the gap either
            // side of it uneven with every other tab-to-tab gap.
            let badge = if *t == MainTab::Activity {
                match app.human_alerts_n {
                    0 => "    ".to_string(),
                    n => format!(" ⚠{:<2}", n.min(99)),
                }
            } else {
                "    ".to_string()
            };
            let text = format!("  {}{badge}  ", t.label());
            let style = if *t == app.main_tab {
                Style::default().fg(ACCENT).bold()
            } else if *t == MainTab::Activity && app.human_alerts_n > 0 {
                Style::default().fg(ALERT).bold()
            } else {
                dim()
            };
            (*t, text, style)
        })
        .collect()
}

/// One row: the rail's section title on the left, the MainTab labels on the right,
/// and what the active tab is showing, right-aligned. In narrow the rail is gone, so
/// the tabs spread across the whole row — still the escape from the Shell tab on a
/// phone. Records a hit rect per tab.
#[allow(clippy::too_many_arguments)]
fn draw_labels(
    f: &mut Frame,
    lay: &LayoutRects,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    home: &HomeData,
    app: &mut App,
) {
    let area = lay.labels;
    if area.width == 0 || area.height == 0 {
        return;
    }

    if lay.narrow {
        // The wide strip's fixed per-tab padding (badge slot on every tab, U11) exists
        // to keep tabs from shifting under the rail's columns. Narrow has no rail row
        // to stay aligned with, so it would rather spend the width on an even gap —
        // but a badge glued onto Activity alone grows only the gap next to it (AC-4:
        // measured [18, 22, 18] at 79 cols with alerts present). So narrow reserves
        // the same fixed-width slot on every tab too, same as wide, whenever the
        // width can afford it; only once that reservation would starve a tab off the
        // strip entirely does it fall back to gluing the badge onto Activity alone,
        // trading gap uniformity for keeping all four tabs on screen.
        let badge_w: u16 = if app.human_alerts_n > 0 { 4 } else { 0 };
        let raw: Vec<(MainTab, &str, Style)> = MAIN_TABS
            .iter()
            .map(|t| {
                let style = if *t == app.main_tab {
                    Style::default().fg(ACCENT).bold()
                } else if *t == MainTab::Activity && app.human_alerts_n > 0 {
                    Style::default().fg(ALERT).bold()
                } else {
                    dim()
                };
                (*t, t.label(), style)
            })
            .collect();
        let n = raw.len() as u16;
        let plain_total: u16 = raw.iter().map(|(_, l, _)| l.chars().count() as u16).sum();
        let reserved = plain_total + badge_w * n;
        let reserve_all = n > 1 && reserved <= area.width;
        let gap = if n <= 1 {
            0
        } else if reserve_all {
            (area.width - reserved) / (n - 1)
        } else {
            let label_total = plain_total + badge_w;
            area.width.saturating_sub(label_total) / (n - 1)
        };
        let tabs: Vec<(MainTab, String, Style)> = raw
            .into_iter()
            .map(|(t, label, style)| {
                let is_alert_tab = t == MainTab::Activity && app.human_alerts_n > 0;
                let badge = if is_alert_tab {
                    format!(" ⚠{:<2}", app.human_alerts_n.min(99))
                } else if reserve_all {
                    " ".repeat(badge_w as usize)
                } else {
                    String::new()
                };
                (t, format!("{label}{badge}"), style)
            })
            .collect();
        let label_total: u16 = tabs.iter().map(|(_, t, _)| t.chars().count() as u16).sum();
        let total_w = (label_total + gap * n.saturating_sub(1)).min(area.width);
        let start_x = area.x + area.width.saturating_sub(total_w) / 2;
        let mut spans: Vec<Span> = Vec::with_capacity(tabs.len());
        // Label glyph rects first, `gap` columns of dead space between each pair —
        // then a second pass below pads each hit rect out into half of each
        // neighboring gap, so a tap anywhere on the strip lands on a tab (U11's touch
        // requirement) rather than only on the glyphs themselves.
        let mut glyphs: Vec<(MainTab, u16, u16)> = Vec::with_capacity(tabs.len());
        let mut x = start_x;
        let n_tabs = tabs.len();
        for (i, (tab, text, style)) in tabs.into_iter().enumerate() {
            let avail = area.right().saturating_sub(x);
            let raw_w = text.chars().count() as u16;
            let (text, w) = if raw_w > avail {
                (truncate(&text, avail as usize), avail)
            } else {
                (text, raw_w)
            };
            if w == 0 {
                break;
            }
            glyphs.push((tab, x, w));
            spans.push(Span::styled(text, style));
            x = x.saturating_add(w);
            if i + 1 < n_tabs {
                x = x.saturating_add(gap);
                if gap > 0 {
                    spans.push(Span::raw(" ".repeat(gap as usize)));
                }
            }
        }
        let n_glyphs = glyphs.len();
        for (i, (tab, gx, gw)) in glyphs.into_iter().enumerate() {
            let left = if i == 0 {
                area.x
            } else {
                gx.saturating_sub(gap / 2)
            };
            let right = if i + 1 == n_glyphs {
                area.right()
            } else {
                gx + gw + gap.saturating_sub(gap / 2)
            };
            app.main_tabs.push((
                Rect {
                    x: left,
                    y: area.y,
                    width: right.saturating_sub(left),
                    height: 1,
                },
                tab,
            ));
            app.main_tab_glyphs.push((
                Rect {
                    x: gx,
                    y: area.y,
                    width: gw,
                    height: 1,
                },
                tab,
            ));
        }
        let line = Rect {
            x: start_x,
            width: area.right().saturating_sub(start_x),
            ..area
        };
        f.render_widget(Paragraph::new(Line::from(spans)), line);
        return;
    }

    if lay.rail.width > 0 {
        let title = rail_title(projects, runs, full, home, app);
        let rail_row = Rect {
            x: lay.rail.x.saturating_add(1),
            width: lay.rail.width.saturating_sub(1),
            ..area
        };
        let style = if app.focus == Focus::Rail {
            Style::default().fg(ACCENT).bold()
        } else {
            muted().bold()
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                truncate(&title, rail_row.width as usize),
                style,
            )),
            rail_row,
        );
    }

    // Main's tabs sit on the labels row, aligned to Main's column.
    let main = Rect {
        y: area.y,
        height: 1,
        ..lay.main
    };
    if main.width == 0 {
        return;
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut x = main.x;
    for (tab, text, style) in main_tab_spans(app) {
        let w = text.chars().count() as u16;
        if x.saturating_add(w) > main.right() {
            break;
        }
        let rect = Rect {
            x,
            y: area.y,
            width: w,
            height: 1,
        };
        app.main_tabs.push((rect, tab));
        app.main_tab_glyphs.push((rect, tab));
        x = x.saturating_add(w);
        spans.push(Span::styled(text, style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), main);

    // What the active tab is showing, parked on the right so the tabs never move.
    let ctx = main_context(swarm, full, app);
    let used = x.saturating_sub(main.x);
    let room = main.width.saturating_sub(used).saturating_sub(1);
    if !ctx.is_empty() && room > 8 {
        let text = truncate(&ctx, room as usize);
        let w = text.chars().count() as u16;
        f.render_widget(
            Paragraph::new(Span::styled(text, muted())),
            Rect {
                x: main.right().saturating_sub(w + 1),
                y: area.y,
                width: w,
                height: 1,
            },
        );
    }
}

/// Truncate a span list to `width` columns, marking the cut. A Paragraph wider than
/// its rect is clipped by ratatui at the cell boundary with no ellipsis, which reads
/// as a rendering fault: `Needs plan approval` becomes `Needs plan ` and the operator
/// cannot tell whether the phase is truncated or just oddly named.
fn fit_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= width as usize {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for span in spans {
        let w = span.content.chars().count();
        if used + w <= width as usize {
            used += w;
            out.push(span);
            continue;
        }
        let room = (width as usize).saturating_sub(used);
        if room > 1 {
            let style = span.style;
            out.push(Span::styled(truncate(&span.content, room), style));
        }
        break;
    }
    out
}

/// The chrome/content divider. One rule across the frame, tee'd at the rail seam,
/// carrying the active tab's underline — the tab indicator costs no extra row.
fn draw_rule(f: &mut Frame, lay: &LayoutRects, app: &App) {
    let area = lay.rule;
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(
        Paragraph::new(Span::styled(RULE_H.repeat(area.width as usize), rule())),
        area,
    );
    if lay.seam.width > 0 {
        f.render_widget(
            Paragraph::new(Span::styled(RULE_TEE, rule())),
            Rect {
                x: lay.seam.x,
                y: area.y,
                width: 1,
                height: 1,
            },
        );
    }
    if let Some((r, _)) = app.main_tab_glyphs.iter().find(|(_, t)| *t == app.main_tab) {
        let w = r.width.min(area.right().saturating_sub(r.x));
        if w > 0 {
            f.render_widget(
                Paragraph::new(Span::styled(
                    TAB_MARK.repeat(w as usize),
                    Style::default().fg(ACCENT),
                )),
                Rect {
                    x: r.x,
                    y: area.y,
                    width: w,
                    height: 1,
                },
            );
        }
    }
}

/// The one-column seam between rail and Main. No pane borders anywhere else.
fn draw_seam(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    for _ in 0..area.height {
        lines.push(Line::from(Span::styled(RULE_SEAM, rule())));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// One step of the run pipeline. A run *is* a stepper, and the shell says so in one
/// row instead of hiding it in a parenthesised phase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Pending,
    Active,
    Done,
    Failed,
    /// Finished, and now waiting on the operator.
    Gate,
    /// Was running when the run was halted, quota-paused or abandoned. Not progress,
    /// not failure — nobody is driving it.
    Halted,
    /// Never happened and never will: a disabled channel, or a role this run's fleet
    /// does not use. Distinguished from Pending, which promises it is still coming.
    Skipped,
}

impl StepState {
    fn glyph(self) -> &'static str {
        match self {
            StepState::Pending => "○",
            StepState::Active => "◐",
            StepState::Done => "●",
            StepState::Failed => "✗",
            StepState::Gate => "⚑",
            StepState::Halted => "⏸",
            StepState::Skipped => "·",
        }
    }
    fn color(self) -> Color {
        match self {
            StepState::Pending | StepState::Skipped => FG_MUTED,
            StepState::Active => INFO,
            StepState::Done => OK,
            StepState::Failed => ALERT,
            StepState::Gate | StepState::Halted => WARN,
        }
    }
}

/// The pipeline a run of this **kind** walks, as `(label, owning role)`; `ship` is the
/// one step with no role, read off the phase. Keyed on the workflow because the
/// fleets differ: an arena has no planner and a roles run has nothing but peers, so
/// one fixed seven-step table would show them six steps that never existed.
fn steps_for(kind: crate::cli::WorkflowKind) -> &'static [(&'static str, Option<SlotRole>)] {
    use crate::cli::WorkflowKind as W;
    match kind {
        W::Arena => &[
            ("build", Some(SlotRole::Implementer)),
            ("rank", Some(SlotRole::Ranker)),
            ("reconcile", Some(SlotRole::Reconciler)),
            ("review", Some(SlotRole::Reviewer)),
            ("ship", None),
        ],
        W::Roles | W::Peer => &[("peers", Some(SlotRole::Peer)), ("ship", None)],
        W::Review => &[("review", Some(SlotRole::Reviewer)), ("ship", None)],
        W::Plan | W::Loop => &[
            ("plan", Some(SlotRole::Planner)),
            ("critique", Some(SlotRole::PlanCritic)),
            ("spec", Some(SlotRole::TestAuthor)),
            ("build", Some(SlotRole::Implementer)),
            ("tests", Some(SlotRole::Tester)),
            ("review", Some(SlotRole::Reviewer)),
            ("ship", None),
        ],
    }
}

/// Which step a gate is holding. The plan gate hangs off the critic when the fleet
/// ran one (else the planner); the winner gate off ranking, the reconcile gate off
/// reconcile. Applied last and unconditionally, so a step whose slot failed still
/// flies the flag — the gate is the actionable fact.
fn gate_step(st: &RunState, steps: &[(&'static str, StepState)]) -> Option<usize> {
    let find = |label: &str| steps.iter().position(|(l, _)| *l == label);
    match st.phase {
        Phase::AwaitingPlanApproval => plan_step(st, steps),
        Phase::AwaitingWinnerConfirm => find("rank"),
        Phase::AwaitingReconcile => find("reconcile"),
        Phase::AwaitingShipConfirm => find("ship"),
        // The ceiling stops the *build* from being re-dispatched, so it hangs off build.
        Phase::AwaitingRoundExtension => find("build"),
        _ => None,
    }
}

/// The step the plan verdict lands on: the critic when the fleet ran one, else the
/// planner.
fn plan_step(st: &RunState, steps: &[(&'static str, StepState)]) -> Option<usize> {
    let find = |label: &str| steps.iter().position(|(l, _)| *l == label);
    if st.slots.iter().any(|s| s.role == SlotRole::PlanCritic) {
        find("critique").or_else(|| find("plan"))
    } else {
        find("plan")
    }
}

/// Step states read off the slots that actually ran, not off a phase-to-step guess:
/// slots accumulate on the run, so their roles and statuses are the honest record of
/// how far it got. Only `ship` comes from the phase. `abandoned` is the App's view
/// (no orchestrator behind the run), which no field of `RunState` records.
fn run_steps(st: &RunState, abandoned: bool) -> Vec<(&'static str, StepState)> {
    let broken = matches!(st.phase, Phase::Failed | Phase::Stuck | Phase::Escalated);
    let halted = abandoned || matches!(st.phase, Phase::Stopped | Phase::Quota);
    let mut out: Vec<(&'static str, StepState)> = steps_for(st.workflow)
        .iter()
        .map(|(label, role)| {
            let Some(role) = role else {
                let ship = match st.phase {
                    Phase::Done => StepState::Done,
                    Phase::Shipping => StepState::Active,
                    _ if broken => StepState::Failed,
                    _ if halted => StepState::Halted,
                    _ => StepState::Pending,
                };
                return (*label, ship);
            };
            let mine: Vec<&SlotState> = st.slots.iter().filter(|s| s.role == *role).collect();
            if mine.is_empty() {
                return (*label, StepState::Pending);
            }
            let state = if mine.iter().any(|s| s.status == SlotStatus::Running) {
                if broken {
                    StepState::Failed
                } else if halted {
                    StepState::Halted
                } else {
                    StepState::Active
                }
            } else if mine.iter().all(|s| s.status == SlotStatus::Failed) {
                StepState::Failed
            } else if mine.iter().any(|s| s.status == SlotStatus::Done) {
                // A fleet that runs several of a role (arena implementers, two
                // reviewers) survives one of them dying; the rail carries that.
                StepState::Done
            } else {
                StepState::Pending
            };
            (*label, state)
        })
        .collect();

    // A step nothing ever filled, on a run that has already moved past it, did not
    // happen: a disabled channel (`[spec]`, `[suite]`) or an unused optional role.
    // Saying "pending" there promises work that is never coming.
    let terminal = matches!(
        st.phase,
        Phase::Done | Phase::Shipping | Phase::AwaitingShipConfirm
    );
    for i in 0..out.len() {
        if out[i].1 != StepState::Pending {
            continue;
        }
        let passed = terminal || out[i + 1..].iter().any(|(_, s)| *s != StepState::Pending);
        if passed {
            out[i].1 = StepState::Skipped;
        }
    }

    // A rejected plan is not a pending one.
    if st.phase == Phase::PlanRejected {
        if let Some(i) = plan_step(st, &out) {
            out[i].1 = StepState::Failed;
        }
    }
    if let Some(i) = gate_step(st, &out) {
        out[i].1 = StepState::Gate;
    }
    out
}

/// The stepper as spans. Tightens in three tiers — drawn connectors, then the labels
/// on everything that is not live — and appends the live step's name only when it
/// actually fits, so the row never clips mid-word.
fn stepper_spans(
    steps: &[(&'static str, StepState)],
    width: u16,
    spinner: &'static str,
) -> Vec<Span<'static>> {
    let labels_w: usize = steps
        .iter()
        .map(|(l, _)| l.chars().count() + 2)
        .sum::<usize>();
    let gaps = steps.len().saturating_sub(1);
    let width = width as usize;
    let (labelled, sep) = if labels_w + gaps * 3 <= width {
        (true, " ─ ")
    } else if labels_w + gaps <= width {
        (true, " ")
    } else {
        (false, " ")
    };
    // Even glyph-only, seven steps need 13 columns. When they do not all fit, spend
    // the last column on an ellipsis rather than letting the paragraph cut a step in
    // half.
    let sep_w = sep.chars().count();
    let glyphs_w = steps.len() + gaps * sep_w;
    let elided = glyphs_w > width;
    let budget = if elided {
        width.saturating_sub(1)
    } else {
        width
    };
    let mut used = 0usize;
    let mut spans = Vec::with_capacity(steps.len() * 3);
    for (i, (label, state)) in steps.iter().enumerate() {
        // Stop cleanly at a step boundary.
        if used + if i > 0 { sep_w + 1 } else { 1 } > budget {
            break;
        }
        if i > 0 {
            // The connector carries progress: lit behind everything already finished.
            let done = matches!(steps[i - 1].1, StepState::Done | StepState::Gate);
            spans.push(Span::styled(
                sep,
                Style::default().fg(if done { OK } else { RULE }),
            ));
            used += sep.chars().count();
        }
        let glyph = if *state == StepState::Active {
            spinner
        } else {
            state.glyph()
        };
        spans.push(Span::styled(
            glyph.to_string(),
            Style::default().fg(state.color()).bold(),
        ));
        used += 1;
        let live = matches!(
            state,
            StepState::Active | StepState::Gate | StepState::Halted
        );
        let room_for_label = used + 1 + label.chars().count() <= budget;
        if (labelled || live) && room_for_label {
            spans.push(Span::styled(
                format!(" {label}"),
                Style::default()
                    .fg(match state {
                        StepState::Pending | StepState::Skipped => FG_MUTED,
                        StepState::Done => FG_DIM,
                        s => s.color(),
                    })
                    .add_modifier(match state {
                        StepState::Active
                        | StepState::Gate
                        | StepState::Halted
                        | StepState::Failed => Modifier::BOLD,
                        _ => Modifier::empty(),
                    }),
            ));
            used += 1 + label.chars().count();
        }
    }
    if elided && used < width {
        spans.push(Span::styled("…", muted()));
    }
    spans
}

/// The band under the header: the run's pipeline plus its meters, or — with no run in
/// hand — the project's roll-up. Always occupied, so nothing below it moves.
fn draw_context_band(
    f: &mut Frame,
    area: Rect,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    home: &HomeData,
    app: &App,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let pad = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    if pad.width == 0 {
        return;
    }

    if app.browse == BrowseLevel::Home {
        let need = home_needs_you(&home.rows);
        let running = home_band_count(&home.rows, HomeBand::Running);
        let finished = home_band_count(&home.rows, HomeBand::Finished);
        let line = Line::from(vec![
            Span::styled(
                format!("{need} need you"),
                Style::default().fg(if need > 0 { WARN } else { FG_MUTED }),
            ),
            Span::styled(" · ", muted()),
            Span::styled(
                format!("{running} running"),
                Style::default().fg(if running > 0 { INFO } else { FG_MUTED }),
            ),
            Span::styled(" · ", muted()),
            Span::styled(format!("{finished} finished"), dim()),
            Span::styled(" · ", muted()),
            Span::styled(home_scope_label(&app.home_scope), muted()),
        ]);
        f.render_widget(Paragraph::new(line), pad);
        return;
    }

    let Some(st) = full else {
        // Outside a project the refresher hands us no runs at all (`build_snapshot`),
        // so a run roll-up here would always read "none" — count what we do have.
        if !app.browse.in_project() {
            let line = if projects.is_empty() {
                Line::from(Span::styled(
                    "no projects yet — run spar in a repo",
                    muted(),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("{} projects", projects.len()), dim()),
                    Span::styled(" · ", muted()),
                    Span::styled("Enter opens one", muted()),
                ])
            };
            f.render_widget(Paragraph::new(line), pad);
            return;
        }
        let need = runs_needing_attention(runs);
        let running = runs.iter().filter(|r| is_active_phase(r.phase)).count();
        let line = if runs.is_empty() {
            // `n` from Home opens the new-run surface and its fleet picker (Phase D);
            // this level just states what it has.
            Line::from(Span::styled("no runs yet", muted()))
        } else {
            Line::from(vec![
                Span::styled(format!("{} runs", runs.len()), dim()),
                Span::styled(" · ", muted()),
                Span::styled(
                    format!("{running} running"),
                    Style::default().fg(if running > 0 { INFO } else { FG_MUTED }),
                ),
                Span::styled(" · ", muted()),
                Span::styled(
                    format!("⚑{need} need you"),
                    Style::default().fg(if need > 0 { WARN } else { FG_MUTED }),
                ),
            ])
        };
        f.render_widget(Paragraph::new(line), pad);
        return;
    };

    // Right meters first: they set the budget the stepper renders into.
    let done = st
        .slots
        .iter()
        .filter(|s| s.status == SlotStatus::Done)
        .count();
    // `state.usage` is the run's ledger: one entry pushed per dispatch. `slot.usage`
    // is overwritten each time a slot is re-dispatched, so summing that under-reports
    // a run with fix rounds and disagrees with `status --json` (executor.rs:1028).
    let billed: u64 = st.usage.iter().map(|u| u.billed_tokens).sum();
    let mut meters: Vec<Span> = vec![
        Span::styled(relative_age(st.created_at), dim()),
        Span::styled(" · ", muted()),
        Span::styled(format!("{done}/{} agents", st.slots.len()), dim()),
    ];
    // A unit of work says how much of it there is: rounds it has been through, and
    // how many run ids it folds in (U15).
    if st.round > 1 {
        meters.push(Span::styled(" · ", muted()));
        meters.push(Span::styled(format!("round {}", st.round), dim()));
    }
    if let Some(legs) = runs
        .iter()
        .find(|r| r.id == st.id)
        .map(|r| r.legs)
        .filter(|n| *n > 1)
    {
        meters.push(Span::styled(" · ", muted()));
        meters.push(Span::styled(format!("{legs} legs"), dim()));
    }
    if billed > 0 {
        meters.push(Span::styled(" · ", muted()));
        meters.push(Span::styled(
            format!("billed {}", compact_u64(billed)),
            dim(),
        ));
    }
    let meters_w: u16 = meters
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum();

    // The stepper is the point of this band; the meters yield to it, never the other
    // way round, so a narrow terminal never leaves the row blank.
    let (room, meters) = match pad.width.checked_sub(meters_w + 2) {
        Some(w) if w >= 8 => (w, meters),
        _ => (pad.width, Vec::new()),
    };
    let meters_w: u16 = meters
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum();
    let steps = run_steps(st, app.abandoned);
    f.render_widget(
        Paragraph::new(Line::from(stepper_spans(&steps, room, app.spinner()))),
        Rect { width: room, ..pad },
    );
    if meters_w > 0 && meters_w < pad.width {
        f.render_widget(
            Paragraph::new(Line::from(meters)),
            Rect {
                x: pad.right().saturating_sub(meters_w),
                width: meters_w,
                ..pad
            },
        );
    }
}

/// A slot's short name: its role, plus an index when the fleet runs more than one of
/// that role (two reviewers, N arena implementers). The raw slot id carries the
/// provider and is far too long for a breadcrumb or a rail row.
fn slot_short(slots: &[SlotState], i: usize) -> String {
    let Some(s) = slots.get(i) else {
        return "—".into();
    };
    let label = role_label(s.role);
    let peers: Vec<usize> = slots
        .iter()
        .enumerate()
        .filter(|(_, o)| o.role == s.role)
        .map(|(j, _)| j)
        .collect();
    if peers.len() < 2 {
        return label.to_string();
    }
    let n = peers.iter().position(|j| *j == i).unwrap_or(0);
    format!("{label} {n}")
}

/// The model a slot is running, shortened to fit a rail column. Keeps the **tail**
/// and marks the elision, because that is where the tier lives: `gemini-3.7-flash`
/// and `gemini-3.7-pro` differ only in their last segment, and a head-first shortening
/// renders both as `gemini`. Prefers the model the provider says it served over the
/// one that was requested — for an OpenRouter-routed slot they can differ.
fn slot_model(s: &SlotState, max: usize) -> String {
    let served = s
        .usage
        .as_ref()
        .and_then(|u| u.model.as_deref())
        .or(s.model.as_deref());
    let Some(m) = served else {
        // No model recorded: name the adapter. `provider` is the model-free storage
        // key by construction (executor::init_slot_model), so there is no `@model`
        // left on it to strip.
        return truncate(s.provider.rsplit(':').next().unwrap_or(&s.provider), max);
    };
    let m = m.rsplit('/').next().unwrap_or(m);
    let m = m.strip_prefix("claude-").unwrap_or(m);
    // Drop a trailing release date (`claude-opus-4-5-20250929`), never a version.
    let m = match m.rsplit_once('-') {
        Some((head, tail))
            if tail.len() >= 6 && tail.chars().all(|c| c.is_ascii_digit()) && !head.is_empty() =>
        {
            head
        }
        _ => m,
    };
    if m.chars().count() <= max {
        return m.to_string();
    }
    // Still too long: the version segments go before the names do. `opus-4-5` must
    // not shorten to `…4-5`, which every Anthropic tier shares — the 80-119 band's
    // 26-column rail leaves 6 columns here, so this is the common case, not the edge.
    let named: String = m
        .split('-')
        .filter(|seg| !seg.starts_with(|c: char| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join("-");
    if !named.is_empty() && named.chars().count() <= max {
        return named;
    }
    // Names alone still do not fit: keep the tail, which is where the tier lives
    // (`gemini-flash` vs `gemini-pro`), and say that we cut.
    let mut cut = if named.is_empty() { m } else { named.as_str() };
    while let Some((_, tail)) = cut.split_once('-') {
        cut = tail;
        if cut.chars().count() < max {
            return format!("…{cut}");
        }
    }
    truncate(cut, max)
}

/// The header's cue and its colors. `Some(wash)` means an alert state (gate, quota,
/// failure, abandoned) loud enough to earn a full-row background.
fn status_cue(
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    home: &HomeData,
    app: &App,
) -> (String, Color, Option<Color>) {
    if app.browse == BrowseLevel::Home {
        if projects.is_empty() {
            return (
                format!(
                    "no projects yet — run spar in a repo · {}",
                    registry::spar_home().display()
                ),
                FG_DIM,
                None,
            );
        }
        let need = home_needs_you(&home.rows);
        return if need > 0 {
            (format!("⚑{need} need you"), WARN, None)
        } else {
            ("nothing needs you · n starts a run".into(), FG_MUTED, None)
        };
    }
    if app.browse == BrowseLevel::Projects {
        if projects.is_empty() {
            return (
                format!(
                    "no projects yet — run spar in a repo · {}",
                    registry::spar_home().display()
                ),
                FG_DIM,
                None,
            );
        }
        return ("Enter opens a project".into(), FG_MUTED, None);
    }
    let Some(st) = full else {
        return if runs.is_empty() {
            (
                "no runs — spar plan -t \"describe the change\" --providers cli:claude".into(),
                FG_DIM,
                None,
            )
        } else {
            ("select a run".into(), FG_MUTED, None)
        };
    };
    if app.abandoned {
        return (
            format!(
                "ABANDONED — no orchestrator · spar implement --run {}",
                st.id
            ),
            FG,
            Some(ALERT_WASH),
        );
    }
    match st.phase {
        Phase::AwaitingPlanApproval => (
            "plan ready — tap Approve · r reject".into(),
            WARN,
            Some(GATE_WASH),
        ),
        Phase::AwaitingWinnerConfirm => (
            "winner ready — confirm or reconcile".into(),
            WARN,
            Some(GATE_WASH),
        ),
        Phase::AwaitingShipConfirm => {
            ("ready to ship — s (draft PR)".into(), WARN, Some(GATE_WASH))
        }
        Phase::AwaitingReconcile => ("reconcile ready".into(), WARN, Some(GATE_WASH)),
        Phase::AwaitingRoundExtension => (
            "round ceiling — implement --max-rounds to buy more".into(),
            WARN,
            Some(GATE_WASH),
        ),
        Phase::Quota => (
            "all providers paused — spar provider resume".into(),
            INK,
            Some(ALERT),
        ),
        Phase::Failed | Phase::Stuck | Phase::Escalated => (
            format!("{} — check the Log tab", phase_label(st.phase)),
            FG,
            Some(ALERT_WASH),
        ),
        _ if st.dry_run => ("dry-run".into(), FG_DIM, None),
        _ => (String::new(), FG_MUTED, None),
    }
}

/// Width reserved on the right of the header for gate buttons, wide enough for the
/// widest set (`Confirm` + `Reconcile`). Buttons are left-aligned inside it, so a
/// different gate never slides them under a mid-click (U11).
const GATE_ZONE_W: u16 = 23;

/// The gate zone: a fixed slot, or `None` on a phone-width screen that cannot spare
/// one (there the buttons fall back to right-aligned, the old behaviour).
fn gate_zone(area: Rect) -> Option<Rect> {
    if area.width < NARROW_WIDTH {
        return None;
    }
    Some(Rect {
        x: area.right().saturating_sub(GATE_ZONE_W),
        y: area.y,
        width: GATE_ZONE_W,
        height: 1,
    })
}

/// The whole top chrome: one line.
///
/// ` spar  acme/api ▸ run 3f2a ▸ review 0 · Under review        ⚑2 need you  [Ship]`
///
/// Brand + breadcrumb + phase on the left, attention chips on the right, gate buttons
/// in their reserved zone. Counts and progress live one row below, in the stepper.
#[allow(clippy::too_many_arguments)]
fn draw_header(
    f: &mut Frame,
    area: Rect,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    home: &HomeData,
    app: &mut App,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (cue, cue_fg, wash) = status_cue(projects, runs, full, home, app);
    let buttons = gate_buttons_for(full);
    // The only full-row fill in the product, and only for states worth shouting about.
    if let Some(w) = wash {
        f.render_widget(Paragraph::new("").style(Style::default().bg(w)), area);
    }

    let project = swarm
        .project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".");

    let mut spans = vec![
        Span::styled(" spar ", chip(ACCENT)),
        Span::styled(
            format!("  {}", truncate(project, 20)),
            Style::default().fg(FG).bold(),
        ),
    ];
    // No fallback to "—": with no run in hand (and none selected), the breadcrumb
    // omits itself rather than sitting next to the "no runs" cue and contradicting it.
    if app.browse.in_project() {
        let run = full
            .map(|s| s.id.clone())
            .or_else(|| runs.get(app.selected_run).map(|r| r.id.clone()));
        if let Some(run) = run {
            spans.push(Span::styled(" ▸ ", muted()));
            spans.push(Span::styled(
                format!("run {run}"),
                Style::default().fg(INFO),
            ));
        }
    }
    if app.browse == BrowseLevel::Agents {
        let slot = full
            .map(|s| slot_short(&s.slots, app.selected_slot))
            .unwrap_or_else(|| "—".into());
        spans.push(Span::styled(" ▸ ", muted()));
        spans.push(Span::styled(slot, Style::default().fg(HINT)));
    }

    if let Some(st) = full {
        let pc = if wash.is_some() {
            cue_fg
        } else {
            phase_color(st.phase)
        };
        spans.push(Span::styled("  ", Style::default()));
        if !app.abandoned && is_active_phase(st.phase) {
            spans.push(Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(pc),
            ));
        }
        spans.push(Span::styled(
            phase_label(st.phase),
            Style::default().fg(pc).bold(),
        ));
        if st.dry_run {
            spans.push(Span::styled(" dry-run ", chip(WARN)));
        }
    }
    // Right cluster: the fleet roll-up ("what needs me?", independent of the rail
    // selection) and the unread human-alert count.
    let mut right: Vec<Span> = Vec::new();
    let need = if app.browse.in_project() {
        runs_needing_attention(runs)
    } else if app.browse == BrowseLevel::Home {
        // Home is the one view that is organised around this roll-up (U7); it must
        // not be the only view that never shows it.
        home_needs_you(&home.rows)
    } else {
        0
    };
    let attention_token = format!(" ⚑{need} need you · a ");
    if need > 0 {
        right.push(Span::styled(attention_token.clone(), chip(WARN)));
    }
    if app.human_alerts_n > 0 {
        right.push(Span::styled(
            format!(" ⚠{} ", app.human_alerts_n),
            chip(ALERT),
        ));
    }
    if app.abandoned {
        right.push(Span::styled(" ABANDONED ", chip(ALERT)));
    }

    let zone = gate_zone(area);
    // Without a reserved zone (phone width) the buttons overpaint whatever is beneath
    // them, so the breadcrumb has to stop before they start — otherwise it is not
    // clipped, it is buried, and it loses even its ellipsis.
    let right_limit = zone
        .map(|z| z.x)
        .unwrap_or_else(|| area.right().saturating_sub(gate_buttons_width(&buttons)));
    let right_w: u16 = right.iter().map(|s| s.content.chars().count() as u16).sum();
    let right_x = right_limit.saturating_sub(right_w + 1).max(area.x);

    let left_w = right_x.saturating_sub(area.x);

    // At a gate the buttons on the right and the footer already say what to press;
    // repeating it here only crowds the breadcrumb. And like the run breadcrumb
    // above, a cue that cannot fit whole is omitted rather than shown truncated —
    // Main renders the same wording in full, so a clipped fragment here would just
    // be a second, disagreeing spelling of it (round-4 review finding).
    if !cue.is_empty() && buttons.is_empty() {
        let base_w: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
        let cue_w = 3 + cue.chars().count() as u16; // " · " + cue
        if base_w + cue_w <= left_w {
            spans.push(Span::styled(" · ", muted()));
            spans.push(Span::styled(
                cue,
                Style::default().fg(cue_fg).add_modifier(if wash.is_some() {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ));
        }
    }
    f.render_widget(
        Paragraph::new(Line::from(fit_spans(spans, left_w))),
        Rect {
            width: left_w,
            ..area
        },
    );
    if right_w > 0 && right_x + right_w <= area.right() {
        if need > 0 {
            app.rect_attention = Rect {
                x: right_x,
                y: area.y,
                width: attention_token.chars().count() as u16,
                height: 1,
            };
        }
        f.render_widget(
            Paragraph::new(Line::from(right)),
            Rect {
                x: right_x,
                width: right_w,
                ..area
            },
        );
    }
    render_gate_buttons(f, area, app, &buttons);
}

fn button_style(action: GateAction) -> Style {
    let bg = match action {
        GateAction::Approve | GateAction::Ship | GateAction::ConfirmWinner => OK,
        GateAction::Reject => ALERT,
        GateAction::Reconcile | GateAction::MoreRounds => ACCENT,
    };
    Style::default().fg(INK).bg(bg).bold()
}

/// Columns a gate-button set occupies, including the gaps and the right margin.
fn gate_buttons_width(buttons: &[(&str, GateAction)]) -> u16 {
    if buttons.is_empty() {
        return 0;
    }
    let labels: u16 = buttons
        .iter()
        .map(|(l, _)| l.chars().count() as u16 + 2)
        .sum();
    labels + buttons.len() as u16 - 1 + 1
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
    // Inside the reserved zone the buttons start at a fixed x, so swapping gates never
    // moves them; without a zone (narrow) they right-align as before.
    let mut cx = match gate_zone(area) {
        Some(z) => z.x,
        None => area.x + area.width.saturating_sub(total + 1), // 1-col right margin
    };
    cx = cx.max(area.x);
    for (i, ((_, action), w)) in buttons.iter().zip(widths.iter()).enumerate() {
        if cx.saturating_add(*w) > area.right() {
            break;
        }
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

/// The rail's section title, shown on the labels row. While `/` is live the title
/// becomes the filter field so the operator can see what they are narrowing by.
fn rail_title(
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    home: &HomeData,
    app: &App,
) -> String {
    let base = match app.browse {
        BrowseLevel::Home => format!("HOME  {} need you", home_needs_you(&home.rows)),
        BrowseLevel::Projects => format!("PROJECTS  {}", projects.len()),
        BrowseLevel::Runs => format!("RUNS  {}", runs.len()),
        BrowseLevel::Agents => {
            let slots = full.map(|s| s.slots.as_slice()).unwrap_or(&[]);
            let running = slots
                .iter()
                .filter(|s| s.status == SlotStatus::Running)
                .count();
            let word = if app.abandoned { "orphaned" } else { "live" };
            format!("AGENTS  {running}/{} {word}", slots.len())
        }
    };
    match app.filter.as_deref() {
        Some(f) if !app.filter_committed => format!("/{f}▌"),
        Some(f) if !f.is_empty() => format!("{base}  /{f}"),
        _ => base,
    }
}

/// The rail: one drill-down tree (`projects ▸ runs ▸ agents`), never a stack of
/// co-equal panels. `Enter` pushes a level, `Esc` pops one. No border: its title
/// rides the labels row and the seam separates it from Main.
#[allow(clippy::too_many_arguments)]
fn draw_rail(
    f: &mut Frame,
    area: Rect,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    home: &HomeData,
    app: &App,
    state: &mut ListState,
) {
    let focused = app.focus == Focus::Rail;
    let w = area.width;
    let items = match app.browse {
        BrowseLevel::Home => rail_home_items(&home.rows, projects, app, w, focused),
        BrowseLevel::Projects => rail_project_items(projects, &home.project_stats, app, w, focused),
        BrowseLevel::Runs => rail_run_items(runs, app, w, focused),
        BrowseLevel::Agents => {
            let slots = full.map(|s| s.slots.as_slice()).unwrap_or(&[]);
            rail_slot_items(slots, app, w, focused)
        }
    };
    f.render_stateful_widget(List::new(items), area, state);
}

/// The lead columns: the selection bar, then the attention flag. Two cells, both fixed,
/// because they are independent facts — one column for both meant that on a project
/// where every run wants you (biddesk: 12 of 13) the cursor was invisible.
fn rail_lead(sel: bool, focused: bool, flag: Option<Color>) -> Vec<Span<'static>> {
    vec![
        if sel {
            Span::styled(
                SEL_BAR,
                Style::default().fg(if focused { ACCENT } else { FG_MUTED }),
            )
        } else {
            Span::raw(" ")
        },
        match flag {
            Some(c) => Span::styled("⚑", Style::default().fg(c).bold()),
            None => Span::raw(" "),
        },
    ]
}

/// A dimmed row for something the `/` filter did not match. Filtered rows stay in
/// place rather than disappearing — hiding them would desync the selection index (U4).
fn rail_filtered_row(text: &str, w: u16) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("  {}", truncate(text, w.saturating_sub(2) as usize)),
        Style::default().fg(FG_MUTED).dim(),
    )))
}

fn rail_empty(text: &'static str) -> Vec<ListItem<'static>> {
    vec![ListItem::new(Span::styled(
        format!("  {text}"),
        Style::default().fg(FG_MUTED).italic(),
    ))]
}

/// Pad `spans` out to `w` with the status/age column flush right — the rail's only
/// right-aligned cell, and it never moves. The gap aims for one column of air before
/// the seam but yields it when the row is full; the seam has a column of its own, so
/// the two never touch.
fn rail_row(
    lead: Vec<Span<'static>>,
    mut body: Vec<Span<'static>>,
    right: Span<'static>,
    w: u16,
) -> ListItem<'static> {
    let body_w: usize = body.iter().map(|s| s.content.chars().count()).sum();
    let lead_w: usize = lead.iter().map(|s| s.content.chars().count()).sum();
    let right_w = right.content.chars().count();
    // lead + space + body + gap + right + one column of air before the seam
    let gap = (w as usize)
        .saturating_sub(lead_w + 2 + body_w + right_w)
        .max(1);
    let mut spans = lead;
    spans.push(Span::raw(" "));
    spans.append(&mut body);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(right);
    ListItem::new(Line::from(spans))
}

/// Projects-level rows read their counts off `stats` (U13/B) rather than scanning
/// disk. `stats` can lag `projects` by one snapshot right after a new project
/// registers — that degrades to a blank count, never a panic.
fn rail_project_items(
    projects: &[registry::ProjectEntry],
    stats: &[ProjectStat],
    app: &App,
    w: u16,
    focused: bool,
) -> Vec<ListItem<'static>> {
    if projects.is_empty() {
        return rail_empty("(no projects)");
    }
    let filter = app.filter.as_deref().filter(|f| !f.is_empty());
    let name_w = w.saturating_sub(14).clamp(8, 20) as usize;
    projects
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let sel = i == app.selected_project;
            let name = p.name.as_deref().unwrap_or("·");
            if let Some(f) = filter {
                if !project_matches_filter(projects, i, f) {
                    return rail_filtered_row(name, w);
                }
            }
            let stat = stats.get(i).copied().unwrap_or_default();
            // Roll-up: a run that wants the operator makes its whole project fly a ⚑.
            let mut body = vec![Span::styled(
                truncate(name, name_w),
                if sel {
                    selected(focused)
                } else {
                    Style::default().fg(INFO)
                },
            )];
            body.push(Span::styled(format!("  {}r", stat.n_runs), muted()));
            if stat.needs_you > 0 {
                body.push(Span::styled(
                    format!(" ⚑{}", stat.needs_you),
                    Style::default().fg(WARN).bold(),
                ));
            }
            rail_row(
                rail_lead(sel, focused, (stat.needs_you > 0).then_some(WARN)),
                body,
                Span::styled(relative_age(p.last_seen), muted()),
                w,
            )
        })
        .collect()
}

/// Home's rail rows: band headers dim and uppercase, run rows sharing `rail_row`'s
/// two fixed lead columns and right-aligned wait column with every other level, the
/// action row and project switcher closing out band 4.
fn rail_home_items(
    rows: &[HomeRow],
    projects: &[registry::ProjectEntry],
    app: &App,
    w: u16,
    focused: bool,
) -> Vec<ListItem<'static>> {
    if rows.is_empty() {
        return rail_empty("(loading)");
    }
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let sel = i == app.selected_home;
            match row {
                HomeRow::Header(b) => ListItem::new(Span::styled(
                    format!("  {}", home_band_label(*b)),
                    muted().bold(),
                )),
                HomeRow::Run { run, waited, .. } => {
                    let flag = if run.wants > 1 || run_attention(run).needs_you() {
                        Some(if run_attention(run) == Attention::Gate {
                            WARN
                        } else {
                            ALERT
                        })
                    } else {
                        None
                    };
                    let more = if run.wants > 1 {
                        format!(" ⚑{}", run.wants)
                    } else {
                        String::new()
                    };
                    let phase_w = w.saturating_sub(17).clamp(6, 16) as usize;
                    let body = vec![
                        Span::styled(
                            format!("{:<8}", truncate(&run.id, 8)),
                            if sel { selected(focused) } else { dim() },
                        ),
                        Span::styled(
                            format!("  {}", truncate(&rail_phase(run.phase), phase_w)),
                            Style::default().fg(phase_color(run.phase)),
                        ),
                        Span::styled(more, Style::default().fg(WARN).bold()),
                    ];
                    rail_row(
                        rail_lead(sel, focused, flag),
                        body,
                        Span::styled(relative_wait(*waited), muted()),
                        w,
                    )
                }
                HomeRow::More { n, .. } => ListItem::new(Span::styled(
                    format!("  … {n} more"),
                    Style::default().fg(FG_MUTED).italic(),
                )),
                HomeRow::Project(idx) => {
                    let name = projects
                        .get(*idx)
                        .and_then(|p| p.name.as_deref())
                        .unwrap_or("·");
                    let body = vec![Span::styled(
                        truncate(name, w.saturating_sub(4) as usize),
                        if sel {
                            selected(focused)
                        } else {
                            Style::default().fg(INFO)
                        },
                    )];
                    rail_row(rail_lead(sel, focused, None), body, Span::raw(""), w)
                }
                HomeRow::NewRun => {
                    let body = vec![Span::styled(
                        "n · start something new",
                        if sel {
                            selected(focused)
                        } else {
                            Style::default().fg(ACCENT)
                        },
                    )];
                    rail_row(rail_lead(sel, focused, None), body, Span::raw(""), w)
                }
            }
        })
        .collect()
}

fn rail_run_items(
    runs: &[state::RunSummary],
    app: &App,
    w: u16,
    focused: bool,
) -> Vec<ListItem<'static>> {
    if runs.is_empty() {
        return rail_empty("(no runs)");
    }
    let filter = app.filter.as_deref().filter(|f| !f.is_empty());
    let phase_w_base = w.saturating_sub(17).clamp(6, 16) as usize;
    runs.iter()
        .enumerate()
        .map(|(i, r)| {
            let sel = i == app.selected_run;
            if let Some(f) = filter {
                if !run_matches_filter(runs, i, f) {
                    return rail_filtered_row(&r.id, w);
                }
            }
            // A folded unit with more than one leg wanting you says so: the row can
            // only act on one of them at a time. Its columns come out of the phase,
            // never out of the row width.
            let more = if r.wants > 1 {
                format!(" ⚑{}", r.wants)
            } else {
                String::new()
            };
            let phase_w = phase_w_base.saturating_sub(more.chars().count()).max(4);
            // Phase reads "review" forever on a run nobody is driving; the red flag in
            // the lead column already says that, so the phase keeps all its columns.
            let (phase_text, phase_c) = (
                truncate(&rail_phase(r.phase), phase_w),
                if r.abandoned {
                    ALERT
                } else {
                    phase_color(r.phase)
                },
            );
            let flag = match run_attention(r) {
                Attention::Gate => Some(WARN),
                Attention::Broken => Some(ALERT),
                _ => None,
            };
            let body = vec![
                Span::styled(
                    format!("{:<8}", truncate(&r.id, 8)),
                    if sel { selected(focused) } else { dim() },
                ),
                Span::styled(format!("  {phase_text}"), Style::default().fg(phase_c)),
                Span::styled(more, Style::default().fg(WARN).bold()),
            ];
            rail_row(
                rail_lead(sel, focused, flag),
                body,
                Span::styled(relative_age(r.updated_at), muted()),
                w,
            )
        })
        .collect()
}

fn rail_slot_items(
    slots: &[SlotState],
    app: &App,
    w: u16,
    focused: bool,
) -> Vec<ListItem<'static>> {
    if slots.is_empty() {
        return rail_empty("(no agents yet)");
    }
    // Role first and never elided: it is the agent's identity. The provider-suffixed
    // slot id it replaces did not survive the rail at any width.
    let role_w = 9usize;
    let model_w = w.saturating_sub(20).clamp(4, 12) as usize;
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
            let broken = act.stalled || orphaned;
            let color = if broken { ALERT } else { slot_color(s) };
            // A live slot's cell is how long it has been quiet — including when that
            // silence is what makes it stalled or orphaned. The word is redundant
            // there (the red ⚑ in the lead column already says "broken") and the
            // duration is the part the operator cannot get anywhere else in this view.
            let tail = if broken || s.status == SlotStatus::Running {
                act.human_silent()
            } else {
                slot_status_label(s.status).to_string()
            };
            let body = vec![
                Span::styled(
                    format!("{} ", slot_icon(s, app)),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{:<role_w$}", truncate(&slot_short(slots, i), role_w)),
                    if sel { selected(focused) } else { dim() },
                ),
                Span::styled(slot_model(s, model_w), Style::default().fg(HINT)),
            ];
            rail_row(
                rail_lead(sel, focused, broken.then_some(ALERT)),
                body,
                Span::styled(tail, Style::default().fg(color)),
                w,
            )
        })
        .collect()
}

/// Main: ONE area, content = f(rail selection × tab). Its tabs live on the labels
/// row above, so nothing relocates when the tab changes and the pane itself carries
/// no border — one column of padding, then content.
#[allow(clippy::too_many_arguments)]
fn draw_main(
    f: &mut Frame,
    area: Rect,
    full: Option<&RunState>,
    stream_text: &str,
    activity: &[String],
    diff_text: &str,
    home: &HomeData,
    app: &mut App,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The Shell tab is a real terminal: it gets every column, unpadded, so the agent's
    // own layout is not reflowed by ours.
    let inner = if app.main_tab == MainTab::Shell {
        area
    } else {
        Rect {
            x: area.x.saturating_add(1),
            width: area.width.saturating_sub(1),
            ..area
        }
    };
    app.rect_main_inner = inner;
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // At Home every tab but Shell shows the same four-band overview — there is no
    // per-run Activity/Diff to show at a cross-project landing view. Shell stays the
    // real project-scoped workspace terminal regardless (see `manage_terminal`).
    if app.browse == BrowseLevel::Home && app.main_tab != MainTab::Shell {
        draw_home_body(f, inner, home, app);
        return;
    }

    // With no run selected, Log/Activity/Diff have nothing real to show — Log's
    // already-coherent empty message is the one story all three tell instead of each
    // inventing its own. Shell never joins that story: it is project-scoped, not
    // run-scoped (see `manage_terminal`), so it always shows the real workspace
    // terminal regardless of run count.
    match app.main_tab {
        MainTab::Log => draw_log_body(f, inner, full, stream_text, app),
        MainTab::Activity if full.is_none() => draw_log_body(f, inner, full, stream_text, app),
        MainTab::Activity => draw_activity_body(f, inner, activity, app),
        MainTab::Diff if full.is_none() => draw_log_body(f, inner, full, stream_text, app),
        MainTab::Diff => draw_diff_body(f, inner, diff_text, app),
        MainTab::Shell => draw_shell_body(f, inner, app),
    }
}

/// Main's Home body: the four bands (C7). Reuses the same scrollable-log viewport as
/// the other no-run bodies.
fn draw_home_body(f: &mut Frame, inner: Rect, home: &HomeData, app: &mut App) {
    let text = home_overview(&home.rows, &app.home_scope, app.home_watermark);
    app.stream_view_h = inner.height;
    app.stream_max = render_scrollable_log(
        f,
        inner,
        &text,
        &mut app.stream_scroll,
        &mut app.stream_follow,
        false,
        app.log_expand,
    );
}

/// The subtitle that rides after the tab strip: what the active tab is showing.
fn main_context(swarm: &SparPaths, full: Option<&RunState>, app: &App) -> String {
    match app.main_tab {
        // No run: there is no slot to name and nothing live streaming, so the caption
        // would just be a placeholder contradicting the empty state one row up.
        MainTab::Log if full.is_none() => String::new(),
        MainTab::Log => {
            let slot = full
                .map(|st| slot_short(&st.slots, app.selected_slot))
                .unwrap_or_else(|| "—".into());
            let mode = if app.log_expand { "wrap" } else { "trim" };
            let follow = if app.stream_follow { " · live" } else { "" };
            format!("{slot} · {mode}{follow}")
        }
        MainTab::Activity if full.is_none() => String::new(),
        MainTab::Activity => "run timeline + bus".into(),
        MainTab::Diff if full.is_none() => String::new(),
        MainTab::Diff => "artifacts".into(),
        MainTab::Shell => match app.takeover_target.as_deref() {
            Some(_) => {
                let run_id = full
                    .map(|st| truncate(&st.id, 8))
                    .unwrap_or_else(|| "agent".into());
                // The tmux pane is attached (`terminal_pane`) once the window is
                // actually resolvable, at which point the slot it names is worth
                // showing; before that (still resolving, or a run with no slots)
                // the shorter run-only form is all there is to say.
                match (full, app.terminal_pane.is_some()) {
                    (Some(st), true) => {
                        format!(
                            "agent · {run_id} ▸ {}",
                            slot_short(&st.slots, app.selected_slot)
                        )
                    }
                    _ => format!("agent · {run_id}"),
                }
            }
            None => {
                let base = swarm
                    .project_root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project");
                format!("shell · {base}")
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
                    billed_tokens: u.billed_tokens,
                    model: u.model.clone(),
                    session_id: None,
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
            ALERT
        } else {
            FG_MUTED
        };
        Span::styled(silent_hint.to_string(), Style::default().fg(c))
    };
    let Some(s) = stats else {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("waiting for agent output…", muted()),
                quiet,
            ])),
            area,
        );
        return;
    };
    let ctx = s.context_tokens;
    let ctx_color = if ctx > 150_000 {
        ALERT
    } else if ctx > 80_000 {
        WARN
    } else if ctx > 0 {
        OK
    } else {
        FG_MUTED
    };
    let tools_color = if s.tool_errors > 0 {
        ALERT
    } else if s.tools > 0 {
        INFO
    } else {
        FG_MUTED
    };
    let status_span = match status {
        Some(SlotStatus::Running) => Span::styled(" LIVE ", chip(INFO)),
        Some(SlotStatus::Done) => Span::styled(" DONE ", chip(OK)),
        Some(SlotStatus::Failed) => Span::styled(" FAIL ", chip(ALERT)),
        _ => Span::styled(" …… ", muted()),
    };
    let sep = || Span::styled("  ·  ", muted());
    let line = Line::from(vec![
        status_span,
        Span::raw("  "),
        Span::styled(
            format!("context {}", compact_u64(ctx)),
            Style::default().fg(ctx_color),
        ),
        sep(),
        Span::styled(
            format!("{} tools", s.tools),
            Style::default().fg(tools_color),
        ),
        sep(),
        Span::styled(format!("in {}", compact_u64(s.input_tokens)), dim()),
        Span::styled(
            format!("  out {}", compact_u64(s.output_tokens)),
            Style::default().fg(HINT),
        ),
        if s.cache_read_tokens > 0 {
            Span::styled(
                format!("  cache {}", compact_u64(s.cache_read_tokens)),
                dim(),
            )
        } else {
            Span::raw("")
        },
        match s.model.as_deref() {
            Some(m) => Span::styled(format!("  ·  {m}"), muted()),
            None => Span::raw(""),
        },
        quiet,
    ]);
    f.render_widget(
        Paragraph::new(Line::from(fit_spans(line.spans, area.width))),
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
            fill: Style::default().fg(FG),
        },
        text_area,
    );

    // Nothing to scroll to: don't paint a thumb that implies otherwise.
    if max_scroll > 0 {
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
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .style(Style::default().fg(RULE))
                .thumb_style(Style::default().fg(ACCENT_SOFT)),
            area,
            &mut sb,
        );
    }
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
    let base = Style::default();
    // Section headers (Activity's Run / Agents / Timeline / Bus / Quota bands) carry
    // weight in every mode — they are the only structure that view has.
    if line.starts_with('\u{a7}') {
        return base.fg(FG_DIM).bold();
    }
    if !colorize {
        return base.fg(FG_DIM);
    }
    let t = line.trim_start();
    if t.starts_with('▸') || t.starts_with('→') {
        base.fg(INFO)
    } else if t.starts_with('◂') || t.starts_with('←') {
        if t.contains('✗') || t.contains("err") {
            base.fg(ALERT)
        } else {
            base.fg(OK)
        }
    } else if t.starts_with('·') || t.starts_with('…') || t.starts_with('│') {
        base.fg(FG_MUTED).italic()
    } else if t.starts_with('!') {
        base.fg(ALERT).bold()
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
        let style = log_line_style(raw, colorize);
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
            let style = log_line_style(raw, colorize);
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

/// The project root, for shortening the absolute paths agents print. Set once per
/// process from the snapshot's own root — the log viewport has no other way to know it.
static PROJECT_PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Absolute paths under the project (and under its sibling slot worktrees) eat the
/// width without telling the reader anything: `/home/x/projects/biddesk/.spar/runs/...`
/// is 40 columns of prefix the operator already knows.
fn shorten_paths(s: &str) -> String {
    let Some(root) = PROJECT_PREFIX.get() else {
        return s.to_string();
    };
    if !s.contains(root.as_str()) {
        return s.to_string();
    }
    s.replace(&format!("{root}/"), "")
        .replace(root.as_str(), ".")
}

fn compact_log_line(raw: &str) -> String {
    let s = raw.trim_end();
    if s.is_empty() {
        return String::new();
    }
    // Section header: rendered as a spaced-out cap, the one typographic device a
    // terminal has for a heading.
    if let Some(rest) = s.strip_prefix('\u{a7}') {
        return rest.trim().to_uppercase();
    }
    // Tool call / result markers from stream coalescer
    if let Some(rest) = s.strip_prefix('→') {
        let rest = rest.trim();
        // "Bash  Fetch PR diff" → keep short tool + summary
        return format!("▸ {}", shorten_paths(&collapse_ws(rest)));
    }
    if let Some(rest) = s.strip_prefix('←') {
        let rest = strip_tool_id(rest.trim());
        return format!("◂ {}", shorten_paths(&collapse_ws(&rest)));
    }
    if let Some(rest) = s.strip_prefix('·') {
        return format!("  {}", collapse_ws(rest.trim()));
    }
    if s.starts_with('…') {
        return format!("  {}", collapse_ws(s.trim_start_matches('…').trim()));
    }
    collapse_ws(s)
}

/// Drop the provider's tool-call id from a result line. It is ~27 columns of opaque
/// hex that pairs with nothing on screen (the matching call line never carries it),
/// and on a narrow pane it pushed the actual result off the right edge. The ✓/✗ mark
/// stays: it is the row's only pass/fail signal.
fn strip_tool_id(s: &str) -> String {
    let mut it = s.split_whitespace();
    let Some(first) = it.next() else {
        return s.to_string();
    };
    let (mark, id) = if first == "✓" || first == "✗" {
        (Some(first), it.next())
    } else {
        (None, Some(first))
    };
    let Some(id) = id else {
        return s.to_string();
    };
    let opaque = id == "tool"
        || (id.len() >= 10
            && ["toolu_", "tooluse_", "call_", "fc_", "msg_"]
                .iter()
                .any(|p| id.starts_with(p)));
    if !opaque {
        return s.to_string();
    }
    let tail = match s.find(id) {
        Some(i) => s[i + id.len()..].trim_start(),
        None => "",
    };
    match mark {
        Some(m) => format!("{m} {tail}"),
        None => tail.to_string(),
    }
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
    // Show up to 8 completions at a time, scrolled to keep the selection in view —
    // PALETTE_CMDS has 12 verbs, so a hard cap here would make the last four
    // unreachable by browsing. On a short frame, shrink the menu first so the
    // input and hint rows (the frame's edges) are always the last thing cut.
    let max_menu_n = area.height.saturating_sub(4); // borders(2) + input(1) + hint(1)
    let menu_n = comps.len().min(8).min(max_menu_n as usize) as u16;
    let win_start = if menu_n == 0 {
        0
    } else if pal.sel >= menu_n as usize {
        (pal.sel + 1 - menu_n as usize).min(comps.len().saturating_sub(menu_n as usize))
    } else {
        0
    };
    // input row + completion rows + hint row + top/bottom border.
    let h = menu_n + 2 + 2;
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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            " : command ",
            Style::default().fg(ACCENT).bold(),
        ));
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
    for (i, c) in comps
        .iter()
        .enumerate()
        .skip(win_start)
        .take(menu_n as usize)
    {
        let selected = i == pal.sel;
        let mark = if selected { "▸ " } else { "  " };
        let base = if selected {
            Style::default().fg(ACCENT).bold()
        } else {
            dim()
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
        "Tab complete run · Enter run · Esc close".to_string()
    } else {
        "Tab complete · ↑↓ pick · Enter run · Esc close".to_string()
    };
    // The menu scrolls rather than hard-capping at 8 (AC-2), but an 8-row window
    // alone still looks like the whole list — nothing said `spawn`/`chat`/`help`/
    // `quit` exist below the fold. A position counter makes the overflow visible.
    let hint = if comps.len() > menu_n as usize {
        format!("{hint}  ({}/{})", pal.sel + 1, comps.len())
    } else {
        hint
    };
    rows.push(Line::from(Span::styled(
        hint,
        Style::default().fg(FG_MUTED).italic(),
    )));
    f.render_widget(Paragraph::new(rows), inner);
}

/// Driving mode's one-line banner replaces the status line: a loud recolored bar that
/// (with the collapsed rail and the bands folded away) makes the mode structurally
/// obvious — a text label alone is proven insufficient (Raskin).
fn draw_driving_banner(f: &mut Frame, area: Rect, app: &App) {
    let target = app
        .takeover_target
        .as_deref()
        .map(|s| s.strip_prefix("spar-").unwrap_or(s))
        .unwrap_or("workspace shell");
    let left = format!("  ▶ DRIVING · {target} ");
    let right = " keys → agent · F12 / C-a d → spar ";
    let bg = DRIVE_WASH;
    let used = (left.chars().count() + right.chars().count()) as u16;
    let pad = area.width.saturating_sub(used).max(1) as usize;
    let line = Line::from(vec![
        Span::styled(left, Style::default().fg(INK).bg(OK).bold()),
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        Span::styled(right, Style::default().fg(FG).bg(bg)),
    ]);
    f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &mut App, full: Option<&RunState>) {
    app.rect_help = Rect::default();
    app.rect_projects = Rect::default();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (msg, color) = if let Some((_, m, c, _)) = &app.flash {
        (m.as_str(), *c)
    } else if !app.status_line.is_empty() {
        (app.status_line.as_str(), WARN)
    } else {
        (
            situational_footer(full, app.focus, app.browse, app.main_tab),
            FG_MUTED,
        )
    };

    if full.map(|s| s.phase.is_gate()).unwrap_or(false) {
        // At a gate the tappable buttons live on the header; the footer just says so.
        f.render_widget(
            Paragraph::new("").style(Style::default().bg(GATE_WASH)),
            area,
        );
        let right = " YOUR MOVE ";
        let right_w = right.chars().count() as u16;
        if right_w >= area.width {
            return;
        }
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    " {}",
                    truncate(msg, area.width.saturating_sub(right_w + 2) as usize)
                ),
                Style::default().fg(color),
            )),
            area,
        );
        f.render_widget(
            Paragraph::new(Span::styled(right, chip(WARN))),
            Rect {
                x: area.right().saturating_sub(right_w),
                width: right_w,
                ..area
            },
        );
        return;
    }

    // Right cluster: two tappable words and the way out. Dim, because a footer is a
    // reference strip, not a call to action.
    let proj = "Projects";
    let help = "Help";
    let right: Vec<Span> = vec![
        Span::styled(proj, dim()),
        Span::styled("   ", muted()),
        Span::styled(help, dim()),
        Span::styled("  ·  ", muted()),
        Span::styled(": cmd", muted()),
        Span::styled(" · ", muted()),
        Span::styled("q quit", muted()),
        Span::raw(" "),
    ];
    let right_w: u16 = right.iter().map(|s| s.content.chars().count() as u16).sum();
    // On a sliver of a terminal the keys strip is the first thing to go: the left
    // hint is the one that changes with context.
    if right_w + 8 > area.width {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {}", truncate(msg, area.width.saturating_sub(1) as usize)),
                Style::default().fg(color),
            )),
            area,
        );
        return;
    }
    let right_x = area.right().saturating_sub(right_w);

    app.rect_projects = Rect {
        x: right_x,
        y: area.y,
        width: proj.chars().count() as u16,
        height: 1,
    };
    app.rect_help = Rect {
        x: right_x + (proj.chars().count() + 3) as u16,
        y: area.y,
        width: help.chars().count() as u16,
        height: 1,
    };

    let room = right_x.saturating_sub(area.x + 3);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", truncate(msg, room as usize)),
            Style::default().fg(color),
        )),
        area,
    );
    f.render_widget(
        Paragraph::new(Line::from(right)),
        Rect {
            x: right_x,
            width: right_w,
            ..area
        },
    );
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
        if st.phase == Phase::AwaitingRoundExtension {
            return "round ceiling — tap +4 rounds · :implement · CLI --max-rounds N";
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
            BrowseLevel::Home => "j/k · Enter open · n new run · P scope · p projects · a next-alert · : cmd · ? help",
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

/// Word-wrap a single line to `width` columns without collapsing internal
/// whitespace runs (unlike `soft_wrap`, which rejoins on a single space) — a
/// key and its description stay aligned by their run of spaces as long as the
/// line fits on one row; a row that has to wrap restarts at column 0 and does
/// not carry that indent forward.
fn wrap_line_preserve(line: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if width == 0 || chars.len() <= width {
        return vec![line.to_string()];
    }
    let mut rows = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut end = (start + width).min(chars.len());
        if end < chars.len() {
            if let Some(brk) = (start..end).rev().find(|&i| chars[i] == ' ') {
                if brk > start {
                    end = brk;
                }
            }
        }
        let row: String = chars[start..end].iter().collect();
        // A break search that lands inside leading indentation (width small enough
        // that the nearest space behind `end` is part of the indent, not a word gap)
        // produces a row of pure whitespace — drop it rather than growing the overlay
        // with a blank line indentation alone accounts for.
        if !row.is_empty() && !row.chars().all(|c| c == ' ') {
            rows.push(row);
        }
        start = end;
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
    }
    rows
}

const HELP_BODY: &str = r#" spar — rail + one main area

  Shape
    Rail   Home ▸ runs ▸ agents  (Enter pushes, Esc pops)
           p opens the project list; Home bands: needs you, running,
           finished since last look, start something new.
    Main   one area · tabs: Log · Activity · Diff · Shell
    Main always shows the rail's selection — nothing else moves.

  Keyboard
    1 / 2                focus Rail · Main
    Tab / Shift-Tab      cycle Rail ↔ Main
    j k  or  ↑ ↓         move in the rail · scroll Main · scroll this help
    Enter                push a rail level (on an agent: take it over)
    Esc                  pop a rail level · clear filter (never quits)
    [ ]                  previous / next Main tab
    + / _                zoom Main fullscreen / restore
    n                    new run + fleet picker
    P                    toggle Home scope (this project ↔ all)
    p                    jump to Projects
    a                    jump to the next run that needs you
    r / s                reject · ship (when gated; approve = tap / :approve)
    :                    command palette (approve/ship/takeover/…)
    /                    filter the rail
    w                    log wrap ↔ truncate long lines
    g / G                top / bottom of Main
    ?                    this help · Esc closes help
    q                    quit

  Shell tab = a real tmux client: every key goes to the agent (incl.
    Ctrl+C). prefix C-a · Ctrl+a d or F12 hands focus back to spar.
    Focusing it full-screen is Driving mode (green banner, bands collapsed).

  Mouse / touch: tap a tab, a rail row (double-tap = Enter), a gate
  button, or the breadcrumb (back to the rail). Scroll to scroll.

  Esc, ?, or tap to close help"#;

/// Sized to its content, up to the frame — never a fixed box that hard-clips a
/// line mid-word. Wraps at word boundaries when the frame is narrower than the
/// longest line, and scrolls with j/k when it is shorter than the content.
fn draw_help_overlay(f: &mut Frame, area: Rect, app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    const BORDER: u16 = 2;
    let lines: Vec<&str> = HELP_BODY.lines().collect();
    let content_w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let w = (content_w + BORDER).min(area.width);
    if w <= BORDER {
        return;
    }
    let inner_w = (w - BORDER) as usize;
    let wrapped: Vec<String> = lines
        .iter()
        .flat_map(|l| wrap_line_preserve(l, inner_w))
        .collect();
    let content_h = wrapped.len() as u16;
    let h = (content_h + BORDER).min(area.height);
    if h <= BORDER {
        return;
    }
    let inner_h = h - BORDER;
    let max_scroll = content_h.saturating_sub(inner_h);
    app.help_scroll = app.help_scroll.min(max_scroll);

    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let title = if max_scroll > 0 {
        " Help · j/k scroll "
    } else {
        " Help "
    };
    let p = Paragraph::new(wrapped.join("\n"))
        .style(Style::default().fg(FG))
        .scroll((app.help_scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(title, Style::default().fg(ACCENT).bold())),
        );
    f.render_widget(p, rect);
}

/// Phase D's new-run overlay: Project / Task / Fleet, sized to content and clamped
/// to the frame exactly like `draw_help_overlay` — reusing its clamp/centre
/// arithmetic so the 30-column panic class it already fixed cannot come back.
fn draw_new_run(f: &mut Frame, area: Rect, projects: &[registry::ProjectEntry], app: &mut App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(nr) = app.new_run.as_ref() else {
        return;
    };
    const BORDER: u16 = 2;
    let content_w: u16 = 64;
    let w = (content_w + BORDER).min(area.width);
    if w <= BORDER {
        return;
    }
    let inner_w = (w - BORDER) as usize;

    let field_style = |f: NewRunField| {
        if nr.field == f {
            Style::default().fg(ACCENT).bold()
        } else {
            Style::default().fg(FG)
        }
    };
    let project_label = nr
        .project
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "none — open spar in a project or choose one".to_string());
    let cycle_hint = if nr.projects.len() > 1 {
        "  ←/→"
    } else {
        ""
    };

    let mut lines: Vec<(String, Style)> = Vec::new();
    lines.push((
        format!("Project: {project_label}{cycle_hint}"),
        field_style(NewRunField::Project),
    ));
    lines.push((String::new(), Style::default()));
    lines.push((
        format!("Task: {}▌", nr.task),
        field_style(NewRunField::Task),
    ));
    lines.push((String::new(), Style::default()));
    lines.push(("Fleet:".to_string(), field_style(NewRunField::Fleet)));
    if nr.loading {
        lines.push((
            "  checking roster…".to_string(),
            Style::default().fg(FG_MUTED),
        ));
    } else if nr.roster.is_empty() {
        lines.push((
            "  nothing usable — spar doctor".to_string(),
            Style::default().fg(FG_MUTED),
        ));
    }
    const MAX_ROSTER_ROWS: usize = 8;
    // Keep `nr.sel` inside the painted window — otherwise a roster past the cap
    // has rows the keyboard can select but neither paint nor a click can reach.
    let roster_scroll = if nr.roster.len() <= MAX_ROSTER_ROWS {
        0
    } else {
        nr.sel
            .saturating_sub(MAX_ROSTER_ROWS - 1)
            .min(nr.roster.len() - MAX_ROSTER_ROWS)
    };
    let roster_window: Vec<(usize, &RosterEntry)> = nr
        .roster
        .iter()
        .enumerate()
        .skip(roster_scroll)
        .take(MAX_ROSTER_ROWS)
        .collect();
    if roster_scroll > 0 {
        lines.push((
            format!("  ↑ {roster_scroll} more"),
            Style::default().fg(FG_MUTED),
        ));
    }
    let roster_line_start = lines.len();
    for (i, e) in roster_window.iter().copied() {
        let picked_n = nr.picked.iter().position(|&p| p == i);
        let mark = match picked_n {
            Some(n) => format!("{}.", n + 1),
            None if e.available => "[ ]".to_string(),
            None => "[x]".to_string(),
        };
        let cursor = if nr.field == NewRunField::Fleet && nr.sel == i {
            ">"
        } else {
            " "
        };
        let style = if !e.available {
            Style::default().fg(FG_DIM)
        } else if picked_n.is_some() {
            Style::default().fg(OK)
        } else {
            Style::default().fg(FG)
        };
        let reason = e
            .reason
            .as_deref()
            .map(|r| format!("  ({r})"))
            .unwrap_or_default();
        let source = match e.source {
            RosterSource::Configured => "",
            RosterSource::Detected => "  detected",
            RosterSource::RecentFleet => "",
        };
        lines.push((
            format!("{cursor} {mark} {}{source}{reason}", e.label),
            style,
        ));
    }
    let below = nr.roster.len() - roster_scroll - roster_window.len();
    if below > 0 {
        lines.push((format!("  ↓ {below} more"), Style::default().fg(FG_MUTED)));
    }
    lines.push((String::new(), Style::default()));
    lines.push((
        "Tab field · space pick · Enter start · Esc cancel".to_string(),
        Style::default().fg(FG_MUTED),
    ));

    let content_h = lines.len() as u16;
    let h = (content_h + BORDER).min(area.height);
    if h <= BORDER {
        return;
    }
    let inner_h = (h - BORDER) as usize;

    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    // Click-outside-to-cancel / click-a-roster-row-to-toggle (D4), mirroring
    // `rect_palette`. Only rows actually painted (post-truncation, post-scroll)
    // are hit-testable, keyed by the roster's real index so a click on a
    // scrolled-into-view row toggles the right entry.
    app.rect_new_run_roster = roster_window
        .iter()
        .enumerate()
        .filter(|(row, _)| roster_line_start + row < inner_h)
        .map(|(row, (i, _))| {
            (
                *i,
                Rect {
                    x: rect.x + 1,
                    y: rect.y + 1 + (roster_line_start + row) as u16,
                    width: inner_w as u16,
                    height: 1,
                },
            )
        })
        .collect();
    app.rect_new_run = rect;
    f.render_widget(Clear, rect);
    let text: Vec<Line> = lines
        .into_iter()
        .take(inner_h)
        .map(|(s, style)| Line::from(Span::styled(truncate(&s, inner_w), style)))
        .collect();
    let p = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " New run ",
                Style::default().fg(ACCENT).bold(),
            )),
    );
    f.render_widget(p, rect);
    let _ = projects;
}

/// Rows/cols available to the embedded terminal, falling back to a standard 80x24
/// when the pane hasn't been laid out yet. The Shell tab is borderless and unpadded,
/// so this is the rect itself.
fn terminal_dims(rect: Rect) -> (u16, u16) {
    let rows = rect.height;
    let cols = rect.width;
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
        let (rows, cols) = terminal_dims(app.rect_main_inner);
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
             run a dev server, cargo, poke around; the shell stays alive across TUI restarts.\n\n\
             Or select an agent in the rail (Enter on a run, then Enter on a slot) to take over its live pane.\n\n\
             Full tmux underneath: prefix C-a, copy-mode/scroll, splits. Ctrl+a d / F12 → spar.\n\n\
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
            "Ctrl+a d / F12 / tap a tab → spar · C-a [ scroll/copy · ] paste · % / \" split · C-a s tmux picker",
        )
        .style(Style::default().fg(FG_DIM));
        f.render_widget(hint, footer);
    }
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

/// Resolve an `@mention` (from the `:` palette's `@`/`chat` form) to a unique bus id.
/// An already-qualified id (`run:slot`)
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
    let paths = SparPaths::new(&project_root);
    let base = state::RunState::load(&paths, &run.id)
        .ok()
        .and_then(|s| s.base_commit);
    let record =
        crate::worktree::create_worktree(&project_root, &run.id, &agent_id, base.as_deref())?;

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
                    Ok(m) => (m, OK),
                    Err(e) => (format!("spawn failed: {e:#}"), ALERT),
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
    has_runs: bool,
) -> String {
    let Some(st) = full else {
        cache.clear();
        // Distinct from "pick one of these" — there is nothing to pick yet, so the
        // empty state doesn't tell the operator to do something that isn't possible.
        return if has_runs {
            "\n  Select a run on the left.\n".into()
        } else {
            "\n  No runs yet.\n\n  New work:\n    spar plan -t \"describe the change\" --providers cli:claude\n".into()
        };
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

    lines.push(format!("\u{a7}Run {}", st.id));
    lines.push(format!("  {}", phase_label(st.phase)));
    if st.dry_run {
        lines.push("  dry-run".into());
    }
    if let Some(t) = st.task.as_deref() {
        lines.push(format!("  {}", truncate(t, 36)));
    }
    lines.push(String::new());

    // Compact agent status
    lines.push("\u{a7}Agents".into());
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
        lines.push("\u{a7}Timeline".into());
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
            lines.push("\u{a7}Bus".into());
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
        lines.push("\u{a7}Quota".into());
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

/// The phase in the width a rail column actually has. `phase_label` writes a sentence
/// for the header ("Needs plan approval"); at 9 columns that renders "Needs pl…", which
/// tells the operator nothing. These name the same states in the space available.
fn rail_phase(phase: Phase) -> String {
    match phase {
        Phase::Init | Phase::PrepareIsolation | Phase::SpawnSlots => "starting",
        Phase::Dispatch | Phase::WaitCompletion => "running",
        Phase::PlanReady => "plan ready",
        Phase::Spec => "spec",
        Phase::AwaitingPlanApproval => "plan gate",
        Phase::PlanApproved => "approved",
        Phase::PlanRejected => "rejected",
        Phase::Review => "review",
        Phase::Suite => "tests",
        Phase::Rank => "ranking",
        Phase::Fix => "fix",
        Phase::PeerRelay => "peers",
        Phase::AwaitingWinnerConfirm => "winner gate",
        Phase::AwaitingReconcile => "reconcile",
        Phase::AwaitingShipConfirm => "ship gate",
        Phase::AwaitingRoundExtension => "round gate",
        Phase::Shipping => "shipping",
        Phase::Done => "done",
        Phase::Escalated => "escalated",
        Phase::Failed => "failed",
        Phase::Stuck => "stuck",
        Phase::Quota => "quota",
        Phase::Stopped => "stopped",
    }
    .into()
}

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
        Phase::AwaitingRoundExtension => "Needs more rounds".into(),
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
        SlotStatus::Done => OK,
        SlotStatus::Failed | SlotStatus::Stuck => ALERT,
        SlotStatus::Running => INFO,
        SlotStatus::Pending => FG_MUTED,
    }
}

fn phase_color(phase: Phase) -> Color {
    match phase {
        Phase::Done | Phase::PlanApproved => OK,
        Phase::Failed | Phase::PlanRejected | Phase::Quota => ALERT,
        Phase::Stuck | Phase::Escalated => HINT,
        Phase::AwaitingPlanApproval
        | Phase::AwaitingWinnerConfirm
        | Phase::AwaitingShipConfirm
        | Phase::AwaitingReconcile
        | Phase::AwaitingRoundExtension => WARN,
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
/// How many runs want the operator. A folded row (U15) stands for several runs, so it
/// contributes each leg that wants you — otherwise a unit with two gates would read as
/// one and folding would become a way to hide a gate.
fn runs_needing_attention(runs: &[state::RunSummary]) -> usize {
    runs.iter()
        .map(|r| {
            if r.legs > 1 {
                r.wants as usize
            } else {
                usize::from(run_attention(r).needs_you())
            }
        })
        .sum()
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
                    Attention::Gate => ("needs your decision", WARN),
                    _ => ("needs attention", ALERT),
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
/// status line shows its gate/breakage. At Home it cycles through band 1 instead —
/// Home is not a place where `a` is dead (AC-29).
fn jump_to_attention(app: &mut App, runs: &[state::RunSummary], home_rows: &[HomeRow]) {
    if app.browse == BrowseLevel::Home {
        let candidates: Vec<usize> = home_rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                matches!(
                    r,
                    HomeRow::Run {
                        band: HomeBand::NeedsMe,
                        ..
                    }
                )
                .then_some(i)
            })
            .collect();
        if candidates.is_empty() {
            app.flash("nothing needs you", OK);
            return;
        }
        let next = match candidates.iter().position(|&i| i == app.selected_home) {
            Some(p) => candidates[(p + 1) % candidates.len()],
            None => candidates[0],
        };
        app.selected_home = next;
        app.home_key = home_rows.get(next).map(home_row_key);
        if let Some(HomeRow::Run { run, .. }) = home_rows.get(next) {
            app.flash(format!("→ {} needs you", truncate(&run.id, 8)), WARN);
        }
        return;
    }
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
            app.flash(format!("→ {} needs you", truncate(id, 8)), WARN);
        }
        None => app.flash("nothing needs you", OK),
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
    fn wide_layout_is_rail_plus_seam_plus_one_main() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let lay = layout_rects(area, Focus::Main, false, false);
        assert!(!lay.narrow);
        assert_eq!(lay.rail.width, rail_width(120));
        assert!(lay.main.width > 0);
        // Chrome is five rows: header, stepper, labels, rule, footer. No pane borders.
        assert_eq!(lay.header.height, 1);
        assert_eq!(lay.context.height, 1);
        assert_eq!(lay.labels.height, 1);
        assert_eq!(lay.rule.height, 1);
        assert_eq!(lay.footer.height, 1);
        assert_eq!(lay.rail.height + 5, area.height);
        // Rail, seam and Main are side by side and together fill the width.
        assert_eq!(lay.seam.width, 1);
        assert_eq!(lay.rail.right(), lay.seam.x);
        assert_eq!(lay.seam.right(), lay.main.x);
        assert_eq!(lay.main.right(), area.width);
        // The labels row and its rule span the whole frame, so the rail's title and
        // Main's tabs sit on one line.
        assert_eq!(lay.labels.width, area.width);
        assert_eq!(lay.rule.width, area.width);
    }

    /// The bands fold from the bottom up as the terminal shrinks, and the header,
    /// body and footer survive every size — including the 20x5 floor.
    #[test]
    fn chrome_bands_fold_on_short_terminals() {
        let at = |h: u16| {
            layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: h,
                },
                Focus::Main,
                false,
                false,
            )
        };
        let tall = at(40);
        assert_eq!((tall.context.height, tall.labels.height), (1, 1));
        let mid = at(10);
        assert_eq!(mid.context.height, 0, "stepper folds first");
        assert_eq!(mid.labels.height, 1, "tabs survive");
        let short = at(5);
        assert_eq!((short.context.height, short.labels.height), (0, 0));
        assert_eq!(short.header.height, 1);
        assert_eq!(short.footer.height, 1);
        assert!(short.main.height >= 2, "content never vanishes");
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
        assert_eq!(zoomed.seam, Rect::default());
        assert_eq!(zoomed.main.x, area.x);
        assert_eq!(zoomed.main.width, area.width);
        // Nothing else relocates.
        assert_eq!(zoomed.header, plain.header);
        assert_eq!(zoomed.context, plain.context);
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
        // Driving is banner + pane: every other band is gone.
        assert_eq!(driving.context.height, 0);
        assert_eq!(driving.labels.height, 0);
        assert_eq!(driving.rule.height, 0);
        let narrow = Rect { width: 60, ..area };
        let nd = layout_rects(narrow, Focus::Main, false, true);
        assert_eq!(nd.labels.height, 0);
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
        assert!(lay.labels.width > 0, "MainTab strip is tappable on a phone");
        assert!(lay.main.width > 0);
        assert_eq!(lay.rail, Rect::default(), "no rail in narrow");
        assert_eq!(lay.seam, Rect::default(), "no seam without a rail");
        // Rail focus swaps the single stage to the rail; the tab strip stays.
        let rail = layout_rects(area, Focus::Rail, false, false);
        assert!(rail.rail.width > 0);
        assert_eq!(rail.main, Rect::default());
        assert!(rail.labels.width > 0);
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
        let mut app = test_app();
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

    /// The help overlay sizes to its content, so on a tall enough terminal it covers
    /// the tab strip underneath it. A tap meant to dismiss help must not fall through
    /// to the strip's hit-test and silently change the active tab instead.
    #[test]
    fn tapping_help_over_the_tab_strip_dismisses_help_not_the_tab() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        app.open_main(MainTab::Log);
        app.show_help = true;
        // A tab-strip rect that would normally win the hit-test if help were not
        // checked first.
        app.main_tabs = vec![(
            Rect {
                x: 1,
                y: 2,
                width: 5,
                height: 1,
            },
            MainTab::Activity,
        )];
        let mut root = PathBuf::from("/x");
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            &swarm,
            &[],
            &[],
            &[],
            None,
            &mut root,
            None,
            0,
        );
        assert!(!app.show_help, "tap on the overlay must dismiss it");
        assert_eq!(
            app.main_tab,
            MainTab::Log,
            "tap on the overlay must not fall through to the tab strip underneath it"
        );
    }

    #[test]
    fn rail_pop_never_leaves_home() {
        let mut app = test_app();
        app.browse = BrowseLevel::Agents;
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Runs);
        app.rail_pop();
        assert_eq!(
            app.browse,
            BrowseLevel::Home,
            "Runs pops to Home, not Projects"
        );
        // Root: Esc is a no-op, never an exit.
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Home);
        assert_eq!(app.focus, Focus::Rail);
        // Projects survives as navigation reachable from Home, and pops back to it.
        app.browse = BrowseLevel::Projects;
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Home);
    }

    #[test]
    fn shell_active_only_on_focused_main_shell_tab() {
        let mut app = test_app();
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
        let mut app = test_app();
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
        let mut app = test_app();
        app.browse = BrowseLevel::Agents;
        let mut root = PathBuf::from("/x");
        rail_enter(&mut app, &[], &[], &[], Some(&st), &mut root, None);
        assert!(app.takeover_target.is_none());
        assert_eq!(app.focus, Focus::Rail, "headless run: nothing to take over");
    }

    /// The palette's `:implement` and the gate button share one argv builder: without
    /// `--max-rounds` the detached process gates again immediately and the TUI reports
    /// "Implement started" over a run that never moved.
    #[test]
    fn implement_argv_buys_rounds_only_at_the_round_gate() {
        use crate::cli::WorkflowKind;
        let mut st = RunState::new("r1", WorkflowKind::Loop, std::path::PathBuf::from("/x"));
        st.providers = vec!["cli:claude".into()];
        st.max_rounds = 8;

        st.phase = Phase::PlanApproved;
        let args = implement_argv(&st);
        assert!(!args.iter().any(|a| a == "--max-rounds"), "{args:?}");

        st.phase = Phase::AwaitingRoundExtension;
        let args = implement_argv(&st);
        let i = args
            .iter()
            .position(|a| a == "--max-rounds")
            .expect("the round gate must be lifted, not re-hit");
        assert_eq!(args[i + 1], (8 + ROUND_GRANT).to_string());
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
        // Every gate needs a way out of it from the TUI, or the phase is a dead end
        // with a footer hint and nothing to press.
        st.phase = Phase::AwaitingRoundExtension;
        let b = gate_buttons_for(Some(&st));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].1, GateAction::MoreRounds);
    }

    #[test]
    fn gate_buttons_render_and_record_hit_rects() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(90, 3)).unwrap();
        let mut app = test_app();
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
        // Both buttons sit on the top row, in order, inside the area.
        assert!(app
            .gate_buttons
            .iter()
            .all(|(r, _)| r.y == 0 && r.right() <= 90));
        assert!(app.gate_buttons[0].0.x < app.gate_buttons[1].0.x);
        assert_eq!(app.gate_buttons[1].1, GateAction::Reject);
    }

    /// U11: the gate zone is reserved from the layout, so a different gate's labels
    /// cannot slide the first button out from under a click already on its way.
    #[test]
    fn gate_buttons_start_at_a_fixed_x_across_gates() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 1,
        };
        let first_x = |buttons: Vec<(&str, GateAction)>| {
            let mut term = Terminal::new(TestBackend::new(120, 1)).unwrap();
            let mut app = test_app();
            term.draw(|f| render_gate_buttons(f, area, &mut app, &buttons))
                .unwrap();
            app.gate_buttons[0].0.x
        };
        let approve = first_x(vec![
            ("Approve", GateAction::Approve),
            ("Reject", GateAction::Reject),
        ]);
        let ship = first_x(vec![("Ship", GateAction::Ship)]);
        let winner = first_x(vec![
            ("Confirm", GateAction::ConfirmWinner),
            ("Reconcile", GateAction::Reconcile),
        ]);
        assert_eq!(approve, ship);
        assert_eq!(approve, winner);
        assert_eq!(approve, area.right() - GATE_ZONE_W);
        // The zone covers every band that has a rail, not just the widest one.
        assert!(gate_zone(Rect { width: 80, ..area }).is_some());
        assert!(gate_zone(Rect { width: 79, ..area }).is_none());
    }

    /// A Paragraph wider than its rect is clipped with no ellipsis, and gate buttons
    /// overpaint what is under them: either way the breadcrumb loses its tail without
    /// saying so. At every width the text must stop before the buttons.
    #[test]
    fn the_breadcrumb_is_never_buried_under_the_gate_buttons() {
        use crate::cli::WorkflowKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut st = RunState::new(
            "3f2a91c0",
            WorkflowKind::Loop,
            std::path::PathBuf::from("/x/a-project-with-a-long-name"),
        );
        st.phase = Phase::AwaitingPlanApproval;
        let swarm = SparPaths::new("/x/a-project-with-a-long-name");
        for w in 30..=140u16 {
            let mut term = Terminal::new(TestBackend::new(w, 1)).unwrap();
            let mut app = test_app();
            app.human_alerts_n = 7;
            term.draw(|f| {
                let area = f.area();
                draw_header(
                    f,
                    area,
                    &swarm,
                    &[],
                    &[],
                    Some(&st),
                    &HomeData::default(),
                    &mut app,
                );
            })
            .unwrap();
            let Some((first, _)) = app.gate_buttons.first().copied() else {
                continue;
            };
            let buf = term.backend().buffer();
            assert!(first.x > 0, "w={w}");
            assert_eq!(
                buf[(first.x - 1, 0)].symbol(),
                " ",
                "text ran under the gate buttons at w={w}"
            );
        }
    }

    #[test]
    fn header_carries_breadcrumb_and_gate_buttons() {
        use crate::cli::WorkflowKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut st = RunState::new("run1", WorkflowKind::Arena, std::path::PathBuf::from("/x"));
        st.phase = Phase::AwaitingWinnerConfirm;
        let swarm = SparPaths::new("/x");
        let mut term = Terminal::new(TestBackend::new(120, 1)).unwrap();
        let mut app = test_app();
        app.human_alerts_n = 2;
        term.draw(|f| {
            let area = f.area();
            draw_header(
                f,
                area,
                &swarm,
                &[],
                &[],
                Some(&st),
                &HomeData::default(),
                &mut app,
            );
        })
        .unwrap();
        let row: String = {
            let buf = term.backend().buffer();
            (0..120).map(|x| buf[(x, 0)].symbol()).collect()
        };
        assert!(row.contains("spar"), "row was: {row:?}");
        assert!(row.contains("run run1"), "breadcrumb · row was: {row:?}");
        assert!(row.contains("⚠2"), "alert badge · row was: {row:?}");
        assert!(row.contains("Confirm"), "row was: {row:?}");
        assert!(row.contains("Reconcile"), "row was: {row:?}");
        assert_eq!(app.gate_buttons.len(), 2);
    }

    /// The tabs live on the labels row and are marked by the rule beneath them, so a
    /// tab switch repaints two rows and moves nothing.
    #[test]
    fn main_tab_strip_is_hit_testable_and_underlined() {
        use crate::cli::WorkflowKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let st = RunState::new("run1", WorkflowKind::Loop, std::path::PathBuf::from("/x"));
        let swarm = SparPaths::new("/x");
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let mut app = test_app();
        app.human_alerts_n = 3;
        app.open_main(MainTab::Activity);
        let lay = layout_rects(
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 20,
            },
            Focus::Main,
            false,
            false,
        );
        term.draw(|f| {
            draw_labels(
                f,
                &lay,
                &swarm,
                &[],
                &[],
                Some(&st),
                &HomeData::default(),
                &mut app,
            );
            draw_rule(f, &lay, &app);
        })
        .unwrap();
        assert_eq!(app.main_tabs.len(), MAIN_TABS.len());
        assert!(app.main_tabs.iter().all(|(r, _)| r.y == lay.labels.y));
        assert!(app.main_tabs[0].0.x >= lay.main.x);
        assert!(app.main_tabs.windows(2).all(|w| w[0].0.x < w[1].0.x));
        assert_eq!(app.main_tabs[3].1, MainTab::Shell);

        let row = |y: u16| -> String {
            let buf = term.backend().buffer();
            (0..120).map(|x| buf[(x, y)].symbol()).collect()
        };
        let labels = row(lay.labels.y);
        assert!(labels.contains("Activity ⚠3"), "labels were: {labels:?}");
        assert!(labels.contains("Shell"), "labels were: {labels:?}");
        assert!(labels.contains("RUNS"), "rail title · was: {labels:?}");

        // The rule carries the active tab's underline and the rail seam's tee.
        let rule_row = row(lay.rule.y);
        let active = app
            .main_tabs
            .iter()
            .find(|(_, t)| *t == MainTab::Activity)
            .unwrap()
            .0;
        assert_eq!(rule_row.chars().nth(active.x as usize), Some('━'));
        assert_eq!(rule_row.chars().nth(lay.seam.x as usize), Some('┬'));
        assert_eq!(rule_row.chars().next(), Some('─'));
    }

    #[test]
    fn list_row_hit() {
        let r = Rect {
            x: 0,
            y: 10,
            width: 20,
            height: 8,
        };
        // Borderless: the rail's first row IS the first item.
        assert_eq!(list_row_at(r, 10, 3, 0), Some(0));
        assert_eq!(list_row_at(r, 11, 3, 0), Some(1));
        assert_eq!(list_row_at(r, 13, 3, 0), None, "past the last item");
        assert_eq!(list_row_at(r, 20, 30, 0), None, "past the pane");
        assert_eq!(list_row_at(r, 9, 3, 0), None, "above the pane");
        // Scrolled list: first visible row is index 2
        assert_eq!(list_row_at(r, 10, 10, 2), Some(2));
        assert_eq!(list_row_at(r, 11, 10, 2), Some(3));
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
            archived: false,
            phase: Phase::Review,
            updated_at: Utc::now(),
            task: task.map(str::to_string),
            dry_run: false,
            abandoned: false,
            parent_run: None,
            round: 1,
            legs: 1,
            wants: 0,
            base_ref: None,
            base_commit: None,
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
        let mut app = test_app();
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
        let mut app = test_app();
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
        let mut app = test_app();
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        jump_to_attention(&mut app, &runs, &[]);
        assert_eq!(app.selected_run, 2, "lands on the gated run");
        // From the gate it wraps and, finding no other, stays put.
        jump_to_attention(&mut app, &runs, &[]);
        assert_eq!(app.selected_run, 2);
    }

    #[test]
    fn toasts_prime_silently_then_fire_on_transition() {
        let mut app = test_app();
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

/// Rendering stability (U12): the shell must paint without panicking at any size the
/// operator can produce, and the regions the eye anchors on must not move when the
/// data behind them changes.
#[cfg(test)]
mod render_stability {
    use super::*;
    use crate::cli::WorkflowKind;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn run_with(phase: Phase, slots: usize) -> RunState {
        let mut st = RunState::new("3f2a91c0", WorkflowKind::Loop, PathBuf::from("/x"));
        st.phase = phase;
        st.task = Some("stop prose mentions creating phantom criteria".into());
        let roles = [
            crate::state::SlotRole::Planner,
            crate::state::SlotRole::PlanCritic,
            crate::state::SlotRole::TestAuthor,
            crate::state::SlotRole::Implementer,
            crate::state::SlotRole::Tester,
            crate::state::SlotRole::Reviewer,
            crate::state::SlotRole::Reviewer,
        ];
        for (i, role) in roles.iter().take(slots).enumerate() {
            let mut slot = crate::executor::init_slot(format!("slot-{i}"), "cli:claude", *role);
            slot.status = if i + 1 == slots {
                SlotStatus::Running
            } else {
                SlotStatus::Done
            };
            slot.model = Some("claude-opus-5".into());
            st.slots.push(slot);
        }
        st
    }

    fn paint(
        w: u16,
        h: u16,
        projects: &[registry::ProjectEntry],
        runs: &[state::RunSummary],
        full: Option<&RunState>,
    ) -> Terminal<TestBackend> {
        paint_with(w, h, projects, runs, full, |_| {})
    }

    /// `tweak` runs against the `App` after construction, so a sweep can cover the
    /// overlays: the help window and the `:` palette are the only widgets left that
    /// size themselves rather than taking a band, which is exactly where a rect can
    /// still escape the frame.
    fn paint_with(
        w: u16,
        h: u16,
        projects: &[registry::ProjectEntry],
        runs: &[state::RunSummary],
        full: Option<&RunState>,
        tweak: impl Fn(&mut App),
    ) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        tweak(&mut app);
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                projects,
                runs,
                full,
                "→ Bash  read the contract\n← ✓ toolu_01HqnTTSQH5m7ZWYJVAtA7Vj ok\n",
                &["§Timeline".into(), " 19:04 impl done".into()],
                "diff",
                &HomeData::default(),
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        term
    }

    fn row(term: &Terminal<TestBackend>, y: u16) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// Every size from a single cell up, swept rather than sampled: ratatui panics on
    /// any Rect that leaves the buffer, and the band arithmetic has four breakpoints.
    #[test]
    fn renders_at_every_size_without_panicking() {
        let st = run_with(Phase::AwaitingShipConfirm, 7);
        for w in (1..=200).step_by(3) {
            for h in (1..=60).step_by(2) {
                paint(w, h, &[], &[], Some(&st));
                if w % 9 == 1 {
                    // `?` on a 30-column terminal used to panic: the help rect clamped
                    // UP to its minimum and left the buffer.
                    paint_with(w, h, &[], &[], Some(&st), |a| a.show_help = true);
                    paint_with(w, h, &[], &[], Some(&st), |a| {
                        a.palette = Some(Palette::default())
                    });
                }
            }
        }
        // The breakpoints themselves, and the no-run path.
        for (w, h) in [
            (1, 1),
            (20, 5),
            (79, 24),
            (80, 24),
            (89, 24),
            (90, 24),
            (119, 40),
            (120, 40),
            (200, 60),
        ] {
            paint(w, h, &[], &[], Some(&st));
            paint(w, h, &[], &[], None);
        }
    }

    /// The Projects level renders its own row shape, and it is the one rail level the
    /// other tests never reach (they all pass an empty project list).
    #[test]
    fn renders_the_projects_level() {
        let projects: Vec<registry::ProjectEntry> =
            ["acme-api", "spar", "a-very-long-project-name"]
                .iter()
                .map(|n| registry::ProjectEntry {
                    root: PathBuf::from("/nonexistent").join(n),
                    name: Some((*n).to_string()),
                    last_seen: Utc::now(),
                    last_run_id: None,
                })
                .collect();
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        app.open_projects_view();
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                &projects,
                &[],
                None,
                "",
                &[],
                "",
                &HomeData::default(),
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        let painted: String = {
            let buf = term.backend().buffer();
            (0..30)
                .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(painted.contains("acme-api"), "rail was: {painted:?}");
        assert!(painted.contains("PROJECTS"), "rail was: {painted:?}");
        // The band counts what this level actually has. The refresher hands us no
        // runs outside a project, so a run roll-up here would always read "none".
        let band: String = {
            let buf = term.backend().buffer();
            (0..120).map(|x| buf[(x, 1)].symbol()).collect()
        };
        assert!(band.contains("3 projects"), "band was: {band:?}");
        assert!(!band.contains("no runs"), "band was: {band:?}");
    }

    /// The scale that already bit once: thousands of run dirs on one project.
    #[test]
    fn renders_four_hundred_runs() {
        let runs: Vec<state::RunSummary> = (0..400)
            .map(|i| state::RunSummary {
                id: format!("run{i:04}"),
                workflow: WorkflowKind::Loop,
                archived: false,
                phase: Phase::Review,
                updated_at: Utc::now(),
                task: Some("a queued run".into()),
                dry_run: false,
                abandoned: i % 7 == 0,
                parent_run: None,
                round: 1,
                legs: 1,
                wants: 0,
                base_ref: None,
                base_commit: None,
                project_root: None,
                project_name: None,
            })
            .collect();
        let term = paint(120, 40, &[], &runs, None);
        assert!(row(&term, 0).contains("spar"));
    }

    /// U11: every tab keeps its column when the active tab changes AND when the
    /// Activity badge appears or grows — Activity is second of four, so an unreserved
    /// badge would shift Diff and Shell out from under a click.
    #[test]
    fn tab_positions_hold_across_tab_and_badge_changes() {
        let st = run_with(Phase::Review, 7);
        let tabs_x = |tab: MainTab, alerts: usize| {
            let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            app.open_main(tab);
            app.human_alerts_n = alerts;
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    Some(&st),
                    "",
                    &[],
                    "",
                    &HomeData::default(),
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();
            app.main_tabs.iter().map(|(r, _)| r.x).collect::<Vec<_>>()
        };
        let base = tabs_x(MainTab::Log, 0);
        assert_eq!(base.len(), MAIN_TABS.len());
        for tab in MAIN_TABS {
            for alerts in [0, 1, 9, 12, 99] {
                assert_eq!(
                    tabs_x(tab, alerts),
                    base,
                    "tabs moved on {tab:?} with {alerts} alerts"
                );
            }
        }
    }

    /// The stepper is read off the slots that ran, so it says the same thing whether
    /// or not the phase name happens to mention the step.
    #[test]
    fn stepper_tracks_slots_and_gates() {
        let st = run_with(Phase::Review, 6);
        let steps = run_steps(&st, false);
        let by = |steps: &[(&str, StepState)], name: &str| {
            steps.iter().find(|(l, _)| *l == name).unwrap().1
        };
        assert_eq!(by(&steps, "plan"), StepState::Done);
        assert_eq!(by(&steps, "review"), StepState::Active);
        assert_eq!(by(&steps, "ship"), StepState::Pending);

        let mut gated = run_with(Phase::AwaitingPlanApproval, 2);
        gated
            .slots
            .iter_mut()
            .for_each(|s| s.status = SlotStatus::Done);
        let steps = run_steps(&gated, false);
        assert_eq!(by(&steps, "critique"), StepState::Gate);
        assert_eq!(by(&steps, "build"), StepState::Pending);

        let shipping = run_with(Phase::AwaitingShipConfirm, 7);
        assert_eq!(by(&run_steps(&shipping, false), "ship"), StepState::Gate);
    }

    /// The pipeline is keyed on the workflow. An arena has no planner and a roles run
    /// has nothing but peers; showing either one the seven-step loop pipeline invents
    /// steps that never existed and marks them as still to come.
    #[test]
    fn stepper_shape_follows_the_workflow() {
        use crate::cli::WorkflowKind;
        let labels = |k: WorkflowKind| {
            steps_for(k)
                .iter()
                .map(|(l, _)| *l)
                .collect::<Vec<&'static str>>()
        };
        assert_eq!(
            labels(WorkflowKind::Loop),
            ["plan", "critique", "spec", "build", "tests", "review", "ship"]
        );
        assert_eq!(
            labels(WorkflowKind::Arena),
            ["build", "rank", "reconcile", "review", "ship"]
        );
        assert_eq!(labels(WorkflowKind::Roles), ["peers", "ship"]);
        assert_eq!(labels(WorkflowKind::Peer), ["peers", "ship"]);
        assert_eq!(labels(WorkflowKind::Review), ["review", "ship"]);

        // A roles run's peers are its whole pipeline, and they are visible while live.
        let mut st = RunState::new("r", WorkflowKind::Roles, PathBuf::from("/x"));
        st.phase = Phase::Dispatch;
        for i in 0..2 {
            let mut s =
                crate::executor::init_slot(format!("role-{i}"), "cli:claude", SlotRole::Peer);
            s.status = SlotStatus::Running;
            st.slots.push(s);
        }
        let steps = run_steps(&st, false);
        assert_eq!(steps[0], ("peers", StepState::Active));

        // The arena's winner gate is the ranking gate, not the ship gate.
        let mut arena = RunState::new("a", WorkflowKind::Arena, PathBuf::from("/x"));
        arena.phase = Phase::AwaitingWinnerConfirm;
        for (role, status) in [
            (SlotRole::Implementer, SlotStatus::Done),
            (SlotRole::Implementer, SlotStatus::Failed),
            (SlotRole::Ranker, SlotStatus::Done),
        ] {
            let mut s = crate::executor::init_slot("s", "cli:claude", role);
            s.status = status;
            arena.slots.push(s);
        }
        let steps = run_steps(&arena, false);
        let by = |name: &str| steps.iter().find(|(l, _)| *l == name).unwrap().1;
        assert_eq!(by("rank"), StepState::Gate, "the winner gate holds ranking");
        assert_eq!(by("ship"), StepState::Pending);
        // One of four implementers dying is the expected arena outcome, not a failed
        // build step.
        assert_eq!(by("build"), StepState::Done);
    }

    /// A channel that was switched off did not "not happen yet" — it is never coming,
    /// and the row says so with a different mark.
    #[test]
    fn stepper_marks_skipped_rather_than_pending() {
        let mut st = run_with(Phase::Done, 7);
        st.slots.retain(|s| s.role != SlotRole::Tester); // [suite] enabled = false
        st.slots
            .iter_mut()
            .for_each(|s| s.status = SlotStatus::Done);
        let steps = run_steps(&st, false);
        let by = |name: &str| steps.iter().find(|(l, _)| *l == name).unwrap().1;
        assert_eq!(by("tests"), StepState::Skipped);
        assert_eq!(by("ship"), StepState::Done);
        // A run that simply has not got there yet still reads as pending.
        let early = run_with(Phase::Spec, 3);
        let steps = run_steps(&early, false);
        assert_eq!(
            steps.iter().find(|(l, _)| *l == "tests").unwrap().1,
            StepState::Pending
        );
    }

    /// The gate is the actionable fact, so it outranks the state of the slot it hangs
    /// off: a tolerated critic failure must not swallow the plan gate's flag.
    #[test]
    fn stepper_flags_the_gate_even_when_that_step_failed() {
        let mut st = run_with(Phase::AwaitingPlanApproval, 2);
        st.slots[0].status = SlotStatus::Done; // planner
        st.slots[1].status = SlotStatus::Failed; // plan_critic: tolerated, plan.rs:181
        let steps = run_steps(&st, false);
        assert_eq!(
            steps.iter().find(|(l, _)| *l == "critique").unwrap().1,
            StepState::Gate
        );
        // Rejected is not pending either.
        let mut rejected = run_with(Phase::PlanRejected, 2);
        rejected
            .slots
            .iter_mut()
            .for_each(|s| s.status = SlotStatus::Done);
        assert_eq!(
            run_steps(&rejected, false)
                .iter()
                .find(|(l, _)| *l == "critique")
                .unwrap()
                .1,
            StepState::Failed
        );
    }

    /// Nobody is driving it: a halted, quota-paused or abandoned run must not keep
    /// claiming a step is in progress while the header says ABANDONED.
    #[test]
    fn stepper_halts_instead_of_claiming_live() {
        let live = run_with(Phase::Review, 6);
        assert_eq!(
            run_steps(&live, false)
                .iter()
                .find(|(l, _)| *l == "review")
                .unwrap()
                .1,
            StepState::Active
        );
        assert_eq!(
            run_steps(&live, true)
                .iter()
                .find(|(l, _)| *l == "review")
                .unwrap()
                .1,
            StepState::Halted,
            "abandoned"
        );
        for phase in [Phase::Stopped, Phase::Quota] {
            let mut st = run_with(Phase::Review, 6);
            st.phase = phase;
            assert_eq!(
                run_steps(&st, false)
                    .iter()
                    .find(|(l, _)| *l == "review")
                    .unwrap()
                    .1,
                StepState::Halted,
                "{phase:?}"
            );
        }
        for phase in [Phase::Failed, Phase::Stuck, Phase::Escalated] {
            let mut st = run_with(Phase::Review, 6);
            st.phase = phase;
            assert_eq!(
                run_steps(&st, false)
                    .iter()
                    .find(|(l, _)| *l == "review")
                    .unwrap()
                    .1,
                StepState::Failed,
                "{phase:?}"
            );
        }
    }

    /// The stepper degrades to glyphs plus the live label, and drops even that rather
    /// than clip it. Checked against the widest live label, not a convenient one.
    #[test]
    fn stepper_never_clips_a_label() {
        for phase in [
            Phase::Spec,
            Phase::Review,
            Phase::Suite,
            Phase::AwaitingShipConfirm,
        ] {
            for slots in 1..=7 {
                let st = run_with(phase, slots);
                let steps = run_steps(&st, false);
                for w in 0..=90u16 {
                    let spans = stepper_spans(&steps, w, "◐");
                    let painted: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                    assert!(
                        painted <= w as usize,
                        "{phase:?} with {slots} slots overflowed {w}: {painted}"
                    );
                }
            }
        }
        // The live step still keeps its name whenever there is room for it.
        let st = run_with(Phase::Review, 6);
        let tight: String = stepper_spans(&run_steps(&st, false), 20, "◐")
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            tight.contains("review"),
            "the live step keeps its name: {tight:?}"
        );
        assert!(
            !tight.contains("critique"),
            "finished steps give theirs up: {tight:?}"
        );
    }

    /// The rail column is 9-12 columns wide; a phase name that does not fit is a
    /// phase name the operator never reads.
    #[test]
    fn every_phase_fits_the_rail_column() {
        use crate::state::Phase::*;
        for phase in [
            Init,
            PrepareIsolation,
            SpawnSlots,
            Dispatch,
            WaitCompletion,
            PlanReady,
            Spec,
            AwaitingPlanApproval,
            PlanApproved,
            PlanRejected,
            Review,
            Suite,
            Rank,
            Fix,
            PeerRelay,
            AwaitingWinnerConfirm,
            AwaitingReconcile,
            AwaitingShipConfirm,
            AwaitingRoundExtension,
            Shipping,
            Done,
            Escalated,
            Failed,
            Stuck,
            Quota,
            Stopped,
        ] {
            let label = rail_phase(phase);
            assert!(
                label.chars().count() <= 11,
                "{phase:?} renders {label:?}, which the rail truncates"
            );
            assert!(!label.is_empty(), "{phase:?}");
        }
    }

    /// Absolute project paths are 40 columns of prefix the reader already knows.
    #[test]
    fn log_lines_shorten_project_paths() {
        let _ = PROJECT_PREFIX.set("/home/x/projects/acme".into());
        let line =
            compact_log_line("→ Read  /home/x/projects/acme/.spar/runs/3f2a/artifacts/plan.md");
        assert_eq!(line, "▸ Read .spar/runs/3f2a/artifacts/plan.md");
        // A path outside the project keeps every character.
        let other = compact_log_line("→ Read  /etc/hosts");
        assert_eq!(other, "▸ Read /etc/hosts");
    }

    #[test]
    fn tool_ids_are_stripped_from_result_lines() {
        let line = compact_log_line("← ✓  toolu_01HqnTTSQH5m7ZWYJVAtA7Vj  fn git(args: &[&str])");
        assert_eq!(line, "◂ ✓ fn git(args: &[&str])");
        // A result that leads with real content keeps every word of it.
        let plain = compact_log_line("← ✗  cargo test failed");
        assert_eq!(plain, "◂ ✗ cargo test failed");
    }

    /// A slot's identity in the rail is its role, never the provider-suffixed id.
    #[test]
    fn slot_names_are_roles_numbered_only_when_they_collide() {
        let st = run_with(Phase::Review, 7);
        assert_eq!(slot_short(&st.slots, 0), "planner");
        assert_eq!(slot_short(&st.slots, 5), "review 0");
        assert_eq!(slot_short(&st.slots, 6), "review 1");
    }

    /// The model column has to separate the tiers the fleet policy is built on. A
    /// head-first shortening renders every Gemini as `gemini`, which is useless.
    #[test]
    fn model_labels_keep_the_tier() {
        let label = |model: Option<&str>, provider: &str, w: usize| {
            let mut s = crate::executor::init_slot("s", provider, SlotRole::Reviewer);
            s.model = model.map(str::to_string);
            slot_model(&s, w)
        };
        assert_eq!(label(Some("claude-opus-5"), "cli:claude", 12), "opus-5");
        // The 80-119 band's rail leaves 6 columns here, so this is the common case:
        // opus, sonnet and haiku must not all shorten to their shared version.
        let narrow: Vec<String> = [
            "claude-opus-4-5-20250929",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
        ]
        .iter()
        .map(|m| label(Some(m), "cli:claude", 6))
        .collect();
        assert_eq!(narrow, ["opus", "sonnet", "haiku"], "tiers collapsed at 6");
        assert_ne!(
            label(Some("google/gemini-3.7-flash"), "cli:opencode", 6),
            label(Some("google/gemini-3.7-pro"), "cli:opencode", 6),
        );
        // A release date is dropped; a version number is not.
        assert_eq!(
            label(Some("claude-opus-4-5-20250929"), "cli:claude", 12),
            "opus-4-5"
        );
        assert_eq!(
            label(Some("claude-3-5-haiku-20241022"), "cli:claude", 12),
            "3-5-haiku"
        );
        // Same family, different tier: the labels must differ.
        let flash = label(Some("google/gemini-3.7-flash"), "cli:opencode", 12);
        let pro = label(Some("google/gemini-3.7-pro"), "cli:opencode", 12);
        assert_ne!(flash, pro, "flash and pro rendered the same");
        assert!(flash.ends_with("flash"), "{flash}");
        assert!(pro.ends_with("pro"), "{pro}");
        assert!(flash.starts_with("gemini"), "room for both at 12: {flash}");
        // No model recorded: name the adapter, never an empty cell.
        assert_eq!(label(None, "cli:opencode", 12), "opencode");
        assert_eq!(label(None, "cli:claude", 12), "claude");
        // What the provider says it served beats what was asked for.
        let mut s = crate::executor::init_slot("s", "cli:opencode", SlotRole::Reviewer);
        s.model = Some("anthropic/claude-opus-4.8".into());
        s.usage = Some(crate::state::SlotUsage {
            slot_id: "s".into(),
            provider: "cli:opencode".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            context_tokens: 0,
            billed_tokens: 0,
            tools: 0,
            model: Some("x-ai/grok-4.5".into()),
        });
        assert_eq!(slot_model(&s, 12), "grok-4.5");
    }

    /// The band's token meter must agree with `status --json`, which reads the run's
    /// own ledger. `slot.usage` is overwritten on re-dispatch (executor.rs:1024).
    #[test]
    fn token_meter_reads_the_run_ledger_not_the_last_dispatch() {
        let mut st = run_with(Phase::Review, 2);
        let usage = |billed: u64| crate::state::SlotUsage {
            slot_id: "impl".into(),
            provider: "cli:claude".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            context_tokens: 0,
            billed_tokens: billed,
            tools: 0,
            model: None,
        };
        // Three dispatches of one slot: the ledger keeps all three, the slot field
        // only the last.
        st.usage = vec![usage(1000), usage(2000), usage(3000)];
        st.slots[0].usage = Some(usage(3000));
        let ledger: u64 = st.usage.iter().map(|u| u.billed_tokens).sum();
        let per_slot: u64 = st
            .slots
            .iter()
            .filter_map(|s| s.usage.as_ref())
            .map(|u| u.billed_tokens)
            .sum();
        assert_eq!(ledger, 6000);
        assert_eq!(per_slot, 3000, "fixture sanity");

        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                &[],
                &[],
                Some(&st),
                "",
                &[],
                "",
                &HomeData::default(),
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        let band: String = {
            let buf = term.backend().buffer();
            (0..120).map(|x| buf[(x, 1)].symbol()).collect()
        };
        assert!(band.contains("billed 6.0k"), "band was: {band:?}");
    }

    /// The help overlay used to hard-clip at a fixed 72 columns, cutting words like
    /// "approve" and "collapsed" mid-word. It must now size to its longest line (up
    /// to the frame) so every line renders whole.
    #[test]
    fn wrap_line_preserve_breaks_only_at_spaces() {
        let line = "Rail   projects ▸ runs ▸ agents  (Enter pushes, Esc pops)";
        let rows = wrap_line_preserve(line, 20);
        for w in &rows {
            assert!(w.chars().count() <= 20, "row too wide: {w:?}");
        }
        // Rejoining the wrapped rows on a space and re-splitting on whitespace must
        // reproduce the original word sequence: no word was cut mid-token.
        assert_eq!(
            rows.join(" ").split_whitespace().collect::<Vec<_>>(),
            line.split_whitespace().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn wrap_line_preserve_force_splits_a_token_longer_than_width() {
        let rows = wrap_line_preserve("supercalifragilisticexpialidocious", 10);
        assert!(rows.iter().all(|w| w.chars().count() <= 10), "{rows:?}");
        assert_eq!(rows.concat(), "supercalifragilisticexpialidocious");
    }

    #[test]
    fn wrap_line_preserve_handles_zero_width() {
        assert_eq!(wrap_line_preserve("abc", 0), vec!["abc".to_string()]);
    }

    /// A narrow enough width can put the break search's nearest space inside the
    /// line's own leading indentation rather than a word gap — that used to emit a
    /// whitespace-only row ahead of the real content, inflating the overlay's height
    /// with a blank line indentation alone accounted for.
    #[test]
    fn wrap_line_preserve_never_emits_a_whitespace_only_row() {
        let rows = wrap_line_preserve("    Rail   projects", 5);
        for row in &rows {
            assert!(
                !row.chars().all(|c| c == ' '),
                "blank row from indentation alone: {rows:?}"
            );
        }
        let rejoined: String = rows.concat();
        assert_eq!(
            rejoined.chars().filter(|c| *c != ' ').collect::<String>(),
            "Railprojects",
            "no non-space character was dropped along with the indentation: {rows:?}"
        );
    }

    /// AC-1's wrap path: the original lock only ran at a width wide enough that the
    /// longest `HELP_BODY` line never took the wrapping branch. Scan both the
    /// unscrolled top and the scrolled-to-max bottom so every wrapped row is checked.
    #[test]
    fn help_overlay_wraps_narrow_lines_without_cutting_a_word() {
        let st = run_with(Phase::Review, 3);
        let top = paint_with(50, 12, &[], &[], Some(&st), |a| a.show_help = true);
        let bottom = paint_with(50, 12, &[], &[], Some(&st), |a| {
            a.show_help = true;
            a.help_scroll = 9999;
        });
        let joined: String = (0..12)
            .map(|y| row(&top, y))
            .chain((0..12).map(|y| row(&bottom, y)))
            .collect::<Vec<_>>()
            .join(" ");
        for phrase in ["pushes,", "Esc pops)", "bands collapsed)."] {
            assert!(
                joined.contains(phrase),
                "word split by the wrap: {joined:?}"
            );
        }
    }

    #[test]
    fn help_overlay_never_hard_clips_a_line() {
        let st = run_with(Phase::Review, 3);
        let term = paint_with(100, 40, &[], &[], Some(&st), |a| a.show_help = true);
        let rows: Vec<String> = (0..40).map(|y| row(&term, y)).collect();
        let joined = rows.join("\n");
        assert!(
            joined.contains("reject · ship (when gated; approve = tap / :approve)"),
            "line was cut: {joined:?}"
        );
        assert!(
            joined.contains("Driving mode (green banner, bands collapsed)."),
            "line was cut: {joined:?}"
        );
    }

    /// On a terminal too short for the whole body, the overlay must scroll rather
    /// than silently hiding the tail — and the scroll offset must clamp instead of
    /// running past the last line.
    #[test]
    fn help_overlay_scrolls_and_clamps_on_a_short_terminal() {
        let st = run_with(Phase::Review, 3);
        let top = paint_with(100, 12, &[], &[], Some(&st), |a| a.show_help = true);
        let top_text = (0..12).map(|y| row(&top, y)).collect::<Vec<_>>().join("\n");
        assert!(
            top_text.contains("Shape"),
            "top of the body should be visible unscrolled: {top_text:?}"
        );
        assert!(
            !top_text.contains("Esc, ?, or tap to close help"),
            "the last line shouldn't fit an unscrolled 12-row overlay: {top_text:?}"
        );

        let scrolled = paint_with(100, 12, &[], &[], Some(&st), |a| {
            a.show_help = true;
            a.help_scroll = 9999; // clamps to the true max instead of panicking
        });
        let bottom_text = (0..12)
            .map(|y| row(&scrolled, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            bottom_text.contains("Esc, ?, or tap to close help"),
            "scrolling to the max should reach the last line: {bottom_text:?}"
        );
    }

    /// The palette used to under-count its own height by one row, clipping the hint
    /// line every time it opened. It also hard-capped the menu at 8 of the 12 verbs,
    /// so `spawn`/`chat`/`help`/`quit` could never be reached by browsing.
    #[test]
    fn palette_hint_is_never_clipped_and_every_verb_is_reachable() {
        let term = paint_with(120, 40, &[], &[], None, |a| {
            a.palette = Some(Palette::default());
        });
        let buf: String = (0..40)
            .map(|y| row(&term, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            buf.contains("Tab complete · ↑↓ pick · Enter run · Esc close"),
            "hint row was clipped: {buf:?}"
        );

        // `quit` is PALETTE_CMDS[11] — unreachable under the old hard cap of 8. The
        // footer also has a permanent "q quit" hint, so the assertion has to target
        // the menu's own selected-row marker or it would pass even with the window
        // never scrolled at all.
        let unscrolled = paint_with(120, 40, &[], &[], None, |a| {
            a.palette = Some(Palette {
                input: String::new(),
                sel: 0,
            });
        });
        let unscrolled_buf: String = (0..40)
            .map(|y| row(&unscrolled, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !unscrolled_buf.contains("▸ quit"),
            "quit should not be selected/visible at the top of the menu: {unscrolled_buf:?}"
        );

        let term = paint_with(120, 40, &[], &[], None, |a| {
            a.palette = Some(Palette {
                input: String::new(),
                sel: PALETTE_CMDS.len() - 1,
            });
        });
        let buf: String = (0..40)
            .map(|y| row(&term, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            buf.contains("▸ quit"),
            "scrolled menu should reach quit: {buf:?}"
        );
    }

    /// The hint row must survive a short frame too, not just a tall one: shrinking
    /// the completion menu is what has to give, not the hint at the frame's own edge.
    #[test]
    fn palette_hint_survives_a_short_frame() {
        for h in [10u16, 12, 14] {
            let term = paint_with(120, h, &[], &[], None, |a| {
                a.palette = Some(Palette::default());
            });
            let buf: String = (0..h).map(|y| row(&term, y)).collect::<Vec<_>>().join("\n");
            assert!(
                buf.contains("Tab complete · ↑↓ pick · Enter run · Esc close"),
                "hint row was clipped at height {h}: {buf:?}"
            );
        }
    }

    /// A scrollbar thumb implies there is more to see. It must not paint when the
    /// content already fits the viewport.
    #[test]
    fn scrollbar_only_paints_when_content_overflows() {
        let st = run_with(Phase::Review, 1);
        let has_scrollbar = |text: &str| {
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            app.open_main(MainTab::Diff);
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    Some(&st),
                    "",
                    &[],
                    text,
                    &HomeData::default(),
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();
            let inner = app.rect_main_inner;
            let x = inner.right().saturating_sub(1);
            let buf = term.backend().buffer();
            (inner.top()..inner.bottom()).any(|y| {
                let sym = buf[(x, y)].symbol();
                sym == "┃" || sym == "│"
            })
        };
        assert!(!has_scrollbar("one short line"), "no overflow, no thumb");
        let long: String = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(has_scrollbar(&long), "content overflows, thumb expected");
    }

    /// Without a run selected, Activity and Diff fall back to the same overview body
    /// Log uses (`draw_main`'s `full.is_none()` branch, which paints `stream_*` via
    /// `draw_log_body`). Scroll input has to follow that body — `stream_scroll` —
    /// instead of the run-scoped `bus_scroll`/`diff_scroll` those tabs normally own,
    /// or the rendered scrollbar silently stops responding to j/k/G once a run is
    /// deselected (a scrollbar promising an affordance that isn't wired).
    #[test]
    fn overview_tabs_scroll_the_overview_body_not_run_scoped_state() {
        let mut app = test_app();
        app.stream_max = 50;
        app.bus_max = 50;
        app.diff_max = 50;

        app.main_tab = MainTab::Activity;
        app.scroll_main_by(10, false);
        assert_eq!(
            app.stream_scroll, 10,
            "Activity's overview body must scroll stream_scroll"
        );
        assert_eq!(
            app.bus_scroll, 0,
            "Activity's overview body must not touch bus_scroll"
        );

        app.main_tab = MainTab::Diff;
        app.scroll_main_by(10, false);
        assert_eq!(
            app.stream_scroll, 20,
            "Diff's overview body must scroll stream_scroll"
        );
        assert_eq!(
            app.diff_scroll, 0,
            "Diff's overview body must not touch diff_scroll"
        );

        // Once a run is selected, Activity/Diff render their own bodies again and own
        // their own run-scoped scroll state.
        app.main_tab = MainTab::Activity;
        app.scroll_main_by(10, true);
        assert_eq!(
            app.bus_scroll, 10,
            "Activity with a run selected must scroll bus_scroll"
        );
        assert_eq!(
            app.stream_scroll, 20,
            "must not touch stream_scroll once a run is selected"
        );
    }

    /// The gap between adjacent Main tab labels must be the same everywhere — it used
    /// to jump from 4 to 8 columns around Activity's alert-badge slot, and the narrow
    /// strip had its own, differently uneven spacing.
    #[test]
    fn tab_strip_gaps_are_uniform() {
        let st = run_with(Phase::Review, 3);
        // Returns (gaps between labels, painted-start-x per label, recorded hit-rect
        // x per label) so the test can catch not just uneven gaps but a strip that
        // paints contiguous text while its click rects sit elsewhere (the round-2
        // regression: `[0, 0, 0]` gaps read as "uniform" even though nothing painted
        // agreed with where clicks landed).
        let labels = ["Log", "Activity", "Diff", "Shell"];
        // Returns (glyph-to-glyph gaps, cell-to-cell/rect gaps, painted-start-x per
        // label, recorded hit rects). Two different gap metrics because the two bands
        // use two different layouts: wide bakes a fixed-width badge slot into each
        // label's own padded text, so adjacent rects legitimately abut (rect gap 0)
        // and the badge never widens the visible run between labels (glyph gap
        // constant); narrow has no baked-in padding and a badge that really does grow
        // the cell, so its uniformity lives in the rects, not the glyphs.
        struct Probe {
            glyph_gaps: Vec<usize>,
            rect_gaps: Vec<usize>,
            starts: Vec<usize>,
            rects: Vec<Rect>,
        }
        let probe = |width: u16, human_alerts_n: usize| -> Probe {
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            app.human_alerts_n = human_alerts_n;
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 30,
            };
            let lay = layout_rects(area, Focus::Main, false, false);
            term.draw(|f| {
                draw_labels(
                    f,
                    &lay,
                    &swarm,
                    &[],
                    &[],
                    Some(&st),
                    &HomeData::default(),
                    &mut app,
                )
            })
            .unwrap();
            // Column positions, not byte offsets: the Activity badge's `⚠` is a
            // multi-byte char, so a `str::find`-based byte offset silently drifts out
            // of alignment with the terminal columns `app.main_tabs` records.
            let cols: Vec<char> = (0..width)
                .map(|x| {
                    term.backend().buffer()[(x, lay.labels.y)]
                        .symbol()
                        .chars()
                        .next()
                        .unwrap_or(' ')
                })
                .collect();
            let find_at = |label: &str, from: usize| -> usize {
                let needle: Vec<char> = label.chars().collect();
                (from..=cols.len().saturating_sub(needle.len()))
                    .find(|&i| cols[i..i + needle.len()] == needle[..])
                    .unwrap()
            };
            let mut cursor = 0usize;
            let mut starts = Vec::new();
            let mut ends = Vec::new();
            for label in labels {
                let start = find_at(label, cursor);
                starts.push(start);
                cursor = start + label.chars().count();
                ends.push(cursor);
            }
            let glyph_gaps = (0..labels.len() - 1)
                .map(|i| starts[i + 1] - ends[i])
                .collect();
            let rects: Vec<Rect> = app.main_tabs.iter().map(|(r, _)| *r).collect();
            let rect_gaps = (0..rects.len() - 1)
                .map(|i| (rects[i + 1].x - (rects[i].x + rects[i].width)) as usize)
                .collect();
            Probe {
                glyph_gaps,
                rect_gaps,
                starts,
                rects,
            }
        };
        // A recorded hit rect can legitimately be wider than the glyphs it labels (the
        // wide strip pads each cell for a bigger touch target) but it must still cover
        // the text it claims to hit — the round-2 regression left the narrow strip's
        // rects pointing at blank columns entirely disjoint from the painted labels.
        let assert_rects_cover_labels = |band: &str, starts: &[usize], rects: &[Rect], n: usize| {
            for (i, (&start, rect)) in starts.iter().zip(rects).enumerate() {
                let label_end = start + labels[i].len();
                assert!(
                    (rect.x as usize) <= start && label_end <= (rect.x + rect.width) as usize,
                    "{band} rect for {:?} (x={}, w={}) does not cover painted label at {start} (human_alerts_n={n})",
                    labels[i],
                    rect.x,
                    rect.width
                );
            }
        };
        for &n in &[0usize, 3, 12] {
            let wide = probe(120, n);
            assert!(
                wide.glyph_gaps.windows(2).all(|w| w[0] == w[1]) && wide.glyph_gaps[0] > 0,
                "wide tab gaps not uniform (human_alerts_n={n}): {:?}",
                wide.glyph_gaps
            );
            assert_rects_cover_labels("wide", &wide.starts, &wide.rects, n);

            let narrow = probe(79, n);
            // Narrow has no baked-in padding to reserve for a bigger touch target, so
            // its hit rects are instead padded out to split each glyph gap with the
            // neighbor on either side — the strip tiles edge to edge with zero dead
            // columns between rects, rather than a uniform *nonzero* rect gap.
            assert!(
                narrow.rect_gaps.iter().all(|&g| g == 0),
                "narrow tab strip has dead columns between rects (human_alerts_n={n}): {:?}",
                narrow.rect_gaps
            );
            // The visible glyph gaps must be uniform too, alert badge or not — it used
            // to grow only the Activity-Diff gap (18, 22, 18) whenever an alert badge
            // was glued onto Activity alone with no matching reservation on its
            // neighbors.
            assert!(
                narrow.glyph_gaps.windows(2).all(|w| w[0] == w[1]) && narrow.glyph_gaps[0] > 0,
                "narrow tab gaps not uniform (human_alerts_n={n}): {:?}",
                narrow.glyph_gaps
            );
            assert_rects_cover_labels("narrow", &narrow.starts, &narrow.rects, n);
        }
    }

    /// `draw_rule`'s active-tab underline used to read `app.main_tabs`, the touch-target
    /// hit rect — in the narrow band that rect is padded out to split each neighboring
    /// gap for a bigger tap zone, so the accent underline ballooned to 14-27 columns and
    /// sat detached from the 3-8 column label it was meant to mark. The underline must
    /// track the painted glyph span (`main_tab_glyphs`) instead, at both bands.
    #[test]
    fn active_tab_underline_matches_the_painted_label_not_the_touch_target() {
        let st = run_with(Phase::Review, 3);
        for width in [35u16, 60, 79, 120] {
            for &n in &[0usize, 3] {
                let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
                let swarm = SparPaths::new("/x");
                let mut app = test_app();
                app.human_alerts_n = n;
                app.main_tab = MainTab::Log;
                let area = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                };
                let lay = layout_rects(area, Focus::Main, false, false);
                term.draw(|f| {
                    draw_labels(
                        f,
                        &lay,
                        &swarm,
                        &[],
                        &[],
                        Some(&st),
                        &HomeData::default(),
                        &mut app,
                    );
                    draw_rule(f, &lay, &app);
                })
                .unwrap();

                let (glyph_rect, _) = app
                    .main_tab_glyphs
                    .iter()
                    .find(|(_, t)| *t == MainTab::Log)
                    .unwrap();
                let underline_w = (0..width)
                    .filter(|&x| term.backend().buffer()[(x, lay.rule.y)].symbol() == TAB_MARK)
                    .count() as u16;
                assert_eq!(
                    underline_w, glyph_rect.width,
                    "width {width} human_alerts_n={n}: underline is {underline_w} cols wide, \
                     label glyph span is {} cols",
                    glyph_rect.width
                );
            }
        }
    }

    /// The uniform-gap fix for the narrow strip used to buy its spacing by silently
    /// dropping Shell (and, at the narrowest widths, Diff too) once the wide strip's
    /// fixed per-tab padding stopped fitting — invisible and untappable, with no
    /// ellipsis to say a tab existed. Every width in the narrow band must keep all
    /// four — with an alert badge in play too: below ~36 columns the badge-reservation
    /// fallback glues the badge onto Activity alone (trading gap uniformity, covered by
    /// `tab_strip_gaps_are_uniform`'s 79-column probe, for keeping every tab on
    /// screen), and that fallback path was only ever swept with zero alerts.
    #[test]
    fn narrow_tab_strip_never_drops_a_tab() {
        let st = run_with(Phase::Review, 3);
        let labels = ["Log", "Activity", "Diff", "Shell"];
        for width in 24..80u16 {
            for &alerts in &[0usize, 3] {
                let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
                let swarm = SparPaths::new("/x");
                let mut app = test_app();
                app.human_alerts_n = alerts;
                let area = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                };
                let lay = layout_rects(area, Focus::Main, false, false);
                term.draw(|f| {
                    draw_labels(
                        f,
                        &lay,
                        &swarm,
                        &[],
                        &[],
                        Some(&st),
                        &HomeData::default(),
                        &mut app,
                    )
                })
                .unwrap();
                assert_eq!(
                    app.main_tabs.len(),
                    4,
                    "width {width} (alerts={alerts}) dropped a tab: {:?}",
                    app.main_tabs
                );
                // A recorded rect is not enough on its own: it must also point at a
                // painted glyph, not a blank column the label never reached (the
                // round-2 regression, where rects and paint disagreed).
                let row: String = {
                    let buf = term.backend().buffer();
                    (0..width)
                        .map(|x| buf[(x, lay.labels.y)].symbol())
                        .collect()
                };
                let row_chars: Vec<char> = row.chars().collect();
                for (i, (rect, tab)) in app.main_tabs.iter().enumerate() {
                    let expected = labels[i];
                    let window: String = row_chars
                        .iter()
                        .skip(rect.x as usize)
                        .take(rect.width as usize)
                        .collect();
                    assert!(
                        window.contains(expected),
                        "width {width} (alerts={alerts}): rect for {tab:?} (x={}, w={}) does not \
                         cover painted label {expected:?} (row: {row:?})",
                        rect.x,
                        rect.width
                    );
                }
            }
        }
    }

    /// The wide strip's per-tab badge slot (AC-4) grew the strip from 40 to 52
    /// columns, leaving only a one-column margin at width 80 — the narrowest the
    /// wide band ever renders at (`NARROW_WIDTH`). Nothing caught a future badge or
    /// label change eating that margin, so lock it directly.
    #[test]
    fn wide_tab_strip_never_drops_a_tab_at_the_tightest_widths() {
        let st = run_with(Phase::Review, 3);
        for width in [80u16, 81] {
            for &alerts in &[0usize, 3, 12] {
                let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
                let swarm = SparPaths::new("/x");
                let mut app = test_app();
                app.human_alerts_n = alerts;
                let area = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                };
                let lay = layout_rects(area, Focus::Main, false, false);
                assert!(
                    !lay.narrow,
                    "width {width} unexpectedly took the narrow band"
                );
                term.draw(|f| {
                    draw_labels(
                        f,
                        &lay,
                        &swarm,
                        &[],
                        &[],
                        Some(&st),
                        &HomeData::default(),
                        &mut app,
                    )
                })
                .unwrap();
                assert_eq!(
                    app.main_tabs.len(),
                    4,
                    "width {width} (alerts={alerts}) dropped a tab: {:?}",
                    app.main_tabs
                );
            }
        }
    }

    /// With no runs at all, the header, rail and Main must agree on a single story —
    /// not a header offering a run breadcrumb ("run —") next to "no runs", nor a Main
    /// pane still painting stale log content or a scrollbar behind it.
    #[test]
    fn empty_state_is_coherent_with_no_stale_chrome() {
        // Prove the "no stale content" guarantee against a real stale scenario rather
        // than one the test builds for itself: load a run's log that genuinely
        // contains a stream line, then drop to no-run on the same cache and confirm
        // it does not survive the transition.
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("slot.log");
        std::fs::write(&log_path, "→ Bash  read the contract\n← ✓ ok\n").unwrap();
        let mut st = run_with(Phase::Review, 1);
        st.slots[0].log_path = Some(log_path);

        let mut cache = LogCache::empty();
        let live = stream_content(&SparPaths::new("/x"), Some(&st), 0, &mut cache, true);
        assert!(
            live.contains("Bash"),
            "fixture should carry real stream content: {live:?}"
        );

        let text = stream_content(&SparPaths::new("/x"), None, 0, &mut cache, false);
        assert!(
            !text.contains("Bash"),
            "stale log content survived the drop to no-run: {text:?}"
        );
        assert!(
            !text.to_lowercase().contains("select a run"),
            "nothing to select with zero runs: {text:?}"
        );

        // Swept across widths, not just 120: a round-4 review finding was that the
        // "identical wording" invariant below only held at 120 columns — the header's
        // cue is long enough (`describe the change`) that the gate zone at 90 columns
        // used to truncate it with an ellipsis while Main showed the same command in
        // full, i.e. two different renderings of the same CTA on screen at once. The
        // header must now omit a cue it cannot show whole rather than truncate it
        // (same "omit rather than contradict" rule the run breadcrumb already follows
        // a few lines up in `draw_header`). Below 90, Main's own pane column is
        // narrow enough that its body starts trimming the long CTA line on its own
        // (the pre-existing, unrelated "trim" log mode) — nothing to compare the
        // header against there, so that band is excluded rather than asserting
        // Main's line wrapping never trims, which is out of this fix's scope.
        for width in [90u16, 120] {
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    None,
                    &text,
                    &[],
                    "",
                    &HomeData::default(),
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();

            let header = row(&term, 0);
            assert!(
                !header.contains("run —"),
                "width {width}: incoherent breadcrumb: {header:?}"
            );

            let whole: String = {
                let buf = term.backend().buffer();
                (0..30)
                    .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert!(
                whole.contains("(no runs)"),
                "width {width}: rail: {whole:?}"
            );
            assert!(
                !whole.contains("Bash"),
                "width {width}: stale log content: {whole:?}"
            );

            // One coherent call to action. Header and Main both surface the same plan
            // command (reinforcement, not incoherence) — but they used to phrase it
            // two different ways (`"…"` vs `"describe the change"`, a round-2
            // regression), and the context band offered a second, contradictory one:
            // the palette's `plan` command needs an existing run to reuse a fleet
            // from, so "press :" cannot bootstrap the very first run.
            let cta = "spar plan -t \"describe the change\" --providers cli:claude";
            assert_eq!(
                whole.matches(cta).count(),
                whole.matches("spar plan -t").count(),
                "width {width}: every occurrence of the plan CTA must use identical wording: {whole:?}"
            );
            assert!(
                !whole.contains("press :"),
                "width {width}: the command palette cannot start a first run with zero runs to reuse a fleet from: {whole:?}"
            );

            let inner = app.rect_main_inner;
            let x = inner.right().saturating_sub(1);
            let buf = term.backend().buffer();
            let scrollbar = (inner.top()..inner.bottom()).any(|y| {
                let sym = buf[(x, y)].symbol();
                sym == "┃" || sym == "│"
            });
            assert!(
                !scrollbar,
                "width {width}: no content to scroll in the empty state"
            );
        }
    }

    /// The same coherent empty-state text (not a tab-specific message, and not stale
    /// chrome) must show on Log, Activity and Diff when there are no runs at all —
    /// they used to each tell their own, different story. Shell is the one deliberate
    /// exception: it is project-scoped, not run-scoped (`manage_terminal`'s doc
    /// comment), so it keeps showing its own real workspace-shell body regardless of
    /// run count — that is a live surface, not stale content, and its caption must
    /// agree with what it shows rather than claim "no runs" over a working shell.
    #[test]
    fn empty_state_is_uniform_across_every_main_tab() {
        let text = stream_content(
            &SparPaths::new("/x"),
            None,
            0,
            &mut LogCache::empty(),
            false,
        );
        for tab in [MainTab::Log, MainTab::Activity, MainTab::Diff] {
            let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            app.open_main(tab);
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    None,
                    &text,
                    &[],
                    "",
                    &HomeData::default(),
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();
            let whole: String = {
                let buf = term.backend().buffer();
                (0..30)
                    .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert!(
                whole.contains("No runs yet"),
                "{tab:?} did not show the unified empty state: {whole:?}"
            );
            assert!(
                !whole.contains("No run selected"),
                "{tab:?} fell back to its own stale message instead of the unified one: {whole:?}"
            );

            let inner = app.rect_main_inner;
            let x = inner.right().saturating_sub(1);
            let buf = term.backend().buffer();
            let scrollbar = (inner.top()..inner.bottom()).any(|y| {
                let sym = buf[(x, y)].symbol();
                sym == "┃" || sym == "│"
            });
            assert!(
                !scrollbar,
                "{tab:?}: no content to scroll in the empty state"
            );
        }

        // Shell: real workspace-shell hint body, unconditionally, with a caption that
        // agrees with it — never the unified "no runs" message behind it.
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        app.open_main(MainTab::Shell);
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                &[],
                &[],
                None,
                &text,
                &[],
                "",
                &HomeData::default(),
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        let whole: String = {
            let buf = term.backend().buffer();
            (0..30)
                .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            whole.contains("Opening a real tmux client"),
            "Shell must keep showing its own workspace-shell body with zero runs: {whole:?}"
        );
        assert!(
            !whole.contains("No runs yet"),
            "Shell must not show the Log/Activity/Diff empty state behind its own body: {whole:?}"
        );
        assert!(
            whole.contains("shell ·"),
            "Shell's caption must agree with its body, not the unified empty state: {whole:?}"
        );
    }

    /// With zero runs, default focus (`Focus::Rail`) left Main's rect zero-width in the
    /// narrow band (`layout_rects`), so the coherent empty-state CTA above never
    /// painted — the phone screen showed only the rail's bare `(no runs)` row, or
    /// nothing at all once the context band folded too. The no-run case must land on
    /// Main just like the "active run" narrow autofocus already does, so the CTA is
    /// the one thing on screen rather than unreachable behind a dead rail.
    #[test]
    fn empty_state_is_reachable_at_narrow_width() {
        let text = stream_content(
            &SparPaths::new("/x"),
            None,
            0,
            &mut LogCache::empty(),
            false,
        );
        for width in [50u16, 79] {
            let mut term = Terminal::new(TestBackend::new(width, 20)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    None,
                    &text,
                    &[],
                    "",
                    &HomeData::default(),
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();

            assert_eq!(
                app.focus,
                Focus::Main,
                "width {width}: zero runs must autofocus Main so the empty state is reachable"
            );

            let whole: String = {
                let buf = term.backend().buffer();
                (0..20)
                    .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            assert!(
                whole.contains("No runs yet"),
                "width {width}: empty-state CTA unreachable: {whole:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Feature 004 Phase C — Home landing view.
    //
    // Every test below paints at `BrowseLevel::Home` with `HomeData` supplied
    // by hand: that *is* the U13 assertion. `draw` gets its rows and its
    // per-project counts from the snapshot the refresher built off-thread, so
    // these fixtures point at project roots that do not exist on disk and the
    // paint still has to be correct.
    // ---------------------------------------------------------------------

    fn home_project(name: &str) -> registry::ProjectEntry {
        registry::ProjectEntry {
            root: PathBuf::from("/nonexistent").join(name),
            name: Some(name.to_string()),
            last_seen: Utc::now(),
            last_run_id: None,
        }
    }

    fn home_run(id: &str, phase: Phase, mins_ago: i64, project: &str) -> state::RunSummary {
        state::RunSummary {
            id: id.into(),
            workflow: WorkflowKind::Loop,
            archived: false,
            phase,
            updated_at: Utc::now() - chrono::Duration::minutes(mins_ago),
            task: Some(format!("brief for {id}")),
            dry_run: false,
            abandoned: false,
            parent_run: None,
            round: 1,
            legs: 1,
            wants: 0,
            base_ref: None,
            base_commit: None,
            project_root: Some(PathBuf::from("/nonexistent").join(project)),
            project_name: Some(project.to_string()),
        }
    }

    fn home_row(band: HomeBand, run: state::RunSummary, waited_mins: u64) -> HomeRow {
        HomeRow::Run {
            band,
            run,
            waited: Duration::from_secs(waited_mins * 60),
        }
    }

    /// A Home fixture with one row in each of the first three bands.
    fn home_data(projects: &[registry::ProjectEntry]) -> HomeData {
        HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                home_row(
                    HomeBand::NeedsMe,
                    home_run("gate0001", Phase::AwaitingShipConfirm, 90, "acme-api"),
                    90,
                ),
                HomeRow::Header(HomeBand::Running),
                home_row(
                    HomeBand::Running,
                    home_run("work0001", Phase::Review, 4, "spar"),
                    4,
                ),
                HomeRow::Header(HomeBand::Finished),
                home_row(
                    HomeBand::Finished,
                    home_run("done0001", Phase::Done, 20, "spar"),
                    20,
                ),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: projects
                .iter()
                .map(|_| ProjectStat {
                    n_runs: 3,
                    needs_you: 1,
                })
                .collect(),
        }
    }

    fn paint_home(
        w: u16,
        h: u16,
        projects: &[registry::ProjectEntry],
        home: &HomeData,
        tweak: impl Fn(&mut App),
    ) -> Terminal<TestBackend> {
        paint_home_app(w, h, projects, home, tweak).0
    }

    fn paint_home_app(
        w: u16,
        h: u16,
        projects: &[registry::ProjectEntry],
        home: &HomeData,
        tweak: impl Fn(&mut App),
    ) -> (Terminal<TestBackend>, App) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let swarm = SparPaths::new("/x");
        let mut app = App::new(None, Config::default(), None);
        assert_eq!(app.browse, BrowseLevel::Home, "App::new must land on Home");
        tweak(&mut app);
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                projects,
                &[],
                None,
                "",
                &[],
                "",
                home,
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        (term, app)
    }

    fn whole(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// AC-1. The same swept grid the pre-Home levels get, at the new landing
    /// view, with the two self-sizing overlays (help, and Phase D's new-run
    /// surface) opened at the cadence that caught the 30-column help panic.
    #[test]
    fn renders_home_at_every_size_without_panicking() {
        let projects = [home_project("acme-api"), home_project("spar")];
        let home = home_data(&projects);
        for w in (1..=200).step_by(3) {
            for h in (1..=60).step_by(2) {
                paint_home(w, h, &projects, &home, |_| {});
                if w % 9 == 1 {
                    paint_home(w, h, &projects, &home, |a| a.show_help = true);
                    paint_home(w, h, &projects, &home, |a| {
                        a.palette = Some(Palette::default())
                    });
                    paint_home(w, h, &projects, &home, |a| {
                        a.new_run = Some(new_run_fixture());
                    });
                }
            }
        }
        for (w, h) in [
            (1, 1),
            (20, 5),
            (79, 24),
            (80, 24),
            (89, 24),
            (90, 24),
            (119, 40),
            (120, 40),
            (200, 60),
        ] {
            paint_home(w, h, &projects, &home, |_| {});
            paint_home(w, h, &[], &HomeData::default(), |_| {});
        }
    }

    /// AC-2. Nothing registered, nothing run, no watermark: Home is still a
    /// coherent screen. All four band headers, each band's own empty line, and
    /// the `n` call to action reachable — including on the phone-width band
    /// where the rail and Main do not coexist.
    #[test]
    fn home_renders_with_an_empty_everything() {
        let empty = HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                HomeRow::Header(HomeBand::Running),
                HomeRow::Header(HomeBand::Finished),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: Vec::new(),
        };
        for (w, h) in [(120u16, 30u16), (90, 24), (79, 20), (50, 20), (20, 5)] {
            let term = paint_home(w, h, &[], &empty, |_| {});
            let text = whole(&term).to_lowercase();
            if w >= 50 {
                assert!(
                    text.contains("needs you") || text.contains("needs me"),
                    "{w}x{h}: band 1 header missing: {text:?}"
                );
                assert!(
                    text.contains("new run") || text.contains("start something new"),
                    "{w}x{h}: the `n` CTA must be reachable on an empty Home: {text:?}"
                );
            }
            assert!(
                !text.contains("no run selected"),
                "{w}x{h}: stale pre-Home empty state: {text:?}"
            );
        }
    }

    /// AC-3. Scale: hundreds of folded units across several projects. The
    /// paint completes, `NeedsMe` is never truncated, and a capped band says
    /// so rather than silently dropping rows.
    #[test]
    fn home_renders_a_large_run_count() {
        let projects = [
            home_project("acme-api"),
            home_project("spar"),
            home_project("biddesk"),
        ];
        let mut rows = vec![HomeRow::Header(HomeBand::NeedsMe)];
        for i in 0..(HOME_BAND_CAP + 12) {
            rows.push(home_row(
                HomeBand::NeedsMe,
                home_run(
                    &format!("gate{i:04}"),
                    Phase::AwaitingPlanApproval,
                    i as i64,
                    "acme-api",
                ),
                i as u64,
            ));
        }
        rows.push(HomeRow::Header(HomeBand::Running));
        for i in 0..HOME_BAND_CAP {
            rows.push(home_row(
                HomeBand::Running,
                home_run(&format!("work{i:04}"), Phase::Review, i as i64, "spar"),
                i as u64,
            ));
        }
        rows.push(HomeRow::More {
            band: HomeBand::Running,
            n: 300,
        });
        rows.push(HomeRow::Header(HomeBand::Finished));
        rows.push(HomeRow::Header(HomeBand::StartNew));
        rows.push(HomeRow::NewRun);

        let needs_me = rows
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    HomeRow::Run {
                        band: HomeBand::NeedsMe,
                        ..
                    }
                )
            })
            .count();
        assert!(
            needs_me > HOME_BAND_CAP,
            "the NeedsMe band must not be capped: {needs_me} <= {HOME_BAND_CAP}"
        );
        let home = HomeData {
            rows,
            project_stats: projects
                .iter()
                .map(|_| ProjectStat {
                    n_runs: 400,
                    needs_you: 61,
                })
                .collect(),
        };
        let term = paint_home(120, 40, &projects, &home, |_| {});
        let text = whole(&term);
        assert!(text.contains("more"), "a capped band must say so: {text:?}");
    }

    /// AC-4. Layout stability: the four band headers are present, in band
    /// order, whether or not their band has rows — so band 4 does not slide up
    /// under the cursor when band 1 empties.
    #[test]
    fn home_band_headers_hold_their_order_and_never_disappear() {
        let full = HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                home_row(
                    HomeBand::NeedsMe,
                    home_run("gate0001", Phase::AwaitingShipConfirm, 90, "spar"),
                    90,
                ),
                HomeRow::Header(HomeBand::Running),
                home_row(
                    HomeBand::Running,
                    home_run("work0001", Phase::Review, 2, "spar"),
                    2,
                ),
                HomeRow::Header(HomeBand::Finished),
                home_row(
                    HomeBand::Finished,
                    home_run("done0001", Phase::Done, 8, "spar"),
                    8,
                ),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: Vec::new(),
        };
        let drained = HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                HomeRow::Header(HomeBand::Running),
                HomeRow::Header(HomeBand::Finished),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: Vec::new(),
        };
        // Main's body carries the four headers; assert on it directly so the
        // ordering claim does not depend on the rail's viewport height.
        for home in [&full, &drained] {
            let body = home_overview(&home.rows, &HomeScope::All, Utc::now()).to_lowercase();
            let idx = |needle: &str| {
                body.find(needle)
                    .unwrap_or_else(|| panic!("band header {needle:?} missing from: {body:?}"))
            };
            let a = idx("needs");
            let b = idx("running");
            let c = idx("finished");
            let d = idx("start something new");
            assert!(a < b && b < c && c < d, "bands out of order: {body:?}");
        }
        // Each empty band still says what is empty, on its own line.
        let body = home_overview(&drained.rows, &HomeScope::All, Utc::now()).to_lowercase();
        for phrase in [
            "nothing needs you",
            "nothing running",
            "nothing finished since your last look",
        ] {
            assert!(body.contains(phrase), "missing {phrase:?} in: {body:?}");
        }
    }

    /// AC-5. The rail's right-hand wait/age column does not move when the run
    /// id next to it changes length.
    #[test]
    fn home_wait_column_does_not_move_with_row_content() {
        let ts = Utc::now() - chrono::Duration::minutes(7);
        let mut short = home_run("bb22", Phase::AwaitingPlanApproval, 0, "spar");
        short.updated_at = ts;
        short.task = Some("s".into());
        let mut long = home_run("aaaa1111", Phase::AwaitingPlanApproval, 0, "spar");
        long.updated_at = ts;
        long.task = Some("a considerably longer brief for this unit of work".into());
        let home = HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                home_row(HomeBand::NeedsMe, short, 7 * 60),
                home_row(HomeBand::NeedsMe, long, 7 * 60),
                HomeRow::Header(HomeBand::Running),
                HomeRow::Header(HomeBand::Finished),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: Vec::new(),
        };
        let (term, app) = paint_home_app(120, 30, &[], &home, |_| {});
        let rail = app.rect_rail;
        assert!(rail.width > 0, "the rail must be visible at 120 columns");
        let buf = term.backend().buffer();
        let last_glyph_x = |y: u16| -> Option<u16> {
            (rail.x..rail.right())
                .rev()
                .find(|&x| buf[(x, y)].symbol().trim() != "")
        };
        let ends: Vec<u16> = (rail.y..rail.bottom())
            .filter_map(|y| {
                let line: String = (rail.x..rail.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect();
                if line.contains("bb22") || line.contains("aaaa1111") {
                    last_glyph_x(y)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ends.len(), 2, "both run rows must paint in the rail");
        assert_eq!(
            ends[0], ends[1],
            "the wait column moved with the row's content"
        );
    }

    /// AC-6. The `start something new` action row is always present and is the
    /// first row of the last band, so `n` has a visible home no matter what
    /// the other three bands hold.
    #[test]
    fn home_start_something_new_is_always_present() {
        for home in [
            home_data(&[]),
            HomeData {
                rows: vec![
                    HomeRow::Header(HomeBand::NeedsMe),
                    HomeRow::Header(HomeBand::Running),
                    HomeRow::Header(HomeBand::Finished),
                    HomeRow::Header(HomeBand::StartNew),
                    HomeRow::NewRun,
                ],
                project_stats: Vec::new(),
            },
        ] {
            let i = home
                .rows
                .iter()
                .position(|r| matches!(r, HomeRow::Header(HomeBand::StartNew)))
                .expect("band 4 header");
            assert!(
                matches!(home.rows.get(i + 1), Some(HomeRow::NewRun)),
                "the new-run action row must follow band 4's header: {:?}",
                &home.rows[i..]
            );
            assert!(
                home.rows[i + 1..]
                    .iter()
                    .all(|r| matches!(r, HomeRow::NewRun | HomeRow::Project(_))),
                "band 4 holds only the action row and the project list"
            );
        }
    }

    /// AC-7. U14's reserved chrome zones still hold at the new landing view:
    /// painting the same run at Home and at the Runs level must not move the
    /// gate-button zone or the Main tab strip.
    #[test]
    fn home_does_not_move_the_reserved_chrome_zones() {
        let st = run_with(Phase::AwaitingShipConfirm, 7);
        let probe = |level: BrowseLevel| -> (Vec<Rect>, Vec<Rect>) {
            let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = App::new(None, Config::default(), None);
            app.browse = level;
            let mut rail = ListState::default();
            let home = home_data(&[]);
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    Some(&st),
                    "",
                    &[],
                    "",
                    &home,
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();
            (
                app.gate_buttons.iter().map(|(r, _)| *r).collect(),
                app.main_tabs.iter().map(|(r, _)| *r).collect(),
            )
        };
        let (home_gates, home_tabs) = probe(BrowseLevel::Home);
        let (runs_gates, runs_tabs) = probe(BrowseLevel::Runs);
        assert_eq!(home_tabs, runs_tabs, "the tab strip moved at Home");
        assert_eq!(home_gates, runs_gates, "the gate zone moved at Home");
    }

    /// AC-8. R9: at phone width the rail and Main do not coexist. Home must
    /// autofocus Main the way the zero-run Runs level already does, or the
    /// operator is stranded on a rail with no call to action.
    #[test]
    fn home_is_reachable_at_narrow_width() {
        let empty = HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                HomeRow::Header(HomeBand::Running),
                HomeRow::Header(HomeBand::Finished),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: Vec::new(),
        };
        for width in [50u16, 79] {
            let mut term = Terminal::new(TestBackend::new(width, 20)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = App::new(None, Config::default(), None);
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    None,
                    "",
                    &[],
                    "",
                    &empty,
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();
            assert_eq!(
                app.focus,
                Focus::Main,
                "width {width}: an empty Home must land on Main so its CTA is reachable"
            );
            let text = whole(&term).to_lowercase();
            assert!(
                text.contains("start something new") || text.contains("new run"),
                "width {width}: no CTA on screen: {text:?}"
            );
        }
    }

    /// AC-9. U13, the rendering half: the Projects level's per-project run and
    /// attention counts come off the snapshot. Every root here is a path that
    /// does not exist, so a `draw` that still scanned would paint zeroes.
    #[test]
    fn projects_level_counts_come_from_the_snapshot_not_the_disk() {
        let projects = [home_project("acme-api"), home_project("spar")];
        let home = HomeData {
            rows: Vec::new(),
            project_stats: vec![
                ProjectStat {
                    n_runs: 17,
                    needs_you: 3,
                },
                ProjectStat {
                    n_runs: 4,
                    needs_you: 0,
                },
            ],
        };
        let term = paint_home(120, 30, &projects, &home, |a| a.open_projects_view());
        let text = whole(&term);
        assert!(
            text.contains("17"),
            "supplied run count not painted: {text:?}"
        );
        assert!(
            text.contains("⚑3"),
            "supplied attention roll-up not painted: {text:?}"
        );

        // A stat slice shorter than the project list is one snapshot of lag
        // after a project registers. It must degrade, never index-panic.
        let short = HomeData {
            rows: Vec::new(),
            project_stats: vec![ProjectStat {
                n_runs: 1,
                needs_you: 0,
            }],
        };
        paint_home(120, 30, &projects, &short, |a| a.open_projects_view());
        paint_home(120, 30, &projects, &HomeData::default(), |a| {
            a.open_projects_view()
        });
    }

    /// AC-10. Phase A: the tmux session name is an implementation detail of
    /// the tmux backend and must never reach the screen, in the Shell tab's
    /// caption or in its hint body.
    #[test]
    fn the_shell_tab_never_prints_a_tmux_session_name() {
        let st = run_with(Phase::Review, 7);
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        app.open_main(MainTab::Shell);
        app.takeover_target = Some(tmux::session_name(&st.id));
        let caption = main_context(&swarm, Some(&st), &app);
        assert!(
            caption.contains(&st.id[..8]),
            "the caption must name the run: {caption:?}"
        );
        assert!(
            !caption.contains("spar-"),
            "the tmux session name leaked into the caption: {caption:?}"
        );
        assert!(
            !caption.to_lowercase().contains("session"),
            "retired noun in the caption: {caption:?}"
        );

        // The Shell body's own hint text is the other place the noun lived.
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let mut app = test_app();
        app.open_main(MainTab::Shell);
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                &[],
                &[],
                None,
                "",
                &[],
                "",
                &HomeData::default(),
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        let text = whole(&term).to_lowercase();
        assert!(
            !text.contains("session"),
            "retired noun on screen in the Shell tab: {text:?}"
        );
    }
}

/// One row per unit of work (U15).
#[cfg(test)]
mod folding {
    use super::*;
    use crate::cli::WorkflowKind;

    fn summary(id: &str, phase: Phase, parent: Option<&str>, mins_ago: i64) -> state::RunSummary {
        state::RunSummary {
            id: id.into(),
            workflow: WorkflowKind::Loop,
            archived: false,
            phase,
            updated_at: Utc::now() - chrono::Duration::minutes(mins_ago),
            task: Some(format!("brief for {id}")),
            dry_run: false,
            abandoned: false,
            parent_run: parent.map(str::to_string),
            round: 1,
            legs: 1,
            wants: 0,
            base_ref: None,
            base_commit: None,
            project_root: None,
            project_name: None,
        }
    }

    #[test]
    fn a_leg_folds_into_its_parents_row() {
        let runs = vec![
            summary("plan1", Phase::PlanApproved, None, 90),
            summary("impl1", Phase::AwaitingShipConfirm, Some("plan1"), 5),
            summary("other", Phase::Review, None, 20),
        ];
        let (rows, units) = fold_units(runs);
        assert_eq!(rows.len(), 2, "two units of work, three runs");
        let unit = rows.iter().find(|r| r.legs > 1).expect("a folded row");
        // The row acts on the leg that holds the state, so a gate button hits the run
        // that actually has the gate.
        assert_eq!(unit.id, "impl1");
        assert_eq!(unit.phase, Phase::AwaitingShipConfirm);
        // But it is titled by the work, not by the leg.
        assert_eq!(unit.task.as_deref(), Some("brief for plan1"));
        assert_eq!(unit.legs, 2);
        let mut members = units.get("impl1").cloned().unwrap_or_default();
        members.sort();
        assert_eq!(members, vec!["impl1".to_string(), "plan1".to_string()]);
    }

    /// Folding must never hide a run that wants the operator — the failure O36 exists
    /// to prevent, re-appearing one layer up.
    #[test]
    fn the_group_takes_its_loudest_attention() {
        let runs = vec![
            summary("root", Phase::Done, None, 5),
            summary("leg", Phase::AwaitingPlanApproval, Some("root"), 90),
        ];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 1);
        assert_eq!(run_attention(&rows[0]), Attention::Gate);
        assert_eq!(rows[0].id, "leg", "the gate is what the row acts on");
        assert_eq!(runs_needing_attention(&rows), 1);
    }

    /// Folding must never turn two gates into one. The roll-up counts legs, not rows.
    #[test]
    fn two_gates_in_one_unit_still_count_twice() {
        let runs = vec![
            summary("root", Phase::AwaitingShipConfirm, None, 30),
            summary("leg", Phase::AwaitingPlanApproval, Some("root"), 10),
            summary("elsewhere", Phase::Done, None, 5),
        ];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 2, "one unit plus one unrelated run");
        let unit = rows.iter().find(|r| r.legs > 1).unwrap();
        assert_eq!(unit.wants, 2);
        assert_eq!(
            runs_needing_attention(&rows),
            2,
            "both gates are still counted"
        );
    }

    #[test]
    fn an_orphan_leg_stands_on_its_own() {
        // The parent is archived or purged, so it is not in the listing.
        let runs = vec![summary("leg", Phase::Review, Some("gone"), 5)];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 1, "the leg must not vanish with its parent");
        assert_eq!(rows[0].id, "leg");
        assert_eq!(rows[0].legs, 1);
    }

    #[test]
    fn chains_flatten_and_cycles_terminate() {
        let runs = vec![
            summary("a", Phase::Done, None, 30),
            summary("b", Phase::Done, Some("a"), 20),
            summary("c", Phase::Review, Some("b"), 10),
        ];
        let (rows, units) = fold_units(runs);
        assert_eq!(rows.len(), 1, "a → b → c is one unit of work");
        assert_eq!(units.get(&rows[0].id).map(|m| m.len()), Some(3));

        // A cycle on disk (hand-edited) must not hang the render thread.
        let cyclic = vec![
            summary("x", Phase::Review, Some("y"), 10),
            summary("y", Phase::Review, Some("x"), 10),
        ];
        let (rows, _) = fold_units(cyclic);
        assert!(!rows.is_empty());
    }

    /// AC-34. Home ranks the same units the rail folds, so the two roll-ups
    /// must agree: folding across a whole registry must never turn N gates into
    /// fewer than N. Legs are folded **per project** — two projects can hold
    /// two different runs whose 8-hex ids collide, and folding them together
    /// would merge unrelated work.
    #[test]
    fn home_folds_per_project_and_keeps_every_gate() {
        let unfolded = vec![
            summary("root", Phase::AwaitingShipConfirm, None, 30),
            summary("leg", Phase::AwaitingPlanApproval, Some("root"), 10),
            summary("solo", Phase::Failed, None, 5),
        ];
        let before = unfolded
            .iter()
            .filter(|r| run_attention(r).needs_you())
            .count();
        let (rows, _) = fold_units(unfolded);
        assert_eq!(
            runs_needing_attention(&rows),
            before,
            "folding lost a gate on the way to Home"
        );

        // Same id in two projects: folding is per project, so `b`'s parent
        // reference must not reach across into the other project's `a`.
        let mut p1 = summary("a", Phase::Review, None, 5);
        p1.project_root = Some(PathBuf::from("/nonexistent/one"));
        let mut p2 = summary("b", Phase::AwaitingPlanApproval, Some("a"), 5);
        p2.project_root = Some(PathBuf::from("/nonexistent/two"));
        let (r1, _) = fold_units(vec![p1]);
        let (r2, _) = fold_units(vec![p2]);
        assert_eq!(r1.len(), 1);
        assert_eq!(
            r2.len(),
            1,
            "an orphan leg stands on its own in its project"
        );
        assert_eq!(runs_needing_attention(&r2), 1);
    }

    #[test]
    fn the_age_shown_is_the_freshest_leg() {
        let runs = vec![
            summary("root", Phase::PlanApproved, None, 600),
            summary("leg", Phase::Review, Some("root"), 3),
        ];
        let (rows, _) = fold_units(runs);
        assert!(
            (Utc::now() - rows[0].updated_at).num_minutes() < 10,
            "a unit is as old as its newest activity"
        );
    }
}

/// Feature 004's acceptance suite: the information-architecture behaviour that
/// sits under the paint (`mod render_stability` covers the paint itself).
///
/// **Seams this contract binds to.** The assertions are the contract; the names
/// below are the agreed surface the plan (`artifacts/plan.md`) already fixes. If
/// an implementation renames one, rename it here too — do not weaken an
/// assertion to fit a different shape.
///
/// | Seam | Phase | What it must be |
/// |---|---|---|
/// | `BrowseLevel::Home` | C | the rail root; `pop()` lands here from Runs and Projects |
/// | `App::new(seed, cfg, local_root: Option<&Path>)` | C | always starts at Home; `local_root` sets the scope |
/// | `HomeData { rows, project_stats }` on `Snapshot`, passed to `draw` | B/C | the off-thread roll-up `draw` consumes |
/// | `gather_home(&[ProjectEntry]) -> Vec<Vec<RunSummary>>` | B/C | one folded, archived-filtered listing per project; disk, off-thread |
/// | `project_stats_of(&[Vec<RunSummary>]) -> Vec<ProjectStat>` | B | pure; counts folded rows |
/// | `build_home_rows(projects, folded, scope, watermark, now)` | C | pure; banding, ranking, capping |
/// | `home_overview(rows, scope, watermark) -> String` | C | Main's Home body |
/// | `read_watermark` / `write_watermark` / `watermark_path` | C | the "finished since last look" clock |
/// | `cross_project_due(browse, since_last, forced)` | B | bounded cross-project invalidation |
/// | `build_roster(cfg, detected, recent)` / `new_run_providers` / `new_run_launch` | D | the fleet picker, with no disk in `draw` |
#[cfg(test)]
mod home_ia {
    use super::*;
    use crate::cli::WorkflowKind;
    use std::path::Path;

    fn project_at(root: &Path, name: &str) -> registry::ProjectEntry {
        registry::ProjectEntry {
            root: root.to_path_buf(),
            name: Some(name.to_string()),
            last_seen: Utc::now(),
            last_run_id: None,
        }
    }

    fn run_in(id: &str, phase: Phase, mins_ago: i64, root: &Path) -> state::RunSummary {
        state::RunSummary {
            id: id.into(),
            workflow: WorkflowKind::Loop,
            archived: false,
            phase,
            updated_at: Utc::now() - chrono::Duration::minutes(mins_ago),
            task: Some(format!("brief for {id}")),
            dry_run: false,
            abandoned: false,
            parent_run: None,
            round: 1,
            legs: 1,
            wants: 0,
            base_ref: None,
            base_commit: None,
            project_root: Some(root.to_path_buf()),
            project_name: root.file_name().map(|s| s.to_string_lossy().into_owned()),
        }
    }

    fn band_of(rows: &[HomeRow], id: &str) -> Option<HomeBand> {
        rows.iter().find_map(|r| match r {
            HomeRow::Run { band, run, .. } if run.id == id => Some(*band),
            _ => None,
        })
    }

    fn ids_in(rows: &[HomeRow], want: HomeBand) -> Vec<String> {
        rows.iter()
            .filter_map(|r| match r {
                HomeRow::Run { band, run, .. } if *band == want => Some(run.id.clone()),
                _ => None,
            })
            .collect()
    }

    // -- Phase A: the retired noun and the comments that describe a dead model --

    /// AC-11. The four comments the feature file names describe a focus model
    /// that no longer exists. Needles are assembled from fragments so this
    /// test's own source cannot satisfy the search it performs.
    #[test]
    fn retired_focus_and_composer_comments_are_gone() {
        let src = include_str!("tui.rs");
        for needle in [
            concat!("Three focus ", "targets"),
            concat!("composer ", "mention"),
            concat!("or the composer ", "still changes focus"),
            concat!("the composer ", "cursor"),
        ] {
            assert!(
                !src.contains(needle),
                "src/tui.rs still documents a focus model that does not exist: {needle:?}"
            );
        }
        // `1`/`2` are the two real direct-focus keys; `3` was never bound.
        assert!(
            !src.contains(concat!("`1` / `2` / ", "`3` jump")),
            "the focus doc still offers a third target"
        );
    }

    /// AC-12. The product doc bakes the run/Home conflation in at the pillar
    /// level; U6 retires it.
    #[test]
    fn the_product_doc_no_longer_conflates_home_with_a_session() {
        let doc = include_str!("../docs/PRODUCT.md");
        assert!(
            !doc.contains(concat!("Session / ", "run home")),
            "docs/PRODUCT.md still names the retired noun in pillar 1"
        );
        assert!(
            doc.contains("Home"),
            "pillar 1 must name the landing view it describes"
        );
    }

    /// AC-13. Discoverability (R6): `n` and `P` are new bindings, so they have
    /// to appear in the help body, and the help body's rail shape has to
    /// describe the tree that actually exists.
    #[test]
    fn home_keys_are_documented_in_the_help_body() {
        let help = HELP_BODY;
        assert!(help.contains(" n "), "`n` is undiscoverable: {help}");
        assert!(help.contains(" P "), "`P` is undiscoverable: {help}");
        assert!(
            help.contains("Home"),
            "the help body's rail shape must start at Home"
        );
        assert!(
            !help.contains(concat!("projects ▸ runs", " ▸ agents")),
            "the help body still describes Projects as the rail root"
        );
        // The Shape line keeps its parenthetical: `help_overlay_wraps_narrow_lines_
        // without_cutting_a_word` is the only long-line wrap probe in the suite and
        // it reads its phrases off this line.
        assert!(
            help.contains("(Enter pushes, Esc pops)"),
            "the Shape line must keep the parenthetical the wrap test probes"
        );
    }

    // -- Phase B: the render-path scan moves off-thread (U13) ------------------

    /// AC-14. `project_stats_of` counts **folded** rows, so the `⚑N` on the
    /// Projects level agrees with the roll-up the Runs level shows. Today's
    /// `rail_project_items` counts unfolded runs and can disagree.
    #[test]
    fn project_stats_count_folded_units_not_invocations() {
        let root = PathBuf::from("/nonexistent/spar");
        let mut parent = run_in("root0001", Phase::AwaitingShipConfirm, 30, &root);
        parent.legs = 2;
        parent.wants = 2; // the unit holds two gates
        let plain = run_in("solo0001", Phase::Review, 5, &root);
        let stats = project_stats_of(&[vec![parent, plain]]);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].n_runs, 2, "one row per unit of work");
        assert_eq!(
            stats[0].needs_you, 2,
            "a two-gate unit contributes both gates (U15)"
        );
    }

    /// AC-15. The other half of U13: `gather_home` really reads disk, drops
    /// archived runs, folds legs into their parent, and degrades a project root
    /// that is not there to an empty listing instead of panicking.
    #[test]
    fn gather_home_lists_visible_folded_runs_and_survives_a_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".spar/runs")).unwrap();
        let paths = SparPaths::new(&root);

        let save = |id: &str, phase: Phase, parent: Option<&str>, archived: bool| {
            paths.ensure_run_dirs(id).unwrap();
            let mut st = RunState::new(id, WorkflowKind::Loop, root.clone());
            st.phase = phase;
            st.parent_run = parent.map(str::to_string);
            if archived {
                st.archived_at = Some(Utc::now());
            }
            st.save(&paths).unwrap();
        };
        save("aaaa0001", Phase::PlanApproved, None, false);
        save(
            "bbbb0002",
            Phase::AwaitingShipConfirm,
            Some("aaaa0001"),
            false,
        );
        save("cccc0003", Phase::Review, None, false);
        save("dddd0004", Phase::Done, None, true);
        // cccc0003 is mid-flight, not abandoned: hold its lock like a live orchestrator
        // would, or `is_abandoned` reads a lockless active phase as Broken (state.rs:684)
        // and this fixture would assert something other than what it names.
        let _cccc_lock = crate::runlock::RunLock::acquire(&paths, "cccc0003").unwrap();

        let missing = tmp.path().join("gone");
        let projects = [project_at(&root, "proj"), project_at(&missing, "gone")];
        let folded = gather_home(&projects);
        assert_eq!(folded.len(), projects.len(), "index-aligned with projects");

        let ids: Vec<&str> = folded[0].iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            folded[0].len(),
            2,
            "archived dropped, the leg folded into its parent: {ids:?}"
        );
        assert!(
            !ids.contains(&"dddd0004"),
            "an archived run must not reach Home: {ids:?}"
        );
        assert!(
            !ids.contains(&"aaaa0001") || !ids.contains(&"bbbb0002"),
            "the leg and its parent must be one row: {ids:?}"
        );
        assert!(folded[1].is_empty(), "a missing project root reads as zero");

        // And the stats derived from that same pass agree with it.
        let stats = project_stats_of(&folded);
        assert_eq!(stats[0].n_runs, 2);
        assert_eq!(stats[0].needs_you, 1, "one gate in the unit");
        assert_eq!(stats[1].n_runs, 0);
    }

    /// AC-16. The cross-project sweep Home needs is bounded: a Home that has
    /// not changed does not re-list every registered project on every 200ms
    /// refresh tick, entering Home forces one immediate build, and levels that
    /// are not cross-project never trigger it at all.
    #[test]
    fn cross_project_refresh_is_bounded_and_forced_on_entry() {
        assert!(
            CROSS_PROJECT_REFRESH > REFRESH,
            "a per-tick cross-project sweep is the scale failure this moves off draw"
        );
        assert!(
            !cross_project_due(BrowseLevel::Home, REFRESH, false),
            "one refresh tick is not a cross-project rebuild"
        );
        assert!(
            cross_project_due(BrowseLevel::Home, CROSS_PROJECT_REFRESH, false),
            "the cadence must eventually fire"
        );
        assert!(
            cross_project_due(BrowseLevel::Home, Duration::from_millis(0), true),
            "entering Home or toggling scope forces one build"
        );
        assert!(
            cross_project_due(BrowseLevel::Projects, CROSS_PROJECT_REFRESH, false),
            "the Projects level needs the same per-project stats"
        );
        for level in [BrowseLevel::Runs, BrowseLevel::Agents] {
            assert!(
                !cross_project_due(level, CROSS_PROJECT_REFRESH * 10, false),
                "{level:?} is scoped to one project and must not sweep the registry"
            );
        }
    }

    // -- Phase C: bands, ranking, scope, watermark ----------------------------

    /// AC-17. Four bands, in order, headers always emitted, first match wins so
    /// a run is in exactly one band.
    #[test]
    fn home_emits_four_bands_in_order_with_headers_always_present() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let watermark = Utc::now() - chrono::Duration::hours(1);
        let now = Utc::now();
        for folded in [
            vec![vec![]],
            vec![vec![
                run_in("gate0001", Phase::AwaitingPlanApproval, 30, &root),
                run_in("work0001", Phase::Review, 2, &root),
                run_in("done0001", Phase::Done, 10, &root),
            ]],
        ] {
            let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now);
            let headers: Vec<HomeBand> = rows
                .iter()
                .filter_map(|r| match r {
                    HomeRow::Header(b) => Some(*b),
                    _ => None,
                })
                .collect();
            assert_eq!(
                headers,
                vec![
                    HomeBand::NeedsMe,
                    HomeBand::Running,
                    HomeBand::Finished,
                    HomeBand::StartNew
                ],
                "band headers must be present and in order even when empty"
            );
            assert!(
                rows.iter().any(|r| matches!(r, HomeRow::NewRun)),
                "band 4's action row is always there"
            );
            // Exactly one band per run.
            let mut seen: Vec<&str> = Vec::new();
            for r in &rows {
                if let HomeRow::Run { run, .. } = r {
                    assert!(!seen.contains(&run.id.as_str()), "{} in two bands", run.id);
                    seen.push(&run.id);
                }
            }
        }
    }

    /// AC-18. Band membership is declared, not a fallthrough: gates and broken
    /// runs are band 1 (U5's `needs_you`, so a broken run is never dropped),
    /// active runs are band 2, and only genuinely-finished runs newer than the
    /// watermark are band 3. `Stopped` and `PlanRejected` must not be quietly
    /// filed as "finished".
    #[test]
    fn phases_land_in_their_declared_bands() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let watermark = Utc::now() - chrono::Duration::hours(6);
        let now = Utc::now();
        let cases: Vec<(&str, Phase, Option<HomeBand>)> = vec![
            ("gate", Phase::AwaitingPlanApproval, Some(HomeBand::NeedsMe)),
            ("ship", Phase::AwaitingShipConfirm, Some(HomeBand::NeedsMe)),
            ("fail", Phase::Failed, Some(HomeBand::NeedsMe)),
            ("stuk", Phase::Stuck, Some(HomeBand::NeedsMe)),
            ("quot", Phase::Quota, Some(HomeBand::NeedsMe)),
            ("esca", Phase::Escalated, Some(HomeBand::NeedsMe)),
            ("revw", Phase::Review, Some(HomeBand::Running)),
            ("disp", Phase::Dispatch, Some(HomeBand::Running)),
            ("done", Phase::Done, Some(HomeBand::Finished)),
            ("stop", Phase::Stopped, Some(HomeBand::Finished)),
            ("rejd", Phase::PlanRejected, Some(HomeBand::Finished)),
        ];
        let runs: Vec<state::RunSummary> = cases
            .iter()
            .map(|(id, phase, _)| run_in(id, *phase, 1, &root))
            .collect();
        let rows = build_home_rows(&projects, &[runs], &HomeScope::All, watermark, now);
        for (id, phase, want) in &cases {
            assert_eq!(
                band_of(&rows, id),
                *want,
                "{phase:?} landed in the wrong band"
            );
        }
        // An abandoned run is broken, not running.
        let mut abandoned = run_in("aban", Phase::Review, 1, &root);
        abandoned.abandoned = true;
        let rows = build_home_rows(
            &projects,
            &[vec![abandoned]],
            &HomeScope::All,
            watermark,
            now,
        );
        assert_eq!(band_of(&rows, "aban"), Some(HomeBand::NeedsMe));
    }

    /// AC-19. Band 1 is ranked by wait time descending — the longest-waiting
    /// gate first. That is deliberately *not* the rail's recency-first
    /// attention sort.
    #[test]
    fn needs_me_ranks_by_wait_time_descending() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let folded = vec![vec![
            run_in("recent01", Phase::AwaitingShipConfirm, 1, &root),
            run_in("oldest01", Phase::AwaitingPlanApproval, 60, &root),
            run_in("middle01", Phase::AwaitingShipConfirm, 15, &root),
        ]];
        let rows = build_home_rows(
            &projects,
            &folded,
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
        );
        assert_eq!(
            ids_in(&rows, HomeBand::NeedsMe),
            vec!["oldest01", "middle01", "recent01"],
            "band 1 is longest-waiting first"
        );
        // The recorded wait is what the row renders, and it is monotonic with
        // the ranking.
        let waits: Vec<Duration> = rows
            .iter()
            .filter_map(|r| match r {
                HomeRow::Run {
                    band: HomeBand::NeedsMe,
                    waited,
                    ..
                } => Some(*waited),
                _ => None,
            })
            .collect();
        assert!(waits.windows(2).all(|w| w[0] >= w[1]), "{waits:?}");

        // Bands 2 and 3 are recency-first instead.
        let folded = vec![vec![
            run_in("old_work", Phase::Review, 60, &root),
            run_in("new_work", Phase::Review, 1, &root),
        ]];
        let rows = build_home_rows(
            &projects,
            &folded,
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
        );
        assert_eq!(
            ids_in(&rows, HomeBand::Running),
            vec!["new_work", "old_work"]
        );
    }

    /// AC-20. A clock that ran backwards (a future `updated_at` from a skewed
    /// host or a hand-edited state file) must produce a zero wait, not a panic
    /// and not a row that sorts to the top forever.
    #[test]
    fn a_future_updated_at_is_zero_wait_not_a_panic() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let folded = vec![vec![
            run_in("future01", Phase::AwaitingShipConfirm, -600, &root),
            run_in("normal01", Phase::AwaitingShipConfirm, 30, &root),
        ]];
        let rows = build_home_rows(
            &projects,
            &folded,
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
        );
        let waited = |id: &str| {
            rows.iter()
                .find_map(|r| match r {
                    HomeRow::Run { run, waited, .. } if run.id == id => Some(*waited),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(waited("future01"), Duration::from_secs(0));
        assert_eq!(
            ids_in(&rows, HomeBand::NeedsMe),
            vec!["normal01", "future01"],
            "a future timestamp must not outrank a real wait"
        );
    }

    /// AC-21. The band cap keeps a thousand-run workspace from building a
    /// thousand rows a frame — but it must never truncate band 1, and a band it
    /// does cap has to say how many it dropped.
    #[test]
    fn the_band_cap_never_truncates_what_needs_you() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let n = HOME_BAND_CAP + 25;
        let mut runs: Vec<state::RunSummary> = (0..n)
            .map(|i| {
                run_in(
                    &format!("gate{i:04}"),
                    Phase::AwaitingPlanApproval,
                    i as i64,
                    &root,
                )
            })
            .collect();
        runs.extend((0..n).map(|i| run_in(&format!("work{i:04}"), Phase::Review, i as i64, &root)));
        let rows = build_home_rows(
            &projects,
            &[runs],
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
        );
        assert_eq!(
            ids_in(&rows, HomeBand::NeedsMe).len(),
            n,
            "band 1 must never be capped"
        );
        assert_eq!(
            ids_in(&rows, HomeBand::Running).len(),
            HOME_BAND_CAP,
            "band 2 caps"
        );
        let more = rows.iter().find_map(|r| match r {
            HomeRow::More { band, n } => Some((*band, *n)),
            _ => None,
        });
        assert_eq!(
            more,
            Some((HomeBand::Running, n - HOME_BAND_CAP)),
            "a capped band must account for what it dropped"
        );
    }

    /// AC-22. U15 at Home: folding is a display choice, never a way to lose a
    /// gate. A two-leg unit with two gates is one row that says `⚑2` and
    /// contributes 2 to the roll-up.
    #[test]
    fn folding_never_hides_a_gate_from_home() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let mut unit = run_in("legb0002", Phase::AwaitingPlanApproval, 20, &root);
        unit.legs = 2;
        unit.wants = 2;
        let rows = build_home_rows(
            &projects,
            &[vec![unit]],
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
        );
        let band1: Vec<state::RunSummary> = rows
            .iter()
            .filter_map(|r| match r {
                HomeRow::Run {
                    band: HomeBand::NeedsMe,
                    run,
                    ..
                } => Some(run.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(band1.len(), 1, "one unit of work, one row");
        assert_eq!(band1[0].wants, 2, "the row must carry both gates");
        assert_eq!(
            runs_needing_attention(&band1),
            2,
            "the roll-up counts legs, not rows"
        );
        assert!(
            home_overview(&rows, &HomeScope::All, now - chrono::Duration::hours(6)).contains("⚑2"),
            "a multi-gate unit says so on screen"
        );
    }

    /// AC-23. Scope filters rows; it does not change the view. Both scopes emit
    /// the same four headers in the same order (U20).
    #[test]
    fn home_scope_filters_rows_without_changing_the_bands() {
        let a = PathBuf::from("/nonexistent/acme-api");
        let b = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&a, "acme-api"), project_at(&b, "spar")];
        let now = Utc::now();
        let folded = vec![
            vec![run_in("acme0001", Phase::AwaitingShipConfirm, 10, &a)],
            vec![run_in("spar0001", Phase::AwaitingShipConfirm, 10, &b)],
        ];
        let watermark = now - chrono::Duration::hours(6);
        let all = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now);
        let scoped = build_home_rows(
            &projects,
            &folded,
            &HomeScope::Project(b.clone()),
            watermark,
            now,
        );
        let headers = |rows: &[HomeRow]| -> Vec<HomeBand> {
            rows.iter()
                .filter_map(|r| match r {
                    HomeRow::Header(x) => Some(*x),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(headers(&all), headers(&scoped), "scope changed the bands");
        assert_eq!(ids_in(&all, HomeBand::NeedsMe).len(), 2);
        assert_eq!(ids_in(&scoped, HomeBand::NeedsMe), vec!["spar0001"]);

        // `spar` inside a repo lands on Home scoped to that repo, not on the
        // project's raw run list.
        let app = App::new(None, Config::default(), Some(b.as_path()));
        assert_eq!(app.browse, BrowseLevel::Home);
        assert_eq!(app.home_scope, HomeScope::Project(b.clone()));
        // Outside a repo it is every registered project.
        let app = App::new(None, Config::default(), None);
        assert_eq!(app.home_scope, HomeScope::All);
        // `P` toggles between the two and back.
        let mut app = App::new(None, Config::default(), Some(b.as_path()));
        toggle_home_scope(&mut app, Some(b.as_path()));
        assert_eq!(app.home_scope, HomeScope::All);
        toggle_home_scope(&mut app, Some(b.as_path()));
        assert_eq!(app.home_scope, HomeScope::Project(b));
    }

    /// AC-24. The watermark: a run that finished before the operator's last
    /// look is not in band 3; one that finished after it is. A missing or
    /// corrupt file reads as a day ago, so a first run shows a useful band
    /// rather than an empty one, and it lives under the global spar home
    /// because Home is cross-project and `.spar/` is not.
    #[test]
    fn the_finished_band_is_bounded_by_the_watermark() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let watermark = now - chrono::Duration::hours(2);
        let folded = vec![vec![
            run_in("recentdn", Phase::Done, 30, &root),
            run_in("olderdne", Phase::Done, 600, &root),
        ]];
        let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now);
        assert_eq!(
            ids_in(&rows, HomeBand::Finished),
            vec!["recentdn"],
            "only what landed since the last look"
        );

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("home_watermark.json");
        let missing = read_watermark(&path);
        assert!(
            (now - missing).num_hours() >= 23 && (now - missing).num_hours() <= 25,
            "a missing watermark reads as a day ago, not the epoch: {missing}"
        );
        std::fs::write(&path, "{ not json").unwrap();
        let corrupt = read_watermark(&path);
        assert!(
            (now - corrupt).num_hours() >= 23,
            "a corrupt watermark must be nonfatal: {corrupt}"
        );
        let at = now - chrono::Duration::minutes(5);
        write_watermark(&path, at).unwrap();
        assert!(
            (read_watermark(&path) - at).num_seconds().abs() <= 1,
            "watermark round-trip"
        );
        // Writing into a directory that does not exist must not take the app down.
        let _ = write_watermark(&tmp.path().join("nope/deeper/w.json"), at);
        assert!(
            watermark_path().starts_with(registry::spar_home()),
            "a cross-project watermark cannot live in a per-project .spar/"
        );
    }

    /// AC-25. The band the operator is looking at must not empty underneath
    /// them: the watermark is read once and held for the session, so re-deriving
    /// Home from the same `App` gives the same band 3.
    #[test]
    fn the_finished_band_is_stable_while_the_session_is_open() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let app = App::new(None, Config::default(), Some(root.as_path()));
        let folded = vec![vec![run_in("justdone", Phase::Done, 1, &root)]];
        let first = build_home_rows(
            &projects,
            &folded,
            &app.home_scope,
            app.home_watermark,
            Utc::now(),
        );
        let later = build_home_rows(
            &projects,
            &folded,
            &app.home_scope,
            app.home_watermark,
            Utc::now() + chrono::Duration::minutes(30),
        );
        assert_eq!(
            ids_in(&first, HomeBand::Finished),
            ids_in(&later, HomeBand::Finished),
            "the watermark must not advance while the operator is looking at it"
        );
        assert!(!ids_in(&first, HomeBand::Finished).is_empty());
    }

    // -- Phase C: navigation --------------------------------------------------

    fn nav_rows(root: &Path) -> Vec<HomeRow> {
        vec![
            HomeRow::Header(HomeBand::NeedsMe),
            HomeRow::Run {
                band: HomeBand::NeedsMe,
                run: run_in("gate0001", Phase::AwaitingShipConfirm, 90, root),
                waited: Duration::from_secs(5400),
            },
            HomeRow::Header(HomeBand::Running),
            HomeRow::Run {
                band: HomeBand::Running,
                run: run_in("work0001", Phase::Review, 2, root),
                waited: Duration::from_secs(120),
            },
            HomeRow::Header(HomeBand::Finished),
            HomeRow::Header(HomeBand::StartNew),
            HomeRow::NewRun,
        ]
    }

    /// AC-26. Navigation steps over headers, and never lands on one — including
    /// at both ends of the list, where a naive clamp puts the cursor on the
    /// band-1 header or the band-4 header.
    #[test]
    fn home_navigation_never_lands_on_a_header() {
        let root = PathBuf::from("/nonexistent/spar");
        let rows = nav_rows(&root);
        let mut app = App::new(None, Config::default(), None);
        assert_eq!(rail_len(BrowseLevel::Home, 0, rows.len(), 0, 0), rows.len());
        // Sweep the whole list in both directions, plus the paging deltas.
        for delta in [1i32, -1, 5, -5] {
            let mut app = App::new(None, Config::default(), None);
            for _ in 0..(rows.len() * 2) {
                rail_move(&mut app, &[], &rows, &[], 0, delta);
                assert!(
                    !matches!(rows.get(app.selected_home), Some(HomeRow::Header(_))),
                    "delta {delta} landed the cursor on a header at {}",
                    app.selected_home
                );
                assert!(app.selected_home < rows.len(), "cursor left the list");
            }
        }
        // A mouse click on a header is ignored rather than selecting it.
        app.selected_home = 1;
        rail_select(&mut app, 0, 0, &rows, 0, 0);
        assert_eq!(
            app.selected_home, 1,
            "a click on a header must not move the cursor"
        );
        rail_select(&mut app, 3, 0, &rows, 0, 0);
        assert_eq!(app.selected_home, 3, "a click on a run row selects it");
    }

    /// AC-27. `Enter` on a Home run row opens **that run's agents** (the
    /// feature's navigation rule), switching the active project to the row's
    /// own project. `Esc` then exposes that project's runs, and the next `Esc`
    /// returns to Home. `Esc` at Home is a no-op and never quits.
    #[test]
    fn enter_on_a_home_run_row_opens_that_runs_agents() {
        let root = PathBuf::from("/nonexistent/acme-api");
        let rows = nav_rows(&root);
        let mut app = App::new(None, Config::default(), None);
        app.selected_home = 1; // the gated run
        let mut active = PathBuf::from("/nonexistent/elsewhere");
        rail_enter(&mut app, &[], &rows, &[], None, &mut active, None);
        assert_eq!(
            app.browse,
            BrowseLevel::Agents,
            "Enter opens the run's agents"
        );
        assert_eq!(active, root, "the active project follows the row");
        assert_eq!(
            app.home_target_run.as_deref(),
            Some("gate0001"),
            "the run is carried by identity across the snapshot handoff"
        );
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Runs);
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Home);
        app.rail_pop();
        assert_eq!(app.browse, BrowseLevel::Home, "Esc at the root is a no-op");

        // A project row takes the project route instead; the action row opens
        // the Phase D surface.
        let projects = [project_at(&root, "acme-api")];
        let rows = vec![
            HomeRow::Header(HomeBand::StartNew),
            HomeRow::NewRun,
            HomeRow::Project(0),
        ];
        let mut app = App::new(None, Config::default(), None);
        app.selected_home = 2;
        let mut active = PathBuf::from("/nonexistent/elsewhere");
        rail_enter(&mut app, &projects, &rows, &[], None, &mut active, None);
        assert_eq!(app.browse, BrowseLevel::Runs);
        assert_eq!(active, root);

        let mut app = App::new(None, Config::default(), None);
        app.selected_home = 1;
        let mut active = PathBuf::from("/nonexistent/elsewhere");
        rail_enter(&mut app, &projects, &rows, &[], None, &mut active, None);
        assert!(
            app.new_run.is_some(),
            "the action row opens the new-run surface"
        );

        // A header is inert.
        let mut app = App::new(None, Config::default(), None);
        app.selected_home = 0;
        let before = app.browse;
        rail_enter(&mut app, &projects, &rows, &[], None, &mut active, None);
        assert_eq!(app.browse, before, "Enter on a header does nothing");
        assert!(app.new_run.is_none());
    }

    /// AC-28. R3: Home re-ranks every snapshot (wait time changes every
    /// minute), so the cursor is glued to the row's identity, not its index.
    #[test]
    fn the_home_cursor_follows_the_row_not_the_index() {
        let root = PathBuf::from("/nonexistent/spar");
        let rows = nav_rows(&root);
        let mut app = App::new(None, Config::default(), None);
        rail_select(&mut app, 3, 0, &rows, 0, 0); // the running run
        assert_eq!(app.home_key.as_deref(), Some("run:work0001"));

        // Next snapshot: a new gate arrives and pushes everything down.
        let mut reordered = vec![
            HomeRow::Header(HomeBand::NeedsMe),
            HomeRow::Run {
                band: HomeBand::NeedsMe,
                run: run_in("newgate1", Phase::AwaitingPlanApproval, 120, &root),
                waited: Duration::from_secs(7200),
            },
        ];
        reordered.extend(rows.iter().skip(1).cloned());
        resync_home_selection(&mut app, &reordered);
        match reordered.get(app.selected_home) {
            Some(HomeRow::Run { run, .. }) => assert_eq!(run.id, "work0001"),
            other => panic!("cursor jumped to {other:?}"),
        }

        // The row it was on disappearing must clamp, not index out of bounds.
        let shrunk = vec![HomeRow::Header(HomeBand::StartNew), HomeRow::NewRun];
        resync_home_selection(&mut app, &shrunk);
        assert!(app.selected_home < shrunk.len());
        assert!(!matches!(shrunk[app.selected_home], HomeRow::Header(_)));
    }

    /// AC-29. `a` still works at the landing view: it cycles the Home cursor
    /// through band 1 instead of telling the operator to open a project first.
    #[test]
    fn a_cycles_the_needs_me_band_at_home() {
        let root = PathBuf::from("/nonexistent/spar");
        let rows = vec![
            HomeRow::Header(HomeBand::NeedsMe),
            HomeRow::Run {
                band: HomeBand::NeedsMe,
                run: run_in("gate0001", Phase::AwaitingShipConfirm, 90, &root),
                waited: Duration::from_secs(5400),
            },
            HomeRow::Run {
                band: HomeBand::NeedsMe,
                run: run_in("gate0002", Phase::AwaitingPlanApproval, 30, &root),
                waited: Duration::from_secs(1800),
            },
            HomeRow::Header(HomeBand::Running),
            HomeRow::Run {
                band: HomeBand::Running,
                run: run_in("work0001", Phase::Review, 2, &root),
                waited: Duration::from_secs(120),
            },
            HomeRow::Header(HomeBand::Finished),
            HomeRow::Header(HomeBand::StartNew),
            HomeRow::NewRun,
        ];
        let mut app = App::new(None, Config::default(), None);
        app.selected_home = 1;
        jump_to_attention(&mut app, &[], &rows);
        assert_eq!(app.selected_home, 2, "next gate");
        jump_to_attention(&mut app, &[], &rows);
        assert_eq!(app.selected_home, 1, "wraps within band 1");
        let flashed = app
            .flash
            .as_ref()
            .map(|(_, m, _, _)| m.clone())
            .unwrap_or_default();
        assert!(
            !flashed.contains("open a project first"),
            "Home is not a place where `a` is dead: {flashed:?}"
        );
    }

    // -- Phase D: the new-run surface and the fleet picker --------------------

    fn cfg_with(order: &[&str]) -> Config {
        Config {
            providers: crate::config::ProviderConfig {
                order: order.iter().map(|s| s.to_string()).collect(),
            },
            ..Config::default()
        }
    }

    /// AC-30. The roster is built from configured refs, detected CLIs and the
    /// most recent fleet. A configured `api:` ref stays selectable even though
    /// CLI detection knows nothing about it; a configured native ref whose
    /// binary is not on PATH is disabled **with a reason**; a malformed ref is
    /// disabled and says why.
    #[test]
    fn the_roster_keeps_api_refs_selectable_and_explains_what_it_disables() {
        let cfg = cfg_with(&[
            "api:openai@gpt-5.6",
            "cli:claude@opus",
            "cli:nosuchcli",
            "claude",
        ]);
        let detected = [
            ("claude".to_string(), true),
            ("codex".to_string(), true),
            ("nosuchcli".to_string(), false),
        ];
        let roster = build_roster(&cfg, &detected, None);
        let by = |label: &str| {
            roster
                .iter()
                .find(|e| e.label.contains(label))
                .unwrap_or_else(|| panic!("{label} missing from roster: {roster:?}"))
        };
        let api = by("api:openai");
        assert!(api.available, "a supported api: ref must stay selectable");
        assert_eq!(api.source, RosterSource::Configured);

        let claude = by("cli:claude@opus");
        assert!(claude.available);
        assert!(
            claude.label.contains("@opus"),
            "a configured model pin must survive into the picker: {:?}",
            claude.label
        );

        let missing = by("cli:nosuchcli");
        assert!(!missing.available);
        assert!(
            missing.reason.as_deref().is_some_and(|r| !r.is_empty()),
            "a disabled row must say why"
        );

        let malformed = by("claude");
        assert!(
            !malformed.available && malformed.reason.is_some(),
            "a bare name is not a provider ref and must explain itself: {malformed:?}"
        );

        // Detection adds what config did not list, and never duplicates it.
        assert_eq!(
            roster
                .iter()
                .filter(|e| matches!(&e.choice, RosterChoice::Provider(p) if p.starts_with("cli:claude")))
                .count(),
            1,
            "a detected CLI must not duplicate its configured entry: {roster:?}"
        );
        let codex = by("cli:codex");
        assert_eq!(codex.source, RosterSource::Detected);

        // R7: nothing configured, nothing on PATH — an explanatory empty
        // roster, not an empty selectable list.
        let bare = build_roster(&cfg_with(&[]), &[("claude".into(), false)], None);
        assert!(
            bare.iter().all(|e| !e.available),
            "nothing usable must be nothing selectable: {bare:?}"
        );
    }

    /// AC-31. A recent fleet is one roster row standing for several providers.
    /// Picking it expands, and expansion deduplicates in first-picked order —
    /// a comma-joined string is not a provider reference.
    #[test]
    fn a_recent_fleet_expands_and_dedupes_in_pick_order() {
        let cfg = cfg_with(&["cli:claude@opus", "cli:codex@gpt-5.6-terra"]);
        let fleet = vec![
            "cli:codex@gpt-5.6-terra".to_string(),
            "cli:muse@muse-spark-1.2-contributor".to_string(),
        ];
        let roster = build_roster(&cfg, &[("claude".into(), true)], Some(("ab12cd34", &fleet)));
        let fleet_idx = roster
            .iter()
            .position(|e| matches!(e.choice, RosterChoice::Fleet(_)))
            .expect("the recent fleet is a roster choice");
        assert_eq!(roster[fleet_idx].source, RosterSource::RecentFleet);
        assert!(
            roster[fleet_idx].label.contains("ab12cd34"),
            "the fleet row names the run it came from: {:?}",
            roster[fleet_idx].label
        );

        let claude_idx = roster
            .iter()
            .position(|e| matches!(&e.choice, RosterChoice::Provider(p) if p == "cli:claude@opus"))
            .unwrap();
        let codex_idx = roster
            .iter()
            .position(
                |e| matches!(&e.choice, RosterChoice::Provider(p) if p == "cli:codex@gpt-5.6-terra"),
            )
            .unwrap();

        let mut nr = new_run_fixture();
        nr.roster = roster;
        nr.picked = vec![claude_idx, fleet_idx, codex_idx];
        assert_eq!(
            new_run_providers(&nr),
            vec![
                "cli:claude@opus".to_string(),
                "cli:codex@gpt-5.6-terra".to_string(),
                "cli:muse@muse-spark-1.2-contributor".to_string(),
            ],
            "expanded, deduplicated, in the order they were picked"
        );
    }

    /// AC-32. R8/O-invariant: `--providers` is required on `plan`, so the
    /// surface refuses rather than building a malformed argv. It also refuses
    /// without a task and without a target project — the empty-registry case,
    /// where an arbitrary cwd must never be treated as a project.
    #[test]
    fn the_new_run_surface_refuses_before_it_spawns() {
        let mut nr = new_run_fixture();
        nr.picked.clear();
        assert!(
            new_run_launch(&nr).is_err(),
            "zero providers must not dispatch a fleet-less plan"
        );

        let mut nr = new_run_fixture();
        nr.task = "   ".into();
        assert!(
            new_run_launch(&nr).is_err(),
            "an empty task must not dispatch"
        );

        let mut nr = new_run_fixture();
        nr.project = None;
        nr.projects.clear();
        let err = new_run_launch(&nr).unwrap_err();
        assert!(
            !err.is_empty(),
            "no target project must be an explained refusal, not a launch against the cwd"
        );

        // A disabled roster row cannot be picked into a fleet.
        let mut nr = new_run_fixture();
        nr.roster[1].available = false;
        nr.roster[1].reason = Some("not on PATH".into());
        nr.picked = vec![1];
        assert!(
            new_run_launch(&nr).is_err(),
            "an unavailable provider must not reach argv"
        );

        // The happy path: the target project comes from the surface, not from
        // whatever the active root happens to be, and the argv is the same
        // `plan -t … --providers …` the palette already sends.
        let nr = new_run_fixture();
        let (target, argv) = new_run_launch(&nr).expect("a valid surface launches");
        assert_eq!(target, PathBuf::from("/nonexistent/spar"));
        assert_eq!(argv[0], "plan");
        let t = argv.iter().position(|a| a == "-t").expect("-t");
        assert_eq!(argv[t + 1], nr.task);
        let p = argv
            .iter()
            .position(|a| a == "--providers")
            .expect("--providers is required on plan");
        assert_eq!(argv[p + 1], "cli:claude@opus");
    }

    /// AC-35. The docs and decision rows are part of this change, not a
    /// follow-up: the embedded operator skill describes a rail root that will
    /// no longer exist, the IA doc still lists the Phase B scan as outstanding,
    /// and the calls a future agent could reverse need rows.
    #[test]
    fn the_agent_facing_docs_move_with_the_feature() {
        let core = include_str!("../skills/core.md");
        assert!(
            !core.contains(concat!("projects ▸ runs", " ▸ agents")),
            "skills/core.md still calls Projects the rail root"
        );
        assert!(
            core.contains("Home"),
            "skills/core.md must describe the landing view"
        );
        for key in ["`n`", "`P`"] {
            assert!(
                core.contains(key),
                "skills/core.md must document the new key {key}"
            );
        }

        let ia = include_str!("../docs/architecture-tui-ia.md");
        assert!(
            !ia.contains(concat!("remains 004 ", "Phase B's job")),
            "the IA doc still lists the render-path scan as outstanding"
        );

        let decisions = include_str!("../DECISIONS.md");
        for row in [
            "| U18 |", "| U19 |", "| U20 |", "| U21 |", "| U22 |", "| U23 |",
        ] {
            assert!(
                decisions.contains(row),
                "DECISIONS.md is missing {row} — a reversible call went unrecorded"
            );
        }
    }

    /// AC-33. U3's punt is retired where it is spoken: with no run selected the
    /// palette's `plan` opens the surface pre-filled instead of erroring to the
    /// CLI, `spar --task` seeds the surface rather than the palette, and the
    /// palette's own help text no longer promises only a reused fleet.
    #[test]
    fn the_fresh_fleet_punt_is_retired() {
        let app = App::new(Some("describe the change".into()), Config::default(), None);
        let nr = app
            .new_run
            .as_ref()
            .expect("`spar --task` must open the new-run surface");
        assert_eq!(nr.task, "describe the change");
        assert!(
            app.palette.is_none(),
            "the task seed no longer opens a pre-filled palette"
        );

        let plan_help = PALETTE_CMDS
            .iter()
            .find(|c| c.name == "plan")
            .map(|c| c.help)
            .expect("a plan verb");
        assert!(
            !plan_help.contains("reuses the selected run's fleet"),
            "the palette still says a fresh fleet is impossible: {plan_help:?}"
        );
    }
}
