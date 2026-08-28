//! MCP over HTTP, reachable only through the gateway's forward and only with a
//! token minted for one session. The harness cannot address another session's
//! tools, and a token outlives nothing.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde_json::{json, Value};

use super::api::AppState;
use crate::mcp::CallContext;

/// `POST /mcp/{session_id}`, with `Authorization: Bearer <session token>`.
pub async fn handle(
    State(s): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(msg): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    let Some(channel) = s.manager.authorize_tool_call(&session_id, presented).await else {
        // Deliberately the same answer for a bad token and an unknown session:
        // a caller learns nothing about which sessions exist.
        return (
            StatusCode::UNAUTHORIZED,
            Json(
                json!({ "jsonrpc": "2.0", "error": { "code": -32001, "message": "unauthorized" } }),
            ),
        );
    };

    let ctx = CallContext {
        session_id,
        channel,
        node_id: s.node_id.clone(),
    };
    match s.tools.handle(&ctx, &msg).await {
        Some(response) => (StatusCode::OK, Json(response)),
        // A notification: accepted, nothing to say back.
        None => (StatusCode::OK, Json(json!({}))),
    }
}
