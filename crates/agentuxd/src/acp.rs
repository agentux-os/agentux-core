//! [`AcpExecutor`]: runs agent steps through harness sessions (ADR 0002) on
//! top of `agentux-harness`, and the `pull_request` step through `gh`.
//!
//! Each role of a run gets one session, started in the run's worktree when
//! the run first reaches that role and reused by its later steps (a fix-up
//! after a failing gate goes to the same implementer session). Prompts come
//! from [`crate::prompts`] and are self-contained, so a fresh session after a
//! daemon restart gets everything it needs. Session events are coalesced and
//! handed to the engine's [`StepHost`]; permission requests become approval
//! requests through it.
//!
//! Every session gets the `agentux` bus as an MCP server in `session/new`,
//! and the bus's session prompt before its first prompt. Wakes from the bus
//! are extra turns in the target session, queued behind a turn in progress.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agentux_api::{
    MessageFrom, PlanItem, PlanItemStatus, SessionEvent, SessionState, SessionUsage, StepKind,
    ToolKind, ToolStatus,
};
use agentux_harness::{
    AcpHarness, AcpSession, Decision, Event, Events, Harness, HarnessSession, HarnessSpec,
    McpServer, ModelSelection, PermissionHandler, PermissionRequest, PlanStatus, StopReason,
    permission_handler,
};

use crate::executor::{
    AgentOutcome, AgentTask, BoxFuture, PullRequestOutcome, PullRequestTask, StepExecutor,
    StepHost, WakeTask,
};
use crate::forge;
use crate::prompts::{self, Verdict};

/// Agent text is stored in chunks of at most about this size...
const MESSAGE_FLUSH_BYTES: usize = 4096;
/// ...or after this long without a flush.
const MESSAGE_FLUSH_EVERY: Duration = Duration::from_millis(750);
/// Tool output and diff texts kept per event.
const MAX_EVENT_TEXT: usize = 16 * 1024;
/// How long a released session gets to exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What to launch: the harness, where, with which model and MCP servers.
#[derive(Debug, Clone, Copy)]
pub struct LaunchSpec<'a> {
    pub harness: &'a str,
    pub cwd: &'a Path,
    /// The role's `model` from `agentux.yaml`.
    pub model: Option<&'a str>,
    /// The `agentux` bus, to pass in `session/new`.
    pub mcp_servers: &'a [McpServer],
}

/// Starts harness sessions. The daemon uses [`HarnessLauncher`]; tests plug in
/// in-process agents.
pub trait Launcher: Send + Sync + 'static {
    fn launch<'a>(
        &'a self,
        spec: LaunchSpec<'a>,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>>;
}

/// Launches the built-in harnesses ([`HarnessSpec::builtin`]) as ACP agents.
pub struct HarnessLauncher;

impl Launcher for HarnessLauncher {
    fn launch<'a>(
        &'a self,
        spec: LaunchSpec<'a>,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            let Some(harness) = HarnessSpec::find(spec.harness) else {
                let known: Vec<String> = HarnessSpec::builtin().into_iter().map(|s| s.id).collect();
                return Err(format!(
                    "unknown harness `{}` (known: {})",
                    spec.harness,
                    known.join(", ")
                ));
            };
            AcpHarness::new(harness)
                .with_mcp_servers(spec.mcp_servers.to_vec())
                .with_model(spec.model.map(str::to_string))
                .start(spec.cwd, permissions)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

type HostSlot = Arc<Mutex<Option<Arc<dyn StepHost>>>>;

/// One live session, for one role of one run.
struct Slot {
    /// The daemon's id of the session.
    id: String,
    harness: String,
    /// The host of the turn currently running in the session (a step's, or
    /// a bus wake's), which permission requests go to. `None` between turns:
    /// requests are then denied.
    host: HostSlot,
    /// The bus system prompt, prepended to the session's first prompt.
    intro: Mutex<Option<String>>,
    /// Held for a whole turn (or a step's turns): a wake for a session that
    /// is mid-turn waits here.
    live: tokio::sync::Mutex<Live>,
}

struct Live {
    session: Option<AcpSession>,
    events: Events,
}

impl Slot {
    /// `text`, preceded by the session prompt the first time.
    fn with_intro(&self, text: &str) -> String {
        match lock(&self.intro).take() {
            Some(intro) => format!("{intro}\n\n---\n\n{text}"),
            None => text.to_string(),
        }
    }
}

/// Sets the slot's host for as long as it lives.
struct HostGuard<'a>(&'a Slot);

impl<'a> HostGuard<'a> {
    fn set(slot: &'a Slot, host: &Arc<dyn StepHost>) -> Self {
        *lock(&slot.host) = Some(Arc::clone(host));
        Self(slot)
    }
}

impl Drop for HostGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.host) = None;
    }
}

/// The session a step or a wake needs.
struct Want<'a> {
    run_id: &'a str,
    role: &'a str,
    harness: &'a str,
    model: Option<&'a str>,
    worktree: &'a Path,
}

pub struct AcpExecutor {
    launcher: Box<dyn Launcher>,
    /// Keyed by (run id, role).
    sessions: Mutex<HashMap<(String, String), Arc<Slot>>>,
    /// Held while a session starts, so that a step and a wake for the same
    /// role do not both start one.
    starting: tokio::sync::Mutex<()>,
}

impl Default for AcpExecutor {
    fn default() -> Self {
        Self::new(HarnessLauncher)
    }
}

impl AcpExecutor {
    pub fn new(launcher: impl Launcher) -> Self {
        Self {
            launcher: Box::new(launcher),
            sessions: Mutex::new(HashMap::new()),
            starting: tokio::sync::Mutex::new(()),
        }
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<(String, String), Arc<Slot>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The role's live session, started if needed.
    async fn slot(&self, want: &Want<'_>, host: &Arc<dyn StepHost>) -> Result<Arc<Slot>, String> {
        let key = (want.run_id.to_string(), want.role.to_string());
        let _starting = self.starting.lock().await;
        if let Some(slot) = self.sessions().get(&key)
            && slot.harness == want.harness
        {
            return Ok(Arc::clone(slot));
        }
        let opened = host.open_session(want.harness, want.model)?;
        let host_slot: HostSlot = Arc::default();
        let handler = permissions(
            Arc::clone(&host_slot),
            opened.id.clone(),
            want.role.to_string(),
            want.harness.to_string(),
        );
        let spec = LaunchSpec {
            harness: want.harness,
            cwd: want.worktree,
            model: want.model,
            mcp_servers: &opened.mcp_servers,
        };
        match self.launcher.launch(spec, handler).await {
            Ok((session, events)) => {
                if let ModelSelection::Unavailable(note) = session.model_selection() {
                    host.session_event(
                        &opened.id,
                        SessionEvent::Message {
                            from: MessageFrom::System,
                            text: note.clone(),
                        },
                    );
                }
                let slot = Arc::new(Slot {
                    id: opened.id,
                    harness: want.harness.to_string(),
                    host: host_slot,
                    intro: Mutex::new(opened.system_prompt),
                    live: tokio::sync::Mutex::new(Live {
                        session: Some(session),
                        events,
                    }),
                });
                if let Some(old) = self.sessions().insert(key, Arc::clone(&slot)) {
                    tokio::spawn(shutdown(old));
                }
                Ok(slot)
            }
            Err(e) => {
                host.session_state(&opened.id, SessionState::Ended);
                Err(format!("cannot start the {} harness: {e}", want.harness))
            }
        }
    }

    /// The step's turns: the prompt, and for a review without a verdict, one
    /// reminder.
    async fn work(
        &self,
        task: &AgentTask,
        slot: &Slot,
        live: &mut Live,
        host: &Arc<dyn StepHost>,
    ) -> Result<AgentOutcome, TurnError> {
        let prompt = slot.with_intro(&prompts::step_prompt(task));
        let reply = turn(live, &slot.id, host, &prompt, true).await?;
        match task.step {
            StepKind::Review => {
                let verdict = match prompts::parse_verdict(&reply) {
                    Some(verdict) => verdict,
                    None => {
                        let again =
                            turn(live, &slot.id, host, prompts::VERDICT_REMINDER, true).await?;
                        prompts::parse_verdict(&again).ok_or_else(|| {
                            TurnError::Failed(format!(
                                "the reviewer gave no verdict (expected APPROVE or CHANGES_REQUESTED); it replied: {}",
                                excerpt(&reply)
                            ))
                        })?
                    }
                };
                Ok(match verdict {
                    Verdict::Approve(summary) => AgentOutcome::Done { summary },
                    Verdict::ChangesRequested(comments) => {
                        AgentOutcome::ChangesRequested { comments }
                    }
                })
            }
            StepKind::Plan if reply.trim().is_empty() => {
                Err(TurnError::Failed("the planner replied with no plan".into()))
            }
            _ => Ok(AgentOutcome::Done {
                summary: if reply.trim().is_empty() {
                    "(the agent sent no summary)".into()
                } else {
                    reply.trim().to_string()
                },
            }),
        }
    }

    fn forget(&self, run_id: &str, role: &str, slot: &Arc<Slot>) {
        let key = (run_id.to_string(), role.to_string());
        let mut sessions = self.sessions();
        if sessions.get(&key).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            sessions.remove(&key);
        }
    }

    /// Ends a session whose harness died or broke the protocol: the next
    /// step of the role starts a new one.
    fn broken(&self, run_id: &str, role: &str, slot: Arc<Slot>, host: &Arc<dyn StepHost>) {
        self.forget(run_id, role, &slot);
        host.session_state(&slot.id, SessionState::Ended);
        tokio::spawn(shutdown(slot));
    }

    /// The live session with this id, if any.
    fn find(&self, run_id: &str, session_id: &str) -> Option<(String, Arc<Slot>)> {
        self.sessions()
            .iter()
            .find(|((run, _), slot)| run == run_id && slot.id == session_id)
            .map(|((_, role), slot)| (role.clone(), Arc::clone(slot)))
    }
}

impl StepExecutor for AcpExecutor {
    fn run_agent<'a>(
        &'a self,
        task: &'a AgentTask,
        host: Arc<dyn StepHost>,
    ) -> BoxFuture<'a, Result<AgentOutcome, String>> {
        Box::pin(async move {
            let want = Want {
                run_id: &task.run_id,
                role: &task.role,
                harness: &task.harness,
                model: task.model.as_deref(),
                worktree: &task.worktree,
            };
            let slot = self.slot(&want, &host).await?;
            let result = {
                let mut live = slot.live.lock().await;
                let _host = HostGuard::set(&slot, &host);
                let result = self.work(task, &slot, &mut live, &host).await;
                // Before the next turn (a queued wake) can start.
                if !matches!(result, Err(TurnError::Broken(_))) {
                    host.session_state(&slot.id, SessionState::Idle);
                }
                result
            };
            match result {
                Ok(outcome) => Ok(outcome),
                Err(TurnError::Failed(message)) => Err(message),
                Err(TurnError::Broken(message)) => {
                    self.broken(&task.run_id, &task.role, slot, &host);
                    Err(format!("the {} session failed: {message}", task.harness))
                }
            }
        })
    }

    fn wake<'a>(
        &'a self,
        task: &'a WakeTask,
        host: Arc<dyn StepHost>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if task.human {
                // A session that is still starting is live once this is free.
                drop(self.starting.lock().await);
            }
            let (role, slot) = match &task.session_id {
                Some(id) => match self.find(&task.run_id, id) {
                    Some(found) => found,
                    None if task.human => {
                        return Err("the session ended before it got the message".into());
                    }
                    // The session ended meanwhile; its mail went with it.
                    None => return Ok(()),
                },
                None => {
                    let want = Want {
                        run_id: &task.run_id,
                        role: &task.role,
                        harness: &task.harness,
                        model: task.model.as_deref(),
                        worktree: &task.worktree,
                    };
                    (task.role.clone(), self.slot(&want, &host).await?)
                }
            };
            let result = {
                let mut live = slot.live.lock().await;
                let _host = HostGuard::set(&slot, &host);
                let prompt = slot.with_intro(&task.prompt);
                // The human's message is recorded when it is accepted.
                let result = turn(&mut live, &slot.id, &host, &prompt, !task.human).await;
                if !matches!(result, Err(TurnError::Broken(_))) {
                    host.session_state(&slot.id, SessionState::Idle);
                }
                result
            };
            match result {
                Ok(_) => Ok(()),
                Err(TurnError::Failed(message)) => Err(message),
                Err(TurnError::Broken(message)) => {
                    self.broken(&task.run_id, &role, slot, &host);
                    Err(format!("the {} session failed: {message}", task.harness))
                }
            }
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        task: &'a PullRequestTask,
    ) -> BoxFuture<'a, Result<PullRequestOutcome, String>> {
        Box::pin(forge::open_pull_request(task))
    }

    fn release<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let slots: Vec<Arc<Slot>> = {
                let mut sessions = self.sessions();
                let keys: Vec<_> = sessions.keys().filter(|k| k.0 == run_id).cloned().collect();
                keys.iter().filter_map(|k| sessions.remove(k)).collect()
            };
            for slot in slots {
                shutdown(slot).await;
            }
        })
    }
}

async fn shutdown(slot: Arc<Slot>) {
    let session = slot.live.lock().await.session.take();
    if let Some(session) = session {
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, session.shutdown()).await;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

enum TurnError {
    /// The step failed; the session is fine.
    Failed(String),
    /// The session is unusable.
    Broken(String),
}

/// Sends one prompt and streams the turn's events to the host, recording the
/// prompt as a `user` message when `record` is set. Returns the agent's reply
/// text.
async fn turn(
    live: &mut Live,
    session_id: &str,
    host: &Arc<dyn StepHost>,
    text: &str,
    record: bool,
) -> Result<String, TurnError> {
    let Live { session, events } = live;
    let session = session
        .as_ref()
        .ok_or_else(|| TurnError::Broken("the session is closed".into()))?;
    let mut sink = Sink::new(host, session_id);
    // Whatever arrived between turns.
    while let Ok(event) = events.try_recv() {
        sink.push(event);
    }
    sink.flush();
    sink.reply.clear();

    if record {
        host.session_event(
            session_id,
            SessionEvent::Message {
                from: MessageFrom::User,
                text: text.to_string(),
            },
        );
    }
    host.session_state(session_id, SessionState::Active);
    let mut tick = tokio::time::interval(MESSAGE_FLUSH_EVERY);
    let stop = {
        let prompt = session.prompt(text);
        tokio::pin!(prompt);
        loop {
            tokio::select! {
                stop = &mut prompt => break stop,
                Some(event) = events.recv() => sink.push(event),
                _ = tick.tick() => sink.flush(),
            }
        }
    };
    while let Ok(event) = events.try_recv() {
        sink.push(event);
    }
    sink.flush();
    match stop {
        Err(e) => Err(TurnError::Broken(e.to_string())),
        Ok(StopReason::Refusal) => Err(TurnError::Failed("the agent refused".into())),
        Ok(StopReason::Cancelled) => {
            Err(TurnError::Failed("the agent's turn was cancelled".into()))
        }
        Ok(StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests) => {
            Ok(sink.reply)
        }
    }
}

/// Converts harness events into session events, coalescing message chunks.
struct Sink<'a> {
    host: &'a Arc<dyn StepHost>,
    session_id: &'a str,
    /// Everything the agent said this turn.
    reply: String,
    /// Agent text not yet recorded.
    pending: String,
    /// Whether something other than agent text came since the last text.
    interrupted: bool,
}

impl<'a> Sink<'a> {
    fn new(host: &'a Arc<dyn StepHost>, session_id: &'a str) -> Self {
        Self {
            host,
            session_id,
            reply: String::new(),
            pending: String::new(),
            interrupted: false,
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let text = std::mem::take(&mut self.pending);
            self.emit(SessionEvent::Message {
                from: MessageFrom::Agent,
                text,
            });
        }
    }

    fn emit(&self, event: SessionEvent) {
        self.host.session_event(self.session_id, event);
    }

    fn push(&mut self, event: Event) {
        let event = match event {
            Event::AgentMessage(text) => {
                // Text before and after a tool call are separate paragraphs.
                if std::mem::take(&mut self.interrupted)
                    && !self.reply.is_empty()
                    && !self.reply.ends_with('\n')
                {
                    self.reply.push_str("\n\n");
                }
                self.reply.push_str(&text);
                self.pending.push_str(&text);
                if self.pending.len() >= MESSAGE_FLUSH_BYTES {
                    self.flush();
                }
                return;
            }
            // Reasoning is not recorded.
            Event::AgentThought(_) => return,
            Event::ToolCall(call) => SessionEvent::ToolCall {
                tool_call_id: call.id,
                tool: Some(tool_kind(call.kind)),
                title: Some(call.title),
                status: Some(tool_status(call.status)),
                output: None,
            },
            Event::ToolCallUpdate(update) => SessionEvent::ToolCall {
                tool_call_id: update.id,
                tool: None,
                title: update.title,
                status: update.status.map(tool_status),
                output: update.output.map(|o| truncate(&o)),
            },
            Event::Plan(entries) => SessionEvent::Plan {
                items: entries
                    .into_iter()
                    .map(|entry| PlanItem {
                        text: entry.content,
                        status: match entry.status {
                            PlanStatus::Pending => PlanItemStatus::Pending,
                            PlanStatus::InProgress => PlanItemStatus::InProgress,
                            PlanStatus::Completed => PlanItemStatus::Done,
                        },
                    })
                    .collect(),
            },
            Event::Diff(diff) => SessionEvent::Diff {
                tool_call_id: diff.tool_call_id,
                path: diff.path.to_string_lossy().into_owned(),
                old_text: diff.old_text.map(|t| truncate(&t)),
                new_text: truncate(&diff.new_text),
            },
            Event::Usage(usage) => SessionEvent::Usage {
                usage: SessionUsage {
                    used_tokens: usage.used_tokens,
                    context_tokens: usage.context_tokens,
                    // Other currencies are not converted.
                    cost_usd: usage
                        .cost
                        .filter(|c| c.currency.eq_ignore_ascii_case("USD"))
                        .map(|c| c.amount),
                },
            },
        };
        self.interrupted = true;
        self.flush();
        self.emit(event);
    }
}

/// Routes the session's permission requests to the current step's host.
fn permissions(
    host: HostSlot,
    session_id: String,
    role: String,
    harness: String,
) -> PermissionHandler {
    permission_handler(move |request: PermissionRequest| {
        let current = lock(&host).clone();
        let (title, detail) = describe(&role, &harness, &request);
        let session_id = session_id.clone();
        async move {
            let Some(host) = current else {
                return Decision::Deny;
            };
            if host.ask_permission(&session_id, title, detail).await {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }
    })
}

fn describe(role: &str, harness: &str, request: &PermissionRequest) -> (String, String) {
    use agentux_harness::ToolKind as K;
    let verb = match request.kind {
        Some(K::Read) => "read",
        Some(K::Edit) => "edit",
        Some(K::Delete) => "delete",
        Some(K::Move) => "move",
        Some(K::Search) => "search",
        Some(K::Execute) => "run",
        Some(K::Fetch) => "fetch",
        Some(K::Think) => "think",
        Some(K::Other) | None => "use a tool",
    };
    let what = request
        .title
        .clone()
        .unwrap_or_else(|| format!("tool call {}", request.tool_call_id));
    let title = format!("{role} ({harness}) wants to {verb}: {}", excerpt(&what));
    let detail = format!(
        "{what}\n\nTool call {} ({}). The agent waits for your answer; denying tells it no, and the run goes on.",
        request.tool_call_id,
        request
            .kind
            .map_or("unknown kind", |k| tool_kind(k).as_str()),
    );
    (title, detail)
}

fn tool_kind(kind: agentux_harness::ToolKind) -> ToolKind {
    use agentux_harness::ToolKind as K;
    match kind {
        K::Read => ToolKind::Read,
        K::Edit => ToolKind::Edit,
        K::Delete => ToolKind::Delete,
        K::Move => ToolKind::Move,
        K::Search => ToolKind::Search,
        K::Execute => ToolKind::Execute,
        K::Think => ToolKind::Think,
        K::Fetch => ToolKind::Fetch,
        K::Other => ToolKind::Other,
    }
}

fn tool_status(status: agentux_harness::ToolStatus) -> ToolStatus {
    use agentux_harness::ToolStatus as S;
    match status {
        S::Pending | S::InProgress => ToolStatus::Running,
        S::Completed => ToolStatus::Ok,
        S::Failed => ToolStatus::Error,
    }
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_EVENT_TEXT {
        return text.to_string();
    }
    let mut end = MAX_EVENT_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[…]", &text[..end])
}

/// The first line, cut to 100 characters, for titles and messages.
fn excerpt(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(100) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}
