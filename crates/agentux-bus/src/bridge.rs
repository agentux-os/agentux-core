//! Where an MCP server sends its tool calls.
//!
//! [`BusServer`](crate::BusServer) does not hold a [`Bus`]: it holds a
//! [`BusEndpoint`], one session's view of the bus. [`LocalEndpoint`] calls a
//! [`Bus`] in the same process (the daemon, tests, `aux bus-stdio
//! --standalone`). The daemon bridge will be a second implementation that
//! forwards each [`BusCall`] over `agentuxd`'s Unix socket, speaking the
//! [`BridgeHello`] / [`BridgeWelcome`] / [`BridgeRequest`] / [`BridgeResponse`]
//! messages below as newline-delimited JSON. See the crate README.

use std::sync::Arc;

use agentux_config::BusTool;
use serde::{Deserialize, Serialize};

use crate::backend::BoxFuture;
use crate::bus::{Bus, BusError};
use crate::tools::{BusCall, BusReply};
use crate::types::{SessionId, SessionIdentity};

/// One session's access to the bus.
pub trait BusEndpoint: Send + Sync + 'static {
    /// The session every call is made as.
    fn identity(&self) -> &SessionIdentity;

    /// The tools the session may call; the MCP server lists only these.
    fn allowed_tools(&self) -> &[BusTool];

    /// The max turns per exchange of the run, for the session prompt.
    fn max_turns_per_exchange(&self) -> u32;

    fn call(&self, call: BusCall) -> BoxFuture<Result<BusReply, BusError>>;
}

/// A [`BusEndpoint`] on a [`Bus`] in the same process.
#[derive(Clone)]
pub struct LocalEndpoint {
    bus: Bus,
    identity: SessionIdentity,
    allowed: Vec<BusTool>,
    max_turns: u32,
}

impl LocalEndpoint {
    /// The endpoint of a session that has joined `bus`.
    pub fn new(bus: Bus, session: &SessionId) -> Result<Arc<Self>, BusError> {
        let identity = bus
            .identity(session)
            .ok_or_else(|| BusError::UnknownSession {
                session: session.clone(),
            })?;
        let config = bus.config(&identity.run)?;
        Ok(Arc::new(Self {
            bus,
            identity,
            allowed: config.allowed_tools(),
            max_turns: config.max_turns_per_exchange,
        }))
    }
}

impl BusEndpoint for LocalEndpoint {
    fn identity(&self) -> &SessionIdentity {
        &self.identity
    }

    fn allowed_tools(&self) -> &[BusTool] {
        &self.allowed
    }

    fn max_turns_per_exchange(&self) -> u32 {
        self.max_turns
    }

    fn call(&self, call: BusCall) -> BoxFuture<Result<BusReply, BusError>> {
        let bus = self.bus.clone();
        let session = self.identity.session.clone();
        Box::pin(async move { bus.call(&session, call).await })
    }
}

/// First line `aux bus-stdio` sends on the daemon socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeHello {
    /// The token the daemon put in the session's MCP server spec. It maps to
    /// exactly one session identity and is useless once the session ends.
    pub session_token: String,
}

/// The daemon's answer to [`BridgeHello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BridgeWelcome {
    Accepted {
        identity: SessionIdentity,
        /// Tool names, as in `bus.allow`.
        allowed_tools: Vec<String>,
        max_turns_per_exchange: u32,
    },
    Rejected {
        reason: String,
    },
}

/// One tool call forwarded to the daemon. Calls may be in flight
/// concurrently; responses carry the request's `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeRequest {
    pub id: u64,
    pub call: BusCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub id: u64,
    pub result: Result<BusReply, BusError>,
}

/// The arguments that make `aux` serve the bus for one session over stdio,
/// for the stdio MCP server spec the daemon passes in ACP `session/new`.
pub fn bus_stdio_args(socket: &std::path::Path, session_token: &str) -> Vec<String> {
    vec![
        "bus-stdio".into(),
        "--socket".into(),
        socket.display().to_string(),
        "--session-token".into(),
        session_token.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{PostMessage, Posted};
    use crate::types::Target;

    #[test]
    fn bridge_messages_are_plain_json() {
        let request = BridgeRequest {
            id: 7,
            call: BusCall::PostMessage(PostMessage {
                to: Some(Target::Role("reviewer".into())),
                body: "ping".into(),
                in_reply_to: None,
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": 7,
                "call": {"tool": "post_message", "arguments": {"to": "role:reviewer", "body": "ping"}}
            })
        );
        assert_eq!(
            serde_json::from_value::<BridgeRequest>(json).unwrap(),
            request
        );

        let response = BridgeResponse {
            id: 7,
            result: Err(BusError::TurnLimit {
                exchange: 1,
                max_turns: 6,
            }),
        };
        let back: BridgeResponse =
            serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(back, response);

        let ok = BridgeResponse {
            id: 8,
            result: Ok(BusReply::Posted(Posted {
                message_id: 1,
                exchange: 1,
                turn: 1,
                turns_left: 5,
                delivered_to: vec!["s2".into()],
                queued_for_role: None,
            })),
        };
        let back: BridgeResponse =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(back, ok);
    }
}
