//! Answers that are admin actions. Allowing a repository setup proposal
//! writes `node.toml`; allowing a Dockerfile trial builds it with the network,
//! outside the boundary; allowing an egress ask for the repository saves the
//! host for every later session. Away from the node each needs a passkey
//! bound to that answer, a peer's answer cannot confirm one at all, and every
//! other answer is as it was.

#[path = "support/mod.rs"]
mod support;
use support::{
    harness::{harness_with, Harness},
    soft_passkey::SoftPasskey,
};

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use tracon::{
    config::Config,
    http::{auth, passkey::STEP_UP_HEADER},
    mesh::forward::CommandExecutor,
    store::{approvals::ApprovalRow, now_ms, AuthSessionRow},
};

const ORIGIN: &str = "https://tracon.example.org";
const REMOTE: &str = "203.0.113.7:5000";
const LOCAL: &str = "127.0.0.1:5000";
const COOKIE: &str = "tracon_session=phone";

async fn send(
    h: &Harness,
    method: &str,
    uri: &str,
    peer: &str,
    body: &Value,
    proof: Option<&str>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "host",
            if peer == LOCAL {
                "127.0.0.1:7420"
            } else {
                "tracon.example.org"
            },
        )
        .header("content-type", "application/json");
    if peer != LOCAL {
        b = b.header("cookie", COOKIE).header("origin", ORIGIN);
    }
    if let Some(p) = proof {
        b = b.header(STEP_UP_HEADER, p);
    }
    let mut req = b.body(Body::from(body.to_string())).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    let res = h.operator.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A node with a public origin, a phone paired a moment ago and its passkey.
async fn rig() -> (Harness, SoftPasskey) {
    let mut cfg = Config::default();
    cfg.auth.public_origin = Some(ORIGIN.into());
    let h = harness_with(cfg).await;
    let now = now_ms();
    h.store
        .auth_session_insert(&AuthSessionRow {
            token_hash: auth::hash("phone"),
            created_ms: now,
            last_seen_ms: now,
            expires_ms: now + 600_000,
            user_agent: None,
            paired_ms: Some(now),
            delegated_from: None,
        })
        .unwrap();
    let mut key = SoftPasskey::new(1, ORIGIN);
    let (s, opts) = send(
        &h,
        "POST",
        "/api/passkeys/register/options",
        REMOTE,
        &json!({}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{opts}");
    let credential = key.create(&opts["options"]);
    let (s, v) = send(
        &h,
        "POST",
        "/api/passkeys/register/finish",
        REMOTE,
        &json!({ "challenge_id": opts["challenge_id"], "credential": credential }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (h, key)
}

fn approval(h: &Harness, id: &str, tool: &str, node: &str, arguments: Value) {
    let now = now_ms();
    h.store
        .insert_approval(&ApprovalRow {
            id: id.into(),
            channel: "personal".into(),
            session_id: None,
            node_id: node.into(),
            lane: None,
            tool: tool.into(),
            arguments: arguments.to_string(),
            request_key: format!("key-{id}"),
            title: tool.into(),
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
            expires_ms: now + 600_000,
        })
        .unwrap();
}

fn state_of(h: &Harness, id: &str) -> String {
    h.store.get_approval(id).unwrap().unwrap().state
}

/// The three admin answers: which approval, and the option that allows it.
fn admin_answers(h: &Harness) -> Vec<(&'static str, &'static str)> {
    approval(
        h,
        "propose",
        "repo_setup_propose",
        "n1",
        json!({ "checks": ["just check"], "repo": "/work/x" }),
    );
    approval(
        h,
        "trial",
        "repo_setup_try",
        "n1",
        json!({ "dockerfile": "Dockerfile", "repo": "/work/x" }),
    );
    approval(
        h,
        "egress",
        "request_egress",
        "n1",
        json!({ "host": "example.org" }),
    );
    vec![
        ("propose", "allow_once"),
        ("trial", "allow_once"),
        ("egress", "allow_repo"),
    ]
}

#[tokio::test]
async fn an_admin_answer_from_afar_needs_a_passkey_bound_to_it() {
    let (h, mut key) = rig().await;
    for (id, option) in admin_answers(&h) {
        let uri = format!("/api/permissions/{id}/answer");
        let body = json!({ "option_id": option });
        let (s, refused) = send(&h, "POST", &uri, REMOTE, &body, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{id}: {refused}");
        assert_eq!(
            refused["error"]["kind"], "step_up_required",
            "{id}: {refused}"
        );
        assert_eq!(state_of(&h, id), "pending", "{id}");
        // The prompt says what is being confirmed, in the node's words from
        // the stored approval.
        let summary = refused["step_up"]["summary"].as_str().unwrap_or_default();
        let expected = match id {
            "propose" => "Allow repository setup for /work/x",
            "trial" => "Build Dockerfile for /work/x with the network",
            _ => "Save example.org to the repository's egress",
        };
        assert_eq!(summary, expected, "{id}: {refused}");

        let step_up = &refused["step_up"];
        let credential = key.get(&step_up["options"]);
        let (s, finished) = send(
            &h,
            "POST",
            "/api/passkeys/assert/finish",
            REMOTE,
            &json!({ "challenge_id": step_up["challenge_id"], "credential": credential }),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{id}: {finished}");
        let proof = finished["proof"].as_str().unwrap().to_string();

        // A proof for this answer does not allow it with other arguments,
        // and the prompt for those says they were edited.
        let edited = json!({ "option_id": option, "arguments": { "checks": ["curl evil"] } });
        let (s, v) = send(&h, "POST", &uri, REMOTE, &edited, Some(&proof)).await;
        assert_eq!(v["error"]["kind"], "step_up_required", "{id}: {s} {v}");
        assert_eq!(
            v["step_up"]["summary"].as_str().unwrap_or_default(),
            format!("{expected}, as edited")
        );

        // The answer it was made for goes past the gate.
        let (s, v) = send(&h, "POST", &uri, REMOTE, &body, Some(&proof)).await;
        assert_ne!(v["error"]["kind"], "step_up_required", "{id}: {s} {v}");
        assert_ne!(s, StatusCode::FORBIDDEN, "{id}: {v}");
    }
    // The two the node runs itself were decided: no longer waiting.
    assert_ne!(state_of(&h, "propose"), "pending");
    assert_ne!(state_of(&h, "trial"), "pending");
}

#[tokio::test]
async fn at_the_node_an_admin_answer_needs_no_passkey() {
    let (h, _key) = rig().await;
    for (id, option) in admin_answers(&h) {
        let uri = format!("/api/permissions/{id}/answer");
        let (s, v) = send(
            &h,
            "POST",
            &uri,
            LOCAL,
            &json!({ "option_id": option }),
            None,
        )
        .await;
        assert_ne!(s, StatusCode::FORBIDDEN, "{id}: {v}");
    }
    assert_ne!(state_of(&h, "propose"), "pending");
}

#[tokio::test]
async fn refusing_and_ordinary_approvals_need_no_passkey() {
    let (h, _key) = rig().await;
    admin_answers(&h);
    // Refusing an admin approval, or asking for changes, changes nothing.
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/propose/answer",
        REMOTE,
        &json!({ "option_id": "reject_once", "reason": "not yet" }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(state_of(&h, "propose"), "rejected");
    // Opening a host for this session alone is the session's own grant.
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/egress/answer",
        REMOTE,
        &json!({ "option_id": "allow_session" }),
        None,
    )
    .await;
    assert_ne!(v["error"]["kind"], "step_up_required", "{s} {v}");
    // An ordinary brokered call is answered from the phone as before.
    approval(
        &h,
        "plain",
        "jira_comment",
        "n1",
        json!({ "issue": "X-1", "body": "hi" }),
    );
    let (s, v) = send(
        &h,
        "POST",
        "/api/permissions/plain/answer",
        REMOTE,
        &json!({ "option_id": "allow_once" }),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_ne!(state_of(&h, "plain"), "pending");
}

#[tokio::test]
async fn a_non_json_answer_body_is_refused() {
    let (h, _key) = rig().await;
    admin_answers(&h);
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/permissions/propose/answer")
        .header("host", "tracon.example.org")
        .header("cookie", COOKIE)
        .header("content-type", "text/plain")
        .body(Body::from(json!({ "option_id": "allow_once" }).to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(REMOTE.parse::<SocketAddr>().unwrap()));
    let res = h.operator.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(state_of(&h, "propose"), "pending");
}

#[tokio::test]
async fn a_peer_cannot_answer_an_admin_approval_over_the_mesh() {
    let (h, _key) = rig().await;
    for (id, option) in admin_answers(&h) {
        let result = h
            .state
            .execute(
                "peer",
                proto::frame::Command::Answer {
                    permission_id: id.into(),
                    option_id: option.into(),
                    arguments: None,
                    reason: None,
                    notes: None,
                },
            )
            .await;
        let refusal = result.expect_err(id);
        assert!(
            refusal.contains("a peer's answer cannot confirm it"),
            "{id}: {refusal}"
        );
        assert_eq!(state_of(&h, id), "pending", "{id}");
    }
    // Any other answer meets the usual checks, not this one.
    approval(&h, "plain", "jira_comment", "n1", json!({}));
    let refusal = h
        .state
        .execute(
            "peer",
            proto::frame::Command::Answer {
                permission_id: "plain".into(),
                option_id: "allow_once".into(),
                arguments: None,
                reason: None,
                notes: None,
            },
        )
        .await
        .expect_err("not a channel member");
    assert!(!refusal.contains("a peer's answer"), "{refusal}");
}

#[tokio::test]
async fn an_admin_approval_held_by_a_peer_is_answered_there() {
    let (h, _key) = rig().await;
    h.store.ensure_peer_node("n2").unwrap();
    approval(&h, "theirs", "repo_setup_propose", "n2", json!({}));
    for peer in [LOCAL, REMOTE] {
        let (s, v) = send(
            &h,
            "POST",
            "/api/permissions/theirs/answer",
            peer,
            &json!({ "option_id": "allow_once" }),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("node that holds it"),
            "{v}"
        );
    }
}
