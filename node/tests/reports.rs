//! Narrative reports are operator correspondence, never code-publication intents.

#[path = "support/mod.rs"]
mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::harness::{harness_with, Harness};
use support::http::call;
use tracon::config::Config;

async fn node() -> Harness {
    let mut cfg = Config::default();
    cfg.external.enabled = true;
    harness_with(cfg).await
}

async fn tool(h: &Harness, channel: &str, name: &str, arguments: Value) -> (bool, Value) {
    let (status, response) = call(
        &h.operator,
        "POST",
        &format!("/mcp/external/{channel}"),
        Some(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    (
        response["result"]["isError"] == true,
        serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
    )
}

async fn submit(h: &Harness) -> (String, String) {
    let (failed, result) = tool(
        h,
        "work",
        "submit_report",
        json!({ "title": "Operator interface review", "body": "Make shared notification scope explicit." }),
    )
    .await;
    assert!(!failed, "{result}");
    let id = result["report_id"].as_str().unwrap().to_string();
    let (_, details) = call(&h.operator, "GET", &format!("/api/reviews/{id}"), None).await;
    (
        id,
        details["review"]["head_sha"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn a_report_is_acknowledged_without_a_repository_or_a_publication_path() {
    let h = node().await;
    let (id, version) = submit(&h).await;
    let path = format!("/api/reviews/{id}/verdict");
    let (status, _) = call(
        &h.operator,
        "POST",
        &path,
        Some(json!({ "verdict": "acknowledge" })),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, _) = call(
        &h.operator,
        "POST",
        &path,
        Some(json!({ "verdict": "approve", "head_sha": version })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, evidence) = call(&h.operator, "GET", "/api/evidence/candidates", None).await;
    assert_eq!(status, StatusCode::OK, "{evidence}");
    assert_eq!(evidence["items"], json!([]));
    let (status, result) = call(
        &h.operator,
        "POST",
        &path,
        Some(json!({ "verdict": "acknowledge", "head_sha": version })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (failed, result) = tool(&h, "work", "report_status", json!({ "report_id": id })).await;
    assert!(!failed, "{result}");
    assert_eq!(result["state"], "acknowledged");
    let (_, details) = call(&h.operator, "GET", &format!("/api/reviews/{id}"), None).await;
    assert_eq!(details["publications"], json!([]));
    let (failed, _) = tool(&h, "personal", "report_status", json!({ "report_id": id })).await;
    assert!(failed, "another channel must not read report feedback");
}

#[tokio::test]
async fn report_feedback_and_acknowledgement_are_bound_to_the_content_read() {
    let h = node().await;
    let (id, old_version) = submit(&h).await;
    let path = format!("/api/reviews/{id}/verdict");
    let note = "Include the affected machines.";
    let (status, result) = call(
        &h.operator,
        "POST",
        &path,
        Some(json!({ "verdict": "request_changes", "head_sha": old_version, "reason": note })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (failed, feedback) = tool(&h, "work", "report_status", json!({ "report_id": id })).await;
    assert!(!failed, "{feedback}");
    assert_eq!(feedback["state"], "changes_requested");
    assert_eq!(feedback["notes"], note);
    let (failed, result) = tool(
        &h,
        "work",
        "submit_report",
        json!({ "report_id": id, "title": "Operator interface review", "body": "The shared rule affects both the laptop and the peer." }),
    )
    .await;
    assert!(!failed, "{result}");
    // The store precondition also protects an acknowledgement racing resubmission.
    assert!(!h.store.acknowledge_report(&id, None, &old_version).unwrap());
    let (status, _) = call(
        &h.operator,
        "POST",
        &path,
        Some(json!({ "verdict": "acknowledge", "head_sha": old_version })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, details) = call(&h.operator, "GET", &format!("/api/reviews/{id}"), None).await;
    let (status, result) = call(
        &h.operator,
        "POST",
        &path,
        Some(json!({ "verdict": "acknowledge", "head_sha": details["review"]["head_sha"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["state"], "acknowledged");
}

#[tokio::test]
async fn a_report_or_mismatched_code_review_cannot_create_a_review_session() {
    let h = node().await;
    let (report_id, _) = submit(&h).await;
    let report = h.store.get_review(&report_id).unwrap().unwrap();

    let mut wrong_channel = report.clone();
    wrong_channel.id = "code-review-in-another-channel".into();
    wrong_channel.kind = "pr".into();
    wrong_channel.channel = "personal".into();
    h.store.insert_review(&wrong_channel).unwrap();

    let mut foreign_owner = report.clone();
    foreign_owner.id = "code-review-on-a-peer".into();
    foreign_owner.kind = "mr".into();
    foreign_owner.node_id = "peer".into();
    h.store.ensure_peer_node("peer").unwrap();
    h.store.insert_review(&foreign_owner).unwrap();

    let cases = [
        (
            "a report is never an execute-session context",
            json!({ "channel": "work", "repo_path": "/nonexistent/repo", "model": "local",
                    "phase": "execute", "review_id": report_id }),
        ),
        (
            "a report is never a review-session context",
            json!({ "channel": "work", "repo_path": "/nonexistent/repo", "model": "local",
                    "phase": "review", "review_id": report.id }),
        ),
        (
            "a code review cannot cross channels",
            json!({ "channel": "work", "repo_path": "/nonexistent/repo", "model": "local",
                    "phase": "review", "review_id": wrong_channel.id }),
        ),
        (
            "a code review cannot run on a node other than its owner",
            json!({ "channel": "work", "repo_path": "/nonexistent/repo", "model": "local",
                    "phase": "review", "review_id": foreign_owner.id }),
        ),
    ];

    for (case, body) in cases {
        let sessions_before = h.store.sessions_of_node("n1").unwrap().len();
        let events_before = h.store.all_events_after(-1, 1_000).unwrap().len();
        let (status, response) = call(&h.operator, "POST", "/api/sessions", Some(body)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{case}: {response}");
        assert_eq!(
            h.store.sessions_of_node("n1").unwrap().len(),
            sessions_before,
            "{case}"
        );
        assert_eq!(
            h.store.all_events_after(-1, 1_000).unwrap().len(),
            events_before,
            "{case}"
        );
    }
}
