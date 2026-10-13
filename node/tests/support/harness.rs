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
    /// The tool surface behind both routers, and with it the working
    /// agreements. A test whose subject is what a tool *does*, rather than
    /// that it is asked, widens these the way `work_tools.rs` does.
    pub tools: Arc<Tools>,
    /// What the interface would be watching: tests that care about a live
    /// client, rather than a fresh page load, assert on these.
    pub bus: Bus,
    /// The state behind the routers, for a test that drives what the mesh
    /// would (`CommandExecutor`).
    pub state: AppState,
}

pub async fn harness() -> Harness {
    harness_with(Config::default()).await
}

pub async fn harness_with(cfg: Config) -> Harness {
    harness_on(cfg, Arc::new(tracon::runner::local::LocalBackend)).await
}

/// As `harness_with`, on a backend of the test's own: one that can build
/// images, say, which the local backend cannot.
pub async fn harness_on(cfg: Config, backend: Arc<dyn tracon::boundary::Backend>) -> Harness {
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
    let bus = Bus::new();
    let manager = Manager::new(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        backend,
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
        tools: tools.clone(),
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    Harness {
        harness: tracon::http::harness_router(state.clone()),
        operator: tracon::http::router(state.clone()),
        state,
        store,
        manager,
        tools,
        bus,
    }
}
