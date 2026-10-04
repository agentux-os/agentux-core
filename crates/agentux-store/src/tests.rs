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
        },
        phase: Phase::Setup,
        loops: BTreeMap::new(),
        feedback: None,
        config_yaml: None,
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
