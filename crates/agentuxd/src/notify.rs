//! Desktop notifications for what needs the user's eye while the cockpit is
//! closed: an agent at its limit, another taking over, a run that waits or
//! goes on. Sent with `gdbus` (from GLib, on every desktop) to the session's
//! `org.freedesktop.Notifications`; nothing happens where there is none.

use std::process::{Command, Stdio};
use std::sync::Arc;

/// Shows a notification `(title, body)`. The daemon's sends one to the
/// desktop; the default (tests) does nothing.
#[derive(Clone)]
pub struct Notifier(Arc<dyn Fn(&str, &str) + Send + Sync>);

impl Notifier {
    pub fn new(f: impl Fn(&str, &str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// Notifications on the desktop session bus.
    pub fn desktop() -> Self {
        Self::new(send)
    }

    pub fn notify(&self, title: &str, body: &str) {
        (self.0)(title, body)
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new(|_, _| {})
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Notifier(..)")
    }
}

/// GVariant text for a string.
fn gvariant(text: &str) -> String {
    let escaped: String = text
        .chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '\'' => vec!['\\', '\''],
            '\n' => vec!['\\', 'n'],
            c if c.is_control() => vec![' '],
            c => vec![c],
        })
        .collect();
    format!("'{escaped}'")
}

fn send(title: &str, body: &str) {
    let spawned = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            "'AgentUX'",
            "0",
            "'agentux-cockpit'",
            &gvariant(title),
            &gvariant(body),
            "[]",
            "{}",
            "10000",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // Reaped in the background; a missing gdbus or bus is not an error.
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_quoted_for_gdbus() {
        assert_eq!(gvariant("Codex's limit"), r"'Codex\'s limit'");
        assert_eq!(gvariant("a\nb\\c"), r"'a\nb\\c'");
    }
}
