//! How each harness is launched as an ACP agent. The crate README records the
//! source of every entry.

/// The command that starts a harness as an ACP agent speaking JSON-RPC on its
/// stdin and stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSpec {
    /// The name `agentux.yaml` roles refer to, e.g. `claude-code`.
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    /// Added to the environment AgentUX itself runs with.
    pub env: Vec<(String, String)>,
    /// Not expected to work reliably yet.
    pub experimental: bool,
}

impl HarnessSpec {
    /// The harnesses AgentUX ships with.
    ///
    /// Adapter versions are pinned to the ones published in the ACP registry
    /// (<https://github.com/agentclientprotocol/registry>) so a run does not
    /// change behavior when an adapter is released.
    pub fn builtin() -> Vec<Self> {
        vec![
            Self::new(
                "claude-code",
                "npx",
                &["-y", "@agentclientprotocol/claude-agent-acp@0.85.1"],
            ),
            Self::new(
                "codex",
                "npx",
                &["-y", "@agentclientprotocol/codex-acp@2.1.1"],
            ),
            Self::new("opencode", "opencode", &["acp"]),
            Self {
                experimental: true,
                ..Self::new("antigravity", "agy_acp_server.par", &["--uid="])
            },
        ]
    }

    /// The built-in harness called `id`.
    pub fn find(id: &str) -> Option<Self> {
        Self::builtin().into_iter().find(|spec| spec.id == id)
    }

    fn new(id: &str, command: &str, args: &[&str]) -> Self {
        Self {
            id: id.to_string(),
            command: command.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env: Vec::new(),
            experimental: false,
        }
    }

    /// The command line, for messages.
    pub fn command_line(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_ids_are_unique_and_findable() {
        let specs = HarnessSpec::builtin();
        for spec in &specs {
            assert_eq!(HarnessSpec::find(&spec.id).as_ref(), Some(spec));
        }
        let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "codex", "opencode", "antigravity"]);
        assert!(HarnessSpec::find("nope").is_none());
    }

    #[test]
    fn only_antigravity_is_experimental() {
        for spec in HarnessSpec::builtin() {
            assert_eq!(spec.experimental, spec.id == "antigravity", "{}", spec.id);
        }
    }

    #[test]
    fn command_line() {
        assert_eq!(
            HarnessSpec::find("opencode").unwrap().command_line(),
            "opencode acp"
        );
    }
}
