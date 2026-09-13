//! Rendering. Diff left, question and answer right.

use super::{App, Mode};
use crate::llm::Label;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

fn diff_line_style(line: &str, ai: bool) -> Style {
    let base = if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with("diff --git") || line.starts_with("index ") {
        Style::default().fg(Color::DarkGray)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Gray)
    };
    // Hunks outside the AI-authored set are dimmed rather than hidden — they are
    // context for the question, but they are not what is being asked about.
    if ai {
        base
    } else {
        base.add_modifier(Modifier::DIM)
    }
}

fn label_style(label: Label) -> Style {
    let c = match label {
        Label::Demonstrates => Color::Green,
        Label::Partial => Color::Yellow,
        Label::Restates => Color::Magenta,
        Label::Wrong => Color::Red,
    };
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

pub fn render(f: &mut Frame, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(f.area());

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(outer[0]);

    render_diff(f, app, panes[0]);
    render_question(f, app, panes[1]);

    let status = Paragraph::new(Line::from(Span::styled(
        format!(" {}", app.status),
        Style::default().fg(Color::Black).bg(Color::Cyan),
    )));
    f.render_widget(status, outer[1]);
}

fn render_diff(f: &mut Frame, app: &App, area: Rect) {
    let lines: Vec<Line> = app
        .diff
        .iter()
        .map(|(l, ai)| Line::from(Span::styled(l.clone(), diff_line_style(l, *ai))))
        .collect();

    let title = format!(
        " diff · {} lines · dimmed hunks are not AI-authored ",
        app.diff.len()
    );
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((app.scroll, 0));
    f.render_widget(p, area);
}

fn render_question(f: &mut Frame, app: &App, area: Rect) {
    let Some(q) = app.question() else { return };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(45),
            Constraint::Percentage(55),
        ])
        .split(area);

    let mut top: Vec<Line> = Vec::new();
    top.push(Line::from(Span::styled(
        q.text.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));

    for hint in q.hints.iter().take(app.hints_shown) {
        top.push(Line::from(""));
        top.push(Line::from(Span::styled(
            format!("hint: {hint}"),
            Style::default().fg(Color::Yellow),
        )));
    }

    if let Some((label, feedback)) = &app.verdict {
        top.push(Line::from(""));
        top.push(Line::from(Span::styled(label.as_str(), label_style(*label))));
        top.push(Line::from(Span::styled(
            feedback.clone(),
            Style::default().fg(Color::Gray),
        )));
        // The reference is revealed only on a pass or after a second failed
        // attempt — showing it earlier turns the next attempt into recall.
        if *label == Label::Demonstrates {
            top.push(Line::from(""));
            top.push(Line::from(Span::styled(
                format!("reference: {}", q.reference),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let title = format!(
        " {}/{} · {} · {} ",
        app.current + 1,
        app.questions.len(),
        q.kind,
        q.file
    );
    f.render_widget(
        Paragraph::new(top)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        rows[0],
    );

    let (body, style) = if app.mode == Mode::Grading {
        ("grading…".to_string(), Style::default().fg(Color::Yellow))
    } else if app.answer.is_empty() {
        (
            "press a to write your answer in $EDITOR".to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (app.answer.clone(), Style::default())
    };

    f.render_widget(
        Paragraph::new(Span::styled(body, style))
            .block(Block::default().borders(Borders::ALL).title(" your answer "))
            .wrap(Wrap { trim: false }),
        rows[1],
    );
}
