//! The numbers, from a seeded store: approvals and tokens per accepted
//! change, priced where a provider carries a price; provenance per commit;
//! channel bindings merged through the API.

#[path = "support/mod.rs"]
mod support;
use support::http::{call, call_local};
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
        harness_agent: None,
        harness_found: None,
        harness_protocol: None,
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
        legacy_ms: None,
        parent_session: None,
        continued_from: None,
        manifest_digest: None,
    }
}

fn review(id: &str, session: &str, sha: &str, state: &str, reviewer: Option<&str>) -> ReviewRow {
    ReviewRow {
        id: id.into(),
        session_id: Some(session.into()),
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
        lane: None,
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
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        request_id: None,
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
            discovered_by_lane: None,
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
                session_id: Some("e1".into()),
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
    assert_eq!(m["interventions"], 3);
    assert_eq!(m["human_wait_seconds"], 6.0);
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

#[test]
fn workflow_metrics_distinguish_verified_work_setup_failures_and_operator_intervention() {
    state::isolate();
    let store = Store::open_in_memory().unwrap();
    store.ensure_peer_node("n1").unwrap();
    for (id, channel, state) in [
        ("work", "work", "closed"),
        ("setup", "work", "failed"),
        ("runtime", "work", "failed"),
        ("other", "personal", "failed"),
    ] {
        let mut row = session(id, channel, "execute", None);
        row.created_ms = 1_000;
        row.state = state.into();
        store.insert_session(&row).unwrap();
    }
    for (id, kind, at_ms, payload) in [
        ("runtime", "session_started", 2_000, json!({})),
        (
            "work",
            "candidate_verified",
            6_000,
            json!({"candidate_id": "first"}),
        ),
        (
            "work",
            "candidate_verified",
            9_000,
            json!({"candidate_id": "second"}),
        ),
        (
            "work",
            "operator_question_answered",
            7_000,
            json!({"waiting_ms": 3_000}),
        ),
        (
            "work",
            "session_paused",
            8_000,
            json!({"source": "operator"}),
        ),
        (
            "work",
            "session_paused",
            9_000,
            json!({"source": "watchdog"}),
        ),
        (
            "work",
            "review_decision",
            9_000,
            json!({"source": "operator", "waiting_ms": 4_000}),
        ),
        (
            "work",
            "review_decision",
            9_500,
            json!({"source": "authority", "waiting_ms": 5_000}),
        ),
        (
            "other",
            "candidate_verified",
            5_000,
            json!({"candidate_id": "private"}),
        ),
    ] {
        store
            .append_event(&NewEvent {
                session_id: id.into(),
                work_item_id: None,
                kind: kind.into(),
                ref_id: None,
                payload,
                at_ms,
                mono_ms: at_ms - 1_000,
            })
            .unwrap();
    }
    let metrics = store.workflow_metrics("work", 500).unwrap();
    assert_eq!(metrics.setup_failures, 1);
    assert_eq!(metrics.verified_sessions, 1);
    assert_eq!(metrics.seconds_to_first_verified_candidate, Some(5.0));
    assert_eq!(metrics.interventions, 3);
    assert_eq!(metrics.question_wait_seconds, 3.0);
    assert_eq!(metrics.human_wait_seconds, 7.0);
    let empty = store.workflow_metrics("work", 10_000).unwrap();
    assert_eq!(empty.verified_sessions, 0);
    assert_eq!(empty.seconds_to_first_verified_candidate, None);
}

#[tokio::test]
async fn bindings_merge_by_dotted_key_and_the_channel_reports_its_ceiling() {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store
        .channel_put("work", b"ring", r#"{"providers":["anthropic"]}"#)
        .unwrap();
    let app = app(store.clone()).await;
    let (st, v) = call_local(
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
    let (_, v) = call_local(
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
    let (st, _) = call_local(
        &app,
        "PUT",
        "/api/channels/nope/bindings",
        Some(json!({"a": 1})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ---- per-request reconciliation ------------------------------------------

/// A store with one open session, on its first turn.
fn turn_store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store.ensure_peer_node("n1").unwrap();
    store
        .insert_session(&session("s-req", "work", "execute", None))
        .unwrap();
    assert_eq!(store.begin_turn("s-req").unwrap(), 1);
    store
}

fn gateway(store: &Store, id: &str, input: i64, output: i64) {
    store
        .record_usage(&UsageRow {
            request_id: Some(id.into()),
            ..usage("s-req", "anthropic", input, output)
        })
        .unwrap();
}

fn harness(store: &Store, id: Option<&str>, input: u64, output: u64) {
    tracon::metrics::record_harness_request(
        store,
        "s-req",
        &tracon::adapter::types::RequestUsage {
            request_id: id.map(str::to_string),
            input_tokens: input,
            output_tokens: output,
            cached_read_tokens: 0,
        },
    );
}

#[test]
fn requests_the_two_sources_both_counted_reconcile_one_by_one() {
    let store = turn_store();
    harness(&store, Some("chatcmpl-a"), 400, 100);
    gateway(&store, "chatcmpl-a", 400, 100);
    harness(&store, Some("chatcmpl-b"), 300, 50);
    gateway(&store, "chatcmpl-b", 300, 50);
    let r = tracon::metrics::settle_turn(&store, "s-req", Some(850), None);
    assert_eq!(r.state, "reconciled");
    assert!(r.event_kind().is_none());
    let ledger = store.turn_ledger("s-req", 10).unwrap();
    assert_eq!(
        (ledger[0].requests_matched, ledger[0].requests_unmatched),
        (Some(2), Some(0))
    );
    let usage = tracon::metrics::session_usage(&store, "s-req");
    assert_eq!(usage["requests_matched"], 2);
    assert_eq!(usage["requests_unmatched"], 0);
}

/// The totals agree within tolerance, so turn against turn this would pass;
/// request by request, one the gateway never saw is named.
#[test]
fn a_request_the_gateway_never_saw_is_a_mismatch_that_names_it() {
    let store = turn_store();
    harness(&store, Some("chatcmpl-a"), 4_000, 1_000);
    gateway(&store, "chatcmpl-a", 4_000, 1_000);
    harness(&store, Some("chatcmpl-ghost"), 60, 20);
    let r = tracon::metrics::settle_turn(&store, "s-req", Some(5_080), None);
    assert_eq!(r.state, "mismatch");
    assert_eq!(r.event_kind(), Some("usage_mismatch"));
    let detail = r.detail();
    assert_eq!(detail["requests_matched"], 1);
    assert_eq!(
        detail["unmatched_requests"],
        json!({ "harness_only": ["chatcmpl-ghost"], "gateway_only": [] })
    );
    let ledger = store.turn_ledger("s-req", 10).unwrap();
    assert_eq!(
        (ledger[0].requests_matched, ledger[0].requests_unmatched),
        (Some(1), Some(1))
    );
}

/// `model_usage.turn` is stamped when the stream finishes, which can be after
/// the next turn opened. The id still ties the request to the turn it was
/// made in, and the next turn does not count it as the gateway's alone.
#[test]
fn a_request_the_gateway_stamped_on_the_next_turn_still_matches() {
    let store = turn_store();
    harness(&store, Some("chatcmpl-a"), 400, 100);
    gateway(&store, "chatcmpl-a", 400, 100);
    harness(&store, Some("chatcmpl-late"), 300, 50);
    let first = tracon::metrics::settle_turn(&store, "s-req", Some(850), None);
    assert_eq!(first.requests.as_ref().unwrap().matched, 1);
    assert_eq!(store.begin_turn("s-req").unwrap(), 2);
    gateway(&store, "chatcmpl-late", 300, 50);
    harness(&store, Some("chatcmpl-c"), 200, 20);
    gateway(&store, "chatcmpl-c", 200, 20);
    let second = tracon::metrics::settle_turn(&store, "s-req", Some(220), None);
    let requests = second.requests.unwrap();
    assert_eq!(requests.matched, 1);
    assert!(
        requests.gateway_only.is_empty(),
        "the late row is the first turn's request: {requests:?}"
    );
    // The first turn closed before the late row was written, so it settled
    // with that request unmatched; once the row lands its ledger says so.
    assert_eq!(
        first.requests.unwrap().harness_only,
        [Some("chatcmpl-late".to_string())]
    );
    let ledger = store.turn_ledger("s-req", 10).unwrap();
    assert_eq!(
        (ledger[0].requests_matched, ledger[0].requests_unmatched),
        (Some(1), Some(0))
    );
    assert_eq!(
        (ledger[1].requests_matched, ledger[1].requests_unmatched),
        (Some(2), Some(0))
    );
    assert_eq!(
        ledger[1].state, "mismatch",
        "the verdict it settled with stands"
    );
}

#[test]
fn a_gateway_request_the_harness_never_reported_is_named() {
    let store = turn_store();
    harness(&store, Some("msg_a"), 400, 100);
    gateway(&store, "msg_a", 400, 100);
    gateway(&store, "msg_unseen", 30, 10);
    let r = tracon::metrics::settle_turn(&store, "s-req", Some(540), None);
    assert_eq!(r.state, "mismatch");
    assert_eq!(
        r.detail()["unmatched_requests"]["gateway_only"],
        json!(["msg_unseen"])
    );
}

/// Claude Code reports a turn's usage only as a total: its
/// turns carry no per-request verdict, and the session says null, not zero.
#[test]
fn a_harness_that_reports_no_requests_leaves_the_fields_empty() {
    let store = turn_store();
    gateway(&store, "msg_a", 400, 100);
    let r = tracon::metrics::settle_turn(&store, "s-req", Some(500), None);
    assert_eq!(r.state, "reconciled");
    assert!(r.requests.is_none());
    assert!(r.detail().get("unmatched_requests").is_none());
    let ledger = store.turn_ledger("s-req", 10).unwrap();
    assert_eq!(
        (ledger[0].requests_matched, ledger[0].requests_unmatched),
        (None, None)
    );
    let usage = tracon::metrics::session_usage(&store, "s-req");
    assert!(usage["requests_matched"].is_null());
    assert!(usage["requests_unmatched"].is_null());
}

/// Usage at an instant, on a channel, from a provider and model.
fn usage_at(
    channel: &str,
    session: Option<&str>,
    provider: &str,
    model: &str,
    at_ms: i64,
) -> UsageRow {
    UsageRow {
        channel: channel.into(),
        node_id: "n1".into(),
        session_id: session.map(str::to_string),
        provider: provider.into(),
        model: Some(model.into()),
        at_ms,
        input_tokens: 100,
        output_tokens: 10,
        requests: 1,
        cache_read_tokens: 40,
        cache_write_tokens: 5,
        request_id: None,
    }
}

/// Two days of usage on `work` from two providers, with a call either side
/// of the window `[day, day + 2 days)`, and a little on `personal`.
fn seeded_usage(store: &Store) -> i64 {
    use tracon::corpus::promote::chrono_free::day_start_of;
    let day = day_start_of(now_ms()) - 3 * 86_400_000;
    for (channel, session, provider, model, at) in [
        ("work", Some("e1"), "openai-codex", "gpt-5", day - 1),
        ("work", Some("e1"), "openai-codex", "gpt-5", day),
        ("work", Some("e2"), "openai-codex", "gpt-5", day + 3_600_000),
        (
            "work",
            Some("e2"),
            "openai-codex",
            "gpt-5-mini",
            day + 7_200_000,
        ),
        ("work", Some("e1"), "anthropic", "claude", day + 86_400_000),
        ("work", None, "openai-codex", "gpt-5", day + 86_400_000 + 1),
        (
            "work",
            Some("e2"),
            "openai-codex",
            "gpt-5",
            day + 2 * 86_400_000,
        ),
        ("personal", Some("p1"), "openai-codex", "gpt-5", day + 1),
    ] {
        store
            .record_usage(&usage_at(channel, session, provider, model, at))
            .unwrap();
    }
    day
}

#[test]
fn a_usage_report_groups_by_model_day_or_session_inside_its_window() {
    use tracon::corpus::promote::chrono_free::local_date;
    use tracon::store::{UsageGroup, UsageQuery};
    state::isolate();
    let store = Store::open_in_memory().unwrap();
    store.ensure_peer_node("n1").unwrap();
    let day = seeded_usage(&store);
    let window = |group| UsageQuery {
        channel: Some("work"),
        since_ms: day,
        until_ms: Some(day + 2 * 86_400_000),
        group,
        ..UsageQuery::default()
    };

    // Per model: the call a millisecond before the window and the one at its
    // end are both left out, and so is the other channel.
    let by_model = store.usage_report(&window(UsageGroup::Model)).unwrap();
    let keys: Vec<_> = by_model
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_deref().unwrap(), r.requests))
        .collect();
    assert_eq!(
        keys,
        [
            ("anthropic", "claude", 1),
            ("openai-codex", "gpt-5", 3),
            ("openai-codex", "gpt-5-mini", 1)
        ]
    );
    let gpt5 = &by_model[1];
    assert_eq!((gpt5.input_tokens, gpt5.output_tokens), (300, 30));
    assert_eq!((gpt5.cache_read_tokens, gpt5.cache_write_tokens), (120, 15));
    // e1 and e2; the call outside any session counts no session.
    assert_eq!(gpt5.sessions, 2);
    assert!(gpt5.day.is_none() && gpt5.session_id.is_none());

    // Per local day, on the same boundary as the ceiling's "today".
    let by_day = store.usage_report(&window(UsageGroup::Day)).unwrap();
    let keys: Vec<_> = by_day
        .iter()
        .map(|r| (r.day.clone().unwrap(), r.provider.as_str(), r.requests))
        .collect();
    let (d0, d1) = (local_date(day), local_date(day + 86_400_000));
    assert_ne!(d0, d1);
    assert_eq!(
        keys,
        [
            (d0.clone(), "openai-codex", 3),
            (d1.clone(), "anthropic", 1),
            (d1, "openai-codex", 1),
        ]
    );

    // Per session, with a provider filter.
    let by_session = store
        .usage_report(&UsageQuery {
            provider: Some("openai-codex"),
            ..window(UsageGroup::Session)
        })
        .unwrap();
    let keys: Vec<_> = by_session
        .iter()
        .map(|r| (r.session_id.as_deref(), r.requests))
        .collect();
    assert_eq!(keys, [(None, 1), (Some("e1"), 1), (Some("e2"), 2)]);

    // A model filter and a session filter narrow it further.
    let one = store
        .usage_report(&UsageQuery {
            model: Some("gpt-5"),
            session_id: Some("e2"),
            ..window(UsageGroup::Model)
        })
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].requests, 1);

    // Without an end the window runs to now and takes the later call in.
    let open = store
        .usage_report(&UsageQuery {
            until_ms: None,
            provider: Some("openai-codex"),
            model: Some("gpt-5"),
            ..window(UsageGroup::Model)
        })
        .unwrap();
    assert_eq!(open[0].requests, 4);
}

#[test]
fn a_session_row_says_what_the_session_was_for_and_how_its_change_was_judged() {
    use tracon::store::{UsageGroup, UsageQuery};
    state::isolate();
    let store = Store::open_in_memory().unwrap();
    store.ensure_peer_node("n1").unwrap();
    let item = tracon::corpus::work::create(
        &store,
        &Bus::new(),
        "n1",
        tracon::corpus::work::NewWork {
            channel: "work".into(),
            project_id: None,
            title: "Ship the export".into(),
            body: String::new(),
            deps: vec![],
            priority: 0,
            discovered_from: None,
            discovered_by_session: None,
            discovered_by_lane: None,
        },
    )
    .unwrap();
    store
        .insert_session(&session("e1", "work", "execute", Some(item.id.as_str())))
        .unwrap();
    store
        .insert_session(&session("e2", "work", "execute", None))
        .unwrap();
    store
        .insert_review(&review("rv1", "e1", "abc1234567890", "approved", None))
        .unwrap();
    store
        .insert_review(&review("rv2", "e2", "def1234567890", "rejected", None))
        .unwrap();
    let day = seeded_usage(&store);
    let rows = store
        .usage_report(&UsageQuery {
            channel: Some("work"),
            since_ms: day,
            provider: Some("openai-codex"),
            group: UsageGroup::Session,
            ..UsageQuery::default()
        })
        .unwrap();
    let e1 = rows
        .iter()
        .find(|r| r.session_id.as_deref() == Some("e1"))
        .unwrap();
    assert_eq!(e1.title.as_deref(), Some("Ship the export"));
    assert_eq!(e1.work_item_id.as_deref(), Some(item.id.as_str()));
    assert_eq!(e1.review.as_deref(), Some("accepted"));
    // No work item: the review's title stands in.
    let e2 = rows
        .iter()
        .find(|r| r.session_id.as_deref() == Some("e2"))
        .unwrap();
    assert_eq!(e2.title.as_deref(), Some("feat: rv2"));
    assert_eq!(e2.review.as_deref(), Some("rejected"));
}

/// A GET whose body is text, with its headers.
async fn get_text(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .uri(uri)
        .header("host", "127.0.0.1:7420")
        .body(axum::body::Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn the_usage_api_keeps_its_old_rows_adds_cost_and_exports_the_same_rows_as_csv() {
    use tracon::corpus::promote::chrono_free::local_date;
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let day = seeded_usage(&store);
    let app = app(store.clone()).await;

    // Only `channel` and `since_ms`, as callers have always asked: one row
    // per provider and model, with the old fields where they were.
    let (st, v) = call(
        &app,
        "GET",
        &format!("/api/usage?channel=work&since_ms={day}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let totals = v["totals"].as_array().unwrap();
    assert_eq!(totals.len(), 3);
    let claude = &totals[0];
    assert_eq!(claude["channel"], "work");
    assert_eq!(claude["provider"], "anthropic");
    assert_eq!(claude["model"], "claude");
    assert_eq!(claude["requests"], 1);
    assert_eq!(claude["input_tokens"], 100);
    assert_eq!(claude["output_tokens"], 10);
    assert_eq!(claude["cache_read_tokens"], 40);
    assert_eq!(claude["sessions"], 1);
    // Priced as the metrics price it: anthropic at $3 in and $15 out per million.
    let cost = claude["cost_usd"].as_f64().unwrap();
    assert!(
        (cost - (100.0 * 3.0 + 10.0 * 15.0) / 1e6).abs() < 1e-12,
        "{cost}"
    );
    assert!(
        totals[1]["cost_usd"].is_null(),
        "openai-codex carries no price"
    );
    assert!(
        claude.get("day").is_none(),
        "grouping fields appear only when grouped"
    );

    let (st, v) = call(&app, "GET", "/api/usage?group=week", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // The same query as CSV, by day for one provider.
    let until = day + 2 * 86_400_000;
    let uri = format!(
        "/api/usage.csv?channel=work&since_ms={day}&until_ms={until}&provider=openai-codex&group=day"
    );
    let (st, headers, body) = get_text(&app, &uri).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        headers["content-disposition"],
        format!(
            "attachment; filename=\"tracon-usage-work-{}-{}.csv\"",
            local_date(day),
            local_date(until - 1)
        )
    );
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines[0],
        "channel,provider,model,day,session_id,title,work_item_id,review,requests,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,sessions,cost_usd"
    );
    assert_eq!(
        &lines[1..],
        [
            format!(
                "work,openai-codex,,{},,,,,3,300,30,120,15,2,",
                local_date(day)
            ),
            format!(
                "work,openai-codex,,{},,,,,1,100,10,40,5,0,",
                local_date(day + 86_400_000)
            ),
        ]
    );
    // Stable: the same window exports the same bytes.
    let (_, _, again) = get_text(&app, &uri).await;
    assert_eq!(body, again);
    // Every channel, when none is named.
    let (_, headers, _) = get_text(&app, &format!("/api/usage.csv?since_ms={day}")).await;
    assert!(headers["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment; filename=\"tracon-usage-all-"));
}

/// 2026-10-01T00:00Z.
const OCT_1_UTC: i64 = 1_790_812_800_000;

#[test]
fn a_day_row_is_the_callers_local_day_not_the_nodes() {
    use tracon::store::{UsageGroup, UsageQuery};
    state::isolate();
    let store = Store::open_in_memory().unwrap();
    store.ensure_peer_node("n1").unwrap();
    // 23:30 on 2026-09-30 in CDT (UTC-5), already 04:30 on 10-01 in UTC.
    let late = OCT_1_UTC + 4 * 3_600_000 + 30 * 60_000;
    store
        .record_usage(&usage_at("work", None, "anthropic", "claude", late))
        .unwrap();
    let day_in = |tz_offset_minutes| {
        store
            .usage_report(&UsageQuery {
                since_ms: OCT_1_UTC - 86_400_000,
                group: UsageGroup::Day,
                tz_offset_minutes,
                ..UsageQuery::default()
            })
            .unwrap()
            .into_iter()
            .map(|r| r.day.unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(day_in(Some(-300)), ["2026-09-30"]);
    // East of UTC it is the morning of 10-01; with none given, the node's
    // own offset decides.
    assert_eq!(day_in(Some(330)), ["2026-10-01"]);
    assert_eq!(
        day_in(None),
        [tracon::corpus::promote::chrono_free::local_date(late)]
    );
}

#[tokio::test]
async fn a_csv_is_named_for_the_days_the_caller_picked_in_the_callers_zone() {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    // 23:30 CDT on 10-15, the last evening of the window.
    let last_evening = OCT_1_UTC + 15 * 86_400_000 + 4 * 3_600_000 + 30 * 60_000;
    store
        .record_usage(&usage_at("work", None, "anthropic", "claude", last_evening))
        .unwrap();
    let app = app(store.clone()).await;
    // 2026-10-01 to 2026-10-15 picked in CDT: local midnight to local midnight.
    let (since, until) = (
        OCT_1_UTC + 5 * 3_600_000,
        OCT_1_UTC + 15 * 86_400_000 + 5 * 3_600_000,
    );
    let (st, headers, body) = get_text(
        &app,
        &format!(
            "/api/usage.csv?since_ms={since}&until_ms={until}&group=day&tz_offset_minutes=-300"
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"tracon-usage-all-2026-10-01-2026-10-15.csv\""
    );
    assert_eq!(
        body.lines().nth(1).unwrap(),
        "work,anthropic,,2026-10-15,,,,,1,100,10,40,5,0,0.000450"
    );
}

#[tokio::test]
async fn a_hostile_offset_or_window_edge_answers_rather_than_overflowing() {
    state::isolate();
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.ensure_peer_node("n1").unwrap();
    let day = seeded_usage(&store);
    let app = app(store.clone()).await;
    let by_day = |extra: &str| format!("/api/usage?group=day&since_ms={day}{extra}");
    let (st, plain) = call(&app, "GET", &by_day(""), None).await;
    assert_eq!(st, StatusCode::OK, "{plain}");
    // An offset no zone has is ignored: the rows are the node's own.
    for offset in ["-9223372036854775808", "99999"] {
        let (st, v) = call(
            &app,
            "GET",
            &by_day(&format!("&tz_offset_minutes={offset}")),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{offset}: {v}");
        assert_eq!(v["totals"], plain["totals"], "{offset}");
    }
    // Window edges at the ends of i64 still name a file.
    let (st, _, body) = get_text(
        &app,
        "/api/usage.csv?since_ms=9223372036854775807&until_ms=1&tz_offset_minutes=840",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}
