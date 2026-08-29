//! The model gateway: the harness's placeholder key never reaches a provider,
//! the broker's credential does, bindings refuse before anything is forwarded,
//! and what passed through is counted.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::any;
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    broker::Broker,
    config::{Config, Provider, SHAPE_ANTHROPIC},
    http::api::AppState,
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

#[path = "support/fake.rs"]
mod fake;
#[path = "support/state.rs"]
mod state;
use fake::FakeAdapter;

/// `(method, uri, headers, body)` as the stub saw it.
type SeenRequest = (String, String, Vec<(String, String)>, String);

#[derive(Clone, Default)]
struct Seen {
    requests: Arc<Mutex<Vec<SeenRequest>>>,
}

/// A provider stub that records every request and answers a two-event stream
/// carrying usage, the way Anthropic does.
async fn start_upstream(seen: Seen) -> u16 {
    let app = axum::Router::new().fallback(any(
        move |req: Request<Body>| {
            let seen = seen.clone();
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
                let headers: Vec<(String, String)> = parts
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                seen.requests.lock().unwrap().push((
                    parts.method.to_string(),
                    parts.uri.to_string(),
                    headers,
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n\
                           event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n";
                (
                    [("content-type", "text/event-stream"), ("x-upstream", "stub")],
                    sse,
                )
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    manager: Manager,
    seen: Seen,
}

async fn harness(broker_toml: &str, allow: &[&str]) -> Harness {
    let seen = Seen::default();
    let port = start_upstream(seen.clone()).await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut cfg = Config::default();
    cfg.gateway.allow_hosts = allow.iter().map(|s| s.to_string()).collect();
    cfg.providers.clear();
    cfg.providers.insert(
        "stub".into(),
        Provider {
            credential: "stubcred".into(),
            upstream: format!("http://127.0.0.1:{port}"),
            shape: SHAPE_ANTHROPIC.into(),
            login: None,
            price: None,
        },
    );
    cfg.providers.insert(
        "elsewhere".into(),
        Provider {
            credential: "stubcred".into(),
            upstream: "https://example.com".into(),
            shape: SHAPE_ANTHROPIC.into(),
            login: None,
            price: None,
        },
    );
    let cfg = Arc::new(cfg);
    let broker: Broker = toml::from_str(broker_toml).unwrap();
    let tools = Arc::new(Tools {
        broker: broker.shared(),
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
    let app = tracon::http::harness_router(AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
    });
    Harness {
        app,
        store,
        manager,
        seen,
    }
}

const STORE: &str = r#"
    [credentials.stubcred]
    kind = "api_key"
    provider = "stub"
    channels = ["work"]
    [credentials.stubcred.env]
    API_KEY = "real-key"
"#;
const LOOPBACK: &[&str] = &[r"^127\.0\.0\.1$"];

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    key: &str,
    body: Value,
) -> (StatusCode, Vec<(String, String)>, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-api-key", key)
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-beta", "effort-2025-11-24")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn the_placeholder_never_reaches_the_provider_and_the_credential_does() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    let token = h.manager.register_tool_token_for_test("s1", "work").await;
    let (status, headers, body) = call(
        &h.app,
        "POST",
        "/model/stub/v1/messages?beta=true",
        &token,
        json!({"model": "claude-x", "stream": true, "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(header(&headers, "x-upstream"), Some("stub"));
    assert!(
        body.contains("message_delta"),
        "body streamed through: {body}"
    );

    let seen = h.seen.requests.lock().unwrap();
    let (method, uri, up, up_body) = &seen[0];
    assert_eq!(method, "POST");
    assert_eq!(uri, "/v1/messages?beta=true");
    assert_eq!(header(up, "x-api-key"), Some("real-key"));
    assert!(
        header(up, "authorization").is_none(),
        "placeholder bearer stripped"
    );
    assert_eq!(header(up, "anthropic-beta"), Some("effort-2025-11-24"));
    assert!(
        !format!("{up:?}{up_body}").contains(&token),
        "token leaked upstream"
    );
    assert!(up_body.contains("claude-x"));
    drop(seen);

    // Counted, with the tokens read off the stream.
    let totals = h.store.usage_since(Some("work"), 0).unwrap();
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].provider, "stub");
    assert_eq!(totals[0].model.as_deref(), Some("claude-x"));
    assert_eq!(
        (
            totals[0].requests,
            totals[0].input_tokens,
            totals[0].output_tokens
        ),
        (1, 9, 5)
    );
}

#[tokio::test]
async fn an_unknown_key_is_unauthorized_and_nothing_is_forwarded() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    let (status, _, _) = call(&h.app, "POST", "/model/stub/v1/messages", "nope", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(h.seen.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_channel_without_the_credential_is_refused_before_forwarding() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    let token = h
        .manager
        .register_tool_token_for_test("s2", "personal")
        .await;
    let (status, _, body) =
        call(&h.app, "POST", "/model/stub/v1/messages", &token, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("not bound"), "{body}");
    assert!(h.seen.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_channel_bound_to_other_providers_is_refused() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    h.store
        .channel_put("work", b"ring", r#"{"providers":["local-only"]}"#)
        .unwrap();
    let token = h.manager.register_tool_token_for_test("s3", "work").await;
    let (status, _, body) =
        call(&h.app, "POST", "/model/stub/v1/messages", &token, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("not bound to provider stub"), "{body}");
    assert!(h.seen.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_upstream_off_the_egress_allowlist_is_refused() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    let token = h.manager.register_tool_token_for_test("s4", "work").await;
    let (status, _, body) = call(
        &h.app,
        "POST",
        "/model/elsewhere/v1/messages",
        &token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("allowlist"), "{body}");
    let (status, _, _) = call(
        &h.app,
        "POST",
        "/model/nowhere/v1/messages",
        &token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_probe_may_only_read() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    let probe = h.manager.probe_token().to_string();
    let (status, _, _) = call(&h.app, "GET", "/model/stub/v1/models", &probe, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(&h.app, "POST", "/model/stub/v1/messages", &probe, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let seen = h.seen.requests.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(header(&seen[0].2, "x-api-key"), Some("real-key"));
}

#[tokio::test]
async fn an_oauth_credential_becomes_a_bearer_with_the_beta_flag_merged() {
    state::isolate();
    const OAUTH: &str = r#"
        [credentials.stubcred]
        kind = "oauth"
        provider = "stub"
        channels = ["work"]
        [credentials.stubcred.env]
        ACCESS_TOKEN = "at-1"
        REFRESH_TOKEN = "rt-1"
    "#;
    let h = harness(OAUTH, LOOPBACK).await;
    let token = h.manager.register_tool_token_for_test("s5", "work").await;
    let (status, _, _) = call(&h.app, "POST", "/model/stub/v1/messages", &token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let seen = h.seen.requests.lock().unwrap();
    let up = &seen[0].2;
    assert_eq!(header(up, "authorization"), Some("Bearer at-1"));
    assert!(header(up, "x-api-key").is_none());
    assert_eq!(
        header(up, "anthropic-beta"),
        Some("oauth-2025-04-20,effort-2025-11-24")
    );
    assert!(
        !format!("{up:?}").contains("rt-1"),
        "refresh token never leaves the node"
    );
}

#[tokio::test]
async fn a_channel_at_its_daily_ceiling_is_refused_and_told_once_per_session() {
    state::isolate();
    let h = harness(STORE, LOOPBACK).await;
    h.store
        .channel_put("work", b"ring", r#"{"ceiling_tokens_per_day": 100}"#)
        .unwrap();
    h.store.ensure_peer_node("n1").unwrap();
    // A running session on the channel, so the ceiling event has a row to land on.
    let sid = "s-ceiling";
    h.store
        .conn()
        .execute(
            "INSERT INTO session (id, node_id, channel, repo_path, branch, harness_id, harness_version, model,
                budget_tokens, tokens_used, state, turn_active, created_ms, updated_ms)
             VALUES (?1, 'n1', 'work', '/r', 'b', 'fake', '1', 'm', 1000, 0, 'running', 0, 1, 1)",
            [sid],
        )
        .unwrap();
    let token = h.manager.register_tool_token_for_test(sid, "work").await;
    // Under the ceiling: the call goes through and its usage is counted.
    let (status, _, _) = call(
        &h.app,
        "POST",
        "/model/stub/v1/messages",
        &token,
        json!({"model": "claude-x", "messages": []}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Spend the rest of today's tokens.
    h.store
        .record_usage(&tracon::store::UsageRow {
            channel: "work".into(),
            node_id: "n1".into(),
            session_id: Some(sid.into()),
            provider: "stub".into(),
            model: None,
            at_ms: tracon::store::now_ms(),
            input_tokens: 90,
            output_tokens: 10,
            requests: 1,
        })
        .unwrap();
    for _ in 0..2 {
        let (status, _, body) = call(
            &h.app,
            "POST",
            "/model/stub/v1/messages",
            &token,
            json!({"model": "claude-x", "messages": []}),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert!(
            body.contains("daily ceiling") && body.contains("of 100 tokens"),
            "{body}"
        );
    }
    assert_eq!(
        h.seen.requests.lock().unwrap().len(),
        1,
        "nothing forwarded at the ceiling"
    );
    let kinds: Vec<String> = h
        .store
        .events_after(sid, 0, 50)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| *k == "ceiling").count(),
        1,
        "{kinds:?}"
    );
    // The channel reports it, and a new session on it is refused with the figures.
    let (st, chans) = {
        let app = tracon::http::router(tracon::http::api::AppState {
            manager: h.manager.clone(),
            cfg: Arc::new(Config::default()),
            adapter: Arc::new(FakeAdapter {
                tx: Arc::new(tokio::sync::Mutex::new(None)),
                tokens: Arc::new(tokio::sync::Mutex::new(0)),
            }),
            node_id: "n1".into(),
            tools: Arc::new(Tools {
                broker: Broker::default().shared(),
                cfg: Arc::new(Config::default()),
                policy: tracon::policy::Policy::shipped_shared(),
                http: reqwest::Client::new(),
                session: Default::default(),
            }),
            mesh: None,
            auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        });
        let req = Request::builder()
            .method("GET")
            .uri("/api/channels")
            .header("host", "127.0.0.1:7420")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let st = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/api/sessions")
            .header("host", "127.0.0.1:7420")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"channel": "work", "repo_path": "/r", "model": "m", "work_item_id": "x", "phase": "plan"}).to_string(),
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        (st, v)
    };
    assert_eq!(st, StatusCode::OK);
    let work = chans
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "work")
        .unwrap();
    assert_eq!(work["ceiling"]["state"], "at");
    assert_eq!(work["ceiling"]["ceiling"], 100);
    assert!(work["ceiling"]["usage_today"].as_i64().unwrap() >= 100);
}
