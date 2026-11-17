//! `/api/plugins`: what `tracon plugin` drives (`crate::plugin`).
//!
//! Adding or updating a plugin only stages a checked build and raises its
//! install card; the operator's answer to that card is what installs it,
//! and allowing it is an admin action (`api::admin_answer`). Rolling back
//! and removing change what the node holds without a card, so each is an
//! admin action itself (`passkey::AdminAction`).

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, ApiResult, AppState, DEFAULT_CHANNELS};
use super::auth::AdminAction;
use crate::config::Config;
use crate::plugin::install::{self, InstallError, Releases, Staged};
use crate::plugin::{self as plugins, Ask, Offered};
use crate::store::approvals::EXPIRED;
use crate::store::plugins::{Build, PluginRow};

impl From<InstallError> for ApiError {
    fn from(error: InstallError) -> Self {
        let code = match &error {
            InstallError::Refused(_) => StatusCode::UNPROCESSABLE_ENTITY,
            InstallError::Fetch(_) => StatusCode::BAD_GATEWAY,
            InstallError::Io(_) | InstallError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(code, error.to_string())
    }
}

fn build_view(build: &Build) -> Value {
    json!({ "version": build.version, "digest": build.digest })
}

fn row_view(s: &AppState, row: &PluginRow) -> Value {
    let pending = row.pending.as_ref().map(|p| {
        let card = s.store().get_approval(&p.approval_id).ok().flatten();
        let mut view = build_view(&p.build);
        view["approval_id"] = json!(p.approval_id);
        view["approval_state"] = json!(card.map(|c| match c.state.as_str() {
            crate::store::approvals::PENDING if c.expires_ms <= crate::store::now_ms() => {
                EXPIRED.to_string()
            }
            _ => c.state,
        }));
        view
    });
    let active = row.active.as_ref().map(|b| {
        let mut view = build_view(b);
        view["tools"] = json!(b
            .manifest
            .tools
            .iter()
            .map(|t| format!("{}__{}", b.manifest.name, t.name))
            .collect::<Vec<_>>());
        view["formats"] = json!(b.manifest.formats);
        view["ui"] = json!(b.manifest.ui.is_some());
        view
    });
    json!({
        "name": row.name,
        "repo": row.repo,
        "state": row.state,
        "active": active,
        "previous": row.previous.as_ref().map(build_view),
        "pending": pending,
        "updated_ms": row.updated_ms,
    })
}

/// `GET /api/plugins`
pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let rows = s.store().plugins_list()?;
    Ok(Json(json!({
        "plugins": rows.iter().map(|r| row_view(&s, r)).collect::<Vec<_>>(),
    })))
}

/// The channel an install card goes to: the one named, or
/// `[session] default_channel`, and one this node knows.
fn card_channel(s: &AppState, named: Option<&str>) -> ApiResult<String> {
    let live = s.cfg.live();
    let channel = named
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(live.session.default_channel.as_str())
        .to_string();
    if channel.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "name a channel for the install card (--channel), or set [session] default_channel",
        ));
    }
    let known = s.store().channel_list()?;
    let exists = match known.is_empty() {
        true => DEFAULT_CHANNELS.contains(&channel.as_str()),
        false => known.iter().any(|c| c.name == channel),
    };
    if !exists {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("no channel {channel} on this node"),
        ));
    }
    Ok(channel)
}

/// Put a staged build to the operator, and say what came of it.
async fn offer(s: &AppState, channel: &str, staged: Staged) -> ApiResult<Json<Value>> {
    let store = s.store().clone();
    let ask_channel = channel.to_string();
    let node_id = s.node_id.clone();
    let live = s.cfg.live();
    let expiry_secs = live.session.approval_expiry_secs;
    let mcp_servers = live.mcp_server.iter().map(|m| m.name.clone()).collect();
    let offered = tokio::task::spawn_blocking(move || {
        let ask = Ask {
            node_id: &node_id,
            channel: &ask_channel,
            expiry_secs,
            mcp_servers,
        };
        plugins::offer(&store, &Config::state_dir(), &ask, staged)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    match offered {
        Offered::UpToDate(build) => Ok(Json(json!({
            "up_to_date": true,
            "version": build.version,
            "digest": build.digest,
        }))),
        Offered::Asked {
            card,
            new,
            superseded,
        } => {
            if let Some(id) = superseded {
                if let Some(row) = s.store().get_approval(&id)? {
                    s.manager.approval_decided(&row, EXPIRED).await;
                }
            }
            if new {
                s.manager.approval_requested(&card).await;
            }
            let args: Value = serde_json::from_str(&card.arguments).unwrap_or_default();
            Ok(Json(json!({
                "name": args["name"],
                "version": args["version"],
                "from_version": args["from_version"],
                "digest": args["digest"],
                "approval_id": card.id,
                "channel": card.channel,
                "new": new,
            })))
        }
    }
}

#[derive(Deserialize)]
pub struct AddBody {
    /// `owner/repo`.
    repo: String,
    #[serde(default)]
    channel: Option<String>,
}

/// `POST /api/plugins`: fetch the latest release of `repo`, check and stage
/// it, and raise its install card.
pub async fn add(State(s): State<AppState>, Json(body): Json<AddBody>) -> ApiResult<Json<Value>> {
    let channel = card_channel(&s, body.channel.as_deref())?;
    let owners = s.cfg.live().plugins.owners.clone();
    let staged = install::fetch(
        &Releases::github()?,
        &Config::state_dir(),
        &owners,
        body.repo.trim(),
    )
    .await?;
    offer(&s, &channel, staged).await
}

#[derive(Deserialize)]
pub struct ArchiveQuery {
    /// The archive's file name, which must be the one a release would
    /// publish it as.
    name: String,
    #[serde(default)]
    channel: Option<String>,
}

/// `POST /api/plugins/archive?name=&channel=`: an archive the operator
/// hands the node, its bytes the body.
pub async fn add_archive(
    State(s): State<AppState>,
    Query(query): Query<ArchiveQuery>,
    bytes: Bytes,
) -> ApiResult<Json<Value>> {
    let channel = card_channel(&s, query.channel.as_deref())?;
    let staged = tokio::task::spawn_blocking(move || {
        install::stage_local(&Config::state_dir(), &bytes, &query.name)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    offer(&s, &channel, staged).await
}

#[derive(Deserialize, Default)]
pub struct UpdateBody {
    #[serde(default)]
    channel: Option<String>,
}

/// `POST /api/plugins/{name}/update`: fetch the latest release from where
/// `name` was installed from, and raise a card if it is not the active
/// build already.
pub async fn update(
    State(s): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<UpdateBody>>,
) -> ApiResult<Json<Value>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let row = s
        .store()
        .plugin_get(&name)?
        .filter(|r| r.state != crate::store::plugins::REMOVED)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("no plugin {name} is installed"),
            )
        })?;
    if row.repo.starts_with("local:") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{name} was installed from a local archive; add the new archive again to update it"
            ),
        ));
    }
    let channel = card_channel(&s, body.channel.as_deref())?;
    let owners = s.cfg.live().plugins.owners.clone();
    let staged = install::fetch(
        &Releases::github()?,
        &Config::state_dir(),
        &owners,
        &row.repo,
    )
    .await?;
    if staged.build.manifest.name != name {
        // What was fetched was checked and staged under its own name; it is
        // not this plugin's update. Leave nothing of it behind.
        let other = staged.build.manifest.name.clone();
        let held = s.store().plugin_get(&other)?;
        let keep: Vec<&Build> = held
            .iter()
            .flat_map(|r| {
                r.active
                    .iter()
                    .chain(r.previous.iter())
                    .chain(r.pending.iter().map(|p| &p.build))
            })
            .collect();
        install::prune(&Config::state_dir(), &other, &keep).map_err(InstallError::from)?;
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{}'s latest release is plugin {other}, not {name}",
                row.repo
            ),
        ));
    }
    offer(&s, &channel, staged).await
}

/// `POST /api/plugins/{name}/rollback`: go back to the build before the
/// active one. It was approved when it was installed, so there is no card.
pub async fn rollback(
    State(s): State<AppState>,
    Path(name): Path<String>,
    AdminAction(()): AdminAction,
) -> ApiResult<Json<Value>> {
    let store = s.store().clone();
    let (now, was) =
        tokio::task::spawn_blocking(move || plugins::rollback(&store, &Config::state_dir(), &name))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    Ok(Json(
        json!({ "active": build_view(&now), "previous": build_view(&was) }),
    ))
}

#[derive(Deserialize, Default)]
pub struct RemoveQuery {
    #[serde(default)]
    purge: bool,
}

/// `DELETE /api/plugins/{name}?purge=`: remove a plugin's builds, keeping
/// its data unless purged.
pub async fn remove(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<RemoveQuery>,
    AdminAction(()): AdminAction,
) -> ApiResult<Json<Value>> {
    let store = s.store().clone();
    let removed = name.clone();
    let expired = tokio::task::spawn_blocking(move || {
        plugins::remove(&store, &Config::state_dir(), &removed, query.purge)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    if let Some(id) = expired {
        if let Some(row) = s.store().get_approval(&id)? {
            s.manager.approval_decided(&row, EXPIRED).await;
        }
    }
    Ok(Json(json!({ "removed": name, "purged": query.purge })))
}
