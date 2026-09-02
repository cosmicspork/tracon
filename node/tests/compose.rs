//! Starting work from a prompt: one call writes the item and starts the
//! session on it. What matters here is the refusal path — the operator's words
//! are the only copy, so a session that will not start must not take them
//! with it.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

use tracon::{
    config::Config,
    http::api::AppState,
    session::Manager,
    store::{now_ms, NodeRow, Store},
    stream::Bus,
};

use support::fake::FakeAdapter;

struct Harness {
    app: axum::Router,
}

impl Harness {
    async fn new() -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_node(&NodeRow {
                id: "n1".into(),
                name: "test".into(),
                state: "ready".into(),
                failed_check: None,
                failed_detail: None,
                harness_id: "fake".into(),
                harness_pinned: "1.0.0".into(),
                harness_found: Some("1.0.0".into()),
                models_json: Some(r#"[{"value":"m/a","name":"A"}]"#.into()),
                checked_at_ms: Some(now_ms()),
                is_self: 1,
                x25519_pub: None,
                last_seen_ms: None,
                reachable: 1,
                providers_json: None,
            })
            .unwrap();
        let adapter = Arc::new(FakeAdapter {
            tx: Arc::new(Mutex::new(None)),
            tokens: Arc::new(Mutex::new(100)),
        });
        let mut cfg = Config::default();
        cfg.session.budget_tokens = 1000;
        let cfg = Arc::new(cfg);
        let tools = Arc::new(tracon::mcp::Tools {
            broker: Arc::new(Default::default()),
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
        let app = tracon::http::router(AppState {
            manager,
            cfg,
            adapter,
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        });
        Self { app }
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(uri);
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

#[tokio::test]
async fn a_prompt_writes_the_item_and_starts_the_session() {
    state::isolate();
    let h = Harness::new().await;
    let (st, body) = h
        .call(
            "POST",
            "/api/compose",
            Some(json!({
                "channel": "personal",
                "title": "Archive ended sessions from the home",
                "body": "in bulk and per row",
                "repo_path": "/nonexistent/repo",
                "model": "m/a",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["work"]["title"],
        "Archive ended sessions from the home"
    );
    assert_eq!(body["session"]["work_item_id"], body["work"]["id"]);
    // Plan is the phase a prompt starts in, without saying so.
    assert_eq!(body["session"]["phase"], "plan");
    // The item is in the ledger, held by its session.
    let (st, item) = h
        .call(
            "GET",
            &format!("/api/work/{}", body["work"]["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{item}");
    assert_eq!(item["item"]["body"], "in bulk and per row");
}

#[tokio::test]
async fn a_refused_session_keeps_the_item_and_says_where_it_went() {
    state::isolate();
    let h = Harness::new().await;
    // No model, and no binding to supply one.
    let (st, body) = h
        .call(
            "POST",
            "/api/compose",
            Some(json!({
                "channel": "personal",
                "title": "Something worth keeping",
                "repo_path": "/nonexistent/repo",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no model"));
    let id = body["work_item_id"]
        .as_str()
        .expect("the refusal names the item it saved");
    let (st, item) = h.call("GET", &format!("/api/work/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{item}");
    assert_eq!(item["item"]["title"], "Something worth keeping");
}

#[tokio::test]
async fn a_prompt_takes_the_channel_s_bound_model() {
    state::isolate();
    let h = Harness::new().await;
    let (st, body) = h
        .call("POST", "/api/channels", Some(json!({ "name": "personal" })))
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = h
        .call(
            "PUT",
            "/api/channels/personal/bindings",
            Some(json!({ "phases.plan.model": "m/plan" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = h
        .call(
            "POST",
            "/api/compose",
            Some(json!({
                "channel": "personal",
                "title": "No model named here",
                "repo_path": "/nonexistent/repo",
            })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["session"]["model"], "m/plan");
}
