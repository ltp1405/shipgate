//! Gate lifecycle, pass rule, coverage, and the PR description — the thing that
//! makes `shipgate ready` worth running instead of `gh pr ready`.

use crate::{authorship, config, context, db, gh, git, llm, triage, tui};
use anyhow::{bail, Context as _, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::path::Path;

pub struct Coverage {
    pub questions: usize,
    pub hunks_ai: i64,
    pub hunks_total: i64,
    pub hunks_covered: i64,
    pub authorship: String,
    pub has_checkable: bool,
}

impl std::fmt::Display for Coverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} questions · {}/{} AI hunks",
            self.questions, self.hunks_covered, self.hunks_ai
        )?;
        if self.authorship != "trailers" {
            write!(f, " · authorship: {}", self.authorship)?;
        }
        if self.hunks_ai != self.hunks_total {
            write!(f, " · {} hunks in PR", self.hunks_total)?;
        }
        if !self.has_checkable {
            write!(f, " · no checkable")?;
        }
        Ok(())
    }
}

/// §8 pass rule: drop-lowest, not min. All but one question at `partial` or
/// better, and the dropped one no worse than `restates`.
///
/// Requiring every question ≥ 0.7 (v1) blocks a legitimate PR 27% of the time at
/// three questions if judging misfires 10% of the time. Drop-lowest keeps the
/// intent at roughly 92%.
pub fn passes(scores: &[f64]) -> bool {
    if scores.is_empty() {
        return true;
    }
    if scores.len() == 1 {
        return scores[0] >= 0.6;
    }
    let mut sorted = scores.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dropped = sorted[0];
    sorted[1..].iter().all(|s| *s >= 0.6) && dropped >= 0.3
}

pub struct Ready {
    pub dry_run: bool,
    /// Use the offline stand-ins instead of the model. No cost, no network.
    pub offline: bool,
    /// Replay the stored gate instead of generating. Generation is the
    /// expensive half, so without this every look at the quiz costs a model
    /// call.
    pub reuse: bool,
}

impl Ready {
    pub fn run(&self, cwd: &Path) -> Result<()> {
        if self.offline {
            eprintln!("note: --offline — questions and grading are stand-ins, not real");
        }
        let dir = git::repo_root(cwd)?;
        let branch = git::current_branch(&dir)?;
        let repo = gh::repo_slug(&dir)?;

        let pr = gh::pr_for_branch(&dir, &branch)?
            .with_context(|| format!("no pull request found for branch {branch}"))?;

        // §5: the PR's own base is authoritative. Config and the develop →
        // master → main chain only cover the no-PR path.
        let cfg = config::load();
        let remote = git::default_remote(&dir)?;
        let base_ref = format!("{remote}/{}", pr.base_ref_name);
        let base_ref = if git::ref_exists(&dir, &base_ref) {
            base_ref
        } else {
            config::fallback_base(&dir, &repo, &cfg)?
        };

        // Replaying uses the stored snapshot, so none of the HEAD-derived work
        // below applies.
        if self.reuse {
            let conn = db::open()?;
            return self.replay(&conn, &dir, &pr, &repo, &branch, &base_ref);
        }

        let head_sha = git::rev_parse(&dir, "HEAD")?;
        if head_sha != pr.head_ref_oid {
            eprintln!(
                "warning: local HEAD {} differs from the PR head {} — push first?",
                &head_sha[..7.min(head_sha.len())],
                &pr.head_ref_oid[..7.min(pr.head_ref_oid.len())]
            );
        }
        let base_sha = git::merge_base(&dir, &base_ref, &head_sha)?;

        let diff = git::diff(&dir, &base_sha, &head_sha)?;
        let parsed = git::parse_diff(&diff);
        if parsed.is_empty() {
            bail!("no hunks between {base_ref} and HEAD — nothing to quiz");
        }
        let generated = parsed.iter().filter(|h| triage::is_generated(&h.file)).count();
        let all_hunks: Vec<git::Hunk> = parsed
            .into_iter()
            .filter(|h| !triage::is_generated(&h.file))
            .collect();
        if all_hunks.is_empty() {
            println!("Only generated or vendored files changed.");
        }

        // §3 — scope to AI-authored hunks.
        let scope = authorship::resolve(&dir, &base_sha, &head_sha)?;
        let ai_hunks: Vec<git::Hunk> = all_hunks
            .iter()
            .filter(|h| scope.contains(h))
            .cloned()
            .collect();

        print!(
            "{repo}#{} · {branch} · base {base_ref} · {} hunks, {} AI ({})",
            pr.number,
            all_hunks.len(),
            ai_hunks.len(),
            scope.mode.as_str()
        );
        if generated > 0 {
            print!(" · {generated} generated hunks excluded");
        }
        println!();

        let conn = db::open()?;
        // The gate row is written only once there are questions to put in it.
        // Creating it up front means a process killed during generation — a
        // closed pipe, a Ctrl-C — leaves an empty gate stuck in `generating`
        // that every later run has to work around.
        let new_gate = db::NewGate {
            repo: &repo,
            pr_number: pr.number,
            branch: &branch,
            base_ref: &base_ref,
            base_sha: &base_sha,
            head_sha: &head_sha,
            diff: &diff,
            hunks_total: all_hunks.len() as i64,
            hunks_ai: ai_hunks.len() as i64,
            authorship: scope.mode.as_str(),
            state: "open",
        };

        // §3 — trailers present but no AI hunks: nothing here is the model's.
        if ai_hunks.is_empty() && scope.mode == authorship::Mode::Trailers {
            println!("No AI-authored hunks in this PR ({} commits, none from Claude touched the diff).", scope.total_commits);
            let id = db::upsert_gate(&conn, &db::NewGate { state: "trivial", ..new_gate })?;
            return self.finish_trivial(&conn, id, &dir, &pr, "no AI-authored code");
        }

        // §4 — triage before spending anything.
        match triage::assess(&dir, &base_sha, &head_sha, &ai_hunks)? {
            triage::Verdict::Skip(reason) => {
                println!("Nothing worth quizzing: {reason}.");
                let id = db::upsert_gate(&conn, &db::NewGate { state: "trivial", ..new_gate })?;
                return self.finish_trivial(&conn, id, &dir, &pr, &reason);
            }
            triage::Verdict::Quiz => {}
        }

        let ctx = llm::Context {
            diff: &diff,
            hunks: &ai_hunks,
            call_sites: context::call_sites(&dir, &ai_hunks)?,
            test_command: context::test_command(&dir),
        };
        if ctx.test_command.is_none() {
            eprintln!("note: no test command found — no checkable question is possible");
        }

        let generator: Box<dyn llm::Generator> = match self.backend() {
            Some(cli) => {
                println!("Generating questions…");
                Box::new(llm::generate::CliGenerator { cli })
            }
            None => Box::new(llm::stub::StubGenerator),
        };
        let Some(generated) = generator.generate(&ctx)? else {
            println!("Generator declined: nothing worth asking.");
            let id = db::upsert_gate(&conn, &db::NewGate { state: "trivial", ..new_gate })?;
            return self.finish_trivial(&conn, id, &dir, &pr, "generator declined");
        };

        // Generation succeeded, so the gate is worth recording.
        let gate_id = db::upsert_gate(&conn, &new_gate)?;

        let new_qs: Vec<db::NewQuestion> = generated
            .iter()
            .map(|g| db::NewQuestion {
                kind: &g.kind,
                file: &g.file,
                anchor: &g.anchor,
                text: &g.text,
                reference: &g.reference,
                hints: &g.hints,
            })
            .collect();
        db::insert_questions(&conn, gate_id, &new_qs)?;

        let covered: HashSet<&str> = generated.iter().map(|g| g.anchor.as_str()).collect();
        let has_checkable = generated.iter().any(|g| g.kind == "checkable");
        db::set_gate_coverage(&conn, gate_id, covered.len() as i64, has_checkable)?;

        let coverage = Coverage {
            questions: generated.len(),
            hunks_ai: ai_hunks.len() as i64,
            hunks_total: all_hunks.len() as i64,
            hunks_covered: covered.len() as i64,
            authorship: scope.mode.as_str().to_string(),
            has_checkable,
        };
        println!("\n{coverage}\n");

        let questions = db::questions_for(&conn, gate_id)?;
        let scores = self.quiz(&conn, &diff, &ai_hunks, questions.clone())?;

        if !passes(&scores) {
            println!("\nNot cleared. Re-run when you want another go — disagreements are cheap.");
            return Ok(());
        }

        db::set_gate_state(&conn, gate_id, "cleared")?;
        let body = self.description(&conn, &dir, &pr, &questions, &coverage)?;
        self.submit(&dir, &pr, &body)
    }

    /// The model backend, or `None` to run offline. `--offline` forces the
    /// stand-ins; a missing `claude` falls back to them with a warning, since
    /// silently producing stub questions would look like real ones.
    fn backend(&self) -> Option<llm::cli::Cli> {
        if self.offline {
            return None;
        }
        match llm::cli::Cli::detect() {
            Some(cli) => Some(cli),
            None => {
                eprintln!("note: `claude` is not on PATH — falling back to stand-ins");
                None
            }
        }
    }

    /// Re-run the quiz from the stored gate: same diff, same questions, no model
    /// call. The rows are already persisted, so this is free.
    fn replay(
        &self,
        conn: &Connection,
        dir: &Path,
        pr: &gh::Pr,
        repo: &str,
        branch: &str,
        base_ref: &str,
    ) -> Result<()> {
        let Some(gate) = db::gate_for_pr(conn, repo, pr.number)? else {
            bail!("no stored gate for {repo}#{} — run without --reuse first", pr.number);
        };
        let questions = db::questions_for(conn, gate.id)?;
        if questions.is_empty() {
            bail!(
                "the stored gate for {repo}#{} has no questions (state: {}) — \
                 run without --reuse first",
                pr.number,
                gate.state
            );
        }

        // Rebuild the scope from the snapshot, not from HEAD. If the branch has
        // moved since, the stored questions belong to the stored diff.
        let parsed: Vec<git::Hunk> = git::parse_diff(&gate.diff)
            .into_iter()
            .filter(|h| !triage::is_generated(&h.file))
            .collect();
        let scope = authorship::resolve(dir, &gate.base_sha, &gate.head_sha)?;
        let ai_hunks: Vec<git::Hunk> =
            parsed.iter().filter(|h| scope.contains(h)).cloned().collect();

        let coverage = Coverage {
            questions: questions.len(),
            hunks_ai: gate.hunks_ai,
            hunks_total: gate.hunks_total,
            hunks_covered: gate.hunks_covered,
            authorship: gate.authorship.clone(),
            has_checkable: gate.has_checkable,
        };

        println!(
            "{repo}#{} · {branch} · base {base_ref} · replaying the stored gate ({})",
            pr.number,
            &gate.head_sha[..7.min(gate.head_sha.len())]
        );
        if gate.head_sha != git::rev_parse(dir, "HEAD")? {
            eprintln!("note: HEAD has moved since this gate was created");
        }
        println!("\n{coverage}\n");

        let scores = self.quiz(conn, &gate.diff, &ai_hunks, questions.clone())?;
        if !passes(&scores) {
            println!("\nNot cleared.");
            return Ok(());
        }
        db::set_gate_state(conn, gate.id, "cleared")?;
        let body = self.description(conn, dir, pr, &questions, &coverage)?;
        self.submit(dir, pr, &body)
    }

    /// Shared by the fresh and replayed paths.
    fn quiz(
        &self,
        conn: &Connection,
        diff: &str,
        ai_hunks: &[git::Hunk],
        questions: Vec<db::Question>,
    ) -> Result<Vec<f64>> {
        let judge: Arc<dyn llm::Judge + Send + Sync> = match self.backend() {
            Some(cli) => Arc::new(llm::judge::CliJudge { cli, diff: diff.to_string() }),
            None => Arc::new(llm::stub::StubJudge::default()),
        };

        // The TUI needs a terminal. Piped stdin keeps `--dry-run` scriptable.
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            tui::run(
                conn,
                Arc::clone(&judge),
                Arc::new(diff.to_string()),
                ai_hunks,
                questions,
            )
        } else {
            let mut scores = Vec::new();
            for (i, q) in questions.iter().enumerate() {
                scores.push(self.ask(
                    conn, judge.as_ref(), diff, ai_hunks, q, i + 1, questions.len(),
                )?);
            }
            Ok(scores)
        }
    }

    fn ask(
        &self,
        conn: &Connection,
        judge: &dyn llm::Judge,
        diff: &str,
        hunks: &[git::Hunk],
        q: &db::Question,
        n: usize,
        total: usize,
    ) -> Result<f64> {
        println!("── {n}/{total} · {} · {}", q.kind, q.file);
        println!("{}\n", q.text);
        print!("> ");
        std::io::stdout().flush()?;

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_string();

        // §8 precheck, before any API call.
        let verdict = if !llm::cites_the_diff(&answer, hunks) {
            llm::Verdict {
                label: llm::Label::Wrong,
                feedback: "Cites nothing from the diff — name an identifier, file or line."
                    .into(),
            }
        } else {
            judge.judge(diff, &q.text, &answer)?
        };

        let score = verdict.label.score();
        db::record_attempt(
            conn,
            q.id,
            "answer",
            &answer,
            0,
            verdict.label.as_str(),
            score,
            &verdict.feedback,
            judge.model(),
        )?;
        println!("  {} — {}\n", verdict.label.as_str(), verdict.feedback);
        Ok(score)
    }

    fn finish_trivial(
        &self,
        conn: &Connection,
        gate_id: i64,
        dir: &Path,
        pr: &gh::Pr,
        reason: &str,
    ) -> Result<()> {
        db::set_gate_state(conn, gate_id, "trivial")?;
        if self.dry_run {
            println!("[dry-run] would mark #{} ready ({reason})", pr.number);
            return Ok(());
        }
        if pr.is_draft {
            gh::pr_ready(dir, pr.number)?;
            println!("#{} marked ready ({reason}).", pr.number);
        } else {
            println!("#{} already ready ({reason}).", pr.number);
        }
        Ok(())
    }

    /// §10 — built from your answers, not the reference answers, and not a
    /// summary of the diff.
    fn description(
        &self,
        conn: &Connection,
        dir: &Path,
        pr: &gh::Pr,
        questions: &[db::Question],
        coverage: &Coverage,
    ) -> Result<String> {
        let answer_for = |kind: &str| -> Option<String> {
            questions
                .iter()
                .find(|q| q.kind == kind)
                .and_then(|q| db::last_answer(conn, q.id).ok().flatten())
                .filter(|a| !a.trim().is_empty())
        };

        let mut body = String::new();
        body.push_str("## What this changes\n\n");
        match answer_for("justification") {
            Some(a) => body.push_str(&format!("{a}\n")),
            None => {
                let commits = gh::pr_commits(dir, pr.number).unwrap_or_default();
                for c in commits {
                    body.push_str(&format!("- {c}\n"));
                }
            }
        }

        let behaviour: Vec<String> = ["prediction", "adversarial", "cross_cutting"]
            .iter()
            .filter_map(|k| answer_for(k))
            .collect();
        if !behaviour.is_empty() {
            body.push_str("\n## Behaviour worth knowing\n\n");
            for b in behaviour {
                body.push_str(&format!("- {b}\n"));
            }
        }

        if let Some(a) = answer_for("checkable") {
            body.push_str(&format!("\n## Verified\n\n{a}\n"));
        }

        // The coverage line ships in the description too: anyone trusting this
        // should see how much of the diff it covered.
        body.push_str(&format!("\n---\n<sub>{coverage} · shipgate</sub>\n"));
        Ok(body)
    }

    fn submit(&self, dir: &Path, pr: &gh::Pr, body: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("shipgate-pr-{}.md", pr.number));
        std::fs::write(&path, body)?;

        // Never posted unread.
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        if self.dry_run {
            println!("[dry-run] description written to {}", path.display());
            println!("\n{body}");
            println!("[dry-run] would run: gh pr edit {} --body-file …", pr.number);
            println!("[dry-run] would run: gh pr ready {}", pr.number);
            return Ok(());
        }

        let status = std::process::Command::new(&editor).arg(&path).status()?;
        if !status.success() {
            bail!("{editor} exited non-zero; nothing submitted");
        }

        gh::pr_set_body(dir, pr.number, &path)?;
        if pr.is_draft {
            gh::pr_ready(dir, pr.number)?;
        }
        println!("#{} ready · {}", pr.number, pr.url);
        Ok(())
    }
}

/// §11 — the soft teeth. Lists PRs that went ready without a gate.
pub fn status(cwd: &Path) -> Result<()> {
    let dir = git::repo_root(cwd)?;
    let repo = gh::repo_slug(&dir)?;
    let conn = db::open()?;

    let gates = db::gates_for_repo(&conn, &repo)?;
    let prs = gh::my_open_prs(&dir)?;

    for pr in &prs {
        let gate = gates.iter().find(|g| g.pr_number == pr.number);
        match gate {
            Some(g) if g.state == "cleared" || g.state == "trivial" => {
                let cov = Coverage {
                    questions: db::questions_for(&conn, g.id)?.len(),
                    hunks_ai: g.hunks_ai,
                    hunks_total: g.hunks_total,
                    hunks_covered: g.hunks_covered,
                    authorship: g.authorship.clone(),
                    has_checkable: g.has_checkable,
                };
                println!("{repo}#{:<5} {:<10} {cov}", pr.number, g.state);
            }
            Some(g) => println!("{repo}#{:<5} {:<10} in progress", pr.number, g.state),
            None if pr.is_draft => println!("{repo}#{:<5} {:<10} draft", pr.number, "—"),
            None => println!("{repo}#{:<5} {:<10} ready, no gate", pr.number, "—"),
        }
    }

    for o in db::open_obligations(&conn, &repo)? {
        println!("{repo}#{:<5} {:<10} \"{}\"", o.pr_number, "obligation", o.body);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::passes;

    #[test]
    fn empty_passes() {
        assert!(passes(&[]));
    }

    #[test]
    fn single_question_has_no_drop() {
        assert!(passes(&[0.6]));
        assert!(!passes(&[0.3]));
    }

    #[test]
    fn one_weak_answer_is_dropped() {
        // The v1 min rule would block this; drop-lowest does not.
        assert!(passes(&[0.9, 0.9, 0.3]));
    }

    #[test]
    fn two_weak_answers_block() {
        assert!(!passes(&[0.9, 0.3, 0.3]));
    }

    #[test]
    fn the_dropped_one_still_has_a_floor() {
        assert!(!passes(&[0.9, 0.9, 0.0]));
    }
}
