//! The phones this node pushes to: what the service worker registers, and
//! what the operator sees and forgets from the Nodes screen.
//!
//! Registering wants the browser's session cookie, so a revoked or expired
//! login takes its devices with it. A browser on this machine has no cookie —
//! loopback never logs in — and registers as belonging to the machine.

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::{
    api::{ApiError, AppState},
    auth,
};
use crate::{
    notify::{self, webpush},
    store::{now_ms, PushSubscriptionRow},
};

type ApiResult<T> = Result<T, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, msg)
}

/// Who is registering: a logged-in browser, this machine, or nobody we can
/// tie a device to.
fn owner(headers: &HeaderMap, peer: Option<SocketAddr>) -> ApiResult<Option<String>> {
    if let Some(h) = auth::session_hash(headers) {
        return Ok(Some(h));
    }
    if peer.is_some_and(|a| a.ip().is_loopback()) {
        return Ok(None);
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "push subscriptions belong to a logged-in browser session",
    ))
}

/// The key a browser subscribes with.
pub async fn key(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let vapid = webpush::Vapid::load_or_generate(s.store());
    Ok(Json(json!({ "key": vapid.public_key_b64url() })))
}

pub async fn list(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let mine = auth::session_hash(&headers);
    let devices: Vec<_> = s
        .store()
        .push_subscriptions()?
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "user_agent": r.user_agent,
                "created_ms": r.created_ms,
                "last_ok_ms": r.last_ok_ms,
                "fail_count": r.fail_count,
                "local": r.session_hash.is_none(),
                "mine": r.session_hash.is_some() && r.session_hash == mine,
            })
        })
        .collect();
    Ok(Json(json!({ "devices": devices })))
}

/// `PushSubscription.toJSON()`, as the browser produces it.
#[derive(Deserialize)]
pub struct Subscription {
    endpoint: String,
    keys: Keys,
}

#[derive(Deserialize)]
pub struct Keys {
    p256dh: String,
    auth: String,
}

pub async fn subscribe(
    State(s): State<AppState>,
    headers: HeaderMap,
    peer: Result<ConnectInfo<SocketAddr>, axum::extract::rejection::ExtensionRejection>,
    Json(sub): Json<Subscription>,
) -> ApiResult<Json<serde_json::Value>> {
    let session_hash = owner(&headers, peer.ok().map(|p| p.0))?;
    webpush::audience(&sub.endpoint).map_err(|e| bad(e.to_string()))?;
    webpush::decode_keys(&sub.keys.p256dh, &sub.keys.auth).map_err(|e| bad(e.to_string()))?;
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(200).collect::<String>());
    let id = s.store().push_subscription_upsert(&PushSubscriptionRow {
        id: uuid::Uuid::now_v7().to_string(),
        session_hash,
        endpoint: sub.endpoint,
        p256dh: sub.keys.p256dh,
        auth: sub.keys.auth,
        user_agent,
        created_ms: now_ms(),
        last_ok_ms: None,
        fail_count: 0,
    })?;
    Ok(Json(json!({ "id": id })))
}

pub async fn forget(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if s.store().push_subscription_delete(&id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(StatusCode::NOT_FOUND, "no such device"))
    }
}

#[derive(Deserialize)]
pub struct ByEndpoint {
    endpoint: String,
}

/// The service worker knows its endpoint, not our id.
pub async fn forget_endpoint(
    State(s): State<AppState>,
    Json(b): Json<ByEndpoint>,
) -> ApiResult<StatusCode> {
    s.store().push_subscription_delete_endpoint(&b.endpoint)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct TestBody {
    /// Every device, not only this session's — for the CLI, which has none.
    #[serde(default)]
    all: bool,
}

/// A push that says nothing but "this works", so the operator can tell a
/// subscription that landed from one the phone silently dropped.
pub async fn test(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<TestBody>>,
) -> ApiResult<Json<serde_json::Value>> {
    let all = body.map(|b| b.0.all).unwrap_or(false);
    let mine = auth::session_hash(&headers);
    let targets: Vec<_> = s
        .store()
        .push_subscriptions_live(now_ms())?
        .into_iter()
        .filter(|r| all || r.session_hash.is_none() || r.session_hash == mine)
        .collect();
    let n = notify::Notification::test();
    let mut results = Vec::new();
    for r in targets {
        let outcome = notify::deliver(s.store(), &s.cfg, &r, &n, now_ms()).await;
        results.push(json!({ "id": r.id, "outcome": format!("{outcome:?}") }));
    }
    Ok(Json(json!({ "sent": results })))
}
