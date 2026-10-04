//! Terminal mode: `terminals.*` over the socket, with real processes on
//! pseudo-terminals (`sh`, `cat`) and in-process fake ACP agents. No vendor
//! CLI is needed: the harness TUI is stood in for by a shell command.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_api::rpc::{OpenTerminal, TerminalCommand, TerminalState};
use agentux_api::{
    Client, EventBody, MessageFrom, RunStatus, Session, SessionEvent, SessionState, TerminalEvents,
    TerminalNotice,
};
use agentux_fake_agent::{REPLAYED, Setup, acp, script};
use agentux_harness::{AcpSession, Events, PermissionHandler, SessionOptions};
use agentux_store::Store;
use agentuxd::executor::BoxFuture;
use agentuxd::{
    AcpExecutor, Engine, FakeExecutor, LaunchSpec, Launcher, Settings, TuiCommands, server,
};
use common::{Fixture, fixture, wait_for};
use tokio::sync::oneshot;

const PIPELINE: &str = "version: 1
roles:
  planner:
    harness: fake-planner
pipeline:
  - step: plan
    role: planner
    approve: true
";

/// What the fake agents saw: prompts, and the session each launch loaded
/// (`None` for a new session).
#[derive(Default)]
struct Seen {
    prompts: Vec<String>,
    launches: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Seen>>;

/// Starts a fake agent per launch that can reopen sessions (`session/load`).
struct LoadingLauncher {
    seen: Shared,
}

impl Launcher for LoadingLauncher {
    fn launch<'a>(
        &'a self,
        spec: LaunchSpec<'a>,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .launches
                .push(spec.load.map(str::to_string));
            let seen = Arc::clone(&self.seen);
            let agent = script(move |turn| {
                seen.lock().unwrap().prompts.push(turn.prompt.clone());
                async move {
                    turn.message("1. Do it\n")?;
                    Ok(acp::StopReason::EndTurn)
                }
            });
            let setup = Setup {
                load_session: true,
                ..Setup::default()
            };
            let (transport, _agent) = agentux_fake_agent::spawn_with(agent, setup);
            let options = SessionOptions {
                load: spec.load.map(str::to_string),
                ..SessionOptions::default()
            };
            AcpSession::connect_with(transport, spec.cwd, &options, permissions)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Serves the API for `engine` on a socket in the fixture's temp dir.
async fn serve(f: &Fixture, engine: &Engine) -> (PathBuf, oneshot::Sender<()>) {
    let socket = f.tmp.path().join("run").join("agentuxd.sock");
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn({
        let socket = socket.clone();
        let engine = engine.clone();
        async move {
            server::serve(engine, &socket, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    for _ in 0..500 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (socket, stop)
}

/// Reads terminal output until it contains `pattern`; returns all of it.
async fn read_until(events: &mut TerminalEvents, pattern: &str) -> String {
    let mut seen = String::new();
    let read = tokio::time::timeout(Duration::from_secs(15), async {
        while !seen.contains(pattern) {
            match events.next().await.unwrap() {
                Some(TerminalNotice::Output(bytes)) => {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                }
                other => panic!("expected output, got {other:?}; so far: {seen:?}"),
            }
        }
    })
    .await;
    assert!(read.is_ok(), "no {pattern:?} in the output: {seen:?}");
    seen
}

/// Skips output until the exit.
async fn exit_code(events: &mut TerminalEvents) -> Option<i32> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.next().await.unwrap() {
                Some(TerminalNotice::Output(_)) => {}
                Some(TerminalNotice::Exit(code)) => return code,
                None => panic!("the daemon closed the connection before the exit"),
            }
        }
    })
    .await
    .expect("no terminal_exit")
}

async fn session_of(engine: &Engine, run_id: &str, done: impl Fn(&Session) -> bool) -> Session {
    for _ in 0..1000 {
        if let Some(session) = engine
            .sessions(Some(run_id))
            .unwrap()
            .into_iter()
            .find(|s| done(s))
        {
            return session;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no such session: {:?}", engine.sessions(Some(run_id)));
}

fn open(
    session_id: Option<&str>,
    run_id: Option<&str>,
    command: Option<TerminalCommand>,
) -> OpenTerminal {
    OpenTerminal {
        session_id: session_id.map(str::to_string),
        run_id: run_id.map(str::to_string),
        command,
        cols: 80,
        rows: 24,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shell_round_trip_with_resize_and_exit() {
    let f = fixture(PIPELINE);
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let run = f.start(&engine).await;
    let run = wait_for(&engine, &run.id, |r| r.status == RunStatus::Waiting).await;
    let worktree = run.worktree.clone().unwrap();
    let (socket, _stop) = serve(&f, &engine).await;

    let client = Client::connect(&socket).await.unwrap();
    let (terminal, mut events, mut input) = client
        .open_terminal(&open(None, Some(&run.id), None))
        .await
        .unwrap();
    assert_eq!(terminal.command, TerminalCommand::Shell);
    assert_eq!(terminal.state, TerminalState::Running);
    assert_eq!(terminal.cwd, worktree);
    assert_eq!(terminal.fallback, None);

    // The quotes keep the typed command (echoed by the terminal) from
    // matching the output.
    input
        .write(b"echo \"round-$((40+2)):$AGENTUX_RUN_ID:$PWD:end\"\n")
        .await
        .unwrap();
    read_until(&mut events, &format!("round-42:{}:{worktree}:end", run.id)).await;

    input.resize(100, 30).await.unwrap();
    input.write(b"stty size | tr ' ' x\n").await.unwrap();
    read_until(&mut events, "30x100").await;

    let mut lister = Client::connect(&socket).await.unwrap();
    let listed = lister.list_terminals(&Default::default()).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].cols, listed[0].rows), (100, 30));

    input.write(b"exit 7\n").await.unwrap();
    assert_eq!(exit_code(&mut events).await, Some(7));
    // Gone once exited.
    for _ in 0..100 {
        if lister
            .list_terminals(&Default::default())
            .await
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        lister
            .list_terminals(&Default::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_terminal_ends_its_process() {
    let f = fixture(PIPELINE);
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let run = f.start(&engine).await;
    wait_for(&engine, &run.id, |r| r.status == RunStatus::Waiting).await;
    let (socket, _stop) = serve(&f, &engine).await;

    let client = Client::connect(&socket).await.unwrap();
    let (terminal, mut events, mut input) = client
        .open_terminal(&open(None, Some(&run.id), Some(TerminalCommand::Shell)))
        .await
        .unwrap();
    input.write(b"echo \"re\"ady\n").await.unwrap();
    read_until(&mut events, "ready").await;
    input.close().await.unwrap();
    exit_code(&mut events).await;

    // Unknown once closed.
    let mut other = Client::connect(&socket).await.unwrap();
    let err = other
        .call::<_, serde_json::Value>(
            "terminals.write",
            &serde_json::json!({ "terminalId": terminal.terminal_id, "data": "eA==" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no terminal"), "{err}");

    // A terminal belongs to the connection that opened it.
    let client = Client::connect(&socket).await.unwrap();
    let (terminal, events, input) = client
        .open_terminal(&open(None, Some(&run.id), None))
        .await
        .unwrap();
    drop((events, input));
    for _ in 0..500 {
        let list = other.list_terminals(&Default::default()).await.unwrap();
        if list.iter().all(|t| t.terminal_id != terminal.terminal_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the terminal outlived its connection");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_terminal_requests() {
    let f = fixture(PIPELINE);
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let (socket, _stop) = serve(&f, &engine).await;
    let mut client = Client::connect(&socket).await.unwrap();
    let call = |params: serde_json::Value| {
        let socket = socket.clone();
        async move {
            Client::connect(&socket)
                .await
                .unwrap()
                .call::<_, serde_json::Value>("terminals.open", &params)
                .await
                .unwrap_err()
                .to_string()
        }
    };
    assert!(
        call(serde_json::json!({"cols": 80, "rows": 24}))
            .await
            .contains("sessionId or a runId")
    );
    assert!(
        call(serde_json::json!({"runId": "nope", "cols": 80, "rows": 24}))
            .await
            .contains("no run")
    );
    assert!(
        call(serde_json::json!({"sessionId": "nope", "cols": 80, "rows": 24}))
            .await
            .contains("no session")
    );
    assert!(
        call(serde_json::json!({"runId": "x", "cols": 0, "rows": 24}))
            .await
            .contains("at least 1")
    );
    assert!(
        client
            .list_terminals(&Default::default())
            .await
            .unwrap()
            .is_empty()
    );
}

fn acp_engine(f: &Fixture, seen: &Shared, tui: TuiCommands) -> Engine {
    let executor = Arc::new(AcpExecutor::new(LoadingLauncher {
        seen: Arc::clone(seen),
    }));
    let settings = Settings {
        tui,
        ..Settings::default()
    };
    Engine::with_settings(Store::open(&f.database).unwrap(), executor, settings)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_without_tui_resume_gets_a_shell() {
    let f = fixture(PIPELINE);
    let seen = Shared::default();
    // The default table: `fake-planner` is no harness it knows.
    let engine = acp_engine(&f, &seen, TuiCommands::default());
    let run = f.start(&engine).await;
    let run = wait_for(&engine, &run.id, |r| r.status == RunStatus::Waiting).await;
    let session = session_of(&engine, &run.id, |s| s.state == SessionState::Idle).await;
    assert_eq!(
        session.vendor_session_id.as_deref(),
        Some(agentux_fake_agent::SESSION)
    );
    let (socket, _stop) = serve(&f, &engine).await;

    let client = Client::connect(&socket).await.unwrap();
    let (terminal, mut events, mut input) = client
        .open_terminal(&open(Some(&session.id), None, None))
        .await
        .unwrap();
    assert_eq!(terminal.command, TerminalCommand::Shell);
    let reason = terminal.fallback.clone().unwrap();
    assert!(reason.contains("cannot resume a session by id"), "{reason}");
    read_until(&mut events, "This is a shell in the run's worktree").await;
    input
        .write(b"echo \"$AGENTUX_SESSION_ID-$AGENTUX_ROLE\"-x\n")
        .await
        .unwrap();
    read_until(&mut events, &format!("{}-planner-x", session.id)).await;

    // The session was not handed over: it takes messages right away.
    let prompted = engine.prompt_session(&session.id, "still there?").unwrap();
    assert!(!prompted.queued);
    for _ in 0..500 {
        if seen
            .lock()
            .unwrap()
            .prompts
            .iter()
            .any(|p| p == "still there?")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        seen.lock()
            .unwrap()
            .prompts
            .iter()
            .any(|p| p == "still there?")
    );
    assert_eq!(seen.lock().unwrap().launches, [None]);
    input.write(b"exit\n").await.unwrap();
    assert_eq!(exit_code(&mut events).await, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn turns_wait_while_the_session_is_in_its_tui() {
    let f = fixture(PIPELINE);
    let seen = Shared::default();
    // The "TUI" says which session it resumes, then echoes its input.
    let tui = TuiCommands::new(|harness, id| {
        assert_eq!(harness, "fake-planner");
        Some(
            ["sh", "-c", "echo \"resuming $1\"; exec cat", "sh", id]
                .map(str::to_string)
                .to_vec(),
        )
    });
    let engine = acp_engine(&f, &seen, tui);
    let run = f.start(&engine).await;
    let run = wait_for(&engine, &run.id, |r| r.status == RunStatus::Waiting).await;
    let session = session_of(&engine, &run.id, |s| s.state == SessionState::Idle).await;
    let (socket, _stop) = serve(&f, &engine).await;

    let client = Client::connect(&socket).await.unwrap();
    let (terminal, mut events, mut input) = client
        .open_terminal(&open(Some(&session.id), None, None))
        .await
        .unwrap();
    assert_eq!(terminal.command, TerminalCommand::HarnessTui);
    assert_eq!(terminal.fallback, None);
    read_until(
        &mut events,
        &format!("resuming {}", agentux_fake_agent::SESSION),
    )
    .await;
    session_of(&engine, &run.id, |s| s.state == SessionState::Attached).await;
    let mut lister = Client::connect(&socket).await.unwrap();
    let listed = lister.list_terminals(&Default::default()).await.unwrap();
    assert_eq!(listed[0].state, TerminalState::Running);
    assert_eq!(listed[0].argv[0], "sh");

    // The human's message waits for the TUI.
    let prompted = engine.prompt_session(&session.id, "after the TUI").unwrap();
    assert!(prompted.queued);
    input.write(b"typed-in-tui\n").await.unwrap();
    read_until(&mut events, "typed-in-tui").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !seen
            .lock()
            .unwrap()
            .prompts
            .iter()
            .any(|p| p == "after the TUI"),
        "a prompt reached the agent while its TUI had the session"
    );

    input.close().await.unwrap();
    exit_code(&mut events).await;
    for _ in 0..500 {
        if seen
            .lock()
            .unwrap()
            .prompts
            .iter()
            .any(|p| p == "after the TUI")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    {
        let seen = seen.lock().unwrap();
        assert!(seen.prompts.iter().any(|p| p == "after the TUI"));
        // Reopened over ACP with session/load, after the TUI.
        assert_eq!(
            seen.launches,
            [None, Some(agentux_fake_agent::SESSION.to_string())]
        );
    }
    let session = session_of(&engine, &run.id, |s| s.state == SessionState::Idle).await;
    assert_eq!(session.state, SessionState::Idle);

    let events = engine.run_events(agentux_api::rpc::RunEventsParams {
        run_id: run.id.clone(),
        ..Default::default()
    });
    let texts: Vec<String> = events
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::SessionEvent {
                event: SessionEvent::Message { from, text },
                ..
            } => Some(format!("{from:?}: {text}")),
            _ => None,
        })
        .collect();
    let system = |needle: &str| {
        texts
            .iter()
            .any(|t| t.starts_with(&format!("{:?}", MessageFrom::System)) && t.contains(needle))
    };
    assert!(system("Opened in fake-planner's TUI"), "{texts:#?}");
    assert!(system("Back from fake-planner's TUI"), "{texts:#?}");
    // The history replayed by session/load is not recorded again.
    assert!(!texts.iter().any(|t| t.contains(REPLAYED)), "{texts:#?}");
    // Back to normal: the next message goes straight through.
    let prompted = engine.prompt_session(&session.id, "again").unwrap();
    assert!(!prompted.queued);
}
