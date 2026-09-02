//! Putting ended sessions away. Archiving is presentation: the row keeps its
//! state, its tokens, and its place in the history — the home just stops
//! listing it.

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
    store::{now_ms, SessionRow, Store},
    stream::Bus,
};

use support::fake::FakeAdapter;

fn session_row(id: &str, state: &str, created_ms: i64) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: None,
        repo_path: "/src/p".into(),
        worktree_path: None,
        branch: format!("feat/{id}"),
        harness_id: "fake".into(),
        harness_version: "1.0.0".into(),
        harness_session_id: None,
        container_name: None,
        model: "m/a".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 0,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: state.into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms,
        started_mono_ms: None,
        ended_mono_ms: None,
        updated_ms: created_ms,
        archived_ms: None,
    }
}

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
}

impl Harness {
    async fn new() -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        // A session's node_id is a foreign key.
        store
            .put_node(&tracon::store::NodeRow {
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
        let cfg = Arc::new(Config::default());
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
            adapter: Arc::new(FakeAdapter {
                tx: Arc::new(Mutex::new(None)),
                tokens: Arc::new(Mutex::new(100)),
            }),
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        });
        Self { app, store }
    }

    async fn call(&self, method: &str, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
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

    async fn ended_ids(&self) -> Vec<String> {
        let (_, q) = self.call("GET", "/api/queue").await;
        q["ended"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect()
    }
}

#[tokio::test]
async fn an_archived_session_leaves_the_home_and_can_come_back() {
    state::isolate();
    let h = Harness::new().await;
    let t = now_ms();
    h.store
        .insert_session(&session_row("s1", "closed", t))
        .unwrap();
    h.store
        .insert_session(&session_row("s2", "failed", t - 1))
        .unwrap();
    assert_eq!(h.ended_ids().await, vec!["s1", "s2"]);

    let (st, row) = h.call("POST", "/api/sessions/s1/archive").await;
    assert_eq!(st, StatusCode::OK, "{row}");
    assert!(row["archived_ms"].is_number());
    // The state is untouched: archiving is not an ending.
    assert_eq!(row["state"], "closed");
    assert_eq!(h.ended_ids().await, vec!["s2"]);

    // The session itself is still there to open.
    let (st, s) = h.call("GET", "/api/sessions/s1").await;
    assert_eq!(st, StatusCode::OK, "{s}");

    let (st, row) = h.call("POST", "/api/sessions/s1/unarchive").await;
    assert_eq!(st, StatusCode::OK, "{row}");
    assert!(row["archived_ms"].is_null());
    assert_eq!(h.ended_ids().await, vec!["s1", "s2"]);
}

#[tokio::test]
async fn archiving_everything_ended_leaves_what_is_running() {
    state::isolate();
    let h = Harness::new().await;
    for (i, st) in ["closed", "failed", "killed_budget"].iter().enumerate() {
        h.store
            .insert_session(&session_row(&format!("e{i}"), st, now_ms() - i as i64))
            .unwrap();
    }
    h.store
        .insert_session(&session_row("live", "running", now_ms()))
        .unwrap();

    let (st, body) = h.call("POST", "/api/sessions/archive-ended").await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["archived"], 3);
    let (_, q) = h.call("GET", "/api/queue").await;
    assert!(q["ended"].as_array().unwrap().is_empty());
    assert_eq!(q["running"].as_array().unwrap().len(), 1);

    // Nothing left to archive the second time.
    let (_, body) = h.call("POST", "/api/sessions/archive-ended").await;
    assert_eq!(body["archived"], 0);
}

/// The home shows the last twenty; the rest are on the sessions screen. The
/// bound is the database's, not the interface's.
#[tokio::test]
async fn the_home_is_bounded_however_much_has_ended() {
    state::isolate();
    let h = Harness::new().await;
    let t = now_ms();
    for i in 0..25 {
        h.store
            .insert_session(&session_row(&format!("s{i:02}"), "closed", t - i))
            .unwrap();
    }
    let (_, q) = h.call("GET", "/api/queue").await;
    assert_eq!(q["ended"].as_array().unwrap().len(), 20);
    // Newest first: the oldest five are the ones left off.
    assert_eq!(q["ended"][0]["id"], "s00");
    // All twenty-five are still there when the whole history is asked for.
    let (_, all) = h.call("GET", "/api/sessions").await;
    assert_eq!(all.as_array().unwrap().len(), 25);
}

#[tokio::test]
async fn archiving_a_session_that_is_not_there_is_a_404() {
    state::isolate();
    let h = Harness::new().await;
    let (st, _) = h.call("POST", "/api/sessions/nope/archive").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_archived_channel_starts_nothing_new() {
    state::isolate();
    let h = Harness::new().await;
    h.store
        .channel_put(
            "personal",
            b"k",
            &json!({ "archived": now_ms() }).to_string(),
        )
        .unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/api/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "channel": "personal",
                "repo_path": "/nonexistent/repo",
                "model": "m/a",
            })
            .to_string(),
        ))
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("archived"));
}
