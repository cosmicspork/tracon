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

/// A loopback client sets a token too — the macOS desktop wrapper's WKWebView
/// reaches its own node this way — and `Secure` on that cookie is not merely
/// unnecessary there, it is actively harmful: unlike Chromium's loopback
/// exception, WKWebView drops a `Secure` cookie set over plain HTTP outright,
/// so the client's own readback of its login never reflects it. Withholding
/// `Secure` for a loopback peer is what `http::ui`'s cookie already does
/// (`UiState::secure`); this is the same fix for the operator cookie `login`
/// sets. Remote callers are unaffected — `the_token_buys_a_cookie_and_the_cookie_is_what_travels`
/// and `the_browser_side_defences_back_the_middleware` cover those.
#[tokio::test]
async fn a_loopback_login_omits_secure_so_a_local_webview_keeps_the_cookie() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;

    let (s, cookies, _) = call(
        &n,
        "POST",
        "/api/login",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let raw = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie");
    assert!(
        !raw.contains("Secure"),
        "a loopback caller must not be handed a cookie its own browser drops: {raw}"
    );
    assert!(raw.contains("HttpOnly"), "{raw}");
    assert!(raw.contains("SameSite=Lax"), "{raw}");

    // And it still works to reach the operator API with, same as remote.
    let cookie = cookie_of(&cookies);
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        LOCAL,
        "127.0.0.1:7420",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
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

/// `Origin: null` is an opaque origin — a sandboxed iframe, a `file://` page,
/// a cross-origin redirect. It can never equal this node's origin, so it is
/// refused outright rather than read as "no Origin at all" and waved past the
/// same-origin check. Loopback is not an exemption: a sandboxed page on the
/// operator's own machine is exactly the reproduced path.
#[tokio::test]
async fn an_opaque_origin_is_refused_on_operator_routes() {
    state::isolate();
    let n = node();

    // Loopback, state-changing, no credential needed today: still refused.
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/auth/token",
        LOCAL,
        "127.0.0.1:7420",
        &[("origin", "null")],
        Some(json!({ "token_hash": auth::hash("trc1.sandboxed") })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("opaque-origin"),
        "the refusal should name the reason: {v}"
    );

    // The token was never set, which is how we know nothing ran.
    let (s, _, _) = call(&n, "GET", "/api/node", REMOTE, "tracon.example", &[], None).await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "the sandboxed POST must not have configured a token"
    );

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

    // A same-site sandboxed frame does get the cookie attached by the
    // browser — SameSite is site-scoped, and the preview listener shares this
    // node's site. The credential is not enough: the opaque origin is refused.
    for (method, uri) in [
        ("POST", "/api/sessions/abc/kill"),
        ("GET", "/api/node"),
        ("GET", "/api/queue"),
    ] {
        let (s, _, v) = call(
            &n,
            method,
            uri,
            REMOTE,
            "tracon.example",
            &[("cookie", &cookie), ("origin", "null")],
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {v}");
    }

    // Nor does the bearer token buy an opaque origin a way in.
    let (s, _, _) = call(
        &n,
        "GET",
        "/api/node",
        REMOTE,
        "tracon.example",
        &[("authorization", "Bearer trc1.secret"), ("origin", "null")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Even the public shell, which needs no credential at all.
    let (s, _, _) = call(
        &n,
        "GET",
        "/reviews/abc",
        REMOTE,
        "tracon.example",
        &[("origin", "null")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // The node's own origin still works, so the refusal is about opacity and
    // not about sending an Origin at all.
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

/// The middleware is one half; the browser holds the other. The operator
/// response carries a CSP that refuses to be framed and frames only the
/// preview origin, the cookie is withheld from cross-site writes, and the one
/// sandboxed surface the node hosts can originate no request at all.
#[tokio::test]
async fn the_browser_side_defences_back_the_middleware() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;

    let mut req = Request::builder()
        .method("GET")
        .uri("/api/node")
        .header("host", "127.0.0.1:7420")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(LOCAL.parse::<SocketAddr>().unwrap()));
    let res = n.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let csp = res.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(csp.contains("object-src 'none'"), "{csp}");
    assert!(csp.contains("base-uri 'none'"), "{csp}");
    // The only thing the operator page may frame is the preview listener,
    // which is where the sandboxed viewer lives.
    assert!(csp.contains("frame-src http://127.0.0.1:7422"), "{csp}");
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
    assert_eq!(res.headers()["referrer-policy"], "no-referrer");

    // That sandboxed viewer is the one context that legitimately has an
    // opaque origin, and its own CSP leaves it nothing to make a request
    // with — so no route needs an `Origin: null` exception.
    let preview = tracon::http::preview::PREVIEW_CSP;
    assert!(preview.contains("connect-src 'none'"), "{preview}");
    assert!(preview.contains("form-action 'none'"), "{preview}");
    assert!(preview.contains("frame-src 'none'"), "{preview}");

    // And the cookie the browser would attach is withheld from cross-site
    // writes and unreadable by script.
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
    let raw = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie");
    assert!(raw.contains("HttpOnly"), "{raw}");
    assert!(raw.contains("Secure"), "{raw}");
    assert!(raw.contains("SameSite=Lax"), "{raw}");
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

/// The door for a harness the operator runs themselves is on the operator
/// router, so it is guarded exactly like the rest of it: loopback is the
/// operator, and anything else presents the token.
#[tokio::test]
async fn the_external_door_is_guarded_like_the_operator_api() {
    state::isolate();
    let n = node();
    let body = Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }));

    // Off by default, and the refusal comes from the handler rather than the
    // guard: loopback got through.
    let (s, _, v) = call(
        &n,
        "POST",
        "/mcp/external/work",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        body.clone(),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[external]"),
        "{v}"
    );

    set_token(&n, "trc1.secret").await;

    // From elsewhere, without the token, the guard answers first.
    let (s, _, _) = call(
        &n,
        "POST",
        "/mcp/external/work",
        REMOTE,
        "tracon.example",
        &[],
        body.clone(),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // With it, the request reaches the handler, which refuses for its own
    // reason. Either way nothing runs; the point is which gate answered.
    let (s, _, v) = call(
        &n,
        "POST",
        "/mcp/external/work",
        REMOTE,
        "tracon.example",
        &[("authorization", "Bearer trc1.secret")],
        body,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("[external]"),
        "{v}"
    );
}
