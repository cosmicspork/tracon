//! Device approval, end to end through the guard, the store and the real
//! passkey verifier.
//!
//! The asking window is the desktop app against a remote node: a request from
//! a non-loopback peer at the public origin that cannot make a passkey. The
//! approving browser is a paired phone holding a [`SoftPasskey`].

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
        passkey::STEP_UP_HEADER,
    },
    mcp::Tools,
    session::Manager,
    store::{now_ms, DeviceApprovalRow, Store},
    stream::Bus,
};

use support::fake::FakeAdapter;

const ORIGIN: &str = "https://tracon.example.org";
const HOST: &str = "tracon.example.org";
const PHONE: &str = "203.0.113.7:5000";
const DESKTOP: &str = "198.51.100.20:6000";
const LOCAL: &str = "127.0.0.1:5000";
const TOKEN: &str = "trc1.secret";
const DESKTOP_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 tracon-desktop";

struct Node {
    app: axum::Router,
    store: Arc<Store>,
}

fn node_with(public_origin: Option<&str>) -> Node {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let mut cfg = Config::default();
    cfg.auth.public_origin = public_origin.map(str::to_string);
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
    store.channel_put("personal", &[], "{}").unwrap();
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

fn node() -> Node {
    node_with(Some(ORIGIN))
}

struct Reply {
    status: StatusCode,
    cookies: Vec<String>,
    body: Value,
}

impl Reply {
    fn kind(&self) -> &str {
        self.body["error"]["kind"].as_str().unwrap_or("")
    }

    fn session_cookie(&self) -> Option<String> {
        self.cookies
            .iter()
            .find(|c| c.starts_with("tracon_session="))
            .map(|raw| raw.split(';').next().unwrap().to_string())
    }
}

async fn send(
    n: &Node,
    method: &str,
    uri: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Reply {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header(
        "host",
        if peer == LOCAL {
            "127.0.0.1:7420"
        } else {
            HOST
        },
    );
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
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
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
    Reply {
        status,
        cookies,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn local(n: &Node, method: &str, uri: &str, body: Option<&Value>) -> Reply {
    send(n, method, uri, LOCAL, &[], body).await
}

/// A browser at the public origin, from `peer`, holding `cookie` (or none).
async fn at(
    n: &Node,
    peer: &str,
    cookie: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<&Value>,
    proof: Option<&str>,
) -> Reply {
    let mut headers = vec![("origin", ORIGIN), ("user-agent", DESKTOP_AGENT)];
    if let Some(c) = cookie {
        headers.push(("cookie", c));
    }
    if let Some(p) = proof {
        headers.push((STEP_UP_HEADER, p));
    }
    send(n, method, uri, peer, &headers, body).await
}

/// The phone: paired, at the public origin.
async fn phone(
    n: &Node,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<&Value>,
    proof: Option<&str>,
) -> Reply {
    at(n, PHONE, Some(cookie), method, uri, body, proof).await
}

async fn issue_token(n: &Node) {
    let r = local(
        n,
        "POST",
        "/api/auth/token",
        Some(&json!({ "token_hash": auth::hash(TOKEN) })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

async fn pair(n: &Node) -> String {
    let r = send(
        n,
        "POST",
        "/api/login",
        PHONE,
        &[("origin", ORIGIN)],
        Some(&json!({ "token": TOKEN })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.session_cookie().expect("a session cookie")
}

async fn enroll(n: &Node, cookie: &str, key: &mut SoftPasskey) -> Reply {
    let r = phone(
        n,
        cookie,
        "POST",
        "/api/passkeys/register/options",
        Some(&json!({ "label": "iPhone" })),
        None,
    )
    .await;
    if r.status != StatusCode::OK {
        return r;
    }
    let credential = key.create(&r.body["options"]);
    phone(
        n,
        cookie,
        "POST",
        "/api/passkeys/register/finish",
        Some(&json!({ "challenge_id": r.body["challenge_id"], "credential": credential })),
        None,
    )
    .await
}

async fn assert_with(n: &Node, cookie: &str, key: &mut SoftPasskey, step_up: &Value) -> Reply {
    let credential = key.get(&step_up["options"]);
    phone(
        n,
        cookie,
        "POST",
        "/api/passkeys/assert/finish",
        Some(&json!({ "challenge_id": step_up["challenge_id"], "credential": credential })),
        None,
    )
    .await
}

/// A node with a token and a paired phone holding the one passkey.
async fn with_phone() -> (Node, String, SoftPasskey) {
    let n = node();
    issue_token(&n).await;
    let cookie = pair(&n).await;
    let mut key = SoftPasskey::new(1, ORIGIN);
    let r = enroll(&n, &cookie, &mut key).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    (n, cookie, key)
}

/// The desktop window asks to be signed in.
async fn ask_sign_in(n: &Node) -> Value {
    let r = at(n, DESKTOP, None, "POST", "/api/auth/device", None, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body
}

async fn poll(n: &Node, cookie: Option<&str>, device_code: &Value) -> Reply {
    at(
        n,
        DESKTOP,
        cookie,
        "POST",
        "/api/auth/device/token",
        Some(&json!({ "device_code": device_code })),
        None,
    )
    .await
}

/// The code typed into the approving page.
fn typed(code: &str) -> Value {
    json!({ "user_code": code })
}

/// The phone approves `approval_id`, typing `user_code`: refused for want of
/// a passkey, then confirmed with `key`, then approved. Returns the
/// refusal's step-up.
async fn approve_with(
    n: &Node,
    cookie: &str,
    key: &mut SoftPasskey,
    approval_id: &str,
    user_code: &Value,
) -> Value {
    let path = format!("/api/auth/device/approvals/{approval_id}/approve");
    let body = typed(user_code.as_str().unwrap());
    let refused = phone(n, cookie, "POST", &path, Some(&body), None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.kind(), "step_up_required", "{}", refused.body);
    let finished = assert_with(n, cookie, key, &refused.body["step_up"]).await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    let proof = finished.body["proof"].as_str().unwrap();
    let r = phone(n, cookie, "POST", &path, Some(&body), Some(proof)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "approved");
    refused.body["step_up"].clone()
}

fn grant() -> Value {
    json!({
        "action": "terminal",
        "verdict": "deny",
        "target": "workspace",
        "channel": "personal",
        "reason": "test",
    })
}

async fn grants(n: &Node) -> usize {
    let r = local(n, "GET", "/api/authority/grants", None).await;
    r.body["grants"].as_array().unwrap().len()
}

#[tokio::test]
async fn a_window_is_signed_in_once_a_browser_approves_it_with_a_passkey() {
    let (n, phone_cookie, mut key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    let id = asked["approval_id"].as_str().unwrap();
    let code = asked["device_code"].as_str().unwrap();
    // The approval's ID rides in the fragment of the public origin; the
    // secret is in no URL.
    assert_eq!(asked["approve_url"], format!("{ORIGIN}/#approve={id}"));
    assert!(!asked["approve_url"].as_str().unwrap().contains(code));
    assert_eq!(asked["interval"], 5);
    assert_eq!(asked["expires_in"], 600);
    let user_code = asked["user_code"].as_str().unwrap();
    assert!(
        user_code.len() == 9 && user_code.as_bytes()[4] == b'-',
        "{user_code}"
    );

    // Not yet approved.
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert_eq!(r.kind(), "authorization_pending");
    assert!(r.session_cookie().is_none());

    // The phone sees who asked, but not the code: that it must type.
    let view = phone(
        &n,
        &phone_cookie,
        "GET",
        &format!("/api/auth/device/approvals/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert!(view.body.get("user_code").is_none(), "{}", view.body);
    assert_eq!(view.body["code_tries_left"], 3);
    assert_eq!(view.body["kind"], "sign-in");
    assert_eq!(view.body["status"], "pending");
    assert_eq!(view.body["requester_agent"], DESKTOP_AGENT);
    assert_eq!(view.body["requester_addr"], "198.51.100.20");
    assert!(view.body.get("device_code").is_none());

    // Approving is confirmed with the passkey, for this request.
    let step_up = approve_with(&n, &phone_cookie, &mut key, id, &asked["user_code"]).await;
    assert_eq!(step_up["summary"], "Sign in another window");
    // The prompt never repeats the code.
    assert!(!step_up.to_string().contains(&user_code.replace('-', "")));
    assert!(!step_up.to_string().contains(user_code));

    // The window collects its session (after waiting its interval).
    tokio::time::sleep(std::time::Duration::from_millis(4_100)).await;
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["kind"], "sign-in");
    let raw = r
        .cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a cookie");
    assert!(raw.contains("HttpOnly") && raw.contains("Secure") && raw.contains("SameSite=Lax"));
    // Thirty days, not a paired session's ninety.
    assert!(raw.contains("Max-Age=2592000"), "{raw}");
    let desktop = r.session_cookie().unwrap();
    let row = n
        .store
        .auth_session_live(
            &auth::hash(desktop.trim_start_matches("tracon_session=")),
            now_ms(),
        )
        .unwrap()
        .unwrap();
    assert!(row.expires_ms - row.created_ms <= 30 * 24 * 60 * 60 * 1000);

    // The cookie works like any other.
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "GET",
        "/api/passkeys",
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // It is recorded as delegated, by the phone's session.
    let sessions = local(&n, "GET", "/api/auth/sessions", None).await;
    let delegated: Vec<&Value> = sessions.body["clients"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| !c["delegated_from"].is_null())
        .collect();
    assert_eq!(delegated.len(), 1, "{}", sessions.body);
    let phone_id = &auth::hash(phone_cookie.trim_start_matches("tracon_session="))[..12];
    assert_eq!(delegated[0]["delegated_from"], phone_id);
    assert_eq!(delegated[0]["user_agent"], DESKTOP_AGENT);

    // Collected once: the secret is spent.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "invalid_grant", "{}", r.body);
    assert!(r.session_cookie().is_none());
    // And the approval cannot be decided again.
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        &format!("/api/auth/device/approvals/{id}/approve"),
        Some(&typed(user_code)),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
    let row = n.store.device_approval_get(id).unwrap().unwrap();
    assert_eq!(row.status, "consumed");
}

#[tokio::test]
async fn a_denied_request_says_so_and_cannot_be_approved_after() {
    let (n, phone_cookie, mut key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    let id = asked["approval_id"].as_str().unwrap();
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        &format!("/api/auth/device/approvals/{id}/deny"),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "denied");
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "access_denied", "{}", r.body);
    assert!(r.session_cookie().is_none());
    let path = format!("/api/auth/device/approvals/{id}/approve");
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        &path,
        Some(&typed(asked["user_code"].as_str().unwrap())),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
    let _ = &mut key;
    let view = phone(
        &n,
        &phone_cookie,
        "GET",
        &format!("/api/auth/device/approvals/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(view.body["status"], "denied");
}

#[tokio::test]
async fn an_expired_request_cannot_be_approved_or_collected() {
    let (n, phone_cookie, _key) = with_phone().await;
    let now = now_ms();
    let row = DeviceApprovalRow {
        id: "old".into(),
        kind: "sign-in".into(),
        device_code_hash: auth::hash("old-secret"),
        user_code: "BCDFGHJK".into(),
        requester_session: None,
        requester_agent: None,
        requester_addr: None,
        op_hash: None,
        action: None,
        summary: None,
        status: "pending".into(),
        approver: None,
        issued: None,
        code_misses: 0,
        created_ms: now - 11 * 60 * 1000,
        expires_ms: now - 60 * 1000,
        interval_ms: 5_000,
        last_poll_ms: None,
        decided_ms: None,
        consumed_ms: None,
    };
    assert!(n.store.device_approval_insert(&row, None, now).unwrap());
    let r = poll(&n, None, &json!("old-secret")).await;
    assert_eq!(r.kind(), "expired_token", "{}", r.body);
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        "/api/auth/device/approvals/old/approve",
        Some(&typed("BCDF-GHJK")),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expired"),
        "{}",
        r.body
    );
    // Even at the node, where approving needs no passkey.
    let r = local(
        &n,
        "POST",
        "/api/auth/device/approvals/old/approve",
        Some(&typed("BCDF-GHJK")),
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
}

#[tokio::test]
async fn polling_faster_than_asked_is_told_to_slow_down() {
    let (n, _phone, _key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "authorization_pending", "{}", r.body);
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "slow_down", "{}", r.body);
    assert_eq!(r.body["interval"], 10);
    // A code nobody was given.
    let r = poll(&n, None, &json!("not-a-code")).await;
    assert_eq!(r.kind(), "invalid_grant", "{}", r.body);
}

#[tokio::test]
async fn a_cookie_alone_never_approves_and_a_bearer_token_never_decides() {
    let (n, phone_cookie, _key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    let id = asked["approval_id"].as_str().unwrap();
    let path = format!("/api/auth/device/approvals/{id}/approve");
    let code = typed(asked["user_code"].as_str().unwrap());
    // A replayed or foreign proof is no proof.
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        &path,
        Some(&code),
        Some("made-up"),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    // A second paired browser without the passkey gets only the challenge.
    let other = pair(&n).await;
    let r = phone(&n, &other, "POST", &path, Some(&code), None).await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    // A bearer token can neither look nor decide.
    let bearer = format!("Bearer {TOKEN}");
    for (method, p) in [
        ("GET", format!("/api/auth/device/approvals/{id}")),
        ("POST", path.clone()),
        ("POST", format!("/api/auth/device/approvals/{id}/deny")),
    ] {
        let body = (method == "POST").then_some(&code);
        let r = send(&n, method, &p, PHONE, &[("authorization", &bearer)], body).await;
        assert_eq!(r.kind(), "step_up_unavailable", "{p}: {}", r.body);
        assert_eq!(r.body["error"]["reason"], "no_session", "{p}");
    }
    let r = send(
        &n,
        "POST",
        "/api/auth/device/stepup",
        PHONE,
        &[("authorization", &bearer)],
        Some(&json!({ "challenge_id": "x" })),
    )
    .await;
    assert_eq!(r.body["error"]["reason"], "no_session", "{}", r.body);
    // Nothing above approved it.
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "authorization_pending", "{}", r.body);
}

/// The desktop window, signed in, makes an admin action. It cannot make the
/// passkey, so it hands the challenge to a device approval; the phone
/// approves with its passkey; the window collects a proof bound to that
/// operation and its own session, and the retry goes through.
#[tokio::test]
async fn a_windows_admin_action_is_confirmed_by_a_browser_for_that_operation_only() {
    let (n, phone_cookie, mut key) = with_phone().await;
    // The window's own session (a second pairing is as good as a delegated
    // one here, and has no window to enroll).
    let desktop = pair(&n).await;
    let body = grant();
    let refused = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/authority/grants",
        Some(&body),
        None,
    )
    .await;
    assert_eq!(refused.kind(), "step_up_required", "{}", refused.body);
    let op_hash = refused.body["step_up"]["op_hash"].clone();

    // Another session cannot turn the window's challenge into an approval.
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        "/api/auth/device/stepup",
        Some(&json!({ "challenge_id": refused.body["step_up"]["challenge_id"] })),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);

    let asked = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/auth/device/stepup",
        Some(&json!({ "challenge_id": refused.body["step_up"]["challenge_id"] })),
        None,
    )
    .await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    assert_eq!(asked.body["kind"], "step-up");
    assert_eq!(asked.body["op_hash"], op_hash);
    assert_eq!(asked.body["summary"], "Create an authority grant");
    let id = asked.body["approval_id"].as_str().unwrap().to_string();
    let device_code = asked.body["device_code"].clone();
    // The challenge is spent: it cannot also be answered directly.
    let r = assert_with(&n, &desktop, &mut key, &refused.body["step_up"]).await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);

    // The window cannot approve its own request without a passkey.
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        &format!("/api/auth/device/approvals/{id}/approve"),
        Some(&typed(asked.body["user_code"].as_str().unwrap())),
        None,
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);

    // The phone sees the node's line for it, and approves with its passkey.
    let view = phone(
        &n,
        &phone_cookie,
        "GET",
        &format!("/api/auth/device/approvals/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(view.body["summary"], "Create an authority grant");
    assert_eq!(view.body["action"], "POST /api/authority/grants");
    // Which session asked, so the operator can tell it is theirs.
    let desktop_id = &auth::hash(desktop.trim_start_matches("tracon_session="))[..12];
    assert_eq!(view.body["requester_session"], desktop_id);
    assert!(view.body["requester_delegated_from"].is_null());
    let step_up = approve_with(&n, &phone_cookie, &mut key, &id, &asked.body["user_code"]).await;
    assert_eq!(
        step_up["summary"],
        "Confirm for another window: Create an authority grant"
    );
    assert_eq!(step_up["action"], "POST /api/authority/grants");

    // Only the window's own session collects it.
    let r = poll(&n, None, &device_code).await;
    assert_eq!(r.kind(), "invalid_grant", "{}", r.body);
    let r = poll(&n, Some(&phone_cookie), &device_code).await;
    assert_eq!(r.kind(), "invalid_grant", "{}", r.body);
    let r = poll(&n, Some(&desktop), &device_code).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["op_hash"], op_hash);
    assert!(r.session_cookie().is_none(), "a step-up mints no session");
    let proof = r.body["proof"].as_str().unwrap().to_string();
    let r = poll(&n, Some(&desktop), &device_code).await;
    assert_eq!(r.kind(), "invalid_grant", "collected once: {}", r.body);

    // The proof confirms no other operation, and no other session.
    let mut other = grant();
    other["verdict"] = json!("allow");
    other["session_id"] = json!("s1");
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/authority/grants",
        Some(&other),
        Some(&proof),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    assert_eq!(grants(&n).await, 0);

    // It confirms the one it was given for, once.
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(grants(&n).await, 1);
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "replayed: {}", r.body);
    assert_eq!(grants(&n).await, 1);
}

#[tokio::test]
async fn a_delegated_session_has_no_window_to_enroll_the_first_passkey() {
    // No passkey yet: only the node itself can approve.
    let n = node();
    issue_token(&n).await;
    let asked = ask_sign_in(&n).await;
    let id = asked["approval_id"].as_str().unwrap();
    // At the node, approving needs no passkey (but still the code).
    let r = local(
        &n,
        "POST",
        &format!("/api/auth/device/approvals/{id}/approve"),
        Some(&typed(asked["user_code"].as_str().unwrap())),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let desktop = r.session_cookie().unwrap();

    let listed = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "GET",
        "/api/passkeys",
        None,
        None,
    )
    .await;
    assert_eq!(listed.body["enrollment"]["open"], false, "{}", listed.body);
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/passkeys/register/options",
        Some(&json!({ "label": "desktop" })),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.body["error"]["reason"], "enrollment_closed", "{}", r.body);
    let row = n
        .store
        .auth_session_live(
            &auth::hash(desktop.trim_start_matches("tracon_session=")),
            now_ms(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(row.paired_ms, None);
    assert_eq!(row.delegated_from.as_deref(), Some("local"));
}

#[tokio::test]
async fn at_the_node_nothing_needs_a_device_approval() {
    let (n, _phone, _key) = with_phone().await;
    // An admin action at the node goes straight through.
    let r = local(&n, "POST", "/api/authority/grants", Some(&grant())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // So there is no step-up to hand to a device approval.
    let r = local(
        &n,
        "POST",
        "/api/auth/device/stepup",
        Some(&json!({ "challenge_id": "x" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
}

#[tokio::test]
async fn asking_is_bounded_and_needs_a_token_a_public_origin_and_this_origin() {
    // No public origin: there is nowhere to approve.
    let n = node_with(None);
    issue_token(&n).await;
    let r = at(&n, DESKTOP, None, "POST", "/api/auth/device", None, None).await;
    assert_eq!(r.kind(), "step_up_unavailable", "{}", r.body);
    assert_eq!(r.body["error"]["reason"], "no_public_origin");

    // No token: nothing to sign in to, and the guard answers loopback only.
    let n = node();
    let r = local(&n, "POST", "/api/auth/device", None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    let r = at(&n, DESKTOP, None, "POST", "/api/auth/device", None, None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);

    // Another site's page cannot ask or poll.
    issue_token(&n).await;
    for path in ["/api/auth/device", "/api/auth/device/token"] {
        let r = send(
            &n,
            "POST",
            path,
            DESKTOP,
            &[("origin", "https://evil.example")],
            Some(&json!({ "device_code": "x" })),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}: {}", r.body);
    }

    // Per address: four waiting at once.
    let ask = |peer: String| {
        let n = &n;
        async move { at(n, &peer, None, "POST", "/api/auth/device", None, None).await }
    };
    for i in 0..4 {
        let r = ask(DESKTOP.into()).await;
        assert_eq!(r.status, StatusCode::OK, "{i}: {}", r.body);
    }
    let r = ask(DESKTOP.into()).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("waiting"),
        "{}",
        r.body
    );
    // And ten asks in a window, refused or not.
    for _ in 0..5 {
        ask(DESKTOP.into()).await;
    }
    let r = ask(DESKTOP.into()).await;
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("wait a few minutes"),
        "{}",
        r.body
    );
    // No more than sixteen waiting at once, from anywhere.
    for i in 0..12 {
        let peer = format!("198.51.100.{}:6000", 100 + i / 4);
        let r = ask(peer).await;
        assert_eq!(r.status, StatusCode::OK, "{i}: {}", r.body);
    }
    let r = ask("198.51.100.200:6000".into()).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("waiting"),
        "{}",
        r.body
    );
}

/// The PoC that blocked the first version: a session with no passkey asks for
/// a challenge for an operation hash it computed itself, with no request
/// behind it, and hands it to a device approval. The phone would have been
/// asked to approve "an admin action", and the proof would have replaced the
/// operator token. A challenge that does not say what it is for cannot
/// become an approval.
#[tokio::test]
async fn a_computed_operation_without_a_refused_request_is_never_approvable() {
    let (n, _phone, _key) = with_phone().await;
    let desktop = pair(&n).await;
    let body = json!({ "token_hash": auth::hash("trc1.attacker") });
    let op = tracon::http::passkey::op_hash(
        &axum::http::Method::POST,
        "/api/auth/token",
        None,
        body.to_string().as_bytes(),
    );
    let opened = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/passkeys/assert/options",
        Some(&json!({ "op_hash": op })),
        None,
    )
    .await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    assert!(opened.body.get("action").is_none(), "{}", opened.body);
    let r = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/auth/device/stepup",
        Some(&json!({ "challenge_id": opened.body["challenge_id"] })),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("make the change again"),
        "{}",
        r.body
    );
    assert!(n.store.device_approvals(10).unwrap().is_empty());
}

/// The honest refresh: a window whose challenge expired asks for a fresh one
/// for the same operation, and the node, having seen the request, carries
/// its description over, so the approval still says what it is for.
#[tokio::test]
async fn a_fresh_challenge_for_a_refused_request_keeps_its_description() {
    let (n, _phone, _key) = with_phone().await;
    let desktop = pair(&n).await;
    let refused = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/authority/grants",
        Some(&grant()),
        None,
    )
    .await;
    assert_eq!(refused.kind(), "step_up_required", "{}", refused.body);
    let fresh = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/passkeys/assert/options",
        Some(&json!({ "op_hash": refused.body["step_up"]["op_hash"] })),
        None,
    )
    .await;
    assert_eq!(fresh.body["summary"], "Create an authority grant");
    assert_eq!(fresh.body["action"], "POST /api/authority/grants");
    let asked = at(
        &n,
        DESKTOP,
        Some(&desktop),
        "POST",
        "/api/auth/device/stepup",
        Some(&json!({ "challenge_id": fresh.body["challenge_id"] })),
        None,
    )
    .await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    assert_eq!(asked.body["summary"], "Create an authority grant");
    assert_eq!(asked.body["action"], "POST /api/authority/grants");
}

#[tokio::test]
async fn the_approver_types_the_code_and_three_wrong_ones_deny_the_request() {
    let (n, phone_cookie, mut key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    let id = asked["approval_id"].as_str().unwrap();
    let path = format!("/api/auth/device/approvals/{id}/approve");
    // No code at all is not an approval.
    let r = phone(&n, &phone_cookie, "POST", &path, None, None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    // A wrong code is refused before any passkey is asked for.
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        &path,
        Some(&typed("ZZZZ-ZZZZ")),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_ne!(r.kind(), "step_up_required", "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("2 tries left"),
        "{}",
        r.body
    );
    // Typed as people type it, the right one goes on to the passkey.
    let lower = asked["user_code"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .replace('-', " ");
    let r = phone(&n, &phone_cookie, "POST", &path, Some(&typed(&lower)), None).await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    // Two more wrong ones, and it is denied, even with a passkey after.
    for _ in 0..2 {
        phone(
            &n,
            &phone_cookie,
            "POST",
            &path,
            Some(&typed("ZZZZ-ZZZZ")),
            None,
        )
        .await;
    }
    let row = n.store.device_approval_get(id).unwrap().unwrap();
    assert_eq!(row.status, "denied");
    assert_eq!(row.code_misses, 3);
    let r = phone(
        &n,
        &phone_cookie,
        "POST",
        &path,
        Some(&typed(asked["user_code"].as_str().unwrap())),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
    let _ = &mut key;
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "access_denied", "{}", r.body);
}

/// Only polls with a code that answers to nothing count against an address,
/// so a stranger cannot stop a real window collecting.
#[tokio::test]
async fn guessing_polls_are_limited_and_real_ones_are_not() {
    let (n, _phone, _key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    for i in 0..600 {
        let r = poll(&n, None, &json!(format!("guess-{i}"))).await;
        assert_eq!(r.kind(), "invalid_grant", "{i}: {}", r.body);
    }
    let r = poll(&n, None, &json!("guess-more")).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    // The guesser's address is spent; a window elsewhere is not, however
    // often it polls its real code (it is only told to slow down).
    for _ in 0..650 {
        let r = at(
            &n,
            "198.51.100.77:7000",
            None,
            "POST",
            "/api/auth/device/token",
            Some(&json!({ "device_code": asked["device_code"] })),
            None,
        )
        .await;
        assert!(
            matches!(r.kind(), "authorization_pending" | "slow_down"),
            "{}",
            r.body
        );
    }
}

/// Behind a proxy the node believes (loopback here), the address shown is
/// the forwarded one; a stranger's own `X-Forwarded-For` is not read.
#[tokio::test]
async fn the_address_shown_is_the_one_a_trusted_proxy_reports() {
    let (n, phone_cookie, _key) = with_phone().await;
    let view_of = |r: Reply| {
        let n = &n;
        let phone_cookie = phone_cookie.clone();
        async move {
            let id = r.body["approval_id"].as_str().unwrap().to_string();
            phone(
                n,
                &phone_cookie,
                "GET",
                &format!("/api/auth/device/approvals/{id}"),
                None,
                None,
            )
            .await
        }
    };
    let proxied = send(
        &n,
        "POST",
        "/api/auth/device",
        LOCAL,
        &[("x-forwarded-for", "6.6.6.6, 198.51.100.44")],
        None,
    )
    .await;
    assert_eq!(proxied.status, StatusCode::OK, "{}", proxied.body);
    assert_eq!(
        view_of(proxied).await.body["requester_addr"],
        "198.51.100.44"
    );
    let direct = send(
        &n,
        "POST",
        "/api/auth/device",
        DESKTOP,
        &[("x-forwarded-for", "198.51.100.44")],
        None,
    )
    .await;
    assert_eq!(
        view_of(direct).await.body["requester_addr"],
        "198.51.100.20"
    );
}

#[tokio::test]
async fn rotating_the_token_revokes_an_approval_not_yet_collected() {
    let (n, phone_cookie, mut key) = with_phone().await;
    let asked = ask_sign_in(&n).await;
    let id = asked["approval_id"].as_str().unwrap();
    approve_with(&n, &phone_cookie, &mut key, id, &asked["user_code"]).await;
    issue_token(&n).await;
    let r = poll(&n, None, &asked["device_code"]).await;
    assert_eq!(r.kind(), "invalid_grant", "{}", r.body);
    assert!(r.session_cookie().is_none());
    assert_eq!(
        n.store.device_approval_get(id).unwrap().unwrap().status,
        "revoked"
    );
}
