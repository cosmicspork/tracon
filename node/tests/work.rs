//! The work ledger through its surfaces: the operator API the interface and
//! the CLI call, and the tools a session calls. Readiness is derived, ids
//! are hashes, discovered work links back to its origin, and events inherit
//! the session's item.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    broker::Broker,
    config::Config,
    http::api::AppState,
    mcp::Tools,
    session::Manager,
    store::{now_ms, NewEvent, SessionRow, Store},
    stream::Bus,
};

#[path = "support/fake.rs"]
mod fake;
use fake::FakeAdapter;

struct H {
    harness: axum::Router,
    operator: axum::Router,
    store: Arc<Store>,
    manager: Manager,
}

async fn harness() -> H {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
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
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    let state = AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
    };
    H {
        harness: tracon::http::harness_router(state.clone()),
        operator: tracon::http::router(state),
        store,
        manager,
    }
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

async fn mcp(app: &axum::Router, sid: &str, token: &str, name: &str, args: Value) -> Value {
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{sid}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text, "error": v["result"]["isError"] }))
}

fn session_row(id: &str, item: Option<&str>) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: item.map(str::to_string),
        repo_path: "/tmp/repo".into(),
        worktree_path: None,
        branch: "feat/x".into(),
        harness_id: "fake".into(),
        harness_version: "1.0.0".into(),
        harness_session_id: None,
        container_name: None,
        model: "m/a".into(),
        project_id: None,
        budget_tokens: 1000,
        tokens_used: 0,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: "running".into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms: now_ms(),
        started_mono_ms: Some(0),
        ended_mono_ms: None,
        updated_ms: now_ms(),
    }
}

fn ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn readiness_is_derived_from_deps_and_sessions_and_the_order_is_stable() {
    let h = harness().await;
    let (st, a) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Design the schema", "priority": 1})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{a}");
    let a = a["id"].as_str().unwrap().to_string();
    assert_eq!(a.len(), 64, "hash id");
    let (_, b) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Migrate", "deps": [a], "priority": 9})),
    )
    .await;
    let b = b["id"].as_str().unwrap().to_string();
    let (_, c) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Docs", "priority": 5})),
    )
    .await;
    let c = c["id"].as_str().unwrap().to_string();
    let (st, _) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "  "})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Ready: c (p5) before a (p1); b waits on a.
    let (_, ready) = call(&h.operator, "GET", "/api/work/ready?channel=personal", None).await;
    assert_eq!(ids(&ready), vec![c.clone(), a.clone()]);
    let (_, all) = call(&h.operator, "GET", "/api/work?channel=personal", None).await;
    let bv = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == b)
        .unwrap();
    assert_eq!(bv["readiness"]["state"], "blocked");
    assert_eq!(bv["readiness"]["by"][0]["id"], a);

    // A session holding c takes it off the ready list without changing readiness.
    h.store
        .insert_session(&session_row("s-c", Some(&c)))
        .unwrap();
    let (_, ready) = call(&h.operator, "GET", "/api/work/ready?channel=personal", None).await;
    assert_eq!(ids(&ready), vec![a.clone()]);
    let (_, one) = call(&h.operator, "GET", &format!("/api/work/{c}"), None).await;
    assert_eq!(one["item"]["session_id"], "s-c");
    assert_eq!(one["sessions"][0]["id"], "s-c");

    // Closing a frees b, with the highest priority first.
    let (st, closed) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{a}"),
        Some(json!({"state": "closed"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{closed}");
    assert_eq!(closed["state"], "closed");
    let (_, ready) = call(&h.operator, "GET", "/api/work/ready?channel=personal", None).await;
    assert_eq!(ids(&ready), vec![b.clone()]);
    let (_, closed_list) = call(
        &h.operator,
        "GET",
        "/api/work?channel=personal&state=closed",
        None,
    )
    .await;
    assert_eq!(ids(&closed_list), vec![a.clone()]);

    // Events on a session inherit its item without the caller saying so.
    h.store
        .append_event(&NewEvent {
            session_id: "s-c".into(),
            work_item_id: None,
            kind: "message".into(),
            ref_id: None,
            payload: json!({}),
            at_ms: now_ms(),
            mono_ms: 1,
        })
        .unwrap();
    let e = h.store.events_after("s-c", 0, 10).unwrap().pop().unwrap();
    assert_eq!(e.work_item_id.as_deref(), Some(c.as_str()));

    // Delete tombstones.
    let (st, _) = call(&h.operator, "DELETE", &format!("/api/work/{b}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(&h.operator, "GET", &format!("/api/work/{b}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_agent_sees_ready_work_and_discovered_work_links_to_its_item() {
    let h = harness().await;
    let (_, parent) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Parent"})),
    )
    .await;
    let parent = parent["id"].as_str().unwrap().to_string();
    let (_, other) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({"channel": "personal", "title": "Other", "priority": 3})),
    )
    .await;
    let other = other["id"].as_str().unwrap().to_string();
    h.store
        .insert_session(&session_row("s1", Some(&parent)))
        .unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;

    let v = mcp(&h.harness, "s1", &token, "work_ready", json!({})).await;
    assert_eq!(ids(&v), vec![other.clone()], "{v}");

    let v = mcp(
        &h.harness,
        "s1",
        &token,
        "work_discover",
        json!({"title": "Found a flaky test", "body": "see log", "deps": [other]}),
    )
    .await;
    assert_eq!(v["discovered_from"], parent, "{v}");
    let found = v["id"].as_str().unwrap().to_string();
    let (_, one) = call(&h.operator, "GET", &format!("/api/work/{found}"), None).await;
    assert_eq!(one["item"]["discovered_by_session"], "s1");
    assert_eq!(one["item"]["readiness"]["state"], "blocked");
    let (_, p) = call(&h.operator, "GET", &format!("/api/work/{parent}"), None).await;
    assert_eq!(p["discovered"][0]["id"], found);

    // Closing from the agent closes the session's own item, recorded by session.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        "work_close",
        json!({"summary": "done"}),
    )
    .await;
    assert_eq!(v["state"], "closed", "{v}");
    let item = h.store.work_get(&parent).unwrap().unwrap();
    assert_eq!(item.closed_by_session.as_deref(), Some("s1"));
    let kinds: Vec<String> = h
        .store
        .events_after("s1", 0, 50)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"work_closed".to_string()), "{kinds:?}");

    // A session with no item cannot close anything.
    h.store.insert_session(&session_row("s2", None)).unwrap();
    let t2 = h
        .manager
        .register_tool_token_for_test("s2", "personal")
        .await;
    let v = mcp(&h.harness, "s2", &t2, "work_close", json!({})).await;
    assert_eq!(v["error"], true, "{v}");
}
