//! The `agentuxd` API: domain types, the JSON-RPC 2.0 envelope and method
//! parameters, and an async client. The daemon, `aux` and the cockpit's
//! backend share this crate so they cannot drift apart (ADR 0006).
//!
//! Wire format: newline-delimited JSON-RPC 2.0 over a Unix domain socket.
//! Field names are camelCase to match the cockpit's TypeScript model.
//! Timestamps are milliseconds since the Unix epoch. See `docs/api.md`.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod client;
pub mod rpc;

pub use client::{
    Client, ClientError, Notice, Subscription, TerminalEvents, TerminalInput, TerminalNotice,
};

/// Bytes as the API carries them (`terminals.write`, `terminal_output`):
/// base64, standard alphabet, padded.
pub fn encode_bytes(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The inverse of [`encode_bytes`].
pub fn decode_bytes(text: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| format!("invalid base64: {e}"))
}

/// Overrides the socket path for every client and the daemon.
pub const SOCKET_ENV: &str = "AGENTUX_SOCKET";

/// `$AGENTUX_SOCKET`, else `$XDG_RUNTIME_DIR/agentux/agentuxd.sock`.
/// `None` when neither variable is set.
pub fn default_socket_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os(SOCKET_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let runtime = env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty())?;
    Some(PathBuf::from(runtime).join("agentux").join("agentuxd.sock"))
}

/// Defines a string enum with `as_str` and `parse`, serialized in snake_case.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($(#[$vmeta])* $variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(
    /// Pipeline step types (ADR 0005).
    StepKind {
        Plan => "plan",
        Implement => "implement",
        Gate => "gate",
        Review => "review",
        PullRequest => "pull_request",
        Custom => "custom",
    }
);

string_enum!(
    /// `waiting` means the run is paused on a pending approval request.
    RunStatus {
        Running => "running",
        Waiting => "waiting",
        Done => "done",
        Failed => "failed",
        Cancelled => "cancelled",
    }
);

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

string_enum!(
    CheckStatus {
        Pending => "pending",
        Running => "running",
        Passed => "passed",
        Failed => "failed",
    }
);

string_enum!(
    /// What an approval request is about. `plan` is the approval after a plan
    /// step; `step` is any other `approve: true` step; `permission` is an
    /// agent asking before a tool call (the agent waits for the answer);
    /// `budget` pauses a run whose spending passed its budget; `question` is
    /// an agent asking the human through the bus (`ask_human`), answered with
    /// free text or one of its `options`.
    RequestKind {
        Plan => "plan",
        Step => "step",
        Permission => "permission",
        Budget => "budget",
        Question => "question",
    }
);

string_enum!(
    /// `active` while the agent works on a prompt, `waiting` while it waits
    /// for a permission answer, `idle` between steps, `attached` while the
    /// session is open in its harness's own TUI in a daemon terminal (ACP
    /// turns wait until it is closed), `ended` once the harness process is
    /// gone.
    SessionState {
        Active => "active",
        Idle => "idle",
        Waiting => "waiting",
        Attached => "attached",
        Ended => "ended",
    }
);

string_enum!(
    RequestStatus {
        Pending => "pending",
        Approved => "approved",
        Denied => "denied",
        /// The run was cancelled while the request was pending.
        Cancelled => "cancelled",
    }
);

string_enum!(
    AttemptStatus {
        Running => "running",
        Succeeded => "succeeded",
        Failed => "failed",
        /// A review step that asked for changes.
        ChangesRequested => "changes_requested",
        /// The daemon stopped while the step was running; it is run again.
        Interrupted => "interrupted",
        /// The run was cancelled while the step was running.
        Cancelled => "cancelled",
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    /// Root of the git repository.
    pub path: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub name: String,
    pub command: String,
    pub status: CheckStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub prompt: Option<String>,
    pub issue: Option<u64>,
    /// `aux/<run-id>`, set once the worktree exists.
    pub branch: Option<String>,
    /// Path of the run's worktree, set once it exists.
    pub worktree: Option<String>,
    /// The pipeline's step kinds, in order.
    pub steps: Vec<StepKind>,
    /// Index into `steps` of the current (or last) step.
    pub step_index: usize,
    pub step: StepKind,
    pub status: RunStatus,
    /// Role name to harness, from the project's pipeline.
    pub roles: BTreeMap<String, String>,
    /// Results of the most recent gate step.
    pub checks: Vec<CheckResult>,
    /// Consecutive attempts of the most recent gate, and its limit.
    pub gate_attempt: u32,
    pub gate_max_attempts: u32,
    /// Rounds of the most recent review, and its limit.
    pub review_round: u32,
    pub review_max_rounds: u32,
    /// Spending limit: `budget.max_usd_per_run`, raised by that amount each
    /// time the human approves going over it.
    pub budget_usd: Option<f64>,
    /// What the run's sessions have cost so far, as reported by the harnesses
    /// (0 when none reports cost).
    #[serde(default)]
    pub cost_usd: f64,
    /// Role name to the id of its harness session, filled as the run reaches
    /// each role.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    pub started_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    pub pull_request: Option<PullRequest>,
    /// Short human-readable line of what is happening right now.
    pub activity: String,
    /// Why the run failed, if it did.
    pub error: Option<String>,
}

/// One execution of one pipeline step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepAttempt {
    pub id: i64,
    pub run_id: String,
    pub step_index: usize,
    pub step: StepKind,
    pub status: AttemptStatus,
    /// Agent summary, review comments or check output.
    pub output: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// An approval the human must give before the run moves on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    pub kind: RequestKind,
    pub run_id: String,
    pub project_id: String,
    /// The session that asked, for `permission` requests.
    #[serde(default)]
    pub session_id: Option<String>,
    pub step_index: usize,
    pub step: StepKind,
    pub title: String,
    /// What is being approved, e.g. the plan text.
    pub detail: String,
    /// Suggested answers of a `question` request (the human may also answer
    /// in free text); empty otherwise.
    #[serde(default)]
    pub options: Vec<String>,
    pub status: RequestStatus,
    pub answer: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

/// A harness session: one agent process working for one role of a run, in
/// the run's worktree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub run_id: String,
    pub project_id: String,
    pub role: String,
    /// The harness id, e.g. `opencode` (the cockpit's `vendor`).
    pub harness: String,
    pub model: Option<String>,
    pub state: SessionState,
    /// The worktree the harness works in.
    pub cwd: String,
    pub usage: SessionUsage,
    pub started_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    /// The harness's own id of the session: the ACP `sessionId`, which the
    /// built-in adapters share with the vendor CLI (`claude --resume <id>`,
    /// `codex resume <id>`, `opencode --session <id>`). Set once the harness
    /// has started the session.
    #[serde(default)]
    pub vendor_session_id: Option<String>,
}

/// What a harness reported about its session. ACP reports context-window
/// usage, not input/output token counts, and cost only for some harnesses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    /// Tokens currently in the context window.
    pub used_tokens: u64,
    /// Size of the context window.
    pub context_tokens: u64,
    /// Cumulative cost of the session in USD, if the harness reports it.
    pub cost_usd: Option<f64>,
}

/// What happened in a session. Agent message chunks are coalesced into
/// `message` events; a tool call arrives as one `tool_call` when it starts and
/// another for each change, with only the changed fields set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    Message {
        from: MessageFrom,
        text: String,
    },
    ToolCall {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<ToolKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ToolStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// A file edit carried by a tool call. Large texts are truncated.
    Diff {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        path: String,
        #[serde(rename = "oldText")]
        old_text: Option<String>,
        #[serde(rename = "newText")]
        new_text: String,
    },
    /// The agent's current plan, replacing any earlier one.
    Plan {
        items: Vec<PlanItem>,
    },
    /// The agent asked for permission; see the request.
    Permission {
        #[serde(rename = "requestId")]
        request_id: String,
    },
    Usage {
        usage: SessionUsage,
    },
}

string_enum!(
    /// `user` is the prompt AgentUX sent; `human` is a message the human
    /// typed into the session (`sessions.prompt`); `system` is a note from
    /// the daemon.
    MessageFrom {
        User => "user",
        Agent => "agent",
        System => "system",
        Human => "human",
    }
);

string_enum!(
    ToolKind {
        Read => "read",
        Edit => "edit",
        Delete => "delete",
        Move => "move",
        Search => "search",
        Execute => "execute",
        Think => "think",
        Fetch => "fetch",
        Other => "other",
    }
);

string_enum!(
    /// The cockpit's tool call states; ACP's `pending` and `in_progress`
    /// both map to `running`.
    ToolStatus {
        Running => "running",
        Ok => "ok",
        Error => "error",
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    pub text: String,
    pub status: PlanItemStatus,
}

string_enum!(
    PlanItemStatus {
        Pending => "pending",
        InProgress => "in_progress",
        Done => "done",
    }
);

/// A persisted event. `seq` grows by one per event and lets a subscriber
/// resume where it left off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: i64,
    pub at: i64,
    pub run_id: Option<String>,
    #[serde(flatten)]
    pub body: EventBody,
}

/// Snapshot events carry the whole object, so a client can keep a store keyed
/// by id up to date by replacing entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// Events are short-lived; boxing the run snapshot would only add noise.
#[allow(clippy::large_enum_variant)]
pub enum EventBody {
    Project {
        project: Project,
    },
    Run {
        run: Run,
    },
    Request {
        request: PermissionRequest,
    },
    Attempt {
        attempt: StepAttempt,
    },
    /// Free text: check output, agent summaries, state notes.
    Log {
        text: String,
    },
    /// Any change to a session (full snapshot).
    Session {
        session: Session,
    },
    /// Something happened inside a session.
    SessionEvent {
        #[serde(rename = "sessionId")]
        session_id: String,
        event: SessionEvent,
    },
    /// Traffic on the agent bus: a message, a wake, a question to the human
    /// and its answer, a refusal, a session joining or leaving.
    BusMessage {
        message: BusMessage,
    },
}

string_enum!(
    /// What a [`BusMessage`] records. `message`, `review_request`, `handoff`
    /// and `human_answer` are messages routed between participants (an
    /// answer that outlived the agent's `ask_human` call arrives as a
    /// `human_answer` message); `question` and `answer` are an `ask_human`
    /// escalation and the human's answer; `wake` is the daemon prompting a
    /// session (or starting one for a role) for new mail; `turn_limit` and
    /// `tool_denied` are refusals; `joined` and `left` are sessions entering
    /// and leaving the bus.
    BusMessageKind {
        Message => "message",
        ReviewRequest => "review_request",
        Handoff => "handoff",
        HumanAnswer => "human_answer",
        Question => "question",
        Answer => "answer",
        Wake => "wake",
        TurnLimit => "turn_limit",
        ToolDenied => "tool_denied",
        Joined => "joined",
        Left => "left",
    }
);

/// A participant or address on the bus. The cockpit's `BusEndpoint`, plus
/// `role` (every session playing a role) and `run` (the run's channel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BusEndpoint {
    Session {
        #[serde(rename = "sessionId")]
        session_id: String,
        /// Filled by the daemon; clients addressing a session may omit it.
        #[serde(default)]
        role: String,
        /// Filled by the daemon; clients addressing a session may omit it.
        #[serde(default)]
        vendor: String,
    },
    Role {
        role: String,
    },
    Run,
    Human,
    Daemon,
}

/// One entry of a run's bus log, mapped from the bus's own events. Close to
/// the cockpit's `BusMessage`; the fields after `maxTurns` are additions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusMessage {
    /// Unique id of this log entry.
    pub id: String,
    pub run_id: String,
    pub project_id: String,
    pub kind: BusMessageKind,
    /// The bus tool behind it (`post_message`, `request_review`, `handoff`,
    /// `ask_human`, ...), when an agent's tool call caused it.
    pub tool: Option<String>,
    pub from: BusEndpoint,
    pub to: BusEndpoint,
    /// One line for lists.
    pub subject: String,
    pub body: String,
    pub at: i64,
    /// Turn within its exchange (messages; 0 otherwise), and the run's limit.
    pub turn: u32,
    pub max_turns: u32,
    /// The bus's message id (pass it as `in_reply_to`), for messages and the
    /// wakes they caused.
    pub message_id: Option<u64>,
    pub exchange: Option<u64>,
    pub in_reply_to: Option<u64>,
    /// The `ask_human` question, for `question` and `answer`.
    pub question_id: Option<u64>,
    /// The approval request holding a `question`.
    pub request_id: Option<String>,
    /// Sessions whose mailbox received a message.
    #[serde(default)]
    pub delivered_to: Vec<String>,
    /// Set when no session played the target role: the message waits for one.
    pub queued_for_role: Option<String>,
}
