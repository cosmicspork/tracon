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
}

impl AppState {
    pub fn store(&self) -> &Arc<Store> {
        self.manager.store()
    }
}

pub struct ApiError(StatusCode, String);

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
            | SessionError::UnknownChannel(_) => StatusCode::UNPROCESSABLE_ENTITY,
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

pub async fn get_node(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(node_json(&s)?))
}

/// Every node this one knows: itself first, then peers as the mesh reports them.
pub async fn list_nodes(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let rows = s.store().list_nodes()?;
    let mut out: Vec<serde_json::Value> = rows.iter().map(node_row_json).collect();
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
            out.push(json!({ "name": name, "nodes": [s.node_id] }));
        }
    }
    for c in rows {
        if c.name.starts_with('@') {
            continue;
        }
        out.push(json!({ "name": c.name, "nodes": s.store().nodes_in_channel(&c.name)? }));
    }
    Ok(Json(json!(out)))
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
            if !s.store().request_changes(&id, notes)? {
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
    Ok(Json(
        json!({ "waiting": waiting, "reviews": reviews, "running": running, "ended": ended }),
    ))
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
    let url = providers_of(&s)?
        .connect(&name, channels)
        .await
        .map_err(provider_err)?;
    Ok(Json(json!({ "name": name, "url": url })))
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
    providers_of(&s)?
        .code(&name, &body.code)
        .await
        .map_err(provider_err)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn disconnect_provider(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    providers_of(&s)?
        .disconnect(&name)
        .await
        .map_err(provider_err)?;
    Ok(Json(json!({ "ok": true })))
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
        crate::session::materialize::probe_mounts(&backend.harness_home(), &wiring)
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
            } => {
                decide_local(
                    self,
                    &review_id,
                    VerdictBody {
                        verdict,
                        reason,
                        title,
                        body,
                    },
                )
                .await
            }
        };
        r.map_err(|e| e.1)
    }
}
