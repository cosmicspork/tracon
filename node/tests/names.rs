//! Operator names through the API: a rename is validated, refused with the
//! holder on a clash, reverted by a tombstone, reaches open tabs as a `name`
//! frame, and resolution finds a record by its name, its old generated name,
//! its id or a long enough prefix, listing candidates when several answer.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness;
use support::http::call;
use support::rows::{review_row, session_row};
use support::state;

use axum::http::StatusCode;
use serde_json::json;
use tracon::names::generated;
use tracon::stream::Frame;

fn uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[tokio::test]
async fn a_rename_is_validated_unique_revertible_and_resolvable() {
    state::isolate();
    let h = harness().await;
    let (s1, s2) = (uuid(), uuid());
    h.store
        .insert_session(&session_row(&s1, "n1", "personal"))
        .unwrap();
    h.store
        .insert_session(&session_row(&s2, "n1", "personal"))
        .unwrap();
    // A peer's session, mirrored here.
    h.store.ensure_peer_node("peer-b").unwrap();
    let theirs = uuid();
    h.store
        .insert_session(&session_row(&theirs, "peer-b", "personal"))
        .unwrap();
    let rv = uuid();
    h.store.insert_review(&review_row(&rv, &s1, "new")).unwrap();
    let mut frames = h.bus.subscribe();

    // Named; the answer carries the generated name it replaces.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/session/{s1}"),
        Some(json!({"name": "  Login flow "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "Login flow");
    assert_eq!(v["generated"], generated("session", &s1));
    assert_eq!(v["channel"], "personal");

    // Another tab hears it without a reload.
    let mut heard = None;
    while let Ok(f) = frames.try_recv() {
        if let Frame::Name { kind, id, name, .. } = f {
            heard = Some((kind, id, name));
        }
    }
    assert_eq!(
        heard,
        Some(("session".into(), s1.clone(), Some("Login flow".into())))
    );

    let (_, listed) = call(&h.operator, "GET", "/api/names?channel=personal", None).await;
    assert_eq!(listed["names"][0]["id"], s1.as_str());
    assert_eq!(listed["names"][0]["name"], "Login flow");

    // Validation: 422 for a bad name or a kind that is not renamed here,
    // 404 for a record this node does not hold.
    for bad in [
        json!({"name": "x".repeat(65)}),
        json!({"name": "two\nlines"}),
        json!({"name": "0123abcd"}),
    ] {
        let (st, v) = call(
            &h.operator,
            "PUT",
            &format!("/api/names/session/{s2}"),
            Some(bad.clone()),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{bad} {v}");
    }
    let (st, _) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/task/{}", "ab".repeat(32)),
        Some(json!({"name": "Mine"})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/session/{}", uuid()),
        Some(json!({"name": "Nobody"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // A clash, in any case, is a 409 that names the holder.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/session/{s2}"),
        Some(json!({"name": "LOGIN FLOW"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["holder"]["id"], s1.as_str());
    // Another kind may share it.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/review/{rv}"),
        Some(json!({"name": "Login flow"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // A peer's mirrored session is named from here like our own.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/session/{theirs}"),
        Some(json!({"name": "Their spike"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // A launch manifest no session has launched with is named on its
    // channel, which must be one this node knows.
    let digest = "cd".repeat(32);
    let (st, _) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/manifest/{digest}"),
        Some(json!({"name": "Lean kit"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    h.store.channel_put("personal", &[], "{}").unwrap();
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/manifest/{digest}"),
        Some(json!({"name": "Lean kit", "channel": "personal"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "Lean kit");

    // Resolution: by name, ignoring case; by the old generated name; by id.
    let resolve = |q: String| {
        let app = h.operator.clone();
        async move {
            call(
                &app,
                "GET",
                &format!("/api/resolve?kind=session&q={}", url_escape(&q)),
                None,
            )
            .await
        }
    };
    let (st, v) = resolve("login FLOW".into()).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["id"], s1.as_str());
    assert_eq!(v["name"], "Login flow");
    let (st, v) = resolve(generated("session", &s1)).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the old generated name still resolves: {v}"
    );
    assert_eq!(v["id"], s1.as_str());
    let (st, v) = resolve(s2.clone()).await;
    assert_eq!((st, v["id"].clone()), (StatusCode::OK, json!(s2)));
    let (st, v) = resolve(generated("session", &s2)).await;
    assert_eq!((st, v["id"].clone()), (StatusCode::OK, json!(s2)));
    // An 8-character prefix of ids minted in the same minute names all of
    // them: candidates, newest first.
    let (st, v) = resolve(s1[..8].to_string()).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    let candidates = v["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3);
    assert!(candidates.iter().any(|c| c["name"] == "Login flow"));
    // Too short to be a prefix, and nothing at all.
    let (st, _) = resolve(s1[..5].to_string()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = resolve("session-not-a-name-00".into()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // The review's page finds a review by its own name too.
    let (st, v) = call(
        &h.operator,
        "GET",
        "/api/resolve?kind=review&q=Login%20flow",
        None,
    )
    .await;
    assert_eq!((st, v["id"].clone()), (StatusCode::OK, json!(rv)));

    // A revert is a tombstone: the generated name is back, the name is free.
    let (st, v) = call(
        &h.operator,
        "DELETE",
        &format!("/api/names/session/{s1}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["name"], generated("session", &s1));
    assert!(h.store.name_get("session", &s1).unwrap().is_none());
    let tombstones = h
        .store
        .changes_of_site_after("n1", "personal", 0, 100)
        .unwrap()
        .into_iter()
        .filter(|c| c.table == "entity_name" && c.op == tracon_sync::ChangeOp::Delete)
        .count();
    assert_eq!(tombstones, 1);
    let mut reverted = None;
    while let Ok(f) = frames.try_recv() {
        if let Frame::Name { id, name, .. } = f {
            if id == s1 {
                reverted = Some(name);
            }
        }
    }
    assert_eq!(reverted, Some(None), "the revert reaches open tabs");
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/session/{s2}"),
        Some(json!({"name": "Login flow"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, v) = resolve("Login flow".into()).await;
    assert_eq!(v["id"], s2.as_str());
    // An empty name reverts too.
    let (st, v) = call(
        &h.operator,
        "PUT",
        &format!("/api/names/session/{s2}"),
        Some(json!({"name": ""})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(h.store.name_get("session", &s2).unwrap().is_none());
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn mcp(
    app: &axum::Router,
    sid: &str,
    token: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
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
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text, "error": v["result"]["isError"] }))
}

/// Agents share the operator's handles: a tool's output names each record
/// beside its id, an id argument takes a name, and a handle several records
/// answer to is refused with the candidates.
#[tokio::test]
async fn tools_name_records_and_take_names_for_ids() {
    state::isolate();
    let h = harness().await;
    let task = |id: &str, title: &str| {
        h.store
            .write_change(
                "n1",
                "personal",
                "work_item",
                tracon_sync::ChangeOp::Upsert,
                id,
                json!({"channel": "personal", "title": title, "body": "", "state": "open",
                       "priority": 0, "deps_json": "[]", "created_ms": 1, "updated_ms": 1}),
            )
            .unwrap();
    };
    let (a, b) = (
        format!("abcdef01{}", "0".repeat(56)),
        format!("abcdef02{}", "0".repeat(56)),
    );
    task(&a, "First");
    task(&b, "Second");
    let sid = uuid();
    h.store
        .insert_session(&session_row(&sid, "n1", "personal"))
        .unwrap();
    let token = h
        .manager
        .register_tool_token_for_test(&sid, "personal")
        .await;

    let v = mcp(
        &h.harness,
        &sid,
        &token,
        "work_show",
        json!({ "id": generated("task", &a) }),
    )
    .await;
    assert_eq!(v["id"], a.as_str(), "{v}");
    assert_eq!(v["name"], generated("task", &a), "{v}");

    let v = mcp(
        &h.harness,
        &sid,
        &token,
        "work_show",
        json!({ "id": "abcdef" }),
    )
    .await;
    assert_eq!(v["error"], true, "{v}");
    let raw = v["raw"].as_str().unwrap();
    assert!(
        raw.contains(&generated("task", &a)) && raw.contains(&b),
        "{raw}"
    );
}
