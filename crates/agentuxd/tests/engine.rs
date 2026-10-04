//! The run state machine, driven in-process with a fake executor.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentux_api::{AttemptStatus, RequestKind, RequestStatus, RunStatus, StepKind};
use agentuxd::FakeExecutor;
use agentuxd::engine::Error;
use agentuxd::executor::AgentOutcome;
use common::{fixture, pipeline, wait_for, wait_until_settled};

fn kinds(executor: &FakeExecutor) -> Vec<StepKind> {
    executor.calls().iter().map(|c| c.step).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_runs_every_step_in_the_worktree() {
    let f = fixture(&pipeline("test -f README.md || touch README.md"));
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.title, "Fix the login bug");
    assert_eq!(run.gate_max_attempts, 3);
    assert_eq!(run.review_max_rounds, 2);

    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert_eq!(
        kinds(&executor),
        [StepKind::Plan, StepKind::Implement, StepKind::Review]
    );
    assert_eq!(
        run.branch.as_deref(),
        Some(format!("aux/{}", run.id).as_str())
    );
    let worktree = run.worktree.clone().unwrap();
    assert!(Path::new(&worktree).join("agentux.yaml").exists());
    // The check ran in the worktree, not in the project.
    assert!(Path::new(&worktree).join("README.md").exists());
    assert!(!f.repo.join("README.md").exists());
    assert_eq!(run.pull_request.as_ref().unwrap().number, 1);
    assert!(run.finished_at.is_some());

    // Later steps see the plan; the implementer works in the worktree.
    let implement = &executor.calls()[1];
    assert_eq!(implement.plan.as_deref(), Some("fake plan by planner done"));
    assert_eq!(implement.worktree, Path::new(&worktree));
    assert_eq!(implement.prompt.as_deref(), Some("Fix the login bug"));

    let (_, attempts, requests) = engine.run(&run.id).unwrap();
    assert!(requests.is_empty());
    assert_eq!(attempts.len(), 5);
    assert!(
        attempts
            .iter()
            .all(|a| a.status == AttemptStatus::Succeeded)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_gate_loops_back_with_its_output() {
    let f = fixture(&pipeline(
        "test -f fixed || { echo not fixed yet; exit 1; }",
    ));
    // The second implementation attempt fixes the problem.
    let executor = Arc::new(FakeExecutor::new(|task| {
        if task.step == StepKind::Implement && task.attempt == 2 {
            std::fs::write(task.worktree.join("fixed"), "").unwrap();
        }
        Ok(AgentOutcome::Done {
            summary: "ok".into(),
        })
    }));
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert_eq!(
        kinds(&executor),
        [
            StepKind::Plan,
            StepKind::Implement,
            StepKind::Implement,
            StepKind::Review
        ]
    );
    let retry = &executor.calls()[2];
    let feedback = retry.feedback.as_deref().unwrap();
    assert!(feedback.contains("not fixed yet"), "{feedback}");
    // The review after a passing gate gets no stale feedback.
    assert_eq!(executor.calls()[3].feedback, None);
    assert_eq!(run.gate_attempt, 2);

    let (_, attempts, _) = engine.run(&run.id).unwrap();
    let gates: Vec<_> = attempts
        .iter()
        .filter(|a| a.step == StepKind::Gate)
        .map(|a| a.status)
        .collect();
    assert_eq!(gates, [AttemptStatus::Failed, AttemptStatus::Succeeded]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gate_that_keeps_failing_exhausts_its_attempts() {
    let f = fixture(&pipeline("false").replace("max_attempts: 3", "max_attempts: 2"));
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        run.error.as_deref(),
        Some("gate failed after 2 of 2 attempts")
    );
    assert_eq!(
        kinds(&executor),
        [StepKind::Plan, StepKind::Implement, StepKind::Implement]
    );
    assert!(run.pull_request.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn requested_changes_loop_back_to_the_implementer() {
    let f = fixture(&pipeline("true"));
    let executor = Arc::new(FakeExecutor::new(|task| {
        Ok(if task.step == StepKind::Review && task.attempt == 1 {
            AgentOutcome::ChangesRequested {
                comments: "rename the helper".into(),
            }
        } else {
            AgentOutcome::Done {
                summary: "ok".into(),
            }
        })
    }));
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert_eq!(
        kinds(&executor),
        [
            StepKind::Plan,
            StepKind::Implement,
            StepKind::Review,
            StepKind::Implement,
            StepKind::Review
        ]
    );
    assert_eq!(
        executor.calls()[3].feedback.as_deref(),
        Some("rename the helper")
    );
    assert_eq!(run.review_round, 2);
    assert_eq!(run.review_max_rounds, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_review_that_never_approves_exhausts_its_rounds() {
    let f = fixture(&pipeline("true"));
    let executor = Arc::new(FakeExecutor::new(|task| {
        Ok(if task.step == StepKind::Review {
            AgentOutcome::ChangesRequested {
                comments: "no".into(),
            }
        } else {
            AgentOutcome::Done {
                summary: "ok".into(),
            }
        })
    }));
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        run.error.as_deref(),
        Some("review still requests changes after 2 of 2 rounds")
    );
    let reviews = kinds(&executor)
        .into_iter()
        .filter(|k| *k == StepKind::Review)
        .count();
    assert_eq!(reviews, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_gates_pause_the_run_until_approved() {
    let yaml = pipeline("true")
        .replace(
            "  - step: plan\n    role: planner\n",
            "  - step: plan\n    role: planner\n    approve: true\n",
        )
        .replace(
            "  - step: pull_request\n",
            "  - step: pull_request\n    approve: true\n",
        );
    let f = fixture(&yaml);
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;

    // Paused after planning.
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Waiting);
    assert_eq!(run.step, StepKind::Plan);
    assert_eq!(kinds(&executor), [StepKind::Plan]);
    let pending = engine.requests(true).unwrap();
    assert_eq!(pending.len(), 1);
    let plan = &pending[0];
    assert_eq!(plan.kind, RequestKind::Plan);
    assert_eq!(plan.run_id, run.id);
    assert_eq!(plan.detail, "fake plan by planner done");

    let approved = engine.approve(&plan.id, Some("looks good")).unwrap();
    assert_eq!(approved.status, RequestStatus::Approved);
    // Approving twice is a conflict.
    assert!(matches!(
        engine.approve(&plan.id, None),
        Err(Error::Conflict(_))
    ));

    // Paused again before opening the pull request.
    let run = wait_for(&engine, &run.id, |r| {
        r.status == RunStatus::Waiting && r.step == StepKind::PullRequest
    })
    .await;
    assert!(executor.pull_requests().is_empty());
    let pending = engine.requests(true).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, RequestKind::Step);
    assert_eq!(pending[0].step, StepKind::PullRequest);

    engine.approve(&pending[0].id, None).unwrap();
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert_eq!(executor.pull_requests().len(), 1);
    assert!(engine.requests(true).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn denying_an_approval_fails_the_run() {
    let yaml = pipeline("true").replace(
        "  - step: plan\n    role: planner\n",
        "  - step: plan\n    role: planner\n    approve: true\n",
    );
    let f = fixture(&yaml);
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let run = f.start(&engine).await;
    wait_until_settled(&engine, &run.id).await;
    let request = engine.requests(true).unwrap().remove(0);

    let denied = engine.deny(&request.id, Some("too broad")).unwrap();
    assert_eq!(denied.status, RequestStatus::Denied);
    let (run, _, _) = engine.run(&run.id).unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("plan not approved: too broad"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_a_running_step() {
    let f = fixture(&pipeline("true"));
    let executor = Arc::new(FakeExecutor::default().with_delay(Duration::from_secs(60)));
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    wait_for(&engine, &run.id, |r| r.activity.contains("is working")).await;

    let cancelled = engine.cancel(&run.id).unwrap();
    assert_eq!(cancelled.status, RunStatus::Cancelled);
    assert!(matches!(engine.cancel(&run.id), Err(Error::Conflict(_))));
    let (run, attempts, _) = engine.run(&run.id).unwrap();
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(attempts.last().unwrap().status, AttemptStatus::Cancelled);
    assert!(executor.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_resumes_an_interrupted_step() {
    let f = fixture(&pipeline("true"));

    // First daemon: the planner never answers before the "crash".
    let slow = Arc::new(FakeExecutor::default().with_delay(Duration::from_secs(60)));
    let engine = f.engine(slow.clone());
    let run = f.start(&engine).await;
    wait_for(&engine, &run.id, |r| {
        r.step == StepKind::Plan && r.activity.contains("is working")
    })
    .await;
    engine.shutdown();
    drop(engine);

    // Second daemon on the same database.
    let fast = Arc::new(FakeExecutor::default());
    let engine = f.engine(fast.clone());
    assert_eq!(engine.resume().unwrap(), 1);
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert!(slow.calls().is_empty());
    let calls = fast.calls();
    assert_eq!(calls[0].step, StepKind::Plan);
    assert_eq!(calls[0].attempt, 2);
    let (_, attempts, _) = engine.run(&run.id).unwrap();
    assert_eq!(attempts[0].step, StepKind::Plan);
    assert_eq!(attempts[0].status, AttemptStatus::Interrupted);
    assert_eq!(attempts[1].step, StepKind::Plan);
    assert_eq!(attempts[1].status, AttemptStatus::Succeeded);
    // The worktree created by the first daemon was reused.
    assert_eq!(
        run.branch.as_deref(),
        Some(format!("aux/{}", run.id).as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_daemon_keeps_waiting_runs_waiting() {
    let yaml = pipeline("true").replace(
        "  - step: plan\n    role: planner\n",
        "  - step: plan\n    role: planner\n    approve: true\n",
    );
    let f = fixture(&yaml);
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let run = f.start(&engine).await;
    wait_until_settled(&engine, &run.id).await;
    engine.shutdown();
    drop(engine);

    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    engine.resume().unwrap();
    let request = engine.requests(true).unwrap().remove(0);
    let (run, _, _) = engine.run(&run.id).unwrap();
    assert_eq!(run.status, RunStatus::Waiting);
    engine.approve(&request.id, None).unwrap();
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    // The plan was not redone.
    assert_eq!(executor.calls()[0].step, StepKind::Implement);
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_need_a_prompt_or_an_issue() {
    let f = fixture(&pipeline("true"));
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    let project = engine
        .register_project(f.repo.to_str().unwrap())
        .await
        .unwrap();
    let result = engine.start_run(agentux_api::rpc::StartRun {
        project_id: project.id,
        ..Default::default()
    });
    assert!(matches!(result, Err(Error::InvalidParams(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn registering_requires_a_git_repository_with_a_valid_pipeline() {
    let f = fixture("version: 1\nbogus: true\n");
    let engine = f.engine(Arc::new(FakeExecutor::default()));
    assert!(matches!(
        engine.register_project(f.repo.to_str().unwrap()).await,
        Err(Error::InvalidProject(_))
    ));
    assert!(matches!(
        engine
            .register_project(f.tmp.path().to_str().unwrap())
            .await,
        Err(Error::InvalidProject(_))
    ));
    assert!(matches!(
        engine.register_project("relative/path").await,
        Err(Error::InvalidParams(_))
    ));
}

/// `pipeline(check)` with the check's timeout set and one gate attempt.
fn with_timeout(check: &str, timeout: &str, attempts: u32) -> String {
    pipeline(check)
        .replace(
            &format!("    run: {check}\n"),
            &format!("    run: {check}\n    timeout: {timeout}\n"),
        )
        .replace("max_attempts: 3", &format!("max_attempts: {attempts}"))
}

/// Whether `pid` runs (a zombie, waiting to be reaped, does not count).
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        let state = stat.rsplit(')').next().unwrap_or("").trim_start();
        !state.starts_with('Z')
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_that_times_out_fails_with_its_output() {
    let f = fixture(&with_timeout("echo slow; sleep 60", "1s", 2));
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(
        run.error.as_deref(),
        Some("gate failed after 2 of 2 attempts")
    );
    let feedback = executor.calls()[2].feedback.clone().unwrap();
    assert!(feedback.contains("slow"), "{feedback}");
    assert!(
        feedback.contains("the check timed out after 1s and was killed"),
        "{feedback}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_check_kills_everything_it_started() {
    let f = fixture(&pipeline("sleep 300 & echo $! > check.pid; wait"));
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_for(&engine, &run.id, |r| r.activity == "gate: running check").await;
    let pid_file = Path::new(run.worktree.as_deref().unwrap()).join("check.pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .filter(|s| s.ends_with('\n'))
            .and_then(|s| s.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the check did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(alive(pid));

    engine.cancel(&run.id).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the check's `sleep` outlived the cancelled run"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether rootless Podman works here. `AGENTUX_REQUIRE_PODMAN=1` (set in
/// CI) turns a skip into a failure.
fn podman_available() -> bool {
    let works = std::process::Command::new("podman")
        .args(["info", "--format", "{{.Host.Security.Rootless}}"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !works {
        assert!(
            std::env::var_os("AGENTUX_REQUIRE_PODMAN").is_none(),
            "AGENTUX_REQUIRE_PODMAN is set but podman does not work here"
        );
        eprintln!("skipped: podman is not available");
    }
    works
}

fn isolated(check: &str, image: &str) -> String {
    format!(
        "{}isolation:\n  mode: podman\n  image: {image}\n",
        pipeline(check)
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn isolated_checks_run_in_a_podman_container() {
    if !podman_available() {
        return;
    }
    // Only loopback inside, the worktree at its own path, and a file the
    // check writes is the user's.
    let f = fixture(&isolated(
        "test \"$(ls /sys/class/net)\" = lo && test -f agentux.yaml && touch made-in-container",
        "docker.io/library/busybox:1.37",
    ));
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    // Pulling the image can take a while on a fresh machine.
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let run = loop {
        let (run, _, _) = engine.run(&run.id).unwrap();
        if run.status != RunStatus::Running {
            break run;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the run did not finish"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    let made = Path::new(run.worktree.as_deref().unwrap()).join("made-in-container");
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(&made).unwrap().uid();
    assert_eq!(uid, std::fs::metadata(&f.repo).unwrap().uid());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_that_cannot_start_fails_the_run_without_looping() {
    if !podman_available() {
        return;
    }
    let f = fixture(&isolated("true", "localhost/agentux-no-such-image:0"));
    let executor = Arc::new(FakeExecutor::default());
    let engine = f.engine(executor.clone());
    let run = f.start(&engine).await;
    let run = wait_until_settled(&engine, &run.id).await;

    assert_eq!(run.status, RunStatus::Failed);
    let error = run.error.unwrap();
    assert!(
        error.starts_with("check check could not run: podman could not run the check container"),
        "{error}"
    );
    // Not the agent's fault, so no attempt to fix it.
    assert_eq!(kinds(&executor), [StepKind::Plan, StepKind::Implement]);
}
