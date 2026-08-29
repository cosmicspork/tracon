//! Memory and documents through the surfaces that use them: the MCP tools a
//! session calls, and the operator API the interface and the CLI call.

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
#[path = "support/state.rs"]
mod state;
use fake::FakeAdapter;

struct H {
    harness: axum::Router,
    operator: axum::Router,
    store: Arc<Store>,
    manager: Manager,
}

async fn harness() -> H {
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
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
    };
    H {
        harness: tracon::http::harness_router(state.clone()),
        operator: tracon::http::router(state),
        store,
        manager,
    }
}

async fn mcp(app: &axum::Router, sid: &str, token: &str, body: Value) -> Value {
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
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    if_match: Option<&str>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:7420");
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    if let Some(h) = if_match {
        b = b.header("if-match", h);
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

fn tool_call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

fn text(v: &Value) -> String {
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn every_session_is_offered_memory_and_document_tools() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in ["recall", "retain", "doc_read", "doc_search", "doc_write"] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    assert!(!names.contains(&"query"), "no credential, no consulta");
}

#[tokio::test]
async fn retain_then_recall_round_trips_and_a_lesson_waits_for_the_batch() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    // A session with no project identity retains globally.
    let v = mcp(&h.harness, "s1", &token, tool_call(1, "retain", json!({"kind": "fact", "scope": "global", "body": "the test command is just test", "confidence": 0.95}))).await;
    assert_eq!(v["result"]["isError"], false, "{v}");
    assert!(text(&v).contains("active"));
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            2,
            "retain",
            json!({"kind": "lesson", "scope": "global", "body": "flaky tests hide behind retries"}),
        ),
    )
    .await;
    assert!(text(&v).contains("candidate"), "{v}");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            3,
            "retain",
            json!({"kind": "directive", "scope": "global", "body": "no"}),
        ),
    )
    .await;
    assert_eq!(
        v["result"]["isError"], true,
        "directives are the operator's"
    );
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            4,
            "retain",
            json!({"kind": "fact", "scope": "project", "body": "x"}),
        ),
    )
    .await;
    assert_eq!(
        v["result"]["isError"], true,
        "no project identity on this session"
    );

    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(5, "recall", json!({"query": "how do I run the tests?"})),
    )
    .await;
    let hits: Vec<Value> = serde_json::from_str::<Value>(&text(&v)).unwrap()["hits"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the candidate lesson is not context: {hits:?}"
    );
    assert_eq!(hits[0]["kind"], "fact");
    // And the operator sees both, with the lesson held.
    let (_, v) = call(
        &h.operator,
        "GET",
        "/api/memories?channel=personal",
        None,
        None,
    )
    .await;
    let states: Vec<&str> = v["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["state"].as_str().unwrap())
        .collect();
    assert!(states.contains(&"active") && states.contains(&"candidate"));
    // A directive from the operator ranks first.
    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/memories",
        Some(json!({"channel": "personal", "body": "run just test before every commit"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(6, "recall", json!({"query": "test"})),
    )
    .await;
    let hits: Vec<Value> = serde_json::from_str::<Value>(&text(&v)).unwrap()["hits"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(hits[0]["kind"], "directive");
    assert_eq!(hits[1]["kind"], "fact");
}

#[tokio::test]
async fn documents_are_written_by_the_operator_read_by_the_agent_and_edits_conflict_honestly() {
    state::isolate();
    let h = harness().await;
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;
    let (st, v) = call(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-workspace",
        Some(json!({"body": "# Workspace\n\nRun `just test`."})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hash = v["hash"].as_str().unwrap().to_string();
    assert_eq!(v["kind"], "guide");
    assert_eq!(v["title"], "Workspace");
    // Bad slugs are refused.
    let (st, _) = call(
        &h.operator,
        "PUT",
        "/api/docs/personal/Bad%20Slug",
        Some(json!({"body": "x"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(1, "doc_search", json!({"query": "just test"})),
    )
    .await;
    let hits = serde_json::from_str::<Value>(&text(&v)).unwrap();
    assert_eq!(hits["hits"][0]["slug"], "guide-workspace");
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(2, "doc_read", json!({"slug": "guide-workspace"})),
    )
    .await;
    let doc = serde_json::from_str::<Value>(&text(&v)).unwrap();
    assert_eq!(doc["hash"], hash);
    assert!(doc["body"].as_str().unwrap().contains("just test"));
    // doc_write is not named by the bundle: it is asked, and with no live
    // session to carry the question it is refused rather than run.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        tool_call(
            3,
            "doc_write",
            json!({"slug": "guide-workspace", "body": "gone"}),
        ),
    )
    .await;
    assert_eq!(v["result"]["isError"], true, "{v}");
    assert_eq!(
        h.store
            .doc_get("personal", "guide-workspace")
            .unwrap()
            .unwrap()
            .body,
        "# Workspace\n\nRun `just test`."
    );

    // An edit against a stale hash is refused with the current state.
    let (st, v) = call(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-workspace",
        Some(json!({"body": "v2"})),
        Some(&hash),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let hash2 = v["hash"].as_str().unwrap().to_string();
    let (st, v) = call(
        &h.operator,
        "PUT",
        "/api/docs/personal/guide-workspace",
        Some(json!({"body": "v3"})),
        Some(&hash),
    )
    .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    assert_eq!(v["hash"], hash2);
    assert_eq!(v["body"], "v2");
    // Same id throughout: the document evolved, it was not recreated.
    let (_, list) = call(&h.operator, "GET", "/api/docs?channel=personal", None, None).await;
    assert_eq!(list["docs"].as_array().unwrap().len(), 1);
    let (st, _) = call(
        &h.operator,
        "DELETE",
        "/api/docs/personal/guide-workspace",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &h.operator,
        "GET",
        "/api/docs/personal/guide-workspace",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
