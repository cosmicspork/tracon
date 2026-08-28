use std::convert::Infallible;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    Extension, Json,
};
use proto::enroll::{normalize_code, EnrollRequest};
use proto::frame::{valid_channel, Envelope, MAX_FRAME_BYTES, MESH_CHANNEL};
use proto::keys::key32;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};

use crate::auth::{now_ms, now_unix, Owner};
use crate::store::{Fill, Member, Take};
use crate::AppState;

type ApiResult = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": msg.into()})))
}

fn io(e: std::io::Error) -> (StatusCode, Json<Value>) {
    tracing::error!(error = %e, "store failure");
    err(StatusCode::INTERNAL_SERVER_ERROR, "store failure")
}

/// The caller as a member record, or 403.
fn member_of(state: &AppState, owner: &Owner) -> Result<Member, (StatusCode, Json<Value>)> {
    state
        .members
        .get(&owner.hex())
        .map_err(io)?
        .ok_or_else(|| err(StatusCode::FORBIDDEN, "not a member of this hub"))
}

fn in_channel(m: &Member, channel: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if m.channels.iter().any(|c| c == channel) {
        Ok(())
    } else {
        Err(err(
            StatusCode::FORBIDDEN,
            format!("not a member of channel {channel}"),
        ))
    }
}

pub async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

pub async fn info(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "contract_version": proto::CONTRACT_VERSION,
        "contract_major": proto::contract_major(),
        "retain_days": s.cfg.retain_days,
        "max_channel_bytes": s.cfg.max_channel_bytes,
        "max_frame": MAX_FRAME_BYTES,
        "enroll_ttl_secs": s.cfg.enroll_ttl_secs,
        "replica": s.replica.is_some(),
        "hub_node_id": s.replica.as_ref().map(|r| r.node_id()),
        "hub_x25519_pub": s.replica.as_ref().map(|r| r.x25519_hex()),
        "replica_channels": s.replica.as_ref().map(|r| r.readable_channels()),
        "replica_undecryptable": s.replica.as_ref().map(|r| r.undecryptable()),
    }))
}

// ------------------------------------------------------------------ frames

pub async fn post_frame(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    body: String,
) -> ApiResult {
    if body.len() > MAX_FRAME_BYTES {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "frame exceeds max_frame",
        ));
    }
    let env: Envelope = serde_json::from_str(&body)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("not a frame: {e}")))?;
    let sender = env
        .verify()
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if sender != owner.0 {
        return Err(err(
            StatusCode::FORBIDDEN,
            "frame sender is not the authenticated key",
        ));
    }
    if !valid_channel(&env.channel) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid channel name"));
    }
    let me = member_of(&s, &owner)?;
    in_channel(&me, &env.channel)?;
    // Store exactly the bytes that were signed, re-serialized canonically by
    // serde so the on-disk form is one object per file.
    let stored = serde_json::to_string(&env).expect("envelope serializes");
    let seq = s
        .frames
        .append(&env.channel, &stored, now_ms())
        .map_err(io)?;
    for m in s.members.members_of(&env.channel).map_err(io)? {
        if let Some(k) = key32(&m.node_id) {
            s.pokes.poke(&k);
        }
    }
    if let Some(r) = &s.replica {
        r.wake.notify_one();
    }
    Ok((StatusCode::CREATED, Json(json!({"seq": seq}))))
}

#[derive(Deserialize)]
pub struct FramesQuery {
    channel: String,
    #[serde(default)]
    after: u64,
    limit: Option<usize>,
}

pub async fn get_frames(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Query(q): Query<FramesQuery>,
) -> ApiResult {
    if !valid_channel(&q.channel) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid channel name"));
    }
    let me = member_of(&s, &owner)?;
    in_channel(&me, &q.channel)?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let page = s.frames.read(&q.channel, q.after, limit).map_err(io)?;
    // Behind retention: the caller's cursor points before the oldest frame we
    // still hold, so frames were lost to it. It must resync from a snapshot.
    if page.oldest > 0 && q.after + 1 < page.oldest {
        return Err((
            StatusCode::GONE,
            Json(
                json!({"error": "cursor behind retention", "oldest": page.oldest, "latest": page.latest}),
            ),
        ));
    }
    let frames: Vec<Value> = page
        .frames
        .iter()
        .map(|(seq, e)| {
            json!({"seq": seq, "envelope": serde_json::from_str::<Value>(e).unwrap_or(Value::Null)})
        })
        .collect();
    let next = if page.frames.len() == limit {
        page.frames.last().map(|f| f.0)
    } else {
        None
    };
    Ok((
        StatusCode::OK,
        Json(json!({"frames": frames, "next": next, "oldest": page.oldest, "latest": page.latest})),
    ))
}

/// Payload-free pokes. Subscribed before the response is built so a poke
/// racing the handshake is not lost. Heartbeat comment every 25 s keeps the
/// ingress from timing the stream out.
pub async fn events(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<Value>)> {
    member_of(&s, &owner)?;
    let stream = BroadcastStream::new(s.pokes.subscribe(&owner.0)).map(|r| {
        let ev = match r {
            Ok(()) => Event::default().event("frames"),
            Err(_) => Event::default().event("sync"),
        };
        Ok(ev.data("1"))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(25))))
}

pub async fn list_members(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
) -> Result<Json<Vec<Member>>, (StatusCode, Json<Value>)> {
    member_of(&s, &owner)?;
    Ok(Json(s.members.list().map_err(io)?))
}

// ---------------------------------------------------------------- enrollment

#[derive(Deserialize, Default)]
pub struct OpenEnroll {
    ttl_secs: Option<u64>,
}

fn code_of(raw: &str) -> Result<String, (StatusCode, Json<Value>)> {
    normalize_code(raw).ok_or_else(|| err(StatusCode::BAD_REQUEST, "malformed enrollment code"))
}

pub async fn open_enroll(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Path(code): Path<String>,
    body: Option<Json<OpenEnroll>>,
) -> ApiResult {
    member_of(&s, &owner)?;
    let code = code_of(&code)?;
    let ttl = body
        .and_then(|b| b.ttl_secs)
        .unwrap_or(s.cfg.enroll_ttl_secs)
        .min(s.cfg.enroll_ttl_secs);
    let now = now_unix();
    let expires_at = now + ttl;
    if !s.enroll.open(&code, owner.0, expires_at, now) {
        return Err(err(
            StatusCode::CONFLICT,
            "a live slot already exists under that code",
        ));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"code": code, "expires_at": expires_at})),
    ))
}

/// The one public write. Rate-limited per source; stores public keys and a
/// name in the clear because none of it is secret.
pub async fn fill_enroll(
    State(s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(req): Json<EnrollRequest>,
) -> ApiResult {
    let source = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    if !s
        .limiter
        .allow(&source, s.cfg.enroll_rate_per_min, 60, now_unix())
    {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "slow down"));
    }
    let code = code_of(&code)?;
    if key32(&req.node_id).is_none() || key32(&req.x25519_pub).is_none() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "node_id and x25519_pub must be 32-byte hex",
        ));
    }
    if req.name.is_empty() || req.name.len() > 64 || req.facts.len() > 256 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "name must be 1–64 chars; facts at most 256",
        ));
    }
    if req.contract != proto::CONTRACT_VERSION {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "contract {} is not this hub's {}",
                req.contract,
                proto::CONTRACT_VERSION
            ),
        ));
    }
    let body = serde_json::to_string(&req).expect("serializes");
    match s.enroll.fill(&code, body, now_unix()) {
        Fill::Filled => Ok((StatusCode::NO_CONTENT, Json(Value::Null))),
        Fill::Unknown => Err(err(
            StatusCode::NOT_FOUND,
            "no such invitation, or it expired",
        )),
        Fill::AlreadyFilled => Err(err(StatusCode::CONFLICT, "invitation already used")),
    }
}

pub async fn take_enroll(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Path(code): Path<String>,
) -> ApiResult {
    member_of(&s, &owner)?;
    let code = code_of(&code)?;
    match s.enroll.take(&code, &owner.0, now_unix()) {
        Take::NotYet => Ok((StatusCode::NO_CONTENT, Json(Value::Null))),
        Take::Ready(body) => Ok((
            StatusCode::OK,
            Json(serde_json::from_str(&body).unwrap_or(Value::Null)),
        )),
        Take::Unknown => Err(err(
            StatusCode::NOT_FOUND,
            "no such invitation, or it expired",
        )),
        Take::NotYours => Err(err(
            StatusCode::FORBIDDEN,
            "another node opened that invitation",
        )),
    }
}

pub async fn cancel_enroll(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Path(code): Path<String>,
) -> ApiResult {
    member_of(&s, &owner)?;
    let code = code_of(&code)?;
    if s.enroll.cancel(&code, &owner.0) {
        Ok((StatusCode::NO_CONTENT, Json(Value::Null)))
    } else {
        Err(err(StatusCode::NOT_FOUND, "no such invitation"))
    }
}

// ---------------------------------------------------------------- membership

#[derive(Deserialize, Serialize)]
pub struct AdmitBody {
    pub node_id: String,
    pub x25519_pub: String,
    pub name: String,
    #[serde(default)]
    pub channels: Vec<String>,
    /// `hub` when a node shares channels with the replica. Only the hub's own
    /// id may be admitted as a hub.
    #[serde(default)]
    pub role: Option<String>,
}

/// Admit or extend a member. The admitter must itself be in every channel it
/// grants, except that a member may always extend its own record: the hub is
/// not the authority on channel keys, only routing, so self-extension leaks
/// nothing.
pub async fn admit(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Json(body): Json<AdmitBody>,
) -> ApiResult {
    let me = member_of(&s, &owner)?;
    let node_id = body.node_id.to_ascii_lowercase();
    if key32(&node_id).is_none() || key32(&body.x25519_pub).is_none() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "node_id and x25519_pub must be 32-byte hex",
        ));
    }
    if body.name.len() > 64 {
        return Err(err(StatusCode::BAD_REQUEST, "name at most 64 chars"));
    }
    let is_self = node_id == me.node_id;
    let role = match body.role.as_deref() {
        Some("hub") => {
            let hub_id = s.replica.as_ref().map(|r| r.node_id());
            if hub_id.as_deref() != Some(node_id.as_str()) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "only this hub's own id may be admitted as a hub",
                ));
            }
            crate::store::MemberRole::Hub
        }
        Some(other) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("unknown role {other}"),
            ))
        }
        None => crate::store::MemberRole::Node,
    };
    let mut channels: Vec<String> = vec![MESH_CHANNEL.to_string()];
    for c in &body.channels {
        if !valid_channel(c) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("invalid channel name {c}"),
            ));
        }
        if !is_self {
            in_channel(&me, c)?;
        }
        if !channels.contains(c) {
            channels.push(c.clone());
        }
    }
    let existing = s.members.get(&node_id).map_err(io)?;
    if let Some(e) = &existing {
        for c in &e.channels {
            if !channels.contains(c) {
                channels.push(c.clone());
            }
        }
    }
    let member = Member {
        node_id: node_id.clone(),
        x25519_pub: body.x25519_pub.to_ascii_lowercase(),
        name: body.name,
        channels,
        admitted_ms: existing
            .as_ref()
            .map(|e| e.admitted_ms)
            .unwrap_or_else(now_ms),
        admitted_by: existing
            .as_ref()
            .map(|e| e.admitted_by.clone())
            .unwrap_or_else(|| me.node_id.clone()),
        role: existing
            .as_ref()
            .map(|e| e.role)
            .filter(|r| *r == crate::store::MemberRole::Hub)
            .unwrap_or(role),
    };
    s.members.put(&member).map_err(io)?;
    if member.role == crate::store::MemberRole::Hub {
        if let Some(r) = &s.replica {
            r.wake.notify_one();
        }
    }
    if let Some(k) = key32(&node_id) {
        s.pokes.poke(&k);
    }
    Ok((StatusCode::OK, Json(serde_json::to_value(member).unwrap())))
}

pub async fn remove_member(
    State(s): State<AppState>,
    Extension(owner): Extension<Owner>,
    Path(node_id): Path<String>,
) -> ApiResult {
    let me = member_of(&s, &owner)?;
    let node_id = node_id.to_ascii_lowercase();
    if node_id == me.node_id {
        return Err(err(StatusCode::BAD_REQUEST, "a node cannot remove itself"));
    }
    if s.members.remove(&node_id).map_err(io)? {
        Ok((StatusCode::NO_CONTENT, Json(Value::Null)))
    } else {
        Err(err(StatusCode::NOT_FOUND, "no such member"))
    }
}
