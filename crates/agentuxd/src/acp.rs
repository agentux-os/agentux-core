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
    PermissionHandler, PermissionRequest, PlanStatus, StopReason, permission_handler,
};

use crate::executor::{
    AgentOutcome, AgentTask, BoxFuture, PullRequestOutcome, PullRequestTask, StepExecutor,
    StepHost,
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

/// Starts harness sessions. The daemon uses [`HarnessLauncher`]; tests plug in
/// in-process agents.
pub trait Launcher: Send + Sync + 'static {
    fn launch<'a>(
        &'a self,
        harness: &'a str,
        cwd: &'a Path,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>>;
}

/// Launches the built-in harnesses ([`HarnessSpec::builtin`]) as ACP agents.
pub struct HarnessLauncher;

impl Launcher for HarnessLauncher {
    fn launch<'a>(
        &'a self,
        harness: &'a str,
        cwd: &'a Path,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            let Some(spec) = HarnessSpec::find(harness) else {
                let known: Vec<String> =
                    HarnessSpec::builtin().into_iter().map(|s| s.id).collect();
                return Err(format!(
                    "unknown harness `{harness}` (known: {})",
                    known.join(", ")
                ));
            };
            AcpHarness::new(spec)
                .start(cwd, permissions)
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
    /// The host of the step currently using the session, which permission
    /// requests go to. `None` between steps: requests are then denied.
    host: HostSlot,
    live: tokio::sync::Mutex<Live>,
}

struct Live {
    session: Option<AcpSession>,
    events: Events,
}

pub struct AcpExecutor {
    launcher: Box<dyn Launcher>,
    /// Keyed by (run id, role).
    sessions: Mutex<HashMap<(String, String), Arc<Slot>>>,
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
        }
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<(String, String), Arc<Slot>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The role's live session, started if needed.
    async fn slot(&self, task: &AgentTask, host: &Arc<dyn StepHost>) -> Result<Arc<Slot>, String> {
        let key = (task.run_id.clone(), task.role.clone());
        if let Some(slot) = self.sessions().get(&key)
            && slot.harness == task.harness
        {
            return Ok(Arc::clone(slot));
        }
        let id = host.open_session(&task.harness, task.model.as_deref())?;
        let host_slot: HostSlot = Arc::default();
        let handler = permissions(
            Arc::clone(&host_slot),
            id.clone(),
            task.role.clone(),
            task.harness.clone(),
        );
        match self
            .launcher
            .launch(&task.harness, &task.worktree, handler)
            .await
        {
            Ok((session, events)) => {
                let slot = Arc::new(Slot {
                    id,
                    harness: task.harness.clone(),
                    host: host_slot,
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
                host.session_state(&id, SessionState::Ended);
                Err(format!("cannot start the {} harness: {e}", task.harness))
            }
        }
    }

    /// The step's turns: the prompt, and for a review without a verdict, one
    /// reminder.
    async fn work(
        &self,
        task: &AgentTask,
        slot: &Slot,
        host: &Arc<dyn StepHost>,
    ) -> Result<AgentOutcome, TurnError> {
        let mut live = slot.live.lock().await;
        let reply = turn(&mut live, &slot.id, host, &prompts::step_prompt(task)).await?;
        match task.step {
            StepKind::Review => {
                let verdict = match prompts::parse_verdict(&reply) {
                    Some(verdict) => verdict,
                    None => {
                        let again =
                            turn(&mut live, &slot.id, host, prompts::VERDICT_REMINDER).await?;
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

    fn forget(&self, task: &AgentTask, slot: &Arc<Slot>) {
        let key = (task.run_id.clone(), task.role.clone());
        let mut sessions = self.sessions();
        if sessions.get(&key).is_some_and(|s| Arc::ptr_eq(s, slot)) {
            sessions.remove(&key);
        }
    }
}

impl StepExecutor for AcpExecutor {
    fn run_agent<'a>(
        &'a self,
        task: &'a AgentTask,
        host: Arc<dyn StepHost>,
    ) -> BoxFuture<'a, Result<AgentOutcome, String>> {
        Box::pin(async move {
            let slot = self.slot(task, &host).await?;
            *lock(&slot.host) = Some(Arc::clone(&host));
            let result = self.work(task, &slot, &host).await;
            *lock(&slot.host) = None;
            match result {
                Ok(outcome) => {
                    host.session_state(&slot.id, SessionState::Idle);
                    Ok(outcome)
                }
                Err(TurnError::Failed(message)) => {
                    host.session_state(&slot.id, SessionState::Idle);
                    Err(message)
                }
                Err(TurnError::Broken(message)) => {
                    // The harness died or broke the protocol: the next step of
                    // this role starts a new session.
                    self.forget(task, &slot);
                    host.session_state(&slot.id, SessionState::Ended);
                    tokio::spawn(shutdown(slot));
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

/// Sends one prompt and streams the turn's events to the host. Returns the
/// agent's reply text.
async fn turn(
    live: &mut Live,
    session_id: &str,
    host: &Arc<dyn StepHost>,
    text: &str,
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

    host.session_event(
        session_id,
        SessionEvent::Message {
            from: MessageFrom::User,
            text: text.to_string(),
        },
    );
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
        Ok(StopReason::Cancelled) => Err(TurnError::Failed("the agent's turn was cancelled".into())),
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
}

impl<'a> Sink<'a> {
    fn new(host: &'a Arc<dyn StepHost>, session_id: &'a str) -> Self {
        Self {
            host,
            session_id,
            reply: String::new(),
            pending: String::new(),
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
        request.kind.map_or("unknown kind", |k| tool_kind(k).as_str()),
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
