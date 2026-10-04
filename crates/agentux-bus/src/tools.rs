//! Arguments and results of the bus tools. The same types are the MCP tool
//! schemas and the payload of the daemon bridge (see [`crate::bridge`]).

use agentux_config::BusTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::backend::RunState;
use crate::types::{HumanQuestion, Message, RunId, SessionId, Target};

/// Tool descriptions, written for the agents that read them.
pub mod descriptions {
    pub const POST_MESSAGE: &str = "Send a message to another agent of this run, to the whole run, or to the human. \
The recipient is woken up and reads it with read_messages (messages to `run` do not wake anyone). \
To answer a message, pass its id as `in_reply_to` and leave `to` empty: the reply goes to the sender and stays in the same exchange. \
Each exchange has a limit of turns; when it is reached this tool fails, and you must stop messaging and proceed on your own or ask_human. \
Be concise: say what you need or what you found, and point to files and commits instead of pasting code.";

    pub const READ_MESSAGES: &str = "Read the unread messages in your mailbox, oldest first. Each call removes the returned messages from the mailbox. \
Call this when you are told you have new messages. Reply with post_message using the message's `id` as `in_reply_to`.";

    pub const REQUEST_REVIEW: &str = "Ask the reviewer role (usually an agent from another vendor) to review the current changes on this run's branch. \
Summarize what changed and what you want checked. The reviewer answers with a message in your mailbox; keep working or end your turn meanwhile.";

    pub const HANDOFF: &str = "Hand the task over to another role, with a summary of where it stands and pointers (files, commits, open questions) so it can continue without asking. \
After a handoff, stop working on the task.";

    pub const GET_RUN_STATE: &str = "Get the state of your run: pipeline step, branch, check results, open requests, the sessions on the bus, your unread message count and the questions waiting for the human.";

    pub const ASK_HUMAN: &str = "Escalate a decision to the human supervising the run; it appears in their inbox. \
Use it only for decisions you cannot take yourself (requirements, trade-offs, permissions, being stuck), not for status updates. \
Waits a while for the answer; if none comes in time, the result says the question is pending and the answer arrives later in your mailbox.";
}

/// Arguments of `post_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PostMessage {
    /// Recipient: `role:<name>` (every session playing that role in this run), `session:<id>` (one session), `run` (everyone in this run, without waking them) or `human`. Optional when `in_reply_to` is set: the reply then goes to the sender of that message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Target>,
    /// The message text. Keep it short and specific.
    pub body: String,
    /// Id of the message you are answering. The reply joins that message's exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<u64>,
}

/// Arguments of `read_messages`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadMessages {
    /// Maximum number of messages to return (default 20, at most 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Arguments of `request_review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestReview {
    /// What changed and what the reviewer should focus on.
    pub summary: String,
    /// Role to ask. Defaults to the role of the pipeline's review step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_role: Option<String>,
}

/// Arguments of `handoff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Handoff {
    /// Role that takes over, e.g. `implementer`.
    pub to_role: String,
    /// Where the task stands and what remains to be done.
    pub summary: String,
    /// Files, commits, test names or open questions the next role should look at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pointers: Vec<String>,
}

/// Arguments of `ask_human`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AskHuman {
    /// The decision you need, as a question the human can answer quickly.
    pub question: String,
    /// Possible answers, if the choice is between known options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Background the human needs to decide: what you tried, what each option implies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Result of `post_message`, `request_review` and `handoff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Posted {
    /// Id of the message that was sent.
    pub message_id: u64,
    pub exchange: u64,
    /// Turn this message used in its exchange.
    pub turn: u32,
    /// Turns the exchange has left after this one.
    pub turns_left: u32,
    /// Sessions whose mailbox received the message.
    pub delivered_to: Vec<SessionId>,
    /// Set when no session plays the target role yet: the message waits for
    /// the session the daemon starts for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_for_role: Option<String>,
}

/// Result of `read_messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Inbox {
    pub messages: Vec<Message>,
    /// Unread messages left in the mailbox after this call.
    pub remaining: usize,
}

/// A session on the bus, as other sessions see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    pub session: SessionId,
    pub role: String,
    pub vendor: String,
}

/// Result of `get_run_state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunStateReply {
    pub run: RunId,
    pub project: String,
    /// The calling session.
    pub you: SessionInfo,
    /// What the workflow engine reports.
    pub state: RunState,
    /// Sessions of this run on the bus, including you.
    pub sessions: Vec<SessionInfo>,
    /// Messages waiting in your mailbox.
    pub unread_messages: usize,
    /// Questions of this run waiting for the human.
    pub pending_questions: Vec<HumanQuestion>,
}

/// Result of `ask_human`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HumanReply {
    pub question_id: u64,
    /// `answered`, or `pending` when the human has not answered in time.
    pub status: HumanReplyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HumanReplyStatus {
    Answered,
    /// The answer will arrive in your mailbox and you will be woken up.
    Pending,
}

/// One tool call, as the MCP server hands it to a [`crate::BusEndpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", content = "arguments", rename_all = "snake_case")]
pub enum BusCall {
    PostMessage(PostMessage),
    ReadMessages(ReadMessages),
    RequestReview(RequestReview),
    Handoff(Handoff),
    GetRunState,
    AskHuman(AskHuman),
}

impl BusCall {
    pub fn tool(&self) -> BusTool {
        match self {
            Self::PostMessage(_) => BusTool::PostMessage,
            Self::ReadMessages(_) => BusTool::ReadMessages,
            Self::RequestReview(_) => BusTool::RequestReview,
            Self::Handoff(_) => BusTool::Handoff,
            Self::GetRunState => BusTool::GetRunState,
            Self::AskHuman(_) => BusTool::AskHuman,
        }
    }
}

/// The result of a [`BusCall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum BusReply {
    Posted(Posted),
    Inbox(Inbox),
    RunState(Box<RunStateReply>),
    Human(HumanReply),
}
