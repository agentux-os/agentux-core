//! JSON-RPC 2.0 envelope, error codes, method names and their parameters.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{BusEndpoint, Event, PermissionRequest, Run, Session, StepAttempt};

pub const VERSION: &str = "2.0";

/// Method names. Each line of the socket carries one request, response or
/// notification.
pub mod method {
    pub const PROJECTS_REGISTER: &str = "projects.register";
    pub const PROJECTS_LIST: &str = "projects.list";
    pub const RUNS_START: &str = "runs.start";
    pub const RUNS_LIST: &str = "runs.list";
    pub const RUNS_GET: &str = "runs.get";
    pub const RUNS_CANCEL: &str = "runs.cancel";
    pub const RUNS_EVENTS: &str = "runs.events";
    pub const SESSIONS_LIST: &str = "sessions.list";
    pub const SESSIONS_PROMPT: &str = "sessions.prompt";
    pub const REQUESTS_LIST: &str = "requests.list";
    pub const REQUESTS_APPROVE: &str = "requests.approve";
    pub const REQUESTS_DENY: &str = "requests.deny";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    pub const BUS_LIST: &str = "bus.list";
    pub const BUS_POST: &str = "bus.post";
    /// Used by `aux bus-stdio` (the bus bridge), not by the cockpit.
    pub const BUS_HELLO: &str = "bus.hello";
    /// Used by `aux bus-stdio` (the bus bridge), not by the cockpit.
    pub const BUS_CALL: &str = "bus.call";
    /// Server-to-client notification carrying one [`crate::Event`].
    pub const EVENT: &str = "event";
    /// Server-to-client notification sent once per `events.subscribe`, after
    /// the replayed events and before any live one: [`super::ReplayDone`].
    pub const REPLAY_DONE: &str = "replay_done";
}

/// Error codes. The standard JSON-RPC ones, plus application codes.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The project, run or request does not exist.
    pub const NOT_FOUND: i64 = -32001;
    /// The object is not in a state that allows the operation, e.g.
    /// approving a request that was already resolved.
    pub const CONFLICT: i64 = -32002;
    /// The project directory or its `agentux.yaml` is unusable.
    pub const INVALID_PROJECT: i64 = -32003;
    /// A bus bridge request with an unknown session token, or the token of a
    /// session that has ended.
    pub const UNAUTHORIZED: i64 = -32004;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    /// Absent for notifications, which get no response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: Error) -> Self {
        Self {
            jsonrpc: VERSION.into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
}

impl Error {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterProject {
    /// Any directory inside the project's git repository.
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRun {
    pub project_id: String,
    /// Defaults to the first line of the prompt, or `Issue #<n>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRuns {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRef {
    pub run_id: String,
}

/// Result of `runs.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDetail {
    pub run: Run,
    /// Oldest first.
    pub attempts: Vec<StepAttempt>,
    pub requests: Vec<PermissionRequest>,
    /// Oldest first.
    #[serde(default)]
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessions {
    /// Only the sessions of this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRequests {
    /// `true` returns only pending requests (the approvals inbox).
    #[serde(default)]
    pub pending: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolve {
    pub request_id: String,
    /// Optional note recorded with the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscribe {
    /// Only events of this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Replay persisted events with `seq > since` before live ones. Omit to
    /// receive only new events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListBus {
    pub run_id: String,
}

/// Result of `events.subscribe`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscribed {
    /// `seq` of the newest event at subscription time.
    pub seq: i64,
}

/// Params of `replay_done`: the subscriber has every event of its stream with
/// `seq <= seq`; what follows is live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayDone {
    pub seq: i64,
}

/// Params of `runs.events`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEventsParams {
    pub run_id: String,
    /// Only events with `seq > since_seq` (default 0: from the start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_seq: Option<i64>,
    /// At most this many events (default [`RUN_EVENTS_DEFAULT_LIMIT`], at
    /// most [`RUN_EVENTS_MAX_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

pub const RUN_EVENTS_DEFAULT_LIMIT: u32 = 1000;
pub const RUN_EVENTS_MAX_LIMIT: u32 = 10_000;

/// Result of `runs.events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvents {
    /// Oldest first.
    pub events: Vec<Event>,
    /// More events of the run follow: ask again with `sinceSeq` set to the
    /// last event's `seq`.
    pub more: bool,
    /// The newest `seq` in the whole log when this page was read. Once `more`
    /// is false, `events.subscribe { runId, since: headSeq }` continues
    /// without a gap or duplicate.
    pub head_seq: i64,
}

/// Params of `sessions.prompt`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSession {
    pub session_id: String,
    pub text: String,
}

/// Result of `sessions.prompt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompted {
    pub session: Session,
    /// The session was in a turn: the message waits for that turn (and any
    /// queued before it) to end.
    pub queued: bool,
}

/// Params of `bus.post`: a message from the human on a run's bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusPost {
    pub run_id: String,
    /// `session`, `role` or `run`. Required unless `in_reply_to` is set
    /// (then it defaults to the sender of that message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<BusEndpoint>,
    pub body: String,
    /// One line; becomes the message's first line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The bus message id being answered (`BusMessage.messageId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<u64>,
}

/// Result of `bus.post`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusPosted {
    pub message_id: u64,
    pub exchange: u64,
    pub turn: u32,
    /// Session ids whose mailbox received it.
    pub delivered_to: Vec<String>,
    /// No session plays the target role: the message waits, and a session
    /// is started for the role.
    pub queued_for_role: Option<String>,
}
