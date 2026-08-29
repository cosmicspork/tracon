//! Registering a phone: who may, what is checked, and what a logout takes
//! away. The guard is layered on as `serve` builds it, because the whole
//! point is who the caller is.

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
    http::{api::AppState, auth, auth::AuthState},
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";
const KEY: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

struct Node {
    app: axum::Router,
    store: Arc<Store>,
}

fn node() -> Node {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
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
    };
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::guard));
    Node { app, store }
}

async fn call(
    n: &Node,
    method: &str,
    uri: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Vec<String>, Value) {
    let host = if peer == LOCAL {
        "127.0.0.1:7420"
    } else {
        "tracon.example"
    };
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host)
        .header("user-agent", "TestPhone/1.0");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    let res = n.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let cookies = res
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        cookies,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A token issued over loopback and a remote browser logged in with it.
async fn logged_in(n: &Node) -> String {
    let (s, _, _) = call(
        n,
        "POST",
        "/api/auth/token",
        LOCAL,
        &[],
        Some(json!({ "token_hash": auth::hash("trc1.secret") })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, cookies, _) = call(
        n,
        "POST",
        "/api/login",
        REMOTE,
        &[],
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

fn subscription(endpoint: &str) -> Value {
    json!({ "endpoint": endpoint, "keys": { "p256dh": KEY, "auth": AUTH } })
}

#[tokio::test]
async fn the_key_is_stable_and_a_phone_registers_with_its_cookie() {
    state::isolate();
    let n = node();
    let cookie = logged_in(&n).await;
    let (_, _, k1) = call(
        &n,
        "GET",
        "/api/push/key",
        REMOTE,
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let (_, _, k2) = call(
        &n,
        "GET",
        "/api/push/key",
        REMOTE,
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(k1["key"], k2["key"], "the key is generated once");
    assert_eq!(
        k1["key"].as_str().unwrap().len(),
        87,
        "an uncompressed P-256 point, base64url"
    );

    let (s, _, v) = call(
        &n,
        "POST",
        "/api/push/subscriptions",
        REMOTE,
        &[("cookie", &cookie)],
        Some(subscription("https://push.example/a")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = v["id"].as_str().unwrap().to_string();

    // Registering the same endpoint again is the same device, not a second.
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/push/subscriptions",
        REMOTE,
        &[("cookie", &cookie)],
        Some(subscription("https://push.example/a")),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["id"], id);

    let (_, _, list) = call(
        &n,
        "GET",
        "/api/push/subscriptions",
        REMOTE,
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let devices = list["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["mine"], true);
    assert_eq!(devices[0]["local"], false);
    assert_eq!(devices[0]["user_agent"], "TestPhone/1.0");

    // Logging out takes the device with it.
    let (s, _, _) = call(
        &n,
        "POST",
        "/api/logout",
        REMOTE,
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        n.store.push_subscriptions().unwrap().is_empty(),
        "a logged-out client has no devices"
    );
}

#[tokio::test]
async fn a_bearer_or_bad_subscription_is_refused_and_loopback_belongs_to_the_machine() {
    state::isolate();
    let n = node();
    let _cookie = logged_in(&n).await;

    // A remote caller with the token itself has no browser session to tie a
    // device to.
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/push/subscriptions",
        REMOTE,
        &[("authorization", "Bearer trc1.secret")],
        Some(subscription("https://push.example/b")),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // A browser on this machine never logged in; its device is the machine's.
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/push/subscriptions",
        LOCAL,
        &[],
        Some(subscription("https://push.example/c")),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, _, list) = call(&n, "GET", "/api/push/subscriptions", LOCAL, &[], None).await;
    assert_eq!(list["devices"][0]["local"], true);

    for bad in [
        json!({ "endpoint": "http://push.example/x", "keys": { "p256dh": KEY, "auth": AUTH } }),
        json!({ "endpoint": "https://push.example/x", "keys": { "p256dh": "AAAA", "auth": AUTH } }),
        json!({ "endpoint": "https://push.example/x", "keys": { "p256dh": KEY, "auth": "short" } }),
    ] {
        let (s, _, v) = call(
            &n,
            "POST",
            "/api/push/subscriptions",
            LOCAL,
            &[],
            Some(bad.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} -> {v}");
    }

    // Forgetting: by id from the screen, by endpoint from the worker.
    let id = list["devices"][0]["id"].as_str().unwrap().to_string();
    let (s, _, _) = call(
        &n,
        "DELETE",
        &format!("/api/push/subscriptions/{id}"),
        LOCAL,
        &[],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = call(
        &n,
        "DELETE",
        &format!("/api/push/subscriptions/{id}"),
        LOCAL,
        &[],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    call(
        &n,
        "POST",
        "/api/push/subscriptions",
        LOCAL,
        &[],
        Some(subscription("https://push.example/d")),
    )
    .await;
    let (s, _, _) = call(
        &n,
        "DELETE",
        "/api/push/subscriptions",
        LOCAL,
        &[],
        Some(json!({ "endpoint": "https://push.example/d" })),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(n.store.push_subscriptions().unwrap().is_empty());

    // A test push with nothing subscribed says so rather than failing.
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/push/test",
        LOCAL,
        &[],
        Some(json!({ "all": true })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["sent"].as_array().unwrap().len(), 0);
}
