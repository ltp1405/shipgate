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
];

fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > current {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            match conn.execute_batch(sql) {
                Ok(()) => {
                    conn.execute_batch(&format!("PRAGMA user_version = {version};"))?;
                    conn.execute_batch("COMMIT;")?;
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK;");
                    return Err(e).context(format!("migration {version} failed"));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- rows

#[derive(Debug, Clone)]
pub struct Gate {
    pub id: i64,
    pub repo: String,
    /// The diff snapshot taken when the gate was created. `--reuse` replays
    /// from this rather than re-deriving it, so the questions still line up
    /// with the text they were written against.
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

/// One gate per PR. A re-run replaces the previous one outright — §9 regenerates
/// rather than pretending old anchors survive a base change.
pub fn upsert_gate(conn: &Connection, g: &NewGate) -> Result<i64> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<i64> {
        conn.execute(
            "DELETE FROM gates WHERE repo = ?1 AND pr_number = ?2",
            params![g.repo, g.pr_number as i64],
        )?;
        conn.execute(
            "INSERT INTO gates
               (repo, pr_number, branch, base_ref, base_sha, head_sha, diff,
                hunks_total, hunks_ai, hunks_covered, authorship, state,
                created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10,?11,?12,?12)",
            params![
                g.repo,
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
) -> Result<()> {
    conn.execute(
        "UPDATE gates SET hunks_covered = ?1, has_checkable = ?2, updated_at = ?3 WHERE id = ?4",
        params![covered, has_checkable as i64, now(), gate_id],
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
        state: row.get("state")?,
    })
}

pub fn gate_for_pr(conn: &Connection, repo: &str, pr: u64) -> Result<Option<Gate>> {
    let mut stmt = conn.prepare("SELECT * FROM gates WHERE repo = ?1 AND pr_number = ?2")?;
    let mut rows = stmt.query_map(params![repo, pr as i64], gate_from_row)?;
    Ok(rows.next().transpose()?)
}

pub fn gates_for_repo(conn: &Connection, repo: &str) -> Result<Vec<Gate>> {
    let mut stmt = conn.prepare("SELECT * FROM gates WHERE repo = ?1 ORDER BY pr_number DESC")?;
    let rows = stmt.query_map(params![repo], gate_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    fn re_running_replaces_the_gate_rather_than_duplicating_it() {
        let t = temp_db("upsert");
        let conn = open_at(&t.0).unwrap();
        // The rowid is reused after the delete — harmless, because every table
        // referencing a gate cascades with it.
        seed(&conn);
        seed(&conn);
        let n: i64 = conn
            .query_row("SELECT count(*) FROM gates WHERE repo='o/r'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "one gate per PR");
    }

    #[test]
    fn questions_cascade_when_their_gate_is_replaced() {
        let t = temp_db("cascade");
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
        assert_eq!(questions_for(&conn, gate).unwrap().len(), 1);

        seed(&conn); // replaces the gate
        let orphans: i64 = conn
            .query_row("SELECT count(*) FROM questions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "ON DELETE CASCADE must clear the old questions");
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
