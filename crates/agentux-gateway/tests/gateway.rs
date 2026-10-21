//! The gateway against a real agentuxd (fake agents) on a temp socket, with
//! Command replaced by a sink that records what it is sent.
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_api::{HarnessLimit, LimitKind};
use agentux_gateway::{NodeConfig, Sink};
use agentux_store::Store;
use agentuxd::{Engine, FakeExecutor};
use serde_json::Value;

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Value>>>);

impl Sink for Recorder {
    async fn send(&self, message: &Value) -> Result<(), String> {
        self.0.lock().unwrap().push(message.clone());
        Ok(())
    }
}

impl Recorder {
    fn methods(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_the_node_its_agents_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("agentuxd.sock");
    let engine = Engine::new(
        Store::open(&dir.path().join("agentuxd.db")).unwrap(),
        Arc::new(FakeExecutor::default()),
    );
    let (stop_daemon, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let engine = engine.clone();
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

    let recorder = Recorder::default();
    let (stop_gateway, stop) = tokio::sync::oneshot::channel::<()>();
    let gateway = tokio::spawn({
        let recorder = recorder.clone();
        let socket = socket.clone();
        let state = dir.path().join("gateway.json");
        async move {
            agentux_gateway::run(
                NodeConfig {
                    command_url: "http://localhost:9".into(),
                    organization_id: "org-demo".into(),
                    node_id: "node-01".into(),
                    token: "t".into(),
                    command_instance_id: None,
                },
                socket,
                recorder,
                Some(state),
                "0.4.8",
                None,
                |_| {},
                async {
                    let _ = stop.await;
                },
            )
            .await
        }
    });

    let wait_for = |method: &'static str| {
        let recorder = recorder.clone();
        async move {
            for _ in 0..500 {
                if recorder.methods().iter().any(|m| m == method) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no {method}; sent {:?}", recorder.methods());
        }
    };
    wait_for("harness.status").await;
    let methods = recorder.methods();
    assert_eq!(methods[0], "node.hello");
    assert!(
        methods.contains(&"node.heartbeat".to_string()),
        "{methods:?}"
    );

    engine
        .store()
        .write(|tx| {
            tx.set_limit(&HarnessLimit {
                harness: "codex".into(),
                model: None,
                kind: LimitKind::Weekly,
                until: Some(agentux_store::now_ms() + 3_600_000),
                until_known: true,
                message: "Weekly limit reached".into(),
                at: agentux_store::now_ms(),
            })
        })
        .unwrap();
    wait_for("harness.limits_changed").await;

    let sent = recorder.0.lock().unwrap().clone();
    let limits = sent
        .iter()
        .find(|m| m["method"] == "harness.limits_changed")
        .unwrap();
    assert_eq!(limits["params"]["limits"][0]["harness"], "codex");
    assert_eq!(limits["params"]["limits"][0]["kind"], "weekly");
    let sequences: Vec<i64> = sent
        .iter()
        .filter_map(|m| m["params"]["sequence"].as_i64())
        .collect();
    assert!(
        sequences.windows(2).all(|w| w[1] == w[0] + 1),
        "{sequences:?}"
    );
    for message in &sent {
        assert_eq!(message["params"]["nodeId"], "node-01");
        assert!(!message.to_string().contains("\"token\""));
    }

    stop_gateway.send(()).unwrap();
    gateway.await.unwrap().unwrap();
    stop_daemon.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_run_wait_for_the_node_or_are_refused() {
    use agentux_gateway::commands::{self, Answers, Pending};
    use agentux_gateway::policy::Policy;
    use serde_json::json;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("agentuxd.sock");
    let engine = Engine::new(
        Store::open(&dir.path().join("agentuxd.db")).unwrap(),
        Arc::new(FakeExecutor::default()),
    );
    let (stop_daemon, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let engine = engine.clone();
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
    let mut client = agentux_api::Client::connect(&socket).await.unwrap();
    let policy = Policy::default();
    let pending = Pending::new(dir.path().join("pending.json"));
    let mut answers = Answers::default();
    let node = NodeConfig {
        command_url: "http://localhost:9".into(),
        organization_id: "org".into(),
        node_id: "node-01".into(),
        token: "t".into(),
        command_instance_id: Some("cmd-0123456789abcdef".into()),
    };
    let other_node = NodeConfig {
        node_id: "node-02".into(),
        ..node.clone()
    };
    let request = |id: &str, method: &str, extra: serde_json::Value| {
        let mut params = json!({
            "protocolVersion": "v1", "organizationId": "org", "nodeId": "node-01",
            "messageId": id, "occurredAt": "2026-10-21T12:00:00Z", "commandId": id,
            "commandInstanceId": "cmd-0123456789abcdef",
        });
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    };

    // Allowed: runs at once.
    let listed = commands::handle(
        &mut client,
        &node,
        &policy,
        &pending,
        &mut answers,
        &request("c1", "harness.list", json!({})),
    )
    .await;
    assert_eq!(
        listed.response["result"].as_array().map(Vec::len),
        Some(4),
        "{}",
        listed.response
    );
    assert!(listed.event.is_none());

    // For another node, or with a credential: refused.
    let other = commands::handle(
        &mut client,
        &other_node,
        &policy,
        &pending,
        &mut answers,
        &request("c2", "harness.list", json!({})),
    )
    .await;
    assert_eq!(other.response["error"]["code"], commands::INVALID_PARAMS);
    let secret = commands::handle(
        &mut client,
        &node,
        &policy,
        &pending,
        &mut answers,
        &request("c3", "harness.list", json!({"apiKey": "x"})),
    )
    .await;
    assert_eq!(secret.response["error"]["code"], commands::INVALID_PARAMS);

    // From another Command server, or for another organization: denied.
    let mut foreign = request("c6", "harness.list", json!({}));
    foreign["params"]["commandInstanceId"] = json!("cmd-ffffffffffffffff");
    let refused = commands::handle(
        &mut client,
        &node,
        &policy,
        &pending,
        &mut answers,
        &foreign,
    )
    .await;
    assert_eq!(refused.response["error"]["code"], commands::DENIED);
    let mut other_org = request("c7", "harness.list", json!({}));
    other_org["params"]["organizationId"] = json!("someone-else");
    let refused = commands::handle(
        &mut client,
        &node,
        &policy,
        &pending,
        &mut answers,
        &other_org,
    )
    .await;
    assert_eq!(refused.response["error"]["code"], commands::DENIED);

    // Unknown to the policy: denied, with an event.
    let shell = commands::handle(
        &mut client,
        &node,
        &policy,
        &pending,
        &mut answers,
        &request("c4", "shell.exec", json!({})),
    )
    .await;
    assert_eq!(shell.response["error"]["code"], commands::DENIED);
    assert_eq!(
        shell.event.as_ref().unwrap().1["reason"],
        "denied_by_policy"
    );

    // Starting a run waits for the node; the same commandId gets the same answer.
    let start = request(
        "c5",
        "run.start",
        json!({"repository": "https://github.com/acme/app", "prompt": "Fix it"}),
    );
    let held = commands::handle(&mut client, &node, &policy, &pending, &mut answers, &start).await;
    assert_eq!(held.response["error"]["code"], commands::HELD);
    assert_eq!(held.event.as_ref().unwrap().0, "command.denied");
    let again = commands::handle(&mut client, &node, &policy, &pending, &mut answers, &start).await;
    assert_eq!(again.response, held.response);
    assert!(again.event.is_none());
    assert_eq!(pending.list().len(), 1);

    // Rejected at the node: reported, not run.
    pending.decide("c5", false).unwrap();
    let events = commands::run_decided(&mut client, &pending).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "command.completed");
    assert_eq!(events[0].1["ok"], false);
    assert!(pending.list().is_empty());
    assert!(engine.runs(None).unwrap().is_empty());

    stop_daemon.send(()).unwrap();
    server.await.unwrap().unwrap();
}
