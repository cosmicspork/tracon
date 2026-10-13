//! Repository recipes peers offer this node: staged, shown against the entry
//! they would change, and applied only when the operator at the node accepts
//! one, through the `[[repo]]` writer that keeps what is this node's own.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use proto::frame::Command;
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
    mesh::forward::CommandExecutor,
    session::Manager,
    store::Store,
    stream::Bus,
};

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";
/// The same remote address, presenting a signed-in browser's cookie.
const REMOTE_BROWSER: &str = "203.0.113.7:5001";
const COOKIE_SECRET: &str = "browser-cookie";
const PEER: &str = "peer-node";
const STRANGER: &str = "stranger-node";
const TOKEN: &str = "operator-token";

const NODE_TOML: &str = r#"# The operator's node.

# hounddog, as this node builds it
[[repo]]
path = "github.com/acme/hdr" # the managed clone
dockerfile = "Dockerfile"
prepare = ["composer install"]
egress = ["packagist"]
prepare_credentials = ["nova"]
commits = "keep"

# notes: no environment of its own
[[repo]]
path = "/srv/notes"
checks = []
"#;

struct Node {
    app: axum::Router,
    store: Arc<Store>,
    state: AppState,
}

/// A node serving the configuration in `node.toml`, with the guard layered
/// on as `serve` builds it, sharing `personal` with `PEER`.
fn node() -> Node {
    state::isolate();
    std::fs::write(Config::config_path(), NODE_TOML).unwrap();
    let cfg = Arc::new(Config::try_load().unwrap());
    let store = Arc::new(Store::open_in_memory().unwrap());
    for (node, channel) in [
        ("n1", "personal"),
        (PEER, "personal"),
        (STRANGER, "other"),
        ("n1", "other-n1"),
    ] {
        store.node_channel_add(node, channel).unwrap();
    }
    let now = tracon::store::now_ms();
    store
        .auth_session_insert(&tracon::store::AuthSessionRow {
            token_hash: auth::hash(COOKIE_SECRET),
            created_ms: now,
            last_seen_ms: now,
            expires_ms: now + 3_600_000,
            user_agent: None,
            paired_ms: None,
        })
        .unwrap();
    let mut peer = support::rows::node_row(PEER, "laptop");
    peer.is_self = 0;
    store.put_node(&peer).unwrap();
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
        auth: Arc::new(AuthState::new("127.0.0.1".into(), Some(auth::hash(TOKEN)))),
        enroll: Default::default(),
    };
    let app = tracon::http::router(state.clone()).layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::guard,
    ));
    Node { app, store, state }
}

async fn call(
    n: &Node,
    method: &str,
    uri: &str,
    peer: &str,
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
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    if peer == REMOTE {
        // An operator elsewhere, holding the token.
        req.headers_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    }
    if peer == REMOTE_BROWSER {
        req.headers_mut().insert(
            "cookie",
            format!("{}={COOKIE_SECRET}", auth::COOKIE).parse().unwrap(),
        );
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

fn recipe() -> Value {
    json!({
        "repo": "github.com/acme/hdr",
        "dockerfile": ".devcontainer/Dockerfile",
        "checks": ["just check"],
        "prepare": ["composer install", "npm ci"],
        "egress": ["packagist", "npm"],
    })
}

fn offer(recipe: Value, channel: &str, not_after_ms: i64) -> Command {
    Command::RecipeOffer {
        recipe,
        from_channel: channel.into(),
        note: Some("what CI runs".into()),
        not_after_ms,
    }
}

fn later() -> i64 {
    tracon::store::now_ms() + 3_600_000
}

/// Stage `recipe` as `PEER` would, through the executor a mesh command runs.
async fn staged(n: &Node, recipe: Value) -> String {
    let answer = n
        .state
        .execute(PEER, offer(recipe, "personal", later()))
        .await
        .unwrap();
    assert_eq!(answer["state"], "pending", "{answer}");
    answer["offer_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn an_accepted_offer_changes_the_entry_and_keeps_what_is_local() {
    let _guard = state::config_lock().await;
    let n = node();
    let id = staged(&n, recipe()).await;

    let (s, v) = call(&n, "GET", "/api/repos/offers", LOCAL, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let listed = &v["offers"][0];
    assert_eq!(listed["id"], id.as_str());
    assert_eq!(listed["sender_name"], "laptop");
    assert_eq!(listed["state"], "pending");
    assert_eq!(listed["note"], "what CI runs");

    // What would change, on the entry already answering for the repository.
    let (s, v) = call(&n, "GET", &format!("/api/repos/offers/{id}"), LOCAL, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["target"]["path"], "github.com/acme/hdr");
    assert_eq!(v["target"]["exists"], true);
    let fields: Vec<&str> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["dockerfile", "checks", "prepare", "egress"], "{v}");
    assert_eq!(v["after"]["prepare_credentials"], json!(["nova"]));
    assert!(v["notes"].to_string().contains("nova"), "{v}");

    // An operator elsewhere reads it, but nothing is applied from off the
    // node: what an entry's `prepare` names, the node runs.
    let (s, v) = call(&n, "GET", &format!("/api/repos/offers/{id}"), REMOTE, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let accept = format!("/api/repos/offers/{id}/accept");
    // Accepting is an admin action: off the node a bearer token never
    // passes, and a signed-in browser is asked for a passkey first.
    let (s, v) = call(&n, "POST", &accept, REMOTE, Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v.to_string().contains("passkey"), "{v}");
    let (s, v) = call(&n, "POST", &accept, REMOTE_BROWSER, Some(json!({}))).await;
    assert_ne!(s, StatusCode::OK, "{v}");
    assert!(
        v["error"]["kind"]
            .as_str()
            .unwrap_or_default()
            .starts_with("step_up"),
        "{v}"
    );
    assert_eq!(
        std::fs::read_to_string(Config::config_path()).unwrap(),
        NODE_TOML
    );

    let (s, v) = call(&n, "POST", &accept, LOCAL, Some(json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["changed"], true);
    assert_eq!(v["restart_required"], false);
    assert_eq!(v["path"], "github.com/acme/hdr");

    let text = std::fs::read_to_string(Config::config_path()).unwrap();
    for kept in [
        "# The operator's node.",
        "# hounddog, as this node builds it",
        "# the managed clone",
        "# notes: no environment of its own",
    ] {
        assert!(text.contains(kept), "{kept} was lost:\n{text}");
    }
    let file = Config::try_load().unwrap();
    let entry = &file.repo[0];
    assert_eq!(
        entry.dockerfile.as_deref(),
        Some(".devcontainer/Dockerfile")
    );
    assert_eq!(entry.prepare, ["composer install", "npm ci"]);
    assert_eq!(
        entry.checks.as_deref(),
        Some(&["just check".to_string()][..])
    );
    assert_eq!(entry.prepare_credentials, ["nova"]);
    assert!(matches!(entry.commits, Some(tracon::config::Commits::Keep)));
    assert_eq!(file.repo[1].path, std::path::PathBuf::from("/srv/notes"));
    // The running node resolves against it already.
    assert_eq!(n.state.cfg.repos()[0].prepare.len(), 2);
    let audit = n.store.config_audit_recent(5).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].actor, "recipe offer from laptop");

    let row = n.store.recipe_offer(&id).unwrap().unwrap();
    assert_eq!(row.state, "accepted");
    assert_eq!(row.decided_path.as_deref(), Some("github.com/acme/hdr"));
    let (s, _) = call(&n, "POST", &accept, LOCAL, Some(json!({}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

/// An operator may put an offer on an entry of their choosing, or a new one;
/// a new entry another entry would shadow is refused.
#[tokio::test]
async fn an_offer_lands_where_the_operator_puts_it() {
    let _guard = state::config_lock().await;
    let n = node();
    let mut other = recipe();
    other["repo"] = json!("github.com/acme/notes");
    let id = staged(&n, other.clone()).await;
    // Nothing answers for it yet, so a new entry named by the repository.
    let (_, v) = call(&n, "GET", &format!("/api/repos/offers/{id}"), LOCAL, None).await;
    assert_eq!(v["target"]["path"], "github.com/acme/notes", "{v}");
    assert_eq!(v["target"]["exists"], false);
    assert_eq!(v["before"], Value::Null);

    let accept = format!("/api/repos/offers/{id}/accept");
    for (path, why) in [
        ("/srv/notes/sub/../x", "climb"),
        ("not a path/../x", "neither"),
    ] {
        let (s, v) = call(&n, "POST", &accept, LOCAL, Some(json!({ "path": path }))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {v}");
        assert!(v.to_string().contains(why), "{path}: {v}");
    }
    let (s, v) = call(
        &n,
        "POST",
        &accept,
        LOCAL,
        Some(json!({ "path": "/srv/notes" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let file = Config::try_load().unwrap();
    assert_eq!(file.repo.len(), 2);
    assert_eq!(file.repo[1].prepare, ["composer install", "npm ci"]);

    // A new entry for a checkout no entry answers for is added.
    let id = staged(&n, recipe()).await;
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({ "path": "/elsewhere/hdr" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let file = Config::try_load().unwrap();
    assert_eq!(file.repo.len(), 3);
    assert_eq!(
        file.repo[2].path,
        std::path::PathBuf::from("/elsewhere/hdr")
    );
    assert!(file.repo[2].prepare_credentials.is_empty());
}

#[tokio::test]
async fn an_offer_is_declined_superseded_or_lapses() {
    let _guard = state::config_lock().await;
    let n = node();
    let first = staged(&n, recipe()).await;
    let second = staged(&n, recipe()).await;
    assert_eq!(
        n.store.recipe_offer(&first).unwrap().unwrap().state,
        "superseded"
    );
    let (s, _) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{first}/accept"),
        LOCAL,
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Declining changes nothing on the node, so an operator elsewhere may.
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{second}/decline"),
        REMOTE,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        n.store.recipe_offer(&second).unwrap().unwrap().state,
        "declined"
    );
    let (s, _) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{second}/decline"),
        LOCAL,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // One that lapsed on the way is not staged at all.
    let lapsed = n
        .state
        .execute(PEER, offer(recipe(), "personal", 1))
        .await
        .unwrap_err();
    assert!(lapsed.contains("lapsed"), "{lapsed}");

    // One that lapses while it waits cannot be accepted.
    let short = n
        .state
        .execute(
            PEER,
            offer(recipe(), "personal", tracon::store::now_ms() + 40),
        )
        .await
        .unwrap();
    let id = short["offer_id"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("expired"), "{v}");
    assert_eq!(
        std::fs::read_to_string(Config::config_path()).unwrap(),
        NODE_TOML
    );
}

/// The receiver holds the sender to the channel it names and the recipe to
/// what a recipe may carry, whatever the sender checked.
#[tokio::test]
async fn an_offer_from_outside_the_channel_or_carrying_local_fields_is_refused() {
    let _guard = state::config_lock().await;
    let n = node();
    for (sender, channel, why) in [
        (STRANGER, "personal", "not a member"),
        (PEER, "other", "not a member"),
        (PEER, "@mesh", "ordinary channel"),
    ] {
        let refused = n
            .state
            .execute(sender, offer(recipe(), channel, later()))
            .await
            .unwrap_err();
        assert!(refused.contains(why), "{sender} on {channel}: {refused}");
    }
    for (field, value) in [
        (
            "image",
            json!(format!("localhost/hdr@sha256:{}", "a".repeat(64))),
        ),
        ("prepare_credentials", json!(["nova"])),
        ("path", json!("/var/home/op/src/hdr")),
    ] {
        let mut carrying = recipe();
        carrying[field] = value;
        let refused = n
            .state
            .execute(PEER, offer(carrying, "personal", later()))
            .await
            .unwrap_err();
        assert!(refused.contains("never shared"), "{field}: {refused}");
    }
    let mut homed = recipe();
    homed["prepare"] = json!(["cp /home/op/.npmrc ."]);
    let refused = n
        .state
        .execute(PEER, offer(homed, "personal", later()))
        .await
        .unwrap_err();
    assert!(refused.contains("home directory"), "{refused}");
    assert!(n.store.recipe_offers(10).unwrap().is_empty());

    // A node off the mesh has nobody to offer to.
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/offer",
        LOCAL,
        Some(json!({ "path": "github.com/acme/hdr", "channel": "personal" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
}
