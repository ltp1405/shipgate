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
    /// Discovery is one `gh` round trip per repository, so a refresh blocks for
    /// seconds. Without a frame drawn to say so the list just stops responding,
    /// which reads as a hang rather than as work.
    pub refreshing: bool,
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

    let keys = if d.refreshing {
        "refreshing…"
    } else {
        "enter quiz · r refresh · q quit"
    };
    // Reporting only `problems[0]` silently swallowed every other unreachable
    // repository. Name the first and count the rest.
    let status = match d.problems.len() {
        0 => format!(" {keys} "),
        1 => format!(" {} · {keys} ", d.problems[0]),
        n => format!(" {} (+{} more) · {keys} ", d.problems[0], n - 1),
    };
    let bg = if d.refreshing { Color::Yellow } else { Color::Cyan };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Black).bg(bg),
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
                    d.refreshing = true;
                    terminal.draw(|f| render(f, &d, &mut state))?;
                    let reloaded = reload();
                    d.refreshing = false;
                    let (rows, problems) = reloaded?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, buffer::Buffer};
    use std::path::PathBuf;

    fn draw(d: &Dash, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut state = ListState::default();
        state.select(Some(d.selected));
        t.draw(|f| render(f, d, &mut state)).unwrap();
        t.backend().buffer().clone()
    }

    /// The buffer is a grid, so a string can be split across cells; join each
    /// row and search the rows.
    fn rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect()
            })
            .collect()
    }

    fn dash(problems: Vec<String>, refreshing: bool) -> Dash {
        Dash {
            rows: vec![Row {
                repo: "o/tapir".into(),
                path: PathBuf::from("/tmp"),
                pr_number: 42,
                branch: "b".into(),
                title: "Cache the thing".into(),
                status: Status::Ungated,
                coverage: None,
            }],
            problems,
            selected: 0,
            refreshing,
        }
    }

    #[test]
    fn the_list_names_the_repo_the_number_and_the_title() {
        let text = rows(&draw(&dash(vec![], false), 100, 6)).join("\n");
        assert!(text.contains("tapir"), "{text}");
        assert!(text.contains("#42"), "{text}");
        assert!(text.contains("Cache the thing"), "{text}");
        assert!(text.contains("no gate"), "{text}");
    }

    #[test]
    fn the_title_counts_what_is_waiting_on_you() {
        let text = rows(&draw(&dash(vec![], false), 100, 6)).join("\n");
        assert!(text.contains("1 waiting on you"), "{text}");
    }

    /// Discovery is one `gh` round trip per repository, so a refresh blocks for
    /// seconds. Without this frame the list simply stops responding, which
    /// reads as a hang.
    #[test]
    fn a_refresh_in_flight_says_so_instead_of_looking_hung() {
        let text = rows(&draw(&dash(vec![], true), 100, 6)).join("\n");
        assert!(text.contains("refreshing"), "{text}");
    }

    #[test]
    fn the_keys_are_listed_when_nothing_is_in_flight() {
        let text = rows(&draw(&dash(vec![], false), 100, 6)).join("\n");
        assert!(text.contains("enter quiz"), "{text}");
        assert!(!text.contains("refreshing"), "{text}");
    }

    /// Reporting only the first problem silently swallowed every other
    /// unreachable repository.
    #[test]
    fn unreachable_repositories_beyond_the_first_are_counted_not_dropped() {
        let d = dash(
            vec!["a: boom".into(), "b: boom".into(), "c: boom".into()],
            false,
        );
        let text = rows(&draw(&d, 100, 6)).join("\n");
        assert!(text.contains("a: boom"), "{text}");
        assert!(text.contains("+2 more"), "{text}");
    }

    #[test]
    fn a_lone_problem_is_not_given_a_count() {
        let text = rows(&draw(&dash(vec!["a: boom".into()], false), 100, 6)).join("\n");
        assert!(text.contains("a: boom"), "{text}");
        assert!(!text.contains("more"), "{text}");
    }

    #[test]
    fn an_empty_dashboard_renders_rather_than_panicking() {
        let mut d = dash(vec![], false);
        d.rows.clear();
        let text = rows(&draw(&d, 100, 6)).join("\n");
        assert!(text.contains("0 waiting on you"), "{text}");
    }

    #[test]
    fn survives_a_cramped_terminal() {
        draw(&dash(vec!["x: boom".into()], true), 20, 4);
    }

    #[test]
    fn a_long_title_is_truncated_with_an_ellipsis() {
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
