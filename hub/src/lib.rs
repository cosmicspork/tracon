//! tracon hub: the always-on relay. It verifies who is speaking, appends sealed
//! frames per channel with a monotonic sequence, pokes members to pull, and
//! brokers enrollment slots. It holds no channel keys and opens no frame.
//!
//! [`app`] builds the router over trait-object stores so tests run the same
//! router in-process on memory stores; [`main`](../main.rs) wires the
//! filesystem stores under the data directory.

pub mod auth;
pub mod nonce;
pub mod pokes;
pub mod routes;
pub mod store;

use std::sync::Arc;

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::CorsLayer;

use nonce::NonceStore;
use pokes::PokeHub;
use store::{EnrollSlots, FrameStore, Member, MemberStore, RateLimit};

#[derive(Clone, Debug)]
pub struct HubConfig {
    pub max_skew_secs: u64,
    pub retain_days: u64,
    pub max_channel_bytes: u64,
    pub enroll_ttl_secs: u64,
    /// Public enrollment posts allowed per source per minute.
    pub enroll_rate_per_min: u32,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            max_skew_secs: 300,
            retain_days: 14,
            max_channel_bytes: 256 * 1024 * 1024,
            enroll_ttl_secs: 600,
            enroll_rate_per_min: 10,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub frames: Arc<dyn FrameStore>,
    pub members: Arc<dyn MemberStore>,
    pub cfg: Arc<HubConfig>,
    pub nonces: Arc<NonceStore>,
    pub pokes: Arc<PokeHub>,
    pub enroll: Arc<EnrollSlots>,
    pub limiter: Arc<RateLimit>,
}

/// Admit the bootstrap keys (`TRACON_HUB_ADMIT`) into `@mesh` if absent.
pub fn admit_bootstrap(
    members: &dyn MemberStore,
    node_ids: &[String],
    now_ms: i64,
) -> std::io::Result<usize> {
    let mut added = 0;
    for id in node_ids {
        let id = id.trim().to_ascii_lowercase();
        if proto::keys::key32(&id).is_none() {
            tracing::warn!(node_id = %id, "TRACON_HUB_ADMIT entry is not a 32-byte hex key; skipped");
            continue;
        }
        if members.get(&id)?.is_none() {
            members.put(&Member {
                node_id: id.clone(),
                x25519_pub: String::new(),
                name: String::new(),
                channels: vec![proto::frame::MESH_CHANNEL.to_string()],
                admitted_ms: now_ms,
                admitted_by: "env".into(),
            })?;
            added += 1;
        }
    }
    Ok(added)
}

/// Build the hub router over the given stores.
pub fn app(frames: Arc<dyn FrameStore>, members: Arc<dyn MemberStore>, cfg: HubConfig) -> Router {
    let state = AppState {
        frames,
        members,
        cfg: Arc::new(cfg),
        nonces: Arc::new(NonceStore::new()),
        pokes: Arc::new(PokeHub::new()),
        enroll: Arc::new(EnrollSlots::new()),
        limiter: Arc::new(RateLimit::new()),
    };
    app_with_state(state)
}

pub fn app_with_state(state: AppState) -> Router {
    // Full paths, no `nest`: the auth middleware re-derives the signed
    // descriptor from the request URI, which `nest` would rewrite.
    let authed = Router::new()
        .route(
            "/v0/frames",
            post(routes::post_frame).get(routes::get_frames),
        )
        .route("/v0/events", get(routes::events))
        .route("/v0/members", get(routes::list_members))
        .route(
            "/v0/enroll/{code}",
            put(routes::open_enroll)
                .get(routes::take_enroll)
                .delete(routes::cancel_enroll),
        )
        .route("/v0/admit", post(routes::admit))
        .route("/v0/admit/{node_id}", delete(routes::remove_member))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    Router::new()
        .route("/health", get(routes::health))
        .route("/v0/info", get(routes::info))
        .route("/v0/enroll/{code}", post(routes::fill_enroll))
        .merge(authed)
        .layer(DefaultBodyLimit::max(auth::MAX_BODY))
        .layer(CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Apply retention to every channel. Returns frames dropped.
pub fn sweep(frames: &dyn FrameStore, cfg: &HubConfig, now_ms: i64) -> std::io::Result<usize> {
    let older_than = now_ms - (cfg.retain_days as i64) * 86_400_000;
    let mut dropped = 0;
    for ch in frames.channels()? {
        dropped += frames.prune(&ch, older_than, cfg.max_channel_bytes)?;
    }
    Ok(dropped)
}
