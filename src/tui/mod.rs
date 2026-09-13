//! §12 — the quiz screen.
//!
//! Two panes: the diff on the left, the question and your answer on the right.
//! The diff is not a later addition. These questions are open book by design —
//! the code is days old by the time a PR is ready, and an open-book quiz without
//! the book is a memory test on code you wrote last week.

mod quiz;

use crate::{db, git, llm};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use rusqlite::Connection;
use std::io::Stdout;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

/// Results arrive from a worker thread. A judge call takes tens of seconds and
/// must never run on the UI thread — that freezes the terminal with no redraw
/// and no Ctrl-C.
pub enum AppEvent {
    Graded {
        question_id: i64,
        label: llm::Label,
        feedback: String,
    },
    Failed {
        question_id: i64,
        error: String,
    },
}

#[derive(PartialEq)]
pub enum Mode {
    /// Typing an answer.
    Answering,
    /// A judge call is in flight.
    Grading,
    /// Verdict shown; space moves on.
    Reviewing,
}

pub struct App {
    pub questions: Vec<db::Question>,
    pub current: usize,
    pub answer: String,
    pub mode: Mode,
    pub hints_shown: usize,
    pub verdict: Option<(llm::Label, String)>,
    pub scores: Vec<f64>,
    /// The diff, split into lines, with a flag for AI-authored hunks.
    pub diff: Vec<(String, bool)>,
    pub scroll: u16,
    pub status: String,
    pub quit: bool,
}

impl App {
    pub fn question(&self) -> Option<&db::Question> {
        self.questions.get(self.current)
    }

    /// Scroll the diff to the hunk this question is about, so the reader does
    /// not have to hunt for it.
    ///
    /// The search is anchored to the question's file first. Matching the `@@`
    /// header alone jumps to whichever file happens to share that header — with
    /// headers as common as `@@ -1,1 +1,2 @@` that is routinely the wrong one.
    fn jump_to_anchor(&mut self, hunks: &[git::Hunk]) {
        let Some(q) = self.questions.get(self.current) else { return };
        let Some(hunk) = hunks.iter().find(|h| h.anchor == q.anchor) else { return };

        let file_start = self
            .diff
            .iter()
            .position(|(l, _)| l.starts_with("diff --git") && l.ends_with(&format!("b/{}", q.file)))
            .unwrap_or(0);

        let found = self.diff[file_start..]
            .iter()
            .position(|(l, _)| l == &hunk.header)
            .map(|offset| file_start + offset);

        if let Some(idx) = found {
            self.scroll = idx.saturating_sub(3) as u16;
        }
    }
}

/// Mark each diff line with whether its hunk is AI-authored.
///
/// Keyed on `(file, header)`, not the header alone: `@@ -1,1 +1,2 @@` recurs
/// across files, so matching on the header by itself marks unrelated hunks in
/// other files as AI-authored.
fn build_diff_view(diff: &str, ai_hunks: &[git::Hunk]) -> Vec<(String, bool)> {
    let ai: std::collections::HashSet<(&str, &str)> = ai_hunks
        .iter()
        .map(|h| (h.file.as_str(), h.header.as_str()))
        .collect();

    let mut out = Vec::new();
    let mut file = String::new();
    let mut in_ai_hunk = false;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_ai_hunk = false;
            file = rest
                .split(" b/")
                .nth(1)
                .unwrap_or_else(|| rest.trim_start_matches("a/"))
                .to_string();
        } else if line.starts_with("@@") {
            in_ai_hunk = ai.contains(&(file.as_str(), line));
        }
        out.push((line.to_string(), in_ai_hunk));
    }
    out
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup() -> Result<Term> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Drop the terminal out of raw mode, run $EDITOR on the answer, then restore.
/// A terminal textarea is a poor place to compose technical prose.
fn edit_externally(terminal: &mut Term, current: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("shipgate-answer-{}.md", std::process::id()));
    std::fs::write(&path, current)?;

    restore(terminal)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status();
    *terminal = setup()?;
    terminal.clear()?;

    match status {
        Ok(s) if s.success() => Ok(std::fs::read_to_string(&path)?.trim().to_string()),
        _ => Ok(current.to_string()),
    }
}

/// Returns the per-question scores, in question order.
pub fn run(
    conn: &Connection,
    judge: Arc<dyn llm::Judge + Send + Sync>,
    diff: Arc<String>,
    ai_hunks: &[git::Hunk],
    questions: Vec<db::Question>,
) -> Result<Vec<f64>> {
    let total = questions.len();
    let mut app = App {
        questions,
        current: 0,
        answer: String::new(),
        mode: Mode::Answering,
        hints_shown: 0,
        verdict: None,
        scores: vec![0.0; total],
        diff: build_diff_view(&diff, ai_hunks),
        scroll: 0,
        status: "a/e answer · h hint · ^s submit · j/k scroll · q quit".into(),
        quit: false,
    };
    app.jump_to_anchor(ai_hunks);

    let (tx, rx): (Sender<AppEvent>, Receiver<AppEvent>) = mpsc::channel();
    let mut terminal = setup()?;

    let result = event_loop(&mut terminal, &mut app, conn, &judge, &diff, ai_hunks, &tx, &rx);
    restore(&mut terminal)?;
    result?;
    Ok(app.scores)
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    conn: &Connection,
    judge: &Arc<dyn llm::Judge + Send + Sync>,
    diff: &Arc<String>,
    ai_hunks: &[git::Hunk],
    tx: &Sender<AppEvent>,
    rx: &Receiver<AppEvent>,
) -> Result<()> {
    loop {
        terminal.draw(|f| quiz::render(f, app))?;

        // Non-blocking: results from the worker, then key input.
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Graded { question_id, label, feedback } => {
                    if let Some(i) = app.questions.iter().position(|q| q.id == question_id) {
                        app.scores[i] = label.score();
                    }
                    db::record_attempt(
                        conn, question_id, "answer", &app.answer, app.hints_shown as i64,
                        label.as_str(), label.score(), &feedback, judge.model(),
                    )?;
                    app.verdict = Some((label, feedback));
                    app.mode = Mode::Reviewing;
                    app.status = "space next · e revise · q quit".into();
                }
                AppEvent::Failed { error, .. } => {
                    app.mode = Mode::Answering;
                    app.status = format!("grading failed: {error}");
                }
            }
        }

        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Grading is in flight: accept nothing but quit.
        if app.mode == Mode::Grading {
            if key.code == KeyCode::Char('q') || is_ctrl_c(&key) {
                return Ok(());
            }
            continue;
        }

        match (key.code, app.mode == Mode::Reviewing) {
            (KeyCode::Char('q'), _) => return Ok(()),
            _ if is_ctrl_c(&key) => return Ok(()),

            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                app.scroll = app.scroll.saturating_add(1)
            }
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                app.scroll = app.scroll.saturating_sub(1)
            }
            (KeyCode::PageDown, _) => app.scroll = app.scroll.saturating_add(20),
            (KeyCode::PageUp, _) => app.scroll = app.scroll.saturating_sub(20),

            // Next question.
            (KeyCode::Char(' '), true) => {
                if app.current + 1 >= app.questions.len() {
                    return Ok(());
                }
                app.current += 1;
                app.answer.clear();
                app.hints_shown = 0;
                app.verdict = None;
                app.mode = Mode::Answering;
                app.status = "a/e answer · h hint · ^s submit · j/k scroll · q quit".into();
                app.jump_to_anchor(ai_hunks);
            }

            // Reveal the next hint. Tier 3 is half the answer, so stop there.
            (KeyCode::Char('h'), false) => {
                let available = app.question().map(|q| q.hints.len()).unwrap_or(0);
                if app.hints_shown < available {
                    app.hints_shown += 1;
                } else {
                    app.status = "no more hints".into();
                }
            }

            (KeyCode::Char('a'), _) | (KeyCode::Char('e'), _) => {
                app.answer = edit_externally(terminal, &app.answer)?;
                if !app.answer.is_empty() {
                    app.mode = Mode::Answering;
                    app.verdict = None;
                    app.status = "^s submit · e revise · h hint · q quit".into();
                }
            }

            (KeyCode::Char('s'), false) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                submit(app, judge, diff, ai_hunks, tx);
            }

            _ => {}
        }
    }
}

fn is_ctrl_c(key: &event::KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Spawn the judge call. The §8 precheck runs first, on this thread, because it
/// costs nothing and needs no network.
fn submit(
    app: &mut App,
    judge: &Arc<dyn llm::Judge + Send + Sync>,
    diff: &Arc<String>,
    ai_hunks: &[git::Hunk],
    tx: &Sender<AppEvent>,
) {
    let Some(q) = app.question().cloned() else { return };
    if app.answer.trim().is_empty() {
        app.status = "nothing to submit — press a to write an answer".into();
        return;
    }

    if !llm::cites_the_diff(&app.answer, ai_hunks) {
        let _ = tx.send(AppEvent::Graded {
            question_id: q.id,
            label: llm::Label::Wrong,
            feedback: "Cites nothing from the diff — name an identifier, file or line.".into(),
        });
        app.mode = Mode::Grading;
        return;
    }

    app.mode = Mode::Grading;
    app.status = "grading…".into();

    let tx = tx.clone();
    let answer = app.answer.clone();
    let text = q.text.clone();
    let id = q.id;
    let judge = Arc::clone(judge);
    let diff = Arc::clone(diff);

    // Detached, not scoped. `std::thread::scope` joins before returning, which
    // would block the event loop for the whole judge call — the freeze this
    // whole arrangement exists to avoid.
    std::thread::spawn(move || {
        let ev = match judge.judge(&diff, &text, &answer) {
            Ok(v) => AppEvent::Graded {
                question_id: id,
                label: v.label,
                feedback: v.feedback,
            },
            Err(e) => AppEvent::Failed {
                question_id: id,
                error: e.to_string(),
            },
        };
        let _ = tx.send(ev);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two files whose hunks share the identical `@@` header — the case that
    /// header-only matching gets wrong.
    const DIFF: &str = "\
diff --git a/src/ai.rs b/src/ai.rs
--- a/src/ai.rs
+++ b/src/ai.rs
@@ -1,1 +1,2 @@
+let written_by_the_model = 1;
diff --git a/src/mine.rs b/src/mine.rs
--- a/src/mine.rs
+++ b/src/mine.rs
@@ -1,1 +1,2 @@
+let typed_by_hand = 2;
";

    fn ai_only() -> Vec<git::Hunk> {
        git::parse_diff(DIFF)
            .into_iter()
            .filter(|h| h.file == "src/ai.rs")
            .collect()
    }

    #[test]
    fn marks_only_the_ai_authored_file() {
        let view = build_diff_view(DIFF, &ai_only());
        let marked: Vec<&String> = view
            .iter()
            .filter(|(_, ai)| *ai)
            .map(|(l, _)| l)
            .collect();
        assert!(marked.iter().any(|l| l.contains("written_by_the_model")));
        assert!(
            !marked.iter().any(|l| l.contains("typed_by_hand")),
            "an identical @@ header in another file must not be marked"
        );
    }

    #[test]
    fn a_file_header_ends_the_previous_hunk() {
        let view = build_diff_view(DIFF, &ai_only());
        let second_file = view
            .iter()
            .find(|(l, _)| l.contains("diff --git a/src/mine.rs"))
            .unwrap();
        assert!(!second_file.1);
    }

    #[test]
    fn every_diff_line_is_kept() {
        assert_eq!(build_diff_view(DIFF, &ai_only()).len(), DIFF.lines().count());
    }

    #[test]
    fn jumping_scrolls_to_the_questions_hunk() {
        let hunks = git::parse_diff(DIFF);
        let target = hunks.iter().find(|h| h.file == "src/mine.rs").unwrap();
        let mut app = App {
            questions: vec![db::Question {
                id: 1,
                kind: "prediction".into(),
                file: target.file.clone(),
                anchor: target.anchor.clone(),
                text: "q".into(),
                reference: "r".into(),
                hints: vec![],
                status: "open".into(),
                label: None,
                score: None,
            }],
            current: 0,
            answer: String::new(),
            mode: Mode::Answering,
            hints_shown: 0,
            verdict: None,
            scores: vec![0.0],
            diff: build_diff_view(DIFF, &hunks),
            scroll: 0,
            status: String::new(),
            quit: false,
        };
        app.jump_to_anchor(&hunks);

        // Must land on src/mine.rs's hunk, not on src/ai.rs's identical header.
        let landed = &app.diff[app.scroll as usize..]
            .iter()
            .take(8)
            .map(|(l, _)| l.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            landed.contains("typed_by_hand"),
            "jumped to the wrong file; saw:\n{landed}"
        );
    }
}
