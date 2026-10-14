//! A peer's `node.toml`, read and edited from an admin node over a real hub.
//!
//! A (seed 1) is the node whose interface the operator uses; B (seed 2) is
//! the node being administered. Both run in this process with their mesh
//! loops, so every command crosses the hub sealed, as between machines. The
//! one `node.toml` on disk (`Config::config_path()`) is B's: A only relays.
//! The operator's phone is [`SoftPasskey`], enrolled on A.

#[path = "support/mod.rs"]
mod support;
use support::{soft_passkey::SoftPasskey, state};

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use proto::admin::{AdminStatement, STATEMENT_TTL_MS};
use proto::envelope::DataKey;
use proto::frame::{Command, MESH_CHANNEL};
use proto::keyring::Keyring;
use proto::keys::Identity;
use serde_json::{json, Value};
use support::fake::FakeAdapter;
use support::mesh::{identity, wait_for};
use tower::ServiceExt;
use tracon::config::Config;
use tracon::http::{
    api::AppState,
    auth::{self, AuthState},
    passkey::STEP_UP_HEADER,
};
use tracon::mesh::client::MeshClient;
use tracon::mesh::forward::{CommandError, CommandExecutor};
use tracon::session::Manager;
use tracon::store::Store;
use tracon::stream::Bus;

const ORIGIN: &str = "https://alpha.example.org";
const HOST: &str = "alpha.example.org";
const REMOTE: &str = "203.0.113.7:5000";
const LOCAL: &str = "127.0.0.1:5000";
const TOKEN: &str = "trc1.secret";

struct Node {
    id: Identity,
    app: axum::Router,
    store: Arc<Store>,
    client: Arc<MeshClient>,
    cfg: Arc<Config>,
}

fn node(
    seed: u8,
    name: &str,
    hub: &str,
    rings: &[(&str, Keyring)],
    admins: &[String],
    executor: Option<Arc<dyn CommandExecutor>>,
) -> Node {
    let id = identity(seed);
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&{
            let mut r = support::rows::node_row(&id.node_id(), name);
            r.x25519_pub = Some(id.x25519_hex());
            r
        })
        .unwrap();
    for (c, ring) in rings {
        store.channel_put(c, &ring.to_bytes(), "{}").unwrap();
        store.node_channel_add(&id.node_id(), c).unwrap();
    }
    let bus = Bus::new();
    let mut cfg = Config {
        node_name: name.into(),
        ..Default::default()
    };
    cfg.mesh.hub_url = Some(hub.to_string());
    cfg.mesh.poll_secs = 1;
    cfg.mesh.heartbeat_secs = 5;
    cfg.mesh.command_timeout_secs = 3;
    cfg.mesh.admin_peers = admins.to_vec();
    cfg.auth.public_origin = Some(ORIGIN.into());
    let cfg = Arc::new(cfg);
    let tools = Arc::new(tracon::mcp::Tools {
        broker: tracon::broker::Broker::default().shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        bus.clone(),
        cfg.clone(),
        id.node_id(),
        tools.clone(),
        Default::default(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let client = MeshClient::new(
        identity(seed),
        hub,
        store.clone(),
        bus.clone(),
        cfg.clone(),
        Default::default(),
    );
    bus.with_tap(client.spawn());
    manager.set_mesh(client.clone());
    let state = AppState {
        manager,
        cfg: cfg.clone(),
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: id.node_id(),
        tools,
        mesh: Some(client.clone()),
        auth: Arc::new(AuthState::load(&store, "127.0.0.1".into())),
        enroll: Default::default(),
    };
    client.set_executor(executor.unwrap_or_else(|| Arc::new(state.clone())));
    let app = tracon::http::router(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::guard));
    Node {
        id,
        app,
        store,
        client,
        cfg,
    }
}

/// A and B on one hub, both in `@mesh` and `personal`; B names `b_admins`
/// in its `[mesh] admin_peers`, and B's file says the same.
async fn pair(b_admins: &[String]) -> (Node, Node) {
    pair_with(b_admins, None).await
}

/// As `pair`, with what answers B's commands in place of B's own node.
async fn pair_with(
    b_admins: &[String],
    b_executor: Option<Arc<dyn CommandExecutor>>,
) -> (Node, Node) {
    let (a_id, b_id) = (identity(1), identity(2));
    let both: &[&str] = &[MESH_CHANNEL, "personal"];
    // C is admitted to the hub and never runs: a peer that never answers.
    let ghost = identity(3);
    let hub =
        support::mesh::start_hub(&[(&a_id, both), (&b_id, both), (&ghost, &[MESH_CHANNEL])]).await;
    let mesh = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let personal = Keyring::genesis(&a_id.x25519_public(), &DataKey::generate());
    let b_rings = [
        (
            MESH_CHANNEL,
            mesh.wrap_for(&a_id, &b_id.x25519_public()).unwrap(),
        ),
        (
            "personal",
            personal.wrap_for(&a_id, &b_id.x25519_public()).unwrap(),
        ),
    ];
    write_file(&b_file(b_admins));
    let a = node(
        1,
        "alpha",
        &hub,
        &[(MESH_CHANNEL, mesh), ("personal", personal)],
        &[],
        None,
    );
    let b = node(2, "beta", &hub, &b_rings, b_admins, b_executor);
    a.client.hello().await.unwrap();
    b.client.hello().await.unwrap();
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    wait_for("A to see B", || {
        a.store
            .get_node(&bi)
            .ok()
            .flatten()
            .is_some_and(|n| n.reachable == 1)
    })
    .await;
    wait_for("B to see A in the mesh", || {
        b.store
            .nodes_in_channel(MESH_CHANNEL)
            .is_ok_and(|m| m.contains(&ai))
    })
    .await;
    (a, b)
}

fn b_file(admins: &[String]) -> String {
    format!(
        r#"# B's own file, written by hand.
node_name = "beta"   # what the mesh calls it

[review]
max_files = 40       # small on purpose

[mesh]
admin_peers = [{}]

# A section this node no longer reads, holding a credential.
[qa]
browser_token = "sk-live-unknown-1234"
"#,
        admins
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn file() -> String {
    std::fs::read_to_string(Config::config_path()).unwrap_or_default()
}

fn write_file(text: &str) {
    std::fs::write(Config::config_path(), text).unwrap();
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
    let mut b = Request::builder().method(method).uri(uri).header(
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
        .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
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
    }
}

async fn local(n: &Node, method: &str, uri: &str, body: Option<&Value>) -> Reply {
    send(n, method, uri, LOCAL, &[], body).await
}

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

/// The operator's phone paired with A, one passkey enrolled.
async fn pair_phone(a: &Node) -> (String, SoftPasskey) {
    let r = local(
        a,
        "POST",
        "/api/auth/token",
        Some(&json!({ "token_hash": auth::hash(TOKEN) })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = send(
        a,
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
        a,
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
        a,
        &cookie,
        "POST",
        "/api/passkeys/register/finish",
        Some(&json!({ "challenge_id": r.body["challenge_id"], "credential": credential })),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    (cookie, key)
}

fn set(key: &str, value: Value) -> Value {
    json!({ "op": "set", "key": key, "value": value })
}

fn patch(expected: &str, ops: Vec<Value>) -> Value {
    json!({ "expected_hash": expected, "ops": ops })
}

async fn b_hash(a: &Node, b: &Node) -> String {
    let r = local(
        a,
        "GET",
        &format!("/api/nodes/{}/config", b.id.node_id()),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body["hash"].as_str().unwrap().to_string()
}

/// A config edit A's operator confirmed for `ops` against `expected` on
/// `target`, signed by `from`, as A would send it.
fn signed_patch(
    from: &Identity,
    target: &str,
    expected: &str,
    ops: Vec<Value>,
    at: i64,
) -> Command {
    let op_hash = tracon::mesh::admin::config_patch_op_hash(target, expected, &ops);
    let statement = AdminStatement::sign(from, target, &op_hash, "On beta: test", "local", at);
    Command::ConfigPatch {
        expected_hash: expected.into(),
        ops,
        not_after_ms: statement.not_after_ms,
        statement,
    }
}

fn refused_with(result: &Result<Value, CommandError>, words: &str) -> bool {
    matches!(result, Err(CommandError::Refused(m)) if m.contains(words))
}

/// A push subscription on B whose attempts are recorded: B has no push
/// subject, so each push is recorded as not sent, which is enough to see
/// that one was made for a save.
fn subscribe(store: &Store) {
    store
        .push_subscription_upsert(&tracon::store::PushSubscriptionRow {
            id: "phone".into(),
            session_hash: None,
            endpoint: "https://push.invalid/x".into(),
            p256dh: "x".into(),
            auth: "x".into(),
            user_agent: None,
            created_ms: tracon::store::now_ms(),
            last_ok_ms: None,
            fail_count: 0,
            last_error: None,
            last_error_ms: None,
        })
        .unwrap();
}

/// B names A: A's operator reads B's file as B's own Settings would show
/// it, and edits it. At A's machine the edit needs no passkey and is still
/// signed; off it, A's passkey confirms it with a prompt naming B and the
/// keys. B writes it in place, audits it as A's for that operator session,
/// pushes it, and applies the live key at once.
#[tokio::test]
async fn an_admin_peer_reads_the_redacted_file_and_edits_it_with_a_passkey() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    let (a, b) = pair(&[identity(1).node_id()]).await;
    let bi = b.id.node_id();
    subscribe(&b.store);

    let r = local(&a, "GET", &format!("/api/nodes/{bi}/config"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["node"]["name"], "beta");
    assert_eq!(r.body["document"]["review"]["max_files"], 40);
    assert_eq!(
        r.body["document"]["qa"]["browser_token"],
        r.body["redacted"]
    );
    let text = r.body.to_string();
    assert!(!text.contains("sk-live"), "{text}");
    assert!(!text.contains("small on purpose"), "{text}");
    let admin_peers = r.body["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["key"] == "mesh.admin_peers")
        .unwrap()
        .clone();
    assert_eq!(admin_peers["editable_by_peer"], false, "{admin_peers}");
    let hash = r.body["hash"].as_str().unwrap().to_string();

    // At A's machine.
    let r = local(
        &a,
        "PATCH",
        &format!("/api/nodes/{bi}/config"),
        Some(&patch(&hash, vec![set("review.max_files", json!(41))])),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["changed"], json!(["review.max_files"]));
    assert!(
        file().contains("max_files = 41       # small on purpose"),
        "{}",
        file()
    );
    assert_eq!(b.cfg.live().review.max_files, 41);
    let audit = &b.store.config_audit_recent(10).unwrap()[0];
    let short = &a.id.node_id()[..16];
    assert_eq!(audit.actor, format!("peer {short} for local"));
    let audit_id = audit.id.clone();
    wait_for("B to push the save", || {
        b.store
            .notification_attempts(&audit_id)
            .is_ok_and(|a| !a.is_empty())
    })
    .await;

    // Off A's machine: A's passkey, with a prompt that names B and the key.
    let (cookie, mut key) = pair_phone(&a).await;
    // (Pairing writes the operator token into this process's one node.toml.)
    let hash = b_hash(&a, &b).await;
    let body = patch(&hash, vec![set("review.max_files", json!(42))]);
    let uri = format!("/api/nodes/{bi}/config");
    let refused = remote(&a, &cookie, "PATCH", &uri, Some(&body), None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert_eq!(refused.kind(), "step_up_required", "{}", refused.body);
    let summary = refused.body["step_up"]["summary"].as_str().unwrap();
    assert!(
        summary.starts_with(&format!(
            "On beta ({}\u{2026}): Change node.toml: review.max_files",
            &bi[..8]
        )),
        "{summary}"
    );
    assert!(
        file().contains("max_files = 41"),
        "nothing ran before the passkey"
    );
    let step_up = &refused.body["step_up"];
    let credential = key.get(&step_up["options"]);
    let finished = remote(
        &a,
        &cookie,
        "POST",
        "/api/passkeys/assert/finish",
        Some(&json!({ "challenge_id": step_up["challenge_id"], "credential": credential })),
        None,
    )
    .await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    let proof = finished.body["proof"].as_str().unwrap().to_string();
    // A proof for this edit confirms no other.
    let other = patch(&hash, vec![set("review.max_files", json!(43))]);
    let r = remote(&a, &cookie, "PATCH", &uri, Some(&other), Some(&proof)).await;
    assert_eq!(r.kind(), "step_up_required", "{}", r.body);
    let r = remote(&a, &cookie, "PATCH", &uri, Some(&body), Some(&proof)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(file().contains("max_files = 42"), "{}", file());
    assert_eq!(b.cfg.live().review.max_files, 42);
    let audit = &b.store.config_audit_recent(10).unwrap()[0];
    assert!(
        audit.actor.starts_with(&format!("peer {short} for ")),
        "{}",
        audit.actor
    );
    assert_ne!(audit.actor, format!("peer {short} for local"));
    // And spent: the same proof confirms nothing further.
    let next = patch(
        r.body["hash"].as_str().unwrap(),
        vec![set("review.max_files", json!(42))],
    );
    let again = remote(&a, &cookie, "PATCH", &uri, Some(&next), Some(&proof)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(
        again.body["changed"],
        json!([]),
        "no change, nothing to confirm"
    );
    let next = patch(
        r.body["hash"].as_str().unwrap(),
        vec![set("review.max_files", json!(44))],
    );
    let again = remote(&a, &cookie, "PATCH", &uri, Some(&next), Some(&proof)).await;
    assert_eq!(again.kind(), "step_up_required", "{}", again.body);
    assert!(file().contains("max_files = 42"));
}

/// A node that does not name A takes nothing from it: neither a read nor an
/// edit, at A's machine or not, and A says why.
#[tokio::test]
async fn a_peer_not_named_in_admin_peers_reads_and_edits_nothing() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    let (a, b) = pair(&[]).await;
    let bi = b.id.node_id();
    let before = file();
    let r = local(&a, "GET", &format!("/api/nodes/{bi}/config"), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert_eq!(r.kind(), "not_admin_peer", "{}", r.body);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("admin_peers"),
        "{}",
        r.body
    );
    let r = local(
        &a,
        "PATCH",
        &format!("/api/nodes/{bi}/config"),
        Some(&patch("x", vec![set("review.max_files", json!(1))])),
    )
    .await;
    assert_eq!(r.kind(), "not_admin_peer", "{}", r.body);
    // Nor does a statement A signs itself get past B.
    let hash = tracon::config::edit::hash(before.as_bytes());
    let command = signed_patch(
        &a.id,
        &bi,
        &hash,
        vec![set("review.max_files", json!(1))],
        tracon::store::now_ms(),
    );
    let result = a.client.command(&bi, command, Duration::from_secs(5)).await;
    assert!(refused_with(&result, "admin_peers"), "{result:?}");
    assert_eq!(file(), before);
}

/// B checks every statement against what it is about to run: one spent
/// already, one for other operations, one for another node, one past its
/// time, and an edit whose own deadline passed are each refused, and the
/// file is untouched.
#[tokio::test]
async fn a_statement_runs_once_for_exactly_its_edit_on_exactly_its_node() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    let (a, b) = pair(&[identity(1).node_id()]).await;
    let (ai, bi) = (a.id.node_id(), b.id.node_id());
    let t = Duration::from_secs(5);
    let now = tracon::store::now_ms;
    let hash = b_hash(&a, &b).await;
    let ops = || vec![set("review.max_files", json!(50))];

    let good = signed_patch(&a.id, &bi, &hash, ops(), now());
    let ok = a.client.command(&bi, good.clone(), t).await.unwrap();
    assert_eq!(ok["status"], 200, "{ok}");
    assert!(file().contains("max_files = 50"));
    let hash = ok["body"]["hash"].as_str().unwrap().to_string();

    // The same statement again: spent.
    let result = a.client.command(&bi, good, t).await;
    assert!(refused_with(&result, "already used"), "{result:?}");

    // Confirmed for other operations: the statement does not carry over.
    let Command::ConfigPatch { statement, .. } = signed_patch(
        &a.id,
        &bi,
        &hash,
        vec![set("review.max_files", json!(1))],
        now(),
    ) else {
        unreachable!()
    };
    let swapped = Command::ConfigPatch {
        expected_hash: hash.clone(),
        ops: vec![set("review.max_files", json!(99))],
        not_after_ms: statement.not_after_ms,
        statement,
    };
    let result = a.client.command(&bi, swapped, t).await;
    assert!(refused_with(&result, "another operation"), "{result:?}");

    // For another node.
    let c = identity(3).node_id();
    let Command::ConfigPatch { statement, .. } = signed_patch(&a.id, &c, &hash, ops(), now())
    else {
        unreachable!()
    };
    let elsewhere = Command::ConfigPatch {
        expected_hash: hash.clone(),
        ops: ops(),
        not_after_ms: statement.not_after_ms,
        statement,
    };
    let result = a.client.command(&bi, elsewhere, t).await;
    assert!(refused_with(&result, "another node"), "{result:?}");

    // Confirmed too long ago.
    let stale = signed_patch(&a.id, &bi, &hash, ops(), now() - STATEMENT_TTL_MS - 60_000);
    let result = a.client.command(&bi, stale, t).await;
    assert!(refused_with(&result, "expired"), "{result:?}");

    // An edit whose own deadline passed.
    let Command::ConfigPatch { statement, .. } = signed_patch(&a.id, &bi, &hash, ops(), now())
    else {
        unreachable!()
    };
    let late = Command::ConfigPatch {
        expected_hash: hash.clone(),
        ops: ops(),
        not_after_ms: now() - 120_000,
        statement,
    };
    let result = a.client.command(&bi, late, t).await;
    assert!(refused_with(&result, "expired"), "{result:?}");

    // From a node other than the one that signed it: B checks the issuer
    // against the frame's sender, here A sending C's statement.
    let c_id = identity(3);
    let Command::ConfigPatch { statement, .. } = signed_patch(&c_id, &bi, &hash, ops(), now())
    else {
        unreachable!()
    };
    let relayed = Command::ConfigPatch {
        expected_hash: hash.clone(),
        ops: ops(),
        not_after_ms: statement.not_after_ms,
        statement,
    };
    let result = a.client.command(&bi, relayed, t).await;
    assert!(refused_with(&result, "another node than"), "{result:?}");

    assert!(file().contains("max_files = 50"), "{}", file());
    assert_eq!(b.store.config_audit_recent(10).unwrap().len(), 1);
    let seen = b.client.snapshot().last_refusal.unwrap_or_default();
    assert!(seen.contains(&ai), "{seen}");
}

/// What a peer may never change over the mesh, statement or not: who may
/// administer the node, and the keys only its host changes. A refuses before
/// asking for a passkey; B refuses again, with its own words, if asked
/// directly.
#[tokio::test]
async fn admin_peers_and_host_only_keys_are_not_changed_over_the_mesh() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    let (a, b) = pair(&[identity(1).node_id()]).await;
    let bi = b.id.node_id();
    let hash = b_hash(&a, &b).await;
    let before = file();
    for op in [
        set("mesh.admin_peers", json!([identity(3).node_id()])),
        json!({ "op": "remove", "key": "mesh.admin_peers" }),
        json!({ "op": "remove", "key": "mesh" }),
        set("boundary.harness_image", json!("evil:latest")),
        set("gateway.harness_listen", json!("/tmp/h.sock")),
        set("mesh.hub_url", json!("https://elsewhere.example")),
        set("auth.public_origin", json!("https://elsewhere.example")),
        set(
            "providers.anthropic.upstream",
            json!("https://elsewhere.example"),
        ),
        set("embed.base_url", json!("https://elsewhere.example")),
    ] {
        let r = local(
            &a,
            "PATCH",
            &format!("/api/nodes/{bi}/config"),
            Some(&patch(&hash, vec![op.clone()])),
        )
        .await;
        assert_eq!(
            r.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{op}: {}",
            r.body
        );
        assert_eq!(r.kind(), "refused", "{op}: {}", r.body);

        let direct = signed_patch(&a.id, &bi, &hash, vec![op.clone()], tracon::store::now_ms());
        let answer = a
            .client
            .command(&bi, direct, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer["status"], 422, "{op}: {answer}");
        assert_eq!(answer["body"]["error"]["kind"], "refused", "{op}: {answer}");
    }
    // A character a prompt could not show, in a value or a key: refused on
    // A before anything is confirmed, and on B even with a statement.
    for op in [
        set("publish.gh", json!("/usr/bin/gh\u{202e}")),
        set("review.max_files\u{200b}", json!(1)),
    ] {
        let r = local(
            &a,
            "PATCH",
            &format!("/api/nodes/{bi}/config"),
            Some(&patch(&hash, vec![op.clone()])),
        )
        .await;
        assert_eq!(
            r.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{op}: {}",
            r.body
        );
        assert_eq!(r.kind(), "hidden_character", "{op}: {}", r.body);
        let direct = signed_patch(&a.id, &bi, &hash, vec![op.clone()], tracon::store::now_ms());
        let answer = a
            .client
            .command(&bi, direct, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer["status"], 422, "{op}: {answer}");
        assert_eq!(
            answer["body"]["error"]["kind"], "hidden_character",
            "{op}: {answer}"
        );
    }
    assert_eq!(file(), before);
    assert!(b.store.config_audit_recent(10).unwrap().is_empty());
    // A stale version is a conflict carrying B's version now.
    let r = local(
        &a,
        "PATCH",
        &format!("/api/nodes/{bi}/config"),
        Some(&patch("stale", vec![set("review.max_files", json!(2))])),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert_eq!(r.body["hash"], hash.as_str());
}

/// A peer that never answers and last said it runs an older build is named
/// for it; one that is away is refused at once, and nothing is sent to wait
/// for it.
#[tokio::test]
async fn an_older_or_absent_peer_is_named_and_nothing_is_queued() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    let (a, _b) = pair(&[identity(1).node_id()]).await;
    let gi = identity(3).node_id();
    a.client.refresh_members().await.unwrap();
    wait_for("A to learn the silent member", || {
        a.client.peer_key(&gi).is_some() && a.store.get_node(&gi).ok().flatten().is_some()
    })
    .await;

    // Away: refused up front, and nothing goes into the outbox for it.
    let queued = a.client.snapshot().queued;
    let r = local(
        &a,
        "PATCH",
        &format!("/api/nodes/{gi}/config"),
        Some(&patch("h", vec![set("review.max_files", json!(3))])),
    )
    .await;
    assert_eq!(r.status, StatusCode::GATEWAY_TIMEOUT, "{}", r.body);
    assert_eq!(r.kind(), "unreachable", "{}", r.body);
    assert_eq!(a.client.snapshot().queued, queued);

    // Present by its last hello, on an older build, and silent.
    let mut row = a.store.get_node(&gi).unwrap().unwrap();
    row.app_version = Some("0.0.1".into());
    a.store.put_node(&row).unwrap();
    a.store.set_reachable(&gi, true).unwrap();
    let r = local(&a, "GET", &format!("/api/nodes/{gi}/config"), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert_eq!(r.kind(), "older_build", "{}", r.body);
}

/// The path guard runs on B, against B's host, when a peer's edit is
/// written: a program planted where an agent can write is refused there,
/// and A relays B's refusal as B's, by key.
#[tokio::test]
async fn a_peers_edit_meets_the_path_guard_on_the_node_it_changes() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    let (a, b) = pair(&[identity(1).node_id()]).await;
    let bi = b.id.node_id();
    let hash = b_hash(&a, &b).await;
    let before = file();
    let planted = Config::state_dir().join("workspaces/planted-peer/gh");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(&planted, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
    let r = local(
        &a,
        "PATCH",
        &format!("/api/nodes/{bi}/config"),
        Some(&patch(
            &hash,
            vec![set("publish.gh", json!(planted.to_string_lossy()))],
        )),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.body);
    assert_eq!(r.kind(), "peer_refused", "{}", r.body);
    assert_eq!(r.body["error"]["peer_kind"], "refused", "{}", r.body);
    assert_eq!(
        r.body["error"]["fields"][0]["field"], "publish.gh",
        "{}",
        r.body
    );
    assert_eq!(file(), before);
    assert!(b.store.config_audit_recent(10).unwrap().is_empty());
}

/// What answers for a peer that forges this node's own refusals.
struct Forger(Value);

#[async_trait::async_trait]
impl CommandExecutor for Forger {
    async fn execute(&self, _sender: &str, _command: Command) -> Result<Value, String> {
        Ok(self.0.clone())
    }
}

/// A peer cannot make A's interface ask for a passkey, or sign in again, by
/// answering with what A itself would say: its refusal is relayed as the
/// peer's, `peer_refused`, never as a step-up or a 401.
#[tokio::test]
async fn a_peer_forging_a_step_up_is_relayed_as_its_refusal() {
    let _lock = state::config_lock().await;
    state::isolate_dirs();
    for status in [403, 401] {
        let forged = json!({
            "status": status,
            "body": {
                "error": { "code": status, "kind": "step_up_required", "message": "confirm with your passkey" },
                "step_up": { "challenge_id": "forged", "options": {}, "summary": "Change nothing" },
            },
        });
        let (a, b) = pair_with(&[identity(1).node_id()], Some(Arc::new(Forger(forged)))).await;
        let bi = b.id.node_id();
        let r = local(&a, "GET", &format!("/api/nodes/{bi}/config"), None).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
        assert_eq!(r.kind(), "peer_refused", "{}", r.body);
        assert_eq!(
            r.body["error"]["peer_kind"], "step_up_required",
            "{}",
            r.body
        );
        assert!(r.body.get("step_up").is_none(), "{}", r.body);
        assert!(
            r.body["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("beta refused: "),
            "{}",
            r.body
        );
    }
}
