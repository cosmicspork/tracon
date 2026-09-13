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
    state: AppState,
}

impl Harness {
    async fn new(models_json: Option<&str>) -> Self {
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
                models_json: models_json.map(Into::into),
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
        let state = AppState {
            manager,
            cfg,
            adapter,
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        };
        let app = tracon::http::router(state.clone());
        Self { app, state }
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
    let h = Harness::new(Some(r#"[{"value":"m/a","name":"A"}]"#)).await;
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
    let h = Harness::new(None).await;
    // No model, no binding to supply one, and no node catalogue to fall back to.
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
        .contains("no usable model"));
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
    let h = Harness::new(Some(r#"[{"value":"m/a","name":"A"}]"#)).await;
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

#[tokio::test]
async fn peer_compose_applies_its_scoped_work_before_the_mesh_tap_arrives() {
    use tracon::mesh::forward::CommandExecutor;

    state::isolate();
    let h = Harness::new(Some(r#"[{"value":"m/a","name":"A"}]"#)).await;
    h.state.store().ensure_peer_node("sender").unwrap();
    for node in ["sender", "n1"] {
        h.state.store().node_channel_add(node, "personal").unwrap();
    }
    let source = Store::open_in_memory().unwrap();
    let item = tracon::corpus::work::create(
        &source,
        &Bus::new(),
        "sender",
        tracon::corpus::work::NewWork {
            channel: "personal".into(),
            project_id: None,
            title: "Keep the operator's prompt".into(),
            body: "Plan the work on the selected peer.".into(),
            deps: vec![],
            priority: 0,
            discovered_from: None,
            discovered_by_session: None,
        },
    )
    .unwrap();
    let change = source
        .work_item_change("sender", "personal", &item.id)
        .unwrap()
        .unwrap();
    let command = json!({
        "op": "create",
        "spec": {
            "channel": "personal", "phase": "plan", "repo_path": ".",
            "work_item_id": item.id, "model": "m/a",
        },
        "work_item": change,
    });
    assert!(h.state.store().work_get(&item.id).unwrap().is_none());

    // An authenticated member still cannot smuggle another site's work into
    // this command; a rejected prerequisite must leave no work or session.
    let mut forged = command.clone();
    forged["work_item"]["site"] = json!("another-node");
    assert!(h
        .state
        .execute("sender", serde_json::from_value(forged).unwrap())
        .await
        .is_err());
    assert!(h.state.store().work_get(&item.id).unwrap().is_none());
    assert!(h.state.store().list_sessions(None).unwrap().is_empty());

    let session = h
        .state
        .execute("sender", serde_json::from_value(command).unwrap())
        .await
        .unwrap();
    assert_eq!(session["work_item_id"], item.id);
    assert_eq!(session["node_id"], "n1");
    let received = h.state.store().work_get(&item.id).unwrap().unwrap();
    assert_eq!(received.title, "Keep the operator's prompt");
    assert_eq!(received.body, "Plan the work on the selected peer.");
}
