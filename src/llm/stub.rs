//! Offline stand-ins for the model, selected by `--offline`.
//!
//! These exist so the gate flow and the TUI can be exercised without spending a
//! model call: generation is the expensive half, and the states most likely to
//! be broken in the UI — a call in flight, each verdict, a failure — are exactly
//! the ones a real judge makes slow and expensive to reach.

use super::{Context, Disputed, Generated, Generator, Judge, Label, Verdict};
use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

pub struct StubGenerator;

impl Generator for StubGenerator {
    fn generate(&self, ctx: &Context) -> Result<Option<Vec<Generated>>> {
        if ctx.hunks.is_empty() {
            return Ok(None);
        }
        let pick = |n: usize| &ctx.hunks[n.min(ctx.hunks.len() - 1)];

        let mut out = vec![
            Generated {
                kind: "intent".into(),
                file: pick(0).file.clone(),
                anchor: pick(0).anchor.clone(),
                text: format!(
                    "{} files changed together here. What single change of intent \
                     required all of them, that you could not get from the title?",
                    ctx.all_files.len().max(1)
                ),
                reference: "[stand-in] not a real reference answer".into(),
                hints: vec![
                    "Look at what the files have in common.".into(),
                    "Start from the file with the most added lines.".into(),
                    "[stand-in] half the answer".into(),
                ],
            },
            Generated {
                kind: "prediction".into(),
                file: pick(0).file.clone(),
                anchor: pick(0).anchor.clone(),
                text: format!(
                    "In {}, what does the caller observe if this hunk's happy path does not run?",
                    pick(0).file
                ),
                reference: "[stand-in] not a real reference answer".into(),
                hints: vec![
                    "Look at how control leaves the block.".into(),
                    "Check the error branch.".into(),
                    "[stand-in] half the answer".into(),
                ],
            },
            Generated {
                kind: "adversarial".into(),
                file: pick(1).file.clone(),
                anchor: pick(1).anchor.clone(),
                text: format!("What input to {} breaks this change?", pick(1).file),
                reference: "[stand-in] not a real reference answer".into(),
                hints: vec![
                    "Consider the empty case.".into(),
                    "Check the boundary values.".into(),
                    "[stand-in] half the answer".into(),
                ],
            },
        ];

        // §7: at least one checkable where a test command exists.
        if let Some(cmd) = &ctx.test_command {
            out.push(Generated {
                kind: "checkable".into(),
                file: pick(0).file.clone(),
                anchor: pick(0).anchor.clone(),
                text: format!("Run `{cmd}`. What does it report about this change?"),
                reference: "[stand-in] not a real reference answer".into(),
                hints: vec![
                    "Run it and read the output.".into(),
                    format!("`{cmd}` covers the changed paths."),
                    "[stand-in] half the answer".into(),
                ],
            });
        }
        // The stand-ins answer to the band as well, or --offline would show a
        // quiz shape the real generator cannot produce.
        out.truncate(ctx.questions.1.max(1));
        Ok(Some(out))
    }
}

/// A judge that returns scripted verdicts instead of grading.
///
/// Length-based grading cannot reach `partial` or a failure, so it cannot
/// exercise the UI states around them. A script can.
pub struct StubJudge {
    /// Verdicts to return in order. Once exhausted, the last one repeats.
    /// `None` is a failed call.
    pub script: Mutex<VecDeque<Option<Label>>>,
    /// Simulated latency, so the "grading…" state is observable.
    pub delay: Duration,
    /// What to rule on a dispute. `None` — the only thing `--offline` ever
    /// produces — rejects it; the upheld paths exist for tests, since a
    /// stand-in that waived questions would make `--offline` a way to clear a
    /// gate without answering anything.
    pub dispute_ruling: Option<String>,
}

impl Default for StubJudge {
    /// Length-based, matching the old behaviour: a real attempt passes, a
    /// one-liner does not.
    fn default() -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            delay: Duration::ZERO,
            dispute_ruling: None,
        }
    }
}

impl StubJudge {
    pub fn scripted(labels: impl IntoIterator<Item = Option<Label>>) -> Self {
        Self {
            script: Mutex::new(labels.into_iter().collect()),
            delay: Duration::ZERO,
            dispute_ruling: None,
        }
    }

    /// Uphold every dispute with this kind. Tests only.
    #[cfg(test)]
    pub fn upholding(mut self, kind: &str) -> Self {
        self.dispute_ruling = Some(kind.to_string());
        self
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

impl Judge for StubJudge {
    fn judge(&self, _diff: &str, _question: &str, answer: &str) -> Result<Verdict> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }

        // Outer None: no script at all. Inner None: a scripted failure.
        // The last entry is peeked rather than popped, so it repeats instead of
        // falling back to length-based grading partway through a run.
        let scripted: Option<Option<Label>> = {
            let mut q = self.script.lock().unwrap();
            if q.len() > 1 {
                q.pop_front()
            } else {
                q.front().copied()
            }
        };

        match scripted {
            Some(Some(label)) => Ok(Verdict {
                label,
                feedback: format!("[offline] scripted verdict: {}", label.as_str()),
            }),
            Some(None) => bail!("[offline] scripted failure"),
            // No script: fall back to length, which is enough for a smoke test.
            None if answer.trim().len() > 40 => Ok(Verdict {
                label: Label::Demonstrates,
                feedback: "[offline] accepted on length — no real grading happened.".into(),
            }),
            None => Ok(Verdict {
                label: Label::Restates,
                feedback: "[offline] too short to be a real answer.".into(),
            }),
        }
    }

    fn dispute(&self, _question: &str, _reference: &str, _claim: &str) -> Result<Disputed> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        match &self.dispute_ruling {
            Some(kind) => Ok(Disputed {
                upheld: true,
                kind: kind.clone(),
                feedback: format!("[offline] scripted ruling: upheld as {kind}"),
            }),
            None => Ok(Disputed {
                upheld: false,
                kind: String::new(),
                feedback: "[offline] disputes are not judged without a model.".into(),
            }),
        }
    }

    fn model(&self) -> &str {
        "offline-stub"
    }
}
