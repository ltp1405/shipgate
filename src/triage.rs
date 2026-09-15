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

/// §7 — how many questions this diff is worth: a floor, and a ceiling, with the
/// generator picking inside it.
///
/// Scaled by *areas*, not lines: a 2000-line mechanical rename is one idea and
/// a 40-line lock-ordering change across four files is four. The floor is 3
/// because the §8 pass rule drops the lowest score, and dropping one of two is
/// a coin toss; the ceiling is 6 because past that the quiz costs more than
/// reviewing the PR it is gating.
pub fn question_band(hunks: &[git::Hunk]) -> (usize, usize) {
    const FLOOR: usize = 3;
    const CEILING: usize = 6;

    let mut files: std::collections::BTreeMap<&str, usize> = Default::default();
    for h in hunks {
        if h.added.iter().any(|l| is_semantic(l)) {
            *files.entry(h.file.as_str()).or_default() += 1;
        }
    }
    // A second hunk in the same file is usually the same idea seen twice, so it
    // counts for less than a second file does.
    let areas = files.len() + files.values().map(|n| n - 1).sum::<usize>() / 2;
    let max = (2 + areas / 2).clamp(FLOOR, CEILING);
    (FLOOR.min(max), max)
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

#[cfg(test)]
mod band_tests {
    use super::*;

    fn hunk(file: &str, added: &[&str]) -> git::Hunk {
        let added: Vec<String> = added.iter().map(|s| s.to_string()).collect();
        git::Hunk {
            file: file.into(),
            anchor: git::anchor_of(&added),
            header: "@@ -1,1 +1,2 @@".into(),
            added,
            body: String::new(),
        }
    }

    #[test]
    fn a_single_small_change_gets_the_floor() {
        let (min, max) = question_band(&[hunk("a.rs", &["if x { return 1; }"])]);
        assert_eq!((min, max), (3, 3));
    }

    /// Lines are not areas. A long mechanical diff is still one idea.
    #[test]
    fn line_count_alone_does_not_buy_questions() {
        let lines: Vec<String> = (0..200).map(|i| format!("    let x{i} = 1;")).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        assert_eq!(question_band(&[hunk("a.rs", &refs)]).1, 3);
    }

    #[test]
    fn more_files_carrying_real_change_widen_the_band() {
        let hunks: Vec<git::Hunk> = (0..8)
            .map(|i| {
                let f = format!("f{i}.rs");
                hunk(&f, &["if x { return 1; }"])
            })
            .collect();
        assert_eq!(question_band(&hunks).1, 6, "eight areas should reach the ceiling");
    }

    /// Past six the quiz costs more time than reviewing the PR, which is how
    /// the gate turns into a thing people route around.
    #[test]
    fn the_ceiling_holds_however_large_the_change() {
        let hunks: Vec<git::Hunk> = (0..200)
            .map(|i| hunk(&format!("f{i}.rs"), &["match x { _ => 1 }"]))
            .collect();
        assert_eq!(question_band(&hunks).1, 6);
    }

    /// Hunks with no control flow or signature in them are noise, not areas.
    #[test]
    fn files_with_nothing_semantic_in_them_do_not_count() {
        let hunks = vec![
            hunk("a.rs", &["if x { return 1; }"]),
            hunk("b.md", &["some prose"]),
            hunk("c.txt", &["more prose"]),
        ];
        assert_eq!(question_band(&hunks).1, 3);
    }

    #[test]
    fn the_floor_never_exceeds_the_ceiling() {
        for n in 0..30 {
            let hunks: Vec<git::Hunk> = (0..n)
                .map(|i| hunk(&format!("f{i}.rs"), &["if x { return 1; }"]))
                .collect();
            let (min, max) = question_band(&hunks);
            assert!(min <= max, "band inverted at {n} areas: {min}..{max}");
        }
    }
}
