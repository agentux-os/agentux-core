//! `aux bus-stdio --session-token` against a daemon: a fake ACP agent
//! launches the MCP server the daemon gave it in `session/new` (the real `aux`
//! binary) and calls bus tools through it, the way a harness does.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_api::rpc::StartRun;
use agentux_api::{BusEndpoint, BusMessageKind, RunStatus};
use agentux_fake_agent::{acp, script};
use agentux_harness::{AcpSession, Events, PermissionHandler, SessionOptions};
use agentux_store::Store;
use agentuxd::executor::BoxFuture;
use agentuxd::{AcpExecutor, BusLink, Engine, LaunchSpec, Launcher, Settings, server};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

type Notes = Arc<Mutex<Vec<String>>>;

/// Each session's agent starts the bus MCP server it was given and uses it.
struct Launch {
    notes: Notes,
}

impl Launcher for Launch {
    fn launch<'a>(
        &'a self,
        spec: LaunchSpec<'a>,
        permissions: PermissionHandler,
    ) -> BoxFuture<'a, Result<(AcpSession, Events), String>> {
        Box::pin(async move {
            let server = spec.mcp_servers[0].clone();
            let notes = Arc::clone(&self.notes);
            let script = script(move |turn| {
                let server = server.clone();
                let notes = Arc::clone(&notes);
                async move {
                    let mut command = tokio::process::Command::new(&server.command);
                    command.args(&server.args);
                    let (transport, _stderr) = TokioChildProcess::builder(command)
                        .stderr(Stdio::inherit())
                        .spawn()
                        .unwrap();
                    let client = tokio::time::timeout(TIMEOUT, ().serve(transport))
                        .await
                        .expect("initialize timed out")
                        .expect("initialize failed");
                    let info = client.peer_info().unwrap();
                    notes.lock().unwrap().push(format!(
                        "instructions: {}",
                        info.instructions.clone().unwrap_or_default()
                    ));
                    let mut tools: Vec<String> = client
                        .list_all_tools()
                        .await
                        .unwrap()
                        .into_iter()
                        .map(|tool| tool.name.into_owned())
                        .collect();
                    tools.sort_unstable();
                    notes
                        .lock()
                        .unwrap()
                        .push(format!("tools: {}", tools.join(",")));
                    let call = |name: &'static str, args: Value| {
                        CallToolRequestParams::new(name)
                            .with_arguments(args.as_object().unwrap().clone())
                    };
                    let state = client
                        .call_tool(call("get_run_state", json!({})))
                        .await
                        .unwrap();
                    let state = state.structured_content.unwrap();
                    notes.lock().unwrap().push(format!(
                        "you: {} on {}",
                        state["you"]["role"], state["state"]["branch"]
                    ));
                    let posted = client
                        .call_tool(call(
                            "post_message",
                            json!({"to": "human", "body": "implemented, over"}),
                        ))
                        .await
                        .unwrap();
                    assert_ne!(posted.is_error, Some(true), "{posted:?}");
                    let refused = client
                        .call_tool(call(
                            "post_message",
                            json!({"to": "role:nobody", "body": "?"}),
                        ))
                        .await
                        .unwrap();
                    notes
                        .lock()
                        .unwrap()
                        .push(format!("refused: {}", refused.is_error == Some(true)));
                    client.cancel().await.unwrap();
                    turn.message("done")?;
                    Ok(acp::StopReason::EndTurn)
                }
            });
            let (transport, _agent) = agentux_fake_agent::spawn(script);
            let options = SessionOptions {
                mcp_servers: spec.mcp_servers.to_vec(),
                model: None,
            };
            AcpSession::connect_with(transport, spec.cwd, &options, permissions)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_reaches_the_daemon_bus_through_aux_bus_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("shop");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(
        repo.join("agentux.yaml"),
        "version: 1
roles:
  implementer:
    harness: fake-coder
pipeline:
  - step: implement
    role: implementer
",
    )
    .unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@agentux.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    );

    let socket = tmp.path().join("run").join("agentuxd.sock");
    let notes = Notes::default();
    let engine = Engine::with_settings(
        Store::open(&tmp.path().join("agentuxd.db")).unwrap(),
        Arc::new(AcpExecutor::new(Launch {
            notes: Arc::clone(&notes),
        })),
        Settings {
            bus: Some(BusLink {
                socket: socket.clone(),
                aux: PathBuf::from(env!("CARGO_BIN_EXE_aux")),
            }),
            ..Settings::default()
        },
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn({
        let (engine, socket) = (engine.clone(), socket.clone());
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

    let project = engine
        .register_project(repo.to_str().unwrap())
        .await
        .unwrap();
    let run = engine
        .start_run(StartRun {
            project_id: project.id,
            prompt: Some("Ship it".into()),
            ..Default::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    let run = loop {
        let (run, _, _) = engine.run(&run.id).unwrap();
        if run.status != RunStatus::Running {
            break run;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the run did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(run.status, RunStatus::Done, "{:?}", run.error);

    let notes = notes.lock().unwrap().clone();
    assert!(
        notes[0].starts_with(&format!(
            "instructions: You are the implementer of AgentUX run {} in project shop",
            run.id
        )),
        "{notes:?}"
    );
    assert_eq!(
        notes[1..],
        [
            "tools: ask_human,get_run_state,handoff,post_message,read_messages,request_review"
                .to_string(),
            format!("you: \"implementer\" on \"aux/{}\"", run.id),
            "refused: true".to_string(),
        ]
    );
    let log = engine.bus_list(&run.id).unwrap();
    let message = log
        .iter()
        .find(|m| m.kind == BusMessageKind::Message)
        .unwrap();
    assert_eq!(message.body, "implemented, over");
    assert_eq!(message.to, BusEndpoint::Human);
    assert!(matches!(&message.from, BusEndpoint::Session { role, .. } if role == "implementer"));
    let _ = stop.send(());
}
