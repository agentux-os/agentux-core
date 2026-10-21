use super::*;

fn sample_run(project_id: &str) -> RunRecord {
    let now = now_ms();
    RunRecord {
        run: Run {
            id: new_id(),
            project_id: project_id.into(),
            title: "Fix login".into(),
            prompt: Some("Fix the login bug".into()),
            issue: Some(42),
            branch: None,
            worktree: None,
            steps: vec![StepKind::Implement, StepKind::Gate],
            step_index: 0,
            step: StepKind::Implement,
            status: RunStatus::Running,
            roles: [("implementer".to_string(), "claude-code".to_string())].into(),
            checks: vec![],
            gate_attempt: 0,
            gate_max_attempts: 3,
            review_round: 0,
            review_max_rounds: 0,
            budget_usd: Some(10.0),
            started_at: now,
            updated_at: now,
            finished_at: None,
            pull_request: None,
            activity: "starting".into(),
            error: None,
            cost_usd: 0.0,
            sessions: BTreeMap::new(),
        },
        phase: Phase::Setup,
        loops: BTreeMap::new(),
        feedback: None,
        config_yaml: None,
        base_commit: None,
    }
}

#[test]
fn migrations_apply_once_and_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let store = Store::open(&path).unwrap();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    let version: i64 = store
        .lock()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, schema::VERSION as i64);
    assert_eq!(store.read(|tx| tx.projects()).unwrap(), [project]);
}

#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 999)
        .unwrap();
    assert!(matches!(Store::open(&path), Err(Error::Data(_))));
}

#[test]
fn registering_a_path_twice_returns_the_same_project() {
    let store = Store::open_in_memory().unwrap();
    let a = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let b = store
        .write(|tx| tx.register_project("other", "/src/app"))
        .unwrap();
    assert_eq!(a, b);
}

#[test]
fn runs_round_trip_with_their_private_state() {
    let store = Store::open_in_memory().unwrap();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let mut record = sample_run(&project.id);
    record.loops.insert(1, 2);
    record.feedback = Some("lint failed".into());
    record.run.pull_request = Some(PullRequest {
        number: 7,
        url: "https://example.test/pr/7".into(),
    });
    store.write(|tx| tx.insert_run(&record)).unwrap();

    let loaded = store.read(|tx| tx.run(&record.run.id)).unwrap().unwrap();
    assert_eq!(loaded, record);

    record.phase = Phase::Approval;
    record.run.step_index = 1;
    store.write(|tx| tx.save_run(&mut record)).unwrap();
    let loaded = store.read(|tx| tx.run(&record.run.id)).unwrap().unwrap();
    assert_eq!(loaded.phase, Phase::Approval);
    assert_eq!(loaded.run.step, StepKind::Gate);
    assert_eq!(
        store.read(|tx| tx.active_run_ids()).unwrap(),
        [record.run.id.clone()]
    );
}

#[test]
fn a_failed_transaction_leaves_no_trace() {
    let store = Store::open_in_memory().unwrap();
    let mut events = store.subscribe();
    let result: Result<()> = store.write(|tx| {
        tx.register_project("app", "/src/app")?;
        Err(Error::Data("boom".into()))
    });
    assert!(result.is_err());
    assert!(store.read(|tx| tx.projects()).unwrap().is_empty());
    assert!(
        store
            .read(|tx| tx.events_since(0, None))
            .unwrap()
            .is_empty()
    );
    assert!(events.try_recv().is_err());
}

#[test]
fn events_are_persisted_and_broadcast_after_commit() {
    let store = Store::open_in_memory().unwrap();
    let mut events = store.subscribe();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let record = sample_run(&project.id);
    store
        .write(|tx| {
            tx.insert_run(&record)?;
            tx.log(&record.run.id, "hello")
        })
        .unwrap();

    let persisted = store.read(|tx| tx.events_since(0, None)).unwrap();
    assert_eq!(persisted.len(), 3);
    for event in &persisted {
        assert_eq!(&events.try_recv().unwrap(), event);
    }
    let of_run = store
        .read(|tx| tx.events_since(1, Some(&record.run.id)))
        .unwrap();
    assert_eq!(of_run, persisted[1..]);
    assert_eq!(store.read(|tx| tx.last_event_seq()).unwrap(), 3);
}

#[test]
fn attempts_and_requests_are_tracked() {
    let store = Store::open_in_memory().unwrap();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let record = sample_run(&project.id);
    let run_id = record.run.id.clone();
    store.write(|tx| tx.insert_run(&record)).unwrap();

    let attempt = store
        .write(|tx| tx.start_attempt(&run_id, 0, StepKind::Implement))
        .unwrap();
    let done = store
        .write(|tx| tx.finish_attempt(attempt.id, AttemptStatus::Succeeded, Some("ok")))
        .unwrap();
    assert_eq!(done.status, AttemptStatus::Succeeded);
    assert_eq!(store.read(|tx| tx.attempts(&run_id)).unwrap(), [done]);

    let request = store
        .write(|tx| {
            tx.create_request(NewRequest {
                kind: RequestKind::Plan,
                run_id: run_id.clone(),
                project_id: project.id.clone(),
                step_index: 0,
                step: StepKind::Plan,
                title: "Approve the plan".into(),
                detail: "1. do it".into(),
                session_id: None,
                options: Vec::new(),
            })
        })
        .unwrap();
    let pending = store
        .read(|tx| tx.requests(None, Some(RequestStatus::Pending)))
        .unwrap();
    assert_eq!(pending, std::slice::from_ref(&request));

    let approved = store
        .write(|tx| tx.resolve_request(&request.id, RequestStatus::Approved, Some("go")))
        .unwrap();
    assert_eq!(approved.status, RequestStatus::Approved);
    assert_eq!(approved.answer.as_deref(), Some("go"));
    assert!(approved.resolved_at.is_some());
    assert!(
        store
            .read(|tx| tx.requests(None, Some(RequestStatus::Pending)))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.read(|tx| tx.requests(Some(&run_id), None)).unwrap(),
        [approved]
    );
}

#[test]
fn sessions_round_trip_and_emit_snapshots() {
    let store = Store::open_in_memory().unwrap();
    let mut events = store.subscribe();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let mut record = sample_run(&project.id);
    record.base_commit = Some("abc123".into());
    record.run.cost_usd = 0.5;
    record
        .run
        .sessions
        .insert("implementer".into(), "s1".into());
    store.write(|tx| tx.insert_run(&record)).unwrap();
    let mut session = Session {
        id: "s1".into(),
        run_id: record.run.id.clone(),
        project_id: project.id.clone(),
        role: "implementer".into(),
        harness: "opencode".into(),
        model: None,
        state: SessionState::Active,
        cwd: "/src/app.worktrees/x".into(),
        usage: SessionUsage::default(),
        started_at: now_ms(),
        updated_at: now_ms(),
        ended_at: None,
        vendor_session_id: None,
    };
    store.write(|tx| tx.insert_session(&session)).unwrap();
    session.state = SessionState::Idle;
    session.usage.cost_usd = Some(0.5);
    session.vendor_session_id = Some("ses_123".into());
    store.write(|tx| tx.save_session(&mut session)).unwrap();

    let (stored_run, stored) = store
        .read(|tx| {
            Ok::<_, Error>((
                tx.run(&record.run.id)?.unwrap(),
                tx.sessions(Some(&record.run.id))?,
            ))
        })
        .unwrap();
    assert_eq!(stored, [session.clone()]);
    assert_eq!(stored_run.base_commit.as_deref(), Some("abc123"));
    assert_eq!(stored_run.run.cost_usd, 0.5);
    assert_eq!(stored_run.run.sessions["implementer"], "s1");
    assert_eq!(store.read(|tx| tx.session("s1")).unwrap(), Some(session));

    let mut kinds = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventBody::Session { session } = event.body {
            kinds.push(session.state);
        }
    }
    assert_eq!(kinds, [SessionState::Active, SessionState::Idle]);
}

#[test]
fn questions_keep_their_options_and_bus_messages_are_listed_per_run() {
    use agentux_api::{BusEndpoint, BusMessageKind};

    let store = Store::open_in_memory().unwrap();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let record = sample_run(&project.id);
    let run_id = record.run.id.clone();
    store.write(|tx| tx.insert_run(&record)).unwrap();

    let question = store
        .write(|tx| {
            tx.create_request(NewRequest {
                kind: RequestKind::Question,
                run_id: run_id.clone(),
                project_id: project.id.clone(),
                step_index: 0,
                step: StepKind::Implement,
                title: "implementer asks: which database?".into(),
                detail: "which database?".into(),
                session_id: Some("s1".into()),
                options: vec!["postgres".into(), "sqlite".into()],
            })
        })
        .unwrap();
    let stored = store.read(|tx| tx.request(&question.id)).unwrap().unwrap();
    assert_eq!(stored.kind, RequestKind::Question);
    assert_eq!(stored.options, ["postgres", "sqlite"]);

    let message = |id: &str, body: &str| BusMessage {
        id: id.into(),
        run_id: run_id.clone(),
        project_id: project.id.clone(),
        kind: BusMessageKind::Message,
        tool: Some("post_message".into()),
        from: BusEndpoint::Human,
        to: BusEndpoint::Role {
            role: "implementer".into(),
        },
        subject: body.into(),
        body: body.into(),
        at: now_ms(),
        turn: 1,
        max_turns: 6,
        message_id: Some(1),
        exchange: Some(1),
        in_reply_to: None,
        question_id: None,
        request_id: None,
        delivered_to: vec![],
        queued_for_role: Some("implementer".into()),
    };
    let (first, second) = (message("m1", "hello"), message("m2", "again"));
    store
        .write(|tx| {
            tx.emit(
                Some(&run_id),
                EventBody::BusMessage {
                    message: first.clone(),
                },
            )?;
            tx.log(&run_id, "not a bus message")?;
            tx.emit(
                Some(&run_id),
                EventBody::BusMessage {
                    message: second.clone(),
                },
            )
        })
        .unwrap();
    assert_eq!(
        store.read(|tx| tx.bus_messages(&run_id)).unwrap(),
        [first, second]
    );
    assert!(
        store
            .read(|tx| tx.bus_messages("other"))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_slow_transaction_does_not_stall_other_tasks() {
    use std::time::Instant;
    let store = Store::open_in_memory().unwrap();
    // The runtime has one worker. A write that holds it for the whole
    // transaction would keep the timer task from running until it ends.
    let writer = tokio::spawn(async move {
        store
            .write(|_| {
                std::thread::sleep(Duration::from_millis(300));
                Ok::<_, Error>(())
            })
            .unwrap();
        Instant::now()
    });
    let timer = tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        Instant::now()
    });
    let (written, ticked) = (writer.await.unwrap(), timer.await.unwrap());
    assert!(ticked < written, "the timer waited for the transaction");
}

#[test]
fn an_id_already_in_use_is_drawn_again() {
    let store = Store::open_in_memory().unwrap();
    let taken = store
        .write(|tx| tx.register_project("demo", "/srv/demo"))
        .unwrap()
        .id;
    let mut candidates = vec!["0000beef".to_string(), taken.clone()];
    let id = store
        .read(|tx| tx.unused_id_from(Table::Projects, || candidates.pop().unwrap()))
        .unwrap();
    assert_eq!(id, "0000beef");
}

#[test]
fn harness_limits_are_kept_until_their_time_or_cleared() {
    let store = Store::open_in_memory().unwrap();
    // An hour ahead, so the limits are in force for the emitted events too.
    let t = now_ms() + 3_600_000;
    let limit = |harness: &str, model: Option<&str>, until: Option<i64>| HarnessLimit {
        harness: harness.into(),
        model: model.map(str::to_string),
        kind: agentux_api::LimitKind::Session,
        until,
        until_known: until.is_some(),
        message: "limit reached".into(),
        at: 1_000,
    };
    store
        .write(|tx| {
            tx.set_limit(&limit("claude-code", None, Some(t + 5_000)))?;
            tx.set_limit(&limit("claude-code", Some("opus"), None))?;
            tx.set_limit(&limit("codex", None, Some(t + 9_000)))?;
            // The same harness and model again replaces it.
            tx.set_limit(&limit("codex", None, Some(t + 8_000)))
        })
        .unwrap();
    let at = |now| store.read(|tx| tx.limits(now)).unwrap();
    assert_eq!(at(t + 2_000).len(), 3);
    assert_eq!(at(t + 2_000)[2].until, Some(t + 8_000));
    // Past its time a limit is not in force, whether or not it was expired.
    assert_eq!(at(t + 6_000).len(), 2);

    let expired = store.write(|tx| tx.expire_limits(t + 6_000)).unwrap();
    assert_eq!(expired, [limit("claude-code", None, Some(t + 5_000))]);
    assert_eq!(
        store
            .write(|tx| tx.clear_limits("codex", Some("opus")))
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .write(|tx| tx.clear_limits("claude-code", None))
            .unwrap(),
        1
    );
    assert_eq!(at(t + 6_000), [limit("codex", None, Some(t + 8_000))]);

    let last = store.read(|tx| tx.events_since(0, None)).unwrap();
    match &last.last().unwrap().body {
        EventBody::Limits { limits } => assert_eq!(limits.len(), 1),
        other => panic!("expected the limits in force, got {other:?}"),
    }
}
