//! Product shell — clear fleet dashboard for multi-agent runs.
use crate::config::Config;
use crate::events;
use crate::liveness::SlotActivity;
use crate::paths::{self, SparPaths};
use crate::process;
use crate::quota::QuotaStore;
use crate::record::{self, Record, RecordKind, SourceId};
use crate::registry;
use crate::state::{self, Phase, RunState, SlotRole, SlotState, SlotStatus};
use crate::tmux;
use crate::workflow;
use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
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
    chip, dim, lerp, muted, page, rule, selected, toward_bg, ACCENT, ACCENT_SOFT, ALERT,
    ALERT_WASH, BG_OVERLAY, CODE, DRIVE_WASH, FG, FG_DIM, FG_MUTED, GATE_WASH, HINT, INFO, INK, OK,
    PULSE_HI, PULSE_LO, RULE, SURFACE_RAISED, SURFACE_SUNKEN, TRAIL_FALLOFF, WARN,
};

/// Chrome glyphs. One border language: a thin rule under the chrome bands, a thin
/// seam between rail and Main, and a heavy underline marking the active tab.
const RULE_H: &str = "─";
const RULE_SEAM: &str = "│";
const RULE_TEE: &str = "┬";
const TAB_MARK: &str = "━";
/// The rail's selection bar.
const SEL_BAR: &str = "▌";
/// The state-marker cell for a row that is working: a bar that breathes between
/// `PULSE_LO` and `PULSE_HI` (U30). Deliberately not a spinner — a rail of eight
/// runs with three spinners in it reads as noise, where three bars breathing in
/// phase read as one fact.
const LIVE_BAR: &str = "┃";

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
    Plan,
    Review,
    Chat,
    Shell,
}

/// Tab strip order — also the `[` / `]` cycle order and the narrow strip order.
/// Plan and Review (U9/U35) land feature 005's remainder as their own surfaces.
const MAIN_TABS: [MainTab; 7] = [
    MainTab::Log,
    MainTab::Activity,
    MainTab::Diff,
    MainTab::Plan,
    MainTab::Review,
    MainTab::Chat,
    MainTab::Shell,
];

/// Home's tabs. Log/Activity/Diff are all `f(a selected run)` and Home has none,
/// so at Home the other three rendered the identical body and the strip offered
/// three ways to change nothing (U31). Shell is genuinely still there: it is
/// project-scoped, not run-scoped (see `manage_terminal`).
const HOME_TABS: [MainTab; 3] = [MainTab::Log, MainTab::Chat, MainTab::Shell];

/// The tabs that mean something at `browse`, in strip and `[`/`]` order.
fn tabs_for(browse: BrowseLevel) -> &'static [MainTab] {
    match browse {
        BrowseLevel::Home => &HOME_TABS,
        _ => &MAIN_TABS,
    }
}

impl MainTab {
    fn label(self) -> &'static str {
        match self {
            MainTab::Log => "Log",
            MainTab::Activity => "Activity",
            MainTab::Diff => "Diff",
            MainTab::Plan => "Plan",
            MainTab::Review => "Review",
            MainTab::Chat => "Chat",
            MainTab::Shell => "Shell",
        }
    }

    /// Narrow strip label (U35). Seven tabs don't fit `<80` at full labels; every tab
    /// abbreviates uniformly rather than fitting per-tab, which would move the strip
    /// as labels changed length (U11).
    fn short_label(self) -> &'static str {
        match self {
            MainTab::Log => "Log",
            MainTab::Activity => "Act",
            MainTab::Diff => "Diff",
            MainTab::Plan => "Plan",
            MainTab::Review => "Rev",
            MainTab::Chat => "C",
            MainTab::Shell => "Sh",
        }
    }

    /// What this tab is called at `browse`. The Log tab is the run's live stream
    /// everywhere except Home, where the same slot holds the landing detail — so
    /// it is named for what it shows rather than for which slot it occupies.
    fn label_at(self, browse: BrowseLevel) -> &'static str {
        match (self, browse) {
            (MainTab::Log, BrowseLevel::Home) => "Home",
            _ => self.label(),
        }
    }

    fn idx_in(self, tabs: &[MainTab]) -> usize {
        tabs.iter().position(|t| *t == self).unwrap_or(0)
    }
    fn next_in(self, tabs: &[MainTab]) -> Self {
        tabs[(self.idx_in(tabs) + 1) % tabs.len()]
    }
    fn prev_in(self, tabs: &[MainTab]) -> Self {
        tabs[(self.idx_in(tabs) + tabs.len() - 1) % tabs.len()]
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
        name: "msg",
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
    // Focus reporting: an unfocused window animates nothing (U30). Terminals that
    // do not implement 1004 simply never send the events, which leaves `focused`
    // at its startup `true` and the old always-animating behaviour.
    out.execute(EnableFocusChange)?;
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
        let _ = out.execute(DisableFocusChange);
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
        // Ranking-time snapshot of the wait, kept for AC-19/AC-20's monotonicity
        // assertions; both renderers recompute a live wait from `run.updated_at`
        // at paint time instead of reading this, so it is unread outside tests.
        #[allow(dead_code)]
        waited: Duration,
    },
    /// A capped band's tail: how many more rows it is not showing.
    More {
        band: HomeBand,
        n: usize,
    },
    /// A band with nothing in it, saying so on its own row. The header alone is
    /// reserved space (U14) but not an answer: "RUNNING" with a gap under it does
    /// not distinguish "nothing is running" from "still loading". Not selectable.
    Empty(HomeBand),
    /// Band 4's project switcher: an index into the snapshot's `projects` (for the
    /// lookups that need the full entry) plus that project's root, carried alongside
    /// so `home_row_key` has a stable identity that does not move when the registry
    /// gains or loses an earlier project (AC-28) — the index alone would.
    Project(usize, PathBuf),
    /// Band 4's action row — opens the Phase D new-run surface.
    NewRun,
    /// A placeholder shown while the first cross-project scan is in flight.
    /// Reserved from the layout, not the content (U47).
    Skeleton {
        band: HomeBand,
        slot: usize,
    },
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
    loading: bool,
}

/// Per-band cap on rendered rows, so a thousand-run workspace does not build a
/// thousand `ListItem`s a frame. Band 1 (`NeedsMe`) is exempt — the cap must never
/// hide something that wants the operator.
const HOME_BAND_CAP: usize = 50;

const HOME_SKELETON_ROWS: usize = 3;

/// Field the Phase D new-run modal is currently editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewRunField {
    Project,
    Task,
    Workflow,
    Roles,
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
    workflow: Option<crate::runspec::SpecWorkflow>,
    roles: Vec<crate::runspec::RoleAssignment>,
    arena_pool: Vec<Option<crate::runspec::Pin>>,
    legacy_providers: Vec<String>,
    role_sel: usize,
    editing_backup: bool,
    editing_model: bool,
    model_buffer: String,
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
    plan_scroll: u16,
    review_scroll: u16,
    chat_scroll: u16,
    /// When true, keep the live log pinned to the newest line as content grows.
    stream_follow: bool,
    bus_follow: bool,
    diff_follow: bool,
    chat_follow: bool,
    /// Last known max scroll offsets (from the most recent paint).
    stream_max: u16,
    bus_max: u16,
    diff_max: u16,
    plan_max: u16,
    review_max: u16,
    chat_max: u16,
    /// Log viewport height in rows (for PageUp/PageDown).
    stream_view_h: u16,
    bus_view_h: u16,
    diff_view_h: u16,
    plan_view_h: u16,
    review_view_h: u16,
    chat_view_h: u16,
    /// Chat composer input. Only active when `chat_composing` and Chat tab is focused.
    chat_input: String,
    chat_composing: bool,
    chat_conversations: std::collections::HashMap<String, String>,
    chat_watermarks: std::collections::HashMap<String, usize>,
    chat_turns: std::collections::HashMap<String, String>,
    chat_pending_proposal: Option<crate::orchestrator::Proposal>,
    chat_pending_brief_path: Option<std::path::PathBuf>,
    chat_active_turn: Option<std::sync::Arc<crate::orchestrator::TurnHandle>>,
    chat_latest_stats: std::collections::HashMap<String, crate::process::StreamStats>,
    chat_accum_stats: std::collections::HashMap<String, crate::process::StreamStats>,
    chat_last_preserved_worktree: Option<std::path::PathBuf>,
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
    /// it shows a static glyph when idle instead of a frame frozen mid-spin, and
    /// selects the frame clock (`FRAME_ANIMATING` vs `FRAME_IDLE`).
    animated: bool,
    /// Whether the terminal window has keyboard focus, per DECSET 1004. A window
    /// you are not looking at animates nothing (U30). Starts `true` so a terminal
    /// that never reports focus behaves as it always did.
    focused: bool,
    /// The one motion time origin. Every animation derives its position from this
    /// instant, so effects sharing a period stay in phase (U10).
    clock: crate::motion::Clock,
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
    /// Which population `prev_attention` was built from — Home's cross-project rows or
    /// a project's `snap.runs`. Home and a project alternate as the operator navigates;
    /// a run absent from the *other* population's baseline is not a transition, just a
    /// population swap, so a swap re-primes silently instead of toasting every run the
    /// new population happens to already have at Gate/Broken (round-9 review).
    prev_attention_home: Option<bool>,
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
    /// Wall-clock start of the give-up budget (`HOME_TARGET_GIVE_UP`) once a snapshot
    /// of the target's own project has been seen without finding it — the target's
    /// project may still be one refresh behind (R2), but a run that never reappears
    /// (archived, dir removed, folded into a different loudest leg) must eventually
    /// release the pin rather than leave Main/Agents permanently empty (round-7
    /// review finding: a ghost target used to stick forever).
    home_target_since: Option<Instant>,
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
    /// Records explicitly toggled away from whatever the current base fold state is
    /// (Space), keyed by immutable source identity (correction #6) — never an
    /// index, so a rebuilt snapshot that inserts a record ahead of these does not
    /// shuffle which ones are open. `A` shifts the base every record starts from
    /// via [`App::fold_all`] rather than mutating this set, so a second `A`
    /// restores whatever the operator had individually chosen, and `Space` keeps
    /// working (as an override) no matter which base `A` has selected.
    fold_open: std::collections::HashSet<crate::record::SourceId>,
    /// `A`: shift every record's base fold state to expanded; `Space` still XORs
    /// against that base, so an individually re-folded record while `A` is engaged
    /// actually stays folded (round-11 review, AC-6) instead of being silently
    /// forced back open.
    fold_all: bool,
    /// The record under the cursor (`J`/`K`/`t`/`T`/`e`/`E`/`}`/`{`), resolved to a
    /// row each frame by identity, the same pattern `resync_home_selection` uses.
    record_cursor: Option<crate::record::SourceId>,
    /// Set whenever a structural-navigation key (J/K/t/T/e/E/}/{) moves
    /// `record_cursor`; consumed by `render_record_view`, which snaps the active
    /// tab's scroll to bring the cursor into view exactly once, then clears it
    /// (AC-13) — never on every frame, or a deliberate manual scroll away from a
    /// stationary cursor would be fought back into place.
    record_cursor_dirty: bool,
    /// `R`: fall back to the byte-for-byte raw view (`render_scrollable_log`) on a
    /// tab with exactly one raw source (Log, Diff). Unavailable elsewhere (U36/AC-14).
    raw_mode: bool,
    /// The Log tab's *parsed*-mode scroll/follow/max, kept apart from
    /// `stream_scroll`/`stream_follow`/`stream_max` (which now belong to raw mode
    /// only, alongside the no-run overview) so `R` round-trips without clobbering
    /// either view's position (AC-14).
    stream_parsed_scroll: u16,
    stream_parsed_follow: bool,
    stream_parsed_max: u16,
    /// The Diff tab's *parsed*-mode scroll/max, kept apart from `diff_scroll`/
    /// `diff_max` (raw mode and the no-records fallback) for the same reason.
    diff_parsed_scroll: u16,
    diff_parsed_max: u16,
    /// Set each time `draw_diff_body` runs: whether that paint used the raw
    /// fields (`raw_mode`, or no parsed records to show) or the parsed ones.
    /// Scroll/Home/End key handling reads this rather than re-deriving it, so it
    /// can never disagree with what was actually drawn.
    diff_raw_active: bool,
    /// `f` on Activity: narrow to one slot's rows (toggle). Log stays the
    /// selected-slot view already, so filtering only ever applies to Activity
    /// (correction #7).
    activity_slot_filter: Option<usize>,
    rail_motion: RailMotion,
    tab_strip: TabStripMotion,
    selected_run_key: Option<String>,
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
    /// The absolute byte offset `text` begins at (`TailLog::start`), kept so the
    /// record parser can bisect the time index against the same tail (U33).
    start: u64,
}

impl LogCache {
    fn empty() -> Self {
        Self {
            path: None,
            len: 0,
            mtime: None,
            text: String::new(),
            truncated: false,
            start: 0,
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
            self.start = tail.start;
        }
        (&self.text, self.truncated)
    }

    fn clear(&mut self) {
        self.path = None;
        self.len = 0;
        self.mtime = None;
        self.text.clear();
        self.truncated = false;
        self.start = 0;
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
            plan_scroll: 0,
            review_scroll: 0,
            chat_scroll: 0,
            // Default: follow live output (newest lines).
            stream_follow: true,
            bus_follow: true,
            diff_follow: false,
            chat_follow: true,
            stream_max: 0,
            bus_max: 0,
            diff_max: 0,
            plan_max: 0,
            review_max: 0,
            chat_max: 0,
            stream_view_h: 12,
            bus_view_h: 12,
            diff_view_h: 12,
            plan_view_h: 12,
            review_view_h: 12,
            chat_view_h: 12,
            chat_input: String::new(),
            chat_composing: false,
            chat_conversations: std::collections::HashMap::new(),
            chat_watermarks: std::collections::HashMap::new(),
            chat_turns: std::collections::HashMap::new(),
            chat_pending_proposal: None,
            chat_pending_brief_path: None,
            chat_active_turn: None,
            chat_latest_stats: std::collections::HashMap::new(),
            chat_accum_stats: std::collections::HashMap::new(),
            chat_last_preserved_worktree: None,
            tick: 0,
            flash: None,
            cfg,
            heartbeats: std::collections::HashMap::new(),
            log_expand: false,
            last_click: None,
            show_help: false,
            help_scroll: 0,
            animated: false,
            focused: true,
            clock: crate::motion::Clock::new(),
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
            prev_attention_home: None,
            rect_attention: Rect::default(),
            selected_home: 0,
            home_scope,
            home_watermark: read_watermark(&watermark_path()),
            home_key: None,
            home_target_run: None,
            home_target_since: None,
            new_run,
            new_run_gen,
            rect_new_run: Rect::default(),
            rect_new_run_roster: Vec::new(),
            fold_open: std::collections::HashSet::new(),
            fold_all: false,
            record_cursor: None,
            record_cursor_dirty: false,
            raw_mode: false,
            stream_parsed_scroll: 0,
            stream_parsed_follow: true,
            stream_parsed_max: 0,
            diff_parsed_scroll: 0,
            diff_parsed_max: 0,
            diff_raw_active: true,
            activity_slot_filter: None,
            rail_motion: RailMotion::new(),
            tab_strip: TabStripMotion::new(),
            selected_run_key: None,
        }
    }

    fn motion_in_flight(&self) -> bool {
        let now = Instant::now();
        self.rail_motion.is_animating(now) || self.tab_strip.is_animating(now)
    }

    fn settle_motion(&mut self) {
        self.rail_motion.settle();
        self.tab_strip.settle();
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
        if !self.animated {
            return "·";
        }
        let phase = self.clock.cycle(crate::motion::SPIN_PERIOD);
        crate::motion::frame(crate::motion::BRAILLE, phase).unwrap_or("·")
    }

    /// Brightness of a breathing gutter this frame, `0.0..1.0` (U10). One value
    /// for the whole frame, so every live rail on screen breathes together
    /// instead of each starting its own cycle when it appears.
    fn pulse(&self) -> f32 {
        if !self.animated {
            return 0.55;
        }
        crate::motion::breathe(self.clock.cycle(crate::motion::BREATHE_PERIOD))
    }

    /// The colour of a live block's gutter, `depth` rows below its head. The head
    /// breathes; each row below fades further toward the page, so a streaming
    /// block reads as having a direction (U30).
    fn gutter(&self, depth: usize) -> Color {
        let head = lerp(PULSE_LO, PULSE_HI, self.pulse());
        toward_bg(head, (depth as f32 * TRAIL_FALLOFF).min(1.0))
    }

    fn reset_stream_view(&mut self) {
        self.stream_scroll = 0;
        self.stream_follow = true;
        self.stream_parsed_scroll = 0;
        self.stream_parsed_follow = true;
        self.diff_scroll = 0;
        self.diff_follow = false;
        self.diff_parsed_scroll = 0;
    }

    fn reset_bus_view(&mut self) {
        self.bus_scroll = 0;
        self.bus_follow = true;
    }

    fn select_run(&mut self, idx: usize, runs: &[state::RunSummary]) {
        if runs.is_empty() {
            return;
        }
        self.selected_run = idx.min(runs.len() - 1);
        self.selected_run_key = runs.get(self.selected_run).map(run_row_key);
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
        self.selected_run_key = None;
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
        self.selected_run_key = None;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Rail;
    }

    fn open_projects_view(&mut self) {
        self.browse = BrowseLevel::Projects;
        self.selected_run = 0;
        self.selected_run_key = None;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Rail;
        // Explicit navigation away from a run-scoped level abandons any pending Home
        // `Enter` target (round-9 review): left set, it would either resolve against
        // whatever project is entered next (wrong run) or silently tick toward the
        // "run is gone" flash 25 snapshots later, out of a project the operator chose
        // on purpose.
        self.home_target_run = None;
        self.home_target_since = None;
    }

    /// Back to the landing view. `Projects`/`Runs`/`Agents` all pop here eventually.
    fn open_home(&mut self) {
        self.browse = BrowseLevel::Home;
        self.selected_slot = 0;
        self.reset_stream_view();
        self.reset_bus_view();
        self.focus = Focus::Rail;
        self.home_target_run = None;
        self.home_target_since = None;
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

    fn plan_page(&self) -> u16 {
        self.plan_view_h.saturating_sub(1).max(3)
    }

    fn review_page(&self) -> u16 {
        self.review_view_h.saturating_sub(1).max(3)
    }

    fn chat_page(&self) -> u16 {
        self.chat_view_h.saturating_sub(1).max(3)
    }

    /// `has_full` mirrors exactly what `draw_log_body` branches on: without a
    /// full run the overview always paints via the raw fields (AC-14 does not
    /// apply — there is nothing parsed to preserve), and with one, `raw_mode`
    /// picks which pair of fields this frame's paint actually used.
    fn scroll_stream_by(&mut self, delta: i32, has_full: bool) {
        if has_full && !self.raw_mode {
            apply_scroll_delta(
                &mut self.stream_parsed_scroll,
                &mut self.stream_parsed_follow,
                self.stream_parsed_max,
                delta,
            );
        } else {
            apply_scroll_delta(
                &mut self.stream_scroll,
                &mut self.stream_follow,
                self.stream_max,
                delta,
            );
        }
    }

    fn scroll_bus_by(&mut self, delta: i32) {
        apply_scroll_delta(
            &mut self.bus_scroll,
            &mut self.bus_follow,
            self.bus_max,
            delta,
        );
    }

    /// `diff_raw_active` is computed fresh from `self.raw_mode` and the current diff
    /// records, not read off `App::diff_raw_active` — that field is only updated by
    /// `draw_diff_body`'s paint, and the input loop drains a whole key burst before the
    /// next paint runs. Reading the stale field here meant `R` followed by a scroll key
    /// in the same burst scrolled the *previous* mode's viewport while the *new* mode's
    /// was what got painted (AC-14).
    fn scroll_diff_by(&mut self, delta: i32, diff_raw_active: bool) {
        if diff_raw_active {
            apply_scroll_delta(
                &mut self.diff_scroll,
                &mut self.diff_follow,
                self.diff_max,
                delta,
            );
        } else {
            let mut follow = false;
            apply_scroll_delta(
                &mut self.diff_parsed_scroll,
                &mut follow,
                self.diff_parsed_max,
                delta,
            );
        }
    }

    fn scroll_plan_by(&mut self, delta: i32) {
        let mut follow = false;
        apply_scroll_delta(&mut self.plan_scroll, &mut follow, self.plan_max, delta);
    }

    fn scroll_review_by(&mut self, delta: i32) {
        let mut follow = false;
        apply_scroll_delta(&mut self.review_scroll, &mut follow, self.review_max, delta);
    }

    fn scroll_chat_by(&mut self, delta: i32) {
        apply_scroll_delta(
            &mut self.chat_scroll,
            &mut self.chat_follow,
            self.chat_max,
            delta,
        );
    }

    /// Scroll whichever view Main is showing. The Shell tab is a live tmux client:
    /// it never scrolls from here (its input is forwarded raw). Without a run
    /// selected, Activity/Diff/Plan/Review fall back to the same overview body Log
    /// uses (`draw_log_body`), so scrolling must follow that body — `stream_*` —
    /// rather than the run-scoped state those tabs normally own.
    fn scroll_main_by(&mut self, delta: i32, has_full: bool, diff_records: &[Record]) {
        match self.main_tab {
            MainTab::Log => self.scroll_stream_by(delta, has_full),
            MainTab::Activity if has_full => self.scroll_bus_by(delta),
            MainTab::Activity => self.scroll_stream_by(delta, false),
            MainTab::Diff if has_full => {
                self.scroll_diff_by(delta, self.raw_mode || diff_records.is_empty())
            }
            MainTab::Diff => self.scroll_stream_by(delta, false),
            MainTab::Plan if has_full => self.scroll_plan_by(delta),
            MainTab::Plan => self.scroll_stream_by(delta, false),
            MainTab::Review if has_full => self.scroll_review_by(delta),
            MainTab::Review => self.scroll_stream_by(delta, false),
            MainTab::Chat => self.scroll_chat_by(delta),
            MainTab::Shell => {}
        }
    }

    fn main_page(&self, has_full: bool) -> u16 {
        match self.main_tab {
            MainTab::Activity if has_full => self.bus_page(),
            MainTab::Diff if has_full => self.diff_page(),
            MainTab::Plan if has_full => self.plan_page(),
            MainTab::Review if has_full => self.review_page(),
            MainTab::Chat => self.chat_page(),
            _ => self.stream_page(),
        }
    }

    fn home_for_main(&mut self, has_full: bool, diff_records: &[Record]) {
        let diff_raw_active = self.raw_mode || diff_records.is_empty();
        match self.main_tab {
            MainTab::Activity if has_full => {
                self.bus_follow = false;
                self.bus_scroll = 0;
            }
            MainTab::Diff if has_full && !diff_raw_active => {
                self.diff_parsed_scroll = 0;
            }
            MainTab::Diff if has_full => {
                self.diff_follow = false;
                self.diff_scroll = 0;
            }
            MainTab::Plan if has_full => {
                self.plan_scroll = 0;
            }
            MainTab::Review if has_full => {
                self.review_scroll = 0;
            }
            MainTab::Chat => {
                self.chat_follow = false;
                self.chat_scroll = 0;
            }
            MainTab::Log if has_full && !self.raw_mode => {
                self.stream_parsed_follow = false;
                self.stream_parsed_scroll = 0;
            }
            _ => {
                self.stream_follow = false;
                self.stream_scroll = 0;
            }
        }
    }

    fn end_for_main(&mut self, has_full: bool, diff_records: &[Record]) {
        let diff_raw_active = self.raw_mode || diff_records.is_empty();
        match self.main_tab {
            MainTab::Activity if has_full => {
                self.bus_follow = true;
                self.bus_scroll = self.bus_max;
            }
            MainTab::Diff if has_full && !diff_raw_active => {
                self.diff_parsed_scroll = self.diff_parsed_max;
            }
            MainTab::Diff if has_full => {
                self.diff_follow = true;
                self.diff_scroll = self.diff_max;
            }
            MainTab::Plan if has_full => {
                self.plan_scroll = self.plan_max;
            }
            MainTab::Review if has_full => {
                self.review_scroll = self.review_max;
            }
            MainTab::Chat => {
                self.chat_scroll = self.chat_max;
            }
            MainTab::Log if has_full && !self.raw_mode => {
                self.stream_parsed_follow = true;
                self.stream_parsed_scroll = self.stream_parsed_max;
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
        self.palette.is_some()
            || self.filter.is_some()
            || (self.chat_composing && self.main_tab == MainTab::Chat)
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
        workflow: Some(crate::runspec::SpecWorkflow::Plan),
        roles: vec![],
        arena_pool: vec![],
        legacy_providers: vec![],
        role_sel: 0,
        editing_backup: false,
        editing_model: false,
        model_buffer: String::new(),
    }
}

/// How often the background thread re-reads the run state from disk.
const REFRESH: Duration = Duration::from_millis(200);
/// Upper bound on how long the render thread sleeps while something on screen is
/// moving: ~60fps (U10). Nothing derives its *speed* from this — every animation
/// reads the wall clock through `motion` — so raising or lowering it changes only
/// how smooth the motion is.
const FRAME_ANIMATING: Duration = Duration::from_millis(16);
/// One full blink of a text cursor. Time-based like everything else, so it keeps
/// its rate across the frame-clock ramp instead of speeding up sixfold (U10).
const CURSOR_BLINK: Duration = Duration::from_millis(1060);
/// Upper bound on how long the render thread sleeps with nothing moving. Long
/// enough that an idle spar costs nothing, short enough that input still feels
/// immediate, since any input wakes the loop rather than waiting this out.
const FRAME_IDLE: Duration = Duration::from_millis(250);
/// How often Home repaints purely to age its wait/age columns forward when nothing
/// on disk moved. Home has no selected `full` run, so `animating()` never fires for
/// it; without this a static Home (only gates/broken/finished rows) freezes its
/// `relative_wait`/`relative_age` labels at whatever they read at the last real
/// rebuild instead of counting up (round-11 review finding).
const HOME_CLOCK_TICK: Duration = Duration::from_secs(5);

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
    chat_conversation: Option<String>,
}

/// An immutable view of the world, produced off-thread and rendered as-is.
struct Snapshot {
    swarm: SparPaths,
    /// The browse level this snapshot was actually built for. `runs` is only
    /// populated `if sel.browse.in_project()` (`build_snapshot`), so a Home-level
    /// snapshot whose `swarm.project_root` happens to already equal the browsing
    /// root carries no real per-project scan — root equality alone cannot tell
    /// that apart from a genuine target-project snapshot (round-2 review, major).
    browse: BrowseLevel,
    projects: Vec<registry::ProjectEntry>,
    runs: Vec<state::RunSummary>,
    full: Option<RunState>,
    stream_text: String,
    /// Exactly the bytes `stream_content` tailed from disk, with none of that
    /// function's cosmetic truncation banner or waiting-for-stream placeholder
    /// (AC-14): `R`'s raw view must never show text that was never persisted.
    stream_text_raw: String,
    /// Structured records for the Log tab (U33/AC-1), stamped from the sidecar
    /// byte-offset -> time index when one exists.
    log_records: Vec<Record>,
    activity: Vec<Record>,
    /// Main's Diff tab: the run's plan/artifacts, or a placeholder.
    diff_text: String,
    diff_records: Vec<Record>,
    plan_docs: Vec<Record>,
    review: Vec<Record>,
    chat: Vec<Record>,
    /// Unresolved `@human`/`Blocked` alerts for the selected run (status-line badge count).
    human_alerts: usize,
    /// Selected run is in flight with no live orchestrator.
    abandoned: bool,
    /// Freshest process heartbeat per slot id for the selected run.
    heartbeats: std::collections::HashMap<String, DateTime<Utc>>,
    /// Home's bands and the per-project roll-up, built off-thread (U13/B).
    home: HomeData,
    /// The selected slot's log stats (U13): `StreamStats::load` reads a file, so it
    /// is computed here rather than in `draw_log_body`.
    log_stats: Option<process::StreamStats>,
}

impl Snapshot {
    /// The frame painted before the refresher thread's first real build lands.
    /// Landing on Home means the first snapshot is cross-project (U13/B); building it
    /// synchronously on the UI thread blocks the very first paint on a scan across
    /// every registered project's run directory, which is the "thousands of run
    /// dirs" scale the IA doc names as the thing that already bit once (round-11
    /// review finding). `SparPaths::new` only joins paths, so this touches no disk.
    ///
    /// `home.rows` is seeded with `build_home_rows` over an empty project/run
    /// listing rather than left empty: that is a pure function over no data, so it
    /// still touches no disk, but it means the four band headers and the `n` CTA are
    /// present on frame one instead of Home looking blank for a `REFRESH` tick
    /// (round-11 review, minor).
    fn loading(root: &Path) -> Self {
        let now = Utc::now();
        Self {
            swarm: SparPaths::new(root),
            browse: BrowseLevel::Home,
            projects: Vec::new(),
            runs: Vec::new(),
            full: None,
            stream_text: String::new(),
            stream_text_raw: String::new(),
            log_records: Vec::new(),
            activity: Vec::new(),
            diff_text: String::new(),
            diff_records: Vec::new(),
            plan_docs: Vec::new(),
            review: Vec::new(),
            chat: Vec::new(),
            human_alerts: 0,
            abandoned: false,
            heartbeats: std::collections::HashMap::new(),
            home: HomeData {
                rows: build_home_rows(&[], &[], &HomeScope::All, now, now, true),
                project_stats: Vec::new(),
                loading: true,
            },
            log_stats: None,
        }
    }
}

#[allow(clippy::large_enum_variant)]
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
    ChatTurnDone,
    ChatTurnResult {
        conv: String,
        stats: Option<crate::process::StreamStats>,
        worktree: Option<std::path::PathBuf>,
        error: Option<String>,
    },
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
            let mut r = group.pop().expect("len 1");
            r.unit_id = Some(root.clone());
            members.insert(r.id.clone(), vec![r.id.clone()]);
            out.push(r);
            continue;
        }
        // Loudest attention first (U28): a working leg must outrank an idle one so the
        // row's id and phase, which gate buttons, `:approve` and drill-down all act on,
        // never come from a leg that isn't actually holding the state. Reordering by
        // `wants_operator` first (instead of as a tiebreak) sank a `PlanApproved` unit
        // with a live child below every working run and retargeted those actions onto
        // the idle leg — see U28. `wants_operator` only breaks a tie *within* one
        // attention level: an idle `PlanApproved` leg and an idle `Done` leg sort
        // equally on attention alone, and picking the `Done` one as representative
        // would leave a NEEDS YOU row with nothing to act on. Attention can never tie
        // across a Working leg, since every active phase scores `Working`, so this
        // tiebreak can only ever reorder among idle (or among gate, or among broken)
        // legs — it never overrides the active leg attention already selected.
        group.sort_by(|a, b| {
            run_attention(b)
                .cmp(&run_attention(a))
                .then_with(|| wants_operator(b).cmp(&wants_operator(a)))
                .then(b.updated_at.cmp(&a.updated_at))
        });
        let ids: Vec<String> = group.iter().map(|r| r.id.clone()).collect();
        // Every leg that wants the operator still counts, so two gates folded into one
        // row read as two in the roll-up rather than one. But `PlanApproved` (U28) is a
        // handoff only while nothing in the unit is running: once a sibling leg has
        // taken it up, the approval is stale bookkeeping, not a second thing waiting on
        // the operator, so it must not inflate `wants` (and, via `unit_wants_operator`,
        // must not raise a NEEDS YOU nobody can act on for the unit's live leg).
        // Transcribed straight from U28's two rules rather than gated off
        // `wants_operator` (round-2 review, minor): `wants_operator`'s own
        // `PlanApproved` branch fires on phase alone, so filtering on
        // `phase == PlanApproved` first would drop an *abandoned* PlanApproved leg
        // too — U28 says `Gate`/`Broken` want the operator unconditionally, and this
        // form can never suppress one even if `PlanApproved` and `abandoned` ever
        // become reachable together.
        // An abandoned leg is not actually running (no live orchestrator, see
        // `state.rs`'s `abandoned` check) even though its phase reads as active, so it
        // must not suppress a sibling `PlanApproved` leg's handoff — that approval is
        // live again, not stale bookkeeping (round-2 review, minor).
        let unit_has_active_leg = group
            .iter()
            .any(|r| is_active_phase(r.phase) && !r.abandoned);
        let wants = group
            .iter()
            .filter(|r| {
                run_attention(r).needs_you()
                    || (r.phase == Phase::PlanApproved && !unit_has_active_leg)
            })
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
        row.unit_id = Some(root.clone());
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
                false,
            )
        } else {
            Vec::new()
        };
        HomeData {
            rows,
            project_stats,
            loading: false,
        }
    } else {
        HomeData::default()
    };
    let stream_text_raw = if sel.browse.in_project() {
        stream_raw_content(&swarm, full.as_ref(), sel.slot_idx, cache)
    } else {
        String::new()
    };
    let stream_text = if sel.browse.in_project() {
        stream_content(&swarm, full.as_ref(), sel.slot_idx, cache, !runs.is_empty())
    } else if sel.browse == BrowseLevel::Home {
        // `draw_home_body` renders Home's body straight from `HomeData` with a
        // freshly read clock (so the wait column doesn't freeze between snapshot
        // rebuilds); a `home_overview` built here off a snapshot-time clock would
        // never be read.
        cache.clear();
        String::new()
    } else {
        cache.clear();
        project_overview(&projects, sel.project_idx)
    };
    let shortener = path_shortener_for(&swarm, full.as_ref());
    let log_records = full
        .as_ref()
        .and_then(|st| {
            st.slots
                .get(sel.slot_idx.min(st.slots.len().saturating_sub(1)))
                .map(|slot| (st, slot))
        })
        .map(|(st, slot)| {
            let path = slot
                .log_path
                .clone()
                .unwrap_or_else(|| swarm.log_file(&st.id, &slot.id));
            if !path.is_file() {
                return Vec::new();
            }
            let (raw, truncated) = cache.load(&path, LOG_TAIL_BYTES);
            let raw = raw.to_string();
            let start = cache.start;
            // `from` is 0, not `start`: `time_at` resolves an offset to the index
            // entry at or immediately *before* it, so the entry covering the first
            // retained byte is by construction `< start` and must not be filtered
            // out here, or every record in the first retained append has no time
            // (AC-11) — the ordinary state of any log past `LOG_TAIL_BYTES`.
            let index =
                process::read_log_index(&path, 0, start + raw.len() as u64).unwrap_or_default();
            let mut records: Vec<Record> =
                record::parse_log_records(&raw, start, &index, &shortener, &st.id, &slot.id)
                    .iter()
                    .map(|lr| lr.to_record())
                    .collect();
            // The parsed path never said anything about a truncated tail before
            // this (round-9 finding 5) — `stream_content`'s raw banner said so,
            // but the record view read `log_records`, not that string.
            if truncated {
                records.insert(
                    0,
                    record::truncated_log_notice(&st.id, &slot.id, LOG_TAIL_BYTES / 1024),
                );
            }
            records
        })
        .unwrap_or_default();
    let diff_text = diff_content(full.as_ref(), sel.slot_idx);
    let diff_records = full
        .as_ref()
        .and_then(|st| {
            st.worktrees
                .iter()
                .find(|w| st.slots.get(sel.slot_idx).map(|s| &s.id) == Some(&w.slot_id))
                .map(|w| (st, w))
        })
        .map(|(st, w)| {
            apply_diff_watermark(
                &st.id,
                &w.slot_id,
                st.base_commit.as_deref(),
                record::parse_diff(&diff_text, &w.slot_id),
            )
        })
        .unwrap_or_default();
    let plan_docs_v = plan_docs(&swarm, full.as_ref());
    // O27: the Review projection evaluates the *run's own frozen* config, never the
    // live `spar.toml` the TUI happened to start with — otherwise a browsed run's
    // displayed blockers can disagree with what the gate actually enforced for it.
    // A run's frozen config is required to show the gate's own blockers (O27,
    // AC-17): falling back to the TUI's live `spar.toml` here would let the
    // Review tab silently show a *different* set of blockers than the one the
    // ship gate actually evaluated for this run. Fail closed instead — `None`
    // tells `review_records` to say so rather than guess.
    let run_cfg = full.as_ref().and_then(|st| {
        let snap = swarm.run_config_file(&st.id);
        if snap.is_file() {
            Config::for_run(&swarm, &st.id).ok()
        } else {
            None
        }
    });
    let review_v = review_records(&swarm, full.as_ref(), run_cfg.as_ref());
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
    let log_stats = full
        .as_ref()
        .and_then(|st| st.slots.get(sel.slot_idx))
        .and_then(|s| {
            s.log_path
                .as_ref()
                .and_then(|p| process::StreamStats::load(p))
                .or_else(|| {
                    s.usage.as_ref().map(|u| process::StreamStats {
                        tools: u.tools,
                        tool_errors: 0,
                        quota_rejected: None,
                        quota_resets_at: None,
                        quota_recovered: false,
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_read_tokens: u.cache_read_tokens,
                        cache_write_tokens: 0,
                        context_tokens: u.context_tokens,
                        billed_tokens: u.billed_tokens,
                        model: u.model.clone(),
                        session_id: None,
                        session_id_recovery_stash: None,
                        lines_in: 0,
                        chars_out: 0,
                        last_log_at: None,
                        cost_usd: u.cost_usd,
                        subagent_stats: u.subagent_stats.clone(),
                        model_usage: u.model_usage.clone(),
                    })
                })
        });
    let chat_v = {
        let scope = full
            .as_ref()
            .map(|st| crate::orchestrator::Scope::Run(st.id.clone()))
            .unwrap_or(crate::orchestrator::Scope::Home);
        let conv = sel.chat_conversation.as_deref();
        crate::orchestrator::transcript(&swarm, &scope, conv).unwrap_or_default()
    };
    Snapshot {
        swarm,
        browse: sel.browse,
        projects,
        runs,
        full,
        stream_text,
        stream_text_raw,
        log_records,
        activity,
        diff_text,
        diff_records,
        plan_docs: plan_docs_v,
        review: review_v,
        chat: chat_v,
        human_alerts: alerts.len(),
        abandoned,
        heartbeats,
        home,
        log_stats,
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
    if runs.is_empty() {
        rows.push(HomeRow::Empty(band));
        return;
    }
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

/// The identity Home shows and drills into: `unit_id` when folded (stable across a
/// loudest-leg swap), else `id`. Never `id` alone — it follows whichever leg
/// `fold_units` currently picks as loudest and can visibly change between snapshots,
/// which used to make a Home run row change its displayed identity out from under
/// the operator (round-11 review, major; AC-28).
fn home_display_id(run: &state::RunSummary) -> &str {
    run.unit_id.as_deref().unwrap_or(&run.id)
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
    loading: bool,
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
            // `unit_wants_operator` folds `run_attention`'s Gate/Broken together with
            // the PlanApproved handoff AND accounts for a folded row whose active leg
            // is not the one waiting, so this band agrees with `runs_needing_attention`
            // and the rail flag rather than drifting from them (AC-18; round-9 review,
            // and the round-12 roll-up disagreement).
            if unit_wants_operator(r) {
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

    // Band 1: longest wait first, then Gate above Broken (above the PlanApproved
    // handoff, which carries no `Attention` of its own) as the tiebreak on equal wait.
    needs_me.sort_by(|a, b| {
        home_wait(b.updated_at, now)
            .cmp(&home_wait(a.updated_at, now))
            .then_with(|| run_attention(b).cmp(&run_attention(a)))
    });
    running.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    finished.sort_by_key(|r| std::cmp::Reverse(r.updated_at));

    let mut rows = Vec::new();
    rows.push(HomeRow::Header(HomeBand::StartNew));
    rows.push(HomeRow::NewRun);
    for (i, proj) in projects.iter().enumerate() {
        if let HomeScope::Project(root) = scope {
            if &proj.root != root {
                continue;
            }
        }
        rows.push(HomeRow::Project(i, proj.root.clone()));
    }
    rows.push(HomeRow::Header(HomeBand::NeedsMe));
    if loading {
        for slot in 0..HOME_SKELETON_ROWS {
            rows.push(HomeRow::Skeleton {
                band: HomeBand::NeedsMe,
                slot,
            });
        }
    } else {
        push_home_band(&mut rows, HomeBand::NeedsMe, &needs_me, now, None);
    }
    rows.push(HomeRow::Header(HomeBand::Running));
    if loading {
        for slot in 0..HOME_SKELETON_ROWS {
            rows.push(HomeRow::Skeleton {
                band: HomeBand::Running,
                slot,
            });
        }
    } else {
        push_home_band(
            &mut rows,
            HomeBand::Running,
            &running,
            now,
            Some(HOME_BAND_CAP),
        );
    }
    rows.push(HomeRow::Header(HomeBand::Finished));
    if loading {
        for slot in 0..HOME_SKELETON_ROWS {
            rows.push(HomeRow::Skeleton {
                band: HomeBand::Finished,
                slot,
            });
        }
    } else {
        push_home_band(
            &mut rows,
            HomeBand::Finished,
            &finished,
            now,
            Some(HOME_BAND_CAP),
        );
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
/// so on its own line (U14's reserved-space rule applied to Home). `now` is read
/// fresh at render time rather than trusting each row's stored `waited` (which is
/// only as fresh as the last snapshot rebuild, and a gated run with nothing else
/// changing can go a long time between rebuilds) — a review finding.
/// Main's body at Home: the **detail** of whatever the rail is sitting on.
///
/// This replaces a re-render of the rail's own list. Main is `f(rail selection)`
/// at every other level (X2, U1) and Home was the one exception, which cost the
/// whole right-hand pane to say a second time what the rail had already said. The
/// list is the rail's job; saying what one row *is* is Main's.
fn home_detail(
    rows: &[HomeRow],
    projects: &[registry::ProjectEntry],
    sel: usize,
    scope: &HomeScope,
    watermark: DateTime<Utc>,
    now: DateTime<Utc>,
    width: u16,
) -> String {
    // Two columns of padding, and never narrower than something can be read in.
    let wrap_w = width.saturating_sub(4).max(20) as usize;
    let Some(row) = rows.get(sel) else {
        return format!("\n  Home · {}\n{}", home_scope_label(scope), HOME_ACTIONS);
    };
    let body = match row {
        HomeRow::Header(band) => {
            let n = home_band_count(rows, *band);
            let mut out = format!("\n  {}\n\n", home_band_label(*band));
            let what = match band {
                HomeBand::NeedsMe => {
                    "Runs stopped at a gate, broken, or abandoned. Ranked by how long \
                     they have been waiting, longest first. Nothing here moves until \
                     you move it."
                }
                HomeBand::Running => {
                    "Runs with work in flight. They need nothing from you; the rail's \
                     bar breathes while they are moving."
                }
                HomeBand::Finished => {
                    "Runs that reached a terminal phase since your last look. The \
                     watermark advances when you quit, so this band empties itself."
                }
                HomeBand::StartNew => {
                    "Compose a new run, or change which projects Home is looking at."
                }
            };
            out.push_str(&wrap_indent(what, wrap_w, "  "));
            out.push_str(&format!("\n\n  {n} row(s) in this band.\n"));
            if *band == HomeBand::Finished {
                out.push_str(&format!("  Last look: {} ago.\n", relative_age(watermark)));
            }
            out
        }
        HomeRow::Run { run, .. } => {
            let mut out = String::from("\n");
            out.push_str(&format!(
                "  {} · {}\n",
                run.project_name.as_deref().unwrap_or("?"),
                home_display_id(run),
            ));
            out.push_str(&format!("  {}\n\n", rail_phase(run.phase)));
            match run.task.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                Some(task) => {
                    out.push_str(&wrap_indent(task, wrap_w, "  "));
                    out.push('\n');
                }
                // A run with no brief is a real state (a bare `spar review`), not a
                // rendering gap: say so rather than leaving Main blank.
                None => out.push_str("  (no brief on this run)\n"),
            }
            out.push('\n');
            let waited = relative_wait(home_wait(run.updated_at, now));
            out.push_str(&format!("  waited     {waited}\n"));
            // Every variant is one word, so the Debug name is the spelling the
            // operator types on the CLI.
            out.push_str(&format!(
                "  workflow   {}\n",
                format!("{:?}", run.workflow).to_lowercase()
            ));
            out.push_str(&format!("  round      {}\n", run.round));
            if run.wants > 1 {
                out.push_str(&format!(
                    "  legs       {} of this unit want you\n",
                    run.wants
                ));
            }
            if run.dry_run {
                out.push_str("  dry-run    yes\n");
            }
            if run.abandoned {
                out.push_str("  abandoned  no live orchestrator owns this run\n");
            }
            out.push_str("\n  Enter opens it.\n");
            out
        }
        HomeRow::Empty(band) => format!(
            "\n  {}\n\n  {}.\n",
            home_band_label(*band),
            home_band_empty_text(*band),
        ),
        HomeRow::More { band, n } => format!(
            "\n  {}\n\n  {n} more row(s) than this band shows.\n\n  \
             Enter opens the project to see all of them.\n",
            home_band_label(*band),
        ),
        HomeRow::NewRun => String::from(
            "\n  START SOMETHING NEW\n\n  \
             Compose a run: a brief, a workflow, and a fleet.\n",
        ),
        HomeRow::Project(i, root) => {
            let name = projects
                .get(*i)
                .and_then(|p| p.name.as_deref())
                .unwrap_or_else(|| root.file_name().and_then(|s| s.to_str()).unwrap_or("?"));
            format!(
                "\n  {name}\n\n  {}\n\n  Enter scopes Home to this project.\n",
                root.display(),
            )
        }
        HomeRow::Skeleton { band, .. } => format!(
            "\n  {}\n\n  scanning {}…\n",
            home_band_label(*band),
            home_band_label(*band).to_lowercase()
        ),
    };
    format!("{body}{HOME_ACTIONS}")
}

/// The standing actions, appended to every Home detail. Main is the whole screen
/// at phone width, so this is the only CTA there is; it is a footer rather than a
/// row so it cannot be scrolled away from or selected past.
const HOME_ACTIONS: &str = "\n  ─────\n  n start something new · p projects · a next alert\n";

/// Wrap `text` to `w` columns, prefixing every line with `indent`. Word-wrapping
/// only: a token longer than the width goes on its own line rather than being cut,
/// because the tokens that get long here are run ids and paths.
fn wrap_indent(text: &str, w: usize, indent: &str) -> String {
    let mut out = String::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= w {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push_str(indent);
                out.push_str(&line);
                out.push('\n');
                line.clear();
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            out.push_str(indent);
            out.push_str(&line);
            out.push('\n');
        }
    }
    // The caller decides the trailing blank line.
    while out.ends_with('\n') {
        out.pop();
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

fn run_row_key(run: &state::RunSummary) -> String {
    format!("run:{}", run.unit_id.as_deref().unwrap_or(&run.id))
}

fn rail_keys(snap: &Snapshot, browse: BrowseLevel) -> Vec<String> {
    match browse {
        BrowseLevel::Home => snap.home.rows.iter().map(home_row_key).collect(),
        BrowseLevel::Runs => snap.runs.iter().map(run_row_key).collect(),
        _ => Vec::new(),
    }
}

/// Identity of a Home row, for `resync_home_selection` to follow the cursor across a
/// re-ranked snapshot instead of a position (R3).
fn home_row_key(row: &HomeRow) -> String {
    match row {
        HomeRow::Header(b) => format!("hdr:{b:?}"),
        // `run.id` follows whichever leg `fold_units` currently judges loudest, which
        // can change between snapshots. `unit_id` is the fold root and does not move
        // (AC-28); a row that was never folded has no `unit_id`, so `id` is stable.
        // Home is cross-project by definition, and folding is per project (round-11
        // review, AC-34), so the project root is part of the identity too — without
        // it, two projects whose 8-hex ids happen to collide would glue the cursor to
        // the wrong project's row (round-11 review, minor).
        HomeRow::Run { run, .. } => {
            format!(
                "run:{}:{}",
                run.project_root
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                run.unit_id.as_deref().unwrap_or(&run.id)
            )
        }
        HomeRow::More { band, .. } => format!("more:{band:?}"),
        HomeRow::Empty(b) => format!("empty:{b:?}"),
        HomeRow::Project(_, root) => format!("proj:{}", root.display()),
        HomeRow::NewRun => "newrun".to_string(),
        HomeRow::Skeleton { band, slot } => format!("skel:{band:?}:{slot}"),
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
    let unselectable = |r: &HomeRow| {
        matches!(
            r,
            HomeRow::Header(_)
                | HomeRow::More { .. }
                | HomeRow::Empty(_)
                | HomeRow::Skeleton { .. }
        )
    };
    if unselectable(&rows[i]) {
        if let Some(f) = (i..rows.len()).find(|&j| !unselectable(&rows[j])) {
            i = f;
        } else if let Some(b) = (0..i).rev().find(|&j| !unselectable(&rows[j])) {
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

/// The Diff tab's "since you last looked" watermark (005 C/AC-18): per-run, per-file
/// content hashes as of the last time the operator actually looked at the Diff tab.
/// Cross-project state, so it lives at `spar_home()` like the Home watermark (U19),
/// keyed by run id rather than a single timestamp since "changed" is per-file.
fn diff_watermark_path() -> PathBuf {
    registry::spar_home().join("diff_watermark.json")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct DiffWatermarkFile {
    #[serde(default)]
    runs: std::collections::HashMap<String, RunDiffWatermark>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RunDiffWatermark {
    at: DateTime<Utc>,
    #[serde(default)]
    files: std::collections::HashMap<String, u64>,
}

/// A missing or corrupt file reads as "never looked" (empty `runs`) — every file in
/// the current diff will then read as new (AC-18: "never fewer").
fn read_diff_watermark(path: &Path) -> DiffWatermarkFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_diff_watermark(path: &Path, file: &DiffWatermarkFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(file)?;
    std::fs::write(path, text)?;
    Ok(())
}

fn hash_diff_body(body: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for l in body {
        l.hash(&mut h);
    }
    h.finish()
}

/// The diff records shown for a run are always the *selected slot's* worktree
/// (`diff_records` in `build_snapshot`), so the watermark must be scoped the same
/// way: keying on `run_id` alone let slot A's "seen" mark suppress `NEW` on the
/// same path in slot B's worktree under one run (round-review minor finding).
/// `base_commit` folds in as well (AC-18): a worktree recreated or rebased onto a
/// different base is a different diff identity even under the same run/slot id,
/// so its watermark key must differ too — an *absent* prior key reads as "never
/// looked" (AC-18's "only ever mark more"), never as an obsolete match.
fn diff_watermark_key(run_id: &str, slot_id: &str, base_commit: Option<&str>) -> String {
    format!("{run_id}::{slot_id}::{}", base_commit.unwrap_or("none"))
}

/// Records the diff records' current per-file hashes as "seen" for `run_id`'s
/// selected `slot_id` at `base_commit`. Called when the operator actually leaves
/// the Diff tab or quits (`handle_key`/`handle_mouse`), never on every render —
/// that would mark everything seen before it was ever shown as new.
fn mark_diff_seen(run_id: &str, slot_id: &str, base_commit: Option<&str>, records: &[Record]) {
    mark_diff_seen_at(
        &diff_watermark_path(),
        run_id,
        slot_id,
        base_commit,
        records,
    );
}

fn mark_diff_seen_at(
    path: &Path,
    run_id: &str,
    slot_id: &str,
    base_commit: Option<&str>,
    records: &[Record],
) {
    let mut files = std::collections::HashMap::new();
    for r in records {
        if matches!(r.kind, RecordKind::FileDiff) {
            files.insert(r.head.clone(), hash_diff_body(&r.body));
        }
    }
    if files.is_empty() {
        return;
    }
    let mut file = read_diff_watermark(path);
    file.runs.insert(
        diff_watermark_key(run_id, slot_id, base_commit),
        RunDiffWatermark {
            at: Utc::now(),
            files,
        },
    );
    let _ = write_diff_watermark(path, &file);
}

/// Marks each `FileDiff` record whose content hash differs from the stored
/// watermark (or every one, if this run/slot/base was never looked at before), and
/// prepends a banner Section when there is a previous look to compare against and
/// at least one file changed since it.
fn apply_diff_watermark(
    run_id: &str,
    slot_id: &str,
    base_commit: Option<&str>,
    records: Vec<Record>,
) -> Vec<Record> {
    apply_diff_watermark_at(
        &diff_watermark_path(),
        run_id,
        slot_id,
        base_commit,
        records,
    )
}

fn apply_diff_watermark_at(
    path: &Path,
    run_id: &str,
    slot_id: &str,
    base_commit: Option<&str>,
    mut records: Vec<Record>,
) -> Vec<Record> {
    let file = read_diff_watermark(path);
    let key = diff_watermark_key(run_id, slot_id, base_commit);
    let prev = file.runs.get(&key);
    let mut changed = 0usize;
    for r in &mut records {
        if !matches!(r.kind, RecordKind::FileDiff) {
            continue;
        }
        let hash = hash_diff_body(&r.body);
        let is_new = prev
            .map(|rw| rw.files.get(&r.head).map(|h| *h != hash).unwrap_or(true))
            .unwrap_or(true);
        if is_new {
            changed += 1;
            r.summary = format!("NEW · {}", r.summary);
        }
    }
    if let Some(rw) = prev {
        if changed > 0 {
            records.insert(
                0,
                Record {
                    kind: RecordKind::Section,
                    glyph: "§",
                    verb: "Diff".to_string(),
                    head: "since you last looked".to_string(),
                    summary: format!(
                        "{changed} file{} changed · {}",
                        if changed == 1 { "" } else { "s" },
                        relative_age(rw.at)
                    ),
                    body: Vec::new(),
                    time: Some(rw.at),
                    elapsed: None,
                    actor: None,
                    ok: None,
                    source: SourceId::Diff {
                        worktree: run_id.to_string(),
                        path: "__watermark__".to_string(),
                    },
                    folded_by_default: false,
                    has_command_row: false,
                },
            );
        }
    }
    records
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
            if crate::providers::api_provider_supported(&r.name) {
                (true, None)
            } else {
                (
                    false,
                    Some(format!("unsupported api provider '{}'", r.name)),
                )
            }
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
        if !avail {
            continue;
        }
        if configured_native.iter().any(|n| n == name) {
            continue;
        }
        roster.push(RosterEntry {
            choice: RosterChoice::Provider(format!("cli:{name}")),
            label: format!("cli:{name}"),
            available: true,
            reason: None,
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
        Ok(r) if r.is_api() => crate::providers::api_provider_supported(&r.name),
        Ok(r) => detected.iter().any(|(n, avail)| *n == r.name && *avail),
        Err(_) => false,
    }
}

/// Expand `picked` roster indices into provider refs, in pick order, deduplicated
/// keeping the first occurrence — a `Fleet` choice can repeat a provider a `Provider`
/// choice already picked. Dedup keys on `storage_key()` (X8: model-free), not the raw
/// string, or `cli:claude` (detected) and `cli:claude@opus` (configured) pick as two
/// dispatches of the same CLI (round-9 review).
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
    out.retain(|p| {
        let key = crate::provider_ref::ProviderRef::parse(p)
            .map(|r| r.storage_key())
            .unwrap_or_else(|_| p.clone());
        seen.insert(key)
    });
    out
}

fn new_run_spec(nr: &NewRun, cfg: &crate::config::Config) -> crate::runspec::RunSpec {
    let wf = nr.workflow;
    let mut spec = crate::runspec::RunSpec {
        task: nr.task.clone(),
        workflow: wf,
        ..Default::default()
    };
    spec.legacy_providers = nr.legacy_providers.clone();
    if let Some(wf) = wf {
        if wf == crate::runspec::SpecWorkflow::Arena {
            if !nr.arena_pool.is_empty() {
                spec.arena_pool = nr.arena_pool.clone();
                let expected = crate::runspec::spec_rows(wf, cfg).len();
                if spec.arena_pool.len() != expected {
                    spec.arena_pool.resize_with(expected, || None);
                }
            } else {
                let providers = new_run_providers(nr);
                let expected = crate::runspec::spec_rows(wf, cfg).len();
                let mut pool: Vec<Option<crate::runspec::Pin>> = Vec::new();
                for (i, raw) in providers.iter().enumerate().take(expected) {
                    if let Ok(pin) = crate::runspec::Pin::parse(raw) {
                        if pool.len() <= i {
                            pool.resize_with(i + 1, || None);
                        }
                        pool[i] = Some(pin);
                    }
                }
                while pool.len() < expected {
                    pool.push(None);
                }
                spec.arena_pool = pool;
            }
        } else {
            spec.roles = nr.roles.clone();
        }
    }
    spec
}

fn new_run_launch_with_cfg(
    nr: &NewRun,
    cfg: &crate::config::Config,
) -> std::result::Result<(PathBuf, Vec<String>), String> {
    let target = nr.project.clone().ok_or_else(|| {
        "no project to launch into — open spar in a project or choose a registered project"
            .to_string()
    })?;
    if nr.task.trim().is_empty() {
        return Err("task cannot be empty".to_string());
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
    let spec = new_run_spec(nr, cfg);
    spec.validate_for_launch(cfg)?;
    let argv = spec.argv(cfg)?;
    Ok((target, argv))
}

/// Validate the new-run surface, then build the argv `run_palette`'s `plan` arm
/// already sends. The `--base` resolution stays at the call site (it needs the
/// operator's actual cwd, which this pure function does not have).
#[allow(dead_code)]
fn new_run_launch(nr: &NewRun) -> std::result::Result<(PathBuf, Vec<String>), String> {
    let cfg = crate::config::Config::default();
    new_run_launch_with_cfg(nr, &cfg)
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

/// Main's Diff tab (Stage B): the selected slot's worktree diff against HEAD. A
/// slot with no worktree (plan/review slots, headless runs) reports that plainly
/// (AC-18) rather than falling back to an arbitrary artifact file — that fallback
/// used to make the tab lie about what it shows, and is now an explicit non-goal.
fn diff_content(full: Option<&RunState>, slot_idx: usize) -> String {
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

    // No worktree for this slot (e.g. a plan/review slot, or headless): the Diff
    // tab is specifically the selected worktree's `git diff HEAD` (U4). Dumping an
    // arbitrary artifact file here used to make the tab lie about what it shows —
    // AC-18 makes this an explicit non-goal. Say there is no worktree diff instead.
    let slot_id = st
        .slots
        .get(slot_idx)
        .map(|s| s.id.as_str())
        .unwrap_or("this slot");
    format!("\n  {slot_id} has no worktree.\n\n  No worktree diff for this slot.")
}

/// Redraw is only worth it while something is moving on screen: a flash timer,
/// the palette/filter cursor, or a run that is actively working (active phase or a
/// running slot). An active phase with no running slot — Suite, Review,
/// Shipping — still animates so the header spinner keeps turning.
fn animating(app: &App, snap: &Snapshot) -> bool {
    // A window you are not looking at is not worth a frame (U30). Focus reporting
    // is best-effort: a terminal that never sends it leaves `focused` true.
    if !app.focused {
        return false;
    }
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
        || app.motion_in_flight()
        || snap.home.loading
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
        chat_conversation: app.chat_conversations.get("home").cloned(),
    };

    // The first paint must not block on a scan (U13): the refresher thread below
    // builds the real first snapshot off-thread, forced on its very first iteration.
    let mut cache = LogCache::empty();
    let snapshot = Arc::new(Mutex::new(Arc::new(Snapshot::loading(&active_root))));

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
    let mut last_home_clock = Instant::now();
    loop {
        let snap = Arc::clone(&*snapshot.lock().unwrap());

        if let Some((t, _, _, dur)) = &app.flash {
            if t.elapsed() > *dur {
                app.flash = None;
                dirty = true;
            }
        }

        // Rail reorder: the animated order becomes the frame's canonical order.
        let now = Instant::now();
        let rail_keys_vec = rail_keys(&snap, app.browse);
        let perm = app.rail_motion.observe(app.browse, rail_keys_vec, now);
        let mut moved_runs: Option<Vec<state::RunSummary>> = None;
        let mut moved_home: Option<HomeData> = None;
        if let Some(p) = perm {
            match app.browse {
                BrowseLevel::Runs => {
                    let v: Vec<state::RunSummary> =
                        p.iter().map(|&i| snap.runs[i].clone()).collect();
                    moved_runs = Some(v);
                }
                BrowseLevel::Home => {
                    let v: Vec<HomeRow> = p.iter().map(|&i| snap.home.rows[i].clone()).collect();
                    moved_home = Some(HomeData {
                        rows: v,
                        project_stats: snap.home.project_stats.clone(),
                        loading: snap.home.loading,
                    });
                }
                _ => {}
            }
        }
        let runs: &[state::RunSummary] = moved_runs.as_deref().unwrap_or(&snap.runs);
        let home: &HomeData = moved_home.as_ref().unwrap_or(&snap.home);

        // Keep the Runs cursor glued to identity through travel.
        if app.browse == BrowseLevel::Runs && !runs.is_empty() {
            if let Some(key) = app.selected_run_key.clone() {
                if let Some(pos) = runs.iter().position(|r| run_row_key(r) == key) {
                    app.selected_run = pos;
                }
            }
            if let Some(run) = runs.get(app.selected_run) {
                app.selected_run_key = Some(run_row_key(run));
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
        if app.home_target_run.is_some() {
            // Must run even with an empty `runs` — a Home Enter into a project
            // whose Runs listing has not landed yet (R2's one-tick lag) would
            // otherwise never start the give-up clock and pin `home_target_run`
            // forever, leaving Main/Agents blank with no way out (round-11 review,
            // major). Called unconditionally, every loop iteration: `resolve_home_target`
            // itself withholds the give-up clock until `snap` is actually a scan of the
            // target's project (below), so this being unconditional does not start the
            // budget early.
            let snapshot_for_target = snapshot_covers_target(&snap, &active_root);
            resolve_home_target(&mut app, runs, snapshot_for_target);
            if !runs.is_empty() {
                app.selected_run = app.selected_run.min(runs.len() - 1);
            }
        } else if runs.is_empty() {
            app.selected_run = 0;
        } else {
            // The attention sort reorders the rail as runs change state; keep the
            // cursor glued to the same run id rather than the same row.
            if let Some(prev) = sel.run_id.as_deref() {
                if let Some(pos) = runs.iter().position(|r| r.id == prev) {
                    app.selected_run = pos;
                }
            }
            app.selected_run = app.selected_run.min(runs.len() - 1);
        }
        // Keep key in sync after clamp
        if app.browse == BrowseLevel::Runs && !runs.is_empty() {
            if let Some(run) = runs.get(app.selected_run) {
                app.selected_run_key = Some(run_row_key(run));
            }
        }
        // Toast a run the moment it starts wanting the operator (gate/broken), so a
        // fleet transition is noticed even while looking at another run. At Home
        // there is no `snap.runs` (it is cross-project) — feed it Home's rows instead
        // so the toast still fires there.
        if app.browse == BrowseLevel::Home {
            let home_runs: Vec<state::RunSummary> = home
                .rows
                .iter()
                .filter_map(|r| match r {
                    HomeRow::Run { run, .. } => Some(run.clone()),
                    _ => None,
                })
                .collect();
            emit_attention_toasts(&mut app, &home_runs, true);
        } else {
            emit_attention_toasts(&mut app, runs, false);
        }
        let n_slots = snap.full.as_ref().map(|s| s.slots.len()).unwrap_or(0);
        app.selected_slot = if n_slots == 0 {
            0
        } else {
            app.selected_slot.min(n_slots - 1)
        };
        if app.browse == BrowseLevel::Home {
            resync_home_selection(&mut app, &home.rows);
        }

        rail_state.select(match app.browse {
            BrowseLevel::Home if !home.rows.is_empty() => Some(app.selected_home),
            BrowseLevel::Projects if !snap.projects.is_empty() => Some(app.selected_project),
            BrowseLevel::Runs if !runs.is_empty() => Some(app.selected_run),
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
            // Atomic frame (DECSET 2026). At `FRAME_ANIMATING` a 200-column repaint
            // is otherwise wide enough to be caught mid-flight and tear. Terminals
            // without it ignore both halves, so this costs nothing where it is not
            // understood.
            let _ = std::io::stdout().execute(BeginSynchronizedUpdate);
            terminal.draw(|f| {
                draw(
                    f,
                    &snap.swarm,
                    &snap.projects,
                    runs,
                    snap.full.as_ref(),
                    &snap.stream_text,
                    &snap.stream_text_raw,
                    &snap.log_records,
                    &snap.activity,
                    &snap.diff_text,
                    &snap.diff_records,
                    &snap.plan_docs,
                    &snap.review,
                    &snap.chat,
                    home,
                    snap.log_stats.as_ref(),
                    &mut app,
                    &mut rail_state,
                );
            })?;
            let _ = std::io::stdout().execute(EndSynchronizedUpdate);
            // Re-evaluate after paint: the tab strip tween is retargeted inside draw_labels.
            app.animated = animating(&app, &snap);
            dirty = false;
        }

        let frame = if app.animated {
            FRAME_ANIMATING
        } else {
            FRAME_IDLE
        };
        match msg_rx.recv_timeout(frame) {
            Ok(Msg::Data) => dirty = true,
            Ok(Msg::Flash(msg, color)) => {
                app.flash(msg, color);
                dirty = true;
            }
            Ok(Msg::RosterReady(gen, roster)) => {
                apply_roster_ready(&mut app, gen, roster);
                dirty = true;
            }
            Ok(Msg::ChatTurnDone) => {
                app.chat_active_turn = None;
                dirty = true;
            }
            Ok(Msg::ChatTurnResult {
                conv,
                stats,
                worktree,
                error,
            }) => {
                app.chat_active_turn = None;
                if let Some(s) = stats.clone() {
                    app.chat_latest_stats.insert(conv.clone(), s.clone());
                    let entry = app.chat_accum_stats.entry(conv.clone()).or_default();
                    entry.billed_tokens = entry.billed_tokens.saturating_add(s.billed_tokens);
                    entry.input_tokens = entry.input_tokens.saturating_add(s.input_tokens);
                    entry.output_tokens = entry.output_tokens.saturating_add(s.output_tokens);
                    entry.cache_read_tokens =
                        entry.cache_read_tokens.saturating_add(s.cache_read_tokens);
                    entry.cache_write_tokens = entry
                        .cache_write_tokens
                        .saturating_add(s.cache_write_tokens);
                    entry.context_tokens = entry.context_tokens.max(s.context_tokens);
                    entry.tools = entry.tools.saturating_add(s.tools);
                }
                if let Some(wt) = worktree.clone() {
                    if wt.exists() {
                        app.chat_last_preserved_worktree = Some(wt.clone());
                        app.flash(format!("preserved dirty worktree: {}", wt.display()), ALERT);
                    }
                }
                if let Some(err) = error {
                    if !err.is_empty() {
                        app.flash(format!("turn: {err}"), ALERT);
                    }
                }
                dirty = true;
            }
            Ok(Msg::Input(ev)) => {
                dirty = true;
                let mut ev = Some(ev);
                // Drain the burst so wheel/key spam cannot outpace the redraw.
                while let Some(e) = ev {
                    match e {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            let active_records = active_records_for(&app, &snap);
                            if handle_key(
                                &mut app,
                                key.code,
                                key.modifiers,
                                &snap.swarm,
                                &snap.projects,
                                &home.rows,
                                runs,
                                snap.full.as_ref(),
                                &active_records,
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
                            &home.rows,
                            runs,
                            snap.full.as_ref(),
                            &snap.diff_records,
                            &mut active_root,
                            local_root.as_deref(),
                            rail_state.offset(),
                        ),
                        // DECSET 1004. Repaint on the transition so the frame the
                        // window is left on is the unfocused one, then stop.
                        Event::FocusGained => app.focused = true,
                        Event::FocusLost => {
                            app.focused = false;
                            app.settle_motion();
                        }
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
                        Ok(Msg::ChatTurnDone) => {
                            app.chat_active_turn = None;
                            None
                        }
                        Ok(Msg::ChatTurnResult {
                            conv,
                            stats,
                            worktree,
                            error,
                        }) => {
                            app.chat_active_turn = None;
                            if let Some(s) = stats.clone() {
                                app.chat_latest_stats.insert(conv.clone(), s.clone());
                                let entry = app.chat_accum_stats.entry(conv.clone()).or_default();
                                entry.billed_tokens =
                                    entry.billed_tokens.saturating_add(s.billed_tokens);
                                entry.input_tokens =
                                    entry.input_tokens.saturating_add(s.input_tokens);
                                entry.output_tokens =
                                    entry.output_tokens.saturating_add(s.output_tokens);
                                entry.cache_read_tokens =
                                    entry.cache_read_tokens.saturating_add(s.cache_read_tokens);
                                entry.cache_write_tokens = entry
                                    .cache_write_tokens
                                    .saturating_add(s.cache_write_tokens);
                                entry.context_tokens = entry.context_tokens.max(s.context_tokens);
                                entry.tools = entry.tools.saturating_add(s.tools);
                            }
                            if let Some(wt) = worktree.clone() {
                                if wt.exists() {
                                    app.chat_last_preserved_worktree = Some(wt.clone());
                                    app.flash(
                                        format!("preserved dirty worktree: {}", wt.display()),
                                        ALERT,
                                    );
                                }
                            }
                            if let Some(err) = error {
                                if !err.is_empty() {
                                    app.flash(format!("turn: {err}"), ALERT);
                                }
                            }
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
                } else if app.browse == BrowseLevel::Home
                    && last_home_clock.elapsed() >= HOME_CLOCK_TICK
                {
                    dirty = true;
                    last_home_clock = Instant::now();
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
            // outgoing snapshot's `runs`/`app.selected_run` still describe the
            // *previous* project. Keep resending it (not `take()`) until the clamp
            // above observes a snapshot that actually contains it and clears it —
            // otherwise this send races that clamp and can overwrite the target
            // with `None` a tick early (R2/AC-27).
            run_id: app
                .home_target_run
                .clone()
                .or_else(|| runs.get(app.selected_run).map(|r| r.id.clone())),
            slot_idx: app.selected_slot,
            project_idx: app.selected_project,
            home_scope: app.home_scope.clone(),
            home_watermark: app.home_watermark,
            chat_conversation: if app.browse == BrowseLevel::Home {
                app.chat_conversations.get("home").cloned()
            } else {
                snap.runs
                    .get(app.selected_run)
                    .and_then(|r| app.chat_conversations.get(&r.id).cloned())
                    .or_else(|| app.chat_conversations.get("home").cloned())
            },
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

/// Thin wrapper around [`handle_key_inner`]: on the way out, if the key just
/// dispatched left the Diff tab (or quit the app while on it), records the
/// current diff records as "seen" for the watermark (AC-18/005 C) — written here,
/// not per-frame, so a file only reads as "new" until the operator actually
/// leaves the tab having looked at it.
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
    active_records: &[Record],
    active_root: &mut PathBuf,
    local_root: Option<&std::path::Path>,
) -> Result<bool> {
    let was_diff = app.main_tab == MainTab::Diff;
    let diff_run_id = full.map(|st| st.id.clone());
    let diff_base_commit = full.and_then(|st| st.base_commit.clone());
    let diff_slot_id = full
        .and_then(|st| st.slots.get(app.selected_slot))
        .map(|s| s.id.clone());
    let diff_snapshot: Vec<Record> = if was_diff {
        active_records.to_vec()
    } else {
        Vec::new()
    };
    let quit = handle_key_inner(
        app,
        code,
        mods,
        swarm,
        projects,
        home_rows,
        runs,
        full,
        active_records,
        active_root,
        local_root,
    )?;
    if was_diff && (quit || app.main_tab != MainTab::Diff) {
        if let (Some(run_id), Some(slot_id)) = (diff_run_id, diff_slot_id) {
            mark_diff_seen(
                &run_id,
                &slot_id,
                diff_base_commit.as_deref(),
                &diff_snapshot,
            );
        }
    }
    Ok(quit)
}

#[allow(clippy::too_many_arguments)]
fn handle_key_inner(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_records: &[Record],
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
        return handle_palette_key(
            app,
            code,
            mods,
            swarm,
            projects,
            home_rows,
            local_root,
            runs,
            full,
            active_root.as_path(),
        );
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

    // Chat composer: starting composition and proposal launch (Chat tab, Main focused)
    if app.focus == Focus::Main && app.main_tab == MainTab::Chat && !app.chat_composing {
        // Proposal launch check first, before composing consumes the key.
        if app.browse == BrowseLevel::Home
            && (code == KeyCode::Char('o') && mods.is_empty() || code == KeyCode::Enter)
        {
            let scope_key = "home".to_string();
            if let Some(conv_id) = app.chat_conversations.get(&scope_key).cloned() {
                let scope = crate::orchestrator::Scope::Home;
                if let Ok(records) = crate::orchestrator::transcript(swarm, &scope, Some(&conv_id))
                {
                    for rec in records.iter().rev() {
                        if rec.actor.as_deref() != Some("spar") {
                            continue;
                        }
                        if let Ok(Some(proposal)) =
                            crate::orchestrator::parse_proposal(&rec.summary)
                        {
                            let (project, all_projects) =
                                new_run_target(app, projects, home_rows, local_root, None);
                            app.chat_pending_proposal = Some(proposal.clone());
                            app.chat_pending_brief_path = None;
                            begin_new_run(
                                app,
                                project.clone(),
                                all_projects.clone(),
                                proposal.task.clone(),
                                NewRunField::Task,
                            );
                            return Ok(false);
                        }
                    }
                }
            }
            if code == KeyCode::Char('o') {
                return Ok(false);
            }
        }
        // Explicit compose key: only `i` enters composition, so global keys
        // like `q`/`?` and future bindings keep working while not composing,
        // and a message can begin with any character once composing.
        if code == KeyCode::Char('i') && mods.is_empty() {
            app.chat_composing = true;
            return Ok(false);
        }
    }

    // Esc cancels an in-flight turn even when not composing (the operator lands
    // non-composing after every Enter, so requiring `i` first makes cancel
    // unreachable — 008 review finding).
    if app.focus == Focus::Main
        && app.main_tab == MainTab::Chat
        && app.chat_active_turn.is_some()
        && code == KeyCode::Esc
    {
        if let Some(h) = app.chat_active_turn.take() {
            h.cancel();
            app.flash("turn cancelled".to_string(), INFO);
        }
        // Also exit composing if it was active.
        app.chat_composing = false;
        app.chat_input.clear();
        return Ok(false);
    }

    // Chat composer captures input while composing (Main focused).
    if app.focus == Focus::Main && app.main_tab == MainTab::Chat && app.chat_composing {
        match code {
            KeyCode::Esc => {
                if let Some(h) = app.chat_active_turn.take() {
                    h.cancel();
                    app.flash("turn cancelled".to_string(), INFO);
                }
                app.chat_composing = false;
                app.chat_input.clear();
                return Ok(false);
            }
            KeyCode::Enter => {
                if app.chat_active_turn.is_some() {
                    app.flash("turn in flight".to_string(), ALERT);
                    return Ok(false);
                }
                let trimmed = app.chat_input.trim().to_string();
                if trimmed.is_empty() {
                    return Ok(false);
                }
                // Determine scope and conversation id
                let scope_key = if app.browse == BrowseLevel::Home {
                    "home".to_string()
                } else {
                    runs.get(app.selected_run)
                        .map(|r| r.id.clone())
                        .unwrap_or_else(|| "home".to_string())
                };
                let conv_id = app
                    .chat_conversations
                    .entry(scope_key.clone())
                    .or_insert_with(|| format!("talk-{}", crate::bus::new_id()))
                    .clone();
                // Record watermark (event count) and turn nonce
                let events = crate::bus::list_events(swarm, {
                    if scope_key == "home" {
                        None
                    } else {
                        Some(scope_key.as_str())
                    }
                })
                .unwrap_or_default();
                let watermark = events.len();
                let turn_id = crate::bus::new_id();
                app.chat_turns.insert(conv_id.clone(), turn_id.clone());
                app.chat_watermarks.insert(conv_id.clone(), watermark);
                // Send operator message — surface errors instead of silently dropping them
                // and dispatching a turn whose transcript is missing the question.
                if let Err(e) =
                    crate::orchestrator::say_for_tui(swarm, &scope_key, &conv_id, &trimmed)
                {
                    app.flash(format!("chat send failed: {e:#}"), ALERT);
                    app.chat_input.clear();
                    app.chat_composing = false;
                    return Ok(false);
                }
                // Dispatch turn (backend owned) — off the input thread so the TUI stays responsive.
                let req = crate::orchestrator::TurnRequest {
                    scope_key: scope_key.clone(),
                    conversation_id: conv_id.clone(),
                    turn_id: turn_id.clone(),
                    watermark,
                    project_root: swarm.project_root.clone(),
                    run_id: if scope_key == "home" {
                        None
                    } else {
                        Some(scope_key.clone())
                    },
                };
                let handle = std::sync::Arc::new(crate::orchestrator::TurnHandle::new(
                    scope_key.clone(),
                    conv_id.clone(),
                    turn_id.clone(),
                ));
                app.chat_active_turn = Some(handle.clone());
                if let Some(tx) = app.bg_tx.clone() {
                    let swarm_clone = swarm.clone();
                    let conv_clone = conv_id.clone();
                    std::thread::spawn(move || {
                        let res = crate::orchestrator::dispatch_turn_with_handle(
                            swarm_clone,
                            req,
                            &handle,
                        );
                        match res {
                            Ok(out) => {
                                let _ = tx.send(Msg::ChatTurnResult {
                                    conv: conv_clone,
                                    stats: out.stats.clone(),
                                    worktree: out.worktree.clone(),
                                    error: out.error.clone(),
                                });
                                let _ = tx.send(Msg::ChatTurnDone);
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::ChatTurnResult {
                                    conv: conv_clone,
                                    stats: None,
                                    worktree: None,
                                    error: Some(format!("turn failed: {e:#}")),
                                });
                                let _ = tx.send(Msg::ChatTurnDone);
                            }
                        }
                    });
                } else {
                    let out =
                        crate::orchestrator::dispatch_turn_with_handle(swarm.clone(), req, &handle);
                    if let Ok(out) = out {
                        if let Some(s) = out.stats.clone() {
                            app.chat_latest_stats.insert(conv_id.clone(), s.clone());
                            let entry = app.chat_accum_stats.entry(conv_id.clone()).or_default();
                            entry.billed_tokens =
                                entry.billed_tokens.saturating_add(s.billed_tokens);
                            entry.input_tokens = entry.input_tokens.saturating_add(s.input_tokens);
                            entry.output_tokens =
                                entry.output_tokens.saturating_add(s.output_tokens);
                            entry.cache_read_tokens =
                                entry.cache_read_tokens.saturating_add(s.cache_read_tokens);
                            entry.cache_write_tokens = entry
                                .cache_write_tokens
                                .saturating_add(s.cache_write_tokens);
                            entry.context_tokens = entry.context_tokens.max(s.context_tokens);
                            entry.tools = entry.tools.saturating_add(s.tools);
                        }
                        if let Some(wt) = out.worktree.clone() {
                            if wt.exists() {
                                app.chat_last_preserved_worktree = Some(wt);
                            }
                        }
                    }
                    app.chat_active_turn = None;
                }
                app.chat_input.clear();
                app.chat_composing = false;
                return Ok(false);
            }
            KeyCode::Backspace => {
                app.chat_input.pop();
                return Ok(false);
            }
            KeyCode::Char(c) if mods.is_empty() || mods == KeyModifiers::SHIFT => {
                app.chat_input.push(c);
                return Ok(false);
            }
            KeyCode::Char(_) => {
                // Ctrl/Cmd modified chars are not input while composing.
            }
            _ => {}
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
            app.main_tab = app.main_tab.next_in(tabs_for(app.browse));
        }
        KeyCode::Char('[') => {
            app.main_tab = app.main_tab.prev_in(tabs_for(app.browse));
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
            if app.browse == BrowseLevel::Home {
                app.main_tab = MainTab::Chat;
                app.focus = Focus::Main;
                app.chat_composing = true;
                // Ensure a conversation id exists for Home
                let scope_key = "home".to_string();
                app.chat_conversations
                    .entry(scope_key)
                    .or_insert_with(|| format!("talk-{}", crate::bus::new_id()));
            } else {
                // At Runs/Agents, also focus Chat scoped to selected run
                app.main_tab = MainTab::Chat;
                app.focus = Focus::Main;
                app.chat_composing = true;
                if let Some(run) = runs.get(app.selected_run) {
                    let scope_key = run.id.clone();
                    app.chat_conversations
                        .entry(scope_key)
                        .or_insert_with(|| format!("talk-{}", crate::bus::new_id()));
                }
            }
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
            Focus::Main => app.scroll_main_by(3, full.is_some(), active_records),
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, -1),
            Focus::Main => app.scroll_main_by(-3, full.is_some(), active_records),
        },
        KeyCode::PageDown => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, 5),
            Focus::Main => app.scroll_main_by(
                i32::from(app.main_page(full.is_some())),
                full.is_some(),
                active_records,
            ),
        },
        KeyCode::PageUp => match app.focus {
            Focus::Rail => rail_move(app, projects, home_rows, runs, n_slots, -5),
            Focus::Main => app.scroll_main_by(
                -i32::from(app.main_page(full.is_some())),
                full.is_some(),
                active_records,
            ),
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
            app.home_for_main(full.is_some(), active_records);
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.end_for_main(full.is_some(), active_records);
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
        // Structural navigation (Phase C): jump the record cursor to the next/prev
        // record head, tool call, error, or phase/document boundary. Inert on Shell
        // (an attached pane already forwarded the key above; an unattached one has
        // no records to navigate).
        KeyCode::Char('J') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, 1, |_| true);
        }
        KeyCode::Char('K') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, -1, |_| true);
        }
        KeyCode::Char('t') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, 1, |r| {
                matches!(r.kind, RecordKind::Tool(_))
            });
        }
        KeyCode::Char('T') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, -1, |r| {
                matches!(r.kind, RecordKind::Tool(_))
            });
        }
        KeyCode::Char('e') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, 1, is_error_record);
        }
        KeyCode::Char('E') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, -1, is_error_record);
        }
        KeyCode::Char('}') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, 1, is_boundary_record);
        }
        KeyCode::Char('{') if app.focus == Focus::Main => {
            move_record_cursor(app, active_records, -1, is_boundary_record);
        }
        // Space folds/unfolds the record under the cursor (U36); `A` overrides every
        // default at once, leaving individual toggles intact for when it is pressed
        // again.
        KeyCode::Char(' ') if app.focus == Focus::Main => {
            // No cursor yet, or a cursor left over from a Main tab / slot switch
            // that no longer resolves to anything on screen (round-11 review,
            // AC-6): default to the first record rather than silently toggling a
            // fold key for a record that is neither selected nor visible, the same
            // "act, don't no-op" rule `move_cursor` already follows for `J`/`K`.
            let resolved = app
                .record_cursor
                .as_ref()
                .filter(|c| active_records.iter().any(|r| &r.source == *c))
                .cloned();
            let cur = resolved.or_else(|| {
                active_records.first().map(|r| {
                    app.record_cursor = Some(r.source.clone());
                    app.record_cursor_dirty = true;
                    r.source.clone()
                })
            });
            match cur {
                Some(cur) => {
                    if !app.fold_open.remove(&cur) {
                        app.fold_open.insert(cur);
                    }
                }
                None => app.flash("no match", FG_MUTED),
            }
        }
        KeyCode::Char('A') if app.focus == Focus::Main => {
            app.fold_all = !app.fold_all;
        }
        // R: the byte-for-byte raw escape hatch (U36), only where one raw source
        // exists (Log, Diff) — AC-14.
        KeyCode::Char('R') if app.focus == Focus::Main => {
            if matches!(app.main_tab, MainTab::Log | MainTab::Diff) {
                app.raw_mode = !app.raw_mode;
                app.flash(
                    if app.raw_mode {
                        "raw text (R toggles)"
                    } else {
                        "parsed records"
                    },
                    ACCENT,
                );
            }
        }
        // f: narrow Activity to the selected slot (correction #7 — Log is already
        // scoped to the selected slot, so filtering only ever applies to Activity).
        KeyCode::Char('f') if app.focus == Focus::Main && app.main_tab == MainTab::Activity => {
            app.activity_slot_filter = if app.activity_slot_filter.is_some() {
                None
            } else {
                Some(app.selected_slot)
            };
        }
        _ => {}
    }
    Ok(false)
}

fn is_error_record(r: &Record) -> bool {
    matches!(
        r.kind,
        RecordKind::Error | RecordKind::Alert | RecordKind::Result { ok: false }
    ) || (matches!(r.kind, RecordKind::Tool(_)) && r.ok == Some(false))
}

fn is_boundary_record(r: &Record) -> bool {
    matches!(r.kind, RecordKind::Section | RecordKind::Doc)
}

/// Moves the record cursor to the next (`dir > 0`) or previous (`dir < 0`) record
/// matching `pred`, starting just past whatever the cursor currently resolves to. A
/// miss (no such record in that direction) flashes rather than silently doing
/// nothing (AC-13).
fn move_record_cursor(app: &mut App, records: &[Record], dir: i32, pred: impl Fn(&Record) -> bool) {
    match move_cursor(records, app.record_cursor.as_ref(), dir, pred) {
        Some(id) => {
            app.record_cursor = Some(id);
            app.record_cursor_dirty = true;
        }
        None => app.flash("no match", FG_MUTED),
    }
}

fn move_cursor(
    records: &[Record],
    cursor: Option<&SourceId>,
    dir: i32,
    pred: impl Fn(&Record) -> bool,
) -> Option<SourceId> {
    if records.is_empty() {
        return None;
    }
    let cur_idx = cursor.and_then(|c| records.iter().position(|r| &r.source == c));
    let n = records.len() as i32;
    let mut i = match cur_idx {
        Some(idx) => idx as i32 + dir,
        None if dir >= 0 => 0,
        None => n - 1,
    };
    while i >= 0 && i < n {
        let r = &records[i as usize];
        if pred(r) {
            return Some(r.source.clone());
        }
        i += dir;
    }
    None
}

/// Applies the Activity tab's selected-slot filter (`f`), shared verbatim between
/// painting (`draw_activity_body`) and cursor navigation (`active_records_for`) — a
/// separate filtered copy in each place let the cursor move to a record the paint
/// side had already dropped, landing it off-screen (round-review finding 3).
fn filter_activity_records(
    activity: &[Record],
    slot_filter: Option<usize>,
    full: Option<&RunState>,
) -> Vec<Record> {
    match (slot_filter, full) {
        (Some(idx), Some(st)) => {
            let slot_id = st.slots.get(idx).map(|s| s.id.as_str());
            let role = st.slots.get(idx).map(|s| role_label(s.role));
            activity
                .iter()
                .filter(|r| {
                    matches!(r.kind, RecordKind::Section)
                        || r.actor.is_none()
                        || r.actor.as_deref() == Some("")
                        || r.actor.as_deref() == slot_id
                        || r.actor.as_deref() == role
                })
                .cloned()
                .collect()
        }
        _ => activity.to_vec(),
    }
}

/// The Log tab's effective record list: the pre-parsed `log_records` when
/// non-empty, else a CPU-only fallback parse of `stream_text` (U13 forbids the
/// disk read, not the parse). The single list both `draw_log_body`'s paint and
/// `active_records_for`'s navigation must agree on (round-9 finding 5) — the same
/// fix `filter_activity_records` already gives Activity.
fn effective_log_records(log_records: &[Record], stream_text: &str) -> Vec<Record> {
    if log_records.is_empty() && !stream_text.trim().is_empty() {
        record::parse_log_records(
            stream_text,
            0,
            &[],
            &record::PathShortener::new(Vec::new()),
            "",
            "",
        )
        .iter()
        .map(|lr| lr.to_record())
        .collect()
    } else {
        log_records.to_vec()
    }
}

/// Which record list `J`/`K`/`t`/`e`/`}`/`Space`/`R` act on — whatever Main is
/// currently showing, filtered exactly as it is painted.
fn active_records_for(app: &App, snap: &Snapshot) -> Vec<Record> {
    match app.main_tab {
        MainTab::Log => effective_log_records(&snap.log_records, &snap.stream_text),
        MainTab::Activity => {
            filter_activity_records(&snap.activity, app.activity_slot_filter, snap.full.as_ref())
        }
        MainTab::Diff => snap.diff_records.clone(),
        MainTab::Plan => snap.plan_docs.clone(),
        MainTab::Review => snap.review.clone(),
        MainTab::Chat => snap.chat.clone(),
        MainTab::Shell => Vec::new(),
    }
}

/// The project the operator is browsing right now, for surfaces (`n`, `:plan`) that
/// resolve their target at handling time rather than from a snapshot. At `Projects`
/// the highlighted row is the target and must come from `selected_project`:
/// `active_root` is refreshed only in the pre-input clamp, so a queued burst (`j`
/// then the command, or a click then the command) leaves it one selection stale
/// (review e72f434e). Inside a project (`in_project()`, i.e. Runs/Agents) `active_root`
/// is live and correct; `swarm.project_root` is not, since `swarm` is the snapshot
/// handed to `handle_key` and lags the instant `rail_enter` updates `active_root`
/// (review 3be317b2). Outside a project view there is no browsed target.
fn browsed_project_target<'a>(
    app: &App,
    projects: &'a [registry::ProjectEntry],
    active_root: &'a Path,
) -> Option<&'a Path> {
    if app.browse == BrowseLevel::Projects {
        projects.get(app.selected_project).map(|p| p.root.as_path())
    } else if app.browse.in_project() {
        Some(active_root)
    } else {
        None
    }
}

/// `n`: open the Phase D new-run surface. The target project follows the scope
/// (D2): a scoped Home defaults to its project, an all-project Home defaults to the
/// selected row's project (falling back to the local repo), and the picker offers
/// every registered project to cycle through.
/// The new-run surface's target project and its cycle list, from the same rule
/// regardless of who is opening the surface: `n` on Home, and the `:plan <task>`
/// palette fallback when nothing is selected to reuse a fleet from. Both must agree,
/// or `:plan` in cross-project Home can silently target a different project than the
/// one the operator is looking at (round-9 review finding).
///
/// `browsed_project` is `Some` whenever the caller is inside a project view
/// (`BrowseLevel::in_project()` — Runs or Agents), and always wins: the operator has
/// drilled into that project, so it is never stale the way `active_root` can be while
/// still browsing Home. Without this, `n`/`:plan` pressed after `Enter`-ing a Home run
/// row for project B (which sets `active_root = B` but leaves `home_scope`/`home_rows`
/// pointed at A) would target A — the wrong project — with no way to correct it,
/// since a scoped Home offers only its own root to cycle through (round-11 review
/// finding).
fn new_run_target(
    app: &App,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    local_root: Option<&Path>,
    browsed_project: Option<&Path>,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let scoped = browsed_project.is_some() || matches!(app.home_scope, HomeScope::Project(_));
    let target = browsed_project
        .map(Path::to_path_buf)
        .or_else(|| match &app.home_scope {
            HomeScope::Project(root) => Some(root.clone()),
            HomeScope::All => home_rows
                .get(app.selected_home)
                .and_then(|r| match r {
                    HomeRow::Run { run, .. } => run.project_root.clone(),
                    HomeRow::Project(_, root) => Some(root.clone()),
                    _ => None,
                })
                .or_else(|| local_root.map(Path::to_path_buf)),
        });
    // A scoped target (whether from `home_scope` or from browsing inside a project)
    // only offers itself to cycle through — every other registered project is out of
    // scope, and offering them let `←`/`→` launch a plan against a project the
    // operator was not looking at (review finding). Cycling is only meaningful at an
    // unscoped, all-project Home.
    let mut all_projects: Vec<PathBuf> = if scoped {
        Vec::new()
    } else {
        projects.iter().map(|p| p.root.clone()).collect()
    };
    // A scoped Home's target may be one refresh ahead of the registry (a project
    // just entered but not yet registered) — without this, `←`/`→` cycling can't
    // find the target in `nr.projects`, falls back to index 0, and silently
    // retargets the launch away from what the operator scoped to (round-7 review
    // finding). Keep it reachable even if the registry hasn't caught up yet.
    if let Some(t) = &target {
        if !all_projects.contains(t) {
            all_projects.insert(0, t.clone());
        }
    }
    let project = target.or_else(|| all_projects.first().cloned());
    (project, all_projects)
}

fn open_new_run(
    app: &mut App,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    local_root: Option<&Path>,
    browsed_project: Option<&Path>,
) {
    let (project, all_projects) =
        new_run_target(app, projects, home_rows, local_root, browsed_project);
    begin_new_run(app, project, all_projects, String::new(), NewRunField::Task);
}

/// A `NewRun` in its initial "checking roster" state — no disk or process I/O, so
/// this is safe to call before the background channel exists (`App::new`'s task
/// seed, before `run_loop` wires `bg_tx`).
fn rebuild_roles_for_workflow(nr: &mut NewRun, cfg: &crate::config::Config) {
    if let Some(wf) = nr.workflow {
        let expected = crate::runspec::spec_rows(wf, cfg);
        if wf == crate::runspec::SpecWorkflow::Arena {
            let mut new_pool = vec![None; expected.len()];
            for (i, pin) in nr.arena_pool.iter().enumerate().take(expected.len()) {
                new_pool[i] = pin.clone();
            }
            nr.arena_pool = new_pool;
            nr.roles.clear();
            nr.legacy_providers.clear();
        } else {
            let mut new_roles = Vec::new();
            for (role, ordinal) in expected {
                if let Some(existing) = nr
                    .roles
                    .iter()
                    .find(|r| r.role == role && r.ordinal == ordinal)
                {
                    new_roles.push(existing.clone());
                } else {
                    new_roles.push(crate::runspec::RoleAssignment {
                        role,
                        ordinal,
                        primary: None,
                        backup: None,
                    });
                }
            }
            nr.roles = new_roles;
            nr.arena_pool.clear();
            nr.legacy_providers.clear();
        }
        nr.role_sel = 0;
        nr.editing_backup = false;
        nr.editing_model = false;
        nr.model_buffer.clear();
    } else {
        nr.roles.clear();
        nr.arena_pool.clear();
        nr.editing_model = false;
        nr.model_buffer.clear();
    }
}

fn pending_new_run(
    project: Option<PathBuf>,
    projects: Vec<PathBuf>,
    task: String,
    field: NewRunField,
    gen: u64,
) -> NewRun {
    let defaults = crate::defaults::load();
    let mut roles = defaults.roles.clone();
    let mut arena_pool = defaults.arena_pool.clone();
    let cfg_for_rows = project
        .as_ref()
        .and_then(|p| crate::config::Config::load(p).ok())
        .unwrap_or_default();
    if let Some(wf) = defaults.workflow {
        let expected = crate::runspec::spec_rows(wf, &cfg_for_rows);
        if wf == crate::runspec::SpecWorkflow::Arena {
            if arena_pool.len() != expected.len() {
                arena_pool.resize_with(expected.len(), || None);
            }
        } else {
            for (role, ordinal) in expected {
                if !roles.iter().any(|r| r.role == role && r.ordinal == ordinal) {
                    roles.push(crate::runspec::RoleAssignment {
                        role,
                        ordinal,
                        primary: None,
                        backup: None,
                    });
                }
            }
        }
    }
    NewRun {
        project,
        projects,
        task: if task.is_empty() {
            defaults.task.clone()
        } else {
            task
        },
        roster: Vec::new(),
        picked: Vec::new(),
        field,
        sel: 0,
        loading: true,
        gen,
        workflow: defaults.workflow,
        roles,
        arena_pool,
        legacy_providers: Vec::new(),
        role_sel: 0,
        editing_backup: false,
        editing_model: false,
        model_buffer: String::new(),
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
            let mut roster = compute_new_run_roster(&app.cfg);
            if let Some(nr) = app.new_run.as_mut() {
                if let Some(proposal) = app.chat_pending_proposal.clone() {
                    // Temporarily set roster for proposal handling, then merge.
                    nr.roster = roster;
                    apply_proposal_to_roster(&proposal, nr);
                    roster = std::mem::take(&mut nr.roster);
                }
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
    // Registry order is `last_seen` — when a project was last *opened*, not when any
    // run last progressed — so the first entry with a `last_run_id` is not
    // necessarily the actual most recent run across the roster (round-7 review
    // finding). Load every candidate and let `most_recent_fleet` pick by `updated_at`.
    let candidates: Vec<(DateTime<Utc>, String, Vec<String>)> = registry::projects()
        .into_iter()
        .filter_map(|p| p.last_run_id.map(|id| (p.root, id)))
        .filter_map(|(root, id)| {
            let paths = SparPaths::new(&root);
            RunState::load(&paths, &id)
                .ok()
                .map(|st| (st.updated_at, id, st.providers))
        })
        .collect();
    let recent_fleet = most_recent_fleet(candidates);
    let recent_ref = recent_fleet
        .as_ref()
        .map(|(id, v)| (id.as_str(), v.as_slice()));
    build_roster(cfg, &detected, recent_ref)
}

/// Pure half of the recent-fleet lookup (round-7 review finding): picks the
/// candidate with the latest `updated_at`, independent of the order the caller
/// gathered them in (registry order is `last_seen`, not run recency).
fn most_recent_fleet(
    candidates: Vec<(DateTime<Utc>, String, Vec<String>)>,
) -> Option<(String, Vec<String>)> {
    candidates
        .into_iter()
        .max_by_key(|(updated_at, _, _)| *updated_at)
        .map(|(_, id, providers)| (id, providers))
}

fn apply_proposal_to_roster(proposal: &crate::orchestrator::Proposal, nr: &mut NewRun) {
    let workflow_was_none = nr.workflow.is_none();
    if nr.task.trim().is_empty() {
        nr.task = proposal.task.clone();
    }
    if nr.workflow.is_none() {
        if let Some(wf) = proposal.workflow.as_deref() {
            nr.workflow = crate::runspec::SpecWorkflow::parse(wf);
        }
    }
    if workflow_was_none && nr.workflow.is_some() {
        let cfg = crate::config::Config::load(&nr.project.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        }))
        .unwrap_or_default();
        rebuild_roles_for_workflow(nr, &cfg);
    }
    let mut to_add: Vec<String> = Vec::new();
    for prov in &proposal.providers {
        if !to_add.contains(prov) {
            to_add.push(prov.clone());
        }
    }
    let mut sorted_roles: Vec<(&String, &String)> = proposal.roles.iter().collect();
    sorted_roles.sort_by(|a, b| a.0.cmp(b.0));
    for (_, v) in sorted_roles {
        if !to_add.contains(v) {
            to_add.push(v.clone());
        }
    }
    for v in &proposal.reviewer {
        if !to_add.contains(v) {
            to_add.push(v.clone());
        }
    }
    let mut sorted_backups: Vec<(&String, &String)> = proposal.backups.iter().collect();
    sorted_backups.sort_by(|a, b| a.0.cmp(b.0));
    for (_, v) in sorted_backups {
        if !to_add.contains(v) {
            to_add.push(v.clone());
        }
    }
    for v in &proposal.reviewer_backups {
        if !to_add.contains(v) {
            to_add.push(v.clone());
        }
    }
    for prov in to_add {
        if !nr
            .roster
            .iter()
            .any(|e| matches!(&e.choice, RosterChoice::Provider(p) if p == &prov))
        {
            let available = crate::runspec::Pin::parse(&prov)
                .map(|pin| {
                    crate::providers::detect_all().iter().any(|r| {
                        format!("cli:{}", r.name) == crate::quota::normalize_key(&pin.provider)
                            && r.available
                    }) || crate::provider_ref::ProviderRef::parse(&prov)
                        .map(|r| r.is_api())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let reason = if available {
                None
            } else if crate::runspec::Pin::parse(&prov).is_err() {
                Some("invalid provider".to_string())
            } else {
                Some("not in roster".to_string())
            };
            nr.roster.push(RosterEntry {
                choice: RosterChoice::Provider(prov.clone()),
                label: prov.clone(),
                available,
                reason,
                source: RosterSource::Detected,
            });
        }
    }
    if let Some(wf) = nr.workflow {
        if wf != crate::runspec::SpecWorkflow::Arena {
            let mut sorted_roles: Vec<(&String, &String)> = proposal.roles.iter().collect();
            sorted_roles.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in sorted_roles {
                if let Some(role) = crate::state::SlotRole::from_config_key(k) {
                    if let Ok(pin) = crate::runspec::Pin::parse(v) {
                        if let Some(slot) = nr
                            .roles
                            .iter_mut()
                            .find(|r| r.role == role && r.ordinal == 0 && r.primary.is_none())
                        {
                            slot.primary = Some(pin);
                        }
                    }
                }
            }
            for (idx, v) in proposal.reviewer.iter().enumerate() {
                if let Ok(pin) = crate::runspec::Pin::parse(v) {
                    if let Some(slot) = nr.roles.iter_mut().find(|r| {
                        r.role == crate::state::SlotRole::Reviewer
                            && r.ordinal == idx
                            && r.primary.is_none()
                    }) {
                        slot.primary = Some(pin);
                    }
                }
            }
            let mut sorted_backups: Vec<(&String, &String)> = proposal.backups.iter().collect();
            sorted_backups.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in sorted_backups {
                if let Some(role) = crate::state::SlotRole::from_config_key(k) {
                    if let Ok(pin) = crate::runspec::Pin::parse(v) {
                        if let Some(slot) = nr
                            .roles
                            .iter_mut()
                            .find(|r| r.role == role && r.ordinal == 0 && r.backup.is_none())
                        {
                            slot.backup = Some(pin);
                        }
                    }
                }
            }
            for (idx, v) in proposal.reviewer_backups.iter().enumerate() {
                if let Ok(pin) = crate::runspec::Pin::parse(v) {
                    if let Some(slot) = nr.roles.iter_mut().find(|r| {
                        r.role == crate::state::SlotRole::Reviewer
                            && r.ordinal == idx
                            && r.backup.is_none()
                    }) {
                        slot.backup = Some(pin);
                    }
                }
            }
            if proposal.roles.is_empty()
                && proposal.reviewer.is_empty()
                && !proposal.providers.is_empty()
            {
                let rows: Vec<(crate::state::SlotRole, usize)> = {
                    let cfg =
                        crate::config::Config::load(&nr.project.clone().unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        }))
                        .unwrap_or_default();
                    crate::runspec::spec_rows(wf, &cfg)
                };
                for (idx, (role, ordinal)) in rows.iter().enumerate() {
                    if let Some(raw) = proposal.providers.get(idx) {
                        if let Ok(pin) = crate::runspec::Pin::parse(raw) {
                            if let Some(slot) = nr.roles.iter_mut().find(|r| {
                                r.role == *role && r.ordinal == *ordinal && r.primary.is_none()
                            }) {
                                slot.primary = Some(pin);
                            }
                        }
                    }
                }
            }
        } else {
            for (idx, raw) in proposal.providers.iter().enumerate() {
                if let Ok(pin) = crate::runspec::Pin::parse(raw) {
                    if let Some(slot) = nr.arena_pool.get_mut(idx) {
                        if slot.is_none() {
                            *slot = Some(pin);
                        }
                    }
                }
            }
        }
    }
    // Legacy providers: when workflow is unset, or when providers remain unmapped
    // after the above, retain them visibly and block launch. Never silently drop.
    if nr.workflow.is_none() && !proposal.providers.is_empty() {
        for prov in &proposal.providers {
            if !nr.legacy_providers.contains(prov) {
                nr.legacy_providers.push(prov.clone());
            }
        }
    } else if nr.workflow.is_some() && !proposal.providers.is_empty() {
        // Check if any provider remains unmapped (extra providers beyond rows, or
        // providers that would have been legacy when explicit roles present).
        // For explicit-role proposals we intentionally skip legacy, so nothing to do.
        // For positional proposals, any extra beyond rows is legacy.
        if proposal.roles.is_empty() && proposal.reviewer.is_empty() {
            let cfg = crate::config::Config::load(&nr.project.clone().unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            }))
            .unwrap_or_default();
            if let Some(wf) = nr.workflow {
                let rows = crate::runspec::spec_rows(wf, &cfg);
                for idx in rows.len()..proposal.providers.len() {
                    let prov = &proposal.providers[idx];
                    if !nr.legacy_providers.contains(prov) {
                        nr.legacy_providers.push(prov.clone());
                    }
                }
                for (idx, (role, ordinal)) in rows.iter().enumerate() {
                    if let Some(raw) = proposal.providers.get(idx) {
                        if crate::runspec::Pin::parse(raw).is_err()
                            && !nr.legacy_providers.contains(raw)
                        {
                            nr.legacy_providers.push(raw.clone());
                        } else if let Some(slot) = nr
                            .roles
                            .iter()
                            .find(|r| r.role == *role && r.ordinal == *ordinal)
                        {
                            if slot.primary.is_none()
                                && crate::runspec::Pin::parse(raw).is_ok()
                                && !nr.legacy_providers.contains(raw)
                            {
                                // Valid pin but slot already filled via explicit role would have been
                                // skipped; with explicit roles we don't push to legacy (see runspec fix).
                                // Only push when no explicit roles.
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Apply a background roster probe's result (D2) if the modal it was built for is
/// still open on the same generation; a stale result from a cancelled or reopened
/// modal is dropped.
fn apply_roster_ready(app: &mut App, gen: u64, mut roster: Vec<RosterEntry>) {
    if let Some(nr) = app.new_run.as_mut() {
        if nr.gen == gen {
            if let Some(proposal) = app.chat_pending_proposal.clone() {
                let saved_task = nr.task.clone();
                nr.roster = roster;
                apply_proposal_to_roster(&proposal, nr);
                roster = std::mem::take(&mut nr.roster);
                if saved_task.trim() != proposal.task.trim() && !saved_task.trim().is_empty() {
                    nr.task = saved_task;
                }
            }
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
    if let Some(nr) = app.new_run.as_mut() {
        if nr.editing_model {
            match code {
                KeyCode::Esc => {
                    nr.editing_model = false;
                    nr.model_buffer.clear();
                    return;
                }
                KeyCode::Enter => {
                    if nr.workflow == Some(crate::runspec::SpecWorkflow::Arena) {
                        if let Some(Some(pin)) = nr.arena_pool.get_mut(nr.role_sel) {
                            if nr.model_buffer.trim().is_empty() {
                                pin.model = None;
                            } else {
                                pin.model = Some(nr.model_buffer.trim().to_string());
                            }
                        }
                    } else if let Some(ra) = nr.roles.get_mut(nr.role_sel) {
                        let target = if nr.editing_backup {
                            &mut ra.backup
                        } else {
                            &mut ra.primary
                        };
                        if let Some(pin) = target {
                            if nr.model_buffer.trim().is_empty() {
                                pin.model = None;
                            } else {
                                let new_model = nr.model_buffer.trim().to_string();
                                pin.model = Some(new_model);
                            }
                        }
                    }
                    nr.editing_model = false;
                    nr.model_buffer.clear();
                    return;
                }
                _ => {}
            }
        }
    }
    if code == KeyCode::Esc {
        app.new_run = None;
        app.chat_pending_proposal = None;
        app.chat_pending_brief_path = None;
        return;
    }
    if code == KeyCode::Enter {
        // If a Chat proposal is pending, launch via --brief with intake_body
        if let Some(proposal) = app.chat_pending_proposal.clone() {
            // Validate modal first (same as new_run_launch does)
            let Some(nr) = app.new_run.as_ref() else {
                return;
            };
            if nr.task.trim().is_empty() {
                app.flash("task is empty", ALERT);
                return;
            }
            if nr.roster.is_empty() || nr.loading {
                app.flash("roster not ready", ALERT);
                return;
            }
            let Some(project) = nr.project.clone() else {
                app.flash("no project selected", ALERT);
                return;
            };
            let brief_path = if let Some(existing) = app.chat_pending_brief_path.clone() {
                existing
            } else {
                let swarm = SparPaths::new(&project);
                let brief_body = proposal.brief.clone();
                match crate::brief::intake_body(&swarm, &brief_body) {
                    Ok(b) => {
                        app.chat_pending_brief_path = Some(b.path.clone());
                        b.path
                    }
                    Err(e) => {
                        app.flash(format!("brief intake failed: {e:#}"), ALERT);
                        return;
                    }
                }
            };
            let cfg = crate::config::Config::load(&project).unwrap_or_else(|_| app.cfg.clone());
            let mut spec = new_run_spec(nr, &cfg);
            spec = crate::runspec::RunSpec::apply_proposal_to_spec(spec, &proposal, &cfg);
            if spec.workflow == Some(crate::runspec::SpecWorkflow::Plan) {
                spec.brief_path = Some(brief_path.clone());
            }
            if spec.task.trim().is_empty() {
                spec.task = proposal.task.clone();
            }
            if let Err(e) = spec.validate_for_launch(&cfg) {
                app.flash(e.to_string(), ALERT);
                return;
            }
            let mut args = match spec.argv(&cfg) {
                Ok(a) => a,
                Err(e) => {
                    app.flash(e.to_string(), ALERT);
                    return;
                }
            };
            let target_swarm = SparPaths::new(&project);
            if let Ok(cwd) = std::env::current_dir() {
                if let Ok(Some(base)) =
                    crate::worktree::resolve_base(&target_swarm.project_root, &cwd, None)
                {
                    args.push("--base".to_string());
                    args.push(base.reference);
                }
            }
            match spawn_detached_workflow(&target_swarm, &args, "Plan started") {
                Ok(PaletteResult::Flash(msg, color)) => {
                    app.flash(msg, color);
                    app.new_run = None;
                    app.chat_pending_proposal = None;
                    app.chat_pending_brief_path = None;
                }
                Ok(_) => {
                    app.new_run = None;
                    app.chat_pending_proposal = None;
                    app.chat_pending_brief_path = None;
                }
                Err(e) => {
                    app.flash(format!("Plan failed to start: {e:#}"), ALERT);
                    // Keep proposal and brief path so retry does not mint a duplicate
                    // brief — AC-9 forbids a second copy on a failed detached launch.
                }
            }
            return;
        }
        let Some(nr) = app.new_run.as_ref() else {
            return;
        };
        let cfg_for_launch = nr
            .project
            .as_ref()
            .and_then(|p| crate::config::Config::load(p).ok())
            .unwrap_or_else(|| app.cfg.clone());
        match new_run_launch_with_cfg(nr, &cfg_for_launch) {
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
                NewRunField::Task => NewRunField::Workflow,
                NewRunField::Workflow => NewRunField::Roles,
                NewRunField::Roles => NewRunField::Fleet,
                NewRunField::Fleet => NewRunField::Project,
            };
        }
        KeyCode::BackTab => {
            nr.field = match nr.field {
                NewRunField::Project => NewRunField::Fleet,
                NewRunField::Task => NewRunField::Project,
                NewRunField::Workflow => NewRunField::Task,
                NewRunField::Roles => NewRunField::Workflow,
                NewRunField::Fleet => NewRunField::Roles,
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
            if nr.workflow.is_some() {
                let cfg = nr
                    .project
                    .as_ref()
                    .and_then(|p| crate::config::Config::load(p).ok())
                    .unwrap_or_else(|| app.cfg.clone());
                rebuild_roles_for_workflow(nr, &cfg);
            }
        }
        KeyCode::Right if nr.field == NewRunField::Project && !nr.projects.is_empty() => {
            let i = nr
                .project
                .as_ref()
                .and_then(|p| nr.projects.iter().position(|q| q == p))
                .unwrap_or(0);
            let i = (i + 1) % nr.projects.len();
            nr.project = nr.projects.get(i).cloned();
            if nr.workflow.is_some() {
                let cfg = nr
                    .project
                    .as_ref()
                    .and_then(|p| crate::config::Config::load(p).ok())
                    .unwrap_or_else(|| app.cfg.clone());
                rebuild_roles_for_workflow(nr, &cfg);
            }
        }
        KeyCode::Char(' ') if nr.field == NewRunField::Fleet => {
            if nr.workflow.is_some() && (!nr.roles.is_empty() || !nr.arena_pool.is_empty()) {
                if let Some(entry) = nr.roster.get(nr.sel).cloned() {
                    if !entry.available {
                        app.flash(
                            format!(
                                "{} is not available: {}",
                                entry.label,
                                entry.reason.as_deref().unwrap_or("unavailable")
                            ),
                            ALERT,
                        );
                    } else if let RosterChoice::Provider(raw) = entry.choice {
                        if let Ok(pin) = crate::runspec::Pin::parse(&raw) {
                            if nr.workflow == Some(crate::runspec::SpecWorkflow::Arena) {
                                if let Some(slot) = nr.arena_pool.get_mut(nr.role_sel) {
                                    *slot = Some(pin);
                                }
                            } else if let Some(ra) = nr.roles.get_mut(nr.role_sel) {
                                if nr.editing_backup {
                                    if let Some(primary) = &ra.primary {
                                        if primary.storage_key() == pin.storage_key() {
                                            app.flash(
                                                "backup same provider as primary".to_string(),
                                                ALERT,
                                            );
                                        } else {
                                            ra.backup = Some(pin);
                                        }
                                    } else {
                                        ra.backup = Some(pin);
                                    }
                                } else {
                                    if let Some(backup) = &ra.backup {
                                        if backup.storage_key() == pin.storage_key() {
                                            app.flash(
                                                "primary same provider as backup".to_string(),
                                                ALERT,
                                            );
                                        } else {
                                            ra.primary = Some(pin);
                                        }
                                    } else {
                                        ra.primary = Some(pin);
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                toggle_roster_pick(nr, nr.sel);
            }
        }
        KeyCode::Char('j') | KeyCode::Down if nr.field == NewRunField::Fleet => {
            if !nr.roster.is_empty() {
                nr.sel = (nr.sel + 1).min(nr.roster.len() - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up if nr.field == NewRunField::Fleet => {
            nr.sel = nr.sel.saturating_sub(1);
        }
        KeyCode::Char('w') | KeyCode::Char('W') if nr.field == NewRunField::Workflow => {
            nr.workflow = match nr.workflow {
                None => Some(crate::runspec::SpecWorkflow::Plan),
                Some(crate::runspec::SpecWorkflow::Plan) => {
                    Some(crate::runspec::SpecWorkflow::Implement)
                }
                Some(crate::runspec::SpecWorkflow::Implement) => {
                    Some(crate::runspec::SpecWorkflow::Review)
                }
                Some(crate::runspec::SpecWorkflow::Review) => {
                    Some(crate::runspec::SpecWorkflow::Arena)
                }
                Some(crate::runspec::SpecWorkflow::Arena) => None,
            };
            let cfg = nr
                .project
                .as_ref()
                .and_then(|p| crate::config::Config::load(p).ok())
                .unwrap_or_else(|| app.cfg.clone());
            rebuild_roles_for_workflow(nr, &cfg);
        }
        KeyCode::Left
            if nr.field == NewRunField::Workflow && !mods.contains(KeyModifiers::CONTROL) =>
        {
            nr.workflow = match nr.workflow {
                Some(crate::runspec::SpecWorkflow::Plan) => None,
                Some(crate::runspec::SpecWorkflow::Implement) => {
                    Some(crate::runspec::SpecWorkflow::Plan)
                }
                Some(crate::runspec::SpecWorkflow::Review) => {
                    Some(crate::runspec::SpecWorkflow::Implement)
                }
                Some(crate::runspec::SpecWorkflow::Arena) => {
                    Some(crate::runspec::SpecWorkflow::Review)
                }
                None => Some(crate::runspec::SpecWorkflow::Arena),
            };
            let cfg = nr
                .project
                .as_ref()
                .and_then(|p| crate::config::Config::load(p).ok())
                .unwrap_or_else(|| app.cfg.clone());
            rebuild_roles_for_workflow(nr, &cfg);
        }
        KeyCode::Right
            if nr.field == NewRunField::Workflow && !mods.contains(KeyModifiers::CONTROL) =>
        {
            nr.workflow = match nr.workflow {
                None => Some(crate::runspec::SpecWorkflow::Plan),
                Some(crate::runspec::SpecWorkflow::Plan) => {
                    Some(crate::runspec::SpecWorkflow::Implement)
                }
                Some(crate::runspec::SpecWorkflow::Implement) => {
                    Some(crate::runspec::SpecWorkflow::Review)
                }
                Some(crate::runspec::SpecWorkflow::Review) => {
                    Some(crate::runspec::SpecWorkflow::Arena)
                }
                Some(crate::runspec::SpecWorkflow::Arena) => None,
            };
            let cfg = nr
                .project
                .as_ref()
                .and_then(|p| crate::config::Config::load(p).ok())
                .unwrap_or_else(|| app.cfg.clone());
            rebuild_roles_for_workflow(nr, &cfg);
        }
        KeyCode::Char('j') | KeyCode::Down
            if nr.field == NewRunField::Roles && !nr.editing_model =>
        {
            let max = if nr.workflow == Some(crate::runspec::SpecWorkflow::Arena) {
                nr.arena_pool.len()
            } else {
                nr.roles.len()
            };
            if max > 0 {
                nr.role_sel = (nr.role_sel + 1).min(max - 1);
            }
        }
        KeyCode::Char('k') | KeyCode::Up if nr.field == NewRunField::Roles && !nr.editing_model => {
            nr.role_sel = nr.role_sel.saturating_sub(1);
        }
        KeyCode::Char('b') | KeyCode::Char('B')
            if nr.field == NewRunField::Roles && !nr.editing_model =>
        {
            nr.editing_backup = !nr.editing_backup;
        }
        KeyCode::Char('m') | KeyCode::Char('M')
            if nr.field == NewRunField::Roles && !nr.editing_model =>
        {
            if nr.workflow == Some(crate::runspec::SpecWorkflow::Arena) {
                if let Some(slot) = nr.arena_pool.get_mut(nr.role_sel) {
                    if let Some(pin) = slot {
                        nr.model_buffer = pin.model.clone().unwrap_or_default();
                    } else {
                        nr.model_buffer.clear();
                    }
                }
            } else if let Some(ra) = nr.roles.get(nr.role_sel) {
                let target = if nr.editing_backup {
                    &ra.backup
                } else {
                    &ra.primary
                };
                if let Some(pin) = target {
                    nr.model_buffer = pin.model.clone().unwrap_or_default();
                } else {
                    nr.model_buffer.clear();
                }
            }
            nr.editing_model = true;
        }
        KeyCode::Backspace if nr.field == NewRunField::Roles => {
            if nr.editing_model {
                nr.model_buffer.pop();
            } else if nr.workflow == Some(crate::runspec::SpecWorkflow::Arena) {
                if let Some(slot) = nr.arena_pool.get_mut(nr.role_sel) {
                    *slot = None;
                }
            } else if let Some(ra) = nr.roles.get_mut(nr.role_sel) {
                if nr.editing_backup {
                    ra.backup = None;
                } else {
                    ra.primary = None;
                }
            }
        }
        KeyCode::Char(c)
            if nr.field == NewRunField::Roles
                && nr.editing_model
                && !mods.contains(KeyModifiers::CONTROL) =>
        {
            nr.model_buffer.push(c);
        }
        KeyCode::Char('d')
            if nr.field == NewRunField::Roles && mods.contains(KeyModifiers::CONTROL) =>
        {
            let spec = new_run_spec(nr, &app.cfg);
            match crate::defaults::save(&spec) {
                Ok(()) => app.flash("defaults saved".to_string(), INFO),
                Err(e) => app.flash(format!("defaults save failed: {e:#}"), ALERT),
            }
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
    home_rows: &[HomeRow],
    local_root: Option<&Path>,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &Path,
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
            match run_palette(
                app,
                swarm,
                projects,
                home_rows,
                local_root,
                runs,
                full,
                active_root,
                &input,
            ) {
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
    home_rows: &[HomeRow],
    local_root: Option<&Path>,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &Path,
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
                // Target selection goes through `browsed_project_target`, the same
                // helper `n` uses. It must be that helper and not a copy: this arm
                // previously read `swarm.project_root`, which is the *snapshot's*
                // root and lags the instant `rail_enter` updates `active_root`, so
                // `:plan` and `n` disagreed in the window before the snapshot landed
                // (review 3be317b2, after `n` was already fixed here).
                let browsed = browsed_project_target(app, projects, active_root);
                let (target, all_projects) =
                    new_run_target(app, projects, home_rows, local_root, browsed);
                app.chat_pending_proposal = None;
                app.chat_pending_brief_path = None;
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
        "msg" => {
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
                    app.select_run(i, runs);
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
    // `More` is informational ("… N more"), not a row `Enter` can act on
    // (`rail_enter`'s Home arm no-ops it) — selectable-but-inert reads as broken
    // (round-7 review finding), so it is excluded the same way a header is.
    let selectable: Vec<usize> = (0..rows.len())
        .filter(|&i| {
            !matches!(
                rows[i],
                HomeRow::Header(_)
                    | HomeRow::More { .. }
                    | HomeRow::Empty(_)
                    | HomeRow::Skeleton { .. }
            )
        })
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
            app.select_run(next, runs);
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

/// How long to wait for a Home `Enter` target before giving up on it (round-7 review
/// finding). Wall clock, not a counter: ticking per render wake burned the budget in
/// 2.5s during a slow project scan, and ticking per *installed snapshot* starved the
/// escape completely, because the refresher deliberately retains the current `Arc` when
/// nothing on disk changed — a removed run gets one forced empty snapshot and then
/// silence, pinning Main forever (review e72f434e, major). The clock itself only starts
/// once `resolve_home_target` has actually been handed a snapshot of the target's own
/// project (`resolve_home_target`'s `snapshot_for_target`): starting it on the first
/// post-`Enter` frame, whose snapshot is normally still Home's, let an unbounded
/// cross-project scan (thousands of run dirs) burn the whole budget before the target
/// project's snapshot ever landed (review 3be317b2, major). 10s is still generous
/// relative to R2's one-tick lag and Home's `CROSS_PROJECT_REFRESH` cadence once the
/// scan has actually started, so a normal drill-down never trips it.
const HOME_TARGET_GIVE_UP: Duration = Duration::from_secs(10);

/// Whether `snap` is actually a scan of `active_root`'s own runs, not merely a
/// snapshot whose root happens to equal it. Root equality alone is not sufficient
/// (round-2 review, major): `build_snapshot` only populates `runs` when the snapshot
/// was built `in_project()`, so entering a run whose project is already
/// `active_root` leaves the still-displayed Home snapshot's root matching trivially
/// on the very first post-`Enter` frame, even though that snapshot never scanned any
/// project's runs.
fn snapshot_covers_target(snap: &Snapshot, active_root: &Path) -> bool {
    snap.browse.in_project() && snap.swarm.project_root == active_root
}

/// A Home `Enter` carries a run's fold-stable `unit_id` ahead of the snapshot that
/// actually contains it (R2/AC-27): the snapshot in hand may still be Home's
/// (cross-project, `runs` empty) or a stale project's. Hold the target and only
/// clear it once a snapshot arrives whose `runs` actually contains it. Matching on
/// `unit_id` first (falling back to `id` for a row that was never folded) means a
/// unit whose loudest leg changes between the Home snapshot and the target
/// project's own snapshot is still found — matching on `id` alone would miss it,
/// since `id` follows whichever leg is currently loudest (round-11 review, major;
/// AC-28). If it never reappears — archived, its directory removed, or the whole
/// unit gone — give up after `HOME_TARGET_GIVE_UP` and return to Home rather than
/// leaving Main/Agents pinned to a ghost run forever (round-7 review finding).
/// `snapshot_for_target` (`snapshot_covers_target` at the call site) tells the
/// give-up clock whether `runs` actually came from a scan of the target's own
/// project: the first post-`Enter` frame is normally still Home's snapshot
/// (cross-project, `runs` empty), and that project's own scan is unbounded
/// (thousands of run dirs), so starting the clock before the target project has even
/// been scanned once could time out a legitimate Enter before its snapshot ever
/// lands (review 3be317b2, major). The clock only starts once a snapshot for the
/// right project has actually been seen; once started it does not reset. Returns
/// `true` when the target was found and selected this call.
fn resolve_home_target(
    app: &mut App,
    runs: &[state::RunSummary],
    snapshot_for_target: bool,
) -> bool {
    let Some(target) = app.home_target_run.clone() else {
        return false;
    };
    let found = runs
        .iter()
        .position(|r| r.unit_id.as_deref() == Some(target.as_str()))
        .or_else(|| runs.iter().position(|r| r.id == target));
    if let Some(pos) = found {
        app.selected_run = pos;
        app.selected_run_key = runs.get(pos).map(run_row_key);
        app.home_target_run = None;
        app.home_target_since = None;
        return true;
    }
    if !snapshot_for_target && app.home_target_since.is_none() {
        return false;
    }
    let since = *app.home_target_since.get_or_insert_with(Instant::now);
    if since.elapsed() > HOME_TARGET_GIVE_UP {
        app.flash(
            format!("run {target} is gone — back to Home"),
            Color::Yellow,
        );
        app.home_target_run = None;
        app.home_target_since = None;
        app.browse = BrowseLevel::Home;
    }
    false
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
                // Pin the fold-stable `unit_id`, not `id` — `id` follows whichever
                // leg is loudest and can pick a different leg by the time the target
                // project's own snapshot lands, stranding the pin on a leg that no
                // longer exists under that name (round-11 review, major; AC-28).
                app.home_target_run = Some(run.unit_id.clone().unwrap_or_else(|| run.id.clone()));
                app.home_target_since = None;
                app.browse = BrowseLevel::Agents;
                app.selected_slot = 0;
                app.reset_stream_view();
            }
            Some(HomeRow::Project(i, root)) => {
                *active_root = projects
                    .get(*i)
                    .map_or_else(|| root.clone(), |p| p.root.clone());
                app.open_project_runs();
            }
            Some(HomeRow::NewRun) => {
                // `active_root` is the rail's current browsing root, not a verified
                // project — outside a repo with an empty registry it degrades to an
                // arbitrary cwd (`run_loop`'s `active_root` init). Route through the
                // same verified `local_root` the `n` key and `open_new_run`'s own
                // `HomeScope::All` fallback use, or the no-target refusal never fires
                // (AC-32).
                open_new_run(app, projects, home_rows, local_root, None);
            }
            Some(
                HomeRow::Header(_)
                | HomeRow::More { .. }
                | HomeRow::Empty(_)
                | HomeRow::Skeleton { .. },
            )
            | None => {}
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

/// Marks the Diff tab seen whenever a click leaves it, exactly like `handle_key`
/// does for keyboard navigation — the tab strip is chrome reachable by a tap
/// (round-review finding: `open_main` from the mouse path bypassed the watermark,
/// so a mouse-only operator's `NEW` marks never cleared).
#[allow(clippy::too_many_arguments)]
fn handle_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    diff_records: &[Record],
    active_root: &mut PathBuf,
    local_root: Option<&Path>,
    rail_offset: usize,
) {
    let was_diff = app.main_tab == MainTab::Diff;
    let diff_run_id = full.map(|st| st.id.clone());
    let diff_base_commit = full.and_then(|st| st.base_commit.clone());
    let diff_slot_id = full
        .and_then(|st| st.slots.get(app.selected_slot))
        .map(|s| s.id.clone());
    handle_mouse_inner(
        app,
        m,
        swarm,
        projects,
        home_rows,
        runs,
        full,
        diff_records,
        active_root,
        local_root,
        rail_offset,
    );
    if was_diff && app.main_tab != MainTab::Diff {
        if let (Some(run_id), Some(slot_id)) = (diff_run_id, diff_slot_id) {
            mark_diff_seen(&run_id, &slot_id, diff_base_commit.as_deref(), diff_records);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_mouse_inner(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    swarm: &SparPaths,
    projects: &[registry::ProjectEntry],
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    diff_records: &[Record],
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
                app.chat_pending_proposal = None;
                app.chat_pending_brief_path = None;
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
                    let landed = rail_select(app, row, projects.len(), home_rows, runs, n_slots);
                    // Double-click = Enter: drill one level (and take over on a slot).
                    // Only when the click actually landed on content — a double-tap on
                    // a Home header/`More` row must not act on the stale selection.
                    if dbl && landed {
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
                app.scroll_main_by(3, full.is_some(), diff_records);
            } else if contains(app.rect_rail, x, y) {
                app.focus = Focus::Rail;
                rail_move(app, projects, home_rows, runs, n_slots, 1);
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(app.rect_main, x, y) {
                app.focus = Focus::Main;
                app.scroll_main_by(-3, full.is_some(), diff_records);
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
///
/// Returns whether the click actually landed on a selectable row. A double-click on a
/// header or a `More` row must not fall through to `rail_enter` against whatever the
/// cursor happened to be on before — the caller gates on this (round-9 review).
fn rail_select(
    app: &mut App,
    row: usize,
    n_projects: usize,
    home_rows: &[HomeRow],
    runs: &[state::RunSummary],
    n_slots: usize,
) -> bool {
    match app.browse {
        BrowseLevel::Home => {
            if matches!(
                home_rows.get(row),
                Some(
                    HomeRow::Header(_)
                        | HomeRow::More { .. }
                        | HomeRow::Empty(_)
                        | HomeRow::Skeleton { .. }
                )
            ) {
                return false;
            }
            if row < home_rows.len() {
                app.selected_home = row;
                app.home_key = home_rows.get(row).map(home_row_key);
                true
            } else {
                false
            }
        }
        BrowseLevel::Projects => {
            app.select_project(row, n_projects);
            true
        }
        BrowseLevel::Runs => {
            if row >= runs.len() {
                return false;
            }
            app.select_run(row, runs);
            true
        }
        BrowseLevel::Agents => {
            app.select_slot(row, n_slots);
            true
        }
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
    stream_text_raw: &str,
    log_records: &[Record],
    activity: &[Record],
    diff_text: &str,
    diff_records: &[Record],
    plan_docs: &[Record],
    review: &[Record],
    chat: &[Record],
    home: &HomeData,
    log_stats: Option<&process::StreamStats>,
    app: &mut App,
    rail_state: &mut ListState,
) {
    let area = f.area();
    // Full clear each frame — prevents styled-cell ghosting across the whole UI —
    // then paint spar's own ground over it. U12 left the page to the host theme;
    // U29 takes it back, because a raised band, a sunken output block and a fading
    // gutter all need something to be raised or sunken *from*. The cost, paid
    // knowingly, is host-theme transparency.
    f.render_widget(Clear, area);
    f.buffer_mut().set_style(area, page());

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
            && !home.rows.iter().any(|r| {
                matches!(
                    r,
                    HomeRow::Run { .. } | HomeRow::Project(..) | HomeRow::Skeleton { .. }
                )
            });
        if active == Some(true) || no_runs || empty_home {
            if app.focus == Focus::Rail {
                app.open_main(MainTab::Log);
            }
            app.narrow_autofocus_done = true;
        }
    }

    // A level whose strip does not carry the active tab would light none of them
    // and leave `[`/`]` starting from an index that is not on screen. Snap first.
    let tabs = tabs_for(app.browse);
    if !tabs.contains(&app.main_tab) {
        app.main_tab = tabs[0];
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
            draw_labels(f, &lay, swarm, projects, runs, full, app);
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
            projects,
            full,
            stream_text,
            stream_text_raw,
            log_records,
            activity,
            diff_text,
            diff_records,
            plan_docs,
            review,
            chat,
            home,
            log_stats,
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
    tabs_for(app.browse)
        .iter()
        .map(|t| {
            // Every tab reserves the same 4-column badge slot, blank unless it is
            // Activity with something to say: Activity is second of six, so a badge
            // that changed width would shift Diff/Plan/Review/Shell out from under a
            // click (U11) — and a slot reserved on one tab only would make the gap
            // either side of it uneven with every other tab-to-tab gap.
            let badge = if *t == MainTab::Activity {
                match app.human_alerts_n {
                    0 => "    ".to_string(),
                    n => format!(" ⚠{:<2}", n.min(99)),
                }
            } else {
                "    ".to_string()
            };
            let text = format!("  {}{badge}  ", t.label_at(app.browse));
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

/// Narrow strip label (U35): the two-tab Home strip keeps its full labels (they
/// already fit); the seven-tab run/project strip abbreviates uniformly so widening a
/// badge or a label never moves the strip (U11).
fn narrow_label(t: MainTab, browse: BrowseLevel) -> &'static str {
    if browse == BrowseLevel::Home {
        t.label_at(browse)
    } else {
        t.short_label()
    }
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
    app: &mut App,
) {
    app.main_tabs.clear();
    app.main_tab_glyphs.clear();
    let area = lay.labels;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let now = Instant::now();

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
        let raw: Vec<(MainTab, &str, Style)> = tabs_for(app.browse)
            .iter()
            .map(|t| {
                let style = if *t == app.main_tab {
                    Style::default().fg(ACCENT).bold()
                } else if *t == MainTab::Activity && app.human_alerts_n > 0 {
                    Style::default().fg(ALERT).bold()
                } else {
                    dim()
                };
                (*t, narrow_label(*t, app.browse), style)
            })
            .collect();
        let n = raw.len() as u16;
        let plain_total: u16 = raw.iter().map(|(_, l, _)| l.chars().count() as u16).sum();
        let reserved = plain_total + badge_w * n;
        let reserve_all = n > 1 && reserved <= area.width;
        // Six tabs (U9/U35) leave less spare width than four did: the full badge
        // glued to Activity alone can still starve the last tab to zero at the
        // narrowest widths this strip renders at. A minimal one-character badge is
        // the last fallback before that — every tab keeps a slot, at the cost of
        // the exact alert count (round-13 review: silently dropping a tab is worse
        // than a coarser badge).
        let label_total_full_badge = plain_total + badge_w;
        let fits_full_badge = label_total_full_badge <= area.width;
        let minimal_badge_w: u16 = if app.human_alerts_n > 0 { 1 } else { 0 };
        let label_total_min_badge = plain_total + minimal_badge_w;
        let (gap, use_minimal_badge) = if n <= 1 {
            (0, false)
        } else if reserve_all {
            ((area.width - reserved) / (n - 1), false)
        } else if fits_full_badge {
            (
                area.width.saturating_sub(label_total_full_badge) / (n - 1),
                false,
            )
        } else {
            (
                area.width.saturating_sub(label_total_min_badge) / (n - 1),
                true,
            )
        };
        let tabs: Vec<(MainTab, String, Style)> = raw
            .into_iter()
            .map(|(t, label, style)| {
                let is_alert_tab = t == MainTab::Activity && app.human_alerts_n > 0;
                let badge = if is_alert_tab && use_minimal_badge {
                    "!".to_string()
                } else if is_alert_tab {
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
        // Label glyph rects first, `gap` columns of dead space between each pair —
        // then a second pass below pads each hit rect out into half of each
        // neighboring gap, so a tap anywhere on the strip lands on a tab (U11's touch
        // requirement) rather than only on the glyphs themselves.
        let mut new_glyphs: Vec<(MainTab, Rect)> = Vec::new();
        let mut new_hits: Vec<(MainTab, Rect)> = Vec::new();
        let mut glyph_texts: Vec<(MainTab, String, Style)> = Vec::new();
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
            let gx = x;
            let gw = w;
            glyph_texts.push((tab, text.clone(), style));
            new_glyphs.push((
                tab,
                Rect {
                    x: gx,
                    y: area.y,
                    width: gw,
                    height: 1,
                },
            ));
            x = x.saturating_add(w);
            if i + 1 < n_tabs {
                x = x.saturating_add(gap);
            }
        }
        // Hit rects: pad into half gaps. Each hit rect covers its glyph plus half
        // the gap on either side, tiling edge to edge with zero dead columns.
        let n_glyphs = new_glyphs.len();
        for (i, (tab, glyph)) in new_glyphs.iter().enumerate() {
            let gx = glyph.x;
            let gw = glyph.width;
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
            new_hits.push((
                *tab,
                Rect {
                    x: left,
                    y: area.y,
                    width: right.saturating_sub(left),
                    height: 1,
                },
            ));
        }
        let (glyphs, hits) = app.tab_strip.observe(new_glyphs, new_hits, now);
        for (i, ((tab, glyph), (_, hit))) in glyphs.iter().zip(hits.iter()).enumerate() {
            let (_, text, style) = &glyph_texts[i];
            let fitted = truncate(text, glyph.width as usize);
            f.render_widget(Paragraph::new(Span::styled(fitted, *style)), *glyph);
            app.main_tabs.push((*hit, *tab));
            app.main_tab_glyphs.push((*glyph, *tab));
        }
        return;
    }

    // Wide layout: Main's tabs sit on the labels row, aligned to Main's column.
    // Compute new placement first without side effects
    let main = Rect {
        y: area.y,
        height: 1,
        ..lay.main
    };
    if main.width == 0 {
        return;
    }
    let full_tab_spans = main_tab_spans(app);
    let full_w: u16 = full_tab_spans
        .iter()
        .map(|(_, t, _)| t.chars().count() as u16)
        .sum();
    let tab_spans: Vec<(MainTab, String, Style)> = if full_w <= main.width {
        full_tab_spans
    } else {
        let tabs = tabs_for(app.browse);
        let n = tabs.len() as u16;
        let plain_total: u16 = tabs
            .iter()
            .map(|t| t.short_label().chars().count() as u16 + 2)
            .sum();
        let badge_w: u16 = if plain_total + 4 * n <= main.width {
            4
        } else if plain_total + n <= main.width {
            1
        } else {
            0
        };
        tabs.iter()
            .map(|t| {
                let style = if *t == app.main_tab {
                    Style::default().fg(ACCENT).bold()
                } else if *t == MainTab::Activity && app.human_alerts_n > 0 {
                    Style::default().fg(ALERT).bold()
                } else {
                    dim()
                };
                let label = t.short_label();
                let is_alert_tab = *t == MainTab::Activity && app.human_alerts_n > 0;
                let badge = match (badge_w, is_alert_tab) {
                    (4, true) => format!(" ⚠{:<2}", app.human_alerts_n.min(99)),
                    (1, true) => "!".to_string(),
                    (w, _) => " ".repeat(w as usize),
                };
                let text = format!(" {label}{badge} ");
                (*t, text, style)
            })
            .collect()
    };
    let mut new_glyphs: Vec<(MainTab, Rect)> = Vec::new();
    let mut new_hits: Vec<(MainTab, Rect)> = Vec::new();
    let mut x = main.x;
    let mut glyph_texts: Vec<(MainTab, String, Style)> = Vec::new();
    for (tab, text, style) in tab_spans {
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
        new_glyphs.push((tab, rect));
        new_hits.push((tab, rect));
        glyph_texts.push((tab, text, style));
        x = x.saturating_add(w);
    }
    let (glyphs, hits) = app.tab_strip.observe(new_glyphs, new_hits, now);
    let is_animating = app.tab_strip.is_animating(now);
    let rail_overlapped = is_animating && glyphs.iter().any(|(_, r)| r.x < lay.rail.right());
    if lay.rail.width > 0 && !rail_overlapped {
        let title = rail_title(projects, runs, full, app);
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
    for (i, ((tab, glyph), (_, hit))) in glyphs.iter().zip(hits.iter()).enumerate() {
        let (_, text, style) = &glyph_texts[i];
        let fitted = truncate(text, glyph.width as usize);
        f.render_widget(Paragraph::new(Span::styled(fitted, *style)), *glyph);
        app.main_tabs.push((*hit, *tab));
        app.main_tab_glyphs.push((*glyph, *tab));
    }
    // What the active tab is showing, parked on the right so the tabs never move.
    let ctx = main_context(swarm, full, app);
    let used = glyphs.iter().map(|(_, r)| r.width).sum::<u16>();
    let room = main.width.saturating_sub(used).saturating_sub(1);
    if !ctx.is_empty() && room > 2 {
        let text = truncate(&ctx, room as usize);
        let w = text.chars().count() as u16;
        let caption_rect = Rect {
            x: main.right().saturating_sub(w + 1),
            y: area.y,
            width: w,
            height: 1,
        };
        let caption_overlapped = is_animating;
        if !caption_overlapped {
            f.render_widget(Paragraph::new(Span::styled(text, muted())), caption_rect);
        }
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
    if width == 0 {
        return Vec::new();
    }
    if width == 1 {
        let style = spans.first().map(|s| s.style).unwrap_or_default();
        return vec![Span::styled("…".to_string(), style)];
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
            out.push(Span::styled(truncate(&span.content, room), span.style));
        } else if room == 1 {
            out.push(Span::styled("…".to_string(), span.style));
        } else if let Some(last) = out.last_mut() {
            let content = last.content.to_string();
            let mut chars: Vec<char> = content.chars().collect();
            if !chars.is_empty() {
                chars.pop();
                chars.push('…');
                let new_content: String = chars.into_iter().collect();
                let style = last.style;
                *last = Span::styled(new_content, style);
            } else {
                out.push(Span::styled("…".to_string(), span.style));
            }
        } else {
            out.push(Span::styled("…".to_string(), span.style));
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

    // The built-in suite channel (`[suite].command`, O54) runs under the orchestrator and
    // fills no slot, so the honest record for that step is the run's own verdict. Without
    // this the rail marks a suite that ran and gated the ship as `Skipped`.
    if !st.slots.iter().any(|s| s.role == SlotRole::Tester) {
        if let Some(i) = steps_for(st.workflow)
            .iter()
            .position(|(_, role)| *role == Some(SlotRole::Tester))
        {
            // Phase first, verdict second. `suite_outcome` is never cleared between
            // rounds, so reading it first would paint round 2's live suite with round 1's
            // red and the step could only ever show `Active` on the first round.
            out[i].1 = if st.phase == Phase::Suite {
                if broken {
                    StepState::Failed
                } else if halted {
                    StepState::Halted
                } else {
                    StepState::Active
                }
            } else {
                match st.suite_outcome {
                    Some(crate::state::SuiteOutcome::Pass) => StepState::Done,
                    Some(_) => StepState::Failed,
                    None => out[i].1,
                }
            };
        }
    }

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

const METER_ZONE_W: u16 = 34;
/// The stepper's floor, and the number AC-8 is written against: the meter slot is
/// affordable at exactly `METER_ZONE_W + STEPPER_MIN_W`. The separator column comes
/// out of the stepper's own share (`pad.width - METER_ZONE_W - 1`), not out of the
/// affordability test — writing it into the predicate instead made the frozen
/// criterion false and was then "fixed" by editing the contract, which is the one
/// repair that is never available.
const STEPPER_MIN_W: u16 = 9;

fn meter_zone(pad: Rect) -> Option<Rect> {
    if pad.width < METER_ZONE_W + STEPPER_MIN_W {
        return None;
    }
    Some(Rect {
        x: pad.right().saturating_sub(METER_ZONE_W),
        y: pad.y,
        width: METER_ZONE_W,
        height: 1,
    })
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
        // Loading is not an empty state: before the scan lands, "0 projects" is
        // a claim made before anyone has looked. Reserve the band the way the
        // rows are reserved — a scanning shimmer, not a zero count.
        if home.loading {
            let text = "scanning projects…";
            let spans = if app.animated {
                sweep_spans(
                    text,
                    PULSE_LO,
                    PULSE_HI,
                    app.clock.cycle(crate::motion::SWEEP_PERIOD),
                )
            } else {
                vec![Span::styled(
                    text.to_string(),
                    Style::default().fg(PULSE_LO),
                )]
            };
            f.render_widget(Paragraph::new(Line::from(spans)), pad);
            return;
        }
        // Deliberately not the ⚑ roll-up (header chip) or the scope (rail title):
        // this row carries the portfolio totals, which are the one Home fact no
        // other surface has.
        let running = home_band_count(&home.rows, HomeBand::Running);
        let finished = home_band_count(&home.rows, HomeBand::Finished);
        let n_projects = home.project_stats.len();
        let n_runs: usize = home.project_stats.iter().map(|p| p.n_runs).sum();
        let line = Line::from(vec![
            Span::styled(format!("{n_projects} projects"), dim()),
            Span::styled(" · ", muted()),
            Span::styled(format!("{n_runs} runs"), dim()),
            Span::styled(" · ", muted()),
            Span::styled(
                format!("{running} running"),
                Style::default().fg(if running > 0 { INFO } else { FG_MUTED }),
            ),
            Span::styled(" · ", muted()),
            Span::styled(
                format!(
                    "{finished} finished since {}",
                    relative_age(app.home_watermark)
                ),
                dim(),
            ),
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
        let running = runs.iter().filter(|r| is_active_phase(r.phase)).count();
        let line = if runs.is_empty() {
            Line::from(Span::styled(
                "no runs yet — n from Home starts one",
                muted(),
            ))
        } else {
            // No ⚑ term: the header chip carries the roll-up in every view (U31).
            Line::from(vec![
                Span::styled(format!("{} runs", runs.len()), dim()),
                Span::styled(" · ", muted()),
                Span::styled(
                    format!("{running} running"),
                    Style::default().fg(if running > 0 { INFO } else { FG_MUTED }),
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
    if billed > 0 {
        meters.push(Span::styled(" · ", muted()));
        meters.push(Span::styled(
            format!("billed {}", compact_u64(billed)),
            dim(),
        ));
    }
    // A unit of work says how much of it there is: rounds it has been through, and
    // how many run ids it folds in (U15). Billed is placed before round/legs so a
    // truncation at the meter zone keeps the token count — the one meter that moves
    // mid-run — rather than hiding it behind a less critical term.
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
    let meters_w: u16 = meters
        .iter()
        .map(|s| s.content.chars().count() as u16)
        .sum();

    // Fixed slot for meters: the stepper's width is a pure function of pad width,
    // not of billed token count, so the layout never moves as billed ticks up.
    // A separator column between stepper and zone prevents abutting glyphs when
    // the meter line fills the whole zone (34 columns).
    let zone = meter_zone(pad);
    let (room, meters) = if let Some(_z) = zone {
        let r = pad.width.saturating_sub(METER_ZONE_W + 1);
        // Fit meters into the zone, truncating with marker if needed.
        let fitted = fit_spans(meters, METER_ZONE_W);
        (r, fitted)
    } else {
        match pad.width.checked_sub(meters_w + 2) {
            Some(w) if w >= 8 => (w, meters),
            _ => (pad.width, Vec::new()),
        }
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
        if let Some(z) = zone {
            f.render_widget(
                Paragraph::new(Line::from(meters)).alignment(Alignment::Right),
                z,
            );
        } else {
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
        // With gates pending, the right-hand chip on this very row already flies
        // `⚑N need you · a`. An inline cue repeating it made the roll-up appear
        // twice in one row and four times in three (U31). Silence is the cue.
        return if need > 0 {
            (String::new(), WARN, None)
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
const GATE_ZONE_MIN_LEFT: u16 = 12;

/// The gate zone: a fixed slot, or `None` on a phone-width screen that cannot spare
/// one (there the buttons fall back to right-aligned, the old behaviour).
fn gate_zone(area: Rect) -> Option<Rect> {
    if area.width < GATE_ZONE_W + GATE_ZONE_MIN_LEFT {
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

    // At Home scoped to every project, `swarm`/`active_root` still names whichever
    // project the rail happens to be sitting on internally — showing it here would
    // claim a scope the context band right below (`draw_context_band`) does not
    // honour (round-7 review finding). Name the scope instead.
    let project = if app.browse == BrowseLevel::Home && app.home_scope == HomeScope::All {
        "all projects".to_string()
    } else {
        swarm
            .project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(".")
            .to_string()
    };

    let mut spans = vec![
        Span::styled(" spar ", chip(ACCENT)),
        Span::styled(
            format!("  {}", truncate(&project, 20)),
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
    // The zone is reserved whenever the width affords it, independent of
    // whether a gate is live — otherwise the ⚑/⚠ chips slide 23 columns
    // the instant a run enters a gate and a click on its way lands on empty
    // header. Without a reserved zone (phone width) the buttons overpaint
    // whatever is beneath them, so the breadcrumb has to stop before they
    // start — otherwise it is not clipped, it is buried, and it loses even
    // its ellipsis.
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
    app: &App,
) -> String {
    let base = match app.browse {
        // Not the ⚑ roll-up: the header chip already flies that in every view, and
        // three surfaces saying "9 need you" in three consecutive rows is what this
        // row used to be part of. The rail's own fact is what it is listing.
        BrowseLevel::Home => format!("HOME  {}", home_scope_label(&app.home_scope)),
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

/// The rail: one drill-down tree (`Home ▸ runs ▸ agents`, with `Projects` reachable
/// from Home for a specific-project jump), never a stack of co-equal panels. `Enter`
/// pushes a level, `Esc` pops one. No border: its title rides the labels row and the
/// seam separates it from Main.
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

/// The lead columns: the selection bar, then the state marker. Two cells, both fixed,
/// because they are independent facts — one column for both meant that on a project
/// where every run wants you (biddesk: 12 of 13) the cursor was invisible.
///
/// The second cell carries one of two mutually exclusive facts, in this order:
/// a `⚑` when the row wants the operator, or a breathing [`LIVE_BAR`] when it is
/// working (U30). Attention outranks activity — a row that both wants you and is
/// moving is a row you should look at, and a pulse there would read as "fine".
fn rail_lead(
    sel: bool,
    focused: bool,
    flag: Option<Color>,
    live: Option<Color>,
) -> Vec<Span<'static>> {
    vec![
        if sel {
            Span::styled(
                SEL_BAR,
                Style::default().fg(if focused { ACCENT } else { FG_MUTED }),
            )
        } else {
            Span::raw(" ")
        },
        match (flag, live) {
            (Some(c), _) => Span::styled("⚑", Style::default().fg(c).bold()),
            (None, Some(c)) => Span::styled(LIVE_BAR, Style::default().fg(c)),
            (None, None) => Span::raw(" "),
        },
    ]
}

/// A label under a travelling highlight: one span per cell, ramped from `base` to
/// `peak` (U30). This is for work that is **dispatched but not yet producing** —
/// the state a breathing gutter would over-claim, because nothing is moving yet.
/// A sweep says "queued and alive"; the gutter says "working".
fn sweep_spans(text: &str, base: Color, peak: Color, phase: f32) -> Vec<Span<'static>> {
    let n = text.chars().count();
    text.chars()
        .enumerate()
        .map(|(i, ch)| {
            let t = crate::motion::sweep(i, n, SWEEP_HALF, phase);
            Span::styled(ch.to_string(), Style::default().fg(lerp(base, peak, t)))
        })
        .collect()
}

/// Half-width of the sweep's highlight, in cells. Wide enough to read as a
/// gradient rather than a moving dot, narrow enough that a nine-column role label
/// is never lit end to end.
const SWEEP_HALF: f32 = 3.5;

const RAIL_TRAVEL_MAX: f32 = 12.0;
const STRIP_JUMP_MIN: u16 = 3;

/// Rail reorder animator: adjacent swaps, not rank interpolation. A row is one
/// cell tall, so travel means moving through intervening ranks over
/// REORDER_PERIOD.
struct RailMotion {
    level: Option<BrowseLevel>,
    target_keys: Vec<String>,
    displayed_keys: Vec<String>,
    swaps: Vec<usize>,
    applied: usize,
    tween: crate::motion::Tween<f32>,
}

impl RailMotion {
    fn new() -> Self {
        Self {
            level: None,
            target_keys: Vec::new(),
            displayed_keys: Vec::new(),
            swaps: Vec::new(),
            applied: 0,
            tween: crate::motion::Tween::<f32>::settled(0.0),
        }
    }

    fn is_animating(&self, now: Instant) -> bool {
        let _ = now;
        self.applied < self.swaps.len()
    }

    fn settle(&mut self) {
        if !self.swaps.is_empty() {
            for idx in self.applied..self.swaps.len() {
                let i = self.swaps[idx];
                if i + 1 < self.displayed_keys.len() {
                    self.displayed_keys.swap(i, i + 1);
                }
            }
            self.applied = self.swaps.len();
        }
        self.tween.settle();
        if !self.target_keys.is_empty() {
            self.displayed_keys = self.target_keys.clone();
        }
        self.swaps.clear();
        self.applied = 0;
    }

    fn commit(&mut self, now: Instant) {
        if self.swaps.is_empty() {
            return;
        }
        let eased = self.tween.value(now).clamp(0.0, 1.0);
        let mut desired = (eased * self.swaps.len() as f32).ceil() as usize;
        if desired > self.swaps.len() {
            desired = self.swaps.len();
        }
        if eased < 1.0 && desired == self.swaps.len() {
            desired = self.swaps.len() - 1;
        }
        while self.applied < desired {
            let i = self.swaps[self.applied];
            if i + 1 < self.displayed_keys.len() {
                self.displayed_keys.swap(i, i + 1);
            }
            self.applied += 1;
        }
    }

    fn observe(
        &mut self,
        level: BrowseLevel,
        keys: Vec<String>,
        now: Instant,
    ) -> Option<Vec<usize>> {
        self.commit(now);
        let level_changed = self.level != Some(level);
        let target_changed = self.target_keys != keys;
        if level_changed || target_changed {
            if level_changed {
                self.displayed_keys = keys.clone();
                self.target_keys = keys.clone();
                self.swaps.clear();
                self.applied = 0;
                self.tween = crate::motion::Tween::<f32>::settled(1.0);
                self.level = Some(level);
                return None;
            }
            if level == BrowseLevel::Home {
                let is_run = |k: &String| k.starts_with("run:");
                let mut rebased: Vec<Option<String>> = vec![None; keys.len()];
                let mut snapped: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut home_pos: Vec<usize> = Vec::new();
                for (idx, key) in keys.iter().enumerate() {
                    if is_run(key) {
                        home_pos.push(idx);
                    } else {
                        rebased[idx] = Some(key.clone());
                    }
                }
                for (idx, key) in keys.iter().enumerate() {
                    if !is_run(key) {
                        continue;
                    }
                    if let Some(pos) = self.displayed_keys.iter().position(|k| k == key) {
                        let dist = (idx as f32 - pos as f32).abs();
                        if dist > RAIL_TRAVEL_MAX {
                            snapped.insert(key.clone());
                            rebased[idx] = Some(key.clone());
                        }
                    } else {
                        snapped.insert(key.clone());
                        rebased[idx] = Some(key.clone());
                    }
                }
                let target_set: std::collections::HashSet<String> =
                    keys.iter().filter(|k| is_run(k)).cloned().collect();
                let remaining_displayed: Vec<String> = self
                    .displayed_keys
                    .iter()
                    .filter(|k| is_run(k) && target_set.contains(*k) && !snapped.contains(*k))
                    .cloned()
                    .collect();
                let remaining_slots: Vec<usize> = home_pos
                    .iter()
                    .copied()
                    .filter(|idx| rebased[*idx].is_none())
                    .collect();
                let mut rem_idx = 0;
                for &slot in &remaining_slots {
                    if rem_idx < remaining_displayed.len() {
                        rebased[slot] = Some(remaining_displayed[rem_idx].clone());
                        rem_idx += 1;
                    }
                }
                // Duplicate keys or other fill shortfall would leave holes and panic on
                // unwrap. Pad any remaining holes from the target order rather than
                // panicking; the invariant "rail keys are unique" is best-effort here.
                if rebased.iter().any(|o| o.is_none()) {
                    let placed: std::collections::HashSet<String> =
                        rebased.iter().filter_map(|o| o.clone()).collect();
                    for &slot in &remaining_slots {
                        if rebased[slot].is_none() {
                            if let Some(k) = keys
                                .iter()
                                .filter(|k| is_run(k) && !placed.contains(*k))
                                .find(|k| !rebased.iter().any(|o| o.as_ref() == Some(*k)))
                            {
                                rebased[slot] = Some(k.clone());
                            }
                        }
                    }
                }
                // Any still-None would be a duplicate-key invariant violation; bail
                // to a settled state rather than panic in the render loop.
                if rebased.iter().any(|o| o.is_none()) {
                    self.displayed_keys = keys.clone();
                    self.target_keys = keys.clone();
                    self.swaps.clear();
                    self.applied = 0;
                    self.tween = crate::motion::Tween::<f32>::settled(1.0);
                    return None;
                }
                let new_displayed: Vec<String> = rebased.into_iter().map(|o| o.unwrap()).collect();
                // Build fully adjacent swaps over terminal rows: each swap
                // exchanges index i with i+1, so a run traveling across a band
                // header moves the header one step the other way. This is the
                // physical adjacency the correction requires.
                let mut cur = new_displayed.clone();
                let mut swaps: Vec<usize> = Vec::new();
                for target_idx in 0..keys.len() {
                    if cur[target_idx] == keys[target_idx] {
                        continue;
                    }
                    if let Some(pos) = cur.iter().position(|k| k == &keys[target_idx]) {
                        let mut p = pos;
                        while p > target_idx {
                            swaps.push(p - 1);
                            cur.swap(p - 1, p);
                            p -= 1;
                        }
                    }
                }
                self.displayed_keys = new_displayed;
                self.target_keys = keys;
                self.swaps = swaps;
                self.applied = 0;
                self.level = Some(level);
                if self.swaps.is_empty() {
                    self.tween = crate::motion::Tween::<f32>::settled(1.0);
                    return None;
                }
                self.tween = crate::motion::Tween::<f32>::settled(0.0);
                self.tween.retarget(1.0, crate::motion::REORDER_PERIOD, now);
            } else {
                // Runs (and other) — all keys are movable.
                let mut rebased: Vec<Option<String>> = vec![None; keys.len()];
                let mut snapped: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for (idx, key) in keys.iter().enumerate() {
                    if let Some(pos) = self.displayed_keys.iter().position(|k| k == key) {
                        let dist = (idx as f32 - pos as f32).abs();
                        if dist > RAIL_TRAVEL_MAX {
                            snapped.insert(key.clone());
                            rebased[idx] = Some(key.clone());
                        }
                    } else {
                        snapped.insert(key.clone());
                        rebased[idx] = Some(key.clone());
                    }
                }
                let remaining: Vec<String> = self
                    .displayed_keys
                    .iter()
                    .filter(|k| !snapped.contains(*k) && keys.contains(k))
                    .cloned()
                    .collect();
                let mut rem_idx = 0;
                for slot in rebased.iter_mut() {
                    if slot.is_none() && rem_idx < remaining.len() {
                        *slot = Some(remaining[rem_idx].clone());
                        rem_idx += 1;
                    }
                }
                if rebased.iter().any(|o| o.is_none()) {
                    let placed: std::collections::HashSet<String> =
                        rebased.iter().filter_map(|o| o.clone()).collect();
                    for slot in rebased.iter_mut() {
                        if slot.is_none() {
                            if let Some(k) = keys.iter().find(|k| !placed.contains(*k)) {
                                *slot = Some(k.clone());
                            }
                        }
                    }
                }
                if rebased.iter().any(|o| o.is_none()) {
                    self.displayed_keys = keys.clone();
                    self.target_keys = keys.clone();
                    self.swaps.clear();
                    self.applied = 0;
                    self.tween = crate::motion::Tween::<f32>::settled(1.0);
                    return None;
                }
                let new_displayed: Vec<String> = rebased.into_iter().map(|o| o.unwrap()).collect();
                let mut cur = new_displayed.clone();
                let mut swaps: Vec<usize> = Vec::new();
                for target_idx in 0..keys.len() {
                    if cur[target_idx] == keys[target_idx] {
                        continue;
                    }
                    if let Some(pos) = cur.iter().position(|k| k == &keys[target_idx]) {
                        let mut p = pos;
                        while p > target_idx {
                            swaps.push(p - 1);
                            cur.swap(p - 1, p);
                            p -= 1;
                        }
                    }
                }
                self.displayed_keys = new_displayed;
                self.target_keys = keys;
                self.swaps = swaps;
                self.applied = 0;
                self.level = Some(level);
                if self.swaps.is_empty() {
                    self.tween = crate::motion::Tween::<f32>::settled(1.0);
                    return None;
                }
                self.tween = crate::motion::Tween::<f32>::settled(0.0);
                self.tween.retarget(1.0, crate::motion::REORDER_PERIOD, now);
            }
            // Fall through to compute current permutation after retarget (still at start, no swaps applied yet)
        }
        if self.displayed_keys == self.target_keys {
            return None;
        }
        let mut perm: Vec<usize> = Vec::with_capacity(self.displayed_keys.len());
        let mut seen = std::collections::HashSet::new();
        for k in &self.displayed_keys {
            let pos = self.target_keys.iter().position(|t| t == k)?;
            if !seen.insert(pos) {
                return None;
            }
            perm.push(pos);
        }
        // Check if identity (should have returned None earlier, but handle)
        let is_identity = perm.iter().enumerate().all(|(i, &v)| i == v);
        if is_identity {
            return None;
        }
        Some(perm)
    }
}

struct TabStripMotion {
    from_glyphs: Vec<(MainTab, Rect)>,
    from_hits: Vec<(MainTab, Rect)>,
    to_glyphs: Vec<(MainTab, Rect)>,
    to_hits: Vec<(MainTab, Rect)>,
    tween: crate::motion::Tween<f32>,
}

impl TabStripMotion {
    fn new() -> Self {
        Self {
            from_glyphs: Vec::new(),
            from_hits: Vec::new(),
            to_glyphs: Vec::new(),
            to_hits: Vec::new(),
            tween: crate::motion::Tween::<f32>::settled(0.0),
        }
    }

    fn is_animating(&self, now: Instant) -> bool {
        !self.tween.done(now)
    }

    fn settle(&mut self) {
        self.tween.settle();
        if !self.to_glyphs.is_empty() {
            self.from_glyphs = self.to_glyphs.clone();
            self.from_hits = self.to_hits.clone();
        }
    }

    #[allow(clippy::type_complexity)]
    fn displayed(&self, now: Instant) -> (Vec<(MainTab, Rect)>, Vec<(MainTab, Rect)>) {
        if self.from_glyphs.is_empty() || self.tween.done(now) {
            return (self.to_glyphs.clone(), self.to_hits.clone());
        }
        let t = self.tween.value(now).clamp(0.0, 1.0);
        let mut glyphs = Vec::with_capacity(self.from_glyphs.len());
        for ((tab, from), (_, to)) in self.from_glyphs.iter().zip(self.to_glyphs.iter()) {
            let from_right = from.x as f32 + from.width as f32;
            let to_right = to.x as f32 + to.width as f32;
            let x = (from.x as f32 + (to.x as f32 - from.x as f32) * t)
                .round()
                .clamp(0.0, 65535.0) as u16;
            let right = (from_right + (to_right - from_right) * t)
                .round()
                .clamp(0.0, 65535.0) as u16;
            let w = right.saturating_sub(x);
            let y = to.y;
            let h = to.height;
            glyphs.push((
                *tab,
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
            ));
        }
        let mut hits = Vec::with_capacity(self.from_hits.len());
        for ((tab, from), (_, to)) in self.from_hits.iter().zip(self.to_hits.iter()) {
            let from_right = from.x as f32 + from.width as f32;
            let to_right = to.x as f32 + to.width as f32;
            let x = (from.x as f32 + (to.x as f32 - from.x as f32) * t)
                .round()
                .clamp(0.0, 65535.0) as u16;
            let right = (from_right + (to_right - from_right) * t)
                .round()
                .clamp(0.0, 65535.0) as u16;
            let w = right.saturating_sub(x);
            let y = to.y;
            let h = to.height;
            hits.push((
                *tab,
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
            ));
        }
        for (g, h) in glyphs.iter_mut().zip(hits.iter()) {
            let g_rect = &mut g.1;
            let h_rect = &h.1;
            if g_rect.x < h_rect.x {
                let diff = h_rect.x - g_rect.x;
                g_rect.x = h_rect.x;
                g_rect.width = g_rect.width.saturating_sub(diff);
            }
            let g_right = g_rect.x.saturating_add(g_rect.width);
            let h_right = h_rect.x.saturating_add(h_rect.width);
            if g_right > h_right {
                g_rect.width = h_right.saturating_sub(g_rect.x);
            }
        }
        (glyphs, hits)
    }

    #[allow(clippy::type_complexity)]
    fn observe(
        &mut self,
        new_glyphs: Vec<(MainTab, Rect)>,
        new_hits: Vec<(MainTab, Rect)>,
        now: Instant,
    ) -> (Vec<(MainTab, Rect)>, Vec<(MainTab, Rect)>) {
        if self.to_glyphs.is_empty() {
            self.from_glyphs = new_glyphs.clone();
            self.to_glyphs = new_glyphs.clone();
            self.from_hits = new_hits.clone();
            self.to_hits = new_hits.clone();
            self.tween = crate::motion::Tween::<f32>::settled(1.0);
            return (new_glyphs, new_hits);
        }
        let same_tabs = self.to_glyphs.len() == new_glyphs.len()
            && self
                .to_glyphs
                .iter()
                .zip(new_glyphs.iter())
                .all(|((a, _), (b, _))| a == b);
        if !same_tabs {
            self.from_glyphs = new_glyphs.clone();
            self.to_glyphs = new_glyphs.clone();
            self.from_hits = new_hits.clone();
            self.to_hits = new_hits.clone();
            self.tween = crate::motion::Tween::<f32>::settled(1.0);
            return (new_glyphs, new_hits);
        }
        let same_geometry = self
            .to_glyphs
            .iter()
            .zip(new_glyphs.iter())
            .all(|((_, a), (_, b))| a == b)
            && self
                .to_hits
                .iter()
                .zip(new_hits.iter())
                .all(|((_, a), (_, b))| a == b);
        if same_geometry {
            return self.displayed(now);
        }
        let (disp_glyphs, disp_hits) = self.displayed(now);
        let mut max_delta: u16 = 0;
        for ((_, new_g), (_, disp_g)) in new_glyphs.iter().zip(disp_glyphs.iter()) {
            max_delta = max_delta.max(new_g.x.abs_diff(disp_g.x));
            max_delta = max_delta.max(new_g.width.abs_diff(disp_g.width));
        }
        for ((_, new_h), (_, disp_h)) in new_hits.iter().zip(disp_hits.iter()) {
            max_delta = max_delta.max(new_h.x.abs_diff(disp_h.x));
            max_delta = max_delta.max(new_h.width.abs_diff(disp_h.width));
        }
        if max_delta < STRIP_JUMP_MIN {
            self.from_glyphs = new_glyphs.clone();
            self.to_glyphs = new_glyphs.clone();
            self.from_hits = new_hits.clone();
            self.to_hits = new_hits.clone();
            self.tween = crate::motion::Tween::<f32>::settled(1.0);
            return (new_glyphs, new_hits);
        }
        self.from_glyphs = disp_glyphs;
        self.from_hits = disp_hits;
        self.to_glyphs = new_glyphs;
        self.to_hits = new_hits;
        self.tween = crate::motion::Tween::<f32>::settled(0.0);
        self.tween.retarget(1.0, crate::motion::STRIP_PERIOD, now);
        self.displayed(now)
    }
}

/// The breathing colour for a run's state-marker cell, or `None` when the run is
/// not moving. An abandoned run is in an active phase and going nowhere, so it
/// never breathes — the red `⚑` it already flies is the true story.
fn run_live(r: &state::RunSummary, app: &App) -> Option<Color> {
    (!r.abandoned && is_active_phase(r.phase)).then(|| app.gutter(0))
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
                rail_lead(sel, focused, (stat.needs_you > 0).then_some(WARN), None),
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
                HomeRow::Run { run, .. } => {
                    let flag = attention_flag(run, unit_wants_operator(run));
                    let more = if run.wants > 1 {
                        format!(" ⚑{}", run.wants)
                    } else {
                        String::new()
                    };
                    let phase_w = w.saturating_sub(17).clamp(6, 16) as usize;
                    let body = vec![
                        Span::styled(
                            format!("{:<8}", truncate(home_display_id(run), 8)),
                            if sel { selected(focused) } else { dim() },
                        ),
                        Span::styled(
                            format!("  {}", truncate(&rail_phase(run.phase), phase_w)),
                            Style::default().fg(phase_color(run.phase)),
                        ),
                        Span::styled(more, Style::default().fg(WARN).bold()),
                    ];
                    rail_row(
                        rail_lead(sel, focused, flag, run_live(run, app)),
                        body,
                        Span::styled(
                            relative_wait(home_wait(run.updated_at, Utc::now())),
                            muted(),
                        ),
                        w,
                    )
                }
                HomeRow::Empty(b) => ListItem::new(Span::styled(
                    format!("  {}", home_band_empty_text(*b)),
                    muted().italic(),
                )),
                HomeRow::More { n, .. } => ListItem::new(Span::styled(
                    format!("  … {n} more"),
                    Style::default().fg(FG_MUTED).italic(),
                )),
                HomeRow::Project(idx, _) => {
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
                    rail_row(rail_lead(sel, focused, None, None), body, Span::raw(""), w)
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
                    rail_row(rail_lead(sel, focused, None, None), body, Span::raw(""), w)
                }
                HomeRow::Skeleton { .. } => {
                    let bar = format!("{}  {}", "░".repeat(8), "░".repeat(6));
                    let spans = if app.animated {
                        sweep_spans(
                            &bar,
                            PULSE_LO,
                            PULSE_HI,
                            app.clock.cycle(crate::motion::SWEEP_PERIOD),
                        )
                    } else {
                        vec![Span::styled(bar.clone(), Style::default().fg(PULSE_LO))]
                    };
                    let lead = rail_lead(sel, focused, None, None);
                    let right = Span::styled("░".repeat(3), Style::default().fg(PULSE_LO));
                    rail_row(lead, spans, right, w)
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
            let flag = attention_flag(r, unit_wants_operator(r));
            let body = vec![
                Span::styled(
                    format!("{:<8}", truncate(&r.id, 8)),
                    if sel { selected(focused) } else { dim() },
                ),
                Span::styled(format!("  {phase_text}"), Style::default().fg(phase_c)),
                Span::styled(more, Style::default().fg(WARN).bold()),
            ];
            rail_row(
                rail_lead(sel, focused, flag, run_live(r, app)),
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
            let role = format!("{:<role_w$}", truncate(&slot_short(slots, i), role_w));
            let mut body = vec![Span::styled(
                format!("{} ", slot_icon(s, app)),
                Style::default().fg(color),
            )];
            // A pending slot is dispatched and alive but has produced nothing yet:
            // a sweep across its name says so without claiming work is happening.
            if s.status == SlotStatus::Pending && app.animated {
                body.extend(sweep_spans(
                    &role,
                    FG_MUTED,
                    FG,
                    app.clock.cycle(crate::motion::SWEEP_PERIOD),
                ));
            } else {
                body.push(Span::styled(
                    role,
                    if sel { selected(focused) } else { dim() },
                ));
            }
            body.push(Span::styled(
                slot_model(s, model_w),
                Style::default().fg(HINT),
            ));
            rail_row(
                rail_lead(
                    sel,
                    focused,
                    broken.then_some(ALERT),
                    (!broken && s.status == SlotStatus::Running).then(|| app.gutter(0)),
                ),
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
    projects: &[registry::ProjectEntry],
    full: Option<&RunState>,
    stream_text: &str,
    stream_text_raw: &str,
    log_records: &[Record],
    activity: &[Record],
    diff_text: &str,
    diff_records: &[Record],
    plan_docs: &[Record],
    review: &[Record],
    chat: &[Record],
    home: &HomeData,
    log_stats: Option<&process::StreamStats>,
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
        draw_home_body(f, inner, home, projects, app);
        return;
    }

    // With no run selected, Log/Activity/Diff have nothing real to show — Log's
    // already-coherent empty message is the one story all three tell instead of each
    // inventing its own. Shell never joins that story: it is project-scoped, not
    // run-scoped (see `manage_terminal`), so it always shows the real workspace
    // terminal regardless of run count.
    match app.main_tab {
        MainTab::Log => draw_log_body(
            f,
            inner,
            full,
            stream_text,
            stream_text_raw,
            log_records,
            log_stats,
            app,
        ),
        MainTab::Activity if full.is_none() => draw_log_body(
            f,
            inner,
            full,
            stream_text,
            stream_text_raw,
            log_records,
            log_stats,
            app,
        ),
        MainTab::Activity => draw_activity_body(f, inner, activity, full, app),
        MainTab::Diff if full.is_none() => draw_log_body(
            f,
            inner,
            full,
            stream_text,
            stream_text_raw,
            log_records,
            log_stats,
            app,
        ),
        MainTab::Diff => draw_diff_body(f, inner, diff_text, diff_records, app),
        MainTab::Plan if full.is_none() => draw_log_body(
            f,
            inner,
            full,
            stream_text,
            stream_text_raw,
            log_records,
            log_stats,
            app,
        ),
        MainTab::Plan => draw_plan_body(f, inner, plan_docs, app),
        MainTab::Review if full.is_none() => draw_log_body(
            f,
            inner,
            full,
            stream_text,
            stream_text_raw,
            log_records,
            log_stats,
            app,
        ),
        MainTab::Review => draw_review_body(f, inner, review, app),
        MainTab::Chat => draw_chat_body(f, inner, chat, full, app),
        MainTab::Shell => draw_shell_body(f, inner, app),
    }
}

/// Main's Home body: the four bands (C7). Reuses the same scrollable-log viewport as
/// the other no-run bodies.
fn draw_home_body(
    f: &mut Frame,
    inner: Rect,
    home: &HomeData,
    projects: &[registry::ProjectEntry],
    app: &mut App,
) {
    let text = home_detail(
        &home.rows,
        projects,
        app.selected_home,
        &app.home_scope,
        app.home_watermark,
        Utc::now(),
        inner.width,
    );
    app.stream_view_h = inner.height;
    app.stream_max = render_scrollable_log(
        f,
        inner,
        &text,
        &mut app.stream_scroll,
        &mut app.stream_follow,
        false,
        app.log_expand,
        false,
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
            // Parsed mode (a full run, `R` off) tracks its own follow flag
            // (AC-14); everything else — raw mode, and the no-run overview —
            // still reads `stream_follow`.
            let following = if full.is_some() && !app.raw_mode {
                app.stream_parsed_follow
            } else {
                app.stream_follow
            };
            let follow = if following { " · live" } else { "" };
            format!("{slot} · {mode}{follow}")
        }
        MainTab::Activity if full.is_none() => String::new(),
        MainTab::Activity => "run timeline + bus".into(),
        MainTab::Diff if full.is_none() => String::new(),
        MainTab::Diff => "artifacts".into(),
        MainTab::Plan if full.is_none() => String::new(),
        MainTab::Plan => "plan · critique · test-contract".into(),
        MainTab::Review if full.is_none() => String::new(),
        MainTab::Review => "acceptance criteria + verdicts".into(),
        MainTab::Chat => "conversation".into(),
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
/// slot's stall/quiet state and token stats on a one-row band. Records by default
/// (U32); `R` falls back to the byte-for-byte raw view (U36).
#[allow(clippy::too_many_arguments)]
fn draw_log_body(
    f: &mut Frame,
    inner: Rect,
    full: Option<&RunState>,
    stream_text: &str,
    stream_text_raw: &str,
    log_records: &[Record],
    stats: Option<&process::StreamStats>,
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
            false,
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

    draw_stream_stats(
        f,
        chunks[0],
        stats,
        slot.map(|s| s.status),
        &silent_hint,
        app.abandoned,
    );

    app.stream_view_h = chunks[1].height;
    if app.raw_mode {
        // Exactly the persisted bytes (AC-14) — `stream_text` carries a truncation
        // banner and a "waiting for stream" placeholder for the friendly parsed
        // fallback below, neither of which the raw escape hatch may show.
        app.stream_max = render_scrollable_log(
            f,
            chunks[1],
            stream_text_raw,
            &mut app.stream_scroll,
            &mut app.stream_follow,
            true,
            app.log_expand,
            true,
        );
    } else {
        // `log_records` comes pre-parsed off-thread in `build_snapshot` (U13), stamped
        // against the byte-offset index when one exists. A caller that has not built
        // one yet (or a genuinely un-indexed source) still gets a folded, glyphed view
        // through the same fallback `active_records_for` uses (round-9 finding 5): paint
        // and structural navigation must agree on one list, or `J`/`t`/`e`/`Space` flash
        // "no match" over records visibly on screen.
        let records = effective_log_records(log_records, stream_text);
        // The newest record of a running slot breathes with the rail's own live
        // gutter (`App::gutter`) rather than a second fade (U30's standing rule).
        let live = (slot.map(|s| s.status) == Some(SlotStatus::Running))
            .then(|| records.last().map(|r| (&r.source, app.gutter(0))))
            .flatten();
        app.stream_parsed_max = render_record_view(
            f,
            chunks[1],
            &records,
            &mut app.stream_parsed_scroll,
            &mut app.stream_parsed_follow,
            &app.fold_open,
            app.fold_all,
            app.record_cursor.as_ref(),
            &mut app.record_cursor_dirty,
            live,
        );
    }
}

/// Main's Activity tab (AC-2): typed records only — never a joined string. No raw
/// mode (AC-14): Activity is an aggregate over several sources, not one persisted
/// byte range.
fn draw_activity_body(
    f: &mut Frame,
    inner: Rect,
    activity: &[Record],
    full: Option<&RunState>,
    app: &mut App,
) {
    app.bus_view_h = inner.height;
    let filtered = filter_activity_records(activity, app.activity_slot_filter, full);
    app.bus_max = render_record_view(
        f,
        inner,
        &filtered,
        &mut app.bus_scroll,
        &mut app.bus_follow,
        &app.fold_open,
        app.fold_all,
        app.record_cursor.as_ref(),
        &mut app.record_cursor_dirty,
        None,
    );
}

/// Main's Diff tab: the selected worktree's real `git diff HEAD` (U4), split into
/// one foldable `FileDiff` record per file; `R` shows the unsplit patch.
fn draw_diff_body(
    f: &mut Frame,
    inner: Rect,
    diff_text: &str,
    diff_records: &[Record],
    app: &mut App,
) {
    app.diff_view_h = inner.height;
    // No parsed records yet (nothing selected has a real worktree diff to split, or
    // a caller supplied raw text without records) falls back to the same raw
    // viewport the whole tab used before this feature, rather than a `FileDiff`
    // parse invented from text that was never `git diff` shaped.
    app.diff_raw_active = app.raw_mode || diff_records.is_empty();
    if app.diff_raw_active {
        // `diff_text` is always a real `git diff HEAD` (or a plain not-a-diff
        // notice), never a coalesced marker-style log — the marker rewriting
        // `compact_log_line` does is irrelevant here and would mangle a diff's
        // own significant whitespace (AC-14), so this viewport is always raw.
        app.diff_max = render_scrollable_log(
            f,
            inner,
            diff_text,
            &mut app.diff_scroll,
            &mut app.diff_follow,
            false,
            app.log_expand,
            true,
        );
    } else {
        let mut follow = false;
        app.diff_parsed_max = render_record_view(
            f,
            inner,
            diff_records,
            &mut app.diff_parsed_scroll,
            &mut follow,
            &app.fold_open,
            app.fold_all,
            app.record_cursor.as_ref(),
            &mut app.record_cursor_dirty,
            None,
        );
    }
}

/// Main's Plan tab (005 A): `plan.md`, the plan critique, and `test-contract.md` as
/// foldable document records (AC-16). No raw mode: three documents, not one source.
fn draw_plan_body(f: &mut Frame, inner: Rect, plan_docs: &[Record], app: &mut App) {
    app.plan_view_h = inner.height;
    let mut follow = false;
    app.plan_max = render_record_view(
        f,
        inner,
        plan_docs,
        &mut app.plan_scroll,
        &mut follow,
        &app.fold_open,
        app.fold_all,
        app.record_cursor.as_ref(),
        &mut app.record_cursor_dirty,
        None,
    );
}

/// Main's Review tab (005 B/C): one `Criterion` record per `AC-n` plus one foldable
/// record per reviewer's verdict, sourced from the same gate the ship path calls
/// (AC-17). No raw mode: the grid is a projection over several artifacts.
fn draw_review_body(f: &mut Frame, inner: Rect, review: &[Record], app: &mut App) {
    app.review_view_h = inner.height;
    let mut follow = false;
    app.review_max = render_record_view(
        f,
        inner,
        review,
        &mut app.review_scroll,
        &mut follow,
        &app.fold_open,
        app.fold_all,
        app.record_cursor.as_ref(),
        &mut app.record_cursor_dirty,
        None,
    );
}

fn draw_chat_body(
    f: &mut Frame,
    inner: Rect,
    chat: &[Record],
    full: Option<&RunState>,
    app: &mut App,
) {
    // Stats line shows latest + accumulated conversation stats (AC-7) without
    // touching run usage, scoped to the currently selected conversation so a
    // Home Chat does not show run-scoped spend and vice versa. Displayed as
    // "conversation stats" so the feature is discoverable by grep.
    // Reserve the input row permanently (U11) — it is always 1 row; stats is an
    // extra row above it only when the current conversation has spend.
    let input_h: u16 = 1;
    let scope_key = full.map(|s| s.id.as_str()).unwrap_or("home").to_string();
    let cur_conv = app.chat_conversations.get(&scope_key).cloned();
    let (has_stats, total_billed, latest_billed) = if let Some(conv) = &cur_conv {
        let latest = app
            .chat_latest_stats
            .get(conv)
            .map(|s| s.billed_tokens)
            .unwrap_or(0);
        let total = app
            .chat_accum_stats
            .get(conv)
            .map(|s| s.billed_tokens)
            .unwrap_or(0);
        (latest > 0 || total > 0, total, latest)
    } else {
        (false, 0, 0)
    };
    let stats_h = if has_stats { 1 } else { 0 };
    let transcript_h = inner.height.saturating_sub(input_h + stats_h);
    let transcript_area = Rect {
        height: transcript_h,
        ..inner
    };
    let stats_area = Rect {
        y: inner.y + transcript_h,
        height: stats_h.min(inner.height.saturating_sub(input_h)),
        width: inner.width,
        x: inner.x,
    };
    let input_area = Rect {
        y: inner.y + transcript_h + stats_h,
        height: input_h.min(inner.height),
        ..inner
    };
    app.chat_view_h = transcript_area.height;
    app.chat_max = render_record_view(
        f,
        transcript_area,
        chat,
        &mut app.chat_scroll,
        &mut app.chat_follow,
        &app.fold_open,
        app.fold_all,
        app.record_cursor.as_ref(),
        &mut app.record_cursor_dirty,
        None,
    );
    if has_stats && stats_area.height > 0 {
        let chat_stats_line =
            format!("conversation stats — latest {latest_billed} · total {total_billed} tokens");
        f.render_widget(
            Paragraph::new(chat_stats_line).style(Style::default().fg(Color::Rgb(160, 160, 160))),
            stats_area,
        );
    }
    // Input line
    let input_text = if app.chat_composing {
        format!("> {}█", app.chat_input)
    } else if !app.chat_input.is_empty() {
        format!("> {}", app.chat_input)
    } else if app.chat_active_turn.is_some() {
        "> Turn in flight · Esc cancel".to_string()
    } else {
        "> Press i to chat · Enter send · Esc cancel".to_string()
    };
    let mut line = input_text;
    // Hint for proposal launch at Home
    if app.browse == BrowseLevel::Home
        && app.main_tab == MainTab::Chat
        && !app.chat_composing
        && chat.iter().any(|r| {
            r.summary.contains("```spar-proposal")
                || r.body.iter().any(|b| b.contains("```spar-proposal"))
        })
    {
        line.push_str("  [o: open proposal]");
    }
    f.render_widget(
        Paragraph::new(line).style(Style::default().fg(Color::White).bg(Color::Rgb(30, 30, 30))),
        input_area,
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
#[allow(clippy::too_many_arguments)]
fn render_scrollable_log(
    f: &mut Frame,
    area: Rect,
    text: &str,
    scroll: &mut u16,
    follow: &mut bool,
    colorize: bool,
    expand: bool,
    raw_mode: bool,
) -> u16 {
    if area.width == 0 || area.height == 0 {
        clamp_scroll(scroll, follow, 0);
        return 0;
    }

    let sb_w = 1u16;
    let text_w = area.width.saturating_sub(sb_w).max(1) as usize;
    let height = area.height as usize;
    let total = log_row_count(text, text_w, expand, raw_mode).max(1);
    // Cap at u16::MAX so dense tails cannot wrap the scroll type.
    let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    clamp_scroll(scroll, follow, max_scroll);
    let start = *scroll as usize;
    // Materialise only the rows we are about to paint, not the whole tail.
    let visible = log_rows_window(text, text_w, colorize, expand, raw_mode, start, height);

    let text_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(sb_w).max(1),
        height: area.height,
    };
    f.render_widget(Clear, text_area);
    f.buffer_mut().set_style(text_area, page());
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

/// A record is expanded when it has body content and either `fold_all` overrides
/// every default, or the operator's explicit toggle (`fold_open`, keyed by the
/// record's immutable source identity) flips the default (U36/AC-7).
fn is_record_expanded(
    fold_open: &std::collections::HashSet<SourceId>,
    fold_all: bool,
    r: &Record,
) -> bool {
    if r.body.is_empty() {
        return false;
    }
    // `fold_all` only ever shifts the *base* every record starts from; it must not
    // short-circuit past `fold_open`, or `Space` silently does nothing while `A` is
    // engaged and a record the operator explicitly re-folds under `A` can never
    // actually end up folded (round-11 review, AC-6).
    let base_expanded = fold_all || !r.folded_by_default;
    base_expanded ^ fold_open.contains(&r.source)
}

/// The meta column's content (AC-3/AC-4): elapsed and/or an absolute time, else the
/// no-data sentinel `·` — never fabricated (AC-8). Below the wide breakpoint
/// (`meta_width < 15`, `Columns::for_width`'s `WIDE_META_WIDTH`) there is only room
/// for one field, so elapsed wins (it is the more actionable of the two) and the
/// compact 24h clock is used if only a time is known. At the wide breakpoint the
/// column was reserved to carry *both* — U32 promises "elapsed *and* absolute
/// time" at `>=100`, not one displacing the other.
fn record_meta_text(r: &Record, meta_width: u16) -> String {
    let wide = meta_width >= 15;
    match (r.elapsed, r.time) {
        (Some(e), Some(t)) if wide => {
            format!("{} {}", record::fmt_elapsed(e), t.format("%-I:%M %p"))
        }
        (Some(e), _) => record::fmt_elapsed(e),
        (None, Some(t)) if wide => t.format("%-I:%M %p").to_string(),
        (None, Some(t)) => t.format("%H:%M").to_string(),
        (None, None) => "·".to_string(),
    }
}

fn record_kind_style(r: &Record) -> Style {
    if matches!(r.kind, RecordKind::Tool(_)) && r.ok == Some(false) {
        return Style::default().fg(ALERT);
    }
    match r.kind {
        RecordKind::Tool(_) => Style::default().fg(CODE),
        RecordKind::Result { ok: true } => Style::default().fg(OK),
        RecordKind::Result { ok: false } => Style::default().fg(ALERT),
        RecordKind::Thought => Style::default().fg(FG_MUTED).italic(),
        RecordKind::Note | RecordKind::Section => Style::default().fg(FG_MUTED),
        RecordKind::Error | RecordKind::Alert => Style::default().fg(ALERT).bold(),
        RecordKind::Doc | RecordKind::Criterion | RecordKind::FileDiff | RecordKind::Prose => {
            Style::default().fg(FG)
        }
    }
}

/// One record-view row: an optional surface background (`SURFACE_RAISED` for the
/// cursor row, `SURFACE_SUNKEN` for an expanded tool/diff body — AC-19) plus
/// explicitly x-positioned spans, so a fixed column never moves for content (U32).
struct RecordRow {
    bg: Option<Color>,
    spans: Vec<(u16, String, Style)>,
}

fn build_head_row(
    r: &Record,
    cols: record::Columns,
    is_cursor: bool,
    folded: bool,
    live_gutter: Option<Color>,
) -> RecordRow {
    let mut spans = Vec::new();
    // A running slot's newest record breathes with the rail's own live gutter
    // (`App::gutter`, U30) rather than a second fade — this is that streaming
    // record's one caller.
    let gutter_color = live_gutter.unwrap_or(ACCENT);
    spans.push((
        cols.gutter,
        (if is_cursor { SEL_BAR } else { " " }).to_string(),
        Style::default().fg(gutter_color),
    ));
    let fold_mark = if r.body.is_empty() {
        " "
    } else if folded {
        "▸"
    } else {
        "▾"
    };
    spans.push((
        cols.gutter + 1,
        fold_mark.to_string(),
        Style::default().fg(FG_DIM),
    ));
    spans.push((cols.glyph, r.glyph.to_string(), record_kind_style(r)));
    if let Some(actor_x) = cols.actor {
        if let Some(actor) = &r.actor {
            let w = cols.verb.saturating_sub(actor_x).saturating_sub(1) as usize;
            spans.push((
                actor_x,
                truncate_display(actor, w),
                Style::default().fg(FG_MUTED),
            ));
        }
    }
    // Doc/Criterion/Section records carry their real label in `head` (a document
    // heading, an `AC-n` id) — `verb` there is just a generic shape tag ("Doc").
    // FileDiff is deliberately not here (round-9 finding 2): its `verb` is the
    // real `A`/`D`/`R`/`M` status letter, and `head` (the path) already repeats
    // in `summary` — using `head` here painted the path twice and dropped status.
    let verb_text: &str = match r.kind {
        RecordKind::Doc | RecordKind::Criterion | RecordKind::Section => &r.head,
        _ => &r.verb,
    };
    // Below 80 columns `Columns::for_width` folds the verb column into the
    // summary column (`cols.verb == cols.summary`, AC-4): there is no separate
    // span to paint the verb into, so it is prefixed onto the summary text
    // instead of dropped.
    let verb_folded = cols.verb_folded;
    if !verb_text.is_empty() && !verb_folded {
        // An empty summary column has nothing to collide with, so a long head
        // label (Activity's `§ Run 3f2…`) gets the whole run up to meta rather
        // than truncating at the 9-column verb field and losing the rest
        // (round-9 finding 5).
        let w = if r.summary.is_empty() {
            cols.meta.saturating_sub(cols.verb).saturating_sub(1)
        } else {
            cols.summary.saturating_sub(cols.verb).saturating_sub(1)
        } as usize;
        spans.push((
            cols.verb,
            truncate_display(verb_text, w),
            Style::default().fg(FG).bold(),
        ));
    }
    let summary_w = cols.meta.saturating_sub(cols.summary).saturating_sub(1) as usize;
    let summary_text = if verb_folded && !verb_text.is_empty() {
        format!("{verb_text} {}", r.summary)
    } else {
        r.summary.clone()
    };
    spans.push((
        cols.summary,
        truncate_display(&summary_text, summary_w),
        record_kind_style(r),
    ));
    // Right-aligned and never overrunning the row (AC-3): the text pushed must be
    // no longer than what `meta_len` claims, or the alignment math and the actual
    // paint disagree and the tail spills past the row's right edge.
    let meta_text = truncate_display(
        &record_meta_text(r, cols.meta_width),
        cols.meta_width as usize,
    );
    let meta_len = meta_text.chars().count() as u16;
    let meta_x = cols.meta + cols.meta_width.saturating_sub(meta_len);
    spans.push((meta_x, meta_text, Style::default().fg(FG_DIM)));
    RecordRow {
        bg: is_cursor.then_some(SURFACE_RAISED),
        spans,
    }
}

/// One (possibly wrapped) body line. `is_command` is only ever true for a Tool
/// record's `body[0]` — `to_record` inserts the command/path there whether the
/// call is open or already merged with a result — so it always paints `CODE`
/// (AC-19's command/path row); every other body row is the result's own output.
fn build_body_row(
    kind: RecordKind,
    text: String,
    cols: record::Columns,
    is_command: bool,
    live_color: Option<Color>,
) -> RecordRow {
    let style = if is_command {
        Style::default().fg(CODE)
    } else {
        Style::default().fg(FG_DIM)
    };
    let bg = matches!(
        kind,
        RecordKind::Tool(_) | RecordKind::Result { .. } | RecordKind::FileDiff
    )
    .then_some(SURFACE_SUNKEN);
    let mut spans = Vec::new();
    if let Some(color) = live_color {
        spans.push((cols.gutter, "│".to_string(), Style::default().fg(color)));
    }
    spans.push((cols.verb, text, style));
    RecordRow { bg, spans }
}

/// Greedy word-wrap of one body line to `width` display columns (U36: "the raw
/// text must stay reachable" — a body line wider than the pane used to be clipped
/// with no way to reach the rest). A single word longer than `width` hard-breaks
/// by character rather than looping forever. `width == 0` returns the line whole;
/// the caller still has to paint *something*.
fn wrap_body_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.chars().count() <= width {
        return vec![text.to_string()];
    }
    // Leading spaces are content the operator's tool emitted (indentation in a
    // directory listing, a diff hunk); `rest.split(' ')` below would otherwise
    // swallow them, since a boundary before any real word never has anything to
    // attach a separator to (AC-6: expansion must preserve every persisted byte).
    // Stripped up front, budgeted out of the wrap width so the indent plus first
    // line never exceeds `width`, then reattached to the first output line only.
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let indent_len = indent.chars().count();
    let rest = &text[indent.len()..];
    let rest_width = width.saturating_sub(indent_len).max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for raw_word in rest.split(' ') {
        let mut remaining = raw_word.to_string();
        loop {
            let word_len = remaining.chars().count();
            let sep = if cur.is_empty() { 0 } else { 1 };
            if cur.chars().count() + sep + word_len <= rest_width {
                if sep == 1 {
                    cur.push(' ');
                }
                cur.push_str(&remaining);
                break;
            }
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            // A single word longer than the whole (empty) line: hard-break it.
            let take: String = remaining.chars().take(rest_width).collect();
            let rest2: String = remaining.chars().skip(rest_width).collect();
            out.push(take);
            if rest2.is_empty() {
                break;
            }
            remaining = rest2;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    if !indent.is_empty() {
        if let Some(first) = out.first_mut() {
            first.insert_str(0, &indent);
        }
    }
    out
}

/// Paint a `Vec<Record>` viewport (U32): fixed columns, folding by default, a raised
/// cursor row, and a sunken band for expanded tool/diff output. Mirrors
/// `render_scrollable_log`'s contract (same scrollbar, materialises only the visible
/// window) so the two widgets read as one family.
/// One (possibly wrapped) paintable row — `record::FlatRow` after body lines wider
/// than the viewport have been split (U36: a body line must stay reachable, never
/// silently clipped).
enum ExpandedRowKind {
    Head,
    /// `bool` is whether this is a Tool record's command/path row (`body[0]`,
    /// AC-19) rather than result output — true regardless of whether the call is
    /// still open or has already merged with a result.
    Body(String, bool),
}

struct ExpandedRow {
    record_idx: usize,
    kind: ExpandedRowKind,
    /// Rows painted so far for this record, head = 0: feeds `App::gutter`'s fade
    /// so a streaming record's body rows dim with depth the same way its head
    /// does, rather than only the head ever calling `gutter(0)` (round-9 finding 5).
    depth: usize,
}

#[allow(clippy::too_many_arguments)]
fn render_record_view(
    f: &mut Frame,
    area: Rect,
    records: &[Record],
    scroll: &mut u16,
    follow: &mut bool,
    fold_open: &std::collections::HashSet<SourceId>,
    fold_all: bool,
    cursor: Option<&SourceId>,
    cursor_dirty: &mut bool,
    live: Option<(&SourceId, Color)>,
) -> u16 {
    if area.width == 0 || area.height == 0 {
        clamp_scroll(scroll, follow, 0);
        return 0;
    }
    let sb_w = 1u16;
    let text_w = area.width.saturating_sub(sb_w).max(1);
    let cols = record::Columns::for_width(text_w);
    let height = area.height as usize;
    let is_expanded = |r: &Record| is_record_expanded(fold_open, fold_all, r);
    let flat = record::flatten(records, is_expanded);
    // A body line wider than the pane wraps rather than clipping (AC-6): computed
    // here, not in `record::flatten`, since wrapping is a paint-width concern, not
    // a pure domain one.
    let body_width = text_w.saturating_sub(cols.verb).max(1) as usize;
    let mut expanded: Vec<ExpandedRow> = Vec::with_capacity(flat.len());
    let mut depth = 0usize;
    for row in &flat {
        match row.kind {
            record::RowKind::Head => {
                depth = 0;
                expanded.push(ExpandedRow {
                    record_idx: row.record_idx,
                    kind: ExpandedRowKind::Head,
                    depth,
                });
                depth += 1;
            }
            record::RowKind::Body(j) => {
                let rec = &records[row.record_idx];
                let text = rec.body.get(j).map(|s| s.as_str()).unwrap_or("");
                // `to_record` inserts the command/path as `body[0]` for a Tool
                // record whose call carried an argument, open or merged (AC-19),
                // and marks that explicitly via `has_command_row` at construction
                // time — never inferred later by comparing text, which broke on
                // the native Claude coalescer path (round-10 review). A
                // detail-less call has no such row: its `body[0]` is the result
                // preview instead and paints like ordinary output.
                let is_command = j == 0 && rec.has_command_row;
                if matches!(rec.kind, RecordKind::Criterion) {
                    // The criteria grid's row is pre-padded into fixed-width cells
                    // (`review_records`): greedy word-wrap tokenizes on spaces and
                    // re-joins with a single one, destroying that padding and
                    // shifting every reviewer column after the first wrap point
                    // (AC-17). Truncate instead — a table row that doesn't fit is
                    // still a row, not a reflow.
                    expanded.push(ExpandedRow {
                        record_idx: row.record_idx,
                        kind: ExpandedRowKind::Body(truncate_display(text, body_width), is_command),
                        depth,
                    });
                    depth += 1;
                } else {
                    for chunk in wrap_body_line(text, body_width) {
                        expanded.push(ExpandedRow {
                            record_idx: row.record_idx,
                            kind: ExpandedRowKind::Body(chunk, is_command),
                            depth,
                        });
                        depth += 1;
                    }
                }
            }
        }
    }
    let total = expanded.len().max(1);
    let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    clamp_scroll(scroll, follow, max_scroll);
    // Structural navigation (J/K, t/T, e/E, }/{ — AC-13) only ever moves the
    // cursor's identity; without this the matching record could land off-screen
    // with nothing visibly different. Only snaps right after a navigation key (the
    // dirty flag), so a manual scroll away from a stationary cursor is not fought.
    if *cursor_dirty {
        if let Some(cur) = cursor {
            let pos = expanded.iter().position(|row| {
                matches!(row.kind, ExpandedRowKind::Head) && &records[row.record_idx].source == cur
            });
            if let Some(pos) = pos {
                let pos = pos as u16;
                if pos < *scroll {
                    *scroll = pos;
                } else if pos >= scroll.saturating_add(height as u16) {
                    *scroll = pos.saturating_sub(height as u16).saturating_add(1);
                }
                *scroll = (*scroll).min(max_scroll);
            }
        }
        *cursor_dirty = false;
    }
    let start = *scroll as usize;
    let rows: Vec<RecordRow> = expanded
        .iter()
        .skip(start)
        .take(height)
        .map(|row| {
            let r = &records[row.record_idx];
            let is_cursor = cursor == Some(&r.source);
            let live_color = live.and_then(|(id, c)| (id == &r.source).then_some(c));
            match &row.kind {
                ExpandedRowKind::Head => {
                    build_head_row(r, cols, is_cursor, !is_expanded(r), live_color)
                }
                ExpandedRowKind::Body(text, is_command) => {
                    // Same fade `App::gutter(depth)` gives the rail, applied to this
                    // body row's own depth rather than the head's depth 0 (round-9
                    // finding 5) — a live record's expanded output keeps breathing
                    // instead of going flat the moment it scrolls off the head row.
                    let body_color = live_color
                        .map(|c| toward_bg(c, (row.depth as f32 * TRAIL_FALLOFF).min(1.0)));
                    build_body_row(r.kind, text.clone(), cols, *is_command, body_color)
                }
            }
        })
        .collect();

    let text_area = Rect {
        x: area.x,
        y: area.y,
        width: text_w,
        height: area.height,
    };
    f.render_widget(Clear, text_area);
    f.buffer_mut().set_style(text_area, page());
    f.render_widget(
        RecordCellLog {
            rows,
            fill: Style::default().fg(FG),
        },
        text_area,
    );

    if max_scroll > 0 {
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

/// Fills every cell, then paints explicitly x-positioned spans per row — the record
/// view's counterpart to `CellLog`, painting several styled segments per line instead
/// of one.
struct RecordCellLog {
    rows: Vec<RecordRow>,
    fill: Style,
}

impl Widget for RecordCellLog {
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
        for (i, row) in self.rows.iter().enumerate() {
            if i as u16 >= area.height {
                break;
            }
            let y = area.top() + i as u16;
            if let Some(bg) = row.bg {
                for x in area.left()..area.right() {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(self.fill.bg(bg));
                    }
                }
            }
            for (x_off, text, style) in &row.spans {
                let mut col = *x_off;
                for ch in text.chars() {
                    if col >= area.width {
                        break;
                    }
                    let x = area.left() + col;
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_char(ch);
                        let merged = match row.bg {
                            Some(bg) => style.bg(bg),
                            None => *style,
                        };
                        cell.set_style(merged);
                        cell.set_skip(false);
                    }
                    col = col.saturating_add(1);
                }
            }
        }
    }
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

/// The per-line text a log viewport paints: `compact_log_line`'s rewritten form,
/// or (AC-14) exactly the persisted line with only tabs expanded — a terminal
/// rendering necessity, not a data change — when `raw` is the byte-for-byte
/// escape hatch.
fn viewport_log_line(raw: &str, raw_mode: bool) -> String {
    if raw_mode {
        expand_tabs(raw)
    } else {
        compact_log_line(raw)
    }
}

/// Rows the log occupies, without building any of them. In trim mode this is
/// just the line count; wrapping has to measure each line. Matches
/// `log_rows_window`'s empty-output fallback so the two always agree.
fn log_row_count(text: &str, width: usize, expand: bool, raw_mode: bool) -> usize {
    let width = width.max(1);
    let n: usize = if !expand {
        text.lines().count()
    } else {
        text.lines()
            .map(|raw| {
                let line = viewport_log_line(raw, raw_mode);
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
#[allow(clippy::too_many_arguments)]
fn log_rows_window(
    text: &str,
    width: usize,
    colorize: bool,
    expand: bool,
    raw_mode: bool,
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
        let line = viewport_log_line(raw, raw_mode);
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
                        let total_fn = log_row_count(text, w, exp, false);
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
                            let win = log_rows_window(text, w, col, exp, false, start, height);
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
    log_rows_window(text, width, colorize, expand, false, 0, usize::MAX)
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
    // Plain lines keep their own spacing. The marker arms above collapse because
    // what follows a `→`/`←` is one field the coalescer already joined; a plain
    // line is the only place structure can arrive pre-aligned, and squashing it
    // was the renderer destroying information the input had (feature 010's
    // opening complaint). Tabs still normalise: terminals disagree on their width,
    // so a tab is the one whitespace that cannot be trusted to hold a column.
    expand_tabs(s)
}

/// Tabs to spaces on an 8-column grid. Rendering a tab verbatim leaves the column
/// it lands in up to the host terminal, which is exactly what a fixed column
/// cannot depend on.
fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0usize;
    for ch in s.chars() {
        if ch == '\t' {
            let n = 8 - (col % 8);
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
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
    f.buffer_mut()
        .set_style(rect, Style::default().bg(BG_OVERLAY));

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

    let cursor = if app.clock.cycle(CURSOR_BLINK) < 0.5 {
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
            BrowseLevel::Home => {
                "j/k · Enter open · n chat · P scope · p projects · a next-alert · : cmd · ? help"
            }
            BrowseLevel::Projects => "j/k · Enter open · / filter · : cmd · 2 main · ? help",
            BrowseLevel::Runs => "j/k · Enter agents · a next-alert · / filter · : cmd · ? help",
            BrowseLevel::Agents => "j/k · Enter take over · a next-alert · Esc runs · : cmd",
        },
        Focus::Main => match tab {
            MainTab::Log => "J/K t/e/} nav · Space/A fold · R raw · [ ] tabs · 1 rail",
            MainTab::Activity => "J/K t/e/} nav · f filter · Space/A fold · [ ] tabs · 1 rail",
            MainTab::Diff => "J/K } nav · Space/A fold · R raw · [ ] tabs · 1 rail",
            MainTab::Plan => "J/K } nav · Space/A fold · [ ] tabs · 1 rail",
            MainTab::Review => "J/K } nav · Space/A fold · [ ] tabs · 1 rail",
            MainTab::Chat => "i chat · J/K } nav · Space/A fold · [ ] tabs · 1 rail",
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
    Main   one area · tabs: Log · Activity · Diff · Plan · Review · Chat · Shell
    Main always shows the rail's selection — nothing else moves.

  Keyboard
    1 / 2                focus Rail · Main
    Tab / Shift-Tab      cycle Rail ↔ Main
    j k  or  ↑ ↓         move in the rail · scroll Main · scroll this help
    Enter                push a rail level (on an agent: take it over)
    Esc                  pop a rail level · clear filter (never quits)
    [ ]                  previous / next Main tab
    + / _                zoom Main fullscreen / restore
    n                    chat (Home: new conversation, run: gate consultation)
    i / o                compose in Chat · o opens proposal at Home
    P                    toggle Home scope (this project ↔ all)
    p                    jump to Projects
    a                    jump to the next run that needs you
    r / s                reject · ship (when gated; approve = tap / :approve)
    :                    command palette (approve/ship/takeover/… · :msg is raw bus)
    :msg <run> <msg>     raw bus message (use Chat tab for conversation)
    spar plan --brief    launch from a prior .spar/briefs/<slug>.md
    /                    filter the rail
    w                    log wrap ↔ truncate long lines
    g / G                top / bottom of Main
    J/K t/T e/E }/{       record head · tool call · error · phase/doc (Main)
    Space / A / R        fold cursor · fold all · raw text (Log/Diff)
    f                    Activity: filter to the selected slot
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
    f.buffer_mut()
        .set_style(rect, Style::default().bg(BG_OVERLAY));
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
        .map(|p| {
            // Prefer the registry's own name (what the rail shows) over the raw
            // directory basename, so the same project reads identically in the
            // rail and in this modal (round-7 review finding).
            projects
                .iter()
                .find(|e| &e.root == p)
                .and_then(|e| e.name.clone())
                .or_else(|| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| p.display().to_string())
        })
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
    let workflow_label = nr
        .workflow
        .map(|w| w.as_str().to_string())
        .unwrap_or_else(|| "unset (Chat can propose)".to_string());
    lines.push((
        format!("Workflow: {workflow_label}  ←/→ or w"),
        field_style(NewRunField::Workflow),
    ));
    lines.push((String::new(), Style::default()));
    if let Some(wf) = nr.workflow {
        if wf == crate::runspec::SpecWorkflow::Arena {
            lines.push(("Arena pool:".to_string(), field_style(NewRunField::Roles)));
            for (idx, slot) in nr.arena_pool.iter().enumerate() {
                let label = slot
                    .as_ref()
                    .map(|p| p.display())
                    .unwrap_or_else(|| "unassigned".to_string());
                let cursor = if nr.field == NewRunField::Roles && nr.role_sel == idx {
                    ">"
                } else {
                    " "
                };
                let style = if nr.field == NewRunField::Roles && nr.role_sel == idx {
                    Style::default().fg(ACCENT).bold()
                } else {
                    Style::default().fg(FG)
                };
                let extra = if nr.field == NewRunField::Roles && nr.role_sel == idx {
                    if nr.editing_model {
                        format!(" (editing model: {}▌)", nr.model_buffer)
                    } else {
                        " ← Fleet to assign".to_string()
                    }
                } else {
                    String::new()
                };
                lines.push((format!("{cursor} {}. {}{}", idx + 1, label, extra), style));
            }
        } else {
            lines.push(("Roles:".to_string(), field_style(NewRunField::Roles)));
            for (idx, ra) in nr.roles.iter().enumerate() {
                let primary = ra
                    .primary
                    .as_ref()
                    .map(|p| p.display())
                    .unwrap_or_else(|| "unassigned".to_string());
                let backup = ra
                    .backup
                    .as_ref()
                    .map(|p| format!(" backup:{}", p.display()))
                    .unwrap_or_default();
                let name = if ra.role == crate::state::SlotRole::Reviewer {
                    format!("reviewer[{}]", ra.ordinal)
                } else {
                    ra.role.as_config_key().to_string()
                };
                let cursor = if nr.field == NewRunField::Roles && nr.role_sel == idx {
                    ">"
                } else {
                    " "
                };
                let style = if nr.field == NewRunField::Roles && nr.role_sel == idx {
                    Style::default().fg(ACCENT).bold()
                } else if ra.primary.is_none() {
                    Style::default().fg(FG_DIM)
                } else {
                    Style::default().fg(FG)
                };
                let editing = if nr.field == NewRunField::Roles && nr.role_sel == idx {
                    if nr.editing_model {
                        format!(" (editing model: {}▌)", nr.model_buffer)
                    } else if nr.editing_backup {
                        " (editing backup)".to_string()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                lines.push((
                    format!("{cursor} {}: {}{}{}", name, primary, backup, editing),
                    style,
                ));
            }
            lines.push((
                "  j/k select · Fleet to assign · b backup · m model · Backspace clear · Ctrl-D save defaults"
                    .to_string(),
                Style::default().fg(FG_MUTED),
            ));
        }
        lines.push((String::new(), Style::default()));
    }
    lines.push(("Fleet:".to_string(), field_style(NewRunField::Fleet)));
    if nr.loading {
        lines.push((
            "  checking roster…".to_string(),
            Style::default().fg(FG_MUTED),
        ));
    } else if nr.roster.iter().all(|e| !e.available) {
        // Not just "empty": the shipped default `providers.order` still emits one
        // configured, unavailable row per entry when nothing is on PATH, so the
        // roster is rarely literally empty — it is a roster with nothing usable
        // (R7 review finding).
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
            // Not `[x]`: next to numbered picks and an empty `[ ]`, an `x` reads as
            // *checked*, the opposite of disabled (round-9 review).
            None => "[-]".to_string(),
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
    if !nr.legacy_providers.is_empty() {
        lines.push((String::new(), Style::default()));
        lines.push((
            "Legacy providers (needs workflow/role mapping):".to_string(),
            Style::default().fg(ALERT).bold(),
        ));
        for prov in &nr.legacy_providers {
            let pin_ok = crate::runspec::Pin::parse(prov).is_ok();
            let avail = if pin_ok {
                nr.roster
                    .iter()
                    .find(|e| e.label == *prov)
                    .map(|e| e.available)
                    .unwrap_or(false)
            } else {
                false
            };
            let reason = if !pin_ok {
                " (invalid provider)"
            } else if !avail {
                " (unavailable)"
            } else {
                ""
            };
            lines.push((
                format!("  - {}{}", prov, reason),
                Style::default().fg(ALERT),
            ));
        }
        lines.push((
            "  Pick a workflow to map, or clear legacy to launch".to_string(),
            Style::default().fg(FG_MUTED),
        ));
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
    f.buffer_mut()
        .set_style(rect, Style::default().bg(BG_OVERLAY));
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
    // surfaces synchronously as a palette error rather than a silent background drop.
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

/// Whether a slot's log holds nothing but the headless spawn header and its
/// prompt echo — used only to decide the "waiting for stream" placeholder.
/// Mirrors `record::prompt_skip_end`'s notion of where real content starts, but
/// never rewrites what `stream_content` actually returns (AC-14: the raw view is
/// byte-for-byte, so filtering can inform a UI decision but must not touch the
/// displayed/returned text itself).
fn only_header_and_prompt(raw: &str) -> bool {
    let body: Vec<&str> = raw
        .lines()
        .skip_while(|l| l.starts_with('#') || *l == "---" || l.starts_with("cwd=") || l.is_empty())
        .collect();
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
    body[start..].join("\n").trim().is_empty()
}

/// Exactly the persisted bytes of the selected slot's log tail — no truncation
/// banner, no "waiting for stream" placeholder. `stream_content` injects both for
/// its own friendly, non-raw purpose (the empty state and the parsed-view
/// fallback); `R`'s raw view must never show text nothing ever wrote to disk
/// (AC-14). Absent file or run reads as empty: there is no raw source to show.
fn stream_raw_content(
    swarm: &SparPaths,
    full: Option<&RunState>,
    slot_idx: usize,
    cache: &mut LogCache,
) -> String {
    let Some(st) = full else {
        return String::new();
    };
    let Some(slot) = st.slots.get(slot_idx.min(st.slots.len().saturating_sub(1))) else {
        return String::new();
    };
    let path = slot
        .log_path
        .clone()
        .unwrap_or_else(|| swarm.log_file(&st.id, &slot.id));
    if !path.is_file() {
        return String::new();
    }
    let (raw, _truncated) = cache.load(&path, LOG_TAIL_BYTES);
    raw.to_string()
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
        let raw = raw.to_string();
        if only_header_and_prompt(&raw) {
            format!(
                "\n  {} is running — waiting for stream…\n  Quiet time is on Agents; Activity shows phase timeline.",
                slot.id
            )
        } else if truncated {
            format!(
                "… earlier log truncated (showing last ~{} KB)\n{raw}",
                LOG_TAIL_BYTES / 1024
            )
        } else {
            // Exactly the persisted bytes (AC-14): this is also the source for the
            // `R` raw view and the no-index record-parse fallback, neither of which
            // may see anything other than what disk actually holds.
            raw
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

/// Builds one activity record with an identity derived from its own content
/// (AC-7), not from where it lands in the feed: `activity_feed` prepends alerts and
/// slides a `take(N)` window over events/bus messages, so a position-keyed identity
/// (a build-local counter) renumbered every record after a single new alert or
/// event arrived, moving the cursor and the fold set to different rows underneath
/// the operator. Two records with genuinely identical content still collide, but
/// that is a strictly smaller window than "moves whenever anything upstream changes".
fn activity_record(
    time: Option<DateTime<Utc>>,
    actor: impl Into<String>,
    event: impl Into<String>,
    detail: impl Into<String>,
    kind: RecordKind,
) -> record::ActivityRecord {
    let detail = detail.into();
    activity_record_with_identity(time, actor, event, detail.clone(), &detail, kind)
}

/// A running slot's row (AC-7): `detail` carries `SlotActivity::human_silent()`, a
/// "how long since its last log line" string that changes every second
/// (`"3s"`, `"4s"`, …) purely for display. Hashing that into the identity, the
/// way `activity_record` hashes every other call's `detail`, moved this row's
/// `SourceId` on every snapshot rebuild — the cursor and fold state could never
/// land on a live slot. `identity_detail` is what actually distinguishes one
/// slot's row from another (here, nothing beyond actor/event/kind is needed) and
/// is hashed in `detail`'s place.
fn activity_record_live(
    time: Option<DateTime<Utc>>,
    actor: impl Into<String>,
    event: impl Into<String>,
    detail: impl Into<String>,
    identity_detail: &str,
    kind: RecordKind,
) -> record::ActivityRecord {
    activity_record_with_identity(time, actor, event, detail, identity_detail, kind)
}

fn activity_record_with_identity(
    time: Option<DateTime<Utc>>,
    actor: impl Into<String>,
    event: impl Into<String>,
    detail: impl Into<String>,
    identity_detail: &str,
    kind: RecordKind,
) -> record::ActivityRecord {
    let actor = actor.into();
    let event = event.into();
    let detail = detail.into();
    let at_millis = time.map(|t| t.timestamp_millis()).unwrap_or(0);
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    at_millis.hash(&mut hasher);
    std::mem::discriminant(&kind).hash(&mut hasher);
    actor.hash(&mut hasher);
    event.hash(&mut hasher);
    identity_detail.hash(&mut hasher);
    let sequence = hasher.finish();
    record::ActivityRecord {
        time,
        actor,
        event,
        detail,
        kind,
        source: SourceId::Activity {
            at_millis,
            sequence,
        },
    }
}

/// Main's Activity tab (AC-2): typed `(time, actor, event, detail)` records —
/// phase boundaries (`RecordKind::Section`), alerts, slot status and bus chat —
/// never a joined string.
fn activity_feed(
    swarm: &SparPaths,
    full: Option<&RunState>,
    quota: &QuotaStore,
    alerts: &[crate::bus::BusMessage],
    heartbeats: &std::collections::HashMap<String, DateTime<Utc>>,
    cfg: &Config,
) -> Vec<Record> {
    let mut out: Vec<record::ActivityRecord> = Vec::new();
    let Some(st) = full else {
        out.push(activity_record(
            None,
            "",
            "No run selected",
            "Open a project, pick a run.",
            RecordKind::Prose,
        ));
        return out.into_iter().map(|r| r.to_record()).collect();
    };

    // Loudest first: anything waiting on a human sits at the top of the feed.
    if !alerts.is_empty() {
        out.push(activity_record(
            None,
            "",
            "Needs you",
            format!("{} unresolved", alerts.len()),
            RecordKind::Section,
        ));
        for m in alerts.iter().rev().take(6).rev() {
            out.push(activity_record(
                Some(m.ts),
                short_agent(short_in_run(&m.from, &st.id)).to_string(),
                "alert",
                m.body.clone(),
                RecordKind::Alert,
            ));
        }
    }

    let mut run_detail = phase_label(st.phase).to_string();
    if st.dry_run {
        run_detail.push_str(" · dry-run");
    }
    out.push(activity_record(
        None,
        "",
        format!("Run {}", st.id),
        st.task.clone().unwrap_or_else(|| run_detail.clone()),
        RecordKind::Section,
    ));
    if st.task.is_some() {
        out.push(activity_record(
            None,
            "",
            "phase",
            run_detail,
            RecordKind::Note,
        ));
    }

    out.push(activity_record(
        None,
        "",
        "Agents",
        String::new(),
        RecordKind::Section,
    ));
    for s in &st.slots {
        let act = SlotActivity::observe(
            s,
            cfg.timeouts.stall_warn_secs,
            crate::executor::timeout_for_role(cfg, s.role).as_secs(),
            heartbeats.get(&s.id).copied(),
        );
        let kind = if s.status == SlotStatus::Running && act.stalled {
            RecordKind::Alert
        } else {
            RecordKind::Note
        };
        let quiet = if s.status == SlotStatus::Running {
            act.human_silent()
        } else {
            String::new()
        };
        // The slot id, not `role_label` alone (AC-7): two slots sharing a role — two
        // reviewers, two peers — render identical role/status text, and identity
        // hashes on exactly those fields. Only the id tells them apart. `quiet` is
        // display-only here (`activity_record_live`): it ticks every second for a
        // running slot and must never itself move this row's identity.
        out.push(activity_record_live(
            None,
            s.id.clone(),
            slot_status_label(s.status).to_string(),
            quiet,
            "",
            kind,
        ));
    }

    // Orchestrator event timeline (human): phase events are the phase boundaries
    // `}`/`{` navigate to (Phase C).
    let evs = events::read_all(swarm, &st.id).unwrap_or_default();
    if !evs.is_empty() {
        out.push(activity_record(
            None,
            "",
            "Timeline",
            String::new(),
            RecordKind::Section,
        ));
        for e in evs.iter().rev().take(14).rev() {
            let (actor, event, detail, kind) = match e.kind {
                events::EventKind::Phase => {
                    let phase = e.phase.map(phase_label).unwrap_or_else(|| "?".into());
                    (
                        st.id.clone(),
                        "phase".to_string(),
                        phase,
                        RecordKind::Section,
                    )
                }
                events::EventKind::Slot => {
                    let slot = e.slot.clone().unwrap_or_else(|| "agent".into());
                    let status = e.status.map(slot_status_label).unwrap_or("?").to_string();
                    (slot, "slot".to_string(), status, RecordKind::Note)
                }
                events::EventKind::Gate => (
                    st.id.clone(),
                    "gate".to_string(),
                    e.message.clone().unwrap_or_else(|| "waiting on you".into()),
                    RecordKind::Alert,
                ),
                events::EventKind::Info => (
                    st.id.clone(),
                    "info".to_string(),
                    e.message.clone().unwrap_or_default(),
                    RecordKind::Note,
                ),
            };
            out.push(activity_record(Some(e.ts), actor, event, detail, kind));
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
            out.push(activity_record(
                None,
                "",
                "Bus",
                String::new(),
                RecordKind::Section,
            ));
            for m in chat.iter().rev().take(8).rev() {
                out.push(activity_record(
                    Some(m.ts),
                    format!(
                        "{}→{}",
                        short_agent(short_in_run(&m.from, &st.id)),
                        short_agent(short_in_run(&m.to, &st.id))
                    ),
                    "bus",
                    m.body.clone(),
                    RecordKind::Note,
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
        out.push(activity_record(
            None,
            "",
            "Quota",
            String::new(),
            RecordKind::Section,
        ));
        for (name, q) in paused {
            out.push(activity_record(
                None,
                name.clone(),
                "paused",
                format!("{:?}", q.status),
                RecordKind::Note,
            ));
        }
    }

    out.into_iter().map(|r| r.to_record()).collect()
}

/// Main's Plan tab (005 A): `plan.md`, the plan critique (resolved through the
/// `PlanCritic` slot's own artifact), and `test-contract.md`, each as foldable
/// document records. A missing artifact names itself rather than leaving a blank
/// tab (AC-16).
fn plan_docs(swarm: &SparPaths, full: Option<&RunState>) -> Vec<Record> {
    let Some(st) = full else {
        return Vec::new();
    };
    let mut out = Vec::new();
    push_doc_or_missing(&mut out, swarm, &st.id, "plan.md", "plan.md");

    // `SlotState.artifact` is not a usable source here: it is the slot completion
    // gate's `expected_artifact`, which is always `plan.md` for every plan-phase
    // role including the critic (finding 6 — the gate is not this feature's to
    // change). The critique's real filename is the template's own convention
    // (`templates/plan_critic.md`), keyed on the critic slot's own id (AC-16).
    let critic_slot = st.slots.iter().find(|s| s.role == SlotRole::PlanCritic);
    let critic_artifact = match critic_slot {
        Some(s) => format!("plan-critique-{}.md", s.id),
        None => "plan-critique.md".to_string(),
    };
    push_doc_or_missing(&mut out, swarm, &st.id, "plan critique", &critic_artifact);

    push_doc_or_missing(
        &mut out,
        swarm,
        &st.id,
        "test-contract.md",
        "test-contract.md",
    );
    out
}

fn push_doc_or_missing(
    out: &mut Vec<Record>,
    swarm: &SparPaths,
    run_id: &str,
    name: &str,
    artifact: &str,
) {
    let path = swarm.artifact(run_id, artifact);
    match std::fs::read_to_string(&path) {
        Ok(body) if !body.trim().is_empty() => {
            out.extend(record::parse_document(name, &body, &path.to_string_lossy()));
        }
        _ => out.push(record::missing_document(name, &path.to_string_lossy())),
    }
}

/// Main's Review tab (005 B/C): one `Criterion` record per `AC-n` carrying every
/// reviewer's cell, sourced from the same `review_result`/`acceptance_block_reasons`
/// functions the ship gate calls (never a second implementation — AC-17), plus one
/// foldable record per reviewer's raw verdict.
fn review_records(swarm: &SparPaths, full: Option<&RunState>, cfg: Option<&Config>) -> Vec<Record> {
    let Some(st) = full else {
        return Vec::new();
    };
    let evidence = crate::orchestrator::collect_gate_evidence_with_fallback(swarm, &st.id, cfg);
    let criteria = evidence.criteria.clone();
    let reviewers: Vec<&SlotState> = st
        .slots
        .iter()
        .filter(|s| s.role == SlotRole::Reviewer)
        .collect();
    let mut out = Vec::new();

    if evidence.frozen_unavailable {
        let contract_path = swarm.artifact(&st.id, "test-contract.md");
        out.push(Record {
            kind: RecordKind::Alert,
            glyph: "!",
            verb: "Review".to_string(),
            head: "frozen config unavailable".to_string(),
            summary: "cannot evaluate criteria-based blockers for this run".to_string(),
            body: Vec::new(),
            time: None,
            elapsed: None,
            actor: None,
            ok: None,
            source: SourceId::Document {
                path: format!("{}#config", contract_path.display()),
                start: 0,
            },
            folded_by_default: false,
            has_command_row: false,
        });
    }
    let contract_path = swarm.artifact(&st.id, "test-contract.md");

    if criteria.is_empty() {
        out.push(Record {
            kind: RecordKind::Section,
            glyph: "§",
            verb: "Review".to_string(),
            head: "No contract".to_string(),
            summary: "the verdict alone gates".to_string(),
            body: Vec::new(),
            time: None,
            elapsed: None,
            actor: None,
            ok: None,
            source: SourceId::Document {
                path: contract_path.to_string_lossy().into_owned(),
                start: 0,
            },
            folded_by_default: false,
            has_command_row: false,
        });
    } else {
        // One fixed-width cell per reviewer, all on the *same* row (AC-17: a grid,
        // not one line per reviewer stacked in the body) — padded/truncated to a
        // constant width regardless of content, so a longer slot id or verdict word
        // cannot shift a neighbouring reviewer's column. The row can still be wider
        // than the viewport with many reviewers; `render_record_view` wraps a body
        // line that overruns rather than clipping it.
        const CELL_NAME_W: usize = 12;
        const CELL_STATUS_W: usize = 12;
        // Each reviewer's artifact is read and parsed once here, not once per
        // criterion inside the loop below (N criteria x M reviewers file reads
        // otherwise, per snapshot rebuild).
        let parsed_reviews: Vec<Option<workflow::review_result::ReviewResult>> = reviewers
            .iter()
            .map(|s| {
                let artifact = s
                    .artifact
                    .clone()
                    .unwrap_or_else(|| format!("review-{}.md", s.id));
                let path = swarm.artifact(&st.id, &artifact);
                std::fs::read_to_string(&path)
                    .ok()
                    .map(|text| workflow::review_result::parse_review(&text))
            })
            .collect();
        for id in &criteria {
            let mut cells: Vec<String> = Vec::new();
            let mut compact: Vec<String> = Vec::new();
            for (s, parsed) in reviewers.iter().zip(&parsed_reviews) {
                // A slot the executor marked `Failed` mirrors the gate's own `!review_ok`
                // (`implement.rs`'s `if !review_ok || missing_or_empty`): the gate blocks on
                // this regardless of whatever a *stale* review-<slot>.md from an earlier
                // round of the same slot id still holds, so the grid must not read that
                // leftover file as if it were this round's verdict (AC-17).
                let cell = if s.status == SlotStatus::Failed {
                    "failed".to_string()
                } else {
                    match parsed {
                        Some(res) => match res.acceptance.iter().find(|a| &a.id == id) {
                            Some(a) => format!("{:?}", a.status).to_ascii_lowercase(),
                            None => "not reported".to_string(),
                        },
                        None => "missing".to_string(),
                    }
                };
                compact.push(format!("{}: {cell}", short_agent(&s.id)));
                cells.push(format!(
                    "{:<name_w$} {:<status_w$}",
                    truncate_display(short_agent(&s.id), CELL_NAME_W),
                    truncate_display(&cell, CELL_STATUS_W),
                    name_w = CELL_NAME_W,
                    status_w = CELL_STATUS_W,
                ));
            }
            out.push(Record {
                kind: RecordKind::Criterion,
                glyph: "▤",
                verb: id.clone(),
                head: id.clone(),
                summary: compact.join("  ·  "),
                body: if cells.is_empty() {
                    Vec::new()
                } else {
                    vec![cells.join(" ")]
                },
                time: None,
                elapsed: None,
                actor: None,
                ok: None,
                source: SourceId::Document {
                    path: format!("{}#{}", contract_path.display(), id),
                    start: 0,
                },
                folded_by_default: false,
                has_command_row: false,
            });
        }
    }

    for s in &reviewers {
        let artifact = s
            .artifact
            .clone()
            .unwrap_or_else(|| format!("review-{}.md", s.id));
        let path = swarm.artifact(&st.id, &artifact);
        if s.status == SlotStatus::Failed {
            // Same failed-slot predicate as the criteria grid above: a stale artifact from
            // a previous round of this slot id must not read as this round's verdict.
            out.push(Record {
                kind: RecordKind::Error,
                glyph: "!",
                verb: s.id.clone(),
                head: format!("{} · failed", s.id),
                summary: "review slot failed or produced no review".to_string(),
                body: Vec::new(),
                time: None,
                elapsed: None,
                actor: None,
                ok: Some(false),
                source: SourceId::Document {
                    path: path.to_string_lossy().into_owned(),
                    start: 0,
                },
                folded_by_default: false,
                has_command_row: false,
            });
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                let res = workflow::review_result::parse_review(&text);
                let verdict_text = match res.verdict {
                    Some(workflow::review_result::Verdict::Approve) => "approve",
                    Some(workflow::review_result::Verdict::RequestChanges) => "request_changes",
                    None => "no parsed verdict",
                };
                let _ = cfg;
                let mut reasons = if criteria.is_empty() {
                    Vec::new()
                } else if let Some(c) = evidence.cfg.as_ref() {
                    workflow::implement::acceptance_block_reasons(&criteria, &res, c)
                } else {
                    Vec::new()
                };
                let blocked = !res.approves() || !reasons.is_empty();
                if !res.approves() {
                    reasons.insert(0, format!("verdict: {verdict_text}"));
                }
                let source = SourceId::Document {
                    path: path.to_string_lossy().into_owned(),
                    start: 0,
                };
                out.push(Record {
                    kind: if blocked {
                        RecordKind::Error
                    } else {
                        RecordKind::Doc
                    },
                    glyph: if blocked { "!" } else { "✓" },
                    verb: s.id.clone(),
                    head: format!("{} · {}", s.id, verdict_text),
                    summary: reasons.join("; "),
                    body: text.lines().map(|l| l.to_string()).collect(),
                    time: None,
                    elapsed: None,
                    actor: None,
                    ok: None,
                    source,
                    folded_by_default: true,
                    has_command_row: false,
                });
            }
            // The gate's own rule (`implement.rs`'s ship path) treats a missing or
            // empty review artifact as an unconditional `request_changes`, not a
            // neutral "nothing here yet" — this record must say the same, or the
            // Review tab under-reports a blocker the gate actually enforces (AC-17).
            _ => out.push(Record {
                kind: RecordKind::Error,
                glyph: "!",
                verb: s.id.clone(),
                head: format!("{} · missing", s.id),
                summary: "review slot failed or produced no review".to_string(),
                body: Vec::new(),
                time: None,
                elapsed: None,
                actor: None,
                ok: Some(false),
                source: SourceId::Document {
                    path: path.to_string_lossy().into_owned(),
                    start: 0,
                },
                folded_by_default: false,
                has_command_row: false,
            }),
        }
    }
    out
}

/// Paths shorten against the selected run's own worktree roots (U34): the
/// process-global `PROJECT_PREFIX` set once from whichever project was first
/// browsed cannot know a run's per-slot worktrees, and is wrong the moment the
/// operator crosses projects.
fn path_shortener_for(swarm: &SparPaths, full: Option<&RunState>) -> record::PathShortener {
    let mut roots = vec![swarm.project_root.clone()];
    if let Some(st) = full {
        roots.extend(st.worktrees.iter().map(|w| w.path.clone()));
    }
    record::PathShortener::new(roots)
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

/// Whether a single, unfolded run currently wants the operator: a live
/// `Attention::Gate`/`Broken`, or `PlanApproved` — terminal and colored `OK` like `Done`
/// (round-9 review: changing `run_attention` itself would have re-litigated
/// `phase_color`/`sort_runs_by_attention` for behaviour no review flagged), but still a
/// handoff nothing else will complete. This is the per-leg predicate `fold_units` counts
/// into `wants`; Home's band, both rail flags and the attention cycle key read a folded
/// row through `unit_wants_operator` instead, since `PlanApproved` is conditional on the
/// *unit* having no active leg (U28), not on this one leg's phase alone —
/// `unit_wants_operator_matches_wants_operator_on_every_fold_units_row` is what keeps the
/// two predicates from disagreeing on any row `fold_units` can actually produce.
/// `emit_attention_toasts` does not go through either: it diffs `run_attention` directly
/// and so never toasts a `PlanApproved` handoff or a folded unit's wanting leg
/// (pre-existing, flagged by review 3be317b2, not fixed here).
fn wants_operator(r: &state::RunSummary) -> bool {
    run_attention(r).needs_you() || r.phase == Phase::PlanApproved
}

/// Whether the *unit* a row stands for wants the operator (U28). A folded row (U15)
/// carries its active leg's id and phase, so `wants_operator` on the row alone answers
/// for that leg, not the group — reading `r.wants` (each leg that itself wants the
/// operator, `PlanApproved` counted only when no leg in the unit is active) instead is
/// what keeps a unit's `PlanApproved` leg from being reported as a handoff while a sibling
/// is still working it, and what keeps two gates folded into one row from reading as
/// only one. `wants` is computed per leg in `fold_units`, so this is the predicate Home's
/// bands, the rail flag and the attention cycle must use — the same rule
/// `runs_needing_attention` already applies to the roll-up.
fn unit_wants_operator(r: &state::RunSummary) -> bool {
    if r.legs > 1 {
        r.wants > 0
    } else {
        wants_operator(r)
    }
}

/// The rail lead flag color for a run row, shared by the Runs level and Home so a run
/// that counts toward `runs_needing_attention` always shows the same flag it is
/// counted for. `force` is the row's own `unit_wants_operator` — it flags a folded row
/// whenever any leg wants the operator (U28), even if the active leg shown in `r` itself
/// does not.
fn attention_flag(r: &state::RunSummary, force: bool) -> Option<Color> {
    if force || wants_operator(r) {
        Some(if run_attention(r) == Attention::Gate {
            WARN
        } else {
            ALERT
        })
    } else {
        None
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

/// How many runs want the operator (gate, broken, or PlanApproved — see
/// `wants_operator`). A folded row (U15) stands for several runs, so it contributes
/// each leg that wants you — otherwise a unit with two gates would read as one and
/// folding would become a way to hide a gate.
fn runs_needing_attention(runs: &[state::RunSummary]) -> usize {
    runs.iter()
        .map(|r| {
            if r.legs > 1 {
                r.wants as usize
            } else {
                usize::from(wants_operator(r))
            }
        })
        .sum()
}

/// Flash a toast when a run first crosses into wanting the operator (Working/Idle →
/// Gate/Broken) since the last snapshot. The first snapshot only primes the baseline
/// so the existing fleet is never announced. `is_home` names which population `runs`
/// is — Home's cross-project rows or a project's `snap.runs` — so a navigation that
/// swaps the population re-primes instead of diffing against the other population's
/// baseline (which would re-announce every unchanged gate/broken run the swap newly
/// exposes).
fn emit_attention_toasts(app: &mut App, runs: &[state::RunSummary], is_home: bool) {
    let now: Vec<(String, Attention)> = runs
        .iter()
        .map(|r| (r.id.clone(), run_attention(r)))
        .collect();
    if app.prev_attention_home == Some(is_home) {
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
    }
    app.prev_attention = Some(now);
    app.prev_attention_home = Some(is_home);
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
            app.flash(
                format!("→ {} needs you", truncate(home_display_id(run), 8)),
                WARN,
            );
        }
        return;
    }
    if !app.browse.in_project() {
        app.flash("open a project first", FG_MUTED);
        return;
    }
    let n = runs.len();
    let next = (1..=n)
        .map(|off| (app.selected_run + off) % n)
        .find(|&i| runs.get(i).map(unit_wants_operator).unwrap_or(false));
    match next {
        Some(i) => {
            app.selected_run = i;
            app.selected_run_key = runs.get(i).map(run_row_key);
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
        let all = &MAIN_TABS[..];
        assert_eq!(MainTab::Log.next_in(all), MainTab::Activity);
        assert_eq!(MainTab::Shell.next_in(all), MainTab::Log);
        assert_eq!(MainTab::Log.prev_in(all), MainTab::Shell);
        assert_eq!(MainTab::Diff.prev_in(all), MainTab::Activity);
    }

    /// U30. The state-marker column carries one fact, and attention outranks
    /// activity: a run that both wants you and is moving must fly the flag, or
    /// the pulse reads as "fine, leave it alone".
    #[test]
    fn the_state_marker_prefers_attention_over_activity() {
        let glyph = |flag, live| {
            rail_lead(false, false, flag, live)[1]
                .content
                .clone()
                .into_owned()
        };
        assert_eq!(glyph(Some(WARN), Some(ACCENT)), "⚑", "both: flag wins");
        assert_eq!(glyph(Some(WARN), None), "⚑");
        assert_eq!(glyph(None, Some(ACCENT)), LIVE_BAR, "moving: it breathes");
        assert_eq!(glyph(None, None), " ", "at rest: nothing");
    }

    /// The selection bar and the state marker stay two separate cells whatever
    /// they carry, or a project where every run wants you hides the cursor.
    #[test]
    fn the_lead_columns_never_change_width() {
        for flag in [None, Some(WARN)] {
            for live in [None, Some(ACCENT)] {
                for sel in [false, true] {
                    let w: usize = rail_lead(sel, true, flag, live)
                        .iter()
                        .map(|s| s.content.chars().count())
                        .sum();
                    assert_eq!(w, 2, "sel={sel} flag={flag:?} live={live:?}");
                }
            }
        }
    }

    /// An abandoned run sits in an active phase and is going nowhere. Breathing
    /// for it would claim work is happening; the red flag is the true story.
    #[test]
    fn an_abandoned_run_does_not_breathe() {
        let app = test_app();
        let at = |phase| state::RunSummary {
            id: "aband001".into(),
            workflow: crate::cli::WorkflowKind::Loop,
            archived: false,
            phase,
            updated_at: Utc::now(),
            task: None,
            dry_run: false,
            abandoned: false,
            parent_run: None,
            round: 1,
            legs: 1,
            wants: 0,
            unit_id: None,
            base_ref: None,
            base_commit: None,
            project_root: None,
            project_name: None,
        };
        let mut r = at(Phase::WaitCompletion);
        assert!(run_live(&r, &app).is_some(), "an active run breathes");
        r.abandoned = true;
        assert!(run_live(&r, &app).is_none(), "an abandoned one does not");
        assert!(
            run_live(&at(Phase::Done), &app).is_none(),
            "a finished run does not breathe"
        );
        assert!(
            run_live(&at(Phase::AwaitingShipConfirm), &app).is_none(),
            "a run parked at a gate is not moving"
        );
    }

    /// The sweep marks a label without ever consuming or reordering it: a moving
    /// highlight that dropped a character would be a rendering bug that only
    /// shows up one frame in ten.
    #[test]
    fn a_swept_label_is_still_the_same_label() {
        for phase in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let spans = sweep_spans("implementer", FG_MUTED, FG, phase);
            let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(rebuilt, "implementer", "phase {phase}");
        }
    }

    /// At Home, Log/Activity/Diff all rendered the identical body, so the strip
    /// offered three ways to change nothing (U31). The cycle must not visit them.
    #[test]
    fn home_cycles_only_the_tabs_that_mean_something() {
        let home = tabs_for(BrowseLevel::Home);
        assert_eq!(home, &[MainTab::Log, MainTab::Chat, MainTab::Shell]);
        assert_eq!(MainTab::Log.next_in(home), MainTab::Chat);
        assert_eq!(MainTab::Chat.next_in(home), MainTab::Shell);
        assert_eq!(MainTab::Shell.next_in(home), MainTab::Log);
        assert_eq!(MainTab::Shell.prev_in(home), MainTab::Chat);
        // Named for what it shows, not for the slot it occupies.
        assert_eq!(MainTab::Log.label_at(BrowseLevel::Home), "Home");
        assert_eq!(MainTab::Log.label_at(BrowseLevel::Runs), "Log");
        // Every other level keeps all four.
        for lvl in [
            BrowseLevel::Projects,
            BrowseLevel::Runs,
            BrowseLevel::Agents,
        ] {
            assert_eq!(tabs_for(lvl).len(), MAIN_TABS.len(), "{lvl:?}");
        }
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
            &[],
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
            &[],
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
        // The zone is affordable below the rail breakpoint too. Only widths that
        // cannot hold both the buttons and a minimal left breadcrumb fall back to
        // content-sized alignment.
        assert!(gate_zone(Rect { width: 35, ..area }).is_some());
        assert!(gate_zone(Rect { width: 34, ..area }).is_none());
        assert!(gate_zone(Rect { width: 79, ..area }).is_some());
        for w in [40, 60, 79, 80, 120] {
            let area_w = Rect { width: w, ..area };
            let a = first_x_at(
                w,
                area_w,
                vec![
                    ("Approve", GateAction::Approve),
                    ("Reject", GateAction::Reject),
                ],
            );
            let b = first_x_at(w, area_w, vec![("Ship", GateAction::Ship)]);
            let c = first_x_at(
                w,
                area_w,
                vec![
                    ("Confirm", GateAction::ConfirmWinner),
                    ("Reconcile", GateAction::Reconcile),
                ],
            );
            assert_eq!(a, b, "gate x must be fixed across gates at w={w}");
            assert_eq!(a, c, "gate x must be fixed across gates at w={w}");
            assert_eq!(
                a,
                area_w.right() - GATE_ZONE_W,
                "gate x must be zone-aligned at w={w}"
            );
        }
    }

    fn first_x_at(w: u16, area: Rect, buttons: Vec<(&str, GateAction)>) -> u16 {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, 1)).unwrap();
        let mut app = test_app();
        term.draw(|f| render_gate_buttons(f, area, &mut app, &buttons))
            .unwrap();
        app.gate_buttons[0].0.x
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
    fn breadcrumb_retains_space_without_gate_at_narrow_widths() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        // The header reserves the gate zone whenever the width affords it, independent
        // of whether a gate is live — otherwise the ⚑/⚠ chips slide 23 columns the
        // instant a run enters a gate. At phone widths the breadcrumb is truncated
        // to the fixed left limit, but must still show the brand.
        let swarm = SparPaths::new("/x");
        for w in [35, 40, 60, 79] {
            let mut term = Terminal::new(TestBackend::new(w, 1)).unwrap();
            let mut app = test_app();
            term.draw(|f| {
                let area = f.area();
                draw_header(
                    f,
                    area,
                    &swarm,
                    &[],
                    &[],
                    None,
                    &HomeData::default(),
                    &mut app,
                );
            })
            .unwrap();
            assert!(app.gate_buttons.is_empty(), "no gate at w={w}");
            let row: String = {
                let buf = term.backend().buffer();
                (0..w).map(|x| buf[(x, 0)].symbol()).collect()
            };
            assert!(
                row.contains("spar"),
                "breadcrumb collapsed without gate at w={w}: {row:?}"
            );
            assert!(!row.trim().is_empty(), "header empty without gate at w={w}");
            if w >= GATE_ZONE_W + GATE_ZONE_MIN_LEFT {
                let zone = gate_zone(Rect {
                    x: 0,
                    y: 0,
                    width: w,
                    height: 1,
                });
                assert!(zone.is_some(), "zone affordable at w={w} must be Some");
            }
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
            draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app);
            draw_rule(f, &lay, &app);
        })
        .unwrap();
        assert_eq!(app.main_tabs.len(), MAIN_TABS.len());
        assert!(app.main_tabs.iter().all(|(r, _)| r.y == lay.labels.y));
        assert!(app.main_tabs[0].0.x >= lay.main.x);
        assert!(app.main_tabs.windows(2).all(|w| w[0].0.x < w[1].0.x));
        assert_eq!(app.main_tabs[MAIN_TABS.len() - 1].1, MainTab::Shell);

        let row = |y: u16| -> String {
            let buf = term.backend().buffer();
            (0..120).map(|x| buf[(x, y)].symbol()).collect()
        };
        let labels = row(lay.labels.y);
        assert!(
            labels.contains("Act ⚠3") || labels.contains("Activity ⚠3"),
            "labels were: {labels:?}"
        );
        assert!(
            labels.contains("Shell") || labels.contains("Sh"),
            "labels were: {labels:?}"
        );
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

    /// AC-14: the raw viewport must not rewrite markers, collapse whitespace, or
    /// trim trailing whitespace — `compact_log_line`'s job for the parsed record
    /// view, never the byte-for-byte escape hatch's.
    #[test]
    fn raw_mode_bypasses_marker_rewriting_and_preserves_trailing_whitespace() {
        let text = "← toolu_abc123   total 1184   \n+ trailing line   \n";
        let rows = log_rows_window(text, 200, false, false, true, 0, 10);
        assert_eq!(rows[0].0, "← toolu_abc123   total 1184   ");
        assert_eq!(rows[1].0, "+ trailing line   ");
        // The same text in compact (non-raw) mode does rewrite the marker and trim.
        let compact_rows = log_rows_window(text, 200, false, false, false, 0, 10);
        assert_ne!(compact_rows[0].0, rows[0].0);
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
            unit_id: None,
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

    /// `n` at the Projects level must target the row highlighted *now*, not the one
    /// `active_root` was clamped to at the top of the frame. The loop drains a queued
    /// input burst against one mutable `active_root`, so `j` then `n` arriving together
    /// used to open the modal on the previously highlighted project (review e72f434e,
    /// major). Driven through `handle_key` without a frame in between, which is exactly
    /// the shape that broke.
    #[test]
    fn n_at_projects_targets_the_row_selected_in_the_same_input_burst() {
        let projects: Vec<registry::ProjectEntry> = ["alpha", "bravo"]
            .iter()
            .map(|n| registry::ProjectEntry {
                root: PathBuf::from("/nonexistent").join(n),
                name: Some((*n).to_string()),
                last_seen: Utc::now(),
                last_run_id: None,
            })
            .collect();
        let mut app = test_app();
        app.open_projects_view();
        let sw = SparPaths::new(std::path::Path::new("/nonexistent/alpha"));
        // Stale on purpose: this is what the pre-input clamp left behind for `alpha`.
        let mut root = PathBuf::from("/nonexistent/alpha");

        for code in [KeyCode::Char('j'), KeyCode::Char('n')] {
            handle_key(
                &mut app,
                code,
                KeyModifiers::empty(),
                &sw,
                &projects,
                &[],
                &[],
                None,
                &[],
                &mut root,
                None,
            )
            .unwrap();
        }

        assert_eq!(app.selected_project, 1, "`j` moved to bravo");
        assert!(
            app.new_run.is_none(),
            "`n` should not open new-run surface, it focuses Chat"
        );
        assert_eq!(app.main_tab, MainTab::Chat, "`n` should focus Chat tab");
        assert!(app.chat_composing, "`n` should enter composing");
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(!quit, "q inside the palette types, never quits");
        assert_eq!(app.palette.as_ref().map(|p| p.input.as_str()), Some("q"));
    }

    fn sample_log_records() -> Vec<Record> {
        record::parse_log_records(
            "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\ndrwxr-xr-x 2 root root\n! disk full\n",
            0,
            &[],
            &record::PathShortener::default(),
            "r",
            "s",
        )
        .iter()
        .map(|lr| lr.to_record())
        .collect()
    }

    /// AC-13: `J`/`K`/`t`/`T`/`e`/`E` move `App.record_cursor` over whatever Main
    /// is currently showing.
    #[test]
    fn record_navigation_keys_move_the_cursor() {
        let records = sample_log_records();
        let mut app = test_app();
        app.open_main(MainTab::Log);
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");
        assert!(app.record_cursor.is_none());

        handle_key(
            &mut app,
            KeyCode::Char('J'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &records,
            &mut root,
            None,
        )
        .unwrap();
        let first = app.record_cursor.clone().expect("J must set the cursor");
        assert_eq!(first, records[0].source);

        handle_key(
            &mut app,
            KeyCode::Char('t'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &records,
            &mut root,
            None,
        )
        .unwrap();
        let tool_idx = records
            .iter()
            .position(|r| matches!(r.kind, RecordKind::Tool(_)))
            .expect("fixture has a tool record");
        assert_eq!(
            app.record_cursor.as_ref(),
            Some(&records[tool_idx].source),
            "`t` must land on the tool call"
        );

        handle_key(
            &mut app,
            KeyCode::Char('e'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &records,
            &mut root,
            None,
        )
        .unwrap();
        let err_idx = records
            .iter()
            .position(|r| matches!(r.kind, RecordKind::Error))
            .expect("fixture has an error record");
        assert_eq!(
            app.record_cursor.as_ref(),
            Some(&records[err_idx].source),
            "`e` must land on the error"
        );
    }

    /// AC-6: `Space` folds/unfolds exactly the record under the cursor.
    #[test]
    fn space_toggles_fold_for_the_cursor_record_only() {
        let records = sample_log_records();
        let mut app = test_app();
        app.open_main(MainTab::Log);
        app.record_cursor = Some(records[0].source.clone());
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");

        assert!(!app.fold_open.contains(&records[0].source));
        handle_key(
            &mut app,
            KeyCode::Char(' '),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &records,
            &mut root,
            None,
        )
        .unwrap();
        assert!(
            app.fold_open.contains(&records[0].source),
            "Space must open exactly the cursor record"
        );
        assert!(
            records
                .iter()
                .skip(1)
                .all(|r| !app.fold_open.contains(&r.source)),
            "Space must not touch any other record"
        );
    }

    /// AC-6 (round-11 review): a cursor left over from a different Main tab or a
    /// different selected slot does not resolve to any record in the list `Space`
    /// is actually shown against. `space_toggles_fold_for_the_cursor_record_only`
    /// cannot catch this — it seeds the cursor from the same list it passes in, so
    /// the cursor always resolves. `Space` must fall back the same way `J`/`K`
    /// (`move_cursor`) already do for an unresolvable cursor: act on the first
    /// record rather than silently toggling a fold key for a record that is
    /// neither selected nor on screen.
    #[test]
    fn space_falls_back_to_the_first_record_when_the_cursor_does_not_resolve() {
        let records = sample_log_records();
        let mut app = test_app();
        app.open_main(MainTab::Log);
        // A source that cannot appear in `records`: a different slot id entirely,
        // simulating a cursor left over from before the operator switched slots.
        app.record_cursor = Some(crate::record::SourceId::Log {
            run_id: "r".into(),
            slot_id: "some-other-slot".into(),
            start: 0,
            end: 1,
        });
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");

        handle_key(
            &mut app,
            KeyCode::Char(' '),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &records,
            &mut root,
            None,
        )
        .unwrap();

        assert_eq!(
            app.record_cursor.as_ref(),
            Some(&records[0].source),
            "an unresolvable cursor must re-seed to the first record, not stay foreign"
        );
        assert!(
            app.fold_open.contains(&records[0].source),
            "Space must act on the record it re-seeded to, not silently no-op"
        );
    }

    /// AC-6 (round-11 review): `is_record_expanded` used to short-circuit to `true`
    /// whenever `fold_all` (`A`) was engaged, ignoring `fold_open` entirely — so
    /// `Space` had no visible effect while `A` was on, and a record the operator
    /// explicitly re-folded under `A` could never actually end up folded.
    #[test]
    fn space_can_still_re_fold_a_record_while_fold_all_is_engaged() {
        let records = sample_log_records();
        let tool = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Tool(_)))
            .expect("a foldable tool record");
        let fold_open = std::collections::HashSet::new();
        assert!(
            is_record_expanded(&fold_open, true, tool),
            "fold_all alone must still expand a record with no explicit toggle"
        );
        let mut toggled = std::collections::HashSet::new();
        toggled.insert(tool.source.clone());
        assert!(
            !is_record_expanded(&toggled, true, tool),
            "Space must be able to re-fold a record even while fold_all is engaged"
        );
    }

    /// AC-14: `R` toggles raw mode only on Log/Diff, never on a tab with no single
    /// raw source (Activity here).
    #[test]
    fn r_toggles_raw_mode_only_where_a_raw_source_exists() {
        let mut app = test_app();
        app.open_main(MainTab::Log);
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");
        handle_key(
            &mut app,
            KeyCode::Char('R'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(app.raw_mode, "R must toggle raw mode on Log");

        app.raw_mode = false;
        app.open_main(MainTab::Activity);
        handle_key(
            &mut app,
            KeyCode::Char('R'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(!app.raw_mode, "R must be a no-op on Activity");
    }

    /// AC-14: `R` followed by a scroll key in the *same* input burst must scroll the
    /// mode the toggle just switched *to*, not the mode a not-yet-run paint last set
    /// `App::diff_raw_active` from. The input loop drains a whole burst before the next
    /// paint runs, so a naive read of that paint-time field lagged the toggle by one
    /// frame (round-review finding, AC-14) — `end_for_main`/`home_for_main`/
    /// `scroll_diff_by` must derive raw-vs-parsed fresh from `raw_mode` and the diff
    /// records handed to them instead.
    #[test]
    fn r_then_end_in_one_burst_scrolls_the_mode_just_toggled_to() {
        let st = RunState::new(
            "r1",
            crate::cli::WorkflowKind::Loop,
            std::path::PathBuf::from("/x"),
        );
        let mut app = test_app();
        app.open_main(MainTab::Diff);
        app.raw_mode = false;
        app.diff_max = 50;
        app.diff_parsed_max = 30;
        let diff_records = vec![Record {
            kind: RecordKind::FileDiff,
            glyph: "M",
            verb: "src/a.rs".to_string(),
            head: "src/a.rs".to_string(),
            summary: String::new(),
            body: vec!["+new line".to_string()],
            time: None,
            elapsed: None,
            actor: None,
            ok: None,
            source: SourceId::Diff {
                worktree: "wt".to_string(),
                path: "src/a.rs".to_string(),
            },
            folded_by_default: true,
            has_command_row: false,
        }];
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");

        handle_key(
            &mut app,
            KeyCode::Char('R'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            Some(&st),
            &diff_records,
            &mut root,
            None,
        )
        .unwrap();
        assert!(app.raw_mode, "R must have switched to raw mode");

        handle_key(
            &mut app,
            KeyCode::Char('G'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            Some(&st),
            &diff_records,
            &mut root,
            None,
        )
        .unwrap();
        assert_eq!(
            app.diff_scroll, app.diff_max,
            "End must scroll the raw viewport, since R just switched to raw"
        );
        assert_eq!(
            app.diff_parsed_scroll, 0,
            "End must not touch the parsed viewport once raw mode is active"
        );
    }

    /// `f` toggles the Activity selected-slot filter on and off.
    #[test]
    fn f_toggles_the_activity_slot_filter() {
        let mut app = test_app();
        app.open_main(MainTab::Activity);
        app.selected_slot = 2;
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");
        assert!(app.activity_slot_filter.is_none());
        handle_key(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert_eq!(app.activity_slot_filter, Some(2));
        handle_key(
            &mut app,
            KeyCode::Char('f'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert_eq!(app.activity_slot_filter, None);
    }

    /// AC-13: every new binding is inert while an attached Shell pane owns the PTY
    /// — the key must forward to the pane instead of moving the record cursor,
    /// folding, toggling raw mode, or changing the slot filter.
    #[test]
    fn structural_navigation_keys_are_inert_while_shell_owns_the_pty() {
        let records = sample_log_records();
        let mut app = test_app();
        app.open_main(MainTab::Shell);
        app.terminal_pane = Some(crate::terminal::TerminalPane::new(24, 80));
        let sw = SparPaths::new(std::path::Path::new("/x"));
        let mut root = PathBuf::from("/x");

        for code in [
            KeyCode::Char('J'),
            KeyCode::Char('t'),
            KeyCode::Char('e'),
            KeyCode::Char(' '),
            KeyCode::Char('A'),
            KeyCode::Char('R'),
            KeyCode::Char('f'),
        ] {
            handle_key(
                &mut app,
                code,
                KeyModifiers::empty(),
                &sw,
                &[],
                &[],
                &[],
                None,
                &records,
                &mut root,
                None,
            )
            .unwrap();
        }
        assert!(
            app.record_cursor.is_none(),
            "the PTY must have swallowed every navigation key"
        );
        assert!(app.fold_open.is_empty());
        assert!(!app.fold_all);
        assert!(!app.raw_mode);
        assert!(app.activity_slot_filter.is_none());
        assert_eq!(
            app.main_tab,
            MainTab::Shell,
            "none of these keys leave Shell"
        );
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
        emit_attention_toasts(&mut app, &runs, false);
        assert!(app.flash.is_none(), "initial fleet is never toasted");
        // A run that was working and is now working: still silent.
        let runs = vec![summary_phase("r0", Phase::Review)];
        emit_attention_toasts(&mut app, &runs, false);
        assert!(app.flash.is_none());
        // Now it crosses into a gate: toast fires.
        let runs = vec![summary_phase("r0", Phase::AwaitingPlanApproval)];
        emit_attention_toasts(&mut app, &runs, false);
        assert!(app.flash.is_some(), "gate transition toasts");
    }

    /// A population swap (project runs <-> Home's cross-project rows) is not a
    /// transition: a gated run that was simply absent from the other population's
    /// baseline must not be re-announced on every Home <-> project navigation
    /// (round-9 review).
    #[test]
    fn toasts_reprime_silently_across_a_population_swap() {
        let mut app = test_app();
        let project_runs = vec![summary_phase("r0", Phase::AwaitingPlanApproval)];
        emit_attention_toasts(&mut app, &project_runs, false);
        app.flash = None;
        // Home's population includes the same already-gated run: swapping population
        // must not re-fire the toast.
        let home_runs = vec![summary_phase("r0", Phase::AwaitingPlanApproval)];
        emit_attention_toasts(&mut app, &home_runs, true);
        assert!(
            app.flash.is_none(),
            "population swap must not re-announce a gate"
        );
        // Swapping back to the project population is likewise silent.
        emit_attention_toasts(&mut app, &project_runs, false);
        assert!(
            app.flash.is_none(),
            "swapping back must not re-announce either"
        );
        // A real transition within one population still fires.
        let now_idle = vec![summary_phase("r0", Phase::Review)];
        emit_attention_toasts(&mut app, &now_idle, false);
        app.flash = None;
        emit_attention_toasts(&mut app, &project_runs, false);
        assert!(
            app.flash.is_some(),
            "a real transition within one population still toasts"
        );
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
        let activity = vec![
            Record {
                kind: RecordKind::Section,
                glyph: "§",
                verb: "Timeline".to_string(),
                head: "Timeline".to_string(),
                summary: String::new(),
                body: Vec::new(),
                time: None,
                elapsed: None,
                actor: None,
                ok: None,
                source: SourceId::Activity {
                    at_millis: 0,
                    sequence: 1,
                },
                folded_by_default: false,
                has_command_row: false,
            },
            Record {
                kind: RecordKind::Note,
                glyph: "·",
                verb: "phase".to_string(),
                head: "phase".to_string(),
                summary: "impl done".to_string(),
                body: Vec::new(),
                time: None,
                elapsed: None,
                actor: Some("impl".to_string()),
                ok: None,
                source: SourceId::Activity {
                    at_millis: 0,
                    sequence: 2,
                },
                folded_by_default: false,
                has_command_row: false,
            },
        ];
        let diff_text = "diff --git a/src/a.rs b/src/a.rs\n@@ -1,2 +1,3 @@\n+new line\n-old line\n";
        let diff_records = record::parse_diff(diff_text, "wt");
        let plan_docs_v = record::parse_document(
            "plan.md",
            "# Plan\nDo the thing.\n\n## Risks\nWatch out for X.\n",
            "plan.md",
        );
        let review_v = vec![Record {
            kind: RecordKind::Criterion,
            glyph: "▤",
            verb: "AC-1".to_string(),
            head: "AC-1".to_string(),
            summary: "impl: pass".to_string(),
            body: vec!["impl: pass".to_string()],
            time: None,
            elapsed: None,
            actor: None,
            ok: None,
            source: SourceId::Document {
                path: "contract#AC-1".to_string(),
                start: 0,
            },
            folded_by_default: false,
            has_command_row: false,
        }];
        term.draw(|f| {
            draw(
                f,
                &swarm,
                projects,
                runs,
                full,
                "→ Bash  read the contract\n← ✓ toolu_01HqnTTSQH5m7ZWYJVAtA7Vj ok\n",
                "→ Bash  read the contract\n← ✓ toolu_01HqnTTSQH5m7ZWYJVAtA7Vj ok\n",
                &[],
                &activity,
                diff_text,
                &diff_records,
                &plan_docs_v,
                &review_v,
                &[],
                &HomeData::default(),
                None,
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

    #[test]
    fn loading_home_reserves_visible_skeleton_rows_at_narrow_width() {
        let root = PathBuf::from("/nonexistent/definitely-not-here");
        let snap = Snapshot::loading(&root);
        assert!(
            snap.home.loading,
            "only the first cross-project snapshot loads"
        );

        let mut term = Terminal::new(TestBackend::new(79, 24)).unwrap();
        let mut app = App::new(None, Config::default(), None);
        app.focus = Focus::Rail;
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &snap.swarm,
                &snap.projects,
                &snap.runs,
                snap.full.as_ref(),
                &snap.stream_text,
                &snap.stream_text_raw,
                &snap.log_records,
                &snap.activity,
                &snap.diff_text,
                &snap.diff_records,
                &snap.plan_docs,
                &snap.review,
                &snap.chat,
                &snap.home,
                snap.log_stats.as_ref(),
                &mut app,
                &mut rail,
            )
        })
        .unwrap();

        assert_eq!(
            app.focus,
            Focus::Rail,
            "loading Home must not autofocus away from its reserved rows"
        );
        let text = (0..24).map(|y| row(&term, y)).collect::<String>();
        assert!(
            text.contains('░'),
            "loading Home painted no skeleton: {text:?}"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn crossing_the_tab_breakpoint_keeps_painted_and_clickable_geometry_together() {
        // Clock-free via TabStripMotion direct, plus a paint verification that the
        // draw path uses the interpolated geometry. Avoids wall-clock flakiness under
        // loaded `cargo test -j N`.
        let st = run_with(Phase::Review, 3);
        let swarm = SparPaths::new("/x");
        let capture = |width: u16| -> (Vec<(MainTab, Rect)>, Vec<(MainTab, Rect)>) {
            let mut app = test_app();
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let lay = layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                Focus::Main,
                false,
                false,
            );
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
                .unwrap();
            let glyphs = app
                .main_tab_glyphs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            let hits = app
                .main_tabs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            (glyphs, hits)
        };
        let (from_glyphs, from_hits) = capture(79);
        let (to_glyphs, to_hits) = capture(80);
        let start = Instant::now();
        let mut motion = TabStripMotion::new();
        motion.observe(from_glyphs.clone(), from_hits.clone(), start);
        motion.observe(to_glyphs.clone(), to_hits.clone(), start);
        assert!(
            motion.is_animating(start),
            "a 79-to-80 tab placement change must glide"
        );
        let mid = start + crate::motion::STRIP_PERIOD / 2;
        let (moving_glyphs, moving_hits) = motion.displayed(mid);
        assert_eq!(moving_glyphs.len(), MAIN_TABS.len());
        assert_eq!(moving_hits.len(), MAIN_TABS.len());
        for ((tab, glyph), (hit_tab, hit)) in moving_glyphs.iter().zip(&moving_hits) {
            assert_eq!(tab, hit_tab);
            assert!(
                hit.x <= glyph.x && glyph.right() <= hit.right(),
                "{tab:?} glyph {glyph:?} escaped its clickable rect {hit:?} at mid"
            );
        }
        assert!(
            moving_glyphs
                .windows(2)
                .all(|pair| pair[0].1.right() <= pair[1].1.x),
            "moving tab glyphs overlap at mid: {moving_glyphs:?}"
        );
        assert!(
            moving_hits
                .windows(2)
                .all(|pair| pair[0].1.right() <= pair[1].1.x),
            "moving tab hit rects overlap at mid: {moving_hits:?}"
        );
        let mid_is_intermediate = moving_glyphs
            .iter()
            .zip(from_glyphs.iter())
            .any(|((_, g), (_, f))| g != f)
            && moving_glyphs
                .iter()
                .zip(to_glyphs.iter())
                .any(|((_, g), (_, t))| g != t);
        assert!(
            mid_is_intermediate,
            "mid-flight glyphs must be between 79 and 80 endpoints"
        );
        // Verify the paint path also yields in-flight geometry and settles to fresh.
        let mut app = test_app();
        let paint_labels = |width: u16, app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let lay = layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                Focus::Main,
                false,
                false,
            );
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), app))
                .unwrap();
            (app.main_tab_glyphs.clone(), app.main_tabs.clone())
        };
        let _ = paint_labels(79, &mut app);
        let _ = paint_labels(80, &mut app);
        // Motion is still in flight immediately after the 79->80 paint, using the same
        // clock-free check via the underlying motion.
        assert!(
            app.tab_strip.is_animating(Instant::now()),
            "paint path 79->80 must start a glide"
        );
        app.settle_motion();
        let (settled_glyphs, settled_hits) = paint_labels(80, &mut app);
        let mut fresh = test_app();
        let (fresh_glyphs, fresh_hits) = paint_labels(80, &mut fresh);
        // Compare as sets of (tab, rect) via captured geometry; painted geometry
        // should equal fresh 80-column geometry.
        assert_eq!(
            settled_glyphs.len(),
            fresh_glyphs.len(),
            "settled strip differs from a fresh 80-column paint"
        );
        for ((sg, st_idx), (fg, ft_idx)) in settled_glyphs.iter().zip(fresh_glyphs.iter()) {
            assert_eq!(sg, fg, "glyph geometry differs: {sg:?} vs {fg:?}");
            assert_eq!(st_idx, ft_idx);
        }
        for ((sh, st_idx), (fh, ft_idx)) in settled_hits.iter().zip(fresh_hits.iter()) {
            assert_eq!(sh, fh, "hit geometry differs: {sh:?} vs {fh:?}");
            assert_eq!(st_idx, ft_idx);
        }
        // Also verify motion settles to exactly the target geometry.
        let after = start + crate::motion::STRIP_PERIOD + Duration::from_millis(10);
        assert!(!motion.is_animating(after));
        let (settled_g, settled_h) = motion.displayed(after);
        assert_eq!(settled_g, to_glyphs);
        assert_eq!(settled_h, to_hits);
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn tab_strip_mid_flight_geometry_stays_non_overlapping_and_clickable() {
        let st = run_with(Phase::Review, 3);
        let swarm = SparPaths::new("/x");
        let capture = |width: u16| -> (Vec<(MainTab, Rect)>, Vec<(MainTab, Rect)>) {
            let mut app = test_app();
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let lay = layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                Focus::Main,
                false,
                false,
            );
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
                .unwrap();
            let glyphs = app
                .main_tab_glyphs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            let hits = app
                .main_tabs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            (glyphs, hits)
        };
        let (from_glyphs, from_hits) = capture(79);
        let (to_glyphs, to_hits) = capture(80);
        assert_eq!(from_glyphs.len(), MAIN_TABS.len());
        assert_eq!(to_glyphs.len(), MAIN_TABS.len());
        let mut motion = TabStripMotion::new();
        let start = Instant::now();
        motion.observe(from_glyphs.clone(), from_hits.clone(), start);
        let (first_displayed_g, first_displayed_h) =
            motion.observe(to_glyphs.clone(), to_hits.clone(), start);
        assert!(motion.is_animating(start), "79->80 must start a glide");
        // Verify the first displayed frame (t=0) is non-overlapping and clickable
        assert!(
            first_displayed_g
                .windows(2)
                .all(|pair| pair[0].1.right() <= pair[1].1.x),
            "first frame glyphs overlap: {first_displayed_g:?}"
        );
        assert!(
            first_displayed_h
                .windows(2)
                .all(|pair| pair[0].1.right() <= pair[1].1.x),
            "first frame hits overlap: {first_displayed_h:?}"
        );
        for (g, h) in first_displayed_g.iter().zip(first_displayed_h.iter()) {
            assert!(
                h.1.x <= g.1.x && g.1.right() <= h.1.right(),
                "first frame glyph {g:?} escaped hit {h:?}"
            );
        }
        let period = crate::motion::STRIP_PERIOD;
        let mut saw_intermediate = false;
        for i in 0..=400 {
            let now = start + period.mul_f32(i as f32 / 400.0);
            let (glyphs, hits) = motion.displayed(now);
            assert_eq!(glyphs.len(), MAIN_TABS.len());
            assert_eq!(hits.len(), MAIN_TABS.len());
            for ((tab, g), (hit_tab, h)) in glyphs.iter().zip(hits.iter()) {
                assert_eq!(tab, hit_tab);
                assert!(
                    h.x <= g.x && g.right() <= h.right(),
                    "t={i}/400 glyph {g:?} escaped hit {h:?}"
                );
            }
            assert!(
                glyphs
                    .windows(2)
                    .all(|pair| pair[0].1.right() <= pair[1].1.x),
                "t={i}/400 glyphs overlap: {glyphs:?}"
            );
            assert!(
                hits.windows(2).all(|pair| pair[0].1.right() <= pair[1].1.x),
                "t={i}/400 hits overlap: {hits:?}"
            );
            if i > 0 && i < 400 {
                let at_start = glyphs
                    .iter()
                    .zip(from_glyphs.iter())
                    .all(|((_, g), (_, f))| g == f);
                let at_end = glyphs
                    .iter()
                    .zip(to_glyphs.iter())
                    .all(|((_, g), (_, t))| g == t);
                if !at_start && !at_end {
                    saw_intermediate = true;
                }
            }
        }
        assert!(
            saw_intermediate,
            "glide produced no intermediate distinct frame"
        );
        let after = start + period + Duration::from_millis(50);
        assert!(
            !motion.is_animating(after),
            "strip must be settled after period"
        );
        let (settled_g, settled_h) = motion.displayed(after);
        assert_eq!(settled_g, to_glyphs);
        assert_eq!(settled_h, to_hits);
    }

    #[test]
    fn selected_run_key_tracks_navigation_and_clicks() {
        let mut runs = vec![
            home_run("a", Phase::Review, 1, "spar"),
            home_run("b", Phase::Review, 2, "spar"),
            home_run("c", Phase::Review, 3, "spar"),
        ];
        runs[0].unit_id = Some("unit-a".into());
        runs[1].unit_id = Some("unit-b".into());
        runs[2].unit_id = Some("unit-c".into());
        let mut app = test_app();
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        app.selected_run_key = Some(run_row_key(&runs[0]));
        rail_move(&mut app, &[], &[], &runs, 0, 1);
        assert_eq!(app.selected_run, 1);
        assert_eq!(
            app.selected_run_key.as_deref(),
            Some(run_row_key(&runs[1]).as_str())
        );
        let next = app.selected_run;
        rail_select(&mut app, 2, 0, &[], &runs, 0);
        assert_eq!(app.selected_run, 2);
        assert_eq!(
            app.selected_run_key.as_deref(),
            Some(run_row_key(&runs[2]).as_str())
        );
        let _ = next;
        let mut app2 = test_app();
        app2.browse = BrowseLevel::Runs;
        app2.selected_run = 0;
        app2.selected_run_key = Some(run_row_key(&runs[0]));
        let mut attention = vec![
            home_run("x", Phase::Review, 1, "spar"),
            home_run("y", Phase::AwaitingPlanApproval, 2, "spar"),
        ];
        attention[0].unit_id = Some("unit-x".into());
        attention[1].unit_id = Some("unit-y".into());
        attention[1].wants = 1;
        jump_to_attention(&mut app2, &attention, &[]);
        assert_eq!(app2.selected_run_key.as_deref(), Some("run:unit-y"));
        let rows: Vec<HomeRow> = Vec::new();
        let _ = rows;
    }

    #[test]
    fn folded_unit_identity_survives_through_animated_order() {
        // AC-4 plumbing: a selected folded unit whose representative leg changes
        // must remain selected when resolved against the animated order. This pins
        // the run_loop glue (selected_run_key -> animated runs) not just run_row_key.
        let mut leg_a = home_run("leg-a", Phase::Review, 1, "spar");
        let mut leg_b = home_run("leg-b", Phase::Review, 1, "spar");
        leg_a.unit_id = Some("unit-1".into());
        leg_b.unit_id = Some("unit-1".into());
        assert_eq!(run_row_key(&leg_a), run_row_key(&leg_b));
        let mut other = home_run("other", Phase::Review, 2, "spar");
        other.unit_id = Some("unit-other".into());
        let mut app = test_app();
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        app.selected_run_key = Some(run_row_key(&leg_a));
        // Simulate a reorder where the folded unit's representative changes
        // (leg-a -> leg-b) and its rank moves from 0 to 1. Both the identity
        // glue and the physical travel must be correct in the same transition.
        let snap_runs_a = [leg_a.clone(), other.clone()];
        let snap_runs_b = [other.clone(), leg_b.clone()];
        let now = Instant::now();
        let keys_a: Vec<String> = snap_runs_a.iter().map(run_row_key).collect();
        let keys_b: Vec<String> = snap_runs_b.iter().map(run_row_key).collect();
        assert_ne!(
            keys_a, keys_b,
            "keys must differ in order to drive animation"
        );
        assert_eq!(
            keys_a,
            vec!["run:unit-1".to_string(), "run:unit-other".to_string()]
        );
        assert_eq!(
            keys_b,
            vec!["run:unit-other".to_string(), "run:unit-1".to_string()]
        );
        app.rail_motion
            .observe(BrowseLevel::Runs, keys_a.clone(), now);
        let perm = app
            .rail_motion
            .observe(BrowseLevel::Runs, keys_b.clone(), now)
            .expect("reorder with changed rank must animate");
        // At t=0 the displayed order is still the old order: unit-1 at index 0.
        // The animated slice is built by mapping target indices through the
        // permutation, which reconstructs displayed order from the target slice.
        let v0: Vec<state::RunSummary> = perm.iter().map(|&i| snap_runs_b[i].clone()).collect();
        assert_eq!(v0.len(), 2);
        assert_eq!(run_row_key(&v0[0]), "run:unit-1");
        assert_eq!(
            v0[0].id, "leg-b",
            "displayed representative must be the new leg, not stale leg-a"
        );
        if let Some(key) = app.selected_run_key.clone() {
            if let Some(pos) = v0.iter().position(|r| run_row_key(r) == key) {
                app.selected_run = pos;
            }
        }
        assert_eq!(
            app.selected_run, 0,
            "cursor must stay glued to unit-1 at start of travel"
        );
        app.selected_run_key = Some(run_row_key(&v0[app.selected_run]));
        // Mid-flight the unit travels through the adjacent rank.
        let perm_mid = app
            .rail_motion
            .observe(
                BrowseLevel::Runs,
                keys_b.clone(),
                now + crate::motion::REORDER_PERIOD / 2,
            )
            .unwrap_or_else(|| (0..keys_b.len()).collect());
        let vmid: Vec<state::RunSummary> =
            perm_mid.iter().map(|&i| snap_runs_b[i].clone()).collect();
        assert_eq!(vmid.len(), 2);
        assert!(vmid.iter().any(|r| run_row_key(r) == "run:unit-1"));
        // After settling the unit is at its target rank (index 1).
        let settled = app.rail_motion.observe(
            BrowseLevel::Runs,
            keys_b.clone(),
            now + crate::motion::REORDER_PERIOD + Duration::from_millis(10),
        );
        assert!(settled.is_none(), "settled reorder must report identity");
        // Resolve once more against the settled target order.
        let final_runs: &[state::RunSummary] = &snap_runs_b;
        if let Some(key) = app.selected_run_key.clone() {
            if let Some(pos) = final_runs.iter().position(|r| run_row_key(r) == key) {
                app.selected_run = pos;
            }
        }
        assert_eq!(
            app.selected_run, 1,
            "cursor must follow unit-1 to its target rank after settle"
        );
        assert_eq!(final_runs[app.selected_run].id, "leg-b");
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn tab_strip_does_not_restart_and_settles_without_extra_retarget() {
        let st = run_with(Phase::Review, 3);
        let swarm = SparPaths::new("/x");
        // Clock-free via TabStripMotion direct to avoid wall-clock flakiness.
        let capture = |width: u16| -> (Vec<(MainTab, Rect)>, Vec<(MainTab, Rect)>) {
            let mut app = test_app();
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let lay = layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                Focus::Main,
                false,
                false,
            );
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
                .unwrap();
            let glyphs = app
                .main_tab_glyphs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            let hits = app
                .main_tabs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            (glyphs, hits)
        };
        let (from_glyphs, from_hits) = capture(79);
        let (to_glyphs, to_hits) = capture(80);
        let start = Instant::now();
        let mut motion = TabStripMotion::new();
        motion.observe(from_glyphs.clone(), from_hits.clone(), start);
        motion.observe(to_glyphs.clone(), to_hits.clone(), start);
        assert!(motion.is_animating(start), "79->80 must start a glide");
        let (second_glyphs, _) = motion.displayed(start);
        assert_eq!(
            second_glyphs.len(),
            to_glyphs.len(),
            "stable target must not change tab count mid-glide"
        );
        assert!(
            motion.is_animating(start + Duration::from_millis(10)),
            "glide must still be in flight shortly after start"
        );
        // Paint path also must not restart on same target.
        let mut app = test_app();
        let paint = |width: u16, app: &mut App| {
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let lay = layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                Focus::Main,
                false,
                false,
            );
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), app))
                .unwrap();
            (app.main_tab_glyphs.clone(), app.main_tabs.clone())
        };
        let _ = paint(79, &mut app);
        let _ = paint(80, &mut app);
        assert!(
            app.tab_strip.is_animating(Instant::now()),
            "paint 79->80 must start a glide"
        );
        let _ = paint(80, &mut app);
        assert!(
            app.tab_strip.is_animating(Instant::now()),
            "stable repaint must keep glide in flight"
        );
        app.settle_motion();
        assert!(
            !app.tab_strip.is_animating(Instant::now()),
            "settled strip must report not animating"
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn moving_tabs_do_not_overpaint_rail_title_or_context_caption() {
        // 79->80 glide occupies the rail-title portion of the labels row mid-flight.
        // While in flight, the rail title and main_context caption must be suppressed
        // whenever they would intersect a moving glyph, otherwise a tab would be
        // painted under a label.
        let st = run_with(Phase::Review, 3);
        let swarm = SparPaths::new("/x");
        let capture = |width: u16| -> (Vec<(MainTab, Rect)>, Vec<(MainTab, Rect)>) {
            let mut app = test_app();
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            let lay = layout_rects(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 30,
                },
                Focus::Main,
                false,
                false,
            );
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
                .unwrap();
            let glyphs = app
                .main_tab_glyphs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            let hits = app
                .main_tabs
                .iter()
                .map(|(r, t)| (*t, *r))
                .collect::<Vec<_>>();
            (glyphs, hits)
        };
        let (from_glyphs, from_hits) = capture(79);
        let (to_glyphs, to_hits) = capture(80);
        let start = Instant::now();
        let mut motion = TabStripMotion::new();
        motion.observe(from_glyphs.clone(), from_hits.clone(), start);
        motion.observe(to_glyphs.clone(), to_hits.clone(), start);
        assert!(motion.is_animating(start));
        let lay80 = layout_rects(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 30,
            },
            Focus::Main,
            false,
            false,
        );
        let mid = start + crate::motion::STRIP_PERIOD / 2;
        let (mid_glyphs, _) = motion.displayed(mid);
        let rail_overlapped = mid_glyphs.iter().any(|(_, r)| r.x < lay80.rail.right());
        assert!(
            rail_overlapped,
            "mid-flight 79->80 glyphs must overlap rail for suppression test: {mid_glyphs:?} rail {:?}",
            lay80.rail
        );
        let ctx = main_context(&swarm, Some(&st), &test_app());
        let used = mid_glyphs.iter().map(|(_, r)| r.width).sum::<u16>();
        let room = lay80.main.width.saturating_sub(used).saturating_sub(1);
        assert!(
            !ctx.is_empty() && room > 2,
            "caption must be renderable at 80 for suppression test: ctx {ctx:?} room {room} used {used}"
        );
        let text = truncate(&ctx, room as usize);
        let w = text.chars().count() as u16;
        let caption_rect = Rect {
            x: lay80.main.right().saturating_sub(w + 1),
            y: lay80.labels.y,
            width: w,
            height: 1,
        };
        // Caption is unconditionally suppressed while the strip glides; the
        // paint path below verifies the caption cells are blank mid-flight.
        // Keep the geometric calculation for documentation but do not assert
        // a tautology: the real check is the buffer inspection after paint.
        let _caption_overlapped = mid_glyphs
            .iter()
            .any(|(_, r)| r.x < caption_rect.right() && caption_rect.x < r.right());
        assert!(
            motion.is_animating(mid),
            "mid-flight must be animating for caption suppression test"
        );
        // Also verify via actual paint that suppression occurs: paint 79 then 80
        // with same App and inspect buffer for rail title absence mid-flight.
        let mut app = test_app();
        let mut term79 = Terminal::new(TestBackend::new(79, 30)).unwrap();
        let lay79 = layout_rects(
            Rect {
                x: 0,
                y: 0,
                width: 79,
                height: 30,
            },
            Focus::Main,
            false,
            false,
        );
        term79
            .draw(|f| draw_labels(f, &lay79, &swarm, &[], &[], Some(&st), &mut app))
            .unwrap();
        // Now app is at 79 geometry, next paint at 80 will be mid-flight (t ~0)
        let mut term80 = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let lay80b = layout_rects(
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 30,
            },
            Focus::Main,
            false,
            false,
        );
        term80
            .draw(|f| draw_labels(f, &lay80b, &swarm, &[], &[], Some(&st), &mut app))
            .unwrap();
        // In-flight, rail title should be suppressed if glyphs overlap rail.
        // Check buffer: rail title area should not contain "RUNS" or "HOME" if overlapped.
        // Caption is unconditionally suppressed while gliding, so its cells must be blank mid-flight.
        if app.tab_strip.is_animating(Instant::now()) {
            let glyphs_now = app
                .main_tab_glyphs
                .iter()
                .map(|(r, _)| *r)
                .collect::<Vec<_>>();
            let overlapped = glyphs_now.iter().any(|r| r.x < lay80b.rail.right());
            if overlapped {
                let buf = term80.backend().buffer();
                let row: String = (lay80b.rail.x..lay80b.rail.right())
                    .map(|x| buf[(x, lay80b.labels.y)].symbol())
                    .collect();
                assert!(
                    row.trim().is_empty() || !row.contains("RUNS"),
                    "rail title painted over moving tab at mid-flight: {row:?} glyphs {glyphs_now:?}"
                );
            }
            let ctx = main_context(&swarm, Some(&st), &app);
            let used = app
                .main_tab_glyphs
                .iter()
                .map(|(r, _)| r.width)
                .sum::<u16>();
            let room = lay80b.main.width.saturating_sub(used).saturating_sub(1);
            if !ctx.is_empty() && room > 2 {
                let text = truncate(&ctx, room as usize);
                let w = text.chars().count() as u16;
                let caption_rect = Rect {
                    x: lay80b.main.right().saturating_sub(w + 1),
                    y: lay80b.labels.y,
                    width: w,
                    height: 1,
                };
                let buf = term80.backend().buffer();
                let row: String = (caption_rect.x..caption_rect.right())
                    .map(|x| buf[(x, caption_rect.y)].symbol())
                    .collect();
                assert!(
                    !row.contains(text.trim()) && row.trim() != text.trim(),
                    "caption painted over moving tab at mid-flight: row {row:?} should not contain caption {text:?} rect {caption_rect:?}"
                );
            }
        }
        // Settled must restore rail title and caption.
        app.settle_motion();
        let mut term80s = Terminal::new(TestBackend::new(80, 30)).unwrap();
        term80s
            .draw(|f| draw_labels(f, &lay80b, &swarm, &[], &[], Some(&st), &mut app))
            .unwrap();
        let buf = term80s.backend().buffer();
        let row: String = (lay80b.rail.x..lay80b.rail.right())
            .map(|x| buf[(x, lay80b.labels.y)].symbol())
            .collect();
        assert!(
            row.contains("RUNS") || row.contains("HOME") || !row.trim().is_empty(),
            "settled rail title missing after glide: {row:?}"
        );
        {
            let ctx = main_context(&swarm, Some(&st), &app);
            let used = app
                .main_tab_glyphs
                .iter()
                .map(|(r, _)| r.width)
                .sum::<u16>();
            let room = lay80b.main.width.saturating_sub(used).saturating_sub(1);
            if !ctx.is_empty() && room > 2 {
                let text = truncate(&ctx, room as usize);
                let w = text.chars().count() as u16;
                let caption_rect = Rect {
                    x: lay80b.main.right().saturating_sub(w + 1),
                    y: lay80b.labels.y,
                    width: w,
                    height: 1,
                };
                let row: String = (caption_rect.x..caption_rect.right())
                    .map(|x| buf[(x, caption_rect.y)].symbol())
                    .collect();
                assert!(
                    !row.trim().is_empty() && row.contains(text.chars().next().unwrap_or(' ').to_string().as_str()) || row.trim() == text.trim(),
                    "settled caption missing after glide: row {row:?} expected {text:?} rect {caption_rect:?}"
                );
            }
        }
    }

    /// The breakpoints the band/column arithmetic actually branches on — every
    /// full-grid sweep below enumerates these exhaustively regardless of how
    /// coarsely it samples the rest of the range, since a layout bug lives at a
    /// breakpoint or nowhere.
    const BREAKPOINT_SIZES: [(u16, u16); 11] = [
        (1, 1),
        (20, 5),
        (35, 24),
        (46, 24),
        (79, 24),
        (80, 24),
        (89, 24),
        (90, 24),
        (119, 40),
        (120, 40),
        (200, 60),
    ];

    /// Every size from a single cell up, on the default (`Log`) tab: ratatui panics
    /// on any Rect that leaves the buffer, and the band arithmetic has four
    /// breakpoints. Sampled, not exhaustive (round-review AC-20 finding): a fully
    /// exhaustive `1..=200` x `1..=60` grid across this test and its sibling below
    /// measured at 420s and 35+ minutes respectively on this box, which made
    /// `cargo test` impossible to run locally — the contract's own "existing…
    /// sweep" language names the sampled grid this feature's base commit already
    /// used, not a new exhaustive one. `BREAKPOINT_SIZES` below still hits every
    /// branch in the band arithmetic exactly, on every tab.
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
        // AC-20: the sweep above only ever painted the default (`Log`) tab. Every
        // structured-view tab must survive the same breakpoints — a per-pixel
        // sweep on top of the Log tab's own is redundant with it (the same
        // `Columns::for_width` breakpoints drive every tab's layout), so this
        // checks the exact points a layout bug would appear at instead of paying
        // for a second full grid.
        for tab in [
            MainTab::Activity,
            MainTab::Diff,
            MainTab::Plan,
            MainTab::Review,
            MainTab::Shell,
        ] {
            for &(w, h) in &BREAKPOINT_SIZES {
                paint_with(w, h, &[], &[], Some(&st), |a| a.open_main(tab));
            }
        }
        // The breakpoints themselves, and the no-run path.
        for &(w, h) in &BREAKPOINT_SIZES {
            paint(w, h, &[], &[], Some(&st));
            paint(w, h, &[], &[], None);
        }
    }

    /// AC-20: every new record view — not just whatever tab the default sweep
    /// happens to leave the app on — must survive the width/height range, folded,
    /// expanded (`A`), and raw (`R`, where available). `renders_at_every_size_...`
    /// above never switched `main_tab`, so Activity/Diff/Plan/Review were never
    /// actually painted by it.
    #[test]
    fn structured_views_survive_every_tab_fold_and_raw_state() {
        let st = run_with(Phase::AwaitingShipConfirm, 7);
        // `BREAKPOINT_SIZES`, not a full grid (round-review finding: a fully
        // exhaustive 1..=200 x 1..=60 sweep here is 32 tab/fold/raw/run-state
        // combinations x 12,000 sizes — measured past 35 minutes, still not
        // finished, on this box). Per-pixel panic-hunting for the Log tab already
        // happens in `renders_at_every_size_without_panicking`; what this test
        // adds is the fold/raw/run-state combinations, which the breakpoints
        // exercise at every size the layout math actually branches on.
        // `full: None` (no run selected — Home, or Runs with nothing highlighted)
        // is covered alongside a real run: every non-Shell tab falls back to the
        // same coherent empty message in that state, and that fallback path is a
        // paint site of its own, not exercised by the `Some(&st)` leg.
        for full in [Some(&st), None] {
            for tab in [
                MainTab::Log,
                MainTab::Activity,
                MainTab::Diff,
                MainTab::Plan,
                MainTab::Review,
                MainTab::Shell,
            ] {
                // `R` only exists for Log/Diff (AC-14); every other tab always
                // paints with `raw_mode == false`, so looping the second state
                // there would only repeat identical paints.
                let raw_states: &[bool] = if matches!(tab, MainTab::Log | MainTab::Diff) {
                    &[false, true]
                } else {
                    &[false]
                };
                for fold_all in [false, true] {
                    for &raw_mode in raw_states {
                        for &(w, h) in &BREAKPOINT_SIZES {
                            paint_with(w, h, &[], &[], full, |a| {
                                a.open_main(tab);
                                a.fold_all = fold_all;
                                a.raw_mode = raw_mode;
                            });
                        }
                    }
                }
            }
        }
    }

    fn draw_record_view(
        records: &[Record],
        width: u16,
        height: u16,
        cursor: Option<&SourceId>,
        fold_all: bool,
    ) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let mut scroll = 0u16;
        let mut follow = false;
        let fold_open = std::collections::HashSet::new();
        let mut cursor_dirty = false;
        term.draw(|f| {
            render_record_view(
                f,
                area,
                records,
                &mut scroll,
                &mut follow,
                &fold_open,
                fold_all,
                cursor,
                &mut cursor_dirty,
                None,
            );
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    /// AC-19 (round-9 finding 7): nothing before this asserted `SURFACE_RAISED`,
    /// `SURFACE_SUNKEN`, and `CODE` actually reach the screen — only that they
    /// have *a* caller (`rg` in the finding's own verify line). Pin what each
    /// caller paints, not just that the call exists.
    #[test]
    fn record_view_paints_surface_raised_surface_sunken_and_code() {
        let records = record::parse_log_records(
            "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\n",
            0,
            &[],
            &record::PathShortener::default(),
            "r",
            "s",
        )
        .iter()
        .map(|lr| lr.to_record())
        .collect::<Vec<_>>();
        let cursor = records[0].source.clone();
        // `fold_all: true` expands the tool's body so its command row (SURFACE_SUNKEN,
        // CODE) actually paints; the cursor row (SURFACE_RAISED) is the head above it.
        let buf = draw_record_view(&records, 60, 6, Some(&cursor), true);
        assert_eq!(
            buf[(0, 0)].bg,
            SURFACE_RAISED,
            "the cursor's head row must paint SURFACE_RAISED"
        );
        let cols = record::Columns::for_width(59);
        assert_eq!(
            buf[(0, 1)].bg,
            SURFACE_SUNKEN,
            "an expanded tool body row must paint SURFACE_SUNKEN"
        );
        assert_eq!(
            buf[(cols.verb, 1)].fg,
            CODE,
            "the command row (body[0]) must paint CODE"
        );
    }

    /// AC-5 (round-9 finding 7): prose and a tool call must be skimmable apart by
    /// glyph *and* weight, not merely by reading the text. Pins both.
    #[test]
    fn prose_and_tool_heads_differ_in_glyph_and_weight() {
        // A nonzero `start_offset` is a mid-stream tail read, past the spawn
        // header/prompt echo the parser suppresses only at offset 0 — otherwise
        // "Checking scope." here would be swallowed as prompt dump, not parsed
        // as its own Prose record (mirrors `process::tests::
        // indexed_parse_recognizes_a_marker_that_is_not_chunk_initial`).
        let records = record::parse_log_records(
            "Checking scope.\n→ Bash  ls -la\n",
            1000,
            &[],
            &record::PathShortener::default(),
            "r",
            "s",
        )
        .iter()
        .map(|lr| lr.to_record())
        .collect::<Vec<_>>();
        let prose = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Prose))
            .expect("prose record");
        let tool = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Tool(_)))
            .expect("tool record");
        assert_ne!(
            prose.glyph, tool.glyph,
            "prose and a tool call must use different glyphs"
        );
        // >=80 columns: below that, `Columns::for_width` folds the verb column
        // into the summary span (`cols.verb_folded`), which is a different,
        // already-covered case (AC-4) — this test targets the dedicated verb span.
        let buf = draw_record_view(&records, 90, 6, None, false);
        let cols = record::Columns::for_width(89);
        // Prose carries no verb text (`to_record`'s wildcard arm), so it never
        // gets the bold verb span a tool call's head always does.
        assert!(
            buf[(cols.verb, 1)].modifier.contains(Modifier::BOLD),
            "a tool call's verb must be bold: {:?}",
            buf[(cols.verb, 1)]
        );
    }

    /// AC-4 (round-9 finding 7): `record::Columns::for_width`'s own breakpoint
    /// arithmetic is pinned in `record::tests`, but nothing before this asserted
    /// the *painted* glyph actually lands where that arithmetic says it should —
    /// a span-offset bug upstream of the column math would pass every existing
    /// test. Checked at every named breakpoint, for both a short and a very long
    /// summary, so content length cannot move it either.
    #[test]
    fn record_view_paints_the_glyph_at_the_reserved_column_across_breakpoints() {
        let short = record::parse_log_records(
            "→ Bash  x\n",
            0,
            &[],
            &record::PathShortener::default(),
            "r",
            "s",
        )
        .iter()
        .map(|lr| lr.to_record())
        .collect::<Vec<_>>();
        let long = record::parse_log_records(
            "→ Bash  a very very very long command argument that keeps going and going and going\n",
            0,
            &[],
            &record::PathShortener::default(),
            "r",
            "s",
        )
        .iter()
        .map(|lr| lr.to_record())
        .collect::<Vec<_>>();
        for width in [79u16, 80, 99, 100, 119, 120] {
            let cols = record::Columns::for_width(width.saturating_sub(1));
            for records in [&short, &long] {
                let buf = draw_record_view(records, width, 4, None, false);
                assert_eq!(
                    buf[(cols.glyph, 0)].symbol(),
                    records[0].glyph,
                    "glyph must land at the reserved column at width {width}"
                );
            }
        }
    }

    /// Round-9 finding 2: a modified file's head must show its real `A`/`D`/`R`/`M`
    /// status, not the path a second time — `build_head_row` used to override
    /// `FileDiff`'s `verb` with `head` (the path), which `parse_diff` already put
    /// in `summary` too.
    #[test]
    fn file_diff_head_shows_status_not_a_repeated_path() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n@@ -1,2 +1,3 @@\n+new line\n-old line\n";
        let records = record::parse_diff(diff, "wt");
        let file = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::FileDiff))
            .expect("file diff record");
        assert_eq!(file.verb, "M");
        let buf = draw_record_view(&records, 90, 4, None, false);
        let cols = record::Columns::for_width(89);
        assert_eq!(
            buf[(cols.verb, 0)].symbol(),
            "M",
            "the head must paint the status letter, not the path"
        );
    }

    /// Round-9 finding 5: `draw_log_body` falls back to a CPU-only parse of
    /// `stream_text` when `log_records` is empty, but navigation used to read
    /// `snap.log_records` directly and never saw that fallback — `J`/`t`/`e`
    /// flashed "no match" over records visibly on screen. `effective_log_records`
    /// is the one function both paths must now call.
    #[test]
    fn effective_log_records_falls_back_to_stream_text_when_unparsed() {
        assert!(effective_log_records(&[], "").is_empty());
        let fallback = effective_log_records(&[], "→ Bash  ls -la\n");
        assert!(
            fallback
                .iter()
                .any(|r| matches!(r.kind, RecordKind::Tool(_))),
            "a nonempty stream_text with an empty log_records must still parse: {fallback:#?}"
        );
        // A nonempty `log_records` always wins — it is the pre-parsed, time-stamped
        // list; the fallback exists only for when that one is empty.
        let pre_parsed = vec![Record {
            kind: RecordKind::Note,
            glyph: "·",
            verb: "Note".to_string(),
            head: "h".to_string(),
            summary: "s".to_string(),
            body: Vec::new(),
            time: None,
            elapsed: None,
            actor: None,
            ok: None,
            source: SourceId::Activity {
                at_millis: 0,
                sequence: 0,
            },
            folded_by_default: false,
            has_command_row: false,
        }];
        let kept = effective_log_records(&pre_parsed, "→ Bash  ls -la\n");
        assert_eq!(kept.len(), 1);
        assert!(matches!(kept[0].kind, RecordKind::Note));
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
                "",
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                &HomeData::default(),
                None,
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
                unit_id: None,
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
                    "",
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &HomeData::default(),
                    None,
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

    #[test]
    fn structured_views_have_six_stable_tabs_with_narrow_labels() {
        let labels: Vec<_> = MAIN_TABS.iter().map(|tab| tab.label()).collect();
        assert_eq!(
            labels,
            ["Log", "Activity", "Diff", "Plan", "Review", "Chat", "Shell"]
        );

        for width in 20..80 {
            let st = run_with(Phase::Review, 2);
            let mut term = Terminal::new(TestBackend::new(width, 24)).unwrap();
            let swarm = SparPaths::new("/x");
            let mut app = test_app();
            app.human_alerts_n = 99;
            let mut rail = ListState::default();
            term.draw(|f| {
                draw(
                    f,
                    &swarm,
                    &[],
                    &[],
                    Some(&st),
                    "",
                    "",
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &HomeData::default(),
                    None,
                    &mut app,
                    &mut rail,
                )
            })
            .unwrap();
            assert_eq!(app.main_tabs.len(), 7, "width {width} dropped a tab");
        }
    }

    #[test]
    fn log_first_paint_folds_results_and_uses_distinct_tool_glyphs() {
        let st = run_with(Phase::Review, 1);
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        app.open_main(MainTab::Log);
        let mut rail = ListState::default();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                &[],
                &[],
                Some(&st),
                "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\ndrwxr-xr-x 139 root root 12288 Sep 7 10:19 .\n→ Read  /etc/hostname\n",
                "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\ndrwxr-xr-x 139 root root 12288 Sep 7 10:19 .\n→ Read  /etc/hostname\n",
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                &HomeData::default(),
                None,
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
        let painted: String = (0..30)
            .map(|y| row(&term, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(painted.contains("◆ Run"), "tool head: {painted:?}");
        assert!(painted.contains("◈ Read"), "read head: {painted:?}");
        // No `.idx` sidecar here, so no record has a time or an elapsed: the record
        // rows' own meta column must show the no-data sentinel, never a fabricated
        // "0s" (AC-8) — checked per-row, since the header/stepper band legitimately
        // shows an unrelated real "0s" run-duration elsewhere on the same screen.
        for line in painted.lines() {
            if line.contains("◆ Run") || line.contains("◈ Read") {
                assert!(
                    !line.contains("0s"),
                    "fabricated elapsed on record row: {line:?}"
                );
            }
        }
        assert!(
            !painted.contains("drwxr-xr-x 139"),
            "tool output must be folded on first paint: {painted:?}"
        );
    }

    /// AC-3: the meta column's text must end exactly at the row's right edge — not
    /// past it, which is what happened when a narrow-width time format (8 chars)
    /// exceeded `meta_width` (6): `build_head_row` clamped the *alignment* math but
    /// still pushed the untruncated text, so it overran into the row's own edge.
    #[test]
    fn meta_column_is_right_aligned_and_never_overflows_at_narrow_width() {
        for width in [79u16, 87, 99] {
            let cols = record::Columns::for_width(width);
            let r = Record {
                kind: RecordKind::Note,
                glyph: "·",
                verb: "Note".to_string(),
                head: "note".to_string(),
                summary: "a summary long enough to reach the meta column".to_string(),
                body: Vec::new(),
                time: Some(Utc::now()),
                elapsed: None,
                actor: None,
                ok: None,
                source: SourceId::Activity {
                    at_millis: 0,
                    sequence: 0,
                },
                folded_by_default: false,
                has_command_row: false,
            };
            let row = build_head_row(&r, cols, false, true, None);
            let (meta_x, meta_text, _) = row.spans.last().unwrap();
            let meta_len = meta_text.chars().count() as u16;
            assert_eq!(
                *meta_x + meta_len,
                cols.meta + cols.meta_width,
                "width {width}: meta must end exactly at the reserved column's right edge"
            );
            assert!(
                meta_len <= cols.meta_width,
                "width {width}: meta text {meta_text:?} overflows its {}-wide column",
                cols.meta_width
            );
        }
    }

    /// AC-3/AC-4: U32 promises the meta column carries elapsed *and* an absolute
    /// time once the view is wide enough to reserve room for both (`>=100`) — a
    /// timed tool call must not have its timestamp displaced by its elapsed, and
    /// the combined text still respects the reserved width.
    #[test]
    fn wide_meta_column_carries_elapsed_and_absolute_time_together() {
        use chrono::TimeZone;
        let cols = record::Columns::for_width(120);
        assert!(cols.meta_width >= 15, "{cols:?}");
        let r = Record {
            kind: RecordKind::Tool(record::ToolKind::Run),
            glyph: "◆",
            verb: "Run".to_string(),
            head: "ls".to_string(),
            summary: "ls -la".to_string(),
            body: Vec::new(),
            time: Some(Utc.with_ymd_and_hms(2026, 1, 1, 11, 4, 0).unwrap()),
            elapsed: Some(std::time::Duration::from_millis(800)),
            actor: None,
            ok: Some(true),
            source: SourceId::Activity {
                at_millis: 0,
                sequence: 0,
            },
            folded_by_default: false,
            has_command_row: false,
        };
        let text = record_meta_text(&r, cols.meta_width);
        assert!(text.contains("0.8s"), "{text:?}");
        assert!(text.contains("11:04"), "{text:?}");
        assert!(
            text.chars().count() as u16 <= cols.meta_width,
            "meta text {text:?} overflows the {}-wide column",
            cols.meta_width
        );
    }

    /// AC-2: Activity carries phase boundaries and alerts as typed records — never
    /// a joined string — sourced through the same `activity_feed` the tab paints.
    #[test]
    fn activity_feed_includes_alert_and_phase_boundary_records() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let mut st = run_with(Phase::Review, 2);
        events::append(
            &swarm,
            &st.id,
            &events::Event::phase(Phase::Review, Some(Phase::Dispatch)),
        )
        .unwrap();
        st.phase = Phase::Review;
        let alert = crate::bus::BusMessage {
            id: "m1".to_string(),
            ts: Utc::now(),
            from: "reviewer-0".to_string(),
            to: "operator".to_string(),
            kind: crate::bus::MsgKind::Blocked,
            body: "needs a decision".to_string(),
            run: Some(st.id.clone()),
            subject: None,
            refs: crate::bus::MsgRefs::default(),
            requires_ack: true,
            meta: std::collections::HashMap::new(),
        };
        let records = activity_feed(
            &swarm,
            Some(&st),
            &QuotaStore::default(),
            &[alert],
            &std::collections::HashMap::new(),
            &Config::default(),
        );
        assert!(
            records.iter().any(|r| r.kind == RecordKind::Alert),
            "no alert record: {records:#?}"
        );
        assert!(
            records
                .iter()
                .any(|r| r.kind == RecordKind::Section && r.verb == "phase"),
            "no phase boundary record: {records:#?}"
        );
    }

    /// AC-7: two slots sharing a role — here, two `Reviewer`s both `Done` with nothing
    /// to say (`quiet` empty) — render identical role/status/quiet text. Keying identity
    /// off `role_label` alone collided them onto one `SourceId::Activity`, so folding or
    /// selecting one silently acted on both (round-review finding, reproduced live: a
    /// 7-slot run with both `Reviewer` slots `Done`). The slot id is what tells them
    /// apart, so it must be the actor, not the shared role name.
    #[test]
    fn activity_agents_band_keys_identity_on_slot_id_not_shared_role_label() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let mut st = run_with(Phase::Review, 7);
        // Both reviewer slots (indices 5 and 6) `Done`, matching the reproduced case
        // exactly: `run_with` otherwise leaves the last slot `Running`.
        st.slots[5].status = SlotStatus::Done;
        st.slots[6].status = SlotStatus::Done;
        let slot5_id = st.slots[5].id.clone();
        let slot6_id = st.slots[6].id.clone();
        assert_eq!(
            role_label(st.slots[5].role),
            role_label(st.slots[6].role),
            "the two slots must share a role for this regression to be meaningful"
        );

        let records = activity_feed(
            &swarm,
            Some(&st),
            &QuotaStore::default(),
            &[],
            &std::collections::HashMap::new(),
            &Config::default(),
        );
        let agent_rows: Vec<&Record> = records
            .iter()
            .filter(|r| {
                r.actor.as_deref() == Some(slot5_id.as_str())
                    || r.actor.as_deref() == Some(slot6_id.as_str())
            })
            .collect();
        assert_eq!(
            agent_rows.len(),
            2,
            "both reviewer slots must render their own row, {records:#?}"
        );
        assert_ne!(
            agent_rows[0].source, agent_rows[1].source,
            "identical role/status text must not collide onto one source identity"
        );
    }

    /// `f`'s selected-slot filter must apply identically to what `J/K/t/e/}`
    /// navigate over and to what `draw_activity_body` paints — a separate filtered
    /// copy in each place let the cursor move to a record the paint side had
    /// already dropped, landing it off-screen (round-review finding 3).
    #[test]
    fn activity_slot_filter_matches_between_navigation_and_painting() {
        let st = run_with(Phase::Dispatch, 2);
        let slot0 = st.slots[0].id.clone();
        let slot1 = st.slots[1].id.clone();
        let activity = vec![
            activity_record(None, slot0.clone(), "started", "", RecordKind::Note).to_record(),
            activity_record(None, slot1.clone(), "started", "", RecordKind::Note).to_record(),
        ];
        let mut snap = Snapshot::loading(Path::new("/x"));
        snap.activity = activity.clone();
        snap.full = Some(st);

        let mut app = test_app();
        app.main_tab = MainTab::Activity;
        app.activity_slot_filter = Some(0);

        let nav_records = active_records_for(&app, &snap);
        let paint_records =
            filter_activity_records(&snap.activity, app.activity_slot_filter, snap.full.as_ref());
        assert_eq!(
            nav_records.len(),
            paint_records.len(),
            "cursor navigation must see exactly what is painted"
        );
        assert!(
            nav_records
                .iter()
                .all(|r| r.actor.as_deref() == Some(slot0.as_str())),
            "the filtered set must exclude the other slot's records: {nav_records:#?}"
        );
    }

    /// AC-7/AC-13: `J`/`K` must be able to move between a document's own sections.
    /// This was impossible before the per-section `SourceId` fix — every section
    /// shared one identity, so the cursor always re-resolved to the first one.
    #[test]
    fn record_cursor_moves_between_document_sections() {
        let records = record::parse_document(
            "plan.md",
            "# Plan\nfirst\n\n## Risks\nsecond\n\n## Rollout\nthird\n",
            "plan.md",
        );
        assert_eq!(records.len(), 3);
        let first = move_cursor(&records, None, 1, |_| true).expect("first section");
        assert_eq!(first, records[0].source);
        let second = move_cursor(&records, Some(&first), 1, |_| true).expect("second section");
        assert_eq!(second, records[1].source);
        assert_ne!(second, first, "each section must be its own cursor stop");
        let third = move_cursor(&records, Some(&second), 1, |_| true).expect("third section");
        assert_eq!(third, records[2].source);
        // And back.
        let back = move_cursor(&records, Some(&third), -1, |_| true).expect("back to second");
        assert_eq!(back, second);
    }

    /// AC-12: `path_shortener_for` is built fresh per call from the *browsed*
    /// project's own root plus its own run's worktrees — the regression this
    /// guards is the old `PROJECT_PREFIX` `OnceLock`, set once per process from
    /// whichever project was first browsed, which stayed wrong for every other
    /// project for the rest of the session. Two projects in one session must each
    /// shorten against their own root, never the other's.
    #[test]
    fn path_shortening_is_run_local_across_two_projects_in_one_session() {
        let swarm_a = SparPaths::new(std::path::Path::new("/projects/alpha"));
        let mut st_a = run_with(Phase::Dispatch, 1);
        st_a.worktrees.push(state::WorktreeRecord {
            slot_id: st_a.slots[0].id.clone(),
            path: PathBuf::from("/projects/alpha-worktree"),
            branch: "spar/a".into(),
        });
        let shortener_a = path_shortener_for(&swarm_a, Some(&st_a));
        assert_eq!(
            shortener_a.shorten("/projects/alpha-worktree/src/lib.rs"),
            "src/lib.rs"
        );

        let swarm_b = SparPaths::new(std::path::Path::new("/projects/bravo"));
        let mut st_b = run_with(Phase::Dispatch, 1);
        st_b.worktrees.push(state::WorktreeRecord {
            slot_id: st_b.slots[0].id.clone(),
            path: PathBuf::from("/projects/bravo-worktree"),
            branch: "spar/b".into(),
        });
        let shortener_b = path_shortener_for(&swarm_b, Some(&st_b));
        assert_eq!(
            shortener_b.shorten("/projects/bravo-worktree/src/lib.rs"),
            "src/lib.rs"
        );

        // Neither shortener knows about the other project's worktree root at all —
        // browsing bravo second must not leave alpha's root reachable, or vice versa.
        assert!(shortener_a
            .shorten("/projects/bravo-worktree/src/lib.rs")
            .starts_with('/'));
        assert!(shortener_b
            .shorten("/projects/alpha-worktree/src/lib.rs")
            .starts_with('/'));

        // And the first shortener built is unaffected by the second one existing:
        // rebuilding it from the same inputs still shortens against alpha, not bravo.
        let shortener_a_again = path_shortener_for(&swarm_a, Some(&st_a));
        assert_eq!(
            shortener_a_again.shorten("/projects/alpha-worktree/src/lib.rs"),
            "src/lib.rs"
        );
    }

    /// AC-7: an activity record's identity comes from its own content, not from
    /// where it lands in the feed. Two builds of the same event content, at
    /// different positions, must resolve to the same `SourceId` so fold state and
    /// the cursor survive a rebuild that inserts something ahead of them.
    #[test]
    fn activity_record_identity_is_content_derived_not_positional() {
        let a = activity_record(None, "impl", "phase", "review", RecordKind::Note).to_record();
        let b = activity_record(None, "impl", "phase", "review", RecordKind::Note).to_record();
        assert_eq!(
            a.source, b.source,
            "identical activity content must produce the same identity regardless of build order"
        );
        let c = activity_record(None, "impl", "phase", "ship", RecordKind::Note).to_record();
        assert_ne!(
            a.source, c.source,
            "different content must not collide onto the same identity"
        );
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
            cost_usd: None,
            subagent_stats: None,
            model_usage: Default::default(),
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
            cost_usd: None,
            subagent_stats: None,
            model_usage: Default::default(),
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
                "",
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                &HomeData::default(),
                None,
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
                    "",
                    &[],
                    &[],
                    text,
                    &[],
                    &[],
                    &[],
                    &[],
                    &HomeData::default(),
                    None,
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
        app.scroll_main_by(10, false, &[]);
        assert_eq!(
            app.stream_scroll, 10,
            "Activity's overview body must scroll stream_scroll"
        );
        assert_eq!(
            app.bus_scroll, 0,
            "Activity's overview body must not touch bus_scroll"
        );

        app.main_tab = MainTab::Diff;
        app.scroll_main_by(10, false, &[]);
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
        app.scroll_main_by(10, true, &[]);
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
        let wide_labels = ["Log", "Act", "Diff", "Plan", "Rev", "C", "Sh"];
        let narrow_labels = ["Log", "Act", "Diff", "Plan", "Rev", "C", "Sh"];
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
        let probe = |width: u16, human_alerts_n: usize, labels: &[&str]| -> Probe {
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
            term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
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
        let assert_rects_cover_labels = |band: &str,
                                         starts: &[usize],
                                         rects: &[Rect],
                                         n: usize,
                                         labels: &[&str]| {
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
            let wide = probe(120, n, &wide_labels);
            assert!(
                wide.glyph_gaps.windows(2).all(|w| w[0] == w[1]) && wide.glyph_gaps[0] > 0,
                "wide tab gaps not uniform (human_alerts_n={n}): {:?}",
                wide.glyph_gaps
            );
            assert_rects_cover_labels("wide", &wide.starts, &wide.rects, n, &wide_labels);

            let narrow = probe(79, n, &narrow_labels);
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
            assert_rects_cover_labels("narrow", &narrow.starts, &narrow.rects, n, &narrow_labels);
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
                    draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app);
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
    /// six (U9/U35 grew the strip from four; narrow uses the uniform short labels
    /// — U35) — with an alert badge in play too: below ~36 columns the
    /// badge-reservation fallback glues the badge onto Activity alone (trading gap
    /// uniformity, covered by `tab_strip_gaps_are_uniform`'s 79-column probe, for
    /// keeping every tab on screen), and that fallback path was only ever swept
    /// with zero alerts.
    #[test]
    fn narrow_tab_strip_never_drops_a_tab() {
        let st = run_with(Phase::Review, 3);
        let labels = ["Log", "Act", "Diff", "Plan", "Rev", "C", "Sh"];
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
                term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
                    .unwrap();
                assert_eq!(
                    app.main_tabs.len(),
                    7,
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
    /// wide band ever renders at (`NARROW_WIDTH`). Six tabs (U9/U35) do not all fit
    /// at their full padded width there, so the strip falls back to short labels
    /// (same as the narrow strip) rather than silently dropping one (U11).
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
                term.draw(|f| draw_labels(f, &lay, &swarm, &[], &[], Some(&st), &mut app))
                    .unwrap();
                assert_eq!(
                    app.main_tabs.len(),
                    7,
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
                    &text,
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &HomeData::default(),
                    None,
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
                    &text,
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &HomeData::default(),
                    None,
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
                &text,
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                &HomeData::default(),
                None,
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
                    &text,
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &HomeData::default(),
                    None,
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
            unit_id: None,
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
            loading: false,
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
                "",
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                home,
                None,
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

    /// R7 review finding: the doctor pointer must fire on the realistic shape a
    /// default install produces — every entry in `providers.order` configured but
    /// unavailable (nothing on PATH), which is a non-empty roster, not the empty
    /// one an explicit `order = []` produces.
    #[test]
    fn new_run_shows_the_doctor_pointer_when_nothing_is_usable() {
        let projects = [home_project("spar")];
        let home = home_data(&projects);
        let term = paint_home(120, 30, &projects, &home, |a| {
            let mut nr = new_run_fixture();
            nr.roster = vec![
                RosterEntry {
                    choice: RosterChoice::Provider("cli:claude".into()),
                    label: "cli:claude".into(),
                    available: false,
                    reason: Some("not on PATH".into()),
                    source: RosterSource::Configured,
                },
                RosterEntry {
                    choice: RosterChoice::Provider("cli:grok".into()),
                    label: "cli:grok".into(),
                    available: false,
                    reason: Some("not on PATH".into()),
                    source: RosterSource::Configured,
                },
            ];
            nr.picked.clear();
            a.new_run = Some(nr);
        });
        assert!(
            whole(&term).contains("spar doctor"),
            "a non-empty but all-unavailable roster must still point at spar doctor"
        );
    }

    /// Review finding: the wait column must read from the run's own `updated_at`
    /// at paint time, not from `HomeRow::Run.waited`, which is only as fresh as
    /// the last snapshot rebuild and can go stale on an idle fleet. A row whose
    /// `updated_at` is fresh but whose stored `waited` is hours old must still
    /// paint a fresh wait.
    #[test]
    fn home_wait_column_reads_live_not_the_stale_stored_value() {
        let projects = [home_project("spar")];
        let run = home_run("gate0001", Phase::AwaitingPlanApproval, 0, "spar");
        let home = HomeData {
            rows: vec![
                HomeRow::Header(HomeBand::NeedsMe),
                HomeRow::Run {
                    band: HomeBand::NeedsMe,
                    run,
                    waited: Duration::from_secs(6 * 3600),
                },
                HomeRow::Header(HomeBand::Running),
                HomeRow::Header(HomeBand::Finished),
                HomeRow::Header(HomeBand::StartNew),
                HomeRow::NewRun,
            ],
            project_stats: Vec::new(),
            loading: false,
        };
        let term = paint_home(120, 30, &projects, &home, |_| {});
        let screen = whole(&term);
        assert!(
            !screen.contains("6h"),
            "the rail must not paint the stale stored wait: {screen:?}"
        );
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
            loading: false,
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
            loading: false,
        };
        // The rail carries the `… N more` row; Main's detail for a band header
        // states the band's true total, which is the stronger claim and the one
        // that does not depend on where the rail's viewport happens to end.
        let term = paint_home(120, 40, &projects, &home, |_| {});
        let text = whole(&term);
        assert!(
            text.contains(&format!("{needs_me} row(s) in this band")),
            "a capped band must state its true size: {text:?}"
        );
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
            loading: false,
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
            loading: false,
        };
        // Assert on the row list itself: it is the source both the rail and Main
        // render from, so the ordering claim depends on no viewport at all.
        for home in [&full, &drained] {
            let bands: Vec<HomeBand> = home
                .rows
                .iter()
                .filter_map(|r| match r {
                    HomeRow::Header(b) => Some(*b),
                    _ => None,
                })
                .collect();
            assert_eq!(
                bands,
                vec![
                    HomeBand::NeedsMe,
                    HomeBand::Running,
                    HomeBand::Finished,
                    HomeBand::StartNew
                ],
                "bands out of order"
            );
        }
        // Each empty band still says what is empty, on its own row in the rail —
        // a header with a gap under it does not distinguish empty from loading.
        // Built rather than hand-written: the claim is that the *builder* emits
        // these for a drained workspace, which a literal fixture cannot show.
        let built = build_home_rows(&[], &[], &HomeScope::All, Utc::now(), Utc::now(), false);
        let empties: Vec<&str> = built
            .iter()
            .filter_map(|r| match r {
                HomeRow::Empty(b) => Some(home_band_empty_text(*b)),
                _ => None,
            })
            .collect();
        for phrase in [
            "nothing needs you",
            "nothing running",
            "nothing finished since your last look",
        ] {
            assert!(
                empties.contains(&phrase),
                "missing {phrase:?} in: {empties:?}"
            );
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
            loading: false,
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
    /// the other three bands hold. Round-7 review finding: this used to assert
    /// over hand-built `HomeRow` fixtures rather than the real builder, so a
    /// regression in `build_home_rows` itself could not fail it — now it calls
    /// `build_home_rows` directly, once over an empty registry and once over a
    /// populated one covering all three other bands.
    #[test]
    fn home_start_something_new_is_always_present() {
        let projects = [home_project("spar")];
        let empty: Vec<Vec<state::RunSummary>> = vec![Vec::new()];
        let populated = vec![vec![
            home_run("gate0001", Phase::AwaitingShipConfirm, 90, "spar"),
            home_run("work0001", Phase::Review, 4, "spar"),
            home_run("done0001", Phase::Done, 20, "spar"),
        ]];
        let watermark = Utc::now() - chrono::Duration::hours(1);
        for folded in [empty, populated] {
            let rows = build_home_rows(
                &projects,
                &folded,
                &HomeScope::All,
                watermark,
                Utc::now(),
                false,
            );
            let i = rows
                .iter()
                .position(|r| matches!(r, HomeRow::Header(HomeBand::StartNew)))
                .expect("band 1 header");
            assert_eq!(i, 0, "StartNew must be the first header");
            assert!(
                matches!(rows.get(i + 1), Some(HomeRow::NewRun)),
                "the new-run action row must follow band 1's header: {:?}",
                &rows[i..]
            );
            let next_header = rows[i + 2..]
                .iter()
                .position(|r| matches!(r, HomeRow::Header(_)));
            let band_slice = if let Some(pos) = next_header {
                &rows[i + 1..i + 2 + pos]
            } else {
                &rows[i + 1..]
            };
            assert!(
                band_slice
                    .iter()
                    .all(|r| matches!(r, HomeRow::NewRun | HomeRow::Project(..))),
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
                    "",
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &home,
                    None,
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
        // The gate zone is reserved identically at every level: it must not move
        // when the rail is browsed, which is the U11 claim.
        assert_eq!(home_gates, runs_gates, "the gate zone moved at Home");
        // The strip itself carries a different tab set at Home (U31: Log/Activity/
        // Diff are all f(a selected run) and Home has none), so it is not the same
        // rects. What must hold is that both strips start at Main's own column —
        // a strip that started somewhere else would read as a different pane.
        assert_eq!(
            home_tabs.first().map(|r: &Rect| r.x),
            runs_tabs.first().map(|r: &Rect| r.x),
            "the tab strip changed origin at Home"
        );
        assert_eq!(home_tabs.len(), 3, "Home carries Home + Chat + Shell");
        assert_eq!(runs_tabs.len(), MAIN_TABS.len(), "a run carries all seven");
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
            loading: false,
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
                    "",
                    &[],
                    &[],
                    "",
                    &[],
                    &[],
                    &[],
                    &[],
                    &empty,
                    None,
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
            loading: false,
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
            loading: false,
        };
        paint_home(120, 30, &projects, &short, |a| a.open_projects_view());
        paint_home(120, 30, &projects, &HomeData::default(), |a| {
            a.open_projects_view()
        });
    }

    /// U13, startup half — round-11 review finding. Landing on Home makes the very
    /// first snapshot cross-project; building it synchronously before the first
    /// paint would block startup on the same scan U13 already keeps out of `draw`.
    /// `Snapshot::loading` is what the first frame paints instead: it must come back
    /// instantly and must not need its root to exist, proving it does no disk I/O to
    /// construct. `home.rows` is *not* empty — it is seeded with `build_home_rows`
    /// over no projects/runs, a pure function, so the four band headers and the `n`
    /// CTA are present on frame one instead of Home looking blank for a `REFRESH`
    /// tick (round-11 review, minor).
    #[test]
    fn snapshot_loading_needs_no_disk_and_reserves_skeleton_rows() {
        let root = PathBuf::from("/nonexistent/definitely-not-here");
        let snap = Snapshot::loading(&root);
        assert!(snap.projects.is_empty());
        assert!(snap.runs.is_empty());
        assert!(
            snap.home.loading,
            "the first cross-project scan is still in flight"
        );
        let skeletons: Vec<&HomeRow> = snap
            .home
            .rows
            .iter()
            .filter(|r| matches!(r, HomeRow::Skeleton { .. }))
            .collect();
        assert_eq!(
            skeletons.len(),
            3 * HOME_SKELETON_ROWS,
            "the three scan-backed bands reserve a fixed number of rows"
        );
        let keys: std::collections::HashSet<String> =
            skeletons.iter().map(|row| home_row_key(row)).collect();
        assert_eq!(
            keys.len(),
            skeletons.len(),
            "every skeleton needs its own identity"
        );
        assert!(
            snap.home
                .rows
                .iter()
                .all(|r| !matches!(r, HomeRow::Empty(_))),
            "loading must not claim a scan-backed band is empty: {:?}",
            snap.home.rows
        );
        assert!(snap.home.project_stats.is_empty());
        assert!(snap.full.is_none());
    }

    #[test]
    fn skeleton_rows_are_unselectable_and_non_loading_home_stays_non_loading() {
        let now = Utc::now();
        let folded: Vec<Vec<state::RunSummary>> = Vec::new();
        let rows = build_home_rows(&[], &folded, &HomeScope::All, now, now, true);
        assert!(rows.iter().any(|r| matches!(r, HomeRow::Skeleton { .. })));
        assert!(rows.iter().all(|r| !matches!(r, HomeRow::Empty(_))));

        let mut app = App::new(None, Config::default(), None);
        app.selected_home = 0;
        resync_home_selection(&mut app, &rows);
        assert!(
            !matches!(rows.get(app.selected_home), Some(HomeRow::Skeleton { .. })),
            "selection landed on an inert loading placeholder"
        );
        let next = step_home(&rows, app.selected_home, 1);
        assert!(
            !matches!(rows.get(next), Some(HomeRow::Skeleton { .. })),
            "keyboard navigation must skip a loading placeholder"
        );

        let ready = build_home_rows(&[], &folded, &HomeScope::All, now, now, false);
        assert!(ready.iter().all(|r| !matches!(r, HomeRow::Skeleton { .. })));
        assert!(
            !HomeData::default().loading,
            "ordinary HomeData fixtures are never implicitly loading"
        );
    }

    #[test]
    fn rail_reversal_travels_through_distinct_permutations_and_settles_on_focus_loss() {
        let now = Instant::now();
        let mut app = App::new(None, Config::default(), None);
        app.browse = BrowseLevel::Runs;
        let mut first_leg = home_run("leg-a", Phase::Review, 1, "spar");
        let mut replacement_leg = home_run("leg-b", Phase::AwaitingPlanApproval, 1, "spar");
        first_leg.unit_id = Some("unit-1".into());
        replacement_leg.unit_id = Some("unit-1".into());
        assert_eq!(
            run_row_key(&first_leg),
            run_row_key(&replacement_leg),
            "a folded unit's cursor key changed with its representative leg"
        );
        let original = ["a", "b", "c", "d"];
        let target = ["d", "c", "b", "a"];
        let original_keys = original.iter().map(|key| (*key).to_string()).collect();
        let target_keys: Vec<String> = target.iter().map(|key| (*key).to_string()).collect();

        app.rail_motion
            .observe(BrowseLevel::Runs, original_keys, now);
        app.rail_motion
            .observe(BrowseLevel::Runs, target_keys.clone(), now);

        let mut intermediate = Vec::new();
        for elapsed in [55, 110, 145] {
            let permutation = app
                .rail_motion
                .observe(
                    BrowseLevel::Runs,
                    target_keys.clone(),
                    now + Duration::from_millis(elapsed),
                )
                .expect("an in-flight reorder needs a displayed permutation");
            let displayed: Vec<&str> = permutation.iter().map(|&index| target[index]).collect();
            let mut sorted = displayed.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, original, "reorder dropped or duplicated a row");
            assert_ne!(displayed, original, "reversal jumped back to its source");
            assert_ne!(displayed, target, "reversal teleported to its target");
            intermediate.push(displayed);
        }
        intermediate.dedup();
        assert!(
            intermediate.len() >= 2,
            "a reversal needs more than one intermediate displayed order: {intermediate:?}"
        );

        let final_order = app
            .rail_motion
            .observe(
                BrowseLevel::Runs,
                target_keys,
                now + crate::motion::REORDER_PERIOD,
            )
            .unwrap_or_else(|| (0..target.len()).collect());
        assert_eq!(
            final_order,
            vec![0, 1, 2, 3],
            "the settled permutation must be the snapshot's data order"
        );

        let snap = Snapshot::loading(Path::new("/nonexistent/definitely-not-here"));
        assert!(
            !app.motion_in_flight(),
            "the completed tween still schedules frames"
        );
        app.rail_motion.observe(
            BrowseLevel::Runs,
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            now + crate::motion::REORDER_PERIOD,
        );
        assert!(app.motion_in_flight());
        assert!(animating(&app, &snap));
        app.focused = false;
        assert!(
            !animating(&app, &snap),
            "an unfocused window must not animate"
        );
        app.settle_motion();
        assert!(
            !app.motion_in_flight(),
            "focus loss must settle, not freeze motion"
        );
    }

    #[test]
    fn meter_zone_is_a_fixed_slot_even_for_the_largest_token_ledger() {
        let pad = Rect {
            x: 7,
            y: 2,
            width: METER_ZONE_W + STEPPER_MIN_W,
            height: 1,
        };
        let zone = meter_zone(pad).expect("the exact affordable width has a meter slot");
        assert_eq!(zone.width, METER_ZONE_W);
        assert_eq!(zone.right(), pad.right());
        assert!(meter_zone(Rect {
            width: pad.width - 1,
            ..pad
        })
        .is_none());
        assert!(
            compact_u64(u64::MAX).chars().count() as u16 <= METER_ZONE_W,
            "the fixed slot cannot fit the formatter's largest token value"
        );
        // Worst-case assembled meter line: every optional term enabled and billed MAX.
        // The zone must either fit it or truncate with a visible marker while keeping
        // the stepper width fixed (pure function of pad width).
        let worst_meters = vec![
            Span::styled("9999d", dim()),
            Span::styled(" · ", muted()),
            Span::styled("99/99 agents", dim()),
            Span::styled(" · ", muted()),
            Span::styled(format!("billed {}", compact_u64(u64::MAX)), dim()),
            Span::styled(" · ", muted()),
            Span::styled("round 99", dim()),
            Span::styled(" · ", muted()),
            Span::styled("99 legs", dim()),
        ];
        let fitted = fit_spans(worst_meters.clone(), METER_ZONE_W);
        let total: usize = fitted.iter().map(|s| s.content.chars().count()).sum();
        assert!(total <= METER_ZONE_W as usize);
        if worst_meters
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            > METER_ZONE_W as usize
        {
            assert!(
                fitted.iter().any(|s| s.content.contains('…')),
                "worst-case truncated meter must show ellipsis, got {:?}",
                fitted
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<Vec<_>>()
            );
            assert!(
                fitted.iter().any(|s| s.content.contains("billed")),
                "worst-case truncation must preserve billed token count, got {:?}",
                fitted
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<Vec<_>>()
            );
        }
        // Stepper width is fixed when zone affordable, regardless of billed size.
        let pad_wide = Rect {
            x: 0,
            y: 0,
            width: METER_ZONE_W + STEPPER_MIN_W + 1 + 20,
            height: 1,
        };
        let zone_wide = meter_zone(pad_wide).expect("wide pad must have zone");
        assert_eq!(zone_wide.width, METER_ZONE_W);
        assert_eq!(pad_wide.width - METER_ZONE_W - 1, 20 + STEPPER_MIN_W);
    }

    #[test]
    fn home_reorder_travels_through_adjacent_rows() {
        let now = Instant::now();
        let mut app = App::new(None, Config::default(), None);
        app.browse = BrowseLevel::Home;
        let r1 = home_run("r1", Phase::Review, 1, "spar");
        let r2 = home_run("r2", Phase::Review, 2, "spar");
        let r3 = home_run("r3", Phase::Review, 3, "spar");
        let x = home_run("runX", Phase::Done, 10, "spar");
        let build = |order: &[&str]| -> Vec<String> {
            let mut rows: Vec<HomeRow> = Vec::new();
            rows.push(HomeRow::Header(HomeBand::NeedsMe));
            for id in order.iter().filter(|id| **id == "runX") {
                if *id == "runX" && order[0] == "runX" {
                    rows.push(HomeRow::Run {
                        band: HomeBand::NeedsMe,
                        run: x.clone(),
                        waited: Duration::from_secs(0),
                    });
                }
            }
            if !order.contains(&"runX") || order[0] != "runX" {
                rows.push(HomeRow::Empty(HomeBand::NeedsMe));
            }
            rows.push(HomeRow::Header(HomeBand::Running));
            for id in &["r1", "r2", "r3"] {
                let r = match *id {
                    "r1" => r1.clone(),
                    "r2" => r2.clone(),
                    _ => r3.clone(),
                };
                rows.push(HomeRow::Run {
                    band: HomeBand::Running,
                    run: r,
                    waited: Duration::from_secs(0),
                });
            }
            rows.push(HomeRow::Header(HomeBand::Finished));
            if order.contains(&"runX") && order[0] != "runX" {
                rows.push(HomeRow::Run {
                    band: HomeBand::Finished,
                    run: x.clone(),
                    waited: Duration::from_secs(0),
                });
            } else {
                rows.push(HomeRow::Empty(HomeBand::Finished));
            }
            rows.push(HomeRow::Header(HomeBand::StartNew));
            rows.push(HomeRow::NewRun);
            rows.iter().map(home_row_key).collect()
        };
        let source_keys = build(&["r1", "r2", "r3", "runX"]);
        let target_keys = build(&["runX", "r1", "r2", "r3"]);
        app.rail_motion
            .observe(BrowseLevel::Home, source_keys.clone(), now);
        app.rail_motion
            .observe(BrowseLevel::Home, target_keys.clone(), now);
        let swaps = app.rail_motion.swaps.clone();
        assert!(!swaps.is_empty(), "reorder must produce swaps");
        for &idx in &swaps {
            assert!(
                idx + 1 < target_keys.len(),
                "every swap must exchange an index with its right neighbour, got {idx} for len {}",
                target_keys.len()
            );
        }
        let mut seen_full: Vec<Vec<String>> = Vec::new();
        let mut seen_movable: Vec<Vec<String>> = Vec::new();
        let mut saw_header_displacement = false;
        for elapsed in [30, 80, 140, 180] {
            let perm = app
                .rail_motion
                .observe(
                    BrowseLevel::Home,
                    target_keys.clone(),
                    now + Duration::from_millis(elapsed),
                )
                .unwrap_or_else(|| (0..target_keys.len()).collect());
            let displayed: Vec<String> = perm.iter().map(|&i| target_keys[i].clone()).collect();
            let mut sorted_disp = displayed.clone();
            sorted_disp.sort();
            let mut sorted_target = target_keys.clone();
            sorted_target.sort();
            assert_eq!(
                sorted_disp, sorted_target,
                "reorder dropped or duplicated a row at t={elapsed}"
            );
            seen_full.push(displayed.clone());
            let movable: Vec<String> = displayed
                .iter()
                .filter(|k| k.starts_with("run:"))
                .cloned()
                .collect();
            seen_movable.push(movable);
            if displayed.iter().enumerate().any(|(idx, k)| {
                (k.starts_with("hdr:") || k.starts_with("empty:")) && k != &target_keys[idx]
            }) {
                saw_header_displacement = true;
            }
        }
        assert!(
            saw_header_displacement,
            "Home row must travel through intervening rows — header should be displaced mid-flight, got {seen_full:?}"
        );
        let source_movable: Vec<String> = source_keys
            .iter()
            .filter(|k| k.starts_with("run:"))
            .cloned()
            .collect();
        let target_movable: Vec<String> = target_keys
            .iter()
            .filter(|k| k.starts_with("run:"))
            .cloned()
            .collect();
        let mut distinct_full = seen_full.clone();
        distinct_full.sort();
        distinct_full.dedup();
        distinct_full.retain(|o| {
            let mov: Vec<String> = o
                .iter()
                .filter(|k| k.starts_with("run:"))
                .cloned()
                .collect();
            mov != source_movable && mov != target_movable
        });
        assert!(
            !distinct_full.is_empty(),
            "cross-band move must travel through intermediate full orders, got {seen_full:?}"
        );
        let mut distinct_movable = seen_movable.clone();
        distinct_movable.sort();
        distinct_movable.dedup();
        distinct_movable.retain(|o| o != &source_movable && o != &target_movable);
        assert!(
            !distinct_movable.is_empty(),
            "cross-band move must travel through intermediate movable orders, got {seen_movable:?}"
        );
    }

    #[test]
    fn fit_spans_shows_ellipsis_when_truncated_at_boundary() {
        let meters = vec![
            Span::styled("0s", dim()),
            Span::styled(" · ", muted()),
            Span::styled("0/1 agents", dim()),
            Span::styled(" · ", muted()),
            Span::styled("round 2", dim()),
            Span::styled(" · ", muted()),
            Span::styled("2 legs", dim()),
            Span::styled(" · ", muted()),
            Span::styled("billed 12.4k", dim()),
        ];
        let width = METER_ZONE_W;
        let fitted = fit_spans(meters.clone(), width);
        let total: usize = fitted.iter().map(|s| s.content.chars().count()).sum();
        assert!(total <= width as usize);
        assert!(
            fitted.iter().any(|s| s.content.contains('…')),
            "truncated meter line must show ellipsis, got {:?}",
            fitted
                .iter()
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
        );
        let exact = vec![
            Span::styled("0s", dim()),
            Span::styled(" · ", muted()),
            Span::styled("0/1 agents", dim()),
            Span::styled(" · ", muted()),
            Span::styled("round 2", dim()),
            Span::styled(" · ", muted()),
            Span::styled("2 legs", dim()),
        ];
        let exact_total: usize = exact.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(exact_total, 34);
        let mut with_extra = exact.clone();
        with_extra.push(Span::styled("X", dim()));
        let fitted2 = fit_spans(with_extra, width);
        assert!(
            fitted2.iter().any(|s| s.content.contains('…')),
            "exact-fill plus one must ellipsis, got {:?}",
            fitted2
                .iter()
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
        );
    }

    /// U13, round-7 review finding (review-0-cli-codex): `draw_log_body` used to call
    /// `process::StreamStats::load`, a synchronous file read, on every repaint of a
    /// selected run's Log tab. It now only reads the `stats` snapshot already computed
    /// off-thread in `build_snapshot`, so painting a run whose log file does not exist
    /// on disk must still succeed and the log-loading call must not survive inside
    /// `draw_log_body`'s own source.
    #[test]
    fn draw_log_body_does_not_read_the_log_file_itself() {
        let src = include_str!("tui.rs");
        let start = src
            .find("fn draw_log_body(")
            .expect("draw_log_body must exist");
        let body_start = src[start..].find('{').unwrap() + start;
        let mut depth = 0i32;
        let mut end = body_start;
        for (i, c) in src[body_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = body_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &src[body_start..=end];
        assert!(
            !body.contains(concat!("StreamStats", "::load")),
            "draw_log_body must not read the log file itself (U13) — the value must \
             come from the Snapshot built off-thread in build_snapshot"
        );

        // A run whose slot log_path points nowhere must still paint the Log tab
        // without panicking or touching disk during draw.
        let mut st = run_with(Phase::Review, 1);
        st.slots[0].log_path = Some(PathBuf::from("/nonexistent/does/not/exist.log"));
        let swarm = SparPaths::new("/x");
        let mut app = test_app();
        let mut rail = ListState::default();
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &swarm,
                &[],
                &[],
                Some(&st),
                "",
                "",
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                &HomeData::default(),
                None,
                &mut app,
                &mut rail,
            )
        })
        .unwrap();
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
                "",
                &[],
                &[],
                "",
                &[],
                &[],
                &[],
                &[],
                &HomeData::default(),
                None,
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

    /// AC-17: without the run's own frozen config there is no way to evaluate the
    /// gate's own criteria predicate — the tab must say so, not silently show
    /// blockers computed from whatever config the TUI process happened to load.
    #[test]
    fn review_without_frozen_config_says_so_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let st = run_with(Phase::Review, 7);
        swarm.ensure_run_dirs(&st.id).unwrap();
        let records = review_records(&swarm, Some(&st), None);
        assert!(
            records
                .iter()
                .any(|r| r.head.contains("frozen config unavailable")),
            "{records:#?}"
        );
    }

    /// AC-16: the critique is resolved through the critic slot's own id
    /// convention, not `SlotState.artifact` (which is always `plan.md` — the
    /// slot completion gate, not this document's real filename). `plan.md` and
    /// the critique must come from two distinct files, each with its own
    /// sections, not one file read twice under two names.
    #[test]
    fn plan_tab_resolves_the_critique_through_the_critic_slot_id() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let st = run_with(Phase::Review, 7);
        let critic_id = &st.slots[1].id;
        assert_eq!(st.slots[1].role, SlotRole::PlanCritic);
        swarm.ensure_run_dirs(&st.id).unwrap();
        std::fs::write(swarm.artifact(&st.id, "plan.md"), "# Plan\nDo the thing.\n").unwrap();
        std::fs::write(
            swarm.artifact(&st.id, &format!("plan-critique-{critic_id}.md")),
            "# Critique\nLooks fine.\n",
        )
        .unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "test-contract.md"),
            "AC-1: does a thing\n",
        )
        .unwrap();
        let docs = plan_docs(&swarm, Some(&st));
        let plan = docs
            .iter()
            .find(|r| r.body.iter().any(|l| l.contains("Do the thing.")))
            .expect("plan doc");
        let critique = docs
            .iter()
            .find(|r| r.body.iter().any(|l| l.contains("Looks fine.")))
            .expect("critique doc, resolved through the critic slot id, not `plan.md` again");
        assert_ne!(
            plan.source, critique.source,
            "plan.md and the critique must not share an identity: {docs:#?}"
        );
        let contract = docs
            .iter()
            .find(|r| r.body.iter().any(|l| l.contains("AC-1")))
            .expect("test-contract doc");
        assert_ne!(critique.source, contract.source);
    }

    /// AC-16: a critic slot whose critique file never landed on disk must name
    /// the missing document, not silently fall back to showing `plan.md` again.
    #[test]
    fn plan_tab_names_a_missing_critique_rather_than_repeating_plan_md() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let st = run_with(Phase::Review, 7);
        swarm.ensure_run_dirs(&st.id).unwrap();
        std::fs::write(swarm.artifact(&st.id, "plan.md"), "# Plan\nDo the thing.\n").unwrap();
        // No critique file, no test-contract.md written.
        let docs = plan_docs(&swarm, Some(&st));
        let missing = docs
            .iter()
            .filter(|r| matches!(r.kind, RecordKind::Note) && r.verb == "Missing")
            .collect::<Vec<_>>();
        assert!(
            missing
                .iter()
                .any(|r| r.head == "plan critique" && !r.summary.contains("plan.md")),
            "the missing critique must name its own conventional filename, not plan.md: {docs:#?}"
        );
    }

    /// AC-17: the ship gate treats a missing/empty review artifact as an
    /// unconditional blocker (`implement.rs`'s "review slot failed or produced no
    /// review"), not a neutral placeholder — the Review tab must show the same.
    #[test]
    fn review_missing_reviewer_artifact_is_shown_as_a_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let st = run_with(Phase::Review, 7);
        swarm.ensure_run_dirs(&st.id).unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "test-contract.md"),
            "AC-1: does a thing\n",
        )
        .unwrap();
        // Neither reviewer's artifact exists on disk.
        let cfg = Config::default();
        cfg.save_snapshot(&swarm, &st.id).unwrap();
        let records = review_records(&swarm, Some(&st), Some(&cfg));
        let blockers: Vec<_> = records
            .iter()
            .filter(|r| matches!(r.kind, RecordKind::Error))
            .collect();
        assert!(
            blockers.len() >= 2,
            "both reviewer slots have no artifact and must both read as blockers, {records:#?}"
        );
        assert!(
            blockers
                .iter()
                .all(|r| r.summary.contains("failed or produced no review")),
            "{blockers:#?}"
        );
    }

    /// AC-17: a reviewer slot the executor marked `Failed` mirrors the gate's own
    /// `!review_ok` (`implement.rs`'s `if !review_ok || missing_or_empty`) even when a
    /// *stale* `review-<slot>.md` from an earlier round of this same slot id is still on
    /// disk showing `approve` — the gate blocks on this regardless of that leftover file,
    /// so reading only the file (never `SlotState.status`) would under-report the
    /// blocker the gate actually enforces.
    #[test]
    fn review_failed_reviewer_slot_overrides_a_stale_approved_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let mut st = run_with(Phase::Review, 7);
        st.slots[5].status = SlotStatus::Failed;
        swarm.ensure_run_dirs(&st.id).unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "test-contract.md"),
            "AC-1: does a thing\n",
        )
        .unwrap();
        // Stale from a prior round of this same slot id: the current dispatch failed
        // before writing anything, but the file from a previous success is still here.
        std::fs::write(
            swarm.artifact(&st.id, "review-slot-5.md"),
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass\n",
        )
        .unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "review-slot-6.md"),
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass\n",
        )
        .unwrap();
        let cfg = Config::default();
        cfg.save_snapshot(&swarm, &st.id).unwrap();
        let records = review_records(&swarm, Some(&st), Some(&cfg));

        let grid_row = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Criterion))
            .expect("one Criterion record for AC-1");
        assert!(
            grid_row.body[0].contains("failed"),
            "the failed slot's cell must read `failed`, not the stale `pass`: {:?}",
            grid_row.body
        );

        let failed_verdict = records
            .iter()
            .find(|r| r.verb == "slot-5")
            .expect("a verdict record for the failed slot");
        assert_eq!(
            failed_verdict.kind,
            RecordKind::Error,
            "a failed slot must read as a blocker regardless of its stale artifact: {failed_verdict:?}"
        );
        assert!(
            !failed_verdict.summary.contains("pass")
                && !failed_verdict.body.iter().any(|l| l.contains("approve")),
            "must not surface the stale file's own approve/pass text: {failed_verdict:?}"
        );
    }

    /// AC-17: every reviewer's cell for a given `AC-n` lives on the *same* row (a
    /// grid), not one line per reviewer stacked in the body.
    #[test]
    fn review_criteria_grid_puts_every_reviewer_on_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let st = run_with(Phase::Review, 7);
        swarm.ensure_run_dirs(&st.id).unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "test-contract.md"),
            "AC-1: does a thing\n",
        )
        .unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "review-slot-5.md"),
            "## Verdict\napprove\n\n## Acceptance\nAC-1: pass\n",
        )
        .unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "review-slot-6.md"),
            "## Verdict\nrequest_changes\n\n## Acceptance\nAC-1: fail\n",
        )
        .unwrap();
        let cfg = Config::default();
        cfg.save_snapshot(&swarm, &st.id).unwrap();
        let records = review_records(&swarm, Some(&st), Some(&cfg));
        let grid_row = records
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Criterion))
            .expect("one Criterion record for AC-1");
        assert_eq!(
            grid_row.body.len(),
            1,
            "both reviewers' cells must share one row, {:?}",
            grid_row.body
        );
        assert!(grid_row.body[0].contains("pass"), "{:?}", grid_row.body);
        assert!(grid_row.body[0].contains("fail"), "{:?}", grid_row.body);
    }

    /// AC-17: the per-reviewer blocking reasons must relax an `unverified` AC
    /// exactly the same cases `acceptance_blocks_ship`/`acceptance_block_reasons`
    /// do — `require_all_criteria = false` clears it, `= true` keeps it blocking.
    /// A second implementation of the predicate here would be able to drift from
    /// the actual gate.
    #[test]
    fn review_blockers_relax_unverified_only_when_require_all_criteria_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let swarm = SparPaths::new(dir.path());
        let st = run_with(Phase::Review, 7);
        swarm.ensure_run_dirs(&st.id).unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "test-contract.md"),
            "AC-1: does a thing\n",
        )
        .unwrap();
        std::fs::write(
            swarm.artifact(&st.id, "review-slot-5.md"),
            "## Verdict\napprove\n\n## Acceptance\nAC-1: unverified\n",
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.review.require_all_criteria = false;
        cfg.save_snapshot(&swarm, &st.id).unwrap();
        let relaxed = review_records(&swarm, Some(&st), Some(&cfg));
        let reviewer_row = relaxed
            .iter()
            .find(|r| r.kind == RecordKind::Doc || r.kind == RecordKind::Error)
            .expect("one record for the reviewer's own verdict");
        assert_eq!(
            reviewer_row.kind,
            RecordKind::Doc,
            "an unverified AC must not block when require_all_criteria is false: {:?}",
            reviewer_row.body
        );

        cfg.review.require_all_criteria = true;
        cfg.save_snapshot(&swarm, &st.id).unwrap();
        let strict = review_records(&swarm, Some(&st), Some(&cfg));
        let reviewer_row = strict
            .iter()
            .find(|r| r.kind == RecordKind::Doc || r.kind == RecordKind::Error)
            .expect("one record for the reviewer's own verdict");
        assert_eq!(
            reviewer_row.kind,
            RecordKind::Error,
            "the same unverified AC must block when require_all_criteria is true: {:?}",
            reviewer_row.body
        );
    }

    fn diff_records_for(paths: &[(&str, &str)]) -> Vec<Record> {
        paths
            .iter()
            .map(|(path, body)| Record {
                kind: RecordKind::FileDiff,
                glyph: "±",
                verb: "M".to_string(),
                head: path.to_string(),
                summary: format!("{path} +1 -0"),
                body: vec![body.to_string()],
                time: None,
                elapsed: None,
                actor: None,
                ok: None,
                source: SourceId::Diff {
                    worktree: "wt".to_string(),
                    path: path.to_string(),
                },
                folded_by_default: true,
                has_command_row: false,
            })
            .collect()
    }

    /// AC-18: a run never looked at before marks every file as new (over-marking
    /// per "never fewer"), but shows no banner — there is nothing to compare a
    /// timestamp against yet.
    #[test]
    fn diff_watermark_first_look_marks_every_file_new_with_no_banner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diff_watermark.json");
        let records = diff_records_for(&[("a.rs", "+1"), ("b.rs", "+2")]);
        let out = apply_diff_watermark_at(&path, "run1", "s1", Some("base1"), records);
        assert!(
            !out.iter().any(|r| matches!(r.kind, RecordKind::Section)),
            "no previous look to compare against: {out:#?}"
        );
        assert!(
            out.iter().all(|r| r.summary.starts_with("NEW ·")),
            "{out:#?}"
        );
    }

    /// AC-18: after marking seen, an unchanged file loses its NEW mark and a
    /// changed one keeps it, with a banner reporting exactly the changed count.
    #[test]
    fn diff_watermark_marks_only_changed_files_after_a_look() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diff_watermark.json");
        let seen = diff_records_for(&[("a.rs", "+1"), ("b.rs", "+2")]);
        mark_diff_seen_at(&path, "run1", "s1", Some("base1"), &seen);

        let unchanged = diff_records_for(&[("a.rs", "+1"), ("b.rs", "+2")]);
        let out = apply_diff_watermark_at(&path, "run1", "s1", Some("base1"), unchanged);
        assert!(
            !out.iter().any(|r| r.summary.starts_with("NEW ·")),
            "nothing changed since the look: {out:#?}"
        );
        assert!(!out.iter().any(|r| matches!(r.kind, RecordKind::Section)));

        let changed = diff_records_for(&[("a.rs", "+1 changed"), ("b.rs", "+2")]);
        let out = apply_diff_watermark_at(&path, "run1", "s1", Some("base1"), changed);
        let banner = out
            .iter()
            .find(|r| matches!(r.kind, RecordKind::Section))
            .expect("one file changed since the look, banner must appear");
        assert!(banner.summary.contains("1 file"), "{}", banner.summary);
        let a = out.iter().find(|r| r.head == "a.rs").unwrap();
        assert!(a.summary.starts_with("NEW ·"));
        let b = out.iter().find(|r| r.head == "b.rs").unwrap();
        assert!(!b.summary.starts_with("NEW ·"));
    }

    /// AC-18: a corrupt watermark file reads as "never looked" — over-marking,
    /// never under-marking, and never a panic.
    #[test]
    fn diff_watermark_corrupt_file_reads_as_never_looked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diff_watermark.json");
        std::fs::write(&path, "{ not json").unwrap();
        let records = diff_records_for(&[("a.rs", "+1")]);
        let out = apply_diff_watermark_at(&path, "run1", "s1", Some("base1"), records);
        assert!(
            out.iter().all(|r| r.summary.starts_with("NEW ·")),
            "{out:#?}"
        );
        assert!(!out.iter().any(|r| matches!(r.kind, RecordKind::Section)));
    }

    /// AC-18: the watermark is scoped per selected worktree, not just per run — a
    /// path marked seen in slot A's worktree must not suppress `NEW` on the
    /// same-named path in slot B's worktree under the same run (round-review
    /// minor finding).
    #[test]
    fn diff_watermark_is_scoped_to_the_selected_worktree_not_just_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diff_watermark.json");
        let seen_in_a = diff_records_for(&[("a.rs", "+1")]);
        mark_diff_seen_at(&path, "run1", "slot-a", Some("base1"), &seen_in_a);

        let same_path_in_b = diff_records_for(&[("a.rs", "+1")]);
        let out = apply_diff_watermark_at(&path, "run1", "slot-b", Some("base1"), same_path_in_b);
        assert!(
            out.iter().all(|r| r.summary.starts_with("NEW ·")),
            "a different slot's worktree must not inherit another slot's seen mark: {out:#?}"
        );
    }

    /// AC-18: a worktree recreated or rebased onto a different base commit is a
    /// different diff identity even under the same run/slot id — its watermark
    /// must not inherit the old base's "seen" state (round-6 review finding).
    #[test]
    fn diff_watermark_is_scoped_to_the_worktree_base_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diff_watermark.json");
        let seen = diff_records_for(&[("a.rs", "+1")]);
        mark_diff_seen_at(&path, "run1", "s1", Some("base1"), &seen);

        let same_path_rebased = diff_records_for(&[("a.rs", "+1")]);
        let out = apply_diff_watermark_at(&path, "run1", "s1", Some("base2"), same_path_rebased);
        assert!(
            out.iter().all(|r| r.summary.starts_with("NEW ·")),
            "a rebased worktree (different base_commit) must not inherit the old base's seen mark: {out:#?}"
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
            unit_id: None,
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

    /// U28, first rule: `PlanApproved` is a handoff only while nothing in the unit is
    /// running. A unit whose plan is approved *and* which has an active leg has
    /// already been dispatched, so the approval is stale bookkeeping and nobody is
    /// waiting. Two rejected operator fixes (runs `e72f434e`, `3be317b2`) got this
    /// scenario wrong in opposite directions: one retargeted the row onto the idle
    /// `PlanApproved` leg and sank the unit in the attention sort; the other left the
    /// row on the active leg but still reported the unit as wanting the operator,
    /// producing a permanent NEEDS YOU with no action to take (`Review` has no gate
    /// button, and nothing else can act on the stale approval). Neither may happen:
    /// the row acts on the active leg, and the unit must not claim to want you.
    /// `:plan` and `n` must resolve the same browsed project. `:plan` used to read
    /// `swarm.project_root`, the *snapshot's* root, while `n` read the live
    /// `active_root` that `rail_enter` updates the instant the operator drills in — so
    /// in the window before the target project's snapshot lands they disagreed (review
    /// 3be317b2, found after `n` had already been fixed). Both now go through
    /// `browsed_project_target`. The fixture makes the two roots differ on purpose: a
    /// test that passes `swarm.project_root` as `active_root` cannot see this at all.
    #[test]
    fn plan_and_n_agree_on_the_browsed_project_when_the_snapshot_lags() {
        let stale = PathBuf::from("/nonexistent/stale-snapshot-root");
        let live = PathBuf::from("/nonexistent/live-drilled-into");
        let projects: Vec<registry::ProjectEntry> = [&stale, &live]
            .iter()
            .map(|root| registry::ProjectEntry {
                root: (*root).clone(),
                name: root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string),
                last_seen: Utc::now(),
                last_run_id: None,
            })
            .collect();

        let mut app = App::new(None, Config::default(), None);
        app.browse = BrowseLevel::Runs; // drilled in: `in_project()` holds
        assert!(app.browse.in_project());

        assert_eq!(
            browsed_project_target(&app, &projects, live.as_path()),
            Some(live.as_path()),
            "inside a project the live active_root wins, not the lagging snapshot root"
        );

        // And the palette reaches the same answer through the same helper.
        let swarm = SparPaths::new(&stale);
        let runs: Vec<state::RunSummary> = Vec::new();
        run_palette(
            &mut app,
            &swarm,
            &projects,
            &[],
            None,
            &runs,
            None,
            live.as_path(),
            "plan do the thing",
        )
        .unwrap();
        assert_eq!(
            app.new_run.as_ref().unwrap().project.as_deref(),
            Some(live.as_path()),
            "`:plan` must target the project the operator drilled into, not the stale \
             snapshot root"
        );
    }

    #[test]
    fn a_plan_approved_leg_with_an_active_sibling_does_not_want_the_operator() {
        let runs = vec![
            summary("plan1", Phase::PlanApproved, None, 90),
            summary("impl1", Phase::Review, Some("plan1"), 5),
        ];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 1, "one unit of work");
        let unit = &rows[0];

        assert_eq!(unit.id, "impl1", "the row still acts on the active leg");
        assert_eq!(unit.phase, Phase::Review);
        assert_eq!(
            unit.wants, 0,
            "impl1 is live, so plan1's approval is not a live handoff"
        );
        assert!(
            !unit_wants_operator(unit),
            "a working sibling means nobody is actually waiting: {unit:?}"
        );
        assert!(
            !wants_operator(unit),
            "the representative leg agrees — this is the only self-consistent answer"
        );
        assert_eq!(
            runs_needing_attention(&rows),
            0,
            "the roll-up and the row must agree"
        );
        assert!(
            attention_flag(unit, unit_wants_operator(unit)).is_none(),
            "no flag on a row with nothing to act on"
        );
    }

    /// U28's other half: once the active leg finishes, the approval is a live handoff
    /// again, and the row must fold to the *approvable* leg (`plan1`), not the
    /// finished one (`impl1`) — `plan1` is the only leg with a real action
    /// (`:implement`) to take. This is `fold_units`' `wants_operator` tiebreak: both
    /// legs are `Attention::Idle`, so attention alone cannot pick between them, and
    /// picking `impl1` by recency alone would reproduce the unactionable-alert bug
    /// with a `Done` leg standing in for `Review`.
    #[test]
    fn a_plan_approved_leg_wants_the_operator_once_its_sibling_finishes() {
        let runs = vec![
            summary("plan1", Phase::PlanApproved, None, 90),
            summary("impl1", Phase::Done, Some("plan1"), 5),
        ];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 1, "one unit of work");
        let unit = &rows[0];

        assert_eq!(
            unit.id, "plan1",
            "the approvable leg must represent the row, not the finished one"
        );
        assert_eq!(unit.phase, Phase::PlanApproved);
        assert_eq!(unit.wants, 1);
        assert!(unit_wants_operator(unit), "{unit:?}");
        assert!(
            wants_operator(unit),
            "the representative leg agrees — this is the only self-consistent answer"
        );
        assert_eq!(runs_needing_attention(&rows), 1);
        assert!(attention_flag(unit, unit_wants_operator(unit)).is_some());
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

    /// Round-2 review (minor): an abandoned leg is not actually running (no live
    /// orchestrator), so it must not suppress a sibling `PlanApproved` leg's handoff
    /// the way a genuinely active leg does. Before the fix, `unit_has_active_leg`
    /// checked phase alone, so `plan1`'s approval went uncounted here even though
    /// `impl1` was not really running it — an undercount in the `⚑n` roll-up (the
    /// unit itself still read NEEDS YOU, since `impl1`'s own abandonment already
    /// makes it `Attention::Broken` and the representative).
    #[test]
    fn an_abandoned_leg_does_not_suppress_a_siblings_planapproved_handoff() {
        let mut impl1 = summary("impl1", Phase::Review, Some("plan1"), 5);
        impl1.abandoned = true;
        let runs = vec![summary("plan1", Phase::PlanApproved, None, 90), impl1];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 1, "one unit of work");
        let unit = &rows[0];
        assert_eq!(
            unit.id, "impl1",
            "the broken leg is still the representative"
        );
        assert_eq!(
            unit.wants, 2,
            "the abandoned leg does not cancel plan1's live approval"
        );
        assert_eq!(runs_needing_attention(&rows), 2);
    }

    /// AC-28. `fold_units`'s `id` follows whichever leg is loudest, which can change
    /// between snapshots as attention shifts within a unit — but `unit_id` (what a
    /// Home cursor keys on) must not, or the cursor jumps every time the loudest leg
    /// changes (round-11 review finding).
    #[test]
    fn unit_id_is_stable_across_a_change_in_the_loudest_leg() {
        let runs = vec![
            summary("root", Phase::Review, None, 30),
            summary("legA", Phase::AwaitingPlanApproval, Some("root"), 20),
            summary("legB", Phase::Review, Some("root"), 10),
        ];
        let (rows, _) = fold_units(runs);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "legA", "legA is loudest: it holds the gate");
        assert_eq!(rows[0].unit_id.as_deref(), Some("root"));

        // Next snapshot: legB is now the gate instead of legA, so the displayed id
        // moves to it — but it is still the same unit of work.
        let runs2 = vec![
            summary("root", Phase::Review, None, 30),
            summary("legA", Phase::Review, Some("root"), 20),
            summary("legB", Phase::AwaitingPlanApproval, Some("root"), 10),
        ];
        let (rows2, _) = fold_units(runs2);
        assert_eq!(rows2.len(), 1);
        assert_eq!(rows2[0].id, "legB", "the loudest leg moved");
        assert_eq!(
            rows2[0].unit_id, rows[0].unit_id,
            "the unit's identity does not move just because a different leg got loud"
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

    /// AC-34, round-7 review finding: the collision-avoidance half of
    /// `home_folds_per_project_and_keeps_every_gate` called `fold_units` by hand
    /// per project rather than exercising `gather_home`'s own per-project loop, so
    /// a regression there (e.g. accidentally folding across the whole registry)
    /// could not fail it. This drives `gather_home` itself over two real project
    /// directories that each happen to have a run named `a`.
    #[test]
    fn gather_home_folds_per_project_and_avoids_id_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        std::fs::create_dir_all(one.join(".spar/runs")).unwrap();
        std::fs::create_dir_all(two.join(".spar/runs")).unwrap();

        let save = |root: &Path, id: &str, phase: Phase, parent: Option<&str>| {
            let paths = SparPaths::new(root);
            paths.ensure_run_dirs(id).unwrap();
            let mut st = RunState::new(id, WorkflowKind::Loop, root.to_path_buf());
            st.phase = phase;
            st.parent_run = parent.map(str::to_string);
            st.save(&paths).unwrap();
        };
        // Project "one" has a real, unrelated run "a". Project "two" has no run
        // named "a" of its own, only a leg "b" whose `parent_run` happens to name
        // the same id. If folding ever crossed project boundaries, "b" would
        // wrongly merge into project "one"'s "a" instead of standing alone.
        save(&one, "a", Phase::Review, None);
        save(&two, "b", Phase::AwaitingPlanApproval, Some("a"));

        let projects = [
            registry::ProjectEntry {
                root: one.clone(),
                name: Some("one".into()),
                last_seen: Utc::now(),
                last_run_id: None,
            },
            registry::ProjectEntry {
                root: two.clone(),
                name: Some("two".into()),
                last_seen: Utc::now(),
                last_run_id: None,
            },
        ];
        let folded = gather_home(&projects);
        assert_eq!(folded[0].len(), 1);
        assert_eq!(folded[0][0].id, "a", "project one's own run, untouched");
        assert_eq!(
            folded[1].len(),
            1,
            "project two's orphan leg must not merge into project one's \"a\": {:?}",
            folded[1]
        );
        assert_eq!(folded[1][0].id, "b");
        assert_eq!(
            runs_needing_attention(&folded[1]),
            1,
            "the orphan leg's own gate must still count"
        );
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

    /// Round-1 review (major): `unit_wants_operator` and `wants_operator` are
    /// structurally equal on every row `fold_units` can produce, once rule 2's
    /// tiebreak (attention first, `wants_operator` only within a tied level) holds —
    /// a `Gate`/`Broken` leg always outranks every other attention level, so it is
    /// always the representative when one exists; a counted `PlanApproved` leg means
    /// no leg is active, so every leg is `Idle` and the tiebreak makes the
    /// `PlanApproved` leg the representative. That equality is *why* the four call
    /// sites that read `unit_wants_operator` (Home's band, both rail flags, the `a`
    /// cycle key) can share one predicate with the per-leg `wants_operator` used
    /// inside `fold_units` itself — reverting any one of those call sites back to
    /// `wants_operator` cannot fail a test built on real `fold_units` output, because
    /// on every row `fold_units` emits the two predicates already agree. This is the
    /// invariant that makes that true, checked exhaustively over every reachable
    /// (phase, abandoned) pair for a two-leg unit; it fails the moment rule 2's
    /// tiebreak regresses (verified by temporarily deleting the tiebreak: 3
    /// divergences, all the `PlanApproved`-vs-idle-sibling case attempt two got
    /// wrong).
    #[test]
    fn unit_wants_operator_matches_wants_operator_on_every_fold_units_row() {
        const ALL_PHASES: [Phase; 26] = [
            Phase::Init,
            Phase::PrepareIsolation,
            Phase::SpawnSlots,
            Phase::Dispatch,
            Phase::WaitCompletion,
            Phase::PlanReady,
            Phase::Spec,
            Phase::AwaitingPlanApproval,
            Phase::PlanApproved,
            Phase::PlanRejected,
            Phase::Review,
            Phase::Suite,
            Phase::Rank,
            Phase::Fix,
            Phase::PeerRelay,
            Phase::AwaitingWinnerConfirm,
            Phase::AwaitingReconcile,
            Phase::AwaitingShipConfirm,
            Phase::AwaitingRoundExtension,
            Phase::Shipping,
            Phase::Done,
            Phase::Escalated,
            Phase::Failed,
            Phase::Stuck,
            Phase::Quota,
            Phase::Stopped,
        ];

        let mut checked = 0u32;
        for &root_phase in &ALL_PHASES {
            for &leg_phase in &ALL_PHASES {
                for root_abandoned in [false, true] {
                    for leg_abandoned in [false, true] {
                        // `is_abandoned` (state.rs) never sets `abandoned` true on a
                        // waitable-stop phase, so those (phase, abandoned) pairs
                        // cannot occur in practice — skip rather than assert on
                        // unreachable input.
                        if root_abandoned && root_phase.is_waitable_stop() {
                            continue;
                        }
                        if leg_abandoned && leg_phase.is_waitable_stop() {
                            continue;
                        }
                        let mut root = summary("root", root_phase, None, 10);
                        root.abandoned = root_abandoned;
                        let mut leg = summary("leg", leg_phase, Some("root"), 5);
                        leg.abandoned = leg_abandoned;
                        let (rows, _) = fold_units(vec![root, leg]);
                        for row in &rows {
                            assert_eq!(
                                unit_wants_operator(row),
                                wants_operator(row),
                                "diverged for root={root_phase:?}/{root_abandoned} \
                                 leg={leg_phase:?}/{leg_abandoned}: {row:?}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 100, "the phase matrix must actually run");
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
/// | `HomeData { rows, project_stats, loading }` on `Snapshot`, passed to `draw` | B/C | the off-thread roll-up `draw` consumes |
/// | `gather_home(&[ProjectEntry]) -> Vec<Vec<RunSummary>>` | B/C | one folded, archived-filtered listing per project; disk, off-thread |
/// | `project_stats_of(&[Vec<RunSummary>]) -> Vec<ProjectStat>` | B | pure; counts folded rows |
/// | `build_home_rows(projects, folded, scope, watermark, now, loading)` | C | pure; banding, ranking, capping |
/// | `home_overview(rows, scope, watermark, now) -> String` | C | Main's Home body |
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
            unit_id: None,
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
            concat!("as a composer ", "error"),
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

        // And the stats derived from that same pass agree with it. The root is
        // PlanApproved (a handoff `wants_operator` counts, round-9 review) and the leg
        // is a live gate — both legs want the operator, so the folded row's `wants`
        // is 2, not 1.
        let stats = project_stats_of(&folded);
        assert_eq!(stats[0].n_runs, 2);
        assert_eq!(
            stats[0].needs_you, 2,
            "the PlanApproved root and the gated leg both want the operator"
        );
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
            let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now, false);
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
                    HomeBand::StartNew,
                    HomeBand::NeedsMe,
                    HomeBand::Running,
                    HomeBand::Finished
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
            ("plap", Phase::PlanApproved, Some(HomeBand::NeedsMe)),
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
        let rows = build_home_rows(&projects, &[runs], &HomeScope::All, watermark, now, false);
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
            false,
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
            false,
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
            false,
        );
        assert_eq!(
            ids_in(&rows, HomeBand::Running),
            vec!["new_work", "old_work"]
        );
    }

    /// AC-19/review finding: on an equal wait, a Gate outranks a Broken run —
    /// the plan's stated tiebreak, not registry/listing order.
    #[test]
    fn needs_me_ties_break_gate_above_broken() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let same_updated = now - chrono::Duration::minutes(30);
        let mut broken = run_in("broken01", Phase::Failed, 30, &root);
        broken.updated_at = same_updated;
        let mut gate = run_in("gate0001", Phase::AwaitingPlanApproval, 30, &root);
        gate.updated_at = same_updated;
        let folded = vec![vec![broken, gate]];
        let rows = build_home_rows(
            &projects,
            &folded,
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
            false,
        );
        assert_eq!(
            ids_in(&rows, HomeBand::NeedsMe),
            vec!["gate0001", "broken01"],
            "equal wait: Gate ranks above Broken"
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
            false,
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
            false,
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
            false,
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
        // The `⚑N` suffix is rendered from `wants` by both the rail row and Main's
        // detail; assert the fact rather than one of the two renderings.
        assert!(
            rows.iter().any(|r| matches!(
                r,
                HomeRow::Run { run, .. } if run.wants == 2
            )),
            "a multi-gate unit must carry its leg count on the row"
        );
    }

    /// Round-9 review: `build_home_rows` banded `PlanApproved` into NeedsMe, but
    /// `runs_needing_attention`/`home_needs_you`/the rail flag did not agree, so a
    /// single PlanApproved run showed a NEEDS YOU band with `0 need you` above it.
    /// `wants_operator` is the one predicate every one of those goes through now, so
    /// they cannot disagree with each other.
    #[test]
    fn plan_approved_agrees_across_every_needs_you_roll_up() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let run = run_in("plap0001", Phase::PlanApproved, 20, &root);

        assert_eq!(
            runs_needing_attention(std::slice::from_ref(&run)),
            1,
            "the fleet roll-up must count the PlanApproved handoff"
        );

        let folded = vec![vec![run]];
        let stats = project_stats_of(&folded);
        assert_eq!(
            stats[0].needs_you, 1,
            "the Projects-level flag must agree with the fleet roll-up"
        );

        let rows = build_home_rows(
            &projects,
            &folded,
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
            false,
        );
        assert_eq!(
            home_needs_you(&rows),
            1,
            "Home's own header must agree with the band it renders"
        );
        let row = rows
            .iter()
            .find_map(|r| match r {
                HomeRow::Run {
                    band: HomeBand::NeedsMe,
                    run,
                    ..
                } => Some(run),
                _ => None,
            })
            .expect("PlanApproved lands in NeedsMe");
        assert!(
            attention_flag(row, unit_wants_operator(row)).is_some(),
            "the rail flag must fire for the row Home says needs you"
        );
    }

    /// U28, driven through the real pipeline (`fold_units` then `build_home_rows`)
    /// rather than a hand-built row: a `PlanApproved` unit with a live sibling has
    /// already been dispatched, so it must not appear in NEEDS YOU, and the roll-up
    /// Home's header reads (`home_needs_you`) must agree with the fleet-wide one
    /// (`runs_needing_attention`) that a Runs-level view over the same folded data
    /// would compute.
    #[test]
    fn a_plan_approved_unit_with_an_active_leg_is_not_needs_you() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let plan1 = run_in("plan0001", Phase::PlanApproved, 90, &root);
        let mut impl1 = run_in("impl0001", Phase::Review, 5, &root);
        impl1.parent_run = Some("plan0001".to_string());

        let (folded, _) = fold_units(vec![plan1, impl1]);
        assert_eq!(folded.len(), 1, "one unit of work");

        let rows = build_home_rows(
            &projects,
            std::slice::from_ref(&folded),
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
            false,
        );
        assert_eq!(
            band_of(&rows, "impl0001"),
            Some(HomeBand::Running),
            "impl0001 is live and there is nothing to act on"
        );
        assert_eq!(home_needs_you(&rows), 0);
        assert_eq!(
            runs_needing_attention(&folded),
            0,
            "Home's header and the fleet roll-up must agree"
        );
    }

    /// U28's other half, through the same real pipeline: once the active leg
    /// finishes, the unit is a live handoff again, and the row Home renders must be
    /// the approvable leg — the one a NEEDS YOU alert can actually be acted on.
    #[test]
    fn a_plan_approved_unit_wants_you_and_is_actionable_once_its_leg_finishes() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let plan1 = run_in("plan0001", Phase::PlanApproved, 90, &root);
        let mut impl1 = run_in("impl0001", Phase::Done, 5, &root);
        impl1.parent_run = Some("plan0001".to_string());

        let (folded, _) = fold_units(vec![plan1, impl1]);
        assert_eq!(folded.len(), 1, "one unit of work");

        let rows = build_home_rows(
            &projects,
            std::slice::from_ref(&folded),
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
            false,
        );
        assert_eq!(
            band_of(&rows, "plan0001"),
            Some(HomeBand::NeedsMe),
            "the approvable leg, not the finished one, must be the row that's flagged"
        );
        assert_eq!(home_needs_you(&rows), 1);
        assert_eq!(runs_needing_attention(&folded), 1);
        let row = rows
            .iter()
            .find_map(|r| match r {
                HomeRow::Run {
                    band: HomeBand::NeedsMe,
                    run,
                    ..
                } => Some(run),
                _ => None,
            })
            .expect("the row lands in NeedsMe");
        assert_eq!(
            row.id, "plan0001",
            "Home's row must act on the leg that has something to do"
        );
        assert_eq!(row.phase, Phase::PlanApproved);
        assert!(attention_flag(row, unit_wants_operator(row)).is_some());
    }

    /// The `a` cycle key (`jump_to_attention`) goes through `unit_wants_operator` at
    /// the Runs level too — a folded unit stuck on U28's stale-approval case must not
    /// be a cycle target, and must become one again once it is genuinely actionable.
    #[test]
    fn jump_to_attention_follows_the_u28_conditional() {
        let root = PathBuf::from("/nonexistent/spar");
        let plan1 = run_in("plan0001", Phase::PlanApproved, 90, &root);
        let mut impl1 = run_in("impl0001", Phase::Review, 5, &root);
        impl1.parent_run = Some("plan0001".to_string());
        let (stale, _) = fold_units(vec![plan1.clone(), impl1]);
        assert_eq!(stale.len(), 1);

        let mut app = App::new(None, Config::default(), None);
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        jump_to_attention(&mut app, &stale, &[]);
        assert_eq!(
            app.flash.as_ref().map(|(_, msg, ..)| msg.clone()),
            Some("nothing needs you".to_string()),
            "no cycle target while impl0001 is still live"
        );

        let mut impl1_done = run_in("impl0001", Phase::Done, 5, &root);
        impl1_done.parent_run = Some("plan0001".to_string());
        let (live, _) = fold_units(vec![plan1, impl1_done]);
        assert_eq!(live.len(), 1);

        let mut app = App::new(None, Config::default(), None);
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        jump_to_attention(&mut app, &live, &[]);
        assert_eq!(
            app.selected_run, 0,
            "the sole run in the list is the target"
        );
        assert_eq!(
            app.flash.as_ref().map(|(_, msg, ..)| msg.clone()),
            Some("→ plan0001 needs you".to_string())
        );
    }

    /// A row shape `fold_units` can never itself produce
    /// (`unit_wants_operator_matches_wants_operator_on_every_fold_units_row`, `folding`
    /// module, proves the two predicates agree on every row `fold_units` can emit): a
    /// folded unit (`legs > 1`) whose representative leg is merely `Working` (`Review`
    /// — neither a gate, a breakage, nor `PlanApproved`) while `wants` still counts a
    /// sibling. Real `fold_units` output can never disagree with `wants_operator` this
    /// way, so every test that only drives `build_home_rows`, the rail flags, or
    /// `jump_to_attention` through real `fold_units` output cannot tell
    /// `unit_wants_operator` and `wants_operator` apart at those call sites — reverting
    /// any one of them back to `wants_operator` stays green (round-2 review, major).
    /// This row makes the difference observable directly at each site:
    /// `unit_wants_operator` says true (`wants > 0`), `wants_operator` says false.
    fn folded_but_representative_alone_does_not_want_you(
        id: &str,
        root: &Path,
    ) -> state::RunSummary {
        let mut r = run_in(id, Phase::Review, 5, root);
        r.legs = 2;
        r.wants = 1;
        r
    }

    /// Independent coverage of the Home-band call site (`build_home_rows`, U28):
    /// reverting its `unit_wants_operator(r)` check back to `wants_operator(r)` sinks
    /// this row into RUNNING, since `Review` is an active phase and `wants_operator`
    /// alone says false. Driven through the real builder with a hand-set `legs`/
    /// `wants`, the same pattern `folding_never_hides_a_gate_from_home` already uses
    /// to get a row `fold_units` itself would not emit.
    #[test]
    fn home_band_membership_uses_unit_wants_operator() {
        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let row = folded_but_representative_alone_does_not_want_you("legb0003", &root);

        let rows = build_home_rows(
            &projects,
            &[vec![row.clone()]],
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
            false,
        );
        assert_eq!(
            band_of(&rows, "legb0003"),
            Some(HomeBand::NeedsMe),
            "wants > 0 must land in NeedsMe even though the representative leg's own \
             phase (Review) is neither a gate, a breakage, nor PlanApproved"
        );
        assert_eq!(
            home_needs_you(&rows),
            1,
            "Home's own header must agree with the band it just rendered"
        );
        assert_eq!(
            runs_needing_attention(std::slice::from_ref(&row)),
            1,
            "the fleet roll-up must agree with the band too"
        );
    }

    /// Independent coverage of both rail-flag call sites (`rail_home_items` at the
    /// Home level, `rail_run_items` at the Runs level): each renders its `force`
    /// argument as a `⚑` in the lead column, so this exercises the actual painted
    /// row rather than a bare `attention_flag` call — reverting `unit_wants_operator`
    /// back to `wants_operator` at either site drops the flag from the screen.
    #[test]
    fn rail_flags_fire_on_a_row_only_unit_wants_operator_flags() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        fn rendered(items: Vec<ListItem<'static>>, w: u16) -> String {
            let mut term = Terminal::new(TestBackend::new(w, 1)).unwrap();
            term.draw(|f| f.render_widget(List::new(items), f.area()))
                .unwrap();
            let buf = term.backend().buffer();
            (0..w).map(|x| buf[(x, 0)].symbol()).collect()
        }

        let root = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&root, "spar")];
        let now = Utc::now();
        let row = folded_but_representative_alone_does_not_want_you("legb0004", &root);

        let home_rows = build_home_rows(
            &projects,
            &[vec![row.clone()]],
            &HomeScope::All,
            now - chrono::Duration::hours(6),
            now,
            false,
        );
        let app = App::new(None, Config::default(), None);
        let home_run_item = rail_home_items(&home_rows, &projects, &app, 40, false)
            .into_iter()
            .nth(4)
            .expect("the NeedsMe run row after StartNew band");
        assert!(
            rendered(vec![home_run_item], 40).contains('⚑'),
            "the Home rail must flag a row unit_wants_operator flags, even though \
             wants_operator alone would not"
        );

        let runs_items = rail_run_items(std::slice::from_ref(&row), &app, 40, false);
        assert!(
            rendered(runs_items, 40).contains('⚑'),
            "the Runs rail must flag the same row"
        );
    }

    /// Independent coverage of the `a` cycle key's Runs-level call site
    /// (`jump_to_attention`): reverting its `unit_wants_operator` check back to
    /// `wants_operator` makes this row invisible to the cycle, since `wants_operator`
    /// alone says false for a `Review` representative even though the unit's `wants`
    /// is nonzero.
    #[test]
    fn jump_to_attention_targets_a_row_only_unit_wants_operator_flags() {
        let root = PathBuf::from("/nonexistent/spar");
        let row = folded_but_representative_alone_does_not_want_you("legb0005", &root);

        let mut app = App::new(None, Config::default(), None);
        app.browse = BrowseLevel::Runs;
        app.selected_run = 0;
        jump_to_attention(&mut app, std::slice::from_ref(&row), &[]);
        assert_eq!(
            app.flash.as_ref().map(|(_, msg, ..)| msg.clone()),
            Some("→ legb0005 needs you".to_string()),
            "the cycle must land on the row, not report \"nothing needs you\""
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
        let all = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now, false);
        let scoped = build_home_rows(
            &projects,
            &folded,
            &HomeScope::Project(b.clone()),
            watermark,
            now,
            false,
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
        let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now, false);
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
        let mut app = App::new(None, Config::default(), Some(root.as_path()));
        // `App::new` reads whatever watermark happens to be at `watermark_path()`
        // under this process's `spar_home()` — a round-7 review finding noted that
        // the ambient file (real under a caller-set `SPAR_HOME`, a fresh per-process
        // temp dir otherwise) makes this assertion depend on state outside the test.
        // Pin it explicitly so the assertion holds regardless of the environment.
        app.home_watermark = Utc::now() - chrono::Duration::hours(1);
        let folded = vec![vec![run_in("justdone", Phase::Done, 1, &root)]];
        let first = build_home_rows(
            &projects,
            &folded,
            &app.home_scope,
            app.home_watermark,
            Utc::now(),
            false,
        );
        let later = build_home_rows(
            &projects,
            &folded,
            &app.home_scope,
            app.home_watermark,
            Utc::now() + chrono::Duration::minutes(30),
            false,
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
        rail_select(&mut app, 0, 0, &rows, &[], 0);
        assert_eq!(
            app.selected_home, 1,
            "a click on a header must not move the cursor"
        );
        rail_select(&mut app, 3, 0, &rows, &[], 0);
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
            HomeRow::Project(0, root.clone()),
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

    /// Round-7 review finding (review-1-cli-claude, major): a Home `Enter` target
    /// that never reappears in the snapshot (archived, its dir removed, or a
    /// different leg becomes the fold's loudest member) used to pin
    /// `home_target_run` forever, leaving Main/Agents empty with no way out short
    /// of manually picking a different row. `resolve_home_target` must give up once
    /// the wall-clock `HOME_TARGET_GIVE_UP` budget elapses, release the pin, flash,
    /// and return to Home — while a target that is merely one snapshot behind (R2)
    /// still resolves normally and does not trip the give-up path early.
    #[test]
    fn a_ghost_home_target_eventually_releases_the_pin() {
        let root = PathBuf::from("/nonexistent/spar");
        let runs = vec![run_in("keep0001", Phase::Review, 5, &root)];

        // The target is present: resolves immediately, no ticks spent.
        let mut app = App::new(None, Config::default(), None);
        app.home_target_run = Some("keep0001".into());
        app.browse = BrowseLevel::Agents;
        assert!(resolve_home_target(&mut app, &runs, true));
        assert!(app.home_target_run.is_none());
        assert!(app.home_target_since.is_none());
        assert_eq!(app.selected_run, 0);

        // The target is missing (R2's one-tick lag, or a run that is truly gone): the
        // clock starts but the pin holds while inside the budget. Called many times to
        // prove the budget is wall clock, not call count — the previous counter gave up
        // after a fixed number of calls regardless of elapsed time.
        let mut app = App::new(None, Config::default(), None);
        app.home_target_run = Some("ghost0001".into());
        app.browse = BrowseLevel::Agents;
        for _ in 0..200 {
            assert!(!resolve_home_target(&mut app, &runs, true));
            assert!(
                app.home_target_run.is_some(),
                "must not give up while inside the budget"
            );
        }
        assert_eq!(app.browse, BrowseLevel::Agents);

        // Past the deadline: the pin releases and Home reclaims focus. Backdating the
        // start beats sleeping for the budget.
        app.home_target_since = Some(
            Instant::now()
                .checked_sub(HOME_TARGET_GIVE_UP * 2)
                .expect("monotonic clock is older than the give-up budget"),
        );
        assert!(!resolve_home_target(&mut app, &runs, true));
        assert!(app.home_target_run.is_none(), "the ghost pin must release");
        assert!(app.home_target_since.is_none());
        assert_eq!(app.browse, BrowseLevel::Home);
        assert!(
            app.flash.is_some(),
            "the operator must be told the run went away"
        );
    }

    /// Review 3be317b2 (major): the give-up clock used to start on the very first
    /// post-`Enter` frame, whose snapshot is normally still Home's own (cross-project,
    /// `runs` empty) rather than the target project's. A slow scan of the target
    /// project (the "thousands of run dirs" scale the give-up budget exists at) could
    /// then exceed `HOME_TARGET_GIVE_UP` before that project's snapshot ever landed,
    /// discarding a perfectly legitimate Home `Enter`. The clock must not start until
    /// `resolve_home_target` has actually been handed a snapshot for the target's own
    /// project — an arbitrary number of misses against a snapshot for some *other*
    /// project must not burn the budget at all.
    #[test]
    fn the_give_up_clock_does_not_start_before_the_target_projects_own_snapshot() {
        let runs: Vec<state::RunSummary> = Vec::new();

        let mut app = App::new(None, Config::default(), None);
        app.home_target_run = Some("ghost0001".into());
        app.browse = BrowseLevel::Agents;

        // Home's own (cross-project) snapshot keeps arriving instead of the target
        // project's — none of these may start the clock.
        for _ in 0..50 {
            assert!(!resolve_home_target(&mut app, &runs, false));
            assert!(app.home_target_since.is_none(), "clock must not start yet");
        }

        // The target project's own snapshot finally lands (still without the run):
        // only now does the clock start.
        assert!(!resolve_home_target(&mut app, &runs, true));
        assert!(app.home_target_since.is_some(), "clock starts here");
        assert_eq!(app.browse, BrowseLevel::Agents, "still inside the budget");

        // Backdate past the deadline and confirm the give-up still fires from here.
        app.home_target_since = Some(
            Instant::now()
                .checked_sub(HOME_TARGET_GIVE_UP * 2)
                .expect("monotonic clock is older than the give-up budget"),
        );
        assert!(!resolve_home_target(&mut app, &runs, true));
        assert!(app.home_target_run.is_none(), "the ghost pin must release");
        assert_eq!(app.browse, BrowseLevel::Home);
    }

    /// Round-2 review (major, both reviewers): the give-up clock's classification of
    /// "does this snapshot actually cover the target project" used to be root
    /// equality alone (`snap.swarm.project_root == active_root`), which is trivially
    /// true for the still-displayed Home snapshot when the operator enters a run
    /// whose project is already `active_root` — that snapshot never scanned any
    /// project's runs (`build_snapshot` only populates `runs` `if
    /// sel.browse.in_project()`), so the clock could start before any real
    /// per-project snapshot had landed. `snapshot_covers_target` must additionally
    /// require the snapshot to have been built at an in-project browse level.
    #[test]
    fn snapshot_covers_target_requires_an_in_project_snapshot_not_just_a_matching_root() {
        let root = PathBuf::from("/nonexistent/spar");

        let mut home_snap = Snapshot::loading(&root);
        home_snap.browse = BrowseLevel::Home;
        assert!(
            !snapshot_covers_target(&home_snap, &root),
            "a Home-level snapshot never scanned this project's runs, even with a \
             matching root"
        );

        let mut runs_snap = Snapshot::loading(&root);
        runs_snap.browse = BrowseLevel::Runs;
        assert!(
            snapshot_covers_target(&runs_snap, &root),
            "an in-project snapshot with a matching root is the real thing"
        );

        let mut other_project_snap = Snapshot::loading(&PathBuf::from("/nonexistent/elsewhere"));
        other_project_snap.browse = BrowseLevel::Agents;
        assert!(
            !snapshot_covers_target(&other_project_snap, &root),
            "an in-project snapshot for a different project must not count"
        );
    }

    /// AC-28. R3: Home re-ranks every snapshot (wait time changes every
    /// minute), so the cursor is glued to the row's identity, not its index.
    #[test]
    fn the_home_cursor_follows_the_row_not_the_index() {
        let root = PathBuf::from("/nonexistent/spar");
        let rows = nav_rows(&root);
        let mut app = App::new(None, Config::default(), None);
        rail_select(&mut app, 3, 0, &rows, &[], 0); // the running run
        assert_eq!(
            app.home_key.as_deref(),
            Some("run:/nonexistent/spar:work0001")
        );

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
            "api:notreal",
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

        let unsupported = by("api:notreal");
        assert!(
            !unsupported.available,
            "an unsupported api: provider must not be launchable"
        );
        assert!(
            unsupported.reason.as_deref().is_some_and(|r| !r.is_empty()),
            "a disabled unsupported api: ref must say why"
        );

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
        // roster (so the `spar doctor` pointer fires), not disabled rows.
        let bare = build_roster(&cfg_with(&[]), &[("claude".into(), false)], None);
        assert!(
            bare.is_empty(),
            "nothing usable must be an empty roster, not disabled rows: {bare:?}"
        );

        // R7, the realistic shape: an untouched `Config::default()` still carries
        // `default_provider_order()`, so with nothing on PATH the roster is three
        // *configured*, unavailable rows, not empty. `draw_new_run` gates the
        // doctor pointer on "nothing available", not "nothing present" — see the
        // render-level assertion in `render_stability`.
        let default_roster = build_roster(&Config::default(), &[], None);
        assert!(
            !default_roster.is_empty(),
            "the default provider order must still emit configured rows: {default_roster:?}"
        );
        assert!(
            default_roster.iter().all(|e| !e.available),
            "with nothing on PATH every default-order row must be unavailable: {default_roster:?}"
        );
    }

    /// Round-7 review finding (review-0-cli-codex, minor): the registry lists
    /// projects by `last_seen` (last *opened*, not last progressed), so picking the
    /// first project with a `last_run_id` could offer a stale fleet from a project
    /// the operator merely glanced at, instead of the run that actually moved most
    /// recently anywhere in the roster.
    #[test]
    fn most_recent_fleet_ignores_input_order() {
        let old = Utc::now() - chrono::Duration::hours(2);
        let newer = Utc::now() - chrono::Duration::minutes(5);
        // The registry-order-first candidate is the *older* run; the true most
        // recent one comes later in the input order.
        let candidates = vec![
            (old, "stale0001".to_string(), vec!["cli:codex".to_string()]),
            (
                newer,
                "fresh0002".to_string(),
                vec!["cli:claude".to_string()],
            ),
        ];
        let (id, providers) = most_recent_fleet(candidates).expect("a candidate exists");
        assert_eq!(
            id, "fresh0002",
            "the actually-latest run must win, not the first listed"
        );
        assert_eq!(providers, vec!["cli:claude".to_string()]);

        assert!(most_recent_fleet(Vec::new()).is_none());
    }

    /// Round-7 review finding (review-1-cli-claude, minor): a scoped Home target
    /// that has not yet reached the registry (e.g. a project just entered, one
    /// refresh behind `registry::ensure_known`) used to be absent from
    /// `nr.projects`, so `←`/`→` fell back to `unwrap_or(0)` and silently jumped
    /// to an unrelated registered project with no way back to the original scope.
    #[test]
    fn open_new_run_keeps_an_unregistered_scope_target_reachable() {
        let root = PathBuf::from("/nonexistent/unregistered");
        let mut app = App::new(None, Config::default(), Some(root.as_path()));
        assert_eq!(app.home_scope, HomeScope::Project(root.clone()));

        let other = project_at(&PathBuf::from("/nonexistent/acme-api"), "acme-api");
        open_new_run(
            &mut app,
            std::slice::from_ref(&other),
            &[],
            Some(root.as_path()),
            None,
        );
        let nr = app.new_run.as_ref().unwrap();
        assert_eq!(nr.project.as_deref(), Some(root.as_path()));
        assert!(
            nr.projects.contains(&root),
            "the scoped target must be cycleable even if the registry hasn't caught \
             up yet: {:?}",
            nr.projects
        );

        // Cycling all the way around must return to the original target, not strand
        // it: this is what the `unwrap_or(0)` fallback used to break.
        let n = app.new_run.as_ref().unwrap().projects.len();
        app.new_run.as_mut().unwrap().field = NewRunField::Project;
        for _ in 0..n {
            handle_new_run_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        }
        assert_eq!(
            app.new_run.as_ref().unwrap().project.as_deref(),
            Some(root.as_path()),
            "a full cycle must land back on the original scope target"
        );
    }

    /// Review finding (review-0-cli-codex, major): a scoped Home used to offer
    /// every registered project to cycle through even though it is scoped to
    /// one, so `→` could silently launch the plan against a project the
    /// operator was not looking at. With two other registered projects in the
    /// registry, the scoped target must be the *only* entry in `nr.projects`.
    #[test]
    fn open_new_run_cannot_cycle_out_of_a_scoped_home() {
        let root = PathBuf::from("/nonexistent/spar");
        let mut app = App::new(None, Config::default(), Some(root.as_path()));
        assert_eq!(app.home_scope, HomeScope::Project(root.clone()));

        let projects = [
            project_at(&root, "spar"),
            project_at(&PathBuf::from("/nonexistent/acme-api"), "acme-api"),
            project_at(&PathBuf::from("/nonexistent/other"), "other"),
        ];
        open_new_run(&mut app, &projects, &[], Some(root.as_path()), None);
        let nr = app.new_run.as_ref().unwrap();
        assert_eq!(
            nr.projects,
            vec![root.clone()],
            "a scoped Home must not offer projects outside its scope: {:?}",
            nr.projects
        );

        app.new_run.as_mut().unwrap().field = NewRunField::Project;
        handle_new_run_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(
            app.new_run.as_ref().unwrap().project.as_deref(),
            Some(root.as_path()),
            "→ must not be able to leave the scoped target"
        );
    }

    /// Round-9 review (review-0-cli-codex, major): `:plan <task>` with no run
    /// selected used to target `swarm.project_root` (`active_root`, stale while
    /// browsing Home) instead of the highlighted Home row. In cross-project Home with
    /// project A first in the registry and the cursor on a run in project B, the
    /// palette must open the new-run surface targeting B — the same target `n` (via
    /// `open_new_run`) would pick — not silently fall back to A.
    #[test]
    fn plan_palette_fallback_targets_the_selected_home_row_not_active_root() {
        let a = PathBuf::from("/nonexistent/acme-api");
        let b = PathBuf::from("/nonexistent/spar");
        let projects = [project_at(&a, "acme-api"), project_at(&b, "spar")];
        let run_b = run_in("bbbb0001", Phase::Review, 5, &b);
        let home_rows = vec![
            HomeRow::Header(HomeBand::Running),
            HomeRow::Run {
                band: HomeBand::Running,
                run: run_b,
                waited: Duration::ZERO,
            },
        ];
        let mut app = App::new(None, Config::default(), None);
        app.home_scope = HomeScope::All;
        app.selected_home = 1; // the row in project B

        // `swarm` stands for `active_root` — deliberately project A, the way it lags
        // behind while the operator is looking at Home rather than a project.
        let swarm = SparPaths::new(&a);
        let runs: Vec<state::RunSummary> = Vec::new();
        run_palette(
            &mut app,
            &swarm,
            &projects,
            &home_rows,
            None,
            &runs,
            None,
            swarm.project_root.as_path(),
            "plan do the thing",
        )
        .unwrap();
        assert_eq!(
            app.new_run.as_ref().unwrap().project.as_deref(),
            Some(b.as_path()),
            "the fallback must target the selected Home row's project, not active_root"
        );
    }

    /// Review 3be317b2 (major): `:plan <task>` at `BrowseLevel::Projects` had the
    /// identical bug `n` was fixed for — it read `swarm.project_root`, which the run
    /// loop only refreshes in the pre-input clamp, so a queued `j` (or a click)
    /// followed by `:plan` in the same input burst targeted the previously
    /// highlighted project. The fix reads `projects[app.selected_project]` directly,
    /// same as `n`.
    #[test]
    fn plan_palette_at_projects_targets_the_row_selected_in_the_same_input_burst() {
        let a = PathBuf::from("/nonexistent/alpha");
        let b = PathBuf::from("/nonexistent/bravo");
        let projects = [project_at(&a, "alpha"), project_at(&b, "bravo")];
        let mut app = App::new(None, Config::default(), None);
        app.open_projects_view();
        app.selected_project = 1; // `j` already moved the highlight to bravo

        // Stale on purpose: this is what the pre-input clamp left behind for `alpha`,
        // exactly as `run_loop` would hand it to `run_palette` mid-burst.
        let swarm = SparPaths::new(&a);
        let runs: Vec<state::RunSummary> = Vec::new();
        run_palette(
            &mut app,
            &swarm,
            &projects,
            &[],
            None,
            &runs,
            None,
            swarm.project_root.as_path(),
            "plan do the thing",
        )
        .unwrap();
        assert_eq!(
            app.new_run.as_ref().unwrap().project.as_deref(),
            Some(b.as_path()),
            "the modal must target the row `j` just selected, not the stale swarm.project_root"
        );
    }

    /// Round-11 review (review-1-cli-claude, major): `n` used to target the wrong
    /// project whenever the operator had drilled into a project other than their
    /// Home scope, with no way to correct it. `spar` inside repo A scopes Home to A;
    /// `Enter` on a Home run row in project B sets `active_root = B` and opens
    /// `Agents`, at which point `home_rows` is empty (only populated at Home/Projects)
    /// and `home_scope` still reads `Project(A)`. `n` pressed there must target B, the
    /// project actually being browsed, not the stale scope.
    #[test]
    fn open_new_run_targets_the_browsed_project_not_a_stale_home_scope() {
        let a = PathBuf::from("/nonexistent/repo-a");
        let b = PathBuf::from("/nonexistent/repo-b");
        let mut app = App::new(None, Config::default(), Some(a.as_path()));
        assert_eq!(app.home_scope, HomeScope::Project(a.clone()));
        app.browse = BrowseLevel::Agents;

        let projects = [project_at(&a, "repo-a"), project_at(&b, "repo-b")];
        open_new_run(&mut app, &projects, &[], None, Some(b.as_path()));
        let nr = app.new_run.as_ref().unwrap();
        assert_eq!(
            nr.project.as_deref(),
            Some(b.as_path()),
            "n must target the project actually being browsed, not the Home scope"
        );
        assert_eq!(
            nr.projects,
            vec![b.clone()],
            "browsing inside a project is scoped exactly like a scoped Home: no \
             cycling to an unrelated project: {:?}",
            nr.projects
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

    /// Round-9 review (minor): dedup used to key on the exact ref string, so a
    /// detected `cli:claude` row and a configured `cli:claude@opus` row picked
    /// together dispatched the same CLI twice. Dedup keys on `storage_key()` (X8,
    /// model-free) instead, keeping the first-picked ref's `@model` pin.
    #[test]
    fn new_run_providers_dedupes_a_pinned_ref_against_its_bare_form() {
        let mut nr = new_run_fixture();
        nr.roster = vec![
            RosterEntry {
                choice: RosterChoice::Provider("cli:claude".to_string()),
                label: "cli:claude".to_string(),
                available: true,
                reason: None,
                source: RosterSource::Detected,
            },
            RosterEntry {
                choice: RosterChoice::Provider("cli:claude@opus".to_string()),
                label: "cli:claude@opus".to_string(),
                available: true,
                reason: None,
                source: RosterSource::Configured,
            },
        ];
        nr.picked = vec![1, 0]; // the pinned ref picked first
        assert_eq!(
            new_run_providers(&nr),
            vec!["cli:claude@opus".to_string()],
            "the same CLI picked twice (pinned and bare) must dispatch once"
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
        nr.workflow = Some(crate::runspec::SpecWorkflow::Plan);
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
        // whatever the active root happens to be, and the argv uses role pins.
        let mut nr = new_run_fixture();
        nr.workflow = Some(crate::runspec::SpecWorkflow::Plan);
        nr.roles = vec![
            crate::runspec::RoleAssignment {
                role: crate::state::SlotRole::Planner,
                ordinal: 0,
                primary: Some(crate::runspec::Pin::parse("cli:claude@opus").unwrap()),
                backup: None,
            },
            crate::runspec::RoleAssignment {
                role: crate::state::SlotRole::PlanCritic,
                ordinal: 0,
                primary: Some(crate::runspec::Pin::parse("cli:codex@gpt-5.6-terra").unwrap()),
                backup: None,
            },
            crate::runspec::RoleAssignment {
                role: crate::state::SlotRole::TestAuthor,
                ordinal: 0,
                primary: Some(crate::runspec::Pin::parse("cli:grok@fast").unwrap()),
                backup: None,
            },
        ];
        nr.roster = vec![
            RosterEntry {
                choice: RosterChoice::Provider("cli:claude@opus".into()),
                label: "cli:claude@opus".into(),
                available: true,
                reason: None,
                source: RosterSource::Configured,
            },
            RosterEntry {
                choice: RosterChoice::Provider("cli:codex@gpt-5.6-terra".into()),
                label: "cli:codex@gpt-5.6-terra".into(),
                available: true,
                reason: None,
                source: RosterSource::Detected,
            },
            RosterEntry {
                choice: RosterChoice::Provider("cli:grok@fast".into()),
                label: "cli:grok@fast".into(),
                available: true,
                reason: None,
                source: RosterSource::Detected,
            },
        ];
        nr.picked = vec![0, 1, 2];
        let (target, argv) = new_run_launch(&nr).expect("a valid surface launches");
        assert_eq!(target, PathBuf::from("/nonexistent/spar"));
        assert_eq!(argv[0], "plan");
        let t = argv.iter().position(|a| a == "-t").expect("-t");
        assert_eq!(argv[t + 1], nr.task);
        assert!(
            argv.contains(&"--role".to_string()),
            "plan workflow should emit --role pins, not --providers"
        );
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

    /// AC-12 creation row: Home's first selectable row is Start Something New and
    /// default Home selection lands there, not on a header. Neutralizing the
    /// row order (e.g. pushing NewRun after NeedsMe) or making resync prefer
    /// headers would leave selected_home on an unselectable index.
    #[test]
    fn home_creation_row_is_first_selectable_and_default_selection_lands_there() {
        let root = PathBuf::from("/tmp/proj");
        let projects = vec![project_at(&root, "proj")];
        let folded: Vec<Vec<state::RunSummary>> = vec![vec![]];
        let now = Utc::now();
        let watermark = now - chrono::Duration::hours(24);
        let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now, false);
        assert!(
            rows.len() >= 2,
            "Home must have at least StartNew header and NewRun"
        );
        assert!(matches!(rows[0], HomeRow::Header(HomeBand::StartNew)));
        assert!(matches!(rows[1], HomeRow::NewRun));
        let is_selectable = |r: &HomeRow| {
            !matches!(
                r,
                HomeRow::Header(_)
                    | HomeRow::More { .. }
                    | HomeRow::Empty(_)
                    | HomeRow::Skeleton { .. }
            )
        };
        let first_selectable = rows.iter().position(is_selectable).unwrap();
        assert_eq!(
            first_selectable, 1,
            "first selectable row must be NewRun, not a header"
        );
        let mut app = App::new(None, Config::default(), Some(root.as_path()));
        app.browse = BrowseLevel::Home;
        app.selected_home = 0;
        app.home_key = None;
        resync_home_selection(&mut app, &rows);
        assert_eq!(
            app.selected_home, 1,
            "default Home selection must land on NewRun, not header {}",
            app.selected_home
        );
        assert!(matches!(rows[app.selected_home], HomeRow::NewRun));
        let neutralized_first = rows.iter().position(is_selectable).unwrap_or(0);
        assert_ne!(
            neutralized_first, 0,
            "neutralized row order would put a header first and this tautology would pass"
        );
    }

    /// AC-12 over-cap: NeedsMe is uncapped while Running is capped, but the
    /// header count and the roll-up must agree past HOME_BAND_CAP. A truncation
    /// that discards the More.n or caps NeedsMe would undercount.
    #[test]
    fn home_needs_you_is_uncapped_while_band_counts_agree_past_cap() {
        let root = PathBuf::from("/tmp/proj");
        let projects = vec![project_at(&root, "proj")];
        let now = Utc::now();
        let watermark = now - chrono::Duration::hours(24);
        let total = HOME_BAND_CAP + 17;
        let runs: Vec<state::RunSummary> = (0..total)
            .map(|i| {
                run_in(
                    &format!("gate-{i:03}"),
                    Phase::AwaitingPlanApproval,
                    1,
                    &root,
                )
            })
            .collect();
        let folded = vec![runs.clone()];
        let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now, false);
        let rollup = home_needs_you(&rows);
        assert_eq!(
            rollup, total,
            "roll-up must be uncapped past HOME_BAND_CAP: {rollup} vs {total}"
        );
        let band = home_band_count(&rows, HomeBand::NeedsMe);
        assert_eq!(
            band, total,
            "NeedsMe band count must include More.n and equal roll-up past cap: {band} vs {total}"
        );
        let running: Vec<state::RunSummary> = (0..(HOME_BAND_CAP + 10))
            .map(|i| run_in(&format!("run-{i:03}"), Phase::Review, 1, &root))
            .collect();
        let folded2 = vec![running];
        let rows2 = build_home_rows(&projects, &folded2, &HomeScope::All, watermark, now, false);
        let running_band = home_band_count(&rows2, HomeBand::Running);
        assert_eq!(running_band, HOME_BAND_CAP + 10);
        let running_rows = rows2
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    HomeRow::Run {
                        band: HomeBand::Running,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            running_rows, HOME_BAND_CAP,
            "Running rows must be capped at HOME_BAND_CAP"
        );
        assert_ne!(
            rollup, 0,
            "tautology guard: roll-up must never be zero while waiting"
        );
    }

    /// AC-12 rail/roll-up agreement: the roll-up, the band header count, the
    /// per-project rail flag, and toast all share one uncapped NeedsMe count
    /// and never read zero while something waits. A roll-up disagreeing with
    /// the bands is the U28 bug.
    #[test]
    fn home_rail_flag_and_rollup_share_one_uncapped_count_and_never_zero_while_waiting() {
        let root = PathBuf::from("/tmp/proj");
        let projects = vec![project_at(&root, "proj")];
        let now = Utc::now();
        let watermark = now - chrono::Duration::hours(24);
        let gate = run_in("gate-001", Phase::AwaitingPlanApproval, 5, &root);
        let broken = run_in("broken-001", Phase::Failed, 4, &root);
        let running = run_in("run-001", Phase::Review, 3, &root);
        let folded = vec![vec![gate.clone(), broken.clone(), running.clone()]];
        let rows = build_home_rows(&projects, &folded, &HomeScope::All, watermark, now, false);
        let rollup = home_needs_you(&rows);
        assert_eq!(rollup, 2, "gate(1) + broken(1) = 2, running not counted");
        assert_ne!(
            rollup, 0,
            "count must never read zero while something waits"
        );
        let band = home_band_count(&rows, HomeBand::NeedsMe);
        assert_eq!(
            band, rollup,
            "roll-up must agree with band header count; disagreement is the U28 bug"
        );
        let attention = runs_needing_attention(&[gate, broken, running]);
        assert_eq!(
            attention, 2,
            "runs_needing_attention counts folded units, not legs, but must be non-zero while waiting"
        );
        assert!(
            attention > 0,
            "rail flag must be set while a gate or broken run waits"
        );
        let empty_rows =
            build_home_rows(&projects, &[vec![]], &HomeScope::All, watermark, now, false);
        assert_eq!(
            home_needs_you(&empty_rows),
            0,
            "empty Home must read zero, but non-empty must not"
        );
    }

    /// AC-12 Gate and Broken toasts are load-bearing, not decoration: a
    /// transition into either must emit a toast, and each must be impossible
    /// to miss. Neutralizing either branch (e.g. `if attention == Gate` only)
    /// must make one of the two assertions fail.
    #[test]
    fn gate_and_broken_transitions_each_toast() {
        fn summary_phase_local(id: &str, phase: Phase) -> state::RunSummary {
            state::RunSummary {
                id: id.into(),
                workflow: crate::cli::WorkflowKind::Loop,
                archived: false,
                phase,
                updated_at: Utc::now(),
                task: Some(format!("brief for {id}")),
                dry_run: false,
                abandoned: false,
                parent_run: None,
                round: 1,
                legs: 1,
                wants: 0,
                unit_id: None,
                base_ref: None,
                base_commit: None,
                project_root: None,
                project_name: None,
            }
        }
        let mut app = test_app();
        let gate = summary_phase_local("r-gate", Phase::AwaitingPlanApproval);
        let broken = summary_phase_local("r-broken", Phase::Failed);
        let idle = summary_phase_local("r-idle", Phase::Review);

        emit_attention_toasts(&mut app, &[], false);
        app.flash = None;
        emit_attention_toasts(&mut app, std::slice::from_ref(&idle), false);
        assert!(app.flash.is_none(), "idle -> idle must not toast");

        let mut app_gate = test_app();
        emit_attention_toasts(&mut app_gate, &[], false);
        app_gate.flash = None;
        emit_attention_toasts(&mut app_gate, std::slice::from_ref(&gate), false);
        assert!(app_gate.flash.is_some(), "transition into Gate must toast");
        let gate_msg = app_gate.flash.clone().unwrap().1.clone();

        let mut app_broken = test_app();
        emit_attention_toasts(&mut app_broken, &[], false);
        app_broken.flash = None;
        emit_attention_toasts(&mut app_broken, std::slice::from_ref(&broken), false);
        assert!(
            app_broken.flash.is_some(),
            "transition into Broken must toast (not only Gate)"
        );
        let broken_msg = app_broken.flash.clone().unwrap().1.clone();
        assert_ne!(
            gate_msg, broken_msg,
            "Gate and Broken toasts must be distinct"
        );

        let mut app_stays_gate = test_app();
        emit_attention_toasts(&mut app_stays_gate, std::slice::from_ref(&gate), false);
        app_stays_gate.flash = None;
        emit_attention_toasts(&mut app_stays_gate, std::slice::from_ref(&gate), false);
        assert!(
            app_stays_gate.flash.is_none(),
            "staying in Gate must not re-toast"
        );
    }
}

#[cfg(test)]
mod chat_acceptance {
    use super::*;
    use crate::paths::SparPaths;
    use tempfile::tempdir;

    #[test]
    fn chat_tab_is_available_at_home_and_all_run_browse_levels() {
        assert_eq!(MAIN_TABS.len(), 7);
        assert!(MAIN_TABS.contains(&MainTab::Chat));
        assert_eq!(MAIN_TABS.last(), Some(&MainTab::Shell));
        assert_eq!(HOME_TABS.len(), 3);
        assert!(HOME_TABS.contains(&MainTab::Chat));
        assert_eq!(HOME_TABS.last(), Some(&MainTab::Shell));
        for browse in [
            BrowseLevel::Home,
            BrowseLevel::Projects,
            BrowseLevel::Runs,
            BrowseLevel::Agents,
        ] {
            let tabs = tabs_for(browse);
            assert!(
                tabs.contains(&MainTab::Chat),
                "Chat must be in tabs_for({browse:?})"
            );
            assert_eq!(
                tabs.last(),
                Some(&MainTab::Shell),
                "Shell must be last in {browse:?}"
            );
        }
        assert_eq!(MainTab::Chat.short_label(), "C");
    }

    #[test]
    fn home_n_opens_chat_without_launching_and_plan_palette_stays_manual() {
        let mut app = App::new(None, Config::default(), Some(Path::new("/x")));
        app.browse = BrowseLevel::Home;
        let sw = SparPaths::new(Path::new("/x"));
        let projects: Vec<registry::ProjectEntry> = vec![];
        let mut root = PathBuf::from("/x");
        handle_key(
            &mut app,
            KeyCode::Char('n'),
            KeyModifiers::empty(),
            &sw,
            &projects,
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert_eq!(app.main_tab, MainTab::Chat);
        assert_eq!(app.focus, Focus::Main);
        assert!(app.chat_composing);
        assert!(app.new_run.is_none(), "n must not open NewRun");
        assert!(app.chat_conversations.contains_key("home"));

        let mut app2 = test_app();
        let sw2 = SparPaths::new(Path::new("/x"));
        app2.palette = Some(Palette {
            input: "plan do the thing".into(),
            sel: 0,
        });
        let quit = handle_palette_key(
            &mut app2,
            KeyCode::Enter,
            KeyModifiers::empty(),
            &sw2,
            &[],
            &[],
            None,
            &[],
            None,
            Path::new("/x"),
        )
        .unwrap();
        assert!(!quit);
        assert!(
            app2.new_run.is_some(),
            ":plan <task> must open manual NewRun picker"
        );
    }

    #[test]
    fn chat_transcript_filters_scope_and_keeps_event_record_identity() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let home_conv = "talk-home1";
        let run_conv = "talk-run1";
        crate::orchestrator::say(
            &paths,
            &crate::orchestrator::Scope::Home,
            home_conv,
            "home hi",
        )
        .unwrap();
        crate::orchestrator::say(
            &paths,
            &crate::orchestrator::Scope::Run("r1".into()),
            run_conv,
            "run hi",
        )
        .unwrap();
        let home_records = crate::orchestrator::transcript(
            &paths,
            &crate::orchestrator::Scope::Home,
            Some(home_conv),
        )
        .unwrap();
        assert!(
            home_records.iter().any(|r| r.summary.contains("home hi")),
            "home transcript must contain home message"
        );
        assert!(
            !home_records.iter().any(|r| r.summary.contains("run hi")),
            "home transcript must not leak run scope"
        );
        let run_records = crate::orchestrator::transcript(
            &paths,
            &crate::orchestrator::Scope::Run("r1".into()),
            Some(run_conv),
        )
        .unwrap();
        assert!(run_records.iter().any(|r| r.summary.contains("run hi")));
        assert!(!run_records.iter().any(|r| r.summary.contains("home hi")));
        for r in &home_records {
            match r.source {
                crate::record::SourceId::Activity { .. } => {}
                _ => panic!("transcript records must be Activity with event identity"),
            }
        }
        let mut app = App::new(None, Config::default(), Some(Path::new("/x")));
        app.main_tab = MainTab::Chat;
        app.chat_follow = true;
        app.chat_scroll = 5;
        app.scroll_chat_by(0);
        assert!(app.chat_follow);
        app.scroll_chat_by(1);
    }

    #[test]
    fn chat_composer_captures_global_keys_and_rejects_empty_submit() {
        let prev = std::env::var("SPAR_DRY_RUN").ok();
        std::env::set_var("SPAR_DRY_RUN", "1");
        let mut app = App::new(None, Config::default(), Some(Path::new("/x")));
        app.browse = BrowseLevel::Runs;
        app.main_tab = MainTab::Chat;
        app.focus = Focus::Rail;
        let sw = SparPaths::new(Path::new("/x"));
        let mut root = PathBuf::from("/x");
        handle_key(
            &mut app,
            KeyCode::Char('j'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(
            !app.chat_composing,
            "Rail focused + Chat tab must not start composing on j"
        );
        app.focus = Focus::Main;
        app.main_tab = MainTab::Chat;
        app.chat_composing = false;
        handle_key(
            &mut app,
            KeyCode::Char(']'),
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(!app.chat_composing, "] must cycle tab, not start composing");
        assert_eq!(app.main_tab, MainTab::Chat.next_in(tabs_for(app.browse)));
        app.main_tab = MainTab::Chat;
        app.chat_composing = true;
        app.chat_input = "hello".into();
        handle_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(app.chat_input.is_empty(), "Enter must clear after submit");
        app.chat_composing = true;
        app.chat_input.clear();
        handle_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(app.chat_composing, "empty Enter must not exit composing");
        app.chat_composing = true;
        app.chat_input = "hi".into();
        handle_key(
            &mut app,
            KeyCode::Esc,
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(!app.chat_composing);
        assert!(app.chat_input.is_empty());
        app.chat_composing = true;
        app.chat_input.clear();
        handle_key(
            &mut app,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(
            app.chat_input.is_empty(),
            "Ctrl+C must not be captured as literal"
        );
        if let Some(v) = prev {
            std::env::set_var("SPAR_DRY_RUN", v);
        } else {
            std::env::remove_var("SPAR_DRY_RUN");
        }
    }

    #[test]
    fn home_proposal_prefills_picker_but_never_launches_or_drops_roster_entries() {
        let tmp = tempdir().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .output()
            .unwrap();
        let paths = SparPaths::new(&project);
        let conv = format!("talk-{}", crate::bus::new_id());
        let proposal_body = "intro\n```spar-proposal\ntask = \"do thing\"\nbrief = \"detailed brief\"\nproviders = [\"cli:claude\", \"cli:unknown\"]\n```\noutro";
        let mut meta = std::collections::HashMap::new();
        meta.insert("surface".into(), "chat".into());
        meta.insert("conversation".into(), conv.clone());
        meta.insert("turn".into(), "t1".into());
        let agent = crate::bus::agent_ref(None, &conv);
        crate::bus::send(
            &paths,
            crate::bus::BusMessage {
                id: crate::bus::new_id(),
                ts: chrono::Utc::now(),
                from: agent.clone(),
                to: crate::bus::HUMAN.into(),
                kind: crate::bus::MsgKind::Chat,
                body: proposal_body.into(),
                run: None,
                subject: None,
                refs: crate::bus::MsgRefs::default(),
                requires_ack: false,
                meta,
            },
            crate::bus::MessageBudget::Chatty,
        )
        .unwrap();
        let proposal = crate::orchestrator::parse_proposal(proposal_body)
            .unwrap()
            .unwrap();
        assert_eq!(proposal.task, "do thing");
        let mut app = App::new(None, Config::default(), Some(project.as_path()));
        app.browse = BrowseLevel::Home;
        app.main_tab = MainTab::Chat;
        app.focus = Focus::Main;
        app.chat_conversations.insert("home".into(), conv.clone());
        let sw = SparPaths::new(&project);
        let projects = vec![registry::ProjectEntry {
            root: project.clone(),
            name: Some("proj".into()),
            last_seen: chrono::Utc::now(),
            last_run_id: None,
        }];
        handle_key(
            &mut app,
            KeyCode::Char('o'),
            KeyModifiers::empty(),
            &sw,
            &projects,
            &[],
            &[],
            None,
            &[],
            &mut project.clone(),
            None,
        )
        .unwrap();
        assert!(
            app.new_run.is_some(),
            "o at Home with proposal must open picker"
        );
        if let Some(nr) = &app.new_run {
            assert!(
                nr.roster.iter().any(|e| match &e.choice {
                    RosterChoice::Provider(p) => p == "cli:unknown",
                    _ => false,
                }),
                "unknown provider must be visible in roster"
            );
            let entry = nr
                .roster
                .iter()
                .find(|e| matches!(&e.choice, RosterChoice::Provider(p) if p=="cli:unknown"))
                .unwrap();
            assert!(
                !entry.available,
                "unknown provider must be visible but unavailable"
            );
            assert!(
                !nr.picked.contains(
                    &nr.roster
                        .iter()
                        .position(
                            |e| matches!(&e.choice, RosterChoice::Provider(p) if p=="cli:unknown")
                        )
                        .unwrap()
                ),
                "unknown provider must not be auto-picked"
            );
            assert_eq!(nr.task, "do thing");
        } else {
            panic!("new_run missing after Home o");
        }
        // Run-scoped gate reply cannot launch: even with a valid proposal for that run's conversation, `o` on a non-Home browse must not open the picker.
        // Seed a run-scoped proposal.
        let run_conv = "talk-r1-proposal".to_string();
        let run_proposal_body = "intro\n```spar-proposal\ntask = \"run task\"\nbrief = \"run brief\"\nproviders = [\"cli:claude\"]\n```\noutro";
        let mut run_meta = std::collections::HashMap::new();
        run_meta.insert("surface".into(), "chat".into());
        run_meta.insert("conversation".into(), run_conv.clone());
        run_meta.insert("turn".into(), "t2".into());
        let run_agent = crate::bus::agent_ref(Some("r1"), &run_conv);
        crate::bus::send(
            &paths,
            crate::bus::BusMessage {
                id: crate::bus::new_id(),
                ts: chrono::Utc::now(),
                from: run_agent,
                to: crate::bus::HUMAN.into(),
                kind: crate::bus::MsgKind::Chat,
                body: run_proposal_body.into(),
                run: Some("r1".into()),
                subject: None,
                refs: crate::bus::MsgRefs::default(),
                requires_ack: false,
                meta: run_meta,
            },
            crate::bus::MessageBudget::Chatty,
        )
        .unwrap();
        let mut app2 = App::new(None, Config::default(), Some(project.as_path()));
        app2.browse = BrowseLevel::Runs;
        app2.main_tab = MainTab::Chat;
        app2.focus = Focus::Main;
        app2.selected_run = 0;
        app2.chat_conversations
            .insert("r1".into(), run_conv.clone());
        let run_summary = crate::state::RunSummary {
            id: "r1".into(),
            workflow: crate::cli::WorkflowKind::Loop,
            phase: crate::state::Phase::AwaitingPlanApproval,
            updated_at: chrono::Utc::now(),
            task: Some("run task".into()),
            dry_run: false,
            abandoned: false,
            archived: false,
            parent_run: None,
            round: 1,
            legs: 1,
            wants: 0,
            unit_id: None,
            base_ref: None,
            base_commit: None,
            project_root: Some(project.clone()),
            project_name: Some("proj".into()),
        };
        let runs = vec![run_summary];
        let sw2 = SparPaths::new(&project);
        handle_key(
            &mut app2,
            KeyCode::Char('o'),
            KeyModifiers::empty(),
            &sw2,
            &projects,
            &[],
            &runs,
            None,
            &[],
            &mut project.clone(),
            None,
        )
        .unwrap();
        assert!(app2.new_run.is_none(), "run-scoped o must not launch");
    }

    #[test]
    fn chat_pending_proposal_launch_uses_verbatim_brief_once() {
        let prev_dry = std::env::var("SPAR_DRY_RUN").ok();
        std::env::set_var("SPAR_DRY_RUN", "1");
        let tmp = tempdir().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .output()
            .unwrap();
        let proposal = crate::orchestrator::Proposal {
            task: "orig task".into(),
            brief: "# Orig Title\n\nThis is the original brief with orig task inside.\n".into(),
            providers: vec!["cli:claude".into()],
            ..Default::default()
        };
        let edited_task = "edited task";
        let mut app = App::new(None, Config::default(), Some(project.as_path()));
        app.browse = BrowseLevel::Home;
        app.main_tab = MainTab::Chat;
        app.focus = Focus::Main;
        app.chat_pending_proposal = Some(proposal.clone());
        app.chat_pending_brief_path = None;
        // Prepare a NewRun with edited task, as if operator edited the picker.
        let mut nr = pending_new_run(
            Some(project.clone()),
            vec![project.clone()],
            edited_task.into(),
            NewRunField::Task,
            1,
        );
        // Simulate roster ready with cli:claude available.
        nr.roster = vec![RosterEntry {
            choice: RosterChoice::Provider("cli:claude".into()),
            label: "cli:claude".into(),
            available: true,
            reason: None,
            source: RosterSource::Detected,
        }];
        nr.picked = vec![0];
        nr.loading = false;
        app.new_run = Some(nr);
        // Launch via Enter (which should intake verbatim brief and not rewrite with edited task).
        handle_new_run_key(&mut app, KeyCode::Enter, KeyModifiers::empty());
        // Brief must be verbatim, not containing edited task's injected title.
        let briefs: Vec<_> = std::fs::read_dir(project.join(".spar/briefs"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            briefs.len(),
            1,
            "must create exactly one brief, not a duplicate, got {briefs:?}"
        );
        let brief_path = briefs[0].path();
        let body = std::fs::read_to_string(&brief_path).unwrap();
        assert_eq!(
            body, proposal.brief,
            "brief must be byte-identical to proposal.brief, not edited"
        );
        assert!(
            !body.contains(edited_task) || proposal.brief.contains(edited_task),
            "brief must not be rewritten with edited task"
        );
        // Verify the argv would have been `plan --brief <path> --providers cli:claude`
        // by checking the brief path is used and task is not injected.
        // The brief path should be the one we created.
        assert!(
            brief_path.to_string_lossy().contains("orig-title")
                || brief_path.to_string_lossy().contains("brief"),
            "brief path should be derived from proposal.brief, got {brief_path:?}"
        );
        // Simulate a failed detached launch that kept pending_brief_path: second Enter with same
        // proposal but edited task should reuse the same brief path, not create a second file.
        // Re-open the modal with same proposal and pending path.
        app.chat_pending_proposal = Some(proposal.clone());
        // Keep the pending path from previous launch (simulate failure keeping it).
        // The previous launch cleared it on success, so we restore it to test reuse.
        app.chat_pending_brief_path = Some(brief_path.clone());
        let mut nr2 = pending_new_run(
            Some(project.clone()),
            vec![project.clone()],
            "another edit".into(),
            NewRunField::Task,
            2,
        );
        nr2.roster = vec![RosterEntry {
            choice: RosterChoice::Provider("cli:claude".into()),
            label: "cli:claude".into(),
            available: true,
            reason: None,
            source: RosterSource::Detected,
        }];
        nr2.picked = vec![0];
        nr2.loading = false;
        app.new_run = Some(nr2);
        handle_new_run_key(&mut app, KeyCode::Enter, KeyModifiers::empty());
        let briefs2: Vec<_> = std::fs::read_dir(project.join(".spar/briefs"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            briefs2.len(),
            1,
            "retry must not create a second brief file (AC-9), got {briefs2:?}"
        );
        if let Some(v) = prev_dry {
            std::env::set_var("SPAR_DRY_RUN", v);
        } else {
            std::env::remove_var("SPAR_DRY_RUN");
        }
    }

    #[test]
    fn chat_g_clears_follow_and_top_is_reachable() {
        let mut app = App::new(None, Config::default(), Some(Path::new("/x")));
        app.main_tab = MainTab::Chat;
        app.chat_follow = true;
        app.chat_scroll = 5;
        app.chat_max = 10;
        app.home_for_main(true, &[]);
        assert!(
            !app.chat_follow,
            "g (home_for_main) must clear chat_follow like every other followed view"
        );
        assert_eq!(app.chat_scroll, 0);
        app.chat_max = 10;
        app.chat_follow = true;
        app.chat_scroll = 5;
        let mut follow = app.chat_follow;
        let mut scroll = app.chat_scroll;
        clamp_scroll(&mut scroll, &mut follow, app.chat_max);
        assert_eq!(scroll, app.chat_max, "clamp enforces follow pin");
        app.home_for_main(true, &[]);
        let mut follow2 = app.chat_follow;
        let mut scroll2 = app.chat_scroll;
        clamp_scroll(&mut scroll2, &mut follow2, app.chat_max);
        assert_eq!(scroll2, 0, "after g, clamp must stay at top");
        assert!(!follow2);
    }

    #[test]
    fn mouse_dismiss_of_new_run_clears_pending_proposal_and_brief() {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .output()
            .unwrap();
        let mut app = App::new(None, Config::default(), Some(proj.as_path()));
        app.chat_pending_proposal = Some(crate::orchestrator::Proposal {
            task: "t".into(),
            brief: "b".into(),
            providers: vec!["cli:claude".into()],
            ..Default::default()
        });
        app.chat_pending_brief_path = Some(PathBuf::from("/tmp/brief.md"));
        let mut nr = pending_new_run(
            Some(proj.clone()),
            vec![proj.clone()],
            "t".into(),
            NewRunField::Task,
            1,
        );
        nr.roster = vec![RosterEntry {
            choice: RosterChoice::Provider("cli:claude".into()),
            label: "cli:claude".into(),
            available: true,
            reason: None,
            source: RosterSource::Detected,
        }];
        nr.loading = false;
        app.new_run = Some(nr);
        app.rect_new_run = ratatui::layout::Rect {
            x: 10,
            y: 10,
            width: 20,
            height: 10,
        };
        let m = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        handle_mouse_inner(
            &mut app,
            m,
            &SparPaths::new(&proj),
            &[],
            &[],
            &[],
            None,
            &[],
            &mut proj.clone(),
            None,
            0,
        );
        assert!(app.new_run.is_none());
        assert!(
            app.chat_pending_proposal.is_none(),
            "mouse dismiss must clear stale proposal"
        );
        assert!(
            app.chat_pending_brief_path.is_none(),
            "mouse dismiss must clear stale brief path"
        );
    }

    #[test]
    fn manual_plan_clears_stale_proposal() {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .output()
            .unwrap();
        let mut app = App::new(None, Config::default(), Some(proj.as_path()));
        app.chat_pending_proposal = Some(crate::orchestrator::Proposal {
            task: "old".into(),
            brief: "old brief".into(),
            providers: vec!["cli:claude".into()],
            ..Default::default()
        });
        app.chat_pending_brief_path = Some(PathBuf::from("/tmp/old.md"));
        let sw = SparPaths::new(&proj);
        app.palette = Some(Palette {
            input: "plan new task".into(),
            sel: 0,
        });
        let _ = handle_palette_key(
            &mut app,
            KeyCode::Enter,
            KeyModifiers::empty(),
            &sw,
            &[registry::ProjectEntry {
                root: proj.clone(),
                name: Some("proj".into()),
                last_seen: chrono::Utc::now(),
                last_run_id: None,
            }],
            &[],
            None,
            &[],
            None,
            &proj,
        );
        assert!(
            app.chat_pending_proposal.is_none(),
            ":plan must not retain stale proposal"
        );
        assert!(
            app.chat_pending_brief_path.is_none(),
            ":plan must not retain stale brief path"
        );
        assert!(app.new_run.is_some());
        assert_eq!(app.new_run.as_ref().unwrap().task, "new task");
    }

    #[test]
    fn new_chat_proposal_resets_pending_brief_path() {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .output()
            .unwrap();
        let paths = SparPaths::new(&proj);
        let conv = format!("talk-{}", crate::bus::new_id());
        let body_a = "```spar-proposal\ntask = \"a\"\nbrief = \"brief A\"\nproviders = [\"cli:claude\"]\n```";
        let body_b = "```spar-proposal\ntask = \"b\"\nbrief = \"brief B\"\nproviders = [\"cli:claude\"]\n```";
        let agent = crate::bus::agent_ref(None, &conv);
        for body in [body_a, body_b] {
            let mut meta = std::collections::HashMap::new();
            meta.insert("surface".into(), "chat".into());
            meta.insert("conversation".into(), conv.clone());
            meta.insert("turn".into(), crate::bus::new_id());
            crate::bus::send(
                &paths,
                crate::bus::BusMessage {
                    id: crate::bus::new_id(),
                    ts: chrono::Utc::now(),
                    from: agent.clone(),
                    to: crate::bus::HUMAN.into(),
                    kind: crate::bus::MsgKind::Chat,
                    body: body.into(),
                    run: None,
                    subject: None,
                    refs: crate::bus::MsgRefs::default(),
                    requires_ack: false,
                    meta,
                },
                crate::bus::MessageBudget::Chatty,
            )
            .unwrap();
        }
        let mut app = App::new(None, Config::default(), Some(proj.as_path()));
        app.browse = BrowseLevel::Home;
        app.main_tab = MainTab::Chat;
        app.focus = Focus::Main;
        app.chat_conversations.insert("home".into(), conv.clone());
        app.chat_pending_brief_path = Some(PathBuf::from("/tmp/stale.md"));
        let sw = SparPaths::new(&proj);
        let projects = vec![registry::ProjectEntry {
            root: proj.clone(),
            name: Some("proj".into()),
            last_seen: chrono::Utc::now(),
            last_run_id: None,
        }];
        let mut root = proj.clone();
        handle_key(
            &mut app,
            KeyCode::Char('o'),
            KeyModifiers::empty(),
            &sw,
            &projects,
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(
            app.chat_pending_brief_path.is_none(),
            "installing a new proposal must reset stale brief path"
        );
        assert!(app.chat_pending_proposal.is_some());
    }

    #[test]
    fn esc_cancels_in_flight_turn_even_when_not_composing() {
        let mut app = App::new(None, Config::default(), Some(Path::new("/x")));
        app.browse = BrowseLevel::Home;
        app.main_tab = MainTab::Chat;
        app.focus = Focus::Main;
        app.chat_composing = false;
        let h = std::sync::Arc::new(crate::orchestrator::TurnHandle::new(
            "home".into(),
            "talk-x".into(),
            "t1".into(),
        ));
        app.chat_active_turn = Some(h.clone());
        assert!(!h.is_cancelled());
        let sw = SparPaths::new(Path::new("/x"));
        let mut root = PathBuf::from("/x");
        handle_key(
            &mut app,
            KeyCode::Esc,
            KeyModifiers::empty(),
            &sw,
            &[],
            &[],
            &[],
            None,
            &[],
            &mut root,
            None,
        )
        .unwrap();
        assert!(
            h.is_cancelled(),
            "Esc must cancel in-flight turn without requiring composing"
        );
        assert!(
            app.chat_active_turn.is_none(),
            "cancelled turn handle must be taken"
        );
    }

    #[test]
    fn legacy_providers_are_visible_and_block_launch() {
        let tmp = tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .output()
            .unwrap();
        let cfg = Config::default();
        let mut nr = pending_new_run(
            Some(proj.clone()),
            vec![proj.clone()],
            "".into(),
            NewRunField::Task,
            99,
        );
        nr.roster = vec![
            RosterEntry {
                choice: RosterChoice::Provider("cli:claude".into()),
                label: "cli:claude".into(),
                available: true,
                reason: None,
                source: RosterSource::Detected,
            },
            RosterEntry {
                choice: RosterChoice::Provider("cli:grok".into()),
                label: "cli:grok".into(),
                available: false,
                reason: Some("not in roster".into()),
                source: RosterSource::Detected,
            },
        ];
        nr.workflow = None;
        let proposal = crate::orchestrator::Proposal {
            task: "legacy task".into(),
            brief: "brief".into(),
            providers: vec![
                "cli:claude@opus".into(),
                "cli:grok@fast".into(),
                "invalid-provider".into(),
            ],
            ..Default::default()
        };
        apply_proposal_to_roster(&proposal, &mut nr);
        assert_eq!(
            nr.legacy_providers.len(),
            3,
            "all three providers must be retained visibly, including invalid"
        );
        assert!(nr.legacy_providers.contains(&"cli:claude@opus".to_string()));
        assert!(nr.legacy_providers.contains(&"cli:grok@fast".to_string()));
        assert!(nr
            .legacy_providers
            .contains(&"invalid-provider".to_string()));
        let spec = new_run_spec(&nr, &cfg);
        assert_eq!(spec.legacy_providers.len(), 3);
        assert!(
            spec.validate_for_launch(&cfg).is_err(),
            "legacy must block launch"
        );
        // Also visible in roster: providers were added to roster
        assert!(nr.roster.iter().any(|r| r.label == "cli:claude@opus"));
        // Cleanup
        crate::defaults::set_test_home(None);
    }

    #[test]
    fn defaults_prefill_new_run_without_writing_project_toml() {
        let tmp_home = tempdir().unwrap();
        let home = tmp_home.path().join("spar-home");
        std::fs::create_dir_all(&home).unwrap();
        crate::defaults::set_test_home(Some(home.clone()));
        let proj_tmp = tempdir().unwrap();
        let proj = proj_tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&proj)
            .output()
            .unwrap();
        let before = std::fs::read_to_string(proj.join("spar.toml")).unwrap_or_default();
        let def_spec = crate::runspec::RunSpec {
            workflow: Some(crate::runspec::SpecWorkflow::Plan),
            task: "prefilled task".into(),
            roles: vec![crate::runspec::RoleAssignment {
                role: crate::state::SlotRole::Planner,
                ordinal: 0,
                primary: Some(crate::runspec::Pin::parse("cli:claude@opus").unwrap()),
                backup: Some(crate::runspec::Pin::parse("cli:grok@fast").unwrap()),
            }],
            ..Default::default()
        };
        crate::defaults::save(&def_spec).unwrap();
        let defaults_path = home.join("defaults.json");
        assert!(
            defaults_path.is_file(),
            "defaults must be at spar_home/defaults.json"
        );
        let nr = pending_new_run(
            Some(proj.clone()),
            vec![proj.clone()],
            "".into(),
            NewRunField::Task,
            100,
        );
        assert_eq!(nr.workflow, Some(crate::runspec::SpecWorkflow::Plan));
        assert!(nr
            .roles
            .iter()
            .any(|r| r.role == crate::state::SlotRole::Planner
                && r.primary.as_ref().map(|p| p.display()) == Some("cli:claude@opus".into())));
        let after = std::fs::read_to_string(proj.join("spar.toml")).unwrap_or_default();
        assert_eq!(
            before, after,
            "saving defaults must not touch project spar.toml"
        );
        crate::defaults::set_test_home(None);
    }
}
