//! Operator names: list, set, revert and resolve. Every route here is behind
//! the operator guard, like the rest of the API; no agent tool writes a name.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::api::{ApiError, AppState};
use crate::names::{self, RenameError, Resolved};

fn error_body(code: StatusCode, message: &str) -> serde_json::Value {
    json!({ "error": { "code": code.as_u16(), "message": message } })
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub channel: Option<String>,
}

/// `GET /api/names?channel=`: every operator name, on one channel or on all.
/// The interface loads them once and follows `name` frames after.
pub async fn list(State(s): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    match s.store().names_list(q.channel.as_deref()) {
        Ok(rows) => {
            let names: Vec<_> = rows
                .into_iter()
                .map(|n| {
                    json!({
                        "channel": n.channel,
                        "generated": names::generated(&n.kind, &n.id),
                        "kind": n.kind,
                        "id": n.id,
                        "name": n.name,
                    })
                })
                .collect();
            Json(json!({ "names": names })).into_response()
        }
        Err(e) => ApiError::from(e).into_response(),
    }
}

#[derive(Deserialize)]
pub struct NameBody {
    #[serde(default)]
    pub name: Option<String>,
    /// For a launch manifest no session has launched with yet: the channel
    /// whose manifest it is.
    #[serde(default)]
    pub channel: Option<String>,
}

fn rename_response(r: Result<names::Renamed, RenameError>) -> Response {
    match r {
        Ok(done) => Json(json!(done)).into_response(),
        Err(e @ RenameError::NotRenamable(_)) | Err(e @ RenameError::Invalid(_)) => {
            let code = StatusCode::UNPROCESSABLE_ENTITY;
            (code, Json(error_body(code, &e.to_string()))).into_response()
        }
        Err(e @ RenameError::Missing { .. }) => {
            let code = StatusCode::NOT_FOUND;
            (code, Json(error_body(code, &e.to_string()))).into_response()
        }
        Err(RenameError::Clash(holder)) => {
            let code = StatusCode::CONFLICT;
            let message = format!(
                "{} is already the name of {} on {}",
                holder.name,
                names::generated(&holder.kind, &holder.id),
                holder.channel
            );
            let mut body = error_body(code, &message);
            body["holder"] = json!({
                "kind": holder.kind,
                "id": holder.id,
                "name": holder.name,
                "channel": holder.channel,
            });
            (code, Json(body)).into_response()
        }
        Err(RenameError::Store(e)) => ApiError::from(e).into_response(),
    }
}

/// `PUT /api/names/{kind}/{id}` with `{name}`: name a record, this node's or
/// a peer's mirrored one. 422 for an invalid name or a kind that is not
/// renamed this way, 409 naming the holder when another record of the kind
/// already has the name on the channel. An empty name reverts.
pub async fn put(
    State(s): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
    Json(body): Json<NameBody>,
) -> Response {
    rename_response(names::rename(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &kind,
        &id,
        body.name.as_deref(),
        body.channel.as_deref(),
    ))
}

/// `DELETE /api/names/{kind}/{id}`: back to the generated name, as a
/// tombstone that replicates like the rename did.
pub async fn delete(
    State(s): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> Response {
    rename_response(names::rename(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &kind,
        &id,
        None,
        q.channel.as_deref(),
    ))
}

#[derive(Deserialize, Default)]
pub struct ResolveQuery {
    /// Any kind when left out.
    pub kind: Option<String>,
    pub q: String,
    pub channel: Option<String>,
}

/// `GET /api/resolve?kind=&q=&channel=`: what a name, a generated name, a
/// full id or an id prefix of six or more characters names. 404 when
/// nothing does; 409 with `candidates` when several do.
pub async fn resolve(State(s): State<AppState>, Query(q): Query<ResolveQuery>) -> Response {
    let kind = q.kind.as_deref().filter(|k| !k.is_empty());
    match names::resolve(s.store(), kind, &q.q, q.channel.as_deref()) {
        Ok(Resolved::One(hit)) => Json(json!(hit)).into_response(),
        Ok(Resolved::None) => {
            let code = StatusCode::NOT_FOUND;
            let what = kind.unwrap_or("record");
            (
                code,
                Json(error_body(
                    code,
                    &format!("no {what} is named {}", q.q.trim()),
                )),
            )
                .into_response()
        }
        Ok(Resolved::Many(hits)) => {
            let code = StatusCode::CONFLICT;
            let mut body = error_body(code, &names::ambiguous(q.q.trim(), &hits));
            body["candidates"] = json!(hits);
            (code, Json(body)).into_response()
        }
        Err(e) => ApiError::from(e).into_response(),
    }
}
