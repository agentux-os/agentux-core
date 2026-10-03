//! Drives `AcpSession` against a fake ACP agent built with the SDK's agent
//! side, running in-process over a byte pipe. No vendor CLI or network needed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use agentux_harness::{
    AcpHarness, AcpSession, Decision, Error, Event, Events, FileDiff, Harness, HarnessSession,
    HarnessSpec, PermissionHandler, PermissionRequest, PlanEntry, PlanStatus, StopReason, ToolCall,
    ToolCallUpdate, ToolKind, ToolStatus, permission_handler,
};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(10);
const SESSION: &str = "fake-session";

/// Prompts the fake agent understands.
const WORK: &str = "edit the file";
const WAIT_FOR_PERMISSION: &str = "ask and wait";

fn send(
    cx: &ConnectionTo<Client>,
    update: acp::SessionUpdate,
) -> agent_client_protocol::Result<()> {
    cx.send_notification(acp::SessionNotification::new(SESSION, update))
}

fn text(s: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(s))
}

fn permission(tool_call_id: &str) -> acp::RequestPermissionRequest {
    use acp::PermissionOptionKind::*;
    acp::RequestPermissionRequest::new(
        SESSION,
        acp::ToolCallUpdate::new(
            tool_call_id.to_string(),
            acp::ToolCallUpdateFields::new()
                .title("Write src/lib.rs")
                .kind(acp::ToolKind::Edit),
        ),
        vec![
            acp::PermissionOption::new("allow".to_string(), "Allow", AllowOnce),
            acp::PermissionOption::new("deny".to_string(), "Deny", RejectOnce),
        ],
    )
}

/// What the fake agent saw, for assertions.
#[derive(Default)]
struct Seen {
    cwd: Option<PathBuf>,
    permission_outcomes: Vec<String>,
}

/// One prompt turn of the fake agent. `WORK` streams a message, a plan and a
/// file edit that needs permission, then usage. `WAIT_FOR_PERMISSION` asks
/// for permission and ends the turn as cancelled once the client cancels.
async fn turn(
    prompt: String,
    cx: ConnectionTo<Client>,
    cancelled: CancellationToken,
    seen: Arc<Mutex<Seen>>,
) -> agent_client_protocol::Result<acp::StopReason> {
    let outcome = |response: acp::RequestPermissionResponse| match response.outcome {
        acp::RequestPermissionOutcome::Selected(selected) => selected.option_id.0.to_string(),
        _ => "cancelled".to_string(),
    };
    if prompt == WAIT_FOR_PERMISSION {
        let response = cx.send_request(permission("t9")).block_task().await?;
        seen.lock()
            .unwrap()
            .permission_outcomes
            .push(outcome(response));
        cancelled.cancelled().await;
        return Ok(acp::StopReason::Cancelled);
    }

    send(
        &cx,
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text("Editing "))),
    )?;
    send(
        &cx,
        acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(text("hmm"))),
    )?;
    send(
        &cx,
        acp::SessionUpdate::Plan(acp::Plan::new(vec![
            acp::PlanEntry::new(
                "Write the file",
                acp::PlanEntryPriority::High,
                acp::PlanEntryStatus::InProgress,
            ),
            acp::PlanEntry::new(
                "Run tests",
                acp::PlanEntryPriority::Medium,
                acp::PlanEntryStatus::Pending,
            ),
        ])),
    )?;
    send(
        &cx,
        acp::SessionUpdate::ToolCall(
            acp::ToolCall::new("t1".to_string(), "Write src/lib.rs")
                .kind(acp::ToolKind::Edit)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![
                    acp::Diff::new("src/lib.rs", "new")
                        .old_text("old".to_string())
                        .into(),
                ]),
        ),
    )?;
    let response = cx.send_request(permission("t1")).block_task().await?;
    let decision = outcome(response);
    seen.lock()
        .unwrap()
        .permission_outcomes
        .push(decision.clone());
    let (status, output) = if decision == "allow" {
        (acp::ToolCallStatus::Completed, "written")
    } else {
        (acp::ToolCallStatus::Failed, "denied")
    };
    send(
        &cx,
        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            "t1".to_string(),
            acp::ToolCallUpdateFields::new()
                .status(status)
                .content(vec![acp::ToolCallContent::Content(acp::Content::new(
                    text(output),
                ))]),
        )),
    )?;
    send(
        &cx,
        acp::SessionUpdate::UsageUpdate(
            acp::UsageUpdate::new(1200, 200_000).cost(acp::Cost::new(0.25, "USD")),
        ),
    )?;
    send(
        &cx,
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text("done."))),
    )?;
    Ok(acp::StopReason::EndTurn)
}

/// Starts the fake agent and connects a session to it.
async fn connect(
    permissions: PermissionHandler,
) -> (
    AcpSession,
    Events,
    Arc<Mutex<Seen>>,
    JoinHandle<agent_client_protocol::Result<()>>,
) {
    let (client_io, agent_io) = tokio::io::duplex(64 * 1024);
    let (agent_read, agent_write) = tokio::io::split(agent_io);
    let (client_read, client_write) = tokio::io::split(client_io);

    let seen = Arc::new(Mutex::new(Seen::default()));
    let cancelled = CancellationToken::new();
    let agent = Agent
        .builder()
        .name("fake-agent")
        .on_receive_request(
            async |request: acp::InitializeRequest, responder, _cx| {
                responder.respond(acp::InitializeResponse::new(request.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let seen = Arc::clone(&seen);
                async move |request: acp::NewSessionRequest, responder, _cx| {
                    seen.lock().unwrap().cwd = Some(request.cwd);
                    responder.respond(acp::NewSessionResponse::new(SESSION))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let seen = Arc::clone(&seen);
                let cancelled = cancelled.clone();
                async move |request: acp::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let prompt = match request.prompt.first() {
                        Some(acp::ContentBlock::Text(t)) => t.text.clone(),
                        _ => String::new(),
                    };
                    let (seen, cancelled) = (Arc::clone(&seen), cancelled.clone());
                    // The turn sends requests of its own, so it must run
                    // outside the dispatch loop.
                    cx.clone().spawn(async move {
                        let result = turn(prompt, cx, cancelled, seen).await;
                        responder.respond_with_result(result.map(acp::PromptResponse::new))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_: acp::CancelNotification, _cx| {
                cancelled.cancel();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let agent = tokio::spawn(agent.connect_to(ByteStreams::new(
        agent_write.compat_write(),
        agent_read.compat(),
    )));

    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let (session, events) = tokio::time::timeout(
        TIMEOUT,
        AcpSession::connect(transport, Path::new("/work/tree"), permissions),
    )
    .await
    .expect("connect timed out")
    .expect("connect failed");
    (session, events, seen, agent)
}

fn drain(events: &mut Events) -> Vec<Event> {
    std::iter::from_fn(|| events.try_recv().ok()).collect()
}

#[tokio::test]
async fn prompt_streams_events_and_asks_for_permission() {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let permissions = permission_handler({
        let asked = Arc::clone(&asked);
        move |request: PermissionRequest| {
            asked.lock().unwrap().push(request);
            async { Decision::Allow }
        }
    });
    let (session, mut events, seen, agent) = connect(permissions).await;
    assert_eq!(session.id(), SESSION);
    assert_eq!(
        seen.lock().unwrap().cwd.as_deref(),
        Some(Path::new("/work/tree"))
    );

    let stop = tokio::time::timeout(TIMEOUT, session.prompt(WORK))
        .await
        .expect("prompt timed out")
        .expect("prompt failed");
    assert_eq!(stop, StopReason::EndTurn);

    assert_eq!(
        drain(&mut events),
        [
            Event::AgentMessage("Editing ".into()),
            Event::AgentThought("hmm".into()),
            Event::Plan(vec![
                PlanEntry {
                    content: "Write the file".into(),
                    status: PlanStatus::InProgress
                },
                PlanEntry {
                    content: "Run tests".into(),
                    status: PlanStatus::Pending
                },
            ]),
            Event::ToolCall(ToolCall {
                id: "t1".into(),
                title: "Write src/lib.rs".into(),
                kind: ToolKind::Edit,
                status: ToolStatus::Pending,
            }),
            Event::Diff(FileDiff {
                tool_call_id: "t1".into(),
                path: "src/lib.rs".into(),
                old_text: Some("old".into()),
                new_text: "new".into(),
            }),
            Event::ToolCallUpdate(ToolCallUpdate {
                id: "t1".into(),
                title: None,
                status: Some(ToolStatus::Completed),
                output: Some("written".into()),
            }),
            Event::Usage(agentux_harness::Usage {
                used_tokens: 1200,
                context_tokens: 200_000,
                cost: Some(agentux_harness::Cost {
                    amount: 0.25,
                    currency: "USD".into()
                }),
            }),
            Event::AgentMessage("done.".into()),
        ]
    );
    assert_eq!(
        *asked.lock().unwrap(),
        [PermissionRequest {
            tool_call_id: "t1".into(),
            title: Some("Write src/lib.rs".into()),
            kind: Some(ToolKind::Edit),
        }]
    );
    assert_eq!(seen.lock().unwrap().permission_outcomes, ["allow"]);

    tokio::time::timeout(TIMEOUT, session.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
    // Closing the session ends the agent's side of the connection too.
    tokio::time::timeout(TIMEOUT, agent)
        .await
        .expect("agent did not stop")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn denied_permission_reaches_the_agent() {
    let permissions = permission_handler(|_| async { Decision::Deny });
    let (session, mut events, seen, _agent) = connect(permissions).await;

    let stop = tokio::time::timeout(TIMEOUT, session.prompt(WORK))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(seen.lock().unwrap().permission_outcomes, ["deny"]);
    assert!(drain(&mut events).iter().any(|event| matches!(
        event,
        Event::ToolCallUpdate(ToolCallUpdate {
            status: Some(ToolStatus::Failed),
            ..
        })
    )));
    session.shutdown().await.unwrap();
}

/// Prompts `WAIT_FOR_PERMISSION`, waits until the permission request reaches
/// the handler, cancels, and returns how the turn ended.
async fn cancelled_turn(
    session: &AcpSession,
    asked: &mut tokio::sync::mpsc::UnboundedReceiver<PermissionRequest>,
) -> StopReason {
    let prompt = session.prompt(WAIT_FOR_PERMISSION);
    tokio::pin!(prompt);
    tokio::select! {
        request = asked.recv() => assert_eq!(request.unwrap().tool_call_id, "t9"),
        result = &mut prompt => panic!("prompt ended before asking: {result:?}"),
    }
    session.cancel().unwrap();
    tokio::time::timeout(TIMEOUT, prompt)
        .await
        .expect("cancelled prompt did not end")
        .unwrap()
}

#[tokio::test]
async fn cancel_withdraws_pending_permission_and_ends_the_turn() {
    let (asked_tx, mut asked) = tokio::sync::mpsc::unbounded_channel();
    // A handler that never answers, like a human who walked away.
    let permissions = permission_handler(move |request| {
        let _ = asked_tx.send(request);
        std::future::pending()
    });
    let (session, _events, seen, _agent) = connect(permissions).await;

    assert_eq!(
        cancelled_turn(&session, &mut asked).await,
        StopReason::Cancelled
    );
    assert_eq!(seen.lock().unwrap().permission_outcomes, ["cancelled"]);

    // The session stays usable: the next turn gets a fresh cancellation.
    assert_eq!(
        cancelled_turn(&session, &mut asked).await,
        StopReason::Cancelled
    );
    assert_eq!(
        seen.lock().unwrap().permission_outcomes,
        ["cancelled", "cancelled"]
    );
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_command_is_a_spawn_error() {
    let spec = HarnessSpec {
        id: "ghost".into(),
        command: "agentux-no-such-harness".into(),
        args: vec!["acp".into()],
        env: Vec::new(),
        experimental: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let result = AcpHarness::new(spec)
        .start(dir.path(), permission_handler(|_| async { Decision::Deny }))
        .await;
    match result {
        Err(Error::Spawn { command, .. }) => assert_eq!(command, "agentux-no-such-harness acp"),
        Err(other) => panic!("expected a spawn error, got {other}"),
        Ok(_) => panic!("expected a spawn error"),
    }
}

/// The harness runs in the requested directory with the spec's environment,
/// and a harness that exits instead of speaking ACP fails `start`.
#[cfg(unix)]
#[tokio::test]
async fn harness_runs_in_cwd_with_env_and_exit_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let spec = HarnessSpec {
        id: "shell".into(),
        command: "sh".into(),
        args: vec![
            "-c".into(),
            r#"printf '%s %s' "$(pwd)" "$AGENTUX_TEST" > seen"#.into(),
        ],
        env: vec![("AGENTUX_TEST".into(), "hello".into())],
        experimental: false,
    };
    let result = tokio::time::timeout(
        TIMEOUT,
        AcpHarness::new(spec).start(dir.path(), permission_handler(|_| async { Decision::Deny })),
    )
    .await
    .expect("start hung after the harness exited");
    assert!(
        matches!(result, Err(Error::Protocol(_) | Error::Closed)),
        "{:?}",
        result.err()
    );
    let seen = std::fs::read_to_string(dir.path().join("seen")).unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    assert_eq!(seen, format!("{} hello", cwd.display()));
}
