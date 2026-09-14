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

    #[test]
    fn does_not_repeat_a_symbol() {
        let h = hunks("+fn alpha() {}\n+fn alpha() {}\n");
        assert_eq!(changed_symbols(&h).len(), 1);
    }
}
