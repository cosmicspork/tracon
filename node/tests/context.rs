//! A work item's selected context through its surfaces.
//!
//! The claims under test: an item has no selection until the operator makes
//! one, and is whole without it; the selection is a document the operator
//! writes and a session cannot; and what each attempt received is recorded,
//! revisioned by what was received, with the changes since the attempt before
//! it and anything the node could not deliver named.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};
use support::http::call;
use support::state;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::corpus::context;

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

async fn an_item(h: &Harness, title: &str) -> String {
    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({ "channel": "personal", "title": title })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v["id"].as_str().unwrap().to_string()
}

async fn a_doc(h: &Harness, slug: &str, body: &str) {
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/docs/personal/{slug}"),
        Some(json!({ "body": body })),
    )
    .await;
    assert!(st.is_success(), "{st} {v}");
}

#[tokio::test]
async fn the_operator_selects_an_items_context_and_every_attempt_is_on_record() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    let slug = context::slug_for(&id);

    // Nothing selected is an answer, and says where a selection would go.
    let (st, v) = call(&h.operator, "GET", &format!("/api/work/{id}/context"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["selection"], Value::Null);
    assert_eq!(v["slug"], slug.as_str());
    assert_eq!(v["attempts"], json!([]));

    a_doc(
        &h,
        "ref-interviews",
        "# Interviews\n\nAdmins sort by severity.",
    )
    .await;
    a_doc(
        &h,
        "note-adr-sort",
        "# Sort decision\n\nSeverity, then age.",
    )
    .await;

    // Selecting: roles, notes, and a document this node does not hold, which
    // is accepted and said to be missing rather than refused — it may be on
    // its way from another node.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/context"),
        Some(json!({ "picks": [
            { "role": "research", "slug": "ref-interviews", "note": "three admins" },
            { "role": "decisions", "slug": "note-adr-sort" },
            { "role": "constraints", "slug": "guide-not-here" },
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let selection = &v["selection"];
    assert_eq!(selection["slug"], slug.as_str());
    assert_eq!(selection["picks"].as_array().unwrap().len(), 3);
    assert_eq!(selection["resolved"][0]["known"], true);
    assert_eq!(selection["resolved"][0]["title"], "Interviews");
    assert_eq!(selection["resolved"][2]["known"], false);
    assert_eq!(selection["resolved"][2]["reason"], "absent");
    let hash = selection["hash"].as_str().unwrap().to_string();

    // The selection is a readable document of its own kind.
    let doc = h.store.doc_get("personal", &slug).unwrap().unwrap();
    assert_eq!(doc.kind, context::KIND);
    assert!(
        doc.body
            .contains("## Research\n\n- [doc:ref-interviews] three admins"),
        "{}",
        doc.body
    );

    // What cannot be a selection is refused, and a stale edit is a conflict.
    let (st, _) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/context"),
        Some(json!({ "picks": [{ "role": "gossip", "slug": "ref-interviews" }] })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/context"),
        Some(json!({ "picks": [], "if_hash": "stale" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = call(
        &h.operator,
        "PUT",
        "/api/work/no-such-item/context",
        Some(json!({ "picks": [] })),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Two attempts. The first is revision 1; between them the research is
    // edited and the missing constraint dropped, so the second is revision
    // 2 and says what changed.
    let item = h.store.work_get(&id).unwrap().unwrap();
    let (_, first) = context::prepare(&h.store, "s1", &item).unwrap().unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(first.omissions().count(), 1);

    a_doc(
        &h,
        "ref-interviews",
        "# Interviews\n\nAdmins sort by severity; one by age.",
    )
    .await;
    let (st, _) = call(
        &h.operator,
        "PUT",
        &format!("/api/work/{id}/context"),
        Some(json!({ "if_hash": hash, "picks": [
            { "role": "research", "slug": "ref-interviews", "note": "three admins" },
            { "role": "decisions", "slug": "note-adr-sort" },
        ]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (delivered, second) = context::prepare(&h.store, "s2", &item).unwrap().unwrap();
    assert_eq!(second.revision, 2);
    assert!(delivered.text.contains("one by age"));
    assert_eq!(second.previous_session.as_deref(), Some("s1"));

    let (_, v) = call(&h.operator, "GET", &format!("/api/work/{id}/context"), None).await;
    let attempts = v["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    // Newest first, each with what it received and what changed.
    assert_eq!(attempts[0]["session_id"], "s2");
    assert_eq!(attempts[0]["revision"], 2);
    let changes: Vec<(String, String)> = attempts[0]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["kind"].as_str().unwrap().to_string(),
                c["slug"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        changes.contains(&("edited".into(), "ref-interviews".into())),
        "{changes:?}"
    );
    assert!(
        changes.contains(&("removed".into(), "guide-not-here".into())),
        "{changes:?}"
    );
    assert_eq!(attempts[1]["session_id"], "s1");
    assert_eq!(attempts[1]["received"][2]["delivery"], "omitted");
    assert_eq!(attempts[1]["received"][2]["reason"], "absent");
}

#[tokio::test]
async fn a_session_cannot_choose_its_own_context() {
    state::isolate();
    let h = harness().await;
    let id = an_item(&h, "Overnight alert triage").await;
    h.store
        .insert_session(&support::rows::session_row("s1", "n1", "personal"))
        .unwrap();
    let token = h
        .manager
        .register_tool_token_for_test("s1", "personal")
        .await;

    // Refused outright, with the route that does work — not put to the
    // operator as though it were an ordinary document edit.
    let v = mcp(
        &h.harness,
        "s1",
        &token,
        "doc_write",
        json!({ "slug": context::slug_for(&id), "body": "# Context\n\n## Research\n\n- [doc:ref-mine]\n" }),
    )
    .await;
    assert_eq!(v["error"], true, "{v}");
    let raw = v["raw"].as_str().unwrap_or("");
    assert!(raw.contains("only the operator changes"), "{v}");
    assert!(raw.contains("ask_operator"), "{v}");
    assert!(h
        .store
        .doc_get("personal", &context::slug_for(&id))
        .unwrap()
        .is_none());
    assert!(h.store.open_permissions().unwrap().is_empty());
}
