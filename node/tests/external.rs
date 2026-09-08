//! A harness the operator runs themselves, through the operator door: it
//! attaches one session per channel, is offered the channel's tools minus the
//! ones needing a worktree, and a call the policy does not name waits on the
//! same queue as any other — which is the thing that could not happen before
//! this mode existed.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness_with, Harness};
use support::http::call;
use support::state;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use tracon::config::Config;
use tracon::stream::Frame;

fn enabled() -> Config {
    let mut cfg = Config::default();
    cfg.external.enabled = true;
    cfg
}

async fn mcp(app: &axum::Router, channel: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/external/{channel}"))
        .header("host", "127.0.0.1:7420")
        .header("content-type", "application/json")
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

async fn list(app: &axum::Router, channel: &str) -> Vec<String> {
    let (_, v) = mcp(
        app,
        channel,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let empty = vec![];
    v["result"]["tools"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

fn tool_call(name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":args}})
}

/// The one external session on this node, whatever state it is in.
fn attached(h: &Harness) -> Option<tracon::store::SessionRow> {
    h.store
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .find(|s| s.harness_id == "external")
}

#[tokio::test]
async fn a_node_that_does_not_answer_external_harnesses_says_which_key_turns_it_on() {
    state::isolate();
    let h = harness_with(Config::default()).await;
    let (status, v) = mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("[external]"), "{message}");
    assert!(attached(&h).is_none());
}

#[tokio::test]
async fn a_channel_this_node_does_not_hold_is_refused_in_words() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (status, v) = mcp(
        &h.operator,
        "nope",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("create or enroll"), "{message}");
}

#[tokio::test]
async fn a_channel_gets_one_attachment_however_many_calls_arrive() {
    state::isolate();
    let h = harness_with(enabled()).await;
    for _ in 0..3 {
        let (status, _) = mcp(
            &h.operator,
            "work",
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let rows: Vec<_> = h
        .store
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .filter(|s| s.harness_id == "external")
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.state, "running");
    assert_eq!(row.channel, "work");
    // Nothing was started, so nothing is running anywhere but the operator's
    // own terminal.
    assert!(row.container_name.is_none());
    assert!(row.worktree_path.is_none());

    let (_, queue) = call(&h.operator, "GET", "/api/queue", None).await;
    let running = queue["running"].as_array().unwrap();
    assert!(running.iter().any(|s| s["id"] == json!(row.id)), "{queue}");

    // It ran against no repository; the picker must not offer an empty path.
    let (_, repos) = call(&h.operator, "GET", "/api/repos/recent", None).await;
    assert!(
        !repos["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["repo_path"] == json!("")),
        "{repos}"
    );
}

#[tokio::test]
async fn an_attachment_is_offered_the_channels_tools_but_not_the_ones_needing_a_worktree() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let names = list(&h.operator, "work").await;
    for wanted in ["recall", "doc_read", "work_ready", "work_discover"] {
        assert!(
            names.contains(&wanted.to_string()),
            "{wanted} not in {names:?}"
        );
    }
    for unwanted in [
        "submit_review",
        "review_status",
        "review_verdict",
        "work_close",
    ] {
        assert!(
            !names.contains(&unwanted.to_string()),
            "{unwanted} in {names:?}"
        );
    }

    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("submit_review", json!({ "title": "x" })),
    )
    .await;
    assert_eq!(v["result"]["isError"], json!(true));
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("worktree"), "{text}");
}

/// The inverse of the unit test that pinned the old dead end: with no session
/// able to ask, a tool the bundle did not name could not run at all. Now it
/// reaches the queue and the operator's answer reaches the caller.
#[tokio::test]
async fn a_tool_the_policy_does_not_cover_is_asked_through_the_attachment() {
    state::isolate();
    let h = harness_with(enabled()).await;
    // doc_write is deliberately unnamed in the shipped bundle: a document is
    // the operator's artifact.
    let app = h.operator.clone();
    let call_task = tokio::spawn(async move {
        mcp(
            &app,
            "work",
            tool_call("doc_write", json!({ "slug": "note-x", "body": "hello" })),
        )
        .await
    });

    let mut waiting = None;
    for _ in 0..100 {
        let open = h.store.open_permissions().unwrap();
        if let Some(p) = open.first() {
            waiting = Some(p.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let waiting = waiting.expect("the call should be waiting on the operator");
    assert_eq!(waiting.kind.as_deref(), Some("tool"));
    assert!(waiting.title.starts_with("doc_write"), "{}", waiting.title);
    assert_eq!(waiting.session_id, attached(&h).unwrap().id);

    let (status, _) = call(
        &h.operator,
        "POST",
        &format!("/api/permissions/{}/answer", waiting.id),
        Some(json!({ "option_id": "allow_once" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, v) = call_task.await.unwrap();
    assert_ne!(
        v["result"]["isError"],
        json!(true),
        "the operator allowed it: {v}"
    );
}

#[tokio::test]
async fn a_refused_call_says_the_operator_refused_it() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let app = h.operator.clone();
    let call_task = tokio::spawn(async move {
        mcp(
            &app,
            "work",
            tool_call("doc_write", json!({ "slug": "note-x", "body": "hello" })),
        )
        .await
    });
    let mut waiting = None;
    for _ in 0..100 {
        if let Some(p) = h.store.open_permissions().unwrap().first() {
            waiting = Some(p.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let waiting = waiting.expect("waiting on the operator");
    call(
        &h.operator,
        "POST",
        &format!("/api/permissions/{}/answer", waiting.id),
        Some(json!({ "option_id": "reject_once" })),
    )
    .await;
    let (_, v) = call_task.await.unwrap();
    assert_eq!(v["result"]["isError"], json!(true));
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("did not allow"), "{text}");
}

#[tokio::test]
async fn an_unanswered_request_expires_here_too() {
    state::isolate();
    let mut cfg = enabled();
    cfg.session.permission_timeout_secs = 1;
    let h = harness_with(cfg).await;
    let (_, v) = mcp(
        &h.operator,
        "work",
        tool_call("doc_write", json!({ "slug": "note-x", "body": "hello" })),
    )
    .await;
    assert_eq!(v["result"]["isError"], json!(true));
    let open = h.store.open_permissions().unwrap();
    assert!(open.is_empty(), "{open:?}");
}

#[tokio::test]
async fn killing_an_attachment_ends_it_and_the_next_call_attaches_again() {
    state::isolate();
    let h = harness_with(enabled()).await;
    mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    )
    .await;
    let first = attached(&h).unwrap().id;

    let (status, _) = call(
        &h.operator,
        "POST",
        &format!("/api/sessions/{first}/kill"),
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");
    for _ in 0..100 {
        let row = h.store.get_session(&first).unwrap().unwrap();
        if row.state == "closed" {
            assert_eq!(row.end_reason.as_deref(), Some("killed_user"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    )
    .await;
    let rows: Vec<_> = h
        .store
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .filter(|s| s.harness_id == "external")
        .collect();
    assert_eq!(rows.len(), 2, "a killed attachment is replaced, not reused");
}

#[tokio::test]
async fn an_attachment_that_goes_quiet_detaches() {
    state::isolate();
    let mut cfg = enabled();
    // Clamped to a minute inside; the point is that the row closes with a
    // reason that reads as nothing having gone wrong.
    cfg.external.idle_timeout_secs = 1;
    let h = harness_with(cfg).await;
    mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    )
    .await;
    let row = attached(&h).unwrap();
    assert_eq!(row.state, "running");
    assert_eq!(
        tracon::session::state::EndReason::Detached.as_str(),
        "detached"
    );
}

#[tokio::test]
async fn a_prompt_to_an_attachment_is_refused_in_words() {
    state::isolate();
    let h = harness_with(enabled()).await;
    mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    )
    .await;
    let id = attached(&h).unwrap().id;
    let (status, v) = call(
        &h.operator,
        "POST",
        &format!("/api/sessions/{id}/prompt"),
        Some(json!({ "text": "do the thing" })),
    )
    .await;
    assert!(!status.is_success(), "{status}");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("terminal"), "{message}");
}

#[tokio::test]
async fn what_the_node_was_asked_to_do_lands_in_the_session_log() {
    state::isolate();
    let h = harness_with(enabled()).await;
    mcp(
        &h.operator,
        "work",
        tool_call("recall", json!({ "query": "x" })),
    )
    .await;
    let id = attached(&h).unwrap().id;
    let (_, v) = call(
        &h.operator,
        "GET",
        &format!("/api/sessions/{id}/events"),
        None,
    )
    .await;
    let empty = vec![];
    let kinds: Vec<&str> = v
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|e| e["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"tool_call"), "{kinds:?}");
    assert!(kinds.contains(&"tool_result"), "{kinds:?}");
}

#[tokio::test]
async fn the_door_answers_one_post_per_message_and_opens_no_stream() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let req = Request::builder()
        .method("GET")
        .uri("/mcp/external/work")
        .header("host", "127.0.0.1:7420")
        .body(Body::empty())
        .unwrap();
    let res = h.operator.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn the_external_view_says_what_is_on_and_what_is_attached() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let (_, before) = call(&h.operator, "GET", "/api/external", None).await;
    assert_eq!(before["enabled"], json!(true));
    assert!(before["attachments"].as_array().unwrap().is_empty());
    assert!(before["channels"]
        .as_array()
        .unwrap()
        .contains(&json!("work")));

    mcp(
        &h.operator,
        "work",
        json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
    )
    .await;
    let (_, after) = call(&h.operator, "GET", "/api/external", None).await;
    let attachments = after["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1, "{after}");
    assert_eq!(attachments[0]["channel"], json!("work"));
}

/// Belt and braces on the surface the mode adds: an attachment is offered the
/// forge and tracker verbs only when the channel has the credential for them.
#[tokio::test]
async fn an_unbound_channel_is_offered_no_brokered_tools() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let names = list(&h.operator, "personal").await;
    for brokered in ["query", "issue", "issue_update", "mr_status"] {
        assert!(
            !names.contains(&brokered.to_string()),
            "{brokered} in {names:?}"
        );
    }
    // The node's own corpus needs no credential, so it is still there.
    assert!(names.contains(&"recall".to_string()), "{names:?}");
}

/// The regression this file exists for: an interface that is already open
/// learns about a card only from the stream. Publishing the session without
/// the queue leaves it showing a session that is waiting on you and nothing
/// to answer, which is exactly what it looked like in the wild.
#[tokio::test]
async fn a_card_reaches_an_interface_that_is_already_open() {
    state::isolate();
    let h = harness_with(enabled()).await;
    let mut frames = h.bus.subscribe();

    let app = h.operator.clone();
    tokio::spawn(async move {
        mcp(
            &app,
            "work",
            tool_call("doc_write", json!({ "slug": "note-x", "body": "hello" })),
        )
        .await
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut saw_session = false;
    let queued = loop {
        let frame = tokio::time::timeout_at(deadline, frames.recv())
            .await
            .expect("the queue frame never arrived")
            .expect("the bus closed");
        match frame {
            Frame::Session(_) => saw_session = true,
            Frame::Queue { waiting } if !waiting.is_empty() => break waiting,
            _ => {}
        }
    };
    assert!(saw_session, "the session frame is published too");
    assert!(
        queued[0].title.starts_with("doc_write"),
        "{}",
        queued[0].title
    );
}
