//! What AgentUX Command may do on this node: `/etc/agentux/node-policy.yaml`,
//! set by whoever administers the machine. Each AUXP command method is
//! `allow` (runs), `confirm` (waits for someone at the node) or `deny`.
//!
//! ```yaml
//! commands:
//!   run.start: confirm
//!   preferences.set: deny
//! ```
//!
//! Methods the file does not name keep [`Policy::default`]'s decision;
//! methods this gateway does not know are denied.

use std::collections::BTreeMap;
use std::path::Path;
use std::{fs, io};

use serde::{Deserialize, Serialize};

pub const SYSTEM_POLICY: &str = "/etc/agentux/node-policy.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Confirm,
    Deny,
}

/// Every command method the gateway runs, and its default decision: what
/// does not change code or settings runs; starting runs and changing
/// preferences wait for someone at the node.
pub const DEFAULTS: &[(&str, Decision)] = &[
    ("run.snapshot", Decision::Allow),
    ("harness.list", Decision::Allow),
    ("harness.test", Decision::Allow),
    ("run.cancel", Decision::Allow),
    ("run.pause", Decision::Allow),
    ("run.resume", Decision::Allow),
    ("request.resolve", Decision::Allow),
    ("bus.post", Decision::Allow),
    ("limits.clear", Decision::Allow),
    ("run.start", Decision::Confirm),
    ("preferences.set", Decision::Confirm),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy(BTreeMap<String, Decision>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    commands: BTreeMap<String, Decision>,
}

impl Default for Policy {
    fn default() -> Self {
        Self(DEFAULTS.iter().map(|(m, d)| (m.to_string(), *d)).collect())
    }
}

impl Policy {
    /// The defaults with `path` on top; the defaults when there is no file.
    pub fn load(path: &Path) -> Result<Policy, String> {
        let policy = Policy::default();
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(policy),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The defaults with a policy file's text (YAML or JSON) on top.
    pub fn parse(text: &str) -> Result<Policy, String> {
        let mut policy = Policy::default();
        if text.trim().is_empty() {
            return Ok(policy);
        }
        let file: File =
            serde_saphyr::from_str(text).map_err(|e| format!("invalid policy: {e}"))?;
        for (method, decision) in file.commands {
            if !policy.0.contains_key(&method) {
                return Err(format!("unknown command `{method}`"));
            }
            policy.0.insert(method, decision);
        }
        Ok(policy)
    }

    pub fn decide(&self, method: &str) -> Decision {
        self.0.get(method).copied().unwrap_or(Decision::Deny)
    }

    /// Every method and its decision, for `aux node status`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, Decision)> {
        self.0.iter().map(|(m, d)| (m.as_str(), *d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let policy = Policy::default();
        assert_eq!(policy.decide("harness.list"), Decision::Allow);
        assert_eq!(policy.decide("run.start"), Decision::Confirm);
        assert_eq!(policy.decide("shell.exec"), Decision::Deny);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-policy.yaml");
        assert_eq!(Policy::load(&path).unwrap(), Policy::default());
        fs::write(
            &path,
            "commands:\n  run.start: allow\n  limits.clear: deny\n",
        )
        .unwrap();
        let policy = Policy::load(&path).unwrap();
        assert_eq!(policy.decide("run.start"), Decision::Allow);
        assert_eq!(policy.decide("limits.clear"), Decision::Deny);
        assert_eq!(policy.decide("preferences.set"), Decision::Confirm);

        fs::write(&path, "commands:\n  shell.exec: allow\n").unwrap();
        assert!(
            Policy::load(&path)
                .unwrap_err()
                .contains("unknown command `shell.exec`")
        );
    }
}
