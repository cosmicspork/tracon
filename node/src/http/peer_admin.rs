//! Confirming an admin action on a peer, here.
//!
//! The operator administers a peer from this node's interface: signs its
//! providers in and out, reads and edits its `node.toml`. The peer takes such
//! a command only from a node its `[mesh] admin_peers` names, and only with a
//! statement that this node's operator confirmed exactly that command on
//! exactly that peer (`mesh::admin`). This is where the statement is made:
//! the node works out the operation and the line the prompt shows from the
//! request it is about to send, confirms it as it would an admin action of
//! its own (a passkey off this machine, nothing at it), and signs what was
//! confirmed with its node key.

use axum::{
    http::{request::Parts, StatusCode},
    response::IntoResponse,
};
use proto::admin::AdminStatement;

use super::api::{ApiError, AppState};
use super::passkey::{self, clean_text, Caller, Refusal, MAX_SUMMARY};
use crate::store::now_ms;

pub(crate) use super::config_api::Refused;

/// Any refusal, as the response it is.
pub(crate) fn refused(refusal: impl IntoResponse) -> Refused {
    refusal.into_response().into()
}

/// The longest a peer's name is shown.
const MAX_NAME: usize = 40;

/// How a peer is named to the operator: its name as it announced it, made
/// fit to show, or its id's start.
pub(crate) fn peer_label(s: &AppState, node_id: &str) -> String {
    let name = s
        .store()
        .get_node(node_id)
        .ok()
        .flatten()
        .map(|row| clean_text(&row.name, MAX_NAME))
        .unwrap_or_default();
    if name.is_empty() {
        crate::mesh::admin::short(node_id).to_string()
    } else {
        name
    }
}

/// What a prompt for an action on `node_id` starts with: `On <peer> (<id>…): `.
/// The name is whatever the peer announced, so the start of its node id,
/// which no peer can choose, comes with it: a peer calling itself another's
/// name is still told apart.
pub(crate) fn prefix(s: &AppState, node_id: &str) -> String {
    let id: String = node_id.chars().take(8).collect();
    format!("On {} ({id}\u{2026}): ", peer_label(s, node_id))
}

/// The line a prompt shows for `line` done on `node_id`: the [`prefix`],
/// then `line`. Refused when the two do not fit one prompt whole: the
/// statement carries exactly the line the operator read, never a shortened
/// one.
pub(crate) fn summary_for(s: &AppState, node_id: &str, line: &str) -> Result<String, Refusal> {
    let summary = clean_text(&format!("{}{line}", prefix(s, node_id)), usize::MAX);
    if summary.chars().count() > MAX_SUMMARY {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "what this does on the peer does not fit one confirmation \
                 ({} characters, at most {MAX_SUMMARY}); split it",
                summary.chars().count()
            ),
        )));
    }
    Ok(summary)
}

/// Confirm the operation `op_hash` on peer `node_id` for this caller, the
/// prompt showing `summary` (from [`summary_for`]), and sign the statement
/// the peer requires. Local passes without a prompt, and is still signed:
/// the peer requires a statement whoever confirmed it.
pub(crate) fn confirm(
    s: &AppState,
    parts: &Parts,
    node_id: &str,
    op_hash: &str,
    summary: &str,
) -> Result<AdminStatement, Refusal> {
    let mesh = s.mesh.as_ref().ok_or(Refusal::Api(ApiError::new(
        StatusCode::CONFLICT,
        "no hub configured; this node cannot reach peers",
    )))?;
    let action = format!("{} {}", parts.method, parts.uri.path());
    passkey::authorize_op(
        s,
        parts,
        || op_hash.to_string(),
        &action,
        Some(summary),
        true,
    )?;
    let session = match passkey::caller(s, parts) {
        Caller::Local => "local".to_string(),
        Caller::Session(hash) => hash.chars().take(12).collect(),
        // `authorize_op` refuses any other caller before this, and a peer's
        // request never administers a third node through this one.
        Caller::Other | Caller::Peer(_) => {
            return Err(Refusal::unavailable(
                "no_session",
                "an admin action on a peer needs a signed-in browser or the node itself",
            ))
        }
    };
    Ok(AdminStatement::sign(
        mesh.identity(),
        node_id,
        op_hash,
        summary,
        &session,
        now_ms(),
    ))
}

/// The most a peer admin request's body may be.
const MAX_REQUEST_BODY: usize = 256 * 1024;

/// A: `/api/nodes/{id}/admin/{*rest}`, any method: run `/api/{rest}` on peer
/// `id` as its own interface would, for an operator administering it from
/// here. Only the routes the peer lets an admin peer reach
/// (`peer_routes`) are sent. A read needs no confirmation; anything else is
/// confirmed here with a prompt naming the peer and what the request does
/// there, and signed for exactly this request on exactly that peer, which
/// checks it again. The peer's answer comes back as its own route gave it;
/// a refusal is relayed as this node's, never as a challenge to answer here.
pub(crate) async fn relay(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path((node_id, rest)): axum::extract::Path<(String, String)>,
    req: axum::extract::Request,
) -> axum::response::Response {
    match relay_here(&s, &node_id, &rest, req).await {
        Ok(response) => response,
        Err(refused) => refused.into_response(),
    }
}

async fn relay_here(
    s: &AppState,
    node_id: &str,
    rest: &str,
    req: axum::extract::Request,
) -> Result<axum::response::Response, Refused> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_REQUEST_BODY)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"))?;
    passkey::json_or_empty(&parts.headers, &bytes)?;
    if node_id == s.node_id {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "that is this node: use its own route",
        )
        .into());
    }
    if parts.uri.query().is_some() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "an admin request to a peer carries no query",
        )
        .into());
    }
    let body: Option<serde_json::Value> = match bytes.is_empty() {
        true => None,
        false => Some(serde_json::from_slice(&bytes).map_err(|e| {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("bad body: {e}"))
        })?),
    };
    let method = parts.method.as_str().to_string();
    let path = format!("/api/{rest}");
    let line = super::peer_routes::describe(
        &method,
        &path,
        body.as_ref().unwrap_or(&serde_json::Value::Null),
    )
    .map_err(|why| ApiError::new(StatusCode::FORBIDDEN, why))?;
    let mut command = proto::frame::Command::AdminRequest {
        method,
        path,
        body,
        statement: None,
    };
    if let Some(line) = line {
        let summary = summary_for(s, node_id, &line)?;
        let op_hash = crate::mesh::admin::op_hash(node_id, &command);
        let signed = confirm(s, &parts, node_id, &op_hash, &summary)?;
        if let proto::frame::Command::AdminRequest { statement, .. } = &mut command {
            *statement = Some(Box::new(signed));
        }
    }
    let (status, body) = super::config_api::on_peer_status(s, node_id, command).await?;
    Ok((status, axum::Json(body)).into_response())
}

/// B: an admin peer's `admin_request`, run through this node's own route for
/// it. `admitted` is the statement `mesh::admin::admit` checked and spent;
/// only a read runs without one. The route sees a [`passkey::PeerAdmin`]
/// caller, which stands for the operator here only when confirmed. A write
/// that went through is logged and pushed to the operator, naming the peer.
pub(crate) async fn run_for_peer(
    s: &AppState,
    sender: &str,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    admitted: Option<&crate::mesh::admin::Admitted>,
) -> Result<serde_json::Value, ApiError> {
    use tower::ServiceExt;
    let line = super::peer_routes::describe(
        method,
        path,
        body.as_ref().unwrap_or(&serde_json::Value::Null),
    )
    .map_err(|why| ApiError::new(StatusCode::FORBIDDEN, why))?;
    let peer = match (&line, admitted) {
        (None, _) => passkey::PeerAdmin {
            actor: format!("peer {} (read)", crate::mesh::admin::short(sender)),
            confirmed: false,
        },
        (Some(_), Some(admitted)) => passkey::PeerAdmin {
            actor: admitted.actor(),
            confirmed: true,
        },
        (Some(_), None) => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "an admin action from a peer must carry its operator's statement",
            ))
        }
    };
    let method_ = axum::http::Method::from_bytes(method.as_bytes())
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad method"))?;
    let mut builder = axum::http::Request::builder().method(method_).uri(path);
    let bytes = match &body {
        Some(body) => {
            builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
            body.to_string()
        }
        None => String::new(),
    };
    let mut req = builder
        .body(axum::body::Body::from(bytes))
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
    let actor = peer.actor.clone();
    req.extensions_mut().insert(peer);
    let res = super::router(s.clone())
        .oneshot(req)
        .await
        .unwrap_or_else(|never| match never {});
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 4 << 20)
        .await
        .unwrap_or_default();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
    });
    let mut answer = serde_json::json!({ "status": status.as_u16(), "body": body });
    if let (Some(line), true) = (&line, status.is_success()) {
        tracing::info!(%actor, action = %line, "admin action run for a peer");
        let (store, cfg) = (s.store().clone(), s.cfg.clone());
        let (title, text) = (
            "Changed from a peer".to_string(),
            format!("{line}, by {actor}"),
        );
        let id = uuid::Uuid::now_v7().to_string();
        // What ran and for whom, beside the route's answer: the sender
        // relays only the body.
        answer["audit"] = serde_json::json!({ "actor": actor, "action": line, "push": id });
        tokio::spawn(async move {
            crate::notify::send_operator(&store, &cfg, &id, title, text, "/settings".into(), &[])
                .await;
        });
    }
    Ok(answer)
}
