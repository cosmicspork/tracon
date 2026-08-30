//! Session lifecycle over the real HTTP API, with a fake harness in place of a
//! containerised omp. Covers what the operator actually does: start a session,
//! prompt it, answer a permission request, and kill it.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};
use tower::ServiceExt;

use tracon::{
    adapter::{HarnessEvent, PermissionReply},
    config::Config,
    http::api::AppState,
    runner::Runner,
    session::Manager,
    store::{now_ms, NodeRow, Store},
    stream::Bus,
};

use support::fake::{FakeAdapter, FakeHandle};

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
}

impl Harness {
    async fn new(budget: i64) -> Self {
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
        let events = Arc::new(Mutex::new(None));
        let tokens = Arc::new(Mutex::new(100));
        let adapter = Arc::new(FakeAdapter {
            tx: events.clone(),
            tokens: tokens.clone(),
        });
        let mut cfg = Config::default();
        cfg.session.budget_tokens = budget;
        cfg.session.permission_timeout_secs = 1;
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
            auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        });
        Self { app, store }
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
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }
}

#[tokio::test]
async fn a_session_without_a_model_is_refused() {
    state::isolate();
    let h = Harness::new(1000).await;
    let (status, body) = h
        .call(
            "POST",
            "/api/sessions",
            Some(json!({ "channel": "personal", "repo_path": "/nonexistent/repo", "model": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("model is required"));
}

#[tokio::test]
async fn a_session_needs_a_ready_item_and_execute_needs_its_plan() {
    state::isolate();
    let h = Harness::new(1000).await;
    let bus = Bus::new();
    let mk = |title: &str, deps: Vec<String>| {
        tracon::corpus::work::create(
            &h.store,
            &bus,
            "n1",
            tracon::corpus::work::NewWork {
                channel: "personal".into(),
                project_id: None,
                title: title.into(),
                body: String::new(),
                deps,
                priority: 0,
                discovered_from: None,
                discovered_by_session: None,
            },
        )
        .unwrap()
    };
    let a = mk("A", vec![]);
    let b = mk("B", vec![a.id.clone()]);
    let base = json!({ "channel": "personal", "repo_path": "/nonexistent/repo", "model": "m/a" });
    let spec = |extra: Value| {
        let mut v = base.clone();
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    // No item at all.
    let (st, body) = h.call("POST", "/api/sessions", Some(spec(json!({})))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("work item is required"));
    // Blocked.
    let (st, body) = h
        .call(
            "POST",
            "/api/sessions",
            Some(spec(json!({"work_item_id": b.id, "phase": "plan"}))),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("blocked by open item"));
    // Ready, but execute has no plan yet.
    let (st, body) = h
        .call(
            "POST",
            "/api/sessions",
            Some(spec(json!({"work_item_id": a.id, "phase": "execute"}))),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("needs a plan"));
    // Unknown item.
    let (st, body) = h
        .call(
            "POST",
            "/api/sessions",
            Some(spec(json!({"work_item_id": "nope", "phase": "plan"}))),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no work item"));
}

#[tokio::test]
async fn a_refused_node_refuses_sessions_and_says_which_check_failed() {
    state::isolate();
    let h = Harness::new(1000).await;
    h.store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "test".into(),
            state: "refused".into(),
            failed_check: Some("network_isolated".into()),
            failed_detail: Some("tracon-int is not internal".into()),
            harness_id: "fake".into(),
            harness_pinned: "1.0.0".into(),
            harness_found: Some("1.0.0".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
            providers_json: None,
        })
        .unwrap();
    let (status, body) = h
        .call(
            "POST",
            "/api/sessions",
            Some(
                json!({ "channel": "personal", "repo_path": "/nonexistent/repo", "model": "m/a" }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not internal"));

    let (_, node) = h.call("GET", "/api/node", None).await;
    assert_eq!(node["state"], "refused");
    assert_eq!(node["failed_check"], "network_isolated");
}

#[tokio::test]
async fn a_version_mismatch_blocks_new_sessions() {
    state::isolate();
    let h = Harness::new(1000).await;
    h.store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "test".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "fake".into(),
            harness_pinned: "1.0.0".into(),
            harness_found: Some("1.1.0".into()),
            models_json: None,
            checked_at_ms: Some(now_ms()),
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
            providers_json: None,
        })
        .unwrap();
    let (status, body) = h
        .call(
            "POST",
            "/api/sessions",
            Some(
                json!({ "channel": "personal", "repo_path": "/nonexistent/repo", "model": "m/a" }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("1.1.0") && msg.contains("1.0.0"), "{msg}");

    let (_, node) = h.call("GET", "/api/node", None).await;
    assert_eq!(node["harness"]["mismatch"], true);
}

#[tokio::test]
async fn drafts_survive_a_lost_client() {
    state::isolate();
    let h = Harness::new(1000).await;
    let id = insert_running_session(&h.store, 1000);
    let (status, _) = h
        .call(
            "PUT",
            &format!("/api/sessions/{id}/draft"),
            Some(json!({ "text": "half a thought" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = h.call("GET", &format!("/api/sessions/{id}"), None).await;
    assert_eq!(body["session"]["draft"], "half a thought");
}

#[tokio::test]
async fn the_queue_orders_waiting_before_running() {
    state::isolate();
    let h = Harness::new(1000).await;
    let id = insert_running_session(&h.store, 1000);
    h.store
        .insert_permission(&tracon::store::PermissionRow {
            id: "p1".into(),
            session_id: id.clone(),
            node_id: "n1".into(),
            rpc_id: 0,
            tool_call_id: None,
            title: "run just test".into(),
            kind: Some("execute".into()),
            raw_input: None,
            options: "[]".into(),
            state: "new".into(),
            answer_option_id: None,
            created_ms: now_ms(),
            created_mono_ms: 0,
            resolved_mono_ms: None,
            expires_ms: now_ms() + 60_000,
        })
        .unwrap();
    let (status, body) = h.call("GET", "/api/queue", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["waiting"].as_array().unwrap().len(), 1);
    assert_eq!(body["waiting"][0]["title"], "run just test");
    assert_eq!(body["running"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn prompting_a_session_that_is_not_running_is_refused() {
    state::isolate();
    let h = Harness::new(1000).await;
    let id = insert_running_session(&h.store, 1000);
    // Not registered with the manager, so it is not live on this node.
    let (status, body) = h
        .call(
            "POST",
            &format!("/api/sessions/{id}/prompt"),
            Some(json!({ "text": "hello" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not running"));
}

#[tokio::test]
async fn events_are_readable_after_a_given_seq() {
    state::isolate();
    let h = Harness::new(1000).await;
    let id = insert_running_session(&h.store, 1000);
    for i in 0..3 {
        h.store
            .append_event(&tracon::store::NewEvent {
                session_id: id.clone(),
                work_item_id: None,
                kind: "message".into(),
                ref_id: None,
                payload: json!({ "i": i }),
                at_ms: now_ms(),
                mono_ms: i,
            })
            .unwrap();
    }
    let (_, all) = h
        .call("GET", &format!("/api/sessions/{id}/events"), None)
        .await;
    assert_eq!(all.as_array().unwrap().len(), 3);
    let first_seq = all[0]["seq"].as_i64().unwrap();
    let (_, rest) = h
        .call(
            "GET",
            &format!("/api/sessions/{id}/events?after={first_seq}"),
            None,
        )
        .await;
    assert_eq!(rest.as_array().unwrap().len(), 2);
}

fn insert_running_session(store: &Arc<Store>, budget: i64) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    store
        .insert_session(&tracon::store::SessionRow {
            id: id.clone(),
            node_id: "n1".into(),
            channel: "personal".into(),
            work_item_id: None,
            repo_path: "/nonexistent/repo".into(),
            worktree_path: None,
            branch: "feat/x".into(),
            harness_id: "fake".into(),
            harness_version: "1.0.0".into(),
            harness_session_id: None,
            container_name: None,
            model: "m/a".into(),
            project_id: None,
            phase: "execute".into(),
            policy_version: None,
            review_id: None,
            budget_tokens: budget,
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
        })
        .unwrap();
    id
}

// ---- supervisor behaviour -------------------------------------------------
//
// Driven directly, so the states that matter are tested without a container or
// a git repo in the way.

use std::time::{Duration, Instant};
use tracon::session::supervisor::{Command, Supervisor};

/// The supervisor tears down its container on exit; these tests have none.
struct NoRunner;

#[async_trait]
impl Runner for NoRunner {
    async fn spawn(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<tracon::runner::Spawned, tracon::runner::RunnerError> {
        unreachable!("the fake adapter never spawns")
    }
    async fn run_capture(
        &self,
        _cmd: tracon::runner::RunnerCommand,
    ) -> Result<std::process::Output, tracon::runner::RunnerError> {
        unreachable!("the fake adapter never spawns")
    }
    async fn kill(&self, _name: &str) -> Result<(), tracon::runner::RunnerError> {
        Ok(())
    }
}

struct Rig {
    store: Arc<Store>,
    session_id: String,
    events: mpsc::Sender<HarnessEvent>,
    commands: mpsc::Sender<Command>,
}

impl Rig {
    async fn start(budget: i64, permission_timeout: Duration) -> Self {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store
            .put_node(&NodeRow {
                id: "n1".into(),
                name: "t".into(),
                state: "ready".into(),
                failed_check: None,
                failed_detail: None,
                harness_id: "fake".into(),
                harness_pinned: "1.0.0".into(),
                harness_found: Some("1.0.0".into()),
                models_json: None,
                checked_at_ms: Some(now_ms()),
                is_self: 1,
                x25519_pub: None,
                last_seen_ms: None,
                reachable: 1,
                providers_json: None,
            })
            .unwrap();
        let session_id = insert_running_session(&store, budget);
        let (ev_tx, ev_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let handle = Arc::new(FakeHandle {
            prompts: Arc::new(Mutex::new(Vec::new())),
            tokens: Arc::new(Mutex::new(1500)),
            killed: Arc::new(Mutex::new(false)),
        });
        let sup = Supervisor::new(
            session_id.clone(),
            "n1".into(),
            store.clone(),
            Bus::new(),
            handle,
            Instant::now(),
            permission_timeout,
            cmd_tx.clone(),
            Arc::new(NoRunner),
            "tracon-h-test".into(),
            Default::default(),
            "personal".into(),
        );
        tokio::spawn(sup.run(ev_rx, cmd_rx));
        let rig = Self {
            store,
            session_id,
            events: ev_tx,
            commands: cmd_tx,
        };
        assert!(
            rig.await_state("running").await,
            "the session never started: {:?}",
            rig.store.get_session(&rig.session_id).unwrap()
        );
        rig
    }

    async fn await_state(&self, want: &str) -> bool {
        for _ in 0..200 {
            if let Ok(Some(s)) = self.store.get_session(&self.session_id) {
                if s.state == want {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    fn kinds(&self) -> Vec<String> {
        self.store
            .events_after(&self.session_id, 0, 500)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    async fn request_permission(&self) -> oneshot::Receiver<PermissionReply> {
        let (reply, wait) = oneshot::channel();
        self.events
            .send(HarnessEvent::Permission {
                request: tracon::adapter::PermissionRequest {
                    tool_call_id: Some("call|fc".into()),
                    title: "run just test".into(),
                    kind: Some("execute".into()),
                    raw_input: None,
                    options: vec![],
                },
                reply,
            })
            .await
            .unwrap();
        wait
    }
}

#[tokio::test]
async fn a_permission_request_moves_the_session_to_waiting_and_back() {
    state::isolate();
    let rig = Rig::start(10_000, Duration::from_secs(60)).await;
    let answer = rig.request_permission().await;
    assert!(rig.await_state("waiting_on_you").await);

    let open = rig.store.open_permissions().unwrap();
    assert_eq!(open.len(), 1);
    let (ack, done) = oneshot::channel();
    rig.commands
        .send(Command::Answer {
            permission_id: open[0].id.clone(),
            option_id: "allow_once".into(),
            ack,
        })
        .await
        .unwrap();
    done.await.unwrap().unwrap();

    // The harness gets the operator's answer, verbatim.
    match answer.await.unwrap() {
        PermissionReply::Selected(o) => assert_eq!(o, "allow_once"),
        other => panic!("expected a selection, got {other:?}"),
    }
    assert!(rig.await_state("running").await);
    assert!(rig.kinds().contains(&"permission_answer".to_string()));
    assert!(rig.store.open_permissions().unwrap().is_empty());
}

#[tokio::test]
async fn a_brokered_tool_call_the_policy_does_not_cover_waits_on_the_operator() {
    state::isolate();
    // The node's own tools are gated by the same queue as the harness's: a
    // call the bundle does not name lands on the session as a request of kind
    // `tool`, and the operator's answer reaches the caller verbatim.
    let rig = Rig::start(10_000, Duration::from_secs(60)).await;
    let (reply, wait) = oneshot::channel();
    rig.commands
        .send(Command::Permission {
            request: tracon::adapter::PermissionRequest {
                tool_call_id: None,
                title: "issue_comment {\"key\":\"WRK-1\"}".into(),
                kind: Some(tracon::mcp::TOOL_KIND.into()),
                raw_input: Some(json!({ "tool": "issue_comment" })),
                options: vec![],
            },
            reply,
        })
        .await
        .unwrap();
    assert!(rig.await_state("waiting_on_you").await);
    let open = rig.store.open_permissions().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].kind.as_deref(), Some("tool"));
    let (ack, done) = oneshot::channel();
    rig.commands
        .send(Command::Answer {
            permission_id: open[0].id.clone(),
            option_id: "reject_once".into(),
            ack,
        })
        .await
        .unwrap();
    done.await.unwrap().unwrap();
    match wait.await.unwrap() {
        PermissionReply::Selected(o) => assert_eq!(o, "reject_once"),
        other => panic!("expected a selection, got {other:?}"),
    }
    assert!(rig.await_state("running").await);
}

#[tokio::test]
async fn an_unanswered_request_is_denied_by_default() {
    state::isolate();
    // Deny-on-expiry is the whole point of the gate: silence is a refusal.
    let rig = Rig::start(10_000, Duration::from_millis(50)).await;
    let answer = rig.request_permission().await;
    assert!(rig.await_state("waiting_on_you").await);

    match tokio::time::timeout(Duration::from_secs(15), answer).await {
        Ok(Ok(PermissionReply::Selected(o))) => assert_eq!(o, "reject_once"),
        other => panic!("expected reject_once on expiry, got {other:?}"),
    }
    assert!(rig.await_state("running").await);
    let kinds = rig.kinds();
    assert!(
        kinds.contains(&"permission_expired".to_string()),
        "{kinds:?}"
    );
    assert!(rig.store.open_permissions().unwrap().is_empty());
}

#[tokio::test]
async fn a_session_over_budget_is_killed_at_turn_end() {
    state::isolate();
    // The fake handle reports 1500 tokens for the turn, over the 1000 budget.
    let rig = Rig::start(1000, Duration::from_secs(60)).await;
    let (ack, done) = oneshot::channel();
    rig.commands
        .send(Command::Prompt {
            text: "do the thing".into(),
            ack,
        })
        .await
        .unwrap();
    done.await.unwrap().unwrap();

    assert!(rig.await_state("killed_budget").await);
    let s = rig.store.get_session(&rig.session_id).unwrap().unwrap();
    assert_eq!(s.tokens_used, 1500);
    assert_eq!(s.end_reason.as_deref(), Some("budget"));
    assert_eq!(s.turn_active, 0);
}

#[tokio::test]
async fn killing_a_session_closes_it_and_expires_open_requests() {
    state::isolate();
    let rig = Rig::start(10_000, Duration::from_secs(60)).await;
    let answer = rig.request_permission().await;
    assert!(rig.await_state("waiting_on_you").await);

    rig.commands.send(Command::Kill).await.unwrap();
    assert!(rig.await_state("closed").await);

    // A request left open when the session ends is withdrawn, not left hanging.
    assert!(matches!(answer.await.unwrap(), PermissionReply::Cancelled));
    assert!(rig.store.open_permissions().unwrap().is_empty());
    let s = rig.store.get_session(&rig.session_id).unwrap().unwrap();
    assert_eq!(s.end_reason.as_deref(), Some("killed_user"));
}

#[tokio::test]
async fn closing_the_work_item_ends_the_session_after_its_turn() {
    state::isolate();
    // Idle: the end is immediate.
    let rig = Rig::start(10_000, Duration::from_secs(60)).await;
    rig.commands
        .send(Command::EndAfterTurn(
            tracon::session::state::EndReason::ItemClose,
        ))
        .await
        .unwrap();
    assert!(rig.await_state("closed").await);
    let s = rig.store.get_session(&rig.session_id).unwrap().unwrap();
    assert_eq!(s.end_reason.as_deref(), Some("item_close"));

    // Mid-turn: the turn finishes first, then the session ends.
    let rig = Rig::start(10_000, Duration::from_secs(60)).await;
    let (ack, done) = oneshot::channel();
    rig.commands
        .send(Command::Prompt {
            text: "close it".into(),
            ack,
        })
        .await
        .unwrap();
    done.await.unwrap().unwrap();
    rig.commands
        .send(Command::EndAfterTurn(
            tracon::session::state::EndReason::ItemClose,
        ))
        .await
        .unwrap();
    assert!(rig.await_state("closed").await);
    let s = rig.store.get_session(&rig.session_id).unwrap().unwrap();
    assert_eq!(s.end_reason.as_deref(), Some("item_close"));
    assert_eq!(s.turn_active, 0);
    let kinds: Vec<String> = rig
        .store
        .events_after(&rig.session_id, 0, 500)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    let turn = kinds.iter().position(|k| k == "turn_end").unwrap();
    let closed = kinds.iter().rposition(|k| k == "state").unwrap();
    assert!(turn < closed, "{kinds:?}");
}

#[tokio::test]
async fn streamed_chunks_are_coalesced_into_one_logged_message() {
    state::isolate();
    let rig = Rig::start(10_000, Duration::from_secs(60)).await;
    for part in ["Read", "ing the", " file"] {
        rig.events
            .send(HarnessEvent::MessageChunk {
                message_id: Some("m1".into()),
                text: part.into(),
            })
            .await
            .unwrap();
    }
    // A tool call closes the open message.
    rig.events
        .send(HarnessEvent::ToolCall(tracon::acp::types::ToolCall {
            tool_call_id: "call|fc".into(),
            title: "read file".into(),
            kind: Some("read".into()),
            status: Some("pending".into()),
            raw_input: None,
            content: vec![],
            locations: vec![],
        }))
        .await
        .unwrap();

    for _ in 0..200 {
        let events = rig.store.events_after(&rig.session_id, 0, 500).unwrap();
        if let Some(m) = events.iter().find(|e| e.kind == "message") {
            assert_eq!(m.payload["text"], "Reading the file");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no coalesced message event: {:?}", rig.kinds());
}

// ---- brokered tools -------------------------------------------------------

use axum::http::Request as HttpRequest;
use tracon::broker::Broker;
use tracon::mcp::Tools;

/// The MCP surface over its real HTTP route, with the token check in place.
async fn mcp_harness(store_toml: &str) -> (axum::Router, Arc<Store>, Manager) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let cfg = Arc::new(Config::default());
    let broker: Broker = toml::from_str(store_toml).unwrap();
    let tools = Arc::new(Tools {
        broker: broker.shared(),
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
    let app = tracon::http::harness_router(AppState {
        manager: manager.clone(),
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(Mutex::new(None)),
            tokens: Arc::new(Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
    });
    (app, store, manager)
}

const BROKER_STORE: &str = r#"
    [credentials.consulta]
    channels = ["work"]
    [credentials.consulta.env]
    DB_BACKEND = "sqlite"
"#;

async fn mcp_call(app: &axum::Router, sid: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let req = HttpRequest::builder()
        .method("POST")
        .uri(format!("/mcp/{sid}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
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
async fn a_tool_call_without_a_live_session_is_unauthorized() {
    state::isolate();
    let (app, _store, _m) = mcp_harness(BROKER_STORE).await;
    let (status, _) = mcp_call(
        &app,
        "01a0-not-a-session",
        "anything",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_registered_session_can_list_tools_and_a_wrong_token_cannot() {
    state::isolate();
    let (app, _store, manager) = mcp_harness(BROKER_STORE).await;
    let sid = "sess-1";
    let token = manager.register_tool_token_for_test(sid, "work").await;

    let (status, body) = mcp_call(
        &app,
        sid,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"query"));

    // The same session, one character off: refused.
    let mut wrong = token.clone();
    let last = wrong.pop().unwrap();
    wrong.push(if last == '0' { '1' } else { '0' });
    let (status, _) = mcp_call(
        &app,
        sid,
        &wrong,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_write_is_refused_before_the_credential_is_touched() {
    state::isolate();
    let (app, _store, manager) = mcp_harness(BROKER_STORE).await;
    let sid = "sess-2";
    let token = manager.register_tool_token_for_test(sid, "work").await;
    let (status, body) = mcp_call(
        &app,
        sid,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
               "params":{"name":"query","arguments":{"sql":"DELETE FROM people"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["isError"], true);
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("DELETE"), "{text}");
}

#[tokio::test]
async fn a_session_on_an_unbound_channel_is_offered_no_tools() {
    state::isolate();
    let (app, _store, manager) = mcp_harness(BROKER_STORE).await;
    let sid = "sess-3";
    let token = manager.register_tool_token_for_test(sid, "personal").await;
    let (_, body) = mcp_call(
        &app,
        sid,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    assert!(body["result"]["tools"].as_array().unwrap().is_empty());
}

// ---- orientation --------------------------------------------------------

/// Everything a planning session needs to be oriented: a repository with a
/// remote, a directive on the channel, a work item to plan, and the session
/// itself, created but not yet asserted on.
struct Orientation {
    repo: std::path::PathBuf,
    origin_url: String,
    store: Arc<Store>,
    manager: Manager,
    cfg: Arc<Config>,
    adapter: Arc<FakeAdapter>,
    tools: Arc<tracon::mcp::Tools>,
    item: tracon_sync::work::WorkItem,
    row: tracon::store::SessionRow,
}

async fn orientation(tag: &str) -> Orientation {
    state::isolate();
    let dir = state::scratch(&format!("orientation-{tag}"));
    let repo = dir.join("repo");
    let origin = dir.join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();
    let origin_url = format!("file://{}", origin.display());
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@t"],
        vec!["config", "user.name", "t"],
        vec!["config", "commit.gpgsign", "false"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
        vec!["clone", "-q", "--bare", ".", origin.to_str().unwrap()],
        vec!["remote", "add", "origin", &origin_url],
        vec!["fetch", "-q", "origin"],
    ] {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(&args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "orient".into(),
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
    // Something to be told: a directive on the channel.
    tracon::corpus::write(
        &store,
        &Bus::new(),
        "n1",
        "personal",
        "memory",
        tracon_sync::ChangeOp::Upsert,
        "m1",
        json!({"channel": "personal", "scope": "global", "scope_ref": null, "kind": "directive",
               "body": "run just test before every commit", "source_session": null, "source_node": null,
               "confidence": 1.0, "state": "active", "created_ms": now_ms(), "updated_ms": now_ms()}),
    )
    .unwrap();
    let adapter = Arc::new(FakeAdapter {
        tx: Arc::new(Mutex::new(None)),
        tokens: Arc::new(Mutex::new(100)),
    });
    let mut cfg = Config::default();
    cfg.session.worktree_root = dir.join("worktrees");
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
        tracon::policy::Policy::shipped_shared(),
        Arc::new(tracon::runner::local::LocalBackend),
    );
    let _ = tools.session.set(tracon::mcp::SessionAccess {
        store: store.clone(),
        manager: manager.clone(),
    });
    // The item this session plans, and one more so ready work is listed.
    let item = tracon::corpus::work::create(
        &store,
        &Bus::new(),
        "n1",
        tracon::corpus::work::NewWork {
            channel: "personal".into(),
            project_id: None,
            title: "Add the ledger".into(),
            body: "Items, deps, ready-work.".into(),
            deps: vec![],
            priority: 0,
            discovered_from: None,
            discovered_by_session: None,
        },
    )
    .unwrap();
    let row = manager
        .create(
            tracon::session::NewSession {
                channel: "personal".into(),
                repo_path: repo.to_string_lossy().into_owned(),
                branch: None,
                work_item_id: Some(item.id.clone()),
                model: "m/a".into(),
                budget_tokens: Some(1000),
                node_id: None,
                phase: tracon::session::Phase::Plan,
                review_id: None,
                base_sha: None,
            },
            adapter.clone(),
        )
        .await
        .unwrap();
    Orientation {
        repo,
        origin_url,
        store,
        manager,
        cfg,
        adapter,
        tools,
        item,
        row,
    }
}

/// A session is told where it is before its first prompt: the orientation
/// is assembled on the node, recorded as an event, and handed to the harness
/// as a system-prompt file rather than anything in the worktree.
#[tokio::test]
async fn a_session_starts_with_its_orientation_recorded() {
    state::isolate();
    let Orientation {
        repo,
        origin_url,
        store,
        item,
        row,
        ..
    } = orientation("recorded").await;
    assert_eq!(row.phase, "plan");
    assert_eq!(row.policy_version, Some(4));
    // Bank identity from the remote, not the path.
    let canonical = tracon::corpus::project::canonical_remote(&origin_url).unwrap();
    assert_eq!(
        row.project_id.as_deref(),
        Some(tracon::corpus::project::project_id("personal", &canonical).as_str())
    );

    let mut orientation = None;
    for _ in 0..300 {
        if let Some(e) = store
            .events_after(&row.id, 0, 500)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "orientation")
        {
            orientation = Some(e);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let e = orientation.unwrap_or_else(|| {
        let s = store.get_session(&row.id).unwrap().unwrap();
        let kinds: Vec<String> = store
            .events_after(&row.id, 0, 500)
            .unwrap()
            .into_iter()
            .map(|e| format!("{}:{}", e.kind, e.payload))
            .collect();
        panic!(
            "no orientation event; state={} error={:?} events={kinds:#?}",
            s.state, s.last_error
        )
    });
    let text = e.payload["text"].as_str().unwrap();
    assert!(text.contains("## This node"), "{text}");
    assert!(text.contains("Node `orient`"));
    assert!(text.contains("Project `repo`"), "{text}");
    assert!(
        text.contains("no-merge"),
        "the working agreements are named"
    );
    assert!(text.contains("(directive) run just test before every commit"));
    assert!(
        text.contains("`recall`"),
        "the tools offered are named: {text}"
    );
    assert_eq!(e.payload["trimmed"], false);
    assert!(text.contains("## Work"), "{text}");
    assert!(text.contains("**Add the ledger**"), "{text}");
    assert!(text.contains("plan session"), "{text}");
    let slug = tracon::corpus::work::plan_slug(&item.id);
    assert!(text.contains(&slug), "{text}");
    // Never in the worktree: the harness gets it as a mounted system-prompt
    // file (see `materialize`), and the scratch directory is cleaned with the
    // session, so only the repository is asserted here.
    assert!(!repo.join("orientation.md").exists());

    tracon::session::materialize::remove(&row.id);
}

/// Writing the plan document is the phase's artifact: not asked for, recorded
/// on the item, and the session ends with `phase_done`.
#[tokio::test]
async fn writing_the_plan_document_ends_the_plan_phase() {
    state::isolate();
    let Orientation {
        store,
        manager,
        cfg,
        adapter,
        tools,
        item,
        row,
        ..
    } = orientation("plan").await;
    let slug = tracon::corpus::work::plan_slug(&item.id);
    for _ in 0..300 {
        if store.get_session(&row.id).unwrap().unwrap().state == "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = tracon::http::api::AppState {
        manager: manager.clone(),
        cfg: cfg.clone(),
        adapter: adapter.clone(),
        node_id: "n1".into(),
        tools: tools.clone(),
        mesh: None,
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
    };
    let harness_app = tracon::http::harness_router(state);
    let token = manager
        .register_tool_token_for_test(&row.id, "personal")
        .await;
    let (st, v) = mcp_call(
        &harness_app,
        &row.id,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"doc_write","arguments":{"slug": slug, "body": "# Plan\n\n1. Do it."}}}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_ne!(v["result"]["isError"], true, "{v}");
    let planned = store.work_get(&item.id).unwrap().unwrap();
    assert_eq!(planned.phase_plan_slug.as_deref(), Some(slug.as_str()));
    let mut ended = None;
    for _ in 0..300 {
        let s = store.get_session(&row.id).unwrap().unwrap();
        if s.state == "closed" {
            ended = s.end_reason;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(ended.as_deref(), Some("phase_done"));
    let kinds: Vec<String> = store
        .events_after(&row.id, 0, 500)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"plan_artifact".to_string()), "{kinds:?}");
    tracon::session::materialize::remove(&row.id);
}
