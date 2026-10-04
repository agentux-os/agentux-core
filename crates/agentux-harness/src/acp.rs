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
    McpServer, PermissionHandler, PermissionRequest, PlanEntry, PlanStatus, StopReason, ToolCall,
    ToolCallUpdate, ToolKind, ToolStatus, Usage,
};

/// How long [`HarnessSession::shutdown`] waits for the harness to exit after
/// its stdin is closed before killing it.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// A harness launched as an ACP agent.
#[derive(Debug, Clone)]
pub struct AcpHarness {
    spec: HarnessSpec,
    options: SessionOptions,
}

/// How sessions are set up besides their working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOptions {
    /// Passed to the agent in `session/new`.
    pub mcp_servers: Vec<McpServer>,
    /// Model to select through the session's `model` config option, when the
    /// agent offers one (see [`ModelSelection`]).
    pub model: Option<String>,
}

/// What became of the model asked for in [`SessionOptions::model`].
///
/// ACP has no model field in `session/new`. Agents that let clients choose a
/// model list a session config option of category `model` (a select) in the
/// `session/new` response; the client sets it with
/// `session/set_config_option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelection {
    /// No model was asked for: the agent's default applies.
    Default,
    /// The model is selected (or already was).
    Selected(String),
    /// The model could not be selected, and why; the agent's default applies.
    Unavailable(String),
}

impl AcpHarness {
    pub fn new(spec: HarnessSpec) -> Self {
        Self {
            spec,
            options: SessionOptions::default(),
        }
    }

    /// MCP servers to attach to every session this harness starts, e.g. the
    /// `agentux` bus.
    pub fn with_mcp_servers(mut self, servers: Vec<McpServer>) -> Self {
        self.options.mcp_servers = servers;
        self
    }

    /// Model to select in every session this harness starts.
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.options.model = model;
        self
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
        let (mut session, events) =
            AcpSession::connect_with(transport, cwd, &self.options, permissions).await?;
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
    model: ModelSelection,
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
        Self::connect_with_mcp_servers(transport, cwd, &[], permissions).await
    }

    /// Like [`AcpSession::connect`], with `mcp_servers` passed to the agent in
    /// `session/new`.
    pub async fn connect_with_mcp_servers(
        transport: impl ConnectTo<Client> + 'static,
        cwd: &Path,
        mcp_servers: &[McpServer],
        permissions: PermissionHandler,
    ) -> Result<(Self, Events), Error> {
        let options = SessionOptions {
            mcp_servers: mcp_servers.to_vec(),
            model: None,
        };
        Self::connect_with(transport, cwd, &options, permissions).await
    }

    /// Like [`AcpSession::connect`], with MCP servers and a model. A model
    /// the agent does not offer is not an error: see
    /// [`AcpSession::model_selection`].
    pub async fn connect_with(
        transport: impl ConnectTo<Client> + 'static,
        cwd: &Path,
        options: &SessionOptions,
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
            .send_request(new_session_request(cwd, &options.mcp_servers))
            .block_task()
            .await?;
        let model = match &options.model {
            None => ModelSelection::Default,
            Some(model) => {
                let offered = session.config_options.as_deref().unwrap_or_default();
                select_model(&connection, &session.session_id, offered, model).await
            }
        };

        Ok((
            Self {
                connection,
                session_id: session.session_id,
                turn,
                close,
                task,
                process: None,
                model,
            },
            events,
        ))
    }

    /// The agent's identifier for this session.
    pub fn id(&self) -> &str {
        &self.session_id.0
    }

    /// Whether the model asked for in [`SessionOptions`] is in use.
    pub fn model_selection(&self) -> &ModelSelection {
        &self.model
    }
}

/// Selects `model` through the session's model config option.
async fn select_model(
    connection: &ConnectionTo<Agent>,
    session: &acp::SessionId,
    options: &[acp::SessionConfigOption],
    model: &str,
) -> ModelSelection {
    let selector = options.iter().find_map(|option| {
        let is_model = matches!(
            option.category,
            Some(acp::SessionConfigOptionCategory::Model)
        ) || &*option.id.0 == "model";
        match &option.kind {
            acp::SessionConfigKind::Select(select) if is_model => Some((option, select)),
            _ => None,
        }
    });
    let Some((option, select)) = selector else {
        return ModelSelection::Unavailable(format!(
            "model `{model}` not selected: the agent offers no model choice over ACP, so it uses its default"
        ));
    };
    let choices: Vec<&acp::SessionConfigSelectOption> = match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(choices) => choices.iter().collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => Vec::new(),
    };
    let found = choices
        .iter()
        .find(|choice| &*choice.value.0 == model)
        .or_else(|| {
            choices
                .iter()
                .find(|choice| choice.name.eq_ignore_ascii_case(model))
        });
    let Some(choice) = found else {
        let offered: Vec<&str> = choices.iter().map(|choice| &*choice.value.0).collect();
        return ModelSelection::Unavailable(format!(
            "model `{model}` not selected: the agent offers {}, and uses `{}`",
            offered.join(", "),
            select.current_value.0
        ));
    };
    if choice.value == select.current_value {
        return ModelSelection::Selected(choice.value.0.to_string());
    }
    let request = acp::SetSessionConfigOptionRequest::new(
        session.clone(),
        option.id.clone(),
        acp::SessionConfigOptionValue::value_id(choice.value.clone()),
    );
    match connection.send_request(request).block_task().await {
        Ok(_) => ModelSelection::Selected(choice.value.0.to_string()),
        Err(e) => ModelSelection::Unavailable(format!(
            "model `{model}` not selected: the agent refused it ({e}), so it uses its default"
        )),
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

/// The `session/new` request for `cwd`, with `mcp_servers` as stdio servers.
fn new_session_request(cwd: &Path, mcp_servers: &[McpServer]) -> acp::NewSessionRequest {
    let servers = mcp_servers
        .iter()
        .map(|server| {
            let env = server
                .env
                .iter()
                .map(|(name, value)| acp::EnvVariable::new(name.clone(), value.clone()))
                .collect();
            acp::McpServer::Stdio(
                acp::McpServerStdio::new(server.name.clone(), server.command.clone())
                    .args(server.args.clone())
                    .env(env),
            )
        })
        .collect();
    acp::NewSessionRequest::new(cwd).mcp_servers(servers)
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
