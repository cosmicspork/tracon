//! Operator API for candidate-bound QA and repository-derived prototypes.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    authority,
    policy::Verdict,
    qa::{
        self,
        service::{self, QaAccess},
    },
    store::CandidateRow,
};

use super::api::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrototypeRequest {
    pub candidate_id: String,
}

pub async fn candidate_evidence(
    State(state): State<AppState>,
    Path(candidate_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let candidate = candidate(&state, &candidate_id)?;
    let value = qa_evidence_json(&state, &candidate)?;
    Ok(Json(value))
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
        if !granted(state, candidate, qa::DEPLOY_ACTION, &deploy_target)? {
            missing.push(format!("deploy: {deploy_target}"));
        }
        let browser_target = qa::browser_authority_target(id, target)
            .map_err(|error| ApiError::new(StatusCode::CONFLICT, error))?;
        if !granted(state, candidate, qa::BROWSER_VERIFY_ACTION, &browser_target)? {
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
                qa::BROWSER_TEST_ACCOUNT_ACTION,
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
    let policy = state.tools.policy.read().map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "policy lock is unavailable",
        )
    })?;
    let evidence = json!({ "candidate_id": candidate.id, "head_sha": candidate.head_sha });
    let decision = authority::decide(
        state.store(),
        &policy,
        &candidate.channel,
        session_id,
        action,
        target,
        Some(&candidate.head_sha),
        &evidence,
    )
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(decision.verdict == Verdict::Allow)
}

fn conflict(error: String) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, error)
}
