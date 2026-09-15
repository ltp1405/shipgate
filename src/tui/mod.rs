//! §12 — the quiz screen.
//!
//! Two panes: the diff on the left, the question and your answer on the right.
//! The diff is not a later addition. These questions are open book by design —
//! the code is days old by the time a PR is ready, and an open-book quiz without
//! the book is a memory test on code you wrote last week.

pub mod dash;
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

/// One rendered diff row, with everything the pane needs to place it.
pub struct DiffLine {
    pub text: String,
    pub ai: bool,
    /// New-side line number. `None` for removed lines and headers, which have
    /// none — the questions cite new-side positions.
    pub lineno: Option<u32>,
    pub is_hunk_header: bool,
    pub is_file_header: bool,
}

pub struct App {
    pub questions: Vec<db::Question>,
    pub current: usize,
    pub answer: String,
    pub mode: Mode,
    pub hints_shown: usize,
    pub verdict: Option<(llm::Label, String)>,
    pub scores: Vec<f64>,
    pub diff: Vec<DiffLine>,
    pub scroll: u16,
    /// Horizontal offset. Code lines routinely exceed the pane, and wrapping
    /// them would destroy the indentation that makes code readable.
    pub hscroll: u16,
    /// Rows the diff pane last drew, so scrolling can be clamped to content.
    /// Only the renderer knows the height.
    pub viewport: std::cell::Cell<u16>,
    pub status: String,
    pub quit: bool,
}

impl App {
    pub fn question(&self) -> Option<&db::Question> {
        self.questions.get(self.current)
    }

    /// Index of the first question still worth answering, so quitting and
    /// resuming picks up where you stopped instead of re-grading work that
    /// already passed — each of which would cost another judge call.
    pub fn first_unanswered(questions: &[db::Question]) -> usize {
        questions
            .iter()
            .position(|q| q.status != "passed")
            .unwrap_or(0)
    }

    /// Scores carried over from previous runs.
    pub fn restored_scores(questions: &[db::Question]) -> Vec<f64> {
        questions.iter().map(|q| q.score.unwrap_or(0.0)).collect()
    }

    /// The next question that still needs an answer. `None` when the quiz is
    /// done.
    fn next_unanswered(&self) -> Option<usize> {
        self.questions
            .iter()
            .enumerate()
            .skip(self.current + 1)
            .find(|(_, q)| q.status != "passed")
            .map(|(i, _)| i)
    }

    /// Scroll the diff to the hunk this question is about, so the reader does
    /// not have to hunt for it.
    ///
    /// The search is anchored to the question's file first. Matching the `@@`
    /// header alone jumps to whichever file happens to share that header — with
    /// headers as common as `@@ -1,1 +1,2 @@` that is routinely the wrong one.
    /// Highest useful scroll offset. Without this `j` runs off the end into
    /// unbounded blank space with nothing to say you have overrun.
    pub fn max_scroll(&self) -> u16 {
        let viewport = self.viewport.get().max(1);
        (self.diff.len() as u16).saturating_sub(viewport)
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).max(0) as u16;
        self.scroll = next.min(self.max_scroll());
    }

    /// Move to the next or previous row matching `pred`, by hunk or by file.
    fn jump(&mut self, forward: bool, pred: impl Fn(&DiffLine) -> bool) {
        let here = self.scroll as usize;
        let found = if forward {
            self.diff
                .iter()
                .enumerate()
                .find(|(i, d)| *i > here && pred(d))
                .map(|(i, _)| i)
        } else {
            self.diff
                .iter()
                .enumerate()
                .take(here)
                .rfind(|(_, d)| pred(d))
                .map(|(i, _)| i)
        };
        if let Some(i) = found {
            self.scroll = (i as u16).min(self.max_scroll());
        }
    }

    pub fn jump_hunk(&mut self, forward: bool) {
        self.jump(forward, |d| d.is_hunk_header);
    }

    pub fn jump_file(&mut self, forward: bool) {
        self.jump(forward, |d| d.is_file_header);
    }

    fn jump_to_anchor(&mut self, hunks: &[git::Hunk]) {
        let Some(q) = self.questions.get(self.current) else { return };
        let Some(hunk) = hunks.iter().find(|h| h.anchor == q.anchor) else { return };

        let file_start = self
            .diff
            .iter()
            .position(|d| d.is_file_header && d.text.ends_with(&format!("b/{}", q.file)))
            .unwrap_or(0);

        let found = self.diff[file_start..]
            .iter()
            .position(|d| d.text == hunk.header)
            .map(|offset| file_start + offset);

        if let Some(idx) = found {
            self.scroll = (idx.saturating_sub(3) as u16).min(self.max_scroll());
            self.hscroll = 0;
        }
    }
}

/// Mark each diff line with whether its hunk is AI-authored.
///
/// Keyed on `(file, header)`, not the header alone: `@@ -1,1 +1,2 @@` recurs
/// across files, so matching on the header by itself marks unrelated hunks in
/// other files as AI-authored.
/// Parse the new-side start out of `@@ -12,3 +88,9 @@`.
fn hunk_start(header: &str) -> Option<u32> {
    let plus = header.split('+').nth(1)?;
    let digits: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn build_diff_view(diff: &str, ai_hunks: &[git::Hunk]) -> Vec<DiffLine> {
    let ai: std::collections::HashSet<(&str, &str)> = ai_hunks
        .iter()
        .map(|h| (h.file.as_str(), h.header.as_str()))
        .collect();

    let mut out = Vec::new();
    let mut file = String::new();
    let mut in_ai_hunk = false;
    let mut lineno: Option<u32> = None;

    for line in diff.lines() {
        let mut is_file_header = false;
        let mut is_hunk_header = false;
        let mut number = None;

        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_ai_hunk = false;
            lineno = None;
            is_file_header = true;
            file = rest
                .split(" b/")
                .nth(1)
                .unwrap_or_else(|| rest.trim_start_matches("a/"))
                .to_string();
        } else if line.starts_with("@@") {
            in_ai_hunk = ai.contains(&(file.as_str(), line));
            is_hunk_header = true;
            lineno = hunk_start(line);
        } else if line.starts_with("---") || line.starts_with("+++") || line.starts_with("index ") {
            // File metadata, not content.
        } else if let Some(n) = lineno {
            // Removed lines occupy no new-side number; context and additions do.
            if !line.starts_with('-') {
                number = Some(n);
                lineno = Some(n + 1);
            }
        }

        out.push(DiffLine {
            text: line.to_string(),
            ai: in_ai_hunk,
            lineno: number,
            is_hunk_header,
            is_file_header,
        });
    }
    out
}

const STATUS_ANSWERING: &str =
    "a answer · h hint · n/p hunk · [/] file · g back · ←/→ pan · q quit";
const STATUS_ANSWERED: &str = "^s submit · e revise · h hint · n/p hunk · g back · q quit";
const STATUS_REVIEWING: &str = "space next · e revise · n/p hunk · g back · q quit";

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

/// The question, as `#` lines below the answer — what git does with the changes
/// in COMMIT_EDITMSG. $EDITOR covers the quiz screen, so without this you
/// compose the answer from memory of a question you can no longer see.
fn editor_header(app: &App) -> String {
    let Some(q) = app.question() else { return String::new() };
    let mut out = String::new();
    out.push_str(&format!(
        "# Question {}/{} · {} · {}\n#\n",
        app.current + 1,
        app.questions.len(),
        q.kind,
        q.file
    ));
    for line in wrap_comment(&q.text) {
        out.push_str(&line);
        out.push('\n');
    }
    for hint in q.hints.iter().take(app.hints_shown) {
        out.push_str("#\n");
        for line in wrap_comment(&format!("hint: {hint}")) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.push_str("#\n# The `#` block at the end of this file is stripped.\n");
    out
}

/// Wrap at 72 columns, the width a commit message editor assumes.
fn wrap_comment(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::from("#");
    for word in text.split_whitespace() {
        if cur.len() + 1 + word.len() > 72 && cur != "#" {
            lines.push(std::mem::take(&mut cur));
            cur.push('#');
        }
        cur.push(' ');
        cur.push_str(word);
    }
    if cur != "#" {
        lines.push(cur);
    }
    lines
}

/// Drop the trailing run of `#` lines the header put there. Only the trailing
/// run: an answer can legitimately start a line with `#` — `#[derive(...)]` in
/// quoted Rust — and eating that would silently corrupt the answer.
fn strip_comments(body: &str) -> String {
    let mut lines: Vec<&str> = body.lines().collect();
    while lines
        .last()
        .is_some_and(|l| l.starts_with('#') || l.trim().is_empty())
    {
        lines.pop();
    }
    lines.join("\n").trim().to_string()
}

/// Drop the terminal out of raw mode, run $EDITOR on the answer, then restore.
/// A terminal textarea is a poor place to compose technical prose.
fn edit_externally(terminal: &mut Term, current: &str, header: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("shipgate-answer-{}.md", std::process::id()));
    std::fs::write(&path, format!("{current}\n\n{header}"))?;

    restore(terminal)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status();
    *terminal = setup()?;
    terminal.clear()?;

    match status {
        Ok(s) if s.success() => Ok(strip_comments(&std::fs::read_to_string(&path)?)),
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
    let mut app = App {
        current: App::first_unanswered(&questions),
        scores: App::restored_scores(&questions),
        questions,
        answer: String::new(),
        mode: Mode::Answering,
        hints_shown: 0,
        verdict: None,
        diff: build_diff_view(&diff, ai_hunks),
        scroll: 0,
        hscroll: 0,
        viewport: std::cell::Cell::new(20),
        status: STATUS_ANSWERING.into(),
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

    fn edit(&mut self, current: &str, header: &str) -> Result<String> {
        match self {
            Editor::Terminal(t) => edit_externally(t, current, header),
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
                    app.status = STATUS_REVIEWING.into();
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

            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.scroll_by(1),
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.scroll_by(-1),
            (KeyCode::PageDown, _) => app.scroll_by(20),
            (KeyCode::PageUp, _) => app.scroll_by(-20),

            // Horizontal, because long code lines are not wrapped.
            (KeyCode::Right, _) => app.hscroll = app.hscroll.saturating_add(8),
            (KeyCode::Left, _) => app.hscroll = app.hscroll.saturating_sub(8),

            (KeyCode::Char('n'), _) => app.jump_hunk(true),
            (KeyCode::Char('p'), _) => app.jump_hunk(false),
            (KeyCode::Char(']'), _) => app.jump_file(true),
            (KeyCode::Char('['), _) => app.jump_file(false),

            // Back to the hunk this question is about, after wandering off.
            (KeyCode::Char('g'), _) => app.jump_to_anchor(ai_hunks),

            // Next question. Already-passed ones are skipped, since re-grading
            // them costs a judge call and changes nothing.
            (KeyCode::Char(' '), true) => {
                let Some(next) = app.next_unanswered() else {
                    return Ok(());
                };
                app.current = next;
                app.answer.clear();
                app.hints_shown = 0;
                app.verdict = None;
                app.mode = Mode::Answering;
                app.status = STATUS_ANSWERING.into();
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
                app.answer = ui.edit(&app.answer, &editor_header(app))?;
                if !app.answer.is_empty() {
                    app.mode = Mode::Answering;
                    app.verdict = None;
                    app.status = STATUS_ANSWERED.into();
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

/// Build a diff view for render tests, marking the given hunk indexes as
/// AI-authored.
#[cfg(test)]
pub fn build_diff_view_for_test(diff: &str, ai_hunk_indexes: &[usize]) -> Vec<DiffLine> {
    let hunks = git::parse_diff(diff);
    let ai: Vec<git::Hunk> = ai_hunk_indexes
        .iter()
        .filter_map(|i| hunks.get(*i).cloned())
        .collect();
    build_diff_view(diff, &ai)
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
        hscroll: 0,
        viewport: std::cell::Cell::new(20),
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
            .filter(|d| d.ai)
            .map(|d| &d.text)
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
            .find(|d| d.text.contains("diff --git a/src/mine.rs"))
            .unwrap();
        assert!(!second_file.ai);
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
            hscroll: 0,
            // Smaller than the diff, or the clamp pins every jump to the top.
            viewport: std::cell::Cell::new(4),
            status: String::new(),
            quit: false,
        };
        app.jump_to_anchor(&hunks);

        // Must land on src/mine.rs's hunk, not on src/ai.rs's identical header.
        let landed = &app.diff[app.scroll as usize..]
            .iter()
            .take(8)
            .map(|d| d.text.clone())
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
                repo: "o/r", path: "/tmp/o-r", pr_number: 1, branch: "b", base_ref: "origin/main",
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
            diff: build_diff_view(DIFF, &hunks), scroll: 0, hscroll: 0,
            viewport: std::cell::Cell::new(40),
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

#[cfg(test)]
mod diff_nav_tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -10,3 +20,4 @@ fn alpha() {
 context_one
-removed_line
+added_line
 context_two
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1,1 +5,2 @@
+beta_added
";

    fn view() -> Vec<DiffLine> {
        build_diff_view(DIFF, &git::parse_diff(DIFF))
    }

    fn app_with(viewport: u16) -> App {
        App {
            questions: vec![],
            current: 0,
            answer: String::new(),
            mode: Mode::Answering,
            hints_shown: 0,
            verdict: None,
            scores: vec![],
            diff: view(),
            scroll: 0,
            hscroll: 0,
            viewport: std::cell::Cell::new(viewport),
            status: String::new(),
            quit: false,
        }
    }

    fn numbered(text: &str) -> Option<u32> {
        view().into_iter().find(|d| d.text.contains(text))?.lineno
    }

    /// Questions cite new-side positions, so the gutter must count the new side
    /// from the `@@` header, not the old side and not the row index.
    #[test]
    fn line_numbers_come_from_the_new_side_of_the_hunk_header() {
        assert_eq!(numbered("context_one"), Some(20));
        assert_eq!(numbered("added_line"), Some(21));
        assert_eq!(numbered("context_two"), Some(22));
    }

    #[test]
    fn removed_lines_have_no_new_side_number() {
        assert_eq!(numbered("removed_line"), None);
    }

    #[test]
    fn a_removed_line_does_not_advance_the_count() {
        // context_one is 20 and added_line is 21: the removal between them
        // must not consume a number.
        assert_eq!(numbered("added_line"), Some(21));
    }

    #[test]
    fn each_file_restarts_from_its_own_hunk_header() {
        assert_eq!(numbered("beta_added"), Some(5));
    }

    #[test]
    fn headers_and_metadata_carry_no_number() {
        for text in ["diff --git", "@@ -10,3", "--- a/a.rs", "+++ b/a.rs"] {
            assert_eq!(numbered(text), None, "{text} should have no line number");
        }
    }

    #[test]
    fn scrolling_stops_at_the_end_of_the_diff() {
        let mut app = app_with(5);
        for _ in 0..500 {
            app.scroll_by(1);
        }
        assert_eq!(app.scroll, app.max_scroll());
        assert!((app.scroll as usize) < app.diff.len());
    }

    #[test]
    fn a_diff_shorter_than_the_viewport_does_not_scroll() {
        let mut app = app_with(200);
        app.scroll_by(50);
        assert_eq!(app.scroll, 0);
    }

    /// Asserts the target is on screen, not that it is at the top: a hunk near
    /// the end of the diff cannot be scrolled to the first row, because there
    /// is not enough content below it. The clamp is right; top-alignment is not
    /// the contract.
    fn shows(app: &App, index: usize) -> bool {
        let start = app.scroll as usize;
        (start..start + app.viewport.get() as usize).contains(&index)
    }

    #[test]
    fn n_and_p_move_between_hunk_headers() {
        let headers: Vec<usize> = view()
            .iter()
            .enumerate()
            .filter(|(_, d)| d.is_hunk_header)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(headers.len(), 2, "fixture should have two hunks");

        let mut app = app_with(4);
        app.jump_hunk(true);
        assert!(shows(&app, headers[0]), "first hunk not on screen");

        app.jump_hunk(true);
        assert!(shows(&app, headers[1]), "second hunk not on screen");

        app.jump_hunk(false);
        assert!(shows(&app, headers[0]), "did not go back to the first hunk");
    }

    #[test]
    fn bracket_keys_move_between_files() {
        let files: Vec<usize> = view()
            .iter()
            .enumerate()
            .filter(|(_, d)| d.is_file_header)
            .map(|(i, _)| i)
            .collect();

        let mut app = app_with(4);
        app.jump_file(true);
        assert!(shows(&app, files[1]), "b.rs not on screen");
        app.jump_file(false);
        assert!(shows(&app, files[0]), "a.rs not on screen");
    }

    #[test]
    fn jumping_past_the_last_hunk_stays_put() {
        let mut app = app_with(4);
        for _ in 0..10 {
            app.jump_hunk(true);
        }
        let settled = app.scroll;
        app.jump_hunk(true);
        assert_eq!(app.scroll, settled);
    }

    /// Returning to the question's hunk must also reset the horizontal pan,
    /// or the reader lands on the right line scrolled off the side.
    #[test]
    fn returning_to_the_anchor_resets_the_pan() {
        let hunks = git::parse_diff(DIFF);
        let target = &hunks[1];
        let mut app = app_with(4);
        app.questions = vec![db::Question {
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
        }];
        app.hscroll = 40;
        app.jump_to_anchor(&hunks);
        assert_eq!(app.hscroll, 0);
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    fn q(id: i64, status: &str, score: Option<f64>) -> db::Question {
        db::Question {
            id,
            kind: "prediction".into(),
            file: "a.rs".into(),
            anchor: "sha256:x".into(),
            text: "q".into(),
            reference: "r".into(),
            hints: vec![],
            status: status.into(),
            label: None,
            score,
        }
    }

    #[test]
    fn resuming_starts_at_the_first_unanswered_question() {
        let qs = vec![
            q(1, "passed", Some(0.9)),
            q(2, "passed", Some(0.6)),
            q(3, "open", None),
        ];
        assert_eq!(App::first_unanswered(&qs), 2);
    }

    #[test]
    fn a_fresh_gate_starts_at_the_beginning() {
        let qs = vec![q(1, "open", None), q(2, "open", None)];
        assert_eq!(App::first_unanswered(&qs), 0);
    }

    /// Every question passed: there is nothing to ask, and the restored scores
    /// alone must satisfy the pass rule.
    #[test]
    fn a_fully_answered_gate_clears_without_asking_again() {
        let qs = vec![q(1, "passed", Some(0.9)), q(2, "passed", Some(0.6))];
        let scores = App::restored_scores(&qs);
        assert_eq!(scores, vec![0.9, 0.6]);
        assert!(crate::gate::passes(&scores));
    }

    #[test]
    fn scores_carry_over_and_unanswered_ones_start_at_zero() {
        let qs = vec![q(1, "passed", Some(0.9)), q(2, "open", None)];
        assert_eq!(App::restored_scores(&qs), vec![0.9, 0.0]);
    }

    /// A question that was answered but failed is not "done" — it must be
    /// offered again, or the gate can never clear.
    #[test]
    fn a_failed_question_is_offered_again() {
        let qs = vec![q(1, "open", Some(0.3)), q(2, "open", None)];
        assert_eq!(App::first_unanswered(&qs), 0);
        assert_eq!(App::restored_scores(&qs), vec![0.3, 0.0]);
    }

    #[test]
    fn advancing_skips_questions_that_already_passed() {
        let mut app = App {
            questions: vec![
                q(1, "open", None),
                q(2, "passed", Some(0.9)),
                q(3, "open", None),
            ],
            current: 0,
            answer: String::new(),
            mode: Mode::Answering,
            hints_shown: 0,
            verdict: None,
            scores: vec![0.0; 3],
            diff: vec![],
            scroll: 0,
            hscroll: 0,
            viewport: std::cell::Cell::new(10),
            status: String::new(),
            quit: false,
        };
        assert_eq!(app.next_unanswered(), Some(2), "should skip the passed one");
        app.current = 2;
        assert_eq!(app.next_unanswered(), None, "nothing left to ask");
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;

    fn app(hints_shown: usize) -> App {
        let q = crate::tui::quiz::tests::question();
        App {
            questions: vec![q],
            current: 0,
            answer: String::new(),
            mode: Mode::Answering,
            hints_shown,
            verdict: None,
            scores: vec![0.0],
            diff: Vec::new(),
            scroll: 0,
            hscroll: 0,
            viewport: std::cell::Cell::new(20),
            status: String::new(),
            quit: false,
        }
    }

    #[test]
    fn the_header_states_the_question_being_answered() {
        let h = editor_header(&app(0));
        assert!(h.contains("Question 1/1 · prediction · src/sync.rs"), "{h}");
        assert!(h.contains("WHATDOESTHECALLEROBSERVE"), "{h}");
        assert!(h.lines().all(|l| l.starts_with('#')), "{h}");
    }

    /// Unrevealed hints are the point of hints. The header must not leak them.
    #[test]
    fn only_revealed_hints_reach_the_editor() {
        assert!(!editor_header(&app(0)).contains("HINTONE"));
        let one = editor_header(&app(1));
        assert!(one.contains("HINTONE"));
        assert!(!one.contains("HINTTWO"));
    }

    #[test]
    fn long_question_text_is_wrapped_and_still_commented() {
        let long = "word ".repeat(40);
        let lines = wrap_comment(&long);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.starts_with("# ") && l.len() <= 72));
    }

    #[test]
    fn the_header_is_stripped_back_off() {
        let body = format!("my answer\n\n{}", editor_header(&app(1)));
        assert_eq!(strip_comments(&body), "my answer");
    }

    /// A `#` line inside the answer — `#[derive(...)]` in quoted Rust — is not
    /// a comment and must survive.
    #[test]
    fn a_hash_line_inside_the_answer_survives() {
        let body = "the struct is\n\n#[derive(Debug)]\nstruct A;\n\n# Question 1/1\n# text\n";
        assert_eq!(
            strip_comments(body),
            "the struct is\n\n#[derive(Debug)]\nstruct A;"
        );
    }
}
