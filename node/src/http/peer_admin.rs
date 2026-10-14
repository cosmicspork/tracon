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
        // `authorize_op` refuses any other caller before this.
        Caller::Other => {
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
