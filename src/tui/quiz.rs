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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::tui::{App, Mode};
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    const W: u16 = 100;
    const H: u16 = 30;

    fn question() -> db::Question {
        db::Question {
            id: 1,
            kind: "prediction".into(),
            file: "src/sync.rs".into(),
            anchor: "sha256:x".into(),
            text: "WHATDOESTHECALLEROBSERVE".into(),
            reference: "REFERENCEANSWERTEXT".into(),
            hints: vec!["HINTONE".into(), "HINTTWO".into()],
            status: "open".into(),
            label: None,
            score: None,
        }
    }

    fn app() -> App {
        App {
            questions: vec![question()],
            current: 0,
            answer: String::new(),
            mode: Mode::Answering,
            hints_shown: 0,
            verdict: None,
            scores: vec![0.0],
            diff: vec![
                ("@@ -88,1 +88,2 @@".into(), true),
                ("+AIAUTHOREDLINE".into(), true),
                ("+HANDTYPEDLINE".into(), false),
            ],
            scroll: 0,
            status: "STATUSLINE".into(),
            quit: false,
        }
    }

    fn draw(app: &App, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, app)).unwrap();
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

    fn contains(buf: &Buffer, needle: &str) -> bool {
        rows(buf).iter().any(|r| r.contains(needle))
    }

    /// Style of the first cell of the first row containing `needle`.
    ///
    /// The column is counted in characters, not bytes. Rows carry box-drawing
    /// borders at three bytes each, so a byte offset lands several cells to the
    /// right of the text — far enough, past a short label, to read a blank cell
    /// and report the wrong style.
    fn style_of(buf: &Buffer, needle: &str) -> ratatui::style::Style {
        let rows = rows(buf);
        let y = rows.iter().position(|r| r.contains(needle)).expect(needle);
        let byte = rows[y].find(needle).unwrap();
        let x = rows[y][..byte].chars().count() as u16;
        let c = buf.cell((x, y as u16)).unwrap();
        Style::default().fg(c.fg).bg(c.bg).add_modifier(c.modifier)
    }

    #[test]
    fn renders_the_question_the_diff_and_the_status_line() {
        let buf = draw(&app(), W, H);
        assert!(contains(&buf, "WHATDOESTHECALLEROBSERVE"), "question missing");
        assert!(contains(&buf, "AIAUTHOREDLINE"), "diff missing");
        assert!(contains(&buf, "STATUSLINE"), "status line missing");
        assert!(contains(&buf, "your answer"), "answer pane missing");
    }

    #[test]
    fn the_header_names_the_question_kind_and_file() {
        let buf = draw(&app(), W, H);
        assert!(contains(&buf, "1/1"));
        assert!(contains(&buf, "prediction"));
        assert!(contains(&buf, "src/sync.rs"));
    }

    /// Hunks outside the AI-authored set are dimmed, not hidden — context, but
    /// not the subject of the question.
    #[test]
    fn non_ai_hunks_are_dimmed_and_ai_hunks_are_not() {
        let buf = draw(&app(), W, H);
        assert!(
            style_of(&buf, "HANDTYPEDLINE").add_modifier.contains(Modifier::DIM),
            "hand-typed line should be dimmed"
        );
        assert!(
            !style_of(&buf, "AIAUTHOREDLINE").add_modifier.contains(Modifier::DIM),
            "AI-authored line should not be dimmed"
        );
    }

    #[test]
    fn added_and_removed_lines_are_coloured_differently() {
        let mut a = app();
        a.diff = vec![("+ADDEDLINE".into(), true), ("-REMOVEDLINE".into(), true)];
        let buf = draw(&a, W, H);
        assert_eq!(style_of(&buf, "ADDEDLINE").fg, Some(Color::Green));
        assert_eq!(style_of(&buf, "REMOVEDLINE").fg, Some(Color::Red));
    }

    #[test]
    fn hints_appear_only_once_revealed() {
        let mut a = app();
        assert!(!contains(&draw(&a, W, H), "HINTONE"));
        a.hints_shown = 1;
        let buf = draw(&a, W, H);
        assert!(contains(&buf, "HINTONE"));
        assert!(!contains(&buf, "HINTTWO"), "revealed more hints than asked for");
    }

    /// Showing the reference before a pass turns the next attempt into recall.
    #[test]
    fn the_reference_is_hidden_until_the_answer_demonstrates_understanding() {
        let mut a = app();
        for label in [Label::Wrong, Label::Restates, Label::Partial] {
            a.verdict = Some((label, "feedback".into()));
            assert!(
                !contains(&draw(&a, W, H), "REFERENCEANSWERTEXT"),
                "reference leaked at {}",
                label.as_str()
            );
        }
        a.verdict = Some((Label::Demonstrates, "feedback".into()));
        assert!(contains(&draw(&a, W, H), "REFERENCEANSWERTEXT"));
    }

    #[test]
    fn each_verdict_gets_its_own_colour() {
        let mut a = app();
        for (label, expected) in [
            (Label::Demonstrates, Color::Green),
            (Label::Partial, Color::Yellow),
            (Label::Restates, Color::Magenta),
            (Label::Wrong, Color::Red),
        ] {
            a.verdict = Some((label, "feedback".into()));
            let buf = draw(&a, W, H);
            assert_eq!(
                style_of(&buf, label.as_str()).fg,
                Some(expected),
                "wrong colour for {}",
                label.as_str()
            );
        }
    }

    #[test]
    fn a_call_in_flight_is_visible_in_the_answer_pane() {
        let mut a = app();
        a.answer = "SOMEANSWER".into();
        a.mode = Mode::Grading;
        let buf = draw(&a, W, H);
        assert!(contains(&buf, "grading"));
        assert!(!contains(&buf, "SOMEANSWER"), "answer shown while grading");
    }

    #[test]
    fn an_empty_answer_pane_says_how_to_write_one() {
        assert!(contains(&draw(&app(), W, H), "$EDITOR"));
    }

    #[test]
    fn scrolling_moves_the_diff() {
        let mut a = app();
        assert!(contains(&draw(&a, W, H), "@@ -88,1 +88,2 @@"));
        a.scroll = 2;
        assert!(!contains(&draw(&a, W, H), "@@ -88,1 +88,2 @@"));
    }

    /// Layout must not panic on a small terminal.
    #[test]
    fn survives_a_cramped_terminal() {
        for (w, h) in [(20, 6), (40, 10), (200, 60), (10, 3)] {
            draw(&app(), w, h);
        }
    }

    #[test]
    fn renders_nothing_rather_than_panicking_with_no_questions() {
        let mut a = app();
        a.questions.clear();
        draw(&a, W, H);
    }
}
