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
    Setup {
        /// Rebuild the gateway and harness images even if they exist.
        #[arg(long)]
        rebuild: bool,
    },
    /// Verify the harness boundary and exit non-zero naming the first failed check.
    CheckBoundary {
        /// Also probe egress from inside the boundary.
        #[arg(long)]
        deep: bool,
    },
    /// Manage the signed policy bundle.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// Join a hub from an invitation URL or code printed by `tracon mesh invite`.
    Enroll {
        invitation: String,
        /// This node's name as other nodes will see it (default: hostname).
        #[arg(long)]
        name: Option<String>,
        /// The hub URL, if the invitation is a bare code rather than a URL.
        #[arg(long)]
        hub: Option<String>,
    },
    /// The mesh: this node's identity and its hub.
    #[command(subcommand)]
    Mesh(MeshCommand),
    /// Channels: the unit of tenancy, separated by key.
    #[command(subcommand)]
    Channel(ChannelCommand),
    /// The harness's node-owned state volume and its model credentials.
    #[command(subcommand)]
    Harness(HarnessCommand),
}

#[derive(Subcommand)]
enum HarnessCommand {
    /// Copy a model-credential database into the node-owned volume, once.
    /// Defaults to the operator's own `~/.omp/agent/agent.db`.
    ImportCredentials {
        #[arg(long)]
        from: Option<std::path::PathBuf>,
        /// Replace a store already in the volume.
        #[arg(long)]
        force: bool,
    },
    /// Run the harness image interactively on the default network with only
    /// the node-owned volume mounted, to log in where no operator install
    /// exists. Nothing else of this host is visible to it.
    Shell,
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
    /// Invite another node: prints a code and URL, waits for it to answer,
    /// shows its fingerprint for you to confirm, then admits it and hands off
    /// the channel keys and this node's policy bundle.
    Invite {
        /// Channels to hand off besides `@mesh`, comma-separated.
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
        /// How long the invitation stays open, in seconds (hub caps it).
        #[arg(long, default_value_t = 600)]
        ttl: u64,
        /// Admit without asking, once the fingerprint is shown. For scripts.
        #[arg(long)]
        yes: bool,
    },
    /// Hand channel keys to a node that is already a member.
    Admit {
        node_id: String,
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
    },
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
    /// Hand the signed bundle to every member of the hub.
    Push,
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
        Command::Setup { rebuild } => {
            let cfg = config::Config::load();
            let backend = boundary::backend_for(&cfg).await;
            backend.setup(&cfg, rebuild).await?;
            println!("{} boundary is in place", backend.kind());
            Ok(())
        }
        Command::Harness(cmd) => harness_command(cmd).await,
        Command::Policy(PolicyCommand::Push) => {
            let cfg = config::Config::load();
            let hub = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            let n = tracon::mesh::enroll::push_policy(&id, &hub).await?;
            println!(
                "policy bundle handed to {n} member{}",
                if n == 1 { "" } else { "s" }
            );
            Ok(())
        }
        Command::Policy(cmd) => policy_command(cmd),
        Command::Enroll {
            invitation,
            name,
            hub,
        } => enroll_command(invitation, name, hub).await,
        Command::Mesh(cmd) => mesh_command(cmd).await,
        Command::Channel(cmd) => channel_command(cmd),
        Command::CheckBoundary { deep } => {
            let cfg = config::Config::load();
            let report = boundary::backend_for(&cfg)
                .await
                .check_all(&cfg, deep)
                .await;
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
        // Handled in `main`, where the async hub call lives.
        PolicyCommand::Push => unreachable!("push is dispatched before policy_command"),
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
        MeshCommand::Invite { channels, ttl, yes } => invite_command(channels, ttl, yes).await,
        MeshCommand::Admit { node_id, channels } => {
            let cfg = config::Config::load();
            let hub = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let (id, _) = identity::load_or_generate()?;
            let store = tracon::store::Store::open(&config::Config::db_path())?;
            let members =
                tracon::mesh::client::MeshClient::get_once(&id, &hub, "/v0/members").await?;
            let m = members
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|m| m["node_id"].as_str() == Some(node_id.as_str()))
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{node_id} is not a member of the hub"))?;
            tracon::mesh::enroll::admit(
                &store,
                &id,
                &hub,
                &node_id,
                m["x25519_pub"].as_str().unwrap_or(""),
                m["name"].as_str().unwrap_or(""),
                &channels,
            )
            .await?;
            println!(
                "handed {} the keys for {}",
                &node_id[..16],
                channels.join(", ")
            );
            Ok(())
        }
    }
}

struct Stderr;
impl tracon::mesh::enroll::Progress for Stderr {
    fn say(&self, line: &str) {
        eprintln!("{line}");
    }
}

async fn enroll_command(
    invitation: String,
    name: Option<String>,
    hub: Option<String>,
) -> Result<()> {
    let (hub_from_url, code) = proto::enroll::parse_invite(&invitation)
        .ok_or_else(|| anyhow::anyhow!("that is not an invitation URL or code"))?;
    let hub = hub_from_url
        .or(hub)
        .ok_or_else(|| anyhow::anyhow!("pass the full invitation URL, or --hub with the code"))?;
    let (id, fresh) = tracon::mesh::identity::load_or_generate()?;
    if fresh {
        eprintln!("generated this node's identity");
    }
    let mut cfg = config::Config::try_load().map_err(|e| anyhow::anyhow!(e))?;
    let name = name.unwrap_or_else(|| cfg.node_name.clone());
    let store = std::sync::Arc::new(tracon::store::Store::open(&config::Config::db_path())?);
    let facts = format!("{} {}", std::env::consts::ARCH, std::env::consts::OS);
    let channels = tracon::mesh::enroll::accept(
        store,
        &id,
        &hub,
        &code,
        &name,
        &facts,
        std::time::Duration::from_secs(600),
        &Stderr,
    )
    .await?;
    cfg.mesh.hub_url = Some(hub.trim_end_matches('/').to_string());
    cfg.save()?;
    println!("enrolled as {name}; channels: {}", channels.join(", "));
    println!("next: tracon setup, then tracon serve");
    Ok(())
}

async fn invite_command(channels: Vec<String>, ttl: u64, yes: bool) -> Result<()> {
    use tracon::mesh::enroll;
    let cfg = config::Config::load();
    let hub = cfg
        .mesh
        .hub_url
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no hub configured; run tracon mesh init first"))?;
    let (id, _) = tracon::mesh::identity::load_or_generate()?;
    let store = tracon::store::Store::open(&config::Config::db_path())?;
    for c in &channels {
        if store.channel_get(c)?.is_none() {
            anyhow::bail!(
                "this node holds no key for channel {c}; tracon channel create {c} first"
            );
        }
    }
    let inv = enroll::open_invite(&id, &hub, &channels, Some(ttl)).await?;
    println!("invitation code: {}", inv.display_code());
    println!("on the new machine:");
    println!(
        "  curl -fsSL https://raw.githubusercontent.com/cosmicspork/tracon/main/install.sh | sh"
    );
    println!("  tracon enroll {}", inv.url);
    if let Some(qr) = enroll::qr_text(&inv.url) {
        println!("{qr}");
    }
    println!(
        "this node's fingerprint: {}   (the other side prints its own)",
        proto::enroll::fingerprint(&id.verifying_key().to_bytes())
    );
    println!("waiting for the other node… (expires in {ttl}s)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ttl);
    let req = loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the invitation expired");
        }
        if let Some(r) = enroll::poll_invite(&id, &hub, &inv.code).await? {
            break r;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    };
    let fp = proto::enroll::fingerprint_hex(&req.node_id).unwrap_or_default();
    println!();
    println!("received: {}  ({})", req.name, req.facts);
    println!("its fingerprint: {fp}");
    if !yes {
        eprint!("does that match what the other node printed? [y/N] ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            enroll::cancel_invite(&id, &hub, &inv.code).await.ok();
            anyhow::bail!("not admitted");
        }
    }
    enroll::admit(
        &store,
        &id,
        &hub,
        &req.node_id,
        &req.x25519_pub,
        &req.name,
        &inv.channels,
    )
    .await?;
    println!("admitted {} with {}", req.name, inv.channels.join(", "));
    Ok(())
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

async fn harness_command(cmd: HarnessCommand) -> Result<()> {
    use tracon::session::materialize;
    match cmd {
        HarnessCommand::ImportCredentials { from, force } => {
            let from = from.unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".omp/agent/agent.db")
            });
            if !from.exists() {
                anyhow::bail!("{} does not exist; pass --from <agent.db>", from.display());
            }
            let dest =
                materialize::import_credentials(&from, force).map_err(|e| anyhow::anyhow!(e))?;
            println!("credentials imported to {}", dest.display());
            Ok(())
        }
        HarnessCommand::Shell => {
            let cfg = config::Config::load();
            if cfg.runtime.kind != config::RuntimeKind::Podman {
                anyhow::bail!("`harness shell` needs the podman runtime; log in on a laptop and import the store");
            }
            let mounts = materialize::state_mounts()?;
            let mut c = std::process::Command::new("podman");
            c.args(["run", "--rm", "-it", "--network", "podman"]);
            c.args([
                "-e",
                &format!("OMP_STATE_DIR={}", materialize::HARNESS_STATE_TARGET),
            ]);
            for m in mounts {
                c.args(["-v", &format!("{}:{}", m.source, m.target)]);
            }
            c.args([cfg.boundary.harness_image.as_str(), "sh"]);
            eprintln!("harness shell: run `omp` to log in; only the node-owned volume is mounted");
            let status = c.status()?;
            if !status.success() {
                anyhow::bail!("shell exited with {status}");
            }
            Ok(())
        }
    }
}
