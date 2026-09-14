//! Privileged management of signed local policy and its mesh rollout.
//!
//! The private signing key is used only to sign an in-memory preview. It never
//! appears in an API response, a rollout row, or a mesh frame.

use std::collections::BTreeSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    policy::bundle,
    store::{now_ms, PolicyRolloutRow},
};

use super::{
    api::{ApiError, ApiResult, AppState},
    auth::{Administrator, Loopback},
};

const MAX_POLICY_BYTES: usize = 1_048_576;
const MAX_TARGETS: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub toml: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeRequest {
    pub confirm: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    pub toml: String,
    pub signature: String,
    pub bundle_sha256: String,
    /// The exact installed bundle the operator reviewed. A concurrent manual
    /// edit changes this value and fails closed rather than being overwritten.
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
}
/// The verified bundle presently installed on disk, the policy the process is
/// actually running, and every durable rollout. These are separate because an
/// out-of-band file change does not reload the process policy. Receipts are
/// explicitly marked unconfirmed until a verified receiving envelope says it
/// installed the same hash.
pub async fn status(
    State(state): State<AppState>,
    _admin: Administrator,
) -> ApiResult<Json<Value>> {
    let policy = state.tools.policy.read();
    let (installed_bundle, installed_error) = match bundle::current() {
        Ok(bundle) => (Some(bundle), None),
        Err(bundle::BundleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            (None, None)
        }
        Err(error) => (None, Some(error.to_string())),
    };
    let running = running_policy_json(&policy);
    let installation = state.store().policy_installation()?.filter(|record| {
        installed_bundle
            .as_ref()
            .is_some_and(|bundle| bundle.sha256 == record.bundle_sha256)
    });
    let installed = installed_bundle.map(bundle_json);
    let trust_identity = bundle::public_identity().ok();
    drop(policy);
    let rollouts = state
        .store()
        .policy_rollouts()?
        .into_iter()
        .map(|row| rollout_json(state.store(), row))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({
        "installed": installed,
        "installed_error": installed_error,
        "running": running,
        "trust_identity": trust_identity,
        "installation": installation,
        "rollouts": rollouts,
        "signing_key_present": bundle::Paths::signing_key().is_file(),
    })))
}

/// Explicit one-time local setup for a node that has no policy artifacts yet.
/// Existing keys, signatures, or bundles are an error — setup never turns into
/// key rotation or an overwrite of a custom policy.
pub async fn initialize(
    State(state): State<AppState>,
    _admin: Administrator,
    _local: Loopback,
    Json(request): Json<InitializeRequest>,
) -> ApiResult<Json<Value>> {
    if !request.confirm {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm initial policy setup before creating a signing identity",
        ));
    }
    let mut loaded_policy = state.tools.policy.write();
    let signed = bundle::initialize_shipped().map_err(bundle_error)?;
    *loaded_policy = signed.policy.clone();
    state
        .store()
        .record_policy_installation(&signed.sha256, Some(&state.node_id), None)?;
    state.store().set_node_policy_metadata(
        &state.node_id,
        Some(&signed.pubkey_hex),
        Some(&signed.sha256),
        true,
    )?;
    Ok(Json(bundle_json(signed)))
}

/// Parse and sign without touching policy files. Signing requires the existing
/// key pair; this endpoint can never create or rotate one.
pub async fn preview(
    _admin: Administrator,
    _local: Loopback,
    Json(request): Json<PreviewRequest>,
) -> ApiResult<Json<Value>> {
    check_policy_size(&request.toml)?;
    let signed = bundle::preview(&request.toml).map_err(bundle_error)?;
    Ok(Json(bundle_json(signed)))
}

/// Install exactly a previously previewed signed bundle and optionally create a
/// durable rollout for selected peers. Installation remains local-loopback only
/// and is guarded by both the operator credential and machine locality.
pub async fn apply(
    State(state): State<AppState>,
    _admin: Administrator,
    _local: Loopback,
    Json(request): Json<ApplyRequest>,
) -> ApiResult<Json<Value>> {
    check_policy_size(&request.toml)?;
    if bundle::sha256(&request.toml) != request.bundle_sha256 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bundle hash does not match the previewed policy",
        ));
    }
    // Serialize the comparison, disk installation and effective-policy update
    // with other local applies and incoming signed mesh bundles.
    let mut loaded_policy = state.tools.policy.write();
    let current = installed_bundle()?;
    if current.as_ref().map(|bundle| bundle.sha256.as_str()) != request.expected_sha256.as_deref() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the installed policy changed after preview; refresh and review the current policy before applying",
        ));
    }
    let targets = selected_targets(&state, request.targets)?;
    if !targets.is_empty() && state.mesh.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a mesh connection is required to select rollout targets",
        ));
    }
    let pubkey = bundle::public_identity().map_err(bundle_error)?;
    let applied =
        bundle::install(&request.toml, &request.signature, &pubkey, false).map_err(bundle_error)?;
    let policy_version = applied.version;
    *loaded_policy = applied;
    state.store().set_node_policy_metadata(
        &state.node_id,
        Some(&pubkey),
        Some(&request.bundle_sha256),
        true,
    )?;

    let rollout = if targets.is_empty() {
        None
    } else {
        let now = now_ms();
        let rollout = PolicyRolloutRow {
            id: uuid::Uuid::now_v7().to_string(),
            bundle_sha256: request.bundle_sha256.clone(),
            policy_version: i64::from(policy_version),
            toml: request.toml,
            sig_hex: request.signature,
            pubkey_hex: pubkey,
            source_node: state.node_id.clone(),
            created_ms: now,
            updated_ms: now,
        };
        state.store().create_policy_rollout(&rollout, &targets)?;
        Some(rollout)
    };
    state.store().record_policy_installation(
        &request.bundle_sha256,
        Some(&state.node_id),
        rollout.as_ref().map(|r| r.id.as_str()),
    )?;
    if let (Some(mesh), Some(rollout)) = (&state.mesh, &rollout) {
        mesh.dispatch_policy_rollout(&rollout.id)
            .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    }
    Ok(Json(json!({
        "installed": bundle_json(bundle::current().map_err(bundle_error)?),
        "rollout": rollout.map(|row| rollout_json(state.store(), row)).transpose()?,
    })))
}

/// Requeue the retained bundle to targets that have not sent an authenticated
/// application receipt. It never re-signs or reads a mutable policy file.
pub async fn retry(
    State(state): State<AppState>,
    _admin: Administrator,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mesh = state.mesh.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::CONFLICT, "this node is not connected to a mesh")
    })?;
    mesh.dispatch_policy_rollout(&id)
        .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    let rollout = state
        .store()
        .policy_rollout(&id)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "policy rollout not found"))?;
    Ok(Json(rollout_json(state.store(), rollout)?))
}

fn selected_targets(state: &AppState, targets: Vec<String>) -> ApiResult<Vec<String>> {
    if targets.len() > MAX_TARGETS {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too many rollout targets",
        ));
    }
    let mut selected = BTreeSet::new();
    for target in targets {
        if target.trim().is_empty() {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "a rollout target is empty",
            ));
        }
        let node = state.store().get_node(&target)?.ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("unknown rollout target {target}"),
            )
        })?;
        if node.is_self != 0 {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the local node is already applied; do not target it",
            ));
        }
        selected.insert(target);
    }
    Ok(selected.into_iter().collect())
}

fn installed_bundle() -> ApiResult<Option<bundle::SignedBundle>> {
    match bundle::current() {
        Ok(bundle) => Ok(Some(bundle)),
        Err(bundle::BundleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("the installed policy cannot be replaced until it is repaired: {error}"),
        )),
    }
}

fn check_policy_size(toml: &str) -> ApiResult<()> {
    if toml.len() > MAX_POLICY_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "policy bundle exceeds 1 MiB",
        ));
    }
    Ok(())
}

fn bundle_error(error: bundle::BundleError) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
}

fn policy_json(policy: &crate::policy::Policy) -> Value {
    json!({
        "version": policy.version,
        "rules": policy.rules,
        "trusted": policy.trusted,
    })
}

fn running_policy_json(policy: &crate::policy::Policy) -> Value {
    json!({
        "version": policy.version,
        "rule_count": policy.rules.len(),
        "trusted": policy.trusted,
    })
}

fn bundle_json(bundle: bundle::SignedBundle) -> Value {
    json!({
        "toml": bundle.toml,
        "signature": bundle.sig_hex,
        "trust_identity": bundle.pubkey_hex,
        "bundle_sha256": bundle.sha256,
        "policy": policy_json(&bundle.policy),
    })
}

fn rollout_json(store: &crate::store::Store, rollout: PolicyRolloutRow) -> Result<Value, ApiError> {
    let targets = store
        .policy_rollout_targets(&rollout.id)?
        .into_iter()
        .map(|target| {
            let confirmation = match target.status.as_str() {
                "applied" => "applied",
                "rejected" => "rejected",
                _ => "unconfirmed",
            };
            let compatibility = if matches!(confirmation, "applied" | "rejected") {
                "receipt_capable"
            } else {
                "unknown_or_legacy"
            };
            json!({
                "node_id": target.node_id,
                "status": target.status,
                "confirmation": confirmation,
                "compatibility": compatibility,
                "detail": target.detail,
                "attempts": target.attempts,
                "last_sent_ms": target.last_sent_ms,
                "acknowledged_ms": target.acknowledged_ms,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "id": rollout.id,
        "bundle_sha256": rollout.bundle_sha256,
        "policy_version": rollout.policy_version,
        "source_node": rollout.source_node,
        "created_ms": rollout.created_ms,
        "updated_ms": rollout.updated_ms,
        "targets": targets,
    }))
}
