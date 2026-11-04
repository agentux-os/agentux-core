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
        let mut env = Vec::with_capacity(tool.env.len());
        let mut missing = None;
        for (var, value) in &tool.env {
            match value {
                EnvValue::Plain(value) => env.push((var.clone(), value.clone())),
                EnvValue::Secret { secret: name } => match secret(name) {
                    Some(value) => env.push((var.clone(), value)),
                    None => {
                        missing = Some(name.clone());
                        break;
                    }
                },
            }
        }
        if let Some(name) = missing {
            notes.push(format!(
                "tools: `{}` ({} catalog) is left out: the secret {name} is not set \
                 (a systemd credential, or `secret-tool store --label={name} agentux-key {name}`)",
                tool.name, tool.source
            ));
            continue;
        }
        servers.push(McpServer {
            name: tool.name.clone(),
            command: PathBuf::from(&tool.command),
            args: tool.args.clone(),
            env,
        });
    }
    (servers, notes)
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

    #[test]
    fn the_project_catalog_is_kept_when_there_is_no_other() {
        let (merged, _) = super::catalog(catalog());
        assert!(merged.mcp.contains_key("github"));
    }
}
