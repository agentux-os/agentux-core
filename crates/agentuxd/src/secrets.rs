//! API keys the user keeps in the desktop's secret service (KWallet, GNOME
//! Keyring) for the agent CLIs, under the attribute `agentux-key=<VARIABLE>`
//! (the cockpit's Settings, or `secret-tool store --label=... agentux-key
//! ANTHROPIC_API_KEY`). A user service does not read `~/.bashrc`, so the
//! daemon looks each key up when it starts a harness and hands it to that
//! harness's process only, as the variable the CLI reads. A variable already
//! set in the daemon's environment wins. Keys never reach the store, the
//! event log or a file.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Every variable the cockpit can keep a key for.
pub const VARIABLES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// How long a lookup may take: a locked wallet may ask the user to unlock
/// it, and a harness start or `harnesses.list` must not wait on that.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// The variables a harness's CLI reads its key from.
pub fn variables(harness: &str) -> &'static [&'static str] {
    match harness {
        "claude-code" => &["ANTHROPIC_API_KEY"],
        "codex" => &["OPENAI_API_KEY"],
        "antigravity" => &["GEMINI_API_KEY"],
        // OpenCode works with any provider.
        "opencode" => VARIABLES,
        _ => &[],
    }
}

/// The keys to add to `harness`'s environment: from the secret service,
/// for its variables that the daemon's own environment does not set.
pub fn env_for(harness: &str) -> Vec<(String, String)> {
    variables(harness)
        .iter()
        .filter(|var| std::env::var_os(var).is_none_or(|v| v.is_empty()))
        .filter_map(|var| lookup(var).map(|key| (var.to_string(), key)))
        .collect()
}

/// Whether a key for one of `harness`'s variables is in the secret service.
pub fn has_key(harness: &str) -> bool {
    variables(harness).iter().any(|var| lookup(var).is_some())
}

/// The key for `var`: a systemd credential of that name first (servers
/// without a desktop: `LoadCredential=` / `LoadCredentialEncrypted=` on
/// agentuxd.service puts it in `$CREDENTIALS_DIRECTORY`), else the secret
/// service. `None` when there is no such key.
pub fn lookup(var: &str) -> Option<String> {
    credential(var).or_else(|| keyring(var))
}

/// `$CREDENTIALS_DIRECTORY/<VAR>`, as systemd passes credentials to a unit.
fn credential(var: &str) -> Option<String> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY").filter(|v| !v.is_empty())?;
    let key = std::fs::read_to_string(std::path::Path::new(&dir).join(var)).ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// `secret-tool lookup agentux-key VAR`; `None` when there is no such key,
/// no secret service, no `secret-tool`, or no answer in time.
fn keyring(var: &str) -> Option<String> {
    let mut child = Command::new("secret-tool")
        .args(["lookup", "agentux-key", var])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut key = String::new();
    child.stdout.take()?.read_to_string(&mut key).ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_harness_reads_its_vendors_key() {
        assert_eq!(variables("claude-code"), ["ANTHROPIC_API_KEY"]);
        assert_eq!(variables("codex"), ["OPENAI_API_KEY"]);
        assert_eq!(variables("opencode").len(), VARIABLES.len());
        assert!(variables("fake").is_empty());
    }

    #[test]
    fn systemd_credentials_come_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTUX_TEST_CRED"), " sk-test\n").unwrap();
        // SAFETY of the test: no other test reads this variable.
        unsafe { std::env::set_var("CREDENTIALS_DIRECTORY", dir.path()) };
        assert_eq!(lookup("AGENTUX_TEST_CRED").as_deref(), Some("sk-test"));
        unsafe { std::env::remove_var("CREDENTIALS_DIRECTORY") };
        assert_eq!(credential("AGENTUX_TEST_CRED"), None);
    }

    #[test]
    fn no_secret_service_is_no_key() {
        // CI has no secret service (or no secret-tool): nothing, quickly.
        let started = Instant::now();
        assert_eq!(lookup("AGENTUX_TEST_NO_SUCH_KEY"), None);
        assert!(started.elapsed() <= LOOKUP_TIMEOUT + Duration::from_secs(1));
    }
}
