//! The AgentUX node gateway: reports this node to AgentUX Command over the
//! AgentUX Protocol (AUXP v1, `agentux-protocol`).
//!
//! This first, read-only phase sends what Command needs to see the node:
//! `node.hello`, a `node.heartbeat` every 30 seconds, the agent CLIs
//! (`harness.status`) when they change and every 5 minutes, usage limits
//! (`harness.limits_changed`), pending approvals (`approval.requested`) and
//! run progress (`run.updated`, `run.completed`, `run.failed`). It reads
//! everything from the local `agentuxd` through its API; it never exposes
//! the daemon's socket, its database or credentials. Commands from Command
//! come with the next phase, behind the node's own policy.
//!
//! Messages are POSTed to `<command>/v1/protocol/messages` with the node's
//! bearer token. Events get a sequence number that survives restarts
//! ([`State`]); unsent ones wait in order and go out when Command answers
//! again.
//!
//! Commands from Command come over a WebSocket the node opens
//! ([`channel`]); the node's [`policy`] runs, holds or refuses each, and
//! [`commands`] runs it against agentuxd.

pub mod channel;
pub mod commands;
pub mod policy;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, io};

use agentux_api::rpc::HarnessInfo;
use agentux_api::{
    Event, EventBody, HarnessLimit, PermissionRequest, RequestStatus, Run, RunStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub use auxp::PROTOCOL_VERSION;
/// The instance id an enrollment token names (`auxe1_<instanceId>_<secret>`).
pub use auxp::enrollment::token_instance;
/// What this gateway can answer, for `node.hello`.
pub const CAPABILITIES: &[&str] = &[
    "harness.status",
    "harness.limits_changed",
    "run.updated",
    "approval.requested",
    "run.snapshot",
    "run.cancel",
    "run.start",
    "request.resolve",
    "bus.post",
    "harness.list",
    "harness.test",
    "limits.clear",
    "preferences.set",
];
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
const HARNESSES_EVERY: Duration = Duration::from_secs(300);
/// Events kept while Command is unreachable; the oldest go first past this.
const OUTBOX_MAX: usize = 5_000;

/// How this node reaches Command: `~/.config/agentux/node.json`, written by
/// `aux node enroll`, readable by the user only (it holds the node token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeConfig {
    /// `https://command.example.org` (plain http only to this machine).
    pub command_url: String,
    pub organization_id: String,
    pub node_id: String,
    /// The node's own, scoped credential at Command.
    pub token: String,
    /// The Command server's instance id, pinned at enrollment: the node
    /// stops talking to a server that answers with another one, and refuses
    /// its commands. `None` for nodes set up with a static token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_instance_id: Option<String>,
}

/// Joins the Command server at `url` with an enrollment token it issued:
/// first checks the server is the one the token names (so the token never
/// goes to another server), then trades it for this node's credential.
/// Returns the config to save, and whether an operator still has to approve
/// the node.
pub async fn enroll(
    url: &str,
    token: &str,
    node_name: Option<&str>,
    hostname: &str,
) -> Result<(NodeConfig, bool), String> {
    let url = url.trim().trim_end_matches('/').to_string();
    let probe = NodeConfig {
        command_url: url.clone(),
        organization_id: "-".into(),
        node_id: "-".into(),
        token: "-".into(),
        command_instance_id: None,
    };
    probe.validate()?;
    let wanted = token_instance(token).ok_or("this is not an AgentUX Command enrollment token")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("agentux-gateway/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let instance: auxp::enrollment::Instance = client
        .get(format!("{url}/v1/instance"))
        .send()
        .await
        .map_err(|e| format!("cannot reach Command at {url}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{url} is not an AgentUX Command server: {e}"))?;
    instance
        .check()
        .map_err(|e| format!("{url} is not an AgentUX Command server for this node: {e}"))?;
    let actual = instance.instance_id.as_str();
    if actual != wanted {
        return Err(format!(
            "{url} is Command server {actual}, but the token was issued by {wanted}: ask for a token from this server, or use that server's address"
        ));
    }
    let response = client
        .post(format!("{url}/v1/enroll"))
        .json(&auxp::enrollment::Redemption {
            token: token.trim().to_string(),
            node_name: node_name.map(str::to_string),
            hostname: Some(hostname.to_string()),
        })
        .send()
        .await
        .map_err(|e| format!("cannot reach Command at {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await.unwrap_or_default();
        return Err(format!(
            "Command refused the token ({status}): {}",
            body["error"].as_str().unwrap_or("no reason given")
        ));
    }
    let enrolled: auxp::enrollment::Enrolled = response
        .json()
        .await
        .map_err(|e| format!("unexpected answer from Command: {e}"))?;
    enrolled
        .check()
        .map_err(|e| format!("unexpected answer from Command: {e}"))?;
    if enrolled.instance_id != wanted {
        return Err("Command answered for another instance".into());
    }
    let config = NodeConfig {
        command_url: url,
        organization_id: enrolled.organization_id,
        node_id: enrolled.node_id,
        token: enrolled.node_secret,
        command_instance_id: Some(enrolled.instance_id),
    };
    config.validate()?;
    Ok((config, !enrolled.approved))
}

/// The error a pinned node returns when it reached another Command server.
pub const INSTANCE_MISMATCH: &str = "Command instance mismatch";

impl NodeConfig {
    /// Checks the URL and names.
    pub fn validate(&self) -> Result<(), String> {
        let url = self.command_url.trim_end_matches('/');
        let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
            .iter()
            .any(|prefix| {
                url == *prefix
                    || url.starts_with(&format!("{prefix}:"))
                    || url.starts_with(&format!("{prefix}/"))
            });
        if !url.starts_with("https://") && !local {
            return Err(format!(
                "Command must be reached over https (http only on this machine): {url}"
            ));
        }
        for (name, value) in [
            ("organization", &self.organization_id),
            ("node id", &self.node_id),
            ("token", &self.token),
        ] {
            if value.trim().is_empty() {
                return Err(format!("the {name} is empty"));
            }
        }
        Ok(())
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/v1/protocol/messages",
            self.command_url.trim_end_matches('/')
        )
    }
}

/// `$XDG_CONFIG_HOME/agentux/node.json`, else `~/.config/agentux/node.json`.
pub fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("agentux").join("node.json"))
}

/// `$XDG_STATE_HOME/agentux/gateway.json`, else `~/.local/state/...`.
pub fn state_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("agentux").join("gateway.json"))
}

pub fn load_config(path: &Path) -> Result<Option<NodeConfig>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let config: NodeConfig =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            config
                .validate()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Some(config))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Writes the config readable by the user only.
pub fn save_config(path: &Path, config: &NodeConfig) -> Result<(), String> {
    config.validate()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, &text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    fs::write(path, text)
}

/// A node id from the host name: `node-<hostname>`.
pub fn default_node_id() -> String {
    let host = fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| env::var("HOSTNAME").ok())
        .map(|h| h.trim().to_lowercase())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "agentux".into());
    let clean: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("node-{clean}")
}

/// What survives a restart: the last event sequence handed out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub last_sequence: i64,
}

impl State {
    pub fn load(path: &Path) -> State {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(self) {
            let _ = fs::write(path, text);
        }
    }
}

/// Builds AUXP messages for one node.
pub struct Messages {
    config: NodeConfig,
    state: State,
    state_path: Option<PathBuf>,
    counter: u64,
}

fn rfc3339(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

impl Messages {
    pub fn new(config: NodeConfig, state_path: Option<PathBuf>) -> Self {
        let state = state_path.as_deref().map(State::load).unwrap_or_default();
        Self {
            config,
            state,
            state_path,
            counter: 0,
        }
    }

    pub fn last_sequence(&self) -> i64 {
        self.state.last_sequence
    }

    fn base(&mut self, extra: Value) -> Map<String, Value> {
        self.counter += 1;
        let mut params = Map::new();
        params.insert("protocolVersion".into(), PROTOCOL_VERSION.into());
        params.insert(
            "organizationId".into(),
            self.config.organization_id.clone().into(),
        );
        params.insert("nodeId".into(), self.config.node_id.clone().into());
        params.insert(
            "messageId".into(),
            format!(
                "{}-{}-{}",
                self.config.node_id,
                jiff::Timestamp::now().as_millisecond(),
                self.counter
            )
            .into(),
        );
        params.insert("occurredAt".into(), now().into());
        if let Value::Object(extra) = extra {
            params.extend(extra);
        }
        params
    }

    fn request(&mut self, method: &str, extra: Value) -> Value {
        let params = self.base(extra);
        let id = params["messageId"].clone();
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    /// An event: the next sequence, kept across restarts.
    fn event(&mut self, method: &str, extra: Value) -> Value {
        self.state.last_sequence += 1;
        if let Some(path) = &self.state_path {
            self.state.save(path);
        }
        let sequence = self.state.last_sequence;
        let mut params = self.base(extra);
        params.insert("sequence".into(), sequence.into());
        params.insert(
            "eventId".into(),
            format!("{}-{sequence}", self.config.node_id).into(),
        );
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    }

    pub fn hello(&mut self, agentux_version: &str) -> Value {
        self.request(
            "node.hello",
            json!({
                "capabilities": CAPABILITIES,
                "agentuxVersion": agentux_version,
                "supportedProtocolVersions": [PROTOCOL_VERSION],
                "lastSequence": self.state.last_sequence,
            }),
        )
    }

    pub fn heartbeat(&mut self, status: &str, active_runs: usize, waiting: usize) -> Value {
        let params = self.base(json!({
            "status": status,
            "capacity": { "activeRuns": active_runs, "waitingForHuman": waiting },
        }));
        json!({ "jsonrpc": "2.0", "method": "node.heartbeat", "params": params })
    }

    pub fn harness_status(&mut self, harnesses: &[HarnessInfo]) -> Value {
        let harnesses: Vec<Value> = harnesses
            .iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "installed": h.installed,
                    "signedIn": h.signed_in,
                    "version": h.version,
                    "experimental": h.experimental,
                    "usage": h.usage,
                    "quota": h.quota.iter().map(|q| json!({
                        "label": q.label,
                        "usedPercent": q.used_percent,
                        "resetsAt": q.resets_at.map(rfc3339),
                    })).collect::<Vec<_>>(),
                    "limits": h.limits.iter().map(limit_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        self.event("harness.status", json!({ "harnesses": harnesses }))
    }

    /// Any event, e.g. from the command connection.
    pub fn custom_event(&mut self, method: &str, payload: Value) -> Value {
        self.event(method, payload)
    }

    pub fn limits_changed(&mut self, limits: &[HarnessLimit]) -> Value {
        let limits: Vec<Value> = limits.iter().map(limit_json).collect();
        self.event("harness.limits_changed", json!({ "limits": limits }))
    }

    /// The AUXP event for one `agentuxd` event, if Command cares about it.
    pub fn from_event(&mut self, event: &Event) -> Option<Value> {
        match &event.body {
            EventBody::Run { run } => {
                let method = match run.status {
                    RunStatus::Done => "run.completed",
                    RunStatus::Failed => "run.failed",
                    _ => "run.updated",
                };
                Some(self.event(method, run_json(run)))
            }
            EventBody::Request { request } if request.status == RequestStatus::Pending => {
                Some(self.event("approval.requested", request_json(request)))
            }
            EventBody::Limits { limits } => Some(self.limits_changed(limits)),
            _ => None,
        }
    }
}

fn limit_json(l: &HarnessLimit) -> Value {
    json!({
        "harness": l.harness,
        "model": l.model,
        "kind": l.kind,
        "until": l.until.map(rfc3339),
        "untilKnown": l.until_known,
        "message": l.message,
    })
}

/// What Command sees of a run: its state, never the prompt's files or
/// transcripts. The prompt itself is the human's request, so it is included.
fn run_json(run: &Run) -> Value {
    json!({
        "runId": run.id,
        "localProjectId": run.project_id,
        "title": run.title,
        "state": run.status,
        "step": run.step,
        "stepIndex": run.step_index,
        "steps": run.steps,
        "roles": run.roles,
        "models": run.models,
        "activity": run.activity,
        "costUsd": run.cost_usd,
        "budgetUsd": run.budget_usd,
        "branch": run.branch,
        "pullRequest": run.pull_request,
        "error": run.error,
        "startedAt": rfc3339(run.started_at),
        "finishedAt": run.finished_at.map(rfc3339),
    })
}

fn request_json(request: &PermissionRequest) -> Value {
    json!({
        "runId": request.run_id,
        "requestId": request.id,
        "kind": request.kind,
        "title": request.title,
        "detail": request.detail.chars().take(4_000).collect::<String>(),
        "options": request.options,
        "step": request.step,
    })
}

/// Where messages go. [`Http`] in the daemon; tests plug in their own.
pub trait Sink {
    fn send(&self, message: &Value)
    -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// POSTs to Command's `/v1/protocol/messages` with the node token.
pub struct Http {
    client: reqwest::Client,
    url: String,
    token: String,
    pinned: Option<String>,
}

impl Http {
    pub fn new(config: &NodeConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("agentux-gateway/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            url: config.endpoint(),
            token: config.token.clone(),
            pinned: config.command_instance_id.clone(),
        })
    }
}

impl Sink for Http {
    async fn send(&self, message: &Value) -> Result<(), String> {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(message)
            .send()
            .await
            .map_err(|e| format!("cannot reach Command: {e}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        // A pinned node checks every answer that names the server.
        if let Some(pinned) = &self.pinned {
            let answer: Value = serde_json::from_str(&body).unwrap_or_default();
            let named = answer["result"]["instanceId"]
                .as_str()
                .or_else(|| answer["instanceId"].as_str());
            if let Some(named) = named
                && named != pinned
            {
                return Err(format!(
                    "{INSTANCE_MISMATCH}: {} answers as {named}, this node enrolled with {pinned}",
                    self.url
                ));
            }
        }
        if status.is_success() {
            return Ok(());
        }
        Err(format!("Command answered {status}: {}", body.trim()))
    }
}

/// Messages waiting for Command, oldest first.
#[derive(Default)]
pub struct Outbox {
    queue: VecDeque<Value>,
    pub dropped: u64,
}

impl Outbox {
    pub fn push(&mut self, message: Value) {
        if self.queue.len() >= OUTBOX_MAX {
            self.queue.pop_front();
            self.dropped += 1;
        }
        self.queue.push_back(message);
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Sends in order until one fails; returns how many went.
    pub async fn flush(&mut self, sink: &impl Sink) -> Result<usize, String> {
        let mut sent = 0;
        while let Some(message) = self.queue.front() {
            sink.send(message).await?;
            self.queue.pop_front();
            sent += 1;
        }
        Ok(sent)
    }
}

/// Runs the gateway until `agentuxd` goes away for good or `stop` fires.
/// Reconnects to `agentuxd` and retries Command with backoff.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: NodeConfig,
    socket: PathBuf,
    sink: impl Sink,
    state_path: Option<PathBuf>,
    agentux_version: &str,
    commands: Option<channel::CommandOptions>,
    mut log: impl FnMut(&str),
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<channel::Out>();
    let command_task = commands.map(|options| {
        tokio::spawn(channel::command_loop(
            config.clone(),
            socket.clone(),
            options,
            out_tx.clone(),
        ))
    });
    drop(out_tx);
    let result = run_loop(
        config,
        socket,
        sink,
        state_path,
        agentux_version,
        &mut out_rx,
        &mut log,
        stop,
    )
    .await;
    if let Some(task) = command_task {
        task.abort();
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    config: NodeConfig,
    socket: PathBuf,
    sink: impl Sink,
    state_path: Option<PathBuf>,
    agentux_version: &str,
    out_rx: &mut tokio::sync::mpsc::UnboundedReceiver<channel::Out>,
    log: &mut impl FnMut(&str),
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let mut messages = Messages::new(config, state_path);
    let mut outbox = Outbox::default();
    outbox.push(messages.hello(agentux_version));
    tokio::pin!(stop);

    let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
    let mut harness_poll = tokio::time::interval(HARNESSES_EVERY);
    let mut retry = tokio::time::interval(Duration::from_secs(5));
    let mut last_harnesses: Option<Value> = None;
    let mut reachable = true;

    loop {
        let mut client = match agentux_api::Client::connect(&socket).await {
            Ok(client) => client,
            Err(e) => {
                log(&format!(
                    "agentuxd is not reachable ({e}); trying again in 5 s"
                ));
                tokio::select! {
                    _ = &mut stop => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_secs(5)) => continue,
                }
            }
        };
        let mut events = match agentux_api::Client::connect(&socket).await {
            Ok(c) => c
                .subscribe(&agentux_api::rpc::Subscribe::default())
                .await
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(format!("cannot subscribe to agentuxd: {e}")),
        };
        loop {
            tokio::select! {
                _ = &mut stop => {
                    let _ = outbox.flush(&sink).await;
                    return Ok(());
                }
                _ = heartbeat.tick() => {
                    let runs = client.list_runs().await.unwrap_or_default();
                    let active = runs.iter().filter(|r| !r.status.is_terminal()).count();
                    let waiting = runs.iter().filter(|r| r.status == RunStatus::Waiting).count();
                    outbox.push(messages.heartbeat("online", active, waiting));
                }
                _ = harness_poll.tick() => {
                    if let Ok(harnesses) = client.harnesses().await {
                        let snapshot = serde_json::to_value(&harnesses).ok();
                        if snapshot != last_harnesses {
                            outbox.push(messages.harness_status(&harnesses));
                            last_harnesses = snapshot;
                        }
                    }
                }
                event = events.next() => match event {
                    Ok(Some(event)) => {
                        if let Some(message) = messages.from_event(&event) {
                            outbox.push(message);
                        }
                        if matches!(event.body, EventBody::Limits { .. }) {
                            // Limits change what harness.list shows too.
                            harness_poll.reset_immediately();
                        }
                    }
                    Ok(None) | Err(_) => {
                        log("agentuxd closed the event stream; reconnecting");
                        break;
                    }
                },
                Some(out) = out_rx.recv() => match out {
                    channel::Out::Event(method, payload) => outbox.push(messages.custom_event(&method, payload)),
                    channel::Out::Log(line) => log(&line),
                },
                _ = retry.tick() => {}
            }
            if !outbox.is_empty() {
                match outbox.flush(&sink).await {
                    Ok(_) if !reachable => {
                        reachable = true;
                        log("Command is reachable again");
                    }
                    Ok(_) => {}
                    Err(e) if e.starts_with(INSTANCE_MISMATCH) => {
                        log(&e);
                        return Err(e);
                    }
                    Err(e) if reachable => {
                        reachable = false;
                        log(&format!(
                            "{e}; keeping {} message(s) to send later",
                            outbox.len()
                        ));
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NodeConfig {
        NodeConfig {
            command_url: "https://command.example.org/".into(),
            organization_id: "org-demo".into(),
            node_id: "node-01".into(),
            token: "secret".into(),
            command_instance_id: Some("cmd-0123456789abcdef".into()),
        }
    }

    #[test]
    fn enrollment_tokens_name_their_server() {
        let token = format!("auxe1_cmd-0123456789abcdef_{}", "a".repeat(64));
        assert_eq!(token_instance(&token), Some("cmd-0123456789abcdef"));
        assert_eq!(token_instance("auxn1_abc"), None);
        assert_eq!(token_instance("auxe1_cmd-0123456789abcdef_short"), None);
        assert_eq!(token_instance("a static token"), None);
    }

    #[test]
    fn command_is_reached_over_https_or_locally() {
        assert!(config().validate().is_ok());
        assert_eq!(
            config().endpoint(),
            "https://command.example.org/v1/protocol/messages"
        );
        let with = |url: &str| NodeConfig {
            command_url: url.into(),
            ..config()
        };
        assert!(with("http://localhost:8080").validate().is_ok());
        assert!(with("http://127.0.0.1").validate().is_ok());
        assert!(with("http://command.example.org").validate().is_err());
        assert!(with("http://localhost.evil.org").validate().is_err());
        assert!(
            NodeConfig {
                token: " ".into(),
                ..config()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn config_round_trips_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentux/node.json");
        assert_eq!(load_config(&path).unwrap(), None);
        save_config(&path, &config()).unwrap();
        assert_eq!(load_config(&path).unwrap(), Some(config()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn messages_carry_the_envelope_and_sequences_survive_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("gateway.json");
        let mut messages = Messages::new(config(), Some(state.clone()));
        let hello = messages.hello("0.4.8");
        assert_eq!(hello["method"], "node.hello");
        assert_eq!(hello["params"]["protocolVersion"], "v1");
        assert_eq!(hello["params"]["organizationId"], "org-demo");
        assert_eq!(hello["id"], hello["params"]["messageId"]);
        assert!(hello["params"].get("sequence").is_none());

        let first = messages.limits_changed(&[]);
        let second = messages.limits_changed(&[]);
        assert_eq!(first["params"]["sequence"], 1);
        assert_eq!(second["params"]["sequence"], 2);
        assert_eq!(second["params"]["eventId"], "node-01-2");
        assert!(first.get("id").is_none());

        let mut again = Messages::new(config(), Some(state));
        assert_eq!(again.last_sequence(), 2);
        assert_eq!(again.limits_changed(&[])["params"]["sequence"], 3);
        // The token never goes into a message.
        assert!(!hello.to_string().contains("secret"));
    }

    struct Failing;
    impl Sink for Failing {
        async fn send(&self, _: &Value) -> Result<(), String> {
            Err("down".into())
        }
    }

    #[tokio::test]
    async fn the_outbox_keeps_order_and_a_bound() {
        let mut outbox = Outbox::default();
        for i in 0..OUTBOX_MAX + 2 {
            outbox.push(json!(i));
        }
        assert_eq!(outbox.len(), OUTBOX_MAX);
        assert_eq!(outbox.dropped, 2);
        assert!(outbox.flush(&Failing).await.is_err());
        assert_eq!(outbox.queue.front(), Some(&json!(2)));
    }
}
