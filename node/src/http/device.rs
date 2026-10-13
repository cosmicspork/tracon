//! Device approval: signing in, or confirming an admin action, from a window
//! that cannot make a passkey, by approving it in a browser that can.
//!
//! The desktop app pointed at a remote node is such a window: its webview
//! has no WebAuthn on Linux (webkit2gtk) or in an unsigned macOS build. The
//! flow is shaped like RFC 8628 device authorization:
//!
//! 1. The window asks: `POST /api/auth/device` to sign in, or
//!    `POST /api/auth/device/stepup` with the challenge of a refused admin
//!    action. It gets a secret `device_code` it keeps to itself, a short
//!    `user_code` to show, and an `approve_url` (`<public_origin>/#approve=<id>`)
//!    it opens in the system browser or shows as a QR for a phone. The
//!    approval's ID rides in the fragment, so it never reaches a server log;
//!    the secret is never in a URL at all.
//! 2. A browser signed in to the node opens the URL, shows the code, who
//!    asked and when, and the operator compares the code with the window's
//!    and approves or denies. Approving is an admin action bound to this
//!    approval and no other ([`approval_op_hash`]): off the node's machine it
//!    needs a passkey assertion made for it (`http::passkey`), so a cookie
//!    alone, including the asking window's own, never approves anything.
//! 3. The window polls `POST /api/auth/device/token` with its secret and
//!    gets `authorization_pending`, `slow_down`, `access_denied` or
//!    `expired_token`, until the approval is there to collect: then, once, a
//!    new session cookie (sign-in) or a single-use step-up proof bound to the
//!    original operation and to the session that asked (step-up).
//!
//! A session made this way records what approved it (`delegated_from`) and
//! never has a pairing window, so it cannot enroll the node's first passkey.
//! Its sign-in is pushed to the operator's devices. No bearer token takes
//! part anywhere: a bearer caller can neither approve nor ask for a step-up.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{
    api::{ApiError, AppState},
    auth::{self, set_cookie, SESSION_TTL_MS},
    hostname,
    passkey::{
        self, authorize_op, caller, internal, limited, random_secret, relying_party, Caller,
        Refusal, PROOF_TTL_MS,
    },
};
use crate::store::{now_ms, AuthSessionRow, CeremonyPurpose, DeviceApprovalRow, DevicePoll};

/// How long a request waits to be approved.
pub const EXPIRES_MS: i64 = 10 * 60 * 1000;
/// How often the window is asked to poll, to begin with.
pub const INTERVAL_MS: i64 = 5_000;
/// What a user code is made of: consonants only (RFC 8628 §6.1), so no word
/// is spelled and no 0/O or 1/I is confused. Eight of them are about 34
/// bits, which only the approving browser ever compares, never guesses.
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
const USER_CODE_LEN: usize = 8;
/// The most sign-in requests that may wait at once: the route answers
/// strangers, so they must not be able to fill the table or the approval
/// screen.
const MAX_OPEN_SIGN_INS: i64 = 16;
/// The longest secret the poll route reads; the node issues 43 characters.
const MAX_DEVICE_CODE: usize = 128;

fn user_code() -> String {
    let mut rng = rand::rng();
    (0..USER_CODE_LEN)
        .map(|_| USER_CODE_ALPHABET[rng.random_range(0..USER_CODE_ALPHABET.len())] as char)
        .collect()
}

/// `BCDFGHJK` as both screens show it: `BCDF-GHJK`.
pub fn show_code(code: &str) -> String {
    if code.len() == USER_CODE_LEN {
        format!("{}-{}", &code[..4], &code[4..])
    } else {
        code.to_string()
    }
}

/// The operation approving `row` is: its ID, its code, its kind and, for a
/// step-up, the operation it confirms. A passkey assertion bound to this
/// approves this request and nothing else. Separated from an admin request's
/// own hash (`passkey::op_hash`) by its prefix, so neither can stand for the
/// other.
pub fn approval_op_hash(row: &DeviceApprovalRow) -> String {
    let mut h = Sha256::new();
    h.update(b"tracon-device-approval/1\n");
    for part in [
        row.id.as_str(),
        row.user_code.as_str(),
        row.kind.as_str(),
        row.op_hash.as_deref().unwrap_or(""),
    ] {
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

/// What the passkey prompt in the approving browser says it is confirming.
fn approval_summary(row: &DeviceApprovalRow) -> String {
    let text = if row.kind == "sign-in" {
        "Sign in another window".to_string()
    } else {
        let what = row
            .summary
            .as_deref()
            .or(row.action.as_deref())
            .unwrap_or("an admin action");
        format!("Confirm for another window: {what}")
    };
    text.chars().filter(|c| !c.is_control()).take(160).collect()
}

fn printable(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}

/// What the asking window says it is.
fn agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|v| printable(v, 200))
}

/// Where the asking window is, as reported: the first `X-Forwarded-For`
/// entry when a proxy sent one (behind an ingress the peer is the ingress),
/// else the connection's address. Shown for comparison, never trusted.
fn address(parts: &Parts) -> Option<String> {
    let forwarded = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .and_then(|v| {
            v.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .ok()
        });
    forwarded
        .or_else(|| auth::parts_peer(parts))
        .map(|ip| ip.to_canonical().to_string())
}

/// The public routes are reached without the guard's same-origin check,
/// which runs only for an authenticated request. A browser sends its page's
/// `Origin`; one from another site is refused here too.
fn same_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if host.is_some_and(|h| hostname(origin) == hostname(h)) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "cross-origin request refused",
        ))
    }
}

/// The address a request is approved at, or why there is none. The same
/// `[auth] public_origin` the passkeys belong to: the approving browser
/// asserts there.
fn approve_url(s: &AppState, id: &str) -> Result<String, Refusal> {
    relying_party(&s.cfg)?;
    let origin = s.cfg.public_origin().ok_or_else(|| {
        Refusal::unavailable(
            "no_public_origin",
            "device approval needs [auth] public_origin",
        )
    })?;
    Ok(format!("{origin}/#approve={id}"))
}

fn too_many(message: &str) -> Refusal {
    Refusal::Api(ApiError::new(StatusCode::TOO_MANY_REQUESTS, message))
}

fn started(row: &DeviceApprovalRow, device_code: &str, url: String) -> Value {
    let mut out = json!({
        "approval_id": row.id,
        "kind": row.kind,
        "device_code": device_code,
        "user_code": show_code(&row.user_code),
        // The same address as a QR, for a phone that is signed in with a
        // passkey to scan when the browser on this machine is not.
        "approve_qr": crate::mesh::enroll::qr_svg(&url),
        "approve_url": url,
        "interval": row.interval_ms / 1000,
        "expires_in": (row.expires_ms - row.created_ms) / 1000,
        "expires_ms": row.expires_ms,
    });
    if row.kind == "step-up" {
        out["op_hash"] = json!(row.op_hash);
        out["action"] = json!(row.action);
        out["summary"] = json!(row.summary);
    }
    out
}

fn new_row(kind: &str, parts: &Parts, device_code: &str, now: i64) -> DeviceApprovalRow {
    DeviceApprovalRow {
        id: passkey::b64(&rand::random::<[u8; 16]>()),
        kind: kind.into(),
        device_code_hash: auth::hash(device_code),
        user_code: user_code(),
        requester_session: None,
        requester_agent: agent(&parts.headers),
        requester_addr: address(parts),
        op_hash: None,
        action: None,
        summary: None,
        status: "pending".into(),
        approver: None,
        issued: None,
        created_ms: now,
        expires_ms: now + EXPIRES_MS,
        interval_ms: INTERVAL_MS,
        last_poll_ms: None,
        decided_ms: None,
        consumed_ms: None,
    }
}

/// `POST /api/auth/device`: ask to be signed in by a browser that is. Public
/// and bounded per address and in total.
pub async fn start(State(s): State<AppState>, parts: Parts) -> Result<Json<Value>, Refusal> {
    same_origin(&parts.headers)?;
    if !s.auth.token_configured() {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::FORBIDDEN,
            "no operator token is set on this node, so there is nothing to sign in to",
        )));
    }
    let now = now_ms();
    if !s.auth.allow_device_start(auth::parts_peer(&parts), now) {
        return Err(too_many("too many sign-in requests; wait a few minutes"));
    }
    let device_code = random_secret();
    let row = new_row("sign-in", &parts, &device_code, now);
    let url = approve_url(&s, &row.id)?;
    if !s
        .store()
        .device_approval_insert(&row, Some(MAX_OPEN_SIGN_INS), now)?
    {
        return Err(too_many(
            "too many sign-in requests are waiting to be approved; wait for them to expire",
        ));
    }
    tracing::info!(
        approval = %row.id,
        agent = row.requester_agent.as_deref().unwrap_or(""),
        addr = row.requester_addr.as_deref().unwrap_or(""),
        "device sign-in requested"
    );
    Ok(Json(started(&row, &device_code, url)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepUpBody {
    challenge_id: String,
}

/// `POST /api/auth/device/stepup`: have another browser confirm the admin
/// action this session was just refused. Takes the challenge that refusal
/// opened (spending it), so the approval is for the operation, action and
/// summary the node itself recorded, and is bound to this session.
pub async fn step_up(
    State(s): State<AppState>,
    parts: Parts,
    Json(body): Json<StepUpBody>,
) -> Result<Json<Value>, Refusal> {
    let session = match caller(&s, &parts) {
        Caller::Session(h) => h,
        Caller::Local => {
            return Err(Refusal::Api(ApiError::new(
                StatusCode::CONFLICT,
                "at the node an admin action needs no confirmation; send it again",
            )))
        }
        Caller::Other => return Err(Refusal::unavailable(
            "no_session",
            "a confirmation belongs to a signed-in browser session, and a bearer token has none",
        )),
    };
    limited(&s, &session)?;
    let now = now_ms();
    let Some(challenge) =
        s.store()
            .ceremony_take(&body.challenge_id, CeremonyPurpose::Assert, &session, now)?
    else {
        return Err(Refusal::Api(ApiError::new(
            StatusCode::GONE,
            "this confirmation expired or was already used; try the action again",
        )));
    };
    let Some(op_hash) = challenge.op_hash else {
        return Err(internal("an assertion without an operation"));
    };
    let device_code = random_secret();
    let mut row = new_row("step-up", &parts, &device_code, now);
    row.requester_session = Some(session.clone());
    row.op_hash = Some(op_hash);
    row.action = challenge.action;
    row.summary = challenge.summary;
    let url = approve_url(&s, &row.id)?;
    s.store().device_approval_insert(&row, None, now)?;
    tracing::info!(
        approval = %row.id,
        session = %&session[..12.min(session.len())],
        action = row.action.as_deref().unwrap_or(""),
        "device step-up requested"
    );
    Ok(Json(started(&row, &device_code, url)))
}

#[derive(Deserialize)]
pub struct TokenBody {
    device_code: String,
}

/// A poll's answer that is not the approval: RFC 8628's error codes, in the
/// node's usual error object.
fn not_yet(kind: &'static str, message: &str, interval_ms: Option<i64>) -> Response {
    let mut body = json!({
        "error": { "code": 400, "kind": kind, "message": message },
    });
    if let Some(ms) = interval_ms {
        body["interval"] = json!(ms / 1000);
    }
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

fn invalid_grant() -> Response {
    not_yet(
        "invalid_grant",
        "no request answers to that code; it was collected, revoked or never made",
        None,
    )
}

/// `POST /api/auth/device/token`: where the request stands, and once it is
/// approved, what it grants. Public, so a window with no session can sign
/// in; a step-up is collected only by the session that asked for it.
pub async fn token(
    State(s): State<AppState>,
    parts: Parts,
    Json(body): Json<TokenBody>,
) -> Response {
    if let Err(e) = same_origin(&parts.headers) {
        return e.into_response();
    }
    let now = now_ms();
    if !s.auth.allow_device_poll(auth::parts_peer(&parts), now) {
        return too_many("too many polls; wait a few minutes").into_response();
    }
    let code = body.device_code.trim();
    if code.is_empty() || code.len() > MAX_DEVICE_CODE {
        return invalid_grant();
    }
    let code_hash = auth::hash(code);
    let store = s.store();
    let row = match store.device_approval_by_code(&code_hash) {
        Ok(Some(row)) => row,
        Ok(None) => return invalid_grant(),
        Err(e) => return Refusal::from(e).into_response(),
    };
    // A step-up's secret is good only with the session that asked: checked
    // before the poll counts, so another session cannot even slow it down.
    let session = match caller(&s, &parts) {
        Caller::Session(h) => Some(h),
        _ => None,
    };
    if row.kind == "step-up" && session.as_deref() != row.requester_session.as_deref() {
        return invalid_grant();
    }
    let poll = match store.device_approval_poll(&code_hash, now) {
        Ok(poll) => poll,
        Err(e) => return Refusal::from(e).into_response(),
    };
    let approved = match poll {
        DevicePoll::Unknown => return invalid_grant(),
        DevicePoll::Pending => {
            return not_yet(
                "authorization_pending",
                "waiting for a signed-in browser to approve",
                None,
            )
        }
        DevicePoll::SlowDown { interval_ms } => {
            return not_yet(
                "slow_down",
                "polling too often; wait longer between polls",
                Some(interval_ms),
            )
        }
        DevicePoll::Denied => return not_yet("access_denied", "the request was denied", None),
        DevicePoll::Expired => {
            return not_yet(
                "expired_token",
                "the request expired before it was approved; ask again",
                None,
            )
        }
        DevicePoll::Approved(row) => row,
    };
    let result = if approved.kind == "sign-in" {
        collect_sign_in(&s, &parts, &approved, now)
    } else {
        collect_step_up(&s, &approved, session.as_deref().unwrap_or_default(), now)
    };
    result.unwrap_or_else(IntoResponse::into_response)
}

/// What approved a delegated session, as it is recorded: `local`, or the
/// approving session's short ID (the one the session list shows).
fn approver_label(approver: Option<&str>) -> String {
    match approver {
        Some("local") | None => "local".into(),
        Some(h) => h[..12.min(h.len())].to_string(),
    }
}

fn collect_sign_in(
    s: &AppState,
    parts: &Parts,
    row: &DeviceApprovalRow,
    now: i64,
) -> Result<Response, Refusal> {
    let secret = random_secret();
    let delegated_from = approver_label(row.approver.as_deref());
    let session = AuthSessionRow {
        token_hash: auth::hash(&secret),
        created_ms: now,
        last_seen_ms: now,
        expires_ms: now + SESSION_TTL_MS,
        user_agent: agent(&parts.headers),
        // Never a pairing: a session approved from elsewhere may not enroll
        // the node's first passkey.
        paired_ms: None,
        delegated_from: Some(delegated_from.clone()),
    };
    if !s
        .store()
        .device_approval_collect_sign_in(&row.id, &session, now)?
    {
        return Ok(invalid_grant());
    }
    tracing::info!(
        approval = %row.id,
        session = %&session.token_hash[..12],
        delegated_from = %delegated_from,
        agent = session.user_agent.as_deref().unwrap_or(""),
        "device sign-in approved and collected"
    );
    announce_sign_in(s, row, &delegated_from);
    let mut response = Json(json!({ "ok": true, "kind": "sign-in" })).into_response();
    set_cookie(
        &mut response,
        &secret,
        !auth::parts_are_local(parts),
        SESSION_TTL_MS / 1000,
    );
    Ok(response)
}

fn collect_step_up(
    s: &AppState,
    row: &DeviceApprovalRow,
    session: &str,
    now: i64,
) -> Result<Response, Refusal> {
    let proof = random_secret();
    let expires_ms = now + PROOF_TTL_MS;
    if !s.store().device_approval_collect_step_up(
        &row.id,
        session,
        &auth::hash(&proof),
        expires_ms,
        now,
    )? {
        return Ok(invalid_grant());
    }
    tracing::info!(
        approval = %row.id,
        session = %&session[..12.min(session.len())],
        approver = %approver_label(row.approver.as_deref()),
        op_hash = row.op_hash.as_deref().unwrap_or(""),
        "device step-up approved and collected"
    );
    Ok(Json(json!({
        "kind": "step-up",
        "proof": proof,
        "op_hash": row.op_hash,
        "expires_ms": expires_ms,
    }))
    .into_response())
}

/// Tell the operator's devices a window was signed in from elsewhere: one
/// they did not approve is the sign of a stolen session approving, or of an
/// approval they were talked into. Detached, so a slow push service never
/// holds the sign-in up.
fn announce_sign_in(s: &AppState, row: &DeviceApprovalRow, delegated_from: &str) {
    let (store, cfg) = (s.store().clone(), s.cfg.clone());
    let what = row
        .requester_agent
        .as_deref()
        .map(|a| printable(a, 80))
        .unwrap_or_else(|| "a window".into());
    let by = if delegated_from == "local" {
        "at the node".to_string()
    } else {
        format!("by session {delegated_from}")
    };
    let body = format!(
        "{what} was signed in, approved {by}. If that was not you, sign it out in Settings and rotate the token."
    );
    let id = format!("device-{}", row.id);
    tokio::spawn(async move {
        crate::notify::send_operator(
            &store,
            &cfg,
            &id,
            "Signed in from another window".into(),
            body,
            "/settings#access".into(),
            &[],
        )
        .await;
    });
}

/// Who is deciding: the operator at the node, or a signed-in browser. A
/// bearer token decides nothing here.
fn decider(s: &AppState, parts: &Parts) -> Result<String, Refusal> {
    match caller(s, parts) {
        Caller::Local => Ok("local".into()),
        Caller::Session(h) => Ok(h),
        Caller::Other => Err(Refusal::unavailable(
            "no_session",
            "a request is approved from a signed-in browser, or at the node",
        )),
    }
}

fn find(s: &AppState, id: &str) -> Result<DeviceApprovalRow, Refusal> {
    s.store().device_approval_get(id)?.ok_or_else(|| {
        Refusal::Api(ApiError::new(
            StatusCode::NOT_FOUND,
            "no request by that ID; ask again from the window",
        ))
    })
}

fn closed(row: &DeviceApprovalRow, now: i64) -> Refusal {
    Refusal::Api(ApiError::new(
        StatusCode::GONE,
        format!(
            "this request is {} and can no longer be decided",
            row.shown_status(now)
        ),
    ))
}

/// `GET /api/auth/device/approvals/{id}`: what the approving browser shows.
pub async fn view(
    State(s): State<AppState>,
    Path(id): Path<String>,
    parts: Parts,
) -> Result<Json<Value>, Refusal> {
    let me = decider(&s, &parts)?;
    let row = find(&s, &id)?;
    let now = now_ms();
    let local = me == "local";
    Ok(Json(json!({
        "id": row.id,
        "kind": row.kind,
        "user_code": show_code(&row.user_code),
        "status": row.shown_status(now),
        "created_ms": row.created_ms,
        "expires_ms": row.expires_ms,
        "requester_agent": row.requester_agent,
        "requester_addr": row.requester_addr,
        "action": row.action,
        "summary": row.summary,
        "prompt": approval_summary(&row),
        "local": local,
        "passkeys": s.store().passkey_count()?,
        "relying_party": relying_party(&s.cfg).is_ok(),
        // The browser approving is the window that asked: it can, with a
        // passkey, but it is worth saying.
        "same_session": row.requester_session.as_deref() == Some(me.as_str()),
    })))
}

/// `POST /api/auth/device/approvals/{id}/approve`: approve a request still
/// open. At the node, as it stands; anywhere else, only with a passkey
/// assertion bound to exactly this request ([`approval_op_hash`]): the
/// first try is refused with `step_up_required`, and the retry carries the
/// proof.
pub async fn approve(
    State(s): State<AppState>,
    Path(id): Path<String>,
    parts: Parts,
    body: Bytes,
) -> Result<Json<Value>, Refusal> {
    passkey::json_or_empty(&parts.headers, &body)?;
    let approver = decider(&s, &parts)?;
    let row = find(&s, &id)?;
    if !row.open(now_ms()) {
        return Err(closed(&row, now_ms()));
    }
    authorize_op(
        &s,
        &parts,
        || approval_op_hash(&row),
        &format!("code {}", show_code(&row.user_code)),
        Some(&approval_summary(&row)),
    )?;
    let now = now_ms();
    if !s
        .store()
        .device_approval_decide(&row.id, true, &approver, now)?
    {
        return Err(closed(&row, now));
    }
    tracing::info!(
        approval = %row.id,
        kind = %row.kind,
        approver = %approver_label(Some(&approver)),
        "device approval approved"
    );
    Ok(Json(json!({ "id": row.id, "status": "approved" })))
}

/// `POST /api/auth/device/approvals/{id}/deny`: refuse a request still open.
/// Refusing needs no passkey: it grants nothing.
pub async fn deny(
    State(s): State<AppState>,
    Path(id): Path<String>,
    parts: Parts,
) -> Result<Json<Value>, Refusal> {
    let approver = decider(&s, &parts)?;
    let row = find(&s, &id)?;
    let now = now_ms();
    if !s
        .store()
        .device_approval_decide(&row.id, false, &approver, now)?
    {
        return Err(closed(&row, now));
    }
    tracing::info!(
        approval = %row.id,
        kind = %row.kind,
        by = %approver_label(Some(&approver)),
        "device approval denied"
    );
    Ok(Json(json!({ "id": row.id, "status": "denied" })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, op: Option<&str>) -> DeviceApprovalRow {
        let parts = axum::http::Request::new(()).into_parts().0;
        let mut row = new_row(kind, &parts, "secret", 0);
        row.id = "a1".into();
        row.user_code = "BCDFGHJK".into();
        row.op_hash = op.map(str::to_string);
        row
    }

    #[test]
    fn a_user_code_is_eight_unambiguous_consonants() {
        for _ in 0..200 {
            let code = user_code();
            assert_eq!(code.len(), USER_CODE_LEN);
            assert!(
                code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)),
                "{code}"
            );
            assert!(!code.contains(['A', 'E', 'I', 'O', 'U', 'Y', '0', '1']));
        }
        assert_eq!(show_code("BCDFGHJK"), "BCDF-GHJK");
    }

    #[test]
    fn approving_is_bound_to_the_request_its_code_and_its_operation() {
        let base = approval_op_hash(&row("sign-in", None));
        assert_eq!(base.len(), 64);
        let mut other_id = row("sign-in", None);
        other_id.id = "a2".into();
        let mut other_code = row("sign-in", None);
        other_code.user_code = "BCDFGHJL".into();
        for other in [
            approval_op_hash(&other_id),
            approval_op_hash(&other_code),
            approval_op_hash(&row("step-up", None)),
        ] {
            assert_ne!(base, other);
        }
        assert_ne!(
            approval_op_hash(&row("step-up", Some("op-a"))),
            approval_op_hash(&row("step-up", Some("op-b")))
        );
        // Never the hash of an admin request.
        assert_ne!(
            base,
            passkey::op_hash(&axum::http::Method::POST, "a1", None, b"")
        );
    }

    #[test]
    fn the_prompt_says_what_is_approved_in_the_nodes_words() {
        assert_eq!(
            approval_summary(&row("sign-in", None)),
            "Sign in another window"
        );
        let mut up = row("step-up", Some("op"));
        up.summary = Some("Remove a passkey".into());
        assert_eq!(
            approval_summary(&up),
            "Confirm for another window: Remove a passkey"
        );
        up.summary = None;
        up.action = Some("DELETE /api/passkeys/x".into());
        assert_eq!(
            approval_summary(&up),
            "Confirm for another window: DELETE /api/passkeys/x"
        );
    }

    #[test]
    fn a_reported_address_is_an_address_or_nothing() {
        let (mut parts, _) = axum::http::Request::new(()).into_parts();
        parts
            .headers
            .insert("x-forwarded-for", "198.51.100.4, 10.0.0.1".parse().unwrap());
        assert_eq!(address(&parts).as_deref(), Some("198.51.100.4"));
        parts
            .headers
            .insert("x-forwarded-for", "198.51.100.4<script>".parse().unwrap());
        assert_eq!(address(&parts), None);
    }
}
