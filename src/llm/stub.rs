//! Step 1 only. Hardcoded questions and a judge that passes any answer over 40
//! characters, so `shipgate ready` can be proven end to end — the `gh pr ready`
//! flip and the description write — before the model work in steps 2 and 3.

use super::{Context, Generated, Generator, Judge, Label, Verdict};
use anyhow::Result;

pub struct StubGenerator;

impl Generator for StubGenerator {
    fn generate(&self, ctx: &Context) -> Result<Option<Vec<Generated>>> {
        if ctx.hunks.is_empty() {
            return Ok(None);
        }
        let pick = |n: usize| &ctx.hunks[n.min(ctx.hunks.len() - 1)];

        let mut out = vec![
            Generated {
                kind: "prediction".into(),
                file: pick(0).file.clone(),
                anchor: pick(0).anchor.clone(),
                text: format!(
                    "In {}, what does the caller observe if this hunk's happy path does not run?",
                    pick(0).file
                ),
                reference: "[stub] replaced in step 2".into(),
                hints: vec![
                    "Look at how control leaves the block.".into(),
                    "Check the error branch.".into(),
                    "[stub] half the answer".into(),
                ],
            },
            Generated {
                kind: "adversarial".into(),
                file: pick(1).file.clone(),
                anchor: pick(1).anchor.clone(),
                text: format!("What input to {} breaks this change?", pick(1).file),
                reference: "[stub] replaced in step 2".into(),
                hints: vec![
                    "Consider the empty case.".into(),
                    "Check the boundary values.".into(),
                    "[stub] half the answer".into(),
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
                reference: "[stub] replaced in step 2".into(),
                hints: vec![
                    "Run it and read the output.".into(),
                    format!("`{cmd}` covers the changed paths."),
                    "[stub] half the answer".into(),
                ],
            });
        }
        Ok(Some(out))
    }
}

pub struct StubJudge;

impl Judge for StubJudge {
    fn judge(&self, _diff: &str, _question: &str, answer: &str) -> Result<Verdict> {
        if answer.trim().len() > 40 {
            Ok(Verdict {
                label: Label::Demonstrates,
                feedback: "[stub judge] accepted on length; real grading lands in step 3.".into(),
            })
        } else {
            Ok(Verdict {
                label: Label::Restates,
                feedback: "[stub judge] too short to be a real answer.".into(),
            })
        }
    }
    fn model(&self) -> &str {
        "stub"
    }
}
