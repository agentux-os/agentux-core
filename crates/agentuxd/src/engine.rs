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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::{fmt, fs, io};

use agentux_api::rpc::StartRun;
use agentux_api::{
    AttemptStatus, CheckResult, CheckStatus, PermissionRequest, Project, RequestKind,
    RequestStatus, Run, RunStatus, StepAttempt, StepKind,
};
use agentux_config::{Config, FILE_NAME, Loop, Step};
use agentux_store::{NewRequest, Phase, RunRecord, Store, Tx, new_id, now_ms};
use agentux_worktree::Worktrees;
use tokio::task::JoinHandle;

use crate::executor::{AgentOutcome, AgentTask, PullRequestTask, StepExecutor};

/// Keep at most this much of each check's output.
const MAX_CHECK_OUTPUT: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NotFound(String),
    /// The object is not in a state that allows the operation.
    Conflict(String),
    InvalidProject(String),
    InvalidParams(String),
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(m)
            | Self::Conflict(m)
            | Self::InvalidProject(m)
            | Self::InvalidParams(m)
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

/// Owns the store, the executor and one driver task per active run. Cheap to
/// clone.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    executor: Arc<dyn StepExecutor>,
    /// Driver task per run. A run has at most one.
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Engine {
    pub fn new(store: Store, executor: Arc<dyn StepExecutor>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                executor,
                tasks: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// Picks up every unfinished run after a (re)start. Attempts left
    /// `running` by a previous process are marked `interrupted`; their step
    /// runs again. Returns how many runs were resumed.
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
            }
            Ok::<_, Error>(ids)
        })?;
        for id in &ids {
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
            },
            phase: Phase::Setup,
            loops: Default::default(),
            feedback: None,
            config_yaml: yaml,
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

    /// Approves a pending request and resumes its run.
    pub fn approve(&self, request_id: &str, answer: Option<&str>) -> Result<PermissionRequest> {
        let request = self.inner.store.write(|tx| {
            let (request, mut record) = pending_request(tx, request_id)?;
            let config = config_of(&record, &project_root(tx, &record)?)?;
            let request = tx.resolve_request(&request.id, RequestStatus::Approved, answer)?;
            record.run.status = RunStatus::Running;
            match config.pipeline.get(request.step_index) {
                // Steps that ask before acting now get to act.
                Some(Step::PullRequest { .. }) => {
                    record.phase = Phase::Approved;
                    record.run.activity = "approved; opening the pull request".into();
                }
                // Steps that ask after acting are done.
                _ => advance(&mut record),
            }
            tx.save_run(&mut record)?;
            Ok::<_, Error>(request)
        })?;
        self.spawn(&request.run_id);
        Ok(request)
    }

    /// Denies a pending request; the run fails.
    pub fn deny(&self, request_id: &str, answer: Option<&str>) -> Result<PermissionRequest> {
        self.inner.store.write(|tx| {
            let (request, mut record) = pending_request(tx, request_id)?;
            let request = tx.resolve_request(&request.id, RequestStatus::Denied, answer)?;
            let reason = match answer {
                Some(answer) => format!("{} not approved: {answer}", request.step),
                None => format!("{} not approved", request.step),
            };
            fail(&mut record, reason);
            tx.save_run(&mut record)?;
            Ok(request)
        })
    }

    /// Cancels a run that has not finished. Its worktree and branch are kept.
    pub fn cancel(&self, run_id: &str) -> Result<Run> {
        let run = self.inner.store.write(|tx| {
            let mut record = tx
                .run(run_id)?
                .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
            if record.run.status.is_terminal() {
                return Err(Error::Conflict(format!(
                    "run {run_id} is already {}",
                    record.run.status
                )));
            }
            for request in tx.requests(Some(run_id), Some(RequestStatus::Pending))? {
                tx.resolve_request(&request.id, RequestStatus::Cancelled, None)?;
            }
            for attempt in tx.attempts(run_id)? {
                if attempt.status == AttemptStatus::Running {
                    tx.finish_attempt(attempt.id, AttemptStatus::Cancelled, None)?;
                }
            }
            record.run.status = RunStatus::Cancelled;
            record.run.finished_at = Some(now_ms());
            record.run.activity = "cancelled".into();
            tx.save_run(&mut record)?;
            Ok(record.run)
        })?;
        // The cancellation is committed first; aborting the driver also kills
        // a running check (`kill_on_drop`).
        if let Some(task) = self.tasks().remove(run_id) {
            task.abort();
        }
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
            let running = self
                .inner
                .store
                .read(|tx| tx.run(&run_id))
                .ok()
                .flatten()
                .is_some_and(|r| r.run.status == RunStatus::Running);
            if !running {
                tasks.remove(&run_id);
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

        let done = self.transition(&run_id, |tx, record| {
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

        let Some((attempt, number, plan)) = self.transition(&run_id, |tx, record| {
            let attempts = tx.attempts(&run_id)?;
            let number = attempts.iter().filter(|a| a.step_index == index).count() as u32 + 1;
            let plan = attempts
                .iter()
                .rev()
                .find(|a| a.step == StepKind::Plan && a.status == AttemptStatus::Succeeded)
                .and_then(|a| a.output.clone());
            let attempt = tx.start_attempt(&run_id, index, kind)?;
            record.run.activity = format!("{kind}: {role_name} ({}) is working", role.harness);
            Ok((attempt, number, plan))
        })?
        else {
            return Ok(false);
        };

        let task = AgentTask {
            run_id: run_id.clone(),
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
            worktree: worktree_of(&record)?,
            attempt: number,
        };
        let outcome = self.inner.executor.run_agent(&task).await;

        let changes_loop = match step {
            Step::Review {
                on_changes_requested,
                ..
            } => *on_changes_requested,
            _ => None,
        };
        let done = self.transition(&run_id, |tx, record| {
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
        let task = PullRequestTask {
            run_id: run_id.clone(),
            title: record.run.title.clone(),
            branch: record.run.branch.clone().unwrap_or_default(),
            worktree: worktree_of(&record)?,
            draft,
            issue: record.run.issue,
        };
        let result = self.inner.executor.open_pull_request(&task).await;
        let done = self.transition(&run_id, |tx, record| {
            match result {
                Ok(pr) => {
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
    };
    let request = tx.create_request(NewRequest {
        kind,
        run_id: record.run.id.clone(),
        project_id: record.run.project_id.clone(),
        step_index: record.run.step_index,
        step,
        title,
        detail,
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
fn config_of(record: &RunRecord, project_root: &Path) -> Result<Config> {
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

fn project_root(tx: &Tx<'_>, record: &RunRecord) -> Result<PathBuf> {
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
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(80) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
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
