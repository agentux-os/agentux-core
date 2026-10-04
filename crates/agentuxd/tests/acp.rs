//! The daemon driving agents through ACP: `AcpExecutor` with in-process fake
//! agents from `agentux-fake-agent` standing in for the harnesses. Gates run
//! real commands in the run's worktree.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_api::{
    AttemptStatus, Client, EventBody, MessageFrom, RequestKind, RequestStatus, RunStatus,
    SessionEvent, SessionState, StepKind,
};
use agentux_fake_agent::{Script, Turn, acp, script};
use agentux_harness::{AcpSession, Events, PermissionHandler};
use agentux_store::Store;
use agentuxd::executor::BoxFuture;
use agentuxd::{AcpExecutor, Engine, Launcher, Settings, server};
use common::{Fixture, fixture, wait_for, wait_until_settled};
use tokio::sync::oneshot;

type Result = agentux_fake_agent::TurnResult;

/// Starts a fake agent per launch; `scripts` picks its script by harness id.
struct FakeLauncher {
    scripts: Arc<dyn Fn(&str) -> Script + Send + Sync>,
    launched: Arc<Mutex<Vec<(String, PathBuf)>>>,
}

impl Launcher for FakeLauncher {
    fn launch<'a>(
        &'a self,
        harness: &'a str,
        cwd: &'a Path,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, std::result::Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            self.launched
                .lock()
                .unwrap()
                .push((harness.to_string(), cwd.to_path_buf()));
            let (transport, _agent) = agentux_fake_agent::spawn((self.scripts)(harness));
            AcpSession::connect(transport, cwd, permissions)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// What the fake agents saw.
#[derive(Default)]
struct Seen {
    /// (harness, prompt), in order.
    prompts: Vec<(String, String)>,
    reviews: u32,
    permission_outcomes: Vec<String>,
}

type Shared = Arc<Mutex<Seen>>;

fn executor(
    seen: &Shared,
    agent: fn(String, Turn, Shared) -> BoxFuture<'static, Result>,
) -> (Arc<AcpExecutor>, Arc<Mutex<Vec<(String, PathBuf)>>>) {
    let launched = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(seen);
    let launcher = FakeLauncher {
        scripts: Arc::new(move |harness: &str| {
            let harness = harness.to_string();
            let seen = Arc::clone(&seen);
            script(move |turn| {
                seen.lock()
                    .unwrap()
                    .prompts
                    .push((harness.clone(), turn.prompt.clone()));
                agent(harness.clone(), turn, Arc::clone(&seen))
            })
        }),
        launched: Arc::clone(&launched),
    };
    (Arc::new(AcpExecutor::new(launcher)), launched)
}

/// Planner, implementer (writes `hello.txt`) and reviewer (requests changes
/// once, as JSON, then approves with a bare keyword).
fn team(harness: String, turn: Turn, seen: Shared) -> BoxFuture<'static, Result> {
    Box::pin(async move {
        match harness.as_str() {
            "fake-planner" => {
                turn.plan(&[("Write hello.txt", acp::PlanEntryStatus::Pending)])?;
                turn.message("1. Create hello.txt\n")?;
                turn.message("2. Say hello in it\n")?;
            }
            "fake-coder" => {
                let text = if turn.prompt.contains("say world too") {
                    "hello world\n"
                } else {
                    "hello\n"
                };
                turn.tool_call(
                    "w1",
                    "Write hello.txt",
                    acp::ToolKind::Edit,
                    Some(("hello.txt", None, text)),
                )?;
                std::fs::write(turn.cwd.join("hello.txt"), text).unwrap();
                turn.tool_update("w1", acp::ToolCallStatus::Completed, Some("written"))?;
                turn.message("Wrote hello.txt.")?;
            }
            "fake-reviewer" => {
                let round = {
                    let mut seen = seen.lock().unwrap();
                    seen.reviews += 1;
                    seen.reviews
                };
                if round == 1 {
                    turn.message(
                        "Close, but not yet.\n\n```json\n{\"verdict\": \"CHANGES_REQUESTED\", \"comments\": [\"say world too\"]}\n```\n",
                    )?;
                } else {
                    turn.message("Good now.\n\nVerdict: APPROVE")?;
                }
            }
            other => panic!("unexpected harness {other}"),
        }
        Ok(acp::StopReason::EndTurn)
    })
}

const TEAM_PIPELINE: &str = "version: 1
roles:
  planner:
    harness: fake-planner
  implementer:
    harness: fake-coder
  reviewer:
    harness: fake-reviewer
checks:
  - name: hello
    run: test -f hello.txt
pipeline:
  - step: plan
    role: planner
  - step: implement
    role: implementer
  - step: gate
    checks: [hello]
    on_fail: implement
    max_attempts: 2
  - step: review
    role: reviewer
    on_changes_requested: implement
    max_rounds: 3
  - step: pull_request
";

fn git_log(dir: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["-C", dir, "log", "--format=%s", "main..HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

async fn start(f: &Fixture, engine: &Engine, prompt: &str) -> agentux_api::Run {
    let project = engine
        .register_project(f.repo.to_str().unwrap())
        .await
        .unwrap();
    engine
        .start_run(agentux_api::rpc::StartRun {
            project_id: project.id,
            prompt: Some(prompt.into()),
            ..Default::default()
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_pipeline_through_acp_agents() {
    let f = fixture(TEAM_PIPELINE);
    let seen = Shared::default();
    let (executor, launched) = executor(&seen, team);
    let engine = f.engine(executor);
    let run = start(&f, &engine, "Add hello.txt").await;
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert!(run.pull_request.is_none());

    let (_, attempts, requests) = engine.run(&run.id).unwrap();
    assert!(requests.is_empty());
    let steps: Vec<(StepKind, AttemptStatus)> =
        attempts.iter().map(|a| (a.step, a.status)).collect();
    assert_eq!(
        steps,
        [
            (StepKind::Plan, AttemptStatus::Succeeded),
            (StepKind::Implement, AttemptStatus::Succeeded),
            (StepKind::Gate, AttemptStatus::Succeeded),
            (StepKind::Review, AttemptStatus::ChangesRequested),
            (StepKind::Implement, AttemptStatus::Succeeded),
            (StepKind::Gate, AttemptStatus::Succeeded),
            (StepKind::Review, AttemptStatus::Succeeded),
            (StepKind::PullRequest, AttemptStatus::Succeeded),
        ]
    );
    // The agent's reply is the plan; the review's JSON comments are the
    // feedback; the keyword verdict's text is the summary.
    assert_eq!(
        attempts[0].output.as_deref(),
        Some("1. Create hello.txt\n2. Say hello in it")
    );
    assert_eq!(attempts[3].output.as_deref(), Some("- say world too"));
    assert_eq!(
        attempts[6].output.as_deref(),
        Some("Good now.\n\nVerdict: APPROVE")
    );
    let skipped = attempts[7].output.as_deref().unwrap();
    assert!(
        skipped.starts_with("PR skipped: the project has no `origin` remote"),
        "{skipped}"
    );

    // Prompts carried the plan and the reviewer's comments.
    {
        let seen = seen.lock().unwrap();
        let coder: Vec<&String> = seen
            .prompts
            .iter()
            .filter(|(h, _)| h == "fake-coder")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(coder.len(), 2);
        assert!(coder[0].contains("## Plan\n\n1. Create hello.txt"));
        assert!(coder[0].contains("Do not commit"));
        assert!(coder[1].contains("The reviewer requested these changes:\n\n- say world too"));
        let review = &seen.prompts.iter().find(|(h, _)| h == "fake-reviewer").unwrap().1;
        assert!(review.contains("git diff "), "{review}");
    }

    // The daemon committed each implementation in the worktree.
    let worktree = run.worktree.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(Path::new(&worktree).join("hello.txt")).unwrap(),
        "hello world\n"
    );
    assert_eq!(
        git_log(&worktree),
        ["Address review comments: Add hello.txt", "Add hello.txt"]
    );

    // One session per role, the implementer's reused, all in the worktree,
    // all ended with the run.
    let launched = launched.lock().unwrap().clone();
    let harnesses: Vec<&str> = launched.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(harnesses, ["fake-planner", "fake-coder", "fake-reviewer"]);
    assert!(launched.iter().all(|(_, cwd)| cwd == Path::new(&worktree)));
    assert_eq!(run.sessions.len(), 3);
    wait_for(&engine, &run.id, |_| {
        engine
            .sessions(Some(&run.id))
            .unwrap()
            .iter()
            .all(|s| s.state == SessionState::Ended)
    })
    .await;
    let sessions = engine.sessions(Some(&run.id)).unwrap();
    assert_eq!(sessions.len(), 3);
    assert_eq!(run.sessions["implementer"], sessions[1].id);
    assert_eq!(sessions[1].role, "implementer");
    assert_eq!(sessions[1].harness, "fake-coder");

    // Session events: the prompt, coalesced agent text, the tool call, the
    // diff and the plan.
    let events = engine
        .store()
        .read(|tx| tx.events_since(0, Some(&run.id)))
        .unwrap();
    let session_events: Vec<(String, SessionEvent)> = events
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::SessionEvent { session_id, event } => Some((session_id, event)),
            _ => None,
        })
        .collect();
    let planner = &sessions[0].id;
    let planner_events: Vec<&SessionEvent> = session_events
        .iter()
        .filter(|(id, _)| id == planner)
        .map(|(_, e)| e)
        .collect();
    assert!(matches!(
        planner_events[0],
        SessionEvent::Message { from: MessageFrom::User, .. }
    ));
    assert!(matches!(planner_events[1], SessionEvent::Plan { .. }));
    assert_eq!(
        planner_events[2],
        &SessionEvent::Message {
            from: MessageFrom::Agent,
            text: "1. Create hello.txt\n2. Say hello in it\n".into()
        }
    );
    assert!(session_events.iter().any(|(_, e)| matches!(
        e,
        SessionEvent::Diff { path, new_text, .. } if path == "hello.txt" && new_text == "hello world\n"
    )));
    assert!(session_events.iter().any(|(_, e)| matches!(
        e,
        SessionEvent::ToolCall { title: Some(title), .. } if title == "Write hello.txt"
    )));
}

/// Asks before writing `hello.txt`; writes it only if allowed.
fn careful(_: String, turn: Turn, seen: Shared) -> BoxFuture<'static, Result> {
    Box::pin(async move {
        turn.tool_call("x1", "touch hello.txt", acp::ToolKind::Execute, None)?;
        let outcome = turn
            .request_permission("x1", "touch hello.txt", acp::ToolKind::Execute)
            .await?;
        seen.lock()
            .unwrap()
            .permission_outcomes
            .push(outcome.clone());
        if outcome == "allow" {
            std::fs::write(turn.cwd.join("hello.txt"), "hello\n").unwrap();
            turn.tool_update("x1", acp::ToolCallStatus::Completed, None)?;
            turn.message("Created hello.txt.")?;
        } else {
            turn.tool_update("x1", acp::ToolCallStatus::Failed, Some("denied"))?;
            turn.message("I was not allowed to create hello.txt.")?;
        }
        Ok(acp::StopReason::EndTurn)
    })
}

const CAREFUL_PIPELINE: &str = "version: 1
roles:
  implementer:
    harness: fake-coder
checks:
  - name: hello
    run: test -f hello.txt
pipeline:
  - step: implement
    role: implementer
  - step: gate
    checks: [hello]
";

/// Serves the API for `engine` on a socket in the fixture's temp dir.
async fn serve(f: &Fixture, engine: &Engine) -> (Client, oneshot::Sender<()>) {
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
    (Client::connect(&socket).await.unwrap(), stop)
}

async fn pending_permission(client: &mut Client) -> agentux_api::PermissionRequest {
    for _ in 0..1000 {
        let pending = client.list_requests(true).await.unwrap();
        if let Some(request) = pending.into_iter().next() {
            return request;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no permission request arrived");
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_requests_round_trip_through_the_api() {
    let f = fixture(CAREFUL_PIPELINE);
    let seen = Shared::default();
    let (executor, _) = executor(&seen, careful);
    let engine = f.engine(executor);
    let (mut client, _stop) = serve(&f, &engine).await;

    // Allowed: the agent goes on and the gate passes.
    let run = start(&f, &engine, "Create hello.txt").await;
    let request = pending_permission(&mut client).await;
    assert_eq!(request.kind, RequestKind::Permission);
    assert_eq!(request.run_id, run.id);
    assert_eq!(request.step, StepKind::Implement);
    assert_eq!(
        request.title,
        "implementer (fake-coder) wants to run: touch hello.txt"
    );
    assert!(request.detail.contains("touch hello.txt"));
    let detail = client.get_run(&run.id).await.unwrap();
    assert_eq!(detail.run.status, RunStatus::Waiting);
    assert_eq!(detail.sessions.len(), 1);
    assert_eq!(request.session_id.as_deref(), Some(detail.sessions[0].id.as_str()));
    assert_eq!(detail.sessions[0].state, SessionState::Waiting);

    let approved = client.approve(&request.id, None).await.unwrap();
    assert_eq!(approved.status, RequestStatus::Approved);
    let done = wait_until_settled(&engine, &run.id).await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);
    assert_eq!(seen.lock().unwrap().permission_outcomes, ["allow"]);
    let sessions = client.list_sessions(Some(&run.id)).await.unwrap();
    assert_eq!(sessions.len(), 1);

    // Denied: the agent is told no and carries on; here the gate then fails.
    let run = start(&f, &engine, "Create hello.txt again").await;
    let request = pending_permission(&mut client).await;
    assert_eq!(request.run_id, run.id);
    let denied = client
        .deny(&request.id, Some("not now".into()))
        .await
        .unwrap();
    assert_eq!(denied.status, RequestStatus::Denied);
    let failed = wait_until_settled(&engine, &run.id).await;
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(
        failed.error.as_deref(),
        Some("gate failed after 1 of 1 attempts")
    );
    assert_eq!(
        seen.lock().unwrap().permission_outcomes,
        ["allow", "deny"]
    );
    let (_, attempts, _) = engine.run(&run.id).unwrap();
    assert_eq!(
        attempts[0].output.as_deref(),
        Some("I was not allowed to create hello.txt.")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_approve_answers_permissions_without_asking() {
    let f = fixture(CAREFUL_PIPELINE);
    let seen = Shared::default();
    let (executor, _) = executor(&seen, careful);
    let engine = Engine::with_settings(
        Store::open(&f.database).unwrap(),
        executor,
        Settings {
            auto_approve_permissions: true,
        },
    );
    let run = start(&f, &engine, "Create hello.txt").await;
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert!(engine.requests(false).unwrap().is_empty());
    assert_eq!(seen.lock().unwrap().permission_outcomes, ["allow"]);
}

/// Each session reports a cumulative cost of $0.30 (one turn per role here).
fn spender(_: String, turn: Turn, _: Shared) -> BoxFuture<'static, Result> {
    Box::pin(async move {
        turn.usage(1000, 100_000, Some(0.30))?;
        if turn.prompt.contains("## Your step: review") {
            turn.message("```json\n{\"verdict\": \"APPROVE\", \"comments\": \"ok\"}\n```")?;
        } else {
            turn.message("done")?;
        }
        Ok(acp::StopReason::EndTurn)
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_over_budget_pauses_until_approved() {
    let f = fixture(
        "version: 1
roles:
  planner:
    harness: fake-a
  implementer:
    harness: fake-b
  reviewer:
    harness: fake-c
pipeline:
  - step: plan
    role: planner
  - step: implement
    role: implementer
  - step: review
    role: reviewer
budget:
  max_usd_per_run: 0.5
",
    );
    let seen = Shared::default();
    let (executor, _) = executor(&seen, spender);
    let engine = f.engine(executor);
    let run = start(&f, &engine, "Spend").await;

    // $0.30 after planning, $0.60 after implementing: the review waits.
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Waiting, "{:?}", run.error);
    assert_eq!(run.step, StepKind::Review);
    assert!((run.cost_usd - 0.6).abs() < 1e-9, "{}", run.cost_usd);
    let pending = engine.requests(true).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, RequestKind::Budget);
    assert!(pending[0].detail.contains("$0.60"), "{}", pending[0].detail);
    assert_eq!(seen.lock().unwrap().prompts.len(), 2);

    engine.approve(&pending[0].id, None).unwrap();
    let run = wait_until_settled(&engine, &run.id).await;
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);
    assert!((run.budget_usd.unwrap() - 1.1).abs() < 1e-9);
    assert!((run.cost_usd - 0.9).abs() < 1e-9);
    let sessions = engine.sessions(Some(&run.id)).unwrap();
    assert!(sessions.iter().all(|s| s.usage.cost_usd == Some(0.30)));
}
