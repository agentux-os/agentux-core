//! The tool catalog's MCP servers for a session (agentux-core#68): the
//! machine's, the user's and the project's catalogs merged
//! ([`agentux_config::ToolCatalog`]), the entries for the session's harness,
//! with their secrets looked up. Every harness gets them the same way, in
//! ACP's `session/new`, next to the `agentux` bus.

use std::path::{Path, PathBuf};

use agentux_config::{EnvValue, ToolCatalog, ToolSource};
use agentux_harness::McpServer;

/// The machine's and the user's catalogs with the project's over them.
/// Notes say which files could not be used (their entries are left out).
pub fn catalog(project: ToolCatalog) -> (ToolCatalog, Vec<String>) {
    let mut notes = Vec::new();
    let mut layer = |path: Option<PathBuf>, source| match path {
        Some(path) => ToolCatalog::from_file(&path, source).unwrap_or_else(|e| {
            notes.push(format!(
                "tools: {} is not used: {e}",
                Path::new(&path).display()
            ));
            ToolCatalog::default()
        }),
        None => ToolCatalog::default(),
    };
    let machine = layer(
        Some(PathBuf::from(agentux_config::TOOLS_MACHINE_FILE)),
        ToolSource::Machine,
    );
    let user = layer(agentux_config::tools_user_file(), ToolSource::User);
    (machine.merge(user).merge(project), notes)
}

/// The servers `harness`'s session gets, with their secrets from `secret`.
/// An entry whose secret is not set is left out. Notes name what is left
/// out and why (never a secret's value).
pub fn servers(
    catalog: &ToolCatalog,
    harness: &str,
    secret: impl Fn(&str) -> Option<String>,
) -> (Vec<McpServer>, Vec<String>) {
    let mut servers = Vec::new();
    let mut notes = Vec::new();
    for tool in catalog.mcp.values() {
        if !tool.applies_to(harness) {
            notes.push(format!(
                "tools: `{}` ({} catalog) is not for {harness}",
                tool.name, tool.source
            ));
            continue;
        }
        match resolve(tool, &secret) {
            Ok(server) => servers.push(server),
            Err(name) => notes.push(format!(
                "tools: `{}` ({} catalog) is left out: the secret {name} is not set \
                 (a systemd credential, or `secret-tool store --label={name} agentux-key {name}`)",
                tool.name, tool.source
            )),
        }
    }
    (servers, notes)
}

/// `catalog` as `tools.list` shows it: names, never values.
pub fn listing(
    catalog: &ToolCatalog,
    notes: Vec<String>,
    secret: impl Fn(&str) -> Option<String>,
) -> agentux_api::rpc::ToolsListing {
    let mcp = catalog
        .mcp
        .values()
        .map(|tool| {
            let mut secrets = Vec::new();
            for value in tool.env.values().chain(tool.headers.values()) {
                if let EnvValue::Secret { secret: name } = value
                    && !secrets
                        .iter()
                        .any(|s: &agentux_api::rpc::ToolSecret| &s.name == name)
                {
                    secrets.push(agentux_api::rpc::ToolSecret {
                        name: name.clone(),
                        set: secret(name).is_some(),
                    });
                }
            }
            agentux_api::rpc::ToolInfo {
                name: tool.name.clone(),
                source: tool.source.to_string(),
                command: tool.url.is_none().then(|| {
                    std::iter::once(tool.command.as_str())
                        .chain(tool.args.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                }),
                url: tool.url.clone(),
                harnesses: tool.harnesses.clone(),
                env: tool.env.keys().cloned().collect(),
                headers: tool.headers.keys().cloned().collect(),
                secrets,
            }
        })
        .collect();
    agentux_api::rpc::ToolsListing { mcp, notes }
}

/// `tool` as the server to start, with its secrets from `secret`; the name
/// of the first secret that isn't set otherwise.
pub fn resolve(
    tool: &agentux_config::McpTool,
    secret: impl Fn(&str) -> Option<String>,
) -> Result<McpServer, String> {
    let values = |map: &std::collections::BTreeMap<String, EnvValue>| {
        map.iter()
            .map(|(key, value)| match value {
                EnvValue::Plain(value) => Ok((key.clone(), value.clone())),
                EnvValue::Secret { secret: name } => secret(name)
                    .map(|value| (key.clone(), value))
                    .ok_or_else(|| name.clone()),
            })
            .collect::<Result<Vec<_>, String>>()
    };
    Ok(McpServer {
        name: tool.name.clone(),
        command: PathBuf::from(&tool.command),
        args: tool.args.clone(),
        env: values(&tool.env)?,
        url: tool.url.clone(),
        headers: values(&tool.headers)?,
    })
}

/// What an MCP server says about itself (`aux tools test`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// `serverInfo.name` and `.version`.
    pub server: String,
    pub tools: Vec<String>,
}

/// The MCP version `aux tools test` speaks.
const MCP_VERSION: &str = "2025-06-18";

fn initialize_request() -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": MCP_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "aux", "version": env!("CARGO_PKG_VERSION")}
    }})
}

fn initialized_notification() -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

fn tools_list_request() -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
}

/// The [`Probe`] from the answers to `initialize` and `tools/list`.
fn summary(init: &serde_json::Value, listed: &serde_json::Value) -> Probe {
    let info = &init["serverInfo"];
    let server = format!(
        "{} {}",
        info["name"].as_str().unwrap_or("(unnamed)"),
        info["version"].as_str().unwrap_or("")
    )
    .trim()
    .to_string();
    let tools = listed["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Probe { server, tools }
}

/// The result of the JSON-RPC answer with `id` in `message` (one message or
/// a batch), if it is there.
fn answer(message: &serde_json::Value, id: u64) -> Option<Result<serde_json::Value, String>> {
    if let Some(batch) = message.as_array() {
        return batch.iter().find_map(|m| answer(m, id));
    }
    (message.get("id") == Some(&serde_json::json!(id))).then(|| match message.get("error") {
        Some(error) => Err(format!("the server answered with an error: {error}")),
        None => Ok(message.get("result").cloned().unwrap_or_default()),
    })
}

/// Initializes MCP with `server`, started over stdio or reached over HTTP,
/// and lists its tools, within `timeout`. A started server is stopped
/// afterwards.
pub async fn probe(server: &McpServer, timeout: std::time::Duration) -> Result<Probe, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    if let Some(url) = &server.url {
        return tokio::time::timeout(timeout, probe_http(url, &server.headers, timeout))
            .await
            .map_err(|_| format!("no answer within {}s", timeout.as_secs()))?;
    }
    let mut child = tokio::process::Command::new(&server.command)
        .args(&server.args)
        .envs(server.env.iter().cloned())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", server.command.display()))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut lines = BufReader::new(child.stdout.take().ok_or("no stdout")?).lines();
    let talk = async move {
        write_line(&mut stdin, &initialize_request()).await?;
        let init = read_reply(&mut lines, 1).await?;
        write_line(&mut stdin, &initialized_notification()).await?;
        write_line(&mut stdin, &tools_list_request()).await?;
        let listed = read_reply(&mut lines, 2).await?;
        Ok::<_, String>(summary(&init, &listed))
    };
    let result = tokio::time::timeout(timeout, talk)
        .await
        .map_err(|_| format!("no answer within {}s", timeout.as_secs()))?;
    let _ = child.kill().await;

    async fn write_line(
        stdin: &mut tokio::process::ChildStdin,
        message: &serde_json::Value,
    ) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("the server closed its input: {e}"))
    }

    async fn read_reply(
        lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        id: u64,
    ) -> Result<serde_json::Value, String> {
        loop {
            let line = lines
                .next_line()
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the server exited")?;
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if let Some(result) = answer(&message, id) {
                return result;
            }
        }
    }

    result
}

/// [`probe`] over MCP's Streamable HTTP transport: each message is a POST,
/// answered with JSON or an event stream, in the session the server names.
async fn probe_http(
    url: &str,
    headers: &[(String, String)],
    timeout: std::time::Duration,
) -> Result<Probe, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut session = None;
    let init = post(
        &client,
        url,
        headers,
        &mut session,
        &initialize_request(),
        Some(1),
    )
    .await?;
    post(
        &client,
        url,
        headers,
        &mut session,
        &initialized_notification(),
        None,
    )
    .await?;
    let listed = post(
        &client,
        url,
        headers,
        &mut session,
        &tools_list_request(),
        Some(2),
    )
    .await?;
    Ok(summary(&init, &listed))
}

/// POSTs `message` and, for a request, waits for the answer with `id`.
async fn post(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    session: &mut Option<String>,
    message: &serde_json::Value,
    id: Option<u64>,
) -> Result<serde_json::Value, String> {
    let mut request = client
        .post(url)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(message);
    // After `initialize`, every request says the version and the session.
    if id != Some(1) {
        request = request.header("MCP-Protocol-Version", MCP_VERSION);
    }
    if let Some(session) = session.as_deref() {
        request = request.header("Mcp-Session-Id", session);
    }
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => " (check its headers and secrets)",
            _ => "",
        };
        return Err(format!("{url} answered HTTP {status}{hint}"));
    }
    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        *session = Some(id.to_string());
    }
    let Some(id) = id else {
        return Ok(serde_json::Value::Null);
    };
    let stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/event-stream"));
    let mut body = String::new();
    let mut data = String::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        body.push_str(&String::from_utf8_lossy(&chunk));
        if !stream {
            continue;
        }
        // An event stream: events end at a blank line; their `data` lines
        // hold the messages. The answer may come after other messages, and
        // the stream may stay open after it.
        while let Some(end) = body.find('\n') {
            let line: String = body.drain(..=end).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(value) = line.strip_prefix("data:") {
                data.push_str(value.trim_start());
            } else if line.is_empty() && !data.is_empty() {
                let event = std::mem::take(&mut data);
                if let Ok(message) = serde_json::from_str::<serde_json::Value>(&event)
                    && let Some(result) = answer(&message, id)
                {
                    return result;
                }
            }
        }
    }
    if stream {
        return Err(format!("{url} closed its event stream without answering"));
    }
    let message: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("{url} answered with no JSON: {e}"))?;
    answer(&message, id).unwrap_or_else(|| Err(format!("{url} answered something else")))
}

/// A secret for a catalog entry: the daemon's own environment, a systemd
/// credential, or the desktop's secret service.
pub fn secret(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| crate::secrets::lookup(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ToolCatalog {
        ToolCatalog::from_yaml(
            r#"
mcp:
  - name: github
    command: github-mcp-server
    args: [stdio]
    env:
      TOKEN: { secret: GH_TOKEN }
      MODE: read-only
  - name: browser
    command: /usr/bin/browser-mcp
    harnesses: [claude-code]
  - name: wiki
    command: wiki-mcp
    env:
      WIKI_KEY: { secret: WIKI_KEY }
"#,
            ToolSource::Project,
        )
        .unwrap()
    }

    fn secret(name: &str) -> Option<String> {
        (name == "GH_TOKEN").then(|| "gh-secret-value".to_string())
    }

    #[test]
    fn a_session_gets_its_harness_servers_with_secrets_resolved() {
        let (servers, _) = servers(&catalog(), "claude-code", secret);
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["browser", "github"]);
        let github = &servers[1];
        assert_eq!(github.command, PathBuf::from("github-mcp-server"));
        assert_eq!(github.args, ["stdio"]);
        assert_eq!(
            github.env,
            [
                ("MODE".to_string(), "read-only".to_string()),
                ("TOKEN".to_string(), "gh-secret-value".to_string()),
            ]
        );
    }

    #[test]
    fn what_is_left_out_is_said_without_secret_values() {
        let (servers, notes) = servers(&catalog(), "codex", secret);
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["github"]
        );
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("`browser`") && notes[0].contains("not for codex"));
        assert!(notes[1].contains("`wiki`") && notes[1].contains("WIKI_KEY"));
        assert!(notes.iter().all(|n| !n.contains("gh-secret-value")));
    }

    /// A stdio MCP server in `sh`: answers initialize, skips the
    /// notification, and lists two tools.
    const FAKE_SERVER: &str = r#"read l; echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake-mcp","version":"1.2"}}}'; read l; read l; echo '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'; echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search"},{"name":"fetch"}]}}'; sleep 5"#;

    #[tokio::test]
    async fn a_probe_initializes_and_lists_the_tools() {
        let server = McpServer {
            name: "fake".into(),
            command: PathBuf::from("sh"),
            args: vec!["-c".into(), FAKE_SERVER.into()],
            env: Vec::new(),
            ..McpServer::default()
        };
        let probe = probe(&server, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(probe.server, "fake-mcp 1.2");
        assert_eq!(probe.tools, ["search", "fetch"]);
    }

    #[tokio::test]
    async fn a_silent_server_times_out_and_a_missing_one_is_said() {
        let silent = McpServer {
            name: "silent".into(),
            command: PathBuf::from("sh"),
            args: vec!["-c".into(), "sleep 30".into()],
            env: Vec::new(),
            ..McpServer::default()
        };
        let err = probe(&silent, std::time::Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(err.contains("no answer"), "{err}");
        let missing = McpServer {
            command: PathBuf::from("/nonexistent/mcp-server"),
            ..silent
        };
        let err = probe(&missing, std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.contains("cannot start"), "{err}");
    }

    /// A Streamable HTTP MCP server on localhost: `initialize` answered with
    /// JSON and a session id, `tools/list` over an event stream that stays
    /// open. Returns its URL and the requests it got (method, session id,
    /// `Authorization`).
    #[allow(clippy::type_complexity)]
    async fn http_server() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    ) {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        fn response(status: &str, headers: &str, body: &str) -> String {
            format!(
                "HTTP/1.1 {status}\r\n{headers}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 4096];
                    let (head, body) = loop {
                        let n = socket.read(&mut buf).await.unwrap();
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let head = text[..end].to_lowercase();
                            let length: usize = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map_or(0, |v| v.trim().parse().unwrap());
                            if raw.len() >= end + 4 + length {
                                break (head, text[end + 4..end + 4 + length].to_string());
                            }
                        }
                    };
                    let header = |name: &str| {
                        head.lines()
                            .find_map(|l| l.strip_prefix(&format!("{name}:")))
                            .map_or(String::new(), |v| v.trim().to_string())
                    };
                    let message: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let method = message["method"].as_str().unwrap().to_string();
                    log.lock().unwrap().push((
                        method.clone(),
                        header("mcp-session-id"),
                        header("authorization"),
                    ));
                    let reply = match method.as_str() {
                        "initialize" => response(
                            "200 OK",
                            "content-type: application/json\r\nmcp-session-id: s-1",
                            r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"remote-mcp","version":"3.0"}}}"#,
                        ),
                        // No length: the body is the stream.
                        "tools/list" => concat!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
                            r#"data: {"jsonrpc":"2.0","method":"notifications/message"}"#,
                            "\n\n",
                            r#"data: {"jsonrpc":"2.0","id":2,"#,
                            "\n",
                            r#"data: "result":{"tools":[{"name":"query"}]}}"#,
                            "\n\n",
                        )
                        .to_string(),
                        _ => response("202 Accepted", "content-type: application/json", ""),
                    };
                    socket.write_all(reply.as_bytes()).await.unwrap();
                    if method == "tools/list" {
                        // The stream stays open after the answer.
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                });
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn an_http_probe_follows_the_session_and_reads_event_streams() {
        let (url, seen) = http_server().await;
        let server = McpServer {
            name: "remote".into(),
            url: Some(url),
            headers: vec![("Authorization".into(), "Bearer t0k".into())],
            ..McpServer::default()
        };
        let probe = probe(&server, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(probe.server, "remote-mcp 3.0");
        assert_eq!(probe.tools, ["query"]);
        let seen = seen.lock().unwrap().clone();
        let bearer = || "bearer t0k".to_string();
        assert_eq!(
            seen,
            [
                ("initialize".into(), String::new(), bearer()),
                ("notifications/initialized".into(), "s-1".into(), bearer()),
                ("tools/list".into(), "s-1".into(), bearer()),
            ]
        );
    }

    #[tokio::test]
    async fn an_http_server_that_refuses_says_so() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
                )
                .await;
        });
        let server = McpServer {
            url: Some(url),
            ..McpServer::default()
        };
        let err = probe(&server, std::time::Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(err.contains("401") && err.contains("headers"), "{err}");
    }

    #[test]
    fn the_listing_names_secrets_and_variables_but_no_values() {
        let listing = listing(&catalog(), Vec::new(), secret);
        let github = &listing.mcp.iter().find(|t| t.name == "github").unwrap();
        assert_eq!(github.command.as_deref(), Some("github-mcp-server stdio"));
        assert_eq!(github.env, ["MODE", "TOKEN"]);
        assert_eq!(
            github.secrets,
            [agentux_api::rpc::ToolSecret {
                name: "GH_TOKEN".into(),
                set: true
            }]
        );
        let wiki = listing.mcp.iter().find(|t| t.name == "wiki").unwrap();
        assert!(!wiki.secrets[0].set);
        let browser = listing.mcp.iter().find(|t| t.name == "browser").unwrap();
        assert_eq!(
            browser.harnesses.as_deref(),
            Some(&["claude-code".to_string()][..])
        );
        let json = serde_json::to_string(&listing).unwrap();
        assert!(
            !json.contains("gh-secret-value") && !json.contains("read-only"),
            "{json}"
        );
    }

    #[test]
    fn resolve_names_the_missing_secret() {
        let catalog = catalog();
        assert_eq!(
            resolve(&catalog.mcp["wiki"], secret).unwrap_err(),
            "WIKI_KEY"
        );
        assert_eq!(
            resolve(&catalog.mcp["github"], secret).unwrap().env.len(),
            2
        );
    }

    #[test]
    fn the_project_catalog_is_kept_when_there_is_no_other() {
        let (merged, _) = super::catalog(catalog());
        assert!(merged.mcp.contains_key("github"));
    }
}
