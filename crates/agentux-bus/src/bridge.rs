//! Where an MCP server sends its tool calls.
//!
//! [`BusServer`](crate::BusServer) does not hold a [`Bus`]: it holds a
//! [`BusEndpoint`], one session's view of the bus. [`LocalEndpoint`] calls a
//! [`Bus`] in the same process (tests, `aux bus-stdio --standalone`).
//! [`DaemonEndpoint`] forwards each [`BusCall`] to `agentuxd` over its Unix
//! socket, as the JSON-RPC methods in [`method`] (`bus.hello`, `bus.call`),
//! one connection per call. See the crate README.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentux_config::BusTool;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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

/// JSON-RPC methods of `agentuxd` that make up the bridge. Their params and
/// results are the types below; see `docs/api.md`.
pub mod method {
    /// Params [`BridgeHello`](super::BridgeHello), result
    /// [`BridgeWelcome`](super::BridgeWelcome).
    pub const HELLO: &str = "bus.hello";
    /// Params [`BridgeCall`](super::BridgeCall), result
    /// [`BridgeOutcome`](super::BridgeOutcome).
    pub const CALL: &str = "bus.call";
}

/// JSON-RPC error code `agentuxd` answers bridge requests with when the
/// session token is unknown or its session has ended.
pub const UNAUTHORIZED: i64 = -32004;

/// Sent by `aux bus-stdio` once at startup, to learn who it serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeHello {
    /// The token the daemon put in the session's MCP server spec. It maps to
    /// exactly one session identity and is useless once the session ends.
    pub session_token: String,
}

/// The daemon's answer to [`BridgeHello`] for a valid token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeWelcome {
    pub identity: SessionIdentity,
    /// Tool names, as in `bus.allow`.
    pub allowed_tools: Vec<String>,
    pub max_turns_per_exchange: u32,
}

/// One tool call forwarded to the daemon. The token, never anything the agent
/// sends, decides which session the call is made as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeCall {
    pub session_token: String,
    pub call: BusCall,
}

/// The result of a [`BridgeCall`]: the reply, or the refusal the agent reads
/// as a tool error. Refusals are results, not JSON-RPC errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeOutcome {
    Ok(BusReply),
    Err(BusError),
}

impl From<Result<BusReply, BusError>> for BridgeOutcome {
    fn from(result: Result<BusReply, BusError>) -> Self {
        match result {
            Ok(reply) => Self::Ok(reply),
            Err(error) => Self::Err(error),
        }
    }
}

/// A [`BusEndpoint`] that forwards every call to `agentuxd` over its Unix
/// socket: what `aux bus-stdio --session-token` serves.
///
/// The daemon answers requests on one connection in order, and `ask_human`
/// can keep a call open for minutes, so each call uses a connection of its
/// own: calls in flight at the same time never wait for each other.
pub struct DaemonEndpoint {
    socket: PathBuf,
    token: String,
    identity: SessionIdentity,
    allowed: Vec<BusTool>,
    max_turns: u32,
}

impl DaemonEndpoint {
    /// Introduces the session to the daemon (`bus.hello`). Fails when the
    /// daemon is unreachable or rejects the token.
    pub async fn connect(socket: &Path, session_token: &str) -> Result<Arc<Self>, String> {
        let hello = BridgeHello {
            session_token: session_token.to_string(),
        };
        let welcome: BridgeWelcome = rpc(socket, method::HELLO, &hello).await?;
        let allowed = welcome
            .allowed_tools
            .iter()
            .filter_map(|name| BusTool::ALL.into_iter().find(|t| t.as_str() == name))
            .collect();
        Ok(Arc::new(Self {
            socket: socket.to_path_buf(),
            token: hello.session_token,
            identity: welcome.identity,
            allowed,
            max_turns: welcome.max_turns_per_exchange,
        }))
    }
}

impl BusEndpoint for DaemonEndpoint {
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
        let socket = self.socket.clone();
        let request = BridgeCall {
            session_token: self.token.clone(),
            call,
        };
        Box::pin(async move {
            match rpc::<_, BridgeOutcome>(&socket, method::CALL, &request).await {
                Ok(BridgeOutcome::Ok(reply)) => Ok(reply),
                Ok(BridgeOutcome::Err(error)) => Err(error),
                Err(reason) => Err(BusError::Backend { reason }),
            }
        })
    }
}

/// One JSON-RPC request on a fresh connection to the daemon.
async fn rpc<P: Serialize, R: DeserializeOwned>(
    socket: &Path,
    method: &str,
    params: &P,
) -> Result<R, String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("cannot reach agentuxd at {}: {e}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(|e| format!("cannot talk to agentuxd: {e}"))?;
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| format!("cannot talk to agentuxd: {e}"))?
            .ok_or("agentuxd closed the connection")?;
        let response: serde_json::Value =
            serde_json::from_str(&line).map_err(|e| format!("bad response from agentuxd: {e}"))?;
        if response.get("id") != Some(&serde_json::json!(1)) {
            continue;
        }
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(message.to_string());
        }
        let result = response.get("result").cloned().unwrap_or_default();
        return serde_json::from_value(result)
            .map_err(|e| format!("bad response from agentuxd: {e}"));
    }
}

/// The arguments that make `aux` serve the bus for one session over stdio,
/// for the stdio MCP server spec the daemon passes in ACP `session/new`.
pub fn bus_stdio_args(socket: &Path, session_token: &str) -> Vec<String> {
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
        let request = BridgeCall {
            session_token: "t0k".into(),
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
                "sessionToken": "t0k",
                "call": {"tool": "post_message", "arguments": {"to": "role:reviewer", "body": "ping"}}
            })
        );
        assert_eq!(serde_json::from_value::<BridgeCall>(json).unwrap(), request);

        let refused = BridgeOutcome::Err(BusError::TurnLimit {
            exchange: 1,
            max_turns: 6,
        });
        let json = serde_json::to_value(&refused).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"err": {"error": "turn_limit", "exchange": 1, "max_turns": 6}})
        );
        assert_eq!(
            serde_json::from_value::<BridgeOutcome>(json).unwrap(),
            refused
        );

        let ok = BridgeOutcome::Ok(BusReply::Posted(Posted {
            message_id: 1,
            exchange: 1,
            turn: 1,
            turns_left: 5,
            delivered_to: vec!["s2".into()],
            queued_for_role: None,
        }));
        let back: BridgeOutcome =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(back, ok);
    }
}
