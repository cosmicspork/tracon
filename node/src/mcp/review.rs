//! The review tools an agent uses to get something published.
//!
//! `submit_review` states an intent; the node captures the diff from the
//! worktree itself, so the artifact under review is what the branch actually
//! contains rather than what the agent says it contains. `review_status` waits
//! for the verdict and returns it, including the operator's edits.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    mcp::CallContext,
    review::{self, publish::Target},
    session::Manager,
    store::{now_ms, ReviewRow, Store},
};

pub const SUBMIT: &str = "submit_review";
pub const STATUS: &str = "review_status";

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": SUBMIT,
            "description": "Submit the current branch for human review. The node captures the diff \
                            from the worktree, so commit your work first. Nothing is published \
                            until a human approves; call review_status to wait for the verdict. \
                            Resubmit with the same review_id after making changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "The change's title, as it should appear." },
                    "body": { "type": "string", "description": "What is not obvious from the diff: intent, trade-offs, follow-ups." },
                    "provider": { "type": "string", "enum": ["github", "gitlab"] },
                    "project": { "type": "string", "description": "owner/name on GitHub, the project path on GitLab." },
                    "base": { "type": "string", "description": "Branch to merge into. Defaults to the branch the worktree was created from." },
                    "review_id": { "type": "string", "description": "Set to resubmit an existing review after changes were requested." },
                },
                "required": ["title", "body", "provider", "project"],
            },
        }),
        json!({
            "name": STATUS,
            "description": "Wait for a review's verdict and return it. Blocks until the review is \
                            decided or the wait elapses. On approval the node publishes the \
                            approved text itself and returns where it landed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "review_id": { "type": "string" },
                    "wait_secs": { "type": "integer", "description": "How long to block. 0 returns the current state." },
                },
                "required": ["review_id"],
            },
        }),
    ]
}

pub async fn call(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        SUBMIT => submit(store, manager, ctx, args).await,
        STATUS => status(store, ctx, args).await,
        other => Err(format!("no tool named {other}")),
    }
}

async fn submit(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let session = store
        .get_session(&ctx.session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    let worktree = session
        .worktree_path
        .clone()
        .ok_or("this session has no worktree to review")?;

    let title = str_arg(args, "title")?;
    let body = str_arg(args, "body")?;
    let provider = str_arg(args, "provider")?;
    let project = str_arg(args, "project")?;
    if review::publish::Provider::parse(&provider).is_none() {
        return Err(format!(
            "{provider} is not a provider this node publishes to"
        ));
    }
    // The base defaults to what the worktree was branched from — read from the
    // worktree's `origin/HEAD`, not assumed to be `main`.
    let base = match args.get("base").and_then(Value::as_str) {
        Some(b) => b.to_string(),
        None => review::default_base(&worktree)
            .await
            .map_err(|e| e.to_string())?,
    };
    // Diff against the remote-tracking ref, so the review shows exactly what the
    // change introduces over what it will merge into.
    let range_base = format!("origin/{base}");
    let capture = review::capture(&worktree, &range_base, &session.branch)
        .await
        .map_err(|e| e.to_string())?;
    let files = serde_json::to_string(&capture.files).unwrap_or_else(|_| "[]".into());

    // A resubmission keeps the same card and the same thread.
    if let Some(id) = args.get("review_id").and_then(Value::as_str) {
        let existing = store
            .get_review(id)
            .map_err(|e| e.to_string())?
            .ok_or("no review with that id")?;
        if existing.session_id != ctx.session_id {
            return Err("that review belongs to another session".into());
        }
        store
            .revise_review(
                id,
                &capture.diff,
                &files,
                &capture.head_sha,
                capture.added,
                capture.removed,
            )
            .map_err(|e| e.to_string())?;
        manager.publish_queue().await;
        return Ok(json!({
            "review_id": id,
            "state": "new",
            "message": "Resubmitted. Call review_status to wait for the verdict.",
            "uncommitted": capture.uncommitted,
        }));
    }

    let target = Target {
        provider: provider.clone(),
        project: project.clone(),
        base: base.clone(),
        branch: session.branch.clone(),
    };
    let id = uuid::Uuid::now_v7().to_string();
    let row = ReviewRow {
        id: id.clone(),
        session_id: ctx.session_id.clone(),
        node_id: session.node_id.clone(),
        channel: ctx.channel.clone(),
        kind: if provider == "gitlab" {
            "mr".into()
        } else {
            "pr".into()
        },
        title,
        body,
        edited_title: None,
        edited_body: None,
        provider,
        target: serde_json::to_string(&target).unwrap_or_default(),
        diff: capture.diff,
        files,
        head_sha: capture.head_sha,
        base_ref: base,
        added: capture.added,
        removed: capture.removed,
        state: "new".into(),
        verdict_reason: None,
        publish_result: None,
        claimed_ms: None,
        created_ms: now_ms(),
        created_mono_ms: 0,
        resolved_mono_ms: None,
        updated_ms: now_ms(),
    };
    store.insert_review(&row).map_err(|e| e.to_string())?;
    manager.publish_queue().await;

    Ok(json!({
        "review_id": id,
        "state": "new",
        "message": "Submitted for review. Nothing is published until a human approves. \
                    Call review_status to wait for the verdict.",
        "files": row.added + row.removed,
        "uncommitted": capture.uncommitted,
    }))
}

async fn status(store: &Arc<Store>, ctx: &CallContext, args: &Value) -> Result<Value, String> {
    let id = str_arg(args, "review_id")?;
    let wait = args
        .get("wait_secs")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .min(600);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);

    loop {
        let r = store
            .get_review(&id)
            .map_err(|e| e.to_string())?
            .ok_or("no review with that id")?;
        if r.session_id != ctx.session_id {
            return Err("that review belongs to another session".into());
        }
        match r.state.as_str() {
            "revising" => {
                return Ok(json!({
                    "review_id": r.id,
                    "state": "changes_requested",
                    "notes": r.verdict_reason,
                    "message": "Changes were requested. Make them, commit, then call \
                                submit_review again with this review_id.",
                }));
            }
            "approved" | "rejected" => {
                return Ok(json!({
                    "review_id": r.id,
                    "state": r.state,
                    // The approved text, which may not be what was submitted.
                    "title": r.approved_title(),
                    "body": r.approved_body(),
                    "reason": r.verdict_reason,
                    "published": r.publish_result,
                }));
            }
            _ if std::time::Instant::now() >= deadline => {
                return Ok(json!({
                    "review_id": r.id,
                    "state": r.state,
                    "message": "Still waiting on a human. Call review_status again to keep waiting.",
                }));
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{key} is required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_arguments_are_named_when_missing() {
        let args = json!({ "title": "  " });
        assert_eq!(str_arg(&args, "title").unwrap_err(), "title is required");
        assert_eq!(str_arg(&args, "body").unwrap_err(), "body is required");
        assert_eq!(str_arg(&json!({"body":"x"}), "body").unwrap(), "x");
    }
}
