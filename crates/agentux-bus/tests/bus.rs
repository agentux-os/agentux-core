//! The bus core against the in-memory backend: routing, limits, allowed
//! tools, `ask_human` and the audit log.

use std::sync::Arc;
use std::time::Duration;

use agentux_bus::{
    AskHuman, Bus, BusConfig, BusError, BusEvent, BusTool, CheckResult, EventKind, GoneSession,
    Handoff, HumanReplyStatus, MemoryBackend, Message, MessageKind, Participant, PostMessage,
    ReadMessages, RequestReview, Restore, RestoredSent, RunId, RunState, SessionId,
    SessionIdentity, Target, Wake, WakeTarget,
};
use agentux_config::Config;
use tokio::sync::mpsc::UnboundedReceiver;

const RUN: &str = "run-1";
const TIMEOUT: Duration = Duration::from_secs(5);

const YAML: &str = "version: 1
roles:
  implementer:
    harness: claude-code
  reviewer:
    harness: codex
  planner:
    harness: claude-code
pipeline:
  - step: implement
    role: implementer
  - step: review
    role: reviewer
  - step: pull_request
bus:
  max_turns_per_exchange: 3
  allow: [post_message, request_review, handoff, get_run_state, ask_human]
";

fn config() -> BusConfig {
    BusConfig::from_config(&Config::from_yaml(YAML).unwrap())
}

fn identity(session: &str, role: &str, vendor: &str) -> SessionIdentity {
    SessionIdentity {
        run: RUN.into(),
        project: "shop".into(),
        role: role.into(),
        vendor: vendor.into(),
        session: session.into(),
    }
}

struct Fixture {
    bus: Bus,
    backend: Arc<MemoryBackend>,
    events: UnboundedReceiver<BusEvent>,
    run: RunId,
}

/// A run with an implementer (`impl`) and a reviewer (`rev`) on the bus.
fn fixture(config: BusConfig) -> Fixture {
    let backend = MemoryBackend::new();
    let bus = Bus::new(backend.clone());
    let events = bus.subscribe();
    let run = RunId::from(RUN);
    bus.open_run(run.clone(), "shop", config).unwrap();
    bus.join(identity("impl", "implementer", "claude-code"))
        .unwrap();
    bus.join(identity("rev", "reviewer", "codex")).unwrap();
    Fixture {
        bus,
        backend,
        events,
        run,
    }
}

fn drain(events: &mut UnboundedReceiver<BusEvent>) -> Vec<EventKind> {
    std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.kind)
        .collect()
}

fn wakes(events: &[EventKind]) -> Vec<Wake> {
    events
        .iter()
        .filter_map(|event| match event {
            EventKind::Wake(wake) => Some(wake.clone()),
            _ => None,
        })
        .collect()
}

fn post(to: &str, body: &str) -> PostMessage {
    PostMessage {
        to: Some(to.parse().unwrap()),
        body: body.into(),
        in_reply_to: None,
    }
}

fn reply(id: u64, body: &str) -> PostMessage {
    PostMessage {
        to: None,
        body: body.into(),
        in_reply_to: Some(id),
    }
}

fn s(id: &str) -> SessionId {
    id.into()
}

#[test]
fn messages_route_to_sessions_roles_run_and_human() {
    let mut f = fixture(config());
    f.bus
        .join(identity("rev2", "reviewer", "opencode"))
        .unwrap();
    drain(&mut f.events);

    // To a role: every session playing it, each woken.
    let posted = f
        .bus
        .post_message(&s("impl"), post("role:reviewer", "look at src/cart.rs"))
        .unwrap();
    assert_eq!(posted.delivered_to, [s("rev"), s("rev2")]);
    assert_eq!((posted.turn, posted.turns_left), (1, 2));
    let events = drain(&mut f.events);
    let woken: Vec<WakeTarget> = wakes(&events).into_iter().map(|w| w.target).collect();
    assert_eq!(
        woken,
        [
            WakeTarget::Session(s("rev")),
            WakeTarget::Session(s("rev2"))
        ]
    );

    // To one session.
    let posted = f
        .bus
        .post_message(&s("rev"), post("session:impl", "which branch?"))
        .unwrap();
    assert_eq!(posted.delivered_to, [s("impl")]);
    let inbox = f
        .bus
        .read_messages(&s("impl"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.remaining, 0);
    let [message] = inbox.messages.as_slice() else {
        panic!("{inbox:?}")
    };
    assert_eq!(message.body, "which branch?");
    assert_eq!(message.to, Target::Session(s("impl")));
    assert!(matches!(&message.from, Participant::Session { role, .. } if role == "reviewer"));
    // Reading empties the mailbox.
    assert!(
        f.bus
            .read_messages(&s("impl"), ReadMessages::default())
            .unwrap()
            .messages
            .is_empty()
    );

    // To the run: everyone else, nobody woken.
    drain(&mut f.events);
    let posted = f
        .bus
        .post_message(&s("impl"), post("run", "rebased on main"))
        .unwrap();
    assert_eq!(posted.delivered_to, [s("rev"), s("rev2")]);
    assert!(wakes(&drain(&mut f.events)).is_empty());

    // To the human: no mailbox, the event is the delivery.
    let posted = f
        .bus
        .post_message(&s("impl"), post("human", "PR is ready"))
        .unwrap();
    assert!(posted.delivered_to.is_empty());
    let events = drain(&mut f.events);
    assert!(matches!(
        events.as_slice(),
        [EventKind::MessagePosted { message, .. }] if message.to == Target::Human
    ));

    // The human answers; the reply defaults to the asker's session.
    let human = f
        .bus
        .post_from_human(&f.run, reply(posted.message_id, "merge it"))
        .unwrap();
    assert_eq!(human.delivered_to, [s("impl")]);
    let inbox = f
        .bus
        .read_messages(&s("impl"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.messages[0].from, Participant::Human);
}

#[test]
fn messages_for_a_role_without_session_wait_for_it() {
    let mut f = fixture(config());
    drain(&mut f.events);
    let posted = f
        .bus
        .post_message(&s("impl"), post("role:planner", "is step 3 still needed?"))
        .unwrap();
    assert_eq!(posted.queued_for_role.as_deref(), Some("planner"));
    let wake = wakes(&drain(&mut f.events)).remove(0);
    assert_eq!(wake.target, WakeTarget::Role("planner".into()));
    assert!(wake.prompt.contains("read_messages"), "{}", wake.prompt);

    // The session the daemon starts for the role finds it in its mailbox.
    let queued = f
        .bus
        .join(identity("plan", "planner", "claude-code"))
        .unwrap();
    assert_eq!(queued, 1);
    let inbox = f
        .bus
        .read_messages(&s("plan"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.messages[0].body, "is step 3 still needed?");
}

#[test]
fn bad_targets_are_refused_with_guidance() {
    let f = fixture(config());
    let err = |to: &str| f.bus.post_message(&s("impl"), post(to, "hi")).unwrap_err();
    assert!(
        matches!(err("role:qa"), BusError::UnknownRole { known, .. } if known == ["implementer", "planner", "reviewer"])
    );
    assert!(matches!(
        err("session:ghost"),
        BusError::UnknownSession { .. }
    ));
    assert_eq!(err("session:impl"), BusError::MessageToSelf);
    assert!(matches!(
        err("role:implementer"),
        BusError::OnlySessionInRole { .. }
    ));
    let missing_to = f
        .bus
        .post_message(
            &s("impl"),
            PostMessage {
                to: None,
                body: "hi".into(),
                in_reply_to: None,
            },
        )
        .unwrap_err();
    assert!(missing_to.to_string().contains("in_reply_to"));
    assert_eq!(
        f.bus
            .post_message(&s("impl"), reply(999, "hi"))
            .unwrap_err(),
        BusError::UnknownMessage { id: 999 }
    );

    // Replying to a session that left points at its role.
    let first = f
        .bus
        .post_message(&s("rev"), post("session:impl", "ping"))
        .unwrap();
    f.bus.leave(&s("rev"));
    let left = f
        .bus
        .post_message(&s("impl"), reply(first.message_id, "pong"))
        .unwrap_err();
    assert!(left.to_string().contains("role:reviewer"), "{left}");
}

#[test]
fn exchanges_stop_at_max_turns() {
    let mut f = fixture(config());
    let first = f
        .bus
        .post_message(&s("impl"), post("session:rev", "1"))
        .unwrap();
    let second = f
        .bus
        .post_message(&s("rev"), reply(first.message_id, "2"))
        .unwrap();
    let third = f
        .bus
        .post_message(&s("impl"), reply(second.message_id, "3"))
        .unwrap();
    assert_eq!(third.exchange, first.exchange);
    assert_eq!((third.turn, third.turns_left), (3, 0));
    drain(&mut f.events);

    let error = f
        .bus
        .post_message(&s("rev"), reply(third.message_id, "4"))
        .unwrap_err();
    assert_eq!(
        error,
        BusError::TurnLimit {
            exchange: first.exchange,
            max_turns: 3
        }
    );
    let text = error.to_string();
    assert!(text.contains("limit of 3 turns"), "{text}");
    assert!(text.contains("ask_human"), "{text}");
    assert_eq!(
        drain(&mut f.events),
        [EventKind::TurnLimitReached {
            session: s("rev"),
            exchange: first.exchange,
            max_turns: 3
        }]
    );

    // A new exchange starts fresh, and the human is never cut off.
    let fresh = f
        .bus
        .post_message(&s("rev"), post("session:impl", "new topic"))
        .unwrap();
    assert_ne!(fresh.exchange, first.exchange);
    assert_eq!(fresh.turn, 1);
    assert!(
        f.bus
            .post_from_human(
                &f.run,
                PostMessage {
                    to: Some(Target::Session(s("rev"))),
                    body: "stop here".into(),
                    in_reply_to: Some(third.message_id),
                }
            )
            .is_ok()
    );
}

#[tokio::test]
async fn only_allowed_tools_can_be_called() {
    let mut config = config();
    config.allow = vec![BusTool::GetRunState];
    let mut f = fixture(config);
    drain(&mut f.events);

    assert_eq!(
        f.bus.allowed_tools(&s("impl")).unwrap(),
        [BusTool::GetRunState]
    );
    let denied = f
        .bus
        .post_message(&s("impl"), post("run", "hi"))
        .unwrap_err();
    assert_eq!(
        denied,
        BusError::ToolNotAllowed {
            tool: "post_message".into()
        }
    );
    assert!(denied.to_string().contains("bus.allow"));
    assert!(
        f.bus
            .read_messages(&s("impl"), ReadMessages::default())
            .is_err()
    );
    assert!(
        f.bus
            .handoff(
                &s("impl"),
                Handoff {
                    to_role: "reviewer".into(),
                    summary: "x".into(),
                    pointers: vec![],
                }
            )
            .is_err()
    );
    assert!(f.bus.get_run_state(&s("impl")).await.is_ok());
    assert_eq!(
        drain(&mut f.events),
        [
            EventKind::ToolDenied {
                session: s("impl"),
                tool: "post_message".into()
            },
            EventKind::ToolDenied {
                session: s("impl"),
                tool: "read_messages".into()
            },
            EventKind::ToolDenied {
                session: s("impl"),
                tool: "handoff".into()
            },
        ]
    );
}

#[test]
fn read_messages_comes_with_post_message() {
    // The default agentux.yaml allows post_message but does not list
    // read_messages; receiving is part of messaging.
    let config = config();
    assert!(!config.allow.contains(&BusTool::ReadMessages));
    assert!(config.allows(BusTool::ReadMessages));
    assert_eq!(config.allowed_tools().len(), 6);
}

#[tokio::test]
async fn review_requests_and_handoffs_go_to_roles() {
    let mut f = fixture(config());
    f.backend.set_run_state(
        &f.run,
        RunState {
            branch: Some("aux/run-1".into()),
            ..RunState::default()
        },
    );
    drain(&mut f.events);

    let posted = f
        .bus
        .request_review(
            &s("impl"),
            RequestReview {
                summary: "cart totals now include tax".into(),
                reviewer_role: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(posted.delivered_to, [s("rev")]);
    let wake = wakes(&drain(&mut f.events)).remove(0);
    assert_eq!(wake.reason, MessageKind::ReviewRequest);
    assert!(
        wake.prompt
            .contains(&format!("in_reply_to={}", posted.message_id))
    );
    let inbox = f
        .bus
        .read_messages(&s("rev"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.messages[0].kind, MessageKind::ReviewRequest);
    assert_eq!(
        inbox.messages[0].body,
        "Review request: cart totals now include tax\nBranch: aux/run-1"
    );

    let posted = f
        .bus
        .handoff(
            &s("rev"),
            Handoff {
                to_role: "implementer".into(),
                summary: "two issues left".into(),
                pointers: vec!["src/cart.rs:42".into(), "test cart::tax".into()],
            },
        )
        .unwrap();
    assert_eq!(posted.delivered_to, [s("impl")]);
    let inbox = f
        .bus
        .read_messages(&s("impl"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.messages[0].kind, MessageKind::Handoff);
    assert_eq!(
        inbox.messages[0].body,
        "Handoff: two issues left\nPointers:\n- src/cart.rs:42\n- test cart::tax"
    );

    // Without a review step there is no default reviewer.
    let mut config = config();
    config.review_role = None;
    let f = fixture(config);
    assert_eq!(
        f.bus
            .request_review(
                &s("impl"),
                RequestReview {
                    summary: "x".into(),
                    reviewer_role: None,
                }
            )
            .await
            .unwrap_err(),
        BusError::NoReviewer
    );
}

#[tokio::test]
async fn run_state_combines_backend_and_bus() {
    let f = fixture(config());
    f.backend.set_run_state(
        &f.run,
        RunState {
            step: Some("implement".into()),
            checks: vec![CheckResult {
                name: "test".into(),
                passed: Some(false),
                summary: Some("cart::tax failed".into()),
            }],
            ..RunState::default()
        },
    );
    f.bus
        .post_message(&s("rev"), post("session:impl", "hi"))
        .unwrap();
    let state = f.bus.get_run_state(&s("impl")).await.unwrap();
    assert_eq!(state.you.role, "implementer");
    assert_eq!(state.state.step.as_deref(), Some("implement"));
    assert_eq!(state.state.checks[0].passed, Some(false));
    assert_eq!(state.sessions.len(), 2);
    assert_eq!(state.unread_messages, 1);
    assert!(state.pending_questions.is_empty());
}

#[tokio::test]
async fn ask_human_round_trip_through_the_backend() {
    let mut f = fixture(config());
    drain(&mut f.events);

    let bus = f.bus.clone();
    let asking = tokio::spawn(async move {
        bus.ask_human(
            &s("impl"),
            AskHuman {
                question: "Keep the v1 API?".into(),
                options: vec!["yes".into(), "no".into()],
                context: None,
            },
        )
        .await
    });
    let question = tokio::time::timeout(TIMEOUT, f.backend.next_question())
        .await
        .unwrap();
    assert_eq!(question.question, "Keep the v1 API?");
    assert_eq!(question.options, ["yes", "no"]);
    assert!(
        matches!(&question.from, Participant::Session { session, .. } if session == &s("impl"))
    );
    // While pending, the run state shows it.
    let state = f.bus.get_run_state(&s("rev")).await.unwrap();
    assert_eq!(state.pending_questions, std::slice::from_ref(&question));

    assert!(f.backend.answer(question.id, "yes, until 2.0"));
    let reply = tokio::time::timeout(TIMEOUT, asking)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.status, HumanReplyStatus::Answered);
    assert_eq!(reply.answer.as_deref(), Some("yes, until 2.0"));
    assert_eq!(
        drain(&mut f.events),
        [
            EventKind::HumanAsked {
                question: question.clone()
            },
            EventKind::HumanAnswered {
                question: question.id,
                answer: "yes, until 2.0".into()
            },
        ]
    );
    assert!(
        f.bus
            .get_run_state(&s("rev"))
            .await
            .unwrap()
            .pending_questions
            .is_empty()
    );
}

#[tokio::test]
async fn late_human_answers_arrive_by_mail() {
    let mut config = config();
    config.ask_human_wait = Duration::ZERO;
    let mut f = fixture(config);
    drain(&mut f.events);

    let reply = f
        .bus
        .ask_human(
            &s("impl"),
            AskHuman {
                question: "Which DB?".into(),
                options: vec![],
                context: Some("Postgres is already deployed".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(reply.status, HumanReplyStatus::Pending);
    assert!(f.backend.answer(reply.question_id, "Postgres"));

    // The answer becomes a message from the human, with a wake.
    let wake = loop {
        let event = tokio::time::timeout(TIMEOUT, f.events.recv())
            .await
            .unwrap()
            .unwrap();
        if let EventKind::Wake(wake) = event.kind {
            break wake;
        }
    };
    assert_eq!(wake.target, WakeTarget::Session(s("impl")));
    assert_eq!(wake.reason, MessageKind::HumanAnswer);
    let inbox = f
        .bus
        .read_messages(&s("impl"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.messages[0].from, Participant::Human);
    assert_eq!(
        inbox.messages[0].body,
        format!(
            "Answer to your question {} (\"Which DB?\"): Postgres",
            reply.question_id
        )
    );
}

#[test]
fn every_exchange_is_in_the_audit_log() {
    let mut f = fixture(config());
    let first = f
        .bus
        .post_message(&s("impl"), post("role:reviewer", "ready?"))
        .unwrap();
    f.bus
        .post_message(&s("rev"), reply(first.message_id, "yes"))
        .unwrap();
    f.bus.leave(&s("rev"));

    let events: Vec<BusEvent> = std::iter::from_fn(|| f.events.try_recv().ok()).collect();
    // Sequence numbers are gapless and every event names the run.
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.seq, i as u64 + 1);
        assert_eq!(event.run, f.run);
    }
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match &event.kind {
            EventKind::SessionJoined { .. } => "joined",
            EventKind::SessionLeft { .. } => "left",
            EventKind::MessagePosted { .. } => "posted",
            EventKind::Wake(_) => "wake",
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "joined", "joined", "posted", "wake", "posted", "wake", "left"
        ]
    );
    // Events serialize as flat, tagged JSON for the daemon's log.
    let json = serde_json::to_value(&events[2]).unwrap();
    assert_eq!(json["event"], "message_posted");
    assert_eq!(json["run"], RUN);
    assert_eq!(json["message"]["to"], "role:reviewer");
    assert_eq!(json["message"]["body"], "ready?");
}

/// A bus reopened after a restart continues exchanges where the log left
/// them, routes replies to old messages, and holds queued mail for roles.
#[test]
fn a_restored_bus_continues_exchanges_and_delivers_queued_mail() {
    let backend = MemoryBackend::new();
    let bus = Bus::new(backend);
    let mut events = bus.subscribe();
    let run = RunId::from(RUN);
    bus.open_run(run.clone(), "shop", config()).unwrap();
    let old_impl = Participant::Session {
        session: s("old-impl"),
        role: "implementer".into(),
        vendor: "claude-code".into(),
    };
    let queued = Message {
        id: 2,
        exchange: 1,
        turn: 2,
        run: run.clone(),
        from: Participant::Human,
        to: Target::Session(s("old-impl")),
        kind: MessageKind::Message,
        body: "use the v2 API".into(),
        in_reply_to: Some(1),
        sent_at_ms: 1,
    };
    bus.restore(
        &run,
        Restore {
            exchanges: vec![(1, 2)],
            sent: vec![
                RestoredSent {
                    message: 1,
                    exchange: 1,
                    from: old_impl,
                },
                RestoredSent {
                    message: 2,
                    exchange: 1,
                    from: Participant::Human,
                },
            ],
            queued: vec![("implementer".into(), queued.clone())],
            gone: vec![GoneSession {
                session: s("old-impl"),
                role: "implementer".into(),
                requeued: 1,
            }],
        },
    )
    .unwrap();
    let restored = drain(&mut events);
    assert!(matches!(
        &restored[0],
        EventKind::SessionLeft { session, unread: 1, requeued_for: Some(role) }
            if session == &s("old-impl") && role == "implementer"
    ));
    let wake = &wakes(&restored)[0];
    assert_eq!(wake.target, WakeTarget::Role("implementer".into()));
    assert_eq!(wake.message, 2);
    assert!(wake.prompt.contains("1 message(s)"), "{}", wake.prompt);

    // The role's new session finds the mail in its mailbox.
    assert_eq!(
        bus.join(identity("impl", "implementer", "claude-code"))
            .unwrap(),
        1
    );
    let inbox = bus
        .read_messages(&s("impl"), ReadMessages::default())
        .unwrap();
    assert_eq!(inbox.messages, [queued]);

    // A reply to the old message continues its exchange (turn 3 of 3) and
    // goes to the human who sent it; the next post is over the limit.
    let posted = bus.post_message(&s("impl"), reply(2, "done")).unwrap();
    assert_eq!((posted.exchange, posted.turn, posted.message_id), (1, 3, 3));
    assert!(matches!(
        bus.post_message(&s("impl"), reply(2, "more")),
        Err(BusError::TurnLimit { exchange: 1, .. })
    ));
}
