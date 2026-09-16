//! §8 — grading. The reference answer is deliberately absent from this context.

use super::{cli, Disputed, Divergence, Judge, Label, Verdict};
use anyhow::Result;
use serde::Deserialize;

pub struct CliJudge {
    pub cli: cli::Cli,
    pub model: String,
    pub diff: String,
    /// The PR title and commit subjects. The reviewer can read these, so an
    /// answer that merely echoes them demonstrates nothing.
    pub restatement_sources: Vec<String>,
}

const SYSTEM: &str = "\
You grade a reviewer's answer to a question about a code change, against the \
CODE — you are given the diff and nothing else. There is a reference answer on \
file and you are deliberately not shown it: it was written by a model, it may be \
wrong, and grading toward it rewards agreement over understanding.

Write your own answer to the question FIRST, from the diff. Then judge the \
reviewer's answer against the code and against yours.

Labels:
  wrong        — contradicts the code, or is too vague to be checked.
  restates     — accurately paraphrases what the code does, and stops there.
  partial      — names a real consequence, invariant or failure mode, but misses \
                 or muddles a substantial part of it.
  demonstrates — shows understanding of what this change causes: consequences, \
                 invariants, failure modes, or facts not literally present in \
                 the diff.

Three rules that override the rest:

- Score `wrong` if the answer would be equally true of an arbitrary code change. \
  Generic statements about rollbacks, validation, or concurrency that name \
  nothing specific to THIS diff are not answers.
- Score `wrong` if the answer only restates the change's stated purpose. The \
  reviewer was given the title and the commit subjects; repeating them back is \
  not understanding. An answer about intent must say something those do not, and \
  must connect to what the code actually does.
- Length is not quality. One sentence naming the consequence and pointing at a \
  line is a complete answer and should score `demonstrates`. Do not reward \
  volume, hedging, or restatement of the question.";

const DISPUTE_SYSTEM: &str = "\
The reviewer claims a question's premise, its reference answer, or the code \
itself is wrong. Evaluate the claim on its merits against the diff.

Uphold it ONLY if you can state the concrete failing input, or quote the specific \
line that contradicts the premise. A plausible-sounding objection that you cannot \
ground in the diff is not upheld. Disputing is cheap for the reviewer and \
upholding is not — be hard to convince.

kind:
  premise   — the question assumes something the code does not do.
  reference — the question is fair but the stored answer is wrong.
  code_bug  — the code itself is wrong, and the reviewer has identified it.";

const DIVERGENCE_SYSTEM: &str = "\
An `exercise` asked the reviewer to run the real program and report what \
happened. You are given what a careful reader of the diff PREDICTED the run \
would print, and what the reviewer reports it actually printed.

Say only whether the two diverge on something that matters: a different value, a \
different branch taken, an error where none was predicted, nothing happening \
where something was predicted. Wording, formatting and detail the prediction \
did not mention are not divergences.

You are not grading the reviewer. The observation is evidence and the prediction \
is the claim being tested — if they disagree, the prediction is what was wrong, \
and `what` states the disagreement in one sentence.";

#[derive(Deserialize)]
struct DivergenceReply {
    #[allow(dead_code)]
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    diverged: bool,
    #[serde(default)]
    what: String,
}

const DIVERGENCE_SHAPE: &str = r#"{
  "reasoning": "what the prediction claims, and what the observation shows",
  "diverged": false,
  "what": "one sentence naming the disagreement, or an empty string"
}"#;

#[derive(Deserialize)]
struct Graded {
    #[allow(dead_code)]
    own_answer: String,
    #[allow(dead_code)]
    justification: String,
    label: String,
    feedback: String,
}

/// The dispute reply as it comes off the wire. `reasoning` is in the shape so
/// the model has to ground the claim before it commits to upholding it; nothing
/// reads it.
#[derive(Deserialize)]
struct DisputeReply {
    #[allow(dead_code)]
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    upheld: bool,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    feedback: String,
}

/// Stated in the prompt rather than enforced — the CLI has no schema support.
/// Field order matters: the model writes its own answer and its reasoning before
/// it commits to a label.
const ANSWER_SHAPE: &str = r#"{
  "own_answer": "your own answer to the question, written from the diff",
  "justification": "why the reviewer's answer earns its label",
  "label": "wrong | restates | partial | demonstrates",
  "feedback": "one or two sentences for the reviewer"
}"#;

const DISPUTE_SHAPE: &str = r#"{
  "reasoning": "the concrete failing input, or the line that contradicts the premise",
  "upheld": false,
  "kind": "premise | reference | code_bug",
  "feedback": "one or two sentences for the reviewer"
}"#;

fn parse_label(s: &str) -> Label {
    match s {
        "demonstrates" => Label::Demonstrates,
        "partial" => Label::Partial,
        "restates" => Label::Restates,
        _ => Label::Wrong,
    }
}

impl CliJudge {
    fn grade_once(&self, question: &str, answer: &str) -> Result<Graded> {
        let sources = if self.restatement_sources.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n# The reviewer can already read all of this\n\n{}",
                self.restatement_sources
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let system = format!(
            "{SYSTEM}\n\n# Reply with exactly this shape\n\n{ANSWER_SHAPE}{sources}\n\n# The diff\n\n{}",
            self.diff
        );
        let user = format!("# Question\n\n{question}\n\n# The reviewer's answer\n\n{answer}");
        let (value, _) = self.cli.complete_json(&self.model, &system, &user)?;
        Ok(serde_json::from_value(value)?)
    }

    fn dispute_once(&self, question: &str, reference: &str, claim: &str) -> Result<DisputeReply> {
        let system = format!(
            "{DISPUTE_SYSTEM}\n\n# Reply with exactly this shape\n\n{DISPUTE_SHAPE}\n\n# The diff\n\n{}",
            self.diff
        );
        let user = format!(
            "# Question\n\n{question}\n\n# Stored reference answer\n\n{reference}\n\n\
             # The reviewer's claim\n\n{claim}"
        );
        let (value, _) = self.cli.complete_json(&self.model, &system, &user)?;
        Ok(serde_json::from_value(value)?)
    }
}

impl Judge for CliJudge {
    fn dispute(&self, question: &str, reference: &str, claim: &str) -> Result<Disputed> {
        let r = self.dispute_once(question, reference, claim)?;
        // An upheld dispute with no kind would silently become `premise`, which
        // is the one that drops the question. Unrecognised means not upheld.
        let known = matches!(r.kind.as_str(), "premise" | "reference" | "code_bug");
        Ok(Disputed {
            upheld: r.upheld && known,
            kind: r.kind,
            feedback: r.feedback,
        })
    }

    fn judge(&self, _diff: &str, question: &str, answer: &str) -> Result<Verdict> {
        let first = self.grade_once(question, answer)?;
        let label = parse_label(&first.label);

        // Borderline re-judge. `partial` is the label that decides a pass under
        // the drop-lowest rule, so it is the one worth spending on. There is no
        // temperature control here, so the re-runs are plain repeats and the
        // median is taken over the model's natural variance.
        if label != Label::Partial {
            return Ok(Verdict { label, feedback: first.feedback });
        }

        let mut labels = vec![Label::Partial];
        let mut feedback = first.feedback;
        for _ in 0..2 {
            match self.grade_once(question, answer) {
                Ok(g) => {
                    let l = parse_label(&g.label);
                    if l != Label::Partial {
                        feedback = g.feedback;
                    }
                    labels.push(l);
                }
                // A failed re-judge should not fail the answer.
                Err(e) => eprintln!("  re-judge failed, keeping the first label: {e}"),
            }
        }
        labels.sort_by(|a, b| a.score().partial_cmp(&b.score()).unwrap());
        let median = labels[labels.len() / 2];
        Ok(Verdict { label: median, feedback })
    }

    fn divergence(
        &self,
        question: &str,
        prediction: &str,
        observation: &str,
    ) -> Result<Divergence> {
        let system = format!(
            "{DIVERGENCE_SYSTEM}\n\n# Reply with exactly this shape\n\n\
             {DIVERGENCE_SHAPE}\n\n# The diff\n\n{}",
            self.diff
        );
        let user = format!(
            "# The exercise\n\n{question}\n\n# What the run was predicted to print\n\n\
             {prediction}\n\n# What the reviewer reports it printed\n\n{observation}"
        );
        let (value, _) = self.cli.complete_json(&self.model, &system, &user)?;
        let r: DivergenceReply = serde_json::from_value(value)?;
        // A divergence with nothing said about it opens an obligation nobody can
        // settle, so it is not a divergence.
        Ok(Divergence {
            diverged: r.diverged && !r.what.trim().is_empty(),
            what: r.what,
        })
    }

    fn model(&self) -> &str {
        &self.model
    }
}
