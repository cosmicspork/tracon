//! Passkeys, end to end through the guard and the real verifier.
//!
//! The browser and the authenticator are [`SoftPasskey`]: it answers the
//! node's own options with real ES256 signatures over real CBOR, so what
//! `webauthn-rs` checks here is what it checks for a phone. "Remote" is a
//! request from a non-loopback peer addressed to the public origin, as one
//! arrives through an ingress.

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
    store::{now_ms, AuthSessionRow, Store},
    stream::Bus,
};

use support::fake::FakeAdapter;

const ORIGIN: &str = "https://tracon.example.org";
const HOST: &str = "tracon.example.org";
const REMOTE: &str = "203.0.113.7:5000";
const LOCAL: &str = "127.0.0.1:5000";
const TOKEN: &str = "trc1.secret";

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
    let host = if peer == LOCAL {
        "127.0.0.1:7420"
    } else {
        HOST
    };
    b = b.header("host", host);
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

/// A browser at the public origin holding `cookie`.
async fn remote(
    n: &Node,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<&Value>,
    proof: Option<&str>,
) -> Reply {
    let mut headers = vec![("cookie", cookie), ("origin", ORIGIN)];
    if let Some(p) = proof {
        headers.push((STEP_UP_HEADER, p));
    }
    send(n, method, uri, REMOTE, &headers, body).await
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

/// Pair a browser: spend the token for a cookie, from off the machine.
async fn pair(n: &Node) -> String {
    let r = send(
        n,
        "POST",
        "/api/login",
        REMOTE,
        &[("origin", ORIGIN)],
        Some(&json!({ "token": TOKEN })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let raw = r
        .cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie");
    raw.split(';').next().unwrap().to_string()
}

async fn enroll(n: &Node, cookie: &str, key: &mut SoftPasskey, proof: Option<&str>) -> Reply {
    let body = json!({ "label": "iPhone" });
    let r = remote(
        n,
        cookie,
        "POST",
        "/api/passkeys/register/options",
        Some(&body),
        proof,
    )
    .await;
    if r.status != StatusCode::OK {
        return r;
    }
    let credential = key.create(&r.body["options"]);
    remote(
        n,
        cookie,
        "POST",
        "/api/passkeys/register/finish",
        Some(&json!({ "challenge_id": r.body["challenge_id"], "credential": credential })),
        None,
    )
    .await
}

/// Answer a `step_up_required` with `key`, returning the finish reply.
async fn assert_with(n: &Node, cookie: &str, key: &mut SoftPasskey, step_up: &Value) -> Reply {
    let credential = key.get(&step_up["options"]);
    remote(
        n,
        cookie,
        "POST",
        "/api/passkeys/assert/finish",
        Some(&json!({ "challenge_id": step_up["challenge_id"], "credential": credential })),
        None,
    )
    .await
}

/// Refused, confirmed, and the proof handed back.
async fn confirm(
    n: &Node,
    cookie: &str,
    key: &mut SoftPasskey,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> String {
    let refused = remote(n, cookie, method, uri, body, None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.kind(), "step_up_required", "{}", refused.body);
    let finished = assert_with(n, cookie, key, &refused.body["step_up"]).await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    assert_eq!(finished.body["op_hash"], refused.body["step_up"]["op_hash"]);
    finished.body["proof"].as_str().unwrap().to_string()
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

/// A paired phone with one passkey enrolled.
async fn paired_with_passkey() -> (Node, String, SoftPasskey) {
    let n = node();
    issue_token(&n).await;
    let cookie = pair(&n).await;
    let mut key = SoftPasskey::new(1, ORIGIN);
    let r = enroll(&n, &cookie, &mut key, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    (n, cookie, key)
}

#[tokio::test]
async fn pairing_opens_a_window_for_the_first_passkey_and_then_closes_it() {
    let n = node();
    issue_token(&n).await;
    let cookie = pair(&n).await;

    let listed = remote(&n, &cookie, "GET", "/api/passkeys", None, None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(listed.body["enrollment"]["open"], true, "{}", listed.body);
    assert_eq!(listed.body["relying_party"]["id"], "tracon.example.org");

    let mut key = SoftPasskey::new(1, ORIGIN);
    let r = enroll(&n, &cookie, &mut key, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["id"], key.id());
    assert_eq!(r.body["label"], "iPhone");

    let listed = remote(&n, &cookie, "GET", "/api/passkeys", None, None).await;
    let passkeys = listed.body["passkeys"].as_array().unwrap();
    assert_eq!(passkeys.len(), 1);
    assert_eq!(passkeys[0]["backup_eligible"], true);
    assert_eq!(passkeys[0]["transports"], r#"["internal","hybrid"]"#);
    assert!(
        passkeys[0].get("passkey").is_none(),
        "the record stays on the node"
    );
    // With one enrolled the window is over, for this session and any other.
    assert_eq!(listed.body["enrollment"]["open"], false);

    let mut second = SoftPasskey::new(2, ORIGIN);
    let r = enroll(&n, &cookie, &mut second, None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    let other = pair(&n).await;
    let r = enroll(&n, &other, &mut second, None).await;
    assert_eq!(
        r.kind(),
        "step_up_required",
        "a fresh pairing opens no window: {}",
        r.body
    );

    // Adding another is an admin action: confirmed with the first, it goes in.
    let body = json!({ "label": "iPhone" });
    let proof = confirm(
        &n,
        &cookie,
        &mut key,
        "POST",
        "/api/passkeys/register/options",
        Some(&body),
    )
    .await;
    let r = enroll(&n, &cookie, &mut second, Some(&proof)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(n.store.passkey_count().unwrap(), 2);
}

#[tokio::test]
async fn the_window_closes_fifteen_minutes_after_pairing() {
    let n = node();
    issue_token(&n).await;
    let now = now_ms();
    for (secret, paired_ms) in [
        ("old", Some(now - 16 * 60 * 1000)),
        ("never", None),
        ("fresh", Some(now - 14 * 60 * 1000)),
    ] {
        n.store
            .auth_session_insert(&AuthSessionRow {
                token_hash: auth::hash(secret),
                created_ms: now,
                last_seen_ms: now,
                expires_ms: now + 60_000,
                user_agent: None,
                paired_ms,
                delegated_from: None,
            })
            .unwrap();
    }
    let mut key = SoftPasskey::new(1, ORIGIN);
    for secret in ["old", "never"] {
        let cookie = format!("tracon_session={secret}");
        let r = enroll(&n, &cookie, &mut key, None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{secret}: {}", r.body);
        assert_eq!(r.body["error"]["reason"], "enrollment_closed", "{}", r.body);
    }
    let r = enroll(&n, "tracon_session=fresh", &mut key, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

/// Two paired clients inside the window race for the first passkey. Only one
/// is enrolled: the other must confirm with it.
#[tokio::test]
async fn the_window_admits_one_first_passkey_however_many_race_for_it() {
    let n = node();
    issue_token(&n).await;
    let a = pair(&n).await;
    let b = pair(&n).await;
    let body = json!({});
    let opts_a = remote(
        &n,
        &a,
        "POST",
        "/api/passkeys/register/options",
        Some(&body),
        None,
    )
    .await;
    let opts_b = remote(
        &n,
        &b,
        "POST",
        "/api/passkeys/register/options",
        Some(&body),
        None,
    )
    .await;
    assert_eq!(opts_a.status, StatusCode::OK);
    assert_eq!(opts_b.status, StatusCode::OK);
    let mut ka = SoftPasskey::new(1, ORIGIN);
    let mut kb = SoftPasskey::new(2, ORIGIN);
    let fin = |cookie: String, opts: Value, cred: Value| {
        let n = &n;
        async move {
            remote(
                n,
                &cookie,
                "POST",
                "/api/passkeys/register/finish",
                Some(&json!({ "challenge_id": opts["challenge_id"], "credential": cred })),
                None,
            )
            .await
        }
    };
    let ra = fin(
        a.clone(),
        opts_a.body.clone(),
        ka.create(&opts_a.body["options"]),
    )
    .await;
    let rb = fin(
        b.clone(),
        opts_b.body.clone(),
        kb.create(&opts_b.body["options"]),
    )
    .await;
    assert_eq!(ra.status, StatusCode::OK, "{}", ra.body);
    assert_eq!(rb.status, StatusCode::CONFLICT, "{}", rb.body);
    assert_eq!(n.store.passkey_count().unwrap(), 1);
}

#[tokio::test]
async fn an_admin_action_is_confirmed_once_for_exactly_that_request() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let body = grant();

    // Unconfirmed: refused, with a challenge, and nothing done.
    let refused = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.kind(), "step_up_required");
    let step_up = &refused.body["step_up"];
    assert_eq!(step_up["action"], "POST /api/authority/grants");
    assert_eq!(step_up["summary"], "Create an authority grant");
    assert_eq!(step_up["op_hash"].as_str().unwrap().len(), 64);
    assert_eq!(
        step_up["options"]["publicKey"]["userVerification"],
        "required"
    );
    assert_eq!(grants(&n).await, 0);

    // Confirmed: the retry goes through, once.
    let finished = assert_with(&n, &cookie, &mut key, step_up).await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    let proof = finished.body["proof"].as_str().unwrap().to_string();
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(grants(&n).await, 1);

    // Replayed: refused, and a fresh challenge offered instead.
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.kind(), "step_up_required");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already used"),
        "{}",
        r.body
    );
    assert_eq!(grants(&n).await, 1);

    // The challenge itself is single use too.
    let again = assert_with(&n, &cookie, &mut key, step_up).await;
    assert_eq!(again.status, StatusCode::GONE, "{}", again.body);

    // The counter the authenticator reported is kept.
    let listed = local(&n, "GET", "/api/passkeys", None).await;
    assert_eq!(listed.body["passkeys"][0]["sign_count"], 1);
    assert!(listed.body["passkeys"][0]["last_used_ms"].is_i64());
}

#[tokio::test]
async fn a_proof_for_one_operation_does_not_confirm_another() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let a = grant();
    let proof = confirm(
        &n,
        &cookie,
        &mut key,
        "POST",
        "/api/authority/grants",
        Some(&a),
    )
    .await;

    // Another body on the same route.
    let mut b = grant();
    b["verdict"] = json!("allow");
    b["session_id"] = json!("s1");
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&b),
        Some(&proof),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    // The same body sent with different spacing is different bytes.
    let r = send(
        &n,
        "POST",
        "/api/authority/grants",
        REMOTE,
        &[
            ("cookie", &cookie),
            ("origin", ORIGIN),
            (STEP_UP_HEADER, &proof),
            ("content-type", "application/json"),
        ],
        None,
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    // Another route entirely.
    let r = remote(&n, &cookie, "DELETE", "/api/auth/token", None, Some(&proof)).await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    // Another session holding the same proof.
    let other = pair(&n).await;
    let r = remote(
        &n,
        &other,
        "POST",
        "/api/authority/grants",
        Some(&a),
        Some(&proof),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    assert_eq!(grants(&n).await, 0);

    // None of that spent it: it still confirms what it was given for.
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&a),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(grants(&n).await, 1);
}

#[tokio::test]
async fn an_assertion_the_verifier_rejects_yields_no_proof() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let body = grant();
    let mut cases: Vec<(&str, SoftPasskey)> = Vec::new();
    // A page on another origin relaying the challenge.
    cases.push(("origin", SoftPasskey::new(1, "https://evil.example")));
    // The authenticator did not verify the user.
    let mut no_uv = SoftPasskey::new(1, ORIGIN);
    no_uv.user_verified = false;
    cases.push(("uv", no_uv));
    // A key the node never enrolled.
    cases.push(("unknown", SoftPasskey::new(9, ORIGIN)));
    for (why, mut wrong) in cases {
        let refused = remote(
            &n,
            &cookie,
            "POST",
            "/api/authority/grants",
            Some(&body),
            None,
        )
        .await;
        // It signs the node's challenge as asked; only the one property
        // differs.
        let r = assert_with(&n, &cookie, &mut wrong, &refused.body["step_up"]).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{why}: {}", r.body);
        assert!(r.body.get("proof").is_none(), "{why}");
        // And the challenge it burned cannot be answered properly after.
        let r = assert_with(&n, &cookie, &mut key, &refused.body["step_up"]).await;
        assert_eq!(r.status, StatusCode::GONE, "{why}: {}", r.body);
    }
    assert_eq!(grants(&n).await, 0);
}

#[tokio::test]
async fn a_bearer_token_never_confirms_an_admin_action_off_loopback() {
    let (n, _cookie, _key) = paired_with_passkey().await;
    let bearer = format!("Bearer {TOKEN}");
    let r = send(
        &n,
        "POST",
        "/api/authority/grants",
        REMOTE,
        &[("authorization", &bearer)],
        Some(&grant()),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.kind(), "step_up_unavailable");
    assert_eq!(r.body["error"]["reason"], "no_session");
    // Nor can a bearer client ask for a challenge or enroll.
    for path in [
        "/api/passkeys/assert/options",
        "/api/passkeys/register/options",
    ] {
        let r = send(
            &n,
            "POST",
            path,
            REMOTE,
            &[("authorization", &bearer)],
            Some(&json!({ "op_hash": "0".repeat(64) })),
        )
        .await;
        assert_eq!(r.kind(), "step_up_unavailable", "{path}: {}", r.body);
    }
    // The same token over loopback is the operator, as it always was.
    let r = send(
        &n,
        "POST",
        "/api/authority/grants",
        LOCAL,
        &[("authorization", &bearer)],
        Some(&grant()),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

#[tokio::test]
async fn at_the_node_no_passkey_is_asked_for() {
    // No public origin, no token, no passkey: the machine is the recovery
    // root and needs none of them.
    let n = node_with(None);
    let r = local(&n, "POST", "/api/authority/grants", Some(&grant())).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = local(&n, "DELETE", "/api/passkeys/nope", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
}

#[tokio::test]
async fn without_a_public_origin_remote_admin_fails_closed() {
    let n = node_with(None);
    issue_token(&n).await;
    let cookie = pair(&n).await;
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&grant()),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.body["error"]["reason"], "no_public_origin", "{}", r.body);
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/passkeys/register/options",
        Some(&json!({})),
        None,
    )
    .await;
    assert_eq!(r.body["error"]["reason"], "no_public_origin", "{}", r.body);
    let listed = remote(&n, &cookie, "GET", "/api/passkeys", None, None).await;
    assert!(listed.body["relying_party"].is_null());
    assert!(listed.body["unavailable"]
        .as_str()
        .unwrap()
        .contains("public_origin"));
    assert_eq!(grants(&n).await, 0);
}

#[tokio::test]
async fn with_no_passkey_enrolled_a_remote_admin_action_says_to_enroll() {
    let n = node();
    issue_token(&n).await;
    let cookie = pair(&n).await;
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&grant()),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.body["error"]["reason"], "no_passkey", "{}", r.body);
}

#[tokio::test]
async fn rotating_the_token_keeps_the_passkeys() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let r = local(
        &n,
        "POST",
        "/api/auth/token",
        Some(&json!({ "token_hash": auth::hash("trc1.rotated") })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(n.store.passkey_count().unwrap(), 1);
    // The old cookie is gone with the old token.
    let r = remote(&n, &cookie, "GET", "/api/passkeys", None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // A new pairing opens no window (a passkey exists), and the old passkey
    // still confirms from it.
    let r = send(
        &n,
        "POST",
        "/api/login",
        REMOTE,
        &[("origin", ORIGIN)],
        Some(&json!({ "token": "trc1.rotated" })),
    )
    .await;
    let cookie = r
        .cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let body = grant();
    let proof = confirm(
        &n,
        &cookie,
        &mut key,
        "POST",
        "/api/authority/grants",
        Some(&body),
    )
    .await;
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

#[tokio::test]
async fn removing_a_passkey_is_an_admin_action() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let path = format!("/api/passkeys/{}", key.id());
    let proof = confirm(&n, &cookie, &mut key, "DELETE", &path, None).await;
    let r = remote(&n, &cookie, "DELETE", &path, None, Some(&proof)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(n.store.passkey_count().unwrap(), 0);
}

/// Every remote-capable admin route refuses an unconfirmed request from a
/// signed-in browser off the machine, and the refusal is the step-up.
#[tokio::test]
async fn every_moved_admin_route_needs_a_passkey_off_loopback() {
    let (n, cookie, key) = paired_with_passkey().await;
    let member = "a".repeat(64);
    let routes: Vec<(&str, String, Option<Value>)> = vec![
        (
            "POST",
            "/api/auth/token".into(),
            Some(json!({ "token_hash": "0".repeat(64) })),
        ),
        ("DELETE", "/api/auth/token".into(), None),
        ("POST", "/api/authority/grants".into(), Some(grant())),
        ("DELETE", "/api/authority/grants/g1".into(), None),
        (
            "PUT",
            "/api/channels/personal/bindings".into(),
            Some(json!({ "model": "x" })),
        ),
        (
            "PUT",
            "/api/credentials/forge/github".into(),
            Some(json!({ "token": "t", "channels": ["personal"] })),
        ),
        ("DELETE", "/api/credentials/forge/github".into(), None),
        (
            "POST",
            "/api/credentials/jira/share".into(),
            Some(json!({ "to": "n2" })),
        ),
        (
            "POST",
            "/api/credentials/import".into(),
            Some(json!({ "toml": "[credentials.x]\nkind='env'\n" })),
        ),
        ("DELETE", "/api/credentials/jira".into(), None),
        (
            "POST",
            "/api/providers/anthropic/connect".into(),
            Some(json!({})),
        ),
        (
            "PUT",
            "/api/providers/anthropic/key".into(),
            Some(json!({ "key": "k", "channels": [] })),
        ),
        (
            "PUT",
            "/api/providers/anthropic/channels".into(),
            Some(json!({ "channels": [] })),
        ),
        ("POST", "/api/providers/anthropic/disconnect".into(), None),
        (
            "POST",
            "/api/nodes/n1/providers/anthropic/connect".into(),
            Some(json!({})),
        ),
        (
            "POST",
            "/api/nodes/n1/providers/anthropic/disconnect".into(),
            None,
        ),
        ("POST", "/api/boundary/setup".into(), Some(json!({}))),
        (
            "POST",
            "/api/admin/mesh/invitations".into(),
            Some(json!({ "channels": ["personal"] })),
        ),
        (
            "POST",
            "/api/admin/mesh/invitations/ABCD-EFGH/admit".into(),
            Some(json!({ "confirm": true })),
        ),
        (
            "DELETE",
            format!("/api/admin/mesh/members/{member}"),
            Some(json!({ "confirm": true })),
        ),
        (
            "POST",
            "/api/admin/mesh/hub-share".into(),
            Some(json!({ "channels": ["personal"], "confirm": true })),
        ),
        ("DELETE", format!("/api/passkeys/{}", key.id()), None),
        (
            "POST",
            "/api/passkeys/register/options".into(),
            Some(json!({})),
        ),
    ];
    for (method, path, body) in &routes {
        let r = remote(&n, &cookie, method, path, body.as_ref(), None).await;
        assert_eq!(
            r.status,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            r.body
        );
        assert_eq!(r.kind(), "step_up_required", "{method} {path}: {}", r.body);
        assert_eq!(
            r.body["step_up"]["action"],
            format!("{method} {}", path.split('?').next().unwrap()),
        );
    }
    // Nothing any of them would have done happened.
    assert_eq!(grants(&n).await, 0);
    assert_eq!(n.store.passkey_count().unwrap(), 1);
    let r = remote(&n, &cookie, "GET", "/api/passkeys", None, None).await;
    assert_eq!(r.status, StatusCode::OK, "the cookie survived: {}", r.body);
}

#[tokio::test]
async fn a_fresh_challenge_can_be_asked_for_by_operation() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let body = grant();
    let refused = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        None,
    )
    .await;
    let op = refused.body["step_up"]["op_hash"].clone();
    let fresh = remote(
        &n,
        &cookie,
        "POST",
        "/api/passkeys/assert/options",
        Some(&json!({ "op_hash": op })),
        None,
    )
    .await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    assert_ne!(
        fresh.body["challenge_id"],
        refused.body["step_up"]["challenge_id"]
    );
    let finished = assert_with(&n, &cookie, &mut key, &fresh.body).await;
    let proof = finished.body["proof"].as_str().unwrap();
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        Some(proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // Another session cannot answer a challenge opened for this one.
    let refused = remote(
        &n,
        &cookie,
        "POST",
        "/api/authority/grants",
        Some(&body),
        None,
    )
    .await;
    let other = pair(&n).await;
    let r = assert_with(&n, &other, &mut key, &refused.body["step_up"]).await;
    assert_eq!(r.status, StatusCode::GONE, "{}", r.body);
}

/// What a client sends to finish a ceremony is untrusted input to the
/// verifier. Mangled in any of the ways a broken or hostile client could,
/// it is refused, never a server error, and never a proof.
#[tokio::test]
async fn a_mangled_answer_is_refused_without_a_server_error() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let body = grant();
    type Mangle = fn(&mut Value);
    let mangle: Vec<(&str, Mangle)> = vec![
        ("truncated authenticator data", |c| {
            c["response"]["authenticatorData"] = json!("AAAA");
        }),
        ("signature flipped", |c| {
            let s = c["response"]["signature"].as_str().unwrap();
            let mut chars: Vec<char> = s.chars().collect();
            chars[10] = if chars[10] == 'A' { 'B' } else { 'A' };
            c["response"]["signature"] = json!(chars.into_iter().collect::<String>());
        }),
        ("client data not JSON", |c| {
            c["response"]["clientDataJSON"] = json!("bm90IGpzb24");
        }),
        ("not base64", |c| {
            c["response"]["signature"] = json!("!!!");
        }),
        ("raw id of another credential", |c| {
            c["rawId"] = json!("AAAAAAAAAAAAAAAAAAAAAA");
            c["id"] = json!("AAAAAAAAAAAAAAAAAAAAAA");
        }),
        ("no response at all", |c| {
            c.as_object_mut().unwrap().remove("response");
        }),
    ];
    for (why, f) in mangle {
        let refused = remote(
            &n,
            &cookie,
            "POST",
            "/api/authority/grants",
            Some(&body),
            None,
        )
        .await;
        let step_up = &refused.body["step_up"];
        let mut credential = key.get(&step_up["options"]);
        f(&mut credential);
        let r = remote(
            &n,
            &cookie,
            "POST",
            "/api/passkeys/assert/finish",
            Some(&json!({ "challenge_id": step_up["challenge_id"], "credential": credential })),
            None,
        )
        .await;
        assert!(r.status.is_client_error(), "{why}: {} {}", r.status, r.body);
        assert!(r.body.get("proof").is_none(), "{why}");
    }
    // Registration answers too.
    let body = json!({});
    let mut second = SoftPasskey::new(2, ORIGIN);
    let proof = confirm(
        &n,
        &cookie,
        &mut key,
        "POST",
        "/api/passkeys/register/options",
        Some(&body),
    )
    .await;
    let opts = remote(
        &n,
        &cookie,
        "POST",
        "/api/passkeys/register/options",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(opts.status, StatusCode::OK, "{}", opts.body);
    let mut credential = second.create(&opts.body["options"]);
    credential["response"]["attestationObject"] = json!("oWNmbXRkbm9uZQ");
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/passkeys/register/finish",
        Some(&json!({ "challenge_id": opts.body["challenge_id"], "credential": credential })),
        None,
    )
    .await;
    assert!(r.status.is_client_error(), "{} {}", r.status, r.body);
    assert_eq!(n.store.passkey_count().unwrap(), 1);
    assert_eq!(grants(&n).await, 0);
}

/// What is confirmed is the bytes. A body that is not JSON would be read by an
/// optional extractor as no body at all (`POST /api/providers/{name}/connect`
/// takes `Option<Json<_>>`), so the request run would not be the one
/// confirmed: it is refused before any confirmation is asked for or spent.
#[tokio::test]
async fn an_admin_body_that_is_not_json_is_refused() {
    let (n, cookie, mut key) = paired_with_passkey().await;
    let body = json!({ "channels": ["personal"] });
    let proof = confirm(
        &n,
        &cookie,
        &mut key,
        "POST",
        "/api/providers/anthropic/connect",
        Some(&body),
    )
    .await;
    for ct in ["text/plain", "application/x-www-form-urlencoded"] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/providers/anthropic/connect")
            .header("host", HOST)
            .header("cookie", &cookie)
            .header("origin", ORIGIN)
            .header(STEP_UP_HEADER, &proof)
            .header("content-type", ct)
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(REMOTE.parse::<SocketAddr>().unwrap()));
        let res = n.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{ct}");
    }
    // An empty body is the optional extractor's default, confirmed as such.
    let empty = confirm(
        &n,
        &cookie,
        &mut key,
        "POST",
        "/api/providers/anthropic/connect",
        None,
    )
    .await;
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/providers/anthropic/connect",
        None,
        Some(&empty),
    )
    .await;
    assert_ne!(r.kind(), "step_up_required", "{}", r.body);
    assert_ne!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{}", r.body);
    // The proof for the JSON body was not spent by the refusals above.
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/providers/anthropic/connect",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_ne!(r.kind(), "step_up_required", "{}", r.body);
}
