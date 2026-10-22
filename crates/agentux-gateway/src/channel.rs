//! The command connection: the node dials Command's
//! `/v1/protocol/connect?nodeId=<id>` (WebSocket, its bearer token) and
//! answers the commands that come over it ([`crate::commands::handle`]).
//! Every few seconds it also runs the held commands someone at the node
//! decided on. It reconnects with backoff, and runs never depend on it.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::NodeConfig;
use crate::commands::{self, Answers, Pending};
use crate::policy::Policy;

/// What the command connection reports to the gateway's main loop.
pub enum Out {
    /// An event to send to Command: method and payload.
    Event(String, Value),
    Log(String),
}

/// How the node takes commands.
pub struct CommandOptions {
    pub policy: Policy,
    /// Where held commands wait ([`crate::commands::pending_path`]).
    pub pending: PathBuf,
}

/// `wss://command/v1/protocol/connect?nodeId=...` from the config's URL.
pub fn connect_url(config: &NodeConfig) -> String {
    let base = config.command_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    let node: String = config
        .node_id
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect();
    format!("{base}/v1/protocol/connect?nodeId={node}")
}

/// Keeps the command connection up until the task is dropped.
pub async fn command_loop(
    config: NodeConfig,
    socket: PathBuf,
    options: CommandOptions,
    out: UnboundedSender<Out>,
) {
    let pending = Pending::new(options.pending);
    let mut answers = Answers::default();
    let mut backoff = Duration::from_secs(5);
    let mut said_down = false;
    loop {
        let mut client = match agentux_api::Client::connect(&socket).await {
            Ok(client) => client,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let request = match connect_url(&config).into_client_request() {
            Ok(mut request) => {
                if let Ok(value) = format!("Bearer {}", config.token).parse() {
                    request.headers_mut().insert("Authorization", value);
                }
                request
            }
            Err(e) => {
                let _ = out.send(Out::Log(format!("bad Command URL for commands: {e}")));
                return;
            }
        };
        let ws = match tokio_tungstenite::connect_async(request).await {
            Ok((ws, response)) => {
                let named = response
                    .headers()
                    .get(auxp::INSTANCE_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                if let Some(pinned) = &config.command_instance_id
                    && named.as_deref() != Some(pinned.as_str())
                {
                    let _ = out.send(Out::Log(format!(
                        "{}: the command connection answers as {}, this node enrolled with {pinned}; taking no commands",
                        crate::INSTANCE_MISMATCH,
                        named.as_deref().unwrap_or("an unknown server")
                    )));
                    tokio::time::sleep(Duration::from_secs(300)).await;
                    continue;
                }
                ws
            }
            Err(e) => {
                if !said_down {
                    let _ = out.send(Out::Log(format!(
                        "no command connection to Command ({e}); retrying"
                    )));
                    said_down = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        backoff = Duration::from_secs(5);
        said_down = false;
        let _ = out.send(Out::Log("command connection to Command open".into()));
        let (mut write, mut read) = ws.split();
        let mut decided = tokio::time::interval(Duration::from_secs(3));
        loop {
            tokio::select! {
                message = read.next() => {
                    let text = match message {
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(message)) if message.is_text() => message.to_text().unwrap_or_default().to_string(),
                        Some(Ok(_)) => continue,
                    };
                    let Ok(request) = serde_json::from_str::<Value>(&text) else { continue };
                    if request.get("method").is_none() || request.get("id").is_none() {
                        continue;
                    }
                    let handled = commands::handle(
                        &mut client,
                        &config,
                        &options.policy,
                        &pending,
                        &mut answers,
                        &request,
                    )
                    .await;
                    if write.send(Message::text(handled.response.to_string())).await.is_err() {
                        break;
                    }
                    if let Some((method, payload)) = handled.event {
                        let _ = out.send(Out::Event(method, payload));
                    }
                }
                _ = decided.tick() => {
                    for (method, payload) in commands::run_decided(&mut client, &pending).await {
                        let _ = out.send(Out::Event(method, payload));
                    }
                }
            }
        }
        let _ = out.send(Out::Log("command connection closed; reconnecting".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connect_url_follows_the_command_url() {
        let config = |url: &str| NodeConfig {
            command_url: url.into(),
            organization_id: "o".into(),
            node_id: "node a/1".into(),
            token: "t".into(),
            command_instance_id: None,
        };
        assert_eq!(
            connect_url(&config("https://command.example.org/")),
            "wss://command.example.org/v1/protocol/connect?nodeId=node%20a%2F1"
        );
        assert_eq!(
            connect_url(&config("http://localhost:8080")),
            "ws://localhost:8080/v1/protocol/connect?nodeId=node%20a%2F1"
        );
    }
}
