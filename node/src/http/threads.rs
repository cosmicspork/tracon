//! The operator's review threads: open one on a review, on a file of it or
//! on a line or range of lines, reply to one, resolve or reopen one.
//!
//! A thread lives on the node that owns its review. A review mirrored here
//! from a peer is changed by forwarding the operation to that owner
//! (`Command::ReviewThread`), which anchors a new thread on its own copy of
//! the revision's diff; the page reads the owner's threads back with the
//! rest of the review's detail.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::review::threads::{self, Anchor, MAX_COMMENT_BYTES};
use crate::store::review_thread::{ReviewThreadRow, ADDRESSED, OPERATOR, RESOLVED, UNRESOLVED};

use super::api::{ApiError, ApiResult, AppState};

/// One change to a review's threads, as a request names it and as it is
/// forwarded to the review's owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadOp {
    Create {
        /// The revision the operator was reading; the latest when none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision_id: Option<String>,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        side: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_line: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_line: Option<i64>,
        body: String,
    },
    Reply {
        thread_id: String,
        body: String,
    },
    Resolve {
        thread_id: String,
    },
    Reopen {
        thread_id: String,
    },
}

/// `POST /api/reviews/{id}/threads`: open a thread.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    #[serde(default)]
    pub revision_id: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub start_line: Option<i64>,
    #[serde(default)]
    pub end_line: Option<i64>,
    pub body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyBody {
    pub body: String,
}

pub async fn create(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<CreateBody>,
) -> ApiResult<Json<Value>> {
    let op = ThreadOp::Create {
        revision_id: b.revision_id,
        kind: b.kind,
        path: b.path,
        side: b.side,
        start_line: b.start_line,
        end_line: b.end_line,
        body: b.body,
    };
    run(&s, &id, op).await.map(Json)
}

/// `POST /api/reviews/{id}/threads/{thread}/replies`
pub async fn reply(
    State(s): State<AppState>,
    Path((id, thread_id)): Path<(String, String)>,
    Json(b): Json<ReplyBody>,
) -> ApiResult<Json<Value>> {
    run(
        &s,
        &id,
        ThreadOp::Reply {
            thread_id,
            body: b.body,
        },
    )
    .await
    .map(Json)
}

/// `POST /api/reviews/{id}/threads/{thread}/resolve`
pub async fn resolve(
    State(s): State<AppState>,
    Path((id, thread_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    run(&s, &id, ThreadOp::Resolve { thread_id })
        .await
        .map(Json)
}

/// `POST /api/reviews/{id}/threads/{thread}/reopen`
pub async fn reopen(
    State(s): State<AppState>,
    Path((id, thread_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    run(&s, &id, ThreadOp::Reopen { thread_id }).await.map(Json)
}

/// Apply `op` where the review is owned: here, or forwarded to its owner.
/// Answers with the review's threads as they stand after it.
async fn run(s: &AppState, review_id: &str, op: ThreadOp) -> ApiResult<Value> {
    let r = s
        .store()
        .get_review(review_id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such review"))?;
    if r.node_id == s.node_id {
        return apply(s, review_id, op);
    }
    let mesh = s.mesh.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "this review's threads are held by the node that owns it, and this node is not on \
             a mesh",
        )
    })?;
    if !mesh.peer_reachable(&r.node_id) {
        return Err(ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "the node that owns this review is unreachable; its threads can be changed once it \
             returns",
        ));
    }
    let op = serde_json::to_value(&op)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let timeout = std::time::Duration::from_secs(s.cfg.mesh.command_timeout_secs.max(1));
    match mesh
        .command(
            &r.node_id,
            proto::frame::Command::ReviewThread {
                review_id: review_id.to_string(),
                change: op,
            },
            timeout,
        )
        .await
    {
        Ok(v) => Ok(v),
        Err(crate::mesh::forward::CommandError::Timeout) => Err(ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "the node that owns this review did not answer",
        )),
        Err(e) => Err(ApiError::new(StatusCode::CONFLICT, e.to_string())),
    }
}

/// A peer's thread operation on a review this node owns. The command's
/// scope (`forwarded_scope`) has checked the review is owned here and the
/// sender is on its channel.
pub async fn forwarded(s: &AppState, review_id: &str, op: Value) -> ApiResult<Value> {
    let op: ThreadOp = serde_json::from_value(op).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("thread operation is malformed: {e}"),
        )
    })?;
    apply(s, review_id, op)
}

fn comment(body: &str) -> ApiResult<&str> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a thread's comment says something",
        ));
    }
    if body.len() > MAX_COMMENT_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "a thread's comment is kept up to {} KiB",
                MAX_COMMENT_BYTES / 1024
            ),
        ));
    }
    Ok(body)
}

/// The thread `thread_id`, if it is on `review_id`.
fn thread_on(s: &AppState, review_id: &str, thread_id: &str) -> ApiResult<ReviewThreadRow> {
    s.store()
        .review_thread(thread_id)?
        .filter(|t| t.review_id == review_id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such thread on this review"))
}

/// Apply `op` to a review this node owns.
pub(crate) fn apply(s: &AppState, review_id: &str, op: ThreadOp) -> ApiResult<Value> {
    let store = s.store();
    let r = store
        .get_review(review_id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such review"))?;
    if r.kind == crate::store::reports::KIND {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a narrative report has no diff to thread; acknowledge it or request changes",
        ));
    }
    // A rejection is final, and a review gone from its node has nothing to
    // answer a thread with.
    if matches!(r.state.as_str(), "rejected" | "gone") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("this review was {}; its threads are closed", r.state),
        ));
    }
    match op {
        ThreadOp::Create {
            revision_id,
            kind,
            path,
            side,
            start_line,
            end_line,
            body,
        } => {
            let body = comment(&body)?;
            let revision = match revision_id.as_deref().filter(|id| !id.is_empty()) {
                Some(id) => store
                    .review_revision(id)?
                    .filter(|rev| rev.review_id == review_id)
                    .ok_or_else(|| {
                        ApiError::new(StatusCode::NOT_FOUND, "no such revision of this review")
                    })?,
                None => store.latest_review_revision(review_id)?.ok_or_else(|| {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "this review has no revision recorded to anchor a thread to",
                    )
                })?,
            };
            // Bound to a side even when none was named: a file or line
            // thread reads the change, the new side, unless told otherwise.
            let side = match kind.as_str() {
                threads::GENERAL => side,
                _ => Some(side.unwrap_or_else(|| threads::NEW.to_string())),
            };
            let anchor = Anchor {
                kind: kind.clone(),
                path: path.clone(),
                side: side.clone(),
                start_line,
                end_line: if kind == threads::LINE {
                    end_line.or(start_line)
                } else {
                    end_line
                },
            };
            let anchored = threads::anchor_in(&revision.diff, &anchor)
                .map_err(|why| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, why))?;
            let now = crate::store::now_ms();
            store.insert_review_thread(
                &ReviewThreadRow {
                    id: uuid::Uuid::now_v7().to_string(),
                    review_id: review_id.to_string(),
                    revision_id: revision.id.clone(),
                    kind,
                    path: anchor.path,
                    side: anchor.side,
                    start_line: anchor.start_line,
                    end_line: anchor.end_line,
                    anchor: anchored,
                    state: UNRESOLVED.into(),
                    addressed_revision_id: None,
                    created_ms: now,
                    updated_ms: now,
                },
                body,
            )?;
        }
        ThreadOp::Reply { thread_id, body } => {
            let body = comment(&body)?;
            thread_on(s, review_id, &thread_id)?;
            let latest = store.latest_review_revision(review_id)?;
            store.reply_to_review_thread(
                &thread_id,
                OPERATOR,
                body,
                latest.as_ref().map(|rev| rev.id.as_str()),
            )?;
        }
        ThreadOp::Resolve { thread_id } => {
            thread_on(s, review_id, &thread_id)?;
            if !store.set_review_thread_state(&thread_id, &[UNRESOLVED, ADDRESSED], RESOLVED)? {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "this thread is already resolved",
                ));
            }
        }
        ThreadOp::Reopen { thread_id } => {
            thread_on(s, review_id, &thread_id)?;
            if !store.set_review_thread_state(&thread_id, &[RESOLVED, ADDRESSED], UNRESOLVED)? {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "this thread is open already",
                ));
            }
        }
    }
    Ok(json!({ "threads": threads::of_review(store, review_id, false)? }))
}
