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
    session::{state::event_kind as ek, Manager},
    store::{now_ms, ReviewRow, Store},
};

pub const SUBMIT: &str = "submit_review";
pub const STATUS: &str = "review_status";
pub const VERDICT: &str = "review_verdict";

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
        json!({
            "name": VERDICT,
            "description": "Review sessions only: your verdict on the review this session was \
                            spawned for. It informs the human who decides; it publishes nothing. \
                            The session ends once it is given.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verdict": { "type": "string", "enum": ["approve", "request_changes"] },
                    "summary": { "type": "string", "description": "Two or three sentences: what the change does and whether it meets the requirements." },
                    "findings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "line": { "type": "integer" },
                                "severity": { "type": "string", "enum": ["blocking", "should", "nit"] },
                                "note": { "type": "string" },
                            },
                            "required": ["note"],
                        },
                    },
                },
                "required": ["verdict", "summary"],
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
        VERDICT => verdict(store, manager, ctx, args).await,
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

    // The cap, before anything else: complexity accretes because nothing says
    // no at submission time. A resubmission is capped the same way.
    let limits = &manager.cfg().review;
    let lines = capture.added + capture.removed;
    if lines > limits.max_diff_lines || capture.files.len() > limits.max_files {
        let reason = format!(
            "the diff is {lines} lines across {} files; the cap is {} lines and {} files. \
             Split the change into smaller submissions.",
            capture.files.len(),
            limits.max_diff_lines,
            limits.max_files
        );
        manager.record_event(
            &ctx.session_id,
            ek::REVIEW_REJECTED,
            json!({ "reason": reason, "lines": lines, "files": capture.files.len() }),
        );
        return Err(review::ReviewError::Rejected(reason).to_string());
    }

    // Deterministic checks, in a throwaway container, before any human or
    // model reads the diff. A failure is the reason the submit is refused.
    let commands = review::checks::commands_for(manager.cfg(), std::path::Path::new(&worktree));
    let slug = ctx.session_id.rsplit('-').next().unwrap_or("s").to_string();
    manager.set_checking(&ctx.session_id, true);
    manager.record_event(
        &ctx.session_id,
        ek::CHECK_STARTED,
        json!({ "commands": commands }),
    );
    let results = review::checks::run(
        manager.backend().as_ref(),
        manager.cfg(),
        std::path::Path::new(&worktree),
        &slug,
        &commands,
    )
    .await;
    for r in &results {
        manager.record_event(&ctx.session_id, ek::CHECK_RESULT, json!(r));
    }
    manager.set_checking(&ctx.session_id, false);
    if let Some(failed) = results.iter().find(|r| !r.ok) {
        let reason = format!(
            "check failed: `{}` (exit {}). Fix it and submit again.\n\n{}",
            failed.command,
            failed
                .exit
                .map(|c| c.to_string())
                .unwrap_or_else(|| "none".into()),
            failed.tail
        );
        manager.record_event(
            &ctx.session_id,
            ek::REVIEW_REJECTED,
            json!({ "reason": format!("check failed: {}", failed.command), "command": failed.command }),
        );
        return Err(reason);
    }
    let checks_json = serde_json::to_string(&results).ok();

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
        let _ = store.set_checks(id, checks_json.as_deref());
        manager.publish_queue().await;
        let reviewer = spawn_review_session(store, manager, ctx, id, &session).await;
        return Ok(json!({
            "review_id": id,
            "state": "new",
            "message": "Resubmitted. Call review_status to wait for the verdict.",
            "uncommitted": capture.uncommitted,
            "review_session": reviewer,
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
        checks_json,
        review_session_id: None,
        ai_verdict_json: None,
    };
    store.insert_review(&row).map_err(|e| e.to_string())?;
    manager.publish_queue().await;
    let reviewer = spawn_review_session(store, manager, ctx, &id, &session).await;

    Ok(json!({
        "review_id": id,
        "state": "new",
        "message": "Submitted for review. Nothing is published until a human approves. \
                    Call review_status to wait for the verdict.",
        "files": row.added + row.removed,
        "uncommitted": capture.uncommitted,
        "review_session": reviewer,
    }))
}

/// A fresh session that reads only the requirements and the diff, when the
/// channel binds a model for it (`phases.review.model`). Its verdict lands on
/// the review card; the human still decides. Returns what happened, for the
/// submitting agent's information.
async fn spawn_review_session(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    review_id: &str,
    implementing: &crate::store::SessionRow,
) -> Value {
    let bindings = manager.bindings(&ctx.channel);
    let Some(model) = bindings["phases"]["review"]["model"]
        .as_str()
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string)
    else {
        return json!({ "state": "none", "reason": "no review model bound on this channel (phases.review.model)" });
    };
    let Ok(Some(r)) = store.get_review(review_id) else {
        return json!({ "state": "none", "reason": "review not found" });
    };
    let short = &review_id[review_id.len().saturating_sub(12)..];
    let spec = crate::session::NewSession {
        channel: ctx.channel.clone(),
        repo_path: implementing.repo_path.clone(),
        branch: Some(format!("review/{short}")),
        work_item_id: implementing.work_item_id.clone(),
        model,
        budget_tokens: bindings["phases"]["review"]["budget_tokens"].as_i64(),
        node_id: None,
        phase: crate::session::Phase::Review,
        review_id: Some(review_id.to_string()),
        base_sha: Some(r.head_sha.clone()),
    };
    match manager.create_local(spec).await {
        Ok(row) => {
            let _ = store.set_review_session(review_id, &row.id);
            manager.publish_queue().await;
            json!({ "state": "started", "session_id": row.id })
        }
        Err(e) => json!({ "state": "failed", "reason": e.to_string() }),
    }
}

/// `review_verdict`: a review session's verdict on the review it was
/// spawned for. Recorded on the row, never a decision.
async fn verdict(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let session = store
        .get_session(&ctx.session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    if session.phase != "review" {
        return Err(
            "only a review session gives a verdict; submit_review is what an execute session calls"
                .into(),
        );
    }
    let review_id = session
        .review_id
        .clone()
        .ok_or("this review session has no review")?;
    let verdict = str_arg(args, "verdict")?;
    if verdict != "approve" && verdict != "request_changes" {
        return Err(format!("{verdict:?} is not a verdict"));
    }
    let summary = str_arg(args, "summary")?;
    let findings = args.get("findings").cloned().unwrap_or_else(|| json!([]));
    let v = json!({
        "verdict": verdict, "summary": summary, "findings": findings,
        "model": session.model, "session_id": ctx.session_id, "at_ms": now_ms(),
    });
    if !store
        .set_ai_verdict(&review_id, &v.to_string())
        .map_err(|e| e.to_string())?
    {
        return Err("the review is gone".into());
    }
    manager.record_event(
        &ctx.session_id,
        ek::REVIEW_VERDICT,
        json!({ "review_id": review_id, "verdict": verdict, "summary": summary }),
    );
    manager.publish_queue().await;
    manager.phase_done(&ctx.session_id).await;
    Ok(json!({ "review_id": review_id, "recorded": true }))
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
