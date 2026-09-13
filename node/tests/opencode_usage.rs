//! Two usage sources, one ledger. The gateway counts every model call on the
//! wire; the harness reports its own numbers per turn and a harness that saw
//! nothing reports zero rather than "unknown" (`providers.md` §6.3). So both
//! are recorded per turn and reconciled at turn end, the gateway's count is
//! what the budget and the channel ceiling are charged, and a turn nobody
//! could count is marked unmetered rather than free.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use serde_json::json;
use tower::ServiceExt;

use tracon::{
    broker::Broker,
    config::{Config, Provider, SHAPE_ANTHROPIC},
    http::api::AppState,
    mcp::Tools,
    metrics,
    session::Manager,
    store::Store,
    stream::Bus,
};

use support::fake::FakeAdapter;

/// What the upstream reports about its own usage. A provider that omits it is
/// the case the plan names: no usage in the response, no estimator anywhere,
/// and therefore no honest number.
#[derive(Clone, Copy)]
enum Reports {
    Usage { input: i64, output: i64 },
    Nothing,
}

#[derive(Clone)]
struct Upstream {
    reports: Arc<Mutex<Reports>>,
}

async fn start_upstream(up: Upstream) -> u16 {
    let app = axum::Router::new().fallback(any(move |_req: Request<Body>| {
        let up = up.clone();
        async move {
            let sse = match *up.reports.lock().unwrap() {
                Reports::Usage { input, output } => format!(
                    "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}}}\n\n\
                     event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
                ),
                // A local OpenAI-compatible endpoint that answers without any
                // usage object at all.
                Reports::Nothing => "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n\
                     event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                    .to_string(),
            };
            ([("content-type", "text/event-stream")], sse).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

struct Harness {
    app: axum::Router,
    store: Arc<Store>,
    manager: Manager,
    upstream: Upstream,
}

const STORE: &str = r#"
    [credentials.stubcred]
    kind = "api_key"
    provider = "stub"
    channels = ["work"]
    [credentials.stubcred.env]
    API_KEY = "real-key"
"#;

async fn harness() -> Harness {
    let upstream = Upstream {
        reports: Arc::new(Mutex::new(Reports::Usage {
            input: 900,
            output: 100,
        })),
    };
    let port = start_upstream(upstream.clone()).await;
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut cfg = Config::default();
    cfg.gateway.allow_hosts = vec![r"^127\.0\.0\.1$".into()];
    cfg.providers.clear();
    cfg.providers.insert(
        "stub".into(),
        Provider {
            credential: "stubcred".into(),
            upstream: format!("http://127.0.0.1:{port}"),
            shape: SHAPE_ANTHROPIC.into(),
            login: None,
            device_login: None,
            requires_local_callback: false,
            price: None,
            models: Vec::new(),
        },
    );
    let cfg = Arc::new(cfg);
    let broker: Broker = toml::from_str(STORE).unwrap();
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
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    });
    store.ensure_peer_node("n1").unwrap();
    Harness {
        app,
        store,
        manager,
        upstream,
    }
}

impl Harness {
    fn session(&self, id: &str) {
        self.store
            .conn()
            .execute(
                "INSERT INTO session (id, node_id, channel, repo_path, branch, harness_id, harness_version, model,
                    budget_tokens, tokens_used, state, turn_active, created_ms, updated_ms)
                 VALUES (?1, 'n1', 'work', '/r', 'b', 'fake', '1', 'm', 100000, 0, 'running', 1, 1, 1)",
                [id],
            )
            .unwrap();
    }

    /// One model call through the gateway, as the harness would make it. The
    /// body is drained so the counting stream reaches its end.
    async fn model_call(&self, token: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/model/stub/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", token)
            .body(Body::from(
                json!({"model": "claude-x", "stream": true, "messages": []}).to_string(),
            ))
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let _ = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        status
    }

    fn kinds(&self, session_id: &str) -> Vec<String> {
        self.store
            .events_after(session_id, 0, 200)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }
}

#[tokio::test]
async fn the_two_sources_are_recorded_side_by_side_and_agree() {
    state::isolate();
    let h = harness().await;
    h.session("s-agree");
    let token = h
        .manager
        .register_tool_token_for_test("s-agree", "work")
        .await;
    let turn = h.store.begin_turn("s-agree").unwrap();
    assert_eq!(h.model_call(&token).await, StatusCode::OK);

    // The harness reports what the wire carried, near enough.
    let usage = metrics::settle_turn(&h.store, "s-agree", Some(1000), Some(0.01));
    assert_eq!(usage.turn, turn);
    assert_eq!(usage.gateway_tokens, 1000);
    assert_eq!(usage.harness_tokens, Some(1000));
    assert_eq!(usage.charged, 1000);
    assert!(usage.agreed(), "{usage:?}");
    assert_eq!(usage.event_kind(), None, "agreement is not news");

    // Both numbers are in the ledger, not just the one that won.
    let ledger = h.store.turn_ledger("s-agree", 10).unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].state, "reconciled");
    assert_eq!(ledger[0].gateway_input, 900);
    assert_eq!(ledger[0].gateway_output, 100);
    assert_eq!(ledger[0].gateway_requests, 1);
    assert_eq!(ledger[0].harness_tokens, Some(1000));
    assert_eq!(ledger[0].charged_tokens, 1000);
}

#[tokio::test]
async fn a_harness_that_under_reports_is_recorded_and_charged_the_gateway_count() {
    state::isolate();
    let h = harness().await;
    h.session("s-mismatch");
    let token = h
        .manager
        .register_tool_token_for_test("s-mismatch", "work")
        .await;
    h.store.begin_turn("s-mismatch").unwrap();
    assert_eq!(h.model_call(&token).await, StatusCode::OK);

    let usage = metrics::settle_turn(&h.store, "s-mismatch", Some(12), None);
    assert_eq!(usage.state, metrics::usage_state::MISMATCH);
    assert_eq!(
        usage.charged, 1000,
        "the harness's number never lowers the charge"
    );

    // The disagreement is written down with both sides, so the operator can
    // say which source is wrong rather than being handed one number.
    assert_eq!(usage.event_kind(), Some("usage_mismatch"));
    let detail = usage.detail();
    assert_eq!(detail["gateway"]["tokens"], 1000);
    assert_eq!(detail["gateway"]["requests"], 1);
    assert_eq!(detail["harness"]["tokens"], 12);
    assert_eq!(detail["charged_tokens"], 1000);
    assert_eq!(
        h.store.turn_ledger("s-mismatch", 10).unwrap()[0].harness_tokens,
        Some(12)
    );
}

#[tokio::test]
async fn a_provider_that_reports_no_usage_makes_the_turn_unmetered_not_free() {
    state::isolate();
    let h = harness().await;
    *h.upstream.reports.lock().unwrap() = Reports::Nothing;
    h.session("s-unmetered");
    let token = h
        .manager
        .register_tool_token_for_test("s-unmetered", "work")
        .await;
    h.store.begin_turn("s-unmetered").unwrap();
    assert_eq!(h.model_call(&token).await, StatusCode::OK);

    // The harness reads the same missing usage and reports zero for it.
    let usage = metrics::settle_turn(&h.store, "s-unmetered", Some(0), None);
    assert_eq!(usage.state, metrics::usage_state::UNMETERED);
    assert_eq!(usage.gateway_requests, 1, "a call was made");
    assert_eq!(usage.gateway_tokens, 0, "and none of it could be counted");
    assert_eq!(usage.event_kind(), Some("usage_unmetered"));
    assert_eq!(
        h.store.turn_ledger("s-unmetered", 10).unwrap()[0].state,
        "unmetered"
    );
}

#[tokio::test]
async fn a_ceiling_is_enforced_from_the_gateway_count_when_the_harness_under_reports() {
    state::isolate();
    let h = harness().await;
    h.store
        .channel_put("work", b"ring", r#"{"ceiling_tokens_per_day": 1500}"#)
        .unwrap();
    h.session("s-ceiling");
    let token = h
        .manager
        .register_tool_token_for_test("s-ceiling", "work")
        .await;

    // Two turns of 1000 counted tokens each, and a harness insisting on 1.
    for _ in 0..2 {
        h.store.begin_turn("s-ceiling").unwrap();
        assert_eq!(h.model_call(&token).await, StatusCode::OK);
        let usage = metrics::settle_turn(&h.store, "s-ceiling", Some(1), None);
        assert_eq!(usage.state, metrics::usage_state::MISMATCH);
    }

    // The ceiling reads the wire, not the harness: 2000 counted against 1500.
    let bindings = h.manager.bindings("work");
    let info = metrics::ceiling(&h.store, &bindings, "work");
    assert!(info.at(), "{info:?}");
    assert_eq!(info.usage_today, 2000);

    // And the gateway refuses the next model call outright.
    h.store.begin_turn("s-ceiling").unwrap();
    assert_eq!(
        h.model_call(&token).await,
        StatusCode::TOO_MANY_REQUESTS,
        "a session already running stops spending at the ceiling"
    );
    assert!(h.kinds("s-ceiling").iter().any(|k| k == "ceiling"));
}

#[tokio::test]
async fn an_unmetered_turn_does_not_pass_a_ceiling_silently() {
    state::isolate();
    let h = harness().await;
    h.store
        .channel_put("work", b"ring", r#"{"ceiling_tokens_per_day": 1500}"#)
        .unwrap();
    *h.upstream.reports.lock().unwrap() = Reports::Nothing;
    h.session("s-quiet");
    let token = h
        .manager
        .register_tool_token_for_test("s-quiet", "work")
        .await;
    h.store.begin_turn("s-quiet").unwrap();
    assert_eq!(h.model_call(&token).await, StatusCode::OK);
    let usage = metrics::settle_turn(&h.store, "s-quiet", Some(0), None);
    assert!(usage.unmetered());

    // Nothing countable happened, so the ceiling is not reached — but it does
    // not read as an ordinary quiet day either: the turn is named.
    let bindings = h.manager.bindings("work");
    let info = metrics::ceiling(&h.store, &bindings, "work");
    assert_eq!(info.usage_today, 0);
    assert!(!info.at());
    assert_eq!(info.unmetered_turns, 1);
    assert!(
        info.reason().contains("unmetered"),
        "the operator is told rather than shown a zero: {}",
        info.reason()
    );

    // The hard ceiling still bites on what *is* counted, unmetered turns or not.
    *h.upstream.reports.lock().unwrap() = Reports::Usage {
        input: 1500,
        output: 100,
    };
    h.store.begin_turn("s-quiet").unwrap();
    assert_eq!(h.model_call(&token).await, StatusCode::OK);
    metrics::settle_turn(&h.store, "s-quiet", Some(1600), None);
    h.store.begin_turn("s-quiet").unwrap();
    assert_eq!(h.model_call(&token).await, StatusCode::TOO_MANY_REQUESTS);
    let info = metrics::ceiling(&h.store, &h.manager.bindings("work"), "work");
    assert!(info.at());
    assert_eq!(info.unmetered_turns, 1, "still counted separately");
}

#[tokio::test]
async fn gateway_counts_land_on_the_turn_that_made_them() {
    state::isolate();
    let h = harness().await;
    h.session("s-turns");
    let token = h
        .manager
        .register_tool_token_for_test("s-turns", "work")
        .await;

    let first = h.store.begin_turn("s-turns").unwrap();
    h.model_call(&token).await;
    metrics::settle_turn(&h.store, "s-turns", Some(1000), None);

    let second = h.store.begin_turn("s-turns").unwrap();
    h.model_call(&token).await;
    h.model_call(&token).await;
    metrics::settle_turn(&h.store, "s-turns", Some(2000), None);

    assert_eq!((first, second), (1, 2));
    assert_eq!(
        h.store.turn_gateway_counts("s-turns", first).unwrap(),
        (900, 100, 1)
    );
    assert_eq!(
        h.store.turn_gateway_counts("s-turns", second).unwrap(),
        (1800, 200, 2),
        "the second turn's calls are not charged to the first"
    );
    let ledger = h.store.turn_ledger("s-turns", 10).unwrap();
    assert_eq!(ledger.len(), 2, "one row per turn, newest first");
    assert_eq!(ledger[0].turn, 2);
    assert!(ledger.iter().all(|t| t.state == "reconciled"), "{ledger:?}");
}
