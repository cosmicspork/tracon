//! Catalogue services through the session's tools.
//!
//! A session asks for a service by name and is offered only what the
//! operator's catalogue holds. The policy bundle decides each service on its
//! own: the browser runs unattended, anything else waits for the operator. A
//! runtime that cannot put a service in the session's network says so rather
//! than starting it somewhere the session cannot reach.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness_with, Harness};
use support::rows::session_row;

use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::config::{Config, Service};

const PINNED: &str = "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

async fn mcp(h: &Harness, token: &str, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/mcp/s1")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = h.harness.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn tool(h: &Harness, token: &str, name: &str, args: Value) -> Value {
    let v = mcp(
        h,
        token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),
    )
    .await;
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(json!({ "raw": text, "error": v["result"]["isError"] }))
}

fn service(name: &str, port: u16) -> Service {
    Service {
        name: name.into(),
        image: PINNED.into(),
        port,
        ..Service::default()
    }
}

async fn session(h: &Harness) -> String {
    let mut row = session_row("s1", "n1", "personal");
    row.container_name = Some("tracon-h-s1".into());
    h.store.insert_session(&row).unwrap();
    h.manager
        .register_tool_token_for_test("s1", "personal")
        .await
}

#[tokio::test]
async fn a_service_is_asked_for_by_name_and_decided_by_name() {
    let h = harness_with(Config {
        service: vec![service("browser", 9222), service("postgres", 5432)],
        ..Config::default()
    })
    .await;
    let token = session(&h).await;

    let listed = mcp(
        &h,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    let start = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "service_start")
        .expect("service_start is offered when the catalogue has entries")
        .clone();
    assert_eq!(
        start["inputSchema"]["properties"]["name"]["enum"],
        json!(["browser", "postgres"])
    );

    // The browser is allowed; this node's runtime cannot place it, and says so.
    let v = tool(
        &h,
        &token,
        "service_start",
        json!({ "name": "browser", "wait_secs": 0 }),
    )
    .await;
    assert_eq!(v["state"], "failed", "{v}");
    assert!(v["detail"].as_str().unwrap().contains("podman"), "{v}");

    // A database waits for the operator.
    let v = tool(&h, &token, "service_start", json!({ "name": "postgres" })).await;
    assert_eq!(v["state"], "awaiting_operator", "{v}");
    assert_eq!(v["summary"], "service_start postgres", "{v}");

    // Status is a read.
    let v = tool(&h, &token, "service_status", json!({ "name": "postgres" })).await;
    assert_eq!(v["state"], "not_started", "{v}");
    let v = tool(&h, &token, "service_status", json!({ "name": "redis" })).await;
    assert!(
        v["raw"]
            .as_str()
            .unwrap_or_default()
            .contains("catalogue offers browser, postgres"),
        "{v}"
    );
}

#[tokio::test]
async fn no_catalogue_offers_no_service_tools() {
    let h = harness_with(Config::default()).await;
    let token = session(&h).await;
    let listed = mcp(
        &h,
        &token,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    assert!(
        !listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"].as_str().unwrap().starts_with("service_")),
        "{listed}"
    );
}
