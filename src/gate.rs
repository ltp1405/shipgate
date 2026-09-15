//! Gate lifecycle, pass rule, and coverage. Clearing the gate is recorded here
//! and nowhere else: the pull request itself is left alone.

use crate::{authorship, config, context, dash, db, gh, git, llm, triage, tui};
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
    pub has_intent: bool,
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
        if !self.has_intent {
            write!(f, " · NO INTENT QUESTION")?;
        }
        Ok(())
    }
}

/// §8 pass rule: drop the lowest `n / 3`, not min. The rest at `partial` or
/// better, and every dropped one no worse than `restates`.
///
/// Requiring every question ≥ 0.7 (v1) blocks a legitimate PR 27% of the time at
/// three questions if judging misfires on a good answer 10% of the time.
///
/// What has to stay fixed as §7 varies the count is the *share* forgiven, not
/// the number. Dropping exactly one asks for two thirds of the questions at
/// n = 3 and five sixths at n = 6, so a fixed drop turns a wider band into a
/// quietly stricter gate: under the same 10% noise it blocks 2.8% of good PRs
/// at three questions and 11.4% at six. `n / 3` holds the bar at two thirds
/// wherever the band lands — 1.6% at six — and leaves the three-question case
/// exactly as it was.
/// A gate every question left on an upheld dispute. `passes` takes an empty
/// slice as a pass — no questions, nothing failed — which is right for a
/// trivial diff and wrong here: disputing your way to zero questions would
/// clear the gate without answering anything.
fn nothing_was_answered(scores: &[f64], questions: usize) -> bool {
    scores.is_empty() && questions > 0
}

pub fn passes(scores: &[f64]) -> bool {
    if scores.is_empty() {
        return true;
    }
    let n = scores.len();
    // One question is the whole gate: there is nothing to drop and still have
    // asked anything.
    let drop = if n == 1 { 0 } else { (n / 3).max(1) };

    let mut sorted = scores.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // A dropped answer is forgiven, not ignored: `wrong` on any question means
    // the quiz found something you did not know.
    sorted[..drop].iter().all(|s| *s >= 0.3) && sorted[drop..].iter().all(|s| *s >= 0.6)
}

pub struct Ready {
    pub dry_run: bool,
    /// Use the offline stand-ins instead of the model. No cost, no network.
    pub offline: bool,
    /// Throw away the stored gate and generate a new one. Without it a PR that
    /// has been quizzed before replays what it already has: generation is the
    /// expensive half of a run, and the questions are worth keeping stable
    /// while you work through them.
    pub force: bool,
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

        // A stored gate is replayed, not regenerated. Generation is the
        // expensive half of a run — the whole diff through the generator — and
        // re-running `shipgate ready` to look at the quiz again is the common
        // case, not the rare one. Replaying uses the stored snapshot, so none
        // of the HEAD-derived work below applies.
        let conn = db::open()?;
        let stored = db::gate_for_pr(&conn, &repo, pr.number)?;
        if !self.force {
            if let Some(gate) = &stored {
                if !db::questions_for(&conn, gate.id)?.is_empty() {
                    return self.replay(&conn, &dir, &pr, &repo, &branch, &base_ref);
                }
            }
        }

        // Generating replaces the stored gate: the old questions stop being
        // asked, and the answers under them are archived rather than deleted.
        // Say what is being set aside — it is work already paid for, even when
        // it is no longer work you are on the hook for.
        if let Some(gate) = &stored {
            let answered = db::attempt_count(&conn, gate.id)?;
            if answered > 0 {
                eprintln!(
                    "note: archiving {answered} graded answer{} on {repo}#{} — \
                     the new questions start unanswered",
                    if answered == 1 { "" } else { "s" },
                    pr.number
                );
            }
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
            path: &dir.to_string_lossy(),
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

        // §4 — how many questions this diff is worth, before the generator is
        // asked for any. Deterministic, free, and printed, so a thin band on a
        // large PR is visible rather than inferred from the coverage line.
        let band = triage::question_band(&ai_hunks);

        let ctx = llm::Context {
            diff: &diff,
            hunks: &ai_hunks,
            call_sites: context::call_sites(&dir, &ai_hunks)?,
            test_command: context::test_command(&dir),
            pr_title: pr.title.clone(),
            commit_subjects: gh::pr_commits(&dir, pr.number).unwrap_or_default(),
            all_files: git::changed_files(&dir, &base_sha, &head_sha)?,
            questions: band,
        };
        if ctx.test_command.is_none() {
            eprintln!("note: no test command found — no checkable question is possible");
        }

        let generator: Box<dyn llm::Generator> = match self.backend() {
            Some(cli) => {
                let (min, max) = band;
                if min == max {
                    println!("Generating {min} questions…");
                } else {
                    println!("Generating {min}–{max} questions…");
                }
                Box::new(llm::generate::CliGenerator {
                    cli,
                    model: cfg.models.generate.clone(),
                })
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
        let has_intent = generated.iter().any(|g| g.kind == "intent");
        if !has_intent {
            eprintln!("warning: the generator returned no intent question");
        }
        db::set_gate_coverage(&conn, gate_id, covered.len() as i64, has_checkable)?;

        let coverage = Coverage {
            questions: generated.len(),
            hunks_ai: ai_hunks.len() as i64,
            hunks_total: all_hunks.len() as i64,
            hunks_covered: covered.len() as i64,
            authorship: scope.mode.as_str().to_string(),
            has_checkable,
            has_intent,
        };
        println!("\n{coverage}\n");

        let questions = db::questions_for(&conn, gate_id)?;
        let scores = self.quiz(
            &conn,
            &diff,
            &ai_hunks,
            questions.clone(),
            ctx.restatement_sources(),
        )?;

        if nothing_was_answered(&scores, questions.len()) {
            println!("\nEvery question left the quiz on an upheld dispute — nothing was answered.");
            return Ok(());
        }
        if !passes(&scores) {
            println!("\nNot cleared. Re-run when you want another go — disagreements are cheap.");
            return Ok(());
        }

        db::set_gate_state(&conn, gate_id, "cleared")?;
        self.report(&conn, &pr, &questions, &coverage)
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
            bail!("no stored gate for {repo}#{} — run with --force to generate one", pr.number);
        };
        let questions = db::questions_for(conn, gate.id)?;
        if questions.is_empty() {
            bail!(
                "the stored gate for {repo}#{} has no questions (state: {}) — \
                 run with --force to generate a new one",
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
            has_intent: questions.iter().any(|q| q.kind == "intent"),
        };

        println!(
            "{repo}#{} · {branch} · base {base_ref} · replaying the stored gate ({})",
            pr.number,
            &gate.head_sha[..7.min(gate.head_sha.len())]
        );
        // The stored questions belong to the stored diff. Whatever has been
        // pushed since is not being quizzed, and saying so is the difference
        // between a cheap replay and a gate that quietly covers old code.
        if gate.head_sha != git::rev_parse(dir, "HEAD")? {
            eprintln!(
                "warning: HEAD has moved since this gate was created — these questions \
                 cover the stored diff, not your new commits. --force regenerates."
            );
        }
        println!("\n{coverage}\n");

        let mut sources = vec![pr.title.clone()];
        sources.extend(gh::pr_commits(dir, pr.number).unwrap_or_default());
        let scores = self.quiz(conn, &gate.diff, &ai_hunks, questions.clone(), sources)?;
        if nothing_was_answered(&scores, questions.len()) {
            println!("\nEvery question left the quiz on an upheld dispute — nothing was answered.");
            return Ok(());
        }
        if !passes(&scores) {
            println!("\nNot cleared.");
            return Ok(());
        }
        db::set_gate_state(conn, gate.id, "cleared")?;
        self.report(conn, pr, &questions, &coverage)
    }

    /// Shared by the fresh and replayed paths.
    #[allow(clippy::too_many_arguments)]
    fn quiz(
        &self,
        conn: &Connection,
        diff: &str,
        ai_hunks: &[git::Hunk],
        questions: Vec<db::Question>,
        restatement_sources: Vec<String>,
    ) -> Result<Vec<f64>> {
        let done = questions.iter().filter(|q| q.status == "passed").count();
        if done > 0 {
            println!("{done} of {} already passed — resuming.", questions.len());
        }
        let settled = questions
            .iter()
            .filter(|q| q.status == "waived" || q.status == "deferred")
            .count();
        if settled > 0 {
            println!("{settled} out of the quiz on an upheld dispute.");
        }

        let judge: Arc<dyn llm::Judge + Send + Sync> = match self.backend() {
            Some(cli) => Arc::new(llm::judge::CliJudge {
                cli,
                model: config::load().models.judge,
                diff: diff.to_string(),
                restatement_sources,
            }),
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
            // Piped stdin: no TUI, so no disputing either. A question an
            // earlier run took out of the quiz stays out — asking it again
            // here would re-score what a judge already agreed was unfair.
            let restored = tui::App::restored_scores(&questions);
            let mut scores = Vec::new();
            for (i, q) in questions.iter().enumerate() {
                match q.status.as_str() {
                    "passed" => scores.push(restored[i]),
                    "waived" | "deferred" => continue,
                    _ => scores.push(self.ask(
                        conn, judge.as_ref(), diff, ai_hunks, q, i + 1, questions.len(),
                    )?),
                }
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

    /// §10 — what clearing the gate leaves behind. shipgate does not touch the
    /// pull request: it does not write the description, does not mark the PR
    /// ready and does not comment. The answers were for thinking with, and a
    /// description assembled from replies to questions the reader cannot see
    /// reads as a transcript, not as prose — the author writes that themselves.
    ///
    /// What is printed is what only shipgate knows: how much of the diff the
    /// quiz covered, and any obligation an upheld dispute opened.
    fn report(
        &self,
        conn: &Connection,
        pr: &gh::Pr,
        questions: &[db::Question],
        coverage: &Coverage,
    ) -> Result<()> {
        println!("\nCleared · {coverage}");

        // §8 — an upheld code_bug is an open obligation. Quietly clearing the
        // gate without saying so is how "the code is wrong" becomes a free skip.
        let deferred: Vec<&db::Question> =
            questions.iter().filter(|q| q.status == "deferred").collect();
        if !deferred.is_empty() {
            println!("\nOpen on this change:");
            for q in deferred {
                let claim = db::last_dispute(conn, q.id)?.unwrap_or_default();
                println!("  {} — {claim}", q.file);
            }
            println!("\n`shipgate status` lists these again.");
        }

        println!("\n#{} is yours to mark ready · {}", pr.number, pr.url);
        Ok(())
    }
}

/// The dashboard. Unlike every other entry point this has no working directory
/// to infer from, so each row carries the path it belongs to and the quiz runs
/// there.
pub fn dashboard() -> Result<()> {
    let conn = db::open()?;
    eprintln!("Looking for open pull requests…");
    let (rows, problems) = dash::collect(&conn)?;

    if rows.is_empty() {
        println!("No open pull requests found.");
        for p in &problems {
            eprintln!("  {p}");
        }
        if config::load().watch.is_empty() {
            let path = config::config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.config/shipgate/config.toml".into());
            println!(
                "\nNothing is being watched yet. Add a repository to {path}:\n\n                 [[watch]]\npath = \"~/workspace/your-repo\""
            );
        }
        return Ok(());
    }

    // No terminal: print the list rather than failing to start a TUI. Keeps the
    // dashboard usable from a script or a pipe.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        for r in &rows {
            let mark = if r.status.needs_you() { "*" } else { " " };
            println!(
                "{mark} {:<16} #{:<6} {:<40} {:<15} {}",
                r.repo.rsplit('/').next().unwrap_or(&r.repo),
                r.pr_number,
                r.title.chars().take(40).collect::<String>(),
                r.status.label(),
                r.coverage.clone().unwrap_or_default(),
            );
        }
        for p in &problems {
            eprintln!("warning: {p}");
        }
        return Ok(());
    }

    let chosen = tui::dash::run(
        tui::dash::Dash { rows, problems, selected: 0, refreshing: false },
        || {
            let conn = db::open()?;
            dash::collect(&conn)
        },
    )?;

    let tui::dash::Chosen::Quiz(row) = chosen else {
        return Ok(());
    };

    // An existing gate is replayed and a PR with none is quizzed from scratch,
    // which is what `ready` does by default.
    Ready { dry_run: false, offline: false, force: false }.run(&row.path)
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
                    has_intent: db::questions_for(&conn, g.id)?
                        .iter()
                        .any(|q| q.kind == "intent"),
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

    /// An empty score list is a pass — no questions, nothing failed — which is
    /// right for a trivial diff. A gate whose questions all left on a dispute
    /// reaches the same state by a different road, and must not clear.
    #[test]
    fn disputing_every_question_away_is_not_a_pass() {
        assert!(super::nothing_was_answered(&[], 3));
        assert!(!super::nothing_was_answered(&[], 0), "a trivial gate still clears");
        assert!(!super::nothing_was_answered(&[0.9], 3));
    }

    /// §7 scaled the question count; a fixed drop would have scaled the bar
    /// with it. Six questions forgive two, which is the same two thirds that
    /// three questions forgiving one is.
    #[test]
    fn the_share_forgiven_holds_as_the_count_grows() {
        assert!(passes(&[0.9, 0.9, 0.9, 0.9, 0.3, 0.3]));
        assert!(!passes(&[0.9, 0.9, 0.9, 0.3, 0.3, 0.3]));
    }

    /// Four and five questions still forgive one: n / 3 only reaches two at
    /// six, and rounding up would forgive half of a four-question quiz.
    #[test]
    fn four_and_five_questions_forgive_exactly_one() {
        assert!(passes(&[0.9, 0.9, 0.9, 0.3]));
        assert!(!passes(&[0.9, 0.9, 0.3, 0.3]));
        assert!(passes(&[0.9, 0.9, 0.9, 0.9, 0.3]));
        assert!(!passes(&[0.9, 0.9, 0.9, 0.3, 0.3]));
    }

    /// Forgiven is not ignored. A `wrong` answer means the quiz found something
    /// you did not know, at any count.
    #[test]
    fn a_wrong_answer_blocks_however_many_are_dropped() {
        assert!(!passes(&[0.9, 0.9, 0.9, 0.9, 0.3, 0.0]));
    }

    /// The §7 floor is three, but a gate can still hold fewer: an earlier run
    /// may have left one question, and the rule must not change under it.
    #[test]
    fn the_small_cases_are_unchanged() {
        assert!(passes(&[0.9, 0.3]));
        assert!(!passes(&[0.9, 0.0]));
        assert!(!passes(&[0.3, 0.3]));
    }
}
