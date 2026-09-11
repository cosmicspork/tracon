//! The operator API. Small on purpose: the interface reads snapshots and the
//! stream, and writes through a handful of commands.

use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Multipart, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    adapter::HarnessAdapter,
    config::Config,
    session::{Manager, NewSession, Phase, SessionError},
    store::Store,
};

/// The channels a standalone node offers before any has been created. The
/// form shows these, so anything keyed on a channel accepts them too.
pub const DEFAULT_CHANNELS: &[&str] = &["personal", "work"];

#[derive(Clone)]
pub struct AppState {
    pub manager: Manager,
    pub cfg: Arc<Config>,
    pub adapter: Arc<dyn HarnessAdapter>,
    pub node_id: String,
    pub tools: Arc<crate::mcp::Tools>,
    /// The hub client, when a hub is configured.
    pub mesh: Option<Arc<crate::mesh::client::MeshClient>>,
    /// Who the operator API answers to.
    pub auth: Arc<super::auth::AuthState>,
    /// An enrolment this node started from the interface, if any.
    pub enroll: Arc<std::sync::Mutex<EnrollJob>>,
}

impl AppState {
    pub fn store(&self) -> &Arc<Store> {
        self.manager.store()
    }
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn new(code: StatusCode, message: impl Into<String>) -> Self {
        Self(code, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({ "error": { "code": self.0.as_u16(), "message": self.1 } })),
        )
            .into_response()
    }
}

impl From<SessionError> for ApiError {
    fn from(e: SessionError) -> Self {
        let code = match e {
            // A missing model is a malformed request, not a conflict.
            SessionError::ModelRequired
            | SessionError::BadBudget
            | SessionError::UnknownChannel(_)
            | SessionError::WorkItemRequired
            | SessionError::NotReady(_)
            | SessionError::InSession(_)
            | SessionError::PlanRequired(_) => StatusCode::UNPROCESSABLE_ENTITY,
            SessionError::Ceiling(_) => StatusCode::TOO_MANY_REQUESTS,
            SessionError::ChannelArchived(_) => StatusCode::CONFLICT,
            SessionError::NotFound => StatusCode::NOT_FOUND,
            SessionError::PeerUnreachable(_) => StatusCode::GATEWAY_TIMEOUT,
            SessionError::Remote(..) | SessionError::NoMesh => StatusCode::CONFLICT,
            // The node refusing to run harnesses, a version mismatch, or a
            // session that will not take the command are all state conflicts.
            SessionError::NodeRefused(_)
            | SessionError::VersionMismatch { .. }
            | SessionError::Rejected(_) => StatusCode::CONFLICT,
            SessionError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(code, e.to_string())
    }
}

impl From<crate::store::StoreError> for ApiError {
    fn from(e: crate::store::StoreError) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Deserialize)]
pub struct AuthorityGrantBody {
    action: String,
    verdict: String,
    target: String,
    channel: String,
    session_id: Option<String>,
    revision: Option<String>,
    expires_ms: Option<i64>,
    reason: String,
}

/// Local grants are intentionally data-only decisions. The signed policy
/// bundle, signing key, and trust root have no HTTP mutation endpoint.
pub async fn list_authority_grants(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let grants = s
        .store()
        .authority_grants(true)?
        .iter()
        .map(crate::authority::grant_visible)
        .collect::<Vec<_>>();
    let policy = s.tools.policy.read().unwrap();
    Ok(Json(json!({
        "policy": {
            "version": policy.version,
            "rules": policy.rules,
            "trusted": policy.trusted,
        },
        "grants": grants,
    })))
}

pub async fn create_authority_grant(
    State(s): State<AppState>,
    Json(b): Json<AuthorityGrantBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let action = b.action.trim();
    if !crate::authority::valid_action(action) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown authority action",
        ));
    }
    if !matches!(b.verdict.as_str(), "allow" | "ask" | "deny") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "verdict must be allow, ask, or deny",
        ));
    }
    if b.target.trim().is_empty() || b.target.contains(char::is_whitespace) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "target must be a canonical non-empty identifier",
        ));
    }
    if b.channel.trim().is_empty() || b.reason.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "channel and reason are required",
        ));
    }
    if b.verdict == "allow"
        && matches!(
            action,
            crate::authority::MERGE | crate::authority::PUBLISH | crate::authority::DEPLOY
        )
        && b.revision
            .as_deref()
            .is_none_or(|revision| revision.trim().is_empty())
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "merge, publish, and deploy allow grants must bind an immutable revision",
        ));
    }
    if b.expires_ms
        .is_some_and(|expires_ms| expires_ms <= crate::store::now_ms())
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "expiry must be in the future",
        ));
    }
    let row = crate::store::AuthorityGrantRow {
        id: uuid::Uuid::now_v7().to_string(),
        action: action.into(),
        verdict: b.verdict,
        target: b.target,
        channel: b.channel,
        session_id: b.session_id.filter(|v| !v.trim().is_empty()),
        revision: b.revision.filter(|v| !v.trim().is_empty()),
        expires_ms: b.expires_ms,
        revoked_ms: None,
        reason: b.reason,
        created_ms: crate::store::now_ms(),
    };
    s.store().authority_grant_insert(&row)?;
    Ok(Json(crate::authority::grant_visible(&row)))
}

pub async fn revoke_authority_grant(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if !s.store().authority_grant_revoke(&id)? {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no active authority grant",
        ));
    }
    Ok(Json(json!({ "revoked": id })))
}

pub async fn get_node(
    State(s): State<AppState>,
    parts: axum::http::request::Parts,
) -> ApiResult<Json<serde_json::Value>> {
    let mut v = node_json(&s)?;
    // Whether this client may change what the node *is*. The interface asks
    // rather than guesses, and disables those controls with a reason instead
    // of hiding them.
    v["loopback"] = json!(super::auth::extensions_are_loopback(&parts.extensions));
    Ok(Json(v))
}

/// Every node this one knows: itself first, then peers as the mesh reports them.
pub async fn list_nodes(
    State(s): State<AppState>,
    parts: axum::http::request::Parts,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().list_nodes()?;
    let local = super::auth::extensions_are_loopback(&parts.extensions);
    let mut out: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let mut v = node_row_json(r);
            // Only ever on this node's own row: it is a fact about how the
            // client reached *here*, not about a peer. This list is what the
            // interface keeps its node in, so the answer has to live here too.
            if r.is_self == 1 {
                v["loopback"] = json!(local);
                v["default_channel"] = json!(s.cfg.session.default_channel);
            }
            v
        })
        .collect();
    out.sort_by_key(|n| n["is_self"] != true);
    Ok(Json(json!(out)))
}

/// The channels this node holds keys for, and which nodes are bound to each.
/// A standalone node (no hub) reports the two Phase 1 labels so the form has
/// something to offer.
pub async fn list_channels(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let mut out = Vec::new();
    let rows = s.store().channel_list()?;
    if rows.is_empty() {
        for name in DEFAULT_CHANNELS.iter().copied() {
            let ceiling = crate::metrics::ceiling(s.store(), &json!({}), name);
            out.push(
                json!({ "name": name, "nodes": [s.node_id], "bindings": {}, "ceiling": ceiling }),
            );
        }
    }
    for c in rows {
        if c.name.starts_with('@') {
            continue;
        }
        let bindings: serde_json::Value =
            serde_json::from_str(&c.bindings_json).unwrap_or(json!({}));
        let ceiling = crate::metrics::ceiling(s.store(), &bindings, &c.name);
        out.push(json!({
            "name": c.name, "nodes": s.store().nodes_in_channel(&c.name)?,
            // Lifted out of the bindings it lives in, so a screen deciding
            // what to offer does not have to dig for it.
            "archived": bindings.get("archived").cloned(),
            "bindings": bindings, "ceiling": ceiling,
        }));
    }
    Ok(Json(json!(out)))
}

/// `PUT /api/channels/{name}/bindings`: merge keys into the channel's
/// bindings (a null value removes a key) and, on a mesh, hand the channel
/// again to every member so they hold the same bindings.
pub async fn put_channel_bindings(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(patch): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = s.store().channel_get(&name)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        format!("no channel {name} on this node"),
    ))?;
    let mut bindings: serde_json::Value =
        serde_json::from_str(&row.bindings_json).unwrap_or(json!({}));
    let Some(obj) = patch.as_object() else {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "bindings must be an object".into(),
        ));
    };
    for (k, v) in obj {
        merge_path(&mut bindings, k, v.clone());
    }
    s.store()
        .channel_put(&name, &row.keyring, &bindings.to_string())?;
    let mut handed = 0;
    if let Some(hub) = s.cfg.mesh.hub_url.as_deref() {
        match crate::mesh::identity::load_or_generate() {
            Ok((identity, _)) => {
                match crate::mesh::enroll::rehand_channel(s.store(), &identity, hub, &name).await {
                    Ok(n) => handed = n,
                    Err(e) => tracing::warn!(error = %e, channel = %name, "bindings not re-handed"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "no identity to re-hand bindings with"),
        }
    }
    Ok(Json(
        json!({ "name": name, "bindings": bindings, "handed_to": handed }),
    ))
}

/// `DELETE /api/channels/{name}`: forget an archived channel on this node.
/// Its key and membership rows go; sessions, work and documents keep their
/// channel column as history. Archiving first is required, so a channel
/// still taking sessions cannot vanish on a misclick, and a session that has
/// not ended keeps it here. The hub's member record still names the channel
/// (its admit call merges and never removes), which only matters if the same
/// name is created again.
pub async fn delete_channel(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if name.starts_with('@') {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no channel {name} on this node"),
        ));
    }
    let row = s.store().channel_get(&name)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        format!("no channel {name} on this node"),
    ))?;
    let bindings: serde_json::Value = serde_json::from_str(&row.bindings_json).unwrap_or(json!({}));
    if bindings
        .get("archived")
        .is_none_or(serde_json::Value::is_null)
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("archive {name} before deleting it"),
        ));
    }
    let open = s.store().open_sessions_on_channel(&name)?;
    if open > 0 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "{open} session{} on {name} {} not ended",
                if open == 1 { "" } else { "s" },
                if open == 1 { "has" } else { "have" }
            ),
        ));
    }
    s.store().channel_delete(&name)?;
    Ok(Json(json!({ "deleted": name })))
}

/// `a.b.c = v` into nested objects; a null removes the key.
fn merge_path(root: &mut serde_json::Value, path: &str, value: serde_json::Value) {
    let mut cur = root;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        if !cur.is_object() {
            *cur = json!({});
        }
        let map = cur.as_object_mut().unwrap();
        if i == parts.len() - 1 {
            if value.is_null() {
                map.remove(*part);
            } else {
                map.insert(part.to_string(), value);
            }
            return;
        }
        cur = map.entry(part.to_string()).or_insert_with(|| json!({}));
    }
}

/// `GET /api/metrics?channel=&since_ms=`: per channel, the numbers that
/// matter. Default window: the last 30 days.
pub async fn metrics(
    State(s): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let since = q
        .get("since_ms")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| crate::store::now_ms() - 30 * 24 * 3600 * 1000);
    let channels: Vec<String> = match q.get("channel") {
        Some(c) => vec![c.clone()],
        None => {
            let mut names: Vec<String> = s
                .store()
                .channel_list()?
                .into_iter()
                .map(|c| c.name)
                .filter(|n| !n.starts_with('@'))
                .collect();
            if names.is_empty() {
                names = vec!["personal".into(), "work".into()];
            }
            names
        }
    };
    let mut out = Vec::new();
    for c in channels {
        out.push(crate::metrics::channel_metrics(
            s.store(),
            &s.cfg,
            &c,
            since,
        )?);
    }
    Ok(Json(json!({
        "since_ms": since, "node_id": s.node_id,
        "note": "as seen from this node: usage is counted where the model call was made",
        "channels": out,
    })))
}

/// `GET /api/provenance/{sha}`: the trail behind a commit.
pub async fn provenance(
    State(s): State<AppState>,
    Path(sha): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let sha = sha.trim().to_ascii_lowercase();
    if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "give at least seven hex characters of the commit".into(),
        ));
    }
    let v = crate::metrics::provenance(s.store(), &sha)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        format!("no review on this node reviewed or published {sha}"),
    ))?;
    Ok(Json(v))
}

/// Hub reachability and mesh counters. Until the mesh client lands this
/// reports `disabled`, which the interface treats as "no hub configured".
pub async fn get_mesh(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let state = match &s.mesh {
        Some(m) => m.snapshot(),
        None => crate::mesh::MeshState {
            node_id: s.node_id.clone(),
            fingerprint: proto::enroll::fingerprint_hex(&s.node_id),
            ..Default::default()
        },
    };
    Ok(Json(json!(state)))
}

pub(crate) fn node_json(s: &AppState) -> Result<serde_json::Value, ApiError> {
    let row = s.store().get_node(&s.node_id)?;
    let Some(n) = row else {
        return Ok(
            json!({ "id": s.node_id, "state": "unknown", "is_self": true, "reachable": true }),
        );
    };
    let mut v = node_row_json(&n);
    v["providers"] = json!(providers_json(s));
    v["default_channel"] = json!(s.cfg.session.default_channel);
    Ok(v)
}

pub(crate) fn node_row_json(n: &crate::store::NodeRow) -> serde_json::Value {
    n.to_json()
}

#[derive(Deserialize)]
pub struct ListSessions {
    state: Option<String>,
}

pub async fn list_sessions(
    State(s): State<AppState>,
    Query(q): Query<ListSessions>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().list_sessions(q.state.as_deref())?;
    Ok(Json(json!(rows)))
}

/// Where sessions have run before, most recent first, so the form can offer a
/// pick instead of demanding a typed path. Managed clones ride along, so a
/// repo cloned from a forge is offered before it has ever run a session.
pub async fn recent_repos(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().recent_repos(20)?;
    let managed: Vec<serde_json::Value> = crate::forge::managed_repos(&Config::state_dir())
        .into_iter()
        .map(|r| {
            let path = crate::forge::managed_root(&Config::state_dir())
                .join(&r.host)
                .join(&r.owner)
                .join(&r.name);
            json!({ "repo_path": path, "full_name": r.full_name, "host": r.host })
        })
        .collect();
    Ok(Json(json!({ "repos": rows, "managed": managed })))
}

#[derive(Deserialize)]
pub struct ForgeQuery {
    channel: String,
    forge: Option<String>,
    cursor: Option<String>,
}

/// The operator's repositories on every forge whose credential this channel
/// may use. A forge with no credential at all is absent; one this channel is
/// not bound to answers with the refusal, per forge, so one dead forge never
/// hides another's list. A cursor is valid only with its named forge, so a
/// client cannot accidentally advance every configured provider.
pub async fn forge_repos(
    State(s): State<AppState>,
    Query(q): Query<ForgeQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    if q.cursor.is_some() && q.forge.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a forge is required with a repository page cursor",
        ));
    }
    let forge = match q.forge.as_deref() {
        Some(forge) => Some(
            crate::forge::Forge::parse(forge)
                .ok_or_else(|| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "no such forge"))?,
        ),
        None => None,
    };
    let out = crate::forge::list_repos(
        &s.tools.broker,
        &q.channel,
        &s.node_id,
        forge,
        q.cursor.as_deref(),
    )
    .await;
    Ok(Json(json!({ "forges": out })))
}

#[derive(Deserialize)]
pub struct CloneBody {
    channel: String,
    forge: String,
    host: String,
    owner: String,
    name: String,
}

/// Clone into the managed root, with the forge credential injected through
/// the environment. Idempotent: an existing clone answers with its path.
pub async fn clone_repo(
    State(s): State<AppState>,
    Json(b): Json<CloneBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let forge = crate::forge::Forge::parse(&b.forge)
        .ok_or_else(|| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "no such forge"))?;
    let root = crate::forge::managed_root(&Config::state_dir());
    let dest = crate::forge::clone_dest(&root, &b.host, &b.owner, &b.name)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let env = {
        let broker = s.tools.broker.read().unwrap();
        let cred = broker
            .env_for(forge.credential(), &b.channel, &s.node_id)
            .map_err(|e| ApiError::new(StatusCode::CONFLICT, e.to_string()))?;
        match forge.token(&cred) {
            Some(t) => crate::forge::git_credential_env(forge, t),
            None => Vec::new(), // a credential without a token: try anonymously
        }
    };
    let cloned = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        crate::forge::clone(env, &b.host, &b.owner, &b.name, &dest),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, "the clone ran out of time"))?;
    cloned.map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(json!({ "repo_path": dest })))
}

/// Import explicitly uploaded files into a new durable workspace. Browser
/// uploads carry bytes and checked relative paths; this endpoint never accepts
/// a host path and never writes to the selected source.
pub async fn import_workspace(
    State(s): State<AppState>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let id = uuid::Uuid::now_v7().to_string();
    let selected = Config::state_dir().join("workspace-imports").join(&id);
    std::fs::create_dir_all(&selected)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut files = std::collections::BTreeSet::new();
    let mut total = 0u64;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?
    {
        if field.name() != Some("files") {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "only files fields are accepted",
            ));
        }
        let relative = field
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "each file needs a relative name")
            })?
            .to_string();
        if relative.split('/').any(|part| part == ".git") {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Git metadata cannot be imported",
            ));
        }
        if !files.insert(relative.clone()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("duplicate selected file {relative}"),
            ));
        }
        if files.len() > crate::workspace::MAX_IMPORT_FILES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "selected files exceed the managed workspace import limit",
            ));
        }
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
        total = total.saturating_add(bytes.len() as u64);
        if total > crate::workspace::MAX_IMPORT_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "selected files exceed the managed workspace import limit",
            ));
        }
        let destination = crate::workspace::selected_path(&selected, &relative)
            .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        std::fs::write(destination, bytes)
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    if files.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "select at least one file or folder",
        ));
    }
    let workspace = crate::workspace::seed_from_files(&s.cfg.publish.git, &selected, &id)
        .await
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let result = crate::workspace::import(
        s.manager.backend().as_ref(),
        &workspace,
        &crate::workspace::staging_path(&id),
    )
    .await
    .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()));
    let _ = std::fs::remove_dir_all(&selected);
    result?;
    Ok(Json(
        json!({ "workspace_id": workspace.id, "files": files.len() }),
    ))
}

/// Materialize a node-owned snapshot for an explicit export operation.
pub async fn export_workspace(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let _snapshot = s.manager.snapshot_workspace_or_session(&id).await?;
    Ok(Json(json!({ "workspace_id": id, "exported": true })))
}

/// Prepare lockfile dependencies in a separate, credential-free runtime. The
/// project can select only a pinned or explicitly approved image; it cannot
/// supply a setup command.
pub async fn prepare_workspace(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::environment::PreparedEnvironment>> {
    let snapshot = s.manager.snapshot_workspace_or_session(&id).await?;
    let workspace = crate::workspace::Workspace {
        id: id.clone(),
        volume: crate::workspace::volume_name(&id),
        snapshot,
    };
    let plan = crate::environment::inspect(&workspace.snapshot)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let prepared =
        crate::environment::prepare(s.manager.backend().as_ref(), &s.cfg, &workspace, &plan)
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(prepared))
}

/// Download a workspace snapshot as a bounded zip. Symlinks were refused at
/// import and again at runtime export, so this archive cannot carry an escape.
pub async fn download_workspace(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let snapshot = match s.manager.snapshot_workspace_or_session(&id).await {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(),
    };
    let bytes = match zip_workspace(&snapshot) {
        Ok(bytes) => bytes,
        Err(error) => return ApiError(StatusCode::CONFLICT, error).into_response(),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/zip")
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"workspace-{}.zip\"",
                safe_download_name(&id)
            ),
        )
        .header("x-content-type-options", "nosniff")
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|error| {
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        })
}

fn zip_workspace(root: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Write};

    crate::workspace::validate_tree(root).map_err(|e| e.to_string())?;
    fn add(
        root: &std::path::Path,
        dir: &std::path::Path,
        archive: &mut zip::ZipWriter<&mut Cursor<Vec<u8>>>,
        options: zip::write::SimpleFileOptions,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
                return Err(format!("refusing unsafe workspace path {}", path.display()));
            }
            let name = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if metadata.is_dir() {
                add(root, &path, archive, options)?;
            } else {
                archive
                    .start_file(name, options)
                    .map_err(|e| e.to_string())?;
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                archive.write_all(&bytes).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    let mut cursor = Cursor::new(Vec::new());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    {
        let mut archive = zip::ZipWriter::new(&mut cursor);
        add(root, root, &mut archive, options)?;
        archive.finish().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}

pub async fn create_session(
    State(s): State<AppState>,
    Json(spec): Json<NewSession>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let row = s.manager.create(spec, s.adapter.clone()).await?;
    Ok((StatusCode::CREATED, Json(json!(row))))
}

/// One prompt: the work item and the session that starts on it.
#[derive(serde::Deserialize)]
pub struct ComposeBody {
    pub channel: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub priority: i64,
    pub repo_path: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default = "plan_phase")]
    pub phase: Phase,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
    #[serde(default)]
    pub node_id: Option<String>,
}

fn plan_phase() -> Phase {
    Phase::Plan
}

/// Start work from a prompt: the item is written first, then the session on
/// it. A session refused after the item exists keeps the item — the operator
/// typed it, and it is the only copy — and the refusal carries the item's id
/// so the interface can say where the words went.
pub async fn compose(State(s): State<AppState>, Json(c): Json<ComposeBody>) -> Response {
    match compose_inner(s, c).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn compose_inner(s: AppState, c: ComposeBody) -> ApiResult<Response> {
    let item = crate::corpus::work::create(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        crate::corpus::work::NewWork {
            channel: c.channel.clone(),
            project_id: None,
            title: c.title,
            body: c.body,
            deps: vec![],
            priority: c.priority,
            discovered_from: None,
            discovered_by_session: None,
        },
    )
    .map_err(work_err)?;
    let spec = NewSession {
        channel: c.channel,
        repo_path: c.repo_path,
        workspace_id: c.workspace_id,
        branch: c.branch,
        work_item_id: Some(item.id.clone()),
        model: c.model,
        budget_tokens: c.budget_tokens,
        initial_prompt: None,
        node_id: c.node_id,
        phase: c.phase,
        review_id: None,
        base_sha: None,
    };
    match s.manager.create(spec, s.adapter.clone()).await {
        Ok(row) => Ok((
            StatusCode::CREATED,
            Json(json!({ "work": item, "session": row })),
        )
            .into_response()),
        Err(e) => {
            let ApiError(code, message) = ApiError::from(e);
            Ok((
                code,
                Json(json!({
                    "error": { "code": code.as_u16(), "message": message },
                    "work_item_id": item.id,
                })),
            )
                .into_response())
        }
    }
}

pub async fn get_session(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = s
        .store()
        .get_session(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?;
    // Opening a peer's session: ask its owner for whatever history this node
    // has not mirrored yet. The answer arrives as events on the stream.
    if row.node_id != s.node_id {
        if let Some(mesh) = &s.mesh {
            mesh.request_backfill(&row.node_id, &id);
        }
    }
    let waiting: Vec<_> = s
        .store()
        .open_permissions()?
        .into_iter()
        .filter(|p| p.session_id == id)
        .collect();
    let questions = s.store().session_operator_questions(&id)?;
    Ok(Json(
        json!({ "session": row, "waiting": waiting, "questions": questions }),
    ))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    500
}

pub async fn session_events(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = s
        .store()
        .events_after(&id, q.after, q.limit.clamp(0, 2000))?;
    Ok(Json(json!(rows)))
}

#[derive(Deserialize)]
pub struct PromptBody {
    text: String,
}

pub async fn prompt(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PromptBody>,
) -> ApiResult<StatusCode> {
    s.manager.prompt(&id, b.text).await?;
    Ok(StatusCode::ACCEPTED)
}
#[derive(Deserialize)]
pub struct SessionControlBody {
    #[serde(default)]
    reason: Option<String>,
}

pub async fn pause(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SessionControlBody>,
) -> ApiResult<StatusCode> {
    s.manager
        .pause(
            &id,
            body.reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "operator paused the session".into()),
        )
        .await?;
    Ok(StatusCode::OK)
}

pub async fn resume(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SessionControlBody>,
) -> ApiResult<StatusCode> {
    s.manager
        .resume(
            &id,
            body.reason
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| "operator resumed the session".into()),
        )
        .await?;
    Ok(StatusCode::OK)
}

pub async fn stop(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    s.manager.stop(&id).await?;
    Ok(StatusCode::OK)
}

pub async fn kill(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    s.manager.kill(&id).await?;
    Ok(StatusCode::OK)
}

/// Put an ended session away, or bring it back. Presentation only: nothing
/// about the session changes but whether the home still lists it.
pub async fn archive_session(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    set_archived(&s, &id, Some(crate::store::now_ms()))
}

pub async fn unarchive_session(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    set_archived(&s, &id, None)
}

fn set_archived(s: &AppState, id: &str, ms: Option<i64>) -> ApiResult<Json<serde_json::Value>> {
    let row = s
        .store()
        .set_session_archived(id, ms)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?;
    s.manager
        .bus()
        .publish_untapped(crate::stream::Frame::Session(Box::new(row.clone())));
    Ok(Json(json!(row)))
}

/// Put away everything that has ended, in one gesture. Running sessions are
/// left alone.
pub async fn archive_ended(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().archive_ended_sessions(crate::store::now_ms())?;
    for row in &rows {
        s.manager
            .bus()
            .publish_untapped(crate::stream::Frame::Session(Box::new(row.clone())));
    }
    Ok(Json(json!({ "archived": rows.len() })))
}

#[derive(Deserialize)]
pub struct DraftBody {
    text: String,
}

/// The node holds unsent drafts, so a closed tab or an evicted phone loses
/// nothing typed.
pub async fn put_draft(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<DraftBody>,
) -> ApiResult<StatusCode> {
    let text = (!b.text.is_empty()).then_some(b.text);
    s.store().set_draft(&id, text.as_deref())?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct AnswerBody {
    option_id: String,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

pub async fn answer_permission(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<AnswerBody>,
) -> ApiResult<StatusCode> {
    s.manager.answer(&id, b.option_id, b.arguments).await?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize, Clone)]
pub struct VerdictBody {
    /// "approve" or "reject". Anything else is refused rather than guessed at.
    pub verdict: String,
    #[serde(default)]
    pub reason: Option<String>,
    /// The operator's edits to what gets published, if they made any.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    /// A unified diff the operator edited by hand, sent with "revise". The
    /// agent applies it and resubmits, so an edit is a request for changes
    /// rather than an approval of something the operator changed.
    #[serde(default)]
    pub patch: Option<String>,
}

pub async fn get_review(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    // Claim on open, then read: a response that showed the row as it stood
    // before the claim would tell the operator something already untrue.
    let _ = s.store().claim_review(&id);
    let r = s
        .store()
        .get_review(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such review".into()))?;
    let stale = staleness_of(&s, &r).await;
    s.manager.publish_queue().await;
    let revision = s.store().latest_review_revision(&id)?;
    let evidence = match revision.as_ref() {
        Some(revision) => Some(s.store().candidate_evidence(&revision.candidate_id)?),
        None => None,
    };
    let requirements = s
        .store()
        .get_session(&r.session_id)?
        .and_then(|session| session.work_item_id)
        .and_then(|work_item_id| s.store().work_get(&work_item_id).ok().flatten());
    let surrounding_code = revision
        .as_ref()
        .and_then(|revision| serde_json::from_str::<serde_json::Value>(&revision.context_json).ok())
        .unwrap_or_else(|| json!([]));
    let legacy_check_events = s.store().legacy_check_runs_for_session(&r.session_id)?;
    Ok(Json(json!({
        "review": r,
        "stale": stale,
        "requirements": requirements,
        "surrounding_code": surrounding_code,
        "evidence": evidence,
        "legacy_check_events": legacy_check_events,
    })))
}

/// Where a review's diff was captured: the operator's worktree for a harness
/// they run themselves, else the submitting session's own.
fn worktree_of(s: &AppState, r: &crate::store::ReviewRow) -> Option<String> {
    serde_json::from_str::<crate::review::publish::Target>(&r.target)
        .ok()
        .and_then(|t| t.worktree)
        .or_else(|| {
            s.store()
                .get_session(&r.session_id)
                .ok()
                .flatten()
                .and_then(|session| session.worktree_path)
        })
}

/// What changed in the worktree since submit. An empty list means the diff
/// still describes the branch.
async fn staleness_of(s: &AppState, r: &crate::store::ReviewRow) -> Vec<String> {
    if let Ok(Some(session)) = s.store().get_session(&r.session_id) {
        if session.node_id == s.node_id
            && session.harness_id != crate::session::external::HARNESS_ID
            && s.manager.snapshot_workspace(&r.session_id).await.is_err()
        {
            return vec!["the runtime workspace could not be safely snapshotted".into()];
        }
    }
    let Some(worktree) = worktree_of(s, r) else {
        return vec!["the worktree is gone".into()];
    };
    let files: Vec<crate::review::FileAtSubmit> =
        serde_json::from_str(&r.files).unwrap_or_default();
    crate::review::staleness(&worktree, &r.head_sha, &files).await
}

#[derive(Deserialize)]
pub struct FileQuery {
    pub path: String,
}

/// One reviewed file as it was submitted, for the diff editor. Read by the
/// blob hash recorded at submit, so it is the text the diff was taken against
/// even if the worktree has moved since.
pub async fn review_file(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let r = s.store().get_review(&id)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "no review with that id".into(),
    ))?;
    // The worktree lives on the owning node, so a peer's review cannot be
    // edited here. Saying so beats an empty editor.
    if r.node_id != s.node_id {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this review belongs to another node; editing happens where the worktree is".into(),
        ));
    }
    let worktree = worktree_of(&s, &r).ok_or(ApiError(
        StatusCode::CONFLICT,
        "the worktree is gone".into(),
    ))?;
    let files: Vec<crate::review::FileAtSubmit> =
        serde_json::from_str(&r.files).unwrap_or_default();
    let text = crate::review::file_at_submit(&worktree, &files, &q.path)
        .await
        .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({ "path": q.path, "text": text })))
}

fn record_evidence_decision(
    store: &Store,
    review_id: &str,
    decision: &str,
    reason: Option<&str>,
    title: Option<&str>,
    body: Option<&str>,
    patch: Option<&str>,
) -> Result<Option<i64>, crate::store::StoreError> {
    let Some(revision) = store.latest_review_revision(review_id)? else {
        // A row directly written by an old node/test fixture has no immutable
        // revision to honestly associate with a new decision.
        return Ok(None);
    };
    let decided_ms = crate::store::now_ms();
    store.record_review_decision(&crate::store::ReviewDecisionRow {
        id: uuid::Uuid::now_v7().to_string(),
        review_id: review_id.to_string(),
        revision_id: revision.id,
        source: "operator".into(),
        decision: decision.to_string(),
        reason: reason.map(str::to_string),
        title: title.map(str::to_string),
        body: body.map(str::to_string),
        patch: patch.map(str::to_string),
        decided_ms,
    })?;
    Ok(Some((decided_ms - revision.created_ms).max(0)))
}

fn record_operator_decision_event(
    s: &AppState,
    session_id: &str,
    review_id: &str,
    decision: &str,
    waiting_ms: Option<i64>,
) {
    if let Some(waiting_ms) = waiting_ms {
        s.manager.record_event(
            session_id,
            crate::session::state::event_kind::REVIEW_DECISION,
            json!({ "source": "operator", "review_id": review_id, "decision": decision, "waiting_ms": waiting_ms }),
        );
    }
}

#[derive(Deserialize)]
pub struct CandidateLookup {
    pub channel: String,
}

/// Candidate evidence is selected by a persisted immutable id, never by a
/// worktree path. The channel query makes commit lookup explicit across the
/// node's isolated channels.
pub async fn candidate_by_commit(
    State(s): State<AppState>,
    Path(head_sha): Path<String>,
    Query(query): Query<CandidateLookup>,
) -> ApiResult<Json<serde_json::Value>> {
    let candidate = s
        .store()
        .candidate_by_commit(&query.channel, &head_sha)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no candidate on that channel for this commit".into()))?;
    let evidence = s.store().candidate_evidence(&candidate.id)?;
    Ok(Json(json!({ "evidence": evidence })))
}

pub async fn candidate_evidence(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let evidence = s
        .store()
        .candidate_evidence(&id)
        .map_err(|error| match error {
            crate::store::StoreError::Invalid(_) => {
                ApiError(StatusCode::NOT_FOUND, "no such candidate".into())
            }
            other => ApiError::from(other),
        })?;
    Ok(Json(json!({ "evidence": evidence })))
}

#[derive(Deserialize)]
pub struct DemonstrationBody {
    pub slug: String,
    pub label: String,
}

/// Link a curated document snapshot to a candidate. This deliberately does
/// not execute the document — Showboat-style command blocks remain a human
/// authored demonstration, not an untrusted command surface in review.
pub async fn attach_demonstration(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DemonstrationBody>,
) -> ApiResult<Json<crate::store::DemonstrationRow>> {
    let candidate = s
        .store()
        .candidate(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such candidate".into()))?;
    let label = body.label.trim();
    if label.is_empty() || label.chars().count() > 200 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "demonstration label must be between 1 and 200 characters".into(),
        ));
    }
    let document = s
        .store()
        .doc_get(&candidate.channel, &body.slug)?
        .ok_or(ApiError(
            StatusCode::NOT_FOUND,
            "no current document with that slug on the candidate channel".into(),
        ))?;
    let demonstration = crate::store::DemonstrationRow {
        id: uuid::Uuid::now_v7().to_string(),
        candidate_id: candidate.id,
        channel: candidate.channel,
        document_id: document.id,
        document_slug: document.slug,
        document_hash: document.hash,
        label: label.to_string(),
        created_ms: crate::store::now_ms(),
    };
    s.store().attach_demonstration(&demonstration)?;
    Ok(Json(demonstration))
}

/// Called when the operator leaves the review screen. Explicit release is the
/// common case; the sweeper is for clients that vanish without saying so.
pub async fn release_review(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    s.store().release_review(&id)?;
    s.manager.publish_queue().await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn decide_review(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<VerdictBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let r = s
        .store()
        .get_review(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such review".into()))?;
    // A verdict is node-local by construction: staleness and publishing need
    // the worktree and the broker on the owner. Forward it there.
    if r.node_id != s.node_id {
        let mesh = s.mesh.as_ref().ok_or(ApiError(
            StatusCode::CONFLICT,
            "this review belongs to another node and this node is not on a mesh".into(),
        ))?;
        if !mesh.peer_reachable(&r.node_id) {
            return Err(ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "the node that owns this review is unreachable; it cannot be decided until it returns".into(),
            ));
        }
        let timeout = std::time::Duration::from_secs(s.cfg.mesh.command_timeout_secs.max(1));
        return match mesh
            .command(
                &r.node_id,
                proto::frame::Command::Verdict {
                    review_id: id.clone(),
                    verdict: b.verdict.clone(),
                    reason: b.reason.clone(),
                    title: b.title.clone(),
                    body: b.body.clone(),
                    patch: b.patch.clone(),
                },
                timeout,
            )
            .await
        {
            Ok(v) => Ok(Json(v)),
            Err(crate::mesh::forward::CommandError::Timeout) => Err(ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "the node that owns this review did not answer".into(),
            )),
            Err(e) => Err(ApiError(StatusCode::CONFLICT, e.to_string())),
        };
    }
    decide_local(&s, &id, b).await.map(Json)
}

/// The verdict as executed on the owning node.
pub(crate) async fn decide_local(
    s: &AppState,
    id: &str,
    b: VerdictBody,
) -> ApiResult<serde_json::Value> {
    let s = s.clone();
    let id = id.to_string();
    let r = s
        .store()
        .get_review(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such review".into()))?;
    if r.state == "approved" || r.state == "rejected" {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("this review was already {}", r.state),
        ));
    }

    match b.verdict.as_str() {
        "revise" => {
            // Changes requested. The review stays open as one evolving thread,
            // and the notes go back to the agent, which is still the only
            // writer to the worktree.
            let notes = b
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "asking for changes needs a note saying what to change".into(),
                ))?;
            // Not trimmed: a patch's trailing newline is part of it, and
            // `git apply` calls a patch without one corrupt.
            let patch = b.patch.as_deref().filter(|p| !p.trim().is_empty());
            if !s.store().request_changes(&id, notes, patch)? {
                let now = s.store().get_review(&id)?.map(|r| r.state);
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "this review is no longer awaiting a verdict ({})",
                        now.as_deref().unwrap_or("gone")
                    ),
                ));
            }
            let waiting_ms =
                record_evidence_decision(s.store(), &id, "revise", Some(notes), None, None, patch)?;
            record_operator_decision_event(&s, &r.session_id, &id, "revise", waiting_ms);
            s.manager.publish_queue().await;
            Ok(json!({ "state": "revising" }))
        }
        "reject" => {
            // A bare rejection teaches the agent nothing.
            let reason = b
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .ok_or(ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "a rejection needs a reason".into(),
                ))?;
            // The guard is in the UPDATE: if the row is no longer awaiting a
            // verdict, say so rather than reporting a rejection that did not land.
            let resolved =
                s.store()
                    .resolve_review(&id, "rejected", Some(reason), None, None, None, 0)?;
            if !resolved {
                let now = s.store().get_review(&id)?.map(|r| r.state);
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "this review is no longer awaiting a verdict ({})",
                        now.as_deref().unwrap_or("gone")
                    ),
                ));
            }
            let waiting_ms =
                record_evidence_decision(s.store(), &id, "rejected", Some(reason), None, None, None)?;
            record_operator_decision_event(&s, &r.session_id, &id, "rejected", waiting_ms);
            s.manager.publish_queue().await;
            Ok(json!({ "state": "rejected" }))
        }
        "approve" => {
            let title = b.title.as_deref().unwrap_or(r.approved_title()).to_string();
            let body = b.body.as_deref().unwrap_or(r.approved_body()).to_string();
            match crate::authority::publish_review(
                &crate::authority::PublishContext {
                    store: s.store(),
                    manager: &s.manager,
                    broker: &s.tools.broker,
                    cfg: &s.cfg,
                    node_id: &s.node_id,
                },
                crate::authority::PublishRequest {
                    review: &r,
                    title: &title,
                    body: &body,
                    require_evidence: false,
                    recheck_authority: None,
                },
            )
            .await
            {
                Ok(published) => {
                    let waiting_ms = record_evidence_decision(
                        s.store(),
                        &id,
                        "approved",
                        None,
                        Some(&title),
                        Some(&body),
                        None,
                    )?;
                    record_operator_decision_event(&s, &r.session_id, &id, "approved", waiting_ms);
                    Ok(json!({ "state": "approved", "published": published }))
                }
                Err(crate::authority::PublishError::Conflict(message)) => {
                    Err(ApiError(StatusCode::CONFLICT, message))
                }
                Err(crate::authority::PublishError::External(message)) => {
                    Err(ApiError(StatusCode::BAD_GATEWAY, message))
                }
            }
        }
        other => Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{other:?} is not a verdict"),
        )),
    }
}

/// Whether this node answers harnesses outside the boundary, which channels
/// one could attach to, and what is attached now. The CLI prints the line to
/// register with; the interface shows the same.
pub async fn external(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().channel_list()?;
    let channels: Vec<String> = if rows.is_empty() {
        DEFAULT_CHANNELS.iter().map(|c| c.to_string()).collect()
    } else {
        rows.into_iter()
            .filter(|c| !c.name.starts_with('@'))
            .filter(|c| {
                serde_json::from_str::<serde_json::Value>(&c.bindings_json)
                    .map(|b| b["archived"].is_null())
                    .unwrap_or(true)
            })
            .map(|c| c.name)
            .collect()
    };
    let attached: Vec<serde_json::Value> = s
        .manager
        .external_attachments()
        .await
        .into_iter()
        .map(|(channel, session_id)| json!({ "channel": channel, "session_id": session_id }))
        .collect();
    Ok(Json(json!({
        "enabled": s.cfg.external.enabled,
        "channels": channels,
        "attachments": attached,
    })))
}

/// The queue, ordered on the node: waiting-on-you first, oldest first within
/// it. The interface renders this order rather than deciding it.
pub async fn queue(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let waiting = s.store().open_permissions()?;
    // Permission requests before reviews: requests expire, reviews do not.
    let reviews = s.store().open_reviews()?;
    // Bounded and filtered in SQL: the home shows what landed lately, not
    // every session the node has ever run.
    let (running, ended) = s.store().queue_sessions(20)?;
    let promotions = s.store().open_promotions()?;
    Ok(Json(json!({
        "waiting": waiting, "reviews": reviews, "promotions": promotions, "running": running, "ended": ended
    })))
}

#[derive(Deserialize)]
pub struct OperatorAnswerBody {
    pub answer: String,
}

pub async fn operator_questions(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        json!({ "questions": s.store().open_operator_questions()? }),
    ))
}

pub async fn answer_operator_question(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<OperatorAnswerBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let question = s.store().operator_question(&id)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "no such operator question".into(),
    ))?;
    let origin = s
        .store()
        .get_session(&question.session_id)?
        .ok_or(ApiError(
            StatusCode::CONFLICT,
            "question origin session is unavailable".into(),
        ))?;
    if origin.harness_id != crate::session::external::HARNESS_ID
        && matches!(origin.state.as_str(), "closed" | "killed_budget" | "failed")
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this question's session ended; it remains inspectable but cannot be answered".into(),
        ));
    }
    let answer = b.answer.trim();
    if answer.is_empty() || answer.len() > 8 * 1024 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "answer must be 1–8192 bytes".into(),
        ));
    }
    let choices: Vec<String> = serde_json::from_str(&question.choices_json).map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "stored question choices are invalid".into(),
        )
    })?;
    if !choices.is_empty() && !choices.iter().any(|choice| choice == answer) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "answer must select one offered choice".into(),
        ));
    }
    let answer = json!({ "text": answer }).to_string();
    if s.store().answer_operator_question(&id, &answer)?.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this question was already answered or cancelled".into(),
        ));
    }
    Ok(Json(json!({ "answered": true })))
}

pub async fn cancel_operator_question(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if !s.store().cancel_operator_question(&id)? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this question was already answered or cancelled".into(),
        ));
    }
    Ok(Json(json!({ "cancelled": true })))
}

pub async fn operator_notification(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "notification_id": id,
        "attempts": s.store().notification_attempts(&id)?,
        "receipt": "device push-service attempts and peer acknowledgements only; human receipt is unknown",
    })))
}

pub async fn operator_notifications(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let notifications = s.store().operator_notifications()?;
    let rows: Vec<_> = notifications
        .into_iter()
        .map(|notification| {
            let attempts = s.store().notification_attempts(&notification.id).unwrap_or_default();
            json!({ "notification_id": notification.id, "expires_ms": notification.expires_ms, "attempts": attempts })
        })
        .collect();
    Ok(Json(json!({
        "notifications": rows,
        "receipt": "device push-service attempts and peer acknowledgements only; human receipt is unknown",
    })))
}

pub async fn operator_issues(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "issues": s.store().issue_drafts(false)? })))
}

pub async fn operator_issue(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let issue = s.store().issue_draft(&id)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "no such issue draft".into(),
    ))?;
    Ok(Json(json!({ "issue": issue })))
}

/// An issue can leave only through the node-side broker, targeting tracon's
/// repository. The row is claimed before spawning `gh`, so double-clicks and
/// concurrent clients cannot publish two copies.
#[derive(Deserialize)]
pub struct ReconcileIssueBody {
    /// The operator has searched cosmicspork/tracon for this draft's marker
    /// and confirmed no issue was created. This is never inferred.
    pub confirmed_absent: bool,
}

pub async fn reconcile_operator_issue(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReconcileIssueBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if !body.confirmed_absent {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm the draft marker is absent before retrying".into(),
        ));
    }
    if !s.store().retry_uncertain_issue_publication(&id)? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "only an uncertain issue draft can be reconciled".into(),
        ));
    }
    Ok(Json(json!({ "reconciled": true, "state": "draft" })))
}

pub async fn publish_operator_issue(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let issue = s.store().issue_draft(&id)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "no such issue draft".into(),
    ))?;
    let env = s
        .tools
        .broker
        .read()
        .unwrap()
        .env_for("gh", &issue.channel, &s.node_id)
        .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
    let attachments: serde_json::Value =
        serde_json::from_str(&issue.attachments_json).map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored issue attachments are invalid".into(),
            )
        })?;
    let body = format!(
        "{}\n\n## Inspectable attachments\n\n```json\n{}\n```\n\n<!-- tracon-issue-draft:{} -->",
        issue.body,
        serde_json::to_string_pretty(&attachments).unwrap_or_else(|_| "[]".into()),
        issue.id,
    );
    if !s.store().begin_issue_publication(&id)? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "this issue was already authorized or is no longer a draft".into(),
        ));
    }
    let result = tokio::process::Command::new(&s.cfg.publish.gh)
        .args([
            "issue",
            "create",
            "--repo",
            "cosmicspork/tracon",
            "--title",
            &issue.title,
            "--body",
            &body,
        ])
        .current_dir(Config::state_dir())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .envs(env)
        .output()
        .await;
    match result {
        Ok(out) if out.status.success() => {
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            s.store().finish_issue_publication(&id, &url)?;
            Ok(Json(json!({ "published": true, "url": url })))
        }
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            s.store().fail_issue_publication(&id, &err)?;
            Err(ApiError(StatusCode::BAD_GATEWAY, err))
        }
        Err(e) => {
            s.store().mark_issue_publication_uncertain(
                &id,
                &format!("publication dispatch outcome unknown: {e}; reconcile draft marker before retrying"),
            )?;
            Err(ApiError(StatusCode::BAD_GATEWAY, e.to_string()))
        }
    }
}
// ---- promotions ----

pub async fn get_promotion(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let p = s
        .store()
        .promotion_get(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such batch".into()))?;
    let items: serde_json::Value = serde_json::from_str(&p.items_json).unwrap_or(json!([]));
    let verdicts: serde_json::Value = p
        .verdicts_json
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(json!({}));
    Ok(Json(
        json!({ "promotion": p, "items": items, "verdicts": verdicts }),
    ))
}

#[derive(Deserialize)]
pub struct PromotionVerdicts {
    /// `memory_id → "promote" | "reject"`.
    pub verdicts: serde_json::Map<String, serde_json::Value>,
}

pub async fn decide_promotion(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PromotionVerdicts>,
) -> ApiResult<Json<serde_json::Value>> {
    let done =
        crate::corpus::promote::decide(s.store(), s.manager.bus(), &s.node_id, &id, &body.verdicts)
            .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    if !done {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "no open batch by that id".into(),
        ));
    }
    let p = s.store().promotion_get(&id)?;
    Ok(Json(
        json!({ "state": p.map(|p| p.state).unwrap_or_default() }),
    ))
}

/// `POST /api/promotions/batch`: build the batches now rather than tonight.
pub async fn batch_promotions(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let ids = crate::corpus::promote::batch_now(s.store(), s.manager.bus(), &s.node_id, 0);
    Ok(Json(json!({ "created": ids })))
}

// ---- corpus: documents, memories, recall ----

#[derive(Deserialize, Default)]
pub struct DocQuery {
    pub channel: Option<String>,
    pub q: Option<String>,
    pub kind: Option<String>,
    /// Include archived documents in the list. Search leaves them out.
    #[serde(default)]
    pub archived: bool,
}

/// `GET /api/docs?channel=&q=&kind=&archived=`: the list (no bodies), or
/// search hits.
pub async fn list_docs(
    State(s): State<AppState>,
    Query(q): Query<DocQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Some(text) = q.q.as_deref().filter(|t| !t.trim().is_empty()) {
        let near = crate::embed::neighbours(
            &s.cfg,
            s.store(),
            &s.tools.http,
            s.manager.probe_token(),
            q.channel.as_deref(),
            text,
            50,
        )
        .await;
        let hits = s.store().doc_search_hybrid(
            q.channel.as_deref(),
            q.kind.as_deref(),
            text,
            50,
            &near.hits,
        )?;
        return Ok(Json(json!({ "hits": hits, "text_only": near.degraded })));
    }
    let docs = s.store().doc_list(q.channel.as_deref())?;
    let docs: Vec<_> = docs
        .into_iter()
        .filter(|d| q.kind.as_deref().is_none_or(|k| k == d.kind))
        .filter(|d| q.archived || d.archived == 0)
        .collect();
    Ok(Json(json!({ "docs": docs })))
}

pub async fn get_doc(
    State(s): State<AppState>,
    Path((channel, slug)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = s.store().doc_get(&channel, &slug)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        format!("no document {slug} on {channel}"),
    ))?;
    let mut value = json!(doc);
    if doc.format == "html" {
        value["bundle_files"] = json!(s.store().html_bundle_metadata(&doc)?);
    }
    Ok(Json(value))
}

pub async fn preview_doc(
    State(s): State<AppState>,
    Path((channel, slug)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let document = match s.store().doc_get(&channel, &slug) {
        Ok(Some(document)) => document,
        Ok(None) => {
            return ApiError(
                StatusCode::NOT_FOUND,
                format!("no document {slug} on {channel}"),
            )
            .into_response();
        }
        Err(error) => return ApiError::from(error).into_response(),
    };
    if document.format != "html" {
        return ApiError(StatusCode::CONFLICT, "document is not HTML".into()).into_response();
    }
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
        || headers
            .get("forwarded")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .any(|part| part.trim().eq_ignore_ascii_case("proto=https"))
            });
    if forwarded_https && s.cfg.docs.preview_url.is_none() {
        return ApiError(
            StatusCode::CONFLICT,
            "docs.preview_url must name a separate HTTPS preview origin when the operator UI uses HTTPS"
                .into(),
        )
        .into_response();
    }
    let Some(entry_path) = document.entry_path.as_deref() else {
        return ApiError(
            StatusCode::CONFLICT,
            "HTML document has no entry path".into(),
        )
        .into_response();
    };
    let (token, expires_ms) = s.manager.previews().mint(super::preview::PreviewBinding {
        channel,
        document_id: document.id,
        document_hash: document.hash,
    });
    let mut url = match url::Url::parse(
        &s.cfg
            .docs
            .preview_origin()
            .expect("validated preview origin"),
    ) {
        Ok(url) => url,
        Err(error) => {
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    {
        let mut segments = url.path_segments_mut().expect("HTTP(S) origin");
        segments.clear();
        segments.push("p");
        segments.push(&token);
        for segment in entry_path.split('/') {
            segments.push(segment);
        }
    }
    Json(json!({ "url": url.as_str(), "expires_ms": expires_ms })).into_response()
}

pub async fn download_doc(
    State(s): State<AppState>,
    Path((channel, slug)): Path<(String, String)>,
) -> Response {
    use std::io::{Cursor, Write};

    let document = match s.store().doc_get(&channel, &slug) {
        Ok(Some(document)) => document,
        Ok(None) => {
            return ApiError(
                StatusCode::NOT_FOUND,
                format!("no document {slug} on {channel}"),
            )
            .into_response();
        }
        Err(error) => return ApiError::from(error).into_response(),
    };
    if document.format != "html" {
        return ApiError(
            StatusCode::CONFLICT,
            "original downloads are available for HTML documents only".into(),
        )
        .into_response();
    }
    let files = match s.store().read_html_bundle(&document) {
        Ok(files) => files,
        Err(crate::store::HtmlReadError::Corrupt(error)) => {
            return ApiError(StatusCode::CONFLICT, error).into_response();
        }
        Err(crate::store::HtmlReadError::Sqlite(error)) => {
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let source_name = document.source_name.as_deref().unwrap_or("document");
    let (bytes, media_type, filename) = if files.len() == 1 {
        (
            files.into_iter().next().expect("one file").bytes,
            "application/octet-stream",
            safe_download_name(source_name),
        )
    } else {
        let mut output = Cursor::new(Vec::new());
        let zip_result = (|| -> Result<(), zip::result::ZipError> {
            let mut archive = zip::ZipWriter::new(&mut output);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o644);
            for file in files {
                archive.start_file(file.path, options)?;
                archive.write_all(&file.bytes)?;
            }
            archive.finish()?;
            Ok(())
        })();
        if let Err(error) = zip_result {
            return ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
        let mut name = safe_download_name(source_name);
        if !name.to_ascii_lowercase().ends_with(".zip") {
            name.push_str(".zip");
        }
        (output.into_inner(), "application/zip", name)
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, media_type)
        .header(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .header("x-content-type-options", "nosniff")
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|error| {
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        })
}

fn safe_download_name(source_name: &str) -> String {
    let safe: String = source_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "document".into()
    } else {
        safe
    }
}

/// Import one exact HTML file or relative-path bundle under an explicit
/// optimistic-concurrency precondition.
pub async fn import_html(
    State(s): State<AppState>,
    Path((channel, slug)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Response {
    use crate::corpus::html::{
        HtmlBundleError, HtmlFile, MAX_BUNDLE_BYTES, MAX_FILES, MAX_FILE_BYTES,
    };
    use crate::store::HtmlWriteError;

    if !crate::mcp::docs::valid_slug(&slug) {
        return ApiError(
            StatusCode::BAD_REQUEST,
            format!("slug {slug:?} is not usable"),
        )
        .into_response();
    }
    let if_match = headers
        .get("if-match")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim_matches('"').to_string());
    let create_only = headers
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == "*");
    if if_match.is_some() == create_only {
        return ApiError(
            StatusCode::BAD_REQUEST,
            "HTML import requires either If-None-Match: * or If-Match: <current hash>".into(),
        )
        .into_response();
    }

    let mut source_name = None;
    let mut entry_path = None;
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    let mut saw_file = false;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return ApiError(StatusCode::BAD_REQUEST, error.to_string()).into_response();
            }
        };
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "source_name" if !saw_file && source_name.is_none() => {
                match multipart_text(field, 1024).await {
                    Ok(value) => source_name = Some(value),
                    Err(error) => return error.into_response(),
                }
            }
            "entry_path" if !saw_file && source_name.is_some() && entry_path.is_none() => {
                match multipart_text(field, 1024).await {
                    Ok(value) => entry_path = Some(value),
                    Err(error) => return error.into_response(),
                }
            }
            "files" if source_name.is_some() && entry_path.is_some() => {
                saw_file = true;
                if files.len() == MAX_FILES {
                    return ApiError(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        HtmlBundleError::TooManyFiles.to_string(),
                    )
                    .into_response();
                }
                let Some(path) = field.file_name().map(str::to_string) else {
                    return ApiError(
                        StatusCode::BAD_REQUEST,
                        "each files field requires a relative-path filename".into(),
                    )
                    .into_response();
                };
                let mut field = field;
                let mut bytes = Vec::new();
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if bytes.len() + chunk.len() > MAX_FILE_BYTES
                                || total_bytes + chunk.len() > MAX_BUNDLE_BYTES
                            {
                                return ApiError(
                                    StatusCode::PAYLOAD_TOO_LARGE,
                                    "HTML bundle exceeds its size limit".into(),
                                )
                                .into_response();
                            }
                            total_bytes += chunk.len();
                            bytes.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(error) => {
                            return ApiError(StatusCode::BAD_REQUEST, error.to_string())
                                .into_response();
                        }
                    }
                }
                files.push(HtmlFile { path, bytes });
            }
            _ => {
                return ApiError(
                    StatusCode::BAD_REQUEST,
                    "multipart fields must be source_name, entry_path, then repeated files".into(),
                )
                .into_response();
            }
        }
    }

    let (Some(source_name), Some(entry_path)) = (source_name, entry_path) else {
        return ApiError(
            StatusCode::BAD_REQUEST,
            "source_name and entry_path are required".into(),
        )
        .into_response();
    };
    match s.store().write_html_document_change(
        &s.node_id,
        &channel,
        &slug,
        &source_name,
        &entry_path,
        files,
        if_match.as_deref(),
        create_only,
    ) {
        Ok((doc, changes)) => {
            for change in changes {
                s.manager.bus().publish(crate::stream::Frame::Changes {
                    channel: channel.clone(),
                    changes: vec![change],
                });
            }
            Json(json!(doc)).into_response()
        }
        Err(HtmlWriteError::Conflict { hash, body }) => (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({
                "error": { "message": "the document changed since it was read" },
                "hash": hash,
                "body": body,
            })),
        )
            .into_response(),
        Err(HtmlWriteError::Bundle(error)) => {
            let status = match error {
                HtmlBundleError::TooManyFiles
                | HtmlBundleError::FileTooLarge(_)
                | HtmlBundleError::BundleTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                _ => StatusCode::BAD_REQUEST,
            };
            ApiError(status, error.to_string()).into_response()
        }
        Err(HtmlWriteError::InvalidSourceName) => {
            ApiError(StatusCode::BAD_REQUEST, "source name is invalid".into()).into_response()
        }
        Err(HtmlWriteError::Sqlite(error)) => {
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
        Err(HtmlWriteError::Sync(error)) => {
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        }
    }
}

async fn multipart_text(
    mut field: axum::extract::multipart::Field<'_>,
    max_bytes: usize,
) -> ApiResult<String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?
    {
        if bytes.len() + chunk.len() > max_bytes {
            return Err(ApiError(
                StatusCode::PAYLOAD_TOO_LARGE,
                "multipart text field is too large".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "multipart text must be UTF-8".into(),
        )
    })
}

#[derive(Deserialize)]
pub struct PutDoc {
    #[serde(default)]
    pub body: Option<String>,
    /// Archive or restore it with this write; absent keeps it as it was.
    #[serde(default)]
    pub archived: Option<bool>,
}

/// `PUT /api/docs/{channel}/{slug}` with `If-Match: <hash>` to refuse
/// overwriting an edit not yet seen; a conflict returns 412 with the current
/// hash and body, the editor contract the corpus inherited.
pub async fn put_doc(
    State(s): State<AppState>,
    Path((channel, slug)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PutDoc>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_matches('"').to_string());
    let create_only = headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "*");
    if if_match.is_some() && create_only {
        return ApiError(
            StatusCode::BAD_REQUEST,
            "if-match and if-none-match cannot be combined".into(),
        )
        .into_response();
    }
    let existing = match s.store().doc_get(&channel, &slug) {
        Ok(document) => document,
        Err(error) => return ApiError::from(error).into_response(),
    };
    if existing
        .as_ref()
        .is_some_and(|document| document.format == "html")
    {
        if body.body.is_some() {
            return ApiError(
                StatusCode::CONFLICT,
                "HTML documents must be replaced through import".into(),
            )
            .into_response();
        }
        let Some(archived) = body.archived else {
            return ApiError(
                StatusCode::BAD_REQUEST,
                "archived is required when an HTML document body is omitted".into(),
            )
            .into_response();
        };
        if create_only {
            let document = existing.expect("checked above");
            return document_conflict_response(document.hash, document.body);
        }
        return match s.store().set_document_archived_change(
            &s.node_id,
            &channel,
            &slug,
            archived,
            if_match.as_deref(),
        ) {
            Ok(crate::store::DocumentWrite::Written { row, change }) => {
                s.manager.bus().publish(crate::stream::Frame::Changes {
                    channel,
                    changes: vec![change],
                });
                Json(json!(*row)).into_response()
            }
            Ok(crate::store::DocumentWrite::Conflict { hash, body }) => {
                document_conflict_response(hash, body)
            }
            Ok(crate::store::DocumentWrite::HtmlDocument) => unreachable!(),
            Err(error) => ApiError::from(error).into_response(),
        };
    }
    let Some(markdown) = body.body.as_deref() else {
        return ApiError(
            StatusCode::BAD_REQUEST,
            "body is required for Markdown documents".into(),
        )
        .into_response();
    };
    match crate::mcp::docs::write_document(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &channel,
        &slug,
        markdown,
        if_match.as_deref(),
        create_only,
        body.archived,
    ) {
        Ok(doc) => Json(json!(doc)).into_response(),
        Err(crate::mcp::docs::WriteError::Conflict { hash, body }) => {
            document_conflict_response(hash, body)
        }
        Err(crate::mcp::docs::WriteError::Slug(m)) => {
            ApiError(StatusCode::BAD_REQUEST, format!("slug {m:?} is not usable")).into_response()
        }
        Err(crate::mcp::docs::WriteError::HtmlDocument) => ApiError(
            StatusCode::CONFLICT,
            "HTML documents must be replaced through import".into(),
        )
        .into_response(),
        Err(crate::mcp::docs::WriteError::Store(e)) => ApiError::from(e).into_response(),
    }
}

fn document_conflict_response(hash: String, body: String) -> Response {
    (
        StatusCode::PRECONDITION_FAILED,
        Json(json!({
            "error": { "message": "the document changed since it was read" },
            "hash": hash,
            "body": body,
        })),
    )
        .into_response()
}

pub async fn delete_doc(
    State(s): State<AppState>,
    Path((channel, slug)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let removed =
        crate::mcp::docs::delete_document(s.store(), s.manager.bus(), &s.node_id, &channel, &slug)?;
    if !removed {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no document {slug} on {channel}"),
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

// ---- work ledger ----

#[derive(Deserialize, Default)]
pub struct WorkQuery {
    pub channel: Option<String>,
    pub project_id: Option<String>,
    /// `ready`, `blocked`, `open` (ready + blocked), or `closed`; default all.
    pub state: Option<String>,
}

/// `GET /api/work?channel=&project_id=&state=`: the ledger with derived
/// readiness, in the deterministic order every node agrees on.
pub async fn list_work(
    State(s): State<AppState>,
    Query(q): Query<WorkQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    use tracon_sync::work::Readiness;
    let channel = q.channel.clone().ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "channel is required".into(),
    ))?;
    let items = s.store().work_status(&channel, q.project_id.as_deref())?;
    let items: Vec<_> = items
        .into_iter()
        .filter(|v| match q.state.as_deref() {
            Some("ready") => v.readiness == Readiness::Ready && v.session_id.is_none(),
            Some("blocked") => matches!(v.readiness, Readiness::Blocked { .. }),
            Some("open") => v.readiness != Readiness::Closed,
            Some("closed") => v.readiness == Readiness::Closed,
            _ => true,
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `GET /api/work/ready?channel=&project_id=`: what a session may pick.
pub async fn ready_work(
    State(s): State<AppState>,
    Query(q): Query<WorkQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let channel = q.channel.clone().ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "channel is required".into(),
    ))?;
    let items = s.store().work_ready(&channel, q.project_id.as_deref())?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct NewWorkBody {
    pub channel: String,
    #[serde(default)]
    pub project_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub discovered_from: Option<String>,
}

pub async fn add_work(
    State(s): State<AppState>,
    Json(w): Json<NewWorkBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let item = crate::corpus::work::create(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        crate::corpus::work::NewWork {
            channel: w.channel,
            project_id: w.project_id,
            title: w.title,
            body: w.body,
            deps: w.deps,
            priority: w.priority,
            discovered_from: w.discovered_from,
            discovered_by_session: None,
        },
    )
    .map_err(work_err)?;
    Ok(Json(json!(item)))
}

pub async fn get_work(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let item = s.store().work_get(&id)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        format!("no work item {id}"),
    ))?;
    let view = s
        .store()
        .work_status(&item.channel, None)?
        .into_iter()
        .find(|v| v.item.id == id);
    let sessions = s.store().sessions_of_work_item(&id)?;
    let discovered: Vec<_> = s
        .store()
        .work_list(&item.channel, None)?
        .into_iter()
        .filter(|w| w.discovered_from.as_deref() == Some(id.as_str()))
        .map(|w| json!({ "id": w.id, "title": w.title, "state": w.state }))
        .collect();
    Ok(Json(json!({
        "item": view,
        "sessions": sessions,
        "discovered": discovered,
    })))
}

/// `PUT /api/work/{id}` with any of title, body, deps, priority, state.
pub async fn put_work(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(patch): Json<crate::corpus::work::Patch>,
) -> ApiResult<Json<serde_json::Value>> {
    let closing = patch.state.as_deref() == Some(tracon_sync::work::CLOSED);
    let holder = if closing {
        s.store().session_holding(&id)?
    } else {
        None
    };
    let item = crate::corpus::work::update(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &id,
        patch,
        holder.as_ref().map(|h| h.id.as_str()),
    )
    .map_err(work_err)?;
    if let Some(h) = holder {
        s.manager.item_closed(&h.id, "closed by the operator").await;
    }
    Ok(Json(json!(item)))
}

pub async fn delete_work(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let removed = crate::corpus::work::remove(s.store(), s.manager.bus(), &s.node_id, &id)
        .map_err(work_err)?;
    if !removed {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no work item {id}"),
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

fn work_err(e: crate::corpus::work::WorkError) -> ApiError {
    use crate::corpus::work::WorkError::*;
    match e {
        Title => ApiError(StatusCode::BAD_REQUEST, e.to_string()),
        Missing(_) => ApiError(StatusCode::NOT_FOUND, e.to_string()),
        Store(e) => ApiError::from(e),
    }
}

#[derive(Deserialize, Default)]
pub struct MemoryQuery {
    pub channel: Option<String>,
    pub state: Option<String>,
    pub q: Option<String>,
    pub project_id: Option<String>,
}

/// `GET /api/memories?channel=&state=` lists; with `q=` it recalls.
pub async fn list_memories(
    State(s): State<AppState>,
    Query(q): Query<MemoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let channel = q.channel.clone().ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "channel is required".into(),
    ))?;
    if let Some(text) = q.q.as_deref().filter(|t| !t.trim().is_empty()) {
        let near = crate::embed::neighbours(
            &s.cfg,
            s.store(),
            &s.tools.http,
            s.manager.probe_token(),
            Some(&channel),
            text,
            20,
        )
        .await;
        let hits = s.store().recall_hybrid(
            &channel,
            text,
            q.project_id.as_deref(),
            None,
            None,
            20,
            &near.hits,
        )?;
        return Ok(Json(json!({ "hits": hits, "text_only": near.degraded })));
    }
    let rows = s.store().memory_list(&channel, q.state.as_deref(), 200)?;
    Ok(Json(json!({ "memories": rows })))
}

#[derive(Deserialize)]
pub struct NewMemory {
    pub channel: String,
    /// `directive` is the operator's kind; anything else is allowed too.
    #[serde(default = "directive")]
    pub kind: String,
    #[serde(default = "global")]
    pub scope: String,
    #[serde(default)]
    pub scope_ref: Option<String>,
    pub body: String,
    /// `active` unless the operator wants it to wait for a batch (`candidate`).
    #[serde(default = "active")]
    pub state: String,
    #[serde(default = "one")]
    pub confidence: f64,
}
fn active() -> String {
    "active".into()
}
fn one() -> f64 {
    1.0
}
fn directive() -> String {
    "directive".into()
}
fn global() -> String {
    "global".into()
}

/// `POST /api/memories`: the operator writes a memory, a directive by default.
pub async fn add_memory(
    State(s): State<AppState>,
    Json(m): Json<NewMemory>,
) -> ApiResult<Json<serde_json::Value>> {
    if m.body.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "body is required".into()));
    }
    let id = crate::corpus::new_id();
    let now = crate::store::now_ms();
    crate::corpus::write(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &m.channel,
        "memory",
        tracon_sync::ChangeOp::Upsert,
        &id,
        json!({
            "channel": m.channel, "scope": m.scope, "scope_ref": m.scope_ref, "kind": m.kind, "body": m.body.trim(),
            "source_session": null, "source_node": s.node_id, "confidence": m.confidence, "state": m.state,
            "created_ms": now, "updated_ms": now,
        }),
    )?;
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_memory(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let row = s
        .store()
        .memory_get(&id)?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such memory".into()))?;
    crate::corpus::write(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &row.channel,
        "memory",
        tracon_sync::ChangeOp::Delete,
        &id,
        serde_json::Value::Null,
    )?;
    Ok(Json(json!({ "ok": true })))
}

// ---- providers ----

fn providers_of(s: &AppState) -> ApiResult<Arc<crate::providers::Providers>> {
    s.manager.providers().cloned().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "providers are not available on this node".into(),
    ))
}

fn provider_err(e: crate::providers::ProviderError) -> ApiError {
    use crate::providers::ProviderError::*;
    let status = match &e {
        Unknown(_) => StatusCode::NOT_FOUND,
        NoLogin(_)
        | RequiresLocalCallback(_)
        | NotPending(_)
        | Busy(_)
        | WrongOwner
        | RemoteDisconnect => StatusCode::CONFLICT,
        Failed(_) => StatusCode::BAD_GATEWAY,
    };
    ApiError(status, e.to_string())
}

/// What the broker holds: names, kinds, bindings, env key names. Never a
/// value — the response shape is the broker's `summaries`, which cannot
/// carry one.
pub async fn list_credentials(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let b = s.tools.broker.read().unwrap();
    Ok(Json(json!({ "credentials": b.summaries() })))
}

struct ForgeCredential {
    name: &'static str,
    token_key: &'static str,
    alias_key: &'static str,
}

fn forge_credential(forge: &str) -> ApiResult<ForgeCredential> {
    match forge {
        "github" => Ok(ForgeCredential {
            name: "gh",
            token_key: "GH_TOKEN",
            alias_key: "GITHUB_TOKEN",
        }),
        "gitlab" => Ok(ForgeCredential {
            name: "glab",
            token_key: "GITLAB_TOKEN",
            alias_key: "GLAB_TOKEN",
        }),
        _ => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no forge credential route for {forge}"),
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeCredentialBody {
    token: String,
    channels: Vec<String>,
}

pub async fn put_forge_credential(
    State(s): State<AppState>,
    Path(forge): Path<String>,
    Json(body): Json<ForgeCredentialBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let forge = forge_credential(&forge)?;
    let token = body.token.trim();
    if token.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "token must not be empty".into(),
        ));
    }
    let mut channels = body.channels;
    channels.sort();
    channels.dedup();
    if channels.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "choose at least one channel".into(),
        ));
    }
    let existing_channels = s
        .store()
        .channel_list()?
        .into_iter()
        .map(|channel| channel.name)
        .filter(|name| !name.starts_with('@'))
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(missing) = channels
        .iter()
        .find(|channel| !existing_channels.contains(*channel))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("no visible channel named {missing}"),
        ));
    }

    let key = crate::mesh::identity::load_or_generate()
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .0
        .credential_store_key();
    let mut broker = s.tools.broker.write().unwrap();
    let mut staged = broker.clone();
    let mut credential = if let Some(existing) = staged.get(forge.name).cloned() {
        if existing.kind != crate::broker::KIND_ENV {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "credential {} is not an environment credential and cannot be replaced here",
                    forge.name
                ),
            ));
        }
        existing
    } else {
        crate::broker::Credential::default()
    };
    credential.kind = crate::broker::KIND_ENV.into();
    credential.channels = channels;
    credential.provider = None;
    credential.expires_ms = None;
    credential.identity = None;
    credential.env.remove(forge.alias_key);
    credential
        .env
        .insert(forge.token_key.into(), token.to_string());
    staged.put(forge.name, credential);
    staged
        .save(&key)
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    *broker = staged;
    Ok(Json(json!({ "saved": forge.name })))
}

pub async fn delete_forge_credential(
    State(s): State<AppState>,
    Path(forge): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let forge = forge_credential(&forge)?;
    let key = crate::mesh::identity::load_or_generate()
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .0
        .credential_store_key();
    let mut broker = s.tools.broker.write().unwrap();
    let mut staged = broker.clone();
    if !staged.remove(forge.name) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no credential named {}", forge.name),
        ));
    }
    staged
        .save(&key)
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    *broker = staged;
    Ok(Json(json!({ "removed": forge.name })))
}

#[derive(Deserialize)]
pub struct ShareBody {
    to: String,
}

/// Hand one credential to another member, direct-sealed over the hub. The
/// operator sharing from the interface is the explicit widening of the node
/// pin the CLI refuses to do implicitly: the target is added to `nodes` (and
/// this node too, where the list was empty and meant "here only"), sealed to
/// the store, and the handoff queued through the outbox.
pub async fn share_credential(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<ShareBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let to = body.to;
    if to == s.node_id {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "that credential is already here".into(),
        ));
    }
    let mesh = s.mesh.as_ref().ok_or(ApiError(
        StatusCode::CONFLICT,
        "no hub configured; there is nobody to share with".into(),
    ))?;
    let node = s.store().get_node(&to)?.ok_or(ApiError(
        StatusCode::NOT_FOUND,
        format!("no node {to} in this mesh"),
    ))?;
    if node.x25519_pub.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "{} has not said hello yet; nothing can be sealed to it",
                node.name
            ),
        ));
    }
    let identity = crate::mesh::identity::load_or_generate()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .0;
    let rows = {
        let mut broker = s.tools.broker.write().unwrap();
        let mut cred = broker.get(&name).cloned().ok_or(ApiError(
            StatusCode::NOT_FOUND,
            format!("no credential named {name}"),
        ))?;
        if !cred.nodes.iter().any(|n| n == &to) {
            // An empty list means "the node holding the file": pinning the
            // target without also pinning this node would lock it out here.
            if cred.nodes.is_empty() {
                cred.nodes.push(s.node_id.clone());
            }
            cred.nodes.push(to.clone());
            broker.put(&name, cred.clone());
            broker
                .save(&identity.credential_store_key())
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        crate::broker::Broker::handoff_rows(&[(name.clone(), cred)])
    };
    mesh.send_credential_handoff(&to, rows)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "shared": name, "to": to })))
}

pub async fn list_providers(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    match s.manager.providers() {
        Some(p) => Ok(Json(json!(p.list_private()))),
        None => Ok(Json(json!(providers_json(&s)))),
    }
}

pub struct ObservedPeer(Option<std::net::SocketAddr>);

impl<S> axum::extract::FromRequestParts<S> for ObservedPeer
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|ConnectInfo(address)| *address),
        ))
    }
}

#[derive(Deserialize, Default)]
pub struct ConnectBody {
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub local_callback: bool,
}

/// Start the harness's login for a provider; the response carries the URL to
/// open. The card then takes the paste-back.
pub async fn connect_provider(
    State(s): State<AppState>,
    ObservedPeer(peer): ObservedPeer,
    Path(name): Path<String>,
    body: Option<Json<ConnectBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let local_callback = callback_allowed(&s, peer, body.local_callback);
    connect_local(&s, &name, body.channels, local_callback)
        .await
        .map(Json)
}
fn callback_allowed(s: &AppState, peer: Option<std::net::SocketAddr>, requested: bool) -> bool {
    requested
        && peer.is_some_and(|address| address.ip().is_loopback())
        && s.cfg.runtime.kind == crate::config::RuntimeKind::Podman
}

async fn connect_local(
    s: &AppState,
    name: &str,
    channels: Vec<String>,
    local_callback: bool,
) -> ApiResult<serde_json::Value> {
    let result = providers_of(s)?
        .connect(
            name,
            channels,
            crate::providers::LoginOwner::Local,
            local_callback,
        )
        .await
        .map_err(provider_err)?;
    serde_json::to_value(result).map_err(|error| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("provider response: {error}"),
        )
    })
}

#[derive(Deserialize)]
pub struct CodeBody {
    pub code: String,
}

pub async fn provider_code(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<CodeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    code_local(&s, &name, &body.code).await.map(Json)
}

async fn code_local(s: &AppState, name: &str, code: &str) -> ApiResult<serde_json::Value> {
    providers_of(s)?
        .code(name, code, &crate::providers::LoginOwner::Local)
        .await
        .map_err(provider_err)?;
    Ok(json!({ "ok": true }))
}

pub async fn disconnect_provider(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    disconnect_local(&s, &name).await.map(Json)
}

async fn disconnect_local(s: &AppState, name: &str) -> ApiResult<serde_json::Value> {
    providers_of(s)?
        .disconnect(name, &crate::providers::LoginOwner::Local)
        .await
        .map_err(provider_err)?;
    Ok(json!({ "ok": true }))
}

/// The login URL scrape inside a connect can take up to a minute on the
/// owner, so its command gets more rope than `[mesh] command_timeout_secs`.
const PROVIDER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Run a provider command on `node_id`: here when it is this node, sealed to
/// the owner otherwise. The owner's refusal comes back as it phrased it.
async fn peer_command(
    s: &AppState,
    node_id: &str,
    command: proto::frame::Command,
    timeout: std::time::Duration,
) -> ApiResult<serde_json::Value> {
    use crate::mesh::forward::CommandError;
    let mesh = s.mesh.as_ref().ok_or(ApiError(
        StatusCode::CONFLICT,
        "no hub configured; this node cannot reach peers".into(),
    ))?;
    if !mesh.peer_reachable(node_id) {
        return Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "that node is unreachable; try again when it returns".into(),
        ));
    }
    mesh.command(node_id, command, timeout)
        .await
        .map_err(|e| match e {
            CommandError::Timeout => ApiError(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
            CommandError::Refused(m) => ApiError(StatusCode::CONFLICT, m),
            CommandError::Local(m) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, m),
        })
}

pub async fn node_connect_provider(
    State(s): State<AppState>,
    ObservedPeer(peer): ObservedPeer,
    Path((node_id, name)): Path<(String, String)>,
    body: Option<Json<ConnectBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    if node_id == s.node_id {
        let local_callback = callback_allowed(&s, peer, body.local_callback);
        return connect_local(&s, &name, body.channels, local_callback)
            .await
            .map(Json);
    }
    peer_command(
        &s,
        &node_id,
        proto::frame::Command::ProviderConnect {
            name,
            channels: body.channels,
        },
        PROVIDER_CONNECT_TIMEOUT,
    )
    .await
    .map(Json)
}

pub async fn node_provider_code(
    State(s): State<AppState>,
    Path((node_id, name)): Path<(String, String)>,
    Json(body): Json<CodeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if node_id == s.node_id {
        return code_local(&s, &name, &body.code).await.map(Json);
    }
    let timeout = std::time::Duration::from_secs(s.cfg.mesh.command_timeout_secs.max(1));
    peer_command(
        &s,
        &node_id,
        proto::frame::Command::ProviderCode {
            name,
            code: body.code,
        },
        timeout,
    )
    .await
    .map(Json)
}

pub async fn node_disconnect_provider(
    State(s): State<AppState>,
    Path((node_id, name)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    if node_id == s.node_id {
        return disconnect_local(&s, &name).await.map(Json);
    }
    let timeout = std::time::Duration::from_secs(s.cfg.mesh.command_timeout_secs.max(1));
    peer_command(
        &s,
        &node_id,
        proto::frame::Command::ProviderDisconnect { name },
        timeout,
    )
    .await
    .map(Json)
}

/// Each configured provider and whether a credential for it is usable here.
pub(crate) fn providers_json(s: &AppState) -> Vec<serde_json::Value> {
    let broker = s.tools.broker.read().unwrap();
    s.cfg
        .providers
        .keys()
        .map(|name| {
            let cred = broker.model_credential_for(name, &s.node_id);
            json!({
                "name": name,
                "state": if cred.is_some() { "connected" } else { "disconnected" },
                "identity": cred.and_then(|(_, c)| c.identity.clone()),
                "expires_ms": cred.and_then(|(_, c)| c.expires_ms),
            })
        })
        .collect()
}

/// Probe the harness for its model list through the gateway and record it on
/// this node's row. Skipped when no model credential is usable here: the
/// harness would list its catalogue, but nothing could be run against it.
pub async fn probe_models_into_store(
    s: &AppState,
    backend: &dyn crate::boundary::Backend,
) -> Result<Vec<crate::adapter::ModelOption>, String> {
    let Ok(Some(node)) = s.store().get_node(&s.node_id) else {
        return Err("node row missing".into());
    };
    if node.state != "ready" {
        return Err("node is refused; no probe".into());
    }
    if !s
        .tools
        .broker
        .read()
        .unwrap()
        .has_model_credential(&s.node_id)
    {
        tracing::warn!(
            "no model credential on this node; connect a provider on the Nodes screen. \
             Sessions cannot start until one is connected."
        );
        return Err("no model credential".into());
    }
    let wiring = crate::gateway::model::harness_wiring(
        &s.cfg,
        &backend.harness_host(),
        s.manager.probe_token(),
    );
    let scratch = crate::session::materialize::probe_scratch(
        &backend.harness_home(),
        s.adapter.as_ref(),
        &wiring,
    )
    .map_err(|e| e.to_string())?;
    backend
        .import_volume(&scratch.volume, &scratch.dir)
        .await
        .map_err(|e| e.to_string())?;
    let runner = backend.runner(scratch.mounts);
    let models = s
        .adapter
        .probe_models(runner.as_ref(), wiring.env)
        .await
        .map_err(|e| e.to_string())?;
    if let Ok(Some(mut node)) = s.store().get_node(&s.node_id) {
        node.models_json = serde_json::to_string(&models).ok();
        let _ = s.store().put_node(&node);
        // Push the refreshed node to any live client, so a model list (or a
        // refused state) reaches the interface without a reload.
        if let Ok(v) = node_json(s) {
            s.manager.bus().publish(crate::stream::Frame::Node(v));
        }
    }
    tracing::info!(models = models.len(), "model list probed");
    Ok(models)
}

/// Re-probe the harness for its model list, for when the probe failed at
/// startup or a provider was connected since.
pub async fn refresh_models(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let backend = s.manager.backend().clone();
    let models = probe_models_into_store(&s, backend.as_ref())
        .await
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(serde_json::json!(models)))
}

/// `GET /api/usage?channel=&since_ms=`: what the gateway counted. Defaults to
/// the last 24 hours across every channel.
pub async fn usage(
    State(s): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let since = q
        .get("since_ms")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or_else(|| crate::store::now_ms() - 24 * 3600 * 1000);
    let totals = s
        .store()
        .usage_since(q.get("channel").map(String::as_str), since)?;
    Ok(Json(json!({ "since_ms": since, "totals": totals })))
}

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

// ---- enrollment, for the "Enroll a new node" screen ----

fn mesh_or_conflict(s: &AppState) -> ApiResult<Arc<crate::mesh::client::MeshClient>> {
    s.mesh.clone().ok_or(ApiError(
        StatusCode::CONFLICT,
        "no hub configured on this node; run tracon mesh init or tracon enroll first".into(),
    ))
}

fn enroll_err(e: crate::mesh::enroll::EnrollError) -> ApiError {
    use crate::mesh::enroll::EnrollError::*;
    match e {
        Transport(m) => ApiError(StatusCode::BAD_GATEWAY, format!("hub unreachable: {m}")),
        Refused { status, body } => ApiError(
            StatusCode::BAD_GATEWAY,
            format!("hub refused ({status}): {body}"),
        ),
        Local(m) => ApiError(StatusCode::CONFLICT, m),
    }
}

#[derive(Deserialize)]
pub struct InviteBody {
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

fn invite_json(inv: &crate::mesh::enroll::Invite) -> serde_json::Value {
    json!({
        "code": inv.code,
        "display_code": inv.display_code(),
        "url": inv.url,
        "qr_svg": crate::mesh::enroll::qr_svg(&inv.url),
        "channels": inv.channels,
        "expires_at": inv.expires_at,
        "state": if inv.admitted { "admitted" } else if inv.received.is_some() { "received" } else { "waiting" },
        "received": inv.received,
        "received_fingerprint": inv.received_fingerprint(),
        "own_fingerprint": proto::enroll::fingerprint_hex(&s_node_id_placeholder()),
    })
}

fn s_node_id_placeholder() -> String {
    String::new()
}

pub async fn open_invite(
    State(s): State<AppState>,
    Json(b): Json<InviteBody>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let mesh = mesh_or_conflict(&s)?;
    let inv =
        crate::mesh::enroll::open_invite(mesh.identity(), mesh.hub_url(), &b.channels, b.ttl_secs)
            .await
            .map_err(enroll_err)?;
    let mut v = invite_json(&inv);
    v["own_fingerprint"] = json!(proto::enroll::fingerprint_hex(&s.node_id));
    mesh.invites().lock().unwrap().insert(inv.code.clone(), inv);
    Ok((StatusCode::CREATED, Json(v)))
}

pub async fn poll_invite(
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let mesh = mesh_or_conflict(&s)?;
    let code = proto::enroll::normalize_code(&code)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "malformed code".into()))?;
    let mut inv = mesh
        .invites()
        .lock()
        .unwrap()
        .get(&code)
        .cloned()
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such invitation".into()))?;
    if inv.received.is_none() {
        if let Some(req) = crate::mesh::enroll::poll_invite(mesh.identity(), mesh.hub_url(), &code)
            .await
            .map_err(enroll_err)?
        {
            inv.received = Some(req);
            mesh.invites()
                .lock()
                .unwrap()
                .insert(code.clone(), inv.clone());
        }
    }
    let mut v = invite_json(&inv);
    v["own_fingerprint"] = json!(proto::enroll::fingerprint_hex(&s.node_id));
    Ok(Json(v))
}

/// The operator compared fingerprints and said they match.
pub async fn admit_invite(
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let mesh = mesh_or_conflict(&s)?;
    let code = proto::enroll::normalize_code(&code)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "malformed code".into()))?;
    let inv = mesh
        .invites()
        .lock()
        .unwrap()
        .get(&code)
        .cloned()
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such invitation".into()))?;
    let req = inv.received.clone().ok_or(ApiError(
        StatusCode::CONFLICT,
        "the other node has not answered yet".into(),
    ))?;
    let handoff = s.tools.broker.read().unwrap().bound_to(&req.node_id);
    crate::mesh::enroll::admit(
        s.store(),
        mesh.identity(),
        mesh.hub_url(),
        &req.node_id,
        &req.x25519_pub,
        &req.name,
        &inv.channels,
        &handoff,
    )
    .await
    .map_err(enroll_err)?;
    let mut done = inv.clone();
    done.admitted = true;
    mesh.invites().lock().unwrap().insert(code, done.clone());
    if let Ok(Some(row)) = s.store().get_node(&req.node_id) {
        s.manager
            .bus()
            .publish_untapped(crate::stream::Frame::Node(row.to_json()));
    }
    Ok(Json(invite_json(&done)))
}

pub async fn cancel_invite(
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> ApiResult<StatusCode> {
    let mesh = mesh_or_conflict(&s)?;
    let code = proto::enroll::normalize_code(&code)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "malformed code".into()))?;
    mesh.invites().lock().unwrap().remove(&code);
    let _ = crate::mesh::enroll::cancel_invite(mesh.identity(), mesh.hub_url(), &code).await;
    Ok(StatusCode::NO_CONTENT)
}

fn valid_operator_notification(title: &str, body: &str, path: &str, device_ids: &[String]) -> bool {
    !title.trim().is_empty()
        && !body.trim().is_empty()
        && title.len() <= 8 * 1024
        && body.len() <= 8 * 1024
        && device_ids.len() <= 32
        && device_ids
            .iter()
            .all(|id| !id.is_empty() && id.len() <= 256)
        && path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.contains('%')
        && !path.split('/').any(|part| part == "..")
}

/// Commands other nodes forward to this one run exactly as local requests do.
#[async_trait::async_trait]
impl crate::mesh::forward::CommandExecutor for AppState {
    async fn execute(
        &self,
        sender: &str,
        command: proto::frame::Command,
    ) -> Result<serde_json::Value, String> {
        use proto::frame::Command as C;
        let r: Result<serde_json::Value, ApiError> = match command {
            C::Create { spec } => {
                let spec: NewSession = serde_json::from_value(spec).map_err(|e| e.to_string())?;
                self.manager
                    .create(spec, self.adapter.clone())
                    .await
                    .map(|row| json!(row))
                    .map_err(Into::into)
            }
            C::Prompt { session_id, text } => self
                .manager
                .prompt(&session_id, text)
                .await
                .map(|_| json!({ "accepted": true }))
                .map_err(Into::into),
            C::Answer {
                permission_id,
                option_id,
                arguments,
            } => self
                .manager
                .answer(&permission_id, option_id, arguments)
                .await
                .map(|_| json!({ "answered": true }))
                .map_err(Into::into),
            C::Kill { session_id } => self
                .manager
                .stop(&session_id)
                .await
                .map(|_| json!({ "stopped": true }))
                .map_err(Into::into),
            C::Pause { session_id, reason } => self
                .manager
                .pause(&session_id, reason)
                .await
                .map(|_| json!({ "paused": true }))
                .map_err(Into::into),
            C::Resume { session_id, reason } => self
                .manager
                .resume(&session_id, reason)
                .await
                .map(|_| json!({ "resumed": true }))
                .map_err(Into::into),
            C::Verdict {
                review_id,
                verdict,
                reason,
                title,
                body,
                patch,
            } => {
                decide_local(
                    self,
                    &review_id,
                    VerdictBody {
                        verdict,
                        reason,
                        title,
                        body,
                        patch,
                    },
                )
                .await
            }
            // The provider commands run exactly what a local request would:
            // the login subprocess, its stdin, and the lifted credential all
            // stay on this node. Only the URL and the ack travel.
            C::ProviderConnect { name, channels } => match providers_of(self) {
                Ok(providers) => providers
                    .connect(
                        &name,
                        channels,
                        crate::providers::LoginOwner::Peer(sender.to_string()),
                        false,
                    )
                    .await
                    .and_then(|result| {
                        serde_json::to_value(result).map_err(|error| {
                            crate::providers::ProviderError::Failed(error.to_string())
                        })
                    })
                    .map_err(provider_err),
                Err(error) => Err(error),
            },
            C::ProviderCode { name, code } => match providers_of(self) {
                Ok(providers) => providers
                    .code(
                        &name,
                        &code,
                        &crate::providers::LoginOwner::Peer(sender.to_string()),
                    )
                    .await
                    .map(|_| json!({ "ok": true }))
                    .map_err(provider_err),
                Err(error) => Err(error),
            },
            C::ProviderDisconnect { name } => match providers_of(self) {
                Ok(providers) => providers
                    .disconnect(
                        &name,
                        &crate::providers::LoginOwner::Peer(sender.to_string()),
                    )
                    .await
                    .map(|_| json!({ "ok": true }))
                    .map_err(provider_err),
                Err(error) => Err(error),
            },
            C::OperatorNotify {
                channel,
                notification_id,
                title,
                body,
                path,
                device_ids,
            } => {
                let members = self
                    .store()
                    .nodes_in_channel(&channel)
                    .map_err(|e| e.to_string())?;
                if !members.contains(&sender.to_string()) || !members.contains(&self.node_id) {
                    Err(ApiError(
                        StatusCode::FORBIDDEN,
                        "sender is not a member of this notification channel".into(),
                    ))
                } else if !valid_operator_notification(&title, &body, &path, &device_ids) {
                    Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        "invalid operator notification".into(),
                    ))
                } else {
                    let bindings = self
                        .store()
                        .channel_get(&channel)
                        .map_err(|e| e.to_string())?
                        .and_then(|row| {
                            serde_json::from_str::<serde_json::Value>(&row.bindings_json).ok()
                        })
                        .unwrap_or_else(|| json!({}));
                    if !crate::notify::enabled(&bindings) {
                        Err(ApiError(
                            StatusCode::CONFLICT,
                            "notifications are disabled for this channel".into(),
                        ))
                    } else if !self
                        .store()
                        .claim_operator_notification(
                            &format!("remote:{sender}:{notification_id}"),
                            &notification_id,
                            60_000,
                        )
                        .map_err(|e| e.to_string())?
                    {
                        Ok(json!({ "deduplicated": true }))
                    } else if !self
                        .store()
                        .claim_operator_notification_rate(&channel, sender, 10, 60_000)
                        .map_err(|e| e.to_string())?
                    {
                        let _ = self.store().release_operator_notification(&notification_id);
                        Err(ApiError(
                            StatusCode::TOO_MANY_REQUESTS,
                            "operator notification rate limit exceeded".into(),
                        ))
                    } else {
                        let attempts = crate::notify::send_operator(
                            self.store(),
                            &self.cfg,
                            &notification_id,
                            title,
                            body,
                            path,
                            &device_ids,
                        )
                        .await;
                        Ok(json!({ "deduplicated": false, "attempts": attempts }))
                    }
                }
            }
        };
        r.map_err(|e| e.1)
    }
}

// ---------------------------------------------------------------------------
// Settings: standing a node up without a shell on it.
// ---------------------------------------------------------------------------

/// Re-run the boundary checks and record the verdict.
///
/// The refusal is a snapshot taken at startup, and an operator who has just
/// started the runtime or run setup should not have to restart the node to be
/// believed. Publishing the node frame is what clears the banner live.
pub async fn recheck_boundary(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let backend = s.manager.backend().clone();
    let report = backend.check_all(&s.cfg, false).await;
    let row = super::verify_node(
        s.store(),
        &s.cfg,
        s.adapter.as_ref(),
        backend.as_ref(),
        &s.node_id,
        None,
    )
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    s.manager
        .bus()
        .publish(crate::stream::Frame::Node(node_json(&s)?));
    Ok(Json(json!({ "state": row.state, "checks": report })))
}

#[derive(Deserialize)]
pub struct SetupBody {
    #[serde(default)]
    pub rebuild: bool,
}

/// Build the network, gateway and images this node's boundary needs, then
/// re-check. Blocking: image builds are minutes, not seconds, and there is
/// nothing useful to show mid-flight that the verdict does not say better.
pub async fn run_setup(
    State(s): State<AppState>,
    body: Option<Json<SetupBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let rebuild = body.map(|b| b.rebuild).unwrap_or(false);
    let backend = s.manager.backend().clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(600),
        backend.setup(&s.cfg, rebuild),
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "setup is still running after ten minutes; check the node's log".into(),
        )
    })?
    .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?;
    recheck_boundary(State(s)).await
}

#[derive(Deserialize)]
pub struct ChannelBody {
    pub name: String,
}

/// Create a channel: mint its key here and record that this node holds it.
pub async fn create_channel(
    State(s): State<AppState>,
    Json(body): Json<ChannelBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let identity = crate::mesh::identity::load_or_generate()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .0;
    let (created, note) =
        crate::mesh::channels::create_and_sync(s.store(), &identity, &body.name, &s.cfg)
            .await
            .map_err(|e| match e {
                crate::mesh::channels::ChannelError::Name => {
                    ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
                }
                other => ApiError(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
            })?;
    Ok(Json(
        json!({ "name": body.name, "created": created.minted, "note": note }),
    ))
}

#[derive(Deserialize)]
pub struct ImportBody {
    pub toml: String,
}

/// Seal credentials handed over as text. The paste is the same secret the
/// file would have held, and it is never written to disk in the clear.
pub async fn import_credentials(
    State(s): State<AppState>,
    Json(body): Json<ImportBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let parsed = crate::broker::Broker::parse_text(&body.toml)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    if parsed.is_empty() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no credentials in that; expected [credentials.<name>] tables".into(),
        ));
    }
    let identity = crate::mesh::identity::load_or_generate()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .0;
    let names: Vec<String> = {
        let mut broker = s.tools.broker.write().unwrap();
        let mut names = Vec::new();
        for (name, cred) in parsed.iter() {
            broker.put(name, cred.clone());
            names.push(name.to_string());
        }
        broker
            .save(&identity.credential_store_key())
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        names
    };
    // Names only: a value that went in never comes back out.
    Ok(Json(json!({ "imported": names })))
}

/// The configuration this interface writes.
pub async fn get_config(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let cfg = Config::try_load().map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let mut v = super::settings::config_view(&cfg);
    // What is running, so the pane can say a change is owed a restart rather
    // than pretend it already took.
    v["running"] = json!({
        "harness_id": s.cfg.harness.id,
        "harness_version": s.cfg.harness.version,
        "node_name": s.cfg.node_name,
    });
    Ok(Json(v))
}

/// Write the configuration. Loopback only: these keys decide which binaries
/// the node executes.
pub async fn put_config(
    _: super::auth::Loopback,
    Json(patch): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    // try_load, never load: a node.toml that does not parse must be reported,
    // not silently replaced with defaults plus this patch.
    let mut cfg = Config::try_load().map_err(|e| {
        ApiError(
            StatusCode::CONFLICT,
            format!("node.toml does not parse, so it will not be rewritten: {e}"),
        )
    })?;
    let changed = super::settings::apply(&mut cfg, &patch)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    if !changed.is_empty() {
        cfg.save()
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({
        "changed": changed,
        // The running node holds its config in an Arc taken at startup.
        "restart_required": !changed.is_empty(),
    })))
}

#[derive(Deserialize)]
pub struct HubBody {
    pub hub_url: String,
}

fn validate_hub_base(value: &str) -> ApiResult<String> {
    let trimmed = value.trim();
    let url = reqwest::Url::parse(trimmed).map_err(|_| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hub URL must be a valid HTTPS URL".into(),
        )
    })?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hub URL must have a host and no userinfo, query, or fragment".into(),
        ));
    }
    let literal_loopback = trimmed
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|authority| {
            ["localhost", "127.0.0.1", "[::1]"].iter().any(|host| {
                authority == *host
                    || authority
                        .strip_prefix(host)
                        .is_some_and(|suffix| suffix.starts_with(':'))
            })
        });
    if url.scheme() != "https" && !(url.scheme() == "http" && literal_loopback) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hub URL must use HTTPS; HTTP is allowed only on loopback".into(),
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

/// The first node: mint the mesh channel here and point this node at a hub.
/// Loopback only — this decides which hub the node trusts.
pub async fn mesh_init(
    _: super::auth::Loopback,
    State(s): State<AppState>,
    Json(body): Json<HubBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let hub = validate_hub_base(&body.hub_url)?;
    let identity = crate::mesh::identity::load_or_generate()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .0;
    crate::mesh::channels::create(s.store(), &identity, proto::frame::MESH_CHANNEL)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut cfg = Config::try_load().map_err(|e| {
        ApiError(
            StatusCode::CONFLICT,
            format!("node.toml does not parse, so it will not be rewritten: {e}"),
        )
    })?;
    cfg.mesh.hub_url = Some(hub.clone());
    cfg.save()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({
        "hub_url": hub,
        "node_id": identity.node_id(),
        // The hub only talks to a node it has been told to admit, and this
        // node only dials out once it restarts with the new config.
        "admit_with": format!("TRACON_HUB_ADMIT={}", identity.node_id()),
        "restart_required": true,
    })))
}

/// `POST /api/mesh/unpair`: forget the hub. The mesh channel and this node's
/// keys stay, so pairing again is the join it was the first time; the hub
/// still lists this node until `tracon mesh remove` is run there. Takes
/// effect on restart, like every `node.toml` change.
pub async fn mesh_unpair(
    _: super::auth::Loopback,
    State(_): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut cfg = Config::try_load().map_err(|e| {
        ApiError(
            StatusCode::CONFLICT,
            format!("node.toml does not parse, so it will not be rewritten: {e}"),
        )
    })?;
    let was = cfg.mesh.hub_url.take();
    if was.is_some() {
        cfg.save()
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({
        "unpaired": was,
        "restart_required": was.is_some(),
    })))
}

#[derive(Deserialize)]
pub struct QrBody {
    pub text: String,
}

/// Render a QR for text the client already holds. The operator token is minted
/// in the browser and never sent here; this draws the login URL that carries
/// it in a fragment, which is the same thing the CLI prints.
pub async fn qr(Json(body): Json<QrBody>) -> ApiResult<Json<serde_json::Value>> {
    if body.text.len() > 2048 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too long to encode as a QR".into(),
        ));
    }
    Ok(Json(
        json!({ "svg": crate::mesh::enroll::qr_svg(&body.text) }),
    ))
}

/// An enrolment in flight, so the interface can watch one it started.
///
/// Joining a mesh waits on a human at the other end confirming a fingerprint,
/// which is minutes, not seconds. One slot: a node joins one mesh.
#[derive(Default)]
pub struct EnrollJob {
    pub lines: Vec<String>,
    pub done: bool,
    pub error: Option<String>,
    pub channels: Vec<String>,
}

/// Collects `enroll::accept`'s progress for the interface to poll.
struct BufProgress(Arc<std::sync::Mutex<EnrollJob>>);

impl crate::mesh::enroll::Progress for BufProgress {
    fn say(&self, line: &str) {
        if let Ok(mut job) = self.0.lock() {
            job.lines.push(line.to_string());
        }
    }
}

#[derive(Deserialize)]
pub struct EnrollBody {
    pub invitation: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Join a mesh from an invitation. Loopback only: this sets the trust root.
pub async fn start_enroll(
    _: super::auth::Loopback,
    State(s): State<AppState>,
    Json(body): Json<EnrollBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let (hub_from_url, code) = proto::enroll::parse_invite(&body.invitation).ok_or(ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        "that is not an invitation URL or code".into(),
    ))?;
    let hub = hub_from_url.ok_or(ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        "paste the full invitation URL: a bare code does not say which hub".into(),
    ))?;
    let hub = validate_hub_base(&hub)?;
    {
        let job = s.enroll.lock().unwrap();
        if !job.lines.is_empty() && !job.done {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "an enrolment is already running".into(),
            ));
        }
    }
    let identity = crate::mesh::identity::load_or_generate()
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .0;
    let name = body.name.unwrap_or_else(|| s.cfg.node_name.clone());
    *s.enroll.lock().unwrap() = EnrollJob {
        lines: vec![format!("joining {hub}")],
        ..Default::default()
    };

    let slot = s.enroll.clone();
    let store = s.store().clone();
    tokio::spawn(async move {
        let facts = format!("{} {}", std::env::consts::ARCH, std::env::consts::OS);
        let progress = BufProgress(slot.clone());
        let outcome = crate::mesh::enroll::accept(
            store,
            &identity,
            &hub,
            &code,
            &name,
            &facts,
            std::time::Duration::from_secs(600),
            &progress,
        )
        .await;
        let mut job = slot.lock().unwrap();
        match outcome {
            Ok(channels) => {
                // The hub is written only once the mesh has actually taken
                // this node: a config pointing at a hub that refused it is
                // worse than no config at all.
                match Config::try_load() {
                    Ok(mut cfg) => {
                        cfg.mesh.hub_url = Some(hub.clone());
                        if let Err(e) = cfg.save() {
                            job.error =
                                Some(format!("enrolled, but node.toml was not written: {e}"));
                        }
                    }
                    Err(e) => {
                        job.error = Some(format!("enrolled, but node.toml does not parse: {e}"))
                    }
                }
                job.lines
                    .push("enrolled; restart the node to dial the hub".into());
                job.channels = channels;
            }
            Err(e) => job.error = Some(e.to_string()),
        }
        job.done = true;
    });
    Ok(Json(json!({ "started": true })))
}

/// Where an enrolment has got to.
pub async fn enroll_status(
    _: super::auth::Loopback,
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let job = s.enroll.lock().unwrap();
    Ok(Json(json!({
        "lines": job.lines,
        "done": job.done,
        "error": job.error,
        "channels": job.channels,
        "restart_required": job.done && job.error.is_none(),
    })))
}

#[cfg(test)]
mod hub_url_tests {
    use super::validate_hub_base;

    #[test]
    fn allows_https_and_literal_http_loopback_with_base_paths() {
        for (input, normalized) in [
            (
                "https://hub.example.com/team/",
                "https://hub.example.com/team",
            ),
            ("http://localhost:7443/team/", "http://localhost:7443/team"),
            ("http://127.0.0.1:7443", "http://127.0.0.1:7443"),
            ("http://[::1]:7443/team", "http://[::1]:7443/team"),
        ] {
            let Ok(actual) = validate_hub_base(input) else {
                panic!("rejected {input}");
            };
            assert_eq!(actual, normalized);
        }
    }

    #[test]
    fn rejects_nonliteral_or_nonloopback_http_hosts() {
        for input in [
            "http://hub.example.com",
            "http://2130706433",
            "http://127.1",
            "http://LOCALHOST",
        ] {
            assert!(validate_hub_base(input).is_err(), "accepted {input}");
        }
    }
}
