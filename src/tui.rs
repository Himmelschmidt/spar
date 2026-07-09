use crate::events;
use crate::paths::{self, SparPaths};
use crate::process;
use crate::quota::QuotaStore;
use crate::state::{self, Phase, RunState, SlotState};
use crate::workflow;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Runs,
    Fleet,
    Stream,
    Composer,
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

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = run_loop(&mut terminal, &swarm, opts.task_seed);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

struct App {
    selected_run: usize,
    selected_slot: usize,
    focus: Focus,
    composer: String,
    status_line: String,
    stream_scroll: u16,
    event_lines: Vec<String>,
}

impl App {
    fn new(task_seed: Option<String>) -> Self {
        Self {
            selected_run: 0,
            selected_slot: 0,
            focus: Focus::Runs,
            composer: task_seed.unwrap_or_default(),
            status_line: "Tab focus  j/k select  a approve  r reject  q quit  / for commands"
                .into(),
            stream_scroll: 0,
            event_lines: Vec::new(),
        }
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    swarm: &SparPaths,
    task_seed: Option<String>,
) -> Result<crate::exit_codes::ExitCode> {
    let mut app = App::new(task_seed);

    loop {
        let runs = state::list_runs(swarm).unwrap_or_default();
        if !runs.is_empty() {
            app.selected_run = app.selected_run.min(runs.len() - 1);
        } else {
            app.selected_run = 0;
        }

        let selected_id = runs.get(app.selected_run).map(|r| r.id.clone());
        let full = selected_id
            .as_ref()
            .and_then(|id| RunState::load(swarm, id).ok());
        if let Some(ref st) = full {
            app.selected_slot = app.selected_slot.min(st.slots.len().saturating_sub(1));
        } else {
            app.selected_slot = 0;
        }

        let quota = QuotaStore::load(swarm).unwrap_or_default();
        let stream_text = stream_content(swarm, full.as_ref(), app.selected_slot);
        if let Some(id) = &selected_id {
            app.event_lines = events::read_all(swarm, id)
                .unwrap_or_default()
                .into_iter()
                .rev()
                .take(40)
                .map(|e| e.display_line())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        } else {
            app.event_lines.clear();
        }

        terminal.draw(|f| {
            draw(f, swarm, &runs, full.as_ref(), &quota, &stream_text, &app);
        })?;

        if event::poll(Duration::from_millis(300))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.focus == Focus::Composer {
                    match key.code {
                        KeyCode::Esc => {
                            app.focus = Focus::Runs;
                        }
                        KeyCode::Enter => {
                            let line = app.composer.trim().to_string();
                            app.composer.clear();
                            if !line.is_empty() {
                                match handle_composer(swarm, &runs, app.selected_run, &line) {
                                    Ok(msg) => {
                                        if msg == "__quit__" {
                                            break;
                                        }
                                        app.status_line = msg;
                                    }
                                    Err(e) => app.status_line = format!("error: {e:#}"),
                                }
                            }
                        }
                        KeyCode::Backspace => {
                            app.composer.pop();
                        }
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.composer.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Tab => {
                        app.focus = match app.focus {
                            Focus::Runs => Focus::Fleet,
                            Focus::Fleet => Focus::Stream,
                            Focus::Stream => Focus::Composer,
                            Focus::Composer => Focus::Runs,
                        };
                    }
                    KeyCode::Char('/') => {
                        app.focus = Focus::Composer;
                        if !app.composer.starts_with('/') {
                            app.composer = "/".into();
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => match app.focus {
                        Focus::Runs if !runs.is_empty() => {
                            app.selected_run = (app.selected_run + 1).min(runs.len() - 1);
                            app.selected_slot = 0;
                            app.stream_scroll = 0;
                        }
                        Focus::Fleet => {
                            if let Some(ref st) = full {
                                if !st.slots.is_empty() {
                                    app.selected_slot =
                                        (app.selected_slot + 1).min(st.slots.len() - 1);
                                    app.stream_scroll = 0;
                                }
                            }
                        }
                        Focus::Stream => {
                            app.stream_scroll = app.stream_scroll.saturating_add(3);
                        }
                        _ => {}
                    },
                    KeyCode::Char('k') | KeyCode::Up => match app.focus {
                        Focus::Runs => {
                            app.selected_run = app.selected_run.saturating_sub(1);
                            app.selected_slot = 0;
                            app.stream_scroll = 0;
                        }
                        Focus::Fleet => {
                            app.selected_slot = app.selected_slot.saturating_sub(1);
                            app.stream_scroll = 0;
                        }
                        Focus::Stream => {
                            app.stream_scroll = app.stream_scroll.saturating_sub(3);
                        }
                        _ => {}
                    },
                    KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                        let idx = (c as u8 - b'1') as usize;
                        if let Some(ref st) = full {
                            if idx < st.slots.len() {
                                app.selected_slot = idx;
                                app.focus = Focus::Fleet;
                                app.stream_scroll = 0;
                            }
                        }
                    }
                    KeyCode::Char('a') => {
                        if let Some(id) = &selected_id {
                            match workflow::plan::approve(swarm, id, false) {
                                Ok(_) => app.status_line = format!("approved {id}"),
                                Err(e) => app.status_line = format!("approve failed: {e:#}"),
                            }
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Some(id) = &selected_id {
                            match workflow::plan::reject(swarm, id, None, false) {
                                Ok(_) => app.status_line = format!("rejected {id}"),
                                Err(e) => app.status_line = format!("reject failed: {e:#}"),
                            }
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some(id) = &selected_id {
                            match crate::ship::confirm_ship(swarm, id, false) {
                                Ok(_) => {
                                    app.status_line = format!("ship confirmed for {id}")
                                }
                                Err(e) => app.status_line = format!("ship confirm: {e:#}"),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(crate::exit_codes::ExitCode::Success)
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
                "commands: /approve /reject [reason] /ship /quit /help — or plain text (stub)"
                    .into(),
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
                Ok(format!("ship confirmed for {id}"))
            }
            other => Ok(format!("unknown command /{other} — try /help")),
        };
    }
    Ok(format!(
        "composer stub (orchestrator chat later): {}",
        truncate(cmd, 60)
    ))
}

fn stream_content(swarm: &SparPaths, full: Option<&RunState>, slot_idx: usize) -> String {
    let Some(st) = full else {
        return "no run selected".into();
    };
    if st.slots.is_empty() {
        return "no slots".into();
    }
    let slot = &st.slots[slot_idx.min(st.slots.len() - 1)];
    let path = slot
        .log_path
        .clone()
        .unwrap_or_else(|| swarm.log_file(&st.id, &slot.id));
    if path.is_file() {
        process::tail_log(&path, 24_000)
    } else {
        format!("(no log yet for slot {})", slot.id)
    }
}

fn draw(
    f: &mut Frame,
    swarm: &SparPaths,
    runs: &[state::RunSummary],
    full: Option<&RunState>,
    quota: &QuotaStore,
    stream_text: &str,
    app: &App,
) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Percentage(35),
            Constraint::Min(6),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    // Header
    let (run_s, phase_s, gates_s, task_s) = if let Some(st) = full {
        let gates = format!(
            "plan={} winner={} ship={}",
            st.gates.plan_approved,
            st.gates
                .winner_confirmed
                .as_deref()
                .unwrap_or("-"),
            st.gates.ship_confirmed
        );
        (
            st.id.clone(),
            format!("{:?}", st.phase),
            gates,
            st.task
                .as_deref()
                .map(|t| truncate(t, 48))
                .unwrap_or_default(),
        )
    } else {
        ("—".into(), "—".into(), "—".into(), String::new())
    };
    let header = Paragraph::new(format!(
        "spar  {}  run={}  phase={}  gates[{}]  {}",
        swarm.project_root.display(),
        run_s,
        phase_s,
        gates_s,
        task_s
    ))
    .style(Style::default().fg(Color::Cyan))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("spar")
            .border_style(focus_border(app.focus == Focus::Runs)),
    );
    f.render_widget(header, chunks[0]);

    // Runs list
    let run_rows: Vec<Row> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let style = if i == app.selected_run {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                phase_style(r.phase)
            };
            Row::new(vec![
                Cell::from(r.id.clone()),
                Cell::from(format!("{:?}", r.workflow)),
                Cell::from(format!("{:?}", r.phase)),
                Cell::from(r.updated_at.format("%H:%M:%S").to_string()),
            ])
            .style(style)
        })
        .collect();
    let runs_table = Table::new(
        run_rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(24),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["run", "workflow", "phase", "updated"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(focus_title("runs", app.focus == Focus::Runs))
            .border_style(focus_border(app.focus == Focus::Runs)),
    );
    f.render_widget(runs_table, chunks[1]);

    // Fleet + bus split
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(chunks[2]);

    let fleet_rows = fleet_rows(full, app.selected_slot);
    let fleet = Table::new(
        fleet_rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(12),
        ],
    )
    .header(
        Row::new(vec!["slot", "role", "provider", "status", "worktree"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(focus_title("fleet", app.focus == Focus::Fleet))
            .border_style(focus_border(app.focus == Focus::Fleet)),
    );
    f.render_widget(fleet, mid[0]);

    let mut bus_lines = vec!["swarm bus".into()];
    if let Some(id) = full.map(|s| s.id.as_str()) {
        if let Ok(presence) = crate::bus::list_presence(swarm, id) {
            for p in presence.iter().take(4) {
                bus_lines.push(format!("· {} {}", p.agent, p.status));
            }
        }
        if let Ok(evs) = crate::bus::list_events(swarm, id) {
            for e in evs.iter().rev().take(6).rev() {
                bus_lines.push(truncate(
                    &format!("{}→{}: {}", e.from, e.to, e.body),
                    42,
                ));
            }
        }
    }
    for line in app.event_lines.iter().rev().take(3).rev() {
        bus_lines.push(truncate(line, 40));
    }
    if quota.providers.is_empty() {
        bus_lines.push("quota: (none)".into());
    } else {
        for (name, q) in &quota.providers {
            bus_lines.push(format!("q {name}: {:?}", q.status));
        }
    }
    let bus = Paragraph::new(bus_lines.join("\n"))
        .block(Block::default().borders(Borders::ALL).title("bus / events"))
        .wrap(Wrap { trim: true });
    f.render_widget(bus, mid[1]);

    // Stream
    let slot_label = full
        .and_then(|st| st.slots.get(app.selected_slot))
        .map(|s| s.id.as_str())
        .unwrap_or("-");
    let stream = Paragraph::new(stream_text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(focus_title(
                    &format!("stream [{slot_label}]"),
                    app.focus == Focus::Stream,
                ))
                .border_style(focus_border(app.focus == Focus::Stream)),
        )
        .scroll((app.stream_scroll, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(stream, chunks[3]);

    // Composer
    let comp_title = if app.focus == Focus::Composer {
        "composer [editing]"
    } else {
        "composer  (/help)"
    };
    let composer = Paragraph::new(format!("> {}", app.composer))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(comp_title)
                .border_style(focus_border(app.focus == Focus::Composer)),
        );
    f.render_widget(composer, chunks[4]);

    // Help strip
    let help = Paragraph::new(
        "Tab panes  j/k move  1-9 slot  a approve  r reject  s ship-confirm  / commands  q quit",
    )
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, chunks[5]);

    let status = Paragraph::new(app.status_line.as_str()).style(Style::default().fg(Color::Yellow));
    f.render_widget(status, chunks[6]);
}

fn fleet_rows(full: Option<&RunState>, selected_slot: usize) -> Vec<Row<'static>> {
    let Some(st) = full else {
        return vec![Row::new(vec![
            Cell::from("(no run)"),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ])];
    };
    if st.slots.is_empty() {
        return vec![Row::new(vec![
            Cell::from("(no slots)"),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ])];
    }
    st.slots
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = if i == selected_slot {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                slot_style(s)
            };
            let wt = st
                .worktrees
                .iter()
                .find(|w| w.slot_id == s.id)
                .map(|w| truncate(&w.branch, 28))
                .or_else(|| {
                    s.cwd
                        .as_ref()
                        .map(|p| truncate(&p.display().to_string(), 28))
                })
                .unwrap_or_else(|| "-".into());
            Row::new(vec![
                Cell::from(s.id.clone()),
                Cell::from(format!("{:?}", s.role)),
                Cell::from(s.provider.clone()),
                Cell::from(format!("{:?}", s.status)),
                Cell::from(wt),
            ])
            .style(style)
        })
        .collect()
}

fn slot_style(s: &SlotState) -> Style {
    match s.status {
        state::SlotStatus::Done => Style::default().fg(Color::Green),
        state::SlotStatus::Failed | state::SlotStatus::Stuck => Style::default().fg(Color::Red),
        state::SlotStatus::Running => Style::default().fg(Color::Cyan),
        state::SlotStatus::Pending => Style::default().fg(Color::DarkGray),
    }
}

fn focus_title(name: &str, focused: bool) -> String {
    if focused {
        format!("▶ {name}")
    } else {
        name.to_string()
    }
}

fn focus_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn phase_style(phase: Phase) -> Style {
    match phase {
        Phase::Done | Phase::PlanApproved => Style::default().fg(Color::Green),
        Phase::Failed | Phase::PlanRejected => Style::default().fg(Color::Red),
        Phase::Quota => Style::default().fg(Color::Red),
        Phase::Stuck | Phase::Escalated => Style::default().fg(Color::Magenta),
        Phase::AwaitingPlanApproval | Phase::AwaitingWinnerConfirm | Phase::AwaitingShipConfirm => {
            Style::default().fg(Color::Yellow)
        }
        _ => Style::default(),
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
