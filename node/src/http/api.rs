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

pub async fn list_nodes(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    // A list from the start: the mesh adds peers here without changing shape.
    Ok(Json(json!([node_json(&s)?])))
}

fn node_json(s: &AppState) -> Result<serde_json::Value, ApiError> {
    let row = s.store().get_node(&s.node_id)?;
    let Some(n) = row else {
        return Ok(json!({ "id": s.node_id, "state": "unknown" }));
    };
    let models: serde_json::Value = n
        .models_json
        .as_deref()
        .and_then(|m| serde_json::from_str(m).ok())
        .unwrap_or_else(|| json!([]));
    Ok(json!({
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
    }))
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
    let rows = s.store().events_after(&id, q.after, q.limit.min(2000))?;
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

/// The queue, ordered on the node: waiting-on-you first, oldest first within
/// it. The interface renders this order rather than deciding it.
pub async fn queue(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let waiting = s.store().open_permissions()?;
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
        json!({ "waiting": waiting, "running": running, "ended": ended }),
    ))
}

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}
