//! MCP over HTTP, reachable only through the gateway's forward and only with a
//! token minted for one session. The harness cannot address another session's
//! tools, and a token outlives nothing.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
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

    let ctx = CallContext::session(session_id, channel, s.node_id.clone());
    if let Err(error) = s.manager.caller_active(&ctx) {
        return rpc_error(StatusCode::CONFLICT, &error.to_string());
    }
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
///
/// Every request stands alone. No `Mcp-Session-Id` is handed out and none is
/// read: clients we use treat it as disposable, so it named nothing durable.
/// What a call did goes to the channel's external log under the lane the
/// harness labels itself with, and only a tool call is logged, so connecting
/// leaves no trace.
pub async fn handle_external(
    State(s): State<AppState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    Json(msg): Json<Value>,
) -> Response {
    if !s.cfg.external.enabled {
        return rpc_error(
            StatusCode::FORBIDDEN,
            "this node does not answer harnesses outside the boundary: set [external] enabled = true in node.toml and restart",
        )
        .into_response();
    }
    // A newer client probes with `server/discover` before it initializes, and
    // falls back to `initialize` when the method is unknown.
    if msg.get("method").and_then(Value::as_str) == Some("server/discover") {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        return (
            StatusCode::OK,
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "unsupported method server/discover" },
            })),
        )
            .into_response();
    }
    let ctx = CallContext::external(lane_label(&headers), channel, s.node_id.clone());
    answer_external(&s, &ctx, &msg).await.into_response()
}

/// The header a harness labels its calls with, so the operator can tell its
/// agents apart. A label only: nothing is authorized, owned or routed by it.
const LANE_HEADER: &str = "x-tracon-agent";

/// The label a harness sent, trimmed and bounded so a stored column is not
/// whatever a caller sends. Absent, empty or not text is no label.
fn lane_label(headers: &HeaderMap) -> Option<String> {
    let label = headers.get(LANE_HEADER)?.to_str().ok()?.trim();
    (!label.is_empty()).then(|| label.chars().take(200).collect())
}

async fn answer_external(
    s: &AppState,
    ctx: &CallContext,
    msg: &Value,
) -> (StatusCode, Json<Value>) {
    if let Err(e) = s.manager.caller_active(ctx) {
        return rpc_error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string());
    }
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
        s.manager.record_for(
            ctx,
            crate::session::state::event_kind::TOOL_CALL,
            None,
            json!({ "title": title, "kind": crate::mcp::TOOL_KIND }),
        );
    }

    let mut unfinished = call.as_ref().map(|title| Abandoned {
        manager: s.manager.clone(),
        ctx: ctx.clone(),
        title: title.clone(),
        finished: false,
    });
    let answered = s.tools.handle(ctx, msg).await;
    if let Some(guard) = unfinished.as_mut() {
        guard.finished = true;
    }
    match answered {
        Some(response) => {
            if let Some(title) = &call {
                let failed = response["result"]["isError"] == true;
                s.manager.record_for(
                    ctx,
                    crate::session::state::event_kind::TOOL_RESULT,
                    None,
                    json!({ "title": title, "status": if failed { "error" } else { "ok" } }),
                );
            }
            (StatusCode::OK, Json(response))
        }
        None => (StatusCode::OK, Json(json!({}))),
    }
}

/// A call the harness hung up on. The server drops the handler when the
/// client goes away, so without this the log shows a call and never its end,
/// and nothing says whether it ran.
struct Abandoned {
    manager: crate::session::Manager,
    ctx: CallContext,
    title: String,
    finished: bool,
}

impl Drop for Abandoned {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.manager.record_for(
            &self.ctx,
            crate::session::state::event_kind::TOOL_RESULT,
            None,
            json!({ "title": self.title, "status": "abandoned" }),
        );
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

/// A client may close its session on exit. There is none to close.
pub async fn external_delete() -> StatusCode {
    StatusCode::NO_CONTENT
}

fn rpc_error(code: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({ "jsonrpc": "2.0", "error": { "code": -32001, "message": message } })),
    )
}
