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

#[derive(Debug, PartialEq)]
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

/// Where key events come from. The real loop reads the terminal; tests feed a
/// scripted sequence, which is what makes the event loop testable at all.
pub trait Input {
    /// `None` means nothing is ready yet — the loop should keep polling.
    /// The app is passed so a scripted source can wait for a call in flight the
    /// way a person does, rather than racing it.
    fn next(&mut self, app: &App) -> Result<Option<event::KeyEvent>>;
}

pub struct TerminalInput;

impl Input for TerminalInput {
    fn next(&mut self, _app: &App) -> Result<Option<event::KeyEvent>> {
        if !event::poll(Duration::from_millis(16))? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => Ok(Some(k)),
            _ => Ok(None),
        }
    }
}

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

    let result = event_loop(
        &mut Editor::Terminal(&mut terminal),
        &mut TerminalInput,
        &mut app,
        conn,
        &judge,
        &diff,
        ai_hunks,
        &tx,
        &rx,
    );
    restore(&mut terminal)?;
    result?;
    Ok(app.scores)
}

#[allow(clippy::too_many_arguments)]
/// How the loop draws and shells out to $EDITOR. Headless runs do neither.
pub enum Editor<'a> {
    Terminal(&'a mut Term),
    Headless,
}

impl Editor<'_> {
    fn draw(&mut self, app: &App) -> Result<()> {
        if let Editor::Terminal(t) = self {
            t.draw(|f| quiz::render(f, app))?;
        }
        Ok(())
    }

    fn edit(&mut self, current: &str) -> Result<String> {
        match self {
            Editor::Terminal(t) => edit_externally(t, current),
            // Headless: stand in for what the editor would have produced, so the
            // answer path can be driven without spawning one.
            Editor::Headless => Ok(if current.is_empty() {
                "retry_count in sync.rs:88 is never decremented, so the caller \
                 sees Err(Transient) on the first failure."
                    .to_string()
            } else {
                current.to_string()
            }),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    ui: &mut Editor,
    input: &mut dyn Input,
    app: &mut App,
    conn: &Connection,
    judge: &Arc<dyn llm::Judge + Send + Sync>,
    diff: &Arc<String>,
    ai_hunks: &[git::Hunk],
    tx: &Sender<AppEvent>,
    rx: &Receiver<AppEvent>,
) -> Result<()> {
    loop {
        ui.draw(app)?;

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

        let Some(key) = input.next(app)? else { continue };

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
                app.answer = ui.edit(&app.answer)?;
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

/// Drive the loop with scripted keys and no terminal. Returns the final app so
/// tests can assert on what the loop did.
#[cfg(test)]
pub fn run_headless(
    conn: &Connection,
    judge: Arc<dyn llm::Judge + Send + Sync>,
    diff: Arc<String>,
    ai_hunks: &[git::Hunk],
    questions: Vec<db::Question>,
    keys: Vec<event::KeyEvent>,
    // `false` fires keys regardless of a call in flight, to prove the loop
    // drops them rather than acting on them late.
    wait_for_grading: bool,
) -> Result<App> {
    struct Scripted {
        keys: std::vec::IntoIter<event::KeyEvent>,
        polls: usize,
        wait_for_grading: bool,
    }

    impl Input for Scripted {
        fn next(&mut self, app: &App) -> Result<Option<event::KeyEvent>> {
            // A judge call is in flight. Hold, as a person would, so the verdict
            // is processed before the next key is delivered.
            if app.mode == Mode::Grading && self.wait_for_grading {
                self.polls += 1;
                if self.polls > 20_000 {
                    anyhow::bail!("a scripted judge call never returned");
                }
                std::thread::sleep(Duration::from_millis(1));
                return Ok(None);
            }
            self.polls = 0;
            match self.keys.next() {
                Some(k) => Ok(Some(k)),
                // Out of keys. If something is still in flight, hold for it so
                // the verdict lands before the loop stops.
                None if app.mode == Mode::Grading => {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(None)
                }
                None => Ok(Some(event::KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::NONE,
                ))),
            }
        }
    }

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
        status: String::new(),
        quit: false,
    };
    let (tx, rx) = mpsc::channel();
    event_loop(
        &mut Editor::Headless,
        &mut Scripted { keys: keys.into_iter(), polls: 0, wait_for_grading },
        &mut app,
        conn,
        &judge,
        &diff,
        ai_hunks,
        &tx,
        &rx,
    )?;
    Ok(app)
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

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::llm::Label;
    use std::time::Duration;

    fn key(c: char) -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    const DIFF: &str = "\
diff --git a/sync.rs b/sync.rs
--- a/sync.rs
+++ b/sync.rs
@@ -88,1 +88,2 @@
+    let retry_count = 3;
";

    /// Tests run in parallel, so every fixture needs its own database file.
    /// Keying on the process id alone collides between tests that ask for the
    /// same number of questions.
    static FIXTURE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn fixture(n: usize) -> (rusqlite::Connection, Vec<git::Hunk>, Vec<db::Question>) {
        let seq = FIXTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = std::env::temp_dir()
            .join(format!("shipgate-tui-{}-{seq}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = db::open_at(&path).unwrap();

        let hunks = git::parse_diff(DIFF);
        let gate = db::upsert_gate(
            &conn,
            &db::NewGate {
                repo: "o/r", pr_number: 1, branch: "b", base_ref: "origin/main",
                base_sha: "a", head_sha: "b", diff: DIFF,
                hunks_total: 1, hunks_ai: 1, authorship: "trailers", state: "open",
            },
        )
        .unwrap();

        let qs: Vec<db::NewQuestion> = (0..n)
            .map(|_| db::NewQuestion {
                kind: "prediction", file: "sync.rs", anchor: &hunks[0].anchor,
                text: "what does the caller observe?", reference: "the reference",
                hints: &[],
            })
            .collect();
        db::insert_questions(&conn, gate, &qs).unwrap();
        let questions = db::questions_for(&conn, gate).unwrap();
        (conn, hunks, questions)
    }

    fn drive(
        n: usize,
        judge: llm::stub::StubJudge,
        keys: Vec<event::KeyEvent>,
    ) -> App {
        let (conn, hunks, questions) = fixture(n);
        run_headless(
            &conn,
            Arc::new(judge),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            keys,
            true,
        )
        .unwrap()
    }

    #[test]
    fn answering_then_submitting_records_the_scripted_verdict() {
        let app = drive(
            1,
            llm::stub::StubJudge::scripted([Some(Label::Demonstrates)]),
            vec![key('a'), ctrl('s')],
        );
        assert_eq!(app.scores[0], Label::Demonstrates.score());
        assert!(matches!(app.verdict, Some((Label::Demonstrates, _))));
    }

    /// The precheck must run on the UI thread and cost nothing — an answer
    /// citing nothing never reaches the judge.
    #[test]
    fn an_answer_citing_nothing_is_rejected_without_consulting_the_judge() {
        let (conn, hunks, questions) = fixture(1);
        let mut app = App {
            questions, current: 0,
            answer: "This could fail and leave state inconsistent.".into(),
            mode: Mode::Answering, hints_shown: 0, verdict: None, scores: vec![0.0],
            diff: build_diff_view(DIFF, &hunks), scroll: 0,
            status: String::new(), quit: false,
        };
        let (tx, rx) = mpsc::channel();
        // A judge scripted to fail: reaching it would surface as an error.
        let judge: Arc<dyn llm::Judge + Send + Sync> =
            Arc::new(llm::stub::StubJudge::scripted([None]));
        submit(&mut app, &judge, &Arc::new(DIFF.to_string()), &hunks, &tx);

        match rx.recv().unwrap() {
            AppEvent::Graded { label, .. } => assert_eq!(label, Label::Wrong),
            AppEvent::Failed { error, .. } => panic!("reached the judge: {error}"),
        }
        drop(conn);
    }

    #[test]
    fn a_failed_judge_call_returns_to_answering_rather_than_hanging() {
        let app = drive(
            1,
            llm::stub::StubJudge::scripted([None]),
            vec![key('a'), ctrl('s')],
        );
        assert!(app.status.contains("grading failed"), "status: {}", app.status);
        assert_eq!(app.mode, Mode::Answering);
    }

    #[test]
    fn space_advances_and_clears_the_previous_answer() {
        let app = drive(
            2,
            llm::stub::StubJudge::scripted([Some(Label::Demonstrates)]),
            vec![key('a'), ctrl('s'), key(' ')],
        );
        assert_eq!(app.current, 1);
        assert!(app.answer.is_empty());
        assert!(app.verdict.is_none());
        assert_eq!(app.mode, Mode::Answering);
    }

    /// While a call is in flight the loop must keep running and ignore input —
    /// not block, and not accept a second submit.
    /// Keys that land while a judge call is in flight must be dropped, not
    /// queued and acted on when it returns.
    #[test]
    fn input_is_ignored_while_grading_is_in_flight() {
        let (conn, hunks, questions) = fixture(2);
        let app = run_headless(
            &conn,
            Arc::new(
                llm::stub::StubJudge::scripted([Some(Label::Partial)])
                    .with_delay(Duration::from_millis(150)),
            ),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('a'), ctrl('s'), key(' '), key(' '), key(' ')],
            false,
        )
        .unwrap();
        assert_eq!(app.current, 0, "advanced while a judge call was in flight");
    }

    #[test]
    fn hints_reveal_one_at_a_time_and_stop_at_the_end() {
        let (conn, hunks, mut questions) = fixture(1);
        questions[0].hints = vec!["one".into(), "two".into()];
        let app = run_headless(
            &conn,
            Arc::new(llm::stub::StubJudge::default()),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('h'), key('h'), key('h')],
            true,
        )
        .unwrap();
        assert_eq!(app.hints_shown, 2);
        assert!(app.status.contains("no more hints"));
    }

    #[test]
    fn scrolling_never_goes_above_the_top() {
        let app = drive(1, llm::stub::StubJudge::default(), vec![key('k'), key('k')]);
        assert_eq!(app.scroll, 0);
    }
}
