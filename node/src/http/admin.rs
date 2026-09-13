//! Explicitly authenticated administration for the serving node.
//!
//! These routes deliberately sit apart from the ordinary operator surface.
//! Mesh administration requires an explicit [`Administrator`] session; its
//! bounded invitation, admission, removal, and selected-key handoff flows work
//! for an authenticated remote PWA. Host lifecycle and fixed boundary checks
//! additionally require [`Loopback`].

use axum::{
    extract::{Path, State},
    http::{request::Parts, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    api::{self, ApiError, ApiResult, AppState},
    auth::{self, Administrator, Loopback},
};

fn enrollment_error(error: crate::mesh::enroll::EnrollError) -> ApiError {
    let code = match &error {
        crate::mesh::enroll::EnrollError::Transport(_) => StatusCode::BAD_GATEWAY,
        crate::mesh::enroll::EnrollError::Refused { status, .. } => {
            StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        crate::mesh::enroll::EnrollError::Local(_) => StatusCode::CONFLICT,
    };
    ApiError::new(code, error.to_string())
}

fn hub(s: &AppState) -> ApiResult<&crate::mesh::client::MeshClient> {
    s.mesh.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "this node has no running hub client; pair a hub and restart the node before managing members",
        )
    })
}

fn ordinary_channel(name: &str) -> bool {
    !name.starts_with('@') && proto::frame::valid_channel(name)
}

fn peer_state<T: PartialEq + ?Sized>(
    self_row: bool,
    local: Option<&T>,
    peer: Option<&T>,
) -> &'static str {
    if self_row {
        return if local.is_some() { "local" } else { "unknown" };
    }
    match (local, peer) {
        (Some(local), Some(peer)) if local == peer => "compatible",
        (Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    }
}

fn compatibility(
    node: &crate::store::NodeRow,
    local_policy: Option<&crate::policy::bundle::SignedBundle>,
) -> Value {
    let model_count = node
        .models_json
        .as_deref()
        .and_then(|models| serde_json::from_str::<Value>(models).ok())
        .and_then(|models| models.as_array().map(Vec::len));
    let runtime = match (&node.harness_found, node.harness_pinned.as_str()) {
        (Some(found), pinned) if !pinned.is_empty() && found == pinned => "compatible",
        (Some(_), _) => "mismatch",
        (None, _) if node.checked_at_ms.is_some() => "not_found",
        (None, _) => "unknown",
    };
    let models = match model_count {
        None => "unknown",
        Some(0) => "none_offered",
        Some(_) => "offered",
    };
    let self_row = node.is_self != 0;
    let application_state = peer_state(
        self_row,
        Some(env!("CARGO_PKG_VERSION")),
        node.app_version.as_deref(),
    );
    let local_wire = proto::CONTRACT_VERSION;
    let wire_state = peer_state(self_row, Some(&local_wire), node.wire_contract.as_ref());
    let local_identity = local_policy.map(|bundle| bundle.pubkey_hex.as_str());
    let local_sha256 = local_policy.map(|bundle| bundle.sha256.as_str());
    let policy_identity_state =
        peer_state(self_row, local_identity, node.policy_identity.as_deref());
    let policy_bundle_state = peer_state(self_row, local_sha256, node.policy_sha256.as_deref());
    let receipt_state = if self_row || node.policy_receipt_v1 == Some(true) {
        "supported"
    } else if node.policy_receipt_v1 == Some(false) {
        "unsupported"
    } else {
        "unknown_or_legacy"
    };
    json!({
        "runtime": runtime,
        "pinned": (!node.harness_pinned.is_empty()).then_some(&node.harness_pinned),
        "found": node.harness_found,
        "checked_at_ms": node.checked_at_ms,
        "models": { "state": models, "offered": model_count },
        "application": {
            "state": application_state,
            "local": env!("CARGO_PKG_VERSION"),
            "peer": node.app_version,
            "upgrade_needed": application_state == "mismatch",
        },
        "wire": {
            "state": wire_state,
            "local": local_wire,
            "peer": node.wire_contract,
            "upgrade_needed": wire_state == "mismatch",
        },
        "policy": {
            "identity": {
                "state": policy_identity_state,
                "local": local_identity,
                "peer": node.policy_identity,
            },
            "bundle": {
                "state": policy_bundle_state,
                "local": local_sha256,
                "peer": node.policy_sha256,
            },
            "receipt": {
                "state": receipt_state,
                "advertised": node.policy_receipt_v1,
                "upgrade_needed": receipt_state == "unknown_or_legacy",
            },
        },
    })
}

fn overview(s: &AppState, local: bool) -> ApiResult<Value> {
    let policy = crate::policy::bundle::current().ok();
    let nodes = s.store().list_nodes()?;
    let members = nodes
        .into_iter()
        .map(|node| {
            let channels = s.store().node_channels(&node.id)?;
            Ok(json!({
                "id": node.id,
                "name": node.name,
                "self": node.is_self != 0,
                "reachable": node.reachable != 0,
                "last_seen_ms": node.last_seen_ms,
                "channels": channels,
                "compatibility": compatibility(&node, policy.as_ref()),
            }))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let channels = s
        .store()
        .channel_list()?
        .into_iter()
        .filter(|channel| ordinary_channel(&channel.name))
        .map(|channel| {
            Ok(json!({
                "name": channel.name,
                "nodes": s.store().nodes_in_channel(&channel.name)?,
            }))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let mesh = s.mesh.as_ref().map(|mesh| mesh.snapshot());
    Ok(json!({
        "local": local,
        "inventory_source": "last signed hub directory refresh; missing runtime, application, wire, policy, or receipt metadata is shown as unknown or legacy",
        "mesh": mesh,
        "members": members,
        "channels": channels,
        "capabilities": {
            "invite": s.mesh.is_some(),
            "remove_member": s.mesh.is_some(),
            "share_existing_channel_with_hub": s.mesh.is_some(),
            "edit_member_channels": {
                "available": false,
                "reason": "the current hub protocol can add handoffs but cannot retract one channel from a member; remove the member and enrol it again with the intended channels",
            },
        },
    }))
}

fn invite_view(s: &AppState, invite: &crate::mesh::enroll::Invite) -> Value {
    json!({
        "code": invite.code,
        "display_code": invite.display_code(),
        "url": invite.url,
        "qr_svg": crate::mesh::enroll::qr_svg(&invite.url),
        "channels": invite.channels,
        "expires_at": invite.expires_at,
        "state": if invite.admitted { "admitted" } else if invite.received.is_some() { "received" } else { "waiting" },
        "received": invite.received,
        "received_fingerprint": invite.received_fingerprint(),
        "own_fingerprint": proto::enroll::fingerprint_hex(&s.node_id),
    })
}

fn invitation_code(code: &str) -> ApiResult<String> {
    proto::enroll::normalize_code(code)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "malformed invitation code"))
}

fn node_id(id: &str) -> ApiResult<&str> {
    let id = id.trim();
    if id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "member id must be the 64-character node identity",
        ))
    }
}

fn open_sessions(s: &AppState) -> ApiResult<usize> {
    Ok(s.store()
        .list_sessions(None)?
        .into_iter()
        .filter(|session| {
            !matches!(
                session.state.as_str(),
                "closed" | "killed_budget" | "failed"
            )
        })
        .count())
}

/// `GET /api/admin/access`: the only admin route that may be read before a
/// step-up. It exposes no token, hash, or active-session identifiers.
pub async fn access(State(s): State<AppState>, parts: Parts) -> Json<Value> {
    Json(json!({
        "authenticated": auth::administrator_authenticated(&s.auth, s.store(), &parts.headers),
        "token_configured": s.auth.token_configured(),
        "local": auth::extensions_are_loopback(&parts.extensions),
    }))
}

/// `GET /api/admin/mesh`: local cached membership and the compatibility data
/// each node actually reported. It does not ask the hub to sign a new request.
pub async fn mesh(
    _admin: Administrator,
    State(s): State<AppState>,
    parts: Parts,
) -> ApiResult<Json<Value>> {
    Ok(Json(overview(
        &s,
        auth::extensions_are_loopback(&parts.extensions),
    )?))
}

#[derive(Deserialize)]
pub struct InvitationBody {
    channels: Vec<String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

fn checked_channels(s: &AppState, channels: Vec<String>) -> ApiResult<Vec<String>> {
    if channels.len() > 32 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "an invitation can share at most 32 channels",
        ));
    }
    let mut checked = Vec::new();
    for raw in channels {
        let name = raw.trim();
        if !ordinary_channel(name) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invitations can share only existing ordinary channel names",
            ));
        }
        if s.store().channel_get(name)?.is_none() {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("this node does not hold channel {name}"),
            ));
        }
        if !checked.iter().any(|known| known == name) {
            checked.push(name.to_string());
        }
    }
    Ok(checked)
}

/// `POST /api/admin/mesh/invitations`: open an invitation that hands exactly
/// the selected existing channel keys to the verified new member.
pub async fn create_invitation(
    _admin: Administrator,
    State(s): State<AppState>,
    Json(body): Json<InvitationBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if body
        .ttl_secs
        .is_some_and(|ttl| !(60..=604_800).contains(&ttl))
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invitation lifetime must be between one minute and seven days",
        ));
    }
    let channels = checked_channels(&s, body.channels)?;
    let mesh = hub(&s)?;
    let invite =
        crate::mesh::enroll::open_invite(mesh.identity(), mesh.hub_url(), &channels, body.ttl_secs)
            .await
            .map_err(enrollment_error)?;
    {
        let mut invitations = mesh.invites().lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invitation state lock failed",
            )
        })?;
        invitations.insert(invite.code.clone(), invite.clone());
    }
    Ok((StatusCode::CREATED, Json(invite_view(&s, &invite))))
}

/// `GET /api/admin/mesh/invitations/{code}`: poll the existing invitation and
/// return a proved joining identity when one has answered.
pub async fn poll_invitation(
    _admin: Administrator,
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> ApiResult<Json<Value>> {
    let mesh = hub(&s)?;
    let code = invitation_code(&code)?;
    let mut invite = {
        let invitations = mesh.invites().lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invitation state lock failed",
            )
        })?;
        invitations
            .get(&code)
            .cloned()
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such invitation"))?
    };
    if invite.received.is_none() {
        if let Some(request) =
            crate::mesh::enroll::poll_invite(mesh.identity(), mesh.hub_url(), &code)
                .await
                .map_err(enrollment_error)?
        {
            invite.received = Some(request);
            let mut invitations = mesh.invites().lock().map_err(|_| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invitation state lock failed",
                )
            })?;
            invitations.insert(code, invite.clone());
        }
    }
    Ok(Json(invite_view(&s, &invite)))
}

#[derive(Deserialize)]
pub struct Confirmation {
    confirm: bool,
}

/// `POST /api/admin/mesh/invitations/{code}/admit`: the caller has compared the
/// displayed fingerprints and explicitly confirms before this node seals and
/// hands off the selected keys.
pub async fn admit_invitation(
    _admin: Administrator,
    State(s): State<AppState>,
    Path(code): Path<String>,
    Json(body): Json<Confirmation>,
) -> ApiResult<Json<Value>> {
    if !body.confirm {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm the compared fingerprint before admitting this member",
        ));
    }
    let mesh = hub(&s)?;
    let code = invitation_code(&code)?;
    let invite = {
        let invitations = mesh.invites().lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invitation state lock failed",
            )
        })?;
        invitations
            .get(&code)
            .cloned()
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "no such invitation"))?
    };
    let request = invite.received.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "the other node has not answered this invitation yet",
        )
    })?;
    let handoff = s
        .tools
        .broker
        .read()
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "credential store lock failed",
            )
        })?
        .bound_to(&request.node_id);
    crate::mesh::enroll::admit(
        s.store(),
        mesh.identity(),
        mesh.hub_url(),
        &request.node_id,
        &request.x25519_pub,
        &request.binding_sig,
        &request.name,
        &invite.channels,
        &handoff,
    )
    .await
    .map_err(enrollment_error)?;
    let mut admitted = invite;
    admitted.admitted = true;
    {
        let mut invitations = mesh.invites().lock().map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invitation state lock failed",
            )
        })?;
        invitations.insert(code, admitted.clone());
    }
    let directory_refresh_error = mesh
        .refresh_members()
        .await
        .err()
        .map(|error| error.to_string());
    Ok(Json(json!({
        "invitation": invite_view(&s, &admitted),
        "effect": "admitted this verified node and handed it exactly the invitation channels",
        "directory_refreshed": directory_refresh_error.is_none(),
        "directory_refresh_error": directory_refresh_error,
    })))
}

/// `DELETE /api/admin/mesh/invitations/{code}`: remove an unanswered
/// invitation. An already answered slot may already have been consumed by the
/// hub; the response only reports a successful cancellation from that hub.
pub async fn cancel_invitation(
    _admin: Administrator,
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> ApiResult<Json<Value>> {
    let mesh = hub(&s)?;
    let code = invitation_code(&code)?;
    crate::mesh::enroll::cancel_invite(mesh.identity(), mesh.hub_url(), &code)
        .await
        .map_err(enrollment_error)?;
    let mut invitations = mesh.invites().lock().map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invitation state lock failed",
        )
    })?;
    invitations.remove(&code);
    Ok(Json(json!({ "cancelled": code })))
}

/// `DELETE /api/admin/mesh/members/{id}`: revoke one hub membership. This
/// removes future hub routing and local channel grants after refresh; it cannot
/// retract frames, files, or data the member already received.
pub async fn remove_member(
    _admin: Administrator,
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Confirmation>,
) -> ApiResult<Json<Value>> {
    if !body.confirm {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm removal of this member",
        ));
    }
    let id = node_id(&id)?;
    if id == s.node_id {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "use the local mesh unpair flow for this serving node",
        ));
    }
    let mesh = hub(&s)?;
    crate::mesh::enroll::remove_member(mesh.identity(), mesh.hub_url(), id)
        .await
        .map_err(enrollment_error)?;
    let directory_refresh_error = mesh
        .refresh_members()
        .await
        .err()
        .map(|error| error.to_string());
    Ok(Json(json!({
        "removed": id,
        "effect": "removed the member from the hub and cleared its local channel grants when the directory refresh succeeds",
        "data_not_retracted": true,
        "directory_refreshed": directory_refresh_error.is_none(),
        "directory_refresh_error": directory_refresh_error,
    })))
}

#[derive(Deserialize)]
pub struct HubShareBody {
    channels: Vec<String>,
    confirm: bool,
}

/// `POST /api/admin/mesh/hub-share`: share existing selected channel keys with
/// the hub replica so that its opt-in aggregate can process those channels.
pub async fn share_with_hub(
    _admin: Administrator,
    State(s): State<AppState>,
    Json(body): Json<HubShareBody>,
) -> ApiResult<Json<Value>> {
    if !body.confirm {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm sharing these existing channel keys with the hub replica",
        ));
    }
    let channels = checked_channels(&s, body.channels)?;
    if channels.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "select at least one existing channel to share with the hub",
        ));
    }
    let mesh = hub(&s)?;
    let shared =
        crate::mesh::enroll::share_with_hub(s.store(), mesh.identity(), mesh.hub_url(), &channels)
            .await
            .map_err(enrollment_error)?;
    Ok(Json(json!({
        "shared": shared,
        "effect": "the hub replica now holds these channel keys for its declared processing role",
    })))
}

/// `GET /api/admin/maintenance`: bounded facts about this serving node's
/// service and runtime, not an arbitrary host command channel.
pub async fn maintenance(
    _admin: Administrator,
    State(s): State<AppState>,
    parts: Parts,
) -> ApiResult<Json<Value>> {
    let active_sessions = open_sessions(&s)?;
    let mut service = crate::service::diagnostics();
    if active_sessions > 0 {
        for capability in [
            &mut service.install,
            &mut service.uninstall,
            &mut service.restart,
        ] {
            if capability.available {
                capability.available = false;
                capability.reason = format!(
                    "{active_sessions} active session{} would be ended by a service lifecycle change",
                    if active_sessions == 1 { "" } else { "s" }
                );
                capability.recovery =
                    "Pause or stop every active session, then request this fixed lifecycle action again."
                        .into();
            }
        }
    }
    Ok(Json(json!({
        "local": auth::extensions_are_loopback(&parts.extensions),
        "service": service,
        "active_sessions": active_sessions,
        "boundary": api::node_json(&s)?,
        "mesh": s.mesh.as_ref().map(|mesh| mesh.snapshot()),
    })))
}

/// `POST /api/admin/maintenance/boundary-check`: rerun the fixed boundary
/// checks for this serving node. It accepts no executable, command, or path.
pub async fn boundary_check(
    _admin: Administrator,
    _local: Loopback,
    State(s): State<AppState>,
) -> ApiResult<Json<Value>> {
    api::recheck_boundary(State(s)).await
}

fn schedule_lifecycle(
    s: &AppState,
    confirm: bool,
    action: crate::service::LifecycleAction,
) -> ApiResult<Json<Value>> {
    if !confirm {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "confirm this lifecycle change for the serving node",
        ));
    }
    let active_sessions = open_sessions(s)?;
    if active_sessions > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "refusing lifecycle change: {active_sessions} active session{} would be ended; pause or stop them first",
                if active_sessions == 1 { "" } else { "s" }
            ),
        ));
    }
    crate::service::schedule(action)
        .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    let (operation, effect) = match action {
        crate::service::LifecycleAction::Install => (
            "install",
            "the fixed user-service installation and start were scheduled after this response; reconnect when the node answers",
        ),
        crate::service::LifecycleAction::Uninstall => (
            "uninstall",
            "the fixed user service removal was scheduled after this response; this interface disconnects, while node state and credentials remain",
        ),
        crate::service::LifecycleAction::Restart => (
            "restart",
            "the fixed user-service restart was scheduled after this response; reconnect and read diagnostics for the observed result",
        ),
    };
    Ok(Json(json!({
        "scheduled": true,
        "operation": operation,
        "target": "this serving node's fixed user service",
        "effect": effect,
    })))
}

/// `POST /api/admin/maintenance/install`: schedule only the existing fixed
/// user-service installation/start, never a caller-provided host command.
pub async fn install(
    _admin: Administrator,
    _local: Loopback,
    State(s): State<AppState>,
    Json(body): Json<Confirmation>,
) -> ApiResult<Json<Value>> {
    schedule_lifecycle(&s, body.confirm, crate::service::LifecycleAction::Install)
}

/// `POST /api/admin/maintenance/uninstall`: schedule only the existing fixed
/// user-service removal. Node data and credentials are deliberately retained.
pub async fn uninstall(
    _admin: Administrator,
    _local: Loopback,
    State(s): State<AppState>,
    Json(body): Json<Confirmation>,
) -> ApiResult<Json<Value>> {
    schedule_lifecycle(&s, body.confirm, crate::service::LifecycleAction::Uninstall)
}

/// `POST /api/admin/maintenance/restart`: schedule exactly the fixed
/// user-service restart. The result names a schedule, not a successful restart.
pub async fn restart(
    _admin: Administrator,
    _local: Loopback,
    State(s): State<AppState>,
    Json(body): Json<Confirmation>,
) -> ApiResult<Json<Value>> {
    schedule_lifecycle(&s, body.confirm, crate::service::LifecycleAction::Restart)
}
