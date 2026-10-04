//! Schema migrations, tracked with `PRAGMA user_version`. Each entry runs
//! once, in its own transaction; never edit an entry that has shipped, append
//! a new one.

use rusqlite::Connection;

use crate::{Error, Result};

const MIGRATIONS: &[&str] = &[
    // 1: initial schema.
    "
    CREATE TABLE projects (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL,
        path        TEXT NOT NULL UNIQUE,
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE runs (
        id                  TEXT PRIMARY KEY,
        project_id          TEXT NOT NULL REFERENCES projects(id),
        title               TEXT NOT NULL,
        prompt              TEXT,
        issue               INTEGER,
        config_yaml         TEXT,
        status              TEXT NOT NULL,
        phase               TEXT NOT NULL,
        step_index          INTEGER NOT NULL,
        steps               TEXT NOT NULL,
        roles               TEXT NOT NULL,
        branch              TEXT,
        worktree            TEXT,
        loops               TEXT NOT NULL,
        feedback            TEXT,
        checks              TEXT NOT NULL,
        gate_attempt        INTEGER NOT NULL,
        gate_max_attempts   INTEGER NOT NULL,
        review_round        INTEGER NOT NULL,
        review_max_rounds   INTEGER NOT NULL,
        budget_usd          REAL,
        activity            TEXT NOT NULL,
        error               TEXT,
        pull_request        TEXT,
        started_at          INTEGER NOT NULL,
        updated_at          INTEGER NOT NULL,
        finished_at         INTEGER
    );
    CREATE INDEX runs_by_status ON runs (status);
    CREATE INDEX runs_by_project ON runs (project_id);

    CREATE TABLE step_attempts (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id       TEXT NOT NULL REFERENCES runs(id),
        step_index   INTEGER NOT NULL,
        step         TEXT NOT NULL,
        status       TEXT NOT NULL,
        output       TEXT,
        started_at   INTEGER NOT NULL,
        finished_at  INTEGER
    );
    CREATE INDEX step_attempts_by_run ON step_attempts (run_id);

    CREATE TABLE requests (
        id           TEXT PRIMARY KEY,
        kind         TEXT NOT NULL,
        run_id       TEXT NOT NULL REFERENCES runs(id),
        project_id   TEXT NOT NULL REFERENCES projects(id),
        step_index   INTEGER NOT NULL,
        step         TEXT NOT NULL,
        title        TEXT NOT NULL,
        detail       TEXT NOT NULL,
        status       TEXT NOT NULL,
        answer       TEXT,
        created_at   INTEGER NOT NULL,
        resolved_at  INTEGER
    );
    CREATE INDEX requests_by_run ON requests (run_id);
    CREATE INDEX requests_by_status ON requests (status);

    CREATE TABLE events (
        seq     INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id  TEXT,
        at      INTEGER NOT NULL,
        body    TEXT NOT NULL
    );
    CREATE INDEX events_by_run ON events (run_id, seq);
    ",
    // 2: harness sessions, run cost and base commit, the session behind a
    // permission request.
    "
    ALTER TABLE runs ADD COLUMN sessions TEXT NOT NULL DEFAULT '{}';
    ALTER TABLE runs ADD COLUMN cost_usd REAL NOT NULL DEFAULT 0;
    ALTER TABLE runs ADD COLUMN base_commit TEXT;
    ALTER TABLE requests ADD COLUMN session_id TEXT;

    CREATE TABLE sessions (
        id              TEXT PRIMARY KEY,
        run_id          TEXT NOT NULL REFERENCES runs(id),
        project_id      TEXT NOT NULL REFERENCES projects(id),
        role            TEXT NOT NULL,
        harness         TEXT NOT NULL,
        model           TEXT,
        state           TEXT NOT NULL,
        cwd             TEXT NOT NULL,
        usage           TEXT NOT NULL,
        started_at      INTEGER NOT NULL,
        updated_at      INTEGER NOT NULL,
        ended_at        INTEGER
    );
    CREATE INDEX sessions_by_run ON sessions (run_id);
    ",
    // 3: suggested answers of `question` requests (agent bus `ask_human`).
    "
    ALTER TABLE requests ADD COLUMN options TEXT NOT NULL DEFAULT '[]';
    ",
];

/// The schema version this build writes.
pub const VERSION: usize = MIGRATIONS.len();

pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let current = usize::try_from(current).unwrap_or(usize::MAX);
    if current > VERSION {
        return Err(Error::Data(format!(
            "schema version {current} is newer than this agentuxd supports ({VERSION}); \
             upgrade agentuxd"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
