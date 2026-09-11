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
    store::{candidate_id, now_ms, CandidateRow, ReviewRevisionRow, ReviewRow, Store},
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
                            Resubmit with the same review_id after making changes. A harness you \
                            run yourself passes `worktree`: the absolute path of a worktree whose \
                            repository is under the node's [external] repo_roots; its branch is \
                            what gets pushed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "worktree": { "type": "string", "description": "Absolute path of the worktree to review. Only for a harness you run yourself." },
                    "title": { "type": "string", "description": "The change's title, as it should appear." },
                    "body": { "type": "string", "description": "What is not obvious from the diff: intent, trade-offs, follow-ups." },
                    "provider": { "type": "string", "enum": ["github", "gitlab"] },
                    "project": { "type": "string", "description": "owner/name on GitHub, the project path on GitLab." },
                    "base": { "type": "string", "description": "Branch to merge into. Defaults to the branch the worktree was created from." },
                    "review_id": { "type": "string", "description": "Set to resubmit an existing review after changes were requested." },
                    "rerun_checks": { "type": "boolean", "description": "Run configured required checks again even when exact immutable evidence exists. This cannot alter which checks are required." },
                },
                "required": ["title", "body", "provider", "project"],
            },
        }),
        json!({
            "name": STATUS,
            "description": "Wait for a review's verdict and return it. Blocks until the review is \
                            decided or the wait elapses (60 seconds unless you say otherwise; a \
                            human often takes longer, so call it again while it says it is still \
                            waiting). On approval the node publishes the approved text itself and \
                            returns where it landed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "review_id": { "type": "string" },
                    "wait_secs": { "type": "integer", "description": "How long to block, up to 600. 0 returns the current state." },
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
    let mut session = store
        .get_session(&ctx.session_id)
        .map_err(|e| e.to_string())?
        .ok_or("this session is gone")?;
    let external = session.harness_id == crate::session::external::HARNESS_ID;
    let named = args.get("worktree").and_then(Value::as_str);
    let (worktree, branch) = match (external, named) {
        (true, Some(path)) => {
            let roots: Vec<_> = manager
                .cfg()
                .external
                .repo_roots
                .iter()
                .map(|r| crate::config::expand_home(r))
                .collect();
            let at = review::locate_worktree(path, &roots)
                .await
                .map_err(|e| e.to_string())?;
            // A review session reads the diff in its own worktree of the same
            // repository.
            session.repo_path = at.repo;
            (at.worktree, at.branch)
        }
        (true, None) => {
            return Err(
                "worktree is required: the absolute path of the worktree to put up for review"
                    .into(),
            )
        }
        (false, Some(_)) => {
            return Err(
                "worktree is for a harness you run yourself; this session reviews its own".into(),
            )
        }
        (false, None) => (
            manager
                .snapshot_workspace(&ctx.session_id)
                .await
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned(),
            session.branch.clone(),
        ),
    };

    let title = str_arg(args, "title")?;
    let body = str_arg(args, "body")?;
    let provider = str_arg(args, "provider")?;
    let project = str_arg(args, "project")?;
    if review::publish::Provider::parse(&provider).is_none() {
        return Err(format!(
            "{provider} is not a provider this node publishes to"
        ));
    }
    // Validate resubmission ownership before capturing or executing anything
    // from this worktree. A review id cannot be used as a way to make another
    // session's candidate consume a check runner.
    let resubmission = match args.get("review_id").and_then(Value::as_str) {
        Some(id) => {
            let existing = store
                .get_review(id)
                .map_err(|error| error.to_string())?
                .ok_or("no review with that id")?;
            if !owns(store, ctx, &existing) {
                return Err("that review belongs to another session".into());
            }
            let was = serde_json::from_str::<Target>(&existing.target)
                .ok()
                .and_then(|target| target.worktree);
            if external && was.as_deref() != Some(worktree.as_str()) {
                return Err(format!(
                    "resubmit from the worktree the review was made from ({})",
                    was.unwrap_or_default()
                ));
            }
            Some(existing)
        }
        None => None,
    };
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
    let capture = review::capture(&worktree, &range_base, &branch)
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
    // Capture the committed candidate before any check can execute. The
    // snapshot is a hardened Git tree import, not this mutable worktree.
    let snapshot = review::snapshot_candidate(
        &worktree,
        &capture.head_sha,
        manager.cfg().supervision.max_snapshot_bytes,
    )
    .await
    .map_err(|error| error.to_string())?;
    let candidate = CandidateRow {
        id: candidate_id(&capture.head_sha, &ctx.channel),
        head_sha: capture.head_sha.clone(),
        tree_sha: Some(snapshot.tree_sha.clone()),
        channel: ctx.channel.clone(),
        owner_session_id: ctx.session_id.clone(),
        source_kind: "git".into(),
        captured_ms: now_ms(),
        capture_json: json!({
            "method": "hardened_git_tree",
            "materialized": true,
            "max_snapshot_bytes": manager.cfg().supervision.max_snapshot_bytes,
        })
        .to_string(),
    };
    let created = store
        .insert_candidate(&candidate)
        .map_err(|error| error.to_string())?;
    if !created {
        store
            .record_candidate_snapshot(&candidate.id, &snapshot.tree_sha, &candidate.capture_json)
            .map_err(|error| error.to_string())?;
    }
    store
        .insert_candidate_files(&candidate.id, &snapshot.files)
        .map_err(|error| error.to_string())?;
    let candidate = store
        .candidate(&candidate.id)
        .map_err(|error| error.to_string())?
        .ok_or("candidate disappeared while it was captured")?;
    let checks = run_checks(
        store,
        manager,
        ctx,
        &candidate,
        &snapshot.root,
        args.get("rerun_checks").and_then(Value::as_bool).unwrap_or(false),
    )
    .await?;
    let checks_json = Some(serde_json::to_string(&checks.results).unwrap_or_else(|_| "[]".into()));

    // A resubmission keeps the same card and the same thread.
    if let Some(existing) = resubmission {
        let id = existing.id.as_str();
        let revision = ReviewRevisionRow {
            id: uuid::Uuid::now_v7().to_string(),
            review_id: id.to_string(),
            candidate_id: candidate.id.clone(),
            title: title.clone(),
            body: body.clone(),
            diff: capture.diff.clone(),
            files: files.clone(),
            head_sha: capture.head_sha.clone(),
            context_json: serde_json::to_string(&capture.contexts).unwrap_or_else(|_| "[]".into()),
            created_ms: now_ms(),
        };
        let revised = store
            .revise_review_with_revision(
                id,
                &title,
                &body,
                capture.added,
                capture.removed,
                checks_json.as_deref(),
                &revision,
            )
            .map_err(|error| error.to_string())?;
        if !revised {
            return Err("that review is no longer awaiting a revision".into());
        }
        manager.publish_queue().await;
        let reviewer = spawn_review_session(store, manager, ctx, id, &session).await;
        return Ok(json!({
            "review_id": id,
            "state": "new",
            "message": "Resubmitted. Call review_status to wait for the verdict.",
            "uncommitted": capture.uncommitted,
            "review_session": reviewer,
            "candidate_id": candidate.id,
            "checks_reused": checks.reused,
        }));
    }

    let target = Target {
        provider: provider.clone(),
        project: project.clone(),
        base: base.clone(),
        branch,
        worktree: external.then(|| worktree.clone()),
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
        revision_patch: None,
    };
    let revision = ReviewRevisionRow {
        id: uuid::Uuid::now_v7().to_string(),
        review_id: id.clone(),
        candidate_id: candidate.id.clone(),
        title: row.title.clone(),
        body: row.body.clone(),
        diff: row.diff.clone(),
        files: row.files.clone(),
        head_sha: row.head_sha.clone(),
        context_json: serde_json::to_string(&capture.contexts).unwrap_or_else(|_| "[]".into()),
        created_ms: row.created_ms,
    };
    store
        .insert_review_with_revision(&row, &revision)
        .map_err(|error| error.to_string())?;
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
        "candidate_id": candidate.id,
        "checks_reused": checks.reused,
    }))
}

/// Whose review this is. An attachment ends when it goes idle and the next
/// call attaches a new one, so a review a harness the operator runs submitted
/// belongs to the channel's attachments rather than to one of them.
fn owns(store: &Store, ctx: &CallContext, r: &ReviewRow) -> bool {
    if r.session_id == ctx.session_id {
        return true;
    }
    let external = |id: &str| {
        store
            .get_session(id)
            .ok()
            .flatten()
            .is_some_and(|s| s.harness_id == crate::session::external::HARNESS_ID)
    };
    r.channel == ctx.channel && external(&ctx.session_id) && external(&r.session_id)
}

/// Required checks against the immutable candidate snapshot. The candidate
/// owner receives the verification milestone even if a later prose-only
/// revision is submitted by another attached session.
async fn run_checks(
    store: &Arc<Store>,
    manager: &Manager,
    ctx: &CallContext,
    candidate: &CandidateRow,
    snapshot: &std::path::Path,
    force_rerun: bool,
) -> Result<review::checks::CheckReport, String> {
    let commands = review::checks::required_definitions(manager.cfg());
    let slug = ctx.session_id.rsplit('-').next().unwrap_or("s").to_string();
    manager.set_checking(&ctx.session_id, true);
    manager.record_event(
        &ctx.session_id,
        ek::CHECK_STARTED,
        json!({
            "candidate_id": candidate.id,
            "head_sha": candidate.head_sha,
            "commands": commands,
            "rerun": force_rerun,
        }),
    );
    let report = review::checks::run_required(
        manager.backend().as_ref(),
        manager.cfg(),
        store,
        candidate,
        snapshot,
        &slug,
        force_rerun,
    )
    .await;
    manager.set_checking(&ctx.session_id, false);
    let report = report?;
    for result in &report.results {
        manager.record_event(
            &ctx.session_id,
            ek::CHECK_RESULT,
            json!({ "candidate_id": candidate.id, "result": result }),
        );
    }
    if report.all_required_passed {
        manager.record_event(
            &candidate.owner_session_id,
            ek::CANDIDATE_VERIFIED,
            json!({
                "candidate_id": candidate.id,
                "head_sha": candidate.head_sha,
                "reused": report.reused,
            }),
        );
    }
    if let Some(failed) = report.results.iter().find(|result| !result.ok) {
        let reason = format!(
            "check {}: `{}` (exit {}). Fix it and submit again.\n\n{}",
            failed.outcome,
            failed.command,
            failed
                .exit
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".into()),
            failed.tail
        );
        manager.record_event(
            &ctx.session_id,
            ek::REVIEW_REJECTED,
            json!({
                "candidate_id": candidate.id,
                "reason": format!("check {}: {}", failed.outcome, failed.command),
                "command": failed.command,
            }),
        );
        return Err(reason);
    }
    Ok(report)
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
    let source = match manager.snapshot_workspace(&implementing.id).await {
        Ok(path) => path,
        Err(error) => return json!({ "state": "failed", "reason": error.to_string() }),
    };
    let workspace_id = format!("review-{review_id}");
    if let Err(error) =
        crate::workspace::from_snapshot(manager.backend().as_ref(), &workspace_id, &source).await
    {
        return json!({ "state": "failed", "reason": error.to_string() });
    }
    let short = &review_id[review_id.len().saturating_sub(12)..];
    let spec = crate::session::NewSession {
        channel: ctx.channel.clone(),
        repo_path: String::new(),
        branch: Some(format!("review/{short}")),
        work_item_id: implementing.work_item_id.clone(),
        workspace_id: Some(workspace_id),
        model,
        budget_tokens: bindings["phases"]["review"]["budget_tokens"].as_i64(),
        initial_prompt: None,
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
    // Short by default: an MCP client gives up on a call long before a human
    // gets to a review, and a call that comes back saying "still waiting" is
    // one the agent knows to repeat.
    let wait = args
        .get("wait_secs")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .min(600);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);

    loop {
        let r = store
            .get_review(&id)
            .map_err(|e| e.to_string())?
            .ok_or("no review with that id")?;
        if !owns(store, ctx, &r) {
            return Err("that review belongs to another session".into());
        }
        match r.state.as_str() {
            "revising" => {
                // A patch means the operator edited the diff rather than
                // describing the change. Applying it verbatim is the point:
                // it is what they want, and the agent is still the hand that
                // writes it to the worktree.
                let message = if r.revision_patch.is_some() {
                    "Changes were requested, with an edited diff. Apply `patch` to the \
                     worktree exactly as given (`git apply`), make any further changes the \
                     notes ask for, commit, then call submit_review again with this \
                     review_id."
                } else {
                    "Changes were requested. Make them, commit, then call submit_review \
                     again with this review_id."
                };
                return Ok(json!({
                    "review_id": r.id,
                    "state": "changes_requested",
                    "notes": r.verdict_reason,
                    "patch": r.revision_patch,
                    "message": message,
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
