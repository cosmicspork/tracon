//! Passkeys: what confirms an admin action made from off the node's machine.
//!
//! The cookie a paired browser holds is enough to use the node. It is not
//! enough to change who may use it, what credentials it lends, or whom it
//! meshes with: off this machine, each such action ([`AdminAction`]) must be
//! confirmed by a passkey assertion made for that action and no other.
//!
//! - **Pairing.** Spending the operator token (`tracon auth issue --url`)
//!   gives a browser its cookie and opens a short window
//!   ([`ENROLL_WINDOW_MS`]) in which that session may enroll the node's
//!   *first* passkey without one. After that, adding or removing a passkey is
//!   itself an admin action. A leaked token therefore stops being an admin
//!   credential the moment the operator has enrolled.
//! - **Step-up.** An admin request without a confirmation is refused with a
//!   typed `step_up_required` that carries an assertion challenge bound to
//!   `op_hash`, the hash of exactly that request ([`op_hash`]). The client
//!   asserts, receives a single-use proof bound to the same `op_hash` and its
//!   own cookie session, and sends the request again with the proof in
//!   [`STEP_UP_HEADER`]. Challenge and proof each live two minutes and are
//!   spent once.
//! - **Local.** A request that [is local](super::auth::is_local) passes
//!   without one: the operator at the node's own machine (or through
//!   `kubectl port-forward`) is the recovery root, and the desktop app never
//!   sees a prompt.
//! - **Bearer.** A bearer token never confirms anything off this machine: a
//!   confirmation belongs to a cookie session, and a bearer request has none.
//!
//! The relying party is pinned to `[auth] public_origin`. Without one the
//! node cannot say which origin its passkeys belong to, so every passkey
//! route and every remote admin action refuses, saying so.

use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    extract::{FromRequest, OriginalUri, Path, Request, State},
    http::{request::Parts, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder,
};

use super::{
    api::{ApiError, AppState},
    auth::{self, COOKIE},
};
use crate::{
    config::Config,
    store::{now_ms, CeremonyPurpose, CeremonyRow, PasskeyRow},
};

/// The header a retried admin request carries its step-up proof in.
pub const STEP_UP_HEADER: &str = "x-tracon-step-up";
/// The longest summary a confirmation prompt shows. A `node.toml` save's
/// lists every sensitive key it changes with its new value
/// (`config_prompt`), so it is longer than a route's one line.
pub const MAX_SUMMARY: usize = 600;
/// The longest line a device-approval prompt shows: a summary with what
/// it is for in front ("Confirm for another window: "), whole.
pub const MAX_APPROVAL_LINE: usize = MAX_SUMMARY + 40;
/// How long a challenge may be answered.
pub const CHALLENGE_TTL_MS: i64 = 2 * 60 * 1000;
/// How long a proof may be spent once its assertion verified.
pub const PROOF_TTL_MS: i64 = 2 * 60 * 1000;
/// How long after pairing a session may enroll the node's first passkey.
pub const ENROLL_WINDOW_MS: i64 = 15 * 60 * 1000;
/// The largest admin request body the node hashes. Every admin body is a
/// small JSON object; a credentials paste is the largest.
const MAX_ADMIN_BODY: usize = 1024 * 1024;
/// What a local caller's ceremonies are bound to in place of a cookie.
/// Never a session hash, which is 64 hex digits.
const LOCAL: &str = "local";
/// The longest name a passkey may be given.
const MAX_LABEL: usize = 64;

/// What a challenge's `granted_by` says when its assertion may only be made
/// by the browser that asked: confirming a device approval with another
/// device approval would let one browser approve on the strength of whatever
/// approves the next, so the approving browser's own passkey must answer.
pub(crate) const NOT_DELEGABLE: &str = "not-delegable";

/// Text from outside the node (an agent's arguments, a client's user
/// agent, a path) made fit to put in front of the operator: no control,
/// format (bidirectional overrides, zero-width joiners and the like), line or
/// paragraph separator characters, trimmed, and at most `max` characters.
pub(crate) fn clean_text(value: &str, max: usize) -> String {
    let kept: String = value.chars().filter(|c| !hidden(*c)).collect();
    kept.trim().chars().take(max).collect()
}

/// [`clean_text`] for a line a prompt shows whole: cut only if it is
/// longer than `max`, and then visibly, ending in an ellipsis, so nothing is
/// ever dropped from a prompt without its saying so.
pub(crate) fn fit_text(value: &str, max: usize) -> String {
    let clean = clean_text(value, usize::MAX);
    match clean.chars().count() > max {
        true => format!(
            "{}\u{2026}",
            clean
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        ),
        false => clean,
    }
}

/// Unicode general categories Cc, Cf, Zl and Zp.
pub(crate) fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c as u32,
            0x00AD
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x0890..=0x0891
                | 0x08E2
                | 0x180E
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
                | 0xFFF9..=0xFFFB
                | 0x110BD
                | 0x110CD
                | 0x13430..=0x1343F
                | 0x1BCA0..=0x1BCA3
                | 0x1D173..=0x1D17A
                | 0xE0001
                | 0xE0020..=0xE007F
        )
}

/// The operation a confirmation is for: the hash of the request's method,
/// target (path and query), the version it expects to change (`If-Match`,
/// when it sends one) and its exact body bytes. The client never computes
/// it: the node hashes the request it refused and the request it is sent
/// again, and the two must agree.
pub fn op_hash(method: &Method, target: &str, expected: Option<&str>, body: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"tracon-admin-op/1\n");
    h.update(method.as_str().as_bytes());
    h.update(b"\n");
    h.update(target.as_bytes());
    h.update(b"\n");
    h.update(expected.unwrap_or("").as_bytes());
    h.update(b"\n");
    h.update(hex::encode(Sha256::digest(body)).as_bytes());
    hex::encode(h.finalize())
}

/// The request target as the client sent it: path and query.
fn target(parts: &Parts) -> String {
    let uri = parts
        .extensions
        .get::<OriginalUri>()
        .map(|o| &o.0)
        .unwrap_or(&parts.uri);
    uri.path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string())
}

fn expected(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
}

fn parts_op_hash(parts: &Parts, body: &[u8]) -> String {
    op_hash(
        &parts.method,
        &target(parts),
        expected(&parts.headers),
        body,
    )
}

/// The relying party this node's passkeys belong to, from `[auth]
/// public_origin`, or why there is none.
pub fn relying_party(cfg: &Config) -> Result<Webauthn, Refusal> {
    let Some(origin) = cfg.public_origin() else {
        return Err(Refusal::unavailable(
            "no_public_origin",
            "passkeys need [auth] public_origin: set it to the https origin this node is reached at, then restart",
        ));
    };
    let url = Url::parse(&origin).map_err(|_| {
        Refusal::unavailable("no_public_origin", "[auth] public_origin is not a URL")
    })?;
    let Some(rp_id) = url.domain().map(str::to_owned) else {
        return Err(Refusal::unavailable(
            "no_public_origin",
            "[auth] public_origin must name a domain",
        ));
    };
    WebauthnBuilder::new(&rp_id, &url)
        .and_then(|b| {
            b.rp_name("tracon")
                .timeout(Duration::from_millis(CHALLENGE_TTL_MS as u64))
                .build()
        })
        .map_err(|e| {
            Refusal::unavailable(
                "no_public_origin",
                format!("[auth] public_origin cannot anchor passkeys: {e}"),
            )
        })
}

/// Who is asking, as far as passkeys are concerned.
pub(crate) enum Caller {
    /// At the node's own machine.
    Local,
    /// A browser with a live cookie session (its hash).
    Session(String),
    /// Anything else: a bearer token, or no credential the guard knew.
    Other,
}

pub(crate) fn caller(s: &AppState, parts: &Parts) -> Caller {
    if auth::parts_are_local(parts) {
        return Caller::Local;
    }
    let Some(secret) = auth::cookie_value(&parts.headers, COOKIE) else {
        return Caller::Other;
    };
    let h = auth::hash(secret);
    match s.store().auth_session_live(&h, now_ms()) {
        Ok(Some(_)) => Caller::Session(h),
        _ => Caller::Other,
    }
}

/// Why an admin action or a passkey route did not go ahead.
pub enum Refusal {
    /// Confirm with a passkey: the challenge for exactly this request.
    Required {
        message: String,
        step_up: Value,
    },
    /// No confirmation could satisfy this request as it stands.
    Unavailable {
        reason: &'static str,
        message: String,
    },
    Api(ApiError),
}

impl Refusal {
    pub(crate) fn unavailable(reason: &'static str, message: impl Into<String>) -> Self {
        Refusal::Unavailable {
            reason,
            message: message.into(),
        }
    }
}

impl From<ApiError> for Refusal {
    fn from(e: ApiError) -> Self {
        Refusal::Api(e)
    }
}

impl From<crate::config::EditError> for Refusal {
    fn from(e: crate::config::EditError) -> Self {
        Refusal::Api(ApiError::from(e))
    }
}

impl From<crate::store::StoreError> for Refusal {
    fn from(e: crate::store::StoreError) -> Self {
        Refusal::Api(ApiError::from(e))
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        // The same `error` object every other refusal carries, so a client
        // that knows nothing of passkeys still shows the message; `kind` is
        // what one that does acts on.
        match self {
            Refusal::Required { message, step_up } => (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": { "code": 403, "kind": "step_up_required", "message": message },
                    "step_up": step_up,
                })),
            )
                .into_response(),
            Refusal::Unavailable { reason, message } => (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": {
                        "code": 403,
                        "kind": "step_up_unavailable",
                        "reason": reason,
                        "message": message,
                    },
                })),
            )
                .into_response(),
            Refusal::Api(e) => e.into_response(),
        }
    }
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Refusal {
    Refusal::Api(ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        e.to_string(),
    ))
}

pub(crate) fn limited(s: &AppState, session: &str) -> Result<(), Refusal> {
    if s.auth.allow_ceremony(session, now_ms()) {
        Ok(())
    } else {
        Err(Refusal::Api(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many passkey prompts; wait a few minutes",
        )))
    }
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn random_secret() -> String {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    b64(&raw)
}

/// Every enrolled passkey, as the verifier wants them.
fn enrolled(s: &AppState) -> Result<Vec<(PasskeyRow, Passkey)>, Refusal> {
    s.store()
        .passkeys()?
        .into_iter()
        .map(|row| {
            let passkey: Passkey = serde_json::from_str(&row.passkey).map_err(internal)?;
            Ok((row, passkey))
        })
        .collect()
}

/// Open an assertion for `op_hash` on behalf of `session`, remembering what
/// it confirms (`action`, `summary`, cleaned) for a device approval made from
/// it, and whether one may be made from it at all (`delegable`).
fn open_assertion(
    s: &AppState,
    rp: &Webauthn,
    session: &str,
    op_hash: &str,
    action: Option<&str>,
    summary: Option<&str>,
    delegable: bool,
) -> Result<Value, Refusal> {
    let action = action.map(|a| clean_text(a, 160));
    let summary = summary.map(|a| fit_text(a, MAX_SUMMARY));
    let (action, summary) = (action.as_deref(), summary.as_deref());
    let passkeys: Vec<Passkey> = enrolled(s)?.into_iter().map(|(_, p)| p).collect();
    if passkeys.is_empty() {
        return Err(Refusal::unavailable(
            "no_passkey",
            "no passkey is enrolled on this node; add one in Settings, or do this at the node",
        ));
    }
    let (options, state) = rp
        .start_passkey_authentication(&passkeys)
        .map_err(internal)?;
    let now = now_ms();
    let id = random_secret();
    let expires_ms = now + CHALLENGE_TTL_MS;
    s.store().ceremony_insert(
        &CeremonyRow {
            id: id.clone(),
            purpose: CeremonyPurpose::Assert,
            op_hash: Some(op_hash.to_string()),
            session_hash: session.to_string(),
            granted_by: (!delegable).then(|| NOT_DELEGABLE.to_string()),
            label: None,
            state: serde_json::to_string(&state).map_err(internal)?,
            created_ms: now,
            expires_ms,
            action: action.map(str::to_string),
            summary: summary.map(str::to_string),
        },
        now,
    )?;
    let mut step_up = json!({
        "op_hash": op_hash,
        "challenge_id": id,
        "options": options,
        "expires_ms": expires_ms,
    });
    if let Some(action) = action {
        step_up["action"] = json!(action);
    }
    if let Some(summary) = summary {
        step_up["summary"] = json!(summary);
    }
    Ok(step_up)
}

/// Whether this admin request may go ahead: local, or confirmed by a step-up
/// proof for exactly this request. Otherwise the refusal says what would
/// confirm it, opening the challenge when a passkey can.
pub async fn authorize(s: &AppState, parts: &Parts, body: &[u8]) -> Result<(), Refusal> {
    authorize_as(
        s,
        parts,
        body,
        route_summary(&parts.method, parts.uri.path()),
    )
    .await
}

/// What a route does, in a line, for the prompt that confirms it. From the
/// route the node is about to run, never from anything the client says about
/// it; `None` where the method and path say it well enough.
pub fn route_summary(method: &Method, path: &str) -> Option<String> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let m = method.as_str();
    let text = match (m, segs.as_slice()) {
        ("POST", ["api", "auth", "token"]) => "Replace the operator token".to_string(),
        ("DELETE", ["api", "auth", "token"]) => "Remove the operator token".to_string(),
        ("POST", ["api", "authority", "grants"]) => "Create an authority grant".to_string(),
        ("DELETE", ["api", "authority", "grants", _]) => "Revoke an authority grant".to_string(),
        ("PUT", ["api", "channels", name, "bindings"]) => format!("Change {name}'s bindings"),
        ("POST", ["api", "credentials", "import"]) => "Import credentials".to_string(),
        ("POST", ["api", "credentials", name, "share"]) => format!("Share the {name} credential"),
        ("DELETE", ["api", "credentials", name]) => format!("Remove the {name} credential"),
        ("PUT", ["api", "credentials", "forge", forge]) => format!("Set the {forge} token"),
        ("DELETE", ["api", "credentials", "forge", forge]) => format!("Remove the {forge} token"),
        ("POST", ["api", "providers", name, "connect"])
        | ("POST", ["api", "nodes", _, "providers", name, "connect"]) => {
            format!("Sign in to {name}")
        }
        ("POST", ["api", "providers", name, "disconnect"])
        | ("POST", ["api", "nodes", _, "providers", name, "disconnect"]) => {
            format!("Sign out of {name}")
        }
        ("PUT", ["api", "providers", name, "key"]) => format!("Set the {name} API key"),
        ("PUT", ["api", "providers", name, "channels"]) => {
            format!("Change which channels {name} serves")
        }
        ("POST", ["api", "boundary", "setup"]) => "Set up the boundary".to_string(),
        ("POST", ["api", "admin", "mesh", "invitations"]) => {
            "Invite a node to the mesh".to_string()
        }
        ("POST", ["api", "admin", "mesh", "invitations", _, "admit"]) => {
            "Admit a node to the mesh".to_string()
        }
        ("DELETE", ["api", "admin", "mesh", "members", _]) => {
            "Remove a node from the mesh".to_string()
        }
        ("POST", ["api", "admin", "mesh", "hub-share"]) => {
            "Share channels with the hub".to_string()
        }
        ("POST", ["api", "external", channel, "start"])
        | ("DELETE", ["api", "external", channel, "stop"]) => {
            format!("Reopen {channel} to external harnesses")
        }
        ("POST", ["api", "reviews", _, "review-session"]) => {
            "Start a review session on a review".to_string()
        }
        ("POST", ["api", "repos", "offer"]) => {
            "Offer a repository recipe to channel members".to_string()
        }
        ("POST", ["api", "repos", "offers", _, "accept"]) => {
            "Accept a repository recipe offer into node.toml".to_string()
        }
        ("POST", ["api", "admin", "restart"]) => "Restart the node".to_string(),
        ("DELETE", ["api", "admin", "restart"]) => "Withdraw the node's restart".to_string(),
        ("POST", ["api", "passkeys", "register", "options"]) => "Add a passkey".to_string(),
        ("POST", ["api", "mesh", "unpair"]) => "Leave the mesh (forget the hub)".to_string(),
        ("DELETE", ["api", "passkeys", _]) => "Remove a passkey".to_string(),
        _ => return None,
    };
    // Path segments are percent-encoded and bounded only by the URL; keep
    // the line short and printable.
    Some(clean_text(&text, 120))
}

/// [`authorize`], with the line the prompt shows for what is confirmed.
pub async fn authorize_as(
    s: &AppState,
    parts: &Parts,
    body: &[u8],
    summary: Option<String>,
) -> Result<(), Refusal> {
    let action = format!("{} {}", parts.method, parts.uri.path());
    authorize_op(
        s,
        parts,
        || parts_op_hash(parts, body),
        &action,
        summary.as_deref(),
        true,
    )
}

/// Whether the operation `op` names may go ahead for this caller: local, or
/// confirmed by a step-up proof for exactly `op` from this cookie session.
/// Otherwise the refusal opens the challenge for it. `op` is computed by the
/// node, from the request or from what the node stored; `action` and
/// `summary` are what the prompt shows for it. Without `delegable`, the
/// challenge cannot be handed to a device approval (`http::device`).
pub(crate) fn authorize_op(
    s: &AppState,
    parts: &Parts,
    op: impl FnOnce() -> String,
    action: &str,
    summary: Option<&str>,
    delegable: bool,
) -> Result<(), Refusal> {
    let session = match caller(s, parts) {
        Caller::Local => return Ok(()),
        Caller::Session(h) => h,
        Caller::Other => {
            return Err(Refusal::unavailable(
                "no_session",
                "an admin action from off the node needs a passkey, and a bearer token cannot \
                 present one: confirm it from a signed-in browser, or run it at the node",
            ))
        }
    };
    let rp = relying_party(&s.cfg)?;
    let op = op();
    let mut message = "confirm this with your passkey".to_string();
    if let Some(proof) = parts
        .headers
        .get(STEP_UP_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        if s.store()
            .proof_spend(&auth::hash(proof.trim()), &op, &session, now_ms())?
        {
            tracing::info!(
                action = %action,
                session = %&session[..12.min(session.len())],
                op_hash = %op,
                "admin action confirmed with a passkey"
            );
            return Ok(());
        }
        message = "that confirmation was for another action, has expired, or was already used; \
                   confirm this one with your passkey"
            .into();
    }
    limited(s, &session)?;
    let step_up = open_assertion(s, &rp, &session, &op, Some(action), summary, delegable)?;
    Err(Refusal::Required { message, step_up })
}

/// An admin body is JSON or nothing. What is confirmed is the bytes, and a
/// non-JSON body would be read by an optional extractor as no body at all,
/// so the request run would not be the request confirmed.
pub fn json_or_empty(headers: &HeaderMap, body: &[u8]) -> Result<(), ApiError> {
    if body.is_empty() {
        return Ok(());
    }
    let json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|m| m.trim().to_ascii_lowercase())
        .is_some_and(|m| {
            m == "application/json" || (m.starts_with("application/") && m.ends_with("+json"))
        });
    if json {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "an admin request's body must be JSON (Content-Type: application/json)",
        ))
    }
}

/// A request that changes who may use the node or what it holds for them.
///
/// Wraps the handler's body extractor: `AdminAction(Json(body)):
/// AdminAction<Json<T>>`, or `AdminAction(()): AdminAction` for a request
/// with no body. It reads the body once, decides with [`authorize`], and
/// hands the same bytes to the inner extractor, so what was confirmed is what
/// the handler parses.
pub struct AdminAction<T = ()>(pub T);

impl<T, M> FromRequest<AppState, M> for AdminAction<T>
where
    T: FromRequest<AppState, M>,
    M: Send + 'static,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, MAX_ADMIN_BODY)
            .await
            .map_err(|_| {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
                    .into_response()
            })?;
        json_or_empty(&parts.headers, &bytes).map_err(IntoResponse::into_response)?;
        authorize(state, &parts, &bytes)
            .await
            .map_err(IntoResponse::into_response)?;
        let req = Request::from_parts(parts, Body::from(bytes));
        T::from_request(req, state)
            .await
            .map(AdminAction)
            .map_err(IntoResponse::into_response)
    }
}

/// An admin request whose body is read before it is confirmed, so the
/// prompt can say what this body does rather than what the route is: the
/// keys a `node.toml` edit changes, the hub a mesh is started with. The
/// summary is the node's, computed from the very bytes the confirmation is
/// bound to (`op_hash`), never a description the client sent.
///
/// Nothing may act on the request before [`AdminRequest::confirm`] has
/// passed. What may come before it is only what the node can answer without
/// looking past its own state: parsing, a stale version, a value that would
/// not load.
pub struct AdminRequest {
    parts: Parts,
    body: Bytes,
}

impl FromRequest<AppState> for AdminRequest {
    type Rejection = Response;

    async fn from_request(req: Request, _state: &AppState) -> Result<Self, Self::Rejection> {
        let (parts, body) = req.into_parts();
        let body = axum::body::to_bytes(body, MAX_ADMIN_BODY)
            .await
            .map_err(|_| {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
                    .into_response()
            })?;
        json_or_empty(&parts.headers, &body).map_err(IntoResponse::into_response)?;
        Ok(AdminRequest { parts, body })
    }
}

impl AdminRequest {
    /// The body, read as `T`. An empty body reads as JSON `null`.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, ApiError> {
        let bytes: &[u8] = match self.body.is_empty() {
            true => b"null",
            false => &self.body,
        };
        serde_json::from_slice(bytes).map_err(|e| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("the request body does not read: {e}"),
            )
        })
    }

    /// The version the request expects to change, from `If-Match`, with any
    /// quotes an entity tag carries taken off.
    pub fn if_match(&self) -> Option<&str> {
        expected(&self.parts.headers).map(|v| v.trim().trim_matches('"'))
    }

    /// The request's head: what a confirmation for an operation the node
    /// works out from the body itself (`peer_admin`) is decided on.
    pub(crate) fn parts(&self) -> &Parts {
        &self.parts
    }

    /// Whether this is the operator at the node's own machine, who confirms
    /// nothing.
    pub fn is_local(&self) -> bool {
        auth::parts_are_local(&self.parts)
    }

    /// Go ahead only if this request is local, or confirmed with a passkey
    /// for exactly these bytes; otherwise the refusal opens the challenge,
    /// with `summary` as the line the prompt shows.
    pub async fn confirm(&self, s: &AppState, summary: &str) -> Result<(), Refusal> {
        authorize_as(s, &self.parts, &self.body, Some(summary.to_string())).await
    }
}

/// Whether `session` may enroll the node's first passkey without one: none is
/// enrolled yet, and it paired within [`ENROLL_WINDOW_MS`].
fn enrollment_closes(s: &AppState, session: &str, now: i64) -> Result<Option<i64>, Refusal> {
    if s.store().passkey_count()? > 0 {
        return Ok(None);
    }
    let Some(row) = s.store().auth_session_live(session, now)? else {
        return Ok(None);
    };
    Ok(row
        .paired_ms
        .map(|paired| paired + ENROLL_WINDOW_MS)
        .filter(|closes| *closes > now))
}

/// Tell the operator's devices that the set of passkeys changed: an
/// enrollment they did not make is the one sign of a raced pairing window or
/// a confirmed action they did not intend. Detached, so a slow push service
/// never holds the answer up.
fn announce(s: &AppState, title: &str, body: String) {
    let (store, cfg, title) = (s.store().clone(), s.cfg.clone(), title.to_string());
    let id = format!("passkey-{}", Uuid::now_v7());
    tokio::spawn(async move {
        crate::notify::send_operator(
            &store,
            &cfg,
            &id,
            title,
            body,
            "/settings#access".into(),
            &[],
        )
        .await;
    });
}

/// A stable WebAuthn user handle for this node's operator. Not a secret:
/// it groups this node's passkeys under one account in a password manager.
fn user_handle(node_id: &str) -> Uuid {
    let digest = Sha256::digest(format!("tracon operator {node_id}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

fn clean_label(label: Option<&str>) -> String {
    let label: String = label
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_LABEL)
        .collect();
    if label.is_empty() {
        "Passkey".into()
    } else {
        label
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RegisterOptionsBody {
    #[serde(default)]
    label: Option<String>,
}

/// `POST /api/passkeys/register/options`: begin enrolling a passkey. Allowed
/// at the node, in a freshly paired session's window while none is enrolled,
/// and otherwise as an admin action.
pub async fn register_options(
    State(s): State<AppState>,
    parts: Parts,
    body: Bytes,
) -> Result<Json<Value>, Refusal> {
    let now = now_ms();
    let (session, granted_by) = match caller(&s, &parts) {
        Caller::Local => (LOCAL.to_string(), "local"),
        Caller::Session(h) => {
            if enrollment_closes(&s, &h, now)?.is_some() {
                (h, "window")
            } else if s.store().passkey_count()? == 0 {
                return Err(Refusal::unavailable(
                    "enrollment_closed",
                    "the window to enroll this node's first passkey has closed: pair again \
                     (`tracon auth issue --url` on the node) and enroll within 15 minutes",
                ));
            } else {
                authorize(&s, &parts, &body).await?;
                (h, "step_up")
            }
        }
        Caller::Other => {
            return Err(Refusal::unavailable(
                "no_session",
                "passkeys are enrolled from a signed-in browser",
            ))
        }
    };
    let rp = relying_party(&s.cfg)?;
    let parsed: RegisterOptionsBody = if body.is_empty() {
        RegisterOptionsBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("bad body: {e}"))
        })?
    };
    limited(&s, &session)?;
    let exclude: Vec<_> = enrolled(&s)?
        .iter()
        .map(|(_, p)| p.cred_id().clone())
        .collect();
    let rp_id = rp.get_allowed_origins()[0]
        .domain()
        .unwrap_or_default()
        .to_string();
    let (options, state) = rp
        .start_passkey_registration(
            user_handle(&s.node_id),
            "operator",
            &format!("tracon operator on {rp_id}"),
            Some(exclude),
        )
        .map_err(internal)?;
    let mut options = serde_json::to_value(options).map_err(internal)?;
    // A discoverable credential is what a phone's passkey manager makes and
    // syncs; asking for one is a preference, never a requirement, so a
    // security key without resident-key storage still enrolls.
    options["publicKey"]["authenticatorSelection"]["residentKey"] = json!("preferred");
    let id = random_secret();
    let expires_ms = now + CHALLENGE_TTL_MS;
    s.store().ceremony_insert(
        &CeremonyRow {
            id: id.clone(),
            purpose: CeremonyPurpose::Register,
            op_hash: None,
            session_hash: session,
            granted_by: Some(granted_by.into()),
            label: Some(clean_label(parsed.label.as_deref())),
            state: serde_json::to_string(&state).map_err(internal)?,
            created_ms: now,
            expires_ms,
            action: None,
            summary: None,
        },
        now,
    )?;
    Ok(Json(json!({
        "challenge_id": id,
        "options": options,
        "expires_ms": expires_ms,
    })))
}

#[derive(Deserialize)]
pub struct RegisterFinishBody {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
}

/// What a registered passkey's own record says about it: its counter and
/// backup flags.
fn facts(passkey: &Passkey) -> Result<(i64, bool, bool), Refusal> {
    let v = serde_json::to_value(passkey).map_err(internal)?;
    let cred = &v["cred"];
    Ok((
        cred["counter"].as_i64().unwrap_or(0),
        cred["backup_eligible"].as_bool().unwrap_or(false),
        cred["backup_state"].as_bool().unwrap_or(false),
    ))
}

/// `POST /api/passkeys/register/finish`: verify the new credential and
/// enroll it.
pub async fn register_finish(
    State(s): State<AppState>,
    parts: Parts,
    Json(body): Json<RegisterFinishBody>,
) -> Result<Json<Value>, Refusal> {
    let session = match caller(&s, &parts) {
        Caller::Local => LOCAL.to_string(),
        Caller::Session(h) => h,
        Caller::Other => {
            return Err(Refusal::unavailable(
                "no_session",
                "passkeys are enrolled from a signed-in browser",
            ))
        }
    };
    let rp = relying_party(&s.cfg)?;
    limited(&s, &session)?;
    let now = now_ms();
    let Some(row) =
        s.store()
            .ceremony_take(&body.challenge_id, CeremonyPurpose::Register, &session, now)?
    else {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::GONE,
            "this enrollment expired or was already used; start again",
        )));
    };
    let state: PasskeyRegistration = serde_json::from_str(&row.state).map_err(internal)?;
    let passkey = rp
        .finish_passkey_registration(&body.credential, &state)
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("the passkey did not verify: {e}"),
            )
        })?;
    let (sign_count, backup_eligible, backup_state) = facts(&passkey)?;
    // What the browser said the authenticator is reached by. A hint for the
    // list, nothing more: the verifier keeps transports only from an
    // attestation it trusts, and passkeys are enrolled without one.
    let transports = body
        .credential
        .response
        .transports
        .as_ref()
        .and_then(|t| serde_json::to_string(t).ok());
    let id = b64(passkey.cred_id());
    let label = row.label.clone().unwrap_or_else(|| "Passkey".into());
    let enrolled = s.store().passkey_insert(
        &PasskeyRow {
            id: id.clone(),
            passkey: serde_json::to_string(&passkey).map_err(internal)?,
            sign_count,
            backup_eligible,
            backup_state,
            transports,
            label: label.clone(),
            created_ms: now,
            last_used_ms: None,
        },
        // The pairing window admits the first passkey only, decided with
        // the insert: of two paired clients racing, one wins.
        row.granted_by.as_deref() == Some("window"),
    )?;
    if !enrolled {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::CONFLICT,
            "a passkey was enrolled on this node first (or this one already is); \
             adding another needs a confirmation with it",
        )));
    }
    tracing::info!(passkey = %id, %label, granted_by = ?row.granted_by, "passkey enrolled");
    announce(
        &s,
        "Passkey added",
        format!(
            "\"{label}\" can now confirm admin actions on this node{}",
            match row.granted_by.as_deref() {
                Some("window") => " (added right after pairing)",
                Some("local") => " (added at the node)",
                _ => "",
            }
        ),
    );
    Ok(Json(json!({ "id": id, "label": label })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssertOptionsBody {
    op_hash: String,
}

/// `POST /api/passkeys/assert/options`: a fresh challenge for an operation
/// the client was told the hash of, when the one it was handed has expired.
pub async fn assert_options(
    State(s): State<AppState>,
    parts: Parts,
    Json(body): Json<AssertOptionsBody>,
) -> Result<Json<Value>, Refusal> {
    let Caller::Session(session) = caller(&s, &parts) else {
        return Err(Refusal::unavailable(
            "no_session",
            "confirmations belong to a signed-in browser session",
        ));
    };
    let op = body.op_hash.trim().to_ascii_lowercase();
    if op.len() != 64 || !op.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "op_hash must be a hex SHA-256",
        )));
    }
    let rp = relying_party(&s.cfg)?;
    limited(&s, &session)?;
    // What the session's earlier challenge for this operation said it is,
    // and whether it could be delegated: a fresh one is never more than that.
    let (action, summary, granted_by) = s.store().ceremony_describe(&session, &op)?;
    Ok(Json(open_assertion(
        &s,
        &rp,
        &session,
        &op,
        action.as_deref(),
        summary.as_deref(),
        granted_by.as_deref() != Some(NOT_DELEGABLE),
    )?))
}

#[derive(Deserialize)]
pub struct AssertFinishBody {
    challenge_id: String,
    credential: PublicKeyCredential,
}

/// `POST /api/passkeys/assert/finish`: verify the assertion and hand back a
/// single-use proof for the operation it was made for.
pub async fn assert_finish(
    State(s): State<AppState>,
    parts: Parts,
    Json(body): Json<AssertFinishBody>,
) -> Result<Json<Value>, Refusal> {
    let Caller::Session(session) = caller(&s, &parts) else {
        return Err(Refusal::unavailable(
            "no_session",
            "confirmations belong to a signed-in browser session",
        ));
    };
    let rp = relying_party(&s.cfg)?;
    limited(&s, &session)?;
    let now = now_ms();
    let Some(row) =
        s.store()
            .ceremony_take(&body.challenge_id, CeremonyPurpose::Assert, &session, now)?
    else {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::GONE,
            "this confirmation expired or was already used; try the action again",
        )));
    };
    let state: PasskeyAuthentication = serde_json::from_str(&row.state).map_err(internal)?;
    let result = rp
        .finish_passkey_authentication(&body.credential, &state)
        .map_err(|e| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                format!("the passkey did not verify: {e}"),
            )
        })?;
    // Still enrolled now, not only when the challenge was opened: a passkey
    // removed in between confirms nothing.
    let used = b64(result.cred_id());
    let Some((enrolled_row, mut passkey)) =
        enrolled(&s)?.into_iter().find(|(row, _)| row.id == used)
    else {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::FORBIDDEN,
            "that passkey is no longer enrolled on this node",
        )));
    };
    passkey.update_credential(&result);
    s.store().passkey_used(
        &enrolled_row.id,
        &serde_json::to_string(&passkey).map_err(internal)?,
        i64::from(result.counter()),
        result.backup_eligible(),
        result.backup_state(),
        now,
    )?;
    let proof = random_secret();
    let expires_ms = now + PROOF_TTL_MS;
    s.store()
        .ceremony_prove(&row.id, &auth::hash(&proof), expires_ms)?;
    Ok(Json(json!({
        "proof": proof,
        "op_hash": row.op_hash,
        "expires_ms": expires_ms,
    })))
}

/// `GET /api/passkeys`: what is enrolled, what they belong to, and whether
/// this session may still enroll the first without one.
pub async fn list(State(s): State<AppState>, parts: Parts) -> Result<Json<Value>, Refusal> {
    let now = now_ms();
    let rows = s.store().passkeys()?;
    let local = auth::parts_are_local(&parts);
    let closes = match caller(&s, &parts) {
        Caller::Session(h) => enrollment_closes(&s, &h, now)?,
        _ => None,
    };
    let rp = relying_party(&s.cfg);
    let (relying_party, unavailable) = match &rp {
        Ok(rp) => (
            Some(json!({
                "id": rp.get_allowed_origins()[0].domain(),
                "origin": rp.get_allowed_origins()[0].origin().ascii_serialization(),
            })),
            None,
        ),
        Err(Refusal::Unavailable { message, .. }) => (None, Some(message.clone())),
        Err(_) => (None, None),
    };
    Ok(Json(json!({
        "passkeys": rows,
        "local": local,
        "relying_party": relying_party,
        "unavailable": unavailable,
        "enrollment": {
            "open": closes.is_some(),
            "closes_ms": closes,
        },
    })))
}

/// `DELETE /api/passkeys/{id}`: remove a passkey. An admin action: off this
/// machine, confirmed with a passkey (possibly the one being removed).
pub async fn delete(
    State(s): State<AppState>,
    Path(id): Path<String>,
    AdminAction(()): AdminAction,
) -> Result<Json<Value>, Refusal> {
    if !s.store().passkey_delete(&id)? {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::NOT_FOUND,
            "no passkey by that ID",
        )));
    }
    tracing::info!(passkey = %id, "passkey removed");
    announce(
        &s,
        "Passkey removed",
        format!(
            "passkey {}… no longer confirms admin actions on this node",
            &id[..8.min(id.len())]
        ),
    );
    Ok(Json(json!({ "removed": id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_line_is_cut_only_visibly() {
        assert_eq!(fit_text("short\u{202e}", 10), "short");
        let cut = fit_text(&"x".repeat(20), 10);
        assert_eq!(cut, format!("{}\u{2026}", "x".repeat(9)));
        // What a summary may hold fits the line a device approval shows.
        let line = format!("Confirm for another window: {}", "y".repeat(MAX_SUMMARY));
        assert!(!fit_text(&line, MAX_APPROVAL_LINE).ends_with('\u{2026}'));
    }

    #[test]
    fn the_op_hash_binds_every_part_of_the_request() {
        let base = op_hash(&Method::PUT, "/api/auth/token", None, b"{\"a\":1}");
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            op_hash(&Method::PUT, "/api/auth/token", None, b"{\"a\":1}")
        );
        for other in [
            op_hash(&Method::POST, "/api/auth/token", None, b"{\"a\":1}"),
            op_hash(&Method::PUT, "/api/auth/tokens", None, b"{\"a\":1}"),
            op_hash(&Method::PUT, "/api/auth/token?x=1", None, b"{\"a\":1}"),
            op_hash(&Method::PUT, "/api/auth/token", Some("v1"), b"{\"a\":1}"),
            op_hash(&Method::PUT, "/api/auth/token", None, b"{\"a\":2}"),
            op_hash(&Method::PUT, "/api/auth/token", None, b"{\"a\": 1}"),
        ] {
            assert_ne!(base, other);
        }
        // The separators keep one field from bleeding into the next.
        assert_ne!(
            op_hash(&Method::PUT, "/a", Some("b"), b""),
            op_hash(&Method::PUT, "/a\nb", None, b"")
        );
    }

    #[test]
    fn a_route_says_what_it_does() {
        assert_eq!(
            route_summary(&Method::POST, "/api/auth/token").as_deref(),
            Some("Replace the operator token")
        );
        assert_eq!(
            route_summary(&Method::POST, "/api/nodes/n2/providers/anthropic/connect").as_deref(),
            Some("Sign in to anthropic")
        );
        assert_eq!(
            route_summary(&Method::PUT, "/api/channels/personal/bindings").as_deref(),
            Some("Change personal's bindings")
        );
        assert_eq!(route_summary(&Method::GET, "/api/auth/token"), None);
        let long = format!("/api/credentials/{}/share", "x".repeat(500));
        assert!(route_summary(&Method::POST, &long).unwrap().chars().count() <= 120);
    }

    #[test]
    fn text_shown_to_the_operator_hides_nothing() {
        // A right-to-left override, zero-width characters, a line separator
        // and a tag character, around a host.
        let sneaky = "evil\u{202E}moc.elgoog\u{200B}\u{2028}\u{E0041}.example\u{FEFF}\n";
        assert_eq!(clean_text(sneaky, 160), "evilmoc.elgoog.example");
        assert_eq!(clean_text(&"x".repeat(500), 160).chars().count(), 160);
        assert_eq!(clean_text("  café ✓ ", 160), "café ✓");
    }

    #[test]
    fn a_label_is_bounded_and_printable() {
        assert_eq!(clean_label(None), "Passkey");
        assert_eq!(clean_label(Some("  \n ")), "Passkey");
        assert_eq!(clean_label(Some(" iPhone\u{7}  ")), "iPhone");
        assert_eq!(clean_label(Some(&"x".repeat(200))).len(), MAX_LABEL);
    }

    #[test]
    fn the_user_handle_is_stable_per_node() {
        assert_eq!(user_handle("n1"), user_handle("n1"));
        assert_ne!(user_handle("n1"), user_handle("n2"));
    }

    #[test]
    fn no_public_origin_means_no_relying_party() {
        let cfg = Config::default();
        assert!(matches!(
            relying_party(&cfg),
            Err(Refusal::Unavailable {
                reason: "no_public_origin",
                ..
            })
        ));
        let mut cfg = Config::default();
        cfg.auth.public_origin = Some("https://tracon.example.org".into());
        let rp = relying_party(&cfg).ok().expect("a relying party");
        assert_eq!(
            rp.get_allowed_origins()[0].as_str(),
            "https://tracon.example.org/"
        );
        let mut cfg = Config::default();
        cfg.auth.public_origin = Some("http://localhost:5173".into());
        assert!(relying_party(&cfg).is_ok());
    }
}
