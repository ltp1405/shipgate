//! Model calls. Step 1 ships stubs behind these traits so the gate flow can be
//! proven end to end before any token is spent; §7 and §8 replace the bodies.

pub mod cli;
pub mod generate;
pub mod judge;
pub mod stub;

use anyhow::Result;

pub struct Generated {
    pub kind: String,
    pub file: String,
    pub anchor: String,
    pub text: String,
    pub reference: String,
    pub hints: Vec<String>,
}

pub struct Context<'a> {
    pub diff: &'a str,
    /// AI-authored hunks only — the generator never sees the rest.
    pub hunks: &'a [crate::git::Hunk],
    /// Call sites for changed symbols (§6).
    pub call_sites: Vec<String>,
    /// Test invocation, if one was found. `checkable` questions need it.
    pub test_command: Option<String>,
    /// What the change claims to be for. Without this the generator can only
    /// see individual hunks, so it can only ask about individual hunks — and a
    /// reviewer can answer every one and still not know why the PR exists.
    pub pr_title: String,
    pub commit_subjects: Vec<String>,
    /// Every changed path, including files outside the AI scope: coherence is a
    /// property of the whole change, not of the part a model wrote.
    pub all_files: Vec<String>,
    /// How many questions this diff is worth (§4, `triage::question_band`).
    /// The generator picks inside it: only it knows whether one more question
    /// would be one more idea or padding.
    pub questions: (usize, usize),
}

impl Context<'_> {
    /// Text a reviewer could crib an "intent" answer from without reading the
    /// diff. The judge is shown these so it can refuse a restatement.
    pub fn restatement_sources(&self) -> Vec<String> {
        let mut v = vec![self.pr_title.clone()];
        v.extend(self.commit_subjects.iter().cloned());
        v.retain(|s| !s.trim().is_empty());
        v
    }
}

pub trait Generator {
    /// `Ok(None)` is the §4 refusal path: nothing here is worth asking.
    fn generate(&self, ctx: &Context) -> Result<Option<Vec<Generated>>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Wrong,
    Restates,
    Partial,
    Demonstrates,
}

impl Label {
    pub fn score(&self) -> f64 {
        match self {
            Label::Wrong => 0.0,
            Label::Restates => 0.3,
            Label::Partial => 0.6,
            Label::Demonstrates => 0.9,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Label::Wrong => "wrong",
            Label::Restates => "restates",
            Label::Partial => "partial",
            Label::Demonstrates => "demonstrates",
        }
    }
}

pub struct Verdict {
    pub label: Label,
    pub feedback: String,
}

/// §8 dispute: the reviewer's claim that the question, its reference answer or
/// the code itself is wrong, and what the judge made of it.
pub struct Disputed {
    pub upheld: bool,
    /// `premise` | `reference` | `code_bug`. Meaningless unless upheld.
    pub kind: String,
    pub feedback: String,
}

pub trait Judge {
    fn judge(&self, diff: &str, question: &str, answer: &str) -> Result<Verdict>;
    /// The reference answer goes to the judge and never to the reviewer: a
    /// dispute is judged against the code, but the reference is what the claim
    /// is often about, and revealing it would turn disputing into a way to read
    /// the answer.
    fn dispute(&self, question: &str, reference: &str, claim: &str) -> Result<Disputed>;
    fn model(&self) -> &str;
}

/// §8 deterministic precheck, run before any API call. An answer citing nothing
/// from the diff is `wrong` at zero cost — this is what closes the rubric's own
/// back door, where one generic sentence about invariants scores well on
/// anything.
pub fn cites_the_diff(answer: &str, hunks: &[crate::git::Hunk]) -> bool {
    let mut tokens: Vec<String> = Vec::new();
    for h in hunks {
        if let Some(name) = h.file.rsplit('/').next() {
            tokens.push(name.to_lowercase());
        }
        for line in &h.added {
            for word in line.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                if word.len() >= 4 && word.chars().any(|c| c.is_alphabetic()) {
                    tokens.push(word.to_lowercase());
                }
            }
        }
    }
    let lower = answer.to_lowercase();
    // A line reference counts as a citation too.
    if lower.contains(':') && lower.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    tokens.iter().any(|t| lower.contains(t.as_str()))
}

/// §8 dispute guard, run before any call. A dispute has to name a line that is
/// actually in the diff — not a file, a line. The precheck on answers accepts a
/// bare identifier, which is right for an answer and far too loose here: the
/// whole point of the guard is that disputing costs thought, or it becomes the
/// free way past a question you did not like.
pub fn cites_a_changed_line(claim: &str, hunks: &[crate::git::Hunk]) -> bool {
    hunks.iter().any(|h| {
        let Some(start) = hunk_start(&h.header) else { return false };
        // New-side lines: everything in the body but the header and removals.
        let span = h
            .body
            .lines()
            .skip(1)
            .filter(|l| !l.starts_with('-'))
            .count() as u32;
        let names: Vec<&str> = std::iter::once(h.file.as_str())
            .chain(h.file.rsplit('/').next())
            .collect();

        (start..start + span.max(1)).any(|n| {
            let cite = format!(":{n}");
            names.iter().any(|name| {
                claim
                    .match_indices(name)
                    .any(|(i, _)| claim[i + name.len()..].starts_with(&cite))
            })
        })
    })
}

/// Parse the new-side start out of `@@ -12,3 +88,9 @@`.
fn hunk_start(header: &str) -> Option<u32> {
    let plus = header.split('+').nth(1)?;
    let digits: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git;

    fn hunks() -> Vec<git::Hunk> {
        git::parse_diff(
            "diff --git a/src/sync.rs b/src/sync.rs\n@@ -1,1 +1,2 @@\n+    let retry_count = 3;\n",
        )
    }

    #[test]
    fn generic_answer_cites_nothing() {
        // The exact shape the v1 rubric rewarded: consequences, an invariant and
        // a failure mode, equally true of any diff.
        let generic = "If this fails partway the state is left inconsistent because there \
                       is no rollback, and it assumes the input is already validated upstream.";
        assert!(!cites_the_diff(generic, &hunks()));
    }

    #[test]
    fn naming_an_identifier_counts() {
        assert!(cites_the_diff("retry_count is never decremented", &hunks()));
    }

    #[test]
    fn a_line_reference_counts() {
        assert!(cites_the_diff("see sync.rs:88 for the early return", &hunks()));
    }

    #[test]
    fn short_words_do_not_count_as_citations() {
        assert!(!cites_the_diff("let it be", &hunks()));
    }
}

#[cfg(test)]
mod dispute_guard_tests {
    use super::*;
    use crate::git;

    const DIFF: &str = "\
diff --git a/src/sync.rs b/src/sync.rs
--- a/src/sync.rs
+++ b/src/sync.rs
@@ -80,2 +88,3 @@
 let before = 1;
-let gone = 2;
+let retry_count = 3;
";

    fn hunks() -> Vec<git::Hunk> {
        git::parse_diff(DIFF)
    }

    #[test]
    fn a_line_inside_the_hunk_counts() {
        assert!(cites_a_changed_line("sync.rs:89 never decrements it", &hunks()));
        assert!(cites_a_changed_line("see src/sync.rs:88", &hunks()));
    }

    #[test]
    fn a_line_outside_the_hunk_does_not() {
        assert!(!cites_a_changed_line("sync.rs:400 is the problem", &hunks()));
    }

    /// The answer precheck accepts a bare identifier. Disputing is cheap and
    /// upholding is not, so this one wants the line.
    #[test]
    fn naming_the_file_without_a_line_is_not_enough() {
        assert!(!cites_a_changed_line("sync.rs has the wrong premise", &hunks()));
        assert!(!cites_a_changed_line("retry_count is never decremented", &hunks()));
    }

    #[test]
    fn an_unrelated_file_does_not_count() {
        assert!(!cites_a_changed_line("other.rs:88 disagrees", &hunks()));
    }
}
