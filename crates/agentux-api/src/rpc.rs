//! JSON-RPC 2.0 envelope, error codes, method names and their parameters.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{PermissionRequest, Run, StepAttempt};

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
    pub const REQUESTS_LIST: &str = "requests.list";
    pub const REQUESTS_APPROVE: &str = "requests.approve";
    pub const REQUESTS_DENY: &str = "requests.deny";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    /// Server-to-client notification carrying one [`crate::Event`].
    pub const EVENT: &str = "event";
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

/// Result of `events.subscribe`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscribed {
    /// `seq` of the newest event at subscription time.
    pub seq: i64,
}
