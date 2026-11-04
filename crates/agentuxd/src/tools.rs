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

/// Starts `server` over stdio, initializes MCP and lists its tools, within
/// `timeout`. The server is stopped afterwards.
pub async fn probe(server: &McpServer, timeout: std::time::Duration) -> Result<Probe, String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    if let Some(url) = &server.url {
        return Err(format!(
            "{url} is an HTTP server; `aux tools test` starts stdio servers only for now"
        ));
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
        write_line(
            &mut stdin,
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "aux", "version": env!("CARGO_PKG_VERSION")}
            }}),
        )
        .await?;
        let init = read_reply(&mut lines, 1).await?;
        write_line(
            &mut stdin,
            &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await?;
        write_line(
            &mut stdin,
            &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await?;
        let listed = read_reply(&mut lines, 2).await?;
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
        Ok::<_, String>(Probe { server, tools })
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
            if message.get("id") == Some(&serde_json::json!(id)) {
                if let Some(error) = message.get("error") {
                    return Err(format!("the server answered with an error: {error}"));
                }
                return Ok(message.get("result").cloned().unwrap_or_default());
            }
        }
    }

    result
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
