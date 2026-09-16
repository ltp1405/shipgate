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

/// Keywords that open a declaration. A change that adds one of these adds new
/// surface; it cannot have got in front of anything that was already running.
const DECLARES: &[&str] = &[
    "fn", "def", "class", "func", "struct", "enum", "trait", "impl", "interface",
    "module", "type", "const",
];

/// Lines that dispatch — the thing a guard placed above them can shadow.
const DISPATCH: &[&str] = &[
    "match", "if", "elsif", "elif", "else", "when", "case", "switch",
];

/// Extensions whose contents are prose or data, where the keyword scan below
/// reads paragraphs as control flow — "a model can return an anchor" is not a
/// guard, and the sentences after it are not dispatch.
const NOT_CODE: &[&str] = &[
    "md", "markdown", "txt", "rst", "adoc", "org", "csv", "tsv", "json", "yaml",
    "yml", "toml", "lock", "ini", "cfg", "sql", "svg",
];

/// Keyword match on word boundaries. `contains` alone reads `returns` in a
/// sentence and `next` inside `context` as control flow.
fn has_word(line: &str, word: &str) -> bool {
    let boundary = |c: char| !(c.is_alphanumeric() || c == '_');
    let mut from = 0;
    while let Some(off) = line[from..].find(word) {
        let start = from + off;
        let end = start + word.len();
        let before = line[..start].chars().next_back().is_none_or(boundary);
        let after = line[end..].chars().next().is_none_or(boundary);
        if before && after {
            return true;
        }
        from = end;
    }
    false
}

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
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut out = Vec::new();

    for h in hunks {
        let ext = Path::new(&h.file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if NOT_CODE.contains(&ext.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join(&h.file)) else { continue };
        let lines: Vec<&str> = text.lines().collect();

        let added: Vec<&str> = h
            .added
            .iter()
            .map(|l| l.trim_start_matches('+').trim())
            .filter(|t| !t.is_empty())
            .collect();
        let added_set: HashSet<&&str> = added.iter().collect();

        // Does this hunk stop or divert flow at all? The line that does it is
        // usually buried inside the guard it belongs to — `return` sits under
        // the `if` that decides it — so it marks the hunk as a candidate
        // rather than marking the position to read from.
        let intercepts = added.iter().any(|t| {
            !t.starts_with("//")
                && !t.starts_with('#')
                && INTERCEPTORS.iter().any(|k| has_word(t, k))
        });
        if !intercepts {
            continue;
        }

        // Where the hunk landed. The header's new-side start is the estimate;
        // the content confirms it, since an earlier hunk in the same file
        // shifts every line below it.
        let Some(from) = new_start(&h.header) else { continue };
        let find = |needle: &str, after: usize| {
            lines
                .iter()
                .enumerate()
                .skip(after)
                .find(|(_, l)| l.trim() == needle)
                .map(|(i, _)| i)
        };
        let Some(first) = find(added[0], from.saturating_sub(1)) else { continue };
        let last = added
            .iter()
            .rev()
            .find_map(|t| find(t, first))
            .unwrap_or(first);

        // The floor is the outermost line the change added, not the line that
        // returns. What a guard shadows sits at the level the guard was
        // inserted at; reading from the `return` would stop at its own closing
        // brace and see nothing.
        let floor = added
            .iter()
            .filter_map(|t| find(t, first).map(|i| indent(lines[i])))
            .min()
            .unwrap_or_else(|| indent(lines[first]));

        // A hunk whose outermost added line opens a declaration is a new
        // function, not a guard in front of an old one. Reading on would walk
        // out of it and report the next declaration's dispatch as shadowed.
        let outermost = added
            .iter()
            .filter_map(|t| find(t, first).map(|i| (indent(lines[i]), *t)))
            .filter(|(d, _)| *d == floor)
            .map(|(_, t)| t)
            .next()
            .unwrap_or(added[0]);
        if DECLARES.iter().any(|k| has_word(outermost, k)) {
            continue;
        }

        // Downward until the block the change was inserted into ends.
        let mut below = Vec::new();
        for (i, line) in lines.iter().enumerate().skip(last + 1).take(80) {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if indent(line) < floor {
                break;
            }
            // A sibling declaration at the same level: past the end of whatever
            // the change was inserted into, so nothing below it was shadowed.
            if indent(line) == floor && DECLARES.iter().any(|k| has_word(t, k)) {
                break;
            }
            // Lines this change added are not what it shadowed.
            if added_set.contains(&t) {
                continue;
            }
            // `=>` is punctuation and has no word boundary; the rest are words.
            if t.contains("=>") || DISPATCH.iter().any(|k| has_word(t, k)) {
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
            "# `{}:{}` runs before what follows it\n{}",
            h.file,
            first + 1,
            added
                .iter()
                .take(6)
                .map(|t| format!("   {t}"))
                .collect::<Vec<_>>()
                .join("\n")
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

/// How the real program is started, as a user starts it. `exercise` questions
/// need it: without one the model invents a way to run the thing, and a
/// fabricated invocation costs minutes before it is found to be nonsense.
///
/// Deliberately not `test_command`. A test command is the thing that gets run
/// *instead of* the program, which is the habit the kind exists to break.
pub fn run_invocation(dir: &Path) -> Option<String> {
    let has = |p: &str| dir.join(p).exists();
    let read = |p: &str| std::fs::read_to_string(dir.join(p)).ok();

    if has("package.json") {
        if let Some(pkg) = read("package.json") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg) {
                let scripts = v.get("scripts");
                for name in ["dev", "start"] {
                    if scripts.and_then(|s| s.get(name)).is_some() {
                        return Some(format!("npm run {name}"));
                    }
                }
            }
        }
    }
    if has("bin/dev") {
        return Some("bin/dev".into());
    }
    if let Some(proc) = read("Procfile") {
        // `web: bin/rails server` — the process is what starts the program;
        // the label is Procfile's, not the shell's.
        if let Some(line) = proc.lines().find(|l| l.contains(':') && !l.trim().is_empty()) {
            if let Some((_, cmd)) = line.split_once(':') {
                let cmd = cmd.trim();
                if !cmd.is_empty() {
                    return Some(cmd.to_string());
                }
            }
        }
    }
    if let Some(just) = read("justfile") {
        for target in ["run", "dev", "serve"] {
            if just.lines().any(|l| l.starts_with(&format!("{target}:"))) {
                return Some(format!("just {target}"));
            }
        }
    }
    if let Some(cargo) = read("Cargo.toml") {
        if let Some(name) = cargo_bin_name(&cargo) {
            return Some(format!("cargo run --bin {name}"));
        }
    }
    read("README.md").as_deref().and_then(readme_usage)
}

/// The `name` of the first `[[bin]]`, or the package name where the crate has
/// no explicit one — a `src/main.rs` binary is named after its package.
fn cargo_bin_name(cargo: &str) -> Option<String> {
    let value = |line: &str| {
        line.split_once('=')
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    };

    let mut section = "";
    let mut package_name = None;
    for line in cargo.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            section = if t == "[[bin]]" {
                "bin"
            } else if t == "[package]" {
                "package"
            } else {
                ""
            };
            continue;
        }
        if !t.starts_with("name") {
            continue;
        }
        match section {
            "bin" => return value(t),
            "package" => package_name = value(t),
            _ => {}
        }
    }
    package_name
}

/// The first shell block under a README heading that reads like usage. A
/// heading match is required: the first fenced block in a README is as often
/// an install line or an example of the library's API.
fn readme_usage(readme: &str) -> Option<String> {
    const HEADINGS: &[&str] = &["usage", "running", "run", "getting started", "quick start"];

    let mut under_usage = false;
    let mut in_block = false;
    for line in readme.lines() {
        let t = line.trim();
        if let Some(title) = t.strip_prefix('#') {
            let title = title.trim_start_matches('#').trim().to_lowercase();
            under_usage = HEADINGS.iter().any(|h| title == *h || title.starts_with(h));
            in_block = false;
            continue;
        }
        if t.starts_with("```") {
            // A block opening under the heading is the one to read; anything
            // after it closes has left the section the heading vouched for.
            if in_block {
                under_usage = false;
            }
            in_block = !in_block;
            continue;
        }
        if under_usage && in_block {
            let cmd = t.trim_start_matches('$').trim();
            if !cmd.is_empty() && !cmd.starts_with('#') {
                return Some(cmd.to_string());
            }
        }
    }
    None
}

/// Where a changed symbol is reachable from: the route that dispatches to it,
/// the subcommand that reaches it, the key handler that fires it.
///
/// Paired with `run_invocation`, this is what makes an `exercise` real — the
/// invocation starts the program and the surface says where to look once it is
/// running. Either one alone is not enough, so the kind is skipped without
/// both. Migration-only and pure-internals changes legitimately have neither.
pub fn surfaces(dir: &Path, hunks: &[git::Hunk]) -> Result<Vec<String>> {
    // Word-boundary matched for the reason the `shadowed` keywords are: a
    // substring scan makes `route` out of `reroute` and `get` out of `target`.
    const MARKERS: &[&str] = &[
        "route", "routes", "get", "post", "put", "patch", "delete", "resources", "namespace",
        "scope", "Router", "Route", "path", "url", "endpoint", "handler", "Subcommand",
        "subcommand", "command", "Command", "KeyCode", "on_key", "addEventListener", "listen",
        "bind", "mount", "register", "dispatch", "menu", "before_action",
    ];

    let mut out = Vec::new();
    for symbol in changed_symbols(hunks) {
        // Unlike `call_sites`, the changed files are not excluded: a route the
        // change itself added is still the surface it sits behind.
        let hits = git::grep_symbol(dir, &symbol, &HashSet::new())?;
        let dispatches: Vec<String> = hits
            .into_iter()
            .filter(|line| !defines(line, &symbol) && has_marker(line, MARKERS))
            .take(4)
            .collect();
        if dispatches.is_empty() {
            continue;
        }
        out.push(format!("# `{symbol}` is reachable through"));
        out.extend(dispatches);
    }
    out.truncate(40);
    Ok(out)
}

/// A line that declares the symbol is where it lives, not a way to reach it.
fn defines(line: &str, symbol: &str) -> bool {
    const DEFINERS: &[&str] = &[
        "fn ", "def ", "class ", "func ", "struct ", "enum ", "trait ", "impl ", "const ",
        "type ", "interface ", "module ",
    ];
    DEFINERS.iter().any(|kw| line.contains(&format!("{kw}{symbol}")))
}

fn has_marker(line: &str, markers: &[&str]) -> bool {
    // The `file:line:` prefix `git grep -n` writes is not part of the code, and
    // a path like `src/routes/mod.rs` would otherwise vouch for every hit in it.
    let code = line.splitn(3, ':').nth(2).unwrap_or(line);
    markers.iter().any(|m| has_word(code, m))
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
        // The guard itself is echoed back as a preview, so the check is on the
        // numbered lines below it — those are the ones offered as shadowed.
        let out = shadowed(dir.path(), &h).unwrap();
        let reported: Vec<&String> = out
            .iter()
            .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .collect();
        assert!(
            !reported.iter().any(|l| l.contains("Key::Up")),
            "a line the change added was reported as shadowed: {reported:?}"
        );
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

    /// The real case this kind was built for: a prompt branch added above a key
    /// dispatch, where every arm below it is what stopped being reached.
    #[test]
    fn a_guard_added_above_a_dispatch_reports_the_arms_below_it() {
        let file = "\
fn handle(key: Key) -> Result<()> {
    if let Some(q) = typing.clone() {
        if is_quit(key) { return Ok(()); }
        return Ok(());
    }
    match key {
        Key::Up => scroll(-1),
        Key::Down => scroll(1),
    }
    Ok(())
}
";
        let (dir, h) = shadow_fixture(
            file,
            "    if let Some(q) = typing.clone() {\n        if is_quit(key) { return Ok(()); }\n        return Ok(());\n    }",
        );
        let joined = shadowed(dir.path(), &h).unwrap().join("\n");
        assert!(joined.contains("Key::Up"), "the shadowed arms are missing: {joined}");
        assert!(joined.contains("Key::Down"), "{joined}");
    }

    /// A new function is new surface. Reading on from one walks into whatever
    /// declaration follows it and reports that as shadowed.
    #[test]
    fn a_newly_added_function_shadows_nothing() {
        let file = "\
fn added(x: u8) -> bool {
    if x == 0 { return false; }
    true
}
fn existing(x: u8) {
    match x {
        1 => a(),
        2 => b(),
    }
}
";
        let (dir, h) = shadow_fixture(
            file,
            "fn added(x: u8) -> bool {\n    if x == 0 { return false; }\n    true\n}",
        );
        assert!(shadowed(dir.path(), &h).unwrap().is_empty());
    }

    /// Prose is not control flow. "a model can return an anchor" read as a
    /// guard, and the paragraphs under it as dispatch, was the first thing this
    /// reported on a real pull request.
    #[test]
    fn a_prose_file_is_not_scanned() {
        let dir = tempdir::Dir::new();
        std::fs::write(
            dir.path().join("design.md"),
            "# Design\n  a model can return an anchor that matches nothing\n  if it does, remap it\n  else keep it\n",
        )
        .unwrap();
        let diff = "diff --git a/design.md b/design.md\n@@ -1,1 +1,2 @@\n+  a model can return an anchor that matches nothing\n";
        let h = git::parse_diff(diff);
        assert!(shadowed(dir.path(), &h).unwrap().is_empty());
    }

    #[test]
    fn keywords_match_on_word_boundaries() {
        assert!(has_word("    return Ok(());", "return"));
        assert!(!has_word("the call returns early", "return"));
        assert!(!has_word("let ctx = context();", "next"));
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

    fn write(dir: &tempdir::Dir, path: &str, body: &str) {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }

    /// A repository, because `surfaces` reads the tree through `git grep`.
    fn repo() -> tempdir::Dir {
        let dir = tempdir::Dir::new();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .output()
                .unwrap();
        }
        dir
    }

    fn commit(dir: &tempdir::Dir) {
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "x"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .output()
                .unwrap();
        }
    }

    #[test]
    fn a_dev_script_is_how_the_program_starts() {
        let dir = tempdir::Dir::new();
        write(&dir, "package.json", r#"{"scripts": {"dev": "vite", "test": "vitest"}}"#);
        assert_eq!(run_invocation(dir.path()).as_deref(), Some("npm run dev"));
    }

    /// The test command is the thing that gets run *instead of* the program, so
    /// a project with only a test script has no run invocation at all.
    #[test]
    fn a_test_script_alone_is_not_a_run_invocation() {
        let dir = tempdir::Dir::new();
        write(&dir, "package.json", r#"{"scripts": {"test": "vitest"}}"#);
        assert_eq!(run_invocation(dir.path()), None);
    }

    #[test]
    fn a_procfile_process_starts_the_program() {
        let dir = tempdir::Dir::new();
        write(&dir, "Procfile", "web: bin/rails server -p 3000\nworker: bundle exec sidekiq\n");
        assert_eq!(run_invocation(dir.path()).as_deref(), Some("bin/rails server -p 3000"));
    }

    #[test]
    fn a_justfile_run_target_starts_the_program() {
        let dir = tempdir::Dir::new();
        write(&dir, "justfile", "test:\n    cargo test\nserve:\n    cargo run\n");
        assert_eq!(run_invocation(dir.path()).as_deref(), Some("just serve"));
    }

    #[test]
    fn a_crate_runs_by_its_binary_name() {
        let dir = tempdir::Dir::new();
        write(&dir, "Cargo.toml", "[package]\nname = \"shipgate\"\nversion = \"0.1.0\"\n");
        assert_eq!(run_invocation(dir.path()).as_deref(), Some("cargo run --bin shipgate"));
    }

    #[test]
    fn an_explicit_bin_name_wins_over_the_package_name() {
        let dir = tempdir::Dir::new();
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"the-crate\"\n\n[[bin]]\nname = \"the-tool\"\npath = \"src/main.rs\"\n",
        );
        assert_eq!(run_invocation(dir.path()).as_deref(), Some("cargo run --bin the-tool"));
    }

    #[test]
    fn a_readme_usage_block_is_the_last_resort() {
        let dir = tempdir::Dir::new();
        write(&dir, "README.md", "# Thing\n\nWhat it is.\n\n## Usage\n\n```\n$ thing serve --port 8080\n```\n");
        assert_eq!(run_invocation(dir.path()).as_deref(), Some("thing serve --port 8080"));
    }

    /// The first fenced block in a README is as often an install line as a way
    /// to run the thing, so a heading has to vouch for it.
    #[test]
    fn a_readme_block_under_no_usage_heading_is_not_taken() {
        let dir = tempdir::Dir::new();
        write(&dir, "README.md", "# Thing\n\n## Install\n\n```\ncargo install thing\n```\n");
        assert_eq!(run_invocation(dir.path()), None);
    }

    #[test]
    fn a_repo_with_nothing_runnable_has_no_invocation() {
        let dir = tempdir::Dir::new();
        write(&dir, "notes.txt", "nothing here");
        assert_eq!(run_invocation(dir.path()), None);
    }

    #[test]
    fn the_route_that_dispatches_to_a_changed_symbol_is_a_surface() {
        let dir = repo();
        write(&dir, "src/handlers.rs", "pub fn refund_order() {}\n");
        write(&dir, "src/router.rs", "    router.route(\"/refunds\", post(refund_order));\n");
        commit(&dir);

        let h = git::parse_diff(
            "diff --git a/src/handlers.rs b/src/handlers.rs\n@@ -1,1 +1,2 @@\n+pub fn refund_order() {}\n",
        );
        let out = surfaces(dir.path(), &h).unwrap().join("\n");
        assert!(out.contains("refund_order"), "the symbol is missing: {out}");
        assert!(out.contains("src/router.rs"), "the route is missing: {out}");
    }

    /// The definition is where the symbol lives, not a way to reach it. A
    /// surface list that offers it sends the reviewer back to the diff.
    #[test]
    fn the_definition_is_not_offered_as_a_surface() {
        let dir = repo();
        write(&dir, "src/commands.rs", "pub fn run_command(path: &str) {}\n");
        commit(&dir);

        let h = git::parse_diff(
            "diff --git a/src/commands.rs b/src/commands.rs\n@@ -1,1 +1,2 @@\n+pub fn run_command(path: &str) {}\n",
        );
        assert!(surfaces(dir.path(), &h).unwrap().is_empty());
    }

    /// Pure internals have no surface, and saying so is the point: the kind is
    /// skipped rather than invented.
    #[test]
    fn a_symbol_nothing_dispatches_to_has_no_surface() {
        let dir = repo();
        write(&dir, "src/math.rs", "pub fn normalise_ratio(n: f64) -> f64 { n }\n");
        write(&dir, "src/other.rs", "    let x = normalise_ratio(2.0);\n");
        commit(&dir);

        let h = git::parse_diff(
            "diff --git a/src/math.rs b/src/math.rs\n@@ -1,1 +1,2 @@\n+pub fn normalise_ratio(n: f64) -> f64 { n }\n",
        );
        assert!(surfaces(dir.path(), &h).unwrap().is_empty());
    }

    /// A path like `src/routes/mod.rs` would otherwise vouch for every hit in
    /// the file, since `git grep -n` writes it in front of every line.
    #[test]
    fn a_marker_in_the_path_does_not_make_a_surface() {
        let dir = repo();
        write(&dir, "src/routes/calc.rs", "pub fn compute_total() {}\n");
        write(&dir, "src/routes/helpers.rs", "    let t = compute_total();\n");
        commit(&dir);

        let h = git::parse_diff(
            "diff --git a/src/routes/calc.rs b/src/routes/calc.rs\n@@ -1,1 +1,2 @@\n+pub fn compute_total() {}\n",
        );
        assert!(surfaces(dir.path(), &h).unwrap().is_empty());
    }

    #[test]
    fn surface_markers_match_on_word_boundaries() {
        assert!(has_marker("a.rs:3:    router.get(\"/x\", h);", &["get"]));
        assert!(!has_marker("a.rs:3:    let t = target();", &["get"]));
    }

}

