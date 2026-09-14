//! The dashboard: every PR waiting on you, across every repository.

use crate::dash::{Row, Status};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use std::time::Duration;

pub struct Dash {
    pub rows: Vec<Row>,
    pub problems: Vec<String>,
    pub selected: usize,
}

/// What the dashboard hands back to the caller.
pub enum Chosen {
    Quiz(Row),
    Quit,
}

fn status_style(s: &Status) -> Style {
    match s {
        Status::Ungated => Style::default().fg(Color::Yellow),
        Status::InProgress { .. } => Style::default().fg(Color::Cyan),
        // Finished work is dimmed, not hidden: visible enough to confirm, quiet
        // enough not to compete with what still wants you.
        Status::Cleared | Status::Trivial => {
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
        }
    }
}

fn short_repo(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

pub fn render(f: &mut Frame, d: &Dash, state: &mut ListState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    let items: Vec<ListItem> = d
        .rows
        .iter()
        .map(|r| {
            let style = status_style(&r.status);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<16}", short_repo(&r.repo)), style),
                Span::styled(format!("#{:<6}", r.pr_number), style),
                Span::styled(format!("{:<38}", truncate(&r.title, 37)), style),
                Span::styled(format!("{:<15}", r.status.label()), style),
                Span::styled(
                    r.coverage.clone().unwrap_or_default(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let waiting = d.rows.iter().filter(|r| r.status.needs_you()).count();
    let title = format!(" shipgate · {waiting} waiting on you · {} PRs ", d.rows.len());

    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▸ "),
        chunks[0],
        state,
    );

    let status = if d.problems.is_empty() {
        " enter quiz · r refresh · q quit ".to_string()
    } else {
        format!(" {} · enter quiz · r refresh · q quit ", d.problems[0])
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ))),
        chunks[1],
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

/// Runs the list and returns what the user picked. Refreshing re-runs discovery,
/// which is why the reload closure is passed in rather than called here.
pub fn run(
    mut d: Dash,
    mut reload: impl FnMut() -> Result<(Vec<Row>, Vec<String>)>,
) -> Result<Chosen> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let mut state = ListState::default();
    state.select(Some(0));

    let result = (|| -> Result<Chosen> {
        loop {
            terminal.draw(|f| render(f, &d, &mut state))?;

            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl_c = key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL);

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Chosen::Quit),
                _ if ctrl_c => return Ok(Chosen::Quit),

                KeyCode::Char('j') | KeyCode::Down => {
                    if !d.rows.is_empty() {
                        d.selected = (d.selected + 1).min(d.rows.len() - 1);
                        state.select(Some(d.selected));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    d.selected = d.selected.saturating_sub(1);
                    state.select(Some(d.selected));
                }
                KeyCode::Char('r') => {
                    let (rows, problems) = reload()?;
                    d.rows = rows;
                    d.problems = problems;
                    d.selected = d.selected.min(d.rows.len().saturating_sub(1));
                    state.select(Some(d.selected));
                }
                KeyCode::Enter => {
                    if let Some(row) = d.rows.get(d.selected) {
                        return Ok(Chosen::Quiz(row.clone()));
                    }
                }
                _ => {}
            }
        }
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}
