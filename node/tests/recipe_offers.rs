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
    boundary::{
        Backend, BoundaryError, BoundaryReport, BuildFailure, BuiltImage, ImageBuild, ImageBuilder,
    },
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
    node_with(NODE_TOML, Arc::new(tracon::runner::local::LocalBackend))
}

fn node_with(toml: &str, backend: Arc<dyn Backend>) -> Node {
    state::isolate();
    std::fs::write(Config::config_path(), toml).unwrap();
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
        backend,
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
    tracon::store::now_ms() + 2 * 3_600_000
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

/// The offer as the operator reviews it with `query` (`?path=…&…`).
async fn review(n: &Node, id: &str, query: &str) -> Value {
    let (s, v) = call(
        n,
        "GET",
        &format!("/api/repos/offers/{id}{query}"),
        LOCAL,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v
}

/// Accept `id` with `placement`, naming what a review with the same
/// placement showed, as the interface does.
async fn accept(n: &Node, id: &str, peer: &str, placement: Value) -> (StatusCode, Value) {
    let mut query = Vec::new();
    if let Some(path) = placement["path"].as_str() {
        query.push(format!("path={}", path.replace(' ', "%20")));
    }
    for flag in ["replace_image", "keep_credentials"] {
        if placement[flag] == true {
            query.push(format!("{flag}=true"));
        }
    }
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    let seen = review(n, id, &query).await;
    let mut body = if placement.is_object() {
        placement
    } else {
        json!({})
    };
    body["viewed_hash"] = seen["viewed_hash"].clone();
    call(
        n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        peer,
        Some(body),
    )
    .await
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
    assert_eq!(
        fields,
        [
            "prepare_credentials",
            "dockerfile",
            "checks",
            "prepare",
            "egress"
        ],
        "{v}"
    );
    // A peer changed what the default branch's preparation runs and
    // reaches, so the credential it was lent is not lent to that by default.
    assert_eq!(v["after"]["prepare_credentials"], Value::Null, "{v}");
    assert_eq!(
        v["credentials"],
        json!({ "state": "dropped", "names": ["nova"] })
    );
    assert!(v["notes"].to_string().contains("no longer lent"), "{v}");
    assert_eq!(v["kept"], json!(["path", "commits"]));

    // An operator elsewhere reads it, but nothing is applied from off the
    // node: what an entry's `prepare` names, the node runs.
    let (s, v) = call(&n, "GET", &format!("/api/repos/offers/{id}"), REMOTE, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Accepting is an admin action: off the node a bearer token never
    // passes, and a signed-in browser is asked for a passkey first.
    let (s, v) = accept(&n, &id, REMOTE, json!({})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v.to_string().contains("passkey"), "{v}");
    let (s, v) = accept(&n, &id, REMOTE_BROWSER, json!({})).await;
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

    // An accept that names nothing reviewed is refused.
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    let (s, v) = accept(&n, &id, LOCAL, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["changed"], true);
    assert_eq!(v["credentials"]["state"], "dropped");
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
    assert!(entry.prepare_credentials.is_empty());
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
    let (s, _) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({ "viewed_hash": "x" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

/// Keeping the credentials for a preparation a peer changed is the
/// operator's explicit choice, and then nothing is built until they start it.
#[tokio::test]
async fn credentials_are_kept_only_when_asked_and_then_nothing_builds() {
    let _guard = state::config_lock().await;
    let n = node();
    let id = staged(&n, recipe()).await;
    let v = review(&n, &id, "?keep_credentials=true").await;
    assert_eq!(
        v["credentials"],
        json!({ "state": "kept", "names": ["nova"] })
    );
    assert_eq!(v["after"]["prepare_credentials"], json!(["nova"]));
    assert!(v["kept"].to_string().contains("prepare_credentials"), "{v}");

    // A choice the operator did not review is refused: keeping them was
    // reviewed, dropping them was not, and the other way about.
    let dropped = review(&n, &id, "").await;
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({ "keep_credentials": true, "viewed_hash": dropped["viewed_hash"] })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("review"), "{v}");
    assert_eq!(
        std::fs::read_to_string(Config::config_path()).unwrap(),
        NODE_TOML
    );

    let (s, v) = accept(&n, &id, LOCAL, json!({ "keep_credentials": true })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["credentials"]["state"], "kept");
    assert_eq!(v["building"], false);
    let file = Config::try_load().unwrap();
    assert_eq!(file.repo[0].prepare_credentials, ["nova"]);
    assert_eq!(file.repo[0].prepare, ["composer install", "npm ci"]);

    // The entry is held: Settings says so, an ordinary build is refused, and
    // only the start, an admin action, releases it.
    let (_, v) = call(&n, "GET", "/api/repos/environments", LOCAL, None).await;
    assert_eq!(
        v["entries"][0]["hold"]["credentials"],
        json!(["nova"]),
        "{v}"
    );
    assert_eq!(v["entries"][1]["hold"], Value::Null);
    assert!(n
        .store
        .credential_hold("github.com/acme/hdr")
        .unwrap()
        .is_some());
    let hash = v["entries"][0]["hold"]["lending_hash"].clone();
    let start = json!({ "path": "github.com/acme/hdr", "lending_hash": hash });
    // A start for a hold it does not describe is refused, and leaves it.
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/holds/start",
        LOCAL,
        Some(json!({ "path": "github.com/acme/hdr", "lending_hash": "other" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/holds/start",
        REMOTE,
        Some(start.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/holds/start",
        REMOTE_BROWSER,
        Some(start.clone()),
    )
    .await;
    assert_ne!(s, StatusCode::OK, "{v}");
    assert!(n
        .store
        .credential_hold("github.com/acme/hdr")
        .unwrap()
        .is_some());
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/holds/start",
        LOCAL,
        Some(start.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["released"], true);
    assert!(n
        .store
        .credential_hold("github.com/acme/hdr")
        .unwrap()
        .is_none());
    let (s, _) = call(&n, "POST", "/api/repos/holds/start", LOCAL, Some(start)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// A held entry stays held when a later offer leaves its preparation as it
/// is: an accept that changes only the checks or the time limit, or that
/// sends the same recipe again, builds nothing and keeps the hold, so the
/// peer's preparation still waits for the operator's start.
#[tokio::test]
async fn a_later_offer_onto_a_held_entry_keeps_it_held() {
    let _guard = state::config_lock().await;
    let n = node();
    let id = staged(&n, recipe()).await;
    let (s, v) = accept(&n, &id, LOCAL, json!({ "keep_credentials": true })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let first = n
        .store
        .credential_hold("github.com/acme/hdr")
        .unwrap()
        .unwrap();

    let mut checks_only = recipe();
    checks_only["checks"] = json!(["just all"]);
    checks_only["timeout_secs"] = json!(1800);
    for (what, again) in [
        ("checks and time limit", checks_only),
        ("the same recipe", recipe()),
    ] {
        let id = staged(&n, again).await;
        let seen = review(&n, &id, "").await;
        assert_eq!(seen["credentials"]["state"], "still_held", "{what}: {seen}");
        assert!(
            seen["notes"].to_string().contains("stays held"),
            "{what}: {seen}"
        );
        let (s, v) = accept(&n, &id, LOCAL, json!({})).await;
        assert_eq!(s, StatusCode::OK, "{what}: {v}");
        assert_eq!(v["building"], false, "{what}: {v}");
        assert_eq!(v["credentials"]["state"], "still_held", "{what}: {v}");
        let hold = n
            .store
            .credential_hold("github.com/acme/hdr")
            .unwrap()
            .expect("still held");
        // Still the first peer's preparation, still on what it lends.
        assert_eq!(hold.offer_id, first.offer_id, "{what}");
        assert_eq!(hold.lending_hash, first.lending_hash, "{what}");
        let file = Config::try_load().unwrap();
        assert_eq!(file.repo[0].prepare_credentials, ["nova"], "{what}");
    }
    assert_eq!(
        Config::try_load().unwrap().repo[0].checks.as_deref(),
        Some(&["just check".to_string()][..])
    );
}

/// The table moving between the review and the accept refuses the accept
/// rather than writing something the operator did not see.
#[tokio::test]
async fn an_accept_against_a_table_that_moved_is_refused() {
    let _guard = state::config_lock().await;
    let n = node();
    let id = staged(&n, recipe()).await;
    let seen = review(&n, &id, "").await;
    let mut entries = n.state.cfg.repos().as_ref().clone();
    entries[0].commits = None;
    n.state.cfg.live_repo.set(entries);
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({ "viewed_hash": seen["viewed_hash"] })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(n.store.recipe_offer(&id).unwrap().unwrap().state, "pending");
    assert_eq!(
        std::fs::read_to_string(Config::config_path()).unwrap(),
        NODE_TOML
    );
}

/// An offer whose sender has left the channel it came on is withdrawn: it
/// can be neither reviewed nor accepted, and the list says so.
#[tokio::test]
async fn an_offer_from_a_sender_that_left_the_channel_is_withdrawn() {
    let _guard = state::config_lock().await;
    let n = node();
    let id = staged(&n, recipe()).await;
    let seen = review(&n, &id, "").await;
    n.store.node_channels_set(PEER, &[]).unwrap();
    let (s, v) = call(
        &n,
        "POST",
        &format!("/api/repos/offers/{id}/accept"),
        LOCAL,
        Some(json!({ "viewed_hash": seen["viewed_hash"] })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("withdrawn"), "{v}");
    assert_eq!(
        n.store.recipe_offer(&id).unwrap().unwrap().state,
        "withdrawn"
    );
    let (_, v) = call(&n, "GET", "/api/repos/offers", LOCAL, None).await;
    assert_eq!(v["offers"][0]["state"], "withdrawn");
    // The list withdraws one it finds too.
    n.store.node_channel_add(PEER, "personal").unwrap();
    let other = staged(&n, recipe()).await;
    n.store.node_channels_set(PEER, &[]).unwrap();
    let (_, v) = call(&n, "GET", "/api/repos/offers", LOCAL, None).await;
    let listed = v["offers"].as_array().unwrap();
    assert!(
        listed
            .iter()
            .any(|o| o["id"] == other.as_str() && o["state"] == "withdrawn"),
        "{v}"
    );
    assert_eq!(
        std::fs::read_to_string(Config::config_path()).unwrap(),
        NODE_TOML
    );
}

/// A relative entry that only ends like the offered repository might be for
/// another forge's repository of that name: the operator names the entry.
#[tokio::test]
async fn a_suffix_match_is_not_chosen_for_the_operator() {
    let _guard = state::config_lock().await;
    let n = node_with(
        "[[repo]]\npath = \"acme/hdr\"\nchecks = []\n",
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let id = staged(&n, recipe()).await;
    let (s, v) = call(&n, "GET", &format!("/api/repos/offers/{id}"), LOCAL, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert!(v.to_string().contains("name the entry"), "{v}");
    let (s, v) = accept(&n, &id, LOCAL, json!({ "path": "acme/hdr" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(Config::try_load().unwrap().repo[0].prepare.len(), 2);
}

/// A runtime that builds, as far as an accept can tell: it says it can and
/// records nothing else.
struct Building {
    inner: tracon::runner::local::LocalBackend,
    builder: Refusing,
}

struct Refusing;

#[async_trait::async_trait]
impl ImageBuilder for Refusing {
    async fn build(&self, _build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        Err(BuildFailure::transient("this builder builds nothing"))
    }
    async fn exists(&self, _image: &str) -> bool {
        false
    }
    async fn identity(&self, _image: &str) -> Option<String> {
        None
    }
    async fn remove(&self, _image: &str) {}
}

#[async_trait::async_trait]
impl Backend for Building {
    fn kind(&self) -> &'static str {
        "building"
    }
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        self.inner.setup(cfg, rebuild).await
    }
    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        self.inner.check_all(cfg, deep).await
    }
    fn runner(&self, extra: Vec<tracon::runner::Mount>) -> Arc<dyn tracon::runner::Runner> {
        self.inner.runner(extra)
    }
    fn runner_for(
        &self,
        id: &str,
        extra: Vec<tracon::runner::Mount>,
    ) -> Arc<dyn tracon::runner::Runner> {
        self.inner.runner_for(id, extra)
    }
    fn image_builder(&self) -> Option<&dyn ImageBuilder> {
        Some(&self.builder)
    }
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.inner.import_volume(volume, source).await
    }
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.inner.export_volume(volume, destination).await
    }
    fn harness_host(&self) -> String {
        self.inner.harness_host()
    }
    fn harness_home(&self) -> String {
        self.inner.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        self.inner.reconcile(names).await
    }
}

/// On a runtime that builds, an accepted entry that names a Dockerfile for a
/// checkout on this node starts building at once, unless it keeps lending
/// credentials to a preparation the offer changed.
#[tokio::test]
async fn an_accept_starts_the_build_unless_credentials_are_kept() {
    let _guard = state::config_lock().await;
    state::isolate();
    let checkout = state::scratch("recipe-offer-checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let toml = format!(
        "[[repo]]\npath = {:?}\ndockerfile = \"Dockerfile\"\nprepare = [\"composer install\"]\n\
         egress = [\"packagist\"]\nprepare_credentials = [\"nova\"]\n",
        checkout.to_string_lossy()
    );
    let n = node_with(
        &toml,
        Arc::new(Building {
            inner: tracon::runner::local::LocalBackend,
            builder: Refusing,
        }),
    );
    let path = checkout.to_string_lossy().to_string();
    let id = staged(&n, recipe()).await;
    let (s, v) = accept(&n, &id, LOCAL, json!({ "path": path })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["building"], true, "{v}");
    assert_eq!(v["credentials"]["state"], "dropped", "{v}");

    let n = node_with(
        &toml,
        Arc::new(Building {
            inner: tracon::runner::local::LocalBackend,
            builder: Refusing,
        }),
    );
    let id = staged(&n, recipe()).await;
    let (s, v) = accept(
        &n,
        &id,
        LOCAL,
        json!({ "path": path, "keep_credentials": true }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["credentials"]["state"], "kept", "{v}");
    assert_eq!(v["building"], false, "{v}");

    // Held, the repository's own build is refused and says why; started,
    // it builds.
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/environments/build",
        LOCAL,
        Some(json!({ "repo": path })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("held: nova"), "{v}");
    let hash = n
        .store
        .credential_hold(&path)
        .unwrap()
        .unwrap()
        .lending_hash;
    let (s, v) = call(
        &n,
        "POST",
        "/api/repos/holds/start",
        LOCAL,
        Some(json!({ "path": path, "lending_hash": hash })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["building"], true, "{v}");
}

/// A note or a command that hides what it says is refused on receipt.
#[tokio::test]
async fn an_offer_hiding_characters_is_refused_on_receipt() {
    let _guard = state::config_lock().await;
    let n = node();
    let mut bidi = recipe();
    bidi["prepare"] = json!(["npm ci \u{202E}lmth.x"]);
    let refused = n
        .state
        .execute(PEER, offer(bidi, "personal", later()))
        .await
        .unwrap_err();
    assert!(refused.contains("invisible"), "{refused}");
    let refused = n
        .state
        .execute(
            PEER,
            Command::RecipeOffer {
                recipe: recipe(),
                from_channel: "personal".into(),
                note: Some("fine\u{200B}".into()),
                not_after_ms: later(),
            },
        )
        .await
        .unwrap_err();
    assert!(refused.contains("note"), "{refused}");
    let mut address = recipe();
    address["egress"] = json!(["169.254.169.254"]);
    let refused = n
        .state
        .execute(PEER, offer(address, "personal", later()))
        .await
        .unwrap_err();
    assert!(refused.contains("IP address"), "{refused}");
    assert!(n.store.recipe_offers(10).unwrap().is_empty());
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

    let accept_path = format!("/api/repos/offers/{id}/accept");
    for (path, why) in [
        ("/srv/notes/sub/../x", "climb"),
        ("not a path/../x", "neither"),
    ] {
        let (s, v) = call(
            &n,
            "POST",
            &accept_path,
            LOCAL,
            Some(json!({ "path": path, "viewed_hash": "x" })),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {v}");
        assert!(v.to_string().contains(why), "{path}: {v}");
    }
    let (s, v) = accept(&n, &id, LOCAL, json!({ "path": "/srv/notes" })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let file = Config::try_load().unwrap();
    assert_eq!(file.repo.len(), 2);
    assert_eq!(file.repo[1].prepare, ["composer install", "npm ci"]);

    // A new entry for a checkout no entry answers for is added.
    let id = staged(&n, recipe()).await;
    let (s, v) = accept(&n, &id, LOCAL, json!({ "path": "/elsewhere/hdr" })).await;
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
        Some(json!({ "viewed_hash": "x" })),
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

    // One that would lapse within the hour is not staged either: an
    // operator is given time to read an offer. (A staged offer that lapses
    // is refused at accept; the store's tests hold it to its time.)
    let short = n
        .state
        .execute(
            PEER,
            offer(recipe(), "personal", tracon::store::now_ms() + 60_000),
        )
        .await
        .unwrap_err();
    assert!(short.contains("within the hour"), "{short}");
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
