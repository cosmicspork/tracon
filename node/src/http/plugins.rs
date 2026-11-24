//! `/api/plugins`: what `tracon plugin` drives (`crate::plugin`).
//!
//! Adding or updating a plugin only stages a checked build and raises its
//! install card; the operator's answer to that card is what installs it,
//! and allowing it is an admin action (`api::admin_answer`). Rolling back
//! and removing change what the node holds without a card, so each is an
//! admin action itself (`passkey::AdminAction`). Each of those stops the
//! plugin's running instances (`plugin::supervisor`); the next use starts
//! what is active then. Restarting an instance kills a process and starts
//! one, so it is an admin action too.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, ApiResult, AppState, DEFAULT_CHANNELS};
use super::auth::{AdminAction, Local};
use super::plugin_ui::{self, UiBinding};
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
        view["ui_title"] = json!(b.manifest.ui.as_ref().map(|u| &u.title));
        view["icon"] = json!(build_icon(&row.name, b).is_some());
        view
    });
    let instances: Vec<_> = s
        .manager
        .plugins()
        .status()
        .into_iter()
        .filter(|i| i.plugin == row.name)
        .collect();
    let live = s.cfg.live();
    let entry = live.plugin.iter().find(|p| p.name == row.name);
    let offered_on: Option<Vec<String>> = row.active.as_ref().map(|build| {
        node_channels(s)
            .into_iter()
            .filter(|channel| unoffered(s, &row.name, build, channel).is_none())
            .collect()
    });
    json!({
        "name": row.name,
        "repo": row.repo,
        "state": row.state,
        "channels": entry.and_then(|e| e.channels.as_ref()),
        "offered_on": offered_on,
        "active": active,
        "previous": row.previous.as_ref().map(build_view),
        "pending": pending,
        "updated_ms": row.updated_ms,
        "instances": instances,
        "tools": row
            .active
            .as_ref()
            .map(|b| s.tools.plugin_tool_status(&b.manifest))
            .unwrap_or_default(),
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
    if !channel_known(s, &channel)? {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("no channel {channel} on this node"),
        ));
    }
    Ok(channel)
}

/// Why `build` of plugin `name` is not offered on `channel`
/// ([`plugins::unoffered`]); `None` when it is.
fn unoffered(s: &AppState, name: &str, build: &Build, channel: &str) -> Option<String> {
    let live = s.cfg.live();
    let entry = live.plugin.iter().find(|p| p.name == name);
    let broker = s.tools.broker.read().unwrap();
    plugins::unoffered(
        &build.manifest,
        entry,
        channel,
        &broker.available_to(channel, &s.node_id),
    )
}

/// The channels this node has: those it holds, or, holding none yet, the
/// channels a fresh node starts with.
fn node_channels(s: &AppState) -> Vec<String> {
    match s.store().channel_list() {
        Ok(known) if !known.is_empty() => known.into_iter().map(|c| c.name).collect(),
        _ => DEFAULT_CHANNELS.iter().map(|c| c.to_string()).collect(),
    }
}

/// Whether the node has `channel`: one it holds, or, holding none yet, one
/// of the channels a fresh node starts with.
fn channel_known(s: &AppState, channel: &str) -> ApiResult<bool> {
    let known = s.store().channel_list()?;
    Ok(match known.is_empty() {
        true => DEFAULT_CHANNELS.contains(&channel),
        false => known.iter().any(|c| c.name == channel),
    })
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
        let store = s.store().clone();
        tokio::task::spawn_blocking(move || plugins::discard(&store, &Config::state_dir(), staged))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
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
    let name_ = name.clone();
    let (now, was) = tokio::task::spawn_blocking(move || {
        plugins::rollback(&store, &Config::state_dir(), &name_)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    s.manager.plugins().stop_plugin(&name).await;
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
    // Stopped before its builds (and, purged, its data) go, and again after
    // in case a use started it in between.
    s.manager.plugins().stop_plugin(&name).await;
    let expired = tokio::task::spawn_blocking(move || {
        plugins::remove(&store, &Config::state_dir(), &removed, query.purge)
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    s.manager.plugins().stop_plugin(&name).await;
    s.manager.plugin_uis().stop(&name);
    if let Some(id) = expired {
        if let Some(row) = s.store().get_approval(&id)? {
            s.manager.approval_decided(&row, EXPIRED).await;
        }
    }
    Ok(Json(json!({ "removed": name, "purged": query.purge })))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    channel: String,
    #[serde(default)]
    lines: Option<usize>,
}

/// `GET /api/plugins/{name}/logs?channel=&lines=`: the last lines an
/// instance wrote, and what the node said of it.
pub async fn logs(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> ApiResult<Json<Value>> {
    let lines = query.lines.unwrap_or(200).min(1000);
    Ok(Json(json!({
        "lines": s.manager.plugins().logs(&name, &query.channel, lines),
    })))
}

/// The icon of `name`'s build `build`, when its page has one the node will
/// serve (`plugin::icon`).
fn build_icon(
    name: &str,
    build: &Build,
) -> Option<(std::path::PathBuf, crate::plugin::icon::Icon)> {
    let ui = build.manifest.ui.as_ref()?;
    let root = install::build_dir(&Config::state_dir(), name, &build.version, &build.digest);
    let icon = crate::plugin::icon::locate(&root, ui)?;
    Some((root, icon))
}

/// `GET /api/plugins/{name}/icon`: the active build's page icon, from the
/// interface's own origin so the navigation can show it with `<img>`.
/// Sandboxed and never sniffed, since an SVG is a document; its ETag is the
/// build's digest, so a new build is fetched and an unchanged one is not.
pub async fn icon(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse as _;
    let active = s
        .store()
        .plugin_get(&name)
        .ok()
        .flatten()
        .filter(|r| r.state != crate::store::plugins::REMOVED)
        .and_then(|r| r.active);
    let Some(build) = active else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some((root, icon)) = build_icon(&name, &build) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", build.digest);
    let common = |b: axum::http::response::Builder| {
        b.header(header::CONTENT_TYPE, icon.media)
            .header(header::CONTENT_SECURITY_POLICY, "sandbox")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(header::ETAG, etag.as_str())
            .header(header::CACHE_CONTROL, "private, no-cache")
    };
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return common(axum::http::Response::builder())
            .status(StatusCode::NOT_MODIFIED)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response());
    }
    let Ok(bytes) = tokio::fs::read(root.join(&icon.path)).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if bytes.len() as u64 > crate::plugin::icon::MAX_ICON_BYTES {
        return StatusCode::NOT_FOUND.into_response();
    }
    common(axum::http::Response::builder())
        .status(StatusCode::OK)
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response())
}

#[derive(Deserialize)]
pub struct RestartBody {
    channel: String,
}

/// `POST /api/plugins/{name}/restart`: stop the instance on a channel and
/// start it afresh, clearing its backoff, or start it on a channel it has
/// not run on yet; answered once it is ready or has failed.
pub async fn restart(
    State(s): State<AppState>,
    Path(name): Path<String>,
    AdminAction(Json(body)): AdminAction<Json<RestartBody>>,
) -> ApiResult<Json<Value>> {
    let supervisor = s.manager.plugins();
    let channel = body.channel;
    let instance = |supervisor: &crate::plugin::supervisor::Supervisor| {
        supervisor
            .status()
            .into_iter()
            .find(|i| i.plugin == name && i.channel == channel)
    };
    let outcome = if instance(supervisor).is_some() {
        supervisor.restart(&name, &channel).await
    } else {
        // Not run here yet. Starting it is how an operator tries a plugin
        // before a session uses it, so only an installed plugin on a
        // channel the node has.
        let installed = s
            .store()
            .plugin_get(&name)?
            .is_some_and(|row| row.active.is_some() && row.state != crate::store::plugins::REMOVED);
        if !installed {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("plugin {name} is not installed"),
            ));
        }
        if !channel_known(&s, &channel)? {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("no channel {channel} on this node"),
            ));
        }
        let build = s.store().plugin_get(&name)?.and_then(|row| row.active);
        if let Some(why) = build.and_then(|b| unoffered(&s, &name, &b, &channel)) {
            return Err(ApiError::new(StatusCode::FORBIDDEN, why));
        }
        supervisor.ensure(&name, &channel).await
    };
    let Some(status) = instance(supervisor) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("plugin {name} has no instance on {channel}"),
        ));
    };
    Ok(Json(match outcome {
        Ok(_) => json!({ "instance": status }),
        Err(error) => json!({ "instance": status, "error": error }),
    }))
}

#[derive(Deserialize, Default)]
pub struct UiBody {
    #[serde(default)]
    channel: Option<String>,
}

/// `POST /api/plugins/{name}/ui` with `{channel}`: open the plugin's UI for
/// that channel's instance (`[session] default_channel` when none is named).
/// Its listener is started on the port the plugin keeps, if it is not
/// serving already, and a capability token is minted for the frame:
/// `{url, origin, title, channel, expires_ms}`. Served on loopback only, so
/// a caller on another machine is refused rather than handed a URL it
/// cannot open.
pub async fn ui(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Local(local): Local,
    body: Option<Json<UiBody>>,
) -> ApiResult<Json<Value>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    if !local {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plugin UIs are served on the node's loopback only: open them on the node's own machine",
        ));
    }
    let channel = card_channel(&s, body.channel.as_deref())?;
    let build = s
        .store()
        .plugin_get(&name)?
        .filter(|r| r.state != crate::store::plugins::REMOVED)
        .and_then(|r| r.active)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("plugin {name} is not installed on this node"),
            )
        })?;
    let Some(ui) = build.manifest.ui.clone() else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("plugin {name} has no UI"),
        ));
    };
    if let Some(why) = unoffered(&s, &name, &build, &channel) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, why));
    }
    let live = s.cfg.live();
    let ports = live.plugins.ui_ports();
    let Some(port) = s.store().plugin_ui_port(&name, ports.clone())? else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "every port in plugins.ui_port_first..ui_port_last ({}..{}) is another plugin's; widen the range in node.toml",
                ports.start(),
                ports.end()
            ),
        ));
    };
    s.manager
        .plugin_uis()
        .listen(&name, port, || plugin_ui::router(s.clone(), &name, port))
        .map_err(|e| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("plugin {name}'s UI cannot listen on 127.0.0.1:{port}: {e}"),
            )
        })?;
    let (token, expires_ms) = s.manager.plugin_uis().mint(UiBinding {
        plugin: name.clone(),
        channel: channel.clone(),
    });
    let origin = format!("http://127.0.0.1:{port}");
    let entry = ui.entry.trim_start_matches("ui/");
    Ok(Json(json!({
        "url": format!("{origin}/t/{token}/{entry}"),
        "origin": origin,
        "title": ui.title,
        "channel": channel,
        "expires_ms": expires_ms,
    })))
}
