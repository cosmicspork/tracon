//! Per-request authentication: three hex headers (public key, timestamp,
//! signature) over the canonical descriptor from `proto::auth`. The hub holds
//! no keys; it reconstructs the descriptor from the actual request and
//! verifies. On success the request is tagged with its [`Owner`].

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::Response,
};
use proto::auth::{
    verify_request, AuthRequest, HEADER_PUBLIC_KEY, HEADER_SIGNATURE, HEADER_TIMESTAMP,
};

use crate::AppState;

/// Largest body the auth layer will buffer. Frames are capped lower at the
/// route; this bounds hostile requests before signature work.
pub const MAX_BODY: usize = proto::frame::MAX_FRAME_BYTES + 64 * 1024;

/// The authenticated caller's Ed25519 public key.
#[derive(Clone, Copy, Debug)]
pub struct Owner(pub [u8; 32]);

impl Owner {
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }
}

pub async fn require_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let (mut parts, body) = request.into_parts();

    let public_key = hex_header::<32>(&parts.headers, HEADER_PUBLIC_KEY)?;
    let signature = hex_header::<64>(&parts.headers, HEADER_SIGNATURE)?;
    let timestamp: u64 = parts
        .headers
        .get(HEADER_TIMESTAMP)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let now = now_unix();
    if now.abs_diff(timestamp) > state.cfg.max_skew_secs {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let method = parts.method.as_str();
    let path = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| parts.uri.path());

    let bytes = to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;

    let auth = AuthRequest::new(method, path, &bytes, timestamp);
    if !verify_request(&public_key, &signature, &auth) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Replay guard for state-changing methods only; reads are idempotent and
    // second-granular timestamps would falsely reject a repeated poll.
    if !matches!(parts.method, Method::GET | Method::HEAD) {
        let expires_at = timestamp.saturating_add(state.cfg.max_skew_secs);
        if !state.nonces.check_and_remember(&signature, expires_at, now) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    parts.extensions.insert(Owner(public_key));
    Ok(next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await)
}

fn hex_header<const N: usize>(headers: &HeaderMap, name: &str) -> Result<[u8; N], StatusCode> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or(StatusCode::UNAUTHORIZED)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
