//! The outcome record through its surface.
//!
//! What a session came to is read from what was recorded, and the claims under
//! test are about where words stop and verification starts: a review's prose
//! and a report's are claims, and only a check the node ran on the commit a
//! claim was made at backs it. A newer commit with no check on it is said to be
//! unverified, however the review describing it reads.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};
use support::http::call;
use support::rows::session_row;

use axum::http::StatusCode;
use serde_json::{json, Value};

use tracon::store::{now_ms, CandidateRow, CheckRunRow, OperatorQuestionRow, ReviewRow};

fn a_review(id: &str, kind: &str, head: &str, body: &str) -> ReviewRow {
    ReviewRow {
        id: id.into(),
        session_id: Some("s1".into()),
        node_id: "n1".into(),
        channel: "personal".into(),
        kind: kind.into(),
        title: format!("{id}: the change"),
        body: body.into(),
        edited_title: None,
        edited_body: None,
        provider: "github".into(),
        target: json!({ "project": "o/r" }).to_string(),
        diff: String::new(),
        files: json!([{ "path": "src/a.rs", "blob": "1" }]).to_string(),
        head_sha: head.into(),
        base_ref: "main".into(),
        added: 4,
        removed: 2,
        state: "new".into(),
        verdict_reason: None,
        publish_result: None,
        claimed_ms: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now_ms(),
        checks_json: None,
        review_session_id: None,
        ai_verdict_json: None,
        revision_patch: None,
        lane: None,
    }
}

fn a_checked_commit(h: &Harness, head: &str, outcome: &str) {
    let candidate = tracon::store::candidate_id(head, "personal");
    h.store
        .insert_candidate(&CandidateRow {
            id: candidate.clone(),
            head_sha: head.into(),
            tree_sha: None,
            channel: "personal".into(),
            owner_session_id: Some("s1".into()),
            source_kind: "git".into(),
            captured_ms: now_ms(),
            capture_json: "{}".into(),
        })
        .unwrap();
    h.store
        .insert_check_run(&CheckRunRow {
            id: format!("run-{head}"),
            candidate_id: Some(candidate),
            session_id: Some("s1".into()),
            command: Some("just check".into()),
            definition_json: "{}".into(),
            definition_hash: None,
            execution_image: None,
            inputs_json: None,
            reuse_key: None,
            outcome: outcome.into(),
            source_outcome: None,
            exit_code: Some(i64::from(outcome != "passed")),
            log: String::new(),
            duration_ms: Some(1),
            started_ms: now_ms(),
            finished_ms: Some(now_ms()),
            rerun_of: None,
            reused_from_id: None,
            metadata_json: "{}".into(),
        })
        .unwrap();
}

async fn outcome(h: &Harness) -> Value {
    let (st, v) = call(&h.operator, "GET", "/api/sessions/s1/outcome", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn claim<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["claims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == json!(id))
        .unwrap_or_else(|| panic!("no claim {id} in {v}"))
}

#[tokio::test]
async fn a_claim_is_verified_only_by_a_check_on_its_own_commit() {
    let h = harness().await;
    let mut session = session_row("s1", "n1", "personal");
    session.state = "closed".into();
    session.tokens_used = 120;
    h.store.insert_session(&session).unwrap();

    let first = "a".repeat(40);
    a_checked_commit(&h, &first, "passed");
    h.store
        .insert_review(&a_review("rv1", "pr", &first, "Fixes the bug; tests pass."))
        .unwrap();
    h.store
        .insert_review(&a_review(
            "rp1",
            "report",
            "content-hash",
            "Everything works.",
        ))
        .unwrap();
    h.store
        .insert_operator_question(&OperatorQuestionRow {
            id: "q1".into(),
            session_id: Some("s1".into()),
            channel: "personal".into(),
            node_id: "n1".into(),
            request_key: Some("k1".into()),
            prompt: "Which schema?".into(),
            choices_json: "[]".into(),
            state: "unanswered".into(),
            answer_json: None,
            created_ms: now_ms(),
            answered_ms: None,
        })
        .unwrap();

    let v = outcome(&h).await;
    assert_eq!(v["head_sha"], json!(first));
    assert_eq!(v["changed"]["files"], json!(["src/a.rs"]));
    assert_eq!(v["changed"]["added"], 4);
    assert_eq!(claim(&v, "rv1")["backed"], true, "{v}");
    assert_eq!(
        claim(&v, "rv1")["backed_by"],
        json!([format!("run-{first}")])
    );
    assert_eq!(
        claim(&v, "rp1")["backed"],
        false,
        "a report's prose is never verification"
    );
    let pending: Vec<&str> = v["needs_decision"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["kind"].as_str().unwrap())
        .collect();
    assert!(
        pending.contains(&"review") && pending.contains(&"report") && pending.contains(&"question"),
        "{v}"
    );
    assert_eq!(v["cost"]["tokens_used"], 120);

    // The agent moves on and describes the new commit just as confidently.
    let second = "b".repeat(40);
    h.store
        .insert_review(&a_review(
            "rv2",
            "pr",
            &second,
            "Now also faster. All checks green.",
        ))
        .unwrap();
    let v = outcome(&h).await;
    assert_eq!(v["head_sha"], json!(second));
    assert_eq!(claim(&v, "rv2")["backed"], false, "{v}");
    let verified = &v["verified"][0];
    assert_eq!(verified["passed"], true);
    assert_eq!(
        verified["current"], false,
        "the pass is on the older commit"
    );
    let uncertain = v["uncertain"].to_string();
    assert!(uncertain.contains("only on earlier commits"), "{uncertain}");
}

#[tokio::test]
async fn an_unknown_session_has_no_outcome() {
    let h = harness().await;
    let (st, _) = call(&h.operator, "GET", "/api/sessions/nope/outcome", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
