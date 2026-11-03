//! Where a session runs once DigitalOcean is on: a node whose own boundary
//! is not verified asks the operator (an approval card) rather than moving
//! the session; a node at its local limit overflows to DigitalOcean on its
//! own; client work is never sent there. The DigitalOcean runtime here is
//! the provider around a stand-in for its API (`support::do_api`), and the
//! harness is the fake adapter, so what is under test is the decision, the
//! card and what the session records.

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::boundary::{Backend, BoundaryError, BoundaryReport};
use tracon::broker::Broker;
use tracon::config::{Config, Provider, SHAPE_ANTHROPIC};
use tracon::http::api::AppState;
use tracon::provider::digitalocean::listener::DoListener;
use tracon::provider::digitalocean::runtime::{self, DoRuntime};
use tracon::provider::digitalocean::templates::{Source, TemplateState};
use tracon::provider::digitalocean::{ClientSource, Owner, Settings};
use tracon::provider::requirements::{DoMode, RepoPlacement};
use tracon::runner::local::LocalBackend;
use tracon::runner::{Mount, Runner};
use tracon::session::{Manager, OperatorAnswer};
use tracon::store::approvals::PLACE_TOOL;
use tracon::store::{now_ms, NodeRow, Store};
use tracon::stream::Bus;

use support::do_api::FakeDo;
use support::fake::NamedFake;

const IMAGE: &str =
    "ghcr.io/example/tracon-do-sandbox@sha256:1111111111111111222222222222222222222222222222222222222222222222";

const BROKER: &str = r#"
    [credentials.key]
    kind = "api_key"
    provider = "anthropic-key"
    channels = ["personal"]
    [credentials.key.env]
    API_KEY = "k"
"#;

/// The node's own runtime as placement sees Podman: no sandbox runs, the
/// fake adapter never spawns.
struct PodmanLike(LocalBackend);

#[async_trait]
impl Backend for PodmanLike {
    fn kind(&self) -> &'static str {
        "podman"
    }
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        self.0.setup(cfg, rebuild).await
    }
    async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
        BoundaryReport { checks: Vec::new() }
    }
    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.0.runner(extra_mounts)
    }
    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.0.runner_for(harness_id, extra_mounts)
    }
    async fn import_volume(&self, v: &str, s: &std::path::Path) -> Result<(), BoundaryError> {
        self.0.import_volume(v, s).await
    }
    async fn export_volume(&self, v: &str, d: &std::path::Path) -> Result<(), BoundaryError> {
        self.0.export_volume(v, d).await
    }
    async fn export_workspace(&self, v: &str, d: &std::path::Path) -> Result<(), BoundaryError> {
        self.0.export_workspace(v, d).await
    }
    async fn clone_volume(&self, s: &str, d: &str) -> Result<(), BoundaryError> {
        self.0.clone_volume(s, d).await
    }
    fn harness_host(&self) -> String {
        self.0.harness_host()
    }
    fn harness_home(&self) -> String {
        self.0.harness_home()
    }
    async fn reconcile(&self, names: &[String]) {
        self.0.reconcile(names).await
    }
}

fn node_row(ready: bool) -> NodeRow {
    NodeRow {
        id: "n1".into(),
        name: "test".into(),
        state: if ready { "ready" } else { "refused" }.into(),
        failed_check: (!ready).then(|| "network_isolated".into()),
        failed_detail: (!ready).then(|| "the gateway container is not running".into()),
        harness_id: "claude".into(),
        harness_pinned: "1.0.0".into(),
        harness_found: Some("1.0.0".into()),
        models_json: None,
        checked_at_ms: Some(now_ms()),
        is_self: 1,
        x25519_pub: None,
        last_seen_ms: None,
        reachable: 1,
        providers_json: None,
        app_version: None,
        wire_contract: None,
        policy_identity: None,
        policy_sha256: None,
        policy_receipt_v1: None,
        harnesses_json: None,
    }
}

struct Node {
    app: axum::Router,
    store: Arc<Store>,
    tools: Arc<tracon::mcp::Tools>,
    _manager: Manager,
}

fn repo(owner: &str) -> String {
    state::managed_clone(owner);
    format!("github.com/{owner}/repo")
}

async fn node(fake: &FakeDo, client_repo: &str) -> Node {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.put_node(&node_row(false)).unwrap();
    let mut cfg = Config::default();
    cfg.harness.id = "claude".into();
    cfg.providers.clear();
    cfg.providers.insert(
        "anthropic-key".into(),
        Provider {
            credential: "key".into(),
            upstream: "https://example.com".into(),
            shape: SHAPE_ANTHROPIC.into(),
            login: None,
            price: None,
            models: Vec::new(),
        },
    );
    cfg.runtime.podman.sandbox_protocol = true;
    cfg.runtime.podman.sandbox_files = true;
    cfg.runtime.podman.sandbox_tunnels = true;
    cfg.placement.digitalocean = DoMode::Overflow;
    cfg.placement.local_limit = Some(1);
    cfg.placement.repo.push(RepoPlacement {
        path: client_repo.into(),
        client: true,
        ..RepoPlacement::default()
    });
    cfg.digitalocean = Settings {
        credential: "do".into(),
        api_url: fake.url.clone(),
        image: IMAGE.into(),
        listen: "127.0.0.1:0".into(),
        node_addr: "127.0.0.1:443".into(),
        ..Settings::default()
    };
    let cfg = Arc::new(cfg);

    let owner = Owner::new("tracon", "n1");
    fake.api.lock().unwrap().put_template(
        &format!("{}-1111111111111111", owner.template_stem()),
        "TEMPLATE_STATUS_READY",
    );
    let listener = DoListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        tracon::runner::protocol::node_server().unwrap(),
    )
    .await
    .unwrap();
    let client: ClientSource = {
        let fake = fake.clone();
        Arc::new(move || Ok(fake.client()))
    };
    let do_runtime = DoRuntime::with(
        client,
        cfg.digitalocean.clone(),
        Source::Given(IMAGE.into()),
        owner,
        "tracon-gw".into(),
        listener,
        Arc::new(|| None),
    )
    .unwrap();
    assert!(matches!(
        do_runtime.ensure_template(IMAGE).await,
        TemplateState::Ready { .. }
    ));
    assert!(runtime::install(do_runtime));
    assert!(runtime::enabled(&cfg));

    let broker: Broker = toml::from_str(BROKER).unwrap();
    let tools = Arc::new(tracon::mcp::Tools {
        broker: broker.shared(),
        cfg: cfg.clone(),
        policy: tracon::policy::Policy::shipped_shared(),
        http: reqwest::Client::new(),
        session: Default::default(),
        upstream: Default::default(),
    });
    let manager = Manager::new(
        store.clone(),
        Bus::new(),
        cfg.clone(),
        "n1".into(),
        tools.clone(),
        Default::default(),
        Arc::new(PodmanLike(LocalBackend)),
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    let app = tracon::http::router(AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(NamedFake::new("claude")),
        node_id: "n1".into(),
        tools: tools.clone(),
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    });
    Node {
        app,
        store,
        tools,
        _manager: manager,
    }
}

impl Node {
    async fn start(&self, repo: &str) -> String {
        let body =
            json!({"channel": "personal", "repo_path": repo, "model": "anthropic-key/claude-x"});
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    /// The `n`th event of `kind`, once there is one.
    async fn event(&self, id: &str, kind: &str, n: usize) -> Value {
        for _ in 0..500 {
            let found: Vec<Value> = self
                .store
                .events_after(id, 0, 500)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == kind)
                .map(|e| e.payload)
                .collect();
            if let Some(payload) = found.get(n) {
                return payload.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no {kind} #{n} for {id}");
    }

    /// The session's state once it has left `starting`.
    async fn settled(&self, id: &str) -> tracon::store::SessionRow {
        for _ in 0..500 {
            let row = self.store.get_session(id).unwrap().unwrap();
            if row.state != "starting" {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{id} never left starting");
    }

    async fn card(&self, id: &str) -> tracon::store::approvals::ApprovalRow {
        for _ in 0..500 {
            if let Some(card) = self
                .store
                .approvals_of_session(id)
                .unwrap()
                .into_iter()
                .find(|a| a.tool == PLACE_TOOL)
            {
                return card;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no placement card for {id}");
    }

    async fn answer(&self, card: &str, option: &str) {
        self.tools
            .answer_approval(
                card,
                OperatorAnswer {
                    option_id: option.into(),
                    arguments: None,
                    reason: Some("not on DigitalOcean today".into()),
                    notes: None,
                    arguments_hash: None,
                    by: Some("operator (test)".into()),
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placement_asks_overflows_and_keeps_client_work_home() {
    state::isolate();
    let fake = FakeDo::start().await;
    let client_repo = repo("placement-client");
    let client_path =
        tracon::forge::managed_root(&Config::state_dir()).join("github.com/placement-client/repo");
    let node = node(&fake, &client_path.to_string_lossy()).await;

    // The node's boundary is not verified: the session is made, and its
    // operator is asked whether to run it on DigitalOcean instead.
    let asked = node.start(&repo("placement-ask")).await;
    let event = node.event(&asked, "placement", 0).await;
    assert_eq!(event["decision"], "ask", "{event}");
    assert_eq!(event["offer"], "digitalocean");
    assert!(
        event["reason"]
            .as_str()
            .unwrap()
            .contains("the gateway container is not running"),
        "{event}"
    );
    let card = node.card(&asked).await;
    assert_eq!(card.state, "pending");
    let args: Value = serde_json::from_str(&card.arguments).unwrap();
    assert_eq!(args["session"], asked.as_str());
    assert_eq!(args["provider"], "digitalocean");
    // An edit is refused: the card runs this session where it asked.
    let edited = node
        .tools
        .answer_approval(
            &card.id,
            OperatorAnswer {
                option_id: "allow_once".into(),
                arguments: Some(json!({"session": "another"})),
                reason: None,
                notes: None,
                arguments_hash: None,
                by: None,
            },
        )
        .await;
    assert!(edited.is_err());
    node.answer(&card.id, "allow_once").await;
    let placed = node.event(&asked, "placement", 1).await;
    assert_eq!(placed["decision"], "place", "{placed}");
    assert_eq!(placed["provider"], "digitalocean");
    assert_eq!(placed["approval_id"], card.id.as_str());
    node.event(&asked, "session_started", 0).await;
    let settled = node.store.get_approval(&card.id).unwrap().unwrap();
    assert_eq!(settled.state, "succeeded");

    // Refused: the session ends, and says why.
    let declined = node.start(&repo("placement-decline")).await;
    let card = node.card(&declined).await;
    node.answer(&card.id, "reject_once").await;
    let row = node.settled(&declined).await;
    assert_ne!(row.state, "running");
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or("")
            .contains("not run on digitalocean"),
        "{row:?}"
    );

    // Verified again: the first session runs here, the one past
    // `local_limit` overflows on the node's own say. The session on
    // DigitalOcean does not count against the local limit.
    node.store.put_node(&node_row(true)).unwrap();
    let local = node.start(&repo("placement-local")).await;
    let here = node.event(&local, "placement", 0).await;
    assert_eq!(here["decision"], "place", "{here}");
    assert_eq!(here["provider"], "podman");
    node.event(&local, "session_started", 0).await;
    let over = node.start(&repo("placement-over")).await;
    let there = node.event(&over, "placement", 0).await;
    assert_eq!(there["decision"], "place", "{there}");
    assert_eq!(there["provider"], "digitalocean");
    assert!(node.store.approvals_of_session(&over).unwrap().is_empty());
    node.event(&over, "session_started", 0).await;

    // Client work: with this node's boundary unverified again, it is
    // refused rather than offered to DigitalOcean, whose resolver is open.
    // (At its local limit it is refused the same way: `session::placing`.)
    node.store.put_node(&node_row(false)).unwrap();
    let client = node.start(&client_repo).await;
    let refused = node.event(&client, "placement", 0).await;
    assert_eq!(refused["decision"], "refuse", "{refused}");
    assert_eq!(refused["client"], true);
    let reason = refused["reason"].as_str().unwrap();
    assert!(reason.contains("dns is provider-open"), "{refused}");
    assert!(
        reason.contains("the gateway container is not running"),
        "{refused}"
    );
    assert!(node.store.approvals_of_session(&client).unwrap().is_empty());
    let row = node.settled(&client).await;
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or("")
            .contains("no sandbox provider can take this session"),
        "{row:?}"
    );
}
