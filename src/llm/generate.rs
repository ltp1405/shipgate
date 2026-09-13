//! §7 — questions from the AI-authored hunks plus repo context.

use super::{cli, Context, Generated, Generator};
use anyhow::{Context as _, Result};
use serde::Deserialize;

pub struct CliGenerator {
    pub cli: cli::Cli,
}

const SYSTEM: &str = "\
You write review questions for a pull request. You see only the changed hunks \
and static repository context — never the session that produced the code, so do \
not assume any reasoning behind it.

Produce three to six questions that cannot be answered by paraphrasing the code. \
Prefer, in this order:

1. checkable — the reviewer obtains the answer by RUNNING something. Use only a \
   command given to you in the context. If no command was given, do not invent \
   one and do not use this kind.
2. prediction — \"if X were Y, what does the caller at this line observe\".
3. adversarial — \"what input breaks this\".
4. cross_cutting — \"what at <this call site> assumes this\". Use ONLY where a \
   call site appears in the context. Never invent a caller.
5. justification — \"why this over the obvious alternative\".

Never ask what a function does. Demand a specific value, branch, or call site — \
never \"what could go wrong\". Anchor each question to the hunk it came from by \
copying that hunk's anchor string verbatim.

For each question give a reference answer and three hints of increasing strength: \
a nudge, a pointer to specific lines, then half the answer.

If nothing here is worth asking — a version bump, a mechanical rename, a config \
tweak — set skip to true and give a reason instead of inventing questions.";

#[derive(Deserialize)]
struct Output {
    #[serde(default)]
    skip: bool,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    questions: Vec<Question>,
}

#[derive(Deserialize)]
struct Question {
    kind: String,
    file: String,
    anchor: String,
    text: String,
    reference: String,
    hints: Vec<String>,
}

/// The CLI cannot enforce a schema, so the shape is stated in the prompt and
/// validated on the way back in.
const SHAPE: &str = r#"{
  "skip": false,
  "reason": "",
  "questions": [
    {
      "kind": "checkable | prediction | adversarial | cross_cutting | justification",
      "file": "path/to/file",
      "anchor": "the anchor string copied verbatim from the hunk",
      "text": "the question",
      "reference": "the reference answer",
      "hints": ["a nudge", "a pointer to specific lines", "half the answer"]
    }
  ]
}"#;

pub fn render_hunks(ctx: &Context) -> String {
    let mut s = String::new();
    for h in ctx.hunks {
        s.push_str(&format!("--- {} (anchor: {})\n{}\n\n", h.file, h.anchor, h.body));
    }
    s
}

impl Generator for CliGenerator {
    fn generate(&self, ctx: &Context) -> Result<Option<Vec<Generated>>> {
        let mut user = String::from("# AI-authored hunks\n\n");
        user.push_str(&render_hunks(ctx));

        match &ctx.test_command {
            Some(cmd) => user.push_str(&format!(
                "# Test command\n\nThe project is tested with `{cmd}`. \
                 A checkable question may ask the reviewer to run it.\n\n"
            )),
            None => user.push_str(
                "# Test command\n\nNone found. Do not use the checkable kind.\n\n",
            ),
        }

        if ctx.call_sites.is_empty() {
            user.push_str(
                "# Call sites\n\nNone found. Do not use the cross_cutting kind.\n",
            );
        } else {
            user.push_str(&format!("# Call sites\n\n{}\n", ctx.call_sites.join("\n")));
        }

        let system = format!("{SYSTEM}\n\n# Reply with exactly this shape\n\n{SHAPE}");
        let (value, cost) = self
            .cli
            .complete_json(cli::GENERATE_MODEL, &system, &user)?;
        let out: Output = serde_json::from_value(value)
            .context("the reply did not match the expected shape")?;

        eprintln!("  generate: ${cost:.4}");

        if out.skip || out.questions.is_empty() {
            let reason = if out.reason.is_empty() { "nothing worth asking".into() } else { out.reason };
            eprintln!("  generator declined: {reason}");
            return Ok(None);
        }

        // A hallucinated anchor would silently break coverage accounting and
        // the re-quiz path, so map unknown anchors back onto a real hunk.
        let fallback = &ctx.hunks[0];
        let questions = out
            .questions
            .into_iter()
            .map(|q| {
                let known = ctx.hunks.iter().any(|h| h.anchor == q.anchor);
                let (file, anchor) = if known {
                    (q.file, q.anchor)
                } else {
                    (fallback.file.clone(), fallback.anchor.clone())
                };
                Generated {
                    kind: q.kind,
                    file,
                    anchor,
                    text: q.text,
                    reference: q.reference,
                    hints: q.hints,
                }
            })
            .collect();

        Ok(Some(questions))
    }
}
