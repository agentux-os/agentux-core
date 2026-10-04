//! SQLite persistence for `agentuxd` (ADR 0003): projects, runs, step
//! attempts, approval requests and the event log.
//!
//! Every change goes through [`Store::write`], which runs a closure inside one
//! transaction. Events recorded during the transaction are stored in the same
//! transaction and broadcast to subscribers only after it commits, so a
//! subscriber never sees a state that was rolled back, and a restarted daemon
//! finds exactly the state it last announced.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::BuildHasher;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentux_api::{
    AttemptStatus, BusMessage, CheckResult, Event, EventBody, PermissionRequest, Project,
    PullRequest, RequestKind, RequestStatus, Run, RunStatus, Session, SessionState, SessionUsage,
    StepAttempt, StepKind,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;

mod schema;

/// Where a run is within its current step. Persisted, so the engine knows
/// what to do next after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The worktree does not exist yet.
    Setup,
    /// Ready to run `pipeline[step_index]` (or running it: see the attempts).
    Step,
    /// Paused on a pending approval request.
    Approval,
    /// The human approved running `pipeline[step_index]` (used by steps that
    /// ask before acting, such as `pull_request`).
    Approved,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Step => "step",
            Self::Approval => "approval",
            Self::Approved => "approved",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "setup" => Self::Setup,
            "step" => Self::Step,
            "approval" => Self::Approval,
            "approved" => Self::Approved,
            _ => return None,
        })
    }
}

/// A run with the engine's private state next to its public view.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run: Run,
    pub phase: Phase,
    /// Loop counters by step index: consecutive attempts of a gate, rounds
    /// of a review.
    pub loops: BTreeMap<usize, u32>,
    /// Gate output or review comments for the next agent step.
    pub feedback: Option<String>,
    /// The project's `agentux.yaml` when the run started; `None` means the
    /// built-in default pipeline.
    pub config_yaml: Option<String>,
    /// The commit the run's branch started from, for reviewers to diff
    /// against.
    pub base_commit: Option<String>,
}

/// What a new approval request is about.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub kind: RequestKind,
    pub run_id: String,
    pub project_id: String,
    pub step_index: usize,
    pub step: StepKind,
    pub title: String,
    pub detail: String,
    pub session_id: Option<String>,
    /// Suggested answers, for `question` requests.
    pub options: Vec<String>,
}

#[derive(Debug)]
pub enum Error {
    Sqlite(rusqlite::Error),
    /// A stored value could not be decoded, or the database was written by a
    /// newer version.
    Data(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database error: {e}"),
            Self::Data(m) => write!(f, "database error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Handle to the database. Cheap to clone; clones share one connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    events: broadcast::Sender<Event>,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and applies pending
    /// migrations.
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        // FULL: a committed transition survives power loss, not just a crash
        // of the daemon.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )?;
        schema::migrate(&mut conn)?;
        let (events, _) = broadcast::channel(1024);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            events,
        })
    }

    /// Receives every event committed from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Runs `f` in a transaction and commits if it returns `Ok`. Events it
    /// recorded are broadcast after the commit.
    pub fn write<T, E: From<Error>>(
        &self,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let (value, events) = {
            let mut conn = self.lock();
            let mut tx = Tx {
                tx: conn.transaction().map_err(Error::from)?,
                events: Vec::new(),
            };
            let value = f(&mut tx)?;
            let Tx { tx, events } = tx;
            tx.commit().map_err(Error::from)?;
            (value, events)
        };
        for event in events {
            // No subscribers is fine.
            let _ = self.events.send(event);
        }
        Ok(value)
    }

    /// Runs `f` in a read-only snapshot.
    pub fn read<T, E: From<Error>>(&self, f: impl FnOnce(&Tx<'_>) -> Result<T, E>) -> Result<T, E> {
        let mut conn = self.lock();
        let tx = Tx {
            tx: conn.transaction().map_err(Error::from)?,
            events: Vec::new(),
        };
        f(&tx)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves no partial transaction behind
        // (it is rolled back on drop), so the connection is still usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A transaction. Obtained from [`Store::write`] or [`Store::read`].
pub struct Tx<'a> {
    tx: rusqlite::Transaction<'a>,
    events: Vec<Event>,
}

const RUN_COLUMNS: &str = "id, project_id, title, prompt, issue, config_yaml, status, phase, \
     step_index, steps, roles, branch, worktree, loops, feedback, checks, gate_attempt, \
     gate_max_attempts, review_round, review_max_rounds, budget_usd, activity, error, \
     pull_request, started_at, updated_at, finished_at, sessions, cost_usd, base_commit";

const ATTEMPT_COLUMNS: &str =
    "id, run_id, step_index, step, status, output, started_at, finished_at";

const REQUEST_COLUMNS: &str = "id, kind, run_id, project_id, step_index, step, title, detail, \
     status, answer, created_at, resolved_at, session_id, options";

const SESSION_COLUMNS: &str = "id, run_id, project_id, role, harness, model, state, cwd, usage, \
     started_at, updated_at, ended_at, vendor_session_id";

impl Tx<'_> {
    // ---- events ----

    /// Records an event; it is broadcast once the transaction commits.
    pub fn emit(&mut self, run_id: Option<&str>, body: EventBody) -> Result<()> {
        let at = now_ms();
        self.tx.execute(
            "INSERT INTO events (run_id, at, body) VALUES (?1, ?2, ?3)",
            params![run_id, at, to_json(&body)?],
        )?;
        self.events.push(Event {
            seq: self.tx.last_insert_rowid(),
            at,
            run_id: run_id.map(str::to_string),
            body,
        });
        Ok(())
    }

    pub fn log(&mut self, run_id: &str, text: impl Into<String>) -> Result<()> {
        self.emit(Some(run_id), EventBody::Log { text: text.into() })
    }

    /// Persisted events with `seq > since`, optionally only those of one run.
    pub fn events_since(&self, since: i64, run_id: Option<&str>) -> Result<Vec<Event>> {
        self.events_page(since, run_id, None)
    }

    /// Like [`Tx::events_since`], at most `limit` events.
    pub fn events_page(
        &self,
        since: i64,
        run_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<Event>> {
        // SQLite: a negative LIMIT means no limit.
        let limit = limit.map_or(-1, i64::from);
        let mut stmt = self.tx.prepare(
            "SELECT seq, at, run_id, body FROM events
             WHERE seq > ?1 AND (?2 IS NULL OR run_id = ?2) ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![since, run_id, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (seq, at, run_id, body) = row?;
            Ok(Event {
                seq,
                at,
                run_id,
                body: from_json(&body)?,
            })
        })
        .collect()
    }

    /// The bus log of a run (its `bus_message` events), oldest first.
    pub fn bus_messages(&self, run_id: &str) -> Result<Vec<BusMessage>> {
        let mut stmt = self.tx.prepare(
            "SELECT body FROM events
             WHERE run_id = ?1 AND json_extract(body, '$.kind') = 'bus_message' ORDER BY seq",
        )?;
        let rows = stmt.query_map([run_id], |row| row.get::<_, String>(0))?;
        rows.map(|body| match from_json(&body?)? {
            EventBody::BusMessage { message } => Ok(message),
            _ => Err(Error::Data("a bus_message event without a message".into())),
        })
        .collect()
    }

    pub fn last_event_seq(&self) -> Result<i64> {
        Ok(self
            .tx
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    // ---- projects ----

    /// Registers the project at `path`, or returns the one already there.
    pub fn register_project(&mut self, name: &str, path: &str) -> Result<Project> {
        if let Some(project) = self
            .tx
            .query_row(
                "SELECT id, name, path, created_at FROM projects WHERE path = ?1",
                [path],
                project_from_row,
            )
            .optional()?
        {
            return Ok(project);
        }
        let project = Project {
            id: new_id(),
            name: name.into(),
            path: path.into(),
            created_at: now_ms(),
        };
        self.tx.execute(
            "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![project.id, project.name, project.path, project.created_at],
        )?;
        self.emit(
            None,
            EventBody::Project {
                project: project.clone(),
            },
        )?;
        Ok(project)
    }

    pub fn project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self
            .tx
            .query_row(
                "SELECT id, name, path, created_at FROM projects WHERE id = ?1",
                [id],
                project_from_row,
            )
            .optional()?)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .tx
            .prepare("SELECT id, name, path, created_at FROM projects ORDER BY created_at, id")?;
        let rows = stmt.query_map([], project_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- runs ----

    /// Inserts a new run. Its `id`, timestamps and `updated_at` are taken
    /// from `record` as given.
    pub fn insert_run(&mut self, record: &RunRecord) -> Result<()> {
        self.tx.execute(
            &format!(
                "INSERT INTO runs ({RUN_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
                 ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, \
                 ?25, ?26, ?27, ?28, ?29, ?30)"
            ),
            rusqlite::params_from_iter(run_params(record)?),
        )?;
        self.emit(
            Some(&record.run.id),
            EventBody::Run {
                run: record.run.clone(),
            },
        )
    }

    /// Writes `record` back, bumping `updated_at`, and emits a run event.
    pub fn save_run(&mut self, record: &mut RunRecord) -> Result<()> {
        record.run.updated_at = now_ms();
        let changed = self.tx.execute(
            "UPDATE runs SET project_id = ?2, title = ?3, prompt = ?4, issue = ?5, \
             config_yaml = ?6, status = ?7, phase = ?8, step_index = ?9, steps = ?10, \
             roles = ?11, branch = ?12, worktree = ?13, loops = ?14, feedback = ?15, \
             checks = ?16, gate_attempt = ?17, gate_max_attempts = ?18, review_round = ?19, \
             review_max_rounds = ?20, budget_usd = ?21, activity = ?22, error = ?23, \
             pull_request = ?24, started_at = ?25, updated_at = ?26, finished_at = ?27, \
             sessions = ?28, cost_usd = ?29, base_commit = ?30 \
             WHERE id = ?1",
            rusqlite::params_from_iter(run_params(record)?),
        )?;
        if changed != 1 {
            return Err(Error::Data(format!("run {} does not exist", record.run.id)));
        }
        self.emit(
            Some(&record.run.id),
            EventBody::Run {
                run: record.run.clone(),
            },
        )
    }

    pub fn run(&self, id: &str) -> Result<Option<RunRecord>> {
        let mut stmt = self
            .tx
            .prepare(&format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"))?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(run_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Runs, newest first, optionally only those of one project.
    pub fn runs(&self, project_id: Option<&str>) -> Result<Vec<RunRecord>> {
        let mut stmt = self.tx.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE ?1 IS NULL OR project_id = ?1 \
             ORDER BY started_at DESC, id"
        ))?;
        let mut rows = stmt.query([project_id])?;
        let mut runs = Vec::new();
        while let Some(row) = rows.next()? {
            runs.push(run_from_row(row)?);
        }
        Ok(runs)
    }

    /// Ids of runs that are not finished: what a restarted daemon resumes.
    pub fn active_run_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.tx.prepare(
            "SELECT id FROM runs WHERE status IN ('running', 'waiting') ORDER BY started_at, id",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- step attempts ----

    pub fn start_attempt(
        &mut self,
        run_id: &str,
        step_index: usize,
        step: StepKind,
    ) -> Result<StepAttempt> {
        let started_at = now_ms();
        self.tx.execute(
            "INSERT INTO step_attempts (run_id, step_index, step, status, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![run_id, step_index as i64, step.as_str(), started_at],
        )?;
        let attempt = StepAttempt {
            id: self.tx.last_insert_rowid(),
            run_id: run_id.into(),
            step_index,
            step,
            status: AttemptStatus::Running,
            output: None,
            started_at,
            finished_at: None,
        };
        self.emit(
            Some(run_id),
            EventBody::Attempt {
                attempt: attempt.clone(),
            },
        )?;
        Ok(attempt)
    }

    pub fn finish_attempt(
        &mut self,
        id: i64,
        status: AttemptStatus,
        output: Option<&str>,
    ) -> Result<StepAttempt> {
        self.tx.execute(
            "UPDATE step_attempts SET status = ?2, output = ?3, finished_at = ?4 WHERE id = ?1",
            params![id, status.as_str(), output, now_ms()],
        )?;
        let attempt = self
            .tx
            .query_row(
                &format!("SELECT {ATTEMPT_COLUMNS} FROM step_attempts WHERE id = ?1"),
                [id],
                attempt_from_row,
            )
            .optional()?
            .ok_or_else(|| Error::Data(format!("step attempt {id} does not exist")))?
            .map_err(Error::Data)?;
        self.emit(
            Some(&attempt.run_id.clone()),
            EventBody::Attempt {
                attempt: attempt.clone(),
            },
        )?;
        Ok(attempt)
    }

    /// Attempts of a run, oldest first.
    pub fn attempts(&self, run_id: &str) -> Result<Vec<StepAttempt>> {
        let mut stmt = self.tx.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM step_attempts WHERE run_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([run_id], attempt_from_row)?;
        rows.map(|r| r?.map_err(Error::Data)).collect()
    }

    // ---- approval requests ----

    pub fn create_request(&mut self, new: NewRequest) -> Result<PermissionRequest> {
        let request = PermissionRequest {
            id: new_id(),
            kind: new.kind,
            run_id: new.run_id,
            project_id: new.project_id,
            step_index: new.step_index,
            step: new.step,
            session_id: new.session_id,
            title: new.title,
            detail: new.detail,
            options: new.options,
            status: RequestStatus::Pending,
            answer: None,
            created_at: now_ms(),
            resolved_at: None,
        };
        self.tx.execute(
            &format!(
                "INSERT INTO requests ({REQUEST_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                request.id,
                request.kind.as_str(),
                request.run_id,
                request.project_id,
                request.step_index as i64,
                request.step.as_str(),
                request.title,
                request.detail,
                request.status.as_str(),
                request.answer,
                request.created_at,
                request.resolved_at,
                request.session_id,
                to_json(&request.options)?,
            ],
        )?;
        self.emit(
            Some(&request.run_id.clone()),
            EventBody::Request {
                request: request.clone(),
            },
        )?;
        Ok(request)
    }

    /// Records the decision on a request and emits a request event.
    pub fn resolve_request(
        &mut self,
        id: &str,
        status: RequestStatus,
        answer: Option<&str>,
    ) -> Result<PermissionRequest> {
        self.tx.execute(
            "UPDATE requests SET status = ?2, answer = ?3, resolved_at = ?4 WHERE id = ?1",
            params![id, status.as_str(), answer, now_ms()],
        )?;
        let request = self
            .request(id)?
            .ok_or_else(|| Error::Data(format!("request {id} does not exist")))?;
        self.emit(
            Some(&request.run_id.clone()),
            EventBody::Request {
                request: request.clone(),
            },
        )?;
        Ok(request)
    }

    pub fn request(&self, id: &str) -> Result<Option<PermissionRequest>> {
        self.tx
            .query_row(
                &format!("SELECT {REQUEST_COLUMNS} FROM requests WHERE id = ?1"),
                [id],
                request_from_row,
            )
            .optional()?
            .transpose()
            .map_err(Error::Data)
    }

    /// Requests, newest first, filtered by run and/or status.
    pub fn requests(
        &self,
        run_id: Option<&str>,
        status: Option<RequestStatus>,
    ) -> Result<Vec<PermissionRequest>> {
        let mut stmt = self.tx.prepare(&format!(
            "SELECT {REQUEST_COLUMNS} FROM requests
             WHERE (?1 IS NULL OR run_id = ?1) AND (?2 IS NULL OR status = ?2)
             ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map(
            params![run_id, status.map(|s| s.as_str())],
            request_from_row,
        )?;
        rows.map(|r| r?.map_err(Error::Data)).collect()
    }

    // ---- sessions ----

    /// Inserts a new session and emits a session event.
    pub fn insert_session(&mut self, session: &Session) -> Result<()> {
        self.tx.execute(
            &format!(
                "INSERT INTO sessions ({SESSION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ),
            rusqlite::params_from_iter(session_params(session)?),
        )?;
        self.emit(
            Some(&session.run_id),
            EventBody::Session {
                session: session.clone(),
            },
        )
    }

    /// Writes `session` back, bumping `updated_at`, and emits a session
    /// event.
    pub fn save_session(&mut self, session: &mut Session) -> Result<()> {
        session.updated_at = now_ms();
        let changed = self.tx.execute(
            "UPDATE sessions SET run_id = ?2, project_id = ?3, role = ?4, harness = ?5, \
             model = ?6, state = ?7, cwd = ?8, usage = ?9, started_at = ?10, \
             updated_at = ?11, ended_at = ?12, vendor_session_id = ?13 WHERE id = ?1",
            rusqlite::params_from_iter(session_params(session)?),
        )?;
        if changed != 1 {
            return Err(Error::Data(format!(
                "session {} does not exist",
                session.id
            )));
        }
        self.emit(
            Some(&session.run_id),
            EventBody::Session {
                session: session.clone(),
            },
        )
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        self.tx
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                [id],
                session_from_row,
            )
            .optional()?
            .transpose()
    }

    /// Sessions, oldest first, optionally only those of one run.
    pub fn sessions(&self, run_id: Option<&str>) -> Result<Vec<Session>> {
        let mut stmt = self.tx.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE ?1 IS NULL OR run_id = ?1 \
             ORDER BY started_at, id"
        ))?;
        let rows = stmt.query_map([run_id], session_from_row)?;
        rows.map(|r| r?).collect()
    }
}

// ---- row mapping ----

fn session_params(session: &Session) -> Result<Vec<Box<dyn rusqlite::ToSql>>> {
    Ok(vec![
        Box::new(session.id.clone()),
        Box::new(session.run_id.clone()),
        Box::new(session.project_id.clone()),
        Box::new(session.role.clone()),
        Box::new(session.harness.clone()),
        Box::new(session.model.clone()),
        Box::new(session.state.as_str()),
        Box::new(session.cwd.clone()),
        Box::new(to_json(&session.usage)?),
        Box::new(session.started_at),
        Box::new(session.updated_at),
        Box::new(session.ended_at),
        Box::new(session.vendor_session_id.clone()),
    ])
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Result<Session>> {
    let state: String = row.get(6)?;
    let usage: String = row.get(8)?;
    let Some(state) = SessionState::parse(&state) else {
        return Ok(Err(Error::Data(format!("bad session state {state:?}"))));
    };
    let usage: SessionUsage = match from_json(&usage) {
        Ok(usage) => usage,
        Err(e) => return Ok(Err(e)),
    };
    Ok(Ok(Session {
        id: row.get(0)?,
        run_id: row.get(1)?,
        project_id: row.get(2)?,
        role: row.get(3)?,
        harness: row.get(4)?,
        model: row.get(5)?,
        state,
        cwd: row.get(7)?,
        usage,
        started_at: row.get(9)?,
        updated_at: row.get(10)?,
        ended_at: row.get(11)?,
        vendor_session_id: row.get(12)?,
    }))
}

fn project_from_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn run_params(record: &RunRecord) -> Result<Vec<Box<dyn rusqlite::ToSql>>> {
    let run = &record.run;
    let loops: BTreeMap<String, u32> = record
        .loops
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    Ok(vec![
        Box::new(run.id.clone()),
        Box::new(run.project_id.clone()),
        Box::new(run.title.clone()),
        Box::new(run.prompt.clone()),
        Box::new(run.issue.map(|i| i as i64)),
        Box::new(record.config_yaml.clone()),
        Box::new(run.status.as_str()),
        Box::new(record.phase.as_str()),
        Box::new(run.step_index as i64),
        Box::new(to_json(&run.steps)?),
        Box::new(to_json(&run.roles)?),
        Box::new(run.branch.clone()),
        Box::new(run.worktree.clone()),
        Box::new(to_json(&loops)?),
        Box::new(record.feedback.clone()),
        Box::new(to_json(&run.checks)?),
        Box::new(run.gate_attempt),
        Box::new(run.gate_max_attempts),
        Box::new(run.review_round),
        Box::new(run.review_max_rounds),
        Box::new(run.budget_usd),
        Box::new(run.activity.clone()),
        Box::new(run.error.clone()),
        Box::new(run.pull_request.as_ref().map(to_json).transpose()?),
        Box::new(run.started_at),
        Box::new(run.updated_at),
        Box::new(run.finished_at),
        Box::new(to_json(&run.sessions)?),
        Box::new(run.cost_usd),
        Box::new(record.base_commit.clone()),
    ])
}

fn run_from_row(row: &Row<'_>) -> Result<RunRecord> {
    let text = |i: usize| -> Result<String> { Ok(row.get::<_, String>(i)?) };
    let status = text(6)?;
    let phase = text(7)?;
    let loops: BTreeMap<String, u32> = from_json(&text(13)?)?;
    let loops = loops
        .into_iter()
        .map(|(k, v)| {
            k.parse()
                .map(|k| (k, v))
                .map_err(|_| Error::Data(format!("bad loop counter key {k:?}")))
        })
        .collect::<Result<_>>()?;
    let pull_request: Option<String> = row.get(23)?;
    Ok(RunRecord {
        run: Run {
            id: row.get(0)?,
            project_id: row.get(1)?,
            title: row.get(2)?,
            prompt: row.get(3)?,
            issue: row.get::<_, Option<i64>>(4)?.map(|i| i as u64),
            branch: row.get(11)?,
            worktree: row.get(12)?,
            steps: from_json(&text(9)?)?,
            step_index: row.get::<_, i64>(8)? as usize,
            step: StepKind::Plan, // replaced below
            status: RunStatus::parse(&status)
                .ok_or_else(|| Error::Data(format!("bad run status {status:?}")))?,
            roles: from_json(&text(10)?)?,
            checks: from_json::<Vec<CheckResult>>(&text(15)?)?,
            gate_attempt: row.get(16)?,
            gate_max_attempts: row.get(17)?,
            review_round: row.get(18)?,
            review_max_rounds: row.get(19)?,
            budget_usd: row.get(20)?,
            activity: row.get(21)?,
            error: row.get(22)?,
            pull_request: pull_request
                .as_deref()
                .map(from_json::<PullRequest>)
                .transpose()?,
            started_at: row.get(24)?,
            updated_at: row.get(25)?,
            finished_at: row.get(26)?,
            sessions: from_json(&text(27)?)?,
            cost_usd: row.get(28)?,
        },
        phase: Phase::parse(&phase).ok_or_else(|| Error::Data(format!("bad phase {phase:?}")))?,
        loops,
        feedback: row.get(14)?,
        config_yaml: row.get(5)?,
        base_commit: row.get(29)?,
    })
    .map(|mut record| {
        record.run.step = record
            .run
            .steps
            .get(record.run.step_index)
            .or(record.run.steps.last())
            .copied()
            .unwrap_or(StepKind::Plan);
        record
    })
}

fn attempt_from_row(row: &Row<'_>) -> rusqlite::Result<std::result::Result<StepAttempt, String>> {
    let step: String = row.get(3)?;
    let status: String = row.get(4)?;
    let (Some(step), Some(status)) = (StepKind::parse(&step), AttemptStatus::parse(&status)) else {
        return Ok(Err(format!("bad step attempt {step:?} / {status:?}")));
    };
    Ok(Ok(StepAttempt {
        id: row.get(0)?,
        run_id: row.get(1)?,
        step_index: row.get::<_, i64>(2)? as usize,
        step,
        status,
        output: row.get(5)?,
        started_at: row.get(6)?,
        finished_at: row.get(7)?,
    }))
}

fn request_from_row(
    row: &Row<'_>,
) -> rusqlite::Result<std::result::Result<PermissionRequest, String>> {
    let kind: String = row.get(1)?;
    let step: String = row.get(5)?;
    let status: String = row.get(8)?;
    let options: String = row.get(13)?;
    let (Some(kind), Some(step), Some(status), Ok(options)) = (
        RequestKind::parse(&kind),
        StepKind::parse(&step),
        RequestStatus::parse(&status),
        serde_json::from_str::<Vec<String>>(&options),
    ) else {
        return Ok(Err(format!("bad request {kind:?} / {step:?} / {status:?}")));
    };
    Ok(Ok(PermissionRequest {
        id: row.get(0)?,
        kind,
        run_id: row.get(2)?,
        project_id: row.get(3)?,
        session_id: row.get(12)?,
        step_index: row.get::<_, i64>(4)? as usize,
        step,
        title: row.get(6)?,
        detail: row.get(7)?,
        options,
        status,
        answer: row.get(9)?,
        created_at: row.get(10)?,
        resolved_at: row.get(11)?,
    }))
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::Data(e.to_string()))
}

fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| Error::Data(e.to_string()))
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A random 8-hex-digit id. Short enough to type (`aux approve 3f9a0c12`),
/// valid as a branch and directory name.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let hash = RandomState::new().hash_one((now_ms(), n, std::process::id()));
    format!("{:08x}", hash as u32)
}

#[cfg(test)]
mod tests;
