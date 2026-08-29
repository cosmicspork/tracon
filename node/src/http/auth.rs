//! Operator authentication.
//!
//! Loopback callers are the operator by definition — the node's state is their
//! state, and a shell on the machine already has it. That is the whole story
//! until a token is issued. Issuing one (`tracon auth issue`) opens a second
//! door: a client that presents the token gets a cookie, and the cookie is what
//! reaches the node from anywhere else. Nothing else changes; loopback keeps
//! working credential-less so the CLI, `just dev`, and `kubectl port-forward`
//! are untouched.
//!
//! What is stored is only ever a SHA-256: of the token, and of each cookie. A
//! read of the database cannot mint either one.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{host_is_local, hostname};
use crate::store::{now_ms, AuthSessionRow, Store, OPERATOR_TOKEN_KEY};

use super::api::{ApiError, AppState};

/// Cookie name for a logged-in client.
pub const COOKIE: &str = "tracon_session";
/// How long a cookie lives without being used.
const SESSION_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Renew when more than a third of the life has been consumed, so an active
/// client is extended about monthly rather than on every request.
const RENEW_AFTER_MS: i64 = SESSION_TTL_MS / 3;
/// Failed logins tolerated per window, per address and in total. Behind an
/// ingress every caller shares one address, so the global cap is the real
/// limit; a 256-bit token is not reachable at this rate either way.
const LOGIN_WINDOW_MS: i64 = 5 * 60 * 1000;
const LOGIN_PER_IP: u32 = 10;
const LOGIN_GLOBAL: u32 = 50;

pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// A fixed window of failed logins. Cleared wholesale when the window turns, so
/// it cannot grow without bound.
#[derive(Default)]
struct Limiter {
    window_start_ms: i64,
    global: u32,
    per_ip: HashMap<IpAddr, u32>,
}

impl Limiter {
    /// Whether another attempt is allowed, counting it if so.
    fn allow(&mut self, ip: Option<IpAddr>, now: i64) -> bool {
        if now - self.window_start_ms >= LOGIN_WINDOW_MS {
            self.window_start_ms = now;
            self.global = 0;
            self.per_ip.clear();
        }
        if self.global >= LOGIN_GLOBAL {
            return false;
        }
        if let Some(ip) = ip {
            let n = self.per_ip.entry(ip).or_insert(0);
            if *n >= LOGIN_PER_IP {
                return false;
            }
            *n += 1;
        }
        self.global += 1;
        true
    }
}

/// What the guard needs on every request: the address the node was told to
/// bind, whether a token is configured (cached so the common path does not
/// touch the database), and the login limiter.
pub struct AuthState {
    bind: String,
    token_hash: Mutex<Option<String>>,
    limiter: Mutex<Limiter>,
}

impl AuthState {
    pub fn new(bind: String, token_hash: Option<String>) -> Self {
        Self {
            bind,
            token_hash: Mutex::new(token_hash),
            limiter: Mutex::new(Limiter::default()),
        }
    }

    pub fn load(store: &Store, bind: String) -> Self {
        Self::new(bind, store.kv_get(OPERATOR_TOKEN_KEY).ok().flatten())
    }

    fn configured(&self) -> Option<String> {
        self.token_hash.lock().unwrap().clone()
    }

    fn set_configured(&self, hash: Option<String>) {
        *self.token_hash.lock().unwrap() = hash;
    }
}

/// The value of one cookie in a `Cookie:` header.
pub(crate) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// The hash of the session cookie a request carries, if any — what a push
/// subscription is tied to. The guard has already checked it is live.
pub(crate) fn session_hash(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, COOKIE).map(hash)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// A path the guard lets through unauthenticated when a token is configured:
/// the login endpoint itself, and the SPA shell and its assets. The shell is
/// open-source code and holds nothing; serving it is what lets the login screen
/// render and lets a notification's deep link open at all. Every `/api` path
/// other than login stays gated.
fn is_public(req: &Request) -> bool {
    let path = req.uri().path();
    if path == "/api/login" {
        return true;
    }
    req.method() == axum::http::Method::GET && !path.starts_with("/api/")
}

/// Whether the caller is on this machine. A request with no peer address is
/// treated as remote: the guard fails closed.
fn peer_is_loopback(req: &Request) -> bool {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ci| ci.0.ip().is_loopback())
}

fn peer_ip(req: &Request) -> Option<IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
}

/// The one gate on the operator API.
///
/// Order matters and is deny-safe: loopback keeps today's behaviour, a node
/// with no token configured answers nobody else at all, and only then is a
/// credential considered.
pub async fn guard(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let auth = state.auth.clone();
    let headers = req.headers().clone();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .filter(|o| *o != "null");

    // 1. On this machine, with a local Host and Origin: the operator.
    let local_host = host_is_local(host, &auth.bind)
        && origin.is_none_or(|o| host_is_local(Some(o), &auth.bind));
    if peer_is_loopback(&req) && local_host {
        return Ok(next.run(req).await);
    }

    // 2. No token issued: the node answers loopback only, as it always has.
    let Some(configured) = auth.configured() else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this node answers only to loopback; run `tracon auth issue` to reach it from elsewhere",
        ));
    };

    if is_public(&req) {
        // Bound the work a stranger can ask of the login path. Counting every
        // attempt rather than only the failures is the same bound and needs no
        // answer from the handler.
        if req.uri().path() == "/api/login"
            && !auth.limiter.lock().unwrap().allow(peer_ip(&req), now_ms())
        {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts; wait a few minutes",
            ));
        }
        return Ok(next.run(req).await);
    }

    // 3. A cookie, or the token itself for non-browser clients.
    let now = now_ms();
    let store = state.store().clone();
    let mut renew: Option<String> = None;
    let authenticated = if let Some(secret) = cookie_value(&headers, COOKIE) {
        let h = hash(secret);
        match store.auth_session_live(&h, now) {
            Ok(Some(row)) => {
                if now - (row.expires_ms - SESSION_TTL_MS) > RENEW_AFTER_MS {
                    let _ = store.auth_session_touch(&h, now, now + SESSION_TTL_MS);
                    renew = Some(secret.to_string());
                }
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "auth session lookup failed");
                false
            }
        }
    } else {
        bearer(&headers).is_some_and(|t| hash(t) == configured)
    };

    if !authenticated {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "authentication required",
        ));
    }

    // A credential is not enough on its own: a cross-site page carries its own
    // Origin, and the browser attaches the cookie to the request it makes to
    // this node. Requiring the two to agree is what keeps that page from
    // driving the API.
    if let Some(o) = origin {
        let same = host.is_some_and(|h| hostname(o) == hostname(h));
        if !same {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "cross-origin request refused",
            ));
        }
    }

    let mut response = next.run(req).await;
    if let Some(secret) = renew {
        set_cookie(&mut response, &secret, SESSION_TTL_MS / 1000);
    }
    Ok(response)
}

fn set_cookie(response: &mut Response, secret: &str, max_age_secs: i64) {
    // SameSite=Lax rather than Strict: opening a review from a push
    // notification is a top-level cross-site navigation, and Strict would drop
    // the cookie on exactly that. Lax still withholds it from cross-site
    // writes, which every mutating call here is.
    let v = format!(
        "{COOKIE}={secret}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age_secs}"
    );
    if let Ok(value) = axum::http::HeaderValue::from_str(&v) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub token: String,
}

/// Exchange the operator token for a cookie.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<Response, ApiError> {
    let auth = state.auth.clone();
    let now = now_ms();
    let Some(configured) = auth.configured() else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "no operator token is set on this node",
        ));
    };
    // Bounded work before hashing; the token this issues is 43 characters.
    if body.token.len() > 512 || hash(&body.token) != configured {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "token not accepted",
        ));
    }

    let store = state.store();
    let _ = store.auth_sessions_purge(now);
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    store
        .auth_session_insert(&AuthSessionRow {
            token_hash: hash(&secret),
            created_ms: now,
            last_seen_ms: now,
            expires_ms: now + SESSION_TTL_MS,
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(200).collect()),
        })
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut response = Json(json!({ "ok": true })).into_response();
    set_cookie(&mut response, &secret, SESSION_TTL_MS / 1000);
    Ok(response)
}

/// Drop this client's cookie. Other devices keep theirs.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(secret) = cookie_value(&headers, COOKIE) {
        let _ = state.store().auth_session_delete(&hash(secret));
        // Its devices go with it; the JOIN would have silenced them anyway,
        // but a listed device that can never be reached is a lie.
        let _ = state.store().auth_sessions_purge(now_ms());
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    set_cookie(&mut response, "", 0);
    response
}

#[derive(Deserialize)]
pub struct TokenBody {
    /// SHA-256 of the operator token, hex. The CLI hashes it so the token
    /// itself never crosses the wire, not even over loopback.
    pub token_hash: String,
}

/// Set (or replace) the operator token. Every existing cookie dies with it.
pub async fn put_token(
    State(state): State<AppState>,
    Json(body): Json<TokenBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let h = body.token_hash.trim().to_lowercase();
    if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "token_hash must be a hex SHA-256",
        ));
    }
    state
        .store()
        .set_operator_token(Some(&h))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.auth.set_configured(Some(h));
    Ok(Json(json!({ "ok": true })))
}

/// Remove the operator token: the node answers loopback only again.
pub async fn delete_token(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .store()
        .set_operator_token(None)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.auth.set_configured(None);
    Ok(Json(json!({ "ok": true })))
}

/// The clients holding a live cookie, so the operator can see what is logged in.
pub async fn list_sessions(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let now = now_ms();
    let _ = state.store().auth_sessions_purge(now);
    let rows = state
        .store()
        .auth_sessions()
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // The hash is the credential's fingerprint, not the credential; a short
    // prefix is enough to tell two clients apart when revoking.
    let clients: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({
                "id": &r.token_hash[..12],
                "created_ms": r.created_ms,
                "last_seen_ms": r.last_seen_ms,
                "expires_ms": r.expires_ms,
                "user_agent": r.user_agent,
            })
        })
        .collect();
    Ok(Json(json!({
        "configured": state.auth.configured().is_some(),
        "clients": clients,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: header::HeaderName, v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn cookie_is_picked_out_of_a_crowd() {
        let h = headers_with(header::COOKIE, "a=1; tracon_session=abc; b=2");
        assert_eq!(cookie_value(&h, COOKIE), Some("abc"));
        let h = headers_with(header::COOKIE, "tracon_session=only");
        assert_eq!(cookie_value(&h, COOKIE), Some("only"));
        let h = headers_with(header::COOKIE, "other=1");
        assert_eq!(cookie_value(&h, COOKIE), None);
        assert_eq!(cookie_value(&HeaderMap::new(), COOKIE), None);
    }

    #[test]
    fn bearer_needs_the_scheme() {
        let h = headers_with(header::AUTHORIZATION, "Bearer tok");
        assert_eq!(bearer(&h), Some("tok"));
        let h = headers_with(header::AUTHORIZATION, "Basic tok");
        assert_eq!(bearer(&h), None);
    }

    #[test]
    fn hashing_is_stable_and_hides_the_token() {
        let h = hash("trc1.example");
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash("trc1.example"));
        assert_ne!(h, hash("trc1.exampl"));
        assert!(!h.contains("example"));
    }

    #[test]
    fn the_limiter_caps_per_address_then_the_window_turns() {
        let mut l = Limiter::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..LOGIN_PER_IP {
            assert!(l.allow(Some(ip), 1_000));
        }
        assert!(!l.allow(Some(ip), 1_000));
        // A different address has its own budget, up to the global cap.
        assert!(l.allow(Some("10.0.0.2".parse().unwrap()), 1_000));
        // The window turning clears both.
        assert!(l.allow(Some(ip), 1_000 + LOGIN_WINDOW_MS));
    }

    #[test]
    fn the_limiter_caps_everyone_together_behind_one_address() {
        let mut l = Limiter::default();
        // No peer address (or one shared by every caller): the global cap is
        // what holds, and it does.
        for _ in 0..LOGIN_GLOBAL {
            assert!(l.allow(None, 0));
        }
        assert!(!l.allow(None, 0));
        assert!(!l.allow(Some("10.0.0.1".parse().unwrap()), 0));
    }
}
