//! Product shell — fleet control room styled like a first-class coding agent.
use crate::config::Config;
use crate::events;
use crate::liveness::SlotActivity;
use crate::paths::{self, SparPaths};
use crate::process::{self, StreamStats};
use crate::quota::QuotaStore;
use crate::state::{self, Phase, RunState, SlotState, SlotStatus};
use crate::workflow;
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ── palette (dark agent-cli aesthetic) ──────────────────────────────────────

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
const PULSE: &[&str] = &["●", "◉", "○", "◉"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Fleet,
    Stream,
    Bus,
    Composer,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Fleet => Focus::Stream,
            Focus::Stream => Focus::Bus,
            Focus::Bus => Focus::Composer,
            Focus::Composer => Focus::Fleet,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::Fleet => Focus::Composer,
            Focus::Stream => Focus::Fleet,
            Focus::Bus => Focus::Stream,
            Focus::Composer => Focus::Bus,
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
    let root = paths::find_project_root()?;
    let swarm = SparPaths::new(&root);
    let stall_warn_secs = Config::load(&root)
        .map(|c| c.timeouts.stall_warn_secs)
        .unwrap_or(300);

    enable_raw_mode()?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;
    terminal.clear()?;

    let result = run_loop(&mut terminal, &swarm, opts.task_seed, stall_warn_secs);

    disable_raw_mode()?;
    let mut out = stdout();
    out.execute(DisableMouseCapture)?;
    out.execute(LeaveAlternateScreen)?;
    result
}

struct App {
    selected_run: usize,
    selected_slot: usize,
    focus: Focus,
    composer: String,
    status_line: String,
    stream_scroll: u16,
    bus_scroll: u16,
    tick: u64,
    started: Instant,
    flash: Option<(Instant, String, Color)>,
    mouse_hint_shown: bool,
    last_click: Option<(u16, u16, Instant)>,
    stall_warn_secs: u64,
    /// Layout rects for hit-testing mouse clicks
    rect_header: Rect,
    rect_fleet: Rect,
    rect_stream: Rect,
    rect_bus: Rect,
    rect_composer: Rect,
    rect_runs: Rect,
}

impl App {
    fn new(task_seed: Option<String>, stall_warn_secs: u64) -> Self {
        Self {
            selected_run: 0,
            selected_slot: 0,
            focus: Focus::Composer,
            composer: task_seed.unwrap_or_default(),
            status_line: String::new(),
            stream_scroll: 0,
            bus_scroll: 0,
            tick: 0,
            started: Instant::now(),
            flash: None,
            mouse_hint_shown: false,
            stall_warn_secs,
            last_click: None,
            rect_header: Rect::default(),
            rect_fleet: Rect::default(),
            rect_stream: Rect::default(),
            rect_bus: Rect::default(),
            rect_composer: Rect::default(),
            rect_runs: Rect::default(),
        }
    }

    fn flash(&mut self, msg: impl Into<String>, color: Color) {
        self.flash = Some((Instant::now(), msg.into(), color));
        self.status_line.clear();
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.tick as usize) % SPINNER.len()]
    }

    fn pulse(&self) -> &'static str {
        PULSE[(self.tick as usize / 4) % PULSE.len()]
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    swarm: &SparPaths,
    task_seed: Option<String>,
    stall_warn_secs: u64,
) -> Result<crate::exit_codes::ExitCode> {
    let mut app = App::new(task_seed, stall_warn_secs);
    let mut fleet_state = ListState::default();
    let mut runs_state = ListState::default();

    loop {
        app.tick = app.tick.wrapping_add(1);

        let runs = state::list_runs(swarm).unwrap_or_default();
        if !runs.is_empty() {
            app.selected_run = app.selected_run.min(runs.len() - 1);
        } else {
            app.selected_run = 0;
        }
        runs_state.select(if runs.is_empty() {
            None
        } else {
            Some(app.selected_run)
        });

        let selected_id = runs.get(app.selected_run).map(|r| r.id.clone());
        let full = selected_id
            .as_ref()
            .and_then(|id| RunState::load(swarm, id).ok());
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

        let quota = QuotaStore::load(swarm).unwrap_or_default();
        let stream_text = stream_content(swarm, full.as_ref(), app.selected_slot);
        let bus_lines = bus_lines(swarm, full.as_ref(), &quota);

        // expire flash
        if let Some((t, _, _)) = &app.flash {
            if t.elapsed() > Duration::from_secs(3) {
                app.flash = None;
            }
        }

        terminal.draw(|f| {
            draw(f, swarm, &runs, full.as_ref(), &stream_text, &bus_lines, &app, &mut fleet_state, &mut runs_state);
            // store rects from last layout — recompute in draw via app mut isn't possible easily;
            // we recompute hit rects in draw by writing through a side channel
        })?;

        // recompute rects for mouse (same layout as draw)
        let area = terminal.size()?;
        let layout = layout_rects(Rect {
            x: 0,
            y: 0,
            width: area.width,
            height: area.height,
        });
        app.rect_header = layout.header;
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
                        swarm,
                        &runs,
                        full.as_ref(),
                    )? {
                        break;
                    }
                }
                Event::Mouse(m) => {
                    handle_mouse(&mut app, m, &runs, full.as_ref());
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
    Ok(crate::exit_codes::ExitCode::Success)
}

fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    swarm: &SparPaths,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
) -> Result<bool> {
    let selected_id = runs.get(app.selected_run).map(|r| r.id.as_str());

    if app.focus == Focus::Composer {
        match code {
            KeyCode::Esc => {
                app.focus = Focus::Fleet;
            }
            KeyCode::Enter => {
                let line = app.composer.trim().to_string();
                if !line.is_empty() {
                    match handle_composer(swarm, runs, app.selected_run, &line) {
                        Ok(msg) => {
                            if msg == "__quit__" {
                                return Ok(true);
                            }
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
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('/') => {
            app.focus = Focus::Composer;
            if !app.composer.starts_with('/') {
                app.composer = "/".into();
            }
        }
        KeyCode::Char('i') => {
            app.focus = Focus::Composer;
        }
        KeyCode::Char('j') | KeyCode::Down => match app.focus {
            Focus::Fleet => {
                if let Some(st) = full {
                    if !st.slots.is_empty() {
                        app.selected_slot = (app.selected_slot + 1).min(st.slots.len() - 1);
                        app.stream_scroll = 0;
                    }
                }
            }
            Focus::Stream => app.stream_scroll = app.stream_scroll.saturating_add(2),
            Focus::Bus => app.bus_scroll = app.bus_scroll.saturating_add(1),
            Focus::Composer => {}
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            Focus::Fleet => {
                app.selected_slot = app.selected_slot.saturating_sub(1);
                app.stream_scroll = 0;
            }
            Focus::Stream => app.stream_scroll = app.stream_scroll.saturating_sub(2),
            Focus::Bus => app.bus_scroll = app.bus_scroll.saturating_sub(1),
            Focus::Composer => {}
        },
        KeyCode::Char('J') => {
            if !runs.is_empty() {
                app.selected_run = (app.selected_run + 1).min(runs.len() - 1);
                app.selected_slot = 0;
                app.stream_scroll = 0;
            }
        }
        KeyCode::Char('K') => {
            app.selected_run = app.selected_run.saturating_sub(1);
            app.selected_slot = 0;
            app.stream_scroll = 0;
        }
        KeyCode::Char('[') => {
            if !runs.is_empty() {
                app.selected_run = app.selected_run.saturating_sub(1);
                app.selected_slot = 0;
            }
        }
        KeyCode::Char(']') => {
            if !runs.is_empty() {
                app.selected_run = (app.selected_run + 1).min(runs.len() - 1);
                app.selected_slot = 0;
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as u8 - b'1') as usize;
            if let Some(st) = full {
                if idx < st.slots.len() {
                    app.selected_slot = idx;
                    app.focus = Focus::Fleet;
                    app.stream_scroll = 0;
                }
            }
        }
        KeyCode::Char('a') => {
            if let Some(id) = selected_id {
                match workflow::plan::approve(swarm, id, false) {
                    Ok(_) => app.flash(format!("approved {id}"), GREEN),
                    Err(e) => app.flash(format!("approve: {e:#}"), RED),
                }
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = selected_id {
                match workflow::plan::reject(swarm, id, None, false) {
                    Ok(_) => app.flash(format!("rejected {id}"), YELLOW),
                    Err(e) => app.flash(format!("reject: {e:#}"), RED),
                }
            }
        }
        KeyCode::Char('s') => {
            if let Some(id) = selected_id {
                match crate::ship::confirm_ship(swarm, id, false) {
                    Ok(_) => app.flash(format!("ship confirmed {id}"), GREEN),
                    Err(e) => app.flash(format!("ship: {e:#}"), RED),
                }
            }
        }
        KeyCode::Char('g') => {
            app.stream_scroll = 0;
            app.bus_scroll = 0;
        }
        KeyCode::Char('G') => {
            app.stream_scroll = 9999;
        }
        KeyCode::Char('?') => {
            app.flash(
                "Tab panes · j/k slots · J/K or [] runs · a approve · r reject · s ship · i// composer · q quit · click panes",
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
    runs: &[state::RunSummary],
    full: Option<&RunState>,
) {
    let (x, y) = (m.column, m.row);
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // double-click detect
            let now = Instant::now();
            let dbl = app
                .last_click
                .map(|(lx, ly, t)| lx == x && ly == y && t.elapsed() < Duration::from_millis(350))
                .unwrap_or(false);
            app.last_click = Some((x, y, now));

            if contains(app.rect_composer, x, y) {
                app.focus = Focus::Composer;
            } else if contains(app.rect_stream, x, y) {
                app.focus = Focus::Stream;
            } else if contains(app.rect_bus, x, y) {
                app.focus = Focus::Bus;
            } else if contains(app.rect_fleet, x, y) {
                app.focus = Focus::Fleet;
                // row within fleet list (account for border + header)
                if let Some(st) = full {
                    let row = y.saturating_sub(app.rect_fleet.y.saturating_add(1)) as usize;
                    if row < st.slots.len() {
                        app.selected_slot = row;
                        app.stream_scroll = 0;
                    }
                }
            } else if contains(app.rect_runs, x, y) {
                app.focus = Focus::Fleet;
                let row = y.saturating_sub(app.rect_runs.y.saturating_add(1)) as usize;
                if row < runs.len() {
                    app.selected_run = row;
                    app.selected_slot = 0;
                    app.stream_scroll = 0;
                }
            }

            if !app.mouse_hint_shown {
                app.mouse_hint_shown = true;
                app.flash("mouse on · click panes/rows · scroll stream", ACCENT_SOFT);
            }
            if dbl && app.focus == Focus::Composer {
                // ignore
            }
        }
        MouseEventKind::ScrollDown => {
            if contains(app.rect_stream, x, y) {
                app.stream_scroll = app.stream_scroll.saturating_add(3);
            } else if contains(app.rect_bus, x, y) {
                app.bus_scroll = app.bus_scroll.saturating_add(1);
            } else if contains(app.rect_fleet, x, y) {
                if let Some(st) = full {
                    if !st.slots.is_empty() {
                        app.selected_slot = (app.selected_slot + 1).min(st.slots.len() - 1);
                    }
                }
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(app.rect_stream, x, y) {
                app.stream_scroll = app.stream_scroll.saturating_sub(3);
            } else if contains(app.rect_bus, x, y) {
                app.bus_scroll = app.bus_scroll.saturating_sub(1);
            } else if contains(app.rect_fleet, x, y) {
                app.selected_slot = app.selected_slot.saturating_sub(1);
            }
        }
        _ => {}
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

struct LayoutRects {
    header: Rect,
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
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(area);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(47),
            Constraint::Percentage(25),
        ])
        .split(root[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(mid[0]);

    LayoutRects {
        header: root[0],
        runs: left[0],
        fleet: left[1],
        stream: mid[1],
        bus: mid[2],
        composer: root[2],
        footer: root[3],
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut Frame,
    swarm: &SparPaths,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    stream_text: &str,
    bus_lines: &[String],
    app: &App,
    fleet_state: &mut ListState,
    runs_state: &mut ListState,
) {
    let area = f.area();
    // solid bg
    f.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let lay = layout_rects(area);

    draw_header(f, lay.header, swarm, full, app);
    draw_runs(f, lay.runs, runs, app, runs_state);
    draw_fleet(f, lay.fleet, full, app, fleet_state);
    draw_stream(f, lay.stream, full, stream_text, app);
    draw_bus(f, lay.bus, bus_lines, app);
    draw_composer(f, lay.composer, app);
    draw_footer(f, lay.footer, app, full);
}

fn draw_header(f: &mut Frame, area: Rect, swarm: &SparPaths, full: Option<&RunState>, app: &App) {
    let project = swarm
        .project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(".");
    let (run, phase, task) = match full {
        Some(st) => (
            st.id.clone(),
            format!("{:?}", st.phase),
            st.task
                .as_deref()
                .map(|t| truncate(t, 40))
                .unwrap_or_default(),
        ),
        None => ("—".into(), "idle".into(), String::new()),
    };

    let phase_color = full
        .map(|s| phase_color(s.phase))
        .unwrap_or(FG_DIM);
    let anim = if full.map(|s| is_active_phase(s.phase)).unwrap_or(false) {
        format!("{} ", app.spinner())
    } else {
        format!("{} ", app.pulse())
    };

    let elapsed = app.started.elapsed().as_secs();
    let clock = format!("{:02}:{:02}", elapsed / 60, elapsed % 60);

    let left = Line::from(vec![
        Span::styled(" spar ", Style::default().fg(BG).bg(ACCENT).bold()),
        Span::raw(" "),
        Span::styled(project, Style::default().fg(FG).bold()),
        Span::styled(" · ", Style::default().fg(FG_MUTED)),
        Span::styled(run, Style::default().fg(CYAN)),
        Span::raw("  "),
        Span::styled(anim, Style::default().fg(phase_color)),
        Span::styled(phase, Style::default().fg(phase_color).bold()),
    ]);
    let right = Line::from(vec![
        Span::styled(task, Style::default().fg(FG_DIM)),
        Span::raw("  "),
        Span::styled(clock, Style::default().fg(FG_MUTED)),
    ]);

    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(BG_RAISED));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(48)])
        .split(inner);
    f.render_widget(Paragraph::new(left), chunks[0]);
    f.render_widget(Paragraph::new(right).alignment(Alignment::Right), chunks[1]);
}

fn draw_runs(
    f: &mut Frame,
    area: Rect,
    runs: &[state::RunSummary],
    app: &App,
    state: &mut ListState,
) {
    let focused = app.focus == Focus::Fleet;
    let items: Vec<ListItem> = if runs.is_empty() {
        vec![ListItem::new(Span::styled(
            "  no runs yet",
            Style::default().fg(FG_MUTED).italic(),
        ))]
    } else {
        runs.iter()
            .enumerate()
            .map(|(i, r)| {
                let sel = i == app.selected_run;
                let mark = if sel { "›" } else { " " };
                let line = Line::from(vec![
                    Span::styled(format!("{mark} "), Style::default().fg(ACCENT)),
                    Span::styled(
                        format!("{:<8}", truncate(&r.id, 8)),
                        Style::default().fg(if sel { FG } else { FG_DIM }).bold(),
                    ),
                    Span::styled(
                        format!(" {}", format!("{:?}", r.workflow).to_lowercase()),
                        Style::default().fg(FG_MUTED),
                    ),
                    Span::styled(
                        format!(" {}", format!("{:?}", r.phase).to_lowercase()),
                        Style::default().fg(phase_color(r.phase)),
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

    let list = List::new(items).block(panel("runs  [ ]", focused)).highlight_symbol("");
    f.render_stateful_widget(list, area, state);
}

fn draw_fleet(
    f: &mut Frame,
    area: Rect,
    full: Option<&RunState>,
    app: &App,
    state: &mut ListState,
) {
    let focused = app.focus == Focus::Fleet;
    let items: Vec<ListItem> = match full {
        None => vec![ListItem::new(Span::styled(
            "  select a run",
            Style::default().fg(FG_MUTED).italic(),
        ))],
        Some(st) if st.slots.is_empty() => vec![ListItem::new(Span::styled(
            "  no slots",
            Style::default().fg(FG_MUTED).italic(),
        ))],
        Some(st) => st
            .slots
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let sel = i == app.selected_slot;
                let icon = slot_icon(s, app);
                let stats = s.log_path.as_ref().and_then(|p| StreamStats::load(p));
                let badge = stats
                    .as_ref()
                    .filter(|st| st.tools > 0 || st.input_tokens > 0)
                    .map(|st| format!(" {}t {}", st.tools, compact_u64(st.context_tokens)))
                    .unwrap_or_default();
                let act = SlotActivity::observe(s, app.stall_warn_secs);
                let (age, age_color) = if s.status == SlotStatus::Running {
                    if act.stalled {
                        (format!(" STALL {}", act.human_silent()), RED)
                    } else {
                        (format!(" {}", act.human_silent()), FG_MUTED)
                    }
                } else {
                    (String::new(), FG_MUTED)
                };
                let line = Line::from(vec![
                    Span::styled(format!(" {icon} "), Style::default().fg(if act.stalled {
                        RED
                    } else {
                        slot_color(s)
                    })),
                    Span::styled(
                        format!("{:<12}", truncate(&s.id, 12)),
                        Style::default()
                            .fg(if sel { FG } else { FG_DIM })
                            .add_modifier(if sel { Modifier::BOLD } else { Modifier::empty() }),
                    ),
                    Span::styled(truncate(&s.provider, 11), Style::default().fg(ACCENT_SOFT)),
                    Span::styled(badge, Style::default().fg(YELLOW)),
                    Span::styled(age, Style::default().fg(age_color)),
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
        if stalled > 0 {
            format!("fleet  {running}/{n} live  {stalled} stall")
        } else {
            format!("fleet  {running}/{n} live")
        }
    } else {
        "fleet".into()
    };

    let list = List::new(items).block(panel(&title, focused));
    f.render_stateful_widget(list, area, state);

    // activity gauge under fleet if active
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
                .label(format!("{:.0}%", ratio * 100.0));
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
    let focused = app.focus == Focus::Stream;
    let slot = full.and_then(|st| st.slots.get(app.selected_slot));
    let slot_id = slot.map(|s| s.id.as_str()).unwrap_or("—");
    let silent_hint = slot
        .map(|s| {
            let act = SlotActivity::observe(s, app.stall_warn_secs);
            if act.stalled {
                format!("  STALL {}", act.human_silent())
            } else if s.status == SlotStatus::Running {
                format!("  silent {}", act.human_silent())
            } else {
                String::new()
            }
        })
        .unwrap_or_default();
    let title = format!("stream  {slot_id}{silent_hint}");

    let block = panel(&title, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // stats strip
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

    let styled = stream_lines_styled(stream_text);
    let p = Paragraph::new(styled)
        .style(Style::default().bg(BG_PANEL))
        .wrap(Wrap { trim: false })
        .scroll((app.stream_scroll, 0));
    f.render_widget(p, chunks[1]);

    let lines = stream_text.lines().count().max(1);
    let mut sb = ScrollbarState::new(lines).position(app.stream_scroll as usize);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(FG_MUTED))
            .thumb_style(Style::default().fg(ACCENT_SOFT)),
        chunks[1],
        &mut sb,
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
                "  no live stats yet",
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
        Some(SlotStatus::Running) => Span::styled(" LIVE ", Style::default().fg(BG).bg(CYAN).bold()),
        Some(SlotStatus::Done) => Span::styled(" DONE ", Style::default().fg(BG).bg(GREEN).bold()),
        Some(SlotStatus::Failed) => Span::styled(" FAIL ", Style::default().fg(BG).bg(RED).bold()),
        _ => Span::styled(" … ", Style::default().fg(FG_MUTED).bg(BG_RAISED)),
    };
    let line = Line::from(vec![
        status_span,
        Span::raw(" "),
        Span::styled(
            format!(" co {} ", compact_u64(ctx)),
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

fn stream_lines_styled(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            let style = if line.starts_with('→') {
                Style::default().fg(CYAN)
            } else if line.starts_with('←') {
                if line.contains('✗') {
                    Style::default().fg(RED)
                } else {
                    Style::default().fg(GREEN)
                }
            } else if line.starts_with('·') {
                Style::default().fg(FG_MUTED)
            } else if line.starts_with('…') {
                Style::default().fg(FG_MUTED).italic()
            } else if line.starts_with('!') {
                Style::default().fg(RED).bold()
            } else if line.starts_with('#') {
                Style::default().fg(FG_MUTED)
            } else {
                Style::default().fg(FG)
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect()
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

fn draw_bus(f: &mut Frame, area: Rect, bus_lines: &[String], app: &App) {
    let focused = app.focus == Focus::Bus;
    let text = if bus_lines.is_empty() {
        "  bus quiet · peer messages land here".into()
    } else {
        bus_lines.join("\n")
    };
    let p = Paragraph::new(text)
        .style(Style::default().fg(FG_DIM).bg(BG_PANEL))
        .wrap(Wrap { trim: true })
        .scroll((app.bus_scroll, 0))
        .block(panel("bus", focused));
    f.render_widget(p, area);
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
    let hint = if app.composer.is_empty() && focused {
        "  /approve  /reject  /ship  /help  ·  plain text is a stub for now"
    } else {
        ""
    };
    let p = Paragraph::new(vec![
        line,
        Line::from(Span::styled(hint, Style::default().fg(FG_MUTED).italic())),
    ])
    .block(panel(
        if focused {
            "composer  enter send · esc blur"
        } else {
            "composer  i or / to type"
        },
        focused,
    ));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App, full: Option<&RunState>) {
    let (msg, color) = if let Some((_, m, c)) = &app.flash {
        (m.as_str(), *c)
    } else if !app.status_line.is_empty() {
        (app.status_line.as_str(), YELLOW)
    } else {
        (
            "tab panes · j/k slots · J/K runs · a/r/s gates · i compose · ? help · q quit",
            FG_MUTED,
        )
    };

    // gate urgency bar
    let gate = full.map(|s| s.phase.is_gate()).unwrap_or(false);
    let bg = if gate {
        Color::Rgb(40, 30, 12)
    } else {
        BG_RAISED
    };

    let left = Span::styled(format!(" {msg} "), Style::default().fg(color).bg(bg));
    let right = if gate {
        Span::styled(
            "  GATE  press a/r/s  ",
            Style::default().fg(BG).bg(YELLOW).bold(),
        )
    } else {
        Span::styled(
            format!(" {} ", app.spinner()),
            Style::default().fg(FG_MUTED).bg(bg),
        )
    };
    let line = Line::from(vec![left, Span::styled(
        " ".repeat(area.width.saturating_sub(msg.len() as u16 + 20) as usize),
        Style::default().bg(bg),
    ), right]);
    f.render_widget(Paragraph::new(line).style(Style::default().bg(bg)), area);

    // silence unused Clear
    let _ = Clear;
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
        .title(Span::styled(format!(" {title} "), title_style))
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
            "q" | "quit" => Ok("__quit__".into()),
            "help" | "h" | "?" => Ok(
                "/approve /reject [reason] /ship /quit · click panes · scroll stream".into(),
            ),
            "approve" => {
                let id = arg.or(run_id).ok_or_else(|| anyhow::anyhow!("no run"))?;
                workflow::plan::approve(swarm, id, false)?;
                Ok(format!("approved {id}"))
            }
            "reject" => {
                let id = run_id.ok_or_else(|| anyhow::anyhow!("no run"))?;
                workflow::plan::reject(swarm, id, arg.map(|s| s.to_string()), false)?;
                Ok(format!("rejected {id}"))
            }
            "ship" => {
                let id = arg.or(run_id).ok_or_else(|| anyhow::anyhow!("no run"))?;
                crate::ship::confirm_ship(swarm, id, false)?;
                Ok(format!("ship confirmed {id}"))
            }
            other => Ok(format!("unknown /{other} — try /help")),
        };
    }
    Ok(format!("noted (orchestrator chat later): {}", truncate(cmd, 48)))
}

fn stream_content(swarm: &SparPaths, full: Option<&RunState>, slot_idx: usize) -> String {
    let Some(st) = full else {
        return "\n  open a run to stream agent output\n\n  tip: spar plan -t \"…\" --providers cli:claude --dry-run".into();
    };
    if st.slots.is_empty() {
        return "\n  no slots in this run".into();
    }
    let slot = &st.slots[slot_idx.min(st.slots.len() - 1)];
    let path = slot
        .log_path
        .clone()
        .unwrap_or_else(|| swarm.log_file(&st.id, &slot.id));
    if path.is_file() {
        let raw = process::tail_log(&path, 64_000);
        // drop spawn header noise for display when body exists
        let body = raw
            .lines()
            .skip_while(|l| l.starts_with('#') || *l == "---" || l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if body.trim().is_empty() {
            format!("\n  {} waiting for output…", slot.id)
        } else {
            body
        }
    } else {
        format!("\n  no log yet for {}\n  {}", slot.id, slot.provider)
    }
}

fn bus_lines(swarm: &SparPaths, full: Option<&RunState>, quota: &QuotaStore) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(st) = full {
        if let Ok(presence) = crate::bus::list_presence(swarm, &st.id) {
            for p in presence.iter().take(6) {
                lines.push(format!("· {:<12} {}", p.agent, p.status));
            }
        }
        if let Ok(evs) = crate::bus::list_events(swarm, &st.id) {
            for e in evs.iter().rev().take(12).rev() {
                lines.push(truncate(
                    &format!("{}→{}  {}", e.from, e.to, e.body),
                    42,
                ));
            }
        }
        if lines.is_empty() {
            for e in events::read_all(swarm, &st.id).unwrap_or_default().iter().rev().take(10).rev()
            {
                lines.push(truncate(&e.display_line(), 42));
            }
        }
    }
    for (name, q) in &quota.providers {
        lines.push(format!("q {name}: {:?}", q.status));
    }
    lines
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
