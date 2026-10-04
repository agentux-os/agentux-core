//! The agent bus inside the daemon, end to end: fake ACP agents get the
//! `agentux` MCP server spec in `session/new` and call the bus the way
//! `aux bus-stdio` does (a `DaemonEndpoint` with their session token, over
//! the daemon's socket). Wakes come back to them as prompts.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentux_api::rpc::{self, StartRun, Subscribe};
use agentux_api::{
    BusEndpoint, BusMessage, BusMessageKind, Client, ClientError, EventBody, MessageFrom,
    RequestKind, RequestStatus, RunStatus, SessionEvent,
};
use agentux_bus::{
    AskHuman, BusCall, BusEndpoint as _, BusError, BusReply, DaemonEndpoint, HumanReplyStatus,
    PostMessage, ReadMessages, RequestReview, Target,
};
use agentux_fake_agent::{Setup, Turn, TurnResult, acp, script};
use agentux_harness::{AcpSession, Events, McpServer, PermissionHandler, SessionOptions};
use agentux_store::Store;
use agentuxd::executor::BoxFuture;
use agentuxd::{AcpExecutor, BusLink, Engine, LaunchSpec, Launcher, Settings, server};
use common::{Fixture, fixture, wait_for, wait_until_settled};
use tokio::sync::oneshot;

/// What the fake agents saw and did.
#[derive(Default)]
struct Seen {
    /// (harness, prompt), in order.
    prompts: Vec<(String, String)>,
    /// The bus MCP server each session got, by harness.
    servers: Vec<(String, McpServer)>,
    /// `session/set_config_option` calls: (harness, config id, value).
    set_config: Vec<(String, String, String)>,
    /// Free-form notes from the agents.
    notes: Vec<String>,
}

type Shared = Arc<Mutex<Seen>>;

/// One session's view, handed to the agent's behavior at every turn.
#[derive(Clone)]
struct Ctx {
    harness: String,
    socket: PathBuf,
    token: String,
    seen: Shared,
}

impl Ctx {
    /// The bus, reached like `aux bus-stdio` does.
    async fn bus(&self) -> Arc<DaemonEndpoint> {
        DaemonEndpoint::connect(&self.socket, &self.token)
            .await
            .expect("the daemon accepts the session's token")
    }

    fn note(&self, note: impl Into<String>) {
        self.seen.lock().unwrap().notes.push(note.into());
    }
}

type Behave = fn(Ctx, Turn) -> BoxFuture<'static, TurnResult>;

/// Starts an in-process fake agent per session, with `behave` as its script.
struct BusLauncher {
    seen: Shared,
    behave: Behave,
    /// Models the fake agents offer as a session config option.
    models: Vec<String>,
}

impl Launcher for BusLauncher {
    fn launch<'a>(
        &'a self,
        spec: LaunchSpec<'a>,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            let server = spec
                .mcp_servers
                .first()
                .cloned()
                .expect("every session gets the bus MCP server");
            // bus-stdio --socket <socket> --session-token <token>
            let ctx = Ctx {
                harness: spec.harness.to_string(),
                socket: PathBuf::from(&server.args[2]),
                token: server.args[4].clone(),
                seen: Arc::clone(&self.seen),
            };
            self.seen
                .lock()
                .unwrap()
                .servers
                .push((spec.harness.to_string(), server));
            let behave = self.behave;
            let turns = ctx.clone();
            let script = script(move |turn| {
                turns
                    .seen
                    .lock()
                    .unwrap()
                    .prompts
                    .push((turns.harness.clone(), turn.prompt.clone()));
                behave(turns.clone(), turn)
            });
            let configured = ctx.clone();
            let setup = Setup {
                models: self.models.clone(),
                on_set_config: Box::new(move |id, value| {
                    configured.seen.lock().unwrap().set_config.push((
                        configured.harness.clone(),
                        id.to_string(),
                        value.to_string(),
                    ));
                }),
                ..Setup::default()
            };
            let (transport, _agent) = agentux_fake_agent::spawn_with(script, setup);
            let options = SessionOptions {
                mcp_servers: spec.mcp_servers.to_vec(),
                model: spec.model.map(str::to_string),
            };
            AcpSession::connect_with(transport, spec.cwd, &options, permissions)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

const AUX: &str = "/opt/agentux/bin/aux";

struct Daemon {
    engine: Engine,
    client: Client,
    socket: PathBuf,
    stop: oneshot::Sender<()>,
}

/// An engine with the bus linked to its socket, served from the fixture's
/// temp dir.
async fn daemon(f: &Fixture, seen: &Shared, behave: Behave, models: &[&str]) -> Daemon {
    let socket = f.tmp.path().join("run").join("agentuxd.sock");
    let launcher = BusLauncher {
        seen: Arc::clone(seen),
        behave,
        models: models.iter().map(|m| m.to_string()).collect(),
    };
    let engine = Engine::with_settings(
        Store::open(&f.database).unwrap(),
        Arc::new(AcpExecutor::new(launcher)),
        Settings {
            bus: Some(BusLink {
                socket: socket.clone(),
                aux: PathBuf::from(AUX),
            }),
            ..Settings::default()
        },
    );
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
    let client = Client::connect(&socket).await.unwrap();
    Daemon {
        engine,
        client,
        socket,
        stop,
    }
}

async fn start(f: &Fixture, engine: &Engine, prompt: &str) -> agentux_api::Run {
    let project = engine
        .register_project(f.repo.to_str().unwrap())
        .await
        .unwrap();
    engine
        .start_run(StartRun {
            project_id: project.id,
            prompt: Some(prompt.into()),
            ..Default::default()
        })
        .unwrap()
}

/// Polls `check` until it returns `Some`, or panics after 20 s.
async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn prompts_of(seen: &Shared, harness: &str) -> Vec<String> {
    seen.lock()
        .unwrap()
        .prompts
        .iter()
        .filter(|(h, _)| h == harness)
        .map(|(_, p)| p.clone())
        .collect()
}

fn is_wake(prompt: &str) -> bool {
    prompt.contains("[agentux bus]")
}

fn bus_log(engine: &Engine, run_id: &str) -> Vec<BusMessage> {
    engine.bus_list(run_id).unwrap()
}

fn kinds(log: &[BusMessage]) -> Vec<BusMessageKind> {
    log.iter().map(|m| m.kind).collect()
}

async fn read(bus: &DaemonEndpoint) -> agentux_bus::Inbox {
    match bus
        .call(BusCall::ReadMessages(ReadMessages::default()))
        .await
        .unwrap()
    {
        BusReply::Inbox(inbox) => inbox,
        other => panic!("unexpected reply {other:?}"),
    }
}

async fn reply(bus: &DaemonEndpoint, to: u64, body: &str) -> Result<BusReply, BusError> {
    bus.call(BusCall::PostMessage(PostMessage {
        to: None,
        body: body.into(),
        in_reply_to: Some(to),
    }))
    .await
}

const REVIEW_LOOP: &str = "version: 1
roles:
  implementer:
    harness: fake-coder
    model: no-such-model
  reviewer:
    harness: fake-reviewer
    model: big
pipeline:
  - step: implement
    role: implementer
    approve: true
  - step: review
    role: reviewer
";

/// The implementer asks for a review over the bus; the reviewer, woken in a
/// session started for it, answers; the implementer is woken with the reply.
/// The pipeline's own review step then reuses the reviewer's session.
fn review_loop(ctx: Ctx, turn: Turn) -> BoxFuture<'static, TurnResult> {
    Box::pin(async move {
        let bus = ctx.bus().await;
        match (ctx.harness.as_str(), is_wake(&turn.prompt)) {
            ("fake-coder", false) => {
                let posted = bus
                    .call(BusCall::RequestReview(RequestReview {
                        summary: "Added the health endpoint; check the error path".into(),
                        reviewer_role: None,
                    }))
                    .await
                    .unwrap();
                ctx.note(format!("review requested: {posted:?}"));
                turn.message("Implemented; review requested.")?;
            }
            ("fake-coder", true) => {
                let inbox = read(&bus).await;
                for message in inbox.messages {
                    ctx.note(format!("implementer read: {}", message.body));
                }
                turn.message("Noted.")?;
            }
            ("fake-reviewer", true) => {
                let inbox = read(&bus).await;
                let request = &inbox.messages[0];
                assert!(request.body.starts_with("Review request: Added the health"));
                assert!(request.body.contains("Branch: aux/"), "{}", request.body);
                reply(&bus, request.id, "Looks good; handle the 503 case too.")
                    .await
                    .unwrap();
                turn.message("Replied.")?;
            }
            ("fake-reviewer", false) => {
                turn.message("```json\n{\"verdict\": \"APPROVE\", \"comments\": \"ok\"}\n```")?;
            }
            (other, _) => panic!("unexpected harness {other}"),
        }
        Ok(acp::StopReason::EndTurn)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_review_request_wakes_the_reviewer_and_its_reply_wakes_the_implementer() {
    let f = fixture(REVIEW_LOOP);
    let seen = Shared::default();
    let mut d = daemon(&f, &seen, review_loop, &["small", "big"]).await;
    let run = start(&f, &d.engine, "Add a health endpoint").await;

    // The implementer is woken with the reviewer's reply while the run waits
    // for the implement step's approval.
    eventually("the implementer reading the reply", || {
        seen.lock()
            .unwrap()
            .notes
            .iter()
            .any(|n| n == "implementer read: Looks good; handle the 503 case too.")
            .then_some(())
    })
    .await;

    // Every session got the bus as its `agentux` MCP server, with its own
    // token, and the session prompt before its first prompt.
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.servers.len(), 2);
        for (_, server) in &seen.servers {
            assert_eq!(server.name, "agentux");
            assert_eq!(server.command, Path::new(AUX));
            assert_eq!(
                server.args[..4],
                [
                    "bus-stdio",
                    "--socket",
                    d.socket.to_str().unwrap(),
                    "--session-token"
                ]
            );
            assert_eq!(server.args[4].len(), 64);
        }
        assert_ne!(seen.servers[0].1.args[4], seen.servers[1].1.args[4]);
    }
    let coder = prompts_of(&seen, "fake-coder");
    assert_eq!(coder.len(), 2, "{coder:#?}");
    let intro = format!(
        "You are the implementer of AgentUX run {} in project repo, running on fake-coder.",
        run.id
    );
    assert!(coder[0].starts_with(&intro), "{}", coder[0]);
    assert!(coder[0].contains("## Your step: implement"), "{}", coder[0]);
    assert!(
        coder[1].starts_with("[agentux bus] New message from reviewer (fake-reviewer"),
        "{}",
        coder[1]
    );
    let reviewer = prompts_of(&seen, "fake-reviewer");
    assert_eq!(reviewer.len(), 1);
    assert!(
        reviewer[0].starts_with("You are the reviewer of AgentUX run"),
        "{}",
        reviewer[0]
    );
    assert!(
        reviewer[0].contains("asks for your review"),
        "{}",
        reviewer[0]
    );

    // The role's model is selected over ACP when the agent offers it; when
    // it does not, the session says so.
    assert_eq!(
        seen.lock().unwrap().set_config,
        [(
            "fake-reviewer".to_string(),
            "model".to_string(),
            "big".to_string()
        )]
    );
    let events = d
        .engine
        .store()
        .read(|tx| tx.events_since(0, Some(&run.id)))
        .unwrap();
    assert!(events.iter().any(|e| matches!(
        &e.body,
        EventBody::SessionEvent { event: SessionEvent::Message { from: MessageFrom::System, text }, .. }
            if text.starts_with("model `no-such-model` not selected")
    )));

    // The bus log, through the API: who said what to whom, and the wakes.
    let log = d.client.list_bus(&run.id).await.unwrap();
    let sessions = d.engine.sessions(Some(&run.id)).unwrap();
    let (implementer, reviewer) = (&sessions[0], &sessions[1]);
    assert_eq!(
        kinds(&log),
        [
            BusMessageKind::Joined,
            BusMessageKind::ReviewRequest,
            BusMessageKind::Wake,
            BusMessageKind::Joined,
            BusMessageKind::Message,
            BusMessageKind::Wake,
        ]
    );
    let request = &log[1];
    assert_eq!(request.tool.as_deref(), Some("request_review"));
    assert_eq!(
        request.from,
        BusEndpoint::Session {
            session_id: implementer.id.clone(),
            role: "implementer".into(),
            vendor: "fake-coder".into()
        }
    );
    assert_eq!(
        request.to,
        BusEndpoint::Role {
            role: "reviewer".into()
        }
    );
    assert_eq!(request.queued_for_role.as_deref(), Some("reviewer"));
    assert_eq!((request.turn, request.max_turns), (1, 6));
    assert_eq!(
        log[2].to,
        BusEndpoint::Role {
            role: "reviewer".into()
        }
    );
    assert_eq!(log[2].from, BusEndpoint::Daemon);
    assert!(
        log[3].subject.contains("1 message(s) waiting"),
        "{}",
        log[3].subject
    );
    let answer = &log[4];
    assert_eq!(answer.in_reply_to, request.message_id);
    assert_eq!(answer.exchange, request.exchange);
    assert_eq!(answer.turn, 2);
    assert_eq!(answer.delivered_to, std::slice::from_ref(&implementer.id));
    assert!(
        matches!(&answer.from, BusEndpoint::Session { session_id, .. } if *session_id == reviewer.id)
    );
    assert!(matches!(&log[5].to, BusEndpoint::Session { role, .. } if role == "implementer"));
    assert!(
        log.iter()
            .all(|m| m.run_id == run.id && m.project_id == run.project_id)
    );

    // The same entries are `bus_message` events in the event stream.
    let mut events = Client::connect(&d.socket)
        .await
        .unwrap()
        .subscribe(&Subscribe {
            run_id: Some(run.id.clone()),
            since: Some(0),
        })
        .await
        .unwrap();
    let mut streamed = Vec::new();
    while streamed.len() < log.len() {
        if let EventBody::BusMessage { message } = events.next().await.unwrap().unwrap().body {
            streamed.push(message);
        }
    }
    assert_eq!(streamed, log);

    // Approving the step: the review step reuses the reviewer's session.
    let step = d.client.list_requests(true).await.unwrap();
    assert_eq!(step.len(), 1);
    d.client.approve(&step[0].id, None).await.unwrap();
    let done = wait_until_settled(&d.engine, &run.id).await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);
    assert_eq!(d.engine.sessions(Some(&run.id)).unwrap().len(), 2);
    assert_eq!(prompts_of(&seen, "fake-reviewer").len(), 2);

    // The run's bus closes with it: the sessions leave and tokens expire.
    let token = seen.lock().unwrap().servers[0].1.args[4].clone();
    eventually("the sessions leaving the bus", || {
        let log = bus_log(&d.engine, &run.id);
        (log.iter()
            .filter(|m| m.kind == BusMessageKind::Left)
            .count()
            == 2)
            .then_some(())
    })
    .await;
    match DaemonEndpoint::connect(&d.socket, &token).await {
        Err(e) => assert!(e.contains("unknown or expired session token"), "{e}"),
        Ok(_) => panic!("the token outlived its run"),
    }
    let _ = d.stop.send(());
}

const QUESTION: &str = "version: 1
roles:
  implementer:
    harness: fake-coder
pipeline:
  - step: implement
    role: implementer
";

/// Asks the human which database to use, and notes the answer.
fn asks(ctx: Ctx, turn: Turn) -> BoxFuture<'static, TurnResult> {
    Box::pin(async move {
        let bus = ctx.bus().await;
        let reply = bus
            .call(BusCall::AskHuman(AskHuman {
                question: "Postgres or SQLite for the cache?".into(),
                options: vec!["postgres".into(), "sqlite".into()],
                context: Some("SQLite needs no server.".into()),
            }))
            .await
            .unwrap();
        let BusReply::Human(reply) = reply else {
            panic!("unexpected reply {reply:?}");
        };
        assert_eq!(reply.status, HumanReplyStatus::Answered);
        ctx.note(format!("answer: {}", reply.answer.unwrap()));
        turn.message("Using the answer.")?;
        Ok(acp::StopReason::EndTurn)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_human_is_a_question_answered_through_the_api() {
    let f = fixture(QUESTION);
    let seen = Shared::default();
    let mut d = daemon(&f, &seen, asks, &[]).await;
    let run = start(&f, &d.engine, "Add a cache").await;

    let question = eventually("the question", || {
        d.engine.requests(true).unwrap().into_iter().next()
    })
    .await;
    assert_eq!(question.kind, RequestKind::Question);
    assert_eq!(question.run_id, run.id);
    assert_eq!(question.options, ["postgres", "sqlite"]);
    assert_eq!(
        question.title,
        "implementer (fake-coder) asks: Postgres or SQLite for the cache?"
    );
    assert!(question.detail.contains("Context: SQLite needs no server."));
    let session = &d.engine.sessions(Some(&run.id)).unwrap()[0];
    assert_eq!(question.session_id.as_deref(), Some(session.id.as_str()));
    // A question does not pause the run: the agent's tool call waits.
    assert_eq!(d.engine.run(&run.id).unwrap().0.status, RunStatus::Running);

    // An answer is required.
    match d.client.approve(&question.id, None).await {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, rpc::code::INVALID_PARAMS),
        other => panic!("expected invalid params, got {other:?}"),
    }
    let answered = d
        .client
        .approve(&question.id, Some("sqlite".into()))
        .await
        .unwrap();
    assert_eq!(answered.status, RequestStatus::Approved);
    assert_eq!(answered.answer.as_deref(), Some("sqlite"));

    let done = wait_until_settled(&d.engine, &run.id).await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);
    assert!(
        seen.lock()
            .unwrap()
            .notes
            .contains(&"answer: sqlite".to_string())
    );

    let log = bus_log(&d.engine, &run.id);
    let asked = log
        .iter()
        .find(|m| m.kind == BusMessageKind::Question)
        .unwrap();
    assert_eq!(asked.request_id.as_deref(), Some(question.id.as_str()));
    assert_eq!(asked.tool.as_deref(), Some("ask_human"));
    assert_eq!(asked.to, BusEndpoint::Human);
    assert_eq!(asked.subject, "Postgres or SQLite for the cache?");
    let answer = eventually("the answer in the bus log", || {
        bus_log(&d.engine, &run.id)
            .into_iter()
            .find(|m| m.kind == BusMessageKind::Answer)
    })
    .await;
    assert_eq!(answer.body, "sqlite");
    assert_eq!(answer.question_id, asked.question_id);
    assert_eq!(answer.request_id, asked.request_id);
    assert_eq!(answer.from, BusEndpoint::Human);
    assert!(
        matches!(&answer.to, BusEndpoint::Session { session_id, .. } if *session_id == session.id)
    );
    let _ = d.stop.send(());
}

const PING_PONG: &str = "version: 1
roles:
  implementer:
    harness: fake-coder
  reviewer:
    harness: fake-reviewer
pipeline:
  - step: implement
    role: implementer
    approve: true
  - step: review
    role: reviewer
bus:
  max_turns_per_exchange: 3
";

/// Two agents that always answer: the implementer opens an exchange, and
/// each wake makes the woken agent reply to what it read.
fn ping_pong(ctx: Ctx, turn: Turn) -> BoxFuture<'static, TurnResult> {
    Box::pin(async move {
        let bus = ctx.bus().await;
        if !is_wake(&turn.prompt) {
            bus.call(BusCall::PostMessage(PostMessage {
                to: Some(Target::Role("reviewer".into())),
                body: "ping".into(),
                in_reply_to: None,
            }))
            .await
            .unwrap();
        } else {
            for message in read(&bus).await.messages {
                match reply(&bus, message.id, "pong").await {
                    Ok(_) => ctx.note(format!("{} replied", ctx.harness)),
                    Err(e @ BusError::TurnLimit { .. }) => {
                        ctx.note(format!("{} stopped: {e}", ctx.harness));
                    }
                    Err(e) => panic!("unexpected refusal {e}"),
                }
            }
        }
        turn.message("ok")?;
        Ok(acp::StopReason::EndTurn)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_turn_limit_stops_a_ping_pong() {
    let f = fixture(PING_PONG);
    let seen = Shared::default();
    let d = daemon(&f, &seen, ping_pong, &[]).await;
    let run = start(&f, &d.engine, "Chat").await;

    // ping (turn 1) wakes the reviewer, pong (2) the implementer, pong (3)
    // the reviewer, whose fourth message is refused.
    eventually("the refusal", || {
        let notes = seen.lock().unwrap().notes.clone();
        notes
            .iter()
            .any(|n| n.starts_with("fake-reviewer stopped"))
            .then_some(notes)
    })
    .await;
    // Nothing moves after the refusal.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let notes = seen.lock().unwrap().notes.clone();
    assert_eq!(
        notes,
        [
            "fake-reviewer replied".to_string(),
            "fake-coder replied".to_string(),
            format!(
                "fake-reviewer stopped: {}",
                BusError::TurnLimit {
                    exchange: 1,
                    max_turns: 3
                }
            ),
        ]
    );
    assert_eq!(prompts_of(&seen, "fake-coder").len(), 2);
    assert_eq!(prompts_of(&seen, "fake-reviewer").len(), 2);

    let log = bus_log(&d.engine, &run.id);
    let turns: Vec<u32> = log
        .iter()
        .filter(|m| m.kind == BusMessageKind::Message)
        .map(|m| m.turn)
        .collect();
    assert_eq!(turns, [1, 2, 3]);
    assert_eq!(
        log.iter()
            .filter(|m| m.kind == BusMessageKind::Wake)
            .count(),
        3
    );
    let limit = log
        .iter()
        .find(|m| m.kind == BusMessageKind::TurnLimit)
        .unwrap();
    assert_eq!(limit.exchange, Some(1));
    assert!(matches!(&limit.from, BusEndpoint::Session { role, .. } if role == "reviewer"));
    d.engine.cancel(&run.id).unwrap();
    let _ = d.stop.send(());
}

const TWO_RUNS: &str = "version: 1
roles:
  implementer:
    harness: fake-coder
pipeline:
  - step: implement
    role: implementer
    approve: true
";

/// Says nothing; the test drives the bus with the sessions' tokens.
fn quiet(_: Ctx, turn: Turn) -> BoxFuture<'static, TurnResult> {
    Box::pin(async move {
        turn.message("done")?;
        Ok(acp::StopReason::EndTurn)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_token_only_acts_as_its_own_session() {
    let f = fixture(TWO_RUNS);
    let seen = Shared::default();
    let mut d = daemon(&f, &seen, quiet, &[]).await;
    let first = start(&f, &d.engine, "First").await;
    wait_for(&d.engine, &first.id, |r| r.status == RunStatus::Waiting).await;
    let second = start(&f, &d.engine, "Second").await;
    wait_for(&d.engine, &second.id, |r| r.status == RunStatus::Waiting).await;
    let tokens: Vec<String> = seen
        .lock()
        .unwrap()
        .servers
        .iter()
        .map(|(_, s)| s.args[4].clone())
        .collect();
    let first_session = d.engine.sessions(Some(&first.id)).unwrap()[0].id.clone();
    let second_session = d.engine.sessions(Some(&second.id)).unwrap()[0].id.clone();

    // A made-up token gets nothing.
    let forged = "0".repeat(64);
    let rejected = d
        .client
        .call::<_, serde_json::Value>(
            rpc::method::BUS_HELLO,
            &serde_json::json!({"sessionToken": forged}),
        )
        .await;
    match rejected {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, rpc::code::UNAUTHORIZED),
        other => panic!("expected unauthorized, got {other:?}"),
    }
    let call = serde_json::json!({
        "sessionToken": forged,
        "call": {"tool": "read_messages", "arguments": {}},
    });
    match d
        .client
        .call::<_, serde_json::Value>(rpc::method::BUS_CALL, &call)
        .await
    {
        Err(ClientError::Rpc(e)) => assert_eq!(e.code, rpc::code::UNAUTHORIZED),
        other => panic!("expected unauthorized, got {other:?}"),
    }

    // A token is its session: identity, run and reach come from the token.
    let bus = DaemonEndpoint::connect(&d.socket, &tokens[0])
        .await
        .unwrap();
    assert_eq!(bus.identity().session.0, first_session);
    assert_eq!(bus.identity().run.0, first.id);
    let BusReply::RunState(state) = bus.call(BusCall::GetRunState).await.unwrap() else {
        panic!("expected the run state");
    };
    assert_eq!(state.run.0, first.id);
    assert_eq!(state.state.status.as_deref(), Some("waiting for the human"));
    assert_eq!(state.sessions.len(), 1);
    // The other run's session is out of reach.
    let refused = bus
        .call(BusCall::PostMessage(PostMessage {
            to: Some(Target::Session(second_session.as_str().into())),
            body: "hi from the other run".into(),
            in_reply_to: None,
        }))
        .await;
    assert!(
        matches!(refused, Err(BusError::UnknownSession { .. })),
        "{refused:?}"
    );
    let other = DaemonEndpoint::connect(&d.socket, &tokens[1])
        .await
        .unwrap();
    assert_eq!(other.identity().session.0, second_session);

    // Once its session ends (here: the run is cancelled), a token is dead.
    d.engine.cancel(&first.id).unwrap();
    eventually("the first token expiring", || {
        futures_lite_block(DaemonEndpoint::connect(&d.socket, &tokens[0])).err()
    })
    .await;
    assert!(DaemonEndpoint::connect(&d.socket, &tokens[1]).await.is_ok());
    d.engine.cancel(&second.id).unwrap();
    let _ = d.stop.send(());
}

/// Runs a future to completion from synchronous test code.
fn futures_lite_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

const RESTART: &str = "version: 1
roles:
  implementer:
    harness: fake-coder
  reviewer:
    harness: fake-reviewer
pipeline:
  - step: implement
    role: implementer
    approve: true
  - step: review
    role: reviewer
";

/// Both roles tell the human what they did.
fn reports(ctx: Ctx, turn: Turn) -> BoxFuture<'static, TurnResult> {
    Box::pin(async move {
        let bus = ctx.bus().await;
        bus.call(BusCall::PostMessage(PostMessage {
            to: Some(Target::Human),
            body: format!("{} finished its step", ctx.harness),
            in_reply_to: None,
        }))
        .await
        .unwrap();
        if ctx.harness == "fake-reviewer" {
            turn.message("Verdict: APPROVE")?;
        } else {
            turn.message("done")?;
        }
        Ok(acp::StopReason::EndTurn)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn bus_events_survive_a_daemon_restart() {
    let f = fixture(RESTART);
    let seen = Shared::default();
    let d = daemon(&f, &seen, reports, &[]).await;
    let run = start(&f, &d.engine, "Report").await;
    wait_for(&d.engine, &run.id, |r| r.status == RunStatus::Waiting).await;
    let before = eventually("the implementer's report", || {
        let log = bus_log(&d.engine, &run.id);
        log.iter()
            .any(|m| m.kind == BusMessageKind::Message)
            .then_some(log)
    })
    .await;

    // Stop the daemon and start a new one on the same database.
    d.engine.shutdown();
    let _ = d.stop.send(());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut d = daemon(&f, &seen, reports, &[]).await;
    d.engine.resume().unwrap();

    // The log is replayed as it was, through bus.list and the event stream.
    assert_eq!(d.client.list_bus(&run.id).await.unwrap(), before);
    let mut events = Client::connect(&d.socket)
        .await
        .unwrap()
        .subscribe(&Subscribe {
            run_id: Some(run.id.clone()),
            since: Some(0),
        })
        .await
        .unwrap();
    let mut replayed = Vec::new();
    while replayed.len() < before.len() {
        if let EventBody::BusMessage { message } = events.next().await.unwrap().unwrap().body {
            replayed.push(message);
        }
    }
    assert_eq!(replayed, before);

    // The run goes on; message ids continue after the old ones.
    let step = d.client.list_requests(true).await.unwrap();
    d.client.approve(&step[0].id, None).await.unwrap();
    let done = wait_until_settled(&d.engine, &run.id).await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);
    let after = eventually("the reviewer's report", || {
        let log = bus_log(&d.engine, &run.id);
        (log.iter()
            .filter(|m| m.kind == BusMessageKind::Message)
            .count()
            == 2)
            .then_some(log)
    })
    .await;
    assert_eq!(after[..before.len()], before[..]);
    let ids: Vec<u64> = after
        .iter()
        .filter(|m| m.kind == BusMessageKind::Message)
        .filter_map(|m| m.message_id)
        .collect();
    assert_eq!(ids, [1, 2]);
    let _ = d.stop.send(());
}
