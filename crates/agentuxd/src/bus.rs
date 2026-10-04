//! The agent bus inside the daemon (ADR 0004).
//!
//! Each active run gets its own [`Bus`], opened when its first session starts
//! with the `bus` section, roles and review step of the run's `agentux.yaml`,
//! and closed when the run ends. The daemon is the bus's [`BusBackend`]: run
//! state comes from the store, and `ask_human` becomes an approval request of
//! kind `question` that the human answers through the API.
//!
//! Sessions reach the bus through `aux bus-stdio`, which the daemon passes
//! to every harness as the `agentux` MCP server. Its arguments carry a
//! session token: random, issued when the session joins, kept in memory and
//! dropped when the session leaves, so it only ever acts as that session.
//! `aux bus-stdio` forwards tool calls as `bus.call` requests, one connection
//! per call.
//!
//! Everything the bus reports is stored as `bus_message` events. Wakes become
//! prompts in the target session through the executor, queued behind a turn
//! in progress; a wake for a role without a session starts one.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agentux_api::{
    BusEndpoint, BusMessage, BusMessageKind, CheckStatus, EventBody, PermissionRequest,
    RequestKind, RequestStatus, RunStatus,
};
use agentux_bus::{
    BackendError, BridgeOutcome, BridgeWelcome, Bus, BusBackend, BusCall, BusConfig, BusEvent,
    EventKind, GoneSession, HumanQuestion, Message, MessageKind, Participant, Restore,
    RestoredSent, RunId, RunState, SessionId, SessionIdentity, Target, Wake, WakeTarget,
};
use agentux_harness::McpServer;
use agentux_store::{NewRequest, new_id};
use tokio::sync::{mpsc, oneshot};

use crate::engine::{Engine, Error, Inner, RunHost, config_of, project_root};
use crate::executor::WakeTask;

/// How harness sessions reach the daemon's bus: the `aux` binary to launch
/// as their MCP server, and the socket it forwards calls to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusLink {
    pub socket: PathBuf,
    pub aux: PathBuf,
}

impl BusLink {
    /// The link for a daemon listening on `socket`, with [`aux_binary`].
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            aux: aux_binary(),
        }
    }
}

/// The `aux` binary harnesses launch for the bus: this process when it is
/// `aux` (`aux daemon`), else the `aux` next to it (`agentuxd` from the same
/// install), else `aux` from `PATH`.
pub fn aux_binary() -> PathBuf {
    let Ok(exe) = std::env::current_exe() else {
        return PathBuf::from("aux");
    };
    if exe
        .file_stem()
        .is_some_and(|name| name.to_string_lossy() == "aux")
    {
        return exe;
    }
    match exe.parent().map(|dir| dir.join("aux")) {
        Some(sibling) if sibling.is_file() => sibling,
        _ => PathBuf::from("aux"),
    }
}

/// The daemon's buses, session tokens and unanswered questions.
#[derive(Default)]
pub(crate) struct Hub {
    runs: Mutex<HashMap<String, RunBus>>,
    /// Session token to the session it was issued to.
    tokens: Mutex<HashMap<String, Token>>,
    /// `ask_human` calls waiting for an answer, by request id.
    answers: Mutex<HashMap<String, Waiter>>,
}

struct RunBus {
    bus: Bus,
    config: BusConfig,
    /// The project's name, as sessions are told it.
    project: String,
}

#[derive(Clone)]
struct Token {
    run_id: String,
    session_id: String,
}

struct Waiter {
    run_id: String,
    answer: oneshot::Sender<String>,
}

/// Question id to (request id, asker), for linking answers to questions.
type Questions = Arc<Mutex<HashMap<u64, (String, BusEndpoint)>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn internal(e: impl std::fmt::Display) -> Error {
    Error::Internal(e.to_string())
}

impl Engine {
    /// Puts a new session on its run's bus (opening the bus if needed) and
    /// returns the MCP server and session prompt to start it with. Without a
    /// [`BusLink`] the session joins but gets no MCP server.
    pub(crate) fn bus_attach(
        &self,
        run_id: &str,
        role: &str,
        harness: &str,
        session_id: &str,
    ) -> Result<(Vec<McpServer>, Option<String>), Error> {
        let (bus, config, project) = self.run_bus(run_id)?;
        let identity = SessionIdentity {
            run: RunId(run_id.to_string()),
            project,
            role: role.to_string(),
            vendor: harness.to_string(),
            session: SessionId(session_id.to_string()),
        };
        bus.join(identity.clone()).map_err(internal)?;
        let Some(link) = &self.inner.settings.bus else {
            return Ok((Vec::new(), None));
        };
        let token = new_token().map_err(Error::Internal)?;
        lock(&self.inner.hub.tokens).insert(
            token.clone(),
            Token {
                run_id: run_id.to_string(),
                session_id: session_id.to_string(),
            },
        );
        let server = McpServer {
            name: agentux_bus::SERVER_NAME.to_string(),
            command: link.aux.clone(),
            args: agentux_bus::bus_stdio_args(&link.socket),
            // Not in the arguments: other local users can read those.
            env: agentux_bus::bus_stdio_env(&token),
        };
        let prompt = agentux_bus::session_prompt(
            &identity,
            &config.allowed_tools(),
            config.max_turns_per_exchange,
        );
        Ok((vec![server], Some(prompt)))
    }

    /// Takes an ended session off the bus and revokes its token.
    pub(crate) fn bus_leave(&self, session_id: &str) {
        lock(&self.inner.hub.tokens).retain(|_, token| token.session_id != session_id);
        let session = SessionId(session_id.to_string());
        for entry in lock(&self.inner.hub.runs).values() {
            if entry.bus.has_session(&session) {
                entry.bus.leave(&session);
            }
        }
    }

    /// Closes a finished run's bus: its tokens stop working and questions
    /// still waiting for the human are cancelled.
    pub(crate) fn close_bus(&self, run_id: &str) {
        let hub = &self.inner.hub;
        let sessions = self.inner.store.read(|tx| tx.sessions(Some(run_id)));
        if let Some(entry) = lock(&hub.runs).remove(run_id) {
            // Leaving one by one puts each departure in the log.
            for session in sessions.iter().flatten() {
                let id = SessionId(session.id.clone());
                if entry.bus.has_session(&id) {
                    entry.bus.leave(&id);
                }
            }
            entry.bus.close_run(&RunId(run_id.to_string()));
        }
        lock(&hub.tokens).retain(|_, token| token.run_id != run_id);
        lock(&hub.answers).retain(|_, waiter| waiter.run_id != run_id);
        let cancelled = self.inner.store.write(|tx| {
            for request in tx.requests(Some(run_id), Some(RequestStatus::Pending))? {
                if request.kind == RequestKind::Question {
                    tx.resolve_request(
                        &request.id,
                        RequestStatus::Cancelled,
                        Some("the run ended before an answer"),
                    )?;
                }
            }
            Ok::<_, Error>(())
        });
        if let Err(e) = cancelled {
            eprintln!("agentuxd: run {run_id}: cannot cancel its questions: {e}");
        }
    }

    /// The run's bus, opened (and restored from its log) if needed.
    pub(crate) fn open_bus(&self, run_id: &str) -> Result<Bus, Error> {
        Ok(self.run_bus(run_id)?.0)
    }

    /// After a restart: reopens the bus of an active run whose log has mail
    /// still waiting for a role, so that the role is woken for it.
    pub(crate) fn reopen_bus(&self, run_id: &str) {
        let pending = self
            .inner
            .store
            .read(|tx| tx.bus_messages(run_id))
            .map(|log| !restore_from(&log).queued.is_empty());
        match pending {
            Ok(false) => {}
            Ok(true) => {
                if let Err(e) = self.run_bus(run_id) {
                    eprintln!("agentuxd: run {run_id}: cannot reopen its bus: {e}");
                }
            }
            Err(e) => eprintln!("agentuxd: run {run_id}: cannot read its bus log: {e}"),
        }
    }

    /// A run's bus log, oldest first.
    pub fn bus_list(&self, run_id: &str) -> Result<Vec<BusMessage>, Error> {
        self.inner.store.read(|tx| {
            if tx.run(run_id)?.is_none() {
                return Err(Error::NotFound(format!("no run {run_id}")));
            }
            Ok(tx.bus_messages(run_id)?)
        })
    }

    /// `bus.hello`: who a session token stands for.
    pub fn bus_hello(&self, token: &str) -> Result<BridgeWelcome, Error> {
        let (bus, config, token) = self.authorize(token)?;
        let identity = bus
            .identity(&SessionId(token.session_id))
            .ok_or_else(|| Error::Unauthorized("the session has ended".into()))?;
        Ok(BridgeWelcome {
            identity,
            allowed_tools: config
                .allowed_tools()
                .iter()
                .map(|tool| tool.as_str().to_string())
                .collect(),
            max_turns_per_exchange: config.max_turns_per_exchange,
        })
    }

    /// `bus.call`: one tool call, made as the token's session.
    pub async fn bus_call(&self, token: &str, call: BusCall) -> Result<BridgeOutcome, Error> {
        let (bus, _, token) = self.authorize(token)?;
        Ok(bus.call(&SessionId(token.session_id), call).await.into())
    }

    fn authorize(&self, token: &str) -> Result<(Bus, BusConfig, Token), Error> {
        let rejected = || Error::Unauthorized("unknown or expired session token".into());
        let token = lock(&self.inner.hub.tokens)
            .get(token)
            .cloned()
            .ok_or_else(rejected)?;
        let runs = lock(&self.inner.hub.runs);
        let entry = runs.get(&token.run_id).ok_or_else(rejected)?;
        if !entry.bus.has_session(&SessionId(token.session_id.clone())) {
            return Err(rejected());
        }
        Ok((entry.bus.clone(), entry.config.clone(), token))
    }

    /// The run's bus, opened on first use, and the project's name.
    fn run_bus(&self, run_id: &str) -> Result<(Bus, BusConfig, String), Error> {
        let mut runs = lock(&self.inner.hub.runs);
        if let Some(entry) = runs.get(run_id) {
            return Ok((
                entry.bus.clone(),
                entry.config.clone(),
                entry.project.clone(),
            ));
        }
        let (record, root, project, log) = self.inner.store.read(|tx| {
            let record = tx
                .run(run_id)?
                .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
            let root = project_root(tx, &record)?;
            let project = tx
                .project(&record.run.project_id)?
                .map_or_else(|| record.run.project_id.clone(), |p| p.name);
            Ok::<_, Error>((record, root, project, tx.bus_messages(run_id)?))
        })?;
        let config = BusConfig::from_config(&config_of(&record, &root)?);
        let questions = Questions::default();
        let backend = DaemonBackend {
            engine: Arc::downgrade(&self.inner),
            run_id: run_id.to_string(),
            project_id: record.run.project_id.clone(),
            max_turns: config.max_turns_per_exchange,
            questions: Arc::clone(&questions),
        };
        let bus = Bus::new(Arc::new(backend));
        let events = bus.subscribe();
        bus.open_run(RunId(run_id.to_string()), project.clone(), config.clone())
            .map_err(internal)?;
        // A run reopened after a restart continues its ids, so the log never
        // has two messages with the same id.
        let max = |id: fn(&BusMessage) -> Option<u64>| log.iter().filter_map(id).max().unwrap_or(0);
        bus.reserve_ids(
            max(|m| m.message_id),
            max(|m| m.exchange),
            max(|m| m.question_id),
        );
        // Exchanges, reply routing and waiting mail survive a restart.
        bus.restore(&RunId(run_id.to_string()), restore_from(&log))
            .map_err(internal)?;
        let mapper = Mapper {
            run_id: run_id.to_string(),
            project_id: record.run.project_id.clone(),
            max_turns: config.max_turns_per_exchange,
            questions,
            sessions: HashMap::new(),
        };
        tokio::spawn(pump(Arc::downgrade(&self.inner), mapper, events));
        runs.insert(
            run_id.to_string(),
            RunBus {
                bus: bus.clone(),
                config: config.clone(),
                project: project.clone(),
            },
        );
        Ok((bus, config, project))
    }

    /// Records the human's answer to a `question` request (or the refusal
    /// to answer) and hands it to the agent waiting in `ask_human`.
    pub(crate) fn answer_question(
        &self,
        request_id: &str,
        approve: bool,
        answer: Option<&str>,
    ) -> Result<PermissionRequest, Error> {
        let answer = answer.map(str::trim).filter(|a| !a.is_empty());
        if approve && answer.is_none() {
            return Err(Error::InvalidParams(
                "a question needs an answer: pass it as `answer` (one of its options, or free text)"
                    .into(),
            ));
        }
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
            let status = if approve {
                RequestStatus::Approved
            } else {
                RequestStatus::Denied
            };
            Ok(tx.resolve_request(request_id, status, answer)?)
        })?;
        let text = match (approve, answer) {
            (true, Some(answer)) => answer.to_string(),
            (_, Some(reason)) => format!("(the human declined to answer: {reason})"),
            (_, None) => "(the human declined to answer)".to_string(),
        };
        if let Some(waiter) = lock(&self.inner.hub.answers).remove(&request.id) {
            let _ = waiter.answer.send(text);
        }
        Ok(request)
    }

    /// Prompts the target of a wake (starting a session for a role without
    /// one). Failures are logged on the run.
    async fn wake(&self, wake: Wake) {
        let run_id = wake.run.0.clone();
        if let Err(e) = self.try_wake(&run_id, wake).await {
            let logged = self
                .inner
                .store
                .write(|tx| tx.log(&run_id, format!("bus: {e}")));
            if let Err(e) = logged {
                eprintln!("agentuxd: run {run_id}: cannot record a bus failure: {e}");
            }
        }
    }

    async fn try_wake(&self, run_id: &str, wake: Wake) -> Result<(), String> {
        if !lock(&self.inner.hub.runs).contains_key(run_id) {
            return Ok(());
        }
        let session_id = match &wake.target {
            WakeTarget::Session(id) => Some(id.0.clone()),
            WakeTarget::Role(_) => None,
        };
        let (record, root, session) = self
            .inner
            .store
            .read(|tx| {
                let record = tx
                    .run(run_id)?
                    .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
                let root = project_root(tx, &record)?;
                let session = match &session_id {
                    Some(id) => tx.session(id)?,
                    None => None,
                };
                Ok::<_, Error>((record, root, session))
            })
            .map_err(|e| e.to_string())?;
        // A finished run starts nothing; its sessions are gone.
        if record.run.status.is_terminal() {
            return Ok(());
        }
        let role = match (&wake.target, session) {
            (WakeTarget::Role(role), _) => role.clone(),
            (WakeTarget::Session(_), Some(session)) => session.role,
            (WakeTarget::Session(_), None) => return Ok(()),
        };
        let config = config_of(&record, &root).map_err(|e| e.to_string())?;
        let harness = config
            .roles
            .get(&role)
            .ok_or_else(|| format!("cannot wake role {role}: the pipeline has no such role"))?;
        let worktree = record
            .run
            .worktree
            .clone()
            .ok_or_else(|| format!("cannot wake the {role}: the run has no worktree yet"))?;
        let host = Arc::new(RunHost {
            engine: self.clone(),
            run_id: run_id.to_string(),
            project_id: record.run.project_id.clone(),
            role: role.clone(),
            step_index: record.run.step_index,
            step: record.run.step,
            cwd: worktree.clone(),
        });
        let task = WakeTask {
            run_id: run_id.to_string(),
            role: role.clone(),
            harness: harness.harness.clone(),
            model: harness.model.clone(),
            worktree: PathBuf::from(worktree),
            session_id,
            prompt: wake.prompt,
            human: false,
        };
        self.inner
            .executor
            .wake(&task, host)
            .await
            .map_err(|e| format!("could not wake the {role}: {e}"))
    }

    /// The run as the bus's `get_run_state` reports it.
    fn bus_run_state(&self, run_id: &str) -> Result<RunState, BackendError> {
        let (record, pending) = self
            .inner
            .store
            .read(|tx| {
                let record = tx
                    .run(run_id)?
                    .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
                Ok::<_, Error>((
                    record,
                    tx.requests(Some(run_id), Some(RequestStatus::Pending))?,
                ))
            })
            .map_err(|e| BackendError(e.to_string()))?;
        let run = record.run;
        Ok(RunState {
            step: Some(run.step.to_string()),
            status: Some(match run.status {
                RunStatus::Waiting => "waiting for the human".to_string(),
                status => status.to_string(),
            }),
            branch: run.branch,
            checks: run
                .checks
                .into_iter()
                .map(|check| agentux_bus::CheckResult {
                    name: check.name,
                    passed: match check.status {
                        CheckStatus::Passed => Some(true),
                        CheckStatus::Failed => Some(false),
                        CheckStatus::Pending | CheckStatus::Running => None,
                    },
                    summary: None,
                })
                .collect(),
            open_requests: pending
                .into_iter()
                .map(|r| format!("{} request {}: {}", r.kind, r.id, r.title))
                .collect(),
        })
    }

    /// Turns an `ask_human` question into a `question` request (and its bus
    /// log entry) and returns where the answer will arrive.
    fn ask_human(
        &self,
        backend: &DaemonBackend,
        question: HumanQuestion,
    ) -> Result<oneshot::Receiver<String>, BackendError> {
        let (answer, answered) = oneshot::channel();
        let asker = participant(&question.from, None);
        // Held until the waiter is registered, so an answer committed in
        // between finds it.
        let mut answers = lock(&self.inner.hub.answers);
        let request = self
            .inner
            .store
            .write(|tx| {
                let record = tx
                    .run(&backend.run_id)?
                    .ok_or_else(|| Error::NotFound(format!("no run {}", backend.run_id)))?;
                if record.run.status.is_terminal() {
                    return Err(Error::Conflict(format!("the run is {}", record.run.status)));
                }
                let (who, session_id) = match &question.from {
                    Participant::Session {
                        session,
                        role,
                        vendor,
                    } => (format!("{role} ({vendor})"), Some(session.0.clone())),
                    Participant::Human => ("the human".to_string(), None),
                };
                let mut detail = question.question.clone();
                if let Some(context) = &question.context {
                    detail.push_str(&format!("\n\nContext: {context}"));
                }
                if !question.options.is_empty() {
                    detail.push_str(&format!("\n\nOptions: {}", question.options.join(" | ")));
                }
                let request = tx.create_request(NewRequest {
                    kind: RequestKind::Question,
                    run_id: backend.run_id.clone(),
                    project_id: backend.project_id.clone(),
                    step_index: record.run.step_index,
                    step: record.run.step,
                    title: format!("{who} asks: {}", one_line(&question.question)),
                    detail: detail.clone(),
                    session_id,
                    options: question.options.clone(),
                })?;
                let message = BusMessage {
                    question_id: Some(question.id),
                    request_id: Some(request.id.clone()),
                    ..entry(
                        &backend.run_id,
                        &backend.project_id,
                        BusMessageKind::Question,
                        Some("ask_human"),
                        asker.clone(),
                        BusEndpoint::Human,
                        one_line(&question.question),
                        detail,
                        question.asked_at_ms,
                        backend.max_turns,
                    )
                };
                tx.emit(Some(&backend.run_id), EventBody::BusMessage { message })?;
                Ok(request)
            })
            .map_err(|e: Error| BackendError(e.to_string()))?;
        lock(&backend.questions).insert(question.id, (request.id.clone(), asker));
        answers.insert(
            request.id,
            Waiter {
                run_id: backend.run_id.clone(),
                answer,
            },
        );
        Ok(answered)
    }
}

/// The daemon as the bus's backend, for one run.
struct DaemonBackend {
    engine: Weak<Inner>,
    run_id: String,
    project_id: String,
    max_turns: u32,
    questions: Questions,
}

impl DaemonBackend {
    fn engine(&self) -> Result<Engine, BackendError> {
        self.engine
            .upgrade()
            .map(Engine::from_inner)
            .ok_or_else(|| BackendError("agentuxd is shutting down".into()))
    }
}

impl BusBackend for DaemonBackend {
    fn run_state(&self, _run: &RunId) -> agentux_bus::BoxFuture<Result<RunState, BackendError>> {
        let state = self
            .engine()
            .and_then(|engine| engine.bus_run_state(&self.run_id));
        Box::pin(async move { state })
    }

    fn ask_human(
        &self,
        question: HumanQuestion,
    ) -> agentux_bus::BoxFuture<Result<String, BackendError>> {
        let answered = self
            .engine()
            .and_then(|engine| engine.ask_human(self, question));
        Box::pin(async move {
            answered?
                .await
                .map_err(|_| BackendError("the question was withdrawn".into()))
        })
    }
}

/// Stores the run's bus events and acts on its wakes, until the bus closes.
async fn pump(
    engine: Weak<Inner>,
    mut mapper: Mapper,
    mut events: mpsc::UnboundedReceiver<BusEvent>,
) {
    while let Some(event) = events.recv().await {
        let Some(inner) = engine.upgrade() else {
            return;
        };
        let engine = Engine::from_inner(inner);
        if let Some(message) = mapper.map(&engine, &event) {
            let stored = engine
                .inner
                .store
                .write(|tx| tx.emit(Some(&mapper.run_id), EventBody::BusMessage { message }));
            if let Err(e) = stored {
                eprintln!(
                    "agentuxd: run {}: cannot record a bus event: {e}",
                    mapper.run_id
                );
            }
        }
        if let EventKind::Wake(wake) = event.kind {
            tokio::spawn(async move { engine.wake(wake).await });
        }
    }
}

/// Maps the bus's events (snake_case, bus ids) to API [`BusMessage`]s.
struct Mapper {
    run_id: String,
    project_id: String,
    max_turns: u32,
    questions: Questions,
    /// Sessions seen joining, for addressing them by role and vendor.
    sessions: HashMap<String, BusEndpoint>,
}

impl Mapper {
    #[allow(clippy::too_many_arguments)]
    fn entry(
        &self,
        kind: BusMessageKind,
        tool: Option<&str>,
        from: BusEndpoint,
        to: BusEndpoint,
        subject: String,
        body: String,
        at_ms: u64,
    ) -> BusMessage {
        entry(
            &self.run_id,
            &self.project_id,
            kind,
            tool,
            from,
            to,
            subject,
            body,
            at_ms,
            self.max_turns,
        )
    }

    /// A session as an endpoint, from its join or else from the store.
    fn session(&self, engine: &Engine, id: &SessionId) -> BusEndpoint {
        if let Some(endpoint) = self.sessions.get(&id.0) {
            return endpoint.clone();
        }
        let stored = engine
            .inner
            .store
            .read(|tx| tx.session(&id.0))
            .ok()
            .flatten();
        BusEndpoint::Session {
            session_id: id.0.clone(),
            role: stored.as_ref().map_or_else(String::new, |s| s.role.clone()),
            vendor: stored.map_or_else(String::new, |s| s.harness),
        }
    }

    fn target(&self, engine: &Engine, target: &Target) -> BusEndpoint {
        match target {
            Target::Session(id) => self.session(engine, id),
            Target::Role(role) => BusEndpoint::Role { role: role.clone() },
            Target::Run => BusEndpoint::Run,
            Target::Human => BusEndpoint::Human,
        }
    }

    /// `None` for events stored elsewhere: questions are stored with their
    /// request.
    fn map(&mut self, engine: &Engine, event: &BusEvent) -> Option<BusMessage> {
        let at = event.at_ms;
        Some(match &event.kind {
            EventKind::SessionJoined { identity, queued } => {
                let endpoint = participant(&identity.participant(), None);
                self.sessions
                    .insert(identity.session.0.clone(), endpoint.clone());
                let mut subject = format!("{} ({}) joined the bus", identity.role, identity.vendor);
                if *queued > 0 {
                    subject.push_str(&format!(
                        "; {queued} message(s) waiting for the role moved to its mailbox"
                    ));
                }
                self.entry(
                    BusMessageKind::Joined,
                    None,
                    endpoint,
                    BusEndpoint::Run,
                    subject.clone(),
                    subject,
                    at,
                )
            }
            EventKind::SessionLeft {
                session,
                unread,
                requeued_for,
            } => {
                let endpoint = self.session(engine, session);
                let mut subject = format!("{} left the bus", describe(&endpoint));
                match requeued_for {
                    Some(role) => {
                        subject.push_str(" (agentuxd restarted)");
                        if *unread > 0 {
                            subject.push_str(&format!(
                                "; {unread} message(s) delivered to it queued again for role {role}"
                            ));
                        }
                    }
                    None if *unread > 0 => {
                        subject.push_str(&format!("; {unread} unread message(s) dropped"));
                    }
                    None => {}
                }
                BusMessage {
                    queued_for_role: requeued_for.clone(),
                    ..self.entry(
                        BusMessageKind::Left,
                        None,
                        endpoint,
                        BusEndpoint::Run,
                        subject.clone(),
                        subject,
                        at,
                    )
                }
            }
            EventKind::MessagePosted {
                message,
                delivered_to,
                queued_for_role,
            } => {
                let (kind, tool) = match message.kind {
                    MessageKind::Message => (BusMessageKind::Message, Some("post_message")),
                    MessageKind::ReviewRequest => {
                        (BusMessageKind::ReviewRequest, Some("request_review"))
                    }
                    MessageKind::Handoff => (BusMessageKind::Handoff, Some("handoff")),
                    MessageKind::HumanAnswer => (BusMessageKind::HumanAnswer, None),
                };
                let from = participant(&message.from, Some(&self.sessions));
                let to = self.target(engine, &message.to);
                BusMessage {
                    turn: message.turn,
                    message_id: Some(message.id),
                    exchange: Some(message.exchange),
                    in_reply_to: message.in_reply_to,
                    delivered_to: delivered_to.iter().map(|s| s.0.clone()).collect(),
                    queued_for_role: queued_for_role.clone(),
                    ..self.entry(
                        kind,
                        tool,
                        from,
                        to,
                        one_line(&message.body),
                        message.body.clone(),
                        at,
                    )
                }
            }
            EventKind::Wake(wake) => {
                let to = match &wake.target {
                    WakeTarget::Session(id) => self.session(engine, id),
                    WakeTarget::Role(role) => BusEndpoint::Role { role: role.clone() },
                };
                let subject = match &wake.target {
                    WakeTarget::Session(_) => {
                        format!("wake {} for a {}", describe(&to), wake.reason.as_str())
                    }
                    WakeTarget::Role(role) => {
                        format!("start a {role} session for a {}", wake.reason.as_str())
                    }
                };
                BusMessage {
                    message_id: Some(wake.message),
                    ..self.entry(
                        BusMessageKind::Wake,
                        None,
                        BusEndpoint::Daemon,
                        to,
                        subject,
                        wake.prompt.clone(),
                        at,
                    )
                }
            }
            EventKind::HumanAsked { .. } => return None,
            EventKind::HumanAnswered { question, answer } => {
                let (request_id, asker) = lock(&self.questions)
                    .remove(question)
                    .map_or((None, BusEndpoint::Run), |(request, asker)| {
                        (Some(request), asker)
                    });
                BusMessage {
                    question_id: Some(*question),
                    request_id,
                    ..self.entry(
                        BusMessageKind::Answer,
                        Some("ask_human"),
                        BusEndpoint::Human,
                        asker,
                        one_line(answer),
                        answer.clone(),
                        at,
                    )
                }
            }
            EventKind::TurnLimitReached {
                session,
                exchange,
                max_turns,
            } => {
                let subject = format!(
                    "exchange {exchange} reached its limit of {max_turns} turns; post refused"
                );
                BusMessage {
                    exchange: Some(*exchange),
                    ..self.entry(
                        BusMessageKind::TurnLimit,
                        Some("post_message"),
                        self.session(engine, session),
                        BusEndpoint::Daemon,
                        subject.clone(),
                        subject,
                        at,
                    )
                }
            }
            EventKind::ToolDenied { session, tool } => {
                let subject = format!("`{tool}` is not allowed in this project (bus.allow)");
                self.entry(
                    BusMessageKind::ToolDenied,
                    Some(tool.as_str()),
                    self.session(engine, session),
                    BusEndpoint::Daemon,
                    subject.clone(),
                    subject,
                    at,
                )
            }
        })
    }
}

/// A bus log entry with the common fields set.
#[allow(clippy::too_many_arguments)]
fn entry(
    run_id: &str,
    project_id: &str,
    kind: BusMessageKind,
    tool: Option<&str>,
    from: BusEndpoint,
    to: BusEndpoint,
    subject: String,
    body: String,
    at_ms: u64,
    max_turns: u32,
) -> BusMessage {
    BusMessage {
        id: new_id(),
        run_id: run_id.to_string(),
        project_id: project_id.to_string(),
        kind,
        tool: tool.map(str::to_string),
        from,
        to,
        subject,
        body,
        at: i64::try_from(at_ms).unwrap_or(i64::MAX),
        turn: 0,
        max_turns,
        message_id: None,
        exchange: None,
        in_reply_to: None,
        question_id: None,
        request_id: None,
        delivered_to: Vec::new(),
        queued_for_role: None,
    }
}

fn participant(
    participant: &Participant,
    known: Option<&HashMap<String, BusEndpoint>>,
) -> BusEndpoint {
    match participant {
        Participant::Human => BusEndpoint::Human,
        Participant::Session {
            session,
            role,
            vendor,
        } => known
            .and_then(|known| known.get(&session.0).cloned())
            .unwrap_or_else(|| BusEndpoint::Session {
                session_id: session.0.clone(),
                role: role.clone(),
                vendor: vendor.clone(),
            }),
    }
}

fn describe(endpoint: &BusEndpoint) -> String {
    match endpoint {
        BusEndpoint::Session {
            session_id, role, ..
        } if !role.is_empty() => format!("the {role} (session {session_id})"),
        BusEndpoint::Session { session_id, .. } => format!("session {session_id}"),
        BusEndpoint::Role { role } => format!("role {role}"),
        BusEndpoint::Run => "the run".into(),
        BusEndpoint::Human => "the human".into(),
        BusEndpoint::Daemon => "agentuxd".into(),
    }
}

/// The first line, cut to 100 characters.
fn one_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// A session token: 32 random bytes from the kernel, hex-encoded.
fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| format!("cannot read /dev/urandom for a session token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Messages queued again for one role after a restart, at most.
const MAX_REQUEUED_PER_ROLE: usize = 50;

/// What a reopened run's bus takes from its persisted log: turns used per
/// exchange, the sender of every message (for replies), and the mail nobody
/// was told about yet. Reads are not logged, so a message counts as handled
/// once a wake was issued for its recipient after it arrived (the session was
/// prompted to read its mail, or a session was started for its role). What is
/// left: messages still queued for a role, and messages delivered without a
/// wake (the `run` channel) to sessions that were on the bus when the daemon
/// stopped. Those sessions are gone; their mail is queued again for their
/// role, and the `left` entry recording that carries `queuedForRole`, so a
/// later rebuild sees the same queue.
pub(crate) fn restore_from(log: &[BusMessage]) -> Restore {
    let mut restore = Restore::default();
    let mut exchanges: BTreeMap<u64, u32> = BTreeMap::new();
    // Role to messages waiting for it, each with whether a wake announced it.
    let mut pending: BTreeMap<String, Vec<(Message, bool)>> = BTreeMap::new();
    // Session to (role, unannounced messages delivered to it), for the
    // sessions on the bus.
    let mut live: BTreeMap<String, (String, Vec<Message>)> = BTreeMap::new();
    for entry in log {
        match entry.kind {
            BusMessageKind::Message
            | BusMessageKind::ReviewRequest
            | BusMessageKind::Handoff
            | BusMessageKind::HumanAnswer => {
                let (Some(id), Some(exchange)) = (entry.message_id, entry.exchange) else {
                    continue;
                };
                let Some(from) = sender(&entry.from) else {
                    continue;
                };
                let turns = exchanges.entry(exchange).or_insert(0);
                *turns = (*turns).max(entry.turn);
                restore.sent.push(RestoredSent {
                    message: id,
                    exchange,
                    from: from.clone(),
                });
                let Some(to) = target(&entry.to) else {
                    continue;
                };
                let message = Message {
                    id,
                    exchange,
                    turn: entry.turn,
                    run: RunId(entry.run_id.clone()),
                    from,
                    to,
                    kind: match entry.kind {
                        BusMessageKind::ReviewRequest => MessageKind::ReviewRequest,
                        BusMessageKind::Handoff => MessageKind::Handoff,
                        BusMessageKind::HumanAnswer => MessageKind::HumanAnswer,
                        _ => MessageKind::Message,
                    },
                    body: entry.body.clone(),
                    in_reply_to: entry.in_reply_to,
                    sent_at_ms: u64::try_from(entry.at).unwrap_or(0),
                };
                if let Some(role) = &entry.queued_for_role {
                    pending
                        .entry(role.clone())
                        .or_default()
                        .push((message.clone(), false));
                }
                for session in &entry.delivered_to {
                    if let Some((_, mail)) = live.get_mut(session) {
                        mail.push(message.clone());
                    }
                }
            }
            BusMessageKind::Wake => match &entry.to {
                BusEndpoint::Session { session_id, .. } => {
                    if let Some((_, mail)) = live.get_mut(session_id) {
                        mail.clear();
                    }
                }
                BusEndpoint::Role { role } => {
                    for (_, announced) in pending.entry(role.clone()).or_default() {
                        *announced = true;
                    }
                }
                _ => {}
            },
            BusMessageKind::Joined => {
                if let BusEndpoint::Session {
                    session_id, role, ..
                } = &entry.from
                {
                    // The role's waiting mail moved to its mailbox; what a
                    // wake announced, the session is prompted for.
                    let mail = pending
                        .remove(role)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|(_, announced)| !announced)
                        .map(|(message, _)| message)
                        .collect();
                    live.insert(session_id.clone(), (role.clone(), mail));
                }
            }
            BusMessageKind::Left => {
                if let BusEndpoint::Session { session_id, .. } = &entry.from
                    && let Some((_, mail)) = live.remove(session_id)
                    && let Some(role) = &entry.queued_for_role
                {
                    pending
                        .entry(role.clone())
                        .or_default()
                        .extend(mail.into_iter().map(|m| (m, false)));
                }
            }
            _ => {}
        }
    }
    for (session, (role, mail)) in live {
        restore.gone.push(GoneSession {
            session: SessionId(session),
            role: role.clone(),
            requeued: mail.len(),
        });
        pending
            .entry(role)
            .or_default()
            .extend(mail.into_iter().map(|m| (m, false)));
    }
    for (role, mail) in pending {
        let mut mail: Vec<Message> = mail.into_iter().map(|(m, _)| m).collect();
        mail.sort_by_key(|m| m.id);
        mail.dedup_by_key(|m| m.id);
        let skip = mail.len().saturating_sub(MAX_REQUEUED_PER_ROLE);
        restore
            .queued
            .extend(mail.into_iter().skip(skip).map(|m| (role.clone(), m)));
    }
    restore.exchanges = exchanges.into_iter().collect();
    restore
}

fn sender(endpoint: &BusEndpoint) -> Option<Participant> {
    match endpoint {
        BusEndpoint::Session {
            session_id,
            role,
            vendor,
        } => Some(Participant::Session {
            session: SessionId(session_id.clone()),
            role: role.clone(),
            vendor: vendor.clone(),
        }),
        BusEndpoint::Human => Some(Participant::Human),
        BusEndpoint::Role { .. } | BusEndpoint::Run | BusEndpoint::Daemon => None,
    }
}

fn target(endpoint: &BusEndpoint) -> Option<Target> {
    match endpoint {
        BusEndpoint::Session { session_id, .. } => {
            Some(Target::Session(SessionId(session_id.clone())))
        }
        BusEndpoint::Role { role } => Some(Target::Role(role.clone())),
        BusEndpoint::Run => Some(Target::Run),
        BusEndpoint::Human => Some(Target::Human),
        BusEndpoint::Daemon => None,
    }
}
