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
    /// The mesh: this node's identity and its hub.
    #[command(subcommand)]
    Mesh(MeshCommand),
    /// Channels: the unit of tenancy, separated by key.
    #[command(subcommand)]
    Channel(ChannelCommand),
}

#[derive(Subcommand)]
enum MeshCommand {
    /// Print this node's id and fingerprint, generating the identity if needed.
    /// The id is what a hub's TRACON_HUB_ADMIT takes.
    Id,
    /// The first node: create the `@mesh` channel key here and point this node
    /// at a hub that admits it (`TRACON_HUB_ADMIT=<node id>` on the hub).
    Init {
        /// The hub's URL, e.g. https://tracon-hub.0x69.xyz
        #[arg(long)]
        hub: String,
    },
    /// List the hub's members and their channels.
    Members,
}

#[derive(Subcommand)]
enum ChannelCommand {
    /// Create a channel: mint its key here. Other nodes get it by enrollment.
    Create { name: String },
    /// The channels this node holds keys for.
    List,
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
        Command::Mesh(cmd) => mesh_command(cmd).await,
        Command::Channel(cmd) => channel_command(cmd),
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

async fn mesh_command(cmd: MeshCommand) -> Result<()> {
    use tracon::mesh::identity;
    match cmd {
        MeshCommand::Id => {
            let (id, fresh) = identity::load_or_generate()?;
            if fresh {
                eprintln!(
                    "generated a new identity at {}",
                    identity::seed_path().display()
                );
            }
            println!("node id:     {}", id.node_id());
            println!(
                "fingerprint: {}",
                proto::enroll::fingerprint(&id.verifying_key().to_bytes())
            );
            Ok(())
        }
        MeshCommand::Init { hub } => {
            let (id, _) = identity::load_or_generate()?;
            let store = tracon::store::Store::open(&config::Config::db_path())?;
            create_channel(&store, &id, proto::frame::MESH_CHANNEL)?;
            let mut cfg = config::Config::try_load().map_err(|e| anyhow::anyhow!(e))?;
            cfg.mesh.hub_url = Some(hub.trim_end_matches('/').to_string());
            cfg.save()?;
            println!("hub:         {}", cfg.mesh.hub_url.as_deref().unwrap_or(""));
            println!("node id:     {}", id.node_id());
            println!("admit it on the hub with TRACON_HUB_ADMIT={}", id.node_id());
            println!("then `tracon serve`; other nodes join with `tracon mesh invite`");
            Ok(())
        }
        MeshCommand::Members => {
            let cfg = config::Config::load();
            let hub = cfg.mesh.hub_url.clone().ok_or_else(|| {
                anyhow::anyhow!("no hub configured; run tracon mesh init or tracon enroll")
            })?;
            let (id, _) = identity::load_or_generate()?;
            let v = tracon::mesh::client::MeshClient::get_once(&id, &hub, "/v0/members").await?;
            for m in v.as_array().cloned().unwrap_or_default() {
                let channels: Vec<&str> = m["channels"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|c| c.as_str()).collect())
                    .unwrap_or_default();
                let nid = m["node_id"].as_str().unwrap_or("");
                println!(
                    "{}  {:<16} {}{}",
                    &nid[..nid.len().min(16)],
                    m["name"].as_str().unwrap_or(""),
                    channels.join(", "),
                    if nid == id.node_id() { "  (you)" } else { "" }
                );
            }
            Ok(())
        }
    }
}

fn channel_command(cmd: ChannelCommand) -> Result<()> {
    let store = tracon::store::Store::open(&config::Config::db_path())?;
    match cmd {
        ChannelCommand::Create { name } => {
            if !proto::frame::valid_channel(&name) {
                anyhow::bail!("channel names are lowercase [a-z0-9@._-], at most 64 characters");
            }
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            create_channel(&store, &id, &name)?;
            println!("created channel {name}; hand its key to other nodes with tracon mesh invite");
            Ok(())
        }
        ChannelCommand::List => {
            for c in store.channel_list()? {
                let ring = proto::keyring::Keyring::from_bytes(&c.keyring)
                    .map(|r| r.entries().len())
                    .unwrap_or(0);
                println!(
                    "{:<24} {} epoch{}",
                    c.name,
                    ring,
                    if ring == 1 { "" } else { "s" }
                );
            }
            Ok(())
        }
    }
}

/// Mint a genesis keyring for `name` wrapped to this node, unless one exists.
fn create_channel(
    store: &tracon::store::Store,
    id: &proto::keys::Identity,
    name: &str,
) -> Result<()> {
    if store.channel_get(name)?.is_some() {
        println!("channel {name} already exists here");
    } else {
        let ring = proto::keyring::Keyring::genesis(
            &id.x25519_public(),
            &proto::envelope::DataKey::generate(),
        );
        store.channel_put(name, &ring.to_bytes(), "{}")?;
    }
    store.node_channel_add(&id.node_id(), name)?;
    Ok(())
}
