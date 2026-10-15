//! `helper.ask`: the cockpit's helper asks the user's harness one read-only
//! question. `AcpExecutor` runs in-process fake agents from
//! `agentux-fake-agent`; the API round trip uses `FakeExecutor`.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_api::rpc::{self, HelperAsk, code};
use agentux_api::{Client, ClientError};
use agentux_fake_agent::{Script, acp, script};
use agentux_harness::{AcpSession, Events, PermissionHandler};
use agentuxd::engine::Error;
use agentuxd::executor::BoxFuture;
use agentuxd::helper::HELPER_RULES;
use agentuxd::{AcpExecutor, Engine, FakeExecutor, LaunchSpec, Launcher, server};
use common::fixture;
use tokio::sync::oneshot;

/// What one launch looked like.
#[derive(Debug, Clone)]
struct Launch {
    harness: String,
    cwd: PathBuf,
    /// Entries of the working directory when the session started.
    entries: usize,
    mcp_servers: usize,
}

/// Starts a fake agent per launch with the same script, and records each
/// launch.
struct HelperLauncher {
    script: Script,
    launched: Arc<Mutex<Vec<Launch>>>,
}

impl Launcher for HelperLauncher {
    fn launch<'a>(
        &'a self,
        spec: LaunchSpec<'a>,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            self.launched.lock().unwrap().push(Launch {
                harness: spec.harness.to_string(),
                cwd: spec.cwd.to_path_buf(),
                entries: std::fs::read_dir(spec.cwd).unwrap().count(),
                mcp_servers: spec.mcp_servers.len(),
            });
            let (transport, _agent) = agentux_fake_agent::spawn(Arc::clone(&self.script));
            AcpSession::connect(transport, spec.cwd, permissions)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// (prompts the agent got, permission outcomes it saw)
type Seen = Arc<Mutex<(Vec<String>, Vec<String>)>>;

fn engine_with(script: Script) -> (Engine, Arc<Mutex<Vec<Launch>>>, common::Fixture) {
    let launched = Arc::new(Mutex::new(Vec::new()));
    let executor = AcpExecutor::new(HelperLauncher {
        script,
        launched: Arc::clone(&launched),
    });
    let f = fixture("version: 1\n");
    let engine = f.engine(Arc::new(executor));
    (engine, launched, f)
}

fn ask(question: &str) -> HelperAsk {
    HelperAsk {
        harness: "claude-code".into(),
        model: None,
        question: question.into(),
        context: "Cockpit view: run board.\nThe gate step runs the checks.".into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_harness_answers_in_an_empty_directory_without_the_bus() {
    let seen: Seen = Arc::default();
    let agent_seen = Arc::clone(&seen);
    let (engine, launched, _f) = engine_with(script(move |turn| {
        let seen = Arc::clone(&agent_seen);
        async move {
            seen.lock().unwrap().0.push(turn.prompt.clone());
            turn.message("A gate runs the project's checks ")?;
            turn.message("in the run's worktree.")?;
            Ok(acp::StopReason::EndTurn)
        }
    }));

    let answer = engine.ask_helper(ask("What is a gate?")).await.unwrap();
    assert_eq!(answer.harness, "claude-code");
    assert_eq!(
        answer.answer,
        "A gate runs the project's checks in the run's worktree."
    );

    let prompts = seen.lock().unwrap().0.clone();
    assert_eq!(prompts.len(), 1);
    let prompt = &prompts[0];
    assert!(prompt.starts_with(HELPER_RULES), "{prompt}");
    assert!(prompt.contains("The gate step runs the checks."));
    assert!(prompt.ends_with("## Question\n\nWhat is a gate?\n"));

    let launches = launched.lock().unwrap().clone();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].harness, "claude-code");
    assert_eq!(
        launches[0].entries, 0,
        "the helper starts in an empty directory"
    );
    assert_eq!(launches[0].mcp_servers, 0, "no agent bus for the helper");
    assert!(
        !launches[0].cwd.exists(),
        "the helper's directory is removed afterwards"
    );
    // Nothing of it lands in the event log or the runs.
    assert!(engine.runs(None).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_requests_are_denied() {
    let seen: Seen = Arc::default();
    let agent_seen = Arc::clone(&seen);
    let (engine, _launched, _f) = engine_with(script(move |turn| {
        let seen = Arc::clone(&agent_seen);
        async move {
            let outcome = turn
                .request_permission("t1", "Read ~/.ssh/id_ed25519", acp::ToolKind::Read)
                .await?;
            seen.lock().unwrap().1.push(outcome);
            turn.message("I can only answer from the docs.")?;
            Ok(acp::StopReason::EndTurn)
        }
    }));

    let answer = engine.ask_helper(ask("What is aux?")).await.unwrap();
    assert_eq!(answer.answer, "I can only answer from the docs.");
    assert_eq!(seen.lock().unwrap().1, vec!["deny".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_cancels_the_turn_and_drops_the_answer() {
    let (engine, _launched, _f) = engine_with(script(|turn| async move {
        turn.message("Let me look. ")?;
        turn.tool_call(
            "t1",
            "Run cat /etc/os-release",
            acp::ToolKind::Execute,
            None,
        )?;
        tokio::select! {
            () = turn.cancelled() => Ok(acp::StopReason::Cancelled),
            () = tokio::time::sleep(Duration::from_secs(30)) => {
                turn.message("ran it")?;
                Ok(acp::StopReason::EndTurn)
            }
        }
    }));

    let started = std::time::Instant::now();
    let err = engine
        .ask_helper(ask("Which Fedora is this?"))
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the turn was not cancelled"
    );
    match err {
        Error::Internal(message) => {
            assert!(message.contains("tried to use a tool"), "{message}");
            assert!(message.contains("cat /etc/os-release"), "{message}");
        }
        other => panic!("expected an internal error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_questions_are_invalid_params() {
    let (engine, launched, _f) =
        engine_with(script(|_turn| async move { Ok(acp::StopReason::EndTurn) }));
    for params in [
        ask("   "),
        ask(&"x".repeat(rpc::HELPER_MAX_QUESTION + 1)),
        HelperAsk {
            harness: " ".into(),
            ..ask("What is aux?")
        },
    ] {
        assert!(matches!(
            engine.ask_helper(params).await,
            Err(Error::InvalidParams(_))
        ));
    }
    // An agent that says nothing is an error too.
    assert!(matches!(
        engine.ask_helper(ask("Hello?")).await,
        Err(Error::Internal(_))
    ));
    assert_eq!(launched.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn helper_ask_over_the_socket_with_fake_agents() {
    let f = fixture("version: 1\n");
    let socket = f.tmp.path().join("run").join("agentuxd.sock");
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(Arc::clone(&executor) as Arc<dyn agentuxd::StepExecutor>);
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
    let answer = client
        .ask_helper(&HelperAsk {
            harness: "codex".into(),
            model: None,
            question: "How do I approve a plan?".into(),
            context: String::new(),
        })
        .await
        .unwrap();
    assert_eq!(answer.harness, "codex");
    assert!(
        answer.answer.starts_with("(fake agent)"),
        "{}",
        answer.answer
    );
    assert!(answer.answer.contains("How do I approve a plan?"));
    let calls = executor.helper_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].prompt.starts_with(HELPER_RULES));

    match client
        .ask_helper(&HelperAsk {
            harness: "codex".into(),
            question: String::new(),
            ..HelperAsk::default()
        })
        .await
    {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, code::INVALID_PARAMS),
        other => panic!("expected invalid params, got {other:?}"),
    }

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
