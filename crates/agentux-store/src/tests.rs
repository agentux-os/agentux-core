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
            models: BTreeMap::new(),
            origin: None,
            agent_isolation: None,
        },
        phase: Phase::Setup,
        loops: BTreeMap::new(),
        feedback: None,
        config_yaml: None,
        base_commit: None,
    }
}

fn business_projection(
    projection_type: snapshot::BusinessProjectionKind,
    id: &str,
    organization_id: &str,
    run_id: Option<&str>,
    status: &str,
) -> snapshot::BusinessProjection {
    let id_field = match projection_type {
        snapshot::BusinessProjectionKind::Artifact => "artifactId",
        snapshot::BusinessProjectionKind::Task => "taskId",
        snapshot::BusinessProjectionKind::Command => "commandId",
    };
    let mut current_state = serde_json::Map::new();
    current_state.insert(id_field.into(), id.into());
    current_state.insert("organizationId".into(), organization_id.into());
    current_state.insert("status".into(), status.into());
    match projection_type {
        snapshot::BusinessProjectionKind::Artifact => {
            current_state.insert("uri".into(), "artifact://artifact".into());
            current_state.insert("mediaType".into(), "text/plain".into());
            current_state.insert("sha256".into(), "a".repeat(64).into());
        }
        snapshot::BusinessProjectionKind::Command => {
            current_state.insert("method".into(), "run.start".into());
        }
        snapshot::BusinessProjectionKind::Task => {}
    }
    if let Some(run_id) = run_id {
        current_state.insert("runId".into(), run_id.into());
    }
    let mut state = current_state.clone();
    let current_state = serde_json::Value::Object(current_state);
    state.insert("currentState".into(), current_state);
    snapshot::BusinessProjection {
        projection_type,
        id: id.into(),
        organization_id: organization_id.into(),
        run_id: run_id.map(str::to_string),
        state: serde_json::Value::Object(state),
    }
}

#[test]
fn business_projections_round_trip_update_tombstone_and_isolate_organizations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let store = Store::open(&path).unwrap();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let mut run = sample_run(&project.id);
    run.run.origin = Some(agentux_api::RunOrigin {
        organization_id: "org-a".into(),
        mission_id: "mission-a".into(),
        command_run_id: "command-run-a".into(),
        definition_version: None,
    });
    store.write(|tx| tx.insert_run(&run)).unwrap();

    for (kind, id, run_id) in [
        (
            snapshot::BusinessProjectionKind::Artifact,
            "artifact-1",
            Some(run.run.id.as_str()),
        ),
        (
            snapshot::BusinessProjectionKind::Task,
            "task-1",
            Some(run.run.id.as_str()),
        ),
        (snapshot::BusinessProjectionKind::Command, "command-1", None),
    ] {
        let created = business_projection(kind, id, "org-a", run_id, "running");
        store
            .write(|tx| tx.upsert_business_projection(&created))
            .unwrap();
        let updated = business_projection(kind, id, "org-a", run_id, "completed");
        store
            .write(|tx| tx.upsert_business_projection(&updated))
            .unwrap();

        let scoped = store
            .read(|tx| tx.capture_checkpoint_sources("org-a", now_ms()))
            .unwrap();
        let entry = scoped
            .business_projections
            .iter()
            .find(|entry| entry.projection_type == kind && entry.id == id)
            .unwrap();
        assert_eq!(entry.state.as_ref().unwrap()["status"], "completed");
        assert!(entry.updated_sequence > 0);
        let restarted = Store::open(&path).unwrap();
        assert_eq!(
            restarted
                .read(|tx| tx.business_projections("org-a"))
                .unwrap()
                .iter()
                .find(|entry| entry.projection_type == kind && entry.id == id)
                .unwrap()
                .state
                .as_ref()
                .unwrap()["status"],
            "completed"
        );
        assert!(
            store
                .read(|tx| tx.capture_checkpoint_sources("org-b", now_ms()))
                .unwrap()
                .business_projections
                .iter()
                .all(|entry| entry.id != id)
        );
        let wrong_owner = business_projection(kind, id, "org-b", run_id, "failed");
        assert!(
            store
                .write(|tx| tx.upsert_business_projection(&wrong_owner))
                .is_err()
        );

        assert!(
            store
                .write(|tx| tx.remove_business_projection(kind, id, "org-b"))
                .unwrap()
                .is_none()
        );
        let removed_sequence = store
            .write(|tx| tx.remove_business_projection(kind, id, "org-a"))
            .unwrap()
            .unwrap();
        let after_remove = store
            .read(|tx| tx.capture_checkpoint_sources("org-a", now_ms()))
            .unwrap();
        assert_eq!(
            after_remove
                .business_projections
                .iter()
                .find(|entry| entry.projection_type == kind && entry.id == id)
                .unwrap()
                .removed_at_sequence,
            Some(removed_sequence)
        );
    }

    drop(store);
    let reopened = Store::open(&path).unwrap();
    let captured = reopened
        .read(|tx| tx.capture_checkpoint_sources("org-a", now_ms()))
        .unwrap();
    for id in ["artifact-1", "task-1", "command-1"] {
        let tombstone = captured
            .business_projections
            .iter()
            .find(|entry| entry.id == id)
            .unwrap();
        assert!(tombstone.state.is_none());
        assert!(tombstone.removed_at_sequence.unwrap() > 0);
    }
    let unavailable = snapshot::unavailable_projections();
    for kind in [
        snapshot::ProjectionKind::Artifact,
        snapshot::ProjectionKind::Task,
        snapshot::ProjectionKind::Command,
    ] {
        assert!(unavailable.iter().any(|entry| entry.kind == kind));
        assert_eq!(kind.source_kind(), snapshot::SourceKind::Unavailable);
    }
}

#[test]
fn business_projection_capture_and_event_head_share_one_write_boundary() {
    let store = Store::open_in_memory().unwrap();
    let project = store
        .write(|tx| tx.register_project("app", "/src/app"))
        .unwrap();
    let mut run = sample_run(&project.id);
    run.run.origin = Some(agentux_api::RunOrigin {
        organization_id: "org-a".into(),
        mission_id: "mission-a".into(),
        command_run_id: "command-run-a".into(),
        definition_version: None,
    });
    let command = business_projection(
        snapshot::BusinessProjectionKind::Command,
        "command-1",
        "org-a",
        None,
        "completed",
    );
    // Protocol #17's valid command projection is organization-scoped and
    // does not require a runId; command-level operations can be origin-less.
    assert!(command.run_id.is_none());
    store
        .write(|tx| {
            tx.insert_run(&run)?;
            tx.upsert_business_projection(&command)
        })
        .unwrap();

    let captured = store
        .read(|tx| tx.capture_checkpoint_sources("org-a", now_ms()))
        .unwrap();
    let command = captured
        .business_projections
        .iter()
        .find(|entry| entry.id == "command-1")
        .unwrap();
    assert_eq!(captured.captured_event_head, command.updated_sequence);
    assert!(captured.runs.iter().any(|entry| entry.id == run.run.id));
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
    record.run.origin = Some(agentux_api::RunOrigin {
        organization_id: "acme".into(),
        mission_id: "ship".into(),
        command_run_id: "run-1a2b".into(),
        definition_version: Some("definition-3".into()),
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
#[test]
fn snapshot_source_marks_unsupported_projections_unavailable() {
    use crate::snapshot::{ProjectionKind, SourceKind};

    let sources = ProjectionKind::ALL.map(|kind| kind.source_kind());
    assert_eq!(
        sources,
        [
            SourceKind::Sourced,
            SourceKind::Unavailable,
            SourceKind::Unavailable,
            SourceKind::Derived,
            SourceKind::Unavailable,
            SourceKind::Sourced,
            SourceKind::Unavailable,
            SourceKind::Unavailable,
            SourceKind::Sourced,
            SourceKind::Sourced,
            SourceKind::Unavailable,
        ]
    );
    assert!(!crate::snapshot::checkpoint_capability_enabled());
}

#[test]
fn snapshot_materialization_is_immutable_scoped_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.db");
    let store = Store::open(&path).unwrap();
    let snapshot = snapshot::MaterializedSnapshot {
        organization_id: "org-a".into(),
        node_id: "node-a".into(),
        checkpoint_id: "checkpoint-a".into(),
        snapshot_id: "snapshot-a".into(),
        captured_event_head: 4,
        created_at_ms: 1_000,
        expires_at_ms: 1_000 + snapshot::MIN_RETENTION.as_millis() as i64,
        payload: serde_json::json!({"records":[1,2,3]}),
    };
    store
        .write(|tx| tx.materialize_snapshot(&snapshot))
        .unwrap();
    assert!(store.write(|tx| tx.materialize_snapshot(&snapshot)).is_ok());
    let mut changed = snapshot.clone();
    changed.payload = serde_json::json!({"records":[9]});
    assert!(store.write(|tx| tx.materialize_snapshot(&changed)).is_err());
    drop(store);

    let store = Store::open(&path).unwrap();
    let restored = store
        .read(|tx| tx.snapshot_materialization("org-a", "node-a", "checkpoint-a", 2_000))
        .unwrap()
        .unwrap();
    assert_eq!(restored, snapshot);
    let first_page = restored.page("org-a", "node-a", 0, 4, 2_000).unwrap();
    assert!(first_page.len() <= 4);
    assert_eq!(
        first_page,
        restored.page("org-a", "node-a", 0, 4, 2_000).unwrap()
    );
    let mut serialized = first_page;
    for page_index in 1..serde_json::to_vec(&snapshot.payload)
        .unwrap()
        .len()
        .div_ceil(4)
    {
        serialized.extend(
            restored
                .page("org-a", "node-a", page_index, 4, 2_000)
                .unwrap(),
        );
    }
    assert_eq!(serialized, serde_json::to_vec(&snapshot.payload).unwrap());
    assert_eq!(
        restored.page("org-b", "node-a", 0, 4, 2_000),
        Err(snapshot::PageError::ScopeMismatch)
    );
    assert_eq!(
        restored.page("org-a", "node-a", 0, 4, 16 * 60 * 1000),
        Err(snapshot::PageError::Expired)
    );
    assert_eq!(
        store
            .read(|tx| tx.snapshot_materialization(
                "org-a",
                "node-a",
                "checkpoint-a",
                16 * 60 * 1000
            ))
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .write(|tx| tx.prune_snapshot_materializations(16 * 60 * 1000))
            .unwrap(),
        1
    );
}

#[test]
fn checkpoint_capture_keeps_rows_and_event_head_in_one_sqlite_read_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.db");
    let store = Store::open(&path).unwrap();
    let external_path = path.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (written_tx, written_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        started_rx.recv().unwrap();
        let conn = Connection::open(external_path).unwrap();
        conn.execute(
            "INSERT INTO events (run_id, at, body) VALUES (NULL, 1, '{}')",
            [],
        )
        .unwrap();
        written_tx.send(()).unwrap();
    });
    let (projects_seen, event_head) = store
        .read(|tx| {
            let projects = tx.projects()?;
            started_tx.send(()).unwrap();
            written_rx.recv().unwrap();
            Ok::<_, Error>((projects, tx.last_event_seq()?))
        })
        .unwrap();
    writer.join().unwrap();
    assert!(projects_seen.is_empty());
    assert_eq!(event_head, 0);
    assert_eq!(store.read(|tx| tx.last_event_seq()).unwrap(), 1);
}
