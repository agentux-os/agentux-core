//! The run state machine (ADR 0003).
//!
//! Each active run is driven by one tokio task that loops: read the run's
//! committed state, do the side effect that state calls for (create the
//! worktree, call an agent, run checks, open the pull request), then commit
//! the next state. Every step attempt is recorded as `running` before its side
//! effect starts, and its outcome and the transition are committed together
//! afterwards, so after a crash the daemon finds the run exactly where its
//! last commit left it and runs the interrupted step again. Steps are
//! therefore required to be idempotent (see [`StepExecutor`]).
//!
//! While an agent step runs, the executor reports sessions and their events
//! through a [`StepHost`] the engine hands it, and asks it before tool calls.
//! Such a permission request pauses the run (`waiting`) without leaving the
//! step: the agent waits for the answer. After implement and custom steps the
//! daemon commits whatever changed in the worktree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::{fmt, fs, io};

use agentux_api::rpc::StartRun;
use agentux_api::{
    AttemptStatus, CheckResult, CheckStatus, EventBody, PermissionRequest, Project, RequestKind,
    RequestStatus, Run, RunStatus, Session, SessionEvent, SessionState, StepAttempt, StepKind,
};
use agentux_config::{Config, FILE_NAME, Loop, Step};
use agentux_store::{NewRequest, Phase, RunRecord, Store, Tx, new_id, now_ms};
use agentux_worktree::Worktrees;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::bus::{BusLink, Hub};
use crate::executor::{
    AgentOutcome, AgentTask, BoxFuture, OpenedSession, PullRequestOutcome, PullRequestTask,
    StepExecutor, StepHost,
};
use crate::terminal::Terminals;

/// Keep at most this much of each check's output.
const MAX_CHECK_OUTPUT: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NotFound(String),
    /// The object is not in a state that allows the operation.
    Conflict(String),
    InvalidProject(String),
    InvalidParams(String),
    /// A bus bridge request with an unknown or expired session token.
    Unauthorized(String),
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(m)
            | Self::Conflict(m)
            | Self::InvalidProject(m)
            | Self::InvalidParams(m)
            | Self::Unauthorized(m)
            | Self::Internal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<agentux_store::Error> for Error {
    fn from(e: agentux_store::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// Engine behavior chosen when the daemon starts.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Allow every tool call agents ask about, without asking the human.
    /// Dangerous: agents can then run any command in the worktree.
    pub auto_approve_permissions: bool,
    /// How sessions reach the agent bus. `None`: sessions still join their
    /// run's bus (wakes work), but get no `agentux` MCP server.
    pub bus: Option<BusLink>,
    /// How a harness's TUI resumes a session (`terminals.open`).
    pub tui: TuiCommands,
}

/// The command line that opens a harness's own TUI on an existing session:
/// `(harness id, vendor session id) -> program and arguments`, or `None`
/// when the harness cannot do that. The default is
/// [`agentux_harness::HarnessSpec::tui_resume`]; tests plug in their own.
#[derive(Clone)]
pub struct TuiCommands(Arc<TuiFn>);

type TuiFn = dyn Fn(&str, &str) -> Option<Vec<String>> + Send + Sync;

impl TuiCommands {
    pub fn new(f: impl Fn(&str, &str) -> Option<Vec<String>> + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn resume(&self, harness: &str, vendor_session_id: &str) -> Option<Vec<String>> {
        (self.0)(harness, vendor_session_id)
    }
}

impl Default for TuiCommands {
    fn default() -> Self {
        Self::new(|harness, id| agentux_harness::HarnessSpec::find(harness)?.tui_resume(id))
    }
}

impl fmt::Debug for TuiCommands {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TuiCommands(..)")
    }
}

/// Owns the store, the executor and one driver task per active run. Cheap to
/// clone.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) store: Store,
    pub(crate) executor: Arc<dyn StepExecutor>,
    pub(crate) settings: Settings,
    /// Driver task per run. A run has at most one.
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    /// Agents waiting for the answer to a permission request, by request id.
    waiters: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    /// The agent buses of active runs.
    pub(crate) hub: Hub,
    /// Terminals (`terminals.*`).
    pub(crate) terminals: Terminals,
}

impl Engine {
    pub fn new(store: Store, executor: Arc<dyn StepExecutor>) -> Self {
        Self::with_settings(store, executor, Settings::default())
    }

    pub fn with_settings(
        store: Store,
        executor: Arc<dyn StepExecutor>,
        settings: Settings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                executor,
                settings,
                tasks: Mutex::new(HashMap::new()),
                waiters: Mutex::new(HashMap::new()),
                hub: Hub::default(),
                terminals: Terminals::default(),
            }),
        }
    }

    pub(crate) fn from_inner(inner: Arc<Inner>) -> Self {
        Self { inner }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// Picks up every unfinished run after a (re)start. Attempts left
    /// `running` by a previous process are marked `interrupted`; their step
    /// runs again. The previous process's sessions are gone, so their
    /// permission requests and questions to the human are cancelled and the
    /// sessions marked ended.
    /// Returns how many runs were resumed.
    pub fn resume(&self) -> Result<usize> {
        let ids = self.inner.store.write(|tx| {
            let ids = tx.active_run_ids()?;
            for id in &ids {
                for attempt in tx.attempts(id)? {
                    if attempt.status == AttemptStatus::Running {
                        tx.finish_attempt(
                            attempt.id,
                            AttemptStatus::Interrupted,
                            Some("agentuxd stopped during this step"),
                        )?;
                        tx.log(
                            id,
                            format!(
                                "agentuxd restarted during the {} step; running it again",
                                attempt.step
                            ),
                        )?;
                    }
                }
                for request in tx.requests(Some(id), Some(RequestStatus::Pending))? {
                    if matches!(
                        request.kind,
                        RequestKind::Permission | RequestKind::Question
                    ) {
                        tx.resolve_request(
                            &request.id,
                            RequestStatus::Cancelled,
                            Some("agentuxd restarted; the session that asked is gone"),
                        )?;
                    }
                }
                end_sessions(tx, id)?;
                if let Some(mut record) = tx.run(id)?
                    && record.run.status == RunStatus::Waiting
                    && record.phase != Phase::Approval
                {
                    record.run.status = RunStatus::Running;
                    tx.save_run(&mut record)?;
                }
            }
            Ok::<_, Error>(ids)
        })?;
        for id in &ids {
            self.reopen_bus(id);
            self.spawn(id);
        }
        Ok(ids.len())
    }

    /// Stops every driver task. Their runs stay as last committed and resume
    /// on the next start.
    pub fn shutdown(&self) {
        for (_, task) in self.tasks().drain() {
            task.abort();
        }
    }

    // ---- API operations ----

    /// Registers the git repository containing `path` (absolute).
    pub async fn register_project(&self, path: &str) -> Result<Project> {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(Error::InvalidParams(format!(
                "project path must be absolute: {}",
                path.display()
            )));
        }
        let shown = path.display().to_string();
        let repo = tokio::task::spawn_blocking(move || {
            Worktrees::open(&path).map(|w| w.repo().to_path_buf())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
        .map_err(|e| Error::InvalidProject(format!("{shown} is not a git repository: {e}")))?;
        Config::load(&repo).map_err(|e| {
            Error::InvalidProject(format!(
                "{} is invalid: {e}",
                repo.join(FILE_NAME).display()
            ))
        })?;
        let name = repo
            .file_name()
            .map_or_else(|| "project".into(), |n| n.to_string_lossy().into_owned());
        let path = repo.to_string_lossy().into_owned();
        self.inner
            .store
            .write(|tx| tx.register_project(&name, &path))
            .map_err(Error::from)
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        Ok(self.inner.store.read(|tx| tx.projects())?)
    }

    /// Creates a run from the project's current `agentux.yaml` and starts it.
    pub fn start_run(&self, params: StartRun) -> Result<Run> {
        let prompt = params.prompt.filter(|p| !p.trim().is_empty());
        if prompt.is_none() && params.issue.is_none() {
            return Err(Error::InvalidParams(
                "a run needs a prompt or an issue".into(),
            ));
        }
        let project = self
            .inner
            .store
            .read(|tx| tx.project(&params.project_id))?
            .ok_or_else(|| Error::NotFound(format!("no project {}", params.project_id)))?;

        let file = Path::new(&project.path).join(FILE_NAME);
        let yaml = match fs::read_to_string(&file) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(Error::InvalidProject(format!(
                    "cannot read {}: {e}",
                    file.display()
                )));
            }
        };
        let config = match &yaml {
            Some(text) => Config::from_yaml(text).map_err(|e| {
                Error::InvalidProject(format!("{} is invalid: {e}", file.display()))
            })?,
            None => default_pipeline(Path::new(&project.path)),
        };

        let title = params
            .title
            .filter(|t| !t.trim().is_empty())
            .or_else(|| prompt.as_deref().map(first_line))
            .or_else(|| params.issue.map(|n| format!("Issue #{n}")))
            .unwrap_or_default();
        let steps = step_kinds(&config);
        let now = now_ms();
        let record = RunRecord {
            run: Run {
                id: new_id(),
                project_id: project.id,
                title,
                prompt,
                issue: params.issue,
                branch: None,
                worktree: None,
                step: steps[0],
                steps,
                step_index: 0,
                status: RunStatus::Running,
                roles: config
                    .roles
                    .iter()
                    .map(|(name, role)| (name.clone(), role.harness.clone()))
                    .collect(),
                checks: Vec::new(),
                gate_attempt: 0,
                gate_max_attempts: first_limit(&config, StepKind::Gate),
                review_round: 0,
                review_max_rounds: first_limit(&config, StepKind::Review),
                budget_usd: config.budget.max_usd_per_run,
                started_at: now,
                updated_at: now,
                finished_at: None,
                pull_request: None,
                activity: "creating the worktree".into(),
                error: None,
                cost_usd: 0.0,
                sessions: Default::default(),
            },
            phase: Phase::Setup,
            loops: Default::default(),
            feedback: None,
            config_yaml: yaml,
            base_commit: None,
        };
        self.inner.store.write(|tx| tx.insert_run(&record))?;
        self.spawn(&record.run.id);
        Ok(record.run)
    }

    pub fn runs(&self, project_id: Option<&str>) -> Result<Vec<Run>> {
        let records = self.inner.store.read(|tx| tx.runs(project_id))?;
        Ok(records.into_iter().map(|r| r.run).collect())
    }

    pub fn run(&self, run_id: &str) -> Result<(Run, Vec<StepAttempt>, Vec<PermissionRequest>)> {
        self.inner.store.read(|tx| {
            let record = tx
                .run(run_id)?
                .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
            let mut requests = tx.requests(Some(run_id), None)?;
            requests.reverse();
            Ok((record.run, tx.attempts(run_id)?, requests))
        })
    }

    pub fn requests(&self, pending_only: bool) -> Result<Vec<PermissionRequest>> {
        let status = pending_only.then_some(RequestStatus::Pending);
        Ok(self.inner.store.read(|tx| tx.requests(None, status))?)
    }

    /// Sessions, oldest first, optionally only those of one run.
    pub fn sessions(&self, run_id: Option<&str>) -> Result<Vec<Session>> {
        Ok(self.inner.store.read(|tx| tx.sessions(run_id))?)
    }

    fn request_kind(&self, request_id: &str) -> Result<RequestKind> {
        self.inner
            .store
            .read(|tx| tx.request(request_id))?
            .map(|r| r.kind)
            .ok_or_else(|| Error::NotFound(format!("no request {request_id}")))
    }

    /// Approves a pending request. An agent waiting for permission gets to
    /// run its tool call; a paused run resumes.
    pub fn approve(&self, request_id: &str, answer: Option<&str>) -> Result<PermissionRequest> {
        match self.request_kind(request_id)? {
            RequestKind::Permission => return self.answer_permission(request_id, true, answer),
            RequestKind::Question => return self.answer_question(request_id, true, answer),
            _ => {}
        }
        let request = self.inner.store.write(|tx| {
            let (request, mut record) = pending_request(tx, request_id)?;
            let config = config_of(&record, &project_root(tx, &record)?)?;
            let request = tx.resolve_request(&request.id, RequestStatus::Approved, answer)?;
            record.run.status = RunStatus::Running;
            if request.kind == RequestKind::Budget {
                // Another round of the configured budget; the step that was
                // about to start runs.
                let raise = config
                    .budget
                    .max_usd_per_run
                    .or(record.run.budget_usd)
                    .unwrap_or(0.0);
                let budget = record.run.cost_usd + raise;
                record.run.budget_usd = Some(budget);
                record.phase = Phase::Step;
                record.run.activity = format!("budget raised to ${budget:.2}; continuing");
            } else {
                match config.pipeline.get(request.step_index) {
                    // Steps that ask before acting now get to act.
                    Some(Step::PullRequest { .. }) => {
                        record.phase = Phase::Approved;
                        record.run.activity = "approved; opening the pull request".into();
                    }
                    // Steps that ask after acting are done.
                    _ => advance(&mut record),
                }
            }
            tx.save_run(&mut record)?;
            Ok::<_, Error>(request)
        })?;
        self.spawn(&request.run_id);
        Ok(request)
    }

    /// Denies a pending request. An agent asking for permission is told no
    /// and carries on; any other denial fails the run.
    pub fn deny(&self, request_id: &str, answer: Option<&str>) -> Result<PermissionRequest> {
        match self.request_kind(request_id)? {
            RequestKind::Permission => return self.answer_permission(request_id, false, answer),
            RequestKind::Question => return self.answer_question(request_id, false, answer),
            _ => {}
        }
        let (request, run_id) = self.inner.store.write(|tx| {
            let (request, mut record) = pending_request(tx, request_id)?;
            let request = tx.resolve_request(&request.id, RequestStatus::Denied, answer)?;
            let what = if request.kind == RequestKind::Budget {
                format!(
                    "over budget (${:.2} spent, budget ${:.2}) and going on",
                    record.run.cost_usd,
                    record.run.budget_usd.unwrap_or(0.0)
                )
            } else {
                request.step.to_string()
            };
            let reason = match answer {
                Some(answer) => format!("{what} not approved: {answer}"),
                None => format!("{what} not approved"),
            };
            fail(&mut record, reason);
            end_sessions(tx, &record.run.id)?;
            tx.save_run(&mut record)?;
            Ok::<_, Error>((request, record.run.id))
        })?;
        self.release(&run_id);
        Ok(request)
    }

    /// Records the answer to a permission request and passes it to the agent.
    fn answer_permission(
        &self,
        request_id: &str,
        allow: bool,
        answer: Option<&str>,
    ) -> Result<PermissionRequest> {
        let request = self.inner.store.write(|tx| {
            let request = tx
                .request(request_id)?
                .ok_or_else(|| Error::NotFound(format!("no request {request_id}")))?;
            if request.status != RequestStatus::Pending {
                return Err(Error::Conflict(format!(
                    "request {request_id} is already {}",
                    request.status
                )));
            }
            let status = if allow {
                RequestStatus::Approved
            } else {
                RequestStatus::Denied
            };
            let request = tx.resolve_request(request_id, status, answer)?;
            if !allow {
                tx.log(
                    &request.run_id,
                    format!(
                        "permission denied: {}; the agent was told no",
                        request.title
                    ),
                )?;
            }
            resume_after_permission(tx, &request.run_id, request.session_id.as_deref())?;
            Ok(request)
        })?;
        if let Some(waiter) = self.waiters().remove(&request.id) {
            let _ = waiter.send(allow);
        }
        Ok(request)
    }

    /// Creates a permission request for the agent of `host`'s step and
    /// returns a future that resolves with the human's answer.
    fn ask_permission(
        &self,
        host: &RunHost,
        session_id: &str,
        title: String,
        detail: String,
    ) -> BoxFuture<'static, bool> {
        if self.inner.settings.auto_approve_permissions {
            let logged = self.inner.store.write(|tx| {
                tx.log(
                    &host.run_id,
                    format!("permission auto-approved (--auto-approve-permissions): {title}"),
                )
            });
            if let Err(e) = logged {
                eprintln!("agentuxd: run {}: {e}", host.run_id);
            }
            return Box::pin(async { true });
        }
        let (answer, answered) = oneshot::channel();
        // Held until the waiter is registered, so an answer committed in
        // between finds it.
        let mut waiters = self.waiters();
        let created = self.inner.store.write(|tx| {
            let Some(mut record) = tx.run(&host.run_id)? else {
                return Ok(None);
            };
            if record.run.status.is_terminal() {
                return Ok(None);
            }
            let request = tx.create_request(NewRequest {
                kind: RequestKind::Permission,
                run_id: host.run_id.clone(),
                project_id: host.project_id.clone(),
                step_index: host.step_index,
                step: host.step,
                title,
                detail,
                session_id: Some(session_id.to_string()),
                options: Vec::new(),
            })?;
            tx.emit(
                Some(&host.run_id),
                EventBody::SessionEvent {
                    session_id: session_id.to_string(),
                    event: SessionEvent::Permission {
                        request_id: request.id.clone(),
                    },
                },
            )?;
            set_session_state(tx, session_id, SessionState::Waiting)?;
            record.run.status = RunStatus::Waiting;
            record.run.activity =
                format!("waiting for permission ({}): {}", request.id, request.title);
            tx.save_run(&mut record)?;
            Ok::<_, Error>(Some(request.id))
        });
        let id = match created {
            Ok(Some(id)) => id,
            Ok(None) => return Box::pin(async { false }),
            Err(e) => {
                eprintln!(
                    "agentuxd: run {}: cannot record a permission request: {e}",
                    host.run_id
                );
                return Box::pin(async { false });
            }
        };
        waiters.insert(id.clone(), answer);
        drop(waiters);
        let mut pending = PendingPermission {
            engine: self.clone(),
            request_id: Some(id),
        };
        Box::pin(async move {
            // A dropped sender (cancelled run) is a refusal.
            let allow = answered.await.unwrap_or(false);
            pending.answered();
            allow
        })
    }

    /// Cancels pending permission requests of a run (or just `only`): the
    /// agent stopped waiting, or its step ended.
    fn withdraw_permissions(&self, run_id: &str, only: Option<&str>) {
        let withdrawn = self.inner.store.write(|tx| {
            let mut ids = Vec::new();
            for request in tx.requests(Some(run_id), Some(RequestStatus::Pending))? {
                if request.kind != RequestKind::Permission
                    || only.is_some_and(|id| id != request.id)
                {
                    continue;
                }
                tx.resolve_request(
                    &request.id,
                    RequestStatus::Cancelled,
                    Some("withdrawn: the agent stopped waiting"),
                )?;
                resume_after_permission(tx, run_id, request.session_id.as_deref())?;
                ids.push(request.id);
            }
            Ok::<_, Error>(ids)
        });
        match withdrawn {
            Ok(ids) => {
                let mut waiters = self.waiters();
                for id in ids {
                    waiters.remove(&id);
                }
            }
            Err(e) => eprintln!("agentuxd: run {run_id}: cannot withdraw permission requests: {e}"),
        }
    }

    fn waiters(&self) -> MutexGuard<'_, HashMap<String, oneshot::Sender<bool>>> {
        self.inner.waiters.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lets the executor stop what it keeps for a finished run, and closes
    /// the run's bus.
    fn release(&self, run_id: &str) {
        self.close_bus(run_id);
        self.inner.terminals.close_run(run_id);
        let executor = Arc::clone(&self.inner.executor);
        let run_id = run_id.to_string();
        tokio::spawn(async move { executor.release(&run_id).await });
    }

    /// Cancels a run that has not finished. Its worktree and branch are kept.
    pub fn cancel(&self, run_id: &str) -> Result<Run> {
        let (run, cancelled) = self.inner.store.write(|tx| {
            let mut record = tx
                .run(run_id)?
                .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
            if record.run.status.is_terminal() {
                return Err(Error::Conflict(format!(
                    "run {run_id} is already {}",
                    record.run.status
                )));
            }
            let mut cancelled = Vec::new();
            for request in tx.requests(Some(run_id), Some(RequestStatus::Pending))? {
                tx.resolve_request(&request.id, RequestStatus::Cancelled, None)?;
                cancelled.push(request.id);
            }
            for attempt in tx.attempts(run_id)? {
                if attempt.status == AttemptStatus::Running {
                    tx.finish_attempt(attempt.id, AttemptStatus::Cancelled, None)?;
                }
            }
            end_sessions(tx, run_id)?;
            record.run.status = RunStatus::Cancelled;
            record.run.finished_at = Some(now_ms());
            record.run.activity = "cancelled".into();
            tx.save_run(&mut record)?;
            Ok((record.run, cancelled))
        })?;
        // The cancellation is committed first; aborting the driver also kills
        // a running check (`kill_on_drop`). Agents waiting for permission get
        // a refusal as their waiters are dropped.
        if let Some(task) = self.tasks().remove(run_id) {
            task.abort();
        }
        {
            let mut waiters = self.waiters();
            for id in &cancelled {
                waiters.remove(id);
            }
        }
        self.release(run_id);
        Ok(run)
    }

    // ---- driving runs ----

    fn tasks(&self) -> MutexGuard<'_, HashMap<String, JoinHandle<()>>> {
        self.inner.tasks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the driver of a run unless it already has one.
    fn spawn(&self, run_id: &str) {
        let mut tasks = self.tasks();
        if tasks.contains_key(run_id) {
            return;
        }
        let engine = self.clone();
        let id = run_id.to_string();
        tasks.insert(run_id.to_string(), tokio::spawn(engine.drive(id)));
    }

    async fn drive(self, run_id: String) {
        loop {
            match self.step(&run_id).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    let failed = self.inner.store.write(|tx| {
                        let Some(mut record) = tx.run(&run_id)? else {
                            return Ok(());
                        };
                        if !record.run.status.is_terminal() {
                            fail(&mut record, e.to_string());
                            tx.save_run(&mut record)?;
                        }
                        Ok::<_, Error>(())
                    });
                    if let Err(e) = failed {
                        eprintln!("agentuxd: run {run_id}: cannot record failure: {e}");
                    }
                }
            }
            // Decide to stop while holding the task table, so that an approval
            // committed meanwhile either is seen here or finds no driver and
            // spawns a new one.
            let mut tasks = self.tasks();
            let status = self
                .inner
                .store
                .read(|tx| tx.run(&run_id))
                .ok()
                .flatten()
                .map(|r| r.run.status);
            if status != Some(RunStatus::Running) {
                tasks.remove(&run_id);
                drop(tasks);
                if status.is_none_or(RunStatus::is_terminal) {
                    let ended = self.inner.store.write(|tx| end_sessions(tx, &run_id));
                    if let Err(e) = ended {
                        eprintln!("agentuxd: run {run_id}: cannot end its sessions: {e}");
                    }
                    self.release(&run_id);
                }
                return;
            }
        }
    }

    /// Commits `f`'s changes to the run, unless the run stopped running
    /// meanwhile (cancelled, failed). `None` means it did.
    fn transition<T>(
        &self,
        run_id: &str,
        f: impl FnOnce(&mut Tx<'_>, &mut RunRecord) -> Result<T>,
    ) -> Result<Option<T>> {
        self.inner.store.write(|tx| {
            let Some(mut record) = tx.run(run_id)? else {
                return Ok(None);
            };
            if record.run.status != RunStatus::Running {
                return Ok(None);
            }
            let value = f(tx, &mut record)?;
            tx.save_run(&mut record)?;
            Ok(Some(value))
        })
    }

    /// Advances the run by one transition. `Ok(false)` when there is nothing
    /// to do (finished, waiting or cancelled).
    async fn step(&self, run_id: &str) -> Result<bool> {
        let Some(record) = self.inner.store.read(|tx| tx.run(run_id))? else {
            return Ok(false);
        };
        if record.run.status != RunStatus::Running {
            return Ok(false);
        }
        let root = self.inner.store.read(|tx| project_root(tx, &record))?;
        let config = config_of(&record, &root)?;
        let index = record.run.step_index;
        match record.phase {
            Phase::Setup => self.setup(record).await,
            Phase::Approval => Err(Error::Internal(
                "inconsistent state: running while waiting for approval".into(),
            )),
            Phase::Step | Phase::Approved => match &config.pipeline[index] {
                Step::Gate { checks, on_fail } => {
                    self.gate(record, &config, checks, *on_fail).await
                }
                Step::PullRequest { approve: true, .. } if record.phase == Phase::Step => {
                    self.ask_before_pull_request(run_id)
                }
                Step::PullRequest { draft, .. } => self.pull_request(record, *draft).await,
                step => self.agent(record, &config, step).await,
            },
        }
    }

    async fn setup(&self, record: RunRecord) -> Result<bool> {
        let run_id = record.run.id.clone();
        let project = self
            .inner
            .store
            .read(|tx| tx.project(&record.run.project_id))?
            .ok_or_else(|| {
                Error::Internal(format!("project {} vanished", record.run.project_id))
            })?;
        let id = run_id.clone();
        // Idempotent: a worktree left by an interrupted setup is reused.
        let worktree = tokio::task::spawn_blocking(move || {
            let worktrees = Worktrees::open(Path::new(&project.path))?;
            match worktrees.find(&id)? {
                Some(worktree) => Ok(worktree),
                None => worktrees.create(&id, "HEAD"),
            }
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
        .map_err(|e| Error::Internal(format!("cannot create the run's worktree: {e}")))?;
        // What reviewers diff against. The worktree is fresh here (or reused
        // from an interrupted setup, before any step changed it).
        let base = match record.base_commit {
            Some(_) => None,
            None => git(&worktree.path, &["rev-parse", "HEAD"])
                .await
                .ok()
                .map(|sha| sha.trim().to_string()),
        };

        let done = self.transition(&run_id, |tx, record| {
            if base.is_some() {
                record.base_commit = base.clone();
            }
            let path = worktree.path.to_string_lossy().into_owned();
            tx.log(
                &run_id,
                format!("worktree {path} on branch {}", worktree.branch),
            )?;
            record.run.branch = Some(worktree.branch.clone());
            record.run.worktree = Some(path);
            record.phase = Phase::Step;
            record.run.activity = format!("starting {}", record.run.step);
            Ok(())
        })?;
        Ok(done.is_some())
    }

    async fn agent(&self, record: RunRecord, config: &Config, step: &Step) -> Result<bool> {
        let run_id = record.run.id.clone();
        let index = record.run.step_index;
        let kind = kind_of(step);
        let (role_name, instructions, approve) = match step {
            Step::Plan { role, approve }
            | Step::Implement { role, approve }
            | Step::Review { role, approve, .. } => (role, None, *approve),
            Step::Custom {
                role,
                prompt,
                approve,
            } => (role, Some(prompt.clone()), *approve),
            Step::Gate { .. } | Step::PullRequest { .. } => unreachable!("not an agent step"),
        };
        let role = &config.roles[role_name];

        // Spending is checked between agent steps: a run over its budget
        // pauses before the next agent starts.
        if let Some(budget) = record.run.budget_usd
            && record.run.cost_usd > budget
        {
            let raise = config.budget.max_usd_per_run.unwrap_or(budget);
            let done = self.transition(&run_id, |tx, record| {
                let detail = format!(
                    "The run has cost ${:.2}, more than its budget of ${budget:.2}. \
                     Approve to continue with another ${raise:.2}; deny to stop the run.",
                    record.run.cost_usd
                );
                ask(tx, record, RequestKind::Budget, detail)
            })?;
            return Ok(done.is_some());
        }

        let Some((attempt, number, plan, feedback_from)) =
            self.transition(&run_id, |tx, record| {
                let attempts = tx.attempts(&run_id)?;
                let number = attempts.iter().filter(|a| a.step_index == index).count() as u32 + 1;
                let plan = latest_output(&attempts, StepKind::Plan);
                // The gate or review that sent the run back here.
                let feedback_from = record.feedback.as_ref().and_then(|_| {
                    attempts
                        .iter()
                        .rev()
                        .find(|a| {
                            matches!(
                                a.status,
                                AttemptStatus::Failed | AttemptStatus::ChangesRequested
                            )
                        })
                        .map(|a| a.step)
                });
                let attempt = tx.start_attempt(&run_id, index, kind)?;
                record.run.activity = format!("{kind}: {role_name} ({}) is working", role.harness);
                Ok((attempt, number, plan, feedback_from))
            })?
        else {
            return Ok(false);
        };

        let worktree = worktree_of(&record)?;
        let task = AgentTask {
            run_id: run_id.clone(),
            title: record.run.title.clone(),
            step_index: index,
            step: kind,
            role: role_name.clone(),
            harness: role.harness.clone(),
            model: role.model.clone(),
            prompt: record.run.prompt.clone(),
            issue: record.run.issue,
            instructions,
            plan,
            feedback: record.feedback.clone(),
            feedback_from,
            branch: record.run.branch.clone(),
            base_commit: record.base_commit.clone(),
            worktree: worktree.clone(),
            attempt: number,
        };
        let host = Arc::new(RunHost {
            engine: self.clone(),
            run_id: run_id.clone(),
            project_id: record.run.project_id.clone(),
            role: role_name.clone(),
            step_index: index,
            step: kind,
            cwd: worktree.to_string_lossy().into_owned(),
        });
        let outcome = self.inner.executor.run_agent(&task, host).await;
        // Questions the agent left open die with its turn.
        self.withdraw_permissions(&run_id, None);
        // Implementers (and custom steps) edit; the daemon commits. Planners
        // and reviewers are not meant to change files: what they leave behind
        // (e.g. caches from running tests) is not committed under their name.
        let commits = matches!(kind, StepKind::Implement | StepKind::Custom);
        let (outcome, commit) = match outcome {
            Ok(outcome) if !commits => (Ok(outcome), None),
            Ok(outcome) => match commit_changes(&task).await {
                Ok(commit) => (Ok(outcome), commit),
                Err(e) => (Err(format!("cannot commit the agent's changes: {e}")), None),
            },
            Err(e) => (Err(e), None),
        };

        let changes_loop = match step {
            Step::Review {
                on_changes_requested,
                ..
            } => *on_changes_requested,
            _ => None,
        };
        let done = self.transition(&run_id, |tx, record| {
            if let Some(commit) = &commit {
                tx.log(&run_id, format!("committed {commit}"))?;
            }
            if kind == StepKind::Review {
                let round = record.loops.get(&index).copied().unwrap_or(0) + 1;
                record.loops.insert(index, round);
                record.run.review_round = round;
                record.run.review_max_rounds = changes_loop.map_or(1, |l| l.limit);
            }
            match outcome {
                Err(message) => {
                    tx.finish_attempt(attempt.id, AttemptStatus::Failed, Some(&message))?;
                    fail(record, format!("{kind} step failed: {message}"));
                }
                Ok(AgentOutcome::Done { summary }) => {
                    tx.finish_attempt(attempt.id, AttemptStatus::Succeeded, Some(&summary))?;
                    tx.log(&run_id, format!("{kind}: {summary}"))?;
                    record.feedback = None;
                    if approve {
                        ask(tx, record, step_request_kind(kind), summary)?;
                    } else {
                        advance(record);
                    }
                }
                Ok(AgentOutcome::ChangesRequested { comments }) if kind == StepKind::Review => {
                    tx.finish_attempt(
                        attempt.id,
                        AttemptStatus::ChangesRequested,
                        Some(&comments),
                    )?;
                    tx.log(&run_id, format!("review requested changes: {comments}"))?;
                    let round = record.run.review_round;
                    match changes_loop {
                        Some(Loop { target, limit }) if round < limit => {
                            record.feedback = Some(comments);
                            go_to(record, target);
                            record.run.activity = format!(
                                "review round {round}/{limit} requested changes; back to {}",
                                record.run.step
                            );
                        }
                        Some(Loop { limit, .. }) => fail(
                            record,
                            format!(
                                "review still requests changes after {round} of {limit} rounds"
                            ),
                        ),
                        None => fail(record, "review requested changes".into()),
                    }
                }
                Ok(AgentOutcome::ChangesRequested { comments }) => {
                    tx.finish_attempt(attempt.id, AttemptStatus::Failed, Some(&comments))?;
                    fail(
                        record,
                        format!("{kind} step asked for changes, which only review steps can do"),
                    );
                }
            }
            Ok(())
        })?;
        Ok(done.is_some())
    }

    async fn gate(
        &self,
        record: RunRecord,
        config: &Config,
        names: &[String],
        on_fail: Option<Loop>,
    ) -> Result<bool> {
        let run_id = record.run.id.clone();
        let index = record.run.step_index;
        let attempt_number = record.loops.get(&index).copied().unwrap_or(0) + 1;
        let limit = on_fail.map_or(1, |l| l.limit);
        let checks: Vec<(String, String)> = names
            .iter()
            .filter_map(|name| config.checks.iter().find(|c| &c.name == name))
            .map(|c| (c.name.clone(), c.run.clone()))
            .collect();
        let worktree = worktree_of(&record)?;

        let Some(attempt) = self.transition(&run_id, |tx, record| {
            let attempt = tx.start_attempt(&run_id, index, StepKind::Gate)?;
            record.run.gate_attempt = attempt_number;
            record.run.gate_max_attempts = limit;
            record.run.checks = checks
                .iter()
                .map(|(name, command)| CheckResult {
                    name: name.clone(),
                    command: command.clone(),
                    status: CheckStatus::Pending,
                })
                .collect();
            record.run.activity = format!("gate: attempt {attempt_number}/{limit}");
            Ok(attempt)
        })?
        else {
            return Ok(false);
        };

        let mut failures = String::new();
        for (i, (name, command)) in checks.iter().enumerate() {
            let started = self.transition(&run_id, |_, record| {
                record.run.checks[i].status = CheckStatus::Running;
                record.run.activity = format!("gate: running {name}");
                Ok(())
            })?;
            if started.is_none() {
                return Ok(false);
            }
            let (passed, output) = run_check(&worktree, command).await;
            let recorded = self.transition(&run_id, |tx, record| {
                record.run.checks[i].status = if passed {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed
                };
                let verdict = if passed { "passed" } else { "failed" };
                tx.log(&run_id, format!("check {name} {verdict}\n{output}"))?;
                Ok(())
            })?;
            if recorded.is_none() {
                return Ok(false);
            }
            if !passed {
                failures.push_str(&format!("$ {command}\n{output}\n"));
            }
        }

        let done = self.transition(&run_id, |tx, record| {
            if failures.is_empty() {
                tx.finish_attempt(
                    attempt.id,
                    AttemptStatus::Succeeded,
                    Some("all checks passed"),
                )?;
                record.loops.remove(&index);
                advance(record);
                return Ok(());
            }
            tx.finish_attempt(attempt.id, AttemptStatus::Failed, Some(&failures))?;
            match on_fail {
                Some(Loop { target, limit }) if attempt_number < limit => {
                    record.loops.insert(index, attempt_number);
                    record.feedback = Some(failures);
                    go_to(record, target);
                    record.run.activity = format!(
                        "gate failed (attempt {attempt_number}/{limit}); back to {}",
                        record.run.step
                    );
                }
                _ => fail(
                    record,
                    format!("gate failed after {attempt_number} of {limit} attempts"),
                ),
            }
            Ok(())
        })?;
        Ok(done.is_some())
    }

    fn ask_before_pull_request(&self, run_id: &str) -> Result<bool> {
        let done = self.transition(run_id, |tx, record| {
            let detail = format!(
                "Open a pull request from {} for \"{}\"",
                record.run.branch.as_deref().unwrap_or("?"),
                record.run.title
            );
            ask(tx, record, RequestKind::Step, detail)
        })?;
        Ok(done.is_some())
    }

    async fn pull_request(&self, record: RunRecord, draft: bool) -> Result<bool> {
        let run_id = record.run.id.clone();
        let index = record.run.step_index;
        let Some(attempt) = self.transition(&run_id, |tx, record| {
            record.run.activity = "opening the pull request".into();
            Ok(tx.start_attempt(&run_id, index, StepKind::PullRequest)?)
        })?
        else {
            return Ok(false);
        };
        let attempts = self.inner.store.read(|tx| tx.attempts(&run_id))?;
        let task = PullRequestTask {
            run_id: run_id.clone(),
            title: record.run.title.clone(),
            branch: record.run.branch.clone().unwrap_or_default(),
            worktree: worktree_of(&record)?,
            draft,
            prompt: record.run.prompt.clone(),
            issue: record.run.issue,
            plan: latest_output(&attempts, StepKind::Plan),
            review: latest_output(&attempts, StepKind::Review),
        };
        let result = self.inner.executor.open_pull_request(&task).await;
        let done = self.transition(&run_id, |tx, record| {
            match result {
                Ok(PullRequestOutcome::Skipped(reason)) => {
                    let note = format!("PR skipped: {reason}");
                    tx.finish_attempt(attempt.id, AttemptStatus::Succeeded, Some(&note))?;
                    tx.log(
                        &run_id,
                        format!("{note}; the branch {} is kept", task.branch),
                    )?;
                    advance(record);
                }
                Ok(PullRequestOutcome::Opened(pr)) => {
                    tx.finish_attempt(attempt.id, AttemptStatus::Succeeded, Some(&pr.url))?;
                    tx.log(&run_id, format!("pull request #{} {}", pr.number, pr.url))?;
                    record.run.pull_request = Some(pr);
                    advance(record);
                }
                Err(message) => {
                    tx.finish_attempt(attempt.id, AttemptStatus::Failed, Some(&message))?;
                    fail(record, format!("cannot open the pull request: {message}"));
                }
            }
            Ok(())
        })?;
        Ok(done.is_some())
    }
}

// ---- pure state changes ----

/// Moves past the current step; past the last one, the run is done.
fn advance(record: &mut RunRecord) {
    let next = record.run.step_index + 1;
    if next < record.run.steps.len() {
        go_to(record, next);
        record.run.activity = format!("starting {}", record.run.step);
    } else {
        record.phase = Phase::Step;
        record.run.status = RunStatus::Done;
        record.run.finished_at = Some(now_ms());
        record.run.activity = "done".into();
    }
}

fn go_to(record: &mut RunRecord, index: usize) {
    record.run.step_index = index;
    record.run.step = record.run.steps[index];
    record.phase = Phase::Step;
}

fn fail(record: &mut RunRecord, reason: String) {
    record.run.status = RunStatus::Failed;
    record.run.finished_at = Some(now_ms());
    record.run.activity = reason.lines().next().unwrap_or_default().to_string();
    record.run.error = Some(reason);
}

/// Pauses the run on a new approval request about the current step.
fn ask(tx: &mut Tx<'_>, record: &mut RunRecord, kind: RequestKind, detail: String) -> Result<()> {
    let step = record.run.step;
    let title = match kind {
        RequestKind::Plan => format!("Approve the plan for \"{}\"", record.run.title),
        RequestKind::Step => format!("Approve the {step} step of \"{}\"", record.run.title),
        RequestKind::Budget => format!("\"{}\" is over its budget", record.run.title),
        RequestKind::Permission => format!("Permission for \"{}\"", record.run.title),
        RequestKind::Question => format!("A question about \"{}\"", record.run.title),
    };
    let request = tx.create_request(NewRequest {
        kind,
        run_id: record.run.id.clone(),
        project_id: record.run.project_id.clone(),
        step_index: record.run.step_index,
        step,
        title,
        detail,
        session_id: None,
        options: Vec::new(),
    })?;
    record.run.status = RunStatus::Waiting;
    record.phase = Phase::Approval;
    record.run.activity = format!("waiting for approval ({})", request.id);
    Ok(())
}

fn pending_request(tx: &Tx<'_>, request_id: &str) -> Result<(PermissionRequest, RunRecord)> {
    let request = tx
        .request(request_id)?
        .ok_or_else(|| Error::NotFound(format!("no request {request_id}")))?;
    if request.status != RequestStatus::Pending {
        return Err(Error::Conflict(format!(
            "request {request_id} is already {}",
            request.status
        )));
    }
    let record = tx
        .run(&request.run_id)?
        .ok_or_else(|| Error::Internal(format!("run {} vanished", request.run_id)))?;
    if record.run.status != RunStatus::Waiting || record.phase != Phase::Approval {
        return Err(Error::Conflict(format!(
            "run {} is not waiting for approval",
            record.run.id
        )));
    }
    Ok((request, record))
}

/// The pipeline the run started with.
pub(crate) fn config_of(record: &RunRecord, project_root: &Path) -> Result<Config> {
    let config = match &record.config_yaml {
        Some(yaml) => Config::from_yaml(yaml)
            .map_err(|e| Error::Internal(format!("the run's stored pipeline is invalid: {e}")))?,
        None => default_pipeline(project_root),
    };
    // Runs without agentux.yaml rebuild the built-in default from the project
    // each time. If that changed the steps mid-run (a daemon upgrade, or the
    // default's detected checks appearing or vanishing), stop rather than run
    // steps out of order.
    if step_kinds(&config) != record.run.steps {
        return Err(Error::Internal(
            "the built-in default pipeline changed since this run started".into(),
        ));
    }
    Ok(config)
}

/// The built-in pipeline for a project without `agentux.yaml`.
fn default_pipeline(project_root: &Path) -> Config {
    Config::default_for(project_root)
}

pub(crate) fn project_root(tx: &Tx<'_>, record: &RunRecord) -> Result<PathBuf> {
    tx.project(&record.run.project_id)?
        .map(|p| PathBuf::from(p.path))
        .ok_or_else(|| Error::Internal(format!("project {} vanished", record.run.project_id)))
}

fn worktree_of(record: &RunRecord) -> Result<PathBuf> {
    record
        .run
        .worktree
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Internal("the run has no worktree".into()))
}

fn kind_of(step: &Step) -> StepKind {
    StepKind::parse(step.kind().as_str()).expect("config and API step kinds match")
}

fn step_kinds(config: &Config) -> Vec<StepKind> {
    config.pipeline.iter().map(kind_of).collect()
}

fn step_request_kind(kind: StepKind) -> RequestKind {
    if kind == StepKind::Plan {
        RequestKind::Plan
    } else {
        RequestKind::Step
    }
}

/// The loop limit of the first step of `kind`, for display before it runs.
fn first_limit(config: &Config, kind: StepKind) -> u32 {
    config
        .pipeline
        .iter()
        .find_map(|step| match step {
            Step::Gate { on_fail, .. } if kind == StepKind::Gate => {
                Some(on_fail.map_or(1, |l| l.limit))
            }
            Step::Review {
                on_changes_requested,
                ..
            } if kind == StepKind::Review => Some(on_changes_requested.map_or(1, |l| l.limit)),
            _ => None,
        })
        .unwrap_or(0)
}

fn first_line(text: &str) -> String {
    cut_line(text, 80)
}

/// The first line of `text`, cut to `max` characters.
fn cut_line(text: &str, max: usize) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// Output of the most recent successful attempt of a step kind.
fn latest_output(attempts: &[StepAttempt], kind: StepKind) -> Option<String> {
    attempts
        .iter()
        .rev()
        .find(|a| a.step == kind && a.status == AttemptStatus::Succeeded)
        .and_then(|a| a.output.clone())
}

// ---- agent steps: the host, permissions, sessions and commits ----

/// The engine's side of one agent step (or bus wake), handed to the
/// executor.
pub(crate) struct RunHost {
    pub(crate) engine: Engine,
    pub(crate) run_id: String,
    pub(crate) project_id: String,
    pub(crate) role: String,
    /// The run's step when the turn started.
    pub(crate) step_index: usize,
    pub(crate) step: StepKind,
    pub(crate) cwd: String,
}

impl RunHost {
    fn record(&self, what: &str, f: impl FnOnce(&mut Tx<'_>) -> Result<()>) {
        if let Err(e) = self.engine.inner.store.write(f) {
            eprintln!("agentuxd: run {}: cannot record {what}: {e}", self.run_id);
        }
    }
}

impl StepHost for RunHost {
    fn open_session(
        &self,
        harness: &str,
        model: Option<&str>,
    ) -> std::result::Result<OpenedSession, String> {
        let now = now_ms();
        let session = Session {
            id: new_id(),
            run_id: self.run_id.clone(),
            project_id: self.project_id.clone(),
            role: self.role.clone(),
            harness: harness.to_string(),
            model: model.map(str::to_string),
            state: SessionState::Active,
            cwd: self.cwd.clone(),
            usage: Default::default(),
            started_at: now,
            updated_at: now,
            ended_at: None,
            vendor_session_id: None,
        };
        self.engine
            .inner
            .store
            .write(|tx| {
                tx.insert_session(&session)?;
                if let Some(mut record) = tx.run(&self.run_id)? {
                    record
                        .run
                        .sessions
                        .insert(self.role.clone(), session.id.clone());
                    tx.save_run(&mut record)?;
                }
                Ok::<_, Error>(())
            })
            .map_err(|e| e.to_string())?;
        // A session without the bus still works; say why it has none.
        let (mcp_servers, system_prompt) = self
            .engine
            .bus_attach(&self.run_id, &self.role, harness, &session.id)
            .unwrap_or_else(|e| {
                self.record("a bus failure", |tx| {
                    Ok(tx.log(
                        &self.run_id,
                        format!("bus: session {} is not on the bus: {e}", session.id),
                    )?)
                });
                (Vec::new(), None)
            });
        Ok(OpenedSession {
            id: session.id,
            mcp_servers,
            system_prompt,
        })
    }

    fn session_state(&self, session_id: &str, state: SessionState) {
        self.record("a session state", |tx| {
            set_session_state(tx, session_id, state)
        });
        if state == SessionState::Ended {
            self.engine.bus_leave(session_id);
        }
    }

    fn session_vendor_id(&self, session_id: &str, vendor_session_id: &str) {
        self.record("a session's vendor id", |tx| {
            if let Some(mut session) = tx.session(session_id)?
                && session.vendor_session_id.as_deref() != Some(vendor_session_id)
            {
                session.vendor_session_id = Some(vendor_session_id.to_string());
                tx.save_session(&mut session)?;
            }
            Ok(())
        });
    }

    fn session_event(&self, session_id: &str, event: SessionEvent) {
        self.record("a session event", |tx| {
            if let SessionEvent::Usage { usage } = &event
                && let Some(mut session) = tx.session(session_id)?
            {
                session.usage = usage.clone();
                tx.save_session(&mut session)?;
                if let Some(mut record) = tx.run(&self.run_id)? {
                    // Harnesses report each session's cumulative cost.
                    let cost: f64 = tx
                        .sessions(Some(&self.run_id))?
                        .iter()
                        .filter_map(|s| s.usage.cost_usd)
                        .sum();
                    if cost != record.run.cost_usd {
                        record.run.cost_usd = cost;
                        tx.save_run(&mut record)?;
                    }
                }
            }
            tx.emit(
                Some(&self.run_id),
                EventBody::SessionEvent {
                    session_id: session_id.to_string(),
                    event,
                },
            )?;
            Ok(())
        });
    }

    fn ask_permission(
        &self,
        session_id: &str,
        title: String,
        detail: String,
    ) -> BoxFuture<'static, bool> {
        self.engine.ask_permission(self, session_id, title, detail)
    }
}

/// Withdraws its permission request if the agent stops waiting before the
/// answer.
struct PendingPermission {
    engine: Engine,
    request_id: Option<String>,
}

impl PendingPermission {
    fn answered(&mut self) {
        self.request_id = None;
    }
}

impl Drop for PendingPermission {
    fn drop(&mut self) {
        if let Some(id) = self.request_id.take() {
            let run_id = self
                .engine
                .inner
                .store
                .read(|tx| tx.request(&id))
                .ok()
                .flatten()
                .map(|r| r.run_id);
            if let Some(run_id) = run_id {
                self.engine.withdraw_permissions(&run_id, Some(&id));
            }
        }
    }
}

/// After a permission request is resolved: the session works again, and the
/// run runs again once nothing else is pending.
fn resume_after_permission(tx: &mut Tx<'_>, run_id: &str, session_id: Option<&str>) -> Result<()> {
    // Questions to the human do not pause the run.
    let pending: Vec<PermissionRequest> = tx
        .requests(Some(run_id), Some(RequestStatus::Pending))?
        .into_iter()
        .filter(|r| r.kind != RequestKind::Question)
        .collect();
    if let Some(session_id) = session_id
        && !pending
            .iter()
            .any(|r| r.session_id.as_deref() == Some(session_id))
        && tx
            .session(session_id)?
            .is_some_and(|s| s.state == SessionState::Waiting)
    {
        set_session_state(tx, session_id, SessionState::Active)?;
    }
    if let Some(mut record) = tx.run(run_id)?
        && record.run.status == RunStatus::Waiting
        && record.phase == Phase::Step
        && pending.is_empty()
    {
        record.run.status = RunStatus::Running;
        record.run.activity = format!("{}: the agent is working", record.run.step);
        tx.save_run(&mut record)?;
    }
    Ok(())
}

fn set_session_state(tx: &mut Tx<'_>, session_id: &str, state: SessionState) -> Result<()> {
    if let Some(mut session) = tx.session(session_id)?
        && session.state != state
        && session.state != SessionState::Ended
    {
        session.state = state;
        if state == SessionState::Ended {
            session.ended_at = Some(now_ms());
        }
        tx.save_session(&mut session)?;
    }
    Ok(())
}

/// Marks every session of a run ended.
fn end_sessions(tx: &mut Tx<'_>, run_id: &str) -> Result<()> {
    for session in tx.sessions(Some(run_id))? {
        set_session_state(tx, &session.id, SessionState::Ended)?;
    }
    Ok(())
}

/// Commits everything that changed in the worktree since the last commit.
/// Returns `<short sha> <subject>`, or `None` when nothing changed.
async fn commit_changes(task: &AgentTask) -> std::result::Result<Option<String>, String> {
    let dir = &task.worktree;
    if git(dir, &["status", "--porcelain"])
        .await?
        .trim()
        .is_empty()
    {
        return Ok(None);
    }
    git(dir, &["add", "--all"]).await?;
    let (subject, body) = commit_message(task);
    let mut args: Vec<&str> = Vec::new();
    // A daemon cannot ask for an identity; use a neutral one if git has none.
    if !git(dir, &["config", "user.email"])
        .await
        .is_ok_and(|email| !email.trim().is_empty())
    {
        args.extend([
            "-c",
            "user.name=AgentUX",
            "-c",
            "user.email=agentux@localhost",
        ]);
    }
    args.extend(["commit", "--quiet", "-m", &subject, "-m", &body]);
    git(dir, &args).await?;
    let sha = git(dir, &["rev-parse", "--short", "HEAD"]).await?;
    Ok(Some(format!("{} {subject}", sha.trim())))
}

/// Subject and body of the commit after an agent step.
fn commit_message(task: &AgentTask) -> (String, String) {
    let title = &task.title;
    let subject = match (task.step, task.feedback_from) {
        (StepKind::Implement, Some(StepKind::Gate)) => format!("Fix failing checks: {title}"),
        (StepKind::Implement, Some(StepKind::Review)) => {
            format!("Address review comments: {title}")
        }
        (StepKind::Implement, _) => title.clone(),
        (kind, _) => format!("{title} ({kind} step)"),
    };
    let mut body = format!(
        "Changes made by the {} ({}) in AgentUX run {}, {} step, attempt {}.",
        task.role, task.harness, task.run_id, task.step, task.attempt
    );
    if let Some(prompt) = &task.prompt {
        body.push_str(&format!("\n\nRequest: {}", cut_line(prompt, 200)));
    }
    if let Some(issue) = task.issue {
        body.push_str(&format!("\n\nRefs #{issue}"));
    }
    (cut_line(&subject, 72), body)
}

/// Runs git in `dir` and returns its stdout.
async fn git(dir: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("cannot run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Runs one check command with `sh -c` in the worktree. Returns whether it
/// passed and the tail of its stdout and stderr.
async fn run_check(dir: &Path, command: &str) -> (bool, String) {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.success(), tail(text.trim_end()))
        }
        Err(e) => (false, format!("cannot run `sh -c {command}`: {e}")),
    }
}

fn tail(text: &str) -> String {
    if text.len() <= MAX_CHECK_OUTPUT {
        return text.to_string();
    }
    let mut start = text.len() - MAX_CHECK_OUTPUT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…]{}", &text[start..])
}
