//! The JSON-RPC API over a real Unix socket in a temp dir.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agentux_api::rpc::{self, code};
use agentux_api::{Client, ClientError, EventBody, RequestStatus, RunStatus};
use agentuxd::{FakeExecutor, server};
use common::{fixture, pipeline};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

fn rpc_code(result: Result<impl std::fmt::Debug, ClientError>) -> i64 {
    match result {
        Err(ClientError::Rpc(e)) => e.code,
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn api_round_trip_over_a_unix_socket() {
    let yaml = pipeline("true").replace(
        "  - step: plan\n    role: planner\n",
        "  - step: plan\n    role: planner\n    approve: true\n",
    );
    let f = fixture(&yaml);
    let socket = f.tmp.path().join("run").join("agentuxd.sock");
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let socket = socket.clone();
        async move {
            server::serve(engine, &socket, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut client = Client::connect(&socket).await.unwrap();
    let project = client
        .register_project(f.repo.to_str().unwrap())
        .await
        .unwrap();
    // Registering again is idempotent.
    assert_eq!(
        client
            .register_project(f.repo.to_str().unwrap())
            .await
            .unwrap(),
        project
    );

    let run = client
        .start_run(&rpc::StartRun {
            project_id: project.id.clone(),
            title: Some("Login fix".into()),
            prompt: Some("Fix the login bug".into()),
            issue: Some(12),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(run.title, "Login fix");
    assert_eq!(run.issue, Some(12));

    // Watch the run from the beginning on a second connection.
    let mut events = Client::connect(&socket)
        .await
        .unwrap()
        .subscribe(&rpc::Subscribe {
            run_id: Some(run.id.clone()),
            since: Some(0),
        })
        .await
        .unwrap();

    // Wait for the approval request to show up in the stream.
    let request = loop {
        let event = events.next().await.unwrap().expect("stream ended");
        assert_eq!(event.run_id.as_deref(), Some(run.id.as_str()));
        if let EventBody::Request { request } = event.body {
            break request;
        }
    };
    assert_eq!(request.status, RequestStatus::Pending);
    let inbox = client.list_requests(true).await.unwrap();
    assert_eq!(inbox, std::slice::from_ref(&request));
    let runs = client.list_runs().await.unwrap();
    assert_eq!(runs.len(), 1);

    client
        .approve(&request.id, Some("go".into()))
        .await
        .unwrap();
    assert_eq!(
        rpc_code(client.approve(&request.id, None).await),
        code::CONFLICT
    );

    // Follow the stream until the run is done; sequence numbers only grow.
    let mut last_seq = 0;
    loop {
        let event = events.next().await.unwrap().expect("stream ended");
        assert!(event.seq > last_seq);
        last_seq = event.seq;
        if let EventBody::Run { run } = event.body
            && run.status.is_terminal()
        {
            assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
            break;
        }
    }

    let detail = client.get_run(&run.id).await.unwrap();
    assert_eq!(detail.run.status, RunStatus::Done);
    assert_eq!(detail.requests.len(), 1);
    assert_eq!(detail.requests[0].answer.as_deref(), Some("go"));
    assert!(!detail.attempts.is_empty());

    // Errors carry their codes.
    assert_eq!(rpc_code(client.get_run("nope").await), code::NOT_FOUND);
    assert_eq!(rpc_code(client.cancel_run(&run.id).await), code::CONFLICT);
    assert_eq!(
        rpc_code(
            client
                .call::<_, serde_json::Value>("runs.explode", &())
                .await
        ),
        code::METHOD_NOT_FOUND
    );
    assert_eq!(
        rpc_code(
            client
                .call::<_, serde_json::Value>("runs.get", &serde_json::json!({"wrong": 1}))
                .await
        ),
        code::INVALID_PARAMS
    );

    // Malformed lines get a parse error and the connection stays usable.
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    writer.write_all(b"{not json\n").await.unwrap();
    let response: rpc::Response =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(response.error.unwrap().code, code::PARSE_ERROR);
    writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"projects.list\"}\n")
        .await
        .unwrap();
    let response: rpc::Response =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(response.id, serde_json::json!("a"));
    assert_eq!(
        response.result.unwrap()[0]["id"],
        serde_json::json!(project.id)
    );

    // A second daemon refuses to take over a live socket.
    let other = f.engine(Arc::new(FakeExecutor::default()));
    let err = server::serve(other, &socket, async {}).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_describes_the_run_it_would_start() {
    let yaml = pipeline("cargo test").replace(
        "  planner:\n    harness: fake-a\n",
        "  planner:\n    harness: fake-a\n    model: big\n",
    ) + "permissions:\n  read: allow\n  execute: ask\n  roles:\n    reviewer:\n      edit: deny\n";
    let yaml = yaml.replace(
        "  - step: plan\n    role: planner\n",
        "  - step: plan\n    role: planner\n    approve: true\n",
    );
    let f = fixture(&yaml);
    let socket = f.tmp.path().join("run").join("agentuxd.sock");
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn({
        let socket = socket.clone();
        async move {
            server::serve(engine, &socket, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "socket never appeared"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut client = Client::connect(&socket).await.unwrap();
    let project = client
        .register_project(f.repo.to_str().unwrap())
        .await
        .unwrap();

    let plan = client.describe_project(&project.id).await.unwrap();
    assert_eq!(plan.source, "agentux.yaml");
    let kinds: Vec<&str> = plan.steps.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["plan", "implement", "gate", "review", "pull_request"]
    );
    assert!(plan.steps[0].approve, "the plan waits for approval");
    assert_eq!(plan.steps[0].role.as_deref(), Some("planner"));
    assert_eq!(plan.steps[2].checks, ["check"]);
    assert_eq!(
        (plan.steps[2].loop_to, plan.steps[2].loop_limit),
        (Some(1), Some(3))
    );
    assert_eq!(
        (plan.steps[3].loop_to, plan.steps[3].loop_limit),
        (Some(1), Some(2))
    );
    assert_eq!(plan.roles["planner"].harness, "fake-a");
    assert_eq!(plan.roles["planner"].model.as_deref(), Some("big"));
    assert_eq!(plan.roles["reviewer"].harness, "fake-b");
    assert_eq!(plan.checks[0].run, "cargo test");
    let implementer = &plan.permissions["implementer"];
    assert!(implementer["allow"].contains(&"read".to_string()));
    assert!(implementer["ask"].contains(&"execute".to_string()));
    assert!(plan.permissions["reviewer"]["deny"].contains(&"edit".to_string()));

    // Nothing was started.
    assert!(client.list_runs().await.unwrap().is_empty());
    assert_eq!(
        rpc_code(client.describe_project("nope").await),
        code::NOT_FOUND
    );
    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
