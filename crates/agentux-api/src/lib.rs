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

pub use client::{Client, ClientError, Subscription};

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
    /// step; `step` is any other `approve: true` step.
    RequestKind {
        Plan => "plan",
        Step => "step",
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
    /// `budget.max_usd_per_run`. Stored, not enforced yet.
    pub budget_usd: Option<f64>,
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
    pub step_index: usize,
    pub step: StepKind,
    pub title: String,
    /// What is being approved, e.g. the plan text.
    pub detail: String,
    pub status: RequestStatus,
    pub answer: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

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
}
