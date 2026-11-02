//! An approval decides one call, once. What runs is the call the operator
//! read, or their edit of it; an answer that names other arguments, or comes
//! a second time, decides nothing; and asking again after it settled is a new
//! approval. A session's audit lists each call it made to the node's tools,
//! how it was decided and by whom, with its arguments redacted.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};
use support::http::call;
use support::state;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

const SESSION: &str = "s-audit";

async fn session(h: &Harness) -> String {
    h.manager
        .register_tool_token_for_test(SESSION, "work")
        .await
}

/// One tool call from the session: whether it failed, and what it said.
async fn tool(h: &Harness, token: &str, name: &str, args: Value) -> (bool, Value) {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{SESSION}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = h.harness.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    (
        v["result"]["isError"] == json!(true),
        serde_json::from_str(text).unwrap_or(json!(text)),
    )
}

async fn ask(h: &Harness, token: &str, name: &str, args: Value) -> String {
    let (failed, out) = tool(h, token, name, args).await;
    assert!(!failed, "{out}");
    assert_eq!(out["state"], "awaiting_operator", "{out}");
    out["approval_id"].as_str().unwrap().to_string()
}

async fn answer(h: &Harness, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        &h.operator,
        "POST",
        &format!("/api/permissions/{id}/answer"),
        Some(body),
    )
    .await
}

/// What the approval page reads: the arguments and the hash an answer names.
async fn read(h: &Harness, id: &str) -> Value {
    let (status, v) = call(&h.operator, "GET", &format!("/api/approvals/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}

async fn settled(h: &Harness, id: &str) -> String {
    for _ in 0..200 {
        let state = h.store.get_approval(id).unwrap().unwrap().state;
        if !matches!(state.as_str(), "pending" | "running") {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("approval {id} never settled");
}

fn body(h: &Harness, slug: &str) -> Option<String> {
    h.store.doc_get("work", slug).unwrap().map(|d| d.body)
}

#[tokio::test]
async fn an_allowed_approval_runs_once_and_is_never_decided_again() {
    state::isolate();
    let h = harness().await;
    let token = session(&h).await;
    let id = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-once", "body": "as asked" }),
    )
    .await;
    let hash = read(&h, &id).await["arguments_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        hash,
        h.store.get_approval(&id).unwrap().unwrap().arguments_hash()
    );

    // An answer naming arguments other than the ones held decides nothing.
    let (status, v) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments_hash": "0".repeat(64) }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(h.store.get_approval(&id).unwrap().unwrap().state, "pending");
    assert_eq!(body(&h, "plan-once"), None);

    let (status, v) = answer(
        &h,
        &id,
        json!({ "option_id": "allow_once", "arguments_hash": hash }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(settled(&h, &id).await, "succeeded");
    assert_eq!(body(&h, "plan-once").as_deref(), Some("as asked"));

    // The same answer again, an edited one, or a refusal: each is refused,
    // and nothing runs a second time.
    for again in [
        json!({ "option_id": "allow_once", "arguments_hash": hash }),
        json!({ "option_id": "allow_once",
                "arguments": { "slug": "plan-once", "body": "replayed" } }),
        json!({ "option_id": "reject_once", "reason": "too late" }),
    ] {
        let (status, v) = answer(&h, &id, again.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{again}: {v}");
    }
    assert_eq!(body(&h, "plan-once").as_deref(), Some("as asked"));
    let a = h.store.get_approval(&id).unwrap().unwrap();
    assert_eq!(a.state, "succeeded");
    assert_eq!(a.edited_arguments, None);

    // A settled approval is not reused: the same call asks anew.
    let again = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-once", "body": "as asked" }),
    )
    .await;
    assert_ne!(again, id);
    assert_eq!(
        h.store.get_approval(&again).unwrap().unwrap().state,
        "pending"
    );
}

/// The operator's edit is what runs, and is recorded as theirs; an answer
/// whose edit is refused leaves the call waiting with nothing decided.
#[tokio::test]
async fn what_runs_is_the_call_as_read_or_the_operators_edit_of_it() {
    state::isolate();
    let h = harness().await;
    let token = session(&h).await;
    let first = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-a", "body": "draft a" }),
    )
    .await;
    // A second, different ask is its own approval, and allowing the first
    // runs the first's arguments, not the latest.
    let second = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-a", "body": "draft b" }),
    )
    .await;
    assert_ne!(first, second);
    let (status, _) = answer(&h, &first, json!({ "option_id": "allow_once" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settled(&h, &first).await, "succeeded");
    assert_eq!(body(&h, "plan-a").as_deref(), Some("draft a"));
    assert_eq!(
        h.store.get_approval(&second).unwrap().unwrap().state,
        "pending"
    );

    let edited = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-e", "body": "the agent's words" }),
    )
    .await;
    let (status, v) = answer(
        &h,
        &edited,
        json!({ "option_id": "allow_once",
                "arguments": { "slug": "plan-other", "body": "the operator's words" } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(
        h.store.get_approval(&edited).unwrap().unwrap().state,
        "pending"
    );
    let (status, v) = answer(
        &h,
        &edited,
        json!({ "option_id": "allow_once",
                "arguments": { "slug": "plan-e", "body": "the operator's words" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(settled(&h, &edited).await, "succeeded");
    assert_eq!(body(&h, "plan-e").as_deref(), Some("the operator's words"));
}

/// An egress card opens the host it asked about and runs no call, so an
/// edit to it would be dropped on the floor. It is refused instead.
#[tokio::test]
async fn an_egress_card_refuses_an_edit_rather_than_dropping_it() {
    state::isolate();
    let h = harness().await;
    session(&h).await;
    let now = tracon::store::now_ms();
    h.store
        .insert_approval(&tracon::store::approvals::ApprovalRow {
            id: "egress-1".into(),
            channel: "work".into(),
            session_id: Some(SESSION.into()),
            node_id: "n1".into(),
            lane: None,
            tool: "request_egress".into(),
            arguments: json!({ "host": "registry.example.test" }).to_string(),
            request_key: "egress-key".into(),
            title: "reach registry.example.test".into(),
            state: "pending".into(),
            answer_option_id: None,
            edited_arguments: None,
            result: None,
            reason: None,
            operator_note: None,
            claimed_ms: None,
            created_ms: now,
            decided_ms: None,
            finished_ms: None,
            expires_ms: now + 60_000,
        })
        .unwrap();
    for option in ["allow_once", "allow_session"] {
        let (status, v) = answer(
            &h,
            "egress-1",
            json!({ "option_id": option, "arguments": { "host": "elsewhere.example.test" } }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{option}: {v}");
        assert_eq!(
            h.store.get_approval("egress-1").unwrap().unwrap().state,
            "pending"
        );
    }
}

#[tokio::test]
async fn a_sessions_audit_lists_each_call_with_who_decided_it() {
    state::isolate();
    let h = harness().await;
    let token = session(&h).await;
    h.tools.policy.write().rules.insert(
        0,
        tracon::policy::Rule {
            id: "no-secret-notes".into(),
            verdict: tracon::policy::Verdict::Deny,
            reason: "not that one".into(),
            kinds: vec![],
            matches: vec!["note-forbidden".into()],
            args: Default::default(),
            channels: vec![],
        },
    );

    let (failed, out) = tool(&h, &token, "doc_search", json!({ "query": "anything" })).await;
    assert!(!failed, "{out}");
    let (failed, _) = tool(&h, &token, "doc_read", json!({ "slug": "note-forbidden" })).await;
    assert!(failed);
    let (failed, _) = tool(&h, &token, "doc_write", json!({ "body": "no slug" })).await;
    assert!(failed);
    let approved = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-1", "body": "ok\npassword=hunter2\nmore" }),
    )
    .await;
    let edited = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-2", "body": "two" }),
    )
    .await;
    let rejected = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-3", "body": "three" }),
    )
    .await;
    let waiting = ask(
        &h,
        &token,
        "doc_write",
        json!({ "slug": "plan-4", "body": "four" }),
    )
    .await;
    assert_eq!(
        answer(&h, &approved, json!({ "option_id": "allow_once" }))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        answer(
            &h,
            &edited,
            json!({ "option_id": "allow_once",
                    "arguments": { "slug": "plan-2", "body": "two, edited" } }),
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        answer(
            &h,
            &rejected,
            json!({ "option_id": "reject_once", "reason": "no" })
        )
        .await
        .0,
        StatusCode::OK
    );
    settled(&h, &approved).await;
    settled(&h, &edited).await;

    let (status, v) = call(
        &h.operator,
        "GET",
        &format!("/api/sessions/{SESSION}/audit"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let calls = v["calls"].as_array().unwrap();
    let seen: Vec<(&str, &str)> = calls
        .iter()
        .map(|c| {
            (
                c["tool"].as_str().unwrap_or(""),
                c["decision"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("doc_search", "policy_allowed"),
            ("doc_read", "denied"),
            ("doc_write", "refused"),
            ("doc_write", "approved"),
            ("doc_write", "edited"),
            ("doc_write", "rejected"),
            ("doc_write", "pending"),
        ],
        "{v}"
    );
    assert_eq!(calls[0]["decided_by"], "policy", "{v}");
    assert_eq!(calls[1]["decided_by"], "policy", "{v}");
    assert_eq!(calls[1]["rule"], "no-secret-notes", "{v}");
    assert_eq!(calls[2]["decided_by"], "node", "{v}");
    assert!(
        calls[2]["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "{v}"
    );
    for c in &calls[3..6] {
        assert!(
            c["decided_by"]
                .as_str()
                .is_some_and(|by| by.starts_with("operator")),
            "{c}"
        );
        assert!(c["decided_ms"].is_i64(), "{c}");
    }
    assert_eq!(calls[3]["approval_id"], approved.as_str());
    assert_eq!(calls[3]["approval_state"], "succeeded");
    assert_eq!(
        calls[3]["arguments"]["body"], "ok\n[redacted secret-looking content]\nmore",
        "{v}"
    );
    assert_eq!(calls[4]["edited_arguments"]["body"], "two, edited", "{v}");
    assert_eq!(calls[5]["reason"], "no", "{v}");
    assert_eq!(calls[6]["approval_id"], waiting.as_str());
    assert!(calls[6]["decided_by"].is_null(), "{v}");
    assert!(calls.iter().all(|c| c["at_ms"].is_i64()), "{v}");
}
