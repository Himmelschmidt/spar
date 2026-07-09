use crate::paths::{self, SwarmPaths};
use crate::quota::QuotaStore;
use crate::state::{self, Phase};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::io::stdout;
use std::time::Duration;

pub fn run() -> Result<crate::exit_codes::ExitCode> {
    let root = paths::find_project_root()?;
    let swarm = SwarmPaths::new(&root);

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let result = run_loop(&mut terminal, &swarm);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    swarm: &SwarmPaths,
) -> Result<crate::exit_codes::ExitCode> {
    let mut selected = 0usize;
    loop {
        let runs = state::list_runs(swarm).unwrap_or_default();
        let quota = QuotaStore::load(swarm).unwrap_or_default();

        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(8),
                    Constraint::Length(2),
                ])
                .split(area);

            let header = Paragraph::new(format!(
                "agent-swarm dashboard  project={}",
                swarm.project_root.display()
            ))
            .block(Block::default().borders(Borders::ALL).title("swarm"));
            f.render_widget(header, chunks[0]);

            let rows: Vec<Row> = runs
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let phase = format!("{:?}", r.phase);
                    let style = if i == selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        phase_style(r.phase)
                    };
                    Row::new(vec![
                        Cell::from(r.id.clone()),
                        Cell::from(format!("{:?}", r.workflow)),
                        Cell::from(phase),
                        Cell::from(r.updated_at.format("%H:%M:%S").to_string()),
                    ])
                    .style(style)
                })
                .collect();
            let table = Table::new(
                rows,
                [
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(24),
                    Constraint::Length(10),
                ],
            )
            .header(
                Row::new(vec!["run", "workflow", "phase", "updated"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(Block::default().borders(Borders::ALL).title("runs"));
            f.render_widget(table, chunks[1]);

            let mut qlines = vec!["providers:".into()];
            if quota.providers.is_empty() {
                qlines.push("  (no pauses recorded)".into());
            } else {
                for (name, q) in &quota.providers {
                    qlines.push(format!("  {name}: {:?}", q.status));
                }
            }
            let detail = if let Some(r) = runs.get(selected) {
                if let Ok(full) = state::RunState::load(swarm, &r.id) {
                    format!(
                        "selected {}  slots={}  dry_run={}  error={:?}",
                        full.id,
                        full.slots.len(),
                        full.dry_run,
                        full.error
                    )
                } else {
                    String::new()
                }
            } else {
                "no runs".into()
            };
            qlines.push(detail);
            let quota_p = Paragraph::new(qlines.join("\n"))
                .block(Block::default().borders(Borders::ALL).title("detail"));
            f.render_widget(quota_p, chunks[2]);

            let help = Paragraph::new("j/k or ↑↓ move  r refresh  q quit");
            f.render_widget(help, chunks[3]);
        })?;

        if event::poll(Duration::from_millis(500))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        if !runs.is_empty() {
                            selected = (selected + 1).min(runs.len() - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Char('r') => {}
                    _ => {}
                }
            }
        }
        let n = state::list_runs(swarm).map(|r| r.len()).unwrap_or(0);
        if n > 0 {
            selected = selected.min(n - 1);
        } else {
            selected = 0;
        }
    }
    Ok(crate::exit_codes::ExitCode::Success)
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
