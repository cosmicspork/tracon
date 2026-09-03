pub mod api;
pub mod auth;
mod mcp;
pub mod push;
pub mod settings;
mod spa;
mod stream;

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::{self, Next},
    response::Response,
    routing::{get, post, put},
    Router,
};
use tower_http::trace::TraceLayer;

use crate::{
    adapter::HarnessAdapter,
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
        .route("/api/boundary/check", post(api::recheck_boundary))
        .route("/api/boundary/setup", post(api::run_setup))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .route("/api/auth/qr", post(api::qr))
        .route("/api/mesh/init", post(api::mesh_init))
        .route(
            "/api/mesh/enroll",
            get(api::enroll_status).post(api::start_enroll),
        )
        .route("/api/credentials/import", post(api::import_credentials))
        .route("/api/usage", get(api::usage))
        .route("/api/metrics", get(api::metrics))
        .route("/api/provenance/{sha}", get(api::provenance))
        .route(
            "/api/channels/{name}/bindings",
            put(api::put_channel_bindings),
        )
        .route("/api/promotions/batch", post(api::batch_promotions))
        .route("/api/promotions/{id}", get(api::get_promotion))
        .route("/api/promotions/{id}/verdict", post(api::decide_promotion))
        .route("/api/work", get(api::list_work).post(api::add_work))
        .route("/api/work/ready", get(api::ready_work))
        .route(
            "/api/work/{id}",
            get(api::get_work)
                .put(api::put_work)
                .delete(api::delete_work),
        )
        .route("/api/docs", get(api::list_docs))
        .route(
            "/api/docs/{channel}/{slug}",
            get(api::get_doc).put(api::put_doc).delete(api::delete_doc),
        )
        .route(
            "/api/memories",
            get(api::list_memories).post(api::add_memory),
        )
        .route(
            "/api/memories/{id}",
            axum::routing::delete(api::delete_memory),
        )
        .route("/api/credentials", get(api::list_credentials))
        .route("/api/credentials/{name}/share", post(api::share_credential))
        .route(
            "/api/credentials/forge/{forge}",
            put(api::put_forge_credential).delete(api::delete_forge_credential),
        )
        .route("/api/providers", get(api::list_providers))
        .route("/api/providers/{name}/connect", post(api::connect_provider))
        .route("/api/providers/{name}/code", post(api::provider_code))
        .route(
            "/api/providers/{name}/disconnect",
            post(api::disconnect_provider),
        )
        .route("/api/nodes", get(api::list_nodes))
        .route(
            "/api/nodes/{id}/providers/{name}/connect",
            post(api::node_connect_provider),
        )
        .route(
            "/api/nodes/{id}/providers/{name}/code",
            post(api::node_provider_code),
        )
        .route(
            "/api/nodes/{id}/providers/{name}/disconnect",
            post(api::node_disconnect_provider),
        )
        .route("/api/mesh", get(api::get_mesh))
        .route(
            "/api/channels",
            get(api::list_channels).post(api::create_channel),
        )
        .route("/api/mesh/invite", post(api::open_invite))
        .route(
            "/api/mesh/invite/{code}",
            get(api::poll_invite).delete(api::cancel_invite),
        )
        .route("/api/mesh/invite/{code}/admit", post(api::admit_invite))
        .route("/api/repos/recent", get(api::recent_repos))
        .route("/api/repos/clone", post(api::clone_repo))
        .route("/api/forge/repos", get(api::forge_repos))
        .route(
            "/api/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        // One prompt: the item and the session that starts on it.
        .route("/api/compose", post(api::compose))
        .route("/api/sessions/archive-ended", post(api::archive_ended))
        .route("/api/sessions/{id}/archive", post(api::archive_session))
        .route("/api/sessions/{id}/unarchive", post(api::unarchive_session))
        .route("/api/sessions/{id}", get(api::get_session))
        .route("/api/sessions/{id}/events", get(api::session_events))
        .route("/api/sessions/{id}/prompt", post(api::prompt))
        .route("/api/sessions/{id}/kill", post(api::kill))
        .route("/api/sessions/{id}/draft", put(api::put_draft))
        .route("/api/permissions/{id}/answer", post(api::answer_permission))
        .route("/api/reviews/{id}", get(api::get_review))
        .route("/api/reviews/{id}/file", get(api::review_file))
        .route("/api/reviews/{id}/verdict", post(api::decide_review))
        .route("/api/reviews/{id}/release", post(api::release_review))
        .route("/api/queue", get(api::queue))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route(
            "/api/auth/token",
            post(auth::put_token).delete(auth::delete_token),
        )
        .route("/api/auth/sessions", get(auth::list_sessions))
        .route("/api/push/key", get(push::key))
        .route(
            "/api/push/subscriptions",
            get(push::list)
                .post(push::subscribe)
                .delete(push::forget_endpoint),
        )
        .route(
            "/api/push/subscriptions/{id}",
            axum::routing::delete(push::forget),
        )
        .route("/api/push/test", post(push::test))
        .route("/api/stream", get(stream::stream))
        .fallback(spa::serve)
        .layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: https:; connect-src 'self'; object-src 'none'; \
             base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// The hostname portion of a `Host`/`Origin` value, minus any scheme, port, or
/// IPv6 brackets. `http://[::1]:7420` → `::1`, `127.0.0.1:7420` → `127.0.0.1`.
pub(crate) fn hostname(value: &str) -> &str {
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
pub(crate) fn host_is_local(host: Option<&str>, bind: &str) -> bool {
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

/// What the harness can reach: a liveness probe and the node's tools. No
/// operator API, so a harness that finds the forward cannot drive sessions.
pub fn harness_router(state: AppState) -> Router {
    Router::new()
        .route("/harness/ping", get(|| async { "pong" }))
        .route("/mcp/{session_id}", post(mcp::handle))
        .route(
            "/model/{provider}/{*rest}",
            axum::routing::any(crate::gateway::model::handle),
        )
        .with_state(state)
}

pub async fn serve(listen: SocketAddr) -> Result<()> {
    // A config that does not parse is fatal here, not a warning: a typo in
    // `[mesh]` must not quietly run this node unmeshed.
    let cfg = Arc::new(Config::try_load().map_err(|e| anyhow::anyhow!(e))?);
    let store = Arc::new(Store::open(&Config::db_path()).context("open store")?);
    let bus = Bus::new();
    let adapter: Arc<dyn HarnessAdapter> = crate::adapter::adapter_for(&cfg)?;
    // The identity comes first: the credential store is sealed under a key
    // derived from it. `init_node` loads the same seed again below.
    let (store_key_identity, _) = crate::mesh::identity::load_or_generate()?;
    let store_key = store_key_identity.credential_store_key();
    let broker = crate::broker::Broker::load(&store_key)
        .unwrap_or_else(|e| {
            // A broken store must not silently broker nothing: say so loudly and
            // carry on without credentials.
            tracing::error!(error = %e, "credential store could not be read; brokering nothing");
            Default::default()
        })
        .shared();
    {
        let b = broker.read().unwrap();
        if b.is_empty() {
            tracing::info!(path = %crate::broker::Broker::path().display(), "no credentials; no tools offered");
        } else {
            tracing::info!(credentials = ?b.names(), "credential broker loaded");
        }
    }
    // A bundle that cannot be verified yields no rules, and no rules means every
    // request is asked. The failure mode of broken policy is more questions.
    let policy = Arc::new(std::sync::RwLock::new(
        match crate::policy::bundle::load() {
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
        },
    ));

    let tools = Arc::new(crate::mcp::Tools {
        broker: broker.clone(),
        cfg: cfg.clone(),
        policy: policy.clone(),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("http client"),
        session: Default::default(),
    });

    let backend = boundary::backend_for(&cfg).await;
    tracing::info!(runtime = backend.kind(), "boundary backend");
    let (node_id, identity) = init_node(&store, &cfg, adapter.as_ref(), backend.as_ref()).await?;
    let cleaned = crate::session::reconcile_after_restart(&store, &node_id, backend.as_ref()).await;
    if !cleaned.is_empty() {
        tracing::info!(
            sessions = cleaned.len(),
            "closed sessions left over from a previous run"
        );
    }
    let manager = Manager::new(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        node_id.clone(),
        tools.clone(),
        policy.clone(),
        backend.clone(),
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
            policy.clone(),
        );
        client.set_broker(broker.clone(), crate::broker::Broker::path());
        bus.with_tap(client.spawn());
        manager.set_mesh(client.clone());
        tracing::info!(hub = %url, "mesh client started");
        client
    });
    if mesh.is_none() {
        tracing::info!("no hub configured; this node is standalone (tracon enroll to join a mesh)");
    }
    manager.set_adapter(adapter.clone());
    let state = AppState {
        manager,
        cfg: cfg.clone(),
        adapter,
        node_id,
        tools,
        mesh,
        auth: Arc::new(auth::AuthState::load(&store, listen.ip().to_string())),
        enroll: Arc::new(std::sync::Mutex::new(api::EnrollJob::default())),
    };
    if let Some(m) = &state.mesh {
        m.set_executor(Arc::new(state.clone()));
    }
    // The nightly batch, for channels this node processes.
    tokio::spawn(crate::corpus::promote::nightly(
        store.clone(),
        bus.clone(),
        state.node_id.clone(),
        cfg.clone(),
    ));
    // Provider logins run through the same backend as sessions, against a
    // store the node keeps; a connected provider re-runs the model probe.
    {
        let providers = crate::providers::Providers::new(
            cfg.clone(),
            broker.clone(),
            store_key,
            state.adapter.clone(),
            backend.clone(),
            state.node_id.clone(),
            bus.clone(),
        );
        let probe_state = state.clone();
        let probe_backend = backend.clone();
        providers.set_on_connected(Box::new(move || {
            let s = probe_state.clone();
            let b = probe_backend.clone();
            tokio::spawn(async move {
                let _ = api::probe_models_into_store(&s, b.as_ref()).await;
            });
        }));
        // Every provider change lands in this node's row and rides the hello,
        // so a peer's interface renders and drives these providers.
        let pub_store = store.clone();
        let pub_bus = bus.clone();
        let pub_id = state.node_id.clone();
        let carry = move |list: Vec<serde_json::Value>| {
            let json = serde_json::Value::Array(list).to_string();
            if pub_store.set_node_providers(&pub_id, &json).is_ok() {
                if let Ok(Some(row)) = pub_store.get_node(&pub_id) {
                    pub_bus.publish(crate::stream::Frame::Node(row.to_json()));
                }
            }
        };
        carry(providers.list_public()); // seed the row before the first hello
        providers.set_on_publish(Box::new(carry));
        state.manager.set_providers(providers.clone());
        tokio::spawn(providers.refresh_loop());
    }

    // The harness listener is separate from the operator's: it carries only the
    // MCP surface, and the gateway forwards to it from the internal network.
    // Where no gateway container carries the allowlist proxy, the node does.
    if let Some(port) = backend.proxy_port() {
        let allow = crate::gateway::proxy::Allowlist::new(&cfg.gateway.allow_hosts)
            .map_err(|e| anyhow::anyhow!("allowlist: {e}"))?;
        tokio::spawn(crate::gateway::proxy::serve(port, allow));
        tracing::info!(port, "connect proxy listening");
    }
    let harness_app = harness_router(state.clone());
    tracing::info!(listen = %cfg.gateway.harness_listen, "harness listener");
    match &cfg.gateway.harness_listen {
        crate::config::HarnessListen::Tcp(addr) => {
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind {addr}"))?;
            tokio::spawn(async move {
                let _ = axum::serve(l, harness_app).await;
            });
        }
        crate::config::HarnessListen::Unix(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            // A socket left by a previous run refuses a fresh bind.
            let _ = std::fs::remove_file(path);
            let l = tokio::net::UnixListener::bind(path)
                .with_context(|| format!("bind {}", path.display()))?;
            tokio::spawn(async move {
                let _ = axum::serve(l, harness_app).await;
            });
        }
    }

    // Now that the harness can reach the gateway, ask it what models it offers.
    // The probe presents the node's own read-only token; without a model
    // credential there is nothing to inject, so the list stays empty and the
    // interface says to connect a provider.
    {
        let probe_state = state.clone();
        let probe_backend = backend.clone();
        tokio::spawn(async move {
            let _ = api::probe_models_into_store(&probe_state, probe_backend.as_ref()).await;
        });
    }

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

    // What waits on the operator, pushed to the phones subscribed here. Every
    // node delivers for its own devices; a channel opts out by binding.
    tokio::spawn(crate::notify::run(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        state.node_id.clone(),
    ));

    // This node's own vector index. Derived state, rebuilt from the corpus, so
    // it starts after everything that writes the corpus and never gates it.
    tokio::spawn(crate::embed::run(
        cfg.clone(),
        store.clone(),
        bus.clone(),
        state.tools.http.clone(),
        state.manager.probe_token().to_string(),
    ));

    tracing::info!(%listen, "serving");
    let manager = state.manager.clone();
    let bus = manager.bus().clone();
    let operator =
        router(state.clone()).layer(axum::middleware::from_fn_with_state(state, auth::guard));
    // ConnectInfo is how the guard tells a caller on this machine from one that
    // arrived over the network; without it every request is treated as remote.
    axum::serve(
        listener,
        operator.into_make_service_with_connect_info::<SocketAddr>(),
    )
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
    backend: &dyn boundary::Backend,
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
    if let Some(p) = crate::session::materialize::retire_harness_credentials() {
        tracing::warn!(
            path = %p.display(),
            "a credential store was on the harness volume; set aside — models are brokered through the gateway now"
        );
    }
    verify_node(store, cfg, adapter, backend, &id, Some(&identity)).await?;
    // This node is always bound to the channels it holds keys for.
    for c in store.channel_list()? {
        store.node_channel_add(&id, &c.name)?;
    }
    Ok((id, identity))
}

/// Run the boundary checks, probe the harness, and record what this node is.
///
/// Startup calls it, and so does a re-check from the interface: an operator
/// who has just started the runtime should not have to restart the node to be
/// believed. The harness probe belongs to it, not to startup, because a node
/// that goes refused → ready without one has no recorded version and blocks
/// every new session on a mismatch against `None`.
pub(crate) async fn verify_node(
    store: &Arc<Store>,
    cfg: &Arc<Config>,
    adapter: &dyn HarnessAdapter,
    backend: &dyn boundary::Backend,
    id: &str,
    identity: Option<&proto::keys::Identity>,
) -> Result<NodeRow> {
    let report = backend.check_all(cfg, false).await;
    let failed = report.first_failure().cloned();
    let ready = failed.is_none();
    if let Some(f) = &failed {
        tracing::warn!(check = f.id.as_str(), detail = %f.detail, "refusing to run harnesses");
    }
    let found = if ready {
        let runner = backend.runner(
            crate::session::materialize::state_mounts(&backend.harness_home(), adapter.layout())
                .unwrap_or_default(),
        );
        // A probe that cannot read the version is not a pass: record "unknown",
        // which does not equal the pin, so new sessions are blocked with the
        // version pair shown rather than run against an unverified harness.
        Some(
            adapter
                .version(runner.as_ref())
                .await
                .map(|v| v.found)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "harness version probe failed; treating as unknown");
                    "unknown".into()
                }),
        )
    } else {
        None
    };
    let previous = store.get_node(id).ok().flatten();
    // The model list is probed once the gateway is listening (see `serve`);
    // until then the previous run's list stands.
    let models: Vec<crate::adapter::ModelOption> = previous
        .as_ref()
        .and_then(|n| n.models_json.clone())
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();

    let row = NodeRow {
        id: id.to_string(),
        is_self: 1,
        x25519_pub: identity
            .map(|i| i.x25519_hex())
            .or_else(|| previous.as_ref().and_then(|n| n.x25519_pub.clone())),
        last_seen_ms: Some(now_ms()),
        reachable: 1,
        providers_json: None,
        name: cfg.node_name.clone(),
        state: if ready { "ready" } else { "refused" }.into(),
        failed_check: failed.as_ref().map(|f| f.id.as_str().to_string()),
        failed_detail: failed.as_ref().map(|f| f.detail.clone()),
        harness_id: adapter.id().into(),
        harness_pinned: adapter.pinned_version().into(),
        harness_found: found,
        models_json: serde_json::to_string(&models).ok(),
        checked_at_ms: Some(now_ms()),
    };
    store.put_node(&row)?;
    Ok(row)
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
