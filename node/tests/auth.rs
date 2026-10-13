//! Who the operator API answers to. Loopback keeps working without a
//! credential; everything else needs the token, or a cookie earned with it.

#[path = "support/mod.rs"]
mod support;
use support::{soft_passkey::SoftPasskey, state};

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
    let mut cfg = Config::default();
    // Where passkeys are anchored, so a remote admin action can be confirmed.
    cfg.auth.public_origin = Some(PUBLIC_ORIGIN.into());
    let cfg = Arc::new(cfg);
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
    // Providers wired as `serve` wires them, so a sign-in can start.
    store.channel_put("personal", &[], "{}").unwrap();
    manager.set_providers(tracon::providers::Providers::new(
        cfg.clone(),
        tools.broker.clone(),
        proto::envelope::DataKey::from_bytes([7; 32]),
        "n1".into(),
        manager.bus().clone(),
    ));
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
    let mut b = Request::builder().method(method).uri(uri);
    // An empty host sends none at all, as a bare local client may.
    if !host.is_empty() {
        b = b.header("host", host);
    }
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
const PUBLIC_ORIGIN: &str = "https://tracon.example";

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

    // The status endpoint deliberately requires the cookie even on loopback.
    let cookie = cookie_of(&cookies);
    let (s, _, v) = call(
        &n,
        "GET",
        "/api/admin/access",
        LOCAL,
        "127.0.0.1:7420",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["authenticated"], true, "{v}");

    let (s, cleared, _) = call(
        &n,
        "POST",
        "/api/logout",
        LOCAL,
        "127.0.0.1:7420",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let raw = cleared
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a clearing cookie");
    assert!(!raw.contains("Secure"), "{raw}");
    assert!(raw.contains("Max-Age=0"), "{raw}");
}

/// A TLS-terminating proxy may itself connect over loopback. Its external host
/// still needs a Secure cookie even though the node sees a local TCP peer.
#[tokio::test]
async fn a_loopback_reverse_proxy_keeps_the_remote_cookie_secure() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;

    let (s, cookies, _) = call(
        &n,
        "POST",
        "/api/login",
        LOCAL,
        "tracon.example",
        &[("origin", "https://tracon.example")],
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let raw = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie");
    assert!(raw.contains("Secure"), "{raw}");
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

/// What a same-host reverse proxy sends on behalf of a caller somewhere else.
/// Each connects to the node from loopback.
const PROXIES: &[(&str, &[(&str, &str)])] = &[
    // `tailscale serve`: the tailnet name in Host and the caller's address in
    // the forwarding headers.
    (
        "laptop.tail1234.ts.net",
        &[
            ("x-forwarded-for", "100.101.102.103"),
            ("x-forwarded-host", "laptop.tail1234.ts.net"),
            ("x-forwarded-proto", "https"),
        ],
    ),
    // A proxy that rewrites Host to its upstream and says whom it forwards
    // for: a pod's sidecar, Go's default reverse proxy, cloudflared.
    ("127.0.0.1:7420", &[("x-forwarded-for", "198.51.100.4")]),
    (
        "127.0.0.1:7420",
        &[("forwarded", "for=198.51.100.4;proto=https")],
    ),
    ("127.0.0.1:7420", &[("x-real-ip", "198.51.100.4")]),
    // One that passes the public Host through and adds nothing.
    ("tracon-node.example.org", &[]),
];

/// Every kind of decision that used to rest on the peer address alone, made
/// for a request a proxy on the node's own host relayed: none of them treats
/// it as the operator at the machine.
#[tokio::test]
async fn a_request_relayed_by_a_same_host_proxy_is_not_local() {
    state::isolate();
    let n = node();
    // `/api/nodes` reports the flag on this node's own row.
    let mut me = n.store.get_node("n1").unwrap().unwrap();
    me.is_self = 1;
    n.store.put_node(&me).unwrap();

    // 1. The guard's credential-less branch: with no token the node answers
    //    nobody but the machine, and a relayed request is not the machine.
    for (host, forwarded) in PROXIES {
        let (s, _, v) = call(&n, "GET", "/api/node", LOCAL, host, forwarded, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{host} {forwarded:?}: {v}");
    }

    set_token(&n, "trc1.secret").await;

    // With a token, a relayed request without a credential is asked for one.
    for (host, forwarded) in PROXIES {
        let (s, _, v) = call(&n, "GET", "/api/node", LOCAL, host, forwarded, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{host} {forwarded:?}: {v}");
    }

    // A relayed login gets a `Secure` cookie: the browser is on HTTPS at the
    // proxy, not on this machine's plain HTTP.
    let (host, forwarded) = PROXIES[1];
    let (s, cookies, _) = call(
        &n,
        "POST",
        "/api/login",
        LOCAL,
        host,
        forwarded,
        Some(json!({ "token": "trc1.secret" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let raw = cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie");
    assert!(raw.contains("Secure"), "{raw}");
    let cookie = cookie_of(&cookies);

    // 2. `Loopback`: a live cookie relayed through the proxy reaches none of
    //    the routes that rewrite what the node is.
    let gated: &[(&str, &str, Option<Value>)] = &[
        ("PUT", "/api/config", Some(json!({}))),
        ("GET", "/api/model-catalogue", None),
        ("POST", "/api/providers", Some(json!({}))),
        ("POST", "/api/manifest/skills", Some(json!({}))),
        ("PUT", "/api/manifest/text", Some(json!({}))),
        ("DELETE", "/api/manifest/skill/x", None),
        ("POST", "/api/mesh/init", Some(json!({}))),
        ("POST", "/api/mesh/unpair", Some(json!({}))),
        ("GET", "/api/mesh/enroll", None),
        ("POST", "/api/mesh/enroll", Some(json!({}))),
        ("POST", "/api/admin/policy/initialize", Some(json!({}))),
        ("POST", "/api/admin/policy/preview", Some(json!({}))),
        ("POST", "/api/admin/policy/apply", Some(json!({}))),
        (
            "POST",
            "/api/admin/maintenance/boundary-check",
            Some(json!({})),
        ),
        ("POST", "/api/admin/maintenance/restart", Some(json!({}))),
        ("POST", "/api/admin/maintenance/install", Some(json!({}))),
        ("POST", "/api/admin/maintenance/uninstall", Some(json!({}))),
        ("POST", "/api/maintenance/storage", Some(json!({}))),
        ("PUT", "/api/repos/environments", Some(json!({}))),
        ("POST", "/api/repos/environments/build", Some(json!({}))),
    ];
    for (host, forwarded) in PROXIES {
        let mut headers = forwarded.to_vec();
        headers.push(("cookie", &cookie));
        for (method, path, body) in gated {
            let (s, _, v) = call(&n, method, path, LOCAL, host, &headers, body.clone()).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{method} {path} via {host}: {v}");
            assert!(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("at the node itself"),
                "{method} {path} via {host} was refused by something other than the loopback gate: {v}"
            );
        }

        // 3. What the interface is told: not local, so it disables the
        //    controls rather than offering ones that will be refused.
        let (s, _, v) = call(&n, "GET", "/api/node", LOCAL, host, &headers, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["loopback"], false, "{host}: {v}");
        let (s, _, v) = call(&n, "GET", "/api/nodes", LOCAL, host, &headers, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let me = v
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["is_self"] == true)
            .expect("this node's own row");
        assert_eq!(me["loopback"], false, "{host}: {me}");
        let (s, _, v) = call(&n, "GET", "/api/admin/access", LOCAL, host, &headers, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["local"], false, "{host}: {v}");
        assert_eq!(v["authenticated"], true, "{host}: {v}");
    }

    // 4. A push subscription belongs to the machine only when it comes from
    //    the machine: a relayed bearer client holds no session to tie it to.
    let (host, forwarded) = PROXIES[1];
    let mut headers = forwarded.to_vec();
    headers.push(("authorization", "Bearer trc1.secret"));
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/push/subscriptions",
        LOCAL,
        host,
        &headers,
        Some(json!({
            "endpoint": "https://push.example/relayed",
            "keys": {
                "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
                "auth": "BTBZMqHH6r4Tts7J_aSIgg"
            }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("logged-in browser"),
        "{v}"
    );
}

/// The same routes, asked directly on the machine: the CLI (a Host and no
/// Origin) and the desktop app's webview (the node's own origin) are still
/// the operator, with or without a token.
#[tokio::test]
async fn a_direct_request_on_the_machine_is_still_local() {
    state::isolate();
    let n = node();
    let cli: &[(&str, &str)] = &[];
    let desktop: &[(&str, &str)] = &[("origin", "http://127.0.0.1:7420")];

    for token in [false, true] {
        if token {
            set_token(&n, "trc1.secret").await;
        }
        for headers in [cli, desktop] {
            let (s, _, v) = call(
                &n,
                "GET",
                "/api/node",
                LOCAL,
                "127.0.0.1:7420",
                headers,
                None,
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{v}");
            assert_eq!(v["loopback"], true, "{headers:?}: {v}");

            let (s, _, v) = call(
                &n,
                "GET",
                "/api/admin/access",
                LOCAL,
                "127.0.0.1:7420",
                headers,
                None,
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{v}");
            assert_eq!(v["local"], true, "{headers:?}: {v}");

            // A `Loopback` route answers with its own result, not the gate's.
            let (s, _, v) = call(
                &n,
                "GET",
                "/api/mesh/enroll",
                LOCAL,
                "127.0.0.1:7420",
                headers,
                None,
            )
            .await;
            assert_ne!(s, StatusCode::FORBIDDEN, "{headers:?}: {v}");
            assert_ne!(s, StatusCode::UNAUTHORIZED, "{headers:?}: {v}");
        }
    }

    // `kubectl port-forward` to another local port, and `just dev`'s Vite
    // server, still reach the node with a loopback Host and Origin.
    for (host, headers) in [
        ("localhost:8080", cli),
        ("localhost:5173", &[("origin", "http://localhost:5173")][..]),
    ] {
        let (s, _, v) = call(&n, "GET", "/api/node", LOCAL, host, headers, None).await;
        assert_eq!(s, StatusCode::OK, "{host}: {v}");
        assert_eq!(v["loopback"], true, "{host}: {v}");
    }
}

/// A page on another loopback origin is another site, not the operator. The
/// OpenCode UI listener (`127.0.0.1:7423`) and any other local dev server can
/// send a simple cross-origin POST, which needs no preflight; were a loopback
/// Origin enough, it could kill sessions with no credential at all.
#[tokio::test]
async fn a_page_on_another_loopback_origin_cannot_drive_the_node() {
    state::isolate();
    let n = node();
    for origin in ["http://127.0.0.1:7423", "http://localhost:3000"] {
        let (s, _, v) = call(
            &n,
            "POST",
            "/api/sessions/s1/kill",
            LOCAL,
            "127.0.0.1:7420",
            &[("origin", origin)],
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{origin}: {v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("tracon auth issue"),
            "{origin} was refused by something other than the guard: {v}"
        );
    }

    // With a token the page is simply unauthenticated.
    set_token(&n, "trc1.secret").await;
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/sessions/s1/kill",
        LOCAL,
        "127.0.0.1:7420",
        &[("origin", "http://127.0.0.1:7423")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");

    // The node's own page, and the CLI, reach the handler, which answers for
    // itself about a session that does not exist.
    for headers in [&[("origin", "http://127.0.0.1:7420")][..], &[][..]] {
        let (s, _, v) = call(
            &n,
            "POST",
            "/api/sessions/s1/kill",
            LOCAL,
            "127.0.0.1:7420",
            headers,
            None,
        )
        .await;
        assert_ne!(s, StatusCode::FORBIDDEN, "{headers:?}: {v}");
        assert_ne!(s, StatusCode::UNAUTHORIZED, "{headers:?}: {v}");
    }
}

/// A request with no `Host` at all is not a browser, so it is not a
/// rebinding vector: from loopback, directly, it is the operator.
#[tokio::test]
async fn a_bare_local_request_without_a_host_is_local() {
    state::isolate();
    let n = node();
    let (s, _, v) = call(&n, "GET", "/api/node", LOCAL, "", &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["loopback"], true, "{v}");
    // Relayed, it is not.
    let (s, _, v) = call(
        &n,
        "GET",
        "/api/node",
        LOCAL,
        "",
        &[("x-forwarded-for", "198.51.100.4")],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

/// A pending sign-in is handed back as it started; cancel it so the next
/// attempt starts its own.
async fn cancel_sign_in(n: &Node) {
    let (s, _, v) = call(
        n,
        "POST",
        "/api/providers/anthropic/disconnect",
        LOCAL,
        "127.0.0.1:7420",
        &[],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// The provider sign-in only offers a callback on this machine's loopback to
/// a browser on this machine. Relayed through a proxy on the node's host, the
/// browser is elsewhere, and a loopback redirect would land on its own
/// machine, so it is given the paste flow.
#[tokio::test]
async fn only_a_local_browser_is_offered_the_local_sign_in_callback() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let body = json!({ "channels": ["personal"], "local_callback": true });

    let (s, _, v) = call(
        &n,
        "POST",
        "/api/providers/anthropic/connect",
        LOCAL,
        "127.0.0.1:7420",
        &[("origin", "http://127.0.0.1:7420")],
        Some(body.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["completion"], "local_callback", "{v}");
    cancel_sign_in(&n).await;

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
    let cookie = cookie_of(&cookies);
    // Starting a sign-in from off the machine is an admin action: the paired
    // browser enrolls a passkey and confirms each one with it.
    let mut key = SoftPasskey::new(1, PUBLIC_ORIGIN);
    let (s, _, opts) = call(
        &n,
        "POST",
        "/api/passkeys/register/options",
        REMOTE,
        "tracon.example",
        &[("cookie", &cookie)],
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{opts}");
    let credential = key.create(&opts["options"]);
    let (s, _, v) = call(
        &n,
        "POST",
        "/api/passkeys/register/finish",
        REMOTE,
        "tracon.example",
        &[("cookie", &cookie)],
        Some(json!({ "challenge_id": opts["challenge_id"], "credential": credential })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for (host, forwarded) in PROXIES {
        let mut headers = forwarded.to_vec();
        headers.push(("cookie", &cookie));
        let (s, _, refused) = call(
            &n,
            "POST",
            "/api/providers/anthropic/connect",
            LOCAL,
            host,
            &headers,
            Some(body.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{host}: {refused}");
        assert_eq!(
            refused["error"]["kind"], "step_up_required",
            "{host}: {refused}"
        );
        let step_up = &refused["step_up"];
        let credential = key.get(&step_up["options"]);
        let (s, _, finished) = call(
            &n,
            "POST",
            "/api/passkeys/assert/finish",
            LOCAL,
            host,
            &headers,
            Some(json!({ "challenge_id": step_up["challenge_id"], "credential": credential })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{host}: {finished}");
        let proof = finished["proof"].as_str().unwrap().to_string();
        headers.push((tracon::http::passkey::STEP_UP_HEADER, &proof));
        let (s, _, v) = call(
            &n,
            "POST",
            "/api/providers/anthropic/connect",
            LOCAL,
            host,
            &headers,
            Some(body.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{host}: {v}");
        assert_eq!(v["completion"], "paste", "{host}: {v}");
        cancel_sign_in(&n).await;
    }
}

/// The administrator screens tell the interface whether it is at the node, so
/// it can disable what only works there. Both need a credential even on the
/// machine; only the flag differs.
#[tokio::test]
async fn the_admin_screens_report_local_only_for_a_direct_request() {
    state::isolate();
    let n = node();
    set_token(&n, "trc1.secret").await;
    let bearer = ("authorization", "Bearer trc1.secret");
    for path in ["/api/admin/mesh", "/api/admin/maintenance"] {
        let (s, _, v) = call(&n, "GET", path, LOCAL, "127.0.0.1:7420", &[bearer], None).await;
        assert_eq!(s, StatusCode::OK, "{path}: {v}");
        assert_eq!(v["local"], true, "{path}: {v}");
        for (host, forwarded) in PROXIES {
            let mut headers = forwarded.to_vec();
            headers.push(bearer);
            let (s, _, v) = call(&n, "GET", path, LOCAL, host, &headers, None).await;
            assert_eq!(s, StatusCode::OK, "{path} via {host}: {v}");
            assert_eq!(v["local"], false, "{path} via {host}: {v}");
        }
    }
}
