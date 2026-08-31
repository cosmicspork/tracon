//! Who the operator API answers to. Loopback keeps working without a
//! credential; everything else needs the token, or a cookie earned with it.

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

use support::fake::FakeAdapter;

struct Node {
    app: axum::Router,
    store: Arc<Store>,
}

/// The router with the guard layered on, as `serve` builds it.
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
        enroll: Default::default(),
    };
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::guard));
    Node { app, store }
}

/// One request, with a peer address the guard can read.
async fn call(
    n: &Node,
    method: &str,
    uri: &str,
    peer: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Vec<String>, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", host);
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
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, cookies, v)
}

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";

/// Set the operator token the way `tracon auth issue` does: over loopback,
/// through the API, hash only.
async fn set_token(n: &Node, token: &str) {
    let (s, _, _) = call(
        n,
        "POST",
        "/api/auth/token",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        Some(json!({ "token_hash": auth::hash(token) })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

fn cookie_of(cookies: &[String]) -> String {
    let raw = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie");
    raw.split(';').next().unwrap().to_string()
}

/// Everything the CLI and `just dev` do keeps working, token or no token.
#[tokio::test]
async fn loopback_is_the_operator_before_and_after_a_token_exists() {
    state::isolate();
    let n = node();
    let (s, _, _) = call(&n, "GET", "/api/node", LOCAL, "127.0.0.1:7420", &[], None).await;
    assert_eq!(s, StatusCode::OK);

    let (s, _, _) = call(
        &n,
        "POST",
        "/api/auth/token",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        Some(json!({ "token_hash": auth::hash("trc1.secret") })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _, _) = call(&n, "GET", "/api/node", LOCAL, "127.0.0.1:7420", &[], None).await;
    assert_eq!(s, StatusCode::OK, "a token must not lock out loopback");
}

/// Without a token the node has nothing to authenticate with, so it says so
/// rather than pretending a credential would help.
#[tokio::test]
async fn a_stranger_is_refused_outright_until_a_token_is_issued() {
    state::isolate();
    let n = node();
    let (s, _, v) = call(&n, "GET", "/api/node", REMOTE, "tracon.example", &[], None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tracon auth issue"),
        "the refusal should say how to open the door: {v}"
    );
}

#[tokio::test]
async fn the_token_buys_a_cookie_and_the_cookie_is_what_travels() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;

    // Remote, no credential.
    let (s, _, _) = call(&n, "GET", "/api/node", REMOTE, "tracon.example", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // The wrong token is not accepted.
    let (s, _, _) = call(
        &n,
        "POST",
        "/api/login",
        REMOTE,
        "tracon.example",
        &[],
        Some(json!({ "token": "trc1.wrong" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // The right one is.
    let (s, cookies, _) = call(
        &n,
        "POST",
        "/api/login",
        REMOTE,
        "tracon.example",
        &[],
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let raw = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .unwrap();
    assert!(
        raw.contains("HttpOnly"),
        "the cookie must not be readable by script: {raw}"
    );
    assert!(raw.contains("Secure"));
    assert!(raw.contains("SameSite=Lax"));

    let cookie = cookie_of(&cookies);
    for path in ["/api/node", "/api/queue"] {
        let (s, _, _) = call(
            &n,
            "GET",
            path,
            REMOTE,
            "tracon.example",
            &[("cookie", &cookie)],
            None,
        )
        .await;
        assert_eq!(
            s,
            StatusCode::OK,
            "{path} should be reachable with a cookie"
        );
    }

    // The event stream is the one the phone lives on, and EventSource cannot
    // send a header — so the cookie is the whole reason it can be reached.
    // Its body never ends, so only the status is read.
    let mut req = Request::builder()
        .method("GET")
        .uri("/api/stream")
        .header("host", "tracon.example")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(REMOTE.parse::<SocketAddr>().unwrap()));
    let res = n.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    drop(res);

    let mut req = Request::builder()
        .method("GET")
        .uri("/api/stream")
        .header("host", "tracon.example")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(REMOTE.parse::<SocketAddr>().unwrap()));
    let res = n.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "no cookie, no stream"
    );
}

/// A non-browser client (the CLI over the ingress) presents the token itself.
#[tokio::test]
async fn the_token_also_works_as_a_bearer_for_clients_that_hold_no_cookies() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("authorization", "Bearer trc1.secret")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("authorization", "Bearer trc1.nope")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// The cookie travels with a cross-site request; the Origin does not lie.
#[tokio::test]
async fn a_cross_origin_page_cannot_drive_the_api_with_a_stolen_ride() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let (_, cookies, _) = call(
        &n,
        "POST",
        "/api/login",
        REMOTE,
        "tracon.example",
        &[],
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    let cookie = cookie_of(&cookies);

    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("cookie", &cookie), ("origin", "https://evil.example")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // The node's own page is fine.
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("cookie", &cookie), ("origin", "https://tracon.example")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn logging_out_ends_this_client_and_rotating_ends_all_of_them() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let login = |n: &Node| {
        let app = n.app.clone();
        async move {
            let mut req = Request::builder()
                .method("POST")
                .uri("/api/login")
                .header("host", "tracon.example")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "token": "trc1.secret" }).to_string()))
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(REMOTE.parse::<SocketAddr>().unwrap()));
            let res = app.oneshot(req).await.unwrap();
            let raw = res
                .headers()
                .get(axum::http::header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            raw.split(';').next().unwrap().to_string()
        }
    };
    let phone = login(&n).await;
    let laptop = login(&n).await;

    // Logging out is per client.
    let (s, _, _) = call(
        &n,
        "POST",
        "/api/logout",
        REMOTE,
        "tracon.example",
        &[("cookie", &phone)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("cookie", &phone)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("cookie", &laptop)],
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "one client logging out must not log out the rest"
    );

    // Rotating the token is the big hammer: everything logged in dies.
    let (s, _, _) = call(
        &n,
        "POST",
        "/api/auth/token",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        Some(json!({ "token_hash": auth::hash("trc1.rotated") })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("cookie", &laptop)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// Revoking closes the door entirely rather than leaving it ajar.
#[tokio::test]
async fn revoking_returns_the_node_to_loopback_only() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let (s, _, _) = call(
        &n,
        "DELETE",
        "/api/auth/token",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = call(
        &n,
        "POST",
        "/api/login",
        REMOTE,
        "tracon.example",
        &[],
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn guessing_is_rate_limited() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let mut refused = false;
    for _ in 0..12 {
        let (s, _, _) = call(
            &n,
            "POST",
            "/api/login",
            REMOTE,
            "tracon.example",
            &[],
            Some(json!({ "token": "trc1.wrong" })),
        )
        .await;
        if s == StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            break;
        }
    }
    assert!(refused, "a stranger should run out of attempts");
}

/// The login screen has to render before anyone can log in, and a deep link
/// from a notification has to open. The shell is public; the data is not.
#[tokio::test]
async fn the_shell_is_served_without_a_credential_but_the_api_is_not() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let (s, _, _) = call(
        &n,
        "GET",
        "/reviews/abc",
        REMOTE,
        "tracon.example",
        &[],
        None,
    )
    .await;
    assert_ne!(s, StatusCode::UNAUTHORIZED);
    assert_ne!(s, StatusCode::FORBIDDEN);

    let (s, _, _) = call(&n, "GET", "/api/queue", REMOTE, "tracon.example", &[], None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

/// The harness reaches its own router on its own listener. The operator guard
/// is not on it, and neither are the operator's routes.
#[tokio::test]
async fn the_harness_router_carries_no_operator_api() {
    state::isolate();
    let n = node();
    let state = AppState {
        manager: Manager::new(
            n.store.clone(),
            Bus::new(),
            Arc::new(Config::default()),
            "n1".into(),
            Arc::new(Tools {
                broker: Broker::default().shared(),
                cfg: Arc::new(Config::default()),
                policy: tracon::policy::Policy::shipped_shared(),
                http: reqwest::Client::new(),
                session: Default::default(),
            }),
            Default::default(),
            Arc::new(tracon::runner::local::LocalBackend),
        ),
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
        auth: Arc::new(AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    };
    let harness = tracon::http::harness_router(state);
    let ping = harness
        .clone()
        .oneshot(
            Request::builder()
                .uri("/harness/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        ping.status(),
        StatusCode::OK,
        "the harness probe needs no cookie"
    );

    let api = harness
        .oneshot(
            Request::builder()
                .uri("/api/node")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        api.status(),
        StatusCode::NOT_FOUND,
        "no operator API on the harness listener"
    );
}
