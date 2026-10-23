//! The command connection against a stand-in Command (a WebSocket server in
//! the test) and a real agentuxd: commands are answered in order, a close
//! is explained, and the node reconnects with its answers kept.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use agentux_gateway::NodeConfig;
use agentux_gateway::channel::{CommandOptions, Out, command_loop};
use agentux_gateway::policy::Policy;
use agentux_store::Store;
use agentuxd::{Engine, FakeExecutor};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

fn request(id: &str, method: &str, extra: Value) -> Message {
    let mut params = json!({
        "protocolVersion": "v1", "organizationId": "org", "nodeId": "node-01",
        "messageId": id, "occurredAt": "2026-10-23T12:00:00Z", "commandId": id,
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    Message::text(
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
    )
}

type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn answer(ws: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("an answer in time")
            .unwrap()
            .unwrap();
        if message.is_text() {
            return serde_json::from_str(message.to_text().unwrap()).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_are_answered_in_order_and_a_close_is_explained() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("agentuxd.sock");
    let engine = Engine::new(
        Store::open(&dir.path().join("agentuxd.db")).unwrap(),
        Arc::new(FakeExecutor::default()),
    );
    let (_stop_daemon, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn({
        let socket = socket.clone();
        async move {
            agentuxd::server::serve(engine, &socket, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (out, mut logs) = tokio::sync::mpsc::unbounded_channel::<Out>();
    tokio::spawn(command_loop(
        NodeConfig {
            command_url: format!("http://{address}"),
            organization_id: "org".into(),
            node_id: "node-01".into(),
            token: "t".into(),
            command_instance_id: None,
        },
        socket,
        CommandOptions {
            policy: Policy::default(),
            pending: dir.path().join("pending.json"),
        },
        out,
    ));

    let (stream, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    // Three at once: answered in the order sent.
    ws.send(request("c1", "harness.list", json!({})))
        .await
        .unwrap();
    ws.send(request("c2", "run.snapshot", json!({"runId": "nope"})))
        .await
        .unwrap();
    ws.send(request("c3", "harness.list", json!({})))
        .await
        .unwrap();
    let answers = [
        answer(&mut ws).await,
        answer(&mut ws).await,
        answer(&mut ws).await,
    ];
    let ids: Vec<&str> = answers.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    assert!(answers[0]["result"].is_array(), "{}", answers[0]);
    assert!(
        answers[1]["error"].is_object(),
        "an unknown run: {}",
        answers[1]
    );

    // Command closes the connection: the node says why, and comes back.
    ws.close(Some(CloseFrame {
        code: CloseCode::Normal,
        reason: "replaced by a new connection".into(),
    }))
    .await
    .unwrap();
    let explained = loop {
        let Some(Out::Log(line)) = tokio::time::timeout(Duration::from_secs(10), logs.recv())
            .await
            .expect("a log line in time")
        else {
            continue;
        };
        if line.contains("closed") {
            break line;
        }
    };
    assert!(
        explained.contains("closed by Command: 1000 replaced by a new connection"),
        "{explained}"
    );
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("the node reconnects")
        .unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
    // The same commandId gets the same answer on the new connection.
    ws.send(request("c1", "harness.list", json!({})))
        .await
        .unwrap();
    assert_eq!(answer(&mut ws).await, answers[0]);
}
