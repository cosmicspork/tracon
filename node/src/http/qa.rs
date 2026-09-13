//! Operator API for candidate-bound QA and repository-derived prototypes.

use std::{collections::BTreeSet, time::Duration};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    authority,
    policy::Verdict,
    qa::{
        self,
        service::{self, QaAccess},
    },
    store::{CandidateCursor, CandidateListFilter, CandidateRow},
};

use super::api::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrototypeRequest {
    pub candidate_id: String,
}

const CANDIDATE_PAGE_DEFAULT: usize = 16;
const CANDIDATE_PAGE_MAX: usize = 50;

/// Browse filters deliberately accept only bounded, persisted relationships.
/// A candidate id remains the technical lookup for the detailed operation
/// page; ordinary discovery starts with work and review names instead.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    runner: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    review_id: Option<String>,
    #[serde(default)]
    work_item_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    before_ms: Option<i64>,
    #[serde(default)]
    before_id: Option<String>,
}

#[derive(Clone)]
struct CandidatePageRequest {
    filter: CandidateListFilter,
    runner: Option<String>,
    limit: usize,
}

/// A remote detail must name both the node that owns it and the actual shared
/// channel. Neither identifier is inferred from an opaque candidate id.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteCandidateEvidenceRequest {
    candidate_id: String,
    channel: String,
    owner: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEvidenceQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    channel: Option<String>,
}

const REMOTE_EVIDENCE_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Enforce the wire budget while serializing, without allocating a second
/// response-sized buffer merely to count it.
struct EvidenceLimit(usize);

impl std::io::Write for EvidenceLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "mesh evidence limit exceeded",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Return a bounded page of persisted candidate evidence with only real task,
/// review, and session relationships. A session/review/work query is routed to
/// its owning node when that owner is known; the local node never turns a
/// remote miss into an empty answer.
pub async fn list_candidates(
    State(state): State<AppState>,
    Query(mut query): Query<CandidateListQuery>,
) -> ApiResult<Json<Value>> {
    let request = candidate_page_request(query.clone())?;
    let relationship_owner = candidate_owner(&state, &request.filter)?;
    let owner = match (request.runner.as_deref(), relationship_owner.as_deref()) {
        (Some(runner), Some(relationship_owner)) if runner != relationship_owner => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the selected evidence runner does not own the requested relationship",
            ));
        }
        (Some(runner), _) => runner,
        (None, Some(relationship_owner)) => relationship_owner,
        (None, None) => &state.node_id,
    };
    if owner != state.node_id {
        // The receiver rejects a command that names any other runner, so this
        // cannot turn an owner lookup into a relay through an arbitrary peer.
        query.runner = Some(owner.to_string());
        return Ok(Json(remote_candidate_page(&state, owner, query).await?));
    }
    Ok(Json(candidate_page_json(&state, request)?))
}

/// Execute a peer's bounded candidate browse request on the node that holds
/// the immutable evidence. The direct mesh command authenticates `sender`;
/// shared channel membership is still checked before a row leaves this node.
pub fn forwarded_candidates(state: &AppState, sender: &str, query: Value) -> ApiResult<Value> {
    let query = serde_json::from_value(query).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("candidate browse query is malformed: {error}"),
        )
    })?;
    let request = candidate_page_request(query)?;
    if request.runner.as_deref() != Some(state.node_id.as_str()) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "the evidence runner does not match this node",
        ));
    }
    let channel = request.filter.channel.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "remote candidate browse requires its channel",
        )
    })?;
    ensure_shared_evidence_channel(state, sender, channel)?;
    candidate_page_json(state, request)
}

fn candidate_page_request(query: CandidateListQuery) -> ApiResult<CandidatePageRequest> {
    let limit = query.limit.unwrap_or(CANDIDATE_PAGE_DEFAULT);
    if !(1..=CANDIDATE_PAGE_MAX).contains(&limit) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("candidate page limit must be 1 through {CANDIDATE_PAGE_MAX}"),
        ));
    }
    let channel = ordinary_channel(query.channel)?;
    let before = match (query.before_ms, query.before_id) {
        (None, None) => None,
        (Some(captured_ms), Some(id)) if captured_ms >= 0 => Some(CandidateCursor {
            captured_ms,
            id: browse_id(Some(id), "candidate page cursor")?.ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "candidate page cursor is required")
            })?,
        }),
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "candidate page cursor requires a non-negative capture time and id",
            ));
        }
    };
    Ok(CandidatePageRequest {
        filter: CandidateListFilter {
            query: browse_text(query.q, "search", 160)?,
            channel,
            session_id: browse_id(query.session_id, "session id")?,
            review_id: browse_id(query.review_id, "review id")?,
            work_item_id: browse_id(query.work_item_id, "work item id")?,
            before,
            // One extra row determines whether a cursor exists without
            // allowing the endpoint's visible page to grow unbounded.
            limit: limit + 1,
        },
        runner: browse_id(query.runner, "evidence runner")?,
        limit,
    })
}

fn candidate_page_json(state: &AppState, request: CandidatePageRequest) -> ApiResult<Value> {
    let mut items = state.store().list_candidates(&request.filter)?;
    let has_next = items.len() > request.limit;
    if has_next {
        items.pop();
    }
    for item in &mut items {
        item.candidate.owner_node_id = Some(state.node_id.clone());
    }
    let next_before = if has_next {
        items.last().map(
            |item| json!({ "captured_ms": item.candidate.captured_ms, "id": item.candidate.id }),
        )
    } else {
        None
    };
    Ok(json!({ "items": items, "next_before": next_before }))
}

fn candidate_owner(state: &AppState, filter: &CandidateListFilter) -> ApiResult<Option<String>> {
    let mut owners = BTreeSet::new();
    if let Some(session_id) = &filter.session_id {
        if let Some(session) = state.store().get_session(session_id)? {
            owners.insert(session.node_id);
        }
    }
    if let Some(review_id) = &filter.review_id {
        if let Some(review) = state.store().get_review(review_id)? {
            owners.insert(review.node_id);
        }
    }
    if let Some(work_item_id) = &filter.work_item_id {
        for session in state.store().sessions_of_work_item(work_item_id)? {
            owners.insert(session.node_id);
        }
    }
    match owners.len() {
        0 => Ok(None),
        1 => Ok(owners.into_iter().next()),
        _ => Err(ApiError::new(
            StatusCode::CONFLICT,
            "evidence spans multiple runner nodes; open one linked session to inspect its evidence",
        )),
    }
}

async fn remote_candidate_page(
    state: &AppState,
    owner: &str,
    mut query: CandidateListQuery,
) -> ApiResult<Value> {
    if query
        .channel
        .as_deref()
        .is_none_or(|channel| channel.trim().is_empty())
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "choose the evidence channel before querying its runner node",
        ));
    }
    query.runner = Some(owner.to_string());
    let mesh = state.mesh.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the evidence runner is remote and this node has no mesh connection",
        )
    })?;
    let query = serde_json::to_value(query)
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    mesh.command(
        owner,
        proto::frame::Command::EvidenceCandidates { query },
        Duration::from_secs(15),
    )
    .await
    .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, error.to_string()))
}

pub async fn candidate_evidence(
    State(state): State<AppState>,
    Path(candidate_id): Path<String>,
    Query(query): Query<CandidateEvidenceQuery>,
) -> ApiResult<Json<Value>> {
    let candidate_id =
        browse_id(Some(candidate_id), "candidate id")?.expect("a path parameter is always present");
    let channel = ordinary_channel(query.channel)?;
    let owner = browse_id(query.owner, "evidence owner")?;
    if owner.is_some() && channel.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a remote evidence owner requires its shared channel",
        ));
    }
    if let Some(owner) = owner.filter(|owner| owner != &state.node_id) {
        let request = RemoteCandidateEvidenceRequest {
            candidate_id,
            channel: channel.expect("remote evidence requires a channel"),
            owner,
        };
        return Ok(Json(remote_candidate_evidence(&state, request).await?));
    }
    let candidate = candidate(&state, &candidate_id)?;
    ensure_candidate_channel(&candidate, channel.as_deref())?;
    Ok(Json(qa_evidence_json(&state, &candidate)?))
}

/// Execute a direct read at the node that owns a candidate. The source validates
/// its shared membership before queuing; the receiver repeats that check and
/// binds the persisted candidate to the named channel before returning it.
pub fn forwarded_candidate_evidence(
    state: &AppState,
    sender: &str,
    request: Value,
) -> ApiResult<Value> {
    let request: RemoteCandidateEvidenceRequest =
        serde_json::from_value(request).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("candidate evidence request is malformed: {error}"),
            )
        })?;
    if request.owner != state.node_id {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "the evidence owner does not match this node",
        ));
    }
    ensure_shared_evidence_channel(state, sender, &request.channel)?;
    let candidate = candidate(state, &request.candidate_id)?;
    ensure_candidate_channel(&candidate, Some(&request.channel))?;
    let value = qa_evidence_json(state, &candidate)?;
    serde_json::to_writer(EvidenceLimit(REMOTE_EVIDENCE_MAX_BYTES), &value).map_err(|error| {
        if error.is_io() {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "candidate evidence exceeds the safe mesh response limit",
            )
        } else {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
    })?;
    Ok(value)
}

async fn remote_candidate_evidence(
    state: &AppState,
    request: RemoteCandidateEvidenceRequest,
) -> ApiResult<Value> {
    let members = state.store().nodes_in_channel(&request.channel)?;
    if !members.iter().any(|node| node == &state.node_id)
        || !members.iter().any(|node| node == &request.owner)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "the evidence owner and this node do not share the requested channel",
        ));
    }
    let mesh = state.mesh.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "the evidence runner is remote and this node has no mesh connection",
        )
    })?;
    let owner = request.owner.clone();
    let request = serde_json::to_value(request)
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    mesh.command(
        &owner,
        proto::frame::Command::EvidenceCandidate { request },
        Duration::from_secs(15),
    )
    .await
    .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, error.to_string()))
}

pub async fn candidate_targets(
    State(state): State<AppState>,
    Path(candidate_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let candidate = candidate(&state, &candidate_id)?;
    Ok(Json(
        json!({ "targets": target_views(&state, &candidate)? }),
    ))
}

pub async fn deploy(
    State(state): State<AppState>,
    Json(request): Json<qa::DeployRequest>,
) -> ApiResult<Json<Value>> {
    let row = service::deploy(&access(&state), request)
        .await
        .map_err(conflict)?;
    Ok(Json(json!(row)))
}

pub async fn browser_verify(
    State(state): State<AppState>,
    Json(request): Json<qa::BrowserRequest>,
) -> ApiResult<Json<Value>> {
    let result = service::browser_verify(&access(&state), request)
        .await
        .map_err(conflict)?;
    Ok(Json(json!(result)))
}

pub async fn build_prototype(
    State(state): State<AppState>,
    Json(request): Json<PrototypeRequest>,
) -> ApiResult<Json<Value>> {
    let row = service::build_prototype(&access(&state), &request.candidate_id)
        .await
        .map_err(conflict)?;
    Ok(Json(json!(row)))
}

pub async fn prototypes(
    State(state): State<AppState>,
    Path(candidate_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _ = candidate(&state, &candidate_id)?;
    let rows = state.store().prototypes_for_candidate(&candidate_id)?;
    Ok(Json(json!({ "prototypes": rows })))
}

pub async fn browser_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut run = state
        .store()
        .qa_browser_run(&id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "QA browser run was not found"))?;
    let deployment = state
        .store()
        .qa_deployment(&run.deployment_id)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "QA browser run has no deployment record",
            )
        })?;
    let newest = state.store().qa_latest_target_observation(&run.target_id)?;
    run.evidence_state = qa::evidence_state(&deployment, &run, newest.as_ref()).into();
    let assets = state.store().qa_assets_for_run(&id)?;
    Ok(Json(json!({ "run": run, "assets": assets })))
}

fn browse_text(value: Option<String>, field: &str, max_len: usize) -> ApiResult<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > max_len || value.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} is malformed"),
        ));
    }
    Ok(Some(value.into()))
}

fn ordinary_channel(value: Option<String>) -> ApiResult<Option<String>> {
    let channel = browse_text(value, "channel", 128)?;
    if let Some(channel) = &channel {
        if !proto::frame::valid_channel(channel) || channel.starts_with('@') {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "name one ordinary channel",
            ));
        }
    }
    Ok(channel)
}

fn ensure_shared_evidence_channel(state: &AppState, sender: &str, channel: &str) -> ApiResult<()> {
    let members = state.store().nodes_in_channel(channel)?;
    if !members.iter().any(|id| id == sender) || !members.iter().any(|id| id == &state.node_id) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "sender and receiver must belong to the evidence channel",
        ));
    }
    Ok(())
}

fn ensure_candidate_channel(candidate: &CandidateRow, expected: Option<&str>) -> ApiResult<()> {
    if expected.is_some_and(|channel| channel != candidate.channel) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "candidate evidence is not available on the requested channel",
        ));
    }
    Ok(())
}

fn browse_id(value: Option<String>, field: &str) -> ApiResult<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{field} is malformed"),
        ));
    }
    Ok(Some(value.into()))
}

fn access(state: &AppState) -> QaAccess<'_> {
    QaAccess {
        store: state.store(),
        manager: &state.manager,
        cfg: &state.cfg,
        broker: &state.tools.broker,
        http: &state.tools.http,
        policy: &state.tools.policy,
        node_id: &state.node_id,
        requester_session_id: None,
        requester_channel: None,
    }
}

fn candidate(state: &AppState, id: &str) -> ApiResult<CandidateRow> {
    if id.is_empty() || id.len() > 256 || id.contains(['\0', '\r', '\n']) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "candidate id is malformed",
        ));
    }
    state
        .store()
        .candidate(id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "candidate was not found"))
}

fn qa_evidence_json(state: &AppState, candidate: &CandidateRow) -> ApiResult<Value> {
    let deployments = state.store().qa_deployments_for_candidate(&candidate.id)?;
    let mut browser_runs = state.store().qa_browser_runs_for_candidate(&candidate.id)?;
    let mut assets = Vec::new();
    for run in &mut browser_runs {
        let deployment = state
            .store()
            .qa_deployment(&run.deployment_id)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::CONFLICT,
                    "QA browser run has no deployment record",
                )
            })?;
        let newest = state.store().qa_latest_target_observation(&run.target_id)?;
        run.evidence_state = qa::evidence_state(&deployment, run, newest.as_ref()).into();
        assets.extend(state.store().qa_assets_for_run(&run.id)?);
    }
    let prototypes = state.store().prototypes_for_candidate(&candidate.id)?;
    Ok(json!({
        "candidate": {
            "id": candidate.id,
            "channel": candidate.channel,
            "head_sha": candidate.head_sha,
            "owner_session_id": candidate.owner_session_id,
            "owner_node_id": state.node_id,
            "captured_ms": candidate.captured_ms,
        },
        "targets": target_views(state, candidate)?,
        "deployments": deployments,
        "browser_runs": browser_runs,
        "assets": assets,
        "prototypes": prototypes,
    }))
}

fn target_views(state: &AppState, candidate: &CandidateRow) -> ApiResult<Vec<Value>> {
    let mut views = Vec::with_capacity(state.cfg.qa.targets.len());
    for (id, target) in &state.cfg.qa.targets {
        let mut missing = Vec::new();
        let deploy_target = qa::deploy_authority_target(id, target);
        if !granted(state, candidate, authority::DEPLOY, &deploy_target)? {
            missing.push(format!("deploy: {deploy_target}"));
        }
        let browser_target = qa::browser_authority_target(id, target)
            .map_err(|error| ApiError::new(StatusCode::CONFLICT, error))?;
        if !granted(state, candidate, authority::BROWSER_VERIFY, &browser_target)? {
            missing.push(format!("browser verification: {browser_target}"));
        }
        if let Some(credential) = target
            .browser
            .test_credential
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            let account_target = qa::test_account_authority_target(id, credential);
            if !granted(
                state,
                candidate,
                authority::BROWSER_TEST_ACCOUNT,
                &account_target,
            )? {
                missing.push(format!("test account: {account_target}"));
            }
        }
        views.push(json!({
            "id": id,
            "origin": target.origin,
            "identity_url": target.identity_url,
            "identity_header": target.identity_header,
            "execution_image": target.deployment.execution_image,
            "browser_image": target.browser.image,
            "test_credential": target.browser.test_credential,
            "missing_grants": missing,
        }));
    }
    Ok(views)
}

fn granted(
    state: &AppState,
    candidate: &CandidateRow,
    action: &str,
    target: &str,
) -> ApiResult<bool> {
    let session_id = candidate.owner_session_id.trim();
    if session_id.is_empty() {
        return Ok(false);
    }
    let policy = state.tools.policy.read();
    let evidence = json!({ "candidate_id": candidate.id, "head_sha": candidate.head_sha });
    let decision = authority::decide(
        state.store(),
        &policy,
        &authority::AuthorityQuery {
            channel: &candidate.channel,
            session_id,
            action,
            target,
            revision: Some(&candidate.head_sha),
            args: &evidence,
        },
    )
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(decision.verdict == Verdict::Allow)
}

fn conflict(error: String) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, error)
}
