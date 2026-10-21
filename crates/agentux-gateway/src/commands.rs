//! Running AUXP commands from Command on this node: each one through the
//! local agentuxd API, after the node's [`Policy`]. Commands held for
//! confirmation wait in [`Pending`] until someone at the node accepts or
//! rejects them (`aux node confirm`, the cockpit's Settings).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::{env, fs};

use agentux_api::rpc::{self, BusPost, ClearLimit, StartRun, TestHarness};
use agentux_api::{BusEndpoint, Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::policy::{Decision, Policy};

/// JSON-RPC error codes of the AUXP command channel.
pub const NOT_IMPLEMENTED: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const HELD: i64 = -32010;
pub const DENIED: i64 = -32011;
pub const FAILED: i64 = -32012;

/// A command's error: code and message.
pub type Failure = (i64, String);

fn param<'a>(params: &'a Value, name: &str) -> Result<&'a str, Failure> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| (INVALID_PARAMS, format!("`{name}` is required")))
}

fn failed(e: impl std::fmt::Display) -> Failure {
    (FAILED, e.to_string())
}

fn value<T: Serialize>(v: T) -> Result<Value, Failure> {
    serde_json::to_value(v).map_err(failed)
}

/// Runs one command against agentuxd. No policy here: see [`handle`].
pub async fn execute(client: &mut Client, method: &str, params: &Value) -> Result<Value, Failure> {
    match method {
        "run.snapshot" => value(
            client
                .get_run(param(params, "runId")?)
                .await
                .map_err(failed)?,
        ),
        "run.cancel" => value(
            client
                .cancel_run(param(params, "runId")?)
                .await
                .map_err(failed)?,
        ),
        "run.pause" | "run.resume" => {
            Err((NOT_IMPLEMENTED, "this node cannot pause runs yet".into()))
        }
        "request.resolve" => {
            let id = param(params, "requestId")?;
            let answer = params
                .get("answer")
                .and_then(Value::as_str)
                .map(str::to_string);
            let request = match param(params, "resolution")? {
                "approve" | "approved" | "allow" | "answer" => client.approve(id, answer).await,
                "deny" | "denied" | "reject" | "decline" => client.deny(id, answer).await,
                other => return Err((INVALID_PARAMS, format!("unknown resolution `{other}`"))),
            };
            value(request.map_err(failed)?)
        }
        "bus.post" => {
            let message = params
                .get("message")
                .ok_or((INVALID_PARAMS, "`message` is required".to_string()))?;
            let (body, to, subject) = match message {
                Value::String(body) => (body.clone(), None, None),
                _ => (
                    message
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    message.get("to").cloned(),
                    message
                        .get("subject")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                ),
            };
            if body.trim().is_empty() {
                return Err((INVALID_PARAMS, "the message is empty".into()));
            }
            let to: Option<BusEndpoint> = match to {
                Some(to) => Some(
                    serde_json::from_value(to)
                        .map_err(|e| (INVALID_PARAMS, format!("`message.to`: {e}")))?,
                ),
                None => Some(BusEndpoint::Run),
            };
            value(
                client
                    .post_bus(&BusPost {
                        run_id: param(params, "runId")?.to_string(),
                        to,
                        body,
                        subject,
                        in_reply_to: None,
                    })
                    .await
                    .map_err(failed)?,
            )
        }
        "harness.list" => value(client.harnesses().await.map_err(failed)?),
        "harness.test" => value(
            client
                .test_harness(&TestHarness {
                    harness: param(params, "harness")?.to_string(),
                    model: params
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .await
                .map_err(failed)?,
        ),
        "limits.clear" => value(
            client
                .clear_limit(&ClearLimit {
                    harness: param(params, "harness")?.to_string(),
                    model: params
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .await
                .map_err(failed)?,
        ),
        "preferences.set" => {
            let prefs: rpc::AgentPrefs = serde_json::from_value(
                params
                    .get("preferences")
                    .cloned()
                    .ok_or((INVALID_PARAMS, "`preferences` is required".to_string()))?,
            )
            .map_err(|e| (INVALID_PARAMS, format!("`preferences`: {e}")))?;
            let saved: rpc::AgentPrefs = client
                .call(rpc::method::PREFERENCES_SET, &prefs)
                .await
                .map_err(failed)?;
            value(saved)
        }
        "run.start" => start_run(client, params).await,
        other => Err((NOT_IMPLEMENTED, format!("this node does not run `{other}`"))),
    }
}

/// `run.start`: the project the node has for `repository` (a registered
/// project whose origin is that URL), or a fresh clone into ~/src.
async fn start_run(client: &mut Client, params: &Value) -> Result<Value, Failure> {
    let repository = param(params, "repository")?;
    let prompt = param(params, "prompt")?;
    let path = match local_project(client, repository).await? {
        Some(path) => path,
        None => clone(repository).await?,
    };
    let project = client
        .register_project(&path.to_string_lossy())
        .await
        .map_err(failed)?;
    let map = |name: &str| -> BTreeMap<String, String> {
        params
            .get(name)
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let run = client
        .start_run(&StartRun {
            project_id: project.id,
            prompt: Some(prompt.to_string()),
            title: params
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            roles: map("agents"),
            models: map("models"),
            ..Default::default()
        })
        .await
        .map_err(failed)?;
    value(json!({ "localRunId": run.id, "run": run }))
}

/// host/owner/repo of a remote URL, the same for its https and SSH forms.
fn repo_key(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    let url = url.split_once('@').map_or(url, |(_, rest)| rest);
    url.replacen(':', "/", 1).to_lowercase()
}

async fn local_project(client: &mut Client, repository: &str) -> Result<Option<PathBuf>, Failure> {
    let want = repo_key(repository);
    for project in client.list_projects().await.map_err(failed)? {
        let output = tokio::process::Command::new("git")
            .args(["-C", &project.path, "remote", "get-url", "origin"])
            .stdin(Stdio::null())
            .output()
            .await;
        if let Ok(output) = output
            && output.status.success()
            && repo_key(&String::from_utf8_lossy(&output.stdout)) == want
        {
            return Ok(Some(PathBuf::from(project.path)));
        }
    }
    Ok(None)
}

async fn clone(repository: &str) -> Result<PathBuf, Failure> {
    let url = repository.trim();
    if url.starts_with('-')
        || !(url.starts_with("https://") || url.starts_with("ssh://") || url.starts_with("git@"))
    {
        return Err((INVALID_PARAMS, format!("not a repository URL: {url}")));
    }
    let name = repo_key(url)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err((INVALID_PARAMS, format!("no repository name in {url}")));
    }
    let home = env::var_os("HOME").ok_or_else(|| failed("HOME is not set"))?;
    let dir = PathBuf::from(home).join("src").join(&name);
    if dir.exists() {
        return Err(failed(format!(
            "{} exists but is not a project of {url}",
            dir.display()
        )));
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(failed)?;
    }
    let output = tokio::process::Command::new("git")
        .args(["clone", "--quiet", "--", url])
        .arg(&dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
        )
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(failed)?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(&dir);
        return Err(failed(format!(
            "cannot clone {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(dir)
}

/// A command held for someone at the node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub command_id: String,
    pub method: String,
    pub params: Value,
    pub received_at: String,
    /// `accept` or `reject`, set by `aux node confirm` / `reject`.
    #[serde(default)]
    pub decision: Option<String>,
}

/// Commands waiting for confirmation, in `~/.local/state/agentux/gateway-pending.json`.
pub struct Pending {
    path: PathBuf,
}

impl Pending {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn list(&self) -> Vec<Held> {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, held: &[Held]) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_string_pretty(held).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    /// Holds a command, once per commandId.
    pub fn hold(&self, command_id: &str, method: &str, params: &Value) -> Result<(), String> {
        let mut held = self.list();
        if held.iter().any(|h| h.command_id == command_id) {
            return Ok(());
        }
        held.push(Held {
            command_id: command_id.into(),
            method: method.into(),
            params: params.clone(),
            received_at: jiff::Timestamp::now().to_string(),
            decision: None,
        });
        self.save(&held)
    }

    /// Records someone's decision (`accept` or `reject`) on a held command.
    pub fn decide(&self, command_id: &str, accept: bool) -> Result<Held, String> {
        let mut held = self.list();
        let entry = held
            .iter_mut()
            .find(|h| h.command_id == command_id)
            .ok_or_else(|| format!("no command {command_id} is waiting"))?;
        entry.decision = Some(if accept { "accept" } else { "reject" }.into());
        let decided = entry.clone();
        self.save(&held)?;
        Ok(decided)
    }

    /// Takes the decided commands out of the list.
    pub fn take_decided(&self) -> Vec<Held> {
        let held = self.list();
        let (decided, waiting): (Vec<Held>, Vec<Held>) =
            held.into_iter().partition(|h| h.decision.is_some());
        if !decided.is_empty() {
            let _ = self.save(&waiting);
        }
        decided
    }
}

/// `$XDG_STATE_HOME/agentux/gateway-pending.json`.
pub fn pending_path() -> Option<PathBuf> {
    crate::state_path().map(|p| p.with_file_name("gateway-pending.json"))
}

/// What handling one command produced: the JSON-RPC answer, and the event
/// to report when the command was held or denied.
pub struct Handled {
    pub response: Value,
    pub event: Option<(String, Value)>,
}

/// Recent answers by commandId: a command sent again gets the same answer.
#[derive(Default)]
pub struct Answers {
    order: VecDeque<String>,
    by_id: BTreeMap<String, Value>,
}

impl Answers {
    const MAX: usize = 1_000;

    fn get(&self, id: &str) -> Option<&Value> {
        self.by_id.get(id)
    }

    fn put(&mut self, id: &str, answer: Value) {
        if self.by_id.insert(id.to_string(), answer).is_none() {
            self.order.push_back(id.to_string());
            if self.order.len() > Self::MAX
                && let Some(old) = self.order.pop_front()
            {
                self.by_id.remove(&old);
            }
        }
    }
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answers one JSON-RPC request from Command: checks it is for this node,
/// applies the policy, and runs, holds or refuses it.
pub async fn handle(
    client: &mut Client,
    node_id: &str,
    policy: &Policy,
    pending: &Pending,
    answers: &mut Answers,
    request: &Value,
) -> Handled {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let command_id = params
        .get("commandId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let none = |response| Handled {
        response,
        event: None,
    };
    if command_id.is_empty() {
        return none(error(&id, INVALID_PARAMS, "commandId is required"));
    }
    if params.get("nodeId").and_then(Value::as_str) != Some(node_id) {
        return none(error(
            &id,
            INVALID_PARAMS,
            "the command is for another node",
        ));
    }
    if params.get("apiKey").is_some() || params.get("providerToken").is_some() {
        return none(error(
            &id,
            INVALID_PARAMS,
            "provider credentials are not protocol payloads",
        ));
    }
    if let Some(answer) = answers.get(&command_id) {
        return none(answer.clone());
    }
    let denied = |reason: &str| {
        Some((
            "command.denied".to_string(),
            json!({ "commandId": command_id, "method": method, "reason": reason }),
        ))
    };
    let handled = match policy.decide(method) {
        Decision::Deny => Handled {
            response: error(&id, DENIED, "denied_by_policy"),
            event: denied("denied_by_policy"),
        },
        Decision::Confirm => match pending.hold(&command_id, method, &params) {
            Ok(()) => Handled {
                response: error(&id, HELD, "awaiting_local_confirmation"),
                event: denied("awaiting_local_confirmation"),
            },
            Err(e) => none(error(&id, FAILED, &format!("cannot hold the command: {e}"))),
        },
        Decision::Allow => match execute(client, method, &params).await {
            Ok(result) => none(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
            Err((code, message)) => none(error(&id, code, &message)),
        },
    };
    answers.put(&command_id, handled.response.clone());
    handled
}

/// Runs (or drops) the commands someone decided on; each gives a
/// `command.completed` event.
pub async fn run_decided(client: &mut Client, pending: &Pending) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    for held in pending.take_decided() {
        let accepted = held.decision.as_deref() == Some("accept");
        let outcome = if accepted {
            execute(client, &held.method, &held.params).await
        } else {
            Err((DENIED, "rejected at the node".into()))
        };
        let mut payload = json!({
            "commandId": held.command_id,
            "method": held.method,
            "confirmedBy": "local",
            "ok": outcome.is_ok(),
        });
        match outcome {
            Ok(result) => payload["result"] = result,
            Err((code, message)) => payload["error"] = json!({ "code": code, "message": message }),
        }
        events.push(("command.completed".to_string(), payload));
    }
    events
}

/// The policy file the gateway uses: `/etc/agentux/node-policy.yaml`.
pub fn policy_path() -> &'static Path {
    Path::new(crate::policy::SYSTEM_POLICY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_keys_match_across_url_forms() {
        assert_eq!(
            repo_key("https://github.com/Acme/App.git"),
            "github.com/acme/app"
        );
        assert_eq!(
            repo_key("git@github.com:acme/app.git\n"),
            "github.com/acme/app"
        );
        assert_eq!(
            repo_key("ssh://git@github.com/acme/app"),
            "github.com/acme/app"
        );
    }

    #[test]
    fn held_commands_wait_once_and_leave_when_decided() {
        let dir = tempfile::tempdir().unwrap();
        let pending = Pending::new(dir.path().join("pending.json"));
        pending
            .hold("c1", "run.start", &json!({"prompt": "x"}))
            .unwrap();
        pending
            .hold("c1", "run.start", &json!({"prompt": "x"}))
            .unwrap();
        pending.hold("c2", "preferences.set", &json!({})).unwrap();
        assert_eq!(pending.list().len(), 2);
        assert!(pending.take_decided().is_empty());
        pending.decide("c2", false).unwrap();
        assert!(pending.decide("nope", true).is_err());
        let decided = pending.take_decided();
        assert_eq!(decided.len(), 1);
        assert_eq!(decided[0].decision.as_deref(), Some("reject"));
        assert_eq!(pending.list().len(), 1);
    }
}
