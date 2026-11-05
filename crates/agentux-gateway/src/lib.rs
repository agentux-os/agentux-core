//! The AgentUX node gateway: reports this node to AgentUX Command over the
//! AgentUX Protocol (AUXP v1, `agentux-protocol`).
//!
//! This first, read-only phase sends what Command needs to see the node:
//! `node.hello`, a `node.heartbeat` every 30 seconds, the agent CLIs
//! (`harness.status`) when they change and every 5 minutes, usage limits
//! (`harness.limits_changed`), pending approvals (`approval.requested`),
//! run progress (`run.updated`, `run.completed`, `run.failed`) and the safe
//! tool-call lifecycle (`execution.activity`). It reads
//! everything from the local `agentuxd` through its API; it never exposes
//! the daemon's socket, its database or credentials. Commands from Command
//! come with the next phase, behind the node's own policy.
//!
//! Messages are POSTed to `<command>/v1/protocol/messages` with the node's
//! bearer token. Events get a sequence number that survives restarts
//! ([`State`]); the optional replay capability retains them until Command
//! acknowledges its contiguous durable cursor.
//!
//! Commands from Command come over a WebSocket the node opens
//! ([`channel`]); the node's [`policy`] runs, holds or refuses each, and
//! [`commands`] runs it against agentuxd.

pub mod channel;
pub mod commands;
pub mod policy;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, io};

use agentux_api::rpc::HarnessInfo;
use agentux_api::{
    Event, EventBody, HarnessLimit, PermissionRequest, RequestStatus, Run, RunOrigin, RunStatus,
    SessionEvent, SessionState, ToolStatus,
};
use auxp::activity::{ACTIVITY_SCHEMA_VERSION, Activity, ActivityKind};
use auxp::replay::{ReplayResult, ReplayStatus};
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
    "execution.activity.v1",
    "node.event_replay.v1",
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

/// What survives a restart: event cursors, identity context, and messages not
/// yet accepted by Command. Keeping the queue here makes delivery retries
/// reuse the same event IDs and payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub last_sequence: i64,
    #[serde(default)]
    pub last_local_event_sequence: i64,
    #[serde(default)]
    pub pending_outbox: VecDeque<Value>,
    #[serde(default)]
    pub accepted_capabilities: Vec<String>,
    #[serde(default)]
    pub acknowledged_sequence: i64,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionContext>,
    #[serde(default)]
    pub run_origins: BTreeMap<String, RunOrigin>,
    #[serde(default)]
    pub snapshot_bindings: BTreeMap<String, SnapshotBinding>,
}

/// Durable sequence binding for a completed local capture. It is data-layer
/// groundwork only; no AUXP checkpoint is served while coverage is incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBinding {
    pub snapshot_id: String,
    pub captured_local_event_head: i64,
    pub high_water_sequence: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContext {
    pub run_id: String,
    pub role_id: String,
    #[serde(default)]
    pub active_tool_kinds: BTreeMap<String, agentux_api::ToolKind>,
}

impl State {
    pub fn load(path: &Path) -> io::Result<State> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_vec(self).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&text)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
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
    pub fn new(config: NodeConfig, state_path: Option<PathBuf>) -> Result<Self, String> {
        let state = state_path
            .as_deref()
            .map(State::load)
            .transpose()
            .map_err(|error| format!("cannot load gateway state: {error}"))?
            .unwrap_or_default();
        Ok(Self {
            config,
            state,
            state_path,
            counter: 0,
        })
    }

    pub fn last_sequence(&self) -> i64 {
        self.state.last_sequence
    }

    /// Bind a captured SQLite head only after the serial gateway event loop
    /// has sequenced every local event through that head.
    pub fn bind_snapshot(
        &mut self,
        checkpoint_id: &str,
        snapshot_id: &str,
        local_event_head: i64,
    ) -> Result<SnapshotBinding, String> {
        if checkpoint_id.is_empty() || snapshot_id.is_empty() || local_event_head < 0 {
            return Err("snapshot binding identifiers and event head are invalid".into());
        }
        if let Some(existing) = self.state.snapshot_bindings.get(checkpoint_id) {
            if existing.snapshot_id == snapshot_id
                && existing.captured_local_event_head == local_event_head
            {
                return Ok(existing.clone());
            }
            return Err("checkpoint id already has a different sequence binding".into());
        }
        if self.state.last_local_event_sequence != local_event_head {
            return Err(format!(
                "snapshot capture head {local_event_head} is not caught up (gateway has {})",
                self.state.last_local_event_sequence
            ));
        }
        let binding = SnapshotBinding {
            snapshot_id: snapshot_id.into(),
            captured_local_event_head: local_event_head,
            high_water_sequence: self.state.last_sequence,
        };
        self.state
            .snapshot_bindings
            .insert(checkpoint_id.into(), binding.clone());
        self.persist()?;
        Ok(binding)
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
                if run.status.is_terminal() {
                    self.state.run_origins.remove(&run.id);
                    self.state
                        .sessions
                        .retain(|_, session| session.run_id != run.id);
                } else if let Some(origin) = &run.origin {
                    self.state
                        .run_origins
                        .insert(run.id.clone(), origin.clone());
                }
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
            EventBody::Session { session } => {
                if session.state == SessionState::Ended {
                    self.state.sessions.remove(&session.id);
                } else {
                    // A session event comes with every state or usage change,
                    // also mid-call: keep the kinds of the calls in flight.
                    self.state
                        .sessions
                        .entry(session.id.clone())
                        .and_modify(|context| {
                            context.run_id.clone_from(&session.run_id);
                            context.role_id.clone_from(&session.role);
                        })
                        .or_insert_with(|| SessionContext {
                            run_id: session.run_id.clone(),
                            role_id: session.role.clone(),
                            active_tool_kinds: BTreeMap::new(),
                        });
                }
                None
            }
            EventBody::SessionEvent {
                session_id,
                event: session_event,
            } => self.activity_from_session_event(event.seq, session_id, session_event),
            _ => None,
        }
    }

    fn activity_from_session_event(
        &mut self,
        sequence: i64,
        session_id: &str,
        event: &SessionEvent,
    ) -> Option<Value> {
        if !self
            .state
            .accepted_capabilities
            .iter()
            .any(|capability| capability == "execution.activity.v1")
        {
            return None;
        }
        let run_id = {
            let context = self.state.sessions.get(session_id)?;
            context.run_id.clone()
        };
        let origin = self.state.run_origins.get(&run_id)?;
        let SessionEvent::ToolCall {
            tool_call_id,
            tool,
            status,
            ..
        } = event
        else {
            return None;
        };
        let context = self.state.sessions.get_mut(session_id)?;
        if let Some(tool) = tool {
            context
                .active_tool_kinds
                .insert(tool_call_id.clone(), *tool);
        }
        let status = status.as_ref()?;
        let tool_kind = context
            .active_tool_kinds
            .get(tool_call_id)
            .copied()
            .unwrap_or(agentux_api::ToolKind::Other);
        if matches!(status, ToolStatus::Ok | ToolStatus::Error) {
            context.active_tool_kinds.remove(tool_call_id);
        }
        let tool_name = tool_kind.as_str().to_string();
        let summary = match status {
            ToolStatus::Running => format!("Agent started a {tool_name} operation"),
            ToolStatus::Ok => format!("Agent completed a {tool_name} operation"),
            ToolStatus::Error => format!("Agent failed a {tool_name} operation"),
        };
        let kind = match status {
            ToolStatus::Running => ActivityKind::ToolStarted {
                tool_name: tool_name.clone(),
                summary: summary.clone(),
            },
            ToolStatus::Ok => ActivityKind::ToolCompleted {
                tool_name: tool_name.clone(),
                summary: summary.clone(),
            },
            ToolStatus::Error => ActivityKind::ToolFailed {
                tool_name: tool_name.clone(),
                summary: summary.clone(),
            },
        };
        let activity = Activity {
            schema_version: ACTIVITY_SCHEMA_VERSION.into(),
            activity_id: activity_id(&self.config.node_id, sequence),
            mission_id: origin.mission_id.clone(),
            run_id: origin.command_run_id.clone(),
            agent_id: session_id.to_string(),
            role_id: context.role_id.clone(),
            session_id: Some(session_id.to_string()),
            task_id: None,
            parent_task_id: None,
            attempt: None,
            causation_id: None,
            delegation_id: None,
            tool_call_id: Some(tool_call_id.clone()),
            kind,
        };
        if activity.validate().is_err() {
            return None;
        }
        let value = serde_json::to_value(activity).ok()?;
        Some(self.event("execution.activity", json!({ "activity": value })))
    }

    fn commit_local_event(&mut self, seq: i64, outbox: &Outbox) -> Result<(), String> {
        self.state.last_local_event_sequence = seq;
        self.state.pending_outbox = outbox.queue.clone();
        self.persist()
    }

    fn commit_outbox(&mut self, outbox: &Outbox) -> Result<(), String> {
        self.state.pending_outbox = outbox.queue.clone();
        self.persist()
    }

    fn accept_capabilities(&mut self, capabilities: Vec<String>) -> Result<(), String> {
        self.state.accepted_capabilities = capabilities
            .into_iter()
            .filter(|capability| CAPABILITIES.contains(&capability.as_str()))
            .collect();
        self.persist()
    }

    fn commit_acknowledgement(&mut self, through: i64, outbox: &Outbox) -> Result<(), String> {
        self.state.acknowledged_sequence = self.state.acknowledged_sequence.max(through);
        self.state.pending_outbox = outbox.queue.clone();
        self.persist()
    }

    fn replay_result(&self, after: i64, outbox: &mut Outbox) -> Result<ReplayResult, String> {
        let high_water = self.state.last_sequence;
        if after < 0 || after > high_water {
            return Err(format!("replay cursor {after} is outside 0..={high_water}"));
        }
        let queued_sequences: Vec<i64> = outbox
            .queue
            .iter()
            .filter_map(|message| message["params"]["sequence"].as_i64())
            .collect();
        let oldest_queued = queued_sequences.iter().copied().min();
        let oldest_available = self
            .state
            .acknowledged_sequence
            .saturating_add(1)
            .max(oldest_queued.unwrap_or(high_water.saturating_add(1)));
        let expired = after.saturating_add(1) < oldest_available;
        let replayed = if expired {
            0
        } else {
            outbox.replay_after(after).len() as i64
        };
        let result = ReplayResult {
            status: if expired {
                ReplayStatus::CursorExpired
            } else {
                ReplayStatus::Replay
            },
            node_id: self.config.node_id.clone(),
            requested_after_sequence: after,
            oldest_available_sequence: oldest_available,
            high_water_sequence: high_water,
            events_replayed: replayed,
            snapshot_required: expired,
        };
        result.validate()?;
        Ok(result)
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(path) = &self.state_path {
            self.state
                .save(path)
                .map_err(|e| format!("cannot persist gateway state: {e}"))?;
        }
        Ok(())
    }

    fn replay_cursor(&self) -> i64 {
        self.state.last_local_event_sequence
    }

    fn replay_enabled(&self) -> bool {
        self.state
            .accepted_capabilities
            .iter()
            .any(|capability| capability == "node.event_replay.v1")
    }

    fn pending_outbox(&self) -> Outbox {
        Outbox {
            queue: self.state.pending_outbox.clone(),
            dropped: 0,
            sent_event_ids: HashSet::new(),
        }
    }
}

fn activity_id(node_id: &str, local_sequence: i64) -> String {
    let suffix = format!("-event-{local_sequence}");
    let prefix_limit = 128usize.saturating_sub(suffix.chars().count());
    let prefix = node_id.chars().take(prefix_limit).collect::<String>();
    format!("{prefix}{suffix}")
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
    let mut value = json!({
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
    });
    // So Command can see runs whose agents ran unconfined (ADR 0023).
    if let Some(isolation) = &run.agent_isolation {
        value["agentIsolation"] = json!(isolation);
    }
    // A run Command started for a mission: correlated with Command's run,
    // so Office follows the work across both (ADR 0015).
    if let Some(origin) = &run.origin {
        value["missionId"] = json!(origin.mission_id);
        value["correlationId"] = json!(origin.command_run_id);
        if let Some(version) = &origin.definition_version {
            value["definitionVersion"] = json!(version);
        }
        value["origin"] = json!(origin);
    }
    value
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
    fn send(
        &self,
        message: &Value,
    ) -> impl std::future::Future<Output = Result<Option<Value>, String>> + Send;
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
    async fn send(&self, message: &Value) -> Result<Option<Value>, String> {
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
            return Ok(serde_json::from_str(&body).ok());
        }
        Err(format!("Command answered {status}: {}", body.trim()))
    }
}

/// Messages waiting for Command, oldest first.
#[derive(Default)]
pub struct Outbox {
    queue: VecDeque<Value>,
    pub dropped: u64,
    sent_event_ids: HashSet<String>,
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

    /// Sends in order until one fails. When replay is negotiated, sequenced
    /// events remain queued until Command acknowledges their durable cursor.
    pub async fn flush(
        &mut self,
        sink: &impl Sink,
        retain_events: bool,
    ) -> Result<(usize, Option<Vec<String>>), String> {
        let mut sent = 0;
        let mut accepted_capabilities = None;
        let mut index = 0;
        while index < self.queue.len() {
            let message = self.queue[index].clone();
            let event_id = message["params"]["eventId"].as_str();
            if retain_events && event_id.is_some_and(|id| self.sent_event_ids.contains(id)) {
                index += 1;
                continue;
            }
            let response = sink.send(&message).await?;
            if message["method"] == "node.hello" {
                accepted_capabilities = Some(
                    response
                        .as_ref()
                        .and_then(|value| value["result"]["acceptedCapabilities"].as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
            if let Some(event_id) = event_id.filter(|_| retain_events) {
                self.sent_event_ids.insert(event_id.to_string());
                index += 1;
            } else {
                if let Some(event_id) = event_id {
                    self.sent_event_ids.remove(event_id);
                }
                self.queue.remove(index);
            }
            sent += 1;
        }
        Ok((sent, accepted_capabilities))
    }

    fn acknowledge_through(&mut self, through: i64) {
        let sent_event_ids = &mut self.sent_event_ids;
        self.queue.retain(|message| {
            let sequence = message["params"]["sequence"].as_i64();
            if sequence.is_some_and(|sequence| sequence <= through) {
                if let Some(event_id) = message["params"]["eventId"].as_str() {
                    sent_event_ids.remove(event_id);
                }
                false
            } else {
                true
            }
        });
    }

    fn replay_after(&mut self, after: i64) -> Vec<Value> {
        let mut replay = Vec::new();
        let sent_event_ids = &mut self.sent_event_ids;
        for message in &self.queue {
            let sequence = message["params"]["sequence"].as_i64().unwrap_or_default();
            if sequence > after {
                replay.push(message.clone());
                if let Some(event_id) = message["params"]["eventId"].as_str() {
                    sent_event_ids.remove(event_id);
                }
            }
        }
        replay.sort_by_key(|message| message["params"]["sequence"].as_i64().unwrap_or_default());
        replay
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
    let mut messages = Messages::new(config, state_path)?;
    let mut outbox = messages.pending_outbox();
    outbox.push(messages.hello(agentux_version));
    messages.commit_outbox(&outbox)?;
    if let Ok((_, Some(capabilities))) = outbox.flush(&sink, messages.replay_enabled()).await {
        messages.accept_capabilities(capabilities)?;
    }
    messages.commit_outbox(&outbox)?;
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
                    Some(out) = out_rx.recv() => {
                        handle_gateway_out(out, &mut messages, &mut outbox, log)?;
                        continue;
                    }
                }
            }
        };
        let mut events = match agentux_api::Client::connect(&socket).await {
            Ok(c) => c
                .subscribe(&agentux_api::rpc::Subscribe {
                    since: Some(messages.replay_cursor()),
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(format!("cannot subscribe to agentuxd: {e}")),
        };
        loop {
            tokio::select! {
                _ = &mut stop => {
                    let _ = outbox.flush(&sink, messages.replay_enabled()).await;
                    let _ = messages.commit_outbox(&outbox);
                    return Ok(());
                }
                _ = heartbeat.tick() => {
                    let runs = client.list_runs().await.unwrap_or_default();
                    let active = runs.iter().filter(|r| !r.status.is_terminal()).count();
                    let waiting = runs.iter().filter(|r| r.status == RunStatus::Waiting).count();
                    outbox.push(messages.heartbeat("online", active, waiting));
                    messages.commit_outbox(&outbox)?;
                }
                _ = harness_poll.tick() => {
                    if let Ok(harnesses) = client.harnesses().await {
                        let snapshot = serde_json::to_value(&harnesses).ok();
                        if snapshot != last_harnesses {
                            outbox.push(messages.harness_status(&harnesses));
                            messages.commit_outbox(&outbox)?;
                            last_harnesses = snapshot;
                        }
                    }
                }
                event = events.next() => match event {
                    Ok(Some(event)) => {
                        if let Some(message) = messages.from_event(&event) {
                            outbox.push(message);
                        }
                        messages.commit_local_event(event.seq, &outbox)?;
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
                Some(out) = out_rx.recv() => handle_gateway_out(out, &mut messages, &mut outbox, log)?,
                _ = retry.tick() => {}
            }
            if !outbox.is_empty() {
                match outbox.flush(&sink, messages.replay_enabled()).await {
                    Ok((_, accepted)) => {
                        if let Some(accepted) = accepted {
                            messages.accept_capabilities(accepted)?;
                        }
                        messages.commit_outbox(&outbox)?;
                        if !reachable {
                            reachable = true;
                            log("Command is reachable again");
                        }
                    }
                    Err(e) if e.starts_with(INSTANCE_MISMATCH) => {
                        messages.commit_outbox(&outbox)?;
                        log(&e);
                        return Err(e);
                    }
                    Err(e) if reachable => {
                        messages.commit_outbox(&outbox)?;
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

fn handle_gateway_out(
    out: channel::Out,
    messages: &mut Messages,
    outbox: &mut Outbox,
    log: &mut impl FnMut(&str),
) -> Result<(), String> {
    match out {
        channel::Out::Event(method, payload) => {
            outbox.push(messages.custom_event(&method, payload));
            messages.commit_outbox(outbox)?;
        }
        channel::Out::Log(line) => log(&line),
        channel::Out::Resume { after, id, reply } => {
            let response = if !messages.replay_enabled() {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "node.event_replay.v1 was not negotiated" } })
            } else {
                if (0..=messages.last_sequence()).contains(&after) {
                    outbox.acknowledge_through(after);
                    messages.commit_acknowledgement(after, outbox)?;
                }
                match messages
                    .replay_result(after, outbox)
                    .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string()))
                {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(error) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": error } })
                    }
                }
            };
            let _ = reply.send(response);
        }
        channel::Out::Acknowledge { through, id, reply } => {
            let response = if !messages.replay_enabled() {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "node.event_replay.v1 was not negotiated" } })
            } else if through > messages.last_sequence() {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": "acknowledgement exceeds the highest emitted sequence" } })
            } else {
                outbox.acknowledge_through(through);
                messages.commit_acknowledgement(through, outbox)?;
                json!({ "jsonrpc": "2.0", "id": id, "result": { "throughSequence": through } })
            };
            let _ = reply.send(response);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_checkpoint_is_not_advertised_without_complete_sources() {
        assert!(!CAPABILITIES.contains(&"node.snapshot_checkpoint.v1"));
    }

    #[test]
    fn snapshot_binding_requires_the_local_event_stream_to_reach_the_capture_head() {
        let mut messages = Messages::new(config(), None).unwrap();
        assert!(messages.bind_snapshot("cp", "snap", 3).is_err());
        for seq in 1..=3 {
            let outbox = messages.pending_outbox();
            messages.commit_local_event(seq, &outbox).unwrap();
        }
        let binding = messages.bind_snapshot("cp", "snap", 3).unwrap();
        assert_eq!(binding.high_water_sequence, 0);
        assert_eq!(messages.bind_snapshot("cp", "snap", 3).unwrap(), binding);
        assert!(messages.bind_snapshot("cp", "other", 3).is_err());
        let after = messages.custom_event("run.updated", json!({"runId":"r1"}));
        assert_eq!(after["params"]["sequence"], 1);
        assert_eq!(messages.bind_snapshot("cp", "snap", 3).unwrap(), binding);
    }

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
    fn a_mission_runs_events_are_correlated_with_command() {
        let mut run: Run = serde_json::from_value(serde_json::json!({
            "id": "local-7", "projectId": "p", "title": "Ship", "prompt": null, "issue": null,
            "branch": null, "worktree": null, "steps": ["plan"], "stepIndex": 0, "step": "plan",
            "status": "running", "roles": {}, "checks": [], "gateAttempt": 0, "gateMaxAttempts": 0,
            "reviewRound": 0, "reviewMaxRounds": 0, "budgetUsd": null, "startedAt": 1, "updatedAt": 1,
            "finishedAt": null, "pullRequest": null, "activity": "", "error": null
        }))
        .unwrap();
        let plain = run_json(&run);
        assert!(
            plain.get("correlationId").is_none(),
            "a run of its own: nothing to correlate"
        );
        run.origin = Some(agentux_api::RunOrigin {
            organization_id: "acme".into(),
            mission_id: "ship".into(),
            command_run_id: "run-1a2b".into(),
            definition_version: Some("definition-3".into()),
        });
        let json = run_json(&run);
        assert_eq!(
            json["runId"], "local-7",
            "the node's own id stays the runId"
        );
        assert_eq!(json["correlationId"], "run-1a2b");
        assert_eq!(json["missionId"], "ship");
        assert_eq!(json["definitionVersion"], "definition-3");
        assert_eq!(json["origin"]["commandRunId"], "run-1a2b");
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
        let mut messages = Messages::new(config(), Some(state.clone())).unwrap();
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
        let mut outbox = Outbox::default();
        outbox.push(first);
        outbox.push(second);
        messages.commit_outbox(&outbox).unwrap();

        let mut again = Messages::new(config(), Some(state)).unwrap();
        assert_eq!(again.last_sequence(), 2);
        assert_eq!(again.limits_changed(&[])["params"]["sequence"], 3);
        // The token never goes into a message.
        assert!(!hello.to_string().contains("secret"));
    }

    #[test]
    fn acp_tool_lifecycle_fixtures_emit_ordered_safe_replayable_activity() {
        let temp = tempfile::tempdir().unwrap();
        let state_path = temp.path().join("gateway.json");
        let mut messages = Messages::new(config(), Some(state_path.clone())).unwrap();
        messages
            .state
            .accepted_capabilities
            .push("execution.activity.v1".into());
        messages.state.sessions.insert(
            "session-1".into(),
            SessionContext {
                run_id: "local-run".into(),
                role_id: "implementer".into(),
                active_tool_kinds: BTreeMap::new(),
            },
        );
        messages.state.run_origins.insert(
            "local-run".into(),
            RunOrigin {
                organization_id: "org-demo".into(),
                mission_id: "mission-1".into(),
                command_run_id: "command-run-1".into(),
                definition_version: None,
            },
        );
        let fixtures: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/acp-tool-lifecycle.json"))
                .unwrap();
        let mut emitted = Vec::new();
        let mut outbox = Outbox::default();
        for fixture in fixtures {
            let event: Event = serde_json::from_value(fixture["event"].clone()).unwrap();
            let message = messages.from_event(&event).unwrap();
            let activity = &message["params"]["activity"];
            assert_eq!(message["method"], "execution.activity");
            assert_eq!(message["params"]["sequence"], emitted.len() + 1);
            assert_eq!(activity["schemaVersion"], "v1");
            assert_eq!(
                activity["activityId"],
                format!("node-01-event-{}", event.seq)
            );
            assert_eq!(activity["missionId"], "mission-1");
            assert_eq!(activity["runId"], "command-run-1");
            assert_eq!(activity["roleId"], "implementer");
            assert_eq!(activity["type"], fixture["expectedType"]);
            assert_eq!(activity["toolName"], fixture["expectedToolName"]);
            assert_eq!(activity["toolCallId"], fixture["expectedToolCallId"]);
            assert!(!message.to_string().contains("PRIVATE"));
            assert!(!message.to_string().contains("fixture-secret"));
            auxp::validate(&message).unwrap();
            emitted.push(message.clone());
            outbox.push(message);
            messages.commit_local_event(event.seq, &outbox).unwrap();
            // A daemon restart between ACP updates must retain the tool kind
            // associated with each still-active call ID.
            messages = Messages::new(config(), Some(state_path.clone())).unwrap();
        }
        assert_eq!(
            emitted
                .iter()
                .map(|message| message["params"]["activity"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "tool_started",
                "tool_completed",
                "tool_started",
                "tool_failed"
            ]
        );
        assert_eq!(
            emitted
                .iter()
                .map(|message| message["params"]["sequence"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let restored = Messages::new(config(), Some(state_path)).unwrap();
        assert_eq!(restored.pending_outbox().replay_after(0), emitted);
    }

    #[test]
    fn a_session_update_mid_call_keeps_the_calls_tool_kind() {
        let mut messages = Messages::new(config(), None).unwrap();
        messages
            .state
            .accepted_capabilities
            .push("execution.activity.v1".into());
        messages.state.run_origins.insert(
            "local-run".into(),
            RunOrigin {
                organization_id: "org-demo".into(),
                mission_id: "mission-1".into(),
                command_run_id: "command-run-1".into(),
                definition_version: None,
            },
        );
        let session = |seq: i64, state: &str| -> Event {
            serde_json::from_value(json!({
                "seq": seq, "at": 1_794_000_000_000_i64, "runId": "local-run", "kind": "session",
                "session": {
                    "id": "session-1", "runId": "local-run", "projectId": "p", "role": "implementer",
                    "harness": "codex", "state": state, "cwd": "/w",
                    "usage": { "usedTokens": seq, "contextTokens": 1000 },
                    "startedAt": 1, "updatedAt": seq
                }
            }))
            .unwrap()
        };
        let tool = |seq: i64, kind: Option<&str>, status: &str| -> Event {
            let mut call = json!({ "kind": "tool_call", "toolCallId": "tool-9", "status": status });
            if let Some(kind) = kind {
                call["tool"] = json!(kind);
            }
            serde_json::from_value(json!({
                "seq": seq, "at": 1_794_000_000_000_i64, "runId": "local-run",
                "kind": "session_event", "sessionId": "session-1", "event": call
            }))
            .unwrap()
        };
        assert!(messages.from_event(&session(1, "active")).is_none());
        let started = messages
            .from_event(&tool(2, Some("execute"), "running"))
            .unwrap();
        assert_eq!(started["params"]["activity"]["toolName"], "execute");
        // Usage and state changes during the call.
        assert!(messages.from_event(&session(3, "waiting")).is_none());
        assert!(messages.from_event(&session(4, "active")).is_none());
        let done = messages.from_event(&tool(5, None, "ok")).unwrap();
        assert_eq!(done["params"]["activity"]["type"], "tool_completed");
        assert_eq!(done["params"]["activity"]["toolName"], "execute");
        assert!(
            messages.state.sessions["session-1"]
                .active_tool_kinds
                .is_empty()
        );
        // An ended session takes its context with it.
        assert!(messages.from_event(&session(6, "ended")).is_none());
        assert!(!messages.state.sessions.contains_key("session-1"));
    }

    #[test]
    fn execution_activity_is_suppressed_until_the_capability_is_accepted() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/acp-tool-lifecycle.json"))
                .unwrap();
        let event: Event = serde_json::from_value(fixture[0]["event"].clone()).unwrap();
        let mut messages = Messages::new(config(), None).unwrap();

        assert!(messages.from_event(&event).is_none());
        assert_eq!(messages.last_sequence(), 0);
    }

    #[test]
    fn session_plans_and_other_non_tool_events_do_not_become_activity() {
        let mut messages = Messages::new(config(), None).unwrap();
        messages
            .state
            .accepted_capabilities
            .push("execution.activity.v1".into());
        messages.state.sessions.insert(
            "session-1".into(),
            SessionContext {
                run_id: "local-run".into(),
                role_id: "implementer".into(),
                active_tool_kinds: BTreeMap::new(),
            },
        );
        messages.state.run_origins.insert(
            "local-run".into(),
            RunOrigin {
                organization_id: "org-demo".into(),
                mission_id: "mission-1".into(),
                command_run_id: "command-run-1".into(),
                definition_version: None,
            },
        );
        for (seq, session_event) in [
            (
                50,
                SessionEvent::Plan {
                    items: vec![agentux_api::PlanItem {
                        text: "PRIVATE plan text".into(),
                        status: agentux_api::PlanItemStatus::InProgress,
                    }],
                },
            ),
            (
                51,
                SessionEvent::Diff {
                    tool_call_id: "tool-9".into(),
                    path: "private.txt".into(),
                    old_text: None,
                    new_text: "PRIVATE file contents".into(),
                },
            ),
            (
                52,
                SessionEvent::Message {
                    from: agentux_api::MessageFrom::Agent,
                    text: "PRIVATE response".into(),
                },
            ),
            (
                53,
                SessionEvent::Usage {
                    usage: agentux_api::SessionUsage {
                        used_tokens: 1,
                        context_tokens: 2,
                        cost_usd: None,
                    },
                },
            ),
            (
                54,
                SessionEvent::Permission {
                    request_id: "request-1".into(),
                },
            ),
        ] {
            let event = Event {
                seq,
                at: 1_794_000_000_000,
                run_id: Some("local-run".into()),
                body: EventBody::SessionEvent {
                    session_id: "session-1".into(),
                    event: session_event,
                },
            };
            assert!(messages.from_event(&event).is_none());
        }
        assert_eq!(messages.last_sequence(), 0);
    }

    #[test]
    fn local_event_cursor_and_outbox_survive_restart_together() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("gateway.json");
        let mut messages = Messages::new(config(), Some(state_path.clone())).unwrap();
        let mut outbox = Outbox::default();
        outbox.push(messages.limits_changed(&[]));
        messages.commit_local_event(17, &outbox).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let restored = Messages::new(config(), Some(state_path)).unwrap();
        assert_eq!(restored.replay_cursor(), 17);
        let restored_outbox = restored.pending_outbox();
        assert_eq!(restored_outbox.len(), 1);
        assert_eq!(restored_outbox.queue[0]["params"]["eventId"], "node-01-1");
    }

    #[test]
    fn unreadable_gateway_state_fails_closed_instead_of_reusing_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("gateway.json");
        fs::write(&state_path, "{corrupt").unwrap();
        assert!(Messages::new(config(), Some(state_path)).is_err());
    }

    #[test]
    fn replay_is_contiguous_and_acknowledgement_retires_only_committed_prefix() {
        let mut messages = Messages::new(config(), None).unwrap();
        let mut outbox = Outbox::default();
        for _ in 0..3 {
            outbox.push(messages.limits_changed(&[]));
        }
        messages.commit_outbox(&outbox).unwrap();

        let replay = messages.replay_result(0, &mut outbox).unwrap();
        assert_eq!(replay.status, ReplayStatus::Replay);
        assert_eq!(replay.events_replayed, 3);

        outbox.acknowledge_through(1);
        messages.commit_acknowledgement(1, &outbox).unwrap();
        let replay = messages.replay_result(1, &mut outbox).unwrap();
        assert_eq!(replay.events_replayed, 2);

        let expired = messages.replay_result(0, &mut outbox).unwrap();
        assert_eq!(expired.status, ReplayStatus::CursorExpired);
        assert!(expired.snapshot_required);
        assert_eq!(expired.events_replayed, 0);
    }

    struct Failing;
    impl Sink for Failing {
        async fn send(&self, _: &Value) -> Result<Option<Value>, String> {
            Err("down".into())
        }
    }

    struct Accepting;
    impl Sink for Accepting {
        async fn send(&self, _: &Value) -> Result<Option<Value>, String> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn replay_mode_keeps_sent_events_until_ack_and_reuses_the_same_message() {
        let mut messages = Messages::new(config(), None).unwrap();
        let message = messages.limits_changed(&[]);
        let mut outbox = Outbox::default();
        outbox.push(message.clone());

        assert_eq!(outbox.flush(&Accepting, true).await.unwrap().0, 1);
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox.flush(&Accepting, true).await.unwrap().0, 0);
        let replay = outbox.replay_after(0);
        assert_eq!(replay, vec![message.clone()]);
        assert_eq!(outbox.flush(&Accepting, true).await.unwrap().0, 1);
        assert_eq!(outbox.queue.front(), Some(&message));

        outbox.acknowledge_through(1);
        assert!(outbox.is_empty());

        outbox.push(messages.limits_changed(&[]));
        assert_eq!(outbox.flush(&Accepting, true).await.unwrap().0, 1);
        assert_eq!(outbox.flush(&Accepting, false).await.unwrap().0, 1);
        assert!(outbox.is_empty());
    }

    #[tokio::test]
    async fn the_outbox_keeps_order_and_a_bound() {
        let mut outbox = Outbox::default();
        for i in 0..OUTBOX_MAX + 2 {
            outbox.push(json!(i));
        }
        assert_eq!(outbox.len(), OUTBOX_MAX);
        assert_eq!(outbox.dropped, 2);
        assert!(outbox.flush(&Failing, true).await.is_err());
        assert_eq!(outbox.queue.front(), Some(&json!(2)));
    }
}
