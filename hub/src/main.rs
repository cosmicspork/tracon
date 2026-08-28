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

#[tokio::main]
async fn main() {
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
