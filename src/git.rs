//! Git plumbing. Everything shells out; `git2` is not worth linking for this.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run git in `dir`. `core.quotepath=false` keeps non-ASCII paths readable;
/// `GIT_OPTIONAL_LOCKS=0` stops read-only commands taking the index lock.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn repo_root(from: &Path) -> Result<PathBuf> {
    let out = git(from, &["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(out.trim()))
}

pub fn current_branch(dir: &Path) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

pub fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    Ok(git(dir, &["rev-parse", rev])?.trim().to_string())
}

pub fn ref_exists(dir: &Path, r: &str) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", r]).is_ok()
}

/// Default remote. Never assume "origin".
pub fn default_remote(dir: &Path) -> Result<String> {
    let remotes = git(dir, &["remote"])?;
    let list: Vec<&str> = remotes.split_whitespace().collect();
    if list.is_empty() {
        bail!("no git remotes configured");
    }
    Ok(list
        .iter()
        .find(|r| **r == "origin")
        .unwrap_or(&list[0])
        .to_string())
}

pub fn merge_base(dir: &Path, a: &str, b: &str) -> Result<String> {
    Ok(git(dir, &["merge-base", a, b])?.trim().to_string())
}

pub fn diff(dir: &Path, base: &str, head: &str) -> Result<String> {
    git(
        dir,
        &[
            "diff",
            "--find-renames",
            "--no-color",
            &format!("{base}..{head}"),
        ],
    )
}

/// Whitespace-only check, used by triage.
pub fn diff_ignoring_whitespace_empty(dir: &Path, base: &str, head: &str) -> Result<bool> {
    let d = git(
        dir,
        &["diff", "-w", "--no-color", &format!("{base}..{head}")],
    )?;
    Ok(d.trim().is_empty())
}

// ---------------------------------------------------------------- hunks

#[derive(Debug, Clone)]
pub struct Hunk {
    pub file: String,
    /// Content hash of the added lines. Survives rebases and edits above it,
    /// which `@@ -88,6 +88,9 @@` does not.
    pub anchor: String,
    pub header: String,
    pub added: Vec<String>,
    pub body: String,
}

impl Hunk {
    pub fn key(&self) -> (String, String) {
        (self.file.clone(), self.anchor.clone())
    }
}

/// Hash added lines with indentation dropped and internal whitespace collapsed,
/// so reindentation and reflowing do not invalidate an anchor.
pub fn anchor_of(added: &[String]) -> String {
    let mut h = Sha256::new();
    for line in added {
        let norm = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if norm.is_empty() {
            continue;
        }
        h.update(norm.as_bytes());
        h.update(b"\n");
    }
    format!("sha256:{:x}", h.finalize())
}

pub fn parse_diff(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut file = String::new();
    let mut header = String::new();
    let mut added: Vec<String> = Vec::new();
    let mut body: Vec<String> = Vec::new();
    let mut in_hunk = false;

    let flush = |file: &str,
                 header: &str,
                 added: &mut Vec<String>,
                 body: &mut Vec<String>,
                 out: &mut Vec<Hunk>| {
        if header.is_empty() || added.is_empty() {
            added.clear();
            body.clear();
            return;
        }
        out.push(Hunk {
            file: file.to_string(),
            anchor: anchor_of(added),
            header: header.to_string(),
            added: std::mem::take(added),
            body: std::mem::take(body).join("\n"),
        });
    };

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&file, &header, &mut added, &mut body, &mut hunks);
            in_hunk = false;
            header.clear();
            // "a/path b/path" — take the b-side, which is the post-image.
            file = rest
                .split(" b/")
                .nth(1)
                .unwrap_or_else(|| rest.trim_start_matches("a/"))
                .to_string();
        } else if line.starts_with("@@") {
            flush(&file, &header, &mut added, &mut body, &mut hunks);
            in_hunk = true;
            header = line.to_string();
            body.push(line.to_string());
        } else if in_hunk {
            if line.starts_with("+++") || line.starts_with("---") {
                continue;
            }
            body.push(line.to_string());
            if let Some(content) = line.strip_prefix('+') {
                added.push(content.to_string());
            }
        }
    }
    flush(&file, &header, &mut added, &mut body, &mut hunks);
    hunks
}

/// Hunks introduced by a single commit.
pub fn commit_hunks(dir: &Path, sha: &str) -> Result<Vec<Hunk>> {
    let d = git(
        dir,
        &[
            "diff-tree",
            "-p",
            "--find-renames",
            "--no-color",
            "--no-commit-id",
            "-r",
            sha,
        ],
    )?;
    Ok(parse_diff(&d))
}

/// Files changed between two revisions, for triage.
pub fn changed_files(dir: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let out = git(
        dir,
        &["diff", "--name-only", &format!("{base}..{head}")],
    )?;
    Ok(out.lines().map(|s| s.to_string()).collect())
}

/// Rename/move pairs at 100% similarity — no content change to quiz.
pub fn all_hunks_are_pure_renames(dir: &Path, base: &str, head: &str) -> Result<bool> {
    let out = git(
        dir,
        &[
            "diff",
            "--find-renames=100%",
            "--name-status",
            &format!("{base}..{head}"),
        ],
    )?;
    let mut saw_any = false;
    for line in out.lines() {
        saw_any = true;
        if !line.starts_with('R') {
            return Ok(false);
        }
    }
    Ok(saw_any)
}

/// Call sites for a symbol, for §6 context gathering.
pub fn grep_symbol(dir: &Path, symbol: &str, exclude: &HashSet<String>) -> Result<Vec<String>> {
    let out = match git(dir, &["grep", "-n", "-w", "--", symbol]) {
        Ok(o) => o,
        // git grep exits 1 on no match.
        Err(_) => return Ok(Vec::new()),
    };
    Ok(out
        .lines()
        .filter(|l| {
            l.split(':')
                .next()
                .map(|f| !exclude.contains(f))
                .unwrap_or(true)
        })
        .take(40)
        .map(|s| s.to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/sync.rs b/src/sync.rs
index 1111111..2222222 100644
--- a/src/sync.rs
+++ b/src/sync.rs
@@ -88,6 +88,9 @@ impl Syncer {
         let mut tries = 0;
+        if self.retry_count == 0 {
+            return Err(Transient);
+        }
         loop {
@@ -200,2 +203,3 @@ impl Syncer {
     fn flush(&self) {
+        self.buffer.clear();
     }
diff --git a/src/db.rs b/src/db.rs
--- a/src/db.rs
+++ b/src/db.rs
@@ -10,1 +10,2 @@
+pub const TIMEOUT: u64 = 5000;
";

    #[test]
    fn parses_hunks_per_file() {
        let hunks = parse_diff(DIFF);
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0].file, "src/sync.rs");
        assert_eq!(hunks[2].file, "src/db.rs");
        assert_eq!(hunks[0].added.len(), 3);
        assert_eq!(hunks[2].added, vec!["pub const TIMEOUT: u64 = 5000;"]);
    }

    #[test]
    fn plus_plus_plus_header_is_not_an_added_line() {
        for h in parse_diff(DIFF) {
            assert!(h.added.iter().all(|l| !l.starts_with("++")));
        }
    }

    /// The whole point of content hashing: line numbers move under rebase,
    /// anchors must not.
    #[test]
    fn anchor_survives_line_number_shift() {
        let shifted = DIFF.replace("@@ -88,6 +88,9 @@", "@@ -140,6 +140,9 @@");
        assert_eq!(parse_diff(DIFF)[0].anchor, parse_diff(&shifted)[0].anchor);
    }

    #[test]
    fn anchor_survives_reindentation() {
        let reindented = DIFF.replace("+        if self.retry_count", "+    if self.retry_count");
        assert_eq!(parse_diff(DIFF)[0].anchor, parse_diff(&reindented)[0].anchor);
    }

    #[test]
    fn anchor_changes_when_content_changes() {
        let edited = DIFF.replace("retry_count == 0", "retry_count < 1");
        assert_ne!(parse_diff(DIFF)[0].anchor, parse_diff(&edited)[0].anchor);
    }

    #[test]
    fn distinct_hunks_get_distinct_anchors() {
        let hunks = parse_diff(DIFF);
        assert_ne!(hunks[0].anchor, hunks[1].anchor);
    }
}
