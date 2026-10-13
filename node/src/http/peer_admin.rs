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
    response::{IntoResponse, Response},
};
use proto::admin::AdminStatement;

use super::api::{ApiError, AppState};
use super::passkey::{self, clean_text, Caller, Refusal};
use crate::store::now_ms;

/// The longest line a confirmation prompt shows, which the peer's name and
/// what is done there must fit together: the statement carries exactly the
/// line the operator read, never a shortened one.
const MAX_SUMMARY: usize = 160;

/// A refusal already shaped as the response to send. Boxed: a response is
/// large, and most calls succeed.
pub struct Refused(Box<Response>);

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<Response> for Refused {
    fn from(response: Response) -> Self {
        Refused(Box::new(response))
    }
}

/// Any refusal, as the response it is.
pub(crate) fn refused(refusal: impl IntoResponse) -> Refused {
    Refused(Box::new(refusal.into_response()))
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

/// The line a prompt shows for `line` done on `node_id`: `On <peer>: line`.
/// Refused when it does not fit a prompt whole.
pub(crate) fn summary_for(s: &AppState, node_id: &str, line: &str) -> Result<String, Refusal> {
    let summary = clean_text(
        &format!("On {}: {line}", peer_label(s, node_id)),
        usize::MAX,
    );
    if summary.chars().count() > MAX_SUMMARY {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "what this changes on the peer does not fit one confirmation \
                 ({} characters, at most {MAX_SUMMARY}); split it into smaller edits",
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
