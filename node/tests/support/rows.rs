//! Row literals every test used to spell out in full. A builder per table
//! with sensible defaults; a test overrides the one or two fields it is about.

#![allow(dead_code)]

use tracon::store::{now_ms, NodeRow, SessionRow};

/// A ready self node running the fake harness.
pub fn node_row(id: &str, name: &str) -> NodeRow {
    NodeRow {
        id: id.into(),
        name: name.into(),
        state: "ready".into(),
        failed_check: None,
        failed_detail: None,
        harness_id: "fake".into(),
        harness_pinned: "1.0.0".into(),
        harness_found: Some("1.0.0".into()),
        models_json: Some(r#"[{"value":"m/a","name":"A"}]"#.into()),
        checked_at_ms: Some(now_ms()),
        is_self: 1,
        x25519_pub: None,
        last_seen_ms: None,
        reachable: 1,
    }
}

/// A running session on `node` in `channel`, with nothing else set.
pub fn session_row(id: &str, node: &str, channel: &str) -> SessionRow {
    SessionRow {
        id: id.into(),
        node_id: node.into(),
        channel: channel.into(),
        work_item_id: None,
        repo_path: "/r".into(),
        worktree_path: None,
        branch: "b".into(),
        harness_id: "fake".into(),
        harness_version: "1".into(),
        harness_session_id: None,
        container_name: None,
        model: "m".into(),
        project_id: None,
        phase: "execute".into(),
        policy_version: None,
        review_id: None,
        budget_tokens: 1000,
        tokens_used: 0,
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
        started_mono_ms: None,
        ended_mono_ms: None,
        updated_ms: now_ms(),
    }
}
