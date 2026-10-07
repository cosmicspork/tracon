//! The data pane: what the node holds, kind by kind, counted from the
//! store, with a delete offered only where one exists.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness;
use support::http::call;

use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn the_node_says_what_it_holds_and_where_each_kind_is_deleted() {
    let h = harness().await;
    let (st, v) = call(
        &h.operator,
        "POST",
        "/api/work",
        Some(json!({ "channel": "personal", "title": "Count me" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let id = v["id"].as_str().unwrap().to_string();

    let (st, v) = call(&h.operator, "GET", "/api/maintenance/data", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["database_bytes"].as_i64().unwrap() > 0);
    let work = v["holdings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "work")
        .unwrap()
        .clone();
    assert_eq!(work["count"], 1);
    assert!(work["bytes"].as_i64().unwrap() > "Count me".len() as i64);
    assert_eq!(work["delete"]["path"], "/work");
    let sessions = v["holdings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "sessions")
        .unwrap()
        .clone();
    assert_eq!(sessions["delete"], serde_json::Value::Null);

    // A deleted item is a tombstone the node remembers, not something it holds.
    let (st, _) = call(&h.operator, "DELETE", &format!("/api/work/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, v) = call(&h.operator, "GET", "/api/maintenance/data", None).await;
    let work = v["holdings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "work")
        .unwrap()
        .clone();
    assert_eq!(work["count"], 0);
}
