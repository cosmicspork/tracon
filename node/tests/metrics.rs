//! The numbers, from a seeded store: approvals and tokens per accepted
//! change, priced where a provider carries a price; provenance per commit;
//! channel bindings merged through the API.

#[path = "support/mod.rs"]
mod support;
use support::http::call;
use support::state;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use tracon::{
    broker::Broker,
    config::{Config, Price},
    http::api::AppState,
    mcp::Tools,
    session::Manager,
    store::{now_ms, NewEvent, PermissionRow, ReviewRow, SessionRow, Store, UsageRow},
    stream::Bus,
};

use support::fake::FakeAdapter;

fn session(id: &str, channel: &str, phase: &str, item: Option<&str>) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: channel.into(),
        work_item_id: item.map(str::to_string),
        repo_path: "/r".into(),
        worktree_path: None,
        branch: "feat/x".into(),
        harness_id: "fake".into(),
        harness_version: "1".into(),
        harness_session_id: None,
        container_name: None,
        model: if phase == "review" {
            "m/reviewer"
        } else {
            "m/a"
        }
        .into(),
        project_id: None,
        phase: phase.into(),
        policy_version: Some(4),
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 0,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: "closed".into(),
        end_reason: Some("item_close".into()),
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms: now_ms(),
        started_mono_ms: Some(0),
        ended_mono_ms: Some(120_000),
        updated_ms: now_ms(),
        archived_ms: None,
    }
}

fn review(id: &str, session: &str, sha: &str, state: &str, reviewer: Option<&str>) -> ReviewRow {
    ReviewRow {
        id: id.into(),
        session_id: session.into(),
        node_id: "n1".into(),
        channel: "work".into(),
        kind: "pr".into(),
        title: format!("feat: {id}"),
        body: String::new(),
        edited_title: None,
        edited_body: None,
        provider: "github".into(),
        target: "{}".into(),
        diff: "+x".into(),
        files: "[]".into(),
        head_sha: sha.into(),
        base_ref: "main".into(),
        added: 1,
        removed: 0,
        state: state.into(),
        verdict_reason: None,
        publish_result: (state == "approved").then(|| format!("https://github.test/pull/{id}")),
        claimed_ms: Some(now_ms() - 30_000),
        created_ms: now_ms() - 60_000,
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now_ms(),
        checks_json: Some(
            r#"[{"command":"just check","ok":true,"exit":0,"tail":"","ms":10}]"#.into(),
        ),
        review_session_id: reviewer.map(str::to_string),
        ai_verdict_json: reviewer
            .map(|_| r#"{"verdict":"approve","summary":"fine","findings":[]}"#.into()),
        revision_patch: None,
    }
}

fn usage(session: &str, provider: &str, input: i64, output: i64) -> UsageRow {
    UsageRow {
        channel: "work".into(),
        node_id: "n1".into(),
        session_id: Some(session.into()),
        provider: provider.into(),
        model: Some("m".into()),
        at_ms: now_ms(),
        input_tokens: input,
        output_tokens: output,
        requests: 1,
    }
}

async fn app(store: Arc<Store>) -> axum::Router {
    let mut cfg = Config::default();
    cfg.providers.get_mut("anthropic").unwrap().price = Some(Price {
        input_per_mtok: 3.0,
        output_per_mtok: 15.0,
    });
    let cfg = Arc::new(cfg);
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
    tracon::http::router(AppState {
        manager,
        cfg,
        adapter: Arc::new(FakeAdapter {
            tx: Arc::new(tokio::sync::Mutex::new(None)),
            tokens: Arc::new(tokio::sync::Mutex::new(0)),
        }),
        node_id: "n1".into(),
        tools,
        mesh: None,
        auth: std::sync::Arc::new(tracon::http::auth::AuthState::new("127.0.0.1".into(), None)),
        enroll: Default::default(),
    })
}

#[tokio::test]
async fn approvals_and_tokens_per_accepted_change_are_numbers_you_can_read() {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let item = tracon::corpus::work::create(
        &store,
        &Bus::new(),
        "n1",
        tracon::corpus::work::NewWork {
            channel: "work".into(),
            project_id: None,
            title: "Ship".into(),
            body: "the thing".into(),
            deps: vec![],
            priority: 0,
            discovered_from: None,
            discovered_by_session: None,
        },
    )
    .unwrap();
    // Two accepted changes (one with a review session), one rejected.
    for (s, phase, it) in [
        ("e1", "execute", Some(item.id.as_str())),
        ("r1", "review", Some(item.id.as_str())),
        ("e2", "execute", None),
        ("e3", "execute", None),
    ] {
        store
            .insert_session(&session(s, "work", phase, it))
            .unwrap();
    }
    store
        .insert_review(&review(
            "rv1",
            "e1",
            "abc1234567890",
            "approved",
            Some("r1"),
        ))
        .unwrap();
    store
        .insert_review(&review("rv2", "e2", "def1234567890", "approved", None))
        .unwrap();
    store
        .insert_review(&review("rv3", "e3", "0001234567890", "rejected", None))
        .unwrap();
    // Tokens: e1 800 (anthropic, priced), r1 200 (anthropic), e2 1000 (openai, unpriced), e3 5000.
    store
        .record_usage(&usage("e1", "anthropic", 600, 200))
        .unwrap();
    store
        .record_usage(&usage("r1", "anthropic", 150, 50))
        .unwrap();
    store
        .record_usage(&usage("e2", "openai", 900, 100))
        .unwrap();
    store
        .record_usage(&usage("e3", "openai", 4000, 1000))
        .unwrap();
    // Three permission answers on e1, 2s of human latency each; one unanswered.
    for (i, answered) in [(1, true), (2, true), (3, true), (4, false)] {
        let id = format!("p{i}");
        store
            .insert_permission(&PermissionRow {
                id: id.clone(),
                session_id: "e1".into(),
                node_id: "n1".into(),
                rpc_id: i,
                tool_call_id: None,
                title: "run".into(),
                kind: Some("execute".into()),
                raw_input: None,
                options: "[]".into(),
                state: "new".into(),
                answer_option_id: None,
                created_ms: now_ms(),
                created_mono_ms: 1000,
                resolved_mono_ms: None,
                expires_ms: now_ms() + 60_000,
            })
            .unwrap();
        if answered {
            store
                .resolve_permission(&id, "answered", Some("allow_once"), 3000)
                .unwrap();
        }
    }
    // Provenance ingredients on e1.
    for (kind, payload) in [
        ("user_prompt", json!({"text": "ship the thing"})),
        ("permission_answer", json!({"option_id": "allow_once"})),
    ] {
        store
            .append_event(&NewEvent {
                session_id: "e1".into(),
                work_item_id: None,
                kind: kind.into(),
                ref_id: None,
                payload,
                at_ms: now_ms(),
                mono_ms: 1,
            })
            .unwrap();
    }

    let app = app(store.clone()).await;
    let (st, v) = call(&app, "GET", "/api/metrics?channel=work", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let m = &v["channels"][0];
    assert_eq!(m["channel"], "work");
    assert_eq!(m["accepted_changes"], 2);
    assert_eq!(m["rejected_changes"], 1);
    // 3 answers + 3 verdicts, over 2 accepted.
    assert_eq!(m["approvals"], 6);
    assert_eq!(m["approvals_per_accepted_change"], 3.0);
    // (800 + 200 + 1000) / 2: e3 was rejected and is not behind an accepted change.
    assert_eq!(m["tokens_per_accepted_change"], 1000.0);
    assert_eq!(m["tokens"], 7000);
    // Only anthropic is priced: 750 in * $3 + 250 out * $15, per million.
    let cost = m["cost_usd"].as_f64().unwrap();
    assert!(
        (cost - (750.0 * 3.0 + 250.0 * 15.0) / 1_000_000.0).abs() < 1e-9,
        "{cost}"
    );
    // 3 × 2s of permission latency + 3 × ~30s of review claim-to-decision.
    let human = m["human_seconds"].as_f64().unwrap();
    assert!(human > 90.0 && human < 100.0, "{human}");
    assert_eq!(m["agent_seconds"], 480.0);
    assert_eq!(m["sessions"], 4);

    // Provenance by a sha prefix, and by where it was published.
    let (st, p) = call(&app, "GET", "/api/provenance/abc1234", None).await;
    assert_eq!(st, StatusCode::OK, "{p}");
    assert_eq!(p["sha"], "abc1234567890");
    assert_eq!(p["implementing_session"]["model"], "m/a");
    assert_eq!(p["implementing_session"]["phase"], "execute");
    assert_eq!(p["review_session"]["model"], "m/reviewer");
    assert_eq!(p["policy_version"], 4);
    assert_eq!(p["work_item"]["title"], "Ship");
    assert_eq!(p["prompts"][0]["text"], "ship the thing");
    assert_eq!(p["approvals"][0]["kind"], "permission_answer");
    assert_eq!(p["checks"][0]["command"], "just check");
    assert_eq!(p["ai_verdict"]["verdict"], "approve");
    assert_eq!(p["published"], "https://github.test/pull/rv1");
    let (st, _) = call(&app, "GET", "/api/provenance/ffffffff", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(&app, "GET", "/api/provenance/abc", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn bindings_merge_by_dotted_key_and_the_channel_reports_its_ceiling() {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .channel_put("work", b"ring", r#"{"providers":["anthropic"]}"#)
        .unwrap();
    let app = app(store.clone()).await;
    let (st, v) = call(
        &app,
        "PUT",
        "/api/channels/work/bindings",
        Some(json!({"ceiling_tokens_per_day": 1000, "phases.review.model": "m/reviewer", "phases.execute.requires_plan": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["bindings"]["providers"][0], "anthropic",
        "existing keys kept"
    );
    assert_eq!(v["bindings"]["phases"]["review"]["model"], "m/reviewer");
    assert_eq!(v["bindings"]["phases"]["execute"]["requires_plan"], false);
    assert_eq!(v["handed_to"], 0, "standalone: nobody to hand to");
    // A null removes; a nested set keeps its siblings.
    let (_, v) = call(
        &app,
        "PUT",
        "/api/channels/work/bindings",
        Some(json!({"phases.review.budget_tokens": 5000, "providers": null})),
    )
    .await;
    assert!(v["bindings"]["providers"].is_null());
    assert_eq!(v["bindings"]["phases"]["review"]["model"], "m/reviewer");
    assert_eq!(v["bindings"]["phases"]["review"]["budget_tokens"], 5000);
    let (_, chans) = call(&app, "GET", "/api/channels", None).await;
    let work = chans
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "work")
        .unwrap();
    assert_eq!(work["ceiling"]["ceiling"], 1000);
    assert_eq!(work["ceiling"]["state"], "under");
    store
        .record_usage(&usage("s", "anthropic", 800, 50))
        .unwrap();
    let (_, chans) = call(&app, "GET", "/api/channels", None).await;
    let work = chans
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "work")
        .unwrap();
    assert_eq!(work["ceiling"]["state"], "near");
    assert_eq!(work["ceiling"]["usage_today"], 850);
    let (st, _) = call(
        &app,
        "PUT",
        "/api/channels/nope/bindings",
        Some(json!({"a": 1})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
