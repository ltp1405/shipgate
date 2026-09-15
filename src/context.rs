//! §6 — context gathered from static repo state only.
//!
//! v1 sent the diff alone. Two of its five question kinds are not answerable
//! that way: `cross_cutting` needs to know what calls the changed code, and
//! `checkable` needs a command that actually exists. None of this is the coding
//! session's transcript, so the generator still cannot inherit its reasoning.

use crate::git;
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;

/// Identifiers whose definition changed in the diff. Crude on purpose — a
/// missed symbol costs one weaker question, a wrong one costs nothing.
pub fn changed_symbols(hunks: &[git::Hunk]) -> Vec<String> {
    const DEFINERS: &[&str] = &[
        "fn ", "def ", "class ", "func ", "struct ", "enum ", "trait ", "impl ", "const ",
        "type ", "interface ", "module ",
    ];
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for h in hunks {
        for line in &h.added {
            for kw in DEFINERS {
                let Some(idx) = line.find(kw) else { continue };
                let rest = &line[idx + kw.len()..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.len() >= 3 && seen.insert(name.clone()) {
                    out.push(name);
                }
            }
        }
    }
    out.truncate(12);
    out
}

/// Call sites for the changed symbols, excluding the changed files themselves —
/// the point is what *elsewhere* assumes.
pub fn call_sites(dir: &Path, hunks: &[git::Hunk]) -> Result<Vec<String>> {
    let changed: HashSet<String> = hunks.iter().map(|h| h.file.clone()).collect();
    let mut out = Vec::new();
    for symbol in changed_symbols(hunks) {
        let hits = git::grep_symbol(dir, &symbol, &changed)?;
        if hits.is_empty() {
            continue;
        }
        out.push(format!("# call sites for `{symbol}`"));
        out.extend(hits.into_iter().take(8));
    }
    out.truncate(80);
    Ok(out)
}

/// Lines that stop or divert control before whatever follows them. Crude on
/// purpose, and language-agnostic: the cost of a false positive is one skipped
/// candidate, since a guard with no dispatch under it is dropped below.
const INTERCEPTORS: &[&str] = &[
    "return", "continue", "break", "bail!", "raise", "throw", "next", "halt",
    "abort", "redirect_to", "goto",
];

/// Lines that dispatch — the thing a guard placed above them can shadow.
const DISPATCH: &[&str] = &[
    "match ", "=>", "if ", "elsif", "elif", "else", "when ", "case ", "switch",
];

/// Parse the new-side start out of `@@ -12,3 +88,9 @@`.
fn new_start(header: &str) -> Option<usize> {
    let plus = header.split('+').nth(1)?;
    let num: String = plus.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

/// §6 — added guards, with the pre-existing dispatch they now sit in front of.
///
/// This is the context the `shadowed` kind needs and the only kind that needs
/// it. Every other question is about the path the diff adds; this one is about
/// what the diff quietly took over, which is not visible in the hunk at all —
/// the added lines are correct on their own terms, and the behaviour that
/// stopped happening is somewhere below them in a file the generator would
/// otherwise never see.
pub fn shadowed(dir: &Path, hunks: &[git::Hunk]) -> Result<Vec<String>> {
    let mut out = Vec::new();

    for h in hunks {
        let Ok(text) = std::fs::read_to_string(dir.join(&h.file)) else { continue };
        let lines: Vec<&str> = text.lines().collect();

        let added: HashSet<&str> =
            h.added.iter().map(|l| l.trim_start_matches('+').trim()).collect();

        // The guard: an added line that stops flow. The first one is enough —
        // a hunk with several is still one story about what now comes first.
        let Some(guard) = h
            .added
            .iter()
            .map(|l| l.trim_start_matches('+'))
            .find(|l| {
                let t = l.trim();
                !t.starts_with("//") && INTERCEPTORS.iter().any(|k| t.contains(k))
            })
        else {
            continue;
        };

        // Where that guard landed. The header's new-side start is the estimate;
        // the content is what confirms it, since an earlier hunk in the same
        // file shifts every line below it.
        let Some(from) = new_start(&h.header) else { continue };
        let Some(at) = lines
            .iter()
            .enumerate()
            .skip(from.saturating_sub(1))
            .find(|(_, l)| l.trim() == guard.trim())
            .map(|(i, _)| i)
        else {
            continue;
        };

        let indent = |l: &str| l.len() - l.trim_start().len();
        let depth = indent(lines[at]);

        // Downward until the block the guard sits in ends. Dedenting past the
        // guard means the dispatch below is no longer something it precedes.
        let mut below = Vec::new();
        for (i, line) in lines.iter().enumerate().skip(at + 1).take(60) {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if indent(line) < depth {
                break;
            }
            // Lines this change added are not what it shadowed.
            if added.contains(t) {
                continue;
            }
            if DISPATCH.iter().any(|k| t.contains(k)) {
                below.push(format!("{}: {t}", i + 1));
            }
            if below.len() == 8 {
                break;
            }
        }

        // One dispatch line below a guard is as likely to be the guard's own
        // else-branch as anything it shadows. Two is a dispatch worth asking
        // about.
        if below.len() < 2 {
            continue;
        }

        out.push(format!(
            "# `{}:{}` runs before what follows it\n{}: {}",
            h.file,
            at + 1,
            at + 1,
            lines[at].trim()
        ));
        out.extend(below);
    }

    out.truncate(60);
    Ok(out)
}

/// The project's test invocation. Without one, `checkable` questions can only be
/// invented, which is the failure they exist to avoid.
pub fn test_command(dir: &Path) -> Option<String> {
    let has = |p: &str| dir.join(p).exists();

    if has("bin/rails") {
        return Some(if has("spec") { "bundle exec rspec" } else { "bin/rails test" }.into());
    }
    if has("Cargo.toml") {
        return Some("cargo test".into());
    }
    if has("justfile") {
        let j = std::fs::read_to_string(dir.join("justfile")).ok()?;
        return j.lines().any(|l| l.starts_with("test:")).then(|| "just test".into());
    }
    if has("Makefile") {
        let mk = std::fs::read_to_string(dir.join("Makefile")).ok()?;
        return mk.lines().any(|l| l.starts_with("test:")).then(|| "make test".into());
    }
    if has("package.json") {
        let pkg = std::fs::read_to_string(dir.join("package.json")).ok()?;
        let v: serde_json::Value = serde_json::from_str(&pkg).ok()?;
        return v
            .get("scripts")
            .and_then(|s| s.get("test"))
            .is_some()
            .then(|| "npm test".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunks(added: &str) -> Vec<git::Hunk> {
        git::parse_diff(&format!(
            "diff --git a/src/sync.rs b/src/sync.rs\n@@ -1,1 +1,2 @@\n{added}"
        ))
    }

    #[test]
    fn picks_up_definitions() {
        let h = hunks("+pub fn retry_with_backoff(n: u32) {\n+struct SyncState {\n");
        let s = changed_symbols(&h);
        assert!(s.contains(&"retry_with_backoff".to_string()));
        assert!(s.contains(&"SyncState".to_string()));
    }

    #[test]
    fn ignores_lines_that_merely_call_something() {
        let h = hunks("+    let x = compute(1);\n");
        assert!(changed_symbols(&h).is_empty());
    }

    /// Write a file and a diff that claims to have added `guard` at the top of
    /// its function, so the detector has both sides to work from.
    fn shadow_fixture(file: &str, guard: &str) -> (tempdir::Dir, Vec<git::Hunk>) {
        let dir = tempdir::Dir::new();
        std::fs::write(dir.path().join("keys.rs"), file).unwrap();
        let diff = format!(
            "diff --git a/keys.rs b/keys.rs\n@@ -2,1 +2,2 @@\n{}",
            guard
                .lines()
                .map(|l| format!("+{l}\n"))
                .collect::<String>()
        );
        (dir, git::parse_diff(&diff))
    }

    const DISPATCHER: &str = "\
fn handle(key: Key) -> Result<()> {
    if is_quit(key) { return Ok(()); }
    match key {
        Key::Up => scroll(-1),
        Key::Down => scroll(1),
        _ => {}
    }
    Ok(())
}
";

    #[test]
    fn a_guard_is_reported_with_the_dispatch_below_it() {
        let (dir, h) = shadow_fixture(DISPATCHER, "    if is_quit(key) { return Ok(()); }");
        let out = shadowed(dir.path(), &h).unwrap();
        let joined = out.join("\n");
        assert!(joined.contains("runs before what follows it"), "{joined}");
        assert!(joined.contains("Key::Up"), "the shadowed dispatch is missing: {joined}");
    }

    /// The question is what the change took over, so lines the change itself
    /// added below the guard are not an answer to it.
    #[test]
    fn lines_the_change_added_are_not_reported_as_shadowed() {
        let (dir, h) = shadow_fixture(
            DISPATCHER,
            "    if is_quit(key) { return Ok(()); }\n        Key::Up => scroll(-1),",
        );
        let out = shadowed(dir.path(), &h).unwrap();
        assert!(!out.join("\n").contains("Key::Up"), "{out:?}");
    }

    /// A guard with nothing but its own else-branch under it is not a shadow.
    #[test]
    fn a_guard_with_no_dispatch_below_it_is_dropped() {
        let file = "\
fn handle(key: Key) -> Result<()> {
    if is_quit(key) { return Ok(()); }
    scroll(1);
    Ok(())
}
";
        let (dir, h) = shadow_fixture(file, "    if is_quit(key) { return Ok(()); }");
        assert!(shadowed(dir.path(), &h).unwrap().is_empty());
    }

    /// Added code that only computes cannot shadow anything.
    #[test]
    fn a_hunk_that_intercepts_nothing_is_dropped() {
        let (dir, h) = shadow_fixture(DISPATCHER, "    let n = compute(1);");
        assert!(shadowed(dir.path(), &h).unwrap().is_empty());
    }

    /// Dedenting past the guard leaves the block it sits in, so what follows is
    /// no longer something it precedes.
    #[test]
    fn dispatch_outside_the_guards_block_is_not_reported() {
        let file = "\
fn handle(key: Key) -> Result<()> {
    if is_quit(key) { return Ok(()); }
}
fn elsewhere(x: u8) {
    match x {
        1 => a(),
        2 => b(),
    }
}
";
        let (dir, h) = shadow_fixture(file, "    if is_quit(key) { return Ok(()); }");
        assert!(shadowed(dir.path(), &h).unwrap().is_empty());
    }

    #[test]
    fn the_new_side_start_is_read_from_the_header() {
        assert_eq!(new_start("@@ -12,3 +88,9 @@"), Some(88));
        assert_eq!(new_start("@@ -1,1 +5,2 @@ fn alpha()"), Some(5));
    }

    /// A scratch directory that removes itself, so the fixtures do not pile up
    /// in the temp dir across runs.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                static SEQ: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let p = std::env::temp_dir()
                    .join(format!("shipgate-ctx-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn does_not_repeat_a_symbol() {
        let h = hunks("+fn alpha() {}\n+fn alpha() {}\n");
        assert_eq!(changed_symbols(&h).len(), 1);
    }
}
