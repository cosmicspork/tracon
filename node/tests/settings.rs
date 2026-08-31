//! Standing a node up through the interface: the boundary, channels,
//! credentials, and the configuration — and the line between what any client
//! may do and what is done at the node itself.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use support::fake::FakeAdapter;
use tracon::{
    broker::Broker,
    config::Config,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";

struct Node {
    app: axum::Router,
}

/// The router with the guard layered on, as `serve` builds it.
fn node() -> Node {
    // Before anything can reach the credential store or node.toml.
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let cfg = Arc::new(Config::default());
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
    let state = AppState {
        manager,
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(AuthState::load(&store, "127.0.0.1".into())),
        enroll: Default::default(),
    };
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::guard));
    Node { app }
}

/// One request, with a peer address the guard and the extractor can read.
async fn call(
    n: &Node,
    method: &str,
    uri: &str,
    peer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420");
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    if let Some(p) = peer {
        let addr: SocketAddr = p.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
    }
    let res = n.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_channel_is_created_named_and_idempotent() {
    let n = node();
    let (s, v) = call(
        &n,
        "POST",
        "/api/channels",
        Some(LOCAL),
        Some(json!({ "name": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["created"], json!(true));

    // Again: still there, and it says it did not mint a second key.
    let (s, v) = call(
        &n,
        "POST",
        "/api/channels",
        Some(LOCAL),
        Some(json!({ "name": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["created"], json!(false));

    let (s, v) = call(&n, "GET", "/api/channels", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v.as_array()
            .is_some_and(|a| a.iter().any(|c| c["name"] == "work"))
            || v["channels"]
                .as_array()
                .is_some_and(|a| a.iter().any(|c| c["name"] == "work")),
        "created channel is not listed: {v}"
    );
}

#[tokio::test]
async fn a_channel_name_the_protocol_refuses_is_refused_here() {
    let n = node();
    let (s, v) = call(
        &n,
        "POST",
        "/api/channels",
        Some(LOCAL),
        Some(json!({ "name": "Not A Channel" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}

#[tokio::test]
async fn credentials_import_by_paste_and_only_their_names_come_back() {
    let n = node();
    let toml = r#"
[credentials.gh]
channels = ["personal"]
[credentials.gh.env]
GH_TOKEN = "sh-not-a-real-token"
"#;
    let (s, v) = call(
        &n,
        "POST",
        "/api/credentials/import",
        Some(LOCAL),
        Some(json!({ "toml": toml })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["imported"], json!(["gh"]));
    // The response names it and nothing more.
    assert!(!v.to_string().contains("not-a-real-token"));

    // And the list keeps that promise too.
    let (s, v) = call(&n, "GET", "/api/credentials", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK);
    let text = v.to_string();
    assert!(text.contains("gh"), "{text}");
    assert!(
        !text.contains("not-a-real-token"),
        "a value escaped: {text}"
    );
}

#[tokio::test]
async fn nonsense_toml_is_refused_rather_than_stored() {
    let n = node();
    let (s, _) = call(
        &n,
        "POST",
        "/api/credentials/import",
        Some(LOCAL),
        Some(json!({ "toml": "this is not toml = = =" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Valid TOML carrying no credentials is not an import either.
    let (s, _) = call(
        &n,
        "POST",
        "/api/credentials/import",
        Some(LOCAL),
        Some(json!({ "toml": "[something_else]\nkey = 1\n" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn the_config_is_readable_and_carries_no_secrets() {
    let n = node();
    let (s, v) = call(&n, "GET", "/api/config", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["harness"]["id"].is_string());
    assert!(v["running"]["harness_id"].is_string());
    // An allowlist, asserted as one: a new section here is a decision, not an
    // accident, and the broker's secrets are not settings.
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "boundary",
            "gateway",
            "harness",
            "node_name",
            "publish",
            "readonly",
            "review",
            "running",
            "session",
        ]
    );
}

/// The line this feature draws: a phone may run the node, but what the node
/// *is* — the binaries it executes, the hub it trusts — is set at the node.
#[tokio::test]
async fn writing_the_config_is_refused_off_the_machine() {
    let n = node();
    let patch = json!({ "session": { "budget_tokens": 5_000_000 } });

    let (s, v) = call(&n, "PUT", "/api/config", Some(REMOTE), Some(patch.clone())).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // No peer address at all is remote: the extractor fails closed.
    let (s, _) = call(&n, "PUT", "/api/config", None, Some(patch)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mesh_init_and_enroll_are_loopback_only_too() {
    let n = node();
    for (method, uri, body) in [
        (
            "POST",
            "/api/mesh/init",
            Some(json!({ "hub_url": "https://hub.example.com" })),
        ),
        (
            "POST",
            "/api/mesh/enroll",
            Some(json!({ "invitation": "https://hub.example.com/enroll#abc" })),
        ),
        ("GET", "/api/mesh/enroll", None),
    ] {
        let (s, v) = call(&n, method, uri, Some(REMOTE), body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri} answered {s}: {v}");
    }
}

#[tokio::test]
async fn an_unknown_setting_is_refused_rather_than_dropped() {
    let n = node();
    let (s, v) = call(
        &n,
        "PUT",
        "/api/config",
        Some(LOCAL),
        Some(json!({ "mesh": { "hub_url": "https://elsewhere.example.com" } })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("mesh"));
}

#[tokio::test]
async fn the_node_payload_says_whether_this_client_may_configure_it() {
    let n = node();
    let (s, v) = call(&n, "GET", "/api/node", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["loopback"], json!(true));
}
