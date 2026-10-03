//! The ACP implementation of [`Harness`]: the harness runs as a subprocess
//! speaking ACP (protocol v1) over stdio, driven by the official
//! `agent-client-protocol` SDK.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::{
    Cost, Decision, Error, Event, Events, FileDiff, Harness, HarnessSession, HarnessSpec,
    PermissionHandler, PermissionRequest, PlanEntry, PlanStatus, StopReason, ToolCall,
    ToolCallUpdate, ToolKind, ToolStatus, Usage,
};

/// How long [`HarnessSession::shutdown`] waits for the harness to exit after
/// its stdin is closed before killing it.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// A harness launched as an ACP agent.
#[derive(Debug, Clone)]
pub struct AcpHarness {
    spec: HarnessSpec,
}

impl AcpHarness {
    pub fn new(spec: HarnessSpec) -> Self {
        Self { spec }
    }

    pub fn spec(&self) -> &HarnessSpec {
        &self.spec
    }
}

impl Harness for AcpHarness {
    type Session = AcpSession;

    /// Spawns the harness with `cwd` as its working directory, initializes
    /// ACP and creates a session in `cwd`. The harness's stderr is inherited.
    async fn start(
        &self,
        cwd: &Path,
        permissions: PermissionHandler,
    ) -> Result<(AcpSession, Events), Error> {
        let (process, stdin, stdout) = AgentProcess::spawn(&self.spec, cwd)?;
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        // On error `process` is dropped, which kills the harness.
        let (mut session, events) = AcpSession::connect(transport, cwd, permissions).await?;
        session.process = Some(process);
        Ok((session, events))
    }
}

/// An ACP session with one agent.
pub struct AcpSession {
    connection: ConnectionTo<Agent>,
    session_id: acp::SessionId,
    /// Cancelled by [`HarnessSession::cancel`] to withdraw the permission
    /// requests of the current turn; replaced at each prompt.
    turn: Arc<Mutex<CancellationToken>>,
    /// Ends the connection when sent or dropped.
    close: oneshot::Sender<()>,
    task: JoinHandle<Result<(), agent_client_protocol::Error>>,
    process: Option<AgentProcess>,
}

impl AcpSession {
    /// Runs ACP over `transport`: initializes the connection and creates a
    /// session in `cwd`. [`AcpHarness`] calls this with the stdio of the
    /// process it spawned; tests and in-process agents can pass any transport.
    pub async fn connect(
        transport: impl ConnectTo<Client> + 'static,
        cwd: &Path,
        permissions: PermissionHandler,
    ) -> Result<(Self, Events), Error> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let turn = Arc::new(Mutex::new(CancellationToken::new()));
        let (connection_tx, connection_rx) = oneshot::channel();
        let (close, close_rx) = oneshot::channel::<()>();

        let current_turn = Arc::clone(&turn);
        let builder = Client
            .builder()
            .name("agentux")
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _connection| {
                    for event in events_from(notification.update) {
                        // The caller may have stopped listening; that is fine.
                        let _ = events_tx.send(event);
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: acp::RequestPermissionRequest,
                            responder,
                            connection: ConnectionTo<Agent>| {
                    let withdrawn = current_turn.lock().expect("turn lock").clone();
                    let ask = permissions(permission_request(&request));
                    // Answer outside the dispatch loop so updates keep flowing
                    // while the handler waits (e.g. for a human).
                    connection.spawn(async move {
                        let outcome = tokio::select! {
                            decision = ask => select_option(&request.options, decision),
                            () = withdrawn.cancelled() => acp::RequestPermissionOutcome::Cancelled,
                        };
                        responder.respond(acp::RequestPermissionResponse::new(outcome))
                    })
                },
                agent_client_protocol::on_receive_request!(),
            );

        let task = tokio::spawn(builder.connect_with(
            transport,
            async move |connection: ConnectionTo<Agent>| {
                let _ = connection_tx.send(connection);
                // Keep the connection open until the session closes it.
                let _ = close_rx.await;
                Ok(())
            },
        ));
        let Ok(connection) = connection_rx.await else {
            return Err(match task.await {
                Ok(Err(e)) => e.into(),
                _ => Error::Closed,
            });
        };

        connection
            .send_request(
                acp::InitializeRequest::new(ProtocolVersion::V1).client_info(
                    acp::Implementation::new("agentux", env!("CARGO_PKG_VERSION")),
                ),
            )
            .block_task()
            .await?;
        let session = connection
            .send_request(acp::NewSessionRequest::new(cwd))
            .block_task()
            .await?;

        Ok((
            Self {
                connection,
                session_id: session.session_id,
                turn,
                close,
                task,
                process: None,
            },
            events,
        ))
    }

    /// The agent's identifier for this session.
    pub fn id(&self) -> &str {
        &self.session_id.0
    }
}

impl HarnessSession for AcpSession {
    async fn prompt(&self, text: &str) -> Result<StopReason, Error> {
        *self.turn.lock().expect("turn lock") = CancellationToken::new();
        let request = acp::PromptRequest::new(
            self.session_id.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
        );
        let response = self.connection.send_request(request).block_task().await?;
        Ok(stop_reason(response.stop_reason))
    }

    fn cancel(&self) -> Result<(), Error> {
        self.turn.lock().expect("turn lock").cancel();
        self.connection
            .send_notification(acp::CancelNotification::new(self.session_id.clone()))?;
        Ok(())
    }

    async fn shutdown(self) -> Result<(), Error> {
        let Self {
            close,
            task,
            process,
            ..
        } = self;
        let _ = close.send(());
        // Ending the connection closes the harness's stdin.
        let result = match task.await {
            Ok(result) => result.map_err(Error::from),
            Err(_) => Err(Error::Closed),
        };
        if let Some(process) = process {
            process.wait_or_kill(EXIT_GRACE).await;
        }
        result
    }
}

/// The harness subprocess. Dropping it kills the process and, on Unix, its
/// whole process group, so agents started through wrappers such as `npx` do
/// not outlive the session.
struct AgentProcess {
    child: Child,
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
}

impl AgentProcess {
    fn spawn(spec: &HarnessSpec, cwd: &Path) -> Result<(Self, ChildStdin, ChildStdout), Error> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| Error::Spawn {
            command: spec.command_line(),
            source,
        })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        Ok((
            Self {
                #[cfg(unix)]
                group: child
                    .id()
                    .and_then(|id| rustix::process::Pid::from_raw(id.try_into().ok()?)),
                child,
            },
            stdin,
            stdout,
        ))
    }

    /// Waits up to `grace` for the process to exit, then kills it.
    async fn wait_or_kill(mut self, grace: Duration) {
        let _ = tokio::time::timeout(grace, self.child.wait()).await;
        // Drop kills whatever is left, including the rest of the group.
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group {
            // ESRCH just means the group is already gone.
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = self.child.start_kill();
    }
}

fn events_from(update: acp::SessionUpdate) -> Vec<Event> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => text(&chunk.content)
            .map(Event::AgentMessage)
            .into_iter()
            .collect(),
        acp::SessionUpdate::AgentThoughtChunk(chunk) => text(&chunk.content)
            .map(Event::AgentThought)
            .into_iter()
            .collect(),
        acp::SessionUpdate::ToolCall(call) => {
            let id = call.tool_call_id.0.to_string();
            let mut events = vec![Event::ToolCall(ToolCall {
                id: id.clone(),
                title: call.title,
                kind: tool_kind(call.kind),
                status: tool_status(call.status),
            })];
            events.extend(diffs(&id, &call.content));
            events
        }
        acp::SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            let fields = update.fields;
            let content = fields.content.unwrap_or_default();
            let output: Vec<String> = content
                .iter()
                .filter_map(|item| match item {
                    acp::ToolCallContent::Content(c) => text(&c.content),
                    _ => None,
                })
                .collect();
            let mut events = vec![Event::ToolCallUpdate(ToolCallUpdate {
                id: id.clone(),
                title: fields.title,
                status: fields.status.map(tool_status),
                output: (!output.is_empty()).then(|| output.join("\n")),
            })];
            events.extend(diffs(&id, &content));
            events
        }
        acp::SessionUpdate::Plan(plan) => vec![Event::Plan(
            plan.entries
                .into_iter()
                .map(|entry| PlanEntry {
                    content: entry.content,
                    status: match entry.status {
                        acp::PlanEntryStatus::InProgress => PlanStatus::InProgress,
                        acp::PlanEntryStatus::Completed => PlanStatus::Completed,
                        _ => PlanStatus::Pending,
                    },
                })
                .collect(),
        )],
        acp::SessionUpdate::UsageUpdate(usage) => vec![Event::Usage(Usage {
            used_tokens: usage.used,
            context_tokens: usage.size,
            cost: usage.cost.map(|cost| Cost {
                amount: cost.amount,
                currency: cost.currency,
            }),
        })],
        // Echoed user messages, slash commands, modes and session metadata
        // carry nothing a run needs yet.
        _ => Vec::new(),
    }
}

fn text(content: &acp::ContentBlock) -> Option<String> {
    match content {
        acp::ContentBlock::Text(text) => Some(text.text.clone()),
        _ => None,
    }
}

fn diffs(tool_call_id: &str, content: &[acp::ToolCallContent]) -> Vec<Event> {
    content
        .iter()
        .filter_map(|item| match item {
            acp::ToolCallContent::Diff(diff) => Some(Event::Diff(FileDiff {
                tool_call_id: tool_call_id.to_string(),
                path: diff.path.clone(),
                old_text: diff.old_text.clone(),
                new_text: diff.new_text.clone(),
            })),
            _ => None,
        })
        .collect()
}

fn tool_kind(kind: acp::ToolKind) -> ToolKind {
    match kind {
        acp::ToolKind::Read => ToolKind::Read,
        acp::ToolKind::Edit => ToolKind::Edit,
        acp::ToolKind::Delete => ToolKind::Delete,
        acp::ToolKind::Move => ToolKind::Move,
        acp::ToolKind::Search => ToolKind::Search,
        acp::ToolKind::Execute => ToolKind::Execute,
        acp::ToolKind::Think => ToolKind::Think,
        acp::ToolKind::Fetch => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_status(status: acp::ToolCallStatus) -> ToolStatus {
    match status {
        acp::ToolCallStatus::InProgress => ToolStatus::InProgress,
        acp::ToolCallStatus::Completed => ToolStatus::Completed,
        acp::ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

fn stop_reason(reason: acp::StopReason) -> StopReason {
    match reason {
        acp::StopReason::MaxTokens => StopReason::MaxTokens,
        acp::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        acp::StopReason::Refusal => StopReason::Refusal,
        acp::StopReason::Cancelled => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

fn permission_request(request: &acp::RequestPermissionRequest) -> PermissionRequest {
    PermissionRequest {
        tool_call_id: request.tool_call.tool_call_id.0.to_string(),
        title: request.tool_call.fields.title.clone(),
        kind: request.tool_call.fields.kind.map(tool_kind),
    }
}

/// Picks the option matching `decision`, preferring the one-time variant so a
/// decision never outlives this tool call when the agent offers a choice. An
/// agent that offers no matching option gets `Cancelled`, which it must treat
/// as a refusal.
fn select_option(
    options: &[acp::PermissionOption],
    decision: Decision,
) -> acp::RequestPermissionOutcome {
    let preference = match decision {
        Decision::Allow => [
            acp::PermissionOptionKind::AllowOnce,
            acp::PermissionOptionKind::AllowAlways,
        ],
        Decision::Deny => [
            acp::PermissionOptionKind::RejectOnce,
            acp::PermissionOptionKind::RejectAlways,
        ],
    };
    preference
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
        .map_or(acp::RequestPermissionOutcome::Cancelled, |option| {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(id: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(id.to_string(), id, kind)
    }

    fn selected(outcome: acp::RequestPermissionOutcome) -> Option<String> {
        match outcome {
            acp::RequestPermissionOutcome::Selected(selected) => {
                Some(selected.option_id.0.to_string())
            }
            _ => None,
        }
    }

    #[test]
    fn permission_decisions_prefer_one_time_options() {
        use acp::PermissionOptionKind::*;
        let all = [
            option("always", AllowAlways),
            option("once", AllowOnce),
            option("reject-always", RejectAlways),
            option("reject", RejectOnce),
        ];
        assert_eq!(
            selected(select_option(&all, Decision::Allow)).as_deref(),
            Some("once")
        );
        assert_eq!(
            selected(select_option(&all, Decision::Deny)).as_deref(),
            Some("reject")
        );
        assert_eq!(
            selected(select_option(&all[..1], Decision::Allow)).as_deref(),
            Some("always")
        );
        assert_eq!(selected(select_option(&all[..2], Decision::Deny)), None);
    }
}
