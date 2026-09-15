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
    // Tell the app how much it just drew, so scrolling can be clamped to
    // content rather than running off into blank space.
    let inner_h = area.height.saturating_sub(2);
    let inner_w = area.width.saturating_sub(2) as usize;
    app.viewport.set(inner_h);

    const GUTTER: usize = 5;
    // Gutter, its space, and one reserved column for the clip marker. Without
    // the reservation the marker renders past the pane edge and is clipped
    // away — leaving truncation as silent as it was before.
    let text_w = inner_w.saturating_sub(GUTTER + 2);

    let start = app.scroll as usize;
    let end = (start + inner_h as usize).min(app.diff.len());

    let lines: Vec<Line> = app.diff[start.min(app.diff.len())..end]
        .iter()
        .map(|d| {
            let gutter = match d.lineno {
                Some(n) => format!("{n:>GUTTER$} "),
                None => " ".repeat(GUTTER + 1),
            };

            // Pan horizontally instead of wrapping: wrapping destroys the
            // indentation that makes code readable, and it decouples screen
            // rows from diff lines, which the jumps and the clamp depend on.
            let chars: Vec<char> = d.text.chars().collect();
            let from = (app.hscroll as usize).min(chars.len());
            let visible: String = chars[from..].iter().take(text_w).collect();
            // A clipped line must say so. Silently dropping the end of a line
            // means reading truncated code without knowing it.
            let clipped = chars.len() > from + text_w;

            let mut spans = vec![Span::styled(
                gutter,
                Style::default().fg(Color::DarkGray),
            )];
            spans.push(Span::styled(visible, diff_line_style(&d.text, d.ai)));
            if clipped {
                spans.push(Span::styled("›", Style::default().fg(Color::Yellow)));
            }
            Line::from(spans)
        })
        .collect();

    let position = if app.diff.is_empty() {
        String::new()
    } else {
        format!(" {}–{}/{} ", start + 1, end, app.diff.len())
    };
    let pan = if app.hscroll > 0 {
        format!(" +{} cols ", app.hscroll)
    } else {
        String::new()
    };

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" diff{position}{pan}"))
            .title_bottom(" dimmed = not AI-authored "),
    );
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

    for hint in q.hints.iter().take(app.hints_shown()) {
        top.push(Line::from(""));
        top.push(Line::from(Span::styled(
            format!("hint: {hint}"),
            Style::default().fg(Color::Yellow),
        )));
    }

    if let Some((label, feedback)) = app.verdict() {
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
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    // Grading runs in the background, so the state of the other
                    // questions is no longer implied by which one is on screen.
                    .title_bottom(format!(" {} ", app.progress())),
            )
            .wrap(Wrap { trim: false }),
        rows[0],
    );

    let (body, style) = if app.mode() == Mode::Grading {
        ("grading…".to_string(), Style::default().fg(Color::Yellow))
    } else if app.answer().is_empty() {
        (
            "press a to write your answer in $EDITOR".to_string(),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (app.answer().to_string(), Style::default())
    };

    f.render_widget(
        Paragraph::new(Span::styled(body, style))
            .block(Block::default().borders(Borders::ALL).title(" your answer "))
            .wrap(Wrap { trim: false }),
        rows[1],
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db;
    use crate::tui::App;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    const W: u16 = 100;
    const H: u16 = 30;

    pub(crate) fn question() -> db::Question {
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
        let mut app = App::new(
            vec![question()],
            // Two files, so the hand-typed one can sit outside the AI scope.
            crate::tui::build_diff_view_for_test(
                "diff --git a/src/sync.rs b/src/sync.rs\n\
                 @@ -88,1 +88,2 @@\n\
                 +AIAUTHOREDLINE\n\
                 diff --git a/src/mine.rs b/src/mine.rs\n\
                 @@ -1,1 +1,2 @@\n\
                 +HANDTYPEDLINE\n",
                &[0],
            ),
        );
        app.viewport = std::cell::Cell::new(H - 3);
        app.status = "STATUSLINE".into();
        app
    }

    pub(super) fn draw(app: &App, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, app)).unwrap();
        t.backend().buffer().clone()
    }

    /// The buffer is a grid, so a string can be split across cells; join each
    /// row and search the rows.
    pub(super) fn rows(buf: &Buffer) -> Vec<String> {
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
        a.diff = crate::tui::build_diff_view_for_test(
            "diff --git a/f.rs b/f.rs\n@@ -1,1 +1,2 @@\n+ADDEDLINE\n-REMOVEDLINE\n",
            &[0, 1],
        );
        let buf = draw(&a, W, H);
        assert_eq!(style_of(&buf, "ADDEDLINE").fg, Some(Color::Green));
        assert_eq!(style_of(&buf, "REMOVEDLINE").fg, Some(Color::Red));
    }

    #[test]
    fn hints_appear_only_once_revealed() {
        let mut a = app();
        assert!(!contains(&draw(&a, W, H), "HINTONE"));
        a.slots[0].hints_shown = 1;
        let buf = draw(&a, W, H);
        assert!(contains(&buf, "HINTONE"));
        assert!(!contains(&buf, "HINTTWO"), "revealed more hints than asked for");
    }

    /// Showing the reference before a pass turns the next attempt into recall.
    #[test]
    fn the_reference_is_hidden_until_the_answer_demonstrates_understanding() {
        let mut a = app();
        for label in [Label::Wrong, Label::Restates, Label::Partial] {
            a.slots[0].verdict = Some((label, "feedback".into()));
            assert!(
                !contains(&draw(&a, W, H), "REFERENCEANSWERTEXT"),
                "reference leaked at {}",
                label.as_str()
            );
        }
        a.slots[0].verdict = Some((Label::Demonstrates, "feedback".into()));
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
            a.slots[0].verdict = Some((label, "feedback".into()));
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
        a.slots[0].answer = "SOMEANSWER".into();
        a.slots[0].pending = Some(crate::tui::Pending {
            answer: "SOMEANSWER".into(),
            hints_used: 0,
        });
        let buf = draw(&a, W, H);
        assert!(contains(&buf, "grading"));
        assert!(!contains(&buf, "SOMEANSWER"), "answer shown while grading");
    }

    /// With grading in the background, which questions are done, outstanding
    /// or untouched is no longer implied by the one on screen.
    #[test]
    fn the_footer_states_every_questions_state() {
        let mut a = app();
        a.questions.push(question());
        a.questions.push(question());
        a.slots = vec![crate::tui::Slot::default(); 3];
        a.questions[0].status = "passed".into();
        a.slots[1].pending = Some(crate::tui::Pending {
            answer: "x".into(),
            hints_used: 0,
        });
        assert_eq!(a.progress(), "1+ 2~ 3.");
        assert!(contains(&draw(&a, W, H), "1+ 2~ 3."));
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

#[cfg(test)]
mod diff_render_tests {
    use super::tests::{draw, rows};
    use crate::tui::App;

    const LONG: &str = "diff --git a/f.rs b/f.rs\n@@ -1,1 +7,2 @@\n+ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz\n";

    fn long_app() -> App {
        let mut app = App::new(
            vec![super::tests::question()],
            crate::tui::build_diff_view_for_test(LONG, &[0]),
        );
        app.viewport = std::cell::Cell::new(10);
        app.status = "s".into();
        app
    }

    #[test]
    fn the_gutter_shows_new_side_line_numbers() {
        let buf = draw(&long_app(), 60, 12);
        assert!(
            rows(&buf).iter().any(|r| r.contains("    7 ")),
            "line number 7 missing from the gutter"
        );
    }

    /// A clipped line must announce itself. Dropping the end of a line silently
    /// means reading truncated code without knowing it.
    #[test]
    fn a_clipped_line_is_marked() {
        let buf = draw(&long_app(), 44, 12);
        assert!(rows(&buf).iter().any(|r| r.contains('›')), "no clip marker");
    }

    #[test]
    fn a_line_that_fits_is_not_marked() {
        // The diff pane is 55% of the terminal, so a 63-character line needs a
        // good deal more than 63 columns of terminal to fit.
        let buf = draw(&long_app(), 160, 12);
        assert!(!rows(&buf).iter().any(|r| r.contains('›')));
    }

    #[test]
    fn panning_reveals_the_end_of_a_long_line() {
        let mut a = long_app();
        let narrow = 44;
        assert!(!rows(&draw(&a, narrow, 12)).iter().any(|r| r.contains("vwxyz")));
        a.hscroll = 50;
        assert!(
            rows(&draw(&a, narrow, 12)).iter().any(|r| r.contains("vwxyz")),
            "panning right did not reveal the tail"
        );
    }

    #[test]
    fn the_title_reports_position_and_pan() {
        let mut a = long_app();
        assert!(rows(&draw(&a, 60, 12)).iter().any(|r| r.contains("/3")));
        a.hscroll = 16;
        assert!(rows(&draw(&a, 60, 12)).iter().any(|r| r.contains("+16 cols")));
    }

    #[test]
    fn rendering_past_the_end_does_not_panic() {
        let mut a = long_app();
        a.scroll = 9_999;
        draw(&a, 60, 12);
    }
}
