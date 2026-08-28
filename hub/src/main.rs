//! tracon hub binary. Configuration by environment:
//!
//! - `TRACON_HUB_ADDR` — listen address (default `127.0.0.1:8080`; the image sets `0.0.0.0:8080`).
//! - `TRACON_HUB_DATA_DIR` — durable frames and members. Unset means in memory, with a loud warning.
//! - `TRACON_HUB_ADMIT` — comma-separated node ids admitted into `@mesh` at startup (bootstrap).
//! - `TRACON_HUB_MAX_SKEW_SECS` (300), `TRACON_HUB_RETAIN_DAYS` (14),
//!   `TRACON_HUB_MAX_CHANNEL_BYTES` (268435456), `TRACON_HUB_ENROLL_TTL_SECS` (600).
//! - `RUST_LOG` — tracing filter (default `hub=info,tower_http=info`).

use std::sync::Arc;
use std::time::Duration;

use hub::pokes::PokeHub;
use hub::store::{FrameStore, FsFrames, FsMembers, MemberStore, MemoryFrames, MemoryMembers};
use hub::{admit_bootstrap, admit_self, app_with_state, state_for, sweep, HubConfig};
use tracing_subscriber::EnvFilter;

fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an integer, got {v:?}")),
        Err(_) => default,
    }
}

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tracon-hub", version, about = "tracon hub: relay and replica")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hub (the default).
    Serve,
    /// Generate the snapshot restore key: prints the seed once, keeps the
    /// public half under the data directory.
    SnapshotKey,
    /// Take a snapshot now, to S3 (`TRACON_HUB_SNAPSHOT_*`) or a directory.
    Snapshot {
        /// A directory instead of S3.
        #[arg(long)]
        to: Option<std::path::PathBuf>,
    },
    /// Restore a snapshot into an empty directory.
    Restore {
        /// The object key, or `latest`.
        #[arg(long, default_value = "latest")]
        key: String,
        /// A directory instead of S3.
        #[arg(long)]
        from: Option<std::path::PathBuf>,
        #[arg(long)]
        into: std::path::PathBuf,
        /// The seed `snapshot-key` printed.
        #[arg(long, env = "TRACON_HUB_RESTORE_SEED")]
        seed_hex: String,
    },
}

fn data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("TRACON_HUB_DATA_DIR").unwrap_or_else(|_| "/data".into()),
    )
}

fn objects(dir: Option<std::path::PathBuf>) -> Box<dyn hub::snapshot::ObjectStore> {
    match dir {
        Some(d) => Box::new(hub::snapshot::FsObjects::new(&d).expect("snapshot directory")),
        None => {
            let cfg = hub::snapshot::s3::S3Config::from_env()
                .expect("TRACON_HUB_SNAPSHOT_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY, or --to/--from a directory");
            Box::new(hub::snapshot::s3::S3::new(cfg))
        }
    }
}

fn snapshot_prefix() -> String {
    std::env::var("TRACON_HUB_SNAPSHOT_PREFIX").unwrap_or_else(|_| "tracon-hub".into())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::SnapshotKey => {
            let (seed, path) =
                hub::snapshot::create_restore_key(&data_dir()).expect("write restore key");
            println!("restore seed (keep it somewhere the cluster is not; shown once):\n{seed}");
            println!("public half written to {}", path.display());
        }
        Command::Snapshot { to } => {
            let dir = data_dir();
            let recipient = hub::snapshot::recipient(&dir)
                .expect("no restore key: run `tracon-hub snapshot-key` first");
            let store = objects(to);
            let key = hub::snapshot::take(
                &dir,
                &recipient,
                store.as_ref(),
                &snapshot_prefix(),
                hub::auth::now_ms(),
            )
            .expect("snapshot");
            println!("{key}");
        }
        Command::Restore {
            key,
            from,
            into,
            seed_hex,
        } => {
            let store = objects(from);
            let key = if key == "latest" {
                hub::snapshot::latest(store.as_ref(), &snapshot_prefix())
                    .expect("list")
                    .expect("no snapshots")
            } else {
                key
            };
            if into.exists()
                && std::fs::read_dir(&into)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false)
            {
                eprintln!(
                    "{} is not empty; refusing to restore over it",
                    into.display()
                );
                std::process::exit(2);
            }
            let written =
                hub::snapshot::restore(store.as_ref(), &key, &seed_hex, &into).expect("restore");
            println!(
                "{} restored: {} files into {}",
                key,
                written.len(),
                into.display()
            );
        }
    }
}

async fn serve() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("hub=info,tower_http=info")),
        )
        .init();

    let addr = std::env::var("TRACON_HUB_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let defaults = HubConfig::default();
    let cfg = HubConfig {
        max_skew_secs: env_u64("TRACON_HUB_MAX_SKEW_SECS", defaults.max_skew_secs),
        retain_days: env_u64("TRACON_HUB_RETAIN_DAYS", defaults.retain_days),
        max_channel_bytes: env_u64("TRACON_HUB_MAX_CHANNEL_BYTES", defaults.max_channel_bytes),
        enroll_ttl_secs: env_u64("TRACON_HUB_ENROLL_TTL_SECS", defaults.enroll_ttl_secs),
        enroll_rate_per_min: env_u64(
            "TRACON_HUB_ENROLL_RATE_PER_MIN",
            defaults.enroll_rate_per_min as u64,
        ) as u32,
    };

    let data_dir = std::env::var("TRACON_HUB_DATA_DIR").ok();
    let (frames, members): (Arc<dyn FrameStore>, Arc<dyn MemberStore>) = match &data_dir {
        Some(dir) => {
            let d = std::path::Path::new(dir);
            let f = FsFrames::new(d).expect("create data directory");
            let m = FsMembers::new(d).expect("create data directory");
            tracing::info!(data_dir = %dir, "durable filesystem stores");
            (Arc::new(f), Arc::new(m))
        }
        None => {
            tracing::warn!(
                "TRACON_HUB_DATA_DIR unset; frames and members are in memory and lost on restart"
            );
            (
                Arc::new(MemoryFrames::new()),
                Arc::new(MemoryMembers::new()),
            )
        }
    };
    let pokes = Arc::new(PokeHub::new());
    // The replica: on by default when there is somewhere durable to keep it.
    let replica_on = std::env::var("TRACON_HUB_REPLICA")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "off"))
        .unwrap_or(data_dir.is_some());
    let replica = match (&data_dir, replica_on) {
        (Some(dir), true) => {
            let d = std::path::Path::new(dir);
            let (identity, fresh) = hub::identity::load_or_generate(d).expect("hub identity");
            let r = hub::replica::Replica::open(
                d,
                identity,
                frames.clone(),
                members.clone(),
                pokes.clone(),
            )
            .expect("open hub.db");
            admit_self(members.as_ref(), r.as_ref(), hub::auth::now_ms()).expect("admit the hub");
            tracing::info!(hub_node_id = %r.node_id(), fresh_identity = fresh, "replica enabled");
            tokio::spawn(r.clone().run());
            let at = std::env::var("TRACON_HUB_PROMOTE_AT").unwrap_or_else(|_| "03:00".into());
            tokio::spawn(r.clone().nightly(at));
            Some(r)
        }
        _ => {
            tracing::info!("replica disabled; relay only");
            None
        }
    };

    let admits: Vec<String> = std::env::var("TRACON_HUB_ADMIT")
        .ok()
        .map(|s| {
            s.split(',')
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let added = admit_bootstrap(members.as_ref(), &admits, hub::auth::now_ms())
        .expect("admit bootstrap keys");
    let member_count = members.list().expect("list members").len();
    if member_count == 0 {
        tracing::warn!(
            "no members: set TRACON_HUB_ADMIT to the first node's id or nothing can connect"
        );
    } else {
        tracing::info!(
            members = member_count,
            bootstrapped = added,
            "members loaded"
        );
    }

    // Retention sweep: once at start, then hourly.
    {
        let frames = frames.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            loop {
                match sweep(frames.as_ref(), &cfg, hub::auth::now_ms()) {
                    Ok(n) if n > 0 => tracing::info!(dropped = n, "retention sweep"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "retention sweep failed"),
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }

    // Scheduled snapshots, when object storage and a restore key are set.
    if let (Some(dir), Some(s3)) = (&data_dir, hub::snapshot::s3::S3Config::from_env()) {
        let dir = std::path::PathBuf::from(dir);
        match hub::snapshot::recipient(&dir) {
            Some(recipient) => {
                let every = env_u64("TRACON_HUB_SNAPSHOT_EVERY_HOURS", 24);
                let keep = env_u64("TRACON_HUB_SNAPSHOT_KEEP", 14) as usize;
                let prefix = snapshot_prefix();
                tokio::task::spawn_blocking(move || {
                    let store = hub::snapshot::s3::S3::new(s3);
                    loop {
                        match hub::snapshot::take(&dir, &recipient, &store, &prefix, hub::auth::now_ms()) {
                            Ok(k) => {
                                tracing::info!(key = %k, "snapshot written");
                                if let Ok(removed) = hub::snapshot::prune(&store, &prefix, keep) {
                                    if !removed.is_empty() {
                                        tracing::info!(removed = removed.len(), "old snapshots pruned");
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "snapshot failed"),
                        }
                        std::thread::sleep(Duration::from_secs(every * 3600));
                    }
                });
            }
            None => tracing::warn!(
                "TRACON_HUB_SNAPSHOT_* set but no restore key; run `tracon-hub snapshot-key` (snapshots disabled)"
            ),
        }
    }

    let router = app_with_state(state_for(frames, members, cfg, pokes, replica));
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind listen address");
    tracing::info!(addr = %addr, contract_version = proto::CONTRACT_VERSION, "tracon hub listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}
