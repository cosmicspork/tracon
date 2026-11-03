//! A session's harness follows the credential its model spends: an Anthropic
//! credential runs on Claude Code, and every other provider on tracon-agent.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    broker::Broker,
    config::{Config, Provider, SHAPE_ANTHROPIC, SHAPE_OPENAI},
    http::api::AppState,
    session::{Manager, NewSession, Phase},
    store::{now_ms, NodeRow, Store},
    stream::Bus,
};

use support::fake::NamedFake;

const BROKER: &str = r#"
    [credentials.sub]
    kind = "oauth"
    provider = "anthropic"
    channels = ["personal"]
    [credentials.sub.env]
    ACCESS_TOKEN = "at"
    REFRESH_TOKEN = "rt"

    [credentials.key]
    kind = "api_key"
    provider = "anthropic-key"
    channels = ["personal"]
    [credentials.key.env]
    API_KEY = "k"

    [credentials.oai]
    kind = "api_key"
    provider = "openai"
    channels = ["personal"]
    [credentials.oai.env]
    API_KEY = "k"
"#;

struct Node {
    repo: String,
    app: axum::Router,
    store: Arc<Store>,
    manager: Manager,
}

fn provider(credential: &str, shape: &str) -> Provider {
    Provider {
        credential: credential.into(),
        upstream: "https://example.com".into(),
        shape: shape.into(),
        login: None,
        price: None,
        models: Vec::new(),
    }
}

/// A committed repository for sessions to start from.
/// A forge repository the node has cloned, named the way a session names
/// it: by its identity.
fn repo() -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let owner = format!("harness-choice-{n}");
    state::managed_clone(&owner);
    format!("github.com/{owner}/repo")
}

async fn node(default: &'static str) -> Node {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "test".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: default.into(),
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
        })
        .unwrap();
    let mut cfg = Config::default();
    cfg.harness.id = default.into();
    cfg.providers.clear();
    cfg.providers
        .insert("anthropic".into(), provider("sub", SHAPE_ANTHROPIC));
    cfg.providers
        .insert("anthropic-key".into(), provider("key", SHAPE_ANTHROPIC));
    cfg.providers
        .insert("openai".into(), provider("oai", SHAPE_OPENAI));
    // Configured, but its credential is gone: nothing on it is offered.
    cfg.providers
        .insert("retired".into(), provider("gone", SHAPE_OPENAI));
    let cfg = Arc::new(cfg);
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
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let app = tracon::http::router(AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(NamedFake::new(default)),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    });
    for id in tracon::adapter::KNOWN {
        if *id != default {
            manager.set_adapter(Arc::new(NamedFake::new(id)));
        }
    }
    Node {
        repo: repo(),
        app,
        store,
        manager,
    }
}

impl Node {
    async fn start(&self, model: &str, harness: Option<&str>) -> (StatusCode, Value) {
        let mut body = json!({
            "channel": "personal",
            "repo_path": self.repo,
            "model": model,
        });
        if let Some(harness) = harness {
            body["harness"] = json!(harness);
        }
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// An ended session on `model` and `harness`, to reopen.
    fn ended(&self, id: &str, model: &str, harness: &str) {
        let mut row = support::rows::session_row(id, "n1", "personal");
        row.repo_path = self.repo.clone();
        row.model = model.into();
        row.harness_id = harness.into();
        row.state = "closed".into();
        self.store.insert_session(&row).unwrap();
    }

    async fn reopen(&self, id: &str, body: Value) -> (StatusCode, Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/sessions/{id}/reopen"))
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Where `session.started` says the harness came from, once it has.
    async fn harness_source(&self, id: &str) -> Option<String> {
        for _ in 0..300 {
            let started = self
                .store
                .events_after(id, 0, 100)
                .unwrap()
                .into_iter()
                .find(|e| e.kind == "session_started");
            if let Some(event) = started {
                return event.payload["harness_source"].as_str().map(str::to_string);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }
}

fn spec(model: &str, harness: Option<&str>) -> NewSession {
    NewSession {
        channel: "personal".into(),
        repo_path: "/nonexistent/repo".into(),
        branch: None,
        base: None,
        work_item_id: None,
        model: model.into(),
        budget_tokens: None,
        initial_prompt: None,
        node_id: None,
        phase: Phase::Execute,
        review_id: None,
        base_sha: None,
        workspace_id: None,
        parent_session: None,
        continued_from: None,
        harness: harness.map(str::to_string),
        on_exhaustion: None,
        told_reviews: Vec::new(),
    }
}

#[tokio::test]
async fn the_credential_picks_the_harness_whatever_the_node_default() {
    state::isolate();
    for default in ["native", "claude"] {
        let node = node(default).await;
        for (model, harness, source) in [
            ("anthropic/claude-x", "claude", "credential"),
            ("openai/gpt-x", "native", "credential"),
            ("anthropic-key/claude-x", "claude", "credential"),
        ] {
            let (status, body) = node.start(model, None).await;
            assert_eq!(status, StatusCode::CREATED, "{default}, {model}: {body}");
            assert_eq!(body["harness_id"], harness, "{default}, {model}");
            let id = body["id"].as_str().unwrap();
            assert_eq!(
                node.harness_source(id).await.as_deref(),
                Some(source),
                "{default}, {model}"
            );
        }
    }
}

#[tokio::test]
async fn naming_a_harness_the_credential_cannot_run_on_is_refused() {
    state::isolate();
    let node = node("native").await;
    for (model, harness, reason) in [
        ("anthropic/claude-x", "native", "extra usage"),
        (
            "anthropic-key/claude-x",
            "native",
            "does not run Anthropic models",
        ),
        ("openai/gpt-x", "claude", "Anthropic models only"),
    ] {
        let refused = node
            .manager
            .preflight(&spec(model, Some(harness)))
            .expect_err("preflight refuses it")
            .to_string();
        assert!(refused.contains(reason), "{refused}");
        let (status, body) = node.start(model, Some(harness)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["message"], refused);
    }
    assert!(node.store.list_sessions(None).unwrap().is_empty());

    // Named and compatible: the harness asked for runs it.
    let (status, body) = node.start("anthropic-key/claude-x", Some("claude")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["harness_id"], "claude");
}

#[tokio::test]
async fn the_picker_learns_which_harnesses_each_model_runs_on() {
    state::isolate();
    let node = node("native").await;
    for (model, harnesses) in [
        ("anthropic/claude-x", vec!["claude"]),
        ("anthropic-key/claude-x", vec!["claude"]),
        ("openai/gpt-x", vec!["native"]),
        ("sonnet", vec!["claude"]),
        ("elsewhere/m", vec![]),
    ] {
        assert_eq!(node.manager.model_harnesses(model), harnesses, "{model}");
    }
}

/// Planning through compose takes the Adjust panel's harness too.
#[tokio::test]
async fn compose_runs_on_the_harness_the_operator_chose() {
    state::isolate();
    let node = node("native").await;
    let response = node
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/compose")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "channel": "personal",
                        "title": "try it on Claude Code",
                        "repo_path": node.repo,
                        "phase": "plan",
                        "model": "anthropic-key/claude-x",
                        "harness": "claude",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["session"]["harness_id"], "claude");
}

/// Each harness's probe gates the sessions on that harness, not the others.
#[tokio::test]
async fn a_mismatched_harness_blocks_only_its_own_sessions() {
    state::isolate();
    let node = node("native").await;
    let mut row = node.store.get_node("n1").unwrap().unwrap();
    row.harnesses_json = Some(
        json!([
            {"id": "native", "pinned": "1.0.0", "found": "1.0.0", "default": true},
            {"id": "claude", "pinned": "2.0.0", "found": "1.9.0"},
        ])
        .to_string(),
    );
    node.store.put_node(&row).unwrap();

    let (status, body) = node.start("anthropic/claude-x", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("1.9.0") && message.contains("2.0.0"),
        "{message}"
    );

    let (status, body) = node.start("openai/gpt-x", None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["harness_id"], "native");
}

/// A reopened session runs on the model the old one ran, so a session on a
/// non-Anthropic provider reopens on `native` rather than on whatever the
/// node's default model is.
#[tokio::test]
async fn reopening_keeps_the_sessions_model() {
    state::isolate();
    let node = node("claude").await;
    node.ended("old-1", "openai/gpt-x", "native");

    let (status, body) = node.reopen("old-1", json!({ "harness": "native" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["model"], "openai/gpt-x");
    assert_eq!(body["harness_id"], "native");
    assert_eq!(body["continued_from"], "old-1");
}

/// A model named on the reopen replaces the old one's, and the harness is
/// checked against it.
#[tokio::test]
async fn reopening_on_another_model_runs_that_model() {
    state::isolate();
    let node = node("native").await;
    node.ended("old-1", "openai/gpt-x", "native");

    let (status, body) = node
        .reopen(
            "old-1",
            json!({ "harness": "claude", "model": "anthropic/claude-x" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["model"], "anthropic/claude-x");
    assert_eq!(body["harness_id"], "claude");
}

/// A pairing session start refuses, reopen refuses in the same words, whether
/// the model is the old session's or one named on the reopen.
#[tokio::test]
async fn reopening_on_a_harness_the_model_cannot_run_on_is_refused_as_a_start_is() {
    state::isolate();
    let node = node("native").await;
    node.ended("old-1", "anthropic/claude-x", "claude");
    node.ended("old-2", "openai/gpt-x", "native");

    let (status, started) = node.start("anthropic/claude-x", Some("native")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{started}");
    let refusal = &started["error"]["message"];
    assert!(
        refusal.as_str().unwrap().contains("Claude Code only"),
        "{refusal}"
    );
    for (id, body) in [
        ("old-1", json!({ "harness": "native" })),
        (
            "old-2",
            json!({ "harness": "native", "model": "anthropic/claude-x" }),
        ),
    ] {
        let (status, reopened) = node.reopen(id, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{id}: {reopened}");
        assert_eq!(&reopened["error"]["message"], refusal, "{id}");
    }
    assert!(
        !node
            .store
            .list_sessions(None)
            .unwrap()
            .iter()
            .any(|s| s.continued_from.is_some()),
        "nothing was started"
    );
}

/// An old model no longer offered is refused by name, not swapped for the
/// node's default; naming one that is offered carries the work on.
#[tokio::test]
async fn reopening_a_session_whose_model_is_gone_names_it() {
    state::isolate();
    let node = node("native").await;
    node.ended("old-1", "retired/m", "native");

    let (status, body) = node.reopen("old-1", json!({ "harness": "native" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("retired/m") && message.contains("no longer offers"),
        "{message}"
    );

    let (status, body) = node
        .reopen(
            "old-1",
            json!({ "harness": "native", "model": "openai/gpt-x" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["model"], "openai/gpt-x");
}
