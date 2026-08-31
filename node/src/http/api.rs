//! The operator API. Small on purpose: the interface reads snapshots and the
//! stream, and writes through a handful of commands.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    adapter::HarnessAdapter,
    config::Config,
    session::{Manager, NewSession, SessionError},
    store::Store,
};

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
        for name in ["personal", "work"] {
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
}

/// The operator's repositories on every forge whose credential this channel
/// may use. A forge with no credential at all is absent; one this channel is
/// not bound to answers with the refusal, per forge, so one dead forge never
/// hides another's list.
pub async fn forge_repos(
    State(s): State<AppState>,
    Query(q): Query<ForgeQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let out =
        crate::forge::list_repos(&s.tools.http, &s.tools.broker, &q.channel, &s.node_id).await;
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

pub async fn create_session(
    State(s): State<AppState>,
    Json(spec): Json<NewSession>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let row = s.manager.create(spec, s.adapter.clone()).await?;
    Ok((StatusCode::CREATED, Json(json!(row))))
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
    Ok(Json(json!({ "session": row, "waiting": waiting })))
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

pub async fn kill(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    s.manager.kill(&id).await?;
    Ok(StatusCode::OK)
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
}

pub async fn answer_permission(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<AnswerBody>,
) -> ApiResult<StatusCode> {
    s.manager.answer(&id, b.option_id).await?;
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
    Ok(Json(json!({ "review": r, "stale": stale })))
}

/// What changed in the worktree since submit. An empty list means the diff
/// still describes the branch.
async fn staleness_of(s: &AppState, r: &crate::store::ReviewRow) -> Vec<String> {
    let Ok(Some(session)) = s.store().get_session(&r.session_id) else {
        return vec!["the session is gone".into()];
    };
    let Some(worktree) = session.worktree_path else {
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
    let session = s
        .store()
        .get_session(&r.session_id)?
        .ok_or(ApiError(StatusCode::CONFLICT, "the session is gone".into()))?;
    let worktree = session.worktree_path.ok_or(ApiError(
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
            s.manager.publish_queue().await;
            Ok(json!({ "state": "rejected" }))
        }
        "approve" => {
            // Stale means the branch is no longer what was reviewed. Approving
            // it would publish something nobody read.
            let stale = staleness_of(&s, &r).await;
            if !stale.is_empty() {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!("changed since submit: {}", stale.join(", ")),
                ));
            }
            let session = s
                .store()
                .get_session(&r.session_id)?
                .ok_or(ApiError(StatusCode::CONFLICT, "the session is gone".into()))?;
            let worktree = session.worktree_path.ok_or(ApiError(
                StatusCode::CONFLICT,
                "the worktree is gone".into(),
            ))?;
            let target: crate::review::publish::Target = serde_json::from_str(&r.target)
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            let title = b.title.as_deref().unwrap_or(r.approved_title()).to_string();
            let body = b.body.as_deref().unwrap_or(r.approved_body()).to_string();

            // Claim the publish atomically: only the transition that moves the
            // row into `publishing` may push. Two concurrent approves cannot both
            // win this, so the change is opened once, not once per request.
            if !s.store().begin_publish(&id)? {
                let now = s.store().get_review(&id)?.map(|r| r.state);
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    format!(
                        "this review is already being decided ({})",
                        now.as_deref().unwrap_or("gone")
                    ),
                ));
            }

            // The node publishes, using a credential the harness never had, and
            // pins the push to the reviewed commit.
            let result = crate::review::publish::publish(
                &s.tools.broker,
                &s.cfg,
                &r.channel,
                &s.node_id,
                &worktree,
                &target,
                &r.head_sha,
                &title,
                &body,
            )
            .await;

            match result {
                Ok(published) => {
                    s.store().finish_publish(&id, &title, &body, &published)?;
                    s.manager.publish_queue().await;
                    // Published is done: the item closes and the execute
                    // session ends with it. Normal path of the whole chain.
                    if let Some(item) = s
                        .store()
                        .get_session(&r.session_id)?
                        .and_then(|sess| sess.work_item_id)
                    {
                        match crate::corpus::work::close(
                            s.store(),
                            s.manager.bus(),
                            &s.node_id,
                            &item,
                            Some(&r.session_id),
                        ) {
                            Ok(_) => {
                                s.manager
                                    .item_closed(&r.session_id, &format!("published: {published}"))
                                    .await
                            }
                            Err(e) => tracing::warn!(error = %e, "item not closed after publish"),
                        }
                    }
                    Ok(json!({ "state": "approved", "published": published }))
                }
                Err(e) => {
                    // The forge refused: undo the publishing claim so the review
                    // returns to the queue rather than being stuck mid-publish.
                    // The operator approved, the forge refused, and that is a
                    // thing to fix rather than a verdict.
                    s.store().abort_publish(&id)?;
                    s.manager.publish_queue().await;
                    Err(ApiError(StatusCode::BAD_GATEWAY, e.to_string()))
                }
            }
        }
        other => Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{other:?} is not a verdict"),
        )),
    }
}

/// The queue, ordered on the node: waiting-on-you first, oldest first within
/// it. The interface renders this order rather than deciding it.
pub async fn queue(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let waiting = s.store().open_permissions()?;
    // Permission requests before reviews: requests expire, reviews do not.
    let reviews = s.store().open_reviews()?;
    let sessions = s.store().list_sessions(None)?;
    let running: Vec<_> = sessions
        .iter()
        .filter(|s| !crate::session::state::SessionState::from_stored(&s.state).is_terminal())
        .cloned()
        .collect();
    let ended: Vec<_> = sessions
        .iter()
        .filter(|s| crate::session::state::SessionState::from_stored(&s.state).is_terminal())
        .take(20)
        .cloned()
        .collect();
    let promotions = s.store().open_promotions()?;
    Ok(Json(json!({
        "waiting": waiting, "reviews": reviews, "promotions": promotions, "running": running, "ended": ended
    })))
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
}

/// `GET /api/docs?channel=&q=&kind=`: the list (no bodies), or search hits.
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
    Ok(Json(json!(doc)))
}

#[derive(Deserialize)]
pub struct PutDoc {
    pub body: String,
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
    match crate::mcp::docs::write_document(
        s.store(),
        s.manager.bus(),
        &s.node_id,
        &channel,
        &slug,
        &body.body,
        if_match.as_deref(),
        create_only,
    ) {
        Ok(doc) => Json(json!(doc)).into_response(),
        Err(crate::mcp::docs::WriteError::Conflict { hash, body }) => (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({ "error": { "message": "the document changed since it was read" }, "hash": hash, "body": body })),
        )
            .into_response(),
        Err(crate::mcp::docs::WriteError::Slug(m)) => {
            ApiError(StatusCode::BAD_REQUEST, format!("slug {m:?} is not usable")).into_response()
        }
        Err(crate::mcp::docs::WriteError::Store(e)) => ApiError::from(e).into_response(),
    }
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
        NoLogin(_) | NotPending(_) => StatusCode::CONFLICT,
        Busy(_) => StatusCode::CONFLICT,
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
        Some(p) => Ok(Json(json!(p.list()))),
        None => Ok(Json(json!(providers_json(&s)))),
    }
}

#[derive(Deserialize, Default)]
pub struct ConnectBody {
    #[serde(default)]
    pub channels: Vec<String>,
}

/// Start the harness's login for a provider; the response carries the URL to
/// open. The card then takes the paste-back.
pub async fn connect_provider(
    State(s): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<ConnectBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let channels = body.map(|Json(b)| b.channels).unwrap_or_default();
    connect_local(&s, &name, channels).await.map(Json)
}

async fn connect_local(
    s: &AppState,
    name: &str,
    channels: Vec<String>,
) -> ApiResult<serde_json::Value> {
    let url = providers_of(s)?
        .connect(name, channels)
        .await
        .map_err(provider_err)?;
    Ok(json!({ "name": name, "url": url }))
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
        .code(name, code)
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
        .disconnect(name)
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
    Path((node_id, name)): Path<(String, String)>,
    body: Option<Json<ConnectBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let channels = body.map(|Json(b)| b.channels).unwrap_or_default();
    if node_id == s.node_id {
        return connect_local(&s, &name, channels).await.map(Json);
    }
    peer_command(
        &s,
        &node_id,
        proto::frame::Command::ProviderConnect { name, channels },
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
    let runner = backend.runner(
        crate::session::materialize::probe_mounts(
            &backend.harness_home(),
            s.adapter.as_ref(),
            &wiring,
        )
        .unwrap_or_default(),
    );
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

/// Commands other nodes forward to this one run exactly as local requests do.
#[async_trait::async_trait]
impl crate::mesh::forward::CommandExecutor for AppState {
    async fn execute(&self, command: proto::frame::Command) -> Result<serde_json::Value, String> {
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
            } => self
                .manager
                .answer(&permission_id, option_id)
                .await
                .map(|_| json!({ "answered": true }))
                .map_err(Into::into),
            C::Kill { session_id } => self
                .manager
                .kill(&session_id)
                .await
                .map(|_| json!({ "killed": true }))
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
                Ok(p) => p
                    .connect(&name, channels)
                    .await
                    .map(|url| json!({ "name": name, "url": url }))
                    .map_err(provider_err),
                Err(e) => Err(e),
            },
            C::ProviderCode { name, code } => match providers_of(self) {
                Ok(p) => p
                    .code(&name, &code)
                    .await
                    .map(|_| json!({ "ok": true }))
                    .map_err(provider_err),
                Err(e) => Err(e),
            },
            C::ProviderDisconnect { name } => match providers_of(self) {
                Ok(p) => p
                    .disconnect(&name)
                    .await
                    .map(|_| json!({ "ok": true }))
                    .map_err(provider_err),
                Err(e) => Err(e),
            },
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

/// The first node: mint the mesh channel here and point this node at a hub.
/// Loopback only — this decides which hub the node trusts.
pub async fn mesh_init(
    _: super::auth::Loopback,
    State(s): State<AppState>,
    Json(body): Json<HubBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let hub = body.hub_url.trim().trim_end_matches('/').to_string();
    if !hub.starts_with("https://") && !hub.starts_with("http://") {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a hub is an http(s) URL".into(),
        ));
    }
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
                        cfg.mesh.hub_url = Some(hub.trim_end_matches('/').to_string());
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
