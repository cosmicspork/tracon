//! The nightly batch through the operator's surfaces: candidates become a
//! batch, the batch waits in the queue, verdicts settle each memory, and a
//! promoted lesson is context from then on.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    broker::Broker, config::Config, http::api::AppState, mcp::Tools, session::Manager,
    store::Store, stream::Bus,
};

#[path = "support/fake.rs"]
mod fake;
use fake::FakeAdapter;

async fn operator() -> (axum::Router, Arc<Store>) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let cfg = Arc::new(Config::default());
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
    };
    (tracon::http::router(state), store)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420");
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let req = b
        .body(match body {
            Some(v) => Body::from(v.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn candidates_are_batched_decided_and_only_promoted_ones_become_context() {
    let (app, store) = operator().await;
    for (body, state) in [
        ("flaky tests hide behind retries", "candidate"),
        ("the deploy needs the VPN", "candidate"),
        ("already active fact", "active"),
    ] {
        let (st, _) = call(&app, "POST", "/api/memories", Some(json!({"channel": "personal", "kind": "lesson", "body": body, "state": state, "confidence": 0.8}))).await;
        assert_eq!(st, StatusCode::OK);
    }
    // Nothing waits yet; the batch is nightly, but the operator can ask now.
    let (_, q) = call(&app, "GET", "/api/queue", None).await;
    assert_eq!(q["promotions"].as_array().unwrap().len(), 0);
    let (st, v) = call(&app, "POST", "/api/promotions/batch", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ids = v["created"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    let pid = ids[0].as_str().unwrap().to_string();
    // A second run finds nothing new: the candidates are now proposed.
    let (_, v) = call(&app, "POST", "/api/promotions/batch", None).await;
    assert!(v["created"].as_array().unwrap().is_empty());

    let (_, q) = call(&app, "GET", "/api/queue", None).await;
    assert_eq!(q["promotions"][0]["id"], pid);
    let (st, p) = call(&app, "GET", &format!("/api/promotions/{pid}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let items = p["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "the active memory is not in the batch");
    let (flaky, vpn) = (
        items[0]["memory_id"].as_str().unwrap().to_string(),
        items[1]["memory_id"].as_str().unwrap().to_string(),
    );
    // Neither is recalled while proposed.
    let (_, r) = call(
        &app,
        "GET",
        "/api/memories?channel=personal&q=flaky%20tests",
        None,
    )
    .await;
    assert!(r["hits"].as_array().unwrap().is_empty(), "{r}");

    // One verdict at a time: the batch stays open until every item is decided.
    let (st, v) = call(
        &app,
        "POST",
        &format!("/api/promotions/{pid}/verdict"),
        Some(json!({"verdicts": {flaky.clone(): "promote"}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "open");
    let (_, q) = call(&app, "GET", "/api/queue", None).await;
    assert_eq!(q["promotions"].as_array().unwrap().len(), 1);
    let (_, v) = call(
        &app,
        "POST",
        &format!("/api/promotions/{pid}/verdict"),
        Some(json!({"verdicts": {vpn.clone(): "reject"}})),
    )
    .await;
    assert_eq!(v["state"], "decided");
    let (_, q) = call(&app, "GET", "/api/queue", None).await;
    assert!(q["promotions"].as_array().unwrap().is_empty());
    assert_eq!(store.memory_get(&flaky).unwrap().unwrap().state, "promoted");
    assert_eq!(store.memory_get(&vpn).unwrap().unwrap().state, "rejected");
    // Promoted lessons are context; rejected ones never are.
    let (_, r) = call(
        &app,
        "GET",
        "/api/memories?channel=personal&q=flaky%20tests",
        None,
    )
    .await;
    assert_eq!(r["hits"][0]["id"], flaky);
    let (_, r) = call(&app, "GET", "/api/memories?channel=personal&q=VPN", None).await;
    assert!(r["hits"].as_array().unwrap().is_empty());
    // A decided batch takes no more verdicts.
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/promotions/{pid}/verdict"),
        Some(json!({"verdicts": {vpn: "promote"}})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}
