//! The bus itself: sessions, mailboxes, routing, limits and the audit log.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use agentux_config::{BusTool, Config, Step};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::backend::BusBackend;
use crate::tools::{
    AskHuman, BusCall, BusReply, Handoff, HumanReply, HumanReplyStatus, Inbox, PostMessage, Posted,
    ReadMessages, RequestReview, RunStateReply, SessionInfo,
};
use crate::types::{
    BusEvent, EventKind, HumanQuestion, Message, MessageKind, Participant, RunId, SessionId,
    SessionIdentity, Target, Wake, WakeTarget, now_ms,
};

/// Default and maximum number of messages one `read_messages` call returns.
const READ_DEFAULT: u32 = 20;
const READ_MAX: u32 = 100;

/// How long `ask_human` waits for an answer before reporting the question as
/// pending, unless configured otherwise.
pub const DEFAULT_ASK_HUMAN_WAIT: Duration = Duration::from_secs(120);

/// The bus settings of one run, taken from the project's `agentux.yaml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusConfig {
    /// Turns (messages) one exchange may have before posts in it are refused.
    pub max_turns_per_exchange: u32,
    /// Tools sessions may call. `read_messages` is implied by `post_message`:
    /// it is the receiving half of the same conversation.
    pub allow: Vec<BusTool>,
    /// Roles that exist in the run. Empty means any role name is accepted.
    pub roles: BTreeSet<String>,
    /// Role `request_review` asks when the caller names none.
    pub review_role: Option<String>,
    /// How long `ask_human` waits before answering `pending`.
    pub ask_human_wait: Duration,
}

impl Default for BusConfig {
    fn default() -> Self {
        let bus = agentux_config::Bus::default();
        Self {
            max_turns_per_exchange: bus.max_turns_per_exchange,
            allow: bus.allow,
            roles: BTreeSet::new(),
            review_role: None,
            ask_human_wait: DEFAULT_ASK_HUMAN_WAIT,
        }
    }
}

impl BusConfig {
    /// The `bus` section, roles and review step of a validated `agentux.yaml`.
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_turns_per_exchange: config.bus.max_turns_per_exchange,
            allow: config.bus.allow.clone(),
            roles: config.roles.keys().cloned().collect(),
            review_role: config.pipeline.iter().find_map(|step| match step {
                Step::Review { role, .. } => Some(role.clone()),
                _ => None,
            }),
            ask_human_wait: DEFAULT_ASK_HUMAN_WAIT,
        }
    }

    pub fn allows(&self, tool: BusTool) -> bool {
        self.allow.contains(&tool)
            || (tool == BusTool::ReadMessages && self.allow.contains(&BusTool::PostMessage))
    }

    /// The tools a session of this run gets, in a stable order.
    pub fn allowed_tools(&self) -> Vec<BusTool> {
        BusTool::ALL
            .into_iter()
            .filter(|tool| self.allows(*tool))
            .collect()
    }
}

/// Why a bus operation was refused. The text is what the calling agent reads
/// as the tool error, so it says what to do next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum BusError {
    ToolNotAllowed { tool: String },
    TurnLimit { exchange: u64, max_turns: u32 },
    UnknownRun { run: RunId },
    RunAlreadyOpen { run: RunId },
    UnknownSession { session: SessionId },
    SessionAlreadyJoined { session: SessionId },
    SessionLeft { session: SessionId, role: String },
    UnknownRole { role: String, known: Vec<String> },
    OnlySessionInRole { role: String },
    MessageToSelf,
    UnknownMessage { id: u64 },
    NoReviewer,
    Invalid { reason: String },
    Backend { reason: String },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ToolNotAllowed { tool } => write!(
                f,
                "`{tool}` is not allowed in this project (bus.allow in agentux.yaml). Continue without it."
            ),
            Self::TurnLimit {
                exchange,
                max_turns,
            } => write!(
                f,
                "exchange {exchange} reached its limit of {max_turns} turns (bus.max_turns_per_exchange). \
                 Do not send more messages in it: decide with what you have and continue your task, \
                 or call ask_human if you are blocked."
            ),
            Self::UnknownRun { run } => write!(f, "run `{run}` is not open on the bus"),
            Self::RunAlreadyOpen { run } => write!(f, "run `{run}` is already open on the bus"),
            Self::UnknownSession { session } => write!(
                f,
                "there is no session `{session}` in this run. Call get_run_state to see the sessions, or address a role with `role:<name>`."
            ),
            Self::SessionAlreadyJoined { session } => {
                write!(f, "session `{session}` is already on the bus")
            }
            Self::SessionLeft { session, role } => write!(
                f,
                "session `{session}` has left the bus. Address its role instead: `role:{role}`."
            ),
            Self::UnknownRole { role, known } => write!(
                f,
                "there is no role `{role}` in this project. Roles: {}.",
                known.join(", ")
            ),
            Self::OnlySessionInRole { role } => write!(
                f,
                "you are the only session playing `{role}`; there is nobody else to address there"
            ),
            Self::MessageToSelf => f.write_str("you cannot send a message to yourself"),
            Self::UnknownMessage { id } => write!(
                f,
                "there is no message {id} in this run; check the id you passed as in_reply_to"
            ),
            Self::NoReviewer => f.write_str(
                "the pipeline has no review step, so there is no default reviewer: pass reviewer_role",
            ),
            Self::Invalid { reason } => f.write_str(reason),
            Self::Backend { reason } => write!(f, "the AgentUX daemon could not answer: {reason}"),
        }
    }
}

impl std::error::Error for BusError {}

/// The agent bus (ADR 0004). Cheap to clone; clones share the same state.
///
/// The daemon opens a run with [`Bus::open_run`], adds each harness session
/// with [`Bus::join`] and serves it an MCP server scoped to its identity
/// ([`crate::BusServer`]). Everything the bus does is reported on the
/// channels from [`Bus::subscribe`], including the [`Wake`]s the daemon must
/// turn into prompts.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Arc<dyn BusBackend>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    next_message: u64,
    next_exchange: u64,
    next_question: u64,
    runs: BTreeMap<RunId, RunEntry>,
    sessions: BTreeMap<SessionId, SessionEntry>,
    /// Messages for roles no live session plays yet.
    role_queues: BTreeMap<(RunId, String), VecDeque<Message>>,
    /// Every message sent, for replies.
    sent: HashMap<u64, Sent>,
    /// Turns used per exchange.
    exchanges: HashMap<u64, u32>,
    /// Questions waiting for the human.
    questions: BTreeMap<u64, HumanQuestion>,
    subscribers: Vec<mpsc::UnboundedSender<BusEvent>>,
}

struct RunEntry {
    project: String,
    config: BusConfig,
}

struct SessionEntry {
    identity: SessionIdentity,
    mailbox: VecDeque<Message>,
}

struct Sent {
    run: RunId,
    exchange: u64,
    from: Participant,
}

/// Who sends a message and under which limit.
struct Sender {
    from: Participant,
    session: Option<SessionId>,
    /// `None` for the human, who is never cut off.
    max_turns: Option<u32>,
}

impl State {
    fn emit(&mut self, run: &RunId, kind: EventKind) {
        self.seq += 1;
        let event = BusEvent {
            seq: self.seq,
            at_ms: now_ms(),
            run: run.clone(),
            kind,
        };
        self.subscribers
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }

    fn run(&self, run: &RunId) -> Result<&RunEntry, BusError> {
        self.runs
            .get(run)
            .ok_or_else(|| BusError::UnknownRun { run: run.clone() })
    }

    fn session(&self, session: &SessionId) -> Result<&SessionEntry, BusError> {
        self.sessions
            .get(session)
            .ok_or_else(|| BusError::UnknownSession {
                session: session.clone(),
            })
    }

    /// The caller's identity and run config, after checking it may use `tool`.
    fn authorize(
        &mut self,
        caller: &SessionId,
        tool: BusTool,
    ) -> Result<(SessionIdentity, BusConfig), BusError> {
        let identity = self.session(caller)?.identity.clone();
        let config = self.run(&identity.run)?.config.clone();
        if !config.allows(tool) {
            self.emit(
                &identity.run,
                EventKind::ToolDenied {
                    session: caller.clone(),
                    tool: tool.as_str().to_string(),
                },
            );
            return Err(BusError::ToolNotAllowed {
                tool: tool.as_str().to_string(),
            });
        }
        Ok((identity, config))
    }

    fn sessions_in<'a>(&'a self, run: &'a RunId) -> impl Iterator<Item = &'a SessionIdentity> {
        self.sessions
            .values()
            .map(|entry| &entry.identity)
            .filter(move |identity| &identity.run == run)
    }

    /// Validates, counts and delivers one message.
    fn route(
        &mut self,
        run: &RunId,
        sender: Sender,
        to: Option<Target>,
        kind: MessageKind,
        body: String,
        in_reply_to: Option<u64>,
    ) -> Result<Posted, BusError> {
        let config = self.run(run)?.config.clone();
        if body.trim().is_empty() {
            return Err(BusError::Invalid {
                reason: "the message body is empty".into(),
            });
        }

        // The exchange and, for replies, the default recipient.
        let replied = match in_reply_to {
            Some(id) => match self.sent.get(&id) {
                Some(sent) if &sent.run == run => Some((sent.exchange, sent.from.clone())),
                _ => return Err(BusError::UnknownMessage { id }),
            },
            None => None,
        };
        let to = match (to, &replied) {
            (Some(to), _) => to,
            (None, Some((_, Participant::Session { session, .. }))) => {
                Target::Session(session.clone())
            }
            (None, Some((_, Participant::Human))) => Target::Human,
            (None, None) => {
                return Err(BusError::Invalid {
                    reason: "`to` is required unless you reply with `in_reply_to`".into(),
                });
            }
        };

        // Who receives it.
        let mut delivered_to = Vec::new();
        let mut queued_for_role = None;
        let mut wake = false;
        match &to {
            Target::Session(id) => {
                if sender.session.as_ref() == Some(id) {
                    return Err(BusError::MessageToSelf);
                }
                match self.sessions.get(id) {
                    Some(entry) if &entry.identity.run == run => {}
                    _ => {
                        // Answering a session that has left: point at its role.
                        if let Some((_, Participant::Session { session, role, .. })) = &replied
                            && session == id
                        {
                            return Err(BusError::SessionLeft {
                                session: id.clone(),
                                role: role.clone(),
                            });
                        }
                        return Err(BusError::UnknownSession {
                            session: id.clone(),
                        });
                    }
                }
                delivered_to.push(id.clone());
                wake = true;
            }
            Target::Role(role) => {
                if !config.roles.is_empty() && !config.roles.contains(role) {
                    return Err(BusError::UnknownRole {
                        role: role.clone(),
                        known: config.roles.iter().cloned().collect(),
                    });
                }
                let mut players = self
                    .sessions_in(run)
                    .filter(|identity| &identity.role == role)
                    .map(|identity| identity.session.clone())
                    .peekable();
                if players.peek().is_none() {
                    queued_for_role = Some(role.clone());
                } else {
                    delivered_to = players
                        .filter(|session| Some(session) != sender.session.as_ref())
                        .collect();
                    if delivered_to.is_empty() {
                        return Err(BusError::OnlySessionInRole { role: role.clone() });
                    }
                }
                wake = true;
            }
            Target::Run => {
                delivered_to = self
                    .sessions_in(run)
                    .map(|identity| identity.session.clone())
                    .filter(|session| Some(session) != sender.session.as_ref())
                    .collect();
            }
            Target::Human => {}
        }

        // Turn limit.
        let (exchange, turn) = match &replied {
            Some((exchange, _)) => {
                let used = self.exchanges.get(exchange).copied().unwrap_or(0);
                if let Some(max_turns) = sender.max_turns
                    && used >= max_turns
                {
                    if let Some(session) = &sender.session {
                        self.emit(
                            run,
                            EventKind::TurnLimitReached {
                                session: session.clone(),
                                exchange: *exchange,
                                max_turns,
                            },
                        );
                    }
                    return Err(BusError::TurnLimit {
                        exchange: *exchange,
                        max_turns,
                    });
                }
                (*exchange, used + 1)
            }
            None => {
                self.next_exchange += 1;
                (self.next_exchange, 1)
            }
        };

        // Commit.
        self.next_message += 1;
        let message = Message {
            id: self.next_message,
            exchange,
            turn,
            run: run.clone(),
            from: sender.from.clone(),
            to,
            kind,
            body,
            in_reply_to,
            sent_at_ms: now_ms(),
        };
        self.exchanges.insert(exchange, turn);
        self.sent.insert(
            message.id,
            Sent {
                run: run.clone(),
                exchange,
                from: sender.from,
            },
        );
        for session in &delivered_to {
            if let Some(entry) = self.sessions.get_mut(session) {
                entry.mailbox.push_back(message.clone());
            }
        }
        if let Some(role) = &queued_for_role {
            self.role_queues
                .entry((run.clone(), role.clone()))
                .or_default()
                .push_back(message.clone());
        }
        self.emit(
            run,
            EventKind::MessagePosted {
                message: message.clone(),
                delivered_to: delivered_to.clone(),
                queued_for_role: queued_for_role.clone(),
            },
        );
        if wake {
            let prompt = wake_prompt(&message, config.max_turns_per_exchange);
            let targets: Vec<WakeTarget> = match &queued_for_role {
                Some(role) => vec![WakeTarget::Role(role.clone())],
                None => delivered_to
                    .iter()
                    .cloned()
                    .map(WakeTarget::Session)
                    .collect(),
            };
            for target in targets {
                self.emit(
                    run,
                    EventKind::Wake(Wake {
                        run: run.clone(),
                        target,
                        message: message.id,
                        reason: kind,
                        prompt: prompt.clone(),
                    }),
                );
            }
        }

        Ok(Posted {
            message_id: message.id,
            exchange,
            turn,
            turns_left: config.max_turns_per_exchange.saturating_sub(turn),
            delivered_to,
            queued_for_role,
        })
    }
}

/// The prompt that wakes a role whose messages were queued again after a
/// daemon restart.
pub fn restored_prompt(count: usize) -> String {
    format!(
        "[agentux bus] {count} message(s) sent to your role before AgentUX restarted are waiting for you.          Call read_messages to read them; some may already have been handled by the session that          played your role before. Act on what is still relevant."
    )
}

/// What [`Bus::restore`] takes from a run's persisted log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restore {
    /// Turns used per exchange.
    pub exchanges: Vec<(u64, u32)>,
    /// Every message sent in the run, for replies.
    pub sent: Vec<RestoredSent>,
    /// Messages waiting for a role, oldest first.
    pub queued: Vec<(String, Message)>,
    /// Sessions of the previous daemon that never left the bus.
    pub gone: Vec<GoneSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredSent {
    pub message: u64,
    pub exchange: u64,
    pub from: Participant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoneSession {
    pub session: SessionId,
    pub role: String,
    /// How many of `Restore::queued` were delivered to it.
    pub requeued: usize,
}

/// The prompt that wakes a session for `message`.
pub fn wake_prompt(message: &Message, max_turns: u32) -> String {
    let id = message.id;
    let from = &message.from;
    match message.kind {
        MessageKind::Message => format!(
            "[agentux bus] New message from {from} (message {id}, exchange {}, turn {} of {max_turns}). \
             Call read_messages to read it and act on it. Reply only if a reply is needed.",
            message.exchange, message.turn
        ),
        MessageKind::ReviewRequest => format!(
            "[agentux bus] {from} asks for your review (message {id}). Call read_messages for the details, \
             review the changes on the run's branch, and answer with post_message using in_reply_to={id}."
        ),
        MessageKind::Handoff => format!(
            "[agentux bus] {from} handed the task over to you (message {id}). \
             Call read_messages for the summary and pointers, then continue the task."
        ),
        MessageKind::HumanAnswer => format!(
            "[agentux bus] The human answered your question (message {id}). Call read_messages to read the answer."
        ),
    }
}

impl Bus {
    pub fn new(backend: Arc<dyn BusBackend>) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().expect("bus state lock")
    }

    /// A channel receiving every event from now on: the audit log, and the
    /// wakes the daemon must act on.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<BusEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().subscribers.push(tx);
        rx
    }

    pub fn open_run(
        &self,
        run: RunId,
        project: impl Into<String>,
        config: BusConfig,
    ) -> Result<(), BusError> {
        let mut state = self.lock();
        if state.runs.contains_key(&run) {
            return Err(BusError::RunAlreadyOpen { run });
        }
        state.runs.insert(
            run,
            RunEntry {
                project: project.into(),
                config,
            },
        );
        Ok(())
    }

    /// Makes message, exchange and question ids start after these values. A
    /// daemon that reopens a run after a restart passes the highest ids in
    /// the run's persisted log, so new ids never repeat old ones.
    pub fn reserve_ids(&self, message: u64, exchange: u64, question: u64) {
        let mut state = self.lock();
        state.next_message = state.next_message.max(message);
        state.next_exchange = state.next_exchange.max(exchange);
        state.next_question = state.next_question.max(question);
    }

    /// Picks up a run's state from its persisted log after a daemon restart
    /// (call it after [`Bus::open_run`], before sessions join): turns used
    /// per exchange, who sent each message (so replies route and count), and
    /// the messages waiting for a role. Sessions of the previous daemon are
    /// gone: each in `restore.gone` is reported as having left, with the
    /// messages queued again for its role. Each role with waiting messages
    /// gets a [`Wake`], so the daemon starts a session for it.
    pub fn restore(&self, run: &RunId, restore: Restore) -> Result<(), BusError> {
        let mut state = self.lock();
        state.run(run)?;
        for (exchange, turns) in restore.exchanges {
            let used = state.exchanges.entry(exchange).or_insert(0);
            *used = (*used).max(turns);
        }
        for sent in restore.sent {
            state.next_message = state.next_message.max(sent.message);
            state.next_exchange = state.next_exchange.max(sent.exchange);
            state.sent.insert(
                sent.message,
                Sent {
                    run: run.clone(),
                    exchange: sent.exchange,
                    from: sent.from,
                },
            );
        }
        for gone in restore.gone {
            state.emit(
                run,
                EventKind::SessionLeft {
                    session: gone.session,
                    unread: gone.requeued,
                    requeued_for: Some(gone.role),
                },
            );
        }
        let mut roles: Vec<String> = Vec::new();
        for (role, message) in restore.queued {
            if !roles.contains(&role) {
                roles.push(role.clone());
            }
            state
                .role_queues
                .entry((run.clone(), role))
                .or_default()
                .push_back(message);
        }
        for role in roles {
            let queue = &state.role_queues[&(run.clone(), role.clone())];
            let (count, last) = (queue.len(), queue.back().cloned());
            let Some(last) = last else { continue };
            state.emit(
                run,
                EventKind::Wake(Wake {
                    run: run.clone(),
                    target: WakeTarget::Role(role),
                    message: last.id,
                    reason: last.kind,
                    prompt: restored_prompt(count),
                }),
            );
        }
        Ok(())
    }

    /// Removes a run, its sessions, queues and pending questions.
    pub fn close_run(&self, run: &RunId) {
        let mut state = self.lock();
        state.runs.remove(run);
        state.sessions.retain(|_, entry| &entry.identity.run != run);
        state.role_queues.retain(|(r, _), _| r != run);
        state.questions.retain(|_, question| &question.run != run);
        state.sent.retain(|_, sent| &sent.run != run);
    }

    /// Adds a session to its run. Messages waiting for its role move to its
    /// mailbox; returns how many.
    pub fn join(&self, identity: SessionIdentity) -> Result<usize, BusError> {
        let mut state = self.lock();
        let entry = state.run(&identity.run)?;
        if entry.project != identity.project {
            return Err(BusError::Invalid {
                reason: format!(
                    "run `{}` belongs to project `{}`, not `{}`",
                    identity.run, entry.project, identity.project
                ),
            });
        }
        if !entry.config.roles.is_empty() && !entry.config.roles.contains(&identity.role) {
            return Err(BusError::UnknownRole {
                role: identity.role.clone(),
                known: entry.config.roles.iter().cloned().collect(),
            });
        }
        if state.sessions.contains_key(&identity.session) {
            return Err(BusError::SessionAlreadyJoined {
                session: identity.session,
            });
        }
        let mailbox = state
            .role_queues
            .remove(&(identity.run.clone(), identity.role.clone()))
            .unwrap_or_default();
        let queued = mailbox.len();
        state.sessions.insert(
            identity.session.clone(),
            SessionEntry {
                identity: identity.clone(),
                mailbox,
            },
        );
        let run = identity.run.clone();
        state.emit(&run, EventKind::SessionJoined { identity, queued });
        Ok(queued)
    }

    /// Removes a session; its unread messages are dropped.
    pub fn leave(&self, session: &SessionId) {
        let mut state = self.lock();
        if let Some(entry) = state.sessions.remove(session) {
            state.emit(
                &entry.identity.run,
                EventKind::SessionLeft {
                    session: session.clone(),
                    unread: entry.mailbox.len(),
                    requeued_for: None,
                },
            );
        }
    }

    /// Whether `session` is on the bus.
    pub fn has_session(&self, session: &SessionId) -> bool {
        self.lock().sessions.contains_key(session)
    }

    pub fn identity(&self, session: &SessionId) -> Option<SessionIdentity> {
        self.lock()
            .sessions
            .get(session)
            .map(|entry| entry.identity.clone())
    }

    /// The tools `session` may call.
    pub fn allowed_tools(&self, session: &SessionId) -> Result<Vec<BusTool>, BusError> {
        let state = self.lock();
        let run = &state.session(session)?.identity.run;
        Ok(state.run(run)?.config.allowed_tools())
    }

    /// The run's bus settings.
    pub fn config(&self, run: &RunId) -> Result<BusConfig, BusError> {
        Ok(self.lock().run(run)?.config.clone())
    }

    /// Runs one tool call on behalf of `caller`.
    pub async fn call(&self, caller: &SessionId, call: BusCall) -> Result<BusReply, BusError> {
        Ok(match call {
            BusCall::PostMessage(args) => BusReply::Posted(self.post_message(caller, args)?),
            BusCall::ReadMessages(args) => BusReply::Inbox(self.read_messages(caller, args)?),
            BusCall::RequestReview(args) => {
                BusReply::Posted(self.request_review(caller, args).await?)
            }
            BusCall::Handoff(args) => BusReply::Posted(self.handoff(caller, args)?),
            BusCall::GetRunState => BusReply::RunState(Box::new(self.get_run_state(caller).await?)),
            BusCall::AskHuman(args) => BusReply::Human(self.ask_human(caller, args).await?),
        })
    }

    pub fn post_message(&self, caller: &SessionId, args: PostMessage) -> Result<Posted, BusError> {
        let mut state = self.lock();
        let (identity, config) = state.authorize(caller, BusTool::PostMessage)?;
        state.route(
            &identity.run,
            Sender {
                from: identity.participant(),
                session: Some(caller.clone()),
                max_turns: Some(config.max_turns_per_exchange),
            },
            args.to,
            MessageKind::Message,
            args.body,
            args.in_reply_to,
        )
    }

    /// A message from the human, e.g. typed in the cockpit or answering a
    /// message sent to `human`. The human is never cut off by turn limits.
    pub fn post_from_human(&self, run: &RunId, args: PostMessage) -> Result<Posted, BusError> {
        self.lock().route(
            run,
            Sender {
                from: Participant::Human,
                session: None,
                max_turns: None,
            },
            args.to,
            MessageKind::Message,
            args.body,
            args.in_reply_to,
        )
    }

    pub fn read_messages(&self, caller: &SessionId, args: ReadMessages) -> Result<Inbox, BusError> {
        let mut state = self.lock();
        state.authorize(caller, BusTool::ReadMessages)?;
        let limit = args.limit.unwrap_or(READ_DEFAULT).clamp(1, READ_MAX) as usize;
        let entry = state
            .sessions
            .get_mut(caller)
            .expect("authorized session exists");
        let take = limit.min(entry.mailbox.len());
        let messages: Vec<Message> = entry.mailbox.drain(..take).collect();
        Ok(Inbox {
            messages,
            remaining: entry.mailbox.len(),
        })
    }

    pub async fn request_review(
        &self,
        caller: &SessionId,
        args: RequestReview,
    ) -> Result<Posted, BusError> {
        let (identity, config) = self.lock().authorize(caller, BusTool::RequestReview)?;
        let reviewer = args
            .reviewer_role
            .or(config.review_role.clone())
            .ok_or(BusError::NoReviewer)?;
        // The branch helps the reviewer find the diff; it is best effort.
        let branch = self
            .inner
            .backend
            .run_state(&identity.run)
            .await
            .ok()
            .and_then(|state| state.branch);
        let mut body = format!("Review request: {}", args.summary.trim());
        if let Some(branch) = branch {
            body.push_str(&format!("\nBranch: {branch}"));
        }
        self.lock().route(
            &identity.run,
            Sender {
                from: identity.participant(),
                session: Some(caller.clone()),
                max_turns: Some(config.max_turns_per_exchange),
            },
            Some(Target::Role(reviewer)),
            MessageKind::ReviewRequest,
            body,
            None,
        )
    }

    pub fn handoff(&self, caller: &SessionId, args: Handoff) -> Result<Posted, BusError> {
        let mut state = self.lock();
        let (identity, config) = state.authorize(caller, BusTool::Handoff)?;
        let mut body = format!("Handoff: {}", args.summary.trim());
        if !args.pointers.is_empty() {
            body.push_str("\nPointers:");
            for pointer in &args.pointers {
                body.push_str(&format!("\n- {pointer}"));
            }
        }
        state.route(
            &identity.run,
            Sender {
                from: identity.participant(),
                session: Some(caller.clone()),
                max_turns: Some(config.max_turns_per_exchange),
            },
            Some(Target::Role(args.to_role)),
            MessageKind::Handoff,
            body,
            None,
        )
    }

    pub async fn get_run_state(&self, caller: &SessionId) -> Result<RunStateReply, BusError> {
        let (identity, _) = self.lock().authorize(caller, BusTool::GetRunState)?;
        let run_state = self
            .inner
            .backend
            .run_state(&identity.run)
            .await
            .map_err(|e| BusError::Backend { reason: e.0 })?;
        let state = self.lock();
        let info = |identity: &SessionIdentity| SessionInfo {
            session: identity.session.clone(),
            role: identity.role.clone(),
            vendor: identity.vendor.clone(),
        };
        Ok(RunStateReply {
            run: identity.run.clone(),
            project: identity.project.clone(),
            you: info(&identity),
            state: run_state,
            sessions: state.sessions_in(&identity.run).map(info).collect(),
            unread_messages: state
                .sessions
                .get(caller)
                .map_or(0, |entry| entry.mailbox.len()),
            pending_questions: state
                .questions
                .values()
                .filter(|question| question.run == identity.run)
                .cloned()
                .collect(),
        })
    }

    /// Escalates to the human through the backend. Waits up to the run's
    /// `ask_human_wait`; a later answer is delivered to the caller's mailbox
    /// with a wake.
    pub async fn ask_human(
        &self,
        caller: &SessionId,
        args: AskHuman,
    ) -> Result<HumanReply, BusError> {
        if args.question.trim().is_empty() {
            return Err(BusError::Invalid {
                reason: "the question is empty".into(),
            });
        }
        let (question, wait) = {
            let mut state = self.lock();
            let (identity, config) = state.authorize(caller, BusTool::AskHuman)?;
            state.next_question += 1;
            let question = HumanQuestion {
                id: state.next_question,
                run: identity.run.clone(),
                from: identity.participant(),
                question: args.question,
                options: args.options,
                context: args.context,
                asked_at_ms: now_ms(),
            };
            state.questions.insert(question.id, question.clone());
            state.emit(
                &identity.run,
                EventKind::HumanAsked {
                    question: question.clone(),
                },
            );
            (question, config.ask_human_wait)
        };

        let mut answer = self.inner.backend.ask_human(question.clone());
        match tokio::time::timeout(wait, &mut answer).await {
            Ok(Ok(answer)) => {
                self.answered(&question, &answer, None);
                Ok(HumanReply {
                    question_id: question.id,
                    status: HumanReplyStatus::Answered,
                    answer: Some(answer),
                })
            }
            Ok(Err(e)) => {
                self.lock().questions.remove(&question.id);
                Err(BusError::Backend { reason: e.0 })
            }
            Err(_) => {
                let question_id = question.id;
                let bus = self.clone();
                let asker = caller.clone();
                tokio::spawn(async move {
                    match answer.await {
                        Ok(answer) => bus.answered(&question, &answer, Some(&asker)),
                        Err(_) => {
                            bus.lock().questions.remove(&question.id);
                        }
                    }
                });
                Ok(HumanReply {
                    question_id,
                    status: HumanReplyStatus::Pending,
                    answer: None,
                })
            }
        }
    }

    /// Records an answer; `deliver_to` gets it as a mailbox message and a wake
    /// when the asker is no longer waiting on the tool call.
    fn answered(&self, question: &HumanQuestion, answer: &str, deliver_to: Option<&SessionId>) {
        let mut state = self.lock();
        state.questions.remove(&question.id);
        state.emit(
            &question.run,
            EventKind::HumanAnswered {
                question: question.id,
                answer: answer.to_string(),
            },
        );
        if let Some(session) = deliver_to {
            // The asker may have left meanwhile; the answer stays in the log.
            let _ = state.route(
                &question.run,
                Sender {
                    from: Participant::Human,
                    session: None,
                    max_turns: None,
                },
                Some(Target::Session(session.clone())),
                MessageKind::HumanAnswer,
                format!(
                    "Answer to your question {} (\"{}\"): {answer}",
                    question.id, question.question
                ),
                None,
            );
        }
    }
}
