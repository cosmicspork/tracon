//! `node.toml` from off the node's machine: `GET` and `PATCH /api/config`,
//! and every route that used to be the machine's alone and is now an admin
//! action, confirmed with a passkey for exactly that request.
//!
//! The browser and its authenticator are [`SoftPasskey`], as in
//! `passkeys.rs`. "Remote" is a request from a non-loopback peer addressed to
//! the public origin, as one arrives through an ingress. The path guard is
//! left on (`state::isolate_dirs`), so a planted program is refused here as
//! it would be on a node.

#[path = "support/mod.rs"]
mod support;
use support::{soft_passkey::SoftPasskey, state};

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
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
    store::Store,
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

fn node() -> Node {
    // The guard stays on: these tests are about what an edit may name.
    state::isolate_dirs();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let mut cfg = Config::default();
    cfg.auth.public_origin = Some(ORIGIN.to_string());
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

struct Reply {
    status: StatusCode,
    cookies: Vec<String>,
    body: Value,
    text: String,
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
    let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
        .await
        .unwrap();
    Reply {
        status,
        cookies,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
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

/// A paired phone with one passkey enrolled.
async fn paired() -> (Node, String, SoftPasskey) {
    let n = node();
    let r = local(
        &n,
        "POST",
        "/api/auth/token",
        Some(&json!({ "token_hash": auth::hash(TOKEN) })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = send(
        &n,
        "POST",
        "/api/login",
        REMOTE,
        &[("origin", ORIGIN)],
        Some(&json!({ "token": TOKEN })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let cookie = r
        .cookies
        .iter()
        .find(|c| c.starts_with("tracon_session="))
        .expect("a session cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let mut key = SoftPasskey::new(1, ORIGIN);
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/passkeys/register/options",
        Some(&json!({ "label": "iPhone" })),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let credential = key.create(&r.body["options"]);
    let r = remote(
        &n,
        &cookie,
        "POST",
        "/api/passkeys/register/finish",
        Some(&json!({ "challenge_id": r.body["challenge_id"], "credential": credential })),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    (n, cookie, key)
}

/// Refused for want of a passkey, then confirmed: the refusal, and the proof
/// for exactly that request.
async fn confirm(
    n: &Node,
    cookie: &str,
    key: &mut SoftPasskey,
    method: &str,
    uri: &str,
    body: Option<&Value>,
) -> (Reply, String) {
    let refused = remote(n, cookie, method, uri, body, None).await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "{method} {uri}: {}",
        refused.body
    );
    assert_eq!(
        refused.kind(),
        "step_up_required",
        "{method} {uri}: {}",
        refused.body
    );
    let step_up = &refused.body["step_up"];
    let credential = key.get(&step_up["options"]);
    let finished = remote(
        n,
        cookie,
        "POST",
        "/api/passkeys/assert/finish",
        Some(&json!({ "challenge_id": step_up["challenge_id"], "credential": credential })),
        None,
    )
    .await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    let proof = finished.body["proof"].as_str().unwrap().to_string();
    (refused, proof)
}

async fn hash(n: &Node) -> String {
    let r = local(n, "GET", "/api/config", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body["hash"].as_str().unwrap().to_string()
}

fn file() -> String {
    std::fs::read_to_string(Config::config_path()).unwrap_or_default()
}

fn write_file(text: &str) {
    std::fs::write(Config::config_path(), text).unwrap();
}

fn set(key: &str, value: Value) -> Value {
    json!({ "op": "set", "key": key, "value": value })
}

fn patch(expected: &str, ops: Vec<Value>) -> Value {
    json!({ "expected_hash": expected, "ops": ops })
}

const HAND_WRITTEN: &str = r#"# Written by hand.
node_name = "kept"   # what the mesh calls it

[review]
max_files = 40       # small on purpose

# A section this node no longer reads, holding a credential.
[qa]
browser_token = "sk-live-unknown-1234"
# api_key = "sk-commented-5678"
"#;

/// The whole file comes back with its hash and its schema; a value the
/// schema calls secret, and the value of a key the node does not read,
/// never does, nor does a comment, which can hold anything.
#[tokio::test]
async fn the_file_is_read_whole_with_nothing_secret_in_it() {
    let _lock = state::config_lock().await;
    let (n, cookie, _key) = paired().await;
    write_file(HAND_WRITTEN);
    for r in [
        local(&n, "GET", "/api/config", None).await,
        // Reading is not an admin action: the cookie is enough.
        remote(&n, &cookie, "GET", "/api/config", None, None).await,
    ] {
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert!(!r.text.contains("sk-live-unknown-1234"), "{}", r.text);
        assert!(!r.text.contains("sk-commented-5678"), "{}", r.text);
        let v = &r.body;
        assert_eq!(
            v["hash"],
            json!(tracon::config::edit::hash(HAND_WRITTEN.as_bytes()))
        );
        assert_eq!(v["document"]["node_name"], "kept");
        assert_eq!(v["document"]["review"]["max_files"], 40);
        assert_eq!(v["document"]["qa"]["browser_token"], v["redacted"]);
        assert_eq!(v["effective"]["review"]["max_files"], 40);
        // Every default filled in.
        assert!(v["effective"]["publish"]["gh"].is_string());
        assert!(v["toml"].as_str().unwrap().contains("max_files = 40"));
        let keys = v["keys"].as_array().unwrap();
        let spec = |name: &str| {
            keys.iter()
                .find(|k| k["key"] == name)
                .unwrap_or_else(|| panic!("{name} in the schema"))
                .clone()
        };
        assert_eq!(spec("publish.gh")["exec_path"], true);
        assert_eq!(spec("publish.gh")["path"], "executable");
        assert_eq!(spec("repo.egress")["apply"], "live");
        assert_eq!(spec("review.max_files")["apply"], "restart");
        let mount = spec("runtime.kubernetes.state_mount");
        assert_eq!(mount["editable"], false);
        assert!(mount["why_not"].as_str().unwrap().contains("on the host"));
        assert_eq!(spec("publish.gh")["editable"], true);
    }
}

/// At the node an edit needs no passkey. It changes the keys it names, keeps
/// the operator's comments, is audited, and answers with the new hash.
#[tokio::test]
async fn an_edit_at_the_node_is_written_in_place_and_audited() {
    let _lock = state::config_lock().await;
    let n = node();
    write_file(HAND_WRITTEN);
    let before = hash(&n).await;
    let r = local(
        &n,
        "PATCH",
        "/api/config",
        Some(&patch(
            &before,
            vec![
                set("review.max_files", json!(41)),
                set("session.budget_tokens", json!(5_000_000)),
                json!({ "op": "remove", "key": "qa" }),
            ],
        )),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let mut changed: Vec<String> = serde_json::from_value(r.body["changed"].clone()).unwrap();
    changed.sort();
    assert_eq!(
        changed,
        [
            "qa.browser_token",
            "review.max_files",
            "session.budget_tokens"
        ]
    );
    assert_eq!(r.body["restart_required"], true);
    assert_eq!(r.body["old_hash"], json!(before));
    let text = file();
    assert_eq!(
        r.body["hash"],
        json!(tracon::config::edit::hash(text.as_bytes()))
    );
    assert!(
        text.contains("max_files = 41       # small on purpose"),
        "{text}"
    );
    assert!(text.contains("# Written by hand."), "{text}");
    assert!(!text.contains("sk-live-unknown-1234"), "{text}");
    let rows = n.store.config_audit_recent(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor, "Settings (at the node)");
    // The removed unknown key's value stays out of the log too.
    assert!(!rows[0].changes.to_string().contains("sk-live"), "{rows:?}");

    // The same operations again change nothing, and write nothing.
    let now = hash(&n).await;
    let r = local(
        &n,
        "PATCH",
        "/api/config",
        Some(&patch(&now, vec![set("review.max_files", json!(41))])),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["changed"], json!([]));
    assert_eq!(n.store.config_audit_recent(10).unwrap().len(), 1);
}

/// An edit is decided against a version of the file: a stale one is a
/// conflict carrying the version now, and one naming none is refused.
#[tokio::test]
async fn an_edit_against_a_stale_version_is_a_conflict() {
    let _lock = state::config_lock().await;
    let n = node();
    write_file(HAND_WRITTEN);
    let read = hash(&n).await;
    write_file(&format!("{HAND_WRITTEN}\n[session]\nbudget_tokens = 9\n"));
    let r = local(
        &n,
        "PATCH",
        "/api/config",
        Some(&patch(&read, vec![set("review.max_files", json!(1))])),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert_eq!(r.kind(), "conflict");
    let now = tracon::config::edit::hash(file().as_bytes());
    assert_eq!(r.body["hash"], json!(now));
    assert!(file().contains("max_files = 40"));

    let r = local(
        &n,
        "PATCH",
        "/api/config",
        Some(&json!({ "ops": [set("review.max_files", json!(1))] })),
    )
    .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_REQUIRED, "{}", r.body);

    // `If-Match` names the version as well as the body does.
    let body = json!({ "ops": [set("review.max_files", json!(2))] });
    let quoted = format!("\"{now}\"");
    let r = send(
        &n,
        "PATCH",
        "/api/config",
        LOCAL,
        &[("if-match", &quoted)],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(file().contains("max_files = 2"));
}

/// What would not load, a key the node does not read, a `null`, a key only
/// the host may change and a program the path guard refuses are each
/// refused with the key and the reason, and nothing is written.
#[tokio::test]
async fn an_edit_that_would_not_load_or_that_the_guard_refuses_is_refused_by_key() {
    let _lock = state::config_lock().await;
    let n = node();
    write_file(HAND_WRITTEN);
    let h = hash(&n).await;
    let planted = Config::state_dir().join("workspaces/planted/gh");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cases: Vec<(Value, &str, &str)> = vec![
        (set("harness.id", json!("nonesuch")), "", "harness"),
        (
            set("publish.ghh", json!("gh")),
            "publish.ghh",
            "not a node.toml key",
        ),
        (
            set("publish", json!({ "gh": "gh", "typo": 1 })),
            "publish.typo",
            "not a node.toml key",
        ),
        (
            set("review.max_files", Value::Null),
            "ops[0]",
            "remove the key",
        ),
        (
            set("runtime.kubernetes.state_mount", json!("/elsewhere")),
            "runtime.kubernetes.state_mount",
            "on the host",
        ),
        (
            set("publish.gh", json!(planted.to_string_lossy())),
            "publish.gh",
            "which an agent can write",
        ),
        (set("repo[3]", json!({ "path": "/x" })), "", "repo"),
    ];
    for (op, field, why) in cases {
        let r = local(
            &n,
            "PATCH",
            "/api/config",
            Some(&patch(&h, vec![op.clone()])),
        )
        .await;
        assert_eq!(
            r.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{op}: {}",
            r.body
        );
        assert!(r.text.contains(why), "{op}: {}", r.body);
        if !field.is_empty() {
            assert_eq!(
                r.body["error"]["fields"][0]["field"], field,
                "{op}: {}",
                r.body
            );
        }
        assert_eq!(file(), HAND_WRITTEN, "{op}");
    }
    assert!(n.store.config_audit_recent(10).unwrap().is_empty());

    // A body that is not JSON is not an admin request at all.
    let r = n
        .app
        .clone()
        .oneshot({
            let mut req = Request::builder()
                .method("PATCH")
                .uri("/api/config")
                .header("host", "127.0.0.1:7420")
                .header("content-type", "text/plain")
                .body(Body::from("expected_hash=x"))
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(LOCAL.parse::<SocketAddr>().unwrap()));
            req
        })
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

/// Off the machine an edit is confirmed with a passkey, once, for exactly
/// that request; the prompt lists the keys it changes, worked out by the
/// node. Without the confirmation nothing is written.
#[tokio::test]
async fn a_remote_edit_is_confirmed_with_a_passkey_that_names_its_keys() {
    let _lock = state::config_lock().await;
    let (n, cookie, mut key) = paired().await;
    write_file(HAND_WRITTEN);
    let h = hash(&n).await;
    let body = patch(
        &h,
        vec![
            set("session.budget_tokens", json!(7)),
            set("publish.forge_timeout_secs", json!(30)),
        ],
    );
    let (refused, proof) =
        confirm(&n, &cookie, &mut key, "PATCH", "/api/config", Some(&body)).await;
    assert_eq!(
        refused.body["step_up"]["summary"],
        "Change node.toml: publish.forge_timeout_secs, session.budget_tokens"
    );
    assert_eq!(refused.body["step_up"]["action"], "PATCH /api/config");
    assert_eq!(file(), HAND_WRITTEN, "refused means unwritten");

    // The proof confirms this body and no other.
    let other = patch(&h, vec![set("session.budget_tokens", json!(8))]);
    let r = remote(
        &n,
        &cookie,
        "PATCH",
        "/api/config",
        Some(&other),
        Some(&proof),
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    assert_eq!(file(), HAND_WRITTEN);

    let r = remote(
        &n,
        &cookie,
        "PATCH",
        "/api/config",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(file().contains("budget_tokens = 7"));
    let rows = n.store.config_audit_recent(10).unwrap();
    assert_eq!(rows[0].actor, "Settings (confirmed with a passkey)");

    // Spent: the same request again is asked for a fresh confirmation (and
    // would now be a conflict anyway).
    let r = remote(
        &n,
        &cookie,
        "PATCH",
        "/api/config",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_ne!(r.status, StatusCode::OK, "{}", r.body);

    // A long edit's prompt is capped, and says how many more keys it holds.
    let h = hash(&n).await;
    let many: Vec<Value> = (0..40)
        .map(|i| {
            set(
                &format!("github.dispatch.\"owner/repository-{i:02}\""),
                json!(["ci.yml"]),
            )
        })
        .collect();
    let r = remote(
        &n,
        &cookie,
        "PATCH",
        "/api/config",
        Some(&patch(&h, many)),
        None,
    )
    .await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    let summary = r.body["step_up"]["summary"].as_str().unwrap();
    assert!(summary.chars().count() <= 160, "{summary}");
    assert!(
        summary.starts_with("Change node.toml: github.dispatch."),
        "{summary}"
    );
    assert!(summary.ends_with("more"), "{summary}");
}

/// The path guard runs when the edit is written, after it is confirmed:
/// what it knows about the host's files is not for a caller who has not.
#[tokio::test]
async fn a_remote_edit_meets_the_path_guard_after_its_confirmation() {
    let _lock = state::config_lock().await;
    let (n, cookie, mut key) = paired().await;
    write_file(HAND_WRITTEN);
    let planted = Config::state_dir().join("workspaces/planted-remote/gh");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
    let body = patch(
        &hash(&n).await,
        vec![set("publish.gh", json!(planted.to_string_lossy()))],
    );
    let (_, proof) = confirm(&n, &cookie, &mut key, "PATCH", "/api/config", Some(&body)).await;
    let r = remote(
        &n,
        &cookie,
        "PATCH",
        "/api/config",
        Some(&body),
        Some(&proof),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert_eq!(r.kind(), "refused");
    assert_eq!(r.body["error"]["fields"][0]["field"], "publish.gh");
    assert_eq!(file(), HAND_WRITTEN);
}

/// A bearer token is the operator at the node and nobody elsewhere: off the
/// machine it can read the file but confirm no edit of it.
#[tokio::test]
async fn a_bearer_token_edits_nothing_off_the_machine() {
    let _lock = state::config_lock().await;
    let (n, _cookie, _key) = paired().await;
    write_file(HAND_WRITTEN);
    let bearer = format!("Bearer {TOKEN}");
    let r = send(
        &n,
        "GET",
        "/api/config",
        REMOTE,
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let h = r.body["hash"].as_str().unwrap().to_string();
    let body = patch(&h, vec![set("review.max_files", json!(3))]);
    let r = send(
        &n,
        "PATCH",
        "/api/config",
        REMOTE,
        &[("authorization", &bearer)],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.kind(), "step_up_unavailable");
    assert_eq!(r.body["error"]["reason"], "no_session");
    assert_eq!(file(), HAND_WRITTEN);
    // At the node the same token is the operator.
    let r = send(
        &n,
        "PATCH",
        "/api/config",
        LOCAL,
        &[("authorization", &bearer)],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

/// A `[[repo]]` edit made here is live, as the repository table's own
/// route makes it: the running node resolves against it at once.
#[tokio::test]
async fn a_repository_entry_added_by_an_edit_applies_without_a_restart() {
    let _lock = state::config_lock().await;
    let n = node();
    write_file(HAND_WRITTEN);
    let r = local(
        &n,
        "PATCH",
        "/api/config",
        Some(&patch(
            &hash(&n).await,
            vec![json!({
                "op": "insert",
                "array": "repo",
                "entry": { "path": "/src/live", "egress": ["crates"] },
            })],
        )),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["restart_required"], false, "{}", r.body);
    let r = local(&n, "GET", "/api/repos/environments", None).await;
    let entries = r.body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{}", r.body);
    assert_eq!(entries[0]["entry"]["path"], "/src/live");
    assert_eq!(r.body["hash"], json!(hash(&n).await));
}

/// Every route that wrote `node.toml` (or what a harness runs) at the node
/// only is now an admin action: off the machine it is refused until a
/// passkey confirms exactly that request, and then it goes ahead.
#[tokio::test]
async fn every_former_loopback_writer_goes_ahead_once_confirmed() {
    let _lock = state::config_lock().await;
    let (n, cookie, mut key) = paired().await;
    write_file(HAND_WRITTEN);

    // Each body is made when its turn comes: the file's hash moves with
    // every write before it.
    type Body = Box<dyn Fn(&str) -> Option<Value>>;
    let routes: Vec<(&str, &str, Body, StatusCode, &str)> = vec![
        (
            "PUT",
            "/api/config",
            Box::new(|_| Some(json!({ "review": { "max_files": 77 } }))),
            StatusCode::OK,
            "Change node.toml: review.max_files",
        ),
        (
            "PATCH",
            "/api/config",
            Box::new(|h| Some(patch(h, vec![set("review.max_files", json!(78))]))),
            StatusCode::OK,
            "Change node.toml: review.max_files",
        ),
        (
            "POST",
            "/api/providers",
            Box::new(|_| {
                Some(json!({
                    "name": "openrouter",
                    "upstream": "https://openrouter.ai/api/v1",
                    "shape": "openai",
                    "credential": "openrouter",
                }))
            }),
            StatusCode::OK,
            "Add the provider openrouter, sending the openrouter credential to openrouter.ai",
        ),
        (
            "PUT",
            "/api/manifest/text",
            Box::new(|_| {
                Some(json!({ "kind": "instruction", "name": "style", "body": "Be brief." }))
            }),
            StatusCode::OK,
            "Set the instruction style on personal",
        ),
        (
            "DELETE",
            "/api/manifest/instruction/style",
            Box::new(|_| None),
            StatusCode::OK,
            "Remove the instruction style from a launch manifest",
        ),
        (
            "PUT",
            "/api/repos/environments",
            Box::new(|h| {
                Some(json!({
                    "entries": [{ "path": "/src/app", "egress": ["crates"] }],
                    "expected_hash": h,
                }))
            }),
            StatusCode::OK,
            "Change node.toml: repo",
        ),
        (
            "POST",
            "/api/repos/environments/build",
            Box::new(|_| Some(json!({ "repo": "/definitely/not/here" }))),
            // Through the gate, and refused by what it found.
            StatusCode::UNPROCESSABLE_ENTITY,
            "Build the image for the repository /definitely/not/here",
        ),
        (
            "POST",
            "/api/mesh/init",
            Box::new(|_| Some(json!({ "hub_url": "https://hub.example.com" }))),
            StatusCode::OK,
            "Start a mesh with the hub at https://hub.example.com",
        ),
        (
            "POST",
            "/api/mesh/unpair",
            Box::new(|_| None),
            StatusCode::OK,
            "Leave the mesh (forget the hub)",
        ),
        (
            "POST",
            "/api/mesh/enroll",
            Box::new(|_| Some(json!({ "invitation": "https://127.0.0.1:9/#enroll=7KQ4M2XA" }))),
            StatusCode::OK,
            "Join the mesh at https://127.0.0.1:9",
        ),
    ];
    for (method, path, body, ok, summary) in &routes {
        let h = hash(&n).await;
        let body = body(&h);
        let (refused, proof) = confirm(&n, &cookie, &mut key, method, path, body.as_ref()).await;
        assert_eq!(
            refused.body["step_up"]["summary"], *summary,
            "{method} {path}: {}",
            refused.body
        );
        let r = remote(&n, &cookie, method, path, body.as_ref(), Some(&proof)).await;
        assert_eq!(r.status, *ok, "{method} {path}: {}", r.body);
    }
    let text = file();
    assert!(text.contains("max_files = 78"), "{text}");
    assert!(text.contains("[providers.openrouter]"), "{text}");
    assert!(text.contains("/src/app"), "{text}");
    assert!(!text.contains("hub_url"), "unpaired: {text}");
    // The mesh channel was minted by the confirmed init.
    assert!(n
        .store
        .channel_get(proto::frame::MESH_CHANNEL)
        .unwrap()
        .is_some());
}
