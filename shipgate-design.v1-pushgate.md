# shipgate — design sketch

A push gate that makes you demonstrate understanding of AI-written code before it leaves your machine. Rust, Ratatui, SQLite, plain git hooks.

## 1. Crate layout

Single binary, subcommands. Shelling out to `git` is simpler and more robust than `git2` for the handful of operations needed.

```
shipgate/
├── Cargo.toml
├── src/
│   ├── main.rs            # clap: enqueue | generate | check | push | tui | debt | install-hooks
│   ├── db.rs              # rusqlite (bundled), migrations, typed queries
│   ├── git.rs             # repo root, branch, head, upstream, merge-base, diff, push
│   ├── gate.rs            # state machine: pending → open → cleared | debt
│   ├── llm/
│   │   ├── mod.rs         # Anthropic Messages client (reqwest, blocking is fine)
│   │   ├── generate.rs    # diff → questions + hints + reference answers
│   │   └── judge.rs       # (diff, question, answer, reference) → verdict; also dispute path
│   ├── hooks/
│   │   ├── stop.rs        # parse Claude Code Stop hook stdin, enqueue, spawn generate
│   │   └── pre_push.rs    # parse git pre-push stdin, check gate, exit code
│   └── tui/
│       ├── mod.rs         # app loop, key handling
│       ├── inbox.rs       # all open gates across worktrees, badge counts
│       ├── quiz.rs        # one question at a time: answer / hint / dispute
│       ├── debt.rs        # emergency ships, oldest first
│       └── review.rs      # spaced-repetition queue for "shaky" answers
└── hooks/
    ├── pre-push           # 3 lines, installed by `shipgate install-hooks`
    └── claude-stop.sh     # 1 line, referenced from .claude/settings.json
```

Crates: `clap`, `ratatui`, `crossterm`, `rusqlite` (feature `bundled`), `serde`, `serde_json`, `reqwest` (blocking + json), `anyhow`, `chrono`, `directories` (for the data dir).

## 2. Storage

One file: `~/.local/share/shipgate/shipgate.db`. Every worktree writes to the same DB, so the TUI sees everything.

```sql
CREATE TABLE gates (
  id            INTEGER PRIMARY KEY,
  repo          TEXT NOT NULL,          -- canonical path of the *main* repo (git rev-parse --git-common-dir)
  worktree      TEXT NOT NULL,          -- path of the worktree that enqueued it
  branch        TEXT NOT NULL,
  base_sha      TEXT NOT NULL,          -- merge-base with upstream/main at enqueue time
  head_sha      TEXT NOT NULL,
  diff          TEXT NOT NULL,          -- snapshot; hunks re-derived from this
  state         TEXT NOT NULL,          -- pending | open | cleared | debt
  push_on_clear INTEGER NOT NULL DEFAULT 0,
  created_at    TEXT NOT NULL,
  cleared_at    TEXT,
  UNIQUE(repo, branch, head_sha)
);

CREATE TABLE questions (
  id          INTEGER PRIMARY KEY,
  gate_id     INTEGER NOT NULL REFERENCES gates(id),
  kind        TEXT NOT NULL,            -- prediction | cross_cutting | adversarial | justification | checkable
  file        TEXT NOT NULL,
  hunk_header TEXT NOT NULL,            -- "@@ -88,6 +88,9 @@" — used to reopen only affected questions
  text        TEXT NOT NULL,
  reference   TEXT NOT NULL,            -- model answer, treated as *possibly wrong*
  hints       TEXT NOT NULL,            -- JSON array, tier 0..2
  status      TEXT NOT NULL,            -- open | passed | shaky | disputed_upheld | disputed_rejected
  score       REAL
);

CREATE TABLE attempts (
  id          INTEGER PRIMARY KEY,
  question_id INTEGER NOT NULL REFERENCES questions(id),
  mode        TEXT NOT NULL,            -- answer | dispute
  body        TEXT NOT NULL,
  hints_used  INTEGER NOT NULL DEFAULT 0,
  score       REAL,
  feedback    TEXT,
  created_at  TEXT NOT NULL
);

CREATE TABLE debts (
  id         INTEGER PRIMARY KEY,
  gate_id    INTEGER NOT NULL REFERENCES gates(id),
  reason     TEXT NOT NULL,
  pushed_sha TEXT NOT NULL,
  created_at TEXT NOT NULL,
  settled_at TEXT                       -- set when the gate is later cleared
);

CREATE TABLE review_queue (             -- spaced repetition for shaky/hinted answers
  question_id INTEGER PRIMARY KEY REFERENCES questions(id),
  due_at      TEXT NOT NULL,
  interval_d  INTEGER NOT NULL DEFAULT 3
);
```

## 3. Gate state machine

```
                 stop hook / enqueue
  (none) ─────────────────────────────▶ pending
                                            │ generate worker
                                            ▼
              new commits on branch      open ◀──────────────┐
  cleared ◀── reopen affected qs ────┐      │                 │
     ▲                               │      │ all qs ≥ 0.7    │ new commits touch
     │                               │      │ or dispute      │ a question's hunk
     │                               └──────┤ upheld          │
     │                                      ▼                 │
     └────────────────────────────────── cleared ─────────────┘
                                            
  any state ── `push --emergency` ──▶ debt ── later cleared ──▶ settled
```

Pass rule is **min**, not mean: every question ≥ 0.7. A question with hints used still counts as passed but is marked `shaky` and inserted into `review_queue`.

Reopening: on a new `head_sha`, diff old snapshot vs new; questions whose `hunk_header` overlaps a changed region go back to `open`, the rest keep their status. New hunks get new questions.

## 4. Claude Code Stop hook

`.claude/settings.json` (project or user level):

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "shipgate enqueue --from-stop-hook", "timeout": 10 }
        ]
      }
    ]
  }
}
```

Claude Code writes JSON to stdin. The fields we use:

```json
{
  "session_id": "…",
  "transcript_path": "/home/you/.claude/projects/…/….jsonl",
  "cwd": "/home/you/work/repo-wt-feature-x",
  "hook_event_name": "Stop",
  "stop_hook_active": false
}
```

`enqueue --from-stop-hook`:

1. Guard: if `stop_hook_active` is true, exit 0 (avoid loops).
2. `cd cwd`; if not a git repo or no diff vs merge-base, exit 0.
3. Resolve `repo`, `branch`, `base_sha = git merge-base HEAD @{upstream}` (fall back to `origin/main`), `head_sha`, `diff = git diff base_sha..HEAD` (**committed** changes only; uncommitted work isn't ready to be quizzed).
4. Upsert gate as `pending`. If a gate exists for the same `(repo, branch)` with a different `head_sha`, run the reopen logic.
5. Spawn `shipgate generate --gate <id>` detached (`setsid`/`nohup`) and exit 0 immediately. Never block Claude Code.

**Deliberately not passed to the generator:** `transcript_path`. The generator sees only the diff, so it doesn't inherit the coding session's reasoning or mistakes. Keep the path in the gate row anyway; it's useful for the "bug found" note later.

## 5. `generate` worker

Prompt shape (system):

> You are writing review questions for a code change. You have only the diff. Produce 3–6 questions that cannot be answered by paraphrasing the code. Prefer, in order: **checkable** (has an answer the reviewer can obtain by running something), **prediction** ("if X were Y, what does the caller observe"), **adversarial** ("what input breaks this"), **cross_cutting** ("what elsewhere assumes this"), **justification** ("why this over the obvious alternative"). Avoid "what does this function do". For each, give a reference answer and three hints of increasing strength: a nudge, a pointer to specific lines, half the answer. Return JSON only.

Output schema:

```json
[
  {
    "kind": "prediction",
    "file": "src/sync.rs",
    "hunk_header": "@@ -88,6 +88,9 @@",
    "text": "If retry_count were 0, what does the caller at sync.rs:88 observe on a transient failure?",
    "reference": "…",
    "hints": ["Look at how the loop exits.", "Lines 91–94: the Err arm.", "It returns Err(Transient) on the first failure without …"]
  }
]
```

Strip ```` ```json ```` fences before parsing; on parse failure, retry once with the error appended. Flip gate to `open` on success.

## 6. `judge`

Two modes, one prompt family. Input always includes the full diff.

**Answer mode:**

> Grade the reviewer's answer against the **code**, not against the reference. The reference was written by a model and may be wrong; use it only as a hint. Penalise answers that restate what the code does. Reward answers that name consequences, invariants, failure modes, or facts not literally present in the diff. Return `{"score": 0..1, "feedback": "…"}`.

**Dispute mode:**

> The reviewer claims the question's premise, the reference answer, or the code itself is wrong. Evaluate the claim on its merits against the diff. Return `{"upheld": bool, "kind": "premise"|"reference"|"code_bug", "feedback": "…"}`.

Effects:

- `upheld` + `kind = code_bug` → question `disputed_upheld`, gate gains a `bug_found` note (written to `~/.local/share/shipgate/notes/<gate>.md` with the transcript path so you can hand it back to the coding session). Counts as a pass for that question.
- `upheld` + `premise`/`reference` → question dropped, regenerate one replacement for the same hunk.
- not upheld → treated as a scored attempt with the judge's feedback.

## 7. pre-push hook

`hooks/pre-push`:

```sh
#!/bin/sh
exec shipgate check --pre-push "$@"
```

Git feeds lines on stdin: `<local_ref> <local_sha> <remote_ref> <remote_sha>`. `check --pre-push`:

- For each line, look up gate by `(repo, branch from local_ref, head_sha = local_sha)`.
- No gate at all → **allow** (branch never went through Claude Code; not our business). Optional strict mode to block instead.
- `cleared` → allow.
- `pending`/`open` → print the open question count and `shipgate tui` hint, exit 1.
- `SHIPGATE_EMERGENCY` env set → allow and record debt (this is what `shipgate push --emergency` sets before calling `git push`).

Debt pressure, opt-in: if any debt older than `N` days is unsettled, block non-emergency pushes with a message listing it.

## 8. `push`

```
shipgate push                       # git push; if gate open → exit 1 with hint
shipgate push --when-cleared        # sets push_on_clear=1; TUI runs `git push` from the worktree on pass
shipgate push --emergency "reason"  # records debt, sets env, git push
```

`--when-cleared` runs the push from inside the TUI process when the last question passes, using the stored `worktree` path. Print the git output in a modal so a rejected push isn't silent.

## 9. TUI

Four tabs, `Tab` to cycle, `?` for keys.

- **Inbox** — one row per open gate: `repo · branch · 3/5 open · 2h ago`. `Enter` opens quiz.
- **Quiz** — left: question + hints revealed so far; right: textarea. Keys: `a` answer, `h` next hint, `d` dispute, `s` skip (next question), `o` open diff in `$PAGER` (diff pane in-TUI is a later addition). After submit: score bar, feedback, reference revealed only if score ≥ 0.7 or after the second failed attempt.
- **Debt** — oldest first, reason, age, `Enter` to start quizzing that gate now.
- **Review** — due `shaky` questions; passing without hints removes them, failing doubles nothing and re-queues at the same interval.

## 10. Build order

1. `db` + `git` + `gate` with `enqueue`/`check` and the two hooks. No LLM yet; questions hardcoded. Get the pre-push block/allow loop working end to end.
2. `llm::generate` + detached worker.
3. Minimal TUI: inbox + quiz with answer only. `llm::judge` answer mode.
4. Hints, dispute mode, bug-found note.
5. Emergency push, debt tab, debt pressure.
6. Reopen-on-new-commits by hunk overlap.
7. Review queue, `--when-cleared`, in-TUI diff pane.

Steps 1–3 are the useful core; everything after is polish you can stop at any point.
