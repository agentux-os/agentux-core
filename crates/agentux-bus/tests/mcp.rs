//! The `agentux` MCP server against an rmcp client, in-process over a pipe.

use std::time::Duration;

use agentux_bus::{
    Bus, BusConfig, BusServer, BusTool, LocalEndpoint, MemoryBackend, RunId, SessionId,
    SessionIdentity,
};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

fn identity(session: &str, role: &str) -> SessionIdentity {
    SessionIdentity {
        run: "run-1".into(),
        project: "shop".into(),
        role: role.into(),
        vendor: "claude-code".into(),
        session: session.into(),
    }
}

/// Serves `session`'s MCP server on one end of a pipe and connects a client
/// to the other.
async fn connect(bus: &Bus, session: &str) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server =
        BusServer::new(LocalEndpoint::new(bus.clone(), &SessionId::from(session)).unwrap());
    tokio::spawn(async move {
        let service = server.serve(server_io).await.unwrap();
        let _ = service.waiting().await;
    });
    tokio::time::timeout(TIMEOUT, ().serve(client_io))
        .await
        .expect("initialize timed out")
        .expect("initialize failed")
}

fn bus(config: BusConfig) -> Bus {
    let bus = Bus::new(MemoryBackend::new());
    bus.open_run(RunId::from("run-1"), "shop", config).unwrap();
    bus.join(identity("impl", "implementer")).unwrap();
    bus.join(identity("rev", "reviewer")).unwrap();
    bus
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    tokio::time::timeout(
        TIMEOUT,
        client.call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args)),
    )
    .await
    .expect("call timed out")
    .expect("call failed")
}

fn structured(result: &CallToolResult) -> &Value {
    assert_ne!(result.is_error, Some(true), "tool error: {result:?}");
    result
        .structured_content
        .as_ref()
        .expect("structured result")
}

#[tokio::test]
async fn tools_are_listed_with_schemas_and_round_trip() {
    let bus = bus(BusConfig::default());
    let implementer = connect(&bus, "impl").await;
    let reviewer = connect(&bus, "rev").await;

    // The server introduces itself with the session prompt.
    let info = implementer.peer_info().expect("server info");
    let instructions = info.instructions.clone().unwrap_or_default();
    assert!(
        instructions.starts_with("You are the implementer of AgentUX run run-1"),
        "{instructions}"
    );

    let tools = implementer.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "ask_human",
            "get_run_state",
            "handoff",
            "post_message",
            "read_messages",
            "request_review"
        ]
    );
    let post = tools
        .iter()
        .find(|tool| tool.name == "post_message")
        .unwrap();
    assert!(post.description.as_deref().unwrap().contains("in_reply_to"));
    let schema = serde_json::to_value(post.input_schema.as_ref()).unwrap();
    assert_eq!(schema["required"], json!(["body"]));
    assert_eq!(
        schema["properties"]["to"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        schema["properties"]["to"]["pattern"],
        "^(run|human|role:.+|session:.+)$"
    );
    assert!(
        schema["properties"]["to"]["description"]
            .as_str()
            .unwrap()
            .contains("role:<name>")
    );

    // post_message -> read_messages -> reply in the same exchange.
    let posted = call(
        &implementer,
        "post_message",
        json!({"to": "role:reviewer", "body": "please check the tax rounding"}),
    )
    .await;
    let posted = structured(&posted);
    assert_eq!(posted["delivered_to"], json!(["rev"]));
    let id = posted["message_id"].as_u64().unwrap();

    let inbox = call(&reviewer, "read_messages", json!({})).await;
    let inbox = structured(&inbox);
    assert_eq!(inbox["remaining"], 0);
    let message = &inbox["messages"][0];
    assert_eq!(message["body"], "please check the tax rounding");
    assert_eq!(message["from"]["role"], "implementer");
    assert_eq!(message["to"], "role:reviewer");

    let replied = call(
        &reviewer,
        "post_message",
        json!({"in_reply_to": id, "body": "rounding is fine"}),
    )
    .await;
    assert_eq!(structured(&replied)["turn"], 2);
    let inbox = call(&implementer, "read_messages", json!({"limit": 5})).await;
    assert_eq!(
        structured(&inbox)["messages"][0]["body"],
        "rounding is fine"
    );

    let state = call(&implementer, "get_run_state", json!({})).await;
    assert_eq!(structured(&state)["you"]["role"], "implementer");

    implementer.cancel().await.unwrap();
    reviewer.cancel().await.unwrap();
}

#[tokio::test]
async fn refusals_are_tool_errors_and_disallowed_tools_are_hidden() {
    let mut config = BusConfig {
        max_turns_per_exchange: 1,
        ..BusConfig::default()
    };
    config.allow = vec![BusTool::PostMessage];
    let bus = bus(config);
    let client = connect(&bus, "impl").await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["post_message", "read_messages"]);

    // Hidden tools cannot be called either.
    let hidden = client
        .call_tool(
            CallToolRequestParams::new("ask_human")
                .with_arguments(json!({"question": "?"}).as_object().unwrap().clone()),
        )
        .await;
    assert!(hidden.is_err(), "{hidden:?}");

    // The turn limit reaches the agent as a readable tool error.
    let first = call(
        &client,
        "post_message",
        json!({"to": "session:rev", "body": "a"}),
    )
    .await;
    let id = structured(&first)["message_id"].as_u64().unwrap();
    let rev = connect(&bus, "rev").await;
    let refused = call(
        &rev,
        "post_message",
        json!({"in_reply_to": id, "body": "b"}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    let text = serde_json::to_string(&refused.content).unwrap();
    assert!(text.contains("limit of 1 turns"), "{text}");

    // Bad arguments are reported, not panicked on.
    let bad = call(
        &client,
        "post_message",
        json!({"to": "reviewer", "body": "x"}),
    )
    .await;
    let bad_err = bad.is_error == Some(true);
    assert!(bad_err, "{bad:?}");

    client.cancel().await.unwrap();
    rev.cancel().await.unwrap();
}
