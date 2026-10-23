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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    AgentOutcome, AgentTask, BoxFuture, HeldSession, HelperTask, PullRequestOutcome,
    PullRequestTask, StepExecutor, StepHost, WakeTask,
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
    /// Reopen this existing session (ACP `session/load`) instead of creating
    /// one: after the session was open in its harness's TUI.
    pub load: Option<&'a str>,
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
            let Some(mut harness) = HarnessSpec::find(spec.harness) else {
                let known: Vec<String> = HarnessSpec::builtin().into_iter().map(|s| s.id).collect();
                return Err(format!(
                    "unknown harness `{}` (known: {})",
                    spec.harness,
                    known.join(", ")
                ));
            };
            // API keys from the secret service, for this harness only.
            let id = harness.id.clone();
            let keys = tokio::task::spawn_blocking(move || crate::secrets::env_for(&id))
                .await
                .unwrap_or_default();
            harness.env.extend(keys);
            AcpHarness::new(harness)
                .with_mcp_servers(spec.mcp_servers.to_vec())
                .with_model(spec.model.map(str::to_string))
                .with_load(spec.load.map(str::to_string))
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
    /// The harness's id of the session (ACP), which its TUI resumes.
    vendor_id: String,
    role: String,
    harness: String,
    model: Option<String>,
    worktree: PathBuf,
    mcp_servers: Vec<McpServer>,
    /// The agent can reopen the session with `session/load`.
    can_load: bool,
    /// Handed over to the harness's TUI: turns wait (see
    /// [`StepExecutor::take_over`]).
    attached: AtomicBool,
    /// The host of the turn currently running in the session (a step's, or
    /// a bus wake's), which permission requests go to. `None` between turns:
    /// requests are then denied.
    host: HostSlot,
    /// The bus system prompt, prepended to the session's first prompt.
    intro: Mutex<Option<String>>,
    /// Permission requests of the session waiting for the human: the turn
    /// is not stalled while there are any.
    asking: Arc<AtomicUsize>,
    /// Held for a whole turn (or a step's turns): a wake for a session that
    /// is mid-turn waits here. Held as long as the session is open in its
    /// TUI, so turns wait for that too.
    live: Arc<tokio::sync::Mutex<Live>>,
}

struct Live {
    session: Option<AcpSession>,
    events: Events,
}

impl Slot {
    /// Waits for the session to be free for a turn, telling the session's
    /// log when it waits because the session is open in its TUI.
    async fn lock(&self, host: &Arc<dyn StepHost>) -> tokio::sync::MutexGuard<'_, Live> {
        if let Ok(live) = self.live.try_lock() {
            return live;
        }
        if self.attached.load(Ordering::SeqCst) {
            host.session_event(
                &self.id,
                SessionEvent::Message {
                    from: MessageFrom::System,
                    text: "The session is open in its harness's TUI (terminal mode); this turn \
                           waits until the terminal is closed."
                        .into(),
                },
            );
        }
        self.live.lock().await
    }

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
    stall: Arc<dyn Fn() -> StallLimits + Send + Sync>,
}

/// How long a turn may go without a sign of life from the agent before the
/// daemon gives up on it ([`STALLED`]): `idle` with nothing in flight, `tool`
/// while a tool call runs (a long build or test suite says nothing until it
/// ends). Time spent waiting for the human (a permission request, a bus
/// `ask_human`) does not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallLimits {
    pub idle: Duration,
    pub tool: Duration,
}

impl StallLimits {
    /// The defaults, with the user's preferences on top.
    pub fn from_prefs(prefs: &agentux_api::rpc::AgentPrefs) -> Self {
        let mut limits = Self::default();
        if let Some(minutes) = prefs.idle_minutes {
            limits.idle = Duration::from_secs(minutes * 60);
        }
        if let Some(minutes) = prefs.tool_minutes {
            limits.tool = Duration::from_secs(minutes * 60);
        }
        limits
    }
}

impl Default for StallLimits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(10 * 60),
            tool: Duration::from_secs(60 * 60),
        }
    }
}

/// How a stalled turn's error begins; [`crate::limits`] reads it.
pub const STALLED: &str = "the agent stopped responding";

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
            stall: Arc::new(StallLimits::default),
        }
    }

    /// Replaces the default [`StallLimits`].
    pub fn with_stall_limits(mut self, stall: StallLimits) -> Self {
        self.stall = Arc::new(move || stall);
        self
    }

    /// Takes the [`StallLimits`] from `source` at each turn.
    pub fn with_stall_source(
        mut self,
        source: impl Fn() -> StallLimits + Send + Sync + 'static,
    ) -> Self {
        self.stall = Arc::new(source);
        self
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
        let asking = Arc::new(AtomicUsize::new(0));
        let handler = permissions(
            Arc::clone(&host_slot),
            Arc::clone(&asking),
            opened.id.clone(),
            want.role.to_string(),
            want.harness.to_string(),
        );
        let spec = LaunchSpec {
            harness: want.harness,
            cwd: want.worktree,
            model: want.model,
            mcp_servers: &opened.mcp_servers,
            load: None,
        };
        match self.launcher.launch(spec, handler).await {
            Ok((session, events)) => {
                host.session_vendor_id(&opened.id, session.id());
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
                    vendor_id: session.id().to_string(),
                    role: want.role.to_string(),
                    harness: want.harness.to_string(),
                    model: want.model.map(str::to_string),
                    worktree: want.worktree.to_path_buf(),
                    mcp_servers: opened.mcp_servers,
                    can_load: session.can_load(),
                    attached: AtomicBool::new(false),
                    host: host_slot,
                    asking,
                    intro: Mutex::new(opened.system_prompt),
                    live: Arc::new(tokio::sync::Mutex::new(Live {
                        session: Some(session),
                        events,
                    })),
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
        let watch = Watch::new(slot, (self.stall)());
        let reply = turn(live, &slot.id, host, &prompt, true, &watch).await?;
        match task.step {
            StepKind::Review => {
                let verdict = match prompts::parse_verdict(&reply) {
                    Some(verdict) => verdict,
                    None => {
                        let again = turn(
                            live,
                            &slot.id,
                            host,
                            prompts::VERDICT_REMINDER,
                            true,
                            &watch,
                        )
                        .await?;
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

    /// The helper's one turn (see [`crate::helper`]): a fresh session in
    /// `dir`, no MCP servers, every permission request denied, and the turn
    /// cancelled at the first tool call. Returns the agent's reply.
    async fn helper_turn(&self, task: &HelperTask, dir: &Path) -> Result<String, String> {
        let deny = permission_handler(|_: PermissionRequest| async { Decision::Deny });
        let spec = LaunchSpec {
            harness: &task.harness,
            cwd: dir,
            model: task.model.as_deref(),
            mcp_servers: &[],
            load: None,
        };
        let (session, mut events) = self
            .launcher
            .launch(spec, deny)
            .await
            .map_err(|e| format!("cannot start the {} harness: {e}", task.harness))?;
        let mut reply = String::new();
        let mut tool: Option<String> = None;
        let stop = {
            let prompt = session.prompt(&task.prompt);
            tokio::pin!(prompt);
            loop {
                tokio::select! {
                    stop = &mut prompt => break stop,
                    Some(event) = events.recv() => {
                        if let Some(title) = helper_event(event, &mut reply)
                            && tool.is_none()
                        {
                            tool = Some(title);
                            let _ = session.cancel();
                        }
                    }
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            if let Some(title) = helper_event(event, &mut reply) {
                tool.get_or_insert(title);
            }
        }
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, session.shutdown()).await;
        if let Some(title) = tool {
            return Err(format!(
                "{} tried to use a tool ({}); helper answers may not use tools, so the answer was dropped",
                task.harness,
                excerpt(&title)
            ));
        }
        match stop {
            Err(e) => Err(format!("the {} session failed: {e}", task.harness)),
            Ok(StopReason::Refusal) => Err(format!("{} refused to answer", task.harness)),
            Ok(StopReason::Cancelled) => Err(format!("{}'s turn was cancelled", task.harness)),
            Ok(StopReason::EndTurn | StopReason::MaxTokens | StopReason::MaxTurnRequests) => {
                Ok(reply)
            }
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
                let mut live = slot.lock(&host).await;
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
                let mut live = slot.lock(&host).await;
                let _host = HostGuard::set(&slot, &host);
                let prompt = slot.with_intro(&task.prompt);
                // The human's message is recorded when it is accepted.
                let watch = Watch::new(&slot, (self.stall)());
                let result = turn(&mut live, &slot.id, &host, &prompt, !task.human, &watch).await;
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

    fn take_over<'a>(
        &'a self,
        run_id: &'a str,
        session_id: &'a str,
        host: Arc<dyn StepHost>,
    ) -> BoxFuture<'a, Result<HeldSession, String>> {
        Box::pin(async move {
            let (_, slot) = self
                .find(run_id, session_id)
                .ok_or("the session is not running in this daemon")?;
            if !slot.can_load {
                return Err(format!(
                    "the {} agent cannot reopen the session over ACP afterwards (no `loadSession`)",
                    slot.harness
                ));
            }
            let mut live = Arc::clone(&slot.live).lock_owned().await;
            // Ended while we waited.
            if self.find(run_id, session_id).is_none() || live.session.is_none() {
                return Err("the session ended".into());
            }
            slot.attached.store(true, Ordering::SeqCst);
            // One writer at a time: the TUI owns the session until it is
            // given back, then the session is reopened over ACP.
            if let Some(session) = live.session.take() {
                let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, session.shutdown()).await;
            }
            host.session_state(&slot.id, SessionState::Attached);
            Ok(HeldSession {
                vendor_session_id: slot.vendor_id.clone(),
                hold: Box::new(Hold { slot, live }),
            })
        })
    }

    fn give_back<'a>(&'a self, held: HeldSession, host: Arc<dyn StepHost>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok(hold) = held.hold.downcast::<Hold>() else {
                return;
            };
            let Hold { slot, mut live } = *hold;
            let run_id = self
                .sessions()
                .iter()
                .find(|(_, s)| Arc::ptr_eq(s, &slot))
                .map(|((run, _), _)| run.clone());
            let Some(run_id) = run_id else {
                // Released (the run ended) while the TUI had it.
                slot.attached.store(false, Ordering::SeqCst);
                return;
            };
            let handler = permissions(
                Arc::clone(&slot.host),
                Arc::clone(&slot.asking),
                slot.id.clone(),
                slot.role.clone(),
                slot.harness.clone(),
            );
            let spec = LaunchSpec {
                harness: &slot.harness,
                cwd: &slot.worktree,
                model: slot.model.as_deref(),
                mcp_servers: &slot.mcp_servers,
                load: Some(&slot.vendor_id),
            };
            let launched = self.launcher.launch(spec, handler).await;
            slot.attached.store(false, Ordering::SeqCst);
            match launched {
                Ok((session, mut events)) => {
                    // Anything sent while loading is the replayed past.
                    while events.try_recv().is_ok() {}
                    live.session = Some(session);
                    live.events = events;
                    host.session_state(&slot.id, SessionState::Idle);
                    drop(live);
                }
                Err(e) => {
                    host.session_event(
                        &slot.id,
                        SessionEvent::Message {
                            from: MessageFrom::System,
                            text: format!(
                                "The session could not be reopened after the terminal: {e}. \
                                 The role's next turn starts a new session."
                            ),
                        },
                    );
                    drop(live);
                    let role = slot.role.clone();
                    self.broken(&run_id, &role, slot, &host);
                }
            }
        })
    }

    fn ask_helper<'a>(&'a self, task: &'a HelperTask) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let dir =
                helper_dir().map_err(|e| format!("cannot create the helper's directory: {e}"))?;
            let result = self.helper_turn(task, &dir).await;
            let _ = std::fs::remove_dir_all(&dir);
            result
        })
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

/// What [`AcpExecutor::take_over`] keeps while the TUI has the session.
struct Hold {
    slot: Arc<Slot>,
    live: tokio::sync::OwnedMutexGuard<Live>,
}

/// Adds a helper turn's agent text to `reply`; returns the title of a tool
/// call the agent started (which ends the helper's turn).
fn helper_event(event: Event, reply: &mut String) -> Option<String> {
    match event {
        Event::AgentMessage(text) => {
            reply.push_str(&text);
            None
        }
        Event::ToolCall(call) => Some(call.title),
        Event::Diff(diff) => Some(format!("edit {}", diff.path.display())),
        _ => None,
    }
}

/// A new, empty directory only the daemon's user can enter, for a helper
/// session to start in.
fn helper_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = base.join(format!(
        "agentux-helper-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
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

/// What a turn's stall check needs from its session.
struct Watch {
    limits: StallLimits,
    asking: Arc<AtomicUsize>,
}

impl Watch {
    fn new(slot: &Slot, limits: StallLimits) -> Self {
        Self {
            limits,
            asking: Arc::clone(&slot.asking),
        }
    }

    fn waiting_for_human(&self) -> bool {
        self.asking.load(Ordering::SeqCst) > 0
    }
}

/// The agent's last sign of life in a turn, and its tool calls in flight.
struct Alive {
    last: tokio::time::Instant,
    /// Tool call id to title.
    tools: HashMap<String, String>,
}

impl Alive {
    fn new() -> Self {
        Self {
            last: tokio::time::Instant::now(),
            tools: HashMap::new(),
        }
    }

    fn touch(&mut self) {
        self.last = tokio::time::Instant::now();
    }

    fn saw(&mut self, event: &Event) {
        self.touch();
        match event {
            Event::ToolCall(call) => {
                use agentux_harness::ToolStatus as S;
                if !matches!(call.status, S::Completed | S::Failed) {
                    self.tools.insert(call.id.clone(), call.title.clone());
                }
            }
            Event::ToolCallUpdate(update) => {
                use agentux_harness::ToolStatus as S;
                if matches!(update.status, Some(S::Completed | S::Failed)) {
                    self.tools.remove(&update.id);
                }
            }
            _ => {}
        }
    }

    /// The agent asked the human on the bus (`ask_human`) and waits.
    fn asking_human(&self) -> bool {
        self.tools.values().any(|title| title.contains("ask_human"))
    }

    fn limit(&self, watch: &Watch) -> Duration {
        if self.tools.is_empty() {
            watch.limits.idle
        } else {
            watch.limits.tool
        }
    }

    fn deadline(&self, watch: &Watch) -> tokio::time::Instant {
        self.last + self.limit(watch)
    }

    fn stalled(&self, watch: &Watch) -> String {
        let minutes = self.limit(watch).as_secs().div_ceil(60);
        let what = match self.tools.values().next() {
            Some(title) => format!(" while running {}", excerpt(title)),
            None => String::new(),
        };
        format!("{STALLED}: no activity for {minutes} minute(s){what}")
    }
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
    watch: &Watch,
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
    let mut tick = tokio::time::interval_at(
        tokio::time::Instant::now() + MESSAGE_FLUSH_EVERY,
        MESSAGE_FLUSH_EVERY,
    );
    let mut alive = Alive::new();
    let stop = {
        let prompt = session.prompt(text);
        tokio::pin!(prompt);
        loop {
            let deadline = alive.deadline(watch);
            tokio::select! {
                stop = &mut prompt => break Ok(stop),
                Some(event) = events.recv() => {
                    alive.saw(&event);
                    sink.push(event);
                }
                _ = tick.tick() => sink.flush(),
                _ = tokio::time::sleep_until(deadline) => {
                    if watch.waiting_for_human() || alive.asking_human() {
                        alive.touch();
                        continue;
                    }
                    // Give the agent a chance to end the turn cleanly.
                    let _ = session.cancel();
                    let _ = tokio::time::timeout(Duration::from_secs(30), &mut prompt).await;
                    break Err(alive.stalled(watch));
                }
            }
        }
    };
    while let Ok(event) = events.try_recv() {
        sink.push(event);
    }
    sink.flush();
    let stop = match stop {
        Ok(stop) => stop,
        // The session is not trusted any more: the next turn gets a new one.
        Err(message) => return Err(TurnError::Broken(message)),
    };
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
    asking: Arc<AtomicUsize>,
    session_id: String,
    role: String,
    harness: String,
) -> PermissionHandler {
    permission_handler(move |request: PermissionRequest| {
        let current = lock(&host).clone();
        let (title, detail) = describe(&role, &harness, &request);
        let kind = request.kind.map(tool_kind);
        let role = role.clone();
        let session_id = session_id.clone();
        let asking = Arc::clone(&asking);
        async move {
            let Some(host) = current else {
                return Decision::Deny;
            };
            asking.fetch_add(1, Ordering::SeqCst);
            let allowed = host
                .ask_permission(&session_id, &role, kind, title, detail)
                .await;
            asking.fetch_sub(1, Ordering::SeqCst);
            if allowed {
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
