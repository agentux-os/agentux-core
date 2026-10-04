//! Identities, addresses, messages and the events the bus emits.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Identifies a run (one task going through a pipeline).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct RunId(pub String);

/// Identifies one harness session on the bus. Assigned by the daemon.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RunId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Who a harness session is on the bus (ADR 0004): every tool call it makes
/// is scoped to this identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionIdentity {
    pub run: RunId,
    pub project: String,
    /// The role from `agentux.yaml` the session plays, e.g. `implementer`.
    pub role: String,
    /// The harness behind the session, e.g. `claude-code`.
    pub vendor: String,
    pub session: SessionId,
}

impl SessionIdentity {
    pub fn participant(&self) -> Participant {
        Participant::Session {
            session: self.session.clone(),
            role: self.role.clone(),
            vendor: self.vendor.clone(),
        }
    }
}

/// The sender of a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Participant {
    /// An agent session.
    Session {
        session: SessionId,
        role: String,
        vendor: String,
    },
    /// The human supervising the run (through the cockpit or `aux`).
    Human,
}

impl fmt::Display for Participant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session {
                session,
                role,
                vendor,
            } => write!(f, "{role} ({vendor}, session:{session})"),
            Self::Human => f.write_str("the human"),
        }
    }
}

/// Where a message goes. Written as a string in tool arguments:
/// `session:<id>`, `role:<name>`, `run` or `human`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// One session of the same run.
    Session(SessionId),
    /// Every session of the run playing the role; queued for the role (and
    /// a session started for it) when none is live.
    Role(String),
    /// The run's channel: every other session of the run, without waking them.
    Run,
    /// The human supervising the run.
    Human,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(id) => write!(f, "session:{id}"),
            Self::Role(role) => write!(f, "role:{role}"),
            Self::Run => f.write_str("run"),
            Self::Human => f.write_str("human"),
        }
    }
}

impl FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let named = |rest: &str, what: &str| {
            let rest = rest.trim();
            if rest.is_empty() {
                Err(format!("`{s}` is missing the {what} after the colon"))
            } else {
                Ok(rest.to_string())
            }
        };
        match s {
            "run" => Ok(Self::Run),
            "human" => Ok(Self::Human),
            _ => {
                if let Some(rest) = s.strip_prefix("role:") {
                    named(rest, "role name").map(Self::Role)
                } else if let Some(rest) = s.strip_prefix("session:") {
                    named(rest, "session id").map(|id| Self::Session(SessionId(id)))
                } else {
                    Err(format!(
                        "invalid target `{s}`: use `role:<name>`, `session:<id>`, `run` or `human`"
                    ))
                }
            }
        }
    }
}

impl Serialize for Target {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Target {
    fn schema_name() -> Cow<'static, str> {
        "Target".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(run|human|role:.+|session:.+)$",
            "description": "Recipient: `role:<name>` (every session playing that role in this run, e.g. `role:reviewer`), `session:<id>` (one session), `run` (everyone in this run, without waking them) or `human` (the person supervising the run).",
            "examples": ["role:reviewer", "session:3f2a", "run", "human"]
        })
    }
}

/// What a message is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Sent with `post_message`.
    Message,
    /// Sent with `request_review`.
    ReviewRequest,
    /// Sent with `handoff`.
    Handoff,
    /// The human's answer to an `ask_human` question that outlived the call.
    HumanAnswer,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ReviewRequest => "review request",
            Self::Handoff => "handoff",
            Self::HumanAnswer => "answer from the human",
        }
    }
}

/// A message routed by the bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    /// Unique id; pass it as `in_reply_to` to answer.
    pub id: u64,
    /// The conversation this message belongs to. A message without
    /// `in_reply_to` starts a new exchange.
    pub exchange: u64,
    /// Position of this message in its exchange, starting at 1.
    pub turn: u32,
    pub run: RunId,
    pub from: Participant,
    pub to: Target,
    pub kind: MessageKind,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<u64>,
    /// Milliseconds since the Unix epoch.
    pub sent_at_ms: u64,
}

/// A request for the daemon to prompt a session (or start one for a role)
/// so it notices new mail. The daemon turns it into an ACP `session/prompt`
/// with [`Wake::prompt`] as the text; the bus never talks to harnesses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wake {
    pub run: RunId,
    pub target: WakeTarget,
    /// The message that caused the wake.
    pub message: u64,
    pub reason: MessageKind,
    /// The prompt to send.
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum WakeTarget {
    /// Prompt this live session.
    Session(SessionId),
    /// No live session plays this role: start one for it. The messages
    /// queued for the role move to its mailbox when it joins the bus.
    Role(String),
}

/// A question escalated to the human with `ask_human`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HumanQuestion {
    pub id: u64,
    pub run: RunId,
    pub from: Participant,
    pub question: String,
    /// Suggested answers, if the agent offered any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub asked_at_ms: u64,
}

/// One entry of the bus's audit log. Every exchange, wake, escalation and
/// refusal is an event; the daemon persists them and the cockpit shows them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusEvent {
    /// Increases by one for every event the bus emits.
    pub seq: u64,
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub run: RunId,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    SessionJoined {
        identity: SessionIdentity,
        /// Messages that were waiting for its role and moved to its mailbox.
        queued: usize,
    },
    SessionLeft {
        session: SessionId,
        /// Unread messages dropped with its mailbox (or, with
        /// `requeued_for`, moved to its role's queue).
        unread: usize,
        /// Set when the session is marked gone after a daemon restart: the
        /// messages delivered to it were queued again for this role, since
        /// whether it read them is not known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requeued_for: Option<String>,
    },
    /// A message was accepted and delivered.
    MessagePosted {
        message: Message,
        /// Sessions whose mailbox received it.
        delivered_to: Vec<SessionId>,
        /// Set when no live session plays the target role and the message
        /// waits for one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queued_for_role: Option<String>,
    },
    Wake(Wake),
    HumanAsked {
        question: HumanQuestion,
    },
    HumanAnswered {
        question: u64,
        answer: String,
    },
    /// A post was refused because its exchange used all its turns.
    TurnLimitReached {
        session: SessionId,
        exchange: u64,
        max_turns: u32,
    },
    /// A session called a tool `agentux.yaml` does not allow.
    ToolDenied {
        session: SessionId,
        tool: String,
    },
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_round_trip_through_strings() {
        for (text, target) in [
            ("run", Target::Run),
            ("human", Target::Human),
            ("role:reviewer", Target::Role("reviewer".into())),
            ("session:s-1", Target::Session("s-1".into())),
        ] {
            assert_eq!(text.parse::<Target>().unwrap(), target);
            assert_eq!(target.to_string(), text);
            assert_eq!(
                serde_json::to_value(&target).unwrap(),
                serde_json::json!(text)
            );
        }
        assert!("role:".parse::<Target>().is_err());
        assert!("reviewer".parse::<Target>().unwrap_err().contains("role:"));
    }
}
