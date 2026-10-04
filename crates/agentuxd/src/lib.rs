//! `agentuxd`, the AgentUX daemon: persists runs in SQLite, drives each
//! project's pipeline inside a git worktree, and serves the local API that
//! the cockpit and `aux` use (ADR 0003, 0004, 0006).
//!
//! The library exists so `aux daemon` can run the same daemon in the
//! foreground, and so tests can drive the engine in-process.

use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use agentux_store::Store;
use clap::Args;

pub mod acp;
pub mod bus;
mod checks;
pub mod engine;
pub mod executor;
pub mod forge;
mod human;
pub mod prompts;
pub mod server;
mod terminal;

pub use acp::{AcpExecutor, HarnessLauncher, LaunchSpec, Launcher};
pub use bus::{BusLink, aux_binary};
pub use engine::{Engine, Settings, TuiCommands};
pub use executor::{FakeExecutor, StepExecutor, StepHost};

/// Command-line options shared by `agentuxd` and `aux daemon`.
#[derive(Debug, Clone, Args)]
pub struct Options {
    /// Socket to listen on [default: $AGENTUX_SOCKET, else
    /// $XDG_RUNTIME_DIR/agentux/agentuxd.sock]
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// SQLite database [default: $XDG_STATE_HOME/agentux/agentuxd.db, else
    /// ~/.local/state/agentux/agentuxd.db]
    #[arg(long)]
    pub database: Option<PathBuf>,
    /// Answer agent steps with a scripted fake instead of real harnesses
    /// (for development and demos)
    #[arg(long)]
    pub fake_agents: bool,
    /// DANGEROUS: allow every tool call agents ask permission for (running
    /// commands, editing and deleting files, fetching URLs) without asking
    /// you. Only for unattended runs in a sandbox you trust
    #[arg(long)]
    pub auto_approve_permissions: bool,
}

/// Runs the daemon until SIGINT or SIGTERM.
pub async fn run(options: Options) -> Result<(), Box<dyn Error>> {
    let socket = match options.socket {
        Some(socket) => socket,
        None => agentux_api::default_socket_path()
            .ok_or("XDG_RUNTIME_DIR is not set; pass --socket or set AGENTUX_SOCKET")?,
    };
    let database = match options.database {
        Some(database) => database,
        None => default_database_path()
            .ok_or("neither XDG_STATE_HOME nor HOME is set; pass --database")?,
    };
    if let Some(dir) = database.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let store =
        Store::open(&database).map_err(|e| format!("cannot open {}: {e}", database.display()))?;
    let executor: Arc<dyn StepExecutor> = if options.fake_agents {
        Arc::new(FakeExecutor::default())
    } else {
        Arc::new(AcpExecutor::default())
    };
    let settings = Settings {
        auto_approve_permissions: options.auto_approve_permissions,
        bus: Some(BusLink::new(socket.clone())),
        ..Settings::default()
    };
    let engine = Engine::with_settings(store, executor, settings);
    let resumed = engine.resume()?;
    eprintln!(
        "agentuxd: listening on {} (database {}, {resumed} unfinished run(s) resumed{})",
        socket.display(),
        database.display(),
        if options.fake_agents {
            ", fake agents"
        } else {
            ""
        }
    );
    if options.auto_approve_permissions {
        eprintln!(
            "agentuxd: WARNING: --auto-approve-permissions: every tool call agents ask about              is allowed without asking you"
        );
    }

    server::serve(engine.clone(), &socket, shutdown_signal())
        .await
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    engine.shutdown();
    eprintln!("agentuxd: stopped");
    Ok(())
}

fn default_database_path() -> Option<PathBuf> {
    let state = env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|p| !p.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(state.join("agentux").join("agentuxd.db"))
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // Without signal handlers, run until killed.
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
