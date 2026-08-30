//! Recent repositories: the session table remembers where work happened, and
//! the form offers that memory instead of demanding a typed path.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness;
use support::http::call;

use tracon::store::SessionRow;

fn session_at(id: &str, repo: &str, created_ms: i64) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: "n1".into(),
        channel: "personal".into(),
        work_item_id: None,
        repo_path: repo.into(),
        worktree_path: None,
        branch: "feat/x".into(),
        harness_id: "fake".into(),
        harness_version: "1.0.0".into(),
        harness_session_id: None,
        container_name: None,
        model: "m/a".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 0,
        cost_usd: None,
        context_used: None,
        context_size: None,
        state: "ended".into(),
        end_reason: None,
        last_error: None,
        turn_active: 0,
        draft: None,
        draft_updated_ms: None,
        created_ms,
        started_mono_ms: None,
        ended_mono_ms: None,
        updated_ms: created_ms,
    }
}

#[tokio::test]
async fn recent_repos_deduplicate_and_order_by_last_use() {
    let h = harness().await;
    h.store
        .insert_session(&session_at("s1", "/src/old", 100))
        .unwrap();
    h.store
        .insert_session(&session_at("s2", "/src/busy", 200))
        .unwrap();
    h.store
        .insert_session(&session_at("s3", "/src/busy", 400))
        .unwrap();
    h.store
        .insert_session(&session_at("s4", "/src/new", 300))
        .unwrap();

    let (status, body) = call(&h.operator, "GET", "/api/repos/recent", None).await;
    assert_eq!(status, 200);
    let repos = body["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 3);
    assert_eq!(repos[0]["repo_path"], "/src/busy");
    assert_eq!(repos[0]["last_used_ms"], 400);
    assert_eq!(repos[0]["sessions"], 2);
    assert_eq!(repos[1]["repo_path"], "/src/new");
    assert_eq!(repos[2]["repo_path"], "/src/old");
}

#[tokio::test]
async fn a_fresh_node_has_no_recent_repos() {
    let h = harness().await;
    let (status, body) = call(&h.operator, "GET", "/api/repos/recent", None).await;
    assert_eq!(status, 200);
    assert_eq!(body["repos"].as_array().unwrap().len(), 0);
}
