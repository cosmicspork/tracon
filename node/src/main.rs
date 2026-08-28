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
    /// The credential store: what the node brokers, sealed under its identity.
    #[command(subcommand)]
    Credential(CredentialCommand),
    /// Documents on the running node's corpus (talks to `tracon serve`).
    #[command(subcommand)]
    Doc(DocCommand),
    /// Memories on the running node's corpus (talks to `tracon serve`).
    #[command(subcommand)]
    Memory(MemoryCommand),
    /// The work ledger: items, dependencies, ready work.
    #[command(subcommand)]
    Work(WorkCommand),
    /// Who may reach this node's API from off this machine.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Run the node under the platform's supervisor, so it survives a logout,
    /// a crash, and a reboot.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Approvals and tokens per accepted change, human and agent time.
    Metrics {
        #[arg(long)]
        channel: Option<String>,
        /// Window, in days (default 30).
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// The trail behind a commit: model, prompts, approval, policy version.
    Provenance { sha: String },
}

#[derive(Subcommand)]
enum DocCommand {
    /// Import a notebook directory: flat `<kind>-<slug>.md` files.
    Import {
        dir: std::path::PathBuf,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// List documents, optionally on one channel.
    Ls {
        #[arg(long)]
        channel: Option<String>,
    },
    /// Print a document's body.
    Get {
        slug: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Create or replace a document from a file (or stdin with `-`).
    Put {
        slug: String,
        file: std::path::PathBuf,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Delete a document.
    Rm {
        slug: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Write every document on a channel to a directory as `<slug>.md`.
    Export {
        dir: std::path::PathBuf,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Write the unit and start the node. Safe to run again after an upgrade.
    Install,
    /// Stop the node and remove the unit. State and credentials are untouched.
    Uninstall,
    /// What the supervisor says about it.
    Status,
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Issue an operator token, printed once. Until one exists this node
    /// answers only to loopback; with one, a client that presents it gets a
    /// cookie and can reach the node from anywhere.
    Issue,
    /// Remove the operator token and log every client out: loopback only again.
    Revoke,
    /// What is logged in, and whether a token is set at all.
    Sessions,
}

#[derive(Subcommand)]
enum WorkCommand {
    /// Add an item.
    Add {
        title: String,
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "")]
        body: String,
        /// Items this one waits on, comma-separated ids (prefixes are fine).
        #[arg(long, value_delimiter = ',')]
        dep: Vec<String>,
        #[arg(long, default_value_t = 0)]
        priority: i64,
    },
    /// List the ledger with derived readiness, in the ready order.
    Ls {
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        project: Option<String>,
        /// ready | blocked | open | closed
        #[arg(long)]
        state: Option<String>,
    },
    /// Ready work: what a session may pick, in order.
    Ready {
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one item, its sessions, and what was discovered from it.
    Show { id: String },
    /// Close an item; a session holding it ends at its next turn end.
    Close { id: String },
    /// Add a dependency: `dep <id> --on <id>`.
    Dep {
        id: String,
        #[arg(long)]
        on: String,
    },
    /// Delete an item.
    Rm { id: String },
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// List memories on a channel.
    Ls {
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long)]
        state: Option<String>,
    },
    /// Add a directive (or another kind with --kind).
    Add {
        body: String,
        #[arg(long, default_value = "personal")]
        channel: String,
        #[arg(long, default_value = "directive")]
        kind: String,
        #[arg(long, default_value = "global")]
        scope: String,
        #[arg(long)]
        scope_ref: Option<String>,
    },
    /// Remove a memory by id.
    Rm { id: String },
    /// Recall, as a session would.
    Recall {
        query: String,
        #[arg(long, default_value = "personal")]
        channel: String,
    },
    /// Build the promotion batches now instead of tonight.
    Batch {
        #[arg(long)]
        now: bool,
    },
}

#[derive(Subcommand)]
enum CredentialCommand {
    /// Names, kinds, and bindings. Never values.
    Ls,
    /// Add the credentials from a plaintext TOML file (owner-only), sealing
    /// them into the store. Existing names are replaced.
    Import { file: std::path::PathBuf },
    /// Remove one credential.
    Rm { name: String },
    /// Hand a credential to another member node, direct-sealed over the hub.
    /// The credential must list that node in `nodes`, or it is refused there.
    Share {
        name: String,
        #[arg(long)]
        to: String,
    },
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
    /// Revoke a member on the hub (a lost or retired node). Allowed for the
    /// node that admitted it, or for the node itself.
    Remove {
        /// The member's node id.
        node_id: String,
    },
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
    /// Set bindings on a channel: `key=value` pairs, dotted keys nest
    /// (`phases.review.model=m`, `ceiling_tokens_per_day=2000000`,
    /// `key=` removes). Re-handed to every member on a mesh.
    Bind {
        name: String,
        #[arg(required = true)]
        pairs: Vec<String>,
    },
    /// Create a channel: mint its key here. Other nodes get it by enrollment.
    Create { name: String },
    /// The channels this node holds keys for.
    List,
    /// Share channels with the hub's replica: it can then open and index
    /// them, and run the nightly batch for them. Never do this for a work
    /// channel; the hub is rented infrastructure.
    Share {
        #[arg(value_delimiter = ',')]
        channels: Vec<String>,
        #[arg(long)]
        hub: bool,
    },
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
        Command::Credential(cmd) => credential_command(cmd).await,
        Command::Doc(cmd) => doc_command(cmd).await,
        Command::Memory(cmd) => memory_command(cmd).await,
        Command::Work(cmd) => work_command(cmd).await,
        Command::Auth(cmd) => auth_command(cmd).await,
        Command::Service(cmd) => match cmd {
            ServiceCommand::Install => tracon::service::install(),
            ServiceCommand::Uninstall => tracon::service::uninstall(),
            ServiceCommand::Status => tracon::service::status(),
        },
        Command::Metrics { channel, days } => {
            use reqwest::Method;
            let since = tracon::store::now_ms() - days.max(1) * 86_400_000;
            let mut q = format!("/api/metrics?since_ms={since}");
            if let Some(c) = channel {
                q.push_str(&format!("&channel={c}"));
            }
            let v = node_call(Method::GET, &q, None, None).await?;
            println!("{}", v["note"].as_str().unwrap_or(""));
            for c in v["channels"].as_array().cloned().unwrap_or_default() {
                let f = |k: &str| {
                    c[k].as_f64()
                        .map(|x| format!("{x:.1}"))
                        .unwrap_or_else(|| "—".into())
                };
                println!(
                    "{:<12} accepted {:>3}  rejected {:>3}  approvals/accepted {:>6}  tokens/accepted {:>10}  tokens {:>10}  cost {}  human {}s  agent {}s  sessions {}",
                    c["channel"].as_str().unwrap_or(""),
                    c["accepted_changes"],
                    c["rejected_changes"],
                    f("approvals_per_accepted_change"),
                    f("tokens_per_accepted_change"),
                    c["tokens"],
                    c["cost_usd"]
                        .as_f64()
                        .map(|x| format!("${x:.2}"))
                        .unwrap_or_else(|| "unpriced".into()),
                    f("human_seconds"),
                    f("agent_seconds"),
                    c["sessions"],
                );
            }
            Ok(())
        }
        Command::Provenance { sha } => {
            use reqwest::Method;
            let v = node_call(Method::GET, &format!("/api/provenance/{sha}"), None, None).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
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
        Command::Channel(cmd) => channel_command(cmd).await,
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
        MeshCommand::Remove { node_id } => {
            let cfg = config::Config::load();
            let hub = cfg.mesh.hub_url.clone().ok_or_else(|| {
                anyhow::anyhow!("no hub configured; run tracon mesh init or tracon enroll")
            })?;
            let (id, _) = identity::load_or_generate()?;
            tracon::mesh::enroll::remove_member(&id, &hub, &node_id).await?;
            println!("removed {node_id}");
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
                &tracon::broker::Broker::load(&id.credential_store_key())?.bound_to(&node_id),
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
        &tracon::broker::Broker::load(&id.credential_store_key())?.bound_to(&req.node_id),
    )
    .await?;
    println!("admitted {} with {}", req.name, inv.channels.join(", "));
    Ok(())
}

async fn channel_command(cmd: ChannelCommand) -> Result<()> {
    let store = tracon::store::Store::open(&config::Config::db_path())?;
    match cmd {
        ChannelCommand::Share { channels, hub } => {
            if !hub {
                anyhow::bail!("only --hub is supported; a node gets channels by enrollment");
            }
            let cfg = config::Config::load();
            let hub_url = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            let n = tracon::mesh::enroll::share_with_hub(&store, &id, &hub_url, &channels).await?;
            println!("shared {} with the hub's replica", n.join(", "));
            Ok(())
        }
        ChannelCommand::Create { name } => {
            if !proto::frame::valid_channel(&name) {
                anyhow::bail!("channel names are lowercase [a-z0-9@._-], at most 64 characters");
            }
            let (id, _) = tracon::mesh::identity::load_or_generate()?;
            create_channel(&store, &id, &name)?;
            let cfg = config::Config::load();
            if let Some(hub) = &cfg.mesh.hub_url {
                // So an invite can hand it off: the hub only lets a node grant
                // a channel it is recorded in.
                match tracon::mesh::enroll::sync_own_channels(&store, &id, hub, &cfg.node_name)
                    .await
                {
                    Ok(_) => println!("hub record updated"),
                    Err(e) => {
                        println!("hub record not updated ({e}); it is synced on the next invite")
                    }
                }
            }
            println!("created channel {name}; hand its key to other nodes with tracon mesh invite");
            Ok(())
        }
        ChannelCommand::Bind { name, pairs } => {
            use reqwest::Method;
            use serde_json::Value;
            let mut patch = serde_json::Map::new();
            for pair in pairs {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("{pair}: expected key=value"))?;
                let value = if v.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()))
                };
                patch.insert(k.to_string(), value);
            }
            let v = node_call(
                Method::PUT,
                &format!("/api/channels/{name}/bindings"),
                Some(Value::Object(patch)),
                None,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&v["bindings"])?);
            if let Some(n) = v["handed_to"].as_u64() {
                println!("handed to {n} member{}", if n == 1 { "" } else { "s" });
            }
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

/// The running node's operator API. The CLI goes through it rather than the
/// store so every write is published to the mesh like any other.
fn node_url() -> String {
    std::env::var("TRACON_URL").unwrap_or_else(|_| "http://127.0.0.1:7420".into())
}

async fn node_call(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    if_match: Option<&str>,
) -> Result<serde_json::Value> {
    let url = format!("{}{path}", node_url());
    let mut req = reqwest::Client::new().request(method, &url);
    // A node on this machine knows the caller is the operator by the loopback
    // address. One reached over the network wants the token; the CLI holds no
    // cookie jar, so it presents the token itself.
    if let Ok(token) = std::env::var("TRACON_TOKEN") {
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    if let Some(h) = if_match {
        req = req.header("if-match", h);
    }
    let res = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{url}: {e} (is `tracon serve` running? set TRACON_URL)"))?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or(&text).to_string();
        anyhow::bail!("{status}: {msg}");
    }
    Ok(v)
}

async fn auth_command(cmd: AuthCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        AuthCommand::Issue => {
            use base64::Engine;
            use rand::RngCore;
            let mut raw = [0u8; 32];
            rand::rng().fill_bytes(&mut raw);
            let token = format!(
                "trc1.{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
            );
            // Only the hash is sent. The node cannot show the token again, and
            // a database read cannot recover it.
            let hash = tracon::http::auth::hash(&token);
            node_call(
                Method::POST,
                "/api/auth/token",
                Some(serde_json::json!({ "token_hash": hash })),
                None,
            )
            .await?;
            println!("{token}");
            println!();
            println!("Shown once. Any client that presents it gets a cookie for this node.");
            println!("Every client logged in with the previous token has been logged out.");
            Ok(())
        }
        AuthCommand::Revoke => {
            node_call(Method::DELETE, "/api/auth/token", None, None).await?;
            println!("token removed; this node answers only to loopback again");
            Ok(())
        }
        AuthCommand::Sessions => {
            let v = node_call(Method::GET, "/api/auth/sessions", None, None).await?;
            if v["configured"].as_bool() != Some(true) {
                println!("no operator token set; this node answers only to loopback");
            }
            let clients = v["clients"].as_array().cloned().unwrap_or_default();
            if clients.is_empty() {
                println!("no clients logged in");
            }
            for c in clients {
                println!(
                    "{}  last seen {}  {}",
                    c["id"].as_str().unwrap_or(""),
                    c["last_seen_ms"].as_i64().unwrap_or(0),
                    c["user_agent"].as_str().unwrap_or("-"),
                );
            }
            Ok(())
        }
    }
}

async fn doc_command(cmd: DocCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        DocCommand::Import { dir, channel } => {
            let (docs, skipped) = tracon::corpus::import::read_dir(&dir)?;
            for s in &skipped {
                eprintln!("skipped {s}");
            }
            let mut n = 0;
            for d in docs {
                node_call(
                    Method::PUT,
                    &format!("/api/docs/{channel}/{}", d.slug),
                    Some(serde_json::json!({ "body": d.body })),
                    None,
                )
                .await?;
                println!("{:<14} {}", d.kind, d.slug);
                n += 1;
            }
            println!(
                "{n} document{} imported into {channel}",
                if n == 1 { "" } else { "s" }
            );
            Ok(())
        }
        DocCommand::Ls { channel } => {
            let q = channel.map(|c| format!("?channel={c}")).unwrap_or_default();
            let v = node_call(Method::GET, &format!("/api/docs{q}"), None, None).await?;
            for d in v["docs"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:<10} {:<14} {:<36} {}",
                    d["channel"].as_str().unwrap_or(""),
                    d["kind"].as_str().unwrap_or(""),
                    d["slug"].as_str().unwrap_or(""),
                    d["title"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        DocCommand::Get { slug, channel } => {
            let v = node_call(
                Method::GET,
                &format!("/api/docs/{channel}/{slug}"),
                None,
                None,
            )
            .await?;
            print!("{}", v["body"].as_str().unwrap_or(""));
            Ok(())
        }
        DocCommand::Put {
            slug,
            file,
            channel,
        } => {
            let body = if file.as_os_str() == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                std::fs::read_to_string(&file)?
            };
            let v = node_call(
                Method::PUT,
                &format!("/api/docs/{channel}/{slug}"),
                Some(serde_json::json!({ "body": body })),
                None,
            )
            .await?;
            println!("{slug} {}", v["hash"].as_str().unwrap_or(""));
            Ok(())
        }
        DocCommand::Rm { slug, channel } => {
            node_call(
                Method::DELETE,
                &format!("/api/docs/{channel}/{slug}"),
                None,
                None,
            )
            .await?;
            println!("removed {slug}");
            Ok(())
        }
        DocCommand::Export { dir, channel } => {
            std::fs::create_dir_all(&dir)?;
            let v = node_call(
                Method::GET,
                &format!("/api/docs?channel={channel}"),
                None,
                None,
            )
            .await?;
            let mut n = 0;
            for d in v["docs"].as_array().cloned().unwrap_or_default() {
                let slug = d["slug"].as_str().unwrap_or("").to_string();
                let full = node_call(
                    Method::GET,
                    &format!("/api/docs/{channel}/{slug}"),
                    None,
                    None,
                )
                .await?;
                std::fs::write(
                    dir.join(format!("{slug}.md")),
                    full["body"].as_str().unwrap_or(""),
                )?;
                n += 1;
            }
            println!(
                "{n} document{} written to {}",
                if n == 1 { "" } else { "s" },
                dir.display()
            );
            Ok(())
        }
    }
}

/// Resolve an id prefix against the channel's ledger.
async fn work_resolve(channel: &str, prefix: &str) -> Result<String> {
    use reqwest::Method;
    if prefix.len() == 64 {
        return Ok(prefix.to_string());
    }
    let v = node_call(
        Method::GET,
        &format!("/api/work?channel={channel}"),
        None,
        None,
    )
    .await?;
    let hits: Vec<String> = v["items"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i["id"].as_str())
        .filter(|id| id.starts_with(prefix))
        .map(str::to_string)
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!("no work item starts with {prefix} on {channel}"),
        _ => anyhow::bail!("{prefix} is ambiguous on {channel}"),
    }
}

fn work_line(i: &serde_json::Value) -> String {
    let id = i["id"].as_str().unwrap_or("");
    let state = match i["readiness"]["state"].as_str().unwrap_or("") {
        "ready" if i["session_id"].is_string() => "in session".to_string(),
        "blocked" => {
            let by: Vec<String> = i["readiness"]["by"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|b| match b["kind"].as_str() {
                            Some("cycle") => "cycle".to_string(),
                            _ => b["id"].as_str().unwrap_or("?")[..8].to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!("blocked by {}", by.join(","))
        }
        other => other.to_string(),
    };
    format!(
        "{}  p{:<2} {:<24} {}",
        &id[..8.min(id.len())],
        i["priority"].as_i64().unwrap_or(0),
        state,
        i["title"].as_str().unwrap_or("")
    )
}

async fn work_command(cmd: WorkCommand) -> Result<()> {
    use reqwest::Method;
    use serde_json::json;
    match cmd {
        WorkCommand::Add {
            title,
            channel,
            project,
            body,
            dep,
            priority,
        } => {
            let mut deps = Vec::new();
            for d in dep.iter().filter(|d| !d.trim().is_empty()) {
                deps.push(work_resolve(&channel, d.trim()).await?);
            }
            let v = node_call(
                Method::POST,
                "/api/work",
                Some(json!({
                    "channel": channel, "project_id": project, "title": title, "body": body,
                    "deps": deps, "priority": priority,
                })),
                None,
            )
            .await?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(())
        }
        WorkCommand::Ls {
            channel,
            project,
            state,
        } => {
            let mut q = format!("/api/work?channel={channel}");
            if let Some(p) = project {
                q.push_str(&format!("&project_id={p}"));
            }
            if let Some(s) = state {
                q.push_str(&format!("&state={s}"));
            }
            let v = node_call(Method::GET, &q, None, None).await?;
            for i in v["items"].as_array().cloned().unwrap_or_default() {
                println!("{}", work_line(&i));
            }
            Ok(())
        }
        WorkCommand::Ready { channel, project } => {
            let mut q = format!("/api/work/ready?channel={channel}");
            if let Some(p) = project {
                q.push_str(&format!("&project_id={p}"));
            }
            let v = node_call(Method::GET, &q, None, None).await?;
            for i in v["items"].as_array().cloned().unwrap_or_default() {
                println!("{}", work_line(&i));
            }
            Ok(())
        }
        WorkCommand::Show { id } => {
            let v = node_call(Method::GET, &format!("/api/work/{id}"), None, None).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        WorkCommand::Close { id } => {
            let v = node_call(
                Method::PUT,
                &format!("/api/work/{id}"),
                Some(json!({ "state": "closed" })),
                None,
            )
            .await?;
            println!("closed {}", v["id"].as_str().unwrap_or(&id));
            Ok(())
        }
        WorkCommand::Dep { id, on } => {
            let cur = node_call(Method::GET, &format!("/api/work/{id}"), None, None).await?;
            let mut deps: Vec<String> = cur["item"]["deps"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            deps.push(on);
            node_call(
                Method::PUT,
                &format!("/api/work/{id}"),
                Some(json!({ "deps": deps })),
                None,
            )
            .await?;
            println!("ok");
            Ok(())
        }
        WorkCommand::Rm { id } => {
            node_call(Method::DELETE, &format!("/api/work/{id}"), None, None).await?;
            println!("removed {id}");
            Ok(())
        }
    }
}

async fn memory_command(cmd: MemoryCommand) -> Result<()> {
    use reqwest::Method;
    match cmd {
        MemoryCommand::Ls { channel, state } => {
            let q = state.map(|s| format!("&state={s}")).unwrap_or_default();
            let v = node_call(
                Method::GET,
                &format!("/api/memories?channel={channel}{q}"),
                None,
                None,
            )
            .await?;
            for m in v["memories"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}  {:<9} {:<9} {:<8} {:.2}  {}",
                    &m["id"].as_str().unwrap_or("")[..8.min(m["id"].as_str().unwrap_or("").len())],
                    m["kind"].as_str().unwrap_or(""),
                    m["state"].as_str().unwrap_or(""),
                    m["scope"].as_str().unwrap_or(""),
                    m["confidence"].as_f64().unwrap_or(0.0),
                    m["body"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                );
            }
            Ok(())
        }
        MemoryCommand::Add {
            body,
            channel,
            kind,
            scope,
            scope_ref,
        } => {
            let v = node_call(
                Method::POST,
                "/api/memories",
                Some(serde_json::json!({ "channel": channel, "kind": kind, "scope": scope, "scope_ref": scope_ref, "body": body })),
                None,
            )
            .await?;
            println!("{}", v["id"].as_str().unwrap_or(""));
            Ok(())
        }
        MemoryCommand::Rm { id } => {
            node_call(Method::DELETE, &format!("/api/memories/{id}"), None, None).await?;
            println!("removed {id}");
            Ok(())
        }
        MemoryCommand::Batch { now } => {
            if !now {
                anyhow::bail!("pass --now; the nightly batch runs on its own");
            }
            let v = node_call(Method::POST, "/api/promotions/batch", None, None).await?;
            let n = v["created"].as_array().map(|a| a.len()).unwrap_or(0);
            println!("{n} batch{} created", if n == 1 { "" } else { "es" });
            Ok(())
        }
        MemoryCommand::Recall { query, channel } => {
            let q = urlencoding(&query);
            let v = node_call(
                Method::GET,
                &format!("/api/memories?channel={channel}&q={q}"),
                None,
                None,
            )
            .await?;
            for h in v["hits"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:<9} {:<28} {}",
                    h["kind"].as_str().unwrap_or(""),
                    h["slug"].as_str().unwrap_or(""),
                    h["text"]
                        .as_str()
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                );
            }
            Ok(())
        }
    }
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn credential_command(cmd: CredentialCommand) -> Result<()> {
    use tracon::broker::Broker;
    let (id, _) = tracon::mesh::identity::load_or_generate()?;
    let key = id.credential_store_key();
    let mut broker = Broker::load(&key)?;
    match cmd {
        CredentialCommand::Ls => {
            if broker.is_empty() {
                println!("no credentials ({})", Broker::path().display());
            }
            for (name, c) in broker.iter() {
                let mut line = format!("{name:<20} {:<8}", c.kind);
                if let Some(p) = &c.provider {
                    line.push_str(&format!(" provider={p}"));
                }
                line.push_str(&format!(" channels={}", c.channels.join(",")));
                if !c.nodes.is_empty() {
                    line.push_str(&format!(
                        " nodes={}",
                        c.nodes
                            .iter()
                            .map(|n| &n[..8.min(n.len())])
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                }
                if let Some(i) = &c.identity {
                    line.push_str(&format!(" identity={i}"));
                }
                if let Some(e) = c.expires_ms {
                    line.push_str(&format!(" expires_ms={e}"));
                }
                println!("{line}");
            }
            Ok(())
        }
        CredentialCommand::Import { file } => {
            let text = std::fs::read_to_string(&file)?;
            let incoming = Broker::parse_plain(&file, &text)?;
            let mut n = 0;
            for (name, c) in incoming.iter() {
                broker.put(name, c.clone());
                n += 1;
            }
            broker.save(&key)?;
            println!(
                "{n} credential{} sealed into {}",
                if n == 1 { "" } else { "s" },
                Broker::path().display()
            );
            Ok(())
        }
        CredentialCommand::Rm { name } => {
            if !broker.remove(&name) {
                anyhow::bail!("no credential named {name}");
            }
            broker.save(&key)?;
            println!("removed {name}");
            Ok(())
        }
        CredentialCommand::Share { name, to } => {
            let cred = broker
                .get(&name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no credential named {name}"))?;
            if !cred.nodes.iter().any(|n| n == &to) {
                anyhow::bail!("{name} does not list {to} in `nodes`; it would be refused there");
            }
            let cfg = config::Config::load();
            let hub = cfg
                .mesh
                .hub_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no hub configured"))?;
            let members =
                tracon::mesh::client::MeshClient::get_once(&id, &hub, "/v0/members").await?;
            let m = members
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|m| m["node_id"].as_str() == Some(to.as_str()))
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{to} is not a member of the hub"))?;
            let grantee = proto::keys::key32(m["x25519_pub"].as_str().unwrap_or(""))
                .map(x25519_dalek::PublicKey::from)
                .ok_or_else(|| anyhow::anyhow!("the node's sealing key is malformed"))?;
            let payload = proto::frame::Payload::CredentialHandoff {
                credentials: Broker::handoff_rows(&[(name.clone(), cred)]),
            };
            tracon::mesh::enroll::post_direct(&id, &hub, &to, &grantee, &payload).await?;
            println!("handed {name} to {}", &to[..16.min(to.len())]);
            Ok(())
        }
    }
}
