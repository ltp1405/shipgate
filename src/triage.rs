//! §4 — does this diff deserve a quiz at all. Runs before a single token is
//! spent. A model asked for three questions will always produce three,
//! including for a version bump.

use crate::git;
use anyhow::Result;
use std::path::Path;

pub enum Verdict {
    Quiz,
    Skip(String),
}

const GENERATED_MARKERS: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Gemfile.lock",
    "composer.lock",
    "poetry.lock",
    "go.sum",
];

const GENERATED_DIRS: &[&str] = &["vendor/", "node_modules/", "dist/", "build/", "target/"];

/// Generated files are excluded from the quizzable hunk set, not just from the
/// all-or-nothing skip below. A lockfile riding along with real code would
/// otherwise supply hunks to quiz, and "what does the caller observe in
/// Cargo.lock" is exactly the noise §0 says drives people to the override.
pub fn is_generated(path: &str) -> bool {
    if GENERATED_MARKERS.iter().any(|m| path.ends_with(m)) {
        return true;
    }
    if GENERATED_DIRS.iter().any(|d| path.contains(d)) {
        return true;
    }
    path.ends_with(".min.js") || path.ends_with(".min.css") || path.ends_with(".map")
}

/// Crude but deterministic: does this added line carry control flow or a signature?
fn is_semantic(line: &str) -> bool {
    const TOKENS: &[&str] = &[
        "if ", "for ", "while ", "match ", "return", "fn ", "def ", "async", "class ", "func ",
        "=>", "&&", "||", "case ", "rescue", "raise", "throw", "catch",
    ];
    TOKENS.iter().any(|t| line.contains(t))
}

pub fn assess(
    dir: &Path,
    base_sha: &str,
    head_sha: &str,
    hunks: &[git::Hunk],
) -> Result<Verdict> {
    let files = git::changed_files(dir, base_sha, head_sha)?;
    if files.is_empty() {
        return Ok(Verdict::Skip("no files changed".into()));
    }
    if files.iter().all(|f| is_generated(f)) {
        return Ok(Verdict::Skip("only generated or vendored files".into()));
    }
    if git::diff_ignoring_whitespace_empty(dir, base_sha, head_sha)? {
        return Ok(Verdict::Skip("whitespace only".into()));
    }
    if git::all_hunks_are_pure_renames(dir, base_sha, head_sha)? {
        return Ok(Verdict::Skip("pure renames, no content change".into()));
    }

    let added: Vec<&String> = hunks.iter().flat_map(|h| h.added.iter()).collect();
    let semantic = added.iter().filter(|l| is_semantic(l)).count();
    if added.len() < 10 && semantic == 0 {
        return Ok(Verdict::Skip(format!(
            "{} added lines, no control flow or signature change",
            added.len()
        )));
    }

    // No line-count cap. A 2000-line diff gets the same questions and a coverage
    // line that reads badly — the number is the feedback.
    Ok(Verdict::Quiz)
}
