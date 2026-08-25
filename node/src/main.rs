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
    /// Manage the signed policy bundle.
    #[command(subcommand)]
    Policy(PolicyCommand),
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Generate a signing key. The private half stays on this machine and never
    /// reaches the hub, so a compromised hub can serve stale policy but not new.
    Keygen {
        /// Overwrite an existing key. The old one stops being able to sign.
        #[arg(long)]
        force: bool,
    },
    /// Write the five working agreements as a starting bundle, and sign it.
    Init,
    /// Sign the current bundle, after editing it.
    Sign,
    /// Verify the bundle and print what it decides. Exits non-zero if the
    /// signature does not check out.
    Show,
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
        Command::Policy(cmd) => policy_command(cmd),
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

fn policy_command(cmd: PolicyCommand) -> Result<()> {
    use tracon::policy::{bundle, Policy, WORKING_AGREEMENTS};

    match cmd {
        PolicyCommand::Keygen { force } => {
            if bundle::Paths::signing_key().exists() && !force {
                anyhow::bail!(
                    "a signing key already exists at {}; pass --force to replace it",
                    bundle::Paths::signing_key().display()
                );
            }
            let (signing, verifying) = bundle::generate_key();
            bundle::write_key(&signing)?;
            println!("signing key: {}", bundle::Paths::signing_key().display());
            println!("public key:  {}", hex::encode(verifying.to_bytes()));
            Ok(())
        }
        PolicyCommand::Init => {
            let path = bundle::Paths::bundle();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, WORKING_AGREEMENTS)?;
            std::fs::write(
                bundle::Paths::signature(),
                bundle::sign(WORKING_AGREEMENTS)?,
            )?;
            println!("wrote and signed {}", path.display());
            Ok(())
        }
        PolicyCommand::Sign => {
            let text = std::fs::read_to_string(bundle::Paths::bundle())?;
            // Refuse to sign something that will not load, rather than shipping
            // a signed bundle the node then ignores.
            let _: Policy = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("the bundle is not valid policy: {e}"))?;
            std::fs::write(bundle::Paths::signature(), bundle::sign(&text)?)?;
            println!("signed {}", bundle::Paths::bundle().display());
            Ok(())
        }
        PolicyCommand::Show => {
            let policy = bundle::load()?;
            println!("{} rules, version {}", policy.rules.len(), policy.version);
            for rule in &policy.rules {
                println!(
                    "  {:<24} {:?}  {}",
                    rule.id,
                    rule.verdict,
                    if rule.matches.is_empty() {
                        format!("kinds {:?}", rule.kinds)
                    } else {
                        format!("{} patterns", rule.matches.len())
                    }
                );
            }
            Ok(())
        }
    }
}
