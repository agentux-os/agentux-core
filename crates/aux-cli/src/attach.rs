//! `aux attach`: a session in its harness's own TUI, in this terminal.
//!
//! The daemon runs the TUI on a pseudo-terminal (`terminals.open`); this
//! side puts the local terminal in raw mode, copies keystrokes to it and its
//! output to the screen, and passes window size changes (SIGWINCH) on.
//! Ctrl-] detaches: the daemon closes the TUI and takes the session back.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use agentux_api::rpc::{OpenTerminal, TerminalCommand};
use agentux_api::{Client, TerminalNotice};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::Result;

/// Ctrl-]: detaches (as in telnet and virsh console).
pub const DETACH_KEY: u8 = 0x1d;

/// How the attachment ended.
enum End {
    Detached,
    Exited(Option<i32>),
    Lost,
}

pub async fn attach(socket: &Path, session_id: &str, shell: bool) -> Result {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("aux attach needs a terminal (stdin and stdout must be a TTY)".into());
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let client = Client::connect(socket).await?;
    let (terminal, mut events, mut input) = client
        .open_terminal(&OpenTerminal {
            session_id: Some(session_id.to_string()),
            run_id: None,
            command: Some(if shell {
                TerminalCommand::Shell
            } else {
                TerminalCommand::HarnessTui
            }),
            cols,
            rows,
        })
        .await?;
    eprintln!(
        "aux: terminal {} ({}) in {}; detach with Ctrl-]",
        terminal.terminal_id, terminal.command, terminal.cwd
    );

    // Stdin is read on a thread of its own: a blocking read cannot be
    // cancelled, and the thread just ends with the process.
    let (keys_tx, mut keys) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if keys_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut resized = signal(SignalKind::window_change())?;

    let raw = RawMode::enable()?;
    let mut stdout = std::io::stdout();
    let end = loop {
        tokio::select! {
            key = keys.recv() => {
                let Some(bytes) = key else {
                    break End::Detached;
                };
                match bytes.iter().position(|b| *b == DETACH_KEY) {
                    Some(at) => {
                        if at > 0 {
                            input.write(&bytes[..at]).await?;
                        }
                        break End::Detached;
                    }
                    None => input.write(&bytes).await?,
                }
            }
            notice = events.next() => match notice? {
                Some(TerminalNotice::Output(bytes)) => {
                    stdout.write_all(&bytes)?;
                    stdout.flush()?;
                }
                Some(TerminalNotice::Exit(code)) => break End::Exited(code),
                None => break End::Lost,
            },
            _ = resized.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    input.resize(cols, rows).await?;
                }
            }
        }
    };
    drop(raw);
    match end {
        End::Detached => {
            // Closing the TUI hands the session back to agentuxd; the exit
            // tells us it is done.
            input.close().await?;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Ok(Some(notice)) = events.next().await {
                    if matches!(notice, TerminalNotice::Exit(_)) {
                        break;
                    }
                }
            })
            .await;
            eprintln!("\naux: detached; the session is back with agentuxd");
            Ok(())
        }
        End::Exited(code) => {
            let how = code.map_or_else(|| "was closed".to_string(), |c| format!("exited with {c}"));
            eprintln!("\naux: the terminal {how}; the session is back with agentuxd");
            Ok(())
        }
        End::Lost => Err("agentuxd closed the connection".into()),
    }
}

/// Raw mode for as long as it lives.
struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
