//! The operator API. Small on purpose: the interface reads snapshots and the
//! stream, and writes through a handful of commands.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    adapter::HarnessAdapter,
    config::Config,
    session::{Manager, NewSession, SessionError},
    store::Store,
};

#[derive(Clone)]
pub struct AppState {
    pub manager: Manager,
    pub cfg: Arc<Config>,
    pub adapter: Arc<dyn HarnessAdapter>,
    pub node_id: String,
    pub tools: Arc<crate::mcp::Tools>,
}

impl AppState {
    pub fn store(&self) -> &Arc<Store> {
        self.manager.store()
    }
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({ "error": { "code": self.0.as_u16(), "message": self.1 } })),
        )
            .into_response()
    }
}

impl From<SessionError> for ApiError {
    fn from(e: SessionError) -> Self {
        let code = match e {
            // A missing model is a malformed request, not a conflict.
            SessionError::ModelRequired | SessionError::BadBudget => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            SessionError::NotFound => StatusCode::NOT_FOUND,
            // The node refusing to run harnesses, a version mismatch, or a
            // session that will not take the command are all state conflicts.
            SessionError::NodeRefused(_)
            | SessionError::VersionMismatch { .. }
            | SessionError::Rejected(_) => StatusCode::CONFLICT,
            SessionError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(code, e.to_string())
    }
}

impl From<crate::store::StoreError> for ApiError {
    fn from(e: crate::store::StoreError) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub async fn get_node(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(node_json(&s)?))
}

/// Every node this one knows: itself first, then peers as the mesh reports them.
pub async fn list_nodes(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().list_nodes()?;
    let mut out: Vec<serde_json::Value> = rows.iter().map(node_row_json).collect();
    out.sort_by_key(|n| n["is_self"] != true);
    Ok(Json(json!(out)))
}

/// Hub reachability and mesh counters. Until the mesh client lands this
/// reports `disabled`, which the interface treats as "no hub configured".
pub async fn get_mesh(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "hub": "disabled",
        "hub_url": s.cfg.mesh.hub_url,
        "node_id": s.node_id,
        "fingerprint": proto::enroll::fingerprint_hex(&s.node_id),
    })))
}

pub(crate) fn node_json(s: &AppState) -> Result<serde_json::Value, ApiError> {
    let row = s.store().get_node(&s.node_id)?;
    let Some(n) = row else {
        return Ok(
            json!({ "id": s.node_id, "state": "unknown", "is_self": true, "reachable": true }),
        );
    };
    Ok(node_row_json(&n))
}

pub(crate) fn node_row_json(n: &crate::store::NodeRow) -> serde_json::Value {
    let models: serde_json::Value = n
        .models_json
        .as_deref()
        .and_then(|m| serde_json::from_str(m).ok())
        .unwrap_or_else(|| json!([]));
    json!({
        "id": n.id,
        "name": n.name,
        "state": n.state,
        "failed_check": n.failed_check,
        "failed_detail": n.failed_detail,
        "harness": {
            "id": n.harness_id,
            "pinned": n.harness_pinned,
            "found": n.harness_found,
            "mismatch": n.harness_found.as_ref().map(|f| f != &n.harness_pinned).unwrap_or(false),
        },
        "models": models,
        "checked_at_ms": n.checked_at_ms,
        "is_self": n.is_self != 0,
        "reachable": n.reachable != 0,
        "last_seen_ms": n.last_seen_ms,
        "x25519_pub": n.x25519_pub,
    })
}

#[derive(Deserialize)]
pub struct ListSessions {
    state: Option<String>,
}

pub async fn list_sessions(
    State(s): State<AppState>,
    Query(q): Query<ListSessions>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().list_sessions(q.state.as_deref())?;
    Ok(Json(json!(rows)))
}

pub async fn create_session(
    State(s): State<AppState>,
    Json(spec): Json<NewSession>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let row = s.manager.create(spec, s.adapter.clone()).await?;
    Ok((StatusCode::CREATED, Json(json!(row))))
}

pub async fn get_session(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = s
        .store()
        .get_session(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?;
    let waiting: Vec<_> = s
        .store()
        .open_permissions()?
        .into_iter()
        .filter(|p| p.session_id == id)
        .collect();
    Ok(Json(json!({ "session": row, "waiting": waiting })))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    500
}

pub async fn session_events(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = s
        .store()
        .events_after(&id, q.after, q.limit.clamp(0, 2000))?;
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
pub struct PromptBody {
    text: String,
}

pub async fn prompt(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PromptBody>,
) -> ApiResult<StatusCode> {
    s.manager.prompt(&id, b.text).await?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn kill(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    s.manager.kill(&id).await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct DraftBody {
    text: String,
}

/// The node holds unsent drafts, so a closed tab or an evicted phone loses
/// nothing typed.
pub async fn put_draft(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<DraftBody>,
) -> ApiResult<StatusCode> {
    let text = (!b.text.is_empty()).then_some(b.text);
    s.store().set_draft(&id, text.as_deref())?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct AnswerBody {
    option_id: String,
}

pub async fn answer_permission(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<AnswerBody>,
) -> ApiResult<StatusCode> {
    s.manager.answer(&id, b.option_id).await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
pub struct VerdictBody {
    /// "approve" or "reject". Anything else is refused rather than guessed at.
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
    /// The operator's edits to what gets published, if they made any.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

pub async fn get_review(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Claim on open, then read: a response that showed the row as it stood
    // before the claim would tell the operator something already untrue.
    let _ = s.store().claim_review(&id);
    let r = s
        .store()
        .get_review(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such review".into()))?;
    let stale = staleness_of(&s, &r).await;
    s.manager.publish_queue().await;
    Ok(Json(json!({ "review": r, "stale": stale })))
}

/// What changed in the worktree since submit. An empty list means the diff
/// still describes the branch.
async fn staleness_of(s: &AppState, r: &crate::store::ReviewRow) -> Vec<String> {
    let Ok(Some(session)) = s.store().get_session(&r.session_id) else {
        return vec!["the session is gone".into()];
    };
    let Some(worktree) = session.worktree_path else {
        return vec!["the worktree is gone".into()];
    };
    let files: Vec<crate::review::FileAtSubmit> =
        serde_json::from_str(&r.files).unwrap_or_default();
    crate::review::staleness(&worktree, &r.head_sha, &files).await
}

/// Called when the operator leaves the review screen. Explicit release is the
/// common case; the sweeper is for clients that vanish without saying so.
pub async fn release_review(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    s.store().release_review(&id)?;
    s.manager.publish_queue().await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn decide_review(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<VerdictBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let r = s
        .store()
        .get_review(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such review".into()))?;
    if r.state == "approved" || r.state == "rejected" {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("this review was already {}", r.state),
        ));
    }

    match b.verdict.as_str() {
        "revise" => {
            // Changes requested. The review stays open as one evolving thread,
            // and the notes go back to the agent, which is still the only
            // writer to the worktree.
            let notes = b
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "asking for changes needs a note saying what to change".into(),
                ))?;
            if !s.store().request_changes(&id, notes)? {
                let now = s.store().get_review(&id)?.map(|r| r.state);
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "this review is no longer awaiting a verdict ({})",
                        now.as_deref().unwrap_or("gone")
                    ),
                ));
            }
            s.manager.publish_queue().await;
            Ok(Json(json!({ "state": "revising" })))
        }
        "reject" => {
            // A bare rejection teaches the agent nothing.
            let reason = b
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "a rejection needs a reason".into(),
                ))?;
            // The guard is in the UPDATE: if the row is no longer awaiting a
            // verdict, say so rather than reporting a rejection that did not land.
            let resolved =
                s.store()
                    .resolve_review(&id, "rejected", Some(reason), None, None, None, 0)?;
            if !resolved {
                let now = s.store().get_review(&id)?.map(|r| r.state);
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "this review is no longer awaiting a verdict ({})",
                        now.as_deref().unwrap_or("gone")
                    ),
                ));
            }
            s.manager.publish_queue().await;
            Ok(Json(json!({ "state": "rejected" })))
        }
        "approve" => {
            // Stale means the branch is no longer what was reviewed. Approving
            // it would publish something nobody read.
            let stale = staleness_of(&s, &r).await;
            if !stale.is_empty() {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("changed since submit: {}", stale.join(", ")),
                ));
            }
            let session = s
                .store()
                .get_session(&r.session_id)?
                .ok_or(ApiError(StatusCode::CONFLICT, "the session is gone".into()))?;
            let worktree = session.worktree_path.ok_or(ApiError(
                StatusCode::CONFLICT,
                "the worktree is gone".into(),
            ))?;
            let target: crate::review::publish::Target = serde_json::from_str(&r.target)
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            let title = b.title.as_deref().unwrap_or(r.approved_title()).to_string();
            let body = b.body.as_deref().unwrap_or(r.approved_body()).to_string();

            // Claim the publish atomically: only the transition that moves the
            // row into `publishing` may push. Two concurrent approves cannot both
            // win this, so the change is opened once, not once per request.
            if !s.store().begin_publish(&id)? {
                let now = s.store().get_review(&id)?.map(|r| r.state);
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "this review is already being decided ({})",
                        now.as_deref().unwrap_or("gone")
                    ),
                ));
            }

            // The node publishes, using a credential the harness never had, and
            // pins the push to the reviewed commit.
            let result = crate::review::publish::publish(
                &s.tools.broker,
                &s.cfg,
                &r.channel,
                &worktree,
                &target,
                &r.head_sha,
                &title,
                &body,
            )
            .await;

            match result {
                Ok(published) => {
                    s.store().finish_publish(&id, &title, &body, &published)?;
                    s.manager.publish_queue().await;
                    Ok(Json(json!({ "state": "approved", "published": published })))
                }
                Err(e) => {
                    // The forge refused: undo the publishing claim so the review
                    // returns to the queue rather than being stuck mid-publish.
                    // The operator approved, the forge refused, and that is a
                    // thing to fix rather than a verdict.
                    s.store().abort_publish(&id)?;
                    s.manager.publish_queue().await;
                    Err(ApiError(StatusCode::BAD_GATEWAY, e.to_string()))
                }
            }
        }
        other => Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{other:?} is not a verdict"),
        )),
    }
}

/// The queue, ordered on the node: waiting-on-you first, oldest first within
/// it. The interface renders this order rather than deciding it.
pub async fn queue(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let waiting = s.store().open_permissions()?;
    // Permission requests before reviews: requests expire, reviews do not.
    let reviews = s.store().open_reviews()?;
    let sessions = s.store().list_sessions(None)?;
    let running: Vec<_> = sessions
        .iter()
        .filter(|s| !crate::session::state::SessionState::from_stored(&s.state).is_terminal())
        .cloned()
        .collect();
    let ended: Vec<_> = sessions
        .iter()
        .filter(|s| crate::session::state::SessionState::from_stored(&s.state).is_terminal())
        .take(20)
        .cloned()
        .collect();
    Ok(Json(
        json!({ "waiting": waiting, "reviews": reviews, "running": running, "ended": ended }),
    ))
}

/// Re-probe the harness for its model list, for when the probe failed at
/// startup or the harness gained providers since.
pub async fn refresh_models(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let selinux = crate::boundary::selinux_enabled().await;
    let mut spec = crate::runner::podman::RunSpec::from_config(&s.cfg, selinux);
    spec.extra_mounts = crate::session::materialize::state_mounts().unwrap_or_default();
    let runner = crate::runner::podman::PodmanRunner::new(spec);
    let models = s
        .adapter
        .probe_models(&runner)
        .await
        .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
    if let Ok(Some(mut node)) = s.store().get_node(&s.node_id) {
        node.models_json = serde_json::to_string(&models).ok();
        let _ = s.store().put_node(&node);
        // Push the refreshed node to any live client, so a model list (or a
        // refused state) reaches the interface without a reload.
        s.manager
            .bus()
            .publish(crate::stream::Frame::Node(node_json(&s)?));
    }
    Ok(Json(serde_json::json!(models)))
}

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}
