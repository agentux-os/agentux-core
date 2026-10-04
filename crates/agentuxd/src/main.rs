use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(name = "agentuxd", version, about = "The AgentUX daemon")]
struct Cli {
    #[command(flatten)]
    options: agentuxd::Options,
}

#[tokio::main]
async fn main() -> ExitCode {
    match agentuxd::run(Cli::parse().options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentuxd: error: {e}");
            ExitCode::FAILURE
        }
    }
}
