//! SQLite storage. Pragmas on every open; migrations from the first commit,
//! because shipping without them means hand-editing the database later.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

pub fn data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "shipgate")
        .context("cannot resolve a data directory")?;
    let dir = dirs.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn notes_dir() -> Result<PathBuf> {
    let d = data_dir()?.join("notes");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

pub fn open() -> Result<Connection> {
    open_at(&data_dir()?.join("shipgate.db"))
}

pub fn open_at(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // WAL so a reader never blocks the writer. busy_timeout covers the rest.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

const MIGRATIONS: &[&str] = &[
    // 1
    r#"
    CREATE TABLE gates (
      id              INTEGER PRIMARY KEY,
      repo            TEXT NOT NULL,
      pr_number       INTEGER NOT NULL,
      branch          TEXT NOT NULL,
      base_ref        TEXT NOT NULL,
      base_sha        TEXT NOT NULL,
      head_sha        TEXT NOT NULL,
      diff            TEXT NOT NULL,
      hunks_total     INTEGER NOT NULL,
      hunks_ai        INTEGER NOT NULL,
      hunks_covered   INTEGER NOT NULL,
      authorship      TEXT NOT NULL,
      has_checkable   INTEGER NOT NULL DEFAULT 0,
      state           TEXT NOT NULL,
      last_error      TEXT,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL,
      cleared_at      TEXT,
      UNIQUE(repo, pr_number)
    );

    CREATE TABLE questions (
      id          INTEGER PRIMARY KEY,
      gate_id     INTEGER NOT NULL REFERENCES gates(id) ON DELETE CASCADE,
      kind        TEXT NOT NULL,
      file        TEXT NOT NULL,
      anchor      TEXT NOT NULL,
      text        TEXT NOT NULL,
      reference   TEXT NOT NULL,
      hints       TEXT NOT NULL CHECK (json_valid(hints)),
      status      TEXT NOT NULL,
      label       TEXT,
      score       REAL,
      created_at  TEXT NOT NULL
    );

    CREATE TABLE attempts (
      id          INTEGER PRIMARY KEY,
      question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
      mode        TEXT NOT NULL,
      body        TEXT NOT NULL,
      hints_used  INTEGER NOT NULL DEFAULT 0,
      label       TEXT,
      score       REAL,
      feedback    TEXT,
      judge_model TEXT,
      created_at  TEXT NOT NULL
    );

    CREATE TABLE obligations (
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
    "#,
    // 2 — the dashboard has no working directory to infer from, so a gate has
    // to carry the path it was created in.
    r#"
    ALTER TABLE gates ADD COLUMN path TEXT NOT NULL DEFAULT '';
    "#,
    // 3 — a superseded gate is archived, not deleted, so the answers you were
    // graded on survive the regeneration that replaces the questions. That
    // means more than one row per PR, so the table's UNIQUE(repo, pr_number)
    // has to go; SQLite cannot drop a table constraint, hence the rebuild. A
    // partial unique index keeps the invariant that matters — one *live* gate
    // per PR — without constraining the archive.
    r#"
    CREATE TABLE gates_new (
      id              INTEGER PRIMARY KEY,
      repo            TEXT NOT NULL,
      pr_number       INTEGER NOT NULL,
      branch          TEXT NOT NULL,
      base_ref        TEXT NOT NULL,
      base_sha        TEXT NOT NULL,
      head_sha        TEXT NOT NULL,
      diff            TEXT NOT NULL,
      hunks_total     INTEGER NOT NULL,
      hunks_ai        INTEGER NOT NULL,
      hunks_covered   INTEGER NOT NULL,
      authorship      TEXT NOT NULL,
      has_checkable   INTEGER NOT NULL DEFAULT 0,
      state           TEXT NOT NULL,
      last_error      TEXT,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL,
      cleared_at      TEXT,
      path            TEXT NOT NULL DEFAULT '',
      superseded_at   TEXT
    );

    INSERT INTO gates_new
      (id, repo, pr_number, branch, base_ref, base_sha, head_sha, diff,
       hunks_total, hunks_ai, hunks_covered, authorship, has_checkable, state,
       last_error, created_at, updated_at, cleared_at, path, superseded_at)
    SELECT
       id, repo, pr_number, branch, base_ref, base_sha, head_sha, diff,
       hunks_total, hunks_ai, hunks_covered, authorship, has_checkable, state,
       last_error, created_at, updated_at, cleared_at, path, NULL
    FROM gates;

    DROP TABLE gates;
    ALTER TABLE gates_new RENAME TO gates;

    CREATE INDEX idx_gates_state ON gates(state);
    CREATE UNIQUE INDEX idx_gates_live
      ON gates(repo, pr_number) WHERE superseded_at IS NULL;
    CREATE INDEX idx_gates_pr ON gates(repo, pr_number);
    "#,
    // 4 — §7's exercise kind. A gate that could not have one because the
    // repository offered no way to run the program is a different fact from a
    // gate that skipped it, and `stats` reports the share of work that shipped
    // with nothing but the suite watching it. Neither is recoverable from the
    // questions alone, so the gate records it.
    r#"
    ALTER TABLE gates ADD COLUMN has_exercise INTEGER NOT NULL DEFAULT 0;
    "#,
];

fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version <= current {
            continue;
        }
        // Foreign keys off for the duration, and off *outside* the transaction
        // — the pragma is a no-op inside one. A migration that rebuilds a
        // parent table drops it while children still reference it, and with
        // enforcement on, ON DELETE CASCADE would take every question and
        // attempt in the database with it. Off, the children keep their ids
        // and the rename puts the parent back underneath them.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let res = conn
            .execute_batch(sql)
            .and_then(|()| conn.execute_batch(&format!("PRAGMA user_version = {version};")));
        let res = match res {
            // Enforcement is off, so nothing checked the rows this migration
            // just moved. Check them before committing rather than finding out
            // at the next join.
            Ok(()) => orphan_check(conn),
            Err(e) => Err(e.into()),
        };
        match res {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                conn.execute_batch("PRAGMA foreign_keys = ON;")?;
                return Err(e).context(format!("migration {version} failed"));
            }
        }
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    }
    Ok(())
}

/// `PRAGMA foreign_key_check` as a hard error. Reports the first offending
/// table, which is enough to name the migration that broke it.
fn orphan_check(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    if let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        anyhow::bail!("left orphaned rows in {table}");
    }
    Ok(())
}

// ---------------------------------------------------------------- rows

#[derive(Debug, Clone)]
pub struct Gate {
    pub id: i64,
    pub repo: String,
    /// Working tree this gate was created in. The dashboard runs git and gh
    /// there; without it a gate is unreachable from outside its own repo.
    pub path: String,
    /// The diff snapshot taken when the gate was created. A replay works from
    /// this rather than re-deriving it, so the questions still line up with the
    /// text they were written against. Empty on an archived gate, which is
    /// never replayed.
    pub diff: String,
    pub pr_number: u64,
    pub branch: String,
    pub base_ref: String,
    pub base_sha: String,
    pub head_sha: String,
    pub hunks_total: i64,
    pub hunks_ai: i64,
    pub hunks_covered: i64,
    pub authorship: String,
    pub has_checkable: bool,
    pub has_exercise: bool,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: i64,
    pub kind: String,
    pub file: String,
    pub anchor: String,
    pub text: String,
    pub reference: String,
    pub hints: Vec<String>,
    pub status: String,
    pub label: Option<String>,
    pub score: Option<f64>,
}

#[derive(Clone)]
pub struct NewGate<'a> {
    pub repo: &'a str,
    pub path: &'a str,
    pub pr_number: u64,
    pub branch: &'a str,
    pub base_ref: &'a str,
    pub base_sha: &'a str,
    pub head_sha: &'a str,
    pub diff: &'a str,
    pub hunks_total: i64,
    pub hunks_ai: i64,
    pub authorship: &'a str,
    pub state: &'a str,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// One *live* gate per PR. A re-run archives the previous one — §9 regenerates
/// rather than pretending old anchors survive a base change, but the answers
/// you were graded on under the old questions are work you already paid for,
/// and deleting them makes "am I getting better at this" unanswerable.
pub fn upsert_gate(conn: &Connection, g: &NewGate) -> Result<i64> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<i64> {
        archive_live_gate(conn, g.repo, g.pr_number)?;
        conn.execute(
            "INSERT INTO gates
               (repo, path, pr_number, branch, base_ref, base_sha, head_sha, diff,
                hunks_total, hunks_ai, hunks_covered, authorship, state,
                created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,?11,?12,?13,?13)",
            params![
                g.repo,
                g.path,
                g.pr_number as i64,
                g.branch,
                g.base_ref,
                g.base_sha,
                g.head_sha,
                g.diff,
                g.hunks_total,
                g.hunks_ai,
                g.authorship,
                g.state,
                now(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })();
    match res {
        Ok(id) => {
            conn.execute_batch("COMMIT;")?;
            Ok(id)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

pub fn set_gate_state(conn: &Connection, gate_id: i64, state: &str) -> Result<()> {
    let cleared = if state == "cleared" || state == "trivial" {
        Some(now())
    } else {
        None
    };
    conn.execute(
        "UPDATE gates SET state = ?1, updated_at = ?2, cleared_at = COALESCE(?3, cleared_at)
         WHERE id = ?4",
        params![state, now(), cleared, gate_id],
    )?;
    Ok(())
}

pub fn set_gate_coverage(
    conn: &Connection,
    gate_id: i64,
    covered: i64,
    has_checkable: bool,
    has_exercise: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE gates SET hunks_covered = ?1, has_checkable = ?2, has_exercise = ?3, \
         updated_at = ?4 WHERE id = ?5",
        params![covered, has_checkable as i64, has_exercise as i64, now(), gate_id],
    )?;
    Ok(())
}

pub struct NewQuestion<'a> {
    pub kind: &'a str,
    pub file: &'a str,
    pub anchor: &'a str,
    pub text: &'a str,
    pub reference: &'a str,
    pub hints: &'a [String],
}

pub fn insert_questions(conn: &Connection, gate_id: i64, qs: &[NewQuestion]) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<()> {
        for q in qs {
            conn.execute(
                "INSERT INTO questions
                   (gate_id, kind, file, anchor, text, reference, hints, status, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'open',?8)",
                params![
                    gate_id,
                    q.kind,
                    q.file,
                    q.anchor,
                    q.text,
                    q.reference,
                    serde_json::to_string(q.hints)?,
                    now(),
                ],
            )?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

fn question_from_row(row: &rusqlite::Row) -> rusqlite::Result<Question> {
    let hints_json: String = row.get("hints")?;
    Ok(Question {
        id: row.get("id")?,
        kind: row.get("kind")?,
        file: row.get("file")?,
        anchor: row.get("anchor")?,
        text: row.get("text")?,
        reference: row.get("reference")?,
        hints: serde_json::from_str(&hints_json).unwrap_or_default(),
        status: row.get("status")?,
        label: row.get("label")?,
        score: row.get("score")?,
    })
}

pub fn questions_for(conn: &Connection, gate_id: i64) -> Result<Vec<Question>> {
    let mut stmt = conn.prepare("SELECT * FROM questions WHERE gate_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map(params![gate_id], question_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn record_attempt(
    conn: &Connection,
    question_id: i64,
    mode: &str,
    body: &str,
    hints_used: i64,
    label: &str,
    score: f64,
    feedback: &str,
    judge_model: &str,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<()> {
        conn.execute(
            "INSERT INTO attempts
               (question_id, mode, body, hints_used, label, score, feedback, judge_model, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![question_id, mode, body, hints_used, label, score, feedback, judge_model, now()],
        )?;
        conn.execute(
            "UPDATE questions SET label = ?1, score = ?2,
               status = CASE WHEN ?2 >= 0.6 THEN 'passed' ELSE 'open' END
             WHERE id = ?3",
            params![label, score, question_id],
        )?;
        Ok(())
    })();
    match res {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

pub fn last_answer(conn: &Connection, question_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT body FROM attempts WHERE question_id = ?1 AND mode = 'answer'
             ORDER BY id DESC LIMIT 1",
            params![question_id],
            |r| r.get(0),
        )
        .optional()?)
}

fn gate_from_row(row: &rusqlite::Row) -> rusqlite::Result<Gate> {
    Ok(Gate {
        id: row.get("id")?,
        repo: row.get("repo")?,
        path: row.get("path")?,
        diff: row.get("diff")?,
        pr_number: row.get::<_, i64>("pr_number")? as u64,
        branch: row.get("branch")?,
        base_ref: row.get("base_ref")?,
        base_sha: row.get("base_sha")?,
        head_sha: row.get("head_sha")?,
        hunks_total: row.get("hunks_total")?,
        hunks_ai: row.get("hunks_ai")?,
        hunks_covered: row.get("hunks_covered")?,
        authorship: row.get("authorship")?,
        has_checkable: row.get::<_, i64>("has_checkable")? != 0,
        has_exercise: row.get::<_, i64>("has_exercise")? != 0,
        state: row.get("state")?,
    })
}

/// The live gate for a PR. Archived ones are history, never replayed and never
/// quizzed.
pub fn gate_for_pr(conn: &Connection, repo: &str, pr: u64) -> Result<Option<Gate>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM gates
         WHERE repo = ?1 AND pr_number = ?2 AND superseded_at IS NULL",
    )?;
    let mut rows = stmt.query_map(params![repo, pr as i64], gate_from_row)?;
    Ok(rows.next().transpose()?)
}

pub fn gates_for_repo(conn: &Connection, repo: &str) -> Result<Vec<Gate>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM gates WHERE repo = ?1 AND superseded_at IS NULL
         ORDER BY pr_number DESC",
    )?;
    let rows = stmt.query_map(params![repo], gate_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Archive the live gate for a PR, if there is one. The diff snapshot goes:
/// it exists only so a gate can be replayed, an archived gate never is, and it
/// is far the largest column — keeping every diff of every regeneration is how
/// an archive turns into a reason to delete the archive.
fn archive_live_gate(conn: &Connection, repo: &str, pr: u64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE gates SET superseded_at = ?1, updated_at = ?1, diff = ''
         WHERE repo = ?2 AND pr_number = ?3 AND superseded_at IS NULL",
        params![now(), repo, pr as i64],
    )?)
}

/// How much graded work a gate is holding. Regenerating archives it rather than
/// deleting it, so this is what you stop being asked about, not what is lost.
pub fn attempt_count(conn: &Connection, gate_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM attempts a
         JOIN questions q ON q.id = a.question_id
         WHERE q.gate_id = ?1",
        params![gate_id],
        |r| r.get(0),
    )?)
}

/// Every live gate, newest first, for the dashboard.
pub fn all_gates(conn: &Connection) -> Result<Vec<Gate>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM gates WHERE superseded_at IS NULL ORDER BY updated_at DESC",
    )?;
    let rows = stmt.query_map([], gate_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Take gates whose PR is no longer open off the dashboard. Without this it
/// only ever grows, which is what turned v1's debt tab into a list nobody
/// opened.
///
/// Archived, not deleted. A merged PR is where most of the answering happened,
/// so deleting those gates would throw away the bulk of the record on the day
/// it became history.
pub fn prune_closed(conn: &Connection, repo: &str, open_prs: &[u64]) -> Result<usize> {
    let gates = gates_for_repo(conn, repo)?;
    let mut removed = 0;
    for g in gates {
        if !open_prs.contains(&g.pr_number) {
            archive_live_gate(conn, repo, g.pr_number)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// §8 — a dispute, upheld or not, recorded against the question it was about.
///
/// Written as an attempt with `mode = 'dispute'` and no score, and it does not
/// touch the question's label: a rejected dispute leaves the verdict you were
/// arguing with exactly where it was, and an upheld one changes the question's
/// *status*, not its grade.
pub fn record_dispute(
    conn: &Connection,
    question_id: i64,
    body: &str,
    upheld: bool,
    kind: &str,
    feedback: &str,
    judge_model: &str,
) -> Result<()> {
    let label = if upheld { kind } else { "rejected" };
    conn.execute(
        "INSERT INTO attempts
           (question_id, mode, body, hints_used, label, score, feedback, judge_model, created_at)
         VALUES (?1,'dispute',?2,0,?3,NULL,?4,?5,?6)",
        params![question_id, body, label, feedback, judge_model, now()],
    )?;
    Ok(())
}

/// How many disputes a question has already had, and how many its gate has.
/// §8 caps both — one per question, two per gate — because v1 made disputing
/// free and it became the way past any question worth thinking about.
pub fn dispute_counts(conn: &Connection, question_id: i64) -> Result<(i64, i64)> {
    let per_question: i64 = conn.query_row(
        "SELECT count(*) FROM attempts WHERE question_id = ?1 AND mode = 'dispute'",
        params![question_id],
        |r| r.get(0),
    )?;
    let per_gate: i64 = conn.query_row(
        "SELECT count(*) FROM attempts a
         JOIN questions q ON q.id = a.question_id
         WHERE a.mode = 'dispute'
           AND q.gate_id = (SELECT gate_id FROM questions WHERE id = ?1)",
        params![question_id],
        |r| r.get(0),
    )?;
    Ok((per_question, per_gate))
}

/// The claim behind the most recent dispute on a question, for the PR
/// description.
pub fn last_dispute(conn: &Connection, question_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT body FROM attempts WHERE question_id = ?1 AND mode = 'dispute'
             ORDER BY id DESC LIMIT 1",
            params![question_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn set_question_status(conn: &Connection, question_id: i64, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE questions SET status = ?1 WHERE id = ?2",
        params![status, question_id],
    )?;
    Ok(())
}

/// §8 — an upheld `code_bug` does not pass the question and does not block the
/// gate. It opens an obligation, settled by fixing the bug or withdrawing the
/// claim before the next gate on this repo clears.
pub fn open_obligation(conn: &Connection, question_id: i64, body: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO obligations (gate_id, question_id, body, created_at)
         SELECT gate_id, id, ?2, ?3 FROM questions WHERE id = ?1",
        params![question_id, body, now()],
    )?;
    Ok(())
}

pub struct Obligation {
    pub pr_number: u64,
    pub body: String,
}

pub fn open_obligations(conn: &Connection, repo: &str) -> Result<Vec<Obligation>> {
    let mut stmt = conn.prepare(
        "SELECT g.pr_number AS pr_number, o.body AS body
         FROM obligations o JOIN gates g ON g.id = o.gate_id
         WHERE g.repo = ?1 AND o.settled_at IS NULL
         ORDER BY o.created_at",
    )?;
    let rows = stmt.query_map(params![repo], |r| {
        Ok(Obligation {
            pr_number: r.get::<_, i64>("pr_number")? as u64,
            body: r.get("body")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("db-wal"));
            let _ = std::fs::remove_file(self.0.with_extension("db-shm"));
        }
    }

    fn temp_db(name: &str) -> Temp {
        let p = std::env::temp_dir().join(format!("shipgate-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Temp(p)
    }

    #[test]
    fn open_migrates_and_sets_pragmas() {
        let t = temp_db("mig");
        let conn = open_at(&t.0).unwrap();

        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);

        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");

        let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn migrations_are_idempotent() {
        let t = temp_db("idem");
        drop(open_at(&t.0).unwrap());
        let conn = open_at(&t.0).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    fn seed(conn: &Connection) -> i64 {
        upsert_gate(
            conn,
            &NewGate {
                repo: "o/r",
                path: "/tmp/o-r",
                pr_number: 7,
                branch: "feature/x",
                base_ref: "origin/develop",
                base_sha: "aaa",
                head_sha: "bbb",
                diff: "diff",
                hunks_total: 31,
                hunks_ai: 12,
                authorship: "trailers",
                state: "open",
            },
        )
        .unwrap()
    }

    #[test]
    fn re_running_leaves_one_live_gate_per_pr() {
        let t = temp_db("upsert");
        let conn = open_at(&t.0).unwrap();
        seed(&conn);
        seed(&conn);
        let live: i64 = conn
            .query_row(
                "SELECT count(*) FROM gates WHERE repo='o/r' AND superseded_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live, 1, "one live gate per PR");
        let all: i64 = conn
            .query_row("SELECT count(*) FROM gates WHERE repo='o/r'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(all, 2, "the replaced gate should be kept, not deleted");
    }

    /// A second live gate for the same PR is what the partial unique index
    /// exists to refuse — the archive is allowed to hold many, the dashboard
    /// and the replay path assume one.
    #[test]
    fn two_live_gates_for_one_pr_are_refused() {
        let t = temp_db("live-unique");
        let conn = open_at(&t.0).unwrap();
        seed(&conn);
        let direct = conn.execute(
            "INSERT INTO gates
               (repo, path, pr_number, branch, base_ref, base_sha, head_sha, diff,
                hunks_total, hunks_ai, hunks_covered, authorship, state,
                created_at, updated_at)
             VALUES ('o/r','/tmp/o-r',7,'b','origin/main','a','b','d',1,1,0,'trailers','open',
                     '2026-01-01','2026-01-01')",
            [],
        );
        assert!(direct.is_err(), "a second live gate for PR 7 was accepted");
    }

    /// The answers are the expensive half of a run and the whole of the record.
    /// Regenerating replaces the questions; it must not delete what you were
    /// already graded on.
    #[test]
    fn answered_questions_survive_the_gate_being_replaced() {
        let t = temp_db("archive");
        let conn = open_at(&t.0).unwrap();
        let gate = seed(&conn);
        insert_questions(
            &conn,
            gate,
            &[NewQuestion {
                kind: "prediction",
                file: "a.rs",
                anchor: "sha256:x",
                text: "q",
                reference: "r",
                hints: &["h".to_string()],
            }],
        )
        .unwrap();
        let q = questions_for(&conn, gate).unwrap().remove(0);
        record_attempt(&conn, q.id, "answer", "my answer", 0, "demonstrates", 0.9, "ok", "haiku")
            .unwrap();

        let fresh = seed(&conn);
        assert_ne!(fresh, gate, "the replacement should be a new gate row");

        // The new gate starts empty; the old one keeps its question and answer.
        assert!(questions_for(&conn, fresh).unwrap().is_empty());
        assert_eq!(questions_for(&conn, gate).unwrap().len(), 1);
        assert_eq!(attempt_count(&conn, gate).unwrap(), 1);
        assert_eq!(
            last_answer(&conn, q.id).unwrap().as_deref(),
            Some("my answer")
        );
    }

    /// The diff is by far the largest column and exists only so a gate can be
    /// replayed. Keeping one per regeneration is how an archive becomes a
    /// reason to delete the archive.
    #[test]
    fn an_archived_gate_drops_its_diff_snapshot() {
        let t = temp_db("archive-diff");
        let conn = open_at(&t.0).unwrap();
        let gate = seed(&conn);
        seed(&conn);
        let (diff, superseded): (String, Option<String>) = conn
            .query_row(
                "SELECT diff, superseded_at FROM gates WHERE id = ?1",
                params![gate],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(diff, "");
        assert!(superseded.is_some(), "the replaced gate was not marked");
    }

    /// A merged PR is where most of the answering happened. Taking it off the
    /// dashboard must not take it out of the record.
    #[test]
    fn pruning_a_closed_pr_archives_it_rather_than_deleting_it() {
        let t = temp_db("prune");
        let conn = open_at(&t.0).unwrap();
        let gate = seed(&conn);
        assert_eq!(prune_closed(&conn, "o/r", &[]).unwrap(), 1);
        assert!(gate_for_pr(&conn, "o/r", 7).unwrap().is_none(), "still on the dashboard");
        assert!(all_gates(&conn).unwrap().is_empty());
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM gates WHERE id = ?1", params![gate], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1, "the closed PR's gate was deleted");
    }

    /// A gate that could not have an exercise — nothing in the repository says
    /// how to run the program — is a different fact from one that skipped it,
    /// and §11 reports the difference. It has to survive the round trip.
    #[test]
    fn coverage_records_whether_the_gate_had_an_exercise() {
        let t = temp_db("exercise-coverage");
        let conn = open_at(&t.0).unwrap();
        let gate = seed(&conn);

        assert!(!gate_for_pr(&conn, "o/r", 7).unwrap().unwrap().has_exercise);

        set_gate_coverage(&conn, gate, 2, true, true).unwrap();
        let stored = gate_for_pr(&conn, "o/r", 7).unwrap().unwrap();
        assert!(stored.has_exercise);
        assert!(stored.has_checkable);

        set_gate_coverage(&conn, gate, 2, true, false).unwrap();
        assert!(!gate_for_pr(&conn, "o/r", 7).unwrap().unwrap().has_exercise);
    }

    /// Migration 3 rebuilds the gates table, which drops it while questions and
    /// attempts still point at it. With foreign keys enforced that cascade
    /// would empty the database; the migration runs with them off for exactly
    /// this reason, and this is the test that would catch it coming back.
    #[test]
    fn rebuilding_the_gates_table_keeps_the_rows_hanging_off_it() {
        let t = temp_db("migrate-3");
        let conn = Connection::open(&t.0).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        // Stop at version 2, the schema before the archive existed.
        for (i, sql) in MIGRATIONS.iter().take(2).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {};", i + 1)).unwrap();
        }
        // Written the way version 2 wrote it: `upsert_gate` knows about the
        // archive column, and that column is what this migration adds.
        conn.execute(
            "INSERT INTO gates
               (repo, path, pr_number, branch, base_ref, base_sha, head_sha, diff,
                hunks_total, hunks_ai, hunks_covered, authorship, state,
                created_at, updated_at)
             VALUES ('o/r','/tmp/o-r',7,'b','origin/main','a','b','d',1,1,0,'trailers','open',
                     '2026-01-01','2026-01-01')",
            [],
        )
        .unwrap();
        let gate = conn.last_insert_rowid();
        insert_questions(
            &conn,
            gate,
            &[NewQuestion {
                kind: "prediction",
                file: "a.rs",
                anchor: "sha256:x",
                text: "q",
                reference: "r",
                hints: &[],
            }],
        )
        .unwrap();
        let q = questions_for(&conn, gate).unwrap().remove(0);
        record_attempt(&conn, q.id, "answer", "kept", 0, "partial", 0.6, "f", "haiku").unwrap();

        migrate(&conn).unwrap();

        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
        assert_eq!(questions_for(&conn, gate).unwrap().len(), 1, "questions were cascaded away");
        assert_eq!(attempt_count(&conn, gate).unwrap(), 1, "attempts were cascaded away");
        assert!(gate_for_pr(&conn, "o/r", 7).unwrap().is_some(), "the gate itself was lost");
    }

    #[test]
    fn an_attempt_updates_the_question_and_is_retrievable() {
        let t = temp_db("attempt");
        let conn = open_at(&t.0).unwrap();
        let gate = seed(&conn);
        insert_questions(
            &conn,
            gate,
            &[NewQuestion {
                kind: "checkable",
                file: "a.rs",
                anchor: "sha256:x",
                text: "q",
                reference: "r",
                hints: &[],
            }],
        )
        .unwrap();
        let q = questions_for(&conn, gate).unwrap().remove(0);

        record_attempt(&conn, q.id, "answer", "retry_count is never decremented", 0, "demonstrates", 0.9, "ok", "stub").unwrap();
        let after = questions_for(&conn, gate).unwrap().remove(0);
        assert_eq!(after.status, "passed");
        assert_eq!(after.label.as_deref(), Some("demonstrates"));
        assert_eq!(
            last_answer(&conn, q.id).unwrap().as_deref(),
            Some("retry_count is never decremented")
        );
    }

    #[test]
    fn a_restates_answer_leaves_the_question_open() {
        let t = temp_db("open");
        let conn = open_at(&t.0).unwrap();
        let gate = seed(&conn);
        insert_questions(&conn, gate, &[NewQuestion {
            kind: "prediction", file: "a.rs", anchor: "sha256:x",
            text: "q", reference: "r", hints: &[],
        }]).unwrap();
        let q = questions_for(&conn, gate).unwrap().remove(0);
        record_attempt(&conn, q.id, "answer", "it adds a check", 0, "restates", 0.3, "no", "stub").unwrap();
        assert_eq!(questions_for(&conn, gate).unwrap().remove(0).status, "open");
    }
}
