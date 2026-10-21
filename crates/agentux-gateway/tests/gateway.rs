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
                },
                socket,
                recorder,
                Some(state),
                "0.4.8",
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
