//! The data pane: what the node holds, kind by kind, counted from the
//! store, with a delete offered only where one exists.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness;
use support::http::{call, call_local};

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
    assert_eq!(work["delete"]["path"], "/tasks");
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

/// A repository's kept service data is in the inventory, listed with what it
/// weighs, and reset only at the node: then it is gone, and its next session
/// would start the service empty.
#[tokio::test]
async fn a_repositorys_service_data_is_counted_listed_and_reset_only_at_the_node() {
    let h = harness().await;
    let volume = format!("tracon-svcdata-{}", uuid::Uuid::now_v7().simple());
    let files = tracon::runner::sandbox_files::FilesStore::for_node();
    files.ensure(&volume).unwrap();
    let tree = files.tree(&volume).unwrap();
    std::fs::create_dir_all(tree.join("pgdata")).unwrap();
    std::fs::write(tree.join("pgdata/PG_VERSION"), b"17\n").unwrap();
    h.store
        .service_volume_used(&volume, "/src/app", "postgres", 1)
        .unwrap();

    let (st, v) = call(&h.operator, "GET", "/api/maintenance/data", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let held = v["holdings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["kind"] == "service_data")
        .unwrap()
        .clone();
    assert_eq!(held["count"], 1);
    assert!(held["bytes"].as_i64().unwrap() >= 3, "{held}");
    assert_eq!(held["delete"]["path"], "/settings#system");

    let (st, v) = call(&h.operator, "GET", "/api/maintenance/service-volumes", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let listed = &v["volumes"][0];
    assert_eq!(listed["volume"], volume.as_str());
    assert_eq!(listed["repo"], "/src/app");
    assert_eq!(listed["service"], "postgres");
    assert_eq!(listed["live"], false);
    // No [[repo]] entry names it any more.
    assert_eq!(listed["mounted"], false);

    // Not from off the node.
    let reset = Some(json!({ "repo": "/src/app", "service": "postgres" }));
    let (st, _) = call(
        &h.operator,
        "POST",
        "/api/maintenance/service-volumes/reset",
        reset.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(files.holds(&volume));

    let (st, v) = call_local(
        &h.operator,
        "POST",
        "/api/maintenance/service-volumes/reset",
        reset.clone(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["reset"], volume.as_str());
    assert!(!files.holds(&volume));
    let (_, v) = call(&h.operator, "GET", "/api/maintenance/service-volumes", None).await;
    assert_eq!(v["volumes"], json!([]));

    // Asked again, there is nothing to reset.
    let (st, _) = call_local(
        &h.operator,
        "POST",
        "/api/maintenance/service-volumes/reset",
        reset,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
