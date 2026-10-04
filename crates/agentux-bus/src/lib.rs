//! The AgentUX agent bus (ADR 0004): mediated messaging between the harness
//! sessions of a run, served to each session as the `agentux` MCP server.
//!
//! - [`Bus`] holds sessions and mailboxes, routes messages (to a session, to
//!   every session of a role, to the run's channel, to the human), enforces
//!   the `bus` section of `agentux.yaml` (allowed tools, turns per exchange),
//!   and reports everything as [`BusEvent`]s: the audit log, and the
//!   [`Wake`]s the daemon turns into ACP prompts. It never talks to harnesses.
//! - [`BusBackend`] is what the daemon provides: run state and the human.
//!   [`MemoryBackend`] implements it in memory.
//! - [`BusServer`] is the MCP server for one session, over a [`BusEndpoint`]
//!   ([`LocalEndpoint`] in-process; a daemon bridge later, see [`bridge`]).
//! - [`session_prompt`] is the system prompt telling a session about the bus.

mod backend;
pub mod bridge;
mod bus;
mod prompt;
mod server;
mod tools;
mod types;

pub use agentux_config::BusTool;
pub use backend::{BackendError, BoxFuture, BusBackend, CheckResult, MemoryBackend, RunState};
pub use bridge::{BusEndpoint, LocalEndpoint, bus_stdio_args};
pub use bus::{Bus, BusConfig, BusError, DEFAULT_ASK_HUMAN_WAIT, wake_prompt};
pub use prompt::{description, session_prompt};
pub use server::{BusServer, SERVER_NAME};
pub use tools::{
    AskHuman, BusCall, BusReply, Handoff, HumanReply, HumanReplyStatus, Inbox, PostMessage, Posted,
    ReadMessages, RequestReview, RunStateReply, SessionInfo, descriptions,
};
pub use types::{
    BusEvent, EventKind, HumanQuestion, Message, MessageKind, Participant, RunId, SessionId,
    SessionIdentity, Target, Wake, WakeTarget,
};
