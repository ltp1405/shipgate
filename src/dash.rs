//! Discovery for the dashboard: what is waiting on you, across every repo.
//!
//! The gate flow is anchored to a working directory; this is not. Rows come
//! from two places — gates already in the database, which carry their own path,
//! and open PRs in watched repositories that have never been gated.

use crate::{config, db, gh};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// A gate exists and still has questions outstanding.
    InProgress { done: usize, total: usize },
    /// Every question passed.
    Cleared,
    /// Triage or the generator decided there was nothing worth asking.
    Trivial,
    /// An open PR with no gate at all.
    Ungated,
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::InProgress { done, total } => format!("{done}/{total} answered"),
            Status::Cleared => "cleared".into(),
            Status::Trivial => "nothing to ask".into(),
            Status::Ungated => "no gate".into(),
        }
    }

    /// Whether this row is actually asking something of you. The dashboard
    /// sorts by this: a list where finished work sits alongside outstanding
    /// work is a list you stop reading.
    pub fn needs_you(&self) -> bool {
        matches!(self, Status::InProgress { .. } | Status::Ungated)
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub repo: String,
    pub path: PathBuf,
    pub pr_number: u64,
    pub branch: String,
    pub title: String,
    pub status: Status,
    pub coverage: Option<String>,
}

/// Directories to inspect: everything configured, plus every path a gate
/// remembers, so anything already quizzed stays reachable without config.
fn roots(conn: &Connection, cfg: &config::Config) -> Result<Vec<PathBuf>> {
    let mut seen = BTreeSet::new();
    // Canonical, so a symlinked path and its target are one root rather than
    // two views of the same repository.
    let mut add = |p: PathBuf, seen: &mut BTreeSet<PathBuf>| {
        if p.is_dir() {
            seen.insert(p.canonicalize().unwrap_or(p));
        }
    };
    for w in &cfg.watch {
        add(w.expanded(), &mut seen);
    }
    for g in db::all_gates(conn)? {
        if g.path.is_empty() {
            continue;
        }
        add(PathBuf::from(&g.path), &mut seen);
    }
    Ok(seen.into_iter().collect())
}

/// One root per repository. A worktree is a different directory and the same
/// GitHub repository, and a gate created in one adds its path to the roots —
/// so a repo with a worktree checked out listed every one of its PRs twice,
/// once per directory. Paths cannot see this; only the slug can.
///
/// The first root wins, which is the shortest path under `roots`' ordering —
/// the main checkout rather than a worktree of it. It only decides where `gh`
/// is run: a row still carries its own gate's path, so quizzing happens in the
/// directory the gate was created in.
fn one_root_per_repo(resolved: Vec<(PathBuf, String)>) -> Vec<(PathBuf, String)> {
    let mut seen = BTreeSet::new();
    resolved
        .into_iter()
        .filter(|(_, repo)| seen.insert(repo.clone()))
        .collect()
}

/// One network round trip per repository, so this is the slow part of opening
/// the dashboard. Failures are reported rather than fatal: one unreachable
/// repo should not hide the others.
pub fn collect(conn: &Connection) -> Result<(Vec<Row>, Vec<String>)> {
    let cfg = config::load();
    let mut rows = Vec::new();
    let mut problems = Vec::new();

    // Slugs first, then one root per repository: two directories of the same
    // repo would otherwise each list all of its PRs.
    let mut resolved = Vec::new();
    for root in roots(conn, &cfg)? {
        match gh::repo_slug(&root) {
            Ok(repo) => resolved.push((root, repo)),
            Err(e) => problems.push(format!("{}: {e}", root.display())),
        }
    }

    for (root, repo) in one_root_per_repo(resolved) {
        let prs = match gh::my_open_prs(&root) {
            Ok(p) => p,
            Err(e) => {
                problems.push(format!("{repo}: {e}"));
                continue;
            }
        };

        // A gate whose PR is no longer open has nothing left to ask.
        let open: Vec<u64> = prs.iter().map(|p| p.number).collect();
        db::prune_closed(conn, &repo, &open)?;

        for pr in prs {
            let gate = db::gate_for_pr(conn, &repo, pr.number)?;
            let (status, coverage) = match &gate {
                Some(g) if g.state == "cleared" => (Status::Cleared, Some(coverage_of(conn, g)?)),
                Some(g) if g.state == "trivial" => (Status::Trivial, None),
                Some(g) => {
                    let qs = db::questions_for(conn, g.id)?;
                    let done = qs.iter().filter(|q| q.status == "passed").count();
                    (
                        Status::InProgress { done, total: qs.len() },
                        Some(coverage_of(conn, g)?),
                    )
                }
                None => (Status::Ungated, None),
            };

            rows.push(Row {
                repo: repo.clone(),
                path: gate
                    .as_ref()
                    .map(|g| PathBuf::from(&g.path))
                    .filter(|p| p.is_dir())
                    .unwrap_or_else(|| root.clone()),
                pr_number: pr.number,
                branch: pr.head_ref_name,
                title: pr.title,
                status,
                coverage,
            });
        }
    }

    // Work that wants you first; within that, lowest PR number, which is the
    // oldest and the one most worth clearing.
    rows.sort_by_key(|r| (!r.status.needs_you(), r.repo.clone(), r.pr_number));
    Ok((rows, problems))
}

fn coverage_of(conn: &Connection, g: &db::Gate) -> Result<String> {
    let n = db::questions_for(conn, g.id)?.len();
    Ok(format!("{n}q · {}/{} hunks", g.hunks_covered, g.hunks_ai))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worktree is a second directory for the same repository, and a gate
    /// created in one puts its path into the roots. Listing both meant every
    /// PR of that repo appearing twice on the dashboard.
    #[test]
    fn a_repo_with_a_worktree_is_listed_once() {
        let out = one_root_per_repo(vec![
            (PathBuf::from("/w/project"), "Org/project".into()),
            (PathBuf::from("/w/project__worktrees/feature"), "Org/project".into()),
            (PathBuf::from("/w/other"), "Org/other".into()),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, PathBuf::from("/w/project"), "the main checkout should win");
        assert_eq!(out[1].1, "Org/other");
    }

    fn row(repo: &str, pr: u64, status: Status) -> Row {
        Row {
            repo: repo.into(),
            path: PathBuf::from("/tmp"),
            pr_number: pr,
            branch: "b".into(),
            title: "t".into(),
            status,
            coverage: None,
        }
    }

    /// The dashboard is only worth opening if what it asks of you is at the
    /// top. A list where finished work sits among outstanding work is a list
    /// you stop reading.
    #[test]
    fn work_that_wants_you_sorts_above_finished_work() {
        let mut rows = [
            row("a/x", 1, Status::Cleared),
            row("a/x", 2, Status::Ungated),
            row("a/x", 3, Status::Trivial),
            row("a/x", 4, Status::InProgress { done: 1, total: 4 }),
        ];
        rows.sort_by_key(|r| (!r.status.needs_you(), r.repo.clone(), r.pr_number));

        let order: Vec<u64> = rows.iter().map(|r| r.pr_number).collect();
        assert_eq!(order, vec![2, 4, 1, 3]);
    }

    #[test]
    fn only_unfinished_work_asks_anything_of_you() {
        assert!(Status::Ungated.needs_you());
        assert!(Status::InProgress { done: 0, total: 3 }.needs_you());
        assert!(!Status::Cleared.needs_you());
        assert!(!Status::Trivial.needs_you());
    }

    #[test]
    fn progress_is_reported_as_a_fraction() {
        assert_eq!(
            Status::InProgress { done: 2, total: 4 }.label(),
            "2/4 answered"
        );
    }

    /// A gate whose PR has been merged or closed has nothing left to ask.
    /// Without pruning the list only grows, which is what turned v1's debt tab
    /// into something nobody opened.
    #[test]
    fn closed_pull_requests_are_pruned() {
        let path = std::env::temp_dir().join(format!("shipgate-dash-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = db::open_at(&path).unwrap();

        for pr in [1u64, 2, 3] {
            db::upsert_gate(
                &conn,
                &db::NewGate {
                    repo: "o/r", path: "/tmp", pr_number: pr, branch: "b",
                    base_ref: "origin/develop", base_sha: "a", head_sha: "b",
                    diff: "d", hunks_total: 1, hunks_ai: 1,
                    authorship: "trailers", state: "open",
                },
            )
            .unwrap();
        }

        // Only #2 is still open.
        let removed = db::prune_closed(&conn, "o/r", &[2]).unwrap();
        assert_eq!(removed, 2);

        let left = db::gates_for_repo(&conn, "o/r").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].pr_number, 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_tilde_path_expands_to_the_home_directory() {
        let w = config::Watch { path: "~/workspace/project-tapir".into() };
        let p = w.expanded();
        assert!(p.is_absolute(), "{p:?} should be absolute");
        assert!(!p.to_string_lossy().contains('~'));
        assert!(p.ends_with("workspace/project-tapir"));
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        let w = config::Watch { path: "/opt/src/repo".into() };
        assert_eq!(w.expanded(), PathBuf::from("/opt/src/repo"));
    }
}
