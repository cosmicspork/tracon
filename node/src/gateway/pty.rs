//! The terminal capability: a mediated PTY inside one session's workspace.
//!
//! `POST /pty` on the harness is arbitrary command execution as the server
//! user, in any directory the caller names, with any environment it likes, and
//! there is no permission check anywhere in that path
//! (`docs/reference/opencode-v1.18.30/api-ui.md` §5, finding 7). The
//! WebSocket it is reached over is only slightly better: the harness mints a
//! 60-second single-use ticket, but scopes it to `{ptyID, directory,
//! workspaceID}` and to nothing else — not an owner, not an audience, not a
//! browser session — and its origin check accepts any `http://localhost:*`
//! (§8 #12–13).
//!
//! So this module exists to say four things.
//!
//! **A terminal is granted, never inferred.** The capability is default-`ask`:
//! an ungranted `POST /pty` is a 403 the operator can see and act on, and the
//! grant that opens it is an authority grant bound to this session, this
//! session's workspace path, and a channel, with an expiry and a revocation
//! that take effect at the next dispatch rather than at the next poll.
//!
//! **The spawn is rewritten, not forwarded.** `cwd` is pinned to the
//! workspace or a normalised subdirectory of it; `env` keeps only the
//! variables that decide how a terminal *looks* and never one that decides
//! what it can reach; `command` must be one of the shells the image itself
//! lists. What the caller sent is not what the harness is asked for.
//!
//! **The ticket is the node's, and it is owner-bound.** The gateway takes the
//! harness's ticket server-side and hands the browser one of its own, bound to
//! the operator that asked, the session, the PTY, and the exact origin it will
//! be presented from, for 30 seconds, once.
//!
//! **And a terminal is an interactive shell, not a per-command ledger.** This
//! is the honest limit of the whole arrangement. Once a shell is open, every
//! command typed at its prompt runs with no further decision and raises no
//! tool call, because the harness raises none for a PTY. tracon records that a
//! terminal was opened, what shell, in which directory, by whom, and how much
//! traffic crossed it — not what was run. The grant is of the terminal. It is
//! written that way in the event text, in the refusal, and here.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        FromRequestParts, State,
    },
    http::{request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};

use crate::{
    adapter::NativeApi,
    authority,
    http::api::AppState,
    policy::Verdict,
    session::state::event_kind as ek,
    store::{now_ms, AuthorityGrantRow},
};

/// The capability's name, as a grant's action and as a policy rule's title.
pub const TERMINAL: &str = authority::TERMINAL;

/// How long the gateway's own ticket lives. Shorter than the harness's 60 s,
/// because it is redeemed by a browser that has just been handed it — a
/// minute of validity buys the holder nothing and costs a replay window.
const TICKET_TTL: Duration = Duration::from_secs(30);

/// The subprotocol a browser may carry the ticket in when it would rather not
/// put one in a URL. `new WebSocket(url, ["tracon.pty.ticket.<value>"])`.
const TICKET_SUBPROTOCOL_PREFIX: &str = "tracon.pty.ticket.";

/// The largest single terminal frame the proxy will carry in either
/// direction. Keystrokes are bytes and output is chunked upstream at 64 KiB
/// (`packages/core/src/pty/protocol.ts`), so this is generous; it exists so
/// one frame cannot be the whole memory bound on its own.
const MAX_FRAME: usize = 1 << 20;

/// Shells a terminal may be opened with even when the harness lists none.
/// `GET /pty/shells` is the image's own answer and is preferred; this is the
/// floor, so a harness that cannot answer does not silently widen or close
/// the capability.
const SHELL_FLOOR: &[&str] = &["/bin/bash", "/bin/sh"];

/// The only environment variables a caller may contribute to a terminal, and
/// every one of them decides how the terminal *looks*.
///
/// `PATH` and `HOME` are deliberately not here, though a terminal plainly
/// needs both: they come from the harness process the PTY is spawned under,
/// which already has the session's own. A caller-supplied `PATH` would point
/// the shell at whatever the workspace contains, and a caller-supplied `HOME`
/// would move the shell's configuration somewhere tracon did not choose —
/// either of which is the capability check undone from the inside. Everything
/// not on this list is dropped and the names are recorded.
const ENV_ALLOWED: &[&str] = &[
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

/// The sentence the operator reads wherever a terminal is recorded. A grant
/// that is understood as "one command" is a grant given for the wrong reason.
const NOT_A_LEDGER: &str = "a PTY is an interactive shell, not a per-command ledger: \
     everything typed at this prompt runs without a further decision and raises no tool call";

// ---------------------------------------------------------------------------
// The grant
// ---------------------------------------------------------------------------

/// Whether this channel and session may open a terminal in this workspace,
/// and the grant that said so.
pub(super) struct Terminal {
    pub verdict: Verdict,
    pub grant: Option<AuthorityGrantRow>,
    pub reason: Option<String>,
}

impl Terminal {
    pub fn allowed(&self) -> bool {
        self.verdict == Verdict::Allow
    }

    /// What the refusal says. A capability nobody can see the shape of is one
    /// the operator cannot grant on purpose, so the message names the
    /// capability, the channel, the target a grant has to bind, and why the
    /// default is no.
    pub fn refusal(&self, channel: &str, session_id: &str, workspace: &str) -> String {
        if let Some(reason) = self.reason.as_deref().filter(|r| !r.is_empty()) {
            return format!("the {TERMINAL} capability is refused on channel {channel}: {reason}");
        }
        format!(
            "the {TERMINAL} capability is not granted on channel {channel}; \
             a PTY is arbitrary command execution with no permission check of its own \
             (finding 7), so it is default-deny. Grant `{TERMINAL}` for target \
             `{}` bound to session {session_id} to open one — and note that {NOT_A_LEDGER}.",
            authority::terminal_target(session_id, workspace),
        )
    }
}

/// Ask signed policy and the live grants, in that order, at this instant.
///
/// A grant is honoured only when it names *this* session: a terminal grant's
/// target already carries the session and the workspace, but an unscoped row
/// on that target would outlive the session it was made for, and the point of
/// the binding is that it does not.
pub(super) fn decide(s: &AppState, session_id: &str, channel: &str, api: &NativeApi) -> Terminal {
    let target = authority::terminal_target(session_id, &api.directory);
    let args = json!({ "workspace": api.directory, "session": session_id });
    // The same bundle every other authority decision is made against
    // (`http/qa.rs`, `mcp/mod.rs`): a capability and a forge action must not
    // be able to disagree about which policy is in force.
    let decision = {
        let policy = s.tools.policy.read().unwrap();
        authority::decide(
            s.store(),
            &policy,
            &authority::AuthorityQuery {
                channel,
                session_id,
                action: TERMINAL,
                target: &target,
                revision: None,
                args: &args,
            },
        )
    };
    let decision = match decision {
        Ok(decision) => decision,
        Err(e) => {
            tracing::error!(error = %e, session = session_id, "the terminal capability could not be decided; refusing");
            return Terminal {
                verdict: Verdict::Deny,
                grant: None,
                reason: Some("the node could not read its own grants".into()),
            };
        }
    };
    let grant = decision
        .rule_id
        .as_deref()
        .and_then(|id| s.store().authority_grant(id).ok().flatten());
    // A rule in the signed bundle can allow the capability, but a grant is
    // what binds it to a session. An allow that came from neither a
    // session-bound grant nor a bundle rule is not one.
    if decision.verdict == Verdict::Allow {
        if let Some(row) = &grant {
            if row.session_id.as_deref() != Some(session_id) {
                return Terminal {
                    verdict: Verdict::Ask,
                    grant: None,
                    reason: Some(format!(
                        "grant {} is not bound to session {session_id}",
                        row.id
                    )),
                };
            }
        }
    }
    Terminal {
        verdict: decision.verdict,
        grant,
        reason: decision.reason,
    }
}

/// The grant, as it goes on the record: which decision, made when, expiring
/// when. "Who" is the operator — every route into this gateway has already
/// been through the operator guard — and the grant row is the durable
/// evidence of their having made it.
fn grant_note(terminal: &Terminal) -> Value {
    match &terminal.grant {
        Some(row) => json!({
            "id": row.id,
            "channel": row.channel,
            "session_id": row.session_id,
            "target": row.target,
            "granted_ms": row.created_ms,
            "expires_ms": row.expires_ms,
            "reason": row.reason,
        }),
        None => json!({ "id": null, "reason": terminal.reason }),
    }
}

// ---------------------------------------------------------------------------
// The spawn, rewritten
// ---------------------------------------------------------------------------

/// What the harness will actually be asked to spawn, and what was taken out
/// of the request to get there.
pub(super) struct Spawn {
    pub body: Bytes,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env_kept: Vec<String>,
    pub env_dropped: Vec<String>,
}

/// Rewrite a `POST /pty` body into one tracon is willing to send.
///
/// Nothing here trusts a field: `cwd` is replaced, `env` is filtered to an
/// allowlist, `command` is checked against the shells the image lists, and
/// every other key the caller sent is dropped rather than passed through —
/// `Pty.CreateInput` has exactly five fields, and a sixth appearing in a later
/// release should arrive at a refusal, not at the harness.
pub(super) async fn rewrite(body: &Bytes, api: &NativeApi) -> Result<Spawn, String> {
    let asked: Value = if body.trim_ascii().is_empty() {
        json!({})
    } else {
        serde_json::from_slice(body).map_err(|e| format!("the body is not JSON: {e}"))?
    };
    let Some(asked) = asked.as_object() else {
        return Err("the body is not a JSON object".into());
    };

    let cwd = pinned_cwd(asked.get("cwd").and_then(Value::as_str), &api.directory)?;

    let shells = shells(api).await;
    let command = match asked.get("command").and_then(Value::as_str) {
        Some(named) if !named.trim().is_empty() => {
            let named = named.trim();
            if !shells.iter().any(|shell| shell == named) {
                return Err(format!(
                    "{named} is not one of this workspace's shells ({}); \
                     the terminal capability opens a shell, not an arbitrary command",
                    shells.join(", ")
                ));
            }
            named.to_string()
        }
        // The harness's own default is the configured shell, which is one of
        // these anyway; naming it is what makes the event say which.
        _ => shells
            .first()
            .cloned()
            .unwrap_or_else(|| SHELL_FLOOR[SHELL_FLOOR.len() - 1].to_string()),
    };

    // Arguments are passed through and recorded, not restricted. Restricting
    // them would be theatre: the operator is about to be handed a prompt they
    // can type anything at, so an argument filter would narrow nothing while
    // reading as though it narrowed something.
    let args: Vec<String> = asked
        .get("args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let (env, env_kept, env_dropped) = pinned_env(asked.get("env"));

    let mut out = serde_json::Map::new();
    out.insert("command".into(), json!(command));
    if !args.is_empty() {
        out.insert("args".into(), json!(args));
    }
    out.insert("cwd".into(), json!(cwd));
    if let Some(title) = asked.get("title").and_then(Value::as_str) {
        out.insert(
            "title".into(),
            json!(title.chars().take(200).collect::<String>()),
        );
    }
    out.insert("env".into(), Value::Object(env));

    Ok(Spawn {
        body: Bytes::from(Value::Object(out).to_string()),
        command,
        args,
        cwd,
        env_kept,
        env_dropped,
    })
}

/// The directory the terminal opens in: the workspace, or a subdirectory of
/// it that normalises without a `..` in sight.
///
/// The check is lexical on purpose, and it is worth being clear about why.
/// The path is in the runner's namespace, not the node's, so the node cannot
/// resolve it — a symlink inside the workspace pointing out of it is not
/// visible from here. What actually bounds a terminal to the workspace is the
/// container: the filesystem the harness can see is the one tracon mounted.
/// This is the second bound, not the only one, and it is the one that keeps a
/// request from *naming* somewhere else.
fn pinned_cwd(asked: Option<&str>, workspace: &str) -> Result<String, String> {
    let Some(asked) = asked.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok(workspace.to_string());
    };
    if !asked.starts_with('/') {
        return Err(format!(
            "{asked} is not an absolute path; a terminal opens in this session's workspace"
        ));
    }
    let mut normalised = String::from("/");
    for segment in asked.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                return Err(format!(
                    "{asked} climbs out of the path it names; a terminal opens in \
                     {workspace} or a directory under it"
                ))
            }
            _ => {}
        }
        if normalised.len() > 1 {
            normalised.push('/');
        }
        normalised.push_str(segment);
    }
    let workspace_root = workspace.trim_end_matches('/');
    let under = normalised == workspace_root
        || normalised.starts_with(&format!("{workspace_root}/"))
        || workspace_root.is_empty();
    if !under {
        return Err(format!(
            "{asked} is outside this session's workspace ({workspace}); \
             a directory is an implicit authorization to that path (finding 5)"
        ));
    }
    Ok(normalised)
}

/// The environment the caller is allowed to contribute, and the names of
/// everything taken out of what it asked for.
fn pinned_env(asked: Option<&Value>) -> (serde_json::Map<String, Value>, Vec<String>, Vec<String>) {
    let mut kept = serde_json::Map::new();
    let (mut kept_names, mut dropped) = (Vec::new(), Vec::new());
    let Some(map) = asked.and_then(Value::as_object) else {
        return (kept, kept_names, dropped);
    };
    for (name, value) in map {
        let allowed = ENV_ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(name));
        match (allowed, value.as_str()) {
            (true, Some(text)) if text.len() <= 256 && !text.contains('\0') => {
                kept.insert(name.clone(), json!(text));
                kept_names.push(name.clone());
            }
            _ => dropped.push(name.clone()),
        }
    }
    kept_names.sort();
    dropped.sort();
    (kept, kept_names, dropped)
}

/// The shells this workspace's image lists, as the harness itself reports
/// them. Only the acceptable ones, plus the floor — a list the harness cannot
/// answer must not silently become "anything" or "nothing".
async fn shells(api: &NativeApi) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let url = format!(
        "{}/pty/shells?directory={}",
        api.base.trim_end_matches('/'),
        super::opencode::urlencode(&api.directory)
    );
    let listed = super::opencode::client()
        .get(&url)
        .header(
            axum::http::header::AUTHORIZATION,
            api.authorization.as_str(),
        )
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok();
    if let Some(response) = listed.filter(|r| r.status().is_success()) {
        if let Ok(value) = response.json::<Value>().await {
            let items = value
                .as_array()
                .or_else(|| value["data"].as_array())
                .cloned()
                .unwrap_or_default();
            for item in items {
                let acceptable = item["acceptable"].as_bool().unwrap_or(false);
                if let Some(path) = item["path"].as_str().filter(|_| acceptable) {
                    out.push(path.to_string());
                }
            }
        }
    }
    for floor in SHELL_FLOOR {
        if !out.iter().any(|shell| shell == floor) {
            out.push((*floor).to_string());
        }
    }
    out
}

/// What the event says about a spawn that was admitted.
pub(super) fn opened_event(pty_id: Option<&str>, spawn: &Spawn, terminal: &Terminal) -> Value {
    json!({
        "gateway": "opencode",
        "phase": "spawn",
        "pty_id": pty_id,
        "command": spawn.command,
        "args": spawn.args,
        "cwd": spawn.cwd,
        "env_kept": spawn.env_kept,
        "env_dropped": spawn.env_dropped,
        "grant": grant_note(terminal),
        "audit": NOT_A_LEDGER,
    })
}

// ---------------------------------------------------------------------------
// The ticket
// ---------------------------------------------------------------------------

/// One capability to open one terminal, once, from one place.
struct Ticket {
    /// The operator session that asked for it: the hash of the cookie that
    /// carried the request, or the loopback sentinel when the node was
    /// reached on this machine with no cookie at all. A ticket minted for one
    /// operator cannot be redeemed by another.
    operator: String,
    session_id: String,
    pty_id: String,
    /// The `Origin` the ticket will be presented from, exactly. Empty means
    /// it was minted for a client that sent no `Origin` — a terminal, a
    /// script — and it must then be redeemed without one too.
    audience: String,
    /// The harness's own 60-second ticket, obtained server-side. This is the
    /// only copy; it never reaches the browser.
    upstream: String,
    expires_ms: i64,
}

fn tickets() -> &'static Mutex<HashMap<String, Ticket>> {
    static TICKETS: OnceLock<Mutex<HashMap<String, Ticket>>> = OnceLock::new();
    TICKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Who is asking, as far as the node can tell. The operator guard has already
/// decided that they may ask; this is what binds one ticket to one of them.
fn operator_of(headers: &HeaderMap) -> String {
    crate::http::auth::session_hash(headers).unwrap_or_else(|| "loopback".into())
}

/// The origin a ticket is minted for and must be redeemed from.
fn audience_of(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|o| o.trim().to_string())
        .unwrap_or_default()
}

/// Mint the gateway's ticket for one PTY, having first taken the harness's.
///
/// The order matters: the upstream ticket is obtained *before* anything is
/// handed back, so a browser never holds a tracon ticket that cannot be
/// redeemed, and the harness's ticket never exists anywhere the browser can
/// read it.
pub(super) async fn mint(
    session_id: &str,
    api: &NativeApi,
    pty_id: &str,
    joined: &str,
    headers: &HeaderMap,
) -> Response {
    let url = format!(
        "{}/{joined}?directory={}",
        api.base.trim_end_matches('/'),
        super::opencode::urlencode(&api.directory)
    );
    let upstream = super::opencode::client()
        .post(&url)
        .header(
            axum::http::header::AUTHORIZATION,
            api.authorization.as_str(),
        )
        // The header the harness demands of a ticket request. No `Origin`
        // goes with it: the harness accepts an absent one and would accept
        // any `http://localhost:*` (§8 #13), so sending the browser's would
        // be presenting a check tracon does not rely on.
        .header("x-opencode-ticket", "1")
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            let status = response.status();
            return refusal(
                StatusCode::BAD_GATEWAY,
                &format!("the harness would not issue a terminal ticket ({status})"),
            );
        }
        Err(e) => {
            tracing::warn!(session = session_id, error = %e, "the harness would not issue a terminal ticket");
            return refusal(
                StatusCode::BAD_GATEWAY,
                "the harness would not issue a terminal ticket",
            );
        }
    };
    let body: Value = upstream.json().await.unwrap_or(Value::Null);
    let Some(harness_ticket) = body["ticket"]
        .as_str()
        .or_else(|| body["data"]["ticket"].as_str())
        .filter(|t| !t.is_empty())
    else {
        return refusal(
            StatusCode::BAD_GATEWAY,
            "the harness's terminal ticket was unreadable",
        );
    };

    let now = now_ms();
    let secret = {
        use rand::RngCore;
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    };
    let expires_ms = now + TICKET_TTL.as_millis() as i64;
    {
        let mut held = tickets().lock().unwrap();
        // Expiry is enforced on redemption; this keeps the map from holding
        // what can never be redeemed.
        held.retain(|_, t| t.expires_ms > now);
        held.insert(
            secret.clone(),
            Ticket {
                operator: operator_of(headers),
                session_id: session_id.to_string(),
                pty_id: pty_id.to_string(),
                audience: audience_of(headers),
                upstream: harness_ticket.to_string(),
                expires_ms,
            },
        );
    }
    tracing::debug!(
        session = session_id,
        pty = pty_id,
        "minted a terminal ticket"
    );
    (
        StatusCode::OK,
        axum::Json(json!({
            "ticket": secret,
            "expires_in": TICKET_TTL.as_secs(),
            "expires_ms": expires_ms,
            "subprotocol": format!("{TICKET_SUBPROTOCOL_PREFIX}{secret}"),
            "single_use": true,
            "bound_to": {
                "session": session_id,
                "pty": pty_id,
                "audience": audience_of(headers),
            },
            "mediated_by": "tracon",
        })),
    )
        .into_response()
}

/// Take the ticket out of the map — whatever happens next, it is spent.
fn redeem(secret: &str) -> Option<Ticket> {
    tickets().lock().unwrap().remove(secret)
}

/// The ticket the caller presented, from the query or from the subprotocol.
/// A browser cannot set a header on an upgrade, so it gets both spellings;
/// the subprotocol is the one that keeps a credential out of the URL.
fn presented(query: Option<&str>, headers: &HeaderMap) -> Option<(String, bool)> {
    for pair in query.unwrap_or_default().split('&') {
        if let Some(value) = pair.strip_prefix("ticket=") {
            if !value.is_empty() {
                return Some((value.to_string(), false));
            }
        }
    }
    headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())?
        .split(',')
        .map(str::trim)
        .find_map(|offered| {
            offered
                .strip_prefix(TICKET_SUBPROTOCOL_PREFIX)
                .filter(|value| !value.is_empty())
                .map(|value| (value.to_string(), true))
        })
}

fn refusal(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": { "type": "tracon_refused", "message": message } })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The proxy
// ---------------------------------------------------------------------------

/// `GET /api/opencode/{session_id}/[api/]pty/{pty_id}/connect`, registered
/// ahead of the gateway's catch-all because a WebSocket upgrade has to be
/// taken before the body is, and the catch-all buffers the body.
///
/// Everything the catch-all would have checked is checked here too — the
/// session is this node's, the harness has a native API, the path normalises,
/// the route really is the terminal one, the session is active, the
/// capability is granted — and then two things only an upgrade needs: a
/// tracon ticket, and an `Origin` that matches the one it was minted for.
pub async fn connect(State(s): State<AppState>, mut parts: Parts) -> Response {
    let Some((session_id, raw_tail)) = parts
        .uri
        .path()
        .strip_prefix(super::opencode::MOUNT)
        .and_then(|rest| rest.split_once('/'))
    else {
        return refusal(StatusCode::NOT_FOUND, "not a harness API path");
    };
    let session_id = session_id.to_string();
    let Some(path) = super::model::normalised(raw_tail) else {
        return refusal(
            StatusCode::FORBIDDEN,
            "the path does not normalise to an unambiguous route",
        );
    };
    let joined = path.join("/");
    // The same table the rest of the gateway is decided from. A path that
    // reaches this handler but is not the terminal connect route — because
    // the router matched something this module did not expect — fails closed
    // rather than being proxied on the strength of its shape.
    if !super::opencode::is_terminal_connect(&path) {
        return refusal(StatusCode::FORBIDDEN, "not a route this gateway mediates");
    }
    let Some(pty_id) = path
        .get(path.len().saturating_sub(2))
        .filter(|id| super::model::is_opaque(id))
    else {
        return refusal(StatusCode::FORBIDDEN, "that is not a terminal id");
    };
    let pty_id = pty_id.clone();

    let Ok(Some(row)) = s.store().get_session(&session_id) else {
        return refusal(StatusCode::NOT_FOUND, "no such session on this node");
    };
    let Some(api) = s.manager.native_api(&session_id).await else {
        return refusal(
            StatusCode::CONFLICT,
            "this session is not running a harness with a native API",
        );
    };
    if let Err(e) = s.manager.ensure_active(&session_id) {
        return refusal(StatusCode::CONFLICT, &e.to_string());
    }

    // Every refusal from here is on the session's own log, capped per turn the
    // same way the rest of the gateway's are: a native UI that retries an
    // upgrade must not be able to fill the transcript, and a refused terminal
    // must not be invisible.
    let denied = |reason: &str| {
        super::opencode::refuse(&s, &session_id, &axum::http::Method::GET, &joined, reason)
    };

    let terminal = decide(&s, &session_id, &row.channel, &api);
    if !terminal.allowed() {
        return denied(&terminal.refusal(&row.channel, &session_id, &api.directory));
    }

    // The ticket. Single-use from here: it is out of the map whether or not
    // the rest of this succeeds, so a refused upgrade cannot be retried with
    // the same one.
    let Some((secret, by_subprotocol)) = presented(parts.uri.query(), &parts.headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            denied(
                "a terminal upgrade needs a tracon ticket, in `?ticket=` or the \
                 `tracon.pty.ticket.<value>` subprotocol; ask for one at this PTY's \
                 connect-token route",
            ),
        )
            .into_response();
    };
    let Some(ticket) = redeem(&secret) else {
        return denied("that terminal ticket is spent, expired, or was never issued");
    };
    if ticket.expires_ms <= now_ms() {
        return denied("that terminal ticket has expired");
    }
    if ticket.session_id != session_id || ticket.pty_id != pty_id {
        return denied("that terminal ticket was issued for another session or another terminal");
    }
    if ticket.operator != operator_of(&parts.headers) {
        return denied("that terminal ticket was issued to a different operator session");
    }
    // The origin check the harness does not do. Its own accepts any
    // `http://localhost:*` (§8 #13); this one accepts the single origin the
    // ticket was minted from and nothing else, which is what makes the ticket
    // audience-bound rather than merely short.
    if audience_of(&parts.headers) != ticket.audience {
        return denied("that terminal ticket was issued for a different origin");
    }

    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &s).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => {
            let status = rejection.status();
            return refusal(
                if status == StatusCode::METHOD_NOT_ALLOWED {
                    StatusCode::METHOD_NOT_ALLOWED
                } else {
                    StatusCode::BAD_REQUEST
                },
                "the terminal route is a WebSocket upgrade",
            );
        }
    };
    // The size bound, set on the socket itself rather than only checked after
    // the fact: a client must not be able to make the node buffer a frame it
    // was always going to refuse.
    let upgrade = upgrade
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .max_write_buffer_size(MAX_FRAME);
    let upgrade = if by_subprotocol {
        upgrade.protocols([format!("{TICKET_SUBPROTOCOL_PREFIX}{secret}")])
    } else {
        upgrade
    };

    // The upstream connect, opened before the browser's is accepted. A
    // harness that refuses is a plain HTTP refusal the client can read,
    // rather than an upgrade that succeeds and then closes for reasons the
    // browser cannot see.
    let cursor = cursor_of(parts.uri.query());
    let upstream = match open_upstream(&api, &joined, &ticket.upstream, cursor.as_deref()).await {
        Ok(socket) => socket,
        Err(e) => {
            tracing::warn!(session = %session_id, pty = %pty_id, error = %e, "the harness refused a terminal connection");
            return refusal(
                StatusCode::BAD_GATEWAY,
                "the harness refused the terminal connection",
            );
        }
    };

    let manager = s.manager.clone();
    let frames = s.cfg.session.pty_buffer_frames.max(1);
    let capture = s.cfg.session.pty_capture_output;
    let attached = json!({
        "gateway": "opencode",
        "phase": "attach",
        "pty_id": pty_id,
        "grant": grant_note(&terminal),
        "audit": NOT_A_LEDGER,
        "input_replay": "none: a reconnect starts fresh, and the display is \
                         reconstructed by the app, not by tracon",
    });
    manager.record_event(&session_id, ek::PTY_OPENED, attached);
    upgrade.on_upgrade(move |browser| async move {
        let closed = pump(browser, upstream, frames, capture).await;
        manager.record_event(
            &session_id,
            ek::PTY_CLOSED,
            json!({
                "gateway": "opencode",
                "phase": "detached",
                "pty_id": pty_id,
                "bytes_in": closed.bytes_in,
                "bytes_out": closed.bytes_out,
                "duration_ms": closed.duration_ms,
                "reason": closed.reason,
                "output": closed.output,
                "audit": NOT_A_LEDGER,
            }),
        );
    })
}

/// The harness's own output cursor, passed through. It replays *output* the
/// harness still holds, which is the app's business; tracon keeps no input
/// buffer across a connection, so nothing a previous operator typed can be
/// replayed into the shell by reconnecting.
fn cursor_of(query: Option<&str>) -> Option<String> {
    query.unwrap_or_default().split('&').find_map(|pair| {
        pair.strip_prefix("cursor=")
            .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_string)
    })
}

type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open_upstream(
    api: &NativeApi,
    joined: &str,
    ticket: &str,
    cursor: Option<&str>,
) -> Result<Upstream, String> {
    let base = api
        .base
        .trim_end_matches('/')
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let directory = super::opencode::urlencode(&api.directory);
    let mut url = format!(
        "{base}/{joined}?directory={directory}&location%5Bdirectory%5D={directory}\
         &ticket={}",
        super::opencode::urlencode(ticket)
    );
    if let Some(cursor) = cursor {
        url.push_str(&format!("&cursor={cursor}"));
    }
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("the harness's terminal URL is unusable: {e}"))?;
    // The credential goes with it even though the ticket alone would do: the
    // harness skips Basic auth only on the exact v1 connect path, and the v2
    // one still wants it.
    request.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        api.authorization
            .parse()
            .map_err(|_| "the harness credential is unusable as a header".to_string())?,
    );
    // The same size bound upstream as down: the harness is trusted to run the
    // shell, not to decide how much of the node's memory one frame may take.
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        .max_write_buffer_size(MAX_FRAME);
    let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false)
        .await
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// What a terminal connection left behind. Not the transcript: the shape.
struct Closed {
    bytes_in: u64,
    bytes_out: u64,
    duration_ms: i64,
    reason: String,
    output: Option<String>,
}

/// Pump bytes both ways, bounded in both.
///
/// Each direction has a queue of `frames` frames. A browser that stops
/// reading fills its queue and the connection is closed with a reason —
/// never buffered further, because the alternative is one stalled tab holding
/// as much of the node's memory as a shell can produce.
async fn pump(browser: WebSocket, upstream: Upstream, frames: usize, capture: bool) -> Closed {
    let started = now_ms();
    let bytes_in = std::sync::Arc::new(AtomicU64::new(0));
    let bytes_out = std::sync::Arc::new(AtomicU64::new(0));
    let (mut browser_tx, mut browser_rx) = browser.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    // Toward the browser.
    let (to_browser, mut to_browser_rx) = tokio::sync::mpsc::channel::<Message>(frames);
    // Toward the harness.
    let (to_upstream, mut to_upstream_rx) =
        tokio::sync::mpsc::channel::<tokio_tungstenite::tungstenite::Message>(frames);

    let captured = std::sync::Arc::new(Mutex::new(String::new()));

    let downstream_bytes = bytes_out.clone();
    let downstream_capture = captured.clone();
    let downstream = tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message as Up;
        while let Some(message) = up_rx.next().await {
            let message = match message {
                Ok(message) => message,
                Err(_) => return "the harness's terminal connection failed".to_string(),
            };
            let out = match message {
                Up::Text(text) if text.len() <= MAX_FRAME => {
                    downstream_bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
                    if capture {
                        keep_tail(&downstream_capture, text.as_str());
                    }
                    Message::Text(text.as_str().into())
                }
                Up::Binary(data) if data.len() <= MAX_FRAME => {
                    downstream_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    if capture {
                        keep_tail(&downstream_capture, &String::from_utf8_lossy(&data));
                    }
                    Message::Binary(data)
                }
                Up::Close(_) => return "the harness closed the terminal".to_string(),
                Up::Text(_) | Up::Binary(_) => {
                    return "the harness sent a terminal frame past the size limit".to_string()
                }
                // Ping and pong are the transports', not the terminal's.
                _ => continue,
            };
            if to_browser.try_send(out).is_err() {
                return "the terminal was closed: this client stopped reading and \
                        the gateway will not buffer a stalled terminal"
                    .to_string();
            }
        }
        "the harness's terminal ended".to_string()
    });

    let upstream_bytes = bytes_in.clone();
    let upward = tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message as Up;
        while let Some(message) = browser_rx.next().await {
            let message = match message {
                Ok(message) => message,
                Err(_) => return "the client's terminal connection failed".to_string(),
            };
            let out = match message {
                Message::Text(text) if text.len() <= MAX_FRAME => {
                    upstream_bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
                    Up::Text(text.as_str().into())
                }
                Message::Binary(data) if data.len() <= MAX_FRAME => {
                    upstream_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    Up::Binary(data)
                }
                Message::Close(_) => return "the client closed the terminal".to_string(),
                Message::Text(_) | Message::Binary(_) => {
                    return "the client sent a terminal frame past the size limit".to_string()
                }
                _ => continue,
            };
            if to_upstream.try_send(out).is_err() {
                return "the terminal was closed: the harness stopped reading and \
                        the gateway will not buffer a stalled terminal"
                    .to_string();
            }
        }
        "the client's terminal ended".to_string()
    });

    let writing_down = tokio::spawn(async move {
        while let Some(message) = to_browser_rx.recv().await {
            if browser_tx.send(message).await.is_err() {
                break;
            }
        }
        browser_tx
    });
    let writing_up = tokio::spawn(async move {
        while let Some(message) = to_upstream_rx.recv().await {
            if up_tx.send(message).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    });

    // Whichever side ends first ends the connection: a terminal with one half
    // closed is not a terminal. The other reader is aborted rather than left
    // running, which is also what drops its queue's sender so the writer
    // below can drain and hand the sink back.
    let (mut downstream, mut upward) = (downstream, upward);
    let reason = tokio::select! {
        reason = &mut downstream => {
            upward.abort();
            reason.unwrap_or_else(|_| "the terminal proxy stopped".into())
        }
        reason = &mut upward => {
            downstream.abort();
            reason.unwrap_or_else(|_| "the terminal proxy stopped".into())
        }
    };

    // Say why, on the way out, so the close is legible in a browser console
    // rather than a bare 1006.
    if let Ok(Ok(mut sink)) = tokio::time::timeout(Duration::from_secs(2), writing_down).await {
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code: axum::extract::ws::close_code::NORMAL,
                reason: reason.chars().take(120).collect::<String>().into(),
            })))
            .await;
    }
    writing_up.abort();

    let output = capture.then(|| captured.lock().unwrap().clone());
    Closed {
        bytes_in: bytes_in.load(Ordering::Relaxed),
        bytes_out: bytes_out.load(Ordering::Relaxed),
        duration_ms: now_ms() - started,
        reason,
        output,
    }
}

/// How much of a captured terminal's output is kept when capture is on: the
/// tail, bounded, because the session log is a record of decisions and a
/// terminal produces no decisions to record.
const CAPTURE_TAIL: usize = 64 * 1024;

fn keep_tail(held: &Mutex<String>, chunk: &str) {
    let mut held = held.lock().unwrap();
    held.push_str(chunk);
    if held.len() > CAPTURE_TAIL {
        // On a character boundary, so what is written down is still text.
        let cut = held.len() - CAPTURE_TAIL;
        let cut = (cut..held.len())
            .find(|i| held.is_char_boundary(*i))
            .unwrap_or(held.len());
        *held = held[cut..].to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_opens_in_the_workspace_or_under_it() {
        assert_eq!(pinned_cwd(None, "/work").unwrap(), "/work");
        assert_eq!(pinned_cwd(Some(""), "/work").unwrap(), "/work");
        assert_eq!(pinned_cwd(Some("/work"), "/work").unwrap(), "/work");
        assert_eq!(pinned_cwd(Some("/work/src"), "/work").unwrap(), "/work/src");
        // Redundant separators and `.` normalise rather than refuse.
        assert_eq!(
            pinned_cwd(Some("/work//src/./deep"), "/work").unwrap(),
            "/work/src/deep"
        );
        // A prefix is not a parent.
        assert!(pinned_cwd(Some("/workshop"), "/work").is_err());
        for outside in ["/etc", "/", "/work/../etc", "work/src", "/work/.."] {
            assert!(
                pinned_cwd(Some(outside), "/work").is_err(),
                "{outside} should be refused"
            );
        }
    }

    #[test]
    fn only_the_variables_that_decide_how_a_terminal_looks_survive() {
        let asked = json!({
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
            "PATH": "/work/.bin:/usr/bin",
            "HOME": "/work/planted",
            "ANTHROPIC_API_KEY": "sk-not-this",
            "TRACON_GATEWAY_TOKEN": "nor-this",
        });
        let (env, kept, dropped) = pinned_env(Some(&asked));
        assert_eq!(kept, vec!["LANG".to_string(), "TERM".to_string()]);
        assert_eq!(env.len(), 2);
        assert!(dropped.contains(&"PATH".to_string()));
        assert!(dropped.contains(&"HOME".to_string()));
        assert!(dropped.contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(dropped.contains(&"TRACON_GATEWAY_TOKEN".to_string()));
        // Nothing asked for is nothing kept.
        let (env, kept, dropped) = pinned_env(None);
        assert!(env.is_empty() && kept.is_empty() && dropped.is_empty());
    }

    #[test]
    fn a_ticket_is_read_from_the_query_or_the_subprotocol() {
        let none = HeaderMap::new();
        assert_eq!(
            presented(Some("cursor=4&ticket=abc"), &none),
            Some(("abc".into(), false))
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            "tracon.pty.ticket.xyz".parse().unwrap(),
        );
        assert_eq!(presented(None, &headers), Some(("xyz".into(), true)));
        assert_eq!(presented(Some("ticket="), &none), None);
        assert_eq!(presented(None, &none), None);
    }

    #[test]
    fn only_a_plain_integer_cursor_is_passed_through() {
        assert_eq!(cursor_of(Some("cursor=42")).as_deref(), Some("42"));
        assert_eq!(cursor_of(Some("cursor=-1")), None);
        assert_eq!(cursor_of(Some("cursor=4;rm")), None);
        assert_eq!(cursor_of(None), None);
    }

    #[test]
    fn a_captured_tail_stays_bounded() {
        let held = Mutex::new(String::new());
        for _ in 0..64 {
            keep_tail(&held, &"x".repeat(4096));
        }
        assert!(held.lock().unwrap().len() <= CAPTURE_TAIL);
        // Multi-byte output is cut on a character boundary, not mid-codepoint.
        let held = Mutex::new(String::new());
        for _ in 0..40_000 {
            keep_tail(&held, "é");
        }
        let kept = held.lock().unwrap();
        assert!(kept.len() <= CAPTURE_TAIL);
        assert!(kept.chars().all(|c| c == 'é'), "cut mid-codepoint");
    }
}
