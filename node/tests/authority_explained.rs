//! What a session may do, as the node explains it.
//!
//! The claim under test is not that the prose reads well. It is that the
//! explanation is the gate answering about itself: every verdict in it comes
//! from the same call that decides the real request, so an operator who reads
//! "this is asked" and then sees it asked is reading one mechanism, not two
//! that happen to agree today.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness, Harness};
use support::http::call;
use support::state;

use serde_json::{json, Value};
use tracon::store::{now_ms, SessionRow};

fn session_row(id: &str) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: None,
        repo_path: "/nonexistent/repo".into(),
        worktree_path: Some("/work/tree".into()),
        branch: "feat/x".into(),
        harness_id: "fake".into(),
        harness_version: "1.0.0".into(),
        harness_agent: None,
        harness_found: Some("1.0.0".into()),
        harness_protocol: None,
        harness_session_id: None,
        container_name: None,
        model: "m/a".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 250,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: "running".into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms: now_ms(),
        started_mono_ms: Some(0),
        ended_mono_ms: None,
        updated_ms: now_ms(),
        archived_ms: None,
        legacy_ms: None,
        parent_session: None,
        continued_from: None,
        manifest_digest: None,
    }
}

async fn explain(h: &Harness, id: &str) -> Value {
    let (status, body) = call(
        &h.operator,
        "GET",
        &format!("/api/sessions/{id}/authority"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    body
}

fn standing<'a>(view: &'a Value, name: &str) -> Option<&'a Value> {
    view["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
}

#[tokio::test]
async fn the_explanation_answers_in_the_terms_the_operator_thinks_in() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();
    let view = explain(&h, "s1").await;

    assert_eq!(view["session_id"], "s1");
    assert_eq!(view["channel"], "personal");
    assert_eq!(view["node"]["id"], "n1");
    assert_eq!(view["harness"]["id"], "fake");
    // The image is the one the configured runtime would actually launch, not
    // whichever of the two fields was easiest to reach.
    assert!(view["image"]["execution"]
        .as_str()
        .unwrap()
        .contains("harness"));
    assert_eq!(view["access"]["workspace"], "/work/tree");
    assert_eq!(view["access"]["branch"], "feat/x");
    assert_eq!(view["limits"]["budget_tokens"], 1000);
    assert_eq!(view["limits"]["tokens_used"], 250);
    assert!(view["limits"]["permission_timeout_secs"].as_i64().unwrap() > 0);
    assert_eq!(view["policy"]["trusted"], true);
}

/// The point of the whole surface: an action's standing here is produced by
/// the policy, so it cannot disagree with what the gate does.
#[tokio::test]
async fn every_verdict_shown_is_the_one_the_gate_would_give() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();
    let view = explain(&h, "s1").await;
    let policy = tracon::policy::Policy::shipped();

    for action in view["actions"].as_array().unwrap() {
        let name = action["name"].as_str().unwrap();
        let kind = action["surface"].as_str().unwrap();
        let decided = policy.decide(&tracon::policy::Request {
            channel: "personal",
            action: name,
            kind: Some(kind),
            resource: None,
            command: None,
            arguments: None,
        });
        assert_eq!(
            action["verdict"],
            json!(decided.verdict),
            "{name} is explained differently from how it is decided"
        );
    }
}

/// An argument-scoped allow is neither an allow nor a bare ask. Reporting it
/// as either is the failure this exists to prevent: "allowed" invites the
/// operator to expect no card, "asked" hides that the exception is already
/// there and that a slug is all that separates them.
#[tokio::test]
async fn an_allow_the_arguments_narrow_is_shown_as_the_narrowing() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();
    let view = explain(&h, "s1").await;

    let doc_write = standing(&view, "doc_write").expect("doc_write is offered to every session");
    assert_eq!(doc_write["verdict"], "ask");
    let scoped = doc_write["scoped"].as_array().unwrap();
    assert_eq!(scoped.len(), 1, "the shipped bundle scopes doc_write once");
    let slugs = scoped[0]["args"]["slug"].as_array().unwrap();
    assert!(slugs.iter().any(|g| g == "note-*"), "{slugs:?}");
    assert_eq!(scoped[0]["rule_id"], "unattended-notes");

    // A plain allow carries no narrowing, and a decided action carries its rule.
    let doc_read = standing(&view, "doc_read").unwrap();
    assert_eq!(doc_read["verdict"], "allow");
    assert_eq!(doc_read["scoped"].as_array().unwrap().len(), 0);
    assert_eq!(doc_read["rule_id"], "corpus-and-ledger");
}

/// The consequential actions are listed whether or not a grant exists, because
/// "nothing may be merged from here" is an answer the operator needs and an
/// empty list does not give.
#[tokio::test]
async fn consequential_actions_are_listed_with_the_grants_that_bear_on_them() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();

    let before = explain(&h, "s1").await;
    let merge = standing(&before, "merge").expect("merge is always explained");
    assert_eq!(merge["verdict"], "ask");
    assert_eq!(merge["surface"], "authority");
    assert!(merge["grants"].is_null() || merge["grants"].as_array().unwrap().is_empty());
    // A terminal is a capability, not a forge action, and is decided under the
    // kind a bundle rule would have to name to reach it.
    assert_eq!(
        standing(&before, "terminal").unwrap()["surface"],
        "capability"
    );

    let (status, _) = call(
        &h.operator,
        "POST",
        "/api/authority/grants",
        Some(json!({
            "action": "merge",
            "verdict": "allow",
            "target": "github:owner/repo:pr:7",
            "channel": "personal",
            "revision": "abc1234",
            "reason": "approved by the operator for this change",
        })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let after = explain(&h, "s1").await;
    let merge = standing(&after, "merge").unwrap();
    // Still asked: the grant covers one pull request, and saying "allowed"
    // because some grant exists would be the parallel model in miniature.
    assert_eq!(merge["verdict"], "ask");
    let grants = merge["grants"].as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0]["target"], "github:owner/repo:pr:7");
    assert_eq!(after["grants"].as_array().unwrap().len(), 1);
    // A grant on another channel is not this session's authority.
    assert!(standing(&after, "deploy").unwrap()["grants"]
        .as_array()
        .is_none_or(|g| g.is_empty()));
}

/// Revocation takes effect at the next read, because the view is assembled per
/// request. A cached explanation would keep describing authority that is gone.
#[tokio::test]
async fn a_revoked_grant_stops_being_explained_as_live() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();
    let (_, created) = call(
        &h.operator,
        "POST",
        "/api/authority/grants",
        Some(json!({
            "action": "ticket_transition",
            "verdict": "allow",
            "target": "jira:WRK-1",
            "channel": "personal",
            "reason": "the operator moved this one by hand",
        })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(
        explain(&h, "s1").await["grants"].as_array().unwrap().len(),
        1
    );

    let (status, _) = call(
        &h.operator,
        "DELETE",
        &format!("/api/authority/grants/{id}"),
        None,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(explain(&h, "s1").await["grants"]
        .as_array()
        .unwrap()
        .is_empty());
}

/// Unattended execution is the bundle's own allow rules, not a description of
/// them: a rule edited in the bundle changes this list without anything here
/// being updated.
#[tokio::test]
async fn unattended_commands_come_from_the_rules_that_allow_them() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();
    let view = explain(&h, "s1").await;

    let rules = view["unattended_commands"].as_array().unwrap();
    assert!(!rules.is_empty());
    let all: Vec<&str> = rules
        .iter()
        .flat_map(|r| r["commands"].as_array().unwrap())
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(all.contains(&"git status"), "{all:?}");
    assert!(all.contains(&"ls"), "{all:?}");
    // Every one of them carries the rule that allows it, so the operator can
    // go and read it rather than taking this list's word.
    assert!(rules
        .iter()
        .all(|r| !r["rule_id"].as_str().unwrap().is_empty()));
    assert!(rules
        .iter()
        .all(|r| !r["reason"].as_str().unwrap().is_empty()));
    // A write is nowhere in it. The explanation must never be the place an
    // operator learns that something unattended was added quietly.
    assert!(!all.iter().any(|c| c.starts_with("git push")), "{all:?}");
}

/// A review session is offered fewer tools, and the explanation is of the
/// session in front of the operator rather than of sessions in general.
#[tokio::test]
async fn the_explanation_is_of_this_session_not_of_the_node() {
    state::isolate();
    let h = harness().await;
    let mut review = session_row("s2");
    review.phase = "review".into();
    h.store.insert_session(&review).unwrap();
    h.store.insert_session(&session_row("s1")).unwrap();

    let ordinary = explain(&h, "s1").await;
    let reviewing = explain(&h, "s2").await;
    assert!(standing(&ordinary, "work_close").is_some());
    assert!(
        standing(&reviewing, "work_close").is_none(),
        "a review session is not offered the work tools, so it must not be told it has them"
    );
    assert!(standing(&reviewing, "review_verdict").is_some());
}

/// The explanation holds its own list of consequential actions, which is the
/// kind of second list this surface exists to argue against. It is allowed to
/// exist only because this test makes it impossible for the two to diverge.
#[tokio::test]
async fn every_action_a_grant_can_carry_is_explained() {
    state::isolate();
    let h = harness().await;
    h.store.insert_session(&session_row("s1")).unwrap();
    let view = explain(&h, "s1").await;

    for action in [
        "merge",
        "publish",
        "ticket_transition",
        "deploy",
        "browser_verify",
        "browser_test_account",
        "terminal",
    ] {
        assert!(
            tracon::authority::valid_action(action),
            "{action} is no longer an action the node accepts; fix this list"
        );
        assert!(
            standing(&view, action).is_some(),
            "{action} can carry a grant but is not explained"
        );
    }
    // And nothing here claims an authority the node would refuse to grant.
    for action in view["actions"].as_array().unwrap() {
        if action["surface"] == "tool" {
            continue;
        }
        assert!(
            tracon::authority::valid_action(action["name"].as_str().unwrap()),
            "{} is explained as authority the node does not have",
            action["name"]
        );
    }
}

#[tokio::test]
async fn a_session_that_does_not_exist_is_a_404() {
    state::isolate();
    let h = harness().await;
    let (status, _) = call(&h.operator, "GET", "/api/sessions/nope/authority", None).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}
