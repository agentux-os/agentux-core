//! `aux bus-stdio`: the `agentux` bus as a stdio MCP server for one harness
//! session. The daemon puts this command in the MCP server list of ACP
//! `session/new`, and the agent launches it.
//!
//! With a session token (`$AGENTUX_BUS_SESSION_TOKEN`, which is how agentuxd
//! passes it, or `--session-token`), tool calls go to `agentuxd` over its
//! socket (the global `--socket`) as the session the token was issued to; see the
//! `agentux-bus` README. `--standalone` serves an in-memory bus with this one
//! session on it instead, for trying the tools from a harness by hand.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentux_bus::{
    Bus, BusConfig, BusServer, DaemonEndpoint, LocalEndpoint, MemoryBackend, RunId,
    SESSION_TOKEN_ENV, SessionId, SessionIdentity,
};
use agentux_config::Config;
use clap::Args;

use crate::Result;

#[derive(Args)]
pub struct BusStdioArgs {
    /// Token identifying the session, issued by agentuxd [default:
    /// $AGENTUX_BUS_SESSION_TOKEN]. Prefer the variable: command lines are
    /// visible to other local users
    #[arg(long)]
    session_token: Option<String>,
    /// Serve an in-memory bus with only this session on it instead of
    /// agentuxd's (development aid). Bus events are printed to stderr as JSON
    #[arg(long, conflicts_with = "session_token")]
    standalone: bool,
    /// [standalone] Project whose agentux.yaml sets roles and bus limits
    #[arg(long, default_value = ".")]
    project: PathBuf,
    /// [standalone] Role the session plays
    #[arg(long, default_value = "implementer")]
    role: String,
    /// [standalone] Harness behind the session
    #[arg(long, default_value = "claude-code")]
    vendor: String,
    /// [standalone] Run id
    #[arg(long, default_value = "standalone")]
    run: String,
}

/// `socket` is the global `--socket` of the daemon to forward calls to.
pub fn bus_stdio(socket: Option<PathBuf>, args: BusStdioArgs) -> Result {
    let token = if args.standalone {
        None
    } else {
        let token = args.session_token.or_else(|| {
            std::env::var(SESSION_TOKEN_ENV)
                .ok()
                .filter(|t| !t.is_empty())
        });
        Some(token.ok_or(
            "no session token: agentuxd passes it as $AGENTUX_BUS_SESSION_TOKEN              (or give --session-token); use --standalone for an in-memory bus",
        )?)
    };
    if let Some(token) = token {
        let socket = socket.or_else(agentux_api::default_socket_path).ok_or(
            "cannot find the agentuxd socket: XDG_RUNTIME_DIR is not set; \
             pass --socket or set AGENTUX_SOCKET",
        )?;
        return tokio::runtime::Runtime::new()?.block_on(forward(&socket, &token));
    }
    let project = args
        .project
        .canonicalize()
        .map_err(|e| format!("cannot use {} as project: {e}", args.project.display()))?;
    let config = Config::load(&project)?;
    let mut bus_config = BusConfig::from_config(&config);
    // Nobody can answer in standalone mode; do not keep the agent waiting.
    bus_config.ask_human_wait = Duration::ZERO;

    let identity = SessionIdentity {
        run: RunId(args.run),
        project: project_name(&project),
        role: args.role,
        vendor: args.vendor,
        session: SessionId::from("standalone"),
    };
    tokio::runtime::Runtime::new()?.block_on(serve(identity, bus_config))
}

async fn serve(identity: SessionIdentity, config: BusConfig) -> Result {
    let bus = Bus::new(MemoryBackend::new());
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let Ok(line) = serde_json::to_string(&event) {
                // stdout carries MCP; a closed stderr is not worth dying for.
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        }
    });
    bus.open_run(identity.run.clone(), identity.project.clone(), config)?;
    let session = identity.session.clone();
    bus.join(identity)?;
    let server = BusServer::new(LocalEndpoint::new(bus, &session)?);
    server
        .serve_stdio()
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e })
}

/// Serves the session's MCP server, forwarding each tool call to the daemon.
async fn forward(socket: &Path, token: &str) -> Result {
    let endpoint = DaemonEndpoint::connect(socket, token).await?;
    BusServer::new(endpoint)
        .serve_stdio()
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e })
}

fn project_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
