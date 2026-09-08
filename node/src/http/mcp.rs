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

/// `POST /mcp/external/{channel}`: the same tools, for a harness the operator
/// runs themselves outside the boundary.
///
/// On the operator router rather than the harness one, deliberately. The
/// harness listener carries no guard because everything that can reach it is
/// already inside the boundary; a door there addressed by channel name would
/// be open to every session in it. Here `auth::guard` answers first — loopback
/// is the operator, anything else presents the operator token — which is
/// exactly the trust an external harness runs under, being a process of the
/// operator's on the operator's own machine.
pub async fn handle_external(
    State(s): State<AppState>,
    Path(channel): Path<String>,
    Json(msg): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !s.cfg.external.enabled {
        return rpc_error(
            StatusCode::FORBIDDEN,
            "this node does not answer harnesses outside the boundary: set [external] enabled = true in node.toml and restart",
        );
    }
    let session_id = match s.manager.attach_external(&channel).await {
        Ok(id) => id,
        Err(e) => return rpc_error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    };
    let ctx = CallContext {
        session_id: session_id.clone(),
        channel,
        node_id: s.node_id.clone(),
    };

    // A boundaried session's calls arrive as harness events and land in its
    // log on the way past. Nothing reports these, so the door records them:
    // what the node was asked to do on the operator's behalf is the half of
    // the guarantee that survives outside the boundary.
    let call = (msg.get("method").and_then(Value::as_str) == Some("tools/call")).then(|| {
        let p = msg.get("params").cloned().unwrap_or(Value::Null);
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let args = p.get("arguments").cloned().unwrap_or(json!({}));
        crate::mcp::summarize(&name, &args)
    });
    if let Some(title) = &call {
        s.manager.record_event(
            &session_id,
            crate::session::state::event_kind::TOOL_CALL,
            json!({ "title": title, "kind": crate::mcp::TOOL_KIND }),
        );
    }

    match s.tools.handle(&ctx, &msg).await {
        Some(response) => {
            if let Some(title) = &call {
                let failed = response["result"]["isError"] == true;
                s.manager.record_event(
                    &session_id,
                    crate::session::state::event_kind::TOOL_RESULT,
                    json!({ "title": title, "status": if failed { "error" } else { "ok" } }),
                );
            }
            (StatusCode::OK, Json(response))
        }
        None => (StatusCode::OK, Json(json!({}))),
    }
}

/// The transport is one POST per message. A client probing for a server-sent
/// stream is told so rather than being handed the interface's index page by
/// the SPA fallback.
pub async fn external_get() -> (StatusCode, Json<Value>) {
    rpc_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "this endpoint answers one POST per message; it opens no stream",
    )
}

/// Some clients close a session on exit. Honour it: the attachment ends and
/// the home stops showing it as connected.
pub async fn external_delete(State(s): State<AppState>, Path(channel): Path<String>) -> StatusCode {
    let _ = s.manager.detach_external(&channel).await;
    StatusCode::NO_CONTENT
}

fn rpc_error(code: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({ "jsonrpc": "2.0", "error": { "code": -32001, "message": message } })),
    )
}
