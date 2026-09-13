//! Model calls. Step 1 ships stubs behind these traits so the gate flow can be
//! proven end to end before any token is spent; §7 and §8 replace the bodies.

pub mod anthropic;
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

pub trait Judge {
    fn judge(&self, diff: &str, question: &str, answer: &str) -> Result<Verdict>;
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
