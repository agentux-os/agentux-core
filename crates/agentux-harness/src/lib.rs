//! Drives coding-agent harnesses (Claude Code, Codex, OpenCode, Antigravity)
//! as described in ADR 0002: through the Agent Client Protocol, behind a
//! vendor-neutral interface that headless fallbacks can implement later.
//!
//! A [`Harness`] starts a [`HarnessSession`] in a working directory (the run's
//! worktree). The session takes prompts, streams [`Event`]s, routes the
//! agent's permission requests to a caller-supplied [`PermissionHandler`], and
//! can be cancelled and shut down. [`AcpHarness`] is the ACP implementation;
//! [`HarnessSpec::builtin`] lists the harnesses AgentUX knows how to launch.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;

mod acp;
mod spec;

pub use acp::{AcpHarness, AcpSession};
pub use spec::HarnessSpec;

/// Starts sessions with one harness.
pub trait Harness {
    type Session: HarnessSession;

    /// Starts the harness with `cwd` as its working directory and opens a
    /// session there. Everything the session reports arrives on the returned
    /// receiver, in order; permission requests go to `permissions`.
    fn start(
        &self,
        cwd: &Path,
        permissions: PermissionHandler,
    ) -> impl Future<Output = Result<(Self::Session, Events), Error>> + Send;
}

/// A running harness session.
pub trait HarnessSession {
    /// Sends a prompt and waits until the agent ends its turn. Events the turn
    /// produced are on the session's [`Events`] receiver before this returns.
    fn prompt(&self, text: &str) -> impl Future<Output = Result<StopReason, Error>> + Send;

    /// Asks the agent to stop the current turn. The pending [`prompt`] then
    /// returns, normally with [`StopReason::Cancelled`]; permission requests
    /// still waiting for an answer are withdrawn.
    ///
    /// [`prompt`]: HarnessSession::prompt
    fn cancel(&self) -> Result<(), Error>;

    /// Closes the session and stops the harness.
    fn shutdown(self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// An MCP server the agent connects to for the session, such as the
/// `agentux` bus (ADR 0004). Launched by the agent as a subprocess speaking
/// MCP over stdio, the one transport every ACP agent must support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// Name the agent shows the server's tools under.
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Receives the events of a session.
pub type Events = mpsc::UnboundedReceiver<Event>;

/// What a session reports while the agent works.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A chunk of the agent's reply.
    AgentMessage(String),
    /// A chunk of the agent's reasoning, for harnesses that expose it.
    AgentThought(String),
    /// The agent started a tool call.
    ToolCall(ToolCall),
    /// A tool call changed: only the fields that changed are set.
    ToolCallUpdate(ToolCallUpdate),
    /// The agent's current plan, replacing any earlier one.
    Plan(Vec<PlanEntry>),
    /// A file edit proposed or made by a tool call.
    Diff(FileDiff),
    /// Context window usage and, when reported, cost.
    Usage(Usage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallUpdate {
    pub id: String,
    pub title: Option<String>,
    pub status: Option<ToolStatus>,
    /// Text output of the tool, when it reported some.
    pub output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The tool call that produced the edit.
    pub tool_call_id: String,
    pub path: PathBuf,
    /// `None` for a new file.
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    /// Tokens currently in the context window.
    pub used_tokens: u64,
    /// Size of the context window in tokens.
    pub context_tokens: u64,
    /// Cumulative session cost, if the harness reports it.
    pub cost: Option<Cost>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub amount: f64,
    /// ISO 4217 code, e.g. `USD`.
    pub currency: String,
}

/// Why the agent ended its turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// The agent asks before running a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_call_id: String,
    /// What the tool is about to do, e.g. `Run cargo test`.
    pub title: Option<String>,
    pub kind: Option<ToolKind>,
}

/// The caller's answer to a [`PermissionRequest`]. It applies to this one
/// tool call only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

/// Answers permission requests. It may take as long as it needs (e.g. waiting
/// for a human); the session keeps streaming events meanwhile.
pub type PermissionHandler =
    Arc<dyn Fn(PermissionRequest) -> Pin<Box<dyn Future<Output = Decision> + Send>> + Send + Sync>;

/// Wraps an async function as a [`PermissionHandler`].
pub fn permission_handler<F, Fut>(f: F) -> PermissionHandler
where
    F: Fn(PermissionRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Decision> + Send + 'static,
{
    Arc::new(move |request| Box::pin(f(request)))
}

#[derive(Debug)]
pub enum Error {
    /// The harness process could not be started.
    Spawn {
        command: String,
        source: std::io::Error,
    },
    /// The agent returned an error or broke the protocol, or the connection
    /// closed (e.g. because the harness exited).
    Protocol(agent_client_protocol::Error),
    /// The session was already shut down.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { command, source } => write!(f, "cannot start `{command}`: {source}"),
            Self::Protocol(e) => write!(f, "agent error: {e}"),
            Self::Closed => f.write_str("the session is closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Protocol(e) => Some(e),
            Self::Closed => None,
        }
    }
}

impl From<agent_client_protocol::Error> for Error {
    fn from(e: agent_client_protocol::Error) -> Self {
        Self::Protocol(e)
    }
}
