//! `aux bus-stdio` driven by an rmcp client over the child's stdio, the way a
//! harness launches it.

use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::json;

const TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn standalone_serves_the_bus_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("agentux.yaml"),
        "version: 1
roles:
  implementer:
    harness: claude-code
  reviewer:
    harness: codex
pipeline:
  - step: implement
    role: implementer
  - step: pull_request
bus:
  allow: [post_message, get_run_state]
",
    )
    .unwrap();

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_aux"));
    command.args([
        "bus-stdio",
        "--standalone",
        "--project",
        dir.path().to_str().unwrap(),
        "--role",
        "implementer",
    ]);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = tokio::time::timeout(TIMEOUT, ().serve(transport))
        .await
        .expect("initialize timed out")
        .expect("initialize failed");

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["get_run_state", "post_message", "read_messages"]);

    // The only other party in standalone mode is the human.
    let posted = client
        .call_tool(
            CallToolRequestParams::new("post_message").with_arguments(
                json!({"to": "human", "body": "hello"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_ne!(posted.is_error, Some(true), "{posted:?}");
    assert_eq!(posted.structured_content.unwrap()["turn"], 1);

    // A message for a role nobody plays waits for it.
    let queued = client
        .call_tool(
            CallToolRequestParams::new("post_message").with_arguments(
                json!({"to": "role:reviewer", "body": "anyone?"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        queued.structured_content.unwrap()["queued_for_role"],
        "reviewer"
    );
    client.cancel().await.unwrap();
}

#[test]
fn without_standalone_it_needs_the_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_aux"))
        .args([
            "bus-stdio",
            "--socket",
            "/nonexistent/agentuxd.sock",
            "--session-token",
            "t",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot reach agentuxd at /nonexistent/agentuxd.sock"),
        "{stderr}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_aux"))
        .args(["bus-stdio"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--session-token"));
}
