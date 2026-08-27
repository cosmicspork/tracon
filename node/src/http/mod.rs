pub mod api;
mod mcp;
mod spa;
mod stream;

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
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
    stream::Bus,
};

use api::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(api::health))
        .route("/api/node", get(api::get_node))
        .route("/api/node/refresh-models", post(api::refresh_models))
        .route("/api/nodes", get(api::list_nodes))
        .route("/api/mesh", get(api::get_mesh))
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
        .route("/api/reviews/{id}", get(api::get_review))
        .route("/api/reviews/{id}/verdict", post(api::decide_review))
        .route("/api/reviews/{id}/release", post(api::release_review))
        .route("/api/queue", get(api::queue))
        .route("/api/stream", get(stream::stream))
        .fallback(spa::serve)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// The hostname portion of a `Host`/`Origin` value, minus any scheme, port, or
/// IPv6 brackets. `http://[::1]:7420` → `::1`, `127.0.0.1:7420` → `127.0.0.1`.
fn hostname(value: &str) -> &str {
    let v = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    if let Some(rest) = v.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    v.rsplit_once(':').map(|(h, _)| h).unwrap_or(v)
}

/// Whether a `Host`/`Origin` host is one the operator API answers to: loopback,
/// or the exact address the node was told to bind. This is the DNS-rebinding
/// defence — a page on `evil.example` that resolves to `127.0.0.1` still sends
/// `Host: evil.example`, which is refused.
fn host_is_local(host: Option<&str>, bind: &str) -> bool {
    match host {
        // No Host header at all is not a browser (they always send one), so it
        // is not a rebinding vector; local non-browser clients are allowed.
        None => true,
        Some(h) => {
            let h = hostname(h);
            matches!(h, "localhost" | "127.0.0.1" | "::1") || h == bind
        }
    }
}

/// Reject cross-origin drivers of the operator API. Applied only to the operator
/// router; the harness router is reached from the gateway by container name and
/// must not carry this.
async fn local_only(bind: Arc<String>, req: Request, next: Next) -> Result<Response, StatusCode> {
    {
        let headers = req.headers();
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        if !host_is_local(host, &bind) {
            return Err(StatusCode::FORBIDDEN);
        }
        // An Origin, when present, must also be local: a cross-site POST carries
        // the attacker's Origin even when the request reaches loopback.
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if let Some(origin) = origin {
            if origin != "null" && !host_is_local(Some(origin), &bind) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }
    Ok(next.run(req).await)
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
    // A config that does not parse is fatal here, not a warning: a typo in
    // `[mesh]` must not quietly run this node unmeshed.
    let cfg = Arc::new(Config::try_load().map_err(|e| anyhow::anyhow!(e))?);
    let store = Arc::new(Store::open(&Config::db_path()).context("open store")?);
    let bus = Bus::new();
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
        session: Default::default(),
    });

    let (node_id, identity) = init_node(&store, &cfg, adapter.as_ref()).await?;
    let cleaned = crate::session::reconcile_after_restart(&store, &node_id).await;
    if !cleaned.is_empty() {
        tracing::info!(
            sessions = cleaned.len(),
            "closed sessions left over from a previous run"
        );
    }
    // A bundle that cannot be verified yields no rules, and no rules means every
    // request is asked. The failure mode of broken policy is more questions.
    let policy = Arc::new(match crate::policy::bundle::load() {
        Ok(p) => {
            tracing::info!(rules = p.rules.len(), "policy bundle verified");
            p
        }
        Err(crate::policy::bundle::BundleError::Io(e))
            if e.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::info!(
                path = %crate::policy::bundle::Paths::bundle().display(),
                "no policy bundle; every request will be asked"
            );
            Default::default()
        }
        Err(e) => {
            tracing::error!(error = %e, "policy bundle refused; every request will be asked");
            Default::default()
        }
    });

    let manager = Manager::new(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        node_id.clone(),
        tools.clone(),
        policy,
    );
    let _ = tools.session.set(crate::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    // With a hub configured, every frame this node publishes is tapped into
    // the mesh client's outbox, and peer state is pulled into the same tables.
    let mesh = cfg.mesh.hub_url.as_ref().map(|url| {
        let client = crate::mesh::client::MeshClient::new(
            identity,
            url,
            store.clone(),
            bus.clone(),
            cfg.clone(),
        );
        bus.with_tap(client.spawn());
        tracing::info!(hub = %url, "mesh client started");
        client
    });
    if mesh.is_none() {
        tracing::info!("no hub configured; this node is standalone (tracon enroll to join a mesh)");
    }
    let state = AppState {
        manager,
        cfg: cfg.clone(),
        adapter,
        node_id,
        tools,
        mesh,
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
    // A claim measures attention. One left behind by a client that vanished
    // would report a review as attended forever, so claims lapse.
    {
        let store = store.clone();
        let manager = state.manager.clone();
        let grace = std::time::Duration::from_secs(cfg.session.claim_grace_secs);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                tick.tick().await;
                let stale = store
                    .stale_claims(grace.as_millis() as i64)
                    .unwrap_or_default();
                if stale.is_empty() {
                    continue;
                }
                for id in stale {
                    let _ = store.release_review(&id);
                }
                manager.publish_queue().await;
            }
        });
    }

    tracing::info!(%listen, "serving");
    let manager = state.manager.clone();
    let bus = manager.bus().clone();
    // The operator API answers only to loopback callers (and the bind address
    // itself): a page the operator visits cannot drive it by rebinding a name to
    // 127.0.0.1, because the browser still sends the attacker's Host.
    let bind = Arc::new(listen.ip().to_string());
    let operator = router(state).layer(axum::middleware::from_fn(
        move |req: Request, next: Next| {
            let bind = bind.clone();
            async move { local_only(bind, req, next).await }
        },
    ));
    axum::serve(listener, operator)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // End open SSE streams first: a keep-alive stream never completes on
            // its own, so graceful shutdown would otherwise wait on it until the
            // supervisor sends SIGKILL and orphans harness containers.
            bus.begin_shutdown();
        })
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
///
/// The node id is its Ed25519 public key. A database written before identities
/// existed carries a uuid; it is rekeyed once, here, across every table.
async fn init_node(
    store: &Arc<Store>,
    cfg: &Arc<Config>,
    adapter: &dyn HarnessAdapter,
) -> Result<(String, proto::keys::Identity)> {
    let (identity, fresh) = crate::mesh::identity::load_or_generate().context("node identity")?;
    let id = identity.node_id();
    if fresh {
        tracing::info!(node_id = %id, "generated this node's identity");
    }
    match store.self_node_id()? {
        Some(old) if old != id => {
            store.rekey_self_node(&old, &id)?;
            tracing::info!(from = %old, to = %id, "node id rekeyed to the identity key");
        }
        _ => {}
    }
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
        // A probe that cannot read the version is not a pass: record "unknown",
        // which does not equal the pin, so new sessions are blocked with the
        // version pair shown rather than run against an unverified harness.
        let found = Some(
            adapter
                .version(&runner)
                .await
                .map(|v| v.found)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "harness version probe failed; treating as unknown");
                    "unknown".into()
                }),
        );
        let models = adapter.probe_models(&runner).await.unwrap_or_default();
        (found, models)
    } else {
        (None, Vec::new())
    };

    store.put_node(&NodeRow {
        id: id.clone(),
        is_self: 1,
        x25519_pub: Some(identity.x25519_hex()),
        last_seen_ms: Some(now_ms()),
        reachable: 1,
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
    // This node is always bound to the channels it holds keys for.
    for c in store.channel_list()? {
        store.node_channel_add(&id, &c.name)?;
    }
    Ok((id, identity))
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

#[cfg(test)]
mod tests {
    use super::{host_is_local, hostname};

    #[test]
    fn hostname_strips_scheme_port_and_brackets() {
        assert_eq!(hostname("127.0.0.1:7420"), "127.0.0.1");
        assert_eq!(hostname("localhost"), "localhost");
        assert_eq!(hostname("http://[::1]:7420"), "::1");
        assert_eq!(hostname("http://evil.example"), "evil.example");
        assert_eq!(hostname("evil.example:80"), "evil.example");
    }

    #[test]
    fn loopback_and_the_bind_address_are_allowed_others_are_refused() {
        let bind = "127.0.0.1";
        assert!(host_is_local(Some("localhost:7420"), bind));
        assert!(host_is_local(Some("127.0.0.1:7420"), bind));
        assert!(host_is_local(Some("[::1]:7420"), bind));
        // A rebinding attack keeps the attacker's name in Host.
        assert!(!host_is_local(Some("evil.example"), bind));
        assert!(!host_is_local(Some("evil.example:7420"), bind));
        // A non-loopback bind allows its own address.
        assert!(host_is_local(Some("10.0.0.5:7420"), "10.0.0.5"));
        assert!(!host_is_local(Some("10.0.0.9:7420"), "10.0.0.5"));
        // A missing Host is not a browser and is not a rebinding vector.
        assert!(host_is_local(None, bind));
    }
}
