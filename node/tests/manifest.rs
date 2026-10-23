//! The launch manifest end to end: importing a skill through the interface,
//! what the node refuses, and what a launch renders.
//!
//! The through-line of every case here is that a harness's own resolution is
//! not good enough: which of two same-named skills it loads, a skill fetched
//! from a URL, a symlink followed out of a package. Each of those is refused
//! on the node, before a session exists, and each refusal has a test.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use support::fake::FakeAdapter;
use tracon::{
    adapter::{claude::ClaudeAdapter, HarnessAdapter},
    broker::Broker,
    config::Config,
    http::{
        api::AppState,
        auth::{self, AuthState},
    },
    manifest,
    mcp::Tools,
    session::Manager,
    store::Store,
    stream::Bus,
};

const LOCAL: &str = "127.0.0.1:5000";
const REMOTE: &str = "203.0.113.7:5000";

struct Node {
    app: axum::Router,
    store: Arc<Store>,
}

fn node() -> Node {
    node_with(Config::default())
}

/// A node running a particular configuration. The manifest pane builds from
/// the *running* node's config rather than node.toml, so a test about
/// `[launch]` has to start a node with it rather than write the file.
fn node_with(cfg: Config) -> Node {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
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

async fn call(
    n: &Node,
    method: &str,
    uri: &str,
    peer: Option<&str>,
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
    if let Some(p) = peer {
        let addr: SocketAddr = p.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
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

/// A skill package on disk, under this test's own scratch.
fn package(dir: &Path, name: &str, body: &str) -> PathBuf {
    let package = dir.join(name);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: A test skill\n---\n\n{body}\n"),
    )
    .unwrap();
    package
}

// ---------------------------------------------------------------------------
// Through the interface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_skill_is_imported_shown_and_removed() {
    let n = node();
    let root = state::scratch("manifest-import");
    let dir = package(&root, "release-notes", "Write the notes.");

    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "channel": "work", "source": dir.to_string_lossy() })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "release-notes");
    assert_eq!(v["files"], json!(["SKILL.md"]));
    // Never silently: what the operator is told is that this is code.
    assert!(
        v["warnings"][0]
            .as_str()
            .unwrap_or_default()
            .contains("Skill content is code"),
        "{v}"
    );
    // And that nothing running changed.
    assert_eq!(v["applies_at"], "next launch");

    let (s, v) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["name"], "release-notes");
    assert_eq!(v["items"][0]["kind"], "skill");
    assert_eq!(v["next"]["skills"][0]["name"], "release-notes");
    assert!(v["next"]["digest"].as_str().is_some_and(|d| d.len() == 64));
    assert!(v["error"].is_null(), "{v}");

    // Another channel is another manifest.
    let (_, other) = call(
        &n,
        "GET",
        "/api/manifest?channel=personal",
        Some(LOCAL),
        None,
    )
    .await;
    assert_eq!(other["items"], json!([]));
    assert_ne!(other["next"]["digest"], v["next"]["digest"]);

    let (s, _) = call(
        &n,
        "DELETE",
        "/api/manifest/skill/release-notes?channel=work",
        Some(LOCAL),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(v["items"], json!([]));
}

/// Importing a skill puts executable content where a harness will run it, so
/// it is done at the node itself, like every other write that decides what
/// the node executes.
#[tokio::test]
async fn importing_a_skill_is_refused_off_the_machine() {
    let n = node();
    let root = state::scratch("manifest-remote");
    let dir = package(&root, "release-notes", "Write the notes.");
    for peer in [Some(REMOTE), None] {
        let (s, v) = call(
            &n,
            "POST",
            "/api/manifest/skills",
            peer,
            Some(json!({ "channel": "work", "source": dir.to_string_lossy() })),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{peer:?}: {v}");
    }
}

/// The sharpest of the refusals. Upstream logs a warning and overwrites, and
/// `concurrency: "unbounded"` means the winner varies between runs — so a
/// project skill can shadow a global one on Tuesday and not on Wednesday.
#[tokio::test]
async fn a_second_package_claiming_an_imported_name_is_refused() {
    let n = node();
    let root = state::scratch("manifest-duplicate");
    let first = package(&root.join("a"), "release-notes", "The real one.");
    let second = package(&root.join("b"), "release-notes", "Something else.");

    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": first.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": second.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("keeps one skill of a name"), "{message}");

    // Re-importing the *same* package is an update, not a duplicate: the
    // operator changed a file and wants the change.
    std::fs::write(
        first.join("SKILL.md"),
        "---\nname: release-notes\ndescription: A test skill\n---\n\nChanged.\n",
    )
    .unwrap();
    let (s, again) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": first.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    let (_, view) = call(&n, "GET", "/api/manifest?channel=work", Some(LOCAL), None).await;
    assert_eq!(view["items"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn a_url_source_is_refused_at_the_interface() {
    let n = node();
    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": "https://skills.example/pack", "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("never fetches a skill itself"),
        "{message}"
    );
}

/// A symlink is the only way a validated package can still name bytes outside
/// itself, and upstream's scans follow them everywhere.
#[cfg(unix)]
#[tokio::test]
async fn a_package_that_links_out_of_itself_is_refused() {
    let n = node();
    let root = state::scratch("manifest-escape");
    std::fs::write(root.join("secret"), "not the harness's").unwrap();
    let dir = package(&root, "release-notes", "Write the notes.");
    std::os::unix::fs::symlink(root.join("secret"), dir.join("leak.md")).unwrap();

    let (s, v) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("symlink"), "{message}");
}

/// `node.toml`'s version as `GET /api/config` gives it, which a save names.
async fn config_hash(n: &Node) -> String {
    let (s, v) = call(n, "GET", "/api/config", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["hash"].as_str().unwrap().to_string()
}

/// `[launch]` held the OpenCode harness's plugin approvals, and went with it:
/// the settings surface refuses it by name rather than writing a key nothing
/// reads.
#[tokio::test]
async fn the_retired_launch_settings_are_refused() {
    let n = node();
    let (s, v) = call(
        &n,
        "PUT",
        "/api/config",
        Some(LOCAL),
        Some(json!({
            "launch": { "plugins": ["@example/audit@1.0.0"] },
            "expected_hash": config_hash(&n).await,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("launch"), "{message}");
}

// ---------------------------------------------------------------------------
// What a launch renders
// ---------------------------------------------------------------------------

fn wiring_with(manifest: manifest::LaunchManifest) -> tracon::gateway::model::Wiring {
    let mut cfg = Config::default();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic".into(),
        tracon::config::Provider {
            credential: "anthropic".into(),
            upstream: "https://api.anthropic.com".into(),
            shape: tracon::config::SHAPE_ANTHROPIC.into(),
            models: vec![tracon::config::ModelDecl {
                id: "claude-x".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let mut wiring =
        tracon::gateway::model::harness_wiring(&cfg, "tracon-gw", "session-token", |_, _| true);
    wiring.manifest = manifest;
    wiring
}

fn built(store: &Store, channel: &str) -> manifest::LaunchManifest {
    let (skills, instructions, agents) = store.manifest_contents(channel).unwrap();
    manifest::build(manifest::Inputs {
        channel,
        skills,
        instructions,
        agents,
        providers: vec!["anthropic".into()],
        policy_revision: "1".into(),
    })
    .unwrap()
}

fn rendered(wiring: &tracon::gateway::model::Wiring) -> Vec<(String, String)> {
    ClaudeAdapter::new(ClaudeAdapter::PINNED_VERSION).scratch_files(wiring)
}

/// Where a skill is staged: inside the plugin the session's harness loads,
/// sealed read-only.
const STAGED: &str = "plugin/skills/release-notes/SKILL.md";

/// The skill packages are staged, one per skill, inside the plugin the
/// harness is pointed at, and nothing names a URL.
#[tokio::test]
async fn the_rendered_plugin_stages_the_skills_and_names_no_urls() {
    let n = node();
    let root = state::scratch("manifest-render");
    let dir = package(&root, "release-notes", "Write the notes.");
    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let wiring = wiring_with(built(&n.store, "work"));
    let files = rendered(&wiring);
    let plugin: Value = serde_json::from_str(
        &files
            .iter()
            .find(|(name, _)| name == "plugin/.claude-plugin/plugin.json")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(plugin["name"], "tracon", "{plugin}");
    assert!(
        files
            .iter()
            .all(|(_, body)| !body.contains("http://") && !body.contains("https://")),
        "{files:?}"
    );

    // The package itself is staged in the sealed directory, one per skill.
    assert!(
        files
            .iter()
            .any(|(name, body)| name == STAGED && body.contains("Write the notes.")),
        "{:?}",
        files.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

/// A new revision is for the next launch. The files a running session was
/// staged from are already on disk and in its recorded digest; editing the
/// manifest cannot reach either.
#[tokio::test]
async fn a_later_revision_does_not_alter_what_a_running_session_was_staged_from() {
    let n = node();
    let root = state::scratch("manifest-revision");
    let dir = package(&root, "release-notes", "The first version.");
    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // What a launch would stage, and the revision it would record.
    let first = n.store.manifest_record(&built(&n.store, "work")).unwrap();
    assert_eq!(first.revision, 1);
    let staged = rendered(&wiring_with(first.clone()));

    // The operator edits the package and re-imports it.
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: release-notes\ndescription: A test skill\n---\n\nThe second version.\n",
    )
    .unwrap();
    let (s, _) = call(
        &n,
        "POST",
        "/api/manifest/skills",
        Some(LOCAL),
        Some(json!({ "source": dir.to_string_lossy(), "channel": "work" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let second = n.store.manifest_record(&built(&n.store, "work")).unwrap();
    assert_eq!(second.revision, 2);
    assert_ne!(second.digest, first.digest);

    // The running session's files are unchanged — they are a function of the
    // manifest it holds, not of the store.
    assert_eq!(staged, rendered(&wiring_with(first.clone())));
    assert!(staged
        .iter()
        .any(|(name, body)| name == STAGED && body.contains("The first version.")));

    // And the digest a session recorded still resolves to those bytes.
    let looked_up = n.store.manifest_by_digest(&first.digest).unwrap().unwrap();
    assert_eq!(looked_up.revision, 1);
    assert!(looked_up.skills[0].files[0]
        .text
        .contains("The first version."));

    // What the next launch would stage is the second version.
    assert!(rendered(&wiring_with(second))
        .iter()
        .any(|(name, body)| name == STAGED && body.contains("The second version.")));
}

/// A session carries the digest it launched under, written once.
#[tokio::test]
async fn a_session_records_the_manifest_it_launched_under() {
    let n = node();
    let mut row = support::rows::session_row("s-manifest", "n1", "work");
    row.state = "running".into();
    n.store
        .put_node(&support::rows::node_row("n1", "n"))
        .unwrap();
    n.store.insert_session(&row).unwrap();
    assert!(n
        .store
        .get_session("s-manifest")
        .unwrap()
        .unwrap()
        .manifest_digest
        .is_none());

    let manifest = n.store.manifest_record(&built(&n.store, "work")).unwrap();
    n.store
        .update_session(
            "s-manifest",
            tracon::store::SessionPatch {
                manifest_digest: Some(manifest.digest.clone()),
                ..Default::default()
            },
        )
        .unwrap();

    let (s, v) = call(&n, "GET", "/api/sessions/s-manifest", Some(LOCAL), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["session"]["manifest_digest"], manifest.digest);
}
