//! §3 — scope the quiz to hunks from Claude-trailered commits.
//!
//! Being quizzed on lines you typed yourself is noise, and noise is what
//! teaches you to reach for the override.

use crate::git;
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;

/// How confident we are about what the AI wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Multiple commits, at least one carrying the trailer.
    Trailers,
    /// A single commit carrying the trailer — a squash merge. Whole diff in scope.
    Squashed,
    /// No trailer anywhere. Whole diff in scope, and the coverage line says so.
    Unknown,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Trailers => "trailers",
            Mode::Squashed => "squashed",
            Mode::Unknown => "unknown",
        }
    }
}

pub struct Scope {
    pub mode: Mode,
    /// Normalized `(file, added line)` pairs the AI introduced.
    ///
    /// Matching whole-hunk anchors does not work here. A commit's hunks and the
    /// cumulative `base..head` hunks are different text whenever two commits
    /// touch the same region — the branch diff merges them into one hunk whose
    /// content hash matches neither commit. That is the ordinary case on a
    /// feature branch, and it fails toward quizzing *nothing*, which is the one
    /// direction §3 must never fail in. Lines survive the recombination.
    pub ai_lines: HashSet<(String, String)>,
    pub ai_commits: Vec<String>,
    pub total_commits: usize,
}

/// Skip lines that carry no identifying information — a bare `}` or `end`
/// appears in every hunk and would put the whole diff in scope.
fn significant(line: &str) -> Option<String> {
    let norm = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let has_word = norm.chars().filter(|c| c.is_alphanumeric()).count() >= 3;
    (norm.len() >= 6 && has_word).then_some(norm)
}

impl Scope {
    /// Under Squashed and Unknown every hunk is in scope, so this is trivially true.
    pub fn contains(&self, h: &git::Hunk) -> bool {
        match self.mode {
            Mode::Trailers => h.added.iter().any(|l| {
                significant(l)
                    .map(|n| self.ai_lines.contains(&(h.file.clone(), n)))
                    .unwrap_or(false)
            }),
            _ => true,
        }
    }
}

/// The trailer text varies by model — "Claude Opus 5 (1M context)",
/// "Claude Sonnet 4.5" — so match the prefix, never an exact string.
fn is_claude(trailer_value: &str) -> bool {
    trailer_value
        .split(&[',', '<'][..])
        .next()
        .unwrap_or("")
        .trim()
        .starts_with("Claude")
}

pub fn resolve(dir: &Path, base_sha: &str, head_sha: &str) -> Result<Scope> {
    // NUL between sha and trailers so multi-line trailer blocks stay parseable.
    let log = git::git(
        dir,
        &[
            "log",
            "--format=%H%x00%(trailers:key=Co-Authored-By,valueonly)%x00",
            &format!("{base_sha}..{head_sha}"),
        ],
    )?;

    let mut ai_commits = Vec::new();
    let mut total = 0usize;

    for record in log.split('\u{0}').collect::<Vec<_>>().chunks(2) {
        let sha = record[0].trim();
        if sha.is_empty() {
            continue;
        }
        total += 1;
        let trailers = record.get(1).copied().unwrap_or("");
        if trailers.lines().any(is_claude) {
            ai_commits.push(sha.to_string());
        }
    }

    let mode = if ai_commits.is_empty() {
        Mode::Unknown
    } else if total <= 1 {
        Mode::Squashed
    } else {
        Mode::Trailers
    };

    let mut ai_lines = HashSet::new();
    if mode == Mode::Trailers {
        for sha in &ai_commits {
            for h in git::commit_hunks(dir, sha)? {
                for line in &h.added {
                    if let Some(norm) = significant(line) {
                        ai_lines.insert((h.file.clone(), norm));
                    }
                }
            }
        }
    }

    Ok(Scope {
        mode,
        ai_lines,
        ai_commits,
        total_commits: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct TempRepo(std::path::PathBuf);

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sh(dir: &Path, script: &str) {
        let out = Command::new("sh")
            .current_dir(dir)
            .arg("-c")
            .arg(script)
            .output()
            .expect("sh");
        assert!(
            out.status.success(),
            "{script}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// base -> Claude commit (a.rs) -> hand-typed commit (b.rs) -> Claude commit (a.rs)
    fn mixed_repo(name: &str) -> (TempRepo, String, String) {
        let dir = std::env::temp_dir().join(format!("shipgate-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, "git init -q -b main . && git config user.email t@t.t && git config user.name T");
        sh(&dir, "printf 'fn main() {}\\n' > a.rs && git add -A && git commit -qm base");
        let base = crate::git::rev_parse(&dir, "HEAD").unwrap();
        sh(
            &dir,
            "printf 'fn main() {\\n    let retry_count = 3;\\n}\\n' > a.rs && git add -A && \
             git commit -qm 'feat: retry

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>'",
        );
        sh(&dir, "printf 'fn helper() {}\\n' > b.rs && git add -A && git commit -qm 'my own commit'");
        sh(
            &dir,
            "printf 'fn main() {\\n    let retry_count = 3;\\n    let timeout = 5000;\\n}\\n' > a.rs && git add -A && \
             git commit -qm 'feat: timeout

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>'",
        );
        let head = crate::git::rev_parse(&dir, "HEAD").unwrap();
        (TempRepo(dir), base, head)
    }

    #[test]
    fn matches_claude_prefix_across_model_names() {
        let (repo, base, head) = mixed_repo("prefix");
        let scope = resolve(&repo.0, &base, &head).unwrap();
        assert_eq!(scope.mode, Mode::Trailers);
        assert_eq!(scope.total_commits, 3);
        // Opus 5 and Sonnet 4.5 both match; the hand-typed one does not.
        assert_eq!(scope.ai_commits.len(), 2);
    }

    #[test]
    fn hand_typed_hunks_fall_out_of_scope() {
        let (repo, base, head) = mixed_repo("scope");
        let scope = resolve(&repo.0, &base, &head).unwrap();
        let diff = crate::git::diff(&repo.0, &base, &head).unwrap();
        let hunks = crate::git::parse_diff(&diff);

        let in_scope: Vec<&str> = hunks
            .iter()
            .filter(|h| scope.contains(h))
            .map(|h| h.file.as_str())
            .collect();
        assert!(in_scope.contains(&"a.rs"), "AI file must be quizzed");
        assert!(!in_scope.contains(&"b.rs"), "hand-typed file must not be");
    }

    #[test]
    fn no_trailers_anywhere_falls_back_to_the_whole_diff() {
        let dir = std::env::temp_dir().join(format!("shipgate-test-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = TempRepo(dir.clone());
        sh(&dir, "git init -q -b main . && git config user.email t@t.t && git config user.name T");
        sh(&dir, "printf 'a\\n' > a.rs && git add -A && git commit -qm base");
        let base = crate::git::rev_parse(&dir, "HEAD").unwrap();
        sh(&dir, "printf 'a\\nb\\n' > a.rs && git add -A && git commit -qm 'no trailer'");
        let head = crate::git::rev_parse(&dir, "HEAD").unwrap();

        let scope = resolve(&repo.0, &base, &head).unwrap();
        assert_eq!(scope.mode, Mode::Unknown);
        // A squashed or untrailered branch must degrade to quizzing everything,
        // visibly — never to quizzing nothing.
        let diff = crate::git::diff(&repo.0, &base, &head).unwrap();
        assert!(crate::git::parse_diff(&diff).iter().all(|h| scope.contains(h)));
    }

    #[test]
    fn single_trailered_commit_reads_as_squashed() {
        let dir = std::env::temp_dir().join(format!("shipgate-test-sq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = TempRepo(dir.clone());
        sh(&dir, "git init -q -b main . && git config user.email t@t.t && git config user.name T");
        sh(&dir, "printf 'a\\n' > a.rs && git add -A && git commit -qm base");
        let base = crate::git::rev_parse(&dir, "HEAD").unwrap();
        sh(
            &dir,
            "printf 'a\\nb\\n' > a.rs && git add -A && git commit -qm 'squashed

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>'",
        );
        let head = crate::git::rev_parse(&dir, "HEAD").unwrap();
        let scope = resolve(&repo.0, &base, &head).unwrap();
        assert_eq!(scope.mode, Mode::Squashed);
    }
}
