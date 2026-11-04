//! What each harness needs in an agent sandbox (ADRs 0023 and 0024): the
//! variables that move its state into a directory of the sandbox's own, the
//! one credential file shared with the host, and where it is installed.
//!
//! A sandboxed CLI never sees its host settings, hooks or MCP configuration:
//! its state lives in `~/.local/share/agentux/sandbox/state/<harness>/`,
//! and only its credential file is mounted from the host (read-write, so it
//! can refresh a token).

use std::path::Path;

/// A harness's sandbox needs. Harnesses without one (`None` from
/// [`HarnessSandbox::for_harness`]) are not supported in the sandbox until
/// their paths are verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSandbox {
    /// `(variable, path relative to the state directory)`: points the CLI at
    /// its sandbox state.
    pub state_env: &'static [(&'static str, &'static str)],
    /// Directories to create in the state directory before the first start.
    pub state_dirs: &'static [&'static str],
    /// `(host path relative to $HOME, path relative to the state directory)`
    /// of the credential file shared with the host.
    pub credential: Option<(&'static str, &'static str)>,
    /// Paths relative to `$HOME` where the CLI may be installed, mounted
    /// read-only when they exist.
    pub install: &'static [&'static str],
}

impl HarnessSandbox {
    pub fn for_harness(harness: &str) -> Option<Self> {
        match harness {
            // Claude Code keeps everything (settings, history and the OAuth
            // login in .credentials.json) under CLAUDE_CONFIG_DIR.
            "claude-code" => Some(Self {
                state_env: &[("CLAUDE_CONFIG_DIR", "")],
                state_dirs: &[""],
                credential: Some((".claude/.credentials.json", ".credentials.json")),
                install: &[".local/bin/claude", ".local/share/claude"],
            }),
            // Codex keeps its config, sessions and auth.json under CODEX_HOME.
            "codex" => Some(Self {
                state_env: &[("CODEX_HOME", "")],
                state_dirs: &[""],
                credential: Some((".codex/auth.json", "auth.json")),
                install: &[],
            }),
            // OpenCode follows the XDG directories; auth.json is in its data
            // directory.
            "opencode" => Some(Self {
                state_env: &[
                    ("XDG_CONFIG_HOME", "config"),
                    ("XDG_DATA_HOME", "data"),
                    ("XDG_STATE_HOME", "state"),
                    ("XDG_CACHE_HOME", "cache"),
                ],
                state_dirs: &["config", "data/opencode", "state", "cache"],
                credential: Some((".local/share/opencode/auth.json", "data/opencode/auth.json")),
                install: &[".opencode"],
            }),
            // Antigravity's credential path and behavior are not verified.
            _ => None,
        }
    }

    /// The variables to set, for a state directory at `state`.
    pub fn env(&self, state: &Path) -> Vec<(String, String)> {
        self.state_env
            .iter()
            .map(|(var, rel)| {
                let path = if rel.is_empty() {
                    state.to_path_buf()
                } else {
                    state.join(rel)
                };
                (var.to_string(), path.to_string_lossy().into_owned())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HarnessSpec;

    #[test]
    fn verified_harnesses_have_an_entry_and_antigravity_does_not() {
        for spec in HarnessSpec::builtin() {
            let entry = HarnessSandbox::for_harness(&spec.id);
            assert_eq!(entry.is_some(), spec.id != "antigravity", "{}", spec.id);
        }
    }

    #[test]
    fn state_moves_into_the_sandbox_directory() {
        let state = Path::new("/home/u/.local/share/agentux/sandbox/state/opencode");
        let env = HarnessSandbox::for_harness("opencode").unwrap().env(state);
        assert!(env.contains(&(
            "XDG_DATA_HOME".to_string(),
            format!("{}/data", state.display())
        )));
        let env = HarnessSandbox::for_harness("codex").unwrap().env(state);
        assert_eq!(
            env,
            [("CODEX_HOME".to_string(), state.display().to_string())]
        );
    }

    #[test]
    fn credentials_are_inside_the_state_directory() {
        for harness in ["claude-code", "codex", "opencode"] {
            let entry = HarnessSandbox::for_harness(harness).unwrap();
            let (host, inside) = entry.credential.unwrap();
            assert!(!host.starts_with('/') && !inside.starts_with('/'));
            assert!(!inside.contains(".."), "{harness}");
        }
    }
}
