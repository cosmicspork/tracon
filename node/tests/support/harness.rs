//! A node's operator and harness routers over an in-memory store and the
//! fake adapter: what the MCP-tool tests start from.

#![allow(dead_code)]

use std::sync::Arc;

use tracon::{
    broker::Broker, config::Config, http::api::AppState, mcp::Tools, session::Manager,
    store::Store, stream::Bus,
};

use super::fake::FakeAdapter;

pub struct Harness {
    pub harness: axum::Router,
    pub operator: axum::Router,
    pub store: Arc<Store>,
    pub manager: Manager,
}

pub async fn harness() -> Harness {
    harness_with(Config::default()).await
}

pub async fn harness_with(cfg: Config) -> Harness {
    super::state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let cfg = Arc::new(cfg);
    let tools = Arc::new(Tools {
        broker: Broker::default().shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    let state = AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    Harness {
        harness: tracon::http::harness_router(state.clone()),
        operator: tracon::http::router(state),
        store,
        manager,
    }
}
