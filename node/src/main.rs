mod http;

use std::net::SocketAddr;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tracon", version, about = "tracon node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the node: boundary check, harness supervision, HTTP API and interface.
    Serve {
        /// Address for the operator API and SPA.
        #[arg(long, env = "TRACON_LISTEN", default_value = "127.0.0.1:7420")]
        listen: SocketAddr,
    },
    /// Create the harness network, gateway, and images this node owns.
    Setup,
    /// Verify the harness boundary and exit non-zero naming the first failed check.
    CheckBoundary {
        /// Also probe egress from inside the boundary.
        #[arg(long)]
        deep: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tracon=info,tower_http=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { listen } => http::serve(listen).await,
        Command::Setup => anyhow::bail!("setup: not implemented yet"),
        Command::CheckBoundary { .. } => anyhow::bail!("check-boundary: not implemented yet"),
    }
}
