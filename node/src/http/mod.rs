pub mod api;
mod mcp;
mod spa;
mod stream;

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    routing::{get, post, put},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{
    adapter::{omp::OmpAdapter, HarnessAdapter},
    boundary,
    config::Config,
    session::Manager,
    store::{now_ms, NodeRow, Store},
    stream::Hub,
};

use api::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(api::health))
        .route("/api/node", get(api::get_node))
        .route("/api/node/refresh-models", post(api::refresh_models))
        .route("/api/nodes", get(api::list_nodes))
        .route(
            "/api/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        .route("/api/sessions/{id}", get(api::get_session))
        .route("/api/sessions/{id}/events", get(api::session_events))
        .route("/api/sessions/{id}/prompt", post(api::prompt))
        .route("/api/sessions/{id}/kill", post(api::kill))
        .route("/api/sessions/{id}/draft", put(api::put_draft))
        .route("/api/permissions/{id}/answer", post(api::answer_permission))
        .route("/api/queue", get(api::queue))
        .route("/api/stream", get(stream::stream))
        .fallback(spa::serve)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// What the harness can reach: a liveness probe and the node's tools. No
/// operator API, so a harness that finds the forward cannot drive sessions.
pub fn harness_router(state: AppState) -> Router {
    Router::new()
        .route("/harness/ping", get(|| async { "pong" }))
        .route("/mcp/{session_id}", post(mcp::handle))
        .with_state(state)
}

pub async fn serve(listen: SocketAddr) -> Result<()> {
    let cfg = Arc::new(Config::load());
    let store = Arc::new(Store::open(&Config::db_path()).context("open store")?);
    let hub = Hub::new();
    let adapter: Arc<dyn HarnessAdapter> = Arc::new(OmpAdapter::new(cfg.harness.version.clone()));
    let broker = Arc::new(crate::broker::Broker::load().unwrap_or_else(|e| {
        // A broken store must not silently broker nothing: say so loudly and
        // carry on without credentials.
        tracing::error!(error = %e, "credential store could not be read; brokering nothing");
        Default::default()
    }));
    if broker.is_empty() {
        tracing::info!(path = %crate::broker::Broker::path().display(), "no credentials; no tools offered");
    } else {
        tracing::info!(credentials = ?broker.names(), "credential broker loaded");
    }
    let tools = Arc::new(crate::mcp::Tools {
        broker,
        cfg: cfg.clone(),
    });

    let cleaned = crate::session::reconcile_after_restart(&store).await;
    if !cleaned.is_empty() {
        tracing::info!(
            sessions = cleaned.len(),
            "closed sessions left over from a previous run"
        );
    }
    let node_id = init_node(&store, &cfg, adapter.as_ref()).await?;
    let manager = Manager::new(
        store.clone(),
        hub.clone(),
        cfg.clone(),
        node_id.clone(),
        tools.clone(),
    );
    let state = AppState {
        manager,
        cfg: cfg.clone(),
        adapter,
        node_id,
        tools,
    };

    // The harness listener is separate from the operator's: it carries only the
    // MCP surface, and the gateway forwards to it from the internal network.
    let harness_listener = tokio::net::TcpListener::bind(cfg.gateway.harness_listen)
        .await
        .with_context(|| format!("bind {}", cfg.gateway.harness_listen))?;
    let harness_app = harness_router(state.clone());
    tracing::info!(listen = %cfg.gateway.harness_listen, "harness listener");
    tokio::spawn(async move {
        let _ = axum::serve(harness_listener, harness_app).await;
    });

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    tracing::info!(%listen, "serving");
    let manager = state.manager.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server")?;
    // The listener is closed; end every live session so no harness container
    // outlives the process that gates it.
    manager.shutdown_all().await;
    Ok(())
}

/// Verify the boundary and record what this node is. A node that fails the
/// check still serves the interface and says which check failed; it just does
/// not run harnesses.
async fn init_node(
    store: &Arc<Store>,
    cfg: &Arc<Config>,
    adapter: &dyn HarnessAdapter,
) -> Result<String> {
    let id = store
        .get_node_id()?
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
    let report = boundary::check_all(cfg, false).await;
    let failed = report.first_failure().cloned();
    let ready = failed.is_none();
    if let Some(f) = &failed {
        tracing::warn!(check = f.id.as_str(), detail = %f.detail, "refusing to run harnesses");
    }

    let (found, models) = if ready {
        let selinux = boundary::selinux_enabled().await;
        let mut spec = crate::runner::podman::RunSpec::from_config(cfg, selinux);
        // The probe opens a real session, so it needs the credential store the
        // harness reads; without it `session/new` fails and the model list is
        // silently empty.
        spec.extra_mounts = crate::session::materialize::state_mounts().unwrap_or_default();
        let runner = crate::runner::podman::PodmanRunner::new(spec);
        let found = adapter.version(&runner).await.ok().map(|v| v.found);
        let models = adapter.probe_models(&runner).await.unwrap_or_default();
        (found, models)
    } else {
        (None, Vec::new())
    };

    store.put_node(&NodeRow {
        id: id.clone(),
        name: cfg.node_name.clone(),
        state: if ready { "ready" } else { "refused" }.into(),
        failed_check: failed.as_ref().map(|f| f.id.as_str().to_string()),
        failed_detail: failed.as_ref().map(|f| f.detail.clone()),
        harness_id: adapter.id().into(),
        harness_pinned: adapter.pinned_version().into(),
        harness_found: found,
        models_json: serde_json::to_string(&models).ok(),
        checked_at_ms: Some(now_ms()),
    })?;
    Ok(id)
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await.ok();
    tracing::info!("shutting down");
}
