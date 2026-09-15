//! §7 — questions from the AI-authored hunks plus repo context.

use super::{cli, Context, Generated, Generator};
use anyhow::{Context as _, Result};
use serde::Deserialize;

pub struct CliGenerator {
    pub cli: cli::Cli,
    pub model: String,
}

const SYSTEM_TEMPLATE: &str = "\
You write review questions for a pull request. You see the changed hunks, the \
change's stated purpose, and static repository context — never the session that \
produced the code, so do not assume any reasoning behind it.

Produce between {min} and {max} questions that cannot be answered by paraphrasing \
anything you were given. The range is how much distinct change is in front of \
you, not a target: ask {max} only if there are {max} separate things worth \
understanding, and {min} if there are {min}. A padded question is worse than a \
missing one — it teaches the reviewer the quiz is noise.

The FIRST question must be `intent`, and there must be exactly one. It tests \
whether the reviewer understands what this change is *for* — something they \
cannot answer by reading one hunk, and cannot answer from the title or a commit \
subject, because those are handed to them too. Make it require relating at least \
two files or hunks. Good shapes:
  - these files changed together; what single change of intent required all of them
  - which of these changes could be dropped and still deliver the goal
  - what would you expect this change to have touched that it deliberately did not
  - what can a caller do now that they could not before
Never ask 'what does this PR do' or 'summarise this change'.

The remaining questions come from, in this order:
  checkable — the reviewer obtains the answer by RUNNING something. Use only a \
    command given to you in the context. If none was given, do not use this kind.
  prediction — 'if X were Y, what does the caller at this line observe'.
  adversarial — 'what input breaks this'.
  cross_cutting — 'what at <this call site> assumes this'. Use ONLY where a call \
    site appears in the context. Never invent a caller.
  shadowed — 'this guard now runs before the dispatch below it; name something \
    that used to be handled there and say what happens to it now'. Use ONLY \
    where a guard appears in the shadowed context, and name a line from the \
    dispatch quoted under it. The reference answer is read off those lines, not \
    reasoned about: which of them the guard's condition now prevents reaching. \
    Never ask this of code the change itself added below the guard.
  justification — 'why this over the obvious alternative'.

Never ask what a function does. Demand a specific value, branch, or call site — \
never 'what could go wrong'. Anchor each question to the hunk it came from by \
copying that hunk's anchor string verbatim; for the intent question, use the \
anchor of whichever hunk it leans on most.

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
      "kind": "intent | checkable | prediction | adversarial | cross_cutting | shadowed | justification",
      "file": "path/to/file",
      "anchor": "the anchor string copied verbatim from the hunk",
      "text": "the question",
      "reference": "the reference answer",
      "hints": ["a nudge", "a pointer to specific lines", "half the answer"]
    }
  ]
}"#;

/// The band is stated in the prompt rather than enforced only on the way back:
/// a generator told to write four questions writes four, and the last two are
/// padding it had to invent.
fn system_prompt(min: usize, max: usize) -> String {
    format!(
        "{}\n\n# Reply with exactly this shape\n\n{SHAPE}",
        SYSTEM_TEMPLATE
            .replace("{min}", &min.to_string())
            .replace("{max}", &max.to_string())
    )
}

/// Hold the reply to the band. Over the ceiling is trimmed; under the floor is
/// kept, since only the generator can tell a missing question from a padded one
/// and it was told to prefer the former.
fn fit(mut questions: Vec<Question>, min: usize, max: usize) -> Vec<Question> {
    if questions.len() > max {
        // The intent question is the one no other hunk can supply, so it
        // survives truncation wherever the generator happened to put it.
        if let Some(i) = questions.iter().position(|q| q.kind == "intent") {
            questions.swap(0, i);
        }
        eprintln!(
            "  generator returned {} questions for a ceiling of {max} — keeping {max}",
            questions.len()
        );
        questions.truncate(max);
    } else if questions.len() < min {
        eprintln!(
            "  note: {} questions for a floor of {min} — the generator found less to ask about",
            questions.len()
        );
    }
    questions
}

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

        user.push_str(&format!("# What this change says it is for\n\n{}\n", ctx.pr_title));
        if !ctx.commit_subjects.is_empty() {
            for c in &ctx.commit_subjects {
                user.push_str(&format!("- {c}\n"));
            }
        }
        user.push('\n');

        user.push_str(&format!(
            "# Every file this change touches\n\n{}\n\n",
            ctx.all_files.join("\n")
        ));

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
                "# Call sites\n\nNone found. Do not use the cross_cutting kind.\n\n",
            );
        } else {
            user.push_str(&format!("# Call sites\n\n{}\n\n", ctx.call_sites.join("\n")));
        }

        if ctx.shadowed.is_empty() {
            user.push_str(
                "# What this change now runs before\n\nNone found. Do not use the \
                 shadowed kind.\n",
            );
        } else {
            user.push_str(&format!(
                "# What this change now runs before\n\nEach block is a guard this \
                 change added, followed by dispatch that was already there and now \
                 sits behind it.\n\n{}\n",
                ctx.shadowed.join("\n")
            ));
        }

        let (min, max) = ctx.questions;
        let system = system_prompt(min, max);
        let (value, cost) = self
            .cli
            .complete_json(&self.model, &system, &user)?;
        let out: Output = serde_json::from_value(value)
            .context("the reply did not match the expected shape")?;

        eprintln!("  generate: ${cost:.4} ({})", self.model);

        if out.skip || out.questions.is_empty() {
            let reason = if out.reason.is_empty() { "nothing worth asking".into() } else { out.reason };
            eprintln!("  generator declined: {reason}");
            return Ok(None);
        }

        // A hallucinated anchor would silently break coverage accounting and
        // the re-quiz path, so map unknown anchors back onto a real hunk.
        let fallback = &ctx.hunks[0];
        let returned = fit(out.questions, min, max);
        let questions = returned
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

#[cfg(test)]
mod tests {
    use super::*;

    fn q(kind: &str) -> Question {
        Question {
            kind: kind.into(),
            file: "f.rs".into(),
            anchor: "a".into(),
            text: "t".into(),
            reference: "r".into(),
            hints: vec![],
        }
    }

    #[test]
    fn the_prompt_states_the_band_it_was_given() {
        let p = system_prompt(3, 6);
        assert!(p.contains("between 3 and 6 questions"), "{p}");
        assert!(!p.contains("{min}") && !p.contains("{max}"), "placeholder left in the prompt");
    }

    /// The kind is useless without its context, and worse than useless with
    /// invented context — the prompt has to say both.
    #[test]
    fn the_prompt_ties_the_shadowed_kind_to_its_context() {
        let p = system_prompt(3, 6);
        assert!(p.contains("shadowed"), "the kind is missing from the prompt");
        assert!(
            p.contains("Use ONLY \
    where a guard appears in the shadowed context"),
            "the kind is not tied to the context that makes it answerable"
        );
        assert!(p.contains("| shadowed |"), "the kind is missing from the schema");
    }

    #[test]
    fn a_reply_over_the_ceiling_is_trimmed() {
        let out = fit(vec![q("intent"), q("prediction"), q("adversarial"), q("justification")], 3, 3);
        assert_eq!(out.len(), 3);
    }

    /// Every other kind can be asked of another hunk; the intent question
    /// cannot, and a gate without one is what `NO INTENT QUESTION` warns about.
    #[test]
    fn trimming_never_drops_the_intent_question() {
        let out = fit(vec![q("prediction"), q("adversarial"), q("intent")], 3, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "intent");
    }

    /// Under the floor is the generator saying there was less here than the
    /// band assumed. Padding it back up is exactly what the band exists to stop.
    #[test]
    fn a_reply_under_the_floor_is_left_alone() {
        assert_eq!(fit(vec![q("intent"), q("prediction")], 4, 6).len(), 2);
    }
}
