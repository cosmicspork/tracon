//! MCP over HTTP, reachable only through the gateway's forward and only with a
//! token minted for one session. The harness cannot address another session's
//! tools, and a token outlives nothing.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
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

    let ctx = CallContext {
        session_id,
        channel,
        node_id: s.node_id.clone(),
    };
    if let Err(error) = s.manager.ensure_active(&ctx.session_id) {
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
/// Each client is its own session. `initialize` hands out an `Mcp-Session-Id`
/// and a client that echoes it is told apart from every other on the channel;
/// one that does not shares the channel's single header-less attachment, as
/// every client did before. An id the node no longer holds (a restart, an idle
/// detach, a kill) is not refused: the client simply attaches again under it,
/// so no client has to know to re-initialize.
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
    // falls back to `initialize` when the method is unknown. The probe is not
    // a client of this channel yet, so it is answered without attaching one.
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
    let client = match client_id(&headers) {
        Ok(Some(id)) => Some(id),
        Ok(None) if msg.get("method").and_then(Value::as_str) == Some("initialize") => {
            Some(uuid::Uuid::now_v7().to_string())
        }
        Ok(None) => None,
        Err(e) => return rpc_error(StatusCode::BAD_REQUEST, e).into_response(),
    };
    let lane = lane_label(&headers);
    let mut response = answer_external(&s, channel, client.as_deref(), lane.as_deref(), &msg)
        .await
        .into_response();
    if let Some(value) = client.and_then(|id| HeaderValue::from_str(&id).ok()) {
        response.headers_mut().insert(SESSION_HEADER, value);
    }
    response
}

/// The header MCP's streamable HTTP transport names a session with.
const SESSION_HEADER: &str = "mcp-session-id";

/// The header a harness labels its calls with, so the operator can tell its
/// agents apart. A label only: nothing is authorized, owned or routed by it.
const LANE_HEADER: &str = "x-tracon-agent";

/// The label a harness sent, trimmed and bounded so a stored column is not
/// whatever a caller sends. Absent, empty or not text is no label.
fn lane_label(headers: &HeaderMap) -> Option<String> {
    let label = headers.get(LANE_HEADER)?.to_str().ok()?.trim();
    (!label.is_empty()).then(|| label.chars().take(200).collect())
}

/// The id a client echoes, if any. The transport allows visible ASCII; the
/// length bound keeps a stored column from being whatever a caller sends.
fn client_id(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let Some(value) = headers.get(SESSION_HEADER) else {
        return Ok(None);
    };
    let id = value
        .to_str()
        .ok()
        .filter(|id| {
            !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
        })
        .ok_or("Mcp-Session-Id must be 1 to 128 visible ASCII characters")?;
    Ok(Some(id.to_string()))
}

async fn answer_external(
    s: &AppState,
    channel: String,
    client: Option<&str>,
    lane: Option<&str>,
    msg: &Value,
) -> (StatusCode, Json<Value>) {
    let session_id = match s.manager.attach_external(&channel, client).await {
        Ok(id) => id,
        Err(e) => return rpc_error(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    };
    let row = s.manager.store().get_session(&session_id).ok().flatten();
    if let Some(lane) = lane {
        if row
            .as_ref()
            .is_some_and(|r| r.harness_agent.as_deref() != Some(lane))
        {
            let _ = s.manager.store().update_session(
                &session_id,
                crate::store::SessionPatch {
                    harness_agent: Some(lane.to_string()),
                    ..Default::default()
                },
            );
        }
    }
    if row.is_some_and(|row| row.state == crate::session::state::SessionState::Paused.as_str()) {
        return rpc_error(
            StatusCode::CONFLICT,
            "this external harness is still running; Tracon paused only its broker access and cannot control the host process",
        );
    }
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
            json!({ "title": title, "kind": crate::mcp::TOOL_KIND, "lane": lane }),
        );
    }
    if let Err(error) = s.manager.ensure_active(&session_id) {
        return rpc_error(StatusCode::CONFLICT, &error.to_string());
    }

    let mut unfinished = call.as_ref().map(|title| Abandoned {
        manager: s.manager.clone(),
        session_id: session_id.clone(),
        title: title.clone(),
        finished: false,
    });
    let answered = s.tools.handle(&ctx, msg).await;
    if let Some(guard) = unfinished.as_mut() {
        guard.finished = true;
    }
    match answered {
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

/// A call the harness hung up on. The server drops the handler when the
/// client goes away, so without this the log shows a call and never its end,
/// and nothing says whether it ran.
struct Abandoned {
    manager: crate::session::Manager,
    session_id: String,
    title: String,
    finished: bool,
}

impl Drop for Abandoned {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.manager.record_event(
            &self.session_id,
            crate::session::state::event_kind::TOOL_RESULT,
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

/// Some clients close a session on exit. Honour it: that client's attachment
/// ends and the home stops showing it as connected. Any other client on the
/// channel is untouched.
pub async fn external_delete(
    State(s): State<AppState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
) -> StatusCode {
    let Ok(client) = client_id(&headers) else {
        return StatusCode::BAD_REQUEST;
    };
    let _ = s.manager.detach_external(&channel, client.as_deref()).await;
    StatusCode::NO_CONTENT
}

fn rpc_error(code: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({ "jsonrpc": "2.0", "error": { "code": -32001, "message": message } })),
    )
}
