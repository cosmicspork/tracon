use tracon::{boundary, config, http};

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
        Command::Setup => {
            let cfg = config::Config::load();
            boundary::setup(&cfg).await?;
            println!("network, allowlist, and gateway are in place");
            Ok(())
        }
        Command::CheckBoundary { deep } => {
            let cfg = config::Config::load();
            let report = boundary::check_all(&cfg, deep).await;
            for c in &report.checks {
                println!(
                    "{} {:<22} {}",
                    if c.ok { "ok  " } else { "FAIL" },
                    c.id.as_str(),
                    c.detail
                );
            }
            match report.first_failure() {
                None => Ok(()),
                // Refusal is the honest state: name the check and exit non-zero.
                Some(f) => anyhow::bail!("boundary check failed: {}", f.id.as_str()),
            }
        }
    }
}
