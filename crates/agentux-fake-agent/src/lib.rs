//! A scriptable ACP agent that runs in-process over a byte pipe, for tests of
//! `agentux-harness` and `agentuxd`. No vendor CLI or network involved.
//!
//! [`spawn`] starts the agent side of a connection and returns the client
//! side's transport, to hand to `AcpSession::connect`. Each prompt runs the
//! script with a [`Turn`], through which the script streams updates, asks for
//! permission, waits for cancellation and finally returns a stop reason.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

/// The session id the fake agent hands out.
pub const SESSION: &str = "fake-session";

/// The client side of the pipe; implements `ConnectTo<Client>`.
pub type Transport = ByteStreams<Compat<WriteHalf<DuplexStream>>, Compat<ReadHalf<DuplexStream>>>;

pub type TurnResult = agent_client_protocol::Result<acp::StopReason>;

/// What the agent does with one prompt.
pub type Script =
    Arc<dyn Fn(Turn) -> Pin<Box<dyn Future<Output = TurnResult> + Send>> + Send + Sync>;

/// Wraps an async function as a [`Script`].
pub fn script<F, Fut>(f: F) -> Script
where
    F: Fn(Turn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = TurnResult> + Send + 'static,
{
    Arc::new(move |turn| Box::pin(f(turn)))
}

/// One prompt turn, as seen by the script.
pub struct Turn {
    /// The text blocks of the prompt, joined.
    pub prompt: String,
    /// The working directory the session was created in.
    pub cwd: PathBuf,
    cx: ConnectionTo<Client>,
    cancelled: CancellationToken,
}

impl Turn {
    fn send(&self, update: acp::SessionUpdate) -> agent_client_protocol::Result<()> {
        self.cx
            .send_notification(acp::SessionNotification::new(SESSION, update))
    }

    pub fn message(&self, text: &str) -> agent_client_protocol::Result<()> {
        self.send(acp::SessionUpdate::AgentMessageChunk(
            acp::ContentChunk::new(text_block(text)),
        ))
    }

    pub fn thought(&self, text: &str) -> agent_client_protocol::Result<()> {
        self.send(acp::SessionUpdate::AgentThoughtChunk(
            acp::ContentChunk::new(text_block(text)),
        ))
    }

    pub fn plan(
        &self,
        entries: &[(&str, acp::PlanEntryStatus)],
    ) -> agent_client_protocol::Result<()> {
        self.send(acp::SessionUpdate::Plan(acp::Plan::new(
            entries
                .iter()
                .map(|(content, status)| {
                    acp::PlanEntry::new(*content, acp::PlanEntryPriority::Medium, status.clone())
                })
                .collect(),
        )))
    }

    /// Starts a tool call, optionally carrying a file diff `(path, old, new)`.
    pub fn tool_call(
        &self,
        id: &str,
        title: &str,
        kind: acp::ToolKind,
        diff: Option<(&str, Option<&str>, &str)>,
    ) -> agent_client_protocol::Result<()> {
        let mut call = acp::ToolCall::new(id.to_string(), title)
            .kind(kind)
            .status(acp::ToolCallStatus::Pending);
        if let Some((path, old, new)) = diff {
            let mut diff = acp::Diff::new(path, new);
            if let Some(old) = old {
                diff = diff.old_text(old.to_string());
            }
            call = call.content(vec![diff.into()]);
        }
        self.send(acp::SessionUpdate::ToolCall(call))
    }

    pub fn tool_update(
        &self,
        id: &str,
        status: acp::ToolCallStatus,
        output: Option<&str>,
    ) -> agent_client_protocol::Result<()> {
        let mut fields = acp::ToolCallUpdateFields::new().status(status);
        if let Some(output) = output {
            fields = fields.content(vec![acp::ToolCallContent::Content(acp::Content::new(
                text_block(output),
            ))]);
        }
        self.send(acp::SessionUpdate::ToolCallUpdate(
            acp::ToolCallUpdate::new(id.to_string(), fields),
        ))
    }

    /// Reports context usage and, optionally, the session's cumulative cost
    /// in USD.
    pub fn usage(
        &self,
        used: u64,
        size: u64,
        cost_usd: Option<f64>,
    ) -> agent_client_protocol::Result<()> {
        let mut usage = acp::UsageUpdate::new(used, size);
        if let Some(cost) = cost_usd {
            usage = usage.cost(acp::Cost::new(cost, "USD"));
        }
        self.send(acp::SessionUpdate::UsageUpdate(usage))
    }

    /// Asks the client for permission to run tool call `id` and returns the
    /// selected option id (`allow` or `deny`), or `cancelled`.
    pub async fn request_permission(
        &self,
        id: &str,
        title: &str,
        kind: acp::ToolKind,
    ) -> agent_client_protocol::Result<String> {
        use acp::PermissionOptionKind::*;
        let request = acp::RequestPermissionRequest::new(
            SESSION,
            acp::ToolCallUpdate::new(
                id.to_string(),
                acp::ToolCallUpdateFields::new().title(title).kind(kind),
            ),
            vec![
                acp::PermissionOption::new("allow".to_string(), "Allow", AllowOnce),
                acp::PermissionOption::new("deny".to_string(), "Deny", RejectOnce),
            ],
        );
        let response = self.cx.send_request(request).block_task().await?;
        Ok(match response.outcome {
            acp::RequestPermissionOutcome::Selected(selected) => selected.option_id.0.to_string(),
            _ => "cancelled".to_string(),
        })
    }

    /// Completes when the client cancels this turn.
    pub async fn cancelled(&self) {
        self.cancelled.cancelled().await
    }
}

fn text_block(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text))
}

/// Starts a fake agent that answers every prompt with `script`. The returned
/// task ends when the client closes the connection.
pub fn spawn(script: Script) -> (Transport, JoinHandle<agent_client_protocol::Result<()>>) {
    spawn_observed(script, |_| {})
}

/// Like [`spawn`], calling `on_new_session` with each `session/new` request.
pub fn spawn_observed(
    script: Script,
    on_new_session: impl Fn(&acp::NewSessionRequest) + Send + Sync + 'static,
) -> (Transport, JoinHandle<agent_client_protocol::Result<()>>) {
    spawn_with(
        script,
        Setup {
            on_new_session: Box::new(on_new_session),
            ..Setup::default()
        },
    )
}

/// Receives `(config id, value)`.
pub type OnSetConfig = Box<dyn Fn(&str, &str) + Send + Sync>;

/// How [`spawn_with`] sets up the agent.
pub struct Setup {
    /// Called with each `session/new` request.
    pub on_new_session: Box<dyn Fn(&acp::NewSessionRequest) + Send + Sync>,
    /// Models offered as a `model` session config option (the first one is
    /// current). Empty: no model choice is offered.
    pub models: Vec<String>,
    /// Called with the config id and value of each
    /// `session/set_config_option` request.
    pub on_set_config: OnSetConfig,
    /// Advertise `loadSession` and answer `session/load` (replaying one
    /// agent message, [`REPLAYED`], as the history). Without it, the agent
    /// has no `session/load`.
    pub load_session: bool,
    /// Called with the session id of each `session/load` request.
    pub on_load_session: Box<dyn Fn(&str) + Send + Sync>,
}

/// The history a loaded session replays.
pub const REPLAYED: &str = "(replayed history)";

impl Default for Setup {
    fn default() -> Self {
        Self {
            on_new_session: Box::new(|_| {}),
            models: Vec::new(),
            on_set_config: Box::new(|_, _| {}),
            load_session: false,
            on_load_session: Box::new(|_| {}),
        }
    }
}

/// Like [`spawn`], set up by `setup`.
pub fn spawn_with(
    script: Script,
    setup: Setup,
) -> (Transport, JoinHandle<agent_client_protocol::Result<()>>) {
    let Setup {
        on_new_session,
        models,
        on_set_config,
        load_session,
        on_load_session,
    } = setup;
    let model_option = (!models.is_empty()).then(|| {
        acp::SessionConfigOption::select(
            "model",
            "Model",
            models[0].clone(),
            models
                .iter()
                .map(|m| acp::SessionConfigSelectOption::new(m.clone(), m.clone()))
                .collect::<Vec<_>>(),
        )
        .category(acp::SessionConfigOptionCategory::Model)
    });
    let (client_io, agent_io) = tokio::io::duplex(256 * 1024);
    let (agent_read, agent_write) = tokio::io::split(agent_io);
    let (client_read, client_write) = tokio::io::split(client_io);

    let cwd: Arc<Mutex<PathBuf>> = Arc::default();
    let turn_token = Arc::new(Mutex::new(CancellationToken::new()));
    let agent = Agent
        .builder()
        .name("fake-agent")
        .on_receive_request(
            async move |request: acp::InitializeRequest, responder, _cx| {
                responder.respond(
                    acp::InitializeResponse::new(request.protocol_version).agent_capabilities(
                        acp::AgentCapabilities::new().load_session(load_session),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cwd = Arc::clone(&cwd);
                async move |request: acp::LoadSessionRequest,
                            responder,
                            cx: ConnectionTo<Client>| {
                    if !load_session {
                        return responder
                            .respond_with_error(agent_client_protocol::Error::method_not_found());
                    }
                    on_load_session(&request.session_id.0);
                    *cwd.lock().unwrap() = request.cwd;
                    cx.send_notification(acp::SessionNotification::new(
                        request.session_id.clone(),
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text_block(
                            REPLAYED,
                        ))),
                    ))?;
                    responder.respond(acp::LoadSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cwd = Arc::clone(&cwd);
                async move |request: acp::NewSessionRequest, responder, _cx| {
                    on_new_session(&request);
                    *cwd.lock().unwrap() = request.cwd;
                    responder.respond(
                        acp::NewSessionResponse::new(SESSION)
                            .config_options(model_option.clone().map(|option| vec![option])),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::SetSessionConfigOptionRequest, responder, _cx| {
                let value = request
                    .value
                    .as_value_id()
                    .map(|v| v.0.to_string())
                    .unwrap_or_default();
                on_set_config(&request.config_id.0, &value);
                responder.respond(acp::SetSessionConfigOptionResponse::new(Vec::new()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let turn_token = Arc::clone(&turn_token);
                async move |request: acp::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let prompt = request
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            acp::ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let cancelled = CancellationToken::new();
                    *turn_token.lock().unwrap() = cancelled.clone();
                    let turn = Turn {
                        prompt,
                        cwd: cwd.lock().unwrap().clone(),
                        cx: cx.clone(),
                        cancelled,
                    };
                    let run = script(turn);
                    // The turn sends requests of its own, so it must run
                    // outside the dispatch loop.
                    cx.spawn(async move {
                        responder.respond_with_result(run.await.map(acp::PromptResponse::new))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_: acp::CancelNotification, _cx| {
                turn_token.lock().unwrap().cancel();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        );
    let task = tokio::spawn(agent.connect_to(ByteStreams::new(
        agent_write.compat_write(),
        agent_read.compat(),
    )));
    (
        ByteStreams::new(client_write.compat_write(), client_read.compat()),
        task,
    )
}
