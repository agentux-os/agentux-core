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
                ..Self::new("antigravity", "agy-acp-server", &["--uid="])
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

    /// The command line that opens the harness's own interactive TUI on the
    /// existing session `session_id` (the ACP session id, which the built-in
    /// adapters share with the vendor: see the crate README), as program and
    /// arguments. `None` when the harness cannot resume a session by id in
    /// its TUI.
    ///
    /// Checked against each CLI's `--help` (Claude Code 2.1, Codex CLI 0.153,
    /// OpenCode 1.18).
    pub fn tui_resume(&self, session_id: &str) -> Option<Vec<String>> {
        let line: &[&str] = match self.id.as_str() {
            "claude-code" => &["claude", "--resume", session_id],
            "codex" => &["codex", "resume", session_id],
            "opencode" => &["opencode", "--session", session_id],
            _ => return None,
        };
        Some(line.iter().map(|s| s.to_string()).collect())
    }

    /// The program the user installs and signs in to for this harness: the
    /// vendor's CLI. The npx adapters of Claude Code and Codex use the CLI's
    /// sign-in, so without it a run would start and fail.
    pub fn cli(&self) -> &str {
        match self.id.as_str() {
            "claude-code" => "claude",
            "codex" => "codex",
            _ => &self.command,
        }
    }

    /// Whether [`HarnessSpec::cli`] and the command are on `PATH`.
    pub fn installed(&self) -> bool {
        on_path(self.cli()) && on_path(&self.command)
    }

    /// Whether the user signed in to the vendor's CLI, from where each CLI
    /// keeps its sign-in, or the API key variables it reads. `None` when
    /// that cannot be told: OpenCode can take its keys from its config, and
    /// a missing Antigravity token file may only be a newer layout.
    pub fn signed_in(&self) -> Option<bool> {
        use std::path::PathBuf;
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let file = |path: PathBuf| path.metadata().is_ok_and(|m| m.len() > 0);
        match self.id.as_str() {
            "claude-code" => Some(
                file(home.join(".claude/.credentials.json"))
                    || var("ANTHROPIC_API_KEY").is_some()
                    || var("CLAUDE_CODE_OAUTH_TOKEN").is_some(),
            ),
            "codex" => {
                let dir = var("CODEX_HOME").map_or_else(|| home.join(".codex"), PathBuf::from);
                Some(file(dir.join("auth.json")) || var("OPENAI_API_KEY").is_some())
            }
            "opencode" => {
                let data =
                    var("XDG_DATA_HOME").map_or_else(|| home.join(".local/share"), PathBuf::from);
                file(data.join("opencode/auth.json")).then_some(true)
            }
            "antigravity" => {
                file(home.join(".gemini/antigravity-cli/antigravity-oauth-token")).then_some(true)
            }
            _ => None,
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

/// Whether `program` is an executable file in a `PATH` directory (or, with
/// a slash, at that path).
fn on_path(program: &str) -> bool {
    use std::path::Path;
    fn executable(path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            path.is_file()
        }
    }
    if program.contains('/') {
        return executable(Path::new(program));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| executable(&dir.join(program)))
    })
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
    fn tui_resume_commands() {
        let line = |id: &str| HarnessSpec::find(id).unwrap().tui_resume("abc");
        assert_eq!(line("claude-code").unwrap(), ["claude", "--resume", "abc"]);
        assert_eq!(line("codex").unwrap(), ["codex", "resume", "abc"]);
        assert_eq!(line("opencode").unwrap(), ["opencode", "--session", "abc"]);
        assert_eq!(line("antigravity"), None);
    }

    #[test]
    fn cli_is_the_vendor_program() {
        let cli = |id: &str| HarnessSpec::find(id).unwrap().cli().to_string();
        assert_eq!(cli("claude-code"), "claude");
        assert_eq!(cli("codex"), "codex");
        assert_eq!(cli("opencode"), "opencode");
        assert_eq!(cli("antigravity"), "agy-acp-server");
    }

    #[cfg(unix)]
    #[test]
    fn installed_looks_on_path() {
        assert!(on_path("sh"));
        assert!(on_path("/bin/sh"));
        assert!(!on_path("agentux-no-such-program"));
    }

    #[test]
    fn command_line() {
        assert_eq!(
            HarnessSpec::find("opencode").unwrap().command_line(),
            "opencode acp"
        );
    }
}
