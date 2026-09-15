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
    /// §8 dispute: the judge's ruling on a claim that the question, its
    /// reference answer or the code is wrong.
    Disputed {
        question_id: i64,
        upheld: bool,
        kind: String,
        feedback: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// What has been sent to the judge and not yet come back. Held per question,
/// with the answer as it was submitted: you move on and revise while the call
/// is in flight, and the verdict must be recorded against the text that was
/// actually graded.
#[derive(Clone)]
pub struct Pending {
    pub answer: String,
    pub hints_used: usize,
}

/// Per-question state. Answering is no longer one question at a time, so none
/// of this can live on the app as a single value.
#[derive(Clone, Default)]
pub struct Slot {
    pub answer: String,
    pub hints_shown: usize,
    pub verdict: Option<(llm::Label, String)>,
    pub pending: Option<Pending>,
    /// The claim currently with the judge, held for the same reason `pending`
    /// is: the ruling is recorded against what was actually argued.
    pub dispute: Option<String>,
    /// What the judge made of a dispute, shown under the verdict. Separate from
    /// `verdict` because a rejected dispute leaves the grade untouched.
    pub note: Option<String>,
}

pub struct App {
    pub questions: Vec<db::Question>,
    pub current: usize,
    pub slots: Vec<Slot>,
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
    pub fn new(questions: Vec<db::Question>, diff: Vec<DiffLine>) -> Self {
        App {
            current: App::first_unanswered(&questions),
            scores: App::restored_scores(&questions),
            slots: vec![Slot::default(); questions.len()],
            questions,
            diff,
            scroll: 0,
            hscroll: 0,
            viewport: std::cell::Cell::new(20),
            status: STATUS_ANSWERING.into(),
            quit: false,
        }
    }

    pub fn question(&self) -> Option<&db::Question> {
        self.questions.get(self.current)
    }

    fn slot(&self) -> &Slot {
        static EMPTY: std::sync::OnceLock<Slot> = std::sync::OnceLock::new();
        self.slots
            .get(self.current)
            .unwrap_or_else(|| EMPTY.get_or_init(Slot::default))
    }

    fn slot_mut(&mut self) -> Option<&mut Slot> {
        let i = self.current;
        self.slots.get_mut(i)
    }

    pub fn answer(&self) -> &str {
        &self.slot().answer
    }

    pub fn hints_shown(&self) -> usize {
        self.slot().hints_shown
    }

    pub fn verdict(&self) -> Option<&(llm::Label, String)> {
        self.slot().verdict.as_ref()
    }

    /// What the judge made of a dispute on this question, if there was one.
    pub fn slot_note(&self) -> Option<&str> {
        self.slot().note.as_deref()
    }

    /// The mode is a view of the current question's slot, not a state machine
    /// of its own: with several questions in flight at once there is no single
    /// thing the app is doing.
    pub fn mode(&self) -> Mode {
        let slot = self.slot();
        if slot.pending.is_some() {
            Mode::Grading
        } else if slot.verdict.is_some() {
            Mode::Reviewing
        } else {
            Mode::Answering
        }
    }

    /// Judge calls still out, grading and disputes alike. The quiz cannot end
    /// while any of these are outstanding — the score is not known and the
    /// attempt is not recorded.
    pub fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.pending.is_some() || s.dispute.is_some())
            .count()
    }

    /// The scores the pass rule sees. A question taken out of the quiz by an
    /// upheld dispute is not a score of zero — it is not a score. Leaving it in
    /// would let the judge's own mistake block the gate, which is the thing
    /// disputing exists to undo.
    pub fn scoreable(&self) -> Vec<f64> {
        self.questions
            .iter()
            .zip(&self.scores)
            .filter(|(q, _)| q.status != "waived" && q.status != "deferred")
            .map(|(_, s)| *s)
            .collect()
    }

    /// One marker per question, for the pane footer: what is done, what is
    /// being graded, and what is still waiting on you.
    pub fn progress(&self) -> String {
        self.slots
            .iter()
            .zip(&self.questions)
            .enumerate()
            .map(|(i, (slot, q))| {
                let mark = if slot.pending.is_some() || slot.dispute.is_some() {
                    '~'
                } else if q.status == "waived" || q.status == "deferred" {
                    '!'
                } else if q.status == "passed" {
                    '+'
                } else if slot.verdict.is_some() {
                    'x'
                } else if !slot.answer.is_empty() {
                    '*'
                } else {
                    '.'
                };
                format!("{}{}", i + 1, mark)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Index of the first question still worth answering, so quitting and
    /// resuming picks up where you stopped instead of re-grading work that
    /// already passed — each of which would cost another judge call.
    pub fn first_unanswered(questions: &[db::Question]) -> usize {
        questions
            .iter()
            .position(|q| !is_settled(&q.status))
            .unwrap_or(0)
    }

    /// Scores carried over from previous runs.
    pub fn restored_scores(questions: &[db::Question]) -> Vec<f64> {
        questions.iter().map(|q| q.score.unwrap_or(0.0)).collect()
    }

    /// The next question that still needs you: not settled, and not already
    /// sitting with the judge. Searches past the current question first, then
    /// wraps, since submitting out of order is now normal.
    fn next_unanswered(&self) -> Option<usize> {
        if self.questions.is_empty() {
            return None;
        }
        let needs = |i: usize| {
            !is_settled(&self.questions[i].status)
                && self.slots.get(i).is_none_or(|s| s.pending.is_none())
        };
        let n = self.questions.len();
        (1..=n)
            .map(|offset| (self.current + offset) % n)
            .find(|i| needs(*i) && *i != self.current)
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

/// A question nobody is going to answer again: passed, or taken out of the quiz
/// by an upheld dispute. `waived` was the question's own fault and `deferred`
/// is a bug the code owes you — neither is a score you can earn.
pub fn is_settled(status: &str) -> bool {
    matches!(status, "passed" | "waived" | "deferred")
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
    "a answer · h hint · d dispute · tab/1-9 question · n/p hunk · g back · q quit";
const STATUS_ANSWERED: &str =
    "^s submit & move on · e revise · h hint · d dispute · tab/1-9 question · q quit";
const STATUS_REVIEWING: &str =
    "space next · e revise · d dispute · tab/1-9 question · g back · q quit";

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
    for hint in q.hints.iter().take(app.hints_shown()) {
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
    let view = build_diff_view(&diff, ai_hunks);
    let mut app = App::new(questions, view);
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
    Ok(app.scoreable())
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
                    let Some(i) = app.questions.iter().position(|q| q.id == question_id)
                    else {
                        continue;
                    };
                    // The answer as submitted, not whatever is in the slot now:
                    // you are free to have revised it since.
                    let graded = app.slots[i].pending.take().unwrap_or(Pending {
                        answer: app.slots[i].answer.clone(),
                        hints_used: app.slots[i].hints_shown,
                    });
                    app.scores[i] = label.score();
                    db::record_attempt(
                        conn, question_id, "answer", &graded.answer,
                        graded.hints_used as i64,
                        label.as_str(), label.score(), &feedback, judge.model(),
                    )?;
                    // Mirror what the row now says, so the passed question is
                    // skipped rather than offered again.
                    if label.score() >= 0.6 {
                        app.questions[i].status = "passed".into();
                    }
                    app.questions[i].label = Some(label.as_str().into());
                    app.questions[i].score = Some(label.score());
                    app.slots[i].verdict = Some((label, feedback));
                    app.status = if i == app.current {
                        STATUS_REVIEWING.into()
                    } else {
                        format!("question {} graded: {}", i + 1, label.as_str())
                    };
                }
                AppEvent::Failed { question_id, error } => {
                    // Which call failed is what the reviewer needs to know:
                    // a failed dispute leaves the question standing, a failed
                    // grading leaves the answer unsubmitted.
                    let mut what = "grading";
                    if let Some(i) = app.questions.iter().position(|q| q.id == question_id) {
                        if app.slots[i].dispute.take().is_some() {
                            what = "the dispute";
                        }
                        app.slots[i].pending = None;
                    }
                    app.status = format!("{what} failed: {error}");
                }
                AppEvent::Disputed { question_id, upheld, kind, feedback } => {
                    let Some(i) = app.questions.iter().position(|q| q.id == question_id)
                    else {
                        continue;
                    };
                    let claim = app.slots[i].dispute.take().unwrap_or_default();
                    db::record_dispute(
                        conn, question_id, &claim, upheld, &kind, &feedback, judge.model(),
                    )?;

                    // §8: an upheld `code_bug` does not pass the question and
                    // does not block the gate. It opens an obligation, and the
                    // question leaves the quiz either way — there is no answer
                    // to a question whose premise just failed.
                    let status = match (upheld, kind.as_str()) {
                        (true, "code_bug") => {
                            db::open_obligation(conn, question_id, &claim)?;
                            "deferred"
                        }
                        (true, _) => "waived",
                        (false, _) => "open",
                    };
                    if upheld {
                        db::set_question_status(conn, question_id, status)?;
                        app.questions[i].status = status.into();
                    }
                    app.slots[i].note = Some(format!(
                        "dispute {}: {feedback}",
                        if upheld { format!("upheld ({kind})") } else { "rejected".into() }
                    ));
                    app.status = if upheld {
                        format!("question {} {status}", i + 1)
                    } else {
                        format!("dispute on question {} rejected", i + 1)
                    };
                }
            }
        }

        let Some(key) = input.next(app)? else { continue };

        match (key.code, app.mode() == Mode::Reviewing) {
            // Quitting with calls outstanding throws away answers that have
            // already been paid for, so it takes a second press.
            (KeyCode::Char('q'), _) => {
                if app.in_flight() > 0 && !app.quit {
                    app.quit = true;
                    app.status = format!(
                        "{} still grading — q again to abandon them",
                        app.in_flight()
                    );
                    continue;
                }
                return Ok(());
            }
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

            // Free movement between questions. Which one you can answer is not
            // the tool's call: the checkable question is often obvious once you
            // have read the intent one, and the reverse is just as often true.
            // Passed and in-flight questions are reachable too — you may want
            // to re-read what you said.
            (KeyCode::Tab, _) => goto(app, app.current + 1, ai_hunks),
            (KeyCode::BackTab, _) => {
                let n = app.questions.len();
                goto(app, app.current + n.saturating_sub(1), ai_hunks);
            }

            // Straight to a question by number, for a quiz short enough to see
            // the whole footer at once.
            (KeyCode::Char(c), _) if c.is_ascii_digit() && c != '0' => {
                let want = c.to_digit(10).unwrap() as usize - 1;
                if want < app.questions.len() {
                    goto(app, want, ai_hunks);
                } else {
                    app.status = format!("no question {c}");
                }
            }

            // Next question that still needs you. Passed ones are skipped,
            // since re-grading them costs a judge call and changes nothing, and
            // so are the ones sitting with the judge.
            (KeyCode::Char(' '), _) => {
                if !advance(app, ai_hunks) {
                    // Nothing left to answer. Anything still in flight has to
                    // land first: its score decides whether the gate passes.
                    if app.in_flight() == 0 {
                        return Ok(());
                    }
                    app.status = format!("waiting on {} grading…", app.in_flight());
                }
            }

            // Reveal the next hint. Tier 3 is half the answer, so stop there.
            (KeyCode::Char('h'), false) => {
                let available = app.question().map(|q| q.hints.len()).unwrap_or(0);
                let shown = app.hints_shown();
                match app.slot_mut() {
                    Some(slot) if shown < available => slot.hints_shown += 1,
                    _ => app.status = "no more hints".into(),
                }
            }

            (KeyCode::Char('a'), _) | (KeyCode::Char('e'), _) => {
                // Revising an answer the judge is reading would mean showing a
                // verdict against text that is no longer on screen.
                if app.mode() == Mode::Grading {
                    app.status = "this one is with the judge — space to move on".into();
                    continue;
                }
                let edited = ui.edit(app.answer(), &editor_header(app))?;
                let empty = edited.is_empty();
                if let Some(slot) = app.slot_mut() {
                    slot.answer = edited;
                    if !empty {
                        slot.verdict = None;
                    }
                }
                if !empty {
                    app.status = STATUS_ANSWERED.into();
                }
            }

            // §8 dispute. Not gated on having answered: a question whose
            // premise is wrong is worth saying so before you spend an answer
            // on it.
            (KeyCode::Char('d'), _) => {
                dispute(ui, app, conn, judge, ai_hunks, tx)?;
            }

            (KeyCode::Char('s'), _) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.mode() == Mode::Grading {
                    app.status = "already grading this one".into();
                    continue;
                }
                if submit(app, judge, diff, ai_hunks, tx) {
                    // The judge takes tens of seconds. Move on rather than
                    // watch it: that wait is the whole cost of the quiz.
                    if advance(app, ai_hunks) {
                        app.status = format!(
                            "{STATUS_ANSWERING} · {} grading",
                            app.in_flight()
                        );
                    }
                }
            }

            _ => {}
        }
    }
}

/// §8 — argue that the question, its reference answer or the code is wrong.
///
/// Every guard that can run locally runs before the call: disputing is cheap
/// for the reviewer and upholding is not, and v1's free dispute became the way
/// past any question worth thinking about.
#[allow(clippy::too_many_arguments)]
fn dispute(
    ui: &mut Editor,
    app: &mut App,
    conn: &Connection,
    judge: &Arc<dyn llm::Judge + Send + Sync>,
    ai_hunks: &[git::Hunk],
    tx: &Sender<AppEvent>,
) -> Result<()> {
    let Some(q) = app.question().cloned() else { return Ok(()) };
    if app.slot().dispute.is_some() {
        app.status = "that dispute is already with the judge".into();
        return Ok(());
    }
    if is_settled(&q.status) {
        app.status = format!("question {} is already settled", app.current + 1);
        return Ok(());
    }

    let (per_question, per_gate) = db::dispute_counts(conn, q.id)?;
    if per_question >= 1 {
        app.status = "one dispute per question".into();
        return Ok(());
    }
    if per_gate >= 2 {
        app.status = "two disputes per gate — answer this one".into();
        return Ok(());
    }

    let claim = ui.edit("", &dispute_header(app))?;
    if claim.trim().is_empty() {
        app.status = "nothing to dispute".into();
        return Ok(());
    }
    // Local, so a claim that names nothing costs nothing.
    if !llm::cites_a_changed_line(&claim, ai_hunks) {
        app.status = "a dispute must quote a changed line, like sync.rs:88".into();
        return Ok(());
    }

    app.status = "disputing…".into();
    if let Some(slot) = app.slot_mut() {
        slot.dispute = Some(claim.clone());
        slot.note = None;
    }

    let tx = tx.clone();
    let judge = Arc::clone(judge);
    let (id, text, reference) = (q.id, q.text.clone(), q.reference.clone());
    std::thread::spawn(move || {
        let ev = match judge.dispute(&text, &reference, &claim) {
            Ok(d) => AppEvent::Disputed {
                question_id: id,
                upheld: d.upheld,
                kind: d.kind,
                feedback: d.feedback,
            },
            Err(e) => AppEvent::Failed { question_id: id, error: e.to_string() },
        };
        let _ = tx.send(ev);
    });
    Ok(())
}

/// The `#` block above a dispute. States the question and what the judge will
/// hold the claim to — never the reference answer, which is what half the
/// disputes are about and would turn disputing into a way to read it.
fn dispute_header(app: &App) -> String {
    let Some(q) = app.question() else { return String::new() };
    let mut out = format!(
        "# Disputing question {}/{} · {} · {}\n#\n",
        app.current + 1,
        app.questions.len(),
        q.kind,
        q.file
    );
    for line in wrap_comment(&q.text) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("#\n");
    for line in wrap_comment(
        "Say what is wrong: the question assumes something the code does not do \
         (premise), the stored answer is wrong (reference), or the code itself is \
         wrong (code_bug). Quote a changed line — file.rs:88 — or the claim is \
         refused before it costs anything. Upholding is deliberately hard.",
    ) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("#\n# The `#` block at the end of this file is stripped.\n");
    out
}

/// Move to question `i`, wrapping. Unlike `advance` this goes wherever it is
/// told: passed, failed, or with the judge.
fn goto(app: &mut App, i: usize, ai_hunks: &[git::Hunk]) {
    if app.questions.is_empty() {
        return;
    }
    app.current = i % app.questions.len();
    app.status = match app.mode() {
        Mode::Grading => format!("question {} is with the judge", app.current + 1),
        Mode::Reviewing => STATUS_REVIEWING.into(),
        Mode::Answering if app.answer().is_empty() => STATUS_ANSWERING.into(),
        Mode::Answering => STATUS_ANSWERED.into(),
    };
    app.jump_to_anchor(ai_hunks);
}

/// Move to the next question that still needs answering. `false` when there is
/// none, which leaves the current question where it is.
fn advance(app: &mut App, ai_hunks: &[git::Hunk]) -> bool {
    let Some(next) = app.next_unanswered() else { return false };
    app.current = next;
    app.status = STATUS_ANSWERING.into();
    app.jump_to_anchor(ai_hunks);
    true
}

fn is_ctrl_c(key: &event::KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Spawn the judge call. The §8 precheck runs first, on this thread, because it
/// costs nothing and needs no network.
///
/// `true` when the answer is now with the judge, so the caller can move on.
fn submit(
    app: &mut App,
    judge: &Arc<dyn llm::Judge + Send + Sync>,
    diff: &Arc<String>,
    ai_hunks: &[git::Hunk],
    tx: &Sender<AppEvent>,
) -> bool {
    let Some(q) = app.question().cloned() else { return false };
    if app.answer().trim().is_empty() {
        app.status = "nothing to submit — press a to write an answer".into();
        return false;
    }

    let pending = Pending {
        answer: app.answer().to_string(),
        hints_used: app.hints_shown(),
    };
    if let Some(slot) = app.slot_mut() {
        slot.pending = Some(pending);
        slot.verdict = None;
    }

    if !llm::cites_the_diff(app.answer(), ai_hunks) {
        let _ = tx.send(AppEvent::Graded {
            question_id: q.id,
            label: llm::Label::Wrong,
            feedback: "Cites nothing from the diff — name an identifier, file or line.".into(),
        });
        return true;
    }

    app.status = "grading…".into();

    let tx = tx.clone();
    let answer = app.answer().to_string();
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
    true
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
            if app.in_flight() > 0 && self.wait_for_grading {
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
                None if app.in_flight() > 0 => {
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

    let mut app = App::new(questions, build_diff_view(&diff, ai_hunks));
    app.current = 0;
    app.scores = vec![0.0; app.questions.len()];
    app.status = String::new();
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
        let mut app = App::new(
            vec![db::Question {
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
            build_diff_view(DIFF, &hunks),
        );
        // Smaller than the diff, or the clamp pins every jump to the top.
        app.viewport = std::cell::Cell::new(4);
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
        assert!(matches!(app.verdict(), Some((Label::Demonstrates, _))));
    }

    /// The precheck must run on the UI thread and cost nothing — an answer
    /// citing nothing never reaches the judge.
    #[test]
    fn an_answer_citing_nothing_is_rejected_without_consulting_the_judge() {
        let (conn, hunks, questions) = fixture(1);
        let mut app = App::new(questions, build_diff_view(DIFF, &hunks));
        app.slots[0].answer = "This could fail and leave state inconsistent.".into();
        let (tx, rx) = mpsc::channel();
        // A judge scripted to fail: reaching it would surface as an error.
        let judge: Arc<dyn llm::Judge + Send + Sync> =
            Arc::new(llm::stub::StubJudge::scripted([None]));
        submit(&mut app, &judge, &Arc::new(DIFF.to_string()), &hunks, &tx);

        match rx.recv().unwrap() {
            AppEvent::Graded { label, .. } => assert_eq!(label, Label::Wrong),
            AppEvent::Failed { error, .. } => panic!("reached the judge: {error}"),
            AppEvent::Disputed { .. } => panic!("a submit produced a dispute ruling"),
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
        assert_eq!(app.mode(), Mode::Answering);
    }

    #[test]
    fn space_advances_and_clears_the_previous_answer() {
        let app = drive(
            2,
            llm::stub::StubJudge::scripted([Some(Label::Demonstrates)]),
            vec![key('a'), ctrl('s'), key(' ')],
        );
        assert_eq!(app.current, 1);
        assert!(app.answer().is_empty());
        assert!(app.verdict().is_none());
        assert_eq!(app.mode(), Mode::Answering);
    }

    /// A judge call takes tens of seconds. Submitting moves straight to the
    /// next question and grades the last one behind you — the whole point of
    /// running the call in the background.
    #[test]
    fn submitting_moves_on_while_the_judge_is_still_working() {
        let (conn, hunks, questions) = fixture(2);
        let app = run_headless(
            &conn,
            Arc::new(
                llm::stub::StubJudge::scripted([Some(Label::Partial), Some(Label::Partial)])
                    .with_delay(Duration::from_millis(150)),
            ),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('a'), ctrl('s'), key('a')],
            false,
        )
        .unwrap();
        assert_eq!(app.current, 1, "did not move on while the judge worked");
        assert!(!app.slots[1].answer.is_empty(), "could not answer question 2");
    }

    /// The verdict belongs to the question it was asked about, not to whatever
    /// is on screen when it lands.
    #[test]
    fn a_verdict_lands_on_its_own_question_after_moving_on() {
        let (conn, hunks, questions) = fixture(2);
        let ids: Vec<i64> = questions.iter().map(|q| q.id).collect();
        let app = run_headless(
            &conn,
            Arc::new(
                llm::stub::StubJudge::scripted([Some(Label::Demonstrates)])
                    .with_delay(Duration::from_millis(50)),
            ),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('a'), ctrl('s')],
            true,
        )
        .unwrap();
        assert_eq!(app.scores[0], Label::Demonstrates.score());
        assert!(app.slots[0].verdict.is_some(), "verdict missed question 1");
        assert!(app.slots[1].verdict.is_none(), "verdict landed on question 2");
        // Recorded against the answer that was actually graded.
        assert!(db::last_answer(&conn, ids[0]).unwrap().is_some());
        assert!(db::last_answer(&conn, ids[1]).unwrap().is_none());
    }

    /// Quitting with a call outstanding throws away an answer that has already
    /// been paid for, so the first press only warns.
    #[test]
    fn quitting_with_a_call_outstanding_takes_two_presses() {
        let (conn, hunks, questions) = fixture(2);
        let app = run_headless(
            &conn,
            Arc::new(
                llm::stub::StubJudge::scripted([Some(Label::Partial)])
                    .with_delay(Duration::from_millis(120)),
            ),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('a'), ctrl('s'), key('q')],
            false,
        )
        .unwrap();
        // The warning is armed rather than the loop exiting: the outstanding
        // verdict still lands and is recorded.
        assert!(app.quit, "the first q did not arm the warning");
        assert_eq!(app.in_flight(), 0, "quit before the verdict landed");
        assert_eq!(app.scores[0], Label::Partial.score());
    }

    /// Revising an answer the judge is already reading would show a verdict
    /// against text that is no longer on screen.
    #[test]
    fn the_answer_is_frozen_while_it_is_with_the_judge() {
        let (conn, hunks, questions) = fixture(1);
        let app = run_headless(
            &conn,
            Arc::new(
                llm::stub::StubJudge::scripted([Some(Label::Partial)])
                    .with_delay(Duration::from_millis(120)),
            ),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('a'), ctrl('s'), ctrl('s')],
            false,
        )
        .unwrap();
        // One attempt, not two: the second ^s was refused rather than buying a
        // second verdict on the same answer. The judge is scripted with a
        // single reply, so a second call would come back as a failure.
        let attempts: i64 = conn
            .query_row("SELECT count(*) FROM attempts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(attempts, 1, "the answer was graded twice");
        assert_eq!(app.scores[0], Label::Partial.score());
    }

    fn tab() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
    }
    fn back_tab() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    }

    /// Reading order is not answering order: the checkable question is often
    /// only obvious once the intent one has been read, and the reverse too.
    #[test]
    fn a_digit_jumps_straight_to_that_question() {
        let app = drive(
            3,
            llm::stub::StubJudge::default(),
            vec![key('3'), key('a')],
        );
        assert_eq!(app.current, 2);
        assert!(!app.slots[2].answer.is_empty(), "answered the wrong question");
        assert!(app.slots[0].answer.is_empty());
        assert!(app.slots[1].answer.is_empty());
    }

    #[test]
    fn a_digit_past_the_last_question_says_so_and_stays_put() {
        let app = drive(2, llm::stub::StubJudge::default(), vec![key('9')]);
        assert_eq!(app.current, 0);
        assert!(app.status.contains("no question 9"), "status: {}", app.status);
    }

    #[test]
    fn tab_walks_the_questions_and_wraps() {
        let app = drive(3, llm::stub::StubJudge::default(), vec![tab(), tab(), tab()]);
        assert_eq!(app.current, 0, "tab did not wrap back round");
        let app = drive(3, llm::stub::StubJudge::default(), vec![back_tab()]);
        assert_eq!(app.current, 2, "shift-tab did not wrap backwards");
    }

    /// Each question keeps its own draft. Wandering off to read another one
    /// and coming back must not cost you what you had written.
    #[test]
    fn every_question_keeps_its_own_draft() {
        let app = drive(
            2,
            llm::stub::StubJudge::default(),
            vec![key('a'), tab(), key('a'), tab()],
        );
        assert_eq!(app.current, 0);
        assert!(!app.slots[0].answer.is_empty(), "lost the first draft");
        assert!(!app.slots[1].answer.is_empty(), "lost the second draft");
    }

    /// Jumping onto a question that is with the judge is allowed — you may
    /// want to re-read what you sent — but it says so rather than looking idle.
    #[test]
    fn landing_on_a_question_with_the_judge_says_what_it_is_doing() {
        let (conn, hunks, questions) = fixture(2);
        let mut app = App::new(questions, build_diff_view(DIFF, &hunks));
        app.current = 1;
        app.slots[0].pending = Some(Pending { answer: "a".into(), hints_used: 0 });
        goto(&mut app, 0, &hunks);
        assert_eq!(app.current, 0);
        assert!(app.status.contains("with the judge"), "status: {}", app.status);
        drop(conn);
    }

    /// A rejected dispute is recorded and changes nothing: the verdict you were
    /// arguing with stands, and the question is still yours to answer.
    #[test]
    fn a_rejected_dispute_is_recorded_and_leaves_the_question_open() {
        let (conn, hunks, questions) = fixture(1);
        let id = questions[0].id;
        let app = run_headless(
            &conn,
            Arc::new(llm::stub::StubJudge::default()),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('d')],
            true,
        )
        .unwrap();

        let (per_question, _) = db::dispute_counts(&conn, id).unwrap();
        assert_eq!(per_question, 1, "the dispute was not recorded");
        let label: String = conn
            .query_row(
                "SELECT label FROM attempts WHERE mode = 'dispute'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(label, "rejected");
        assert_eq!(app.questions[0].status, "open");
        assert!(app.slots[0].note.as_deref().unwrap().contains("rejected"));
    }

    /// An upheld premise takes the question out of the quiz. It must not leave
    /// a zero behind: the judge's own mistake blocking the gate is the thing
    /// disputing exists to undo.
    #[test]
    fn an_upheld_dispute_waives_the_question_and_its_score() {
        let (conn, hunks, questions) = fixture(2);
        let app = run_headless(
            &conn,
            Arc::new(llm::stub::StubJudge::default().upholding("premise")),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('d')],
            true,
        )
        .unwrap();

        assert_eq!(app.questions[0].status, "waived");
        assert_eq!(app.scoreable().len(), 1, "the waived question is still scored");
        let stored: String = conn
            .query_row("SELECT status FROM questions ORDER BY id LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "waived");
    }

    /// §8: a code bug does not pass the question and does not block the gate.
    /// It opens an obligation, which is what stops "the code is wrong" being a
    /// free skip.
    #[test]
    fn an_upheld_code_bug_defers_the_question_and_opens_an_obligation() {
        let (conn, hunks, questions) = fixture(1);
        let app = run_headless(
            &conn,
            Arc::new(llm::stub::StubJudge::default().upholding("code_bug")),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('d')],
            true,
        )
        .unwrap();

        assert_eq!(app.questions[0].status, "deferred");
        assert_eq!(db::open_obligations(&conn, "o/r").unwrap().len(), 1);
    }

    /// Disputing is cheap and upholding is not, so the caps are what keep it
    /// from becoming the way past every question.
    #[test]
    fn a_question_takes_one_dispute_and_a_gate_takes_two() {
        let (conn, hunks, questions) = fixture(3);
        let app = run_headless(
            &conn,
            Arc::new(llm::stub::StubJudge::default()),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            // Two on the first question, then one each on the next two.
            vec![key('d'), key('d'), key('2'), key('d'), key('3'), key('d')],
            true,
        )
        .unwrap();

        let disputes: i64 = conn
            .query_row("SELECT count(*) FROM attempts WHERE mode = 'dispute'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(disputes, 2, "the caps did not hold");
        assert!(app.status.contains("two disputes per gate"), "status: {}", app.status);
    }

    /// A question already out of the quiz cannot be disputed again — there is
    /// nothing left to argue about, and it would burn the gate's second slot.
    #[test]
    fn a_settled_question_cannot_be_disputed() {
        let (conn, hunks, mut questions) = fixture(1);
        questions[0].status = "passed".into();
        let app = run_headless(
            &conn,
            Arc::new(llm::stub::StubJudge::default()),
            Arc::new(DIFF.to_string()),
            &hunks,
            questions,
            vec![key('d')],
            true,
        )
        .unwrap();
        assert!(app.status.contains("already settled"), "status: {}", app.status);
        let disputes: i64 = conn
            .query_row("SELECT count(*) FROM attempts WHERE mode = 'dispute'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(disputes, 0);
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
        assert_eq!(app.hints_shown(), 2);
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
        let mut app = App::new(vec![], view());
        app.viewport = std::cell::Cell::new(viewport);
        app.status = String::new();
        app
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
        let mut app = App::new(
            vec![
                q(1, "open", None),
                q(2, "passed", Some(0.9)),
                q(3, "open", None),
            ],
            vec![],
        );
        app.current = 0;
        assert_eq!(app.next_unanswered(), Some(2), "should skip the passed one");
        app.current = 2;
        // Wraps: question 1 is still open, and with grading in the background
        // its verdict may have landed long after you moved past it.
        assert_eq!(app.next_unanswered(), Some(0));
        app.questions[0].status = "passed".into();
        app.questions[2].status = "passed".into();
        assert_eq!(app.next_unanswered(), None, "nothing left to ask");
    }

    /// A question already with the judge is not offered again — that would buy
    /// a second verdict on the same question at full price.
    #[test]
    fn advancing_skips_questions_already_with_the_judge() {
        let mut app = App::new(
            vec![q(1, "open", None), q(2, "open", None), q(3, "open", None)],
            vec![],
        );
        app.current = 0;
        app.slots[1].pending = Some(Pending { answer: "a".into(), hints_used: 0 });
        assert_eq!(app.next_unanswered(), Some(2));
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;

    fn app(hints_shown: usize) -> App {
        let q = crate::tui::quiz::tests::question();
        let mut app = App::new(vec![q], Vec::new());
        app.slots[0].hints_shown = hints_shown;
        app
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
