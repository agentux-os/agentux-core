//! Drives `AcpSession` against the fake ACP agent from `agentux-fake-agent`,
//! running in-process over a byte pipe. No vendor CLI or network needed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_fake_agent::{SESSION, Turn, acp, script};
use agentux_harness::{
    AcpHarness, AcpSession, Decision, Error, Event, Events, FileDiff, Harness, HarnessSession,
    HarnessSpec, McpServer, PermissionHandler, PermissionRequest, PlanEntry, PlanStatus,
    StopReason, ToolCall, ToolCallUpdate, ToolKind, ToolStatus, permission_handler,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Prompts the fake agent understands.
const WORK: &str = "edit the file";
const WAIT_FOR_PERMISSION: &str = "ask and wait";

/// What the fake agent saw, for assertions.
#[derive(Default)]
struct Seen {
    cwd: Option<PathBuf>,
    mcp_servers: Vec<acp::McpServer>,
    permission_outcomes: Vec<String>,
}

/// One prompt turn of the fake agent. `WORK` streams a message, a plan and a
/// file edit that needs permission, then usage. `WAIT_FOR_PERMISSION` asks
/// for permission and ends the turn as cancelled once the client cancels.
async fn turn(
    turn: Turn,
    seen: Arc<Mutex<Seen>>,
) -> agent_client_protocol::Result<acp::StopReason> {
    seen.lock().unwrap().cwd = Some(turn.cwd.clone());
    if turn.prompt == WAIT_FOR_PERMISSION {
        let outcome = turn
            .request_permission("t9", "Write src/lib.rs", acp::ToolKind::Edit)
            .await?;
        seen.lock().unwrap().permission_outcomes.push(outcome);
        turn.cancelled().await;
        return Ok(acp::StopReason::Cancelled);
    }

    turn.message("Editing ")?;
    turn.thought("hmm")?;
    turn.plan(&[
        ("Write the file", acp::PlanEntryStatus::InProgress),
        ("Run tests", acp::PlanEntryStatus::Pending),
    ])?;
    turn.tool_call(
        "t1",
        "Write src/lib.rs",
        acp::ToolKind::Edit,
        Some(("src/lib.rs", Some("old"), "new")),
    )?;
    let decision = turn
        .request_permission("t1", "Write src/lib.rs", acp::ToolKind::Edit)
        .await?;
    seen.lock()
        .unwrap()
        .permission_outcomes
        .push(decision.clone());
    let (status, output) = if decision == "allow" {
        (acp::ToolCallStatus::Completed, "written")
    } else {
        (acp::ToolCallStatus::Failed, "denied")
    };
    turn.tool_update("t1", status, Some(output))?;
    turn.usage(1200, 200_000, Some(0.25))?;
    turn.message("done.")?;
    Ok(acp::StopReason::EndTurn)
}

type Connected = (
    AcpSession,
    Events,
    Arc<Mutex<Seen>>,
    JoinHandle<agent_client_protocol::Result<()>>,
);

/// Starts the fake agent and connects a session to it.
async fn connect(permissions: PermissionHandler) -> Connected {
    connect_with(permissions, &[]).await
}

/// Like [`connect`], attaching `mcp_servers` to the session.
async fn connect_with(permissions: PermissionHandler, mcp_servers: &[McpServer]) -> Connected {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (transport, agent) = agentux_fake_agent::spawn_observed(
        script({
            let seen = Arc::clone(&seen);
            move |t| turn(t, Arc::clone(&seen))
        }),
        {
            let seen = Arc::clone(&seen);
            move |request: &acp::NewSessionRequest| {
                seen.lock().unwrap().mcp_servers = request.mcp_servers.clone();
            }
        },
    );
    let (session, events) = tokio::time::timeout(
        TIMEOUT,
        AcpSession::connect_with_mcp_servers(
            transport,
            Path::new("/work/tree"),
            mcp_servers,
            permissions,
        ),
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

    let stop = tokio::time::timeout(TIMEOUT, session.prompt(WORK))
        .await
        .expect("prompt timed out")
        .expect("prompt failed");
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(
        seen.lock().unwrap().cwd.as_deref(),
        Some(Path::new("/work/tree"))
    );

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
async fn mcp_servers_are_passed_in_session_new() {
    let bus = McpServer {
        name: "agentux".into(),
        command: "/usr/bin/aux".into(),
        args: vec!["bus-stdio".into(), "--session-token".into(), "t1".into()],
        env: vec![("AGENTUX_LOG".into(), "debug".into())],
    };
    let (session, _events, seen, _agent) = connect_with(
        permission_handler(|_| async { Decision::Deny }),
        std::slice::from_ref(&bus),
    )
    .await;
    assert_eq!(
        seen.lock().unwrap().mcp_servers,
        [acp::McpServer::Stdio(
            acp::McpServerStdio::new("agentux", "/usr/bin/aux")
                .args(bus.args.clone())
                .env(vec![acp::EnvVariable::new("AGENTUX_LOG", "debug")])
        )]
    );
    session.shutdown().await.unwrap();

    // Without servers the list is empty, as before.
    let (session, _events, seen, _agent) =
        connect(permission_handler(|_| async { Decision::Deny })).await;
    assert!(seen.lock().unwrap().mcp_servers.is_empty());
    session.shutdown().await.unwrap();
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

#[tokio::test]
async fn sessions_reopen_with_session_load_without_replaying_events() {
    use agentux_fake_agent::{REPLAYED, Setup};
    use agentux_harness::SessionOptions;

    let loaded = Arc::new(Mutex::new(Vec::<String>::new()));
    let agent = script(|turn: Turn| async move {
        turn.message(&format!("got {}", turn.prompt))?;
        Ok(acp::StopReason::EndTurn)
    });
    let setup = Setup {
        load_session: true,
        on_load_session: Box::new({
            let loaded = Arc::clone(&loaded);
            move |id| loaded.lock().unwrap().push(id.to_string())
        }),
        ..Setup::default()
    };
    let (transport, _agent) = agentux_fake_agent::spawn_with(agent, setup);
    let options = SessionOptions {
        load: Some("vendor-123".into()),
        ..SessionOptions::default()
    };
    let deny = permission_handler(|_| async { Decision::Deny });
    let (session, mut events) =
        AcpSession::connect_with(transport, Path::new("/work"), &options, deny)
            .await
            .unwrap();
    assert_eq!(session.id(), "vendor-123");
    assert!(session.can_load());
    assert_eq!(*loaded.lock().unwrap(), ["vendor-123"]);
    let stop = tokio::time::timeout(TIMEOUT, session.prompt("next"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    let mut texts = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::AgentMessage(text) = event {
            texts.push(text);
        }
    }
    assert!(!texts.iter().any(|t| t == REPLAYED), "{texts:?}");
    assert_eq!(texts, ["got next"]);

    // An agent without loadSession cannot reopen sessions.
    let (transport, _agent) =
        agentux_fake_agent::spawn(script(|_turn: Turn| async { Ok(acp::StopReason::EndTurn) }));
    let deny = permission_handler(|_| async { Decision::Deny });
    let err = AcpSession::connect_with(transport, Path::new("/work"), &options, deny)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, Error::LoadUnsupported), "{err}");
}
