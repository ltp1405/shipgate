# shipgate — design sketch (v2)

A voluntary quiz you run before marking a pull request ready for review. It generates hard questions from the AI-authored parts of the PR diff, and grades your answers. It does not touch the pull request. Solo tool. Rust, Ratatui, SQLite, `gh`.

> v1 gated `git push` via a pre-push hook and a Claude Code Stop hook. That placement fired at maximum impatience and minimum recall, on an incoherent batch, and was bypassed by `--no-verify` at no cost. It is kept at `shipgate-design.v1-pushgate.md`. This version moves the gate to the draft→ready transition and drops enforcement in favour of convenience.

## 0. The bet

Enforcement is fiction in a solo tool — you hold the override key. So the gate is not a lock. It is the most convenient way to open a PR, and it happens to require you to understand the diff first.

`shipgate ready` does not flip the PR and does not write the description — it is voluntary, and nothing is enforced. What it buys is the questions themselves: the parts of your own diff you cannot explain are cheaper to find here than in review.

Four consequences that shape everything below:

- **Only the AI's code.** Being quizzed on lines you typed yourself is noise, and noise is what teaches you to reach for the override. Scope to hunks from commits carrying a `Co-Authored-By: Claude …` trailer.
- **Open book.** The diff is days old by the time a PR is ready. Questions test reasoning about code you can see, not recall of code you wrote.
- **Honest about coverage.** Three questions on a 200-line diff is a real gate. Three on a 2000-line diff is theatre. The tool says which one you just passed.
- **The answers are the product.** What they buy is the understanding, not a document. The questions you cannot answer are the output worth having.

## 1. Crate layout

Single binary, subcommands. Shelling out to `git` and `gh` beats linking `git2` and an HTTP GitHub client for the handful of operations needed.

```
shipgate/
├── Cargo.toml
├── src/
│   ├── main.rs            # clap: ready | status | stats | show | replay
│   ├── db.rs              # rusqlite (bundled), user_version migrations, typed queries
│   ├── git.rs             # repo root, diff, hunk parsing, call-site grep, OsStr paths
│   ├── gh.rs              # gh pr view/ready/edit/comment (JSON out)
│   ├── authorship.rs      # Co-Authored-By trailers → set of AI-authored hunks
│   ├── triage.rs          # does this diff deserve a quiz at all
│   ├── context.rs         # call sites + test command, gathered from the repo
│   ├── gate.rs            # lifecycle, pass rule, coverage
│   ├── stats.rs           # pass rate by question kind over time
│   ├── llm/
│   │   ├── mod.rs         # Anthropic Messages client (reqwest blocking, on a thread)
│   │   ├── generate.rs    # (ai_hunks, context) → questions + hints + reference
│   │   └── judge.rs       # (diff, question, answer) → label; dispute mode
│   └── tui/
│       ├── mod.rs         # app loop, mpsc event bus
│       ├── quiz.rs        # split pane: diff | question + answer
```

Crates: `clap`, `ratatui`, `crossterm`, `rusqlite` (feature `bundled`), `serde`, `serde_json`, `reqwest` (blocking + json), `anyhow`, `chrono`, `directories`.

Dropped from v1: the Stop hook, the pre-push hook, `install-hooks`, the debt tab, `--emergency`, `--when-cleared`, the spaced-repetition review queue, the inbox. One PR at a time, in the foreground.

## 2. Storage

One file: `~/.local/share/shipgate/shipgate.db`. On **every** connection open:

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
```

Multi-row writes go in `BEGIN IMMEDIATE`, never `BEGIN DEFERRED` — a deferred transaction that upgrades to a write returns `SQLITE_BUSY_SNAPSHOT` immediately, and `busy_timeout` does not retry it.

Schema version lives in `PRAGMA user_version`, with ordered migration functions in `db.rs` from the first commit.

**A regeneration archives the gate it replaces.** `superseded_at` is set, the diff snapshot is cleared, and the questions and attempts hanging off it stay where they are; a partial unique index keeps the invariant that matters — one *live* gate per PR — while the archive holds as many as the PR accumulated. Everything the dashboard, the replay and the status command read is filtered to live gates, so the archive is invisible until something asks for history.

Deleting them instead was the earlier behaviour, and it made the record useless in the one direction it is worth having: the answers are the expensive half of a run, and the question "am I getting better at this" can only be asked over the ones a regeneration replaced. Closing a PR archives its gate for the same reason — a merged PR is where most of the answering happened.

The archive drops the diff because that column is far the largest and exists only so a gate can be replayed; an archived gate never is. Keeping one diff per regeneration is how an archive becomes a reason to delete the archive.

**Rebuilding a parent table needs foreign keys off.** SQLite cannot drop a table constraint, so removing `UNIQUE(repo, pr_number)` meant rebuilding `gates` — which drops it while questions and attempts still reference it. With enforcement on, `ON DELETE CASCADE` empties the database. The pragma is also a no-op inside a transaction, so it is toggled around the transaction, and `PRAGMA foreign_key_check` runs before the commit rather than trusting that it worked.

```sql
CREATE TABLE gates (
  id              INTEGER PRIMARY KEY,
  repo            TEXT NOT NULL,          -- owner/name from gh
  pr_number       INTEGER NOT NULL,
  branch          TEXT NOT NULL,
  base_ref        TEXT NOT NULL,          -- develop | master | main, from the PR
  base_sha        TEXT NOT NULL,
  head_sha        TEXT NOT NULL,
  diff            TEXT NOT NULL,
  hunks_total     INTEGER NOT NULL,       -- all hunks in the PR diff
  hunks_ai        INTEGER NOT NULL,       -- hunks from Claude-trailered commits
  hunks_covered   INTEGER NOT NULL,       -- hunks at least one question anchors to
  authorship      TEXT NOT NULL,          -- trailers | squashed | unknown
  has_checkable   INTEGER NOT NULL DEFAULT 0,
  state           TEXT NOT NULL,          -- generating | open | cleared | trivial | failed
  last_error      TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  cleared_at      TEXT,
  path            TEXT NOT NULL DEFAULT '',
  superseded_at   TEXT                    -- set when a regeneration replaces it
);

CREATE UNIQUE INDEX idx_gates_live
  ON gates(repo, pr_number) WHERE superseded_at IS NULL;

CREATE TABLE questions (
  id          INTEGER PRIMARY KEY,
  gate_id     INTEGER NOT NULL REFERENCES gates(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,              -- checkable | prediction | adversarial | cross_cutting | shadowed | justification
  file        TEXT NOT NULL,
  anchor      TEXT NOT NULL,              -- hash of normalized hunk content, NOT a line-number header
  text        TEXT NOT NULL,
  reference   TEXT NOT NULL,              -- shown after passing; never sent to the judge
  hints       TEXT NOT NULL CHECK (json_valid(hints)),
  status      TEXT NOT NULL,              -- open | passed | deferred | dropped
  label       TEXT,                       -- wrong | restates | partial | demonstrates
  score       REAL,
  created_at  TEXT NOT NULL               -- stats aggregates by kind over time
);

CREATE TABLE attempts (
  id          INTEGER PRIMARY KEY,
  question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
  mode        TEXT NOT NULL,              -- answer | dispute
  body        TEXT NOT NULL,
  hints_used  INTEGER NOT NULL DEFAULT 0,
  label       TEXT,
  score       REAL,
  feedback    TEXT,
  judge_model TEXT,                       -- which model graded; see §8
  created_at  TEXT NOT NULL
);

CREATE TABLE obligations (                -- upheld code_bug findings, settled later
  id          INTEGER PRIMARY KEY,
  gate_id     INTEGER NOT NULL REFERENCES gates(id) ON DELETE CASCADE,
  question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
  body        TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  settled_at  TEXT
);

CREATE INDEX idx_questions_gate ON questions(gate_id);
CREATE INDEX idx_questions_kind ON questions(kind, created_at);
CREATE INDEX idx_attempts_q     ON attempts(question_id);
CREATE INDEX idx_obligations_g  ON obligations(gate_id);
CREATE INDEX idx_gates_state    ON gates(state);
```

`anchor` is a hash of the hunk's added lines with whitespace collapsed and leading indentation dropped — **not** `@@ -88,6 +88,9 @@`. Line-number headers shift whenever anything above them changes, and in a workflow that rebases constantly (feature sync, hotfix→develop merges, worktree churn) every sha and every offset moves on every rebase. Header-overlap matching then silently reopens the wrong questions, or none. Content hashing survives both rebases and edits elsewhere in the file.

## 3. Authorship scoping

The cheapest high-value filter in the design. Your commits already carry the signal:

```
git log --format='%H%x00%(trailers:key=Co-Authored-By,valueonly)' base_sha..head_sha
```

Match the **`Claude` prefix**, not an exact string — the trailer varies by model (`Claude Opus 5 (1M context)`, `Claude Sonnet 4.5`). For each matching commit, `git diff-tree -p` its hunks and collect the `(file, added line)` pairs. A hunk in the cumulative `base..head` diff is in scope if it contains at least one of those lines.

**Match on lines, not on hunk anchors.** An earlier draft unioned the per-commit `(file, anchor)` set and tested the branch hunks against it. That does not work: whenever two commits touch the same region — the ordinary case on a feature branch — the branch diff merges them into one hunk whose content hash matches neither commit, and the file silently falls out of scope. It fails toward quizzing *nothing*, which is the one direction this must never fail in. Lines survive the recombination; hunk boundaries do not. Anchors remain the right identity for *questions* (§2), where both sides are read from the same cumulative diff.

Skip lines carrying no identifying information — under six normalized characters, or fewer than three alphanumerics. A bare `}` or `end` appears in every hunk and would drag the whole diff into scope.

Three honest caveats, each with a defined behaviour rather than a silent one:

- **A trailered commit can still contain hand-typed hunks.** Accepted — over-inclusion within an AI commit is cheap, and the alternative is line-level attribution nobody can produce.
- **Your later hand-edits to AI code land in *your* commits** and drop out of scope. Also accepted: you edited it, so you read it. That is the property the gate is testing for.
- **Squash merges flatten the trailers.** If `base_sha..head_sha` has one commit and it carries the trailer, the whole diff is in scope. If no commit carries it at all, set `authorship = 'unknown'`, scope to the **whole** diff, and say so in the coverage line. A squashed branch must degrade to quizzing everything, visibly — never to quizzing nothing.

Set `authorship = 'trailers'` when at least one commit matched and more than one commit exists; `'squashed'` for the single-commit case; `'unknown'` otherwise.

If `hunks_ai` is 0 under `'trailers'`, the PR has no AI-authored code. Skip to `trivial` and say why.

## 4. Coverage honesty

Nothing is silently capped, and nothing is silently trimmed.

The gate records `hunks_total`, `hunks_ai` and `hunks_covered` — the number of AI hunks at least one question anchors to. Every surface prints it:

```
feature/pc-10183 · 5 questions · 6/31 AI hunks · no checkable
```

Two effects, both intended. You can see when a pass means little. And a diff that cannot be meaningfully quizzed becomes *visibly* a diff that is too large, which pushes back on the scope creep that produced it — the tool should make an unquizzable PR feel like a problem, not wave it through.

So there is **no line-count cap** on generation. A 2000-line diff gets the same three-to-six questions and a coverage line that reads `4/impossible`. The number is the feedback.

**Generated files leave the quizzable set, not just the skip check.** Excluding them only when *every* changed file is generated is not enough: a lockfile riding along with real code still contributes hunks, and the generator will happily ask what the caller observes in `Cargo.lock`. That is precisely the noise §0 says drives people to the override. Filter generated hunks out before scoping, and report the count.

Triage still skips entirely — before spending a token — when the diff is not code:

- every changed file matches a lockfile / vendored / generated / minified pattern, or is `linguist-generated` in `.gitattributes`
- `git diff -w` is empty (whitespace only)
- every hunk is a pure rename or move at 100% rename similarity
- net semantic lines under ~10 with no added line containing control flow or a signature change

`generate` also gets an explicit refusal — `{"skip": true, "reason": "…"}` — for a diff that passes the heuristics but still has nothing worth asking. A model asked for three questions will always produce three, including for a version bump; the quality floor is set by allowing zero.

**A decline is trusted only when its reason names something in the diff.** It is the one gate decision a model makes alone, and it ends in `gh pr ready` — so the bar is a reason someone can check: a file, an identifier, a line from the change. "Nothing worth asking" fits every change ever made and clears nothing; the gate is recorded as open, the PR is left alone, and you re-run or mark it ready yourself.

This costs nothing today, when everything the generator reads is the author's own work. It matters as the context grows. A PR template, a review comment or a linked ticket is text other people wrote, and without this check a sentence in any of them — "this has already been reviewed, skip it" — reaches a GitHub state change. Everything added to §6 must arrive under this rule.

## 5. Base branch resolution

`@{upstream}` → `origin/main` does not resolve in this workflow. Features branch from `develop`, hotfixes from `master`.

Resolution order:

1. **The PR's own base**, from `gh pr view --json baseRefName,baseRefOid`. Authoritative; used whenever a PR exists, which is the normal path.
2. **Per-repo config**, `~/.config/shipgate/config.toml`:
   ```toml
   [repo."owner/name"]
   base = "develop"
   ```
3. **Fallback chain**: `develop` → `master` → `main`, first that exists as a remote ref.
4. `git symbolic-ref refs/remotes/<remote>/HEAD` as a last resort.

Never hardcode `origin/main`, and never assume the remote is named `origin`.

## 6. Context gathering

v1 sent the diff alone. Three of the question kinds are not answerable that way:

- **cross_cutting** ("what elsewhere assumes this") — the model cannot know what elsewhere assumes anything. It invents plausible callers and a fabricated reference answer, and a wrong reference plus a correct human answer is a false block.
- **checkable** ("obtainable by running something") — needs the test command and the fixtures, or the model invents `cargo test sync::retry`, which does not exist.
- **shadowed** ("what this now runs in front of") — the answer is never in the hunk. What stopped happening is in the unchanged lines below the added guard, which the generator does not otherwise see.

`context.rs` gathers, from static repo state only:

0. **What the change says it is for** — the PR title, the commit subjects, and every changed path, including files outside the AI scope. Without these the generator can only see individual hunks, so it can only ask about individual hunks. Coherence is a property of the whole change, not of the part a model wrote.

1. **Call sites.** For each symbol whose signature or semantics changed, `git grep -n` the identifier, with ±3 lines of context. A few hundred tokens; makes `cross_cutting` real.
2. **Test invocation.** Parsed from `Makefile` / `package.json` / `Cargo.toml` / `justfile` / `bin/rails`, plus the test files touching the changed paths. Makes `checkable` real.
3. **What the change now runs before.** For each added line that stops or diverts flow — `return`, `continue`, `break`, `raise`, `redirect_to` — the unchanged dispatch below it, to the end of the block it sits in. Makes `shadowed` real.
4. **Before-content** of each changed file under ~200 lines.

None of it is the coding session's transcript. The generator never sees the reasoning that produced the code, so it cannot inherit its mistakes.

Commit subjects are the one debatable inclusion, being artifacts of that session. They are admitted because they are committed, reviewable text in the repository rather than raw session reasoning — but admitting them means the reviewer can read them too, which is exactly why §8 must refuse an answer that only echoes them.

## 7. `generate`

Input is the **AI-authored hunks** plus context, not the whole diff.

**A band of three to six questions, and the first is always `intent`.**

An earlier version asked for an open three to six, ranked `checkable` first and `justification` last, and fed the generator nothing but the AI-authored hunks. Every question it produced was local mechanism — trace this branch, name the failing test. A reviewer could answer all of them correctly and still not say what the change was *for*. That is the more damaging gap of the two: mechanism can be re-derived from the code later, but a change whose purpose nobody knows is the one that rots. The fix was context and a fixed count of four; the count was the part that did not need fixing.

**The band, not the count.** `triage::question_band` computes a floor and ceiling from the diff before a token is spent, and the generator picks inside it. Deterministic and free — no second model call to choose a number, which would re-read the diff to learn less than the generator already knows while holding it.

Scaled by *areas*, not lines: distinct files carrying a semantic change, plus each further hunk in them at half weight. A 2000-line mechanical rename is one idea; a 40-line lock-ordering change across four files is four. `max = clamp(2 + areas / 2, 3, 6)`.

- **The floor is 3** because the pass rule drops the lowest score, and dropping one of two is a coin toss.
- **The ceiling is 6** because the quiz competes with reviewing the PR. Past that it becomes the thing you route around, which §0 says is how this fails.
- **The prompt states the range as a range**, with "a padded question is worse than a missing one". A generator told to write four writes four, and the last two are invented. Under the floor is accepted and noted; over the ceiling is trimmed, and the `intent` question survives the trim wherever it appeared.

Open: the §8 pass rule is count-sensitive. Drop-lowest tolerates one bad judge call at three questions and demands five good ones at six, so a wider band quietly tightens the gate. Measure the pass rate at five and six before treating the ceiling as settled; the fix, if it needs one, is dropping the lowest `n / 3` rather than the lowest one.

**intent** must relate at least two files or hunks. Shapes that work: what single change of intent required all these files; which of these changes could be dropped and still deliver the goal; what would you expect this to have touched that it deliberately did not; what can a caller do now that they could not before. Never "what does this PR do" or "summarise this change" — those are paraphrase, which this section exists to prevent.

The remaining questions come from, in order: **checkable** (the reviewer obtains the answer by *running* something — only a command actually supplied), **prediction** ("if X were Y, what does the caller at this line observe"), **adversarial** ("what input breaks this"), **cross_cutting** ("what at the given call sites assumes this", only where a call site was supplied), **shadowed** ("name something the dispatch below this guard used to handle, and say what happens to it now", only where a guard was supplied), **justification** ("why this over the obvious alternative").

Never ask what a function does. Demand a specific value, branch or call site, never "what could go wrong". For each, give a reference answer and three hints of increasing strength. Return JSON only, or `{"skip": true, "reason": "…"}`.

If the generator returns no intent question, the coverage line says so rather than passing quietly.

**`shadowed` asks about the code the change did not touch.** Every other kind is aimed at the lines in front of you: trace this branch, break this input, name this caller. None of them reach the behaviour a change *removes by interposition* — a guard clause, a new match arm, a `before_action`, an early `return` placed above dispatch that was already working. The added lines are correct read on their own; what broke is that something below them stopped being reachable. The question names the guard and asks the reviewer to name a case underneath it and say what became of that case.

Its reference answer is unusually solid for a free-text question: the dispatch lines are quoted in the context, so the reference is read off them — which of these does the guard's condition now prevent reaching — rather than reasoned about. That puts it nearer `checkable` than `prediction` on how often the judge can be wrong about a correct answer.

Like `cross_cutting`, it is skipped where the context is empty, and for the same reason: without the quoted dispatch the model invents a plausible shadow, and a fabricated reference plus a correct human answer is a false block. The detector is deliberately crude — a keyword scan for flow control in the added lines, then the unchanged dispatch below them to the end of the block the change was inserted into — and requires two dispatch lines before it reports anything, since one under a guard is as likely to be that guard's own else-branch.

Three rules do the real work, each earned by a false positive on a live PR:

- **Prose and data files are skipped by extension.** "A model can return an anchor" is a sentence, not a guard, and the paragraphs under it are not dispatch. The first thing this ever reported was three paragraphs of this document.
- **Keywords match on word boundaries**, or `returns` is a return and `context` contains a `next`.
- **A hunk whose outermost added line opens a declaration is skipped**, and the downward scan stops at the next declaration at that level. An added function is new surface and shadows nothing; reading on from one walks out of it and reports the following function's dispatch.

The level read from is the outermost line the change added, not the line that returns. A `return` sits inside the `if` that decides it, so reading from the `return` stops at its own closing brace and sees nothing — which is exactly what the first working version did on the case it was built for.

**At least one `checkable` per gate, where a test command exists.** This is the structural defence against the shared blind spot in §8: a question you settle by running `bin/rails runner` or `cargo test` has a ground truth outside the model. Where `context.rs` found no runnable command, or the change has no observable behaviour (pure refactor), the generator may return none — and the gate prints `no checkable` in its coverage line. Requiring one unconditionally would only make the model invent a command, which is the exact failure being defended against.

```json
[
  {
    "kind": "checkable",
    "file": "app/models/return_order.rb",
    "anchor": "sha256:…",
    "text": "Run the reconciler against a return with two partially-refunded items. What does total_refunded report, and does it match the sum of the line items?",
    "reference": "…",
    "hints": ["…", "…", "…"]
  }
]
```

Calls go through the Claude Code CLI (`claude -p --output-format json --restricted`), not the Messages API. Rust has no official Anthropic SDK, so a raw-HTTP client could only ever be checked against documentation rather than a live response — it was written, could not be exercised, and was removed. `--restricted` strips the tools that run commands or code: this needs text in and text out, and the subprocess has no business touching the repository it is being asked about.

The CLI has no schema enforcement, so the expected shape is stated in the prompt and the reply is parsed with a **string-aware brace scanner** — a `{` inside a quoted value is common in answers that quote code, and naive brace counting mis-terminates on it. On a parse failure the model is asked once more with the error appended.

Anchors are still verified locally. A model can return an anchor string that matches no hunk, and an unverified one would silently corrupt coverage accounting and the §9 re-quiz path, so an unknown anchor is remapped onto a real hunk rather than trusted.

## 8. `judge`

**Grade with a different model than the one that generated** — opus generates, sonnet judges. The generator writes the question, the reference *and* — under a single-model design — the grade. A wrong premise then sails through all three unchallenged, and dispute mode is the only escape, which requires you to spot it yourself. Different models do not eliminate correlated error (same family, overlapping training) but they decorrelate it materially, and the cost is zero: generate with the stronger model, judge with the cheaper one. Record `judge_model` on every attempt so a calibration shift is visible later rather than inferred.

This is a mitigation, not a fix. The real defence is `checkable` questions, whose answers come from running code.

**The reference answer is not sent to the judge.** v1 supplied it while instructing the model to grade against the code because the reference "may be wrong". That does not work — the model regresses toward reference-similarity regardless of the instruction, and the only reliable fix is removing the anchor from the context. The stored reference is used for the post-pass reveal and hint tier 2, nothing else.

**Discrete labels, not a continuous score.** The judge returns its own answer first, so reasoning precedes grade:

```json
{"own_answer": "…", "justification": "…", "label": "partial", "feedback": "…"}
```

`wrong` → 0.0, `restates` → 0.3, `partial` → 0.6, `demonstrates` → 0.9.

**Refuse a restatement.** The judge is shown the PR title and commit subjects under the heading *the reviewer can already read all of this*, and scores `wrong` when an answer merely echoes them. Without this the intent question collapses back into paraphrase, since the reviewer is handed the same text the generator was.

**Deterministic precheck, before any API call.** If the answer contains no literal token from the diff — identifier, line number, file name — label it `wrong` locally at zero cost. v1's rubric published its own answer key ("reward consequences, invariants, failure modes, facts not literally present"), and one sentence naming a rollback gap, an unvalidated input and a concurrency invariant scores well on a large fraction of diffs without reading any code. The judge prompt also carries the explicit negative: *score `wrong` if the answer would be equally true of an arbitrary code change.*

**Borderline re-judge.** At `partial` — the label that decides a pass under drop-lowest — re-judge twice and take the median label. An earlier draft said "at temperature 0"; there is no temperature control through the CLI, so the re-runs are plain repeats and the median is taken over the model's natural variance. A failed re-judge keeps the first label rather than failing the answer.

**Pass rule: drop the lowest `n / 3`, not min.** The rest at `partial` or better, and every dropped one no worse than `restates` — forgiven is not ignored, so a `wrong` answer blocks at any count.

v1 required every question ≥ 0.7. If per-question judging misfires on a good answer 10% of the time, that blocks a legitimate PR 27% of the time at three questions and 41% at five; a tolerable 10% gate-level false-block rate under min would need a 2% per-question false-fail rate, which no free-text judge delivers. Dropping the lowest keeps the intent — you cannot ace two and whiff the one that matters — at roughly 92% under the same noise at five questions.

**The share forgiven is fixed, not the count.** §7 made the question count a function of the diff, and a fixed drop-one rule would have made the gate strictness a function of it too: one of three forgiven is two thirds required, one of six is five sixths. Under the same 10% noise that is a 2.8% false-block rate at three questions and 11.4% at six — the wider band would have silently punished the larger PRs it exists to cover. `n / 3` keeps the requirement at two thirds wherever the band lands (1.6% at six, where the extra evidence earns the extra latitude), and leaves three, four and five questions forgiving exactly one as before.

**Dispute mode.** You claim the question's premise, or the code itself, is wrong.

```json
{"upheld": true, "kind": "premise" | "reference" | "code_bug", "feedback": "…"}
```

Guards, because v1 made dispute-everything free:

- at most one dispute per question, two per gate
- the body must cite a file and line present in the diff, checked locally before the call
- a high bar, stated in the prompt rather than through sampling parameters: upheld only if the judge can state the concrete failing input or quote the contradicted line
- **`code_bug` does not auto-pass.** The question goes to `deferred`, the gate can still clear, and an `obligations` row opens — settled by fixing the bug or withdrawing the claim before the next gate on that repo clears
- `premise` / `reference` upheld drops the question and regenerates one replacement for the same anchor
- not upheld is recorded as a scored attempt with the judge's feedback

An upheld rate above 40% means a bad generator or a gamed judge rather than a sharp reviewer. `shipgate stats` shows it.

## 9. Re-quiz on new commits

Review feedback arrives, you push more commits, sometimes after a rebase.

- Recompute hunks and anchors for the new `head_sha`.
- A question whose `anchor` still exists in the new diff keeps its status.
- A question whose anchor is gone goes back to `open`; new AI-authored hunks get new questions.
- **If the PR's `base_sha` changed** — rebase onto a moved `develop`, or a retargeted PR — do not attempt to match. Regenerate the gate. Pretending the old anchors mean anything across a base change produces confident nonsense.

## 10. Output: what clearing leaves behind

**shipgate does not touch the pull request.** It does not write the description, does not mark the PR ready, does not comment. `shipgate ready` clears the gate; flipping the PR is yours.

Earlier versions assembled a description from your answers, between `<!-- shipgate:begin -->` and `<!-- shipgate:end -->`. That is gone. The answers are replies to questions the reader never sees, so they shipped as a transcript — "No, the validation ran in the transaction…" under a heading, answering nothing on the page — sitting next to the description the author had already written. A quiz that produces good thinking does not thereby produce good prose, and pasting one into the other made both worse.

On clear, print to the terminal what only shipgate knows:

```
Cleared · 5 questions · 6/31 AI hunks · trailers

Open on this change:
  app/models/picqer_integration.rb — the validation runs before anything persists

#412 is yours to mark ready · https://github.com/o/r/pull/412
```

The coverage line is printed rather than published: it describes the quiz, not the change, and a reviewer who wants it can ask `shipgate status`.

Upheld `code_bug` findings stay an open obligation in the database and are listed by `status`. Nothing about them is written to the PR — §8's teeth are that `status` keeps naming them, not that a reviewer sees them.

## 11. `status` and `stats`

`status` is the soft teeth:

```
$ shipgate status
owner/repo#41   ready 2d ago   no gate
owner/repo#44   cleared        5 questions · 6/31 AI hunks · no checkable
owner/repo#44   obligation     "drop_path frees twice on early return"
```

It lists PRs that went ready without a gate — the GitHub web UI's "Ready for review" button bypasses `shipgate ready` in one click, leaving no trace — plus coverage and open obligations. Not blocking. Visible skipping beats a debt table nobody opens.

`stats` is the measurement loop. Score data is worthless unless something aggregates it:

```
$ shipgate stats --since 90d
kind            asked  passed  first-try  hints
checkable          22     21       82%      0.4
prediction         31     19       48%      1.7
adversarial        24     20       71%      0.9
cross_cutting      18      9       41%      2.1
shadowed           11      4       36%      1.4
justification      14     13       88%      0.3

disputes: 11 raised, 3 upheld (27%)
```

The point is the low rows. `prediction`, `cross_cutting` and `shadowed` failing consistently is not a scoring artefact — it says which *category* of AI work you routinely accept without understanding: consequences at the call site, effects on code outside the diff, and behaviour that a new branch quietly stopped reaching. That is the finding the whole tool exists to produce, and it only appears in aggregate.

Group by `kind` and by month to see movement. `--by-repo` to see whether it is one codebase or all of them.

## 12. TUI

One screen, two panes. `?` for keys.

- **Left: the diff**, scrollable, syntax-highlighted, jumped to the question's anchor, with AI-authored hunks marked. Not a later addition — an open-book quiz without the book is a memory test on code you wrote last week.
- **Right: the question**, hints revealed so far, and your answer.

Keys: `e` opens `$EDITOR` on the answer buffer, `h` next hint, `d` dispute, `Enter` submit, `j`/`k` scroll the diff.

**Any question, any order.** `tab`/`shift-tab` walk the questions and `1`-`9` jump straight to one; `space` still takes the next one that needs you. The generator's order is not a reading order — the checkable question is often only answerable once the intent question has been thought through, and the reverse as often. Each question keeps its own draft, so wandering off to read another costs nothing. Passed and in-flight questions are reachable too: re-reading what you said is not an error to be prevented.

After submit: label, feedback, and the reference revealed on `demonstrates` or after a second failed attempt. **A short answer must be able to score `demonstrates`** — one sentence naming the consequence plus a line reference is complete, and the rubric says so.

**No blocking calls on the UI thread.** Each request runs on a `std::thread::spawn` writing into an `mpsc::Sender<AppEvent>`; the event loop selects over `crossterm::event::poll(16ms)` and `rx.try_recv()`. `reqwest::blocking` stays inside `llm/`. Without this the terminal freezes 5–30s per judge call with no redraw and no Ctrl-C.

**Grading runs behind you.** Submitting moves straight to the next question and leaves the judge call outstanding; several can be in flight at once. Waiting on each verdict costs 5–30s per question of doing nothing, which at three questions is most of the time the quiz takes. Consequences:

- Answer, hints and verdict are per question (`Slot`), not one set of values on the app. The answer as submitted is held with the pending call, so the attempt is recorded against the text that was actually graded even after you have revised it.
- The question with the judge is frozen: no re-submit, no revision. A verdict shown against text no longer on screen is worse than waiting.
- A verdict landing on a question you have moved past updates its score and says so in the status line rather than hijacking the screen.
- The quiz cannot end while a call is outstanding — its score decides whether the gate clears — so `q` with one in flight warns once and takes a second press.

## 13. Cost

**Measured, not estimated — but read the unit carefully.** Model calls go through the Claude Code CLI (`claude -p`), which uses its own credentials.

The figures below come from the CLI's `total_cost_usd`, which reports `costBasis: "list"`: the **published API list price of the tokens consumed**. What that means for you depends on how Claude Code is authenticated:

- **API key** (`ANTHROPIC_API_KEY`) — this is money billed, directly.
- **OAuth / subscription** (an `oauthAccount` in `~/.claude.json`, no key set) — nothing is billed per call. The usage draws against plan limits, and the dollar figure is a *proxy for token consumption*, not an invoice.

Either way the number is the right relative signal — it is proportional to tokens, so the comparisons below hold — but do not quote it as a bill without checking which case applies.

Measured against this project's own PR, ~22 AI-authored hunks, four questions:

| | cost |
|---|---|
| `generate` on opus, cold / warm | $1.13 / $0.80 |
| `generate` on sonnet | **$0.52** |
| `judge` on haiku, per answered question | ~$0.05 |
| answers killed by the §8 precheck | $0.00 |

The defaults are **sonnet to generate, haiku to judge** — roughly half what opus-and-sonnet cost, with question quality holding up in side-by-side runs. Set them in `~/.config/shipgate/config.toml`:

```toml
[models]
generate = "sonnet"
judge = "haiku"
```

Whatever they are set to, the two must differ: §8 rests on the grader not being the model that wrote the question and the reference.

An earlier draft of this section estimated $0.03–0.10 per PR from token counts alone. That was wrong by more than an order of magnitude, for two reasons it did not account for:

**A stored gate replays; it does not regenerate.** `shipgate ready` on a PR that has been quizzed before re-runs the stored questions against the stored diff and calls the generator not at all. Re-running to look at the quiz again is the common case, and at $0.52 a generation, charging for it would make reopening a gate the most expensive thing in the tool. `--force` throws the gate away — questions and graded answers both — and generates a new one. The cost of the default is that new commits are not covered until you ask: the replay warns when HEAD has moved past the stored gate.

Remaining levers, in order of value: ask for fewer questions; keep the precheck, which is free and killed five of six answers in the run above; and remember the bill scales with **PR count**, not repo size — a busy backlog is where this bites, not a large codebase.

Against the time a PR takes to review, this is small either way — but it is not free, and it scales with PR count, not repo size. The comparison that matters is proportional, so it survives the billing question: v1's Stop hook fired at every Claude Code turn end, so 10–25 gates a day would consume **10–25× what the PR-ready gate does**. On an API key that is $15–35/day; on a subscription it is the difference between a gate you barely notice and one that exhausts your limits by lunchtime. Either way it settles the placement question independently of the ergonomics argument.

## 14. Build order

1. `db` (migrations + pragmas) + `git` + `gh` + `gate`, with base resolution (§5) and **authorship scoping (§3)**. `shipgate ready` end to end with hardcoded questions and a stub judge passing any answer over 40 characters. Prove the `gh pr ready` flip and the description write.
2. `triage` + `context` + `llm::generate` with the skip path and the checkable preference.
3. `llm::judge`: precheck, discrete labels, no reference in context, separate judge model. Pass rule.
4. TUI: diff pane with AI hunks marked, answer pane, `$EDITOR` escape. First version worth daily use.
5. Description assembly with coverage line, `$EDITOR` review before submit.
6. Hints, dispute mode, obligations, bug notes.
7. `shipgate status`, `shipgate stats`, re-quiz on new commits via anchors.

Steps 1–5 are the product. Steps 6–7 are worth having and safe to stop before.

Three things must be in step 1 or they force a rewrite: migrations (shipping without them means hand-editing the database to add a column), content-hash anchors (retrofitting changes `questions` and the re-quiz logic together), and authorship scoping (it determines what a gate even covers, and `hunks_ai` / `hunks_covered` are on the gate row).
