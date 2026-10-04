//! `aux bus-stdio`: the `agentux` bus as a stdio MCP server for one harness
//! session. The daemon puts this command in the MCP server list of ACP
//! `session/new`, and the agent launches it.
//!
//! Tool calls will be forwarded to `agentuxd` over its socket (the global
//! `--socket`) once the daemon serves the bus (see the `agentux-bus` README);
//! until then only `--standalone` works: an in-memory bus with this one
//! session on it, for trying the tools from a harness by hand.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentux_bus::{
    Bus, BusConfig, BusServer, LocalEndpoint, MemoryBackend, RunId, SessionId, SessionIdentity,
};
use agentux_config::Config;
use clap::Args;

use crate::Result;

#[derive(Args)]
pub struct BusStdioArgs {
    /// Token identifying the session, issued by agentuxd
    #[arg(long, required_unless_present = "standalone")]
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

/// `socket` is the global `--socket` the daemon bridge will connect to.
pub fn bus_stdio(socket: Option<PathBuf>, args: BusStdioArgs) -> Result {
    if !args.standalone {
        let _ = socket;
        return Err(
            "forwarding to agentuxd is not available yet (the daemon does not \
                    serve the bus); use --standalone for an in-memory bus"
                .into(),
        );
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

fn project_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}
