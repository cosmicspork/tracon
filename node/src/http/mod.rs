mod spa;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{routing::get, Json, Router};
use tower_http::trace::TraceLayer;

pub async fn serve(listen: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/api/health", get(health))
        .fallback(spa::serve)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    tracing::info!(%listen, "serving");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server")
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
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
