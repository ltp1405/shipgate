//! §8 — grading. The reference answer is deliberately absent from this context.

use super::{anthropic, Judge, Label, Verdict};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

pub struct ApiJudge {
    pub client: anthropic::Client,
    pub diff: String,
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

Two rules that override the rest:

- Score `wrong` if the answer would be equally true of an arbitrary code change. \
  Generic statements about rollbacks, validation, or concurrency that name \
  nothing specific to THIS diff are not answers.
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

#[derive(Deserialize)]
struct Graded {
    #[allow(dead_code)]
    own_answer: String,
    #[allow(dead_code)]
    justification: String,
    label: String,
    feedback: String,
}

#[derive(Deserialize)]
pub struct Disputed {
    pub upheld: bool,
    pub kind: String,
    pub feedback: String,
}

/// Field order matters: the model writes its own answer and its reasoning before
/// it commits to a label.
fn answer_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "own_answer": {"type": "string"},
            "justification": {"type": "string"},
            "label": {"type": "string", "enum": ["wrong", "restates", "partial", "demonstrates"]},
            "feedback": {"type": "string"}
        },
        "required": ["own_answer", "justification", "label", "feedback"],
        "additionalProperties": false
    })
}

fn dispute_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "reasoning": {"type": "string"},
            "upheld": {"type": "boolean"},
            "kind": {"type": "string", "enum": ["premise", "reference", "code_bug"]},
            "feedback": {"type": "string"}
        },
        "required": ["reasoning", "upheld", "kind", "feedback"],
        "additionalProperties": false
    })
}

fn parse_label(s: &str) -> Label {
    match s {
        "demonstrates" => Label::Demonstrates,
        "partial" => Label::Partial,
        "restates" => Label::Restates,
        _ => Label::Wrong,
    }
}

impl ApiJudge {
    fn grade_once(&self, question: &str, answer: &str, effort: &str) -> Result<Graded> {
        let system = format!("{SYSTEM}\n\n# The diff\n\n{}", self.diff);
        let user = format!("# Question\n\n{question}\n\n# The reviewer's answer\n\n{answer}");
        let (value, _) = self.client.complete(
            anthropic::JUDGE_MODEL,
            &system,
            &user,
            answer_schema(),
            effort,
        )?;
        Ok(serde_json::from_value(value)?)
    }

    pub fn dispute(&self, question: &str, reference: &str, claim: &str) -> Result<Disputed> {
        let system = format!("{DISPUTE_SYSTEM}\n\n# The diff\n\n{}", self.diff);
        let user = format!(
            "# Question\n\n{question}\n\n# Stored reference answer\n\n{reference}\n\n\
             # The reviewer's claim\n\n{claim}"
        );
        let (value, _) = self.client.complete(
            anthropic::JUDGE_MODEL,
            &system,
            &user,
            dispute_schema(),
            "high",
        )?;
        Ok(serde_json::from_value(value)?)
    }
}

impl Judge for ApiJudge {
    fn judge(&self, _diff: &str, question: &str, answer: &str) -> Result<Verdict> {
        let first = self.grade_once(question, answer, "high")?;
        let label = parse_label(&first.label);

        // Borderline re-judge. `partial` is the label that decides a pass under
        // the drop-lowest rule, so it is the one worth spending on. Sampling
        // parameters are rejected on these models, so the re-runs are plain
        // repeats and the median is over natural variance.
        if label != Label::Partial {
            return Ok(Verdict { label, feedback: first.feedback });
        }

        let mut labels = vec![Label::Partial];
        let mut feedback = first.feedback;
        for _ in 0..2 {
            match self.grade_once(question, answer, "high") {
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

    fn model(&self) -> &str {
        anthropic::JUDGE_MODEL
    }
}

#[cfg(test)]
pub fn schemas_for_test() -> Vec<(&'static str, serde_json::Value)> {
    vec![("judge.answer", answer_schema()), ("judge.dispute", dispute_schema())]
}
