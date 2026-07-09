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
    ScrollbarOrientation, ScrollbarState, Widget,
};
use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    Composer,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Runs => Focus::Agents,
            Focus::Agents => Focus::Log,
            Focus::Log => Focus::Activity,
            Focus::Activity => Focus::Composer,
            Focus::Composer => Focus::Runs,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::Runs => Focus::Composer,
            Focus::Agents => Focus::Runs,
            Focus::Log => Focus::Agents,
            Focus::Activity => Focus::Log,
            Focus::Composer => Focus::Activity,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Focus::Runs => "Runs",
            Focus::Agents => "Agents",
            Focus::Log => "Live log",
            Focus::Activity => "Activity",
            Focus::Composer => "Command",
        }
    }
}

pub struct TuiOpts {
    pub task_seed: Option<String>,
    pub cwd: Option<PathBuf>,
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
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.clear()?;

    let result = run_loop(
        &mut terminal,
        local_root,
        opts.task_seed,
        stall_warn_secs,
    );

    disable_raw_mode()?;
    let mut out = stdout();
    out.execute(DisableMouseCapture)?;
    out.execute(LeaveAlternateScreen)?;
    result
}

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
    rect_header: Rect,
    rect_action: Rect,
    rect_fleet: Rect,
    rect_stream: Rect,
    rect_bus: Rect,
    rect_composer: Rect,
    rect_runs: Rect,
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
            tick: 0,
            flash: None,
            stall_warn_secs,
            log_expand: false,
            last_ctrl_c: None,
            last_click: None,
            show_help: false,
            rect_header: Rect::default(),
            rect_action: Rect::default(),
            rect_fleet: Rect::default(),
            rect_stream: Rect::default(),
            rect_bus: Rect::default(),
            rect_composer: Rect::default(),
            rect_runs: Rect::default(),
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
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    fn select_run(&mut self, idx: usize, n: usize) {
        if n == 0 {
            return;
        }
        self.selected_run = idx.min(n - 1);
        self.selected_slot = 0;
        self.stream_scroll = 0;
        self.bus_scroll = 0;
    }

    fn select_project(&mut self, idx: usize, n: usize) {
        if n == 0 {
            return;
        }
        self.selected_project = idx.min(n - 1);
        self.selected_run = 0;
        self.selected_slot = 0;
        self.stream_scroll = 0;
        self.bus_scroll = 0;
    }

    fn select_slot(&mut self, idx: usize, n: usize) {
        if n == 0 {
            return;
        }
        self.selected_slot = idx.min(n - 1);
        self.stream_scroll = 0;
    }

    fn open_project_runs(&mut self) {
        self.browse = BrowseLevel::Runs;
        self.selected_run = 0;
        self.selected_slot = 0;
        self.stream_scroll = 0;
        self.focus = Focus::Runs;
    }

    fn open_projects_view(&mut self) {
        self.browse = BrowseLevel::Projects;
        self.selected_run = 0;
        self.selected_slot = 0;
        self.stream_scroll = 0;
        self.focus = Focus::Runs;
    }
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
        registry::Registry::load()
            .ok()
            .and_then(|r| r.projects.into_iter().next())
            .map(|p| p.root)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    });

    loop {
        app.tick = app.tick.wrapping_add(1);

        let projects = load_projects(local_root.as_deref());
        if !projects.is_empty() {
            app.selected_project = app.selected_project.min(projects.len() - 1);
            // When browsing projects, active_root tracks the highlighted project.
            if app.browse == BrowseLevel::Projects {
                active_root = projects[app.selected_project].root.clone();
            }
        } else {
            app.selected_project = 0;
        }

        let runs = if app.browse == BrowseLevel::Runs {
            registry::list_project_runs(&active_root).unwrap_or_default()
        } else {
            Vec::new()
        };
        if !runs.is_empty() {
            app.selected_run = app.selected_run.min(runs.len() - 1);
        } else {
            app.selected_run = 0;
        }

        let rail_sel = match app.browse {
            BrowseLevel::Projects => {
                if projects.is_empty() {
                    None
                } else {
                    Some(app.selected_project)
                }
            }
            BrowseLevel::Runs => {
                if runs.is_empty() {
                    None
                } else {
                    Some(app.selected_run)
                }
            }
        };
        rail_state.select(rail_sel);

        let swarm = SparPaths::new(&active_root);
        let selected_id = runs.get(app.selected_run).map(|r| r.id.clone());
        let full = if app.browse == BrowseLevel::Runs {
            selected_id
                .as_ref()
                .and_then(|id| RunState::load(&swarm, id).ok())
        } else {
            None
        };
        if let Some(ref st) = full {
            if !st.slots.is_empty() {
                app.selected_slot = app.selected_slot.min(st.slots.len() - 1);
            } else {
                app.selected_slot = 0;
            }
        } else {
            app.selected_slot = 0;
        }
        fleet_state.select(if full.as_ref().map(|s| s.slots.is_empty()).unwrap_or(true) {
            None
        } else {
            Some(app.selected_slot)
        });

        let quota = QuotaStore::load(&swarm).unwrap_or_default();
        let stream_text = match app.browse {
            BrowseLevel::Projects => project_overview(&projects, app.selected_project),
            BrowseLevel::Runs => stream_content(&swarm, full.as_ref(), app.selected_slot),
        };
        let activity = activity_feed(&swarm, full.as_ref(), &quota);

        if let Some((t, _, _, dur)) = &app.flash {
            if t.elapsed() > *dur {
                app.flash = None;
            }
        }

        terminal.draw(|f| {
            draw(
                f,
                &swarm,
                &projects,
                &runs,
                full.as_ref(),
                &stream_text,
                &activity,
                &app,
                &mut fleet_state,
                &mut rail_state,
            );
        })?;

        let area = terminal.size()?;
        let layout = layout_rects(Rect {
            x: 0,
            y: 0,
            width: area.width,
            height: area.height,
        });
        app.rect_header = layout.header;
        app.rect_action = layout.action;
        app.rect_runs = layout.runs;
        app.rect_fleet = layout.fleet;
        app.rect_stream = layout.stream;
        app.rect_bus = layout.bus;
        app.rect_composer = layout.composer;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(
                        &mut app,
                        key.code,
                        key.modifiers,
                        &swarm,
                        &projects,
                        &runs,
                        full.as_ref(),
                        &mut active_root,
                        local_root.as_deref(),
                    )? {
                        break;
                    }
                }
                Event::Mouse(m) => {
                    handle_mouse(
                        &mut app,
                        m,
                        &projects,
                        &runs,
                        full.as_ref(),
                        &mut active_root,
                    );
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    Ok(crate::exit_codes::ExitCode::Success)
}

fn load_projects(local_root: Option<&std::path::Path>) -> Vec<registry::ProjectEntry> {
    let reg = registry::ensure_known(local_root);
    reg.projects
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

    // Ctrl+C twice (within 2s) is the only quit path — never Esc or q.
    if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
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

    if app.focus == Focus::Composer {
        match code {
            KeyCode::Esc => app.focus = Focus::Runs,
            KeyCode::Enter => {
                let line = app.composer.trim().to_string();
                if !line.is_empty() {
                    match handle_composer(swarm, runs, app.selected_run, &line) {
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
                        format!(
                            "Opened {}",
                            p.name.as_deref().unwrap_or("project")
                        ),
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
            Focus::Log => app.stream_scroll = app.stream_scroll.saturating_add(3),
            Focus::Activity => app.bus_scroll = app.bus_scroll.saturating_add(1),
            Focus::Composer => {}
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
            Focus::Log => app.stream_scroll = app.stream_scroll.saturating_sub(3),
            Focus::Activity => app.bus_scroll = app.bus_scroll.saturating_sub(1),
            Focus::Composer => {}
        },
        KeyCode::PageDown => match app.focus {
            Focus::Log => app.stream_scroll = app.stream_scroll.saturating_add(12),
            Focus::Activity => app.bus_scroll = app.bus_scroll.saturating_add(6),
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
            Focus::Log => app.stream_scroll = app.stream_scroll.saturating_sub(12),
            Focus::Activity => app.bus_scroll = app.bus_scroll.saturating_sub(6),
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
                match workflow::plan::approve(swarm, id, false) {
                    Ok(_) => app.flash(format!("Approved plan {id}"), GREEN),
                    Err(e) => app.flash(format!("Approve failed: {e:#}"), RED),
                }
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = selected_id {
                match workflow::plan::reject(swarm, id, None, false) {
                    Ok(_) => app.flash(format!("Rejected plan {id}"), YELLOW),
                    Err(e) => app.flash(format!("Reject failed: {e:#}"), RED),
                }
            }
        }
        KeyCode::Char('s') => {
            if let Some(id) = selected_id {
                match crate::ship::confirm_ship(swarm, id, false) {
                    Ok(_) => app.flash(format!("Ship confirmed {id}"), GREEN),
                    Err(e) => app.flash(format!("Ship failed: {e:#}"), RED),
                }
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            app.stream_scroll = 0;
            app.bus_scroll = 0;
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.stream_scroll = 9999;
        }
        KeyCode::Char('?') => {
            app.show_help = true;
        }
        KeyCode::Char('w') => {
            app.log_expand = !app.log_expand;
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

fn handle_mouse(
    app: &mut App,
    m: crossterm::event::MouseEvent,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    active_root: &mut PathBuf,
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

            if contains(app.rect_composer, x, y) {
                app.focus = Focus::Composer;
            } else if contains(app.rect_stream, x, y) {
                app.focus = Focus::Log;
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Activity;
            } else if contains(app.rect_fleet, x, y) {
                app.focus = Focus::Agents;
                if let Some(st) = full {
                    if let Some(row) = list_row_at(app.rect_fleet, y, st.slots.len()) {
                        app.select_slot(row, st.slots.len());
                    }
                }
            } else if contains(app.rect_runs, x, y) {
                app.focus = Focus::Runs;
                match app.browse {
                    BrowseLevel::Projects => {
                        if let Some(row) = list_row_at(app.rect_runs, y, projects.len()) {
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
                        if let Some(row) = list_row_at(app.rect_runs, y, runs.len()) {
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
                app.stream_scroll = app.stream_scroll.saturating_add(3);
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Activity;
                app.bus_scroll = app.bus_scroll.saturating_add(2);
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
                app.stream_scroll = app.stream_scroll.saturating_sub(3);
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Activity;
                app.bus_scroll = app.bus_scroll.saturating_sub(2);
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
fn list_row_at(panel: Rect, y: u16, n_items: usize) -> Option<usize> {
    if n_items == 0 || panel.height < 3 {
        return None;
    }
    // border top + title uses y = panel.y; first item at panel.y + 1
    let inner_y = y.saturating_sub(panel.y.saturating_add(1));
    let row = inner_y as usize;
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
    composer: Rect,
    footer: Rect,
}

fn layout_rects(area: Rect) -> LayoutRects {
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

    LayoutRects {
        header: root[0],
        action: root[1],
        runs: left[0],
        fleet: left[1],
        stream: mid[1],
        bus: mid[2],
        composer: root[3],
        footer: root[4],
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
    app: &App,
    fleet_state: &mut ListState,
    rail_state: &mut ListState,
) {
    let area = f.area();
    // Full clear each frame — prevents styled-cell ghosting across the whole UI.
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let lay = layout_rects(area);

    draw_header(f, lay.header, swarm, full, app);
    draw_action(f, lay.action, projects, runs, full, app);
    draw_rail(f, lay.runs, projects, runs, app, rail_state);
    draw_agents(f, lay.fleet, full, app, fleet_state);
    draw_stream(f, lay.stream, full, stream_text, app);
    draw_activity(f, lay.bus, activity, app);
    draw_composer(f, lay.composer, app);
    draw_footer(f, lay.footer, app, full);

    if app.show_help {
        draw_help_overlay(f, area);
    }

    let _ = Clear;
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

    let phase_color = full
        .map(|s| phase_color(s.phase))
        .unwrap_or(FG_DIM);
    let dry = full
        .filter(|s| s.dry_run)
        .map(|_| " dry-run ")
        .unwrap_or("");

    let scope = match app.browse {
        BrowseLevel::Projects => " projects ",
        BrowseLevel::Runs => " runs ",
    };
    let left = Line::from(vec![
        Span::styled(" spar ", Style::default().fg(BG).bg(ACCENT).bold()),
        Span::raw(" "),
        Span::styled(project, Style::default().fg(FG).bold()),
        Span::styled(scope, Style::default().fg(BG).bg(ACCENT_SOFT)),
        Span::styled("  ·  ", Style::default().fg(FG_MUTED)),
        Span::styled(run, Style::default().fg(CYAN)),
        Span::styled(dry, Style::default().fg(BG).bg(YELLOW).bold()),
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

fn draw_action(
    f: &mut Frame,
    area: Rect,
    projects: &[registry::ProjectEntry],
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    app: &App,
) {
    let (text, fg, bg) = if app.browse == BrowseLevel::Projects {
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
                "  Plan ready — press  a  to approve ·  r  to reject · p = all projects  ".into(),
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
    };

    f.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(fg).bg(bg).bold()))
            .style(Style::default().bg(bg)),
        area,
    );
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
                            Span::styled(format!("{mark}"), Style::default().fg(ACCENT)),
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
                            Span::styled(
                                format!(" {n} runs "),
                                Style::default().fg(FG_MUTED),
                            ),
                            Span::styled(
                                relative_age(p.last_seen),
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
                            Span::styled(format!("{mark}"), Style::default().fg(ACCENT)),
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
                let color = if act.stalled {
                    RED
                } else {
                    slot_color(s)
                };
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
    app: &App,
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
    let title = if focused {
        format!(" Live log  · {slot_id}{silent_hint}  · {mode} · w toggle · scroll ")
    } else {
        format!(" Live log  · {slot_id}{silent_hint} ")
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

    render_scrollable_log(
        f,
        chunks[1],
        stream_text,
        app.stream_scroll,
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
        Some(SlotStatus::Done) => {
            Span::styled(" DONE ", Style::default().fg(BG).bg(GREEN).bold())
        }
        Some(SlotStatus::Failed) => {
            Span::styled(" FAIL ", Style::default().fg(BG).bg(RED).bold())
        }
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
fn render_scrollable_log(
    f: &mut Frame,
    area: Rect,
    text: &str,
    scroll: u16,
    colorize: bool,
    expand: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let sb_w = 1u16;
    let text_w = area.width.saturating_sub(sb_w).max(1) as usize;
    let height = area.height as usize;
    let rows = layout_log_rows(text, text_w, colorize, expand);
    let total = rows.len().max(1);
    let max_scroll = total.saturating_sub(height);
    let start = (scroll as usize).min(max_scroll);
    let visible: Vec<(String, Style)> = rows.into_iter().skip(start).take(height).collect();

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

    let mut sb = ScrollbarState::new(total).position(start);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(FG_MUTED).bg(BG_PANEL))
            .thumb_style(Style::default().fg(ACCENT_SOFT).bg(BG_PANEL)),
        area,
        &mut sb,
    );
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
                    cell.set_symbol(&ch.to_string());
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

/// Compact stream lines (grok-cli style density), then truncate or wrap to width.
fn layout_log_rows(
    text: &str,
    width: usize,
    colorize: bool,
    expand: bool,
) -> Vec<(String, Style)> {
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
            // Soft-wrap at word boundaries when possible.
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

fn draw_activity(f: &mut Frame, area: Rect, activity: &[String], app: &App) {
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
    render_scrollable_log(f, inner, &text, app.bus_scroll, false, true);
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

fn draw_footer(f: &mut Frame, area: Rect, app: &App, full: Option<&RunState>) {
    let (msg, color) = if let Some((_, m, c, _)) = &app.flash {
        (m.as_str(), *c)
    } else if !app.status_line.is_empty() {
        (app.status_line.as_str(), YELLOW)
    } else {
        (
            situational_footer(full, app.focus, app.browse),
            FG_MUTED,
        )
    };

    let gate = full.map(|s| s.phase.is_gate()).unwrap_or(false);
    let bg = if gate {
        Color::Rgb(40, 30, 12)
    } else {
        BG_RAISED
    };

    let left = Span::styled(format!(" {msg} "), Style::default().fg(color).bg(bg));
    let right = if gate {
        Span::styled(
            "  YOUR MOVE  ",
            Style::default().fg(BG).bg(YELLOW).bold(),
        )
    } else {
        Span::styled(
            format!(" {}  ? help  ·  Ctrl+C×2 exit  ", app.spinner()),
            Style::default().fg(FG_MUTED).bg(bg),
        )
    };
    let pad = area
        .width
        .saturating_sub(msg.len() as u16 + 24)
        .max(1) as usize;
    let line = Line::from(vec![
        left,
        Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        right,
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
        if st.phase.is_gate() {
            return "gate waiting · yellow bar above · p projects";
        }
    }
    match focus {
        Focus::Runs => "j/k runs · Esc/p projects · Tab → Agents · ? help",
        Focus::Agents => "j/k agent · log follows · Tab → Live log",
        Focus::Log => "scroll / j k · w wrap/trim · g top · Tab → Activity",
        Focus::Activity => "run timeline · scroll · Tab → Command",
        Focus::Composer => "type /approve /reject /ship /help · Enter · Esc",
    }
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let w = area.width.min(72).max(40);
    let h = area.height.min(22).max(14);
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
  Mouse\n\
    click panel          focus it (border lights up)\n\
    click a run/agent    select that row\n\
    scroll wheel         scroll log/messages · step runs/agents\n\
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
  Esc or ? to close help";
    let p = Paragraph::new(body)
        .style(Style::default().fg(FG).bg(BG_RAISED))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER_FOCUS))
                .title(Span::styled(
                    " Help ",
                    Style::default().fg(ACCENT).bold(),
                ))
                .style(Style::default().bg(BG_RAISED)),
        );
    f.render_widget(p, rect);
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
                let id = arg.or(run_id).ok_or_else(|| anyhow::anyhow!("no run selected"))?;
                workflow::plan::approve(swarm, id, false)?;
                Ok(format!("Approved plan {id}"))
            }
            "reject" => {
                let id = run_id.ok_or_else(|| anyhow::anyhow!("no run selected"))?;
                workflow::plan::reject(swarm, id, arg.map(|s| s.to_string()), false)?;
                Ok(format!("Rejected plan {id}"))
            }
            "ship" => {
                let id = arg.or(run_id).ok_or_else(|| anyhow::anyhow!("no run selected"))?;
                crate::ship::confirm_ship(swarm, id, false)?;
                Ok(format!("Ship confirmed {id}"))
            }
            other => Ok(format!("Unknown /{other} — try /help")),
        };
    }
    Ok(format!(
        "Noted (chat later): {}",
        truncate(cmd, 48)
    ))
}

fn stream_content(swarm: &SparPaths, full: Option<&RunState>, slot_idx: usize) -> String {
    let Some(st) = full else {
        return "\n  Select a run on the left.\n\n  New work:\n    spar plan -t \"describe the change\" --providers cli:claude\n".into();
    };
    if st.slots.is_empty() {
        return "\n  This run has no agents yet.".into();
    }
    let slot = &st.slots[slot_idx.min(st.slots.len() - 1)];
    let path = slot
        .log_path
        .clone()
        .unwrap_or_else(|| swarm.log_file(&st.id, &slot.id));
    if path.is_file() {
        let raw = process::tail_log(&path, 80_000);
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
        } else {
            body
        }
    } else {
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
) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(st) = full else {
        lines.push("No run selected.".into());
        lines.push(String::new());
        lines.push("Open a project, pick a run.".into());
        return lines;
    };

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
    if let Ok(bus) = crate::bus::list_events(swarm, &st.id) {
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
                    short_agent(&m.from),
                    short_agent(&m.to),
                    truncate(&m.body, 28)
                ));
            }
        }
    }

    let paused: Vec<_> = quota
        .providers
        .iter()
        .filter(|(_, q)| format!("{:?}", q.status).to_ascii_lowercase().contains("pause"))
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

fn activity_event_line(e: &events::Event) -> String {
    let t = e.ts.format("%H:%M");
    match e.kind {
        events::EventKind::Phase => {
            let phase = e
                .phase
                .map(phase_label)
                .unwrap_or_else(|| "?".into());
            format!("{t} → {phase}")
        }
        events::EventKind::Slot => {
            let slot = e.slot.as_deref().unwrap_or("agent");
            let st = e
                .status
                .map(slot_status_label)
                .unwrap_or("?");
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
    !phase.is_terminal() && !phase.is_gate()
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
        assert_eq!(phase_label(Phase::AwaitingPlanApproval), "Needs plan approval");
        assert_eq!(phase_label(Phase::AwaitingShipConfirm), "Ready to ship");
        assert!(!phase_label(Phase::Suite).contains('_'));
    }

    #[test]
    fn list_row_hit() {
        let r = Rect {
            x: 0,
            y: 10,
            width: 20,
            height: 8,
        };
        assert_eq!(list_row_at(r, 11, 3), Some(0));
        assert_eq!(list_row_at(r, 12, 3), Some(1));
        assert_eq!(list_row_at(r, 20, 3), None);
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
}
