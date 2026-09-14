//! Retiring a harness without losing what it did.
//!
//! The OpenCode cutover removed the omp adapter, and a node that had run it
//! still holds its sessions. They are archived read-only rather than deleted —
//! harness identity, transcript, evidence and workspace all kept — and the way
//! to carry one forward is a new session, never a relaunch of a harness that
//! is gone.

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
    store::{now_ms, PermissionRow, SessionRow, Store},
    stream::Bus,
};

use support::fake::FakeAdapter;

fn session_row(id: &str, harness: &str, state: &str) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: None,
        repo_path: "workspace://ws-1".into(),
        worktree_path: Some(format!("/state/workspaces/{id}")),
        branch: "feat/half-finished".into(),
        harness_id: harness.into(),
        harness_version: "18.0.4".into(),
        harness_agent: Some("oh-my-pi".into()),
        harness_found: Some("18.0.4".into()),
        harness_protocol: Some("acp/1".into()),
        harness_session_id: None,
        container_name: None,
        model: "m/a".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 400,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: state.into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms: now_ms(),
        started_mono_ms: None,
        ended_mono_ms: None,
        updated_ms: now_ms(),
        archived_ms: None,
        legacy_ms: None,
        parent_session: None,
        continued_from: None,
        manifest_digest: None,
    }
}

fn pending_approval(id: &str, session_id: &str) -> PermissionRow {
    PermissionRow {
        id: id.into(),
        session_id: session_id.into(),
        node_id: "n1".into(),
        rpc_id: 1,
        tool_call_id: None,
        title: "write src/main.rs".into(),
        kind: Some("edit".into()),
        raw_input: None,
        options: "[]".into(),
        state: "new".into(),
        answer_option_id: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        expires_ms: now_ms() + 900_000,
    }
}

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
}

impl Harness {
    async fn new(adapter: Arc<dyn tracon::adapter::HarnessAdapter>) -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_node(&tracon::store::NodeRow {
                id: "n1".into(),
                name: "test".into(),
                state: "ready".into(),
                failed_check: None,
                failed_detail: None,
                harness_id: adapter.id().into(),
                harness_pinned: adapter.pinned_version().into(),
                harness_found: Some(adapter.pinned_version().into()),
                models_json: Some(r#"[{"value":"m/a","name":"A"}]"#.into()),
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
            adapter,
            node_id: "n1".into(),
            tools,
            mesh: None,
            auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
            enroll: Default::default(),
        });
        Self { app, store }
    }

    async fn fake() -> Self {
        Self::new(Arc::new(FakeAdapter {
            tx: Arc::new(Mutex::new(None)),
            tokens: Arc::new(Mutex::new(100)),
        }))
        .await
    }

    async fn post(&self, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method("POST").uri(uri);
        let req = match body {
            Some(v) => req
                .header("content-type", "application/json")
                .body(Body::from(v.to_string())),
            None => req.body(Body::empty()),
        }
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
}

/// Everything the row was is kept; what it loses is the ability to run. A
/// session that had not ended is ended, because the process behind it is long
/// gone, and its pending approvals are closed, because nothing can answer
/// them.
#[tokio::test]
async fn archiving_the_retired_harness_keeps_the_sessions_and_ends_them() {
    state::isolate();
    let h = Harness::fake().await;
    h.store
        .insert_session(&session_row("legacy-1", "omp", "closed"))
        .unwrap();
    h.store
        .insert_session(&session_row("legacy-2", "omp", "waiting_on_you"))
        .unwrap();
    h.store
        .insert_session(&session_row("current", "opencode", "running"))
        .unwrap();
    h.store
        .insert_permission(&pending_approval("p1", "legacy-2"))
        .unwrap();

    let (status, body) = h.post("/api/sessions/archive-legacy", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["harness"], "omp");
    assert_eq!(body["archived"], 2);
    assert_eq!(body["approvals_closed"], 1);
    assert_eq!(
        body["workspaces_retained"],
        json!(["/state/workspaces/legacy-1", "/state/workspaces/legacy-2"]),
        "workspaces are retained, and the operator is told where they are"
    );

    let one = h.store.get_session("legacy-1").unwrap().unwrap();
    assert!(one.legacy_ms.is_some());
    assert!(one.archived_ms.is_some());
    // The identity of the thing that produced the transcript, kept so the
    // transcript can still be read against it.
    assert_eq!(one.harness_id, "omp");
    assert_eq!(one.harness_version, "18.0.4");
    assert_eq!(one.harness_agent.as_deref(), Some("oh-my-pi"));
    assert_eq!(
        one.worktree_path.as_deref(),
        Some("/state/workspaces/legacy-1")
    );

    // A session that had not ended is ended, with the reason on the row.
    let two = h.store.get_session("legacy-2").unwrap().unwrap();
    assert_eq!(two.state, "closed");
    assert_eq!(two.end_reason.as_deref(), Some("harness_retired"));
    let approval = h.store.get_permission("p1").unwrap().unwrap();
    assert_eq!(approval.state, "expired");
    assert!(
        approval
            .answer_option_id
            .as_deref()
            .unwrap_or_default()
            .contains("retired"),
        "the reason is visible on the card: {approval:?}"
    );
    assert!(h.store.open_permissions().unwrap().is_empty());

    // A session of the harness this node still runs is untouched.
    let current = h.store.get_session("current").unwrap().unwrap();
    assert_eq!(current.state, "running");
    assert!(current.legacy_ms.is_none());

    // Idempotent.
    let (_, again) = h.post("/api/sessions/archive-legacy", None).await;
    assert_eq!(again["archived"], 0);
}

/// Archiving a harness the node still runs would take every live session with
/// it. Refused by name rather than obeyed.
#[tokio::test]
async fn a_supported_harness_cannot_be_archived_as_legacy() {
    state::isolate();
    let h = Harness::fake().await;
    let (status, body) = h
        .post(
            "/api/sessions/archive-legacy",
            Some(json!({ "harness": "opencode" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("still runs"),
        "{body}"
    );
}

/// A legacy session is read-only for good. The refusal names the way forward
/// rather than reading as a session that merely will not start.
#[tokio::test]
async fn a_legacy_session_refuses_to_be_put_back_to_work() {
    state::isolate();
    let h = Harness::fake().await;
    h.store
        .insert_session(&session_row("legacy-1", "omp", "paused"))
        .unwrap();
    h.post("/api/sessions/archive-legacy", None).await;

    for (route, body) in [
        (
            "/api/sessions/legacy-1/prompt",
            Some(json!({ "text": "go" })),
        ),
        ("/api/sessions/legacy-1/resume", Some(json!({}))),
    ] {
        let (status, answer) = h.post(route, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{route}: {answer}");
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("retired"), "{route}: {message}");
        assert!(message.contains("session reopen"), "{route}: {message}");
    }
}

/// Reopening is a new session, never the old one restarted: the legacy id
/// names a transcript a harness that is gone produced, and a second session
/// writing under it would make that transcript unreadable.
#[tokio::test]
async fn reopening_makes_a_new_session_with_the_workspace_and_the_lineage() {
    state::isolate();
    let adapter: Arc<dyn tracon::adapter::HarnessAdapter> =
        Arc::new(tracon::adapter::opencode::OpenCodeAdapter::new(
            tracon::adapter::opencode::OpenCodeAdapter::PINNED_VERSION,
        ));
    let h = Harness::new(adapter).await;
    h.store
        .insert_session(&session_row("legacy-1", "omp", "closed"))
        .unwrap();
    h.post("/api/sessions/archive-legacy", None).await;

    let (status, row) = h
        .post(
            "/api/sessions/legacy-1/reopen",
            Some(json!({ "harness": "opencode" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    let new_id = row["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, "legacy-1", "the legacy id is never reused");
    assert_eq!(row["harness_id"], "opencode");
    assert_eq!(row["parent_session"], "legacy-1");
    assert_eq!(row["continued_from"], "legacy-1");
    // The retained workspace and the branch it left off on.
    assert_eq!(row["repo_path"], "workspace://ws-1");
    assert_eq!(row["branch"], "feat/half-finished");

    // The handoff note is held as the session's draft until the harness takes
    // it, so it is on the row rather than only in a log line.
    let created = h.store.get_session(&new_id).unwrap().unwrap();
    let note = created.draft.unwrap_or_default();
    assert!(note.contains("legacy-1"), "{note}");
    assert!(note.contains("omp"), "{note}");
    assert!(
        note.contains("inherit none of its context"),
        "the handoff has to say what is not carried over: {note}"
    );
    assert!(note.contains("feat/half-finished"), "{note}");

    // And the legacy session is exactly as it was: still there, still
    // read-only, still naming the harness that ran it.
    let old = h.store.get_session("legacy-1").unwrap().unwrap();
    assert!(old.legacy_ms.is_some());
    assert_eq!(old.harness_id, "omp");
    assert_eq!(old.state, "closed");
}

/// One node runs one harness image. Offering to reopen under the other one and
/// then failing at the version check would be a worse answer than saying so.
#[tokio::test]
async fn reopening_under_a_harness_this_node_does_not_run_is_refused() {
    state::isolate();
    let adapter: Arc<dyn tracon::adapter::HarnessAdapter> =
        Arc::new(tracon::adapter::opencode::OpenCodeAdapter::new(
            tracon::adapter::opencode::OpenCodeAdapter::PINNED_VERSION,
        ));
    let h = Harness::new(adapter).await;
    h.store
        .insert_session(&session_row("legacy-1", "omp", "closed"))
        .unwrap();
    h.post("/api/sessions/archive-legacy", None).await;

    let (status, body) = h
        .post(
            "/api/sessions/legacy-1/reopen",
            Some(json!({ "harness": "claude" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("runs the `opencode` harness"), "{message}");
    assert!(message.contains("tracon setup"), "{message}");

    // And a harness that does not exist at all is refused by name.
    let (status, body) = h
        .post(
            "/api/sessions/legacy-1/reopen",
            Some(json!({ "harness": "omp" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("no harness `omp`"));
}

/// The node will not start against the retired harness, and what it says is
/// the migration path rather than a list of adapter names.
#[test]
fn a_node_configured_for_the_retired_harness_refuses_to_start() {
    let mut cfg = Config::default();
    cfg.harness.id = "omp".into();
    let error = match tracon::adapter::adapter_for(&cfg) {
        Ok(_) => panic!("the retired harness must not resolve to an adapter"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("was removed"), "{error}");
    assert!(error.contains("archive-legacy"), "{error}");
    assert!(error.contains("session reopen"), "{error}");
}
