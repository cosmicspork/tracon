//! The policy-aware gateway in front of one session's OpenCode server.
//!
//! OpenCode's native API is a single Basic credential over a surface that
//! includes config writes, provider logins, arbitrary process spawning, and a
//! `?directory=` parameter that instantiates any path the server process can
//! reach as a project (`docs/reference/opencode-v1.18.30/api-ui.md` §2,
//! findings 1, 2, 4, 5, 7). Handing that credential to a browser would hand it
//! the node. So nothing reaches the harness directly: the operator's own UI —
//! and, at Gate D, the native one — calls
//! `/api/opencode/{session_id}/…` on the operator router, and this module
//! decides what that means.
//!
//! Three rules carry the design.
//!
//! **The credential never leaves the node.** The gateway resolves the running
//! session's endpoint and password from its handle and injects Basic auth
//! upstream. A request that names any other session's endpoint cannot be
//! expressed: the mount point is the tracon session, and the endpoint is
//! looked up, never supplied.
//!
//! **Deny by default, from the matrix.** Every (method, path) is classified
//! readable, mediated, or forbidden against the route matrix. Anything not on
//! the table — an unknown route, an unknown method, a path that does not
//! normalise cleanly — is refused, named, and recorded. The manifest's deny
//! list is spelled out so a refusal says *why*, not merely *no*.
//!
//! **Scope is pinned, not observed.** Every forwarded request carries this
//! session's workspace as `?directory=`, `location[directory]`, and
//! `x-opencode-directory`, overwriting whatever the caller sent, and a body
//! that names a different directory is refused rather than rewritten.
//!
//! And one rule about what is left behind. **A mutation is written down
//! before it is dispatched.** Every mediated call the gateway forwards writes
//! an `opencode_intent` row first, so a call whose answer never comes back is
//! neither reported as done nor as refused: the row is marked uncertain, the
//! session with it, and the ingestion path settles it against the harness's
//! own durable record (`session/ingest.rs`, `store/opencode.rs`).

use std::{sync::OnceLock, time::Duration};

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

use crate::{
    adapter::NativeApi,
    http::api::AppState,
    policy::{Request as PolicyRequest, Verdict},
    session::state::event_kind as ek,
    store::{intent_kind, intent_state, object_kind},
};

/// The prefix this gateway is mounted at on the operator router.
pub(super) const MOUNT: &str = "/api/opencode/";

/// How long a forwarded call may take when the node's configuration names
/// nothing. A mediated call that outlives this is recorded as uncertain: the
/// harness may or may not have done it, and the ingestion path reconciles
/// against the durable stream rather than guessing.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);

fn forward_timeout(s: &AppState) -> Duration {
    match s.cfg.session.harness_api_timeout_secs {
        0 => FORWARD_TIMEOUT,
        secs => Duration::from_secs(secs),
    }
}

/// At most this many gateway refusals recorded per turn. A native UI that
/// retries a forbidden route would otherwise fill the transcript; the
/// operator learns nothing from the fortieth.
const MAX_REFUSALS_PER_TURN: i64 = 50;

/// The policy kind a mediated API call is decided under.
const API_KIND: &str = "opencode_api";

// ---------------------------------------------------------------------------
// The route matrix
// ---------------------------------------------------------------------------

/// What the gateway does with one (method, path).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// Forwarded with the directory pinned and the credential injected.
    Readable,
    /// The same, proxied as a stream rather than buffered.
    Stream,
    /// Served by the node out of what it already reads upstream, and never
    /// forwarded. The native UI's one live channel (finding 20).
    Synthesised,
    /// Decided by tracon before anything reaches the harness.
    Mediated(Mediation),
    /// Refused, with the reason the operator reads.
    Forbidden(&'static str),
}

/// How a mediated call is decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mediation {
    /// Not forwarded at all: run through the session manager, so the turn
    /// epoch, the watchdog, the budget and the ledger apply to a prompt typed
    /// into the native UI exactly as they do to one typed into tracon's.
    Prompt,
    /// A model switch, held to a provider and model the channel is already
    /// authorised for.
    Model,
    /// A permission answer: every `always` rewritten to `once` and the
    /// attempted broadening recorded, then forwarded (findings 1, 2).
    PermissionReply,
    /// Decided by the session's policy under this name, then forwarded.
    Policy(&'static str),
    /// The terminal capability. Refused unless explicitly granted.
    Terminal,
    /// Mediated in principle; refused until tracon owns what it would make.
    Unavailable(&'static str),
}

/// One row of the matrix: a method, a path pattern, and the class.
///
/// In a pattern, `{session}` is a session id that must be *this* session's
/// (finding 5), `*` is one opaque segment, and `**` is one or more trailing
/// segments. Literal rows come before `{session}` rows so `/session/status`
/// is the status route rather than a session called `status`.
type Row = (&'static str, &'static [&'static str], Class);

const ROUTES: &[Row] = &[
    // --- the manifest's deny list, named ---------------------------------
    (
        "PATCH",
        &["config"],
        Class::Forbidden("a config write installs plugins and MCP servers into the runner"),
    ),
    (
        "PATCH",
        &["global", "config"],
        Class::Forbidden("a global config write rewrites every instance on the server"),
    ),
    (
        "POST",
        &["global", "upgrade"],
        Class::Forbidden("self-update would replace the pinned binary this node was inventoried against"),
    ),
    (
        "POST",
        &["global", "dispose"],
        Class::Forbidden("disposing the server ends every session on it; tracon owns the lifecycle"),
    ),
    (
        "POST",
        &["instance", "dispose"],
        Class::Forbidden("disposing the instance ends this session; tracon owns the lifecycle"),
    ),
    (
        "PATCH",
        &["session", "{session}"],
        Class::Forbidden("the body can rewrite the session's permission ruleset (finding 2)"),
    ),
    (
        "POST",
        &["session"],
        Class::Forbidden("sessions are created by tracon, which owns their identity and workspace"),
    ),
    (
        "POST",
        &["api", "session"],
        Class::Forbidden(
            "sessions are created by tracon; this payload carries its own id and location (finding 5)",
        ),
    ),
    (
        "GET",
        &["event"],
        Class::Forbidden("this stream has no durable replay; use the per-session stream (finding 6)"),
    ),
    // The one route on this matrix the node *answers* rather than decides
    // about. The native app streams from here and from nowhere else
    // (finding 20): refusing it left the page unable to show a permission
    // request, and forwarding it would hand this browser every other
    // session's transcript on a stream that cannot be resumed. So tracon
    // serves its own — session-scoped, type-filtered and sequenced — out of
    // the events the node is already reading (`gateway::native_events`).
    // The upstream route stays unforwarded; nothing below reaches the harness.
    ("GET", &["global", "event"], Class::Synthesised),
    (
        "GET",
        &["api", "event"],
        Class::Forbidden("this stream has no durable replay; use the per-session stream (finding 6)"),
    ),
    (
        "POST",
        &["log"],
        Class::Forbidden("the harness writes its own log; tracon's transcript is the record"),
    ),
    // --- readable, v1 -----------------------------------------------------
    ("GET", &["doc"], Class::Readable),
    ("GET", &["config"], Class::Readable),
    ("GET", &["config", "providers"], Class::Readable),
    ("GET", &["global", "health"], Class::Readable),
    ("GET", &["path"], Class::Readable),
    ("GET", &["find"], Class::Readable),
    ("GET", &["find", "file"], Class::Readable),
    ("GET", &["find", "symbol"], Class::Readable),
    ("GET", &["file"], Class::Readable),
    ("GET", &["file", "content"], Class::Readable),
    ("GET", &["file", "status"], Class::Readable),
    ("GET", &["vcs"], Class::Readable),
    ("GET", &["vcs", "status"], Class::Readable),
    ("GET", &["vcs", "diff"], Class::Readable),
    ("GET", &["vcs", "diff", "raw"], Class::Readable),
    ("GET", &["command"], Class::Readable),
    ("GET", &["agent"], Class::Readable),
    ("GET", &["skill"], Class::Readable),
    ("GET", &["lsp"], Class::Readable),
    ("GET", &["formatter"], Class::Readable),
    ("GET", &["mcp"], Class::Readable),
    ("GET", &["permission"], Class::Readable),
    ("GET", &["provider"], Class::Readable),
    ("GET", &["project"], Class::Readable),
    ("GET", &["project", "current"], Class::Readable),
    ("GET", &["project", "*", "directories"], Class::Readable),
    ("GET", &["question"], Class::Readable),
    ("GET", &["pty"], Class::Readable),
    ("GET", &["pty", "shells"], Class::Readable),
    ("GET", &["session"], Class::Readable),
    ("GET", &["session", "status"], Class::Readable),
    ("GET", &["session", "{session}"], Class::Readable),
    ("GET", &["session", "{session}", "children"], Class::Readable),
    ("GET", &["session", "{session}", "todo"], Class::Readable),
    ("GET", &["session", "{session}", "diff"], Class::Readable),
    ("GET", &["session", "{session}", "message"], Class::Readable),
    (
        "GET",
        &["session", "{session}", "message", "*"],
        Class::Readable,
    ),
    // --- readable, v2 -----------------------------------------------------
    ("GET", &["api", "health"], Class::Readable),
    ("GET", &["api", "location"], Class::Readable),
    ("GET", &["api", "agent"], Class::Readable),
    ("GET", &["api", "command"], Class::Readable),
    ("GET", &["api", "skill"], Class::Readable),
    ("GET", &["api", "reference"], Class::Readable),
    ("GET", &["api", "model"], Class::Readable),
    ("GET", &["api", "provider"], Class::Readable),
    ("GET", &["api", "provider", "*"], Class::Readable),
    ("GET", &["api", "fs", "list"], Class::Readable),
    ("GET", &["api", "fs", "find"], Class::Readable),
    ("GET", &["api", "fs", "read", "**"], Class::Readable),
    ("GET", &["api", "pty"], Class::Readable),
    ("GET", &["api", "permission", "request"], Class::Readable),
    ("GET", &["api", "permission", "saved"], Class::Readable),
    ("GET", &["api", "question", "request"], Class::Readable),
    ("GET", &["api", "session"], Class::Readable),
    ("GET", &["api", "session", "active"], Class::Readable),
    ("GET", &["api", "session", "{session}"], Class::Readable),
    (
        "GET",
        &["api", "session", "{session}", "context"],
        Class::Readable,
    ),
    (
        "GET",
        &["api", "session", "{session}", "history"],
        Class::Readable,
    ),
    (
        "GET",
        &["api", "session", "{session}", "message"],
        Class::Readable,
    ),
    (
        "GET",
        &["api", "session", "{session}", "message", "*"],
        Class::Readable,
    ),
    (
        "GET",
        &["api", "session", "{session}", "permission"],
        Class::Readable,
    ),
    (
        "GET",
        &["api", "session", "{session}", "permission", "*"],
        Class::Readable,
    ),
    (
        "GET",
        &["api", "session", "{session}", "question"],
        Class::Readable,
    ),
    // The one stream with durable, sequenced replay (finding 6). The global
    // ones are refused above rather than filtered: a stream that drops events
    // while disconnected cannot be reconciled afterwards.
    (
        "GET",
        &["api", "session", "{session}", "event"],
        Class::Stream,
    ),
    // --- mediated ---------------------------------------------------------
    (
        "POST",
        &["api", "session", "{session}", "prompt"],
        Class::Mediated(Mediation::Prompt),
    ),
    (
        "POST",
        &["session", "{session}", "message"],
        Class::Mediated(Mediation::Prompt),
    ),
    (
        "POST",
        &["session", "{session}", "prompt_async"],
        Class::Mediated(Mediation::Prompt),
    ),
    (
        "POST",
        &["session", "{session}", "abort"],
        Class::Mediated(Mediation::Policy("abort")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "interrupt"],
        Class::Mediated(Mediation::Policy("abort")),
    ),
    (
        "POST",
        &["session", "{session}", "command"],
        Class::Mediated(Mediation::Policy("command")),
    ),
    (
        "POST",
        &["session", "{session}", "summarize"],
        Class::Mediated(Mediation::Policy("compact")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "compact"],
        Class::Mediated(Mediation::Policy("compact")),
    ),
    (
        "POST",
        &["session", "{session}", "revert"],
        Class::Mediated(Mediation::Policy("revert")),
    ),
    (
        "POST",
        &["session", "{session}", "unrevert"],
        Class::Mediated(Mediation::Policy("revert")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "revert", "stage"],
        Class::Mediated(Mediation::Policy("revert")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "revert", "clear"],
        Class::Mediated(Mediation::Policy("revert")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "revert", "commit"],
        Class::Mediated(Mediation::Policy("revert")),
    ),
    ("POST", &["vcs", "apply"], Class::Mediated(Mediation::Policy("vcs_apply"))),
    (
        "POST",
        &["api", "session", "{session}", "model"],
        Class::Mediated(Mediation::Model),
    ),
    (
        "POST",
        &["api", "session", "{session}", "agent"],
        Class::Mediated(Mediation::Unavailable(
            "the session's agent is bound by tracon when the session starts",
        )),
    ),
    (
        "POST",
        &["session", "{session}", "fork"],
        Class::Mediated(Mediation::Unavailable(
            "a forked session is not registered with tracon, so nothing would supervise it",
        )),
    ),
    (
        "POST",
        &["session", "{session}", "init"],
        Class::Mediated(Mediation::Unavailable(
            "writing AGENTS.md from the harness is not mediated yet",
        )),
    ),
    (
        "POST",
        &["permission", "*", "reply"],
        Class::Mediated(Mediation::PermissionReply),
    ),
    (
        "POST",
        &["session", "{session}", "permissions", "*"],
        Class::Mediated(Mediation::PermissionReply),
    ),
    (
        "POST",
        &["api", "session", "{session}", "permission", "*", "reply"],
        Class::Mediated(Mediation::PermissionReply),
    ),
    (
        "POST",
        &["question", "*", "reply"],
        Class::Mediated(Mediation::Policy("question")),
    ),
    (
        "POST",
        &["question", "*", "reject"],
        Class::Mediated(Mediation::Policy("question")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "question", "*", "reply"],
        Class::Mediated(Mediation::Policy("question")),
    ),
    (
        "POST",
        &["api", "session", "{session}", "question", "*", "reject"],
        Class::Mediated(Mediation::Policy("question")),
    ),
    // --- the terminal capability -----------------------------------------
    ("POST", &["pty"], Class::Mediated(Mediation::Terminal)),
    ("POST", &["api", "pty"], Class::Mediated(Mediation::Terminal)),
    ("PUT", &["pty", "*"], Class::Mediated(Mediation::Terminal)),
    ("DELETE", &["pty", "*"], Class::Mediated(Mediation::Terminal)),
    ("GET", &["pty", "*"], Class::Mediated(Mediation::Terminal)),
    ("PUT", &["api", "pty", "*"], Class::Mediated(Mediation::Terminal)),
    (
        "DELETE",
        &["api", "pty", "*"],
        Class::Mediated(Mediation::Terminal),
    ),
    ("GET", &["api", "pty", "*"], Class::Mediated(Mediation::Terminal)),
    (
        "POST",
        &["pty", "*", "connect-token"],
        Class::Mediated(Mediation::Terminal),
    ),
    (
        "POST",
        &["api", "pty", "*", "connect-token"],
        Class::Mediated(Mediation::Terminal),
    ),
    ("GET", &["pty", "*", "connect"], Class::Mediated(Mediation::Terminal)),
    (
        "GET",
        &["api", "pty", "*", "connect"],
        Class::Mediated(Mediation::Terminal),
    ),
];

/// Whole trees the manifest's deny list closes, checked before the table so
/// no row can accidentally reopen one. Everything here is refused whatever
/// the method; the writes among them are what the reasons name.
fn forbidden_tree(method: &Method, path: &[String]) -> Option<&'static str> {
    let first = path.first().map(String::as_str)?;
    let second = path.get(1).map(String::as_str);
    let write = method != Method::GET;
    // `*/share` publishes the session off the node whatever precedes it.
    if path.last().map(String::as_str) == Some("share") {
        return Some("sharing publishes the session's transcript outside the node");
    }
    Some(match (first, second) {
        ("experimental", _) => {
            "the experimental tree (worktrees, workspaces, console accounts, control plane) is not mediated"
        }
        ("tui", _) => "the TUI control surface drives the harness outside tracon's ledger",
        ("sync", _) => "the sync tree injects raw aggregate events into the session's state",
        ("auth", _) => "provider credentials are held by the node and never written through the harness",
        ("provider", Some(_)) => "provider authentication belongs to the node, not to the harness",
        ("mcp", Some(_)) if write => "MCP servers are registered by tracon, not through the harness",
        ("project", _) if write => "project writes would re-point the harness at another directory",
        ("api", Some("credential")) => "credential writes are refused; the node holds the credentials",
        ("api", Some("integration")) => "provider authentication belongs to the node, not to the harness",
        _ => return None,
    })
}

/// Whether one pattern segment matches one path segment.
fn segment_matches(want: &str, got: &str) -> bool {
    match want {
        "*" | "{session}" => super::model::is_opaque(got),
        literal => literal == got,
    }
}

/// Whether a whole pattern matches a normalised path.
fn pattern_matches(pattern: &[&str], path: &[String]) -> bool {
    for (n, want) in pattern.iter().enumerate() {
        if *want == "**" {
            // Only ever the final segment of a pattern: one or more left.
            return path.len() > n;
        }
        match path.get(n) {
            Some(got) if segment_matches(want, got) => {}
            _ => return false,
        }
    }
    pattern.len() == path.len()
}

/// The matrix's verdict on one call, with the pattern that produced it so the
/// session-id positions can be checked against this session's own.
fn classify(method: &Method, path: &[String]) -> (Class, &'static [&'static str]) {
    if let Some(reason) = forbidden_tree(method, path) {
        return (Class::Forbidden(reason), &[]);
    }
    for (verb, pattern, class) in ROUTES {
        if method.as_str() == *verb && pattern_matches(pattern, path) {
            return (*class, pattern);
        }
    }
    (Class::Forbidden("not a route this gateway mediates"), &[])
}

/// The PTY routes, told apart. The matrix already classifies all of them as
/// the terminal capability; these say *which* one, because a spawn is
/// rewritten, a ticket is minted rather than relayed, a removal is recorded,
/// and an upgrade is not this handler's at all.
fn pty_tail(path: &[String]) -> Option<&[String]> {
    match path.first().map(String::as_str) {
        Some("pty") => Some(path),
        Some("api") if path.get(1).map(String::as_str) == Some("pty") => Some(&path[1..]),
        _ => None,
    }
}

fn is_pty_create(method: &Method, path: &[String]) -> bool {
    method == Method::POST && pty_tail(path).is_some_and(|tail| tail.len() == 1)
}

fn is_pty_remove(method: &Method, path: &[String]) -> bool {
    method == Method::DELETE && pty_tail(path).is_some_and(|tail| tail.len() == 2)
}

/// The PTY a `.../pty/{id}/connect-token` names.
fn connect_token_pty(path: &[String]) -> Option<&str> {
    match pty_tail(path)? {
        [_, id, last] if last == "connect-token" && super::model::is_opaque(id) => {
            Some(id.as_str())
        }
        _ => None,
    }
}

/// Whether this is the terminal's WebSocket upgrade — the one route on the
/// table that the catch-all handler does not answer. `gateway::pty::connect`
/// asks before it proxies anything, so the two cannot drift apart.
pub(super) fn is_terminal_connect(path: &[String]) -> bool {
    match pty_tail(path) {
        Some([_, id, last]) => last == "connect" && super::model::is_opaque(id),
        _ => false,
    }
}

/// Whether a first path segment is one the route matrix knows about at all.
///
/// Added for the native UI's origin (`http::ui`), which serves a static bundle
/// at `/` and has to decide, for a path that is not in that bundle, whether it
/// is an API call the app is making or a path nobody owns. "Is it on the
/// matrix" is the only honest way to ask that, and this is the matrix. The
/// answer is deliberately coarse — a `true` here means only that `classify`
/// gets to see the request, and `classify` refuses everything it does not
/// recognise. A `false` is a 404 on that origin, which is what keeps the UI
/// origin from being a catch-all proxy into the harness (finding 3).
pub fn is_api_root(segment: &str) -> bool {
    if ROUTES
        .iter()
        .any(|(_, pattern, _)| pattern.first().is_some_and(|first| *first == segment))
    {
        return true;
    }
    // The deny list is not in `ROUTES`; `forbidden_tree` names whole subtrees.
    // They belong here too, or the UI origin would answer 404 for a route the
    // gateway has a reason for — and "nobody owns this path" and "this is
    // refused because it would re-point the harness" are different answers.
    // Observed: the app calls `GET /experimental/resource` at startup, and a
    // bare 404 told the operator nothing.
    forbidden_tree(&Method::GET, &[segment.to_string(), "probe".to_string()]).is_some()
        || forbidden_tree(&Method::POST, &[segment.to_string(), "probe".to_string()]).is_some()
}

/// A session id in the path that is not this session's. One Basic credential
/// is authority over every session on the server (finding 5), so the path is
/// the only thing that keeps one session's UI out of another's transcript.
fn foreign_session(pattern: &[&str], path: &[String], mine: &str) -> Option<String> {
    pattern
        .iter()
        .zip(path)
        .find(|(want, got)| **want == "{session}" && got.as_str() != mine)
        .map(|(_, got)| got.clone())
}

// ---------------------------------------------------------------------------
// Scope pinning
// ---------------------------------------------------------------------------

/// The query-string keys that select a scope. Every one is dropped from what
/// the caller sent; `auth_token` and `ticket` go with them because they are
/// credentials the browser must never be able to present (finding 4).
const SCOPE_KEYS: &[&str] = &[
    "directory",
    "workspace",
    "location[directory]",
    "location[workspace]",
    "auth_token",
    "ticket",
];

/// Percent-encode a value for a query parameter. Small on purpose: the only
/// value that reaches it is a container path the node itself chose.
pub(super) fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The caller's query with every scope selector removed and this session's
/// workspace pinned in their place, for both the v1 and the v2 spellings.
fn pinned_query(raw: Option<&str>, directory: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for pair in raw.unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let key = pair.split('=').next().unwrap_or_default();
        let key = key
            .replace("%5B", "[")
            .replace("%5b", "[")
            .replace("%5D", "]")
            .replace("%5d", "]")
            .to_ascii_lowercase();
        if SCOPE_KEYS.contains(&key.as_str()) {
            continue;
        }
        kept.push(pair);
    }
    let mut query = kept.join("&");
    if !query.is_empty() {
        query.push('&');
    }
    let dir = urlencode(directory);
    format!("?{query}directory={dir}&location%5Bdirectory%5D={dir}")
}

/// A directory, working directory, or workspace named in a request body that
/// is not this session's. api-ui.md lists the body-borne selectors
/// (`POST /api/session`, `/sync/replay`, `move-session`,
/// `DELETE /experimental/worktree`) and every one of them is forbidden
/// outright; this is the check that keeps a route added later from being a
/// hole, and it is what holds `POST /pty`'s `cwd` to the workspace.
fn foreign_scope_in_body(body: &Bytes, directory: &str) -> Option<String> {
    fn walk(value: &Value, directory: &str, found: &mut Option<String>) {
        if found.is_some() {
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let named = matches!(
                        key.to_ascii_lowercase().as_str(),
                        "directory" | "cwd" | "worktree" | "workspace" | "workspaceid"
                    );
                    if named {
                        if let Some(text) = child.as_str() {
                            if text != directory {
                                *found = Some(format!("{key}={text}"));
                                return;
                            }
                        }
                    }
                    walk(child, directory, found);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, directory, found);
                }
            }
            _ => {}
        }
    }
    if body.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let mut found = None;
    walk(&value, directory, &mut found);
    found
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// The body with a top-level `cwd` removed, so the generic scope check can
/// still read everything else in a PTY create. Only the top level: a `cwd`
/// nested anywhere else is not `Pty.CreateInput`'s and stays refused.
fn strip_cwd(body: &Bytes) -> Bytes {
    if body.is_empty() {
        return body.clone();
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(mut map)) => {
            map.remove("cwd");
            Bytes::from(Value::Object(map).to_string())
        }
        _ => body.clone(),
    }
}

/// The client the gateway uses upstream: no proxy — the endpoint is a
/// loopback publish or a pod address, and an ambient `HTTPS_PROXY` in the
/// node's environment must not redirect it — and no redirect following, so a
/// `Location` the harness returns cannot become a request the node makes.
pub(super) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn answer(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": { "type": "tracon_refused", "message": message } })),
    )
        .into_response()
}

/// Refuse one call, naming the method and the path, and say so on the
/// session's own log. A refusal the operator cannot see is indistinguishable
/// from a request that was never made.
pub(super) fn refuse(
    s: &AppState,
    session_id: &str,
    method: &Method,
    path: &str,
    reason: &str,
) -> Response {
    tracing::warn!(
        session = session_id,
        method = %method,
        path,
        reason,
        "the OpenCode gateway refused a call"
    );
    let attempt = s
        .manager
        .store()
        .count_events_this_turn(session_id, ek::GATEWAY_REFUSED)
        .unwrap_or(0)
        + 1;
    if attempt <= MAX_REFUSALS_PER_TURN {
        s.manager.record_event(
            session_id,
            ek::GATEWAY_REFUSED,
            json!({
                "gateway": "opencode",
                "method": method.as_str(),
                "path": format!("/{path}"),
                "reason": reason,
                "attempt": attempt,
            }),
        );
    }
    answer(
        StatusCode::FORBIDDEN,
        &format!("{method} /{path} is refused: {reason}"),
    )
}

/// `ANY /api/opencode/{session_id}/{*rest}` on the operator router, so the
/// operator's own guard — the cookie or loopback, the `Host` check, and the
/// same-origin test — has already answered before anything here runs.
pub async fn handle(
    State(s): State<AppState>,
    Path((session_id, _rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(Some(row)) = s.store().get_session(&session_id) else {
        return answer(
            StatusCode::NOT_FOUND,
            &format!("no session {session_id} on this node"),
        );
    };
    // A session another node owns is not a 404 here. The request goes to that
    // node over a bounded encrypted stream the hub only relays, and the answer
    // comes back the same way; the owner repeats every check below on its own
    // state before anything reaches its harness (`mesh::stream`).
    if row.node_id != s.node_id {
        return match s.mesh.clone() {
            Some(mesh) => {
                crate::mesh::stream::remote_gateway(&mesh, &row, &method, &uri, headers, body).await
            }
            None => answer(
                StatusCode::NOT_FOUND,
                &format!(
                    "session {session_id} runs on node {} and this node has no mesh to reach it",
                    row.node_id
                ),
            ),
        };
    }
    let Some(api) = s.manager.native_api(&session_id).await else {
        return answer(
            StatusCode::CONFLICT,
            "this session is not running a harness with a native API",
        );
    };

    // Matched on the raw tail rather than axum's decoded capture, which has
    // already lost the difference between a separator the caller sent and one
    // it encoded.
    let Some(raw_tail) = uri
        .path()
        .strip_prefix(MOUNT)
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, tail)| tail)
    else {
        return answer(StatusCode::NOT_FOUND, "not a harness API path");
    };
    let Some(path) = super::model::normalised(raw_tail) else {
        let shown: String = raw_tail.chars().take(120).collect();
        return refuse(
            &s,
            &session_id,
            &method,
            &shown,
            "the path does not normalise to an unambiguous route",
        );
    };
    let joined = path.join("/");

    let (class, pattern) = classify(&method, &path);
    if let Some(foreign) = foreign_session(pattern, &path, &api.session_id) {
        return refuse(
            &s,
            &session_id,
            &method,
            &joined,
            &format!(
                "{foreign} is another session's id; this mount answers only for {}",
                api.session_id
            ),
        );
    }
    if let Class::Forbidden(reason) = class {
        return refuse(&s, &session_id, &method, &joined, reason);
    }
    // A PTY create is the one route whose `cwd` is not refused for differing
    // from the workspace: `gateway::pty::rewrite` replaces it outright, and
    // accepts a normalised subdirectory of the workspace as well as the
    // workspace itself — a stricter check than this one, because it rewrites
    // rather than trusts. Every other key, on that route as on all the
    // others, still has to name this session's scope or nothing.
    let scoped = if is_pty_create(&method, &path) {
        strip_cwd(&body)
    } else {
        body.clone()
    };
    if let Some(named) = foreign_scope_in_body(&scoped, &api.directory) {
        return refuse(
            &s,
            &session_id,
            &method,
            &joined,
            &format!(
                "the body names a scope outside this session's workspace ({named}); \
                 a directory is an implicit authorization to that path (finding 5)"
            ),
        );
    }

    match class {
        Class::Forbidden(_) => unreachable!("refused above"),
        Class::Readable => {
            forward(
                &s,
                &session_id,
                &api,
                &method,
                &joined,
                &uri,
                &headers,
                body,
                false,
                None,
            )
            .await
        }
        Class::Stream => {
            forward(
                &s,
                &session_id,
                &api,
                &method,
                &joined,
                &uri,
                &headers,
                body,
                true,
                None,
            )
            .await
        }
        // Answered here. No upstream request is built, so the harness never
        // sees a `/global/event` on this session's behalf.
        Class::Synthesised => synthesised_events(&s, &session_id, &api, &headers),
        Class::Mediated(mediation) => {
            // A mediated call changes something. A paused or ended session
            // takes none of them, whatever the operator's UI still shows.
            if let Err(e) = s.manager.ensure_active(&session_id) {
                return answer(StatusCode::CONFLICT, &e.to_string());
            }
            mediate(
                &s,
                &session_id,
                &row.channel,
                &api,
                mediation,
                &method,
                &path,
                &joined,
                &uri,
                &headers,
                body,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// The synthesised live channel
// ---------------------------------------------------------------------------

/// How often the node mints a `server.heartbeat`, matching what upstream's own
/// `/global/event` does (`handlers/global.ts`, a 10 s tick with the first one
/// dropped). The app has no handler for the type — it falls through every
/// switch — so this is liveness for the socket, not news for the page.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// `GET /global/event`, answered by the node instead of the harness.
///
/// The native app opens this at startup, reopens it whenever it drops, and
/// streams from nothing else (finding 20). Upstream's version is unscoped
/// across every session on the server and carries no `id:` at all, so it is
/// neither safe to hand a browser nor possible to resume — it stays
/// unforwarded, and this stands in its place.
///
/// What goes out is the app's own global envelope, `{directory, payload}`,
/// where `payload` is the `{id, type, properties}` the app's legacy path reads
/// (`packages/app/src/context/server-sdk.tsx`; the app runs its v2 adapter
/// only on the v2 transport, so the vocabulary here is already v1 —
/// `native_events` does that normalisation). `directory` is this session's
/// pinned workspace, the same one every forwarded request carries, because the
/// app fans events out by directory before it looks inside them.
fn synthesised_events(
    s: &AppState,
    session_id: &str,
    api: &NativeApi,
    headers: &HeaderMap,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio::sync::broadcast::error::RecvError;

    let events = s.manager.native_events(session_id);
    let directory = api.directory.clone();

    // Where this client left off. The v1 SSE client the app uses parses `id:`
    // and resends it as `Last-Event-ID` across its own internal reconnect
    // (`packages/sdk/js/src/v2/gen/core/serverSentEvents.gen.ts`), so a
    // reconnect resumes rather than restarts — which upstream's stream could
    // never offer, because it emits no ids to resume from.
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());

    // Subscribe before reading the ring, so an event published between the two
    // is delivered by the live half rather than falling down the gap.
    let mut live = events.subscribe();
    let replay = match resume {
        Some(after) => events.replay(after),
        // No resume point: start at the head. The durable per-session stream
        // still guarantees ingestion, so nothing the *node* needs is lost
        // here; the page refetches its own state from the snapshot routes when
        // it sees `server.connected`.
        None => Vec::new(),
    };
    let mut delivered = resume.unwrap_or_else(|| events.head());

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let session = session_id.to_string();
    tokio::spawn(async move {
        let frame = |payload: Value| {
            Event::default().data(json!({ "directory": directory, "payload": payload }).to_string())
        };

        // First, as upstream does: it is what marks the server connected in
        // the app and what makes it refetch its snapshots after a reconnect
        // (`server-sync.tsx`, `home-session-index.ts`). Carries no `id:`,
        // so it never moves the client's resume point.
        let connected = frame(json!({
            "id": format!("evt_connected_{}", crate::store::now_ms()),
            "type": "server.connected",
            "properties": {},
        }));
        if tx.send(Ok(connected)).await.is_err() {
            return;
        }

        for held in replay {
            delivered = delivered.max(held.id);
            if tx
                .send(Ok(frame(held.payload.clone()).id(held.id.to_string())))
                .await
                .is_err()
            {
                return;
            }
        }

        let mut beat = tokio::time::interval(HEARTBEAT);
        // The first tick is immediate; upstream drops it too.
        beat.tick().await;
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    let beat = frame(json!({
                        "id": format!("evt_heartbeat_{}", crate::store::now_ms()),
                        "type": "server.heartbeat",
                        "properties": {},
                    }));
                    if tx.send(Ok(beat)).await.is_err() {
                        return;
                    }
                }
                received = live.recv() => match received {
                    Ok(published) => {
                        // Already sent from the ring: the replay and the live
                        // half overlap by design.
                        if published.id <= delivered {
                            continue;
                        }
                        delivered = published.id;
                        if tx
                            .send(Ok(frame(published.payload.clone()).id(published.id.to_string())))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    // This client fell far enough behind that frames were
                    // dropped. End the stream rather than skipping past them:
                    // the app reconnects in ~250 ms with its `Last-Event-ID`,
                    // and the ring fills whatever gap it still can.
                    Err(RecvError::Lagged(missed)) => {
                        tracing::debug!(
                            session = %session,
                            missed,
                            "a native UI stream lagged; ending it so the page resumes"
                        );
                        return;
                    }
                    Err(RecvError::Closed) => return,
                },
            }
        }
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[allow(clippy::too_many_arguments)]
async fn mediate(
    s: &AppState,
    session_id: &str,
    channel: &str,
    api: &NativeApi,
    mediation: Mediation,
    method: &Method,
    path: &[String],
    joined: &str,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    match mediation {
        Mediation::Unavailable(reason) => refuse(s, session_id, method, joined, reason),

        Mediation::Terminal => {
            let terminal = super::pty::decide(s, session_id, channel, api);
            if !terminal.allowed() {
                let message = terminal.refusal(channel, session_id, &api.directory);
                return refuse(s, session_id, method, joined, &message);
            }
            record_decision(
                s,
                session_id,
                ek::POLICY_ALLOWED,
                method,
                joined,
                super::pty::TERMINAL,
                terminal.grant.as_ref().map(|g| g.id.as_str()),
                terminal.grant.as_ref().map(|g| g.reason.as_str()),
            );

            // A ticket request is answered by the node, not relayed. The
            // harness's own ticket is taken server-side and never leaves it;
            // what comes back is tracon's, bound to this operator, this
            // session, this PTY and the origin that asked (finding 7,
            // §8 #13).
            if let Some(pty_id) = connect_token_pty(path) {
                return super::pty::mint(session_id, api, pty_id, joined, headers).await;
            }
            // The upgrade itself never reaches here: it is routed to
            // `gateway::pty::connect` ahead of this mount, because a
            // WebSocket has to be taken before the body is.
            if path.last().map(String::as_str) == Some("connect") {
                return answer(
                    StatusCode::BAD_REQUEST,
                    "the terminal route is a WebSocket upgrade",
                );
            }

            // A spawn is rewritten rather than trusted: the directory, the
            // environment, and the command are tracon's, whatever the caller
            // sent (finding 7).
            let (body, spawned) = if is_pty_create(method, path) {
                match super::pty::rewrite(&body, api).await {
                    Ok(spawn) => (spawn.body.clone(), Some(spawn)),
                    Err(reason) => return refuse(s, session_id, method, joined, &reason),
                }
            } else {
                (body, None)
            };

            let mediated = begin_intent(
                s,
                session_id,
                super::pty::TERMINAL,
                intent_kind::API,
                Some(joined),
                spawned.as_ref().map(|spawn| spawn.command.as_str()),
            );
            let response = forward(
                s,
                session_id,
                api,
                method,
                joined,
                uri,
                headers,
                body,
                false,
                Some(mediated),
            )
            .await;

            let status = response.status();
            if let Some(spawn) = spawned.filter(|_| status.is_success()) {
                // The id the harness gave it, so the open and the close are
                // the same terminal on the record. It is read back out of the
                // response, which is the only place it exists.
                let (parts, body) = response.into_parts();
                let bytes = axum::body::to_bytes(body, 1 << 20)
                    .await
                    .unwrap_or_default();
                let created: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let pty_id = created["id"]
                    .as_str()
                    .or_else(|| created["data"]["id"].as_str());
                s.manager.record_event(
                    session_id,
                    ek::PTY_OPENED,
                    super::pty::opened_event(pty_id, &spawn, &terminal),
                );
                return Response::from_parts(parts, Body::from(bytes));
            }
            if is_pty_remove(method, path) && status.is_success() {
                s.manager.record_event(
                    session_id,
                    ek::PTY_CLOSED,
                    json!({
                        "gateway": "opencode",
                        "phase": "removed",
                        "pty_id": path.last(),
                        "reason": "the terminal was closed through the gateway",
                    }),
                );
            }
            response
        }

        Mediation::Prompt => {
            let Some(text) = prompt_text(&body) else {
                return answer(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no prompt text in the body",
                );
            };
            // Not forwarded. The manager's prompt entry is what applies the
            // turn epoch, the watchdog, the budget and the ledger, so a
            // prompt typed into the native UI is the same event as one typed
            // into tracon's.
            match s.manager.prompt(session_id, text).await {
                Ok(()) => (
                    StatusCode::ACCEPTED,
                    axum::Json(json!({ "ok": true, "mediated_by": "tracon" })),
                )
                    .into_response(),
                Err(e) => answer(StatusCode::CONFLICT, &e.to_string()),
            }
        }

        Mediation::Model => {
            let Some(model) = requested_model(&body) else {
                return answer(StatusCode::UNPROCESSABLE_ENTITY, "no model in the body");
            };
            if !s.manager.model_authorized(channel, &model) {
                record_decision(
                    s,
                    session_id,
                    ek::POLICY_DENIED,
                    method,
                    joined,
                    "model",
                    None,
                    Some(&format!(
                        "{model} is not a model channel {channel} is authorised for"
                    )),
                );
                return answer(
                    StatusCode::FORBIDDEN,
                    &format!("{model} is not a model channel {channel} is authorised for"),
                );
            }
            record_decision(
                s,
                session_id,
                ek::POLICY_ALLOWED,
                method,
                joined,
                "model",
                None,
                Some(&model),
            );
            let mediated = begin_intent(
                s,
                session_id,
                "model",
                intent_kind::API,
                Some(joined),
                Some(&model),
            );
            forward(
                s,
                session_id,
                api,
                method,
                joined,
                uri,
                headers,
                body,
                false,
                Some(mediated),
            )
            .await
        }

        Mediation::PermissionReply => {
            let mut value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
            let broadened = rewrite_always(&mut value);
            // Either vocabulary's key for the same decision: the v2 routes say
            // `reply`, the v1 one the native app uses says `response`.
            let reply = value
                .get("reply")
                .or_else(|| value.get("response"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let upstream_id = permission_id(joined);
            // The v1 route (`POST /permission/{id}/reply`) names no session at
            // all, so the mount alone cannot tell whose request is being
            // answered. The identity map can: a permission this node raised
            // for another session is refused here rather than decided on that
            // session's behalf (finding 5, in the one place the path does not
            // carry a session id).
            if let Ok(Some(owner)) = s
                .store()
                .opencode_object_owner(object_kind::PERMISSION, &upstream_id)
            {
                if owner != session_id {
                    return refuse(
                        s,
                        session_id,
                        method,
                        joined,
                        &format!(
                            "{upstream_id} is a permission request belonging to session {owner}"
                        ),
                    );
                }
            }
            if broadened {
                // An `always` persists a grant inside the harness — in memory
                // in v1, a project-scoped DB row in v2 — that tracon never
                // decided on (finding 2). The narrowing is the decision, and
                // the attempt is part of the record.
                s.manager.record_event(
                    session_id,
                    ek::POLICY_DENIED,
                    json!({
                        "gateway": "opencode",
                        "decision": "always_rewritten_to_once",
                        "permission_id": upstream_id,
                        "reason": "an upstream grant would broaden policy without a tracon decision",
                    }),
                );
            }
            s.manager.record_event(
                session_id,
                ek::PERMISSION_ANSWER,
                json!({
                    "gateway": "opencode",
                    "permission_id": upstream_id,
                    "option_id": reply,
                    "broadening_refused": broadened,
                }),
            );
            // Recorded in the option vocabulary the ingestion path re-sends
            // from, not OpenCode's: a reply this gateway could not confirm is
            // sent again by reconciliation, and it has to be the same answer.
            let option = if reply == "once" {
                crate::adapter::types::OPTION_ALLOW_ONCE
            } else {
                crate::adapter::types::OPTION_REJECT_ONCE
            };
            let mediated = begin_intent(
                s,
                session_id,
                "permission_reply",
                intent_kind::PERMISSION_REPLY,
                Some(&upstream_id),
                Some(option),
            );
            // The answer goes to the route that holds the request, in that
            // route's own spelling.
            let (target, rewritten) = match v2_reply_route(joined) {
                Some(target) => {
                    let mut v2 = json!({ "reply": reply });
                    if let Some(message) = value.get("message") {
                        v2["message"] = message.clone();
                    }
                    (target, Bytes::from(v2.to_string()))
                }
                None => (joined.to_string(), Bytes::from(value.to_string())),
            };
            forward(
                s,
                session_id,
                api,
                method,
                &target,
                uri,
                headers,
                rewritten,
                false,
                Some(mediated),
            )
            .await
        }

        Mediation::Policy(action) => {
            let decision = {
                let policy = s.manager.policy().read();
                policy.decide(&PolicyRequest {
                    channel,
                    kind: Some(API_KIND),
                    title: action,
                    command: None,
                    arguments: None,
                })
            };
            if decision.verdict == Verdict::Deny {
                record_decision(
                    s,
                    session_id,
                    ek::POLICY_DENIED,
                    method,
                    joined,
                    action,
                    decision.rule_id.as_deref(),
                    decision.reason.as_deref(),
                );
                return answer(
                    StatusCode::FORBIDDEN,
                    &decision
                        .reason
                        .unwrap_or_else(|| format!("{action} is denied on channel {channel}")),
                );
            }
            // `ask` does not queue here. The caller is the operator — the
            // request carried their cookie through the operator guard — so
            // the question would be put to the person already asking it. What
            // matters is that the decision is on the record.
            record_decision(
                s,
                session_id,
                ek::POLICY_ALLOWED,
                method,
                joined,
                action,
                decision.rule_id.as_deref(),
                decision.reason.as_deref(),
            );
            let kind = if action == "abort" {
                intent_kind::ABORT
            } else {
                intent_kind::API
            };
            let mediated = begin_intent(s, session_id, action, kind, Some(joined), None);
            let response = forward(
                s,
                session_id,
                api,
                method,
                joined,
                uri,
                headers,
                body,
                false,
                Some(mediated),
            )
            .await;
            // A revert rewrites the working tree, and a candidate review is
            // bound to the tree that was captured (#188). tracon cannot see
            // that happen from the outside, so the one place that knows says
            // so on the session's own log.
            if TREE_CHANGING.contains(&action) && response.status().is_success() {
                s.manager.record_event(
                    session_id,
                    ek::WORKSPACE_CHANGED,
                    json!({
                        "gateway": "opencode",
                        "action": action,
                        "method": method.as_str(),
                        "path": format!("/{joined}"),
                        "reason": "the harness's own API changed the workspace tree",
                    }),
                );
            }
            response
        }
    }
}

/// Mediated actions that move the working tree, and so invalidate anything
/// bound to the tree as it was (#188).
const TREE_CHANGING: &[&str] = &["revert", "vcs_apply"];

// ---------------------------------------------------------------------------
// What a mediated mutation leaves behind
// ---------------------------------------------------------------------------

/// A call tracon decided to make, and the `opencode_intent` row written before
/// it was dispatched.
///
/// The ordering is the point. A mutation whose answer never comes back is
/// neither sent nor not-sent, and the row written *first* is what makes the
/// honest third answer expressible: the session is marked `uncertain`, the
/// next prompt is refused with the reason, and the ingestion path asks the
/// harness what actually happened rather than guessing (`session/ingest.rs`).
/// Without the row, a timed-out revert is simply forgotten.
struct Mediated {
    /// What the call was decided as, for the record.
    action: String,
    /// The intent row's id.
    intent: String,
}

/// Write the intent down, before anything is sent.
fn begin_intent(
    s: &AppState,
    session_id: &str,
    action: &str,
    kind: &str,
    target: Option<&str>,
    detail: Option<&str>,
) -> Mediated {
    let intent = uuid::Uuid::now_v7().to_string();
    if let Err(e) = s
        .store()
        .opencode_intent_begin(&intent, session_id, kind, target, detail)
    {
        // Not fatal: the call is still decided and still recorded as an event.
        // What is lost is the ability to ask about it afterwards, so it is an
        // error rather than a warning.
        tracing::error!(error = %e, session = session_id, "the OpenCode gateway could not record a mutation's intent");
    }
    Mediated {
        action: action.to_string(),
        intent,
    }
}

/// The harness answered, so the outcome is known. A refusal is `failed` —
/// nothing changed upstream — and anything else is `admitted`.
fn settle_intent(s: &AppState, mediated: &Mediated, status: StatusCode) {
    let (state, note) = if status.is_success() {
        (
            intent_state::ADMITTED,
            format!("the harness answered {status}"),
        )
    } else {
        (
            intent_state::FAILED,
            format!("the harness refused it with {status}; nothing changed upstream"),
        )
    };
    let _ = s
        .store()
        .opencode_intent_settle(&mediated.intent, state, Some(&note));
}

/// The harness did not answer. The intent stays on the record as uncertain,
/// the session is marked so a prompt is refused rather than duplicated, and
/// the operator sees why.
fn uncertain_intent(
    s: &AppState,
    session_id: &str,
    mediated: &Mediated,
    method: &Method,
    joined: &str,
    reason: &str,
) {
    let note = format!("{method} /{joined}: {reason}");
    let store = s.store();
    let _ = store.opencode_intent_settle(&mediated.intent, intent_state::UNCERTAIN, Some(&note));
    let _ = store.opencode_set_uncertain(session_id, &note);
    s.manager.record_event(
        session_id,
        ek::UNCERTAIN,
        json!({
            "gateway": "opencode",
            "action": mediated.action,
            "intent": mediated.intent,
            "reason": note,
            "refusing": "prompt",
        }),
    );
}

#[allow(clippy::too_many_arguments)]
fn record_decision(
    s: &AppState,
    session_id: &str,
    kind: &str,
    method: &Method,
    joined: &str,
    action: &str,
    rule: Option<&str>,
    reason: Option<&str>,
) {
    s.manager.record_event(
        session_id,
        kind,
        json!({
            "gateway": "opencode",
            "action": action,
            "method": method.as_str(),
            "path": format!("/{joined}"),
            "rule": rule,
            "reason": reason,
        }),
    );
}

/// The prompt as the native UI sends it, in either the v1 part list or the v2
/// `{prompt:{text}}` shape.
fn prompt_text(body: &Bytes) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let direct = value["prompt"]["text"]
        .as_str()
        .or_else(|| value["text"].as_str());
    if let Some(text) = direct.filter(|t| !t.trim().is_empty()) {
        return Some(text.to_string());
    }
    let parts = value["prompt"]["parts"]
        .as_array()
        .or_else(|| value["parts"].as_array())?;
    let joined: Vec<&str> = parts
        .iter()
        .filter_map(|part| part["text"].as_str())
        .filter(|text| !text.trim().is_empty())
        .collect();
    (!joined.is_empty()).then(|| joined.join("\n"))
}

/// The `provider/model` a model switch asks for, in either spelling.
fn requested_model(body: &Bytes) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if let Some(model) = value["model"].as_str().filter(|m| !m.trim().is_empty()) {
        return Some(model.to_string());
    }
    let provider = value["providerID"]
        .as_str()
        .or_else(|| value["provider"].as_str())?;
    let model = value["modelID"]
        .as_str()
        .or_else(|| value["model"].as_str())?;
    Some(format!("{provider}/{model}"))
}

/// Narrow every reply that would persist a grant inside the harness. Returns
/// whether anything was narrowed.
fn rewrite_always(body: &mut Value) -> bool {
    let mut broadened = false;
    for key in ["reply", "response", "action", "decision"] {
        if body.get(key).and_then(Value::as_str) == Some("always") {
            body[key] = json!("once");
            broadened = true;
        }
    }
    // v2 persists only when the reply carries a non-empty `save`; an empty
    // one is the difference between a decision and a standing grant.
    if body
        .get("save")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        body["save"] = json!([]);
        broadened = true;
    }
    broadened
}

/// The v1 permission-answer route, mapped onto the one that holds the request.
///
/// The native app is a v1 client — upstream's own protocol probe asks
/// `/global/health` first, and this build answers it — so it answers a
/// permission on `POST /session/{id}/permissions/{permissionID}` with
/// `{"response": …}`. But this build *raises* its asks in the v2 permission
/// system, whose store the v1 reply handler does not read: the answer comes
/// back **404**, the harness stays blocked, and the operator is looking at a
/// control that did nothing. That only became visible once the page could show
/// a control at all (finding 20); the route trace caught it in the same run.
///
/// So the gateway spells the answer the way the ask was raised. This is the
/// same kind of mediation as narrowing `always` to `once`: *what* the operator
/// decided is theirs, and which of two upstream vocabularies carries it across
/// the wire is the gateway's business. It also leaves one vocabulary on the
/// node — the adapter already answers every permission on this route
/// (`adapter::opencode`), so the operator's UI and tracon's own reconciliation
/// now re-send the identical request, which is what makes re-sending safe.
fn v2_reply_route(joined: &str) -> Option<String> {
    match joined.split('/').collect::<Vec<&str>>().as_slice() {
        ["session", session, "permissions", id] => {
            Some(format!("api/session/{session}/permission/{id}/reply"))
        }
        _ => None,
    }
}

/// The harness's own permission id out of the path, for the record.
fn permission_id(joined: &str) -> String {
    let segments: Vec<&str> = joined.split('/').collect();
    match segments.as_slice() {
        [.., id, "reply"] => (*id).to_string(),
        [.., "permissions", id] => (*id).to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Forwarding
// ---------------------------------------------------------------------------

/// Headers never passed upstream: the operator's own credentials, the
/// caller's scope selectors, and the hop-by-hop ones reqwest sets itself.
const DROP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "cookie",
    "content-length",
    "connection",
    "transfer-encoding",
    "upgrade",
    "origin",
    "referer",
    "x-opencode-directory",
    "x-opencode-workspace",
    "x-opencode-ticket",
];

/// Headers never passed back: the harness's own CSP and CORS (tracon sets its
/// own, finding 3), its challenge, and anything that would set a cookie on
/// the operator's origin.
/// The snapshot routes that answer "what is waiting to be answered", and the
/// one place the gateway rewrites a readable body.
///
/// These are how the app finds a permission that was raised before the page
/// opened — it never learns those from the stream (`bootstrap.ts`,
/// `permission.tsx`) — and every one of them is **instance**-wide. The path
/// names no session, and one OpenCode server holds more than one, so
/// forwarding the body unchanged would put another session's pending request
/// on this session's page and invite the operator to answer it. Pinning
/// `?directory=` does not help: sibling sessions share the directory.
///
/// So the entries are filtered to this session. The route stays readable —
/// the harness is still asked, and the answer is still its own — but what
/// comes back is scoped the way every other route on this mount is scoped by
/// its path (finding 5).
const SCOPED_ASK_ROUTES: &[&str] = &[
    // v1: a bare array of requests.
    "permission",
    "question",
    // v2: `{location, data: [...]}`.
    "api/permission/request",
    "api/question/request",
];

/// Keep only the entries that belong to this session. Anything that is not one
/// of the shapes above is returned untouched — this rewrites a body it
/// recognises and never guesses at one it does not.
fn scope_asks(joined: &str, session: &str, bytes: Bytes) -> Bytes {
    if !SCOPED_ASK_ROUTES.contains(&joined) {
        return bytes;
    }
    let Ok(mut body) = serde_json::from_slice::<Value>(&bytes) else {
        return bytes;
    };
    let mine = |items: &Vec<Value>| -> Vec<Value> {
        items
            .iter()
            .filter(|item| super::native_events::ask_is_mine(item, session))
            .cloned()
            .collect()
    };
    match &body {
        Value::Array(items) => body = Value::Array(mine(items)),
        Value::Object(_) => match body["data"].as_array() {
            Some(items) => body["data"] = Value::Array(mine(items)),
            None => return bytes,
        },
        _ => return bytes,
    }
    Bytes::from(body.to_string())
}

const DROP_RESPONSE_HEADERS: &[&str] = &[
    "transfer-encoding",
    "content-length",
    "connection",
    "set-cookie",
    "www-authenticate",
    "content-security-policy",
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
];

#[allow(clippy::too_many_arguments)]
async fn forward(
    s: &AppState,
    session_id: &str,
    api: &NativeApi,
    method: &Method,
    joined: &str,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
    stream: bool,
    mediated: Option<Mediated>,
) -> Response {
    let query = pinned_query(uri.query(), &api.directory);
    let target = format!("{}/{joined}{query}", api.base.trim_end_matches('/'));
    let mut req = client().request(method.clone(), &target);
    for (name, value) in headers.iter() {
        if DROP_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        req = req.header(name, value);
    }
    // The credential, injected here and nowhere else: the browser holds a
    // tracon cookie and never the harness password (finding 4).
    req = req.header(
        axum::http::header::AUTHORIZATION,
        api.authorization.as_str(),
    );
    if let Ok(directory) = HeaderValue::from_str(&api.directory) {
        req = req.header(HeaderName::from_static("x-opencode-directory"), directory);
    }
    if !body.is_empty() {
        req = req.body(body);
    }
    if !stream {
        req = req.timeout(forward_timeout(s));
    }

    let upstream = match req.send().await {
        Ok(response) => response,
        Err(e) => {
            if let Some(mediated) = &mediated {
                // Whether the harness did it is unknown: the request may have
                // been received and answered late. The intent written before
                // dispatch is marked uncertain, so reconciliation can settle
                // it against the durable stream rather than guessing.
                let reason = if e.is_timeout() {
                    "the harness did not answer in time"
                } else {
                    "the harness was unreachable"
                };
                s.manager.record_event(
                    session_id,
                    ek::ERROR,
                    json!({
                        "gateway": "opencode",
                        "action": mediated.action,
                        "method": method.as_str(),
                        "path": format!("/{joined}"),
                        "outcome": "uncertain",
                        "intent": mediated.intent,
                        "reason": reason,
                    }),
                );
                uncertain_intent(s, session_id, mediated, method, joined, reason);
            }
            tracing::warn!(session = session_id, error = %e, path = joined, "the OpenCode gateway could not reach the harness");
            return answer(
                if e.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "the harness did not answer",
            );
        }
    };

    if let Some(mediated) = &mediated {
        settle_intent(s, mediated, upstream.status());
    }

    let mut out = Response::builder().status(upstream.status().as_u16());
    for (name, value) in upstream.headers() {
        if DROP_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out = out.header(name, value);
    }
    let body = if stream {
        Body::from_stream(upstream.bytes_stream())
    } else {
        match upstream.bytes().await {
            Ok(bytes) => Body::from(scope_asks(joined, &api.session_id, bytes)),
            Err(e) => {
                tracing::warn!(session = session_id, error = %e, "the harness's answer was cut short");
                return answer(
                    StatusCode::BAD_GATEWAY,
                    "the harness's answer was cut short",
                );
            }
        }
    };
    out.body(body)
        .unwrap_or_else(|_| answer(StatusCode::BAD_GATEWAY, "the harness answered unusably"))
}

// ---------------------------------------------------------------------------
// The route trace
// ---------------------------------------------------------------------------

/// The class names the native UI's route trace
/// (`docs/reference/opencode-v1.18.30/ui-route-trace.tsv`) writes down, and the
/// vocabulary `node/tests/opencode_route_trace.rs` re-derives every recorded
/// row from. They are the matrix's own [`Class`] made into strings, with one
/// split: [`Mediation::Unavailable`] is named apart from the rest of
/// [`Class::Mediated`] because it is a row the matrix *has* and still refuses,
/// which is a different fact from a route the matrix never heard of.
pub mod trace {
    pub const READABLE: &str = "readable";
    pub const STREAM: &str = "stream";
    /// Served by the node out of what it already reads, never forwarded: the
    /// native UI's live channel (finding 20). Distinct from `stream` on
    /// purpose — one is a proxied upstream response, the other is a route the
    /// harness is never asked about.
    pub const SYNTHESISED: &str = "synthesised";
    pub const MEDIATED: &str = "mediated";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const FORBIDDEN: &str = "forbidden";
    /// What the trace records for a call nothing on this node claimed, and
    /// what a recorded row may never be. Deny-by-default means the matrix
    /// always has an answer for a request that reaches it, so the only ones
    /// here are the requests that do not: a path that does not normalise, a
    /// method that is not one, and a path the UI origin answered 404 without
    /// asking (`http::ui::trace::UNKNOWN`).
    pub const UNKNOWN: &str = "unknown";
}

/// What the matrix makes of one (method, path), named for the route trace.
///
/// The same `classify` the handler runs, reached from a test with no node
/// behind it — so a checked-in trace can be re-derived from the matrix in CI
/// with no browser, no harness, and no bundle on the machine. A path that does
/// not normalise, or a method that is not one, is [`trace::UNKNOWN`]: the
/// handler refuses both, and the trace must not launder either into a class.
pub fn trace_class(method: &str, path: &str) -> &'static str {
    let Ok(method) = Method::from_bytes(method.as_bytes()) else {
        return trace::UNKNOWN;
    };
    let Some(path) = super::model::normalised(path.trim_start_matches('/')) else {
        return trace::UNKNOWN;
    };
    match classify(&method, &path).0 {
        Class::Readable => trace::READABLE,
        Class::Stream => trace::STREAM,
        Class::Synthesised => trace::SYNTHESISED,
        Class::Mediated(Mediation::Unavailable(_)) => trace::UNAVAILABLE,
        Class::Mediated(_) => trace::MEDIATED,
        Class::Forbidden(_) => trace::FORBIDDEN,
    }
}

/// Whether a class the trace recorded is one the gateway answers by refusing,
/// with a 403 and a `gateway_refused` event behind it. Both halves of the deny
/// list end there: the trees and rows `classify` calls [`Class::Forbidden`],
/// and the rows it mediates into [`Mediation::Unavailable`], which `mediate`
/// refuses through the same `refuse`.
pub fn trace_class_refuses(class: &str) -> bool {
    matches!(class, trace::FORBIDDEN | trace::UNAVAILABLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> Vec<String> {
        super::super::model::normalised(raw).expect("a normalising path")
    }

    fn class_of(method: &str, raw: &str) -> Class {
        classify(&Method::from_bytes(method.as_bytes()).unwrap(), &path(raw)).0
    }

    /// Every route on the manifest's deny list, and the trees around them.
    #[test]
    fn the_deny_list_is_refused() {
        for (method, route) in [
            ("PATCH", "config"),
            ("PATCH", "global/config"),
            ("PUT", "auth/anthropic"),
            ("DELETE", "auth/anthropic"),
            ("GET", "provider/auth"),
            ("POST", "provider/anthropic/oauth/authorize"),
            ("POST", "global/upgrade"),
            ("PATCH", "session/ses_x"),
            ("GET", "experimental/capabilities"),
            ("POST", "experimental/worktree"),
            ("POST", "tui/execute-command"),
            ("POST", "sync/replay"),
            ("POST", "mcp/name/connect"),
            ("POST", "session/ses_x/share"),
            ("DELETE", "session/ses_x/share"),
            ("POST", "global/dispose"),
            ("POST", "instance/dispose"),
            ("POST", "project/git/init"),
            ("PATCH", "api/credential/c1"),
            ("POST", "api/integration/i1/connect/key"),
            ("GET", "event"),
            ("GET", "api/event"),
            ("POST", "api/session"),
        ] {
            assert!(
                matches!(class_of(method, route), Class::Forbidden(_)),
                "{method} /{route} is not refused: {:?}",
                class_of(method, route)
            );
        }
    }

    /// Anything the table does not name at all, including a method the route
    /// does have under another verb.
    #[test]
    fn an_unknown_route_or_method_fails_closed() {
        for (method, route) in [
            ("GET", "not/a/route"),
            ("POST", "config"),
            ("DELETE", "api/session/ses_x"),
            ("POST", "api/session/ses_x/wait"),
            ("PUT", "api/session/ses_x/prompt"),
        ] {
            assert!(
                matches!(class_of(method, route), Class::Forbidden(_)),
                "{method} /{route} should fail closed"
            );
        }
    }

    /// A literal route is itself, not a session whose id happens to spell it.
    #[test]
    fn literal_routes_win_over_the_session_pattern() {
        assert_eq!(class_of("GET", "session/status"), Class::Readable);
        assert_eq!(class_of("GET", "api/session/active"), Class::Readable);
        assert_eq!(
            classify(&Method::GET, &path("api/session/active")).1,
            &["api", "session", "active"]
        );
    }

    /// The durable per-session stream is the only upstream stream forwarded,
    /// and `/global/event` is served rather than forwarded — which is a
    /// different thing from being allowed. Nothing here widens the deny list:
    /// the two remaining unscoped, unreplayable streams stay refused, and the
    /// route the app actually opens never becomes an upstream request.
    #[test]
    fn the_durable_stream_is_the_only_one_proxied() {
        assert_eq!(class_of("GET", "api/session/ses_x/event"), Class::Stream);
        assert_eq!(class_of("GET", "global/event"), Class::Synthesised);
        assert!(matches!(class_of("GET", "event"), Class::Forbidden(_)));
        assert!(matches!(class_of("GET", "api/event"), Class::Forbidden(_)));
        // The synthesised route is one exact (method, path). A neighbour on
        // the same tree is not swept along with it.
        assert!(matches!(
            class_of("POST", "global/event"),
            Class::Forbidden(_)
        ));
        assert!(matches!(
            class_of("GET", "global/event/all"),
            Class::Forbidden(_)
        ));
    }

    /// The one readable body the gateway rewrites, and the reason it does.
    /// These snapshots are instance-wide — the path names no session — so a
    /// sibling session's pending permission would otherwise land on this
    /// session's page.
    #[test]
    fn the_pending_ask_snapshots_are_scoped_to_this_session() {
        let mine = json!({ "id": "per_1", "sessionID": "ses_x" });
        let theirs = json!({ "id": "per_2", "sessionID": "ses_other" });

        // v1: a bare array.
        let body = Bytes::from(json!([mine, theirs]).to_string());
        let scoped: Value =
            serde_json::from_slice(&scope_asks("permission", "ses_x", body)).unwrap();
        assert_eq!(scoped, json!([mine]));

        // v2: `{location, data}` — the envelope survives, the entries are cut.
        let body = Bytes::from(
            json!({ "location": { "directory": "/work" }, "data": [mine, theirs] }).to_string(),
        );
        let scoped: Value =
            serde_json::from_slice(&scope_asks("api/permission/request", "ses_x", body)).unwrap();
        assert_eq!(scoped["data"], json!([mine]));
        assert_eq!(scoped["location"]["directory"], "/work");

        // Questions are asks too, and go the same way.
        let body = Bytes::from(json!([mine, theirs]).to_string());
        let scoped: Value = serde_json::from_slice(&scope_asks("question", "ses_x", body)).unwrap();
        assert_eq!(scoped, json!([mine]));

        // Every other readable body is passed through untouched — this
        // rewrites what it recognises and never guesses at what it does not.
        let body = Bytes::from(json!([mine, theirs]).to_string());
        assert_eq!(
            scope_asks("session", "ses_x", body.clone()),
            body,
            "an unrelated route's body is not rewritten"
        );
        let opaque = Bytes::from_static(b"not json at all");
        assert_eq!(scope_asks("permission", "ses_x", opaque.clone()), opaque);
    }

    #[test]
    fn a_pty_needs_the_capability_and_a_prompt_never_forwards() {
        assert_eq!(
            class_of("POST", "pty"),
            Class::Mediated(Mediation::Terminal)
        );
        assert_eq!(
            class_of("POST", "api/session/ses_x/prompt"),
            Class::Mediated(Mediation::Prompt)
        );
    }

    /// A foreign session id is caught where the pattern says a session id is.
    #[test]
    fn a_foreign_session_id_is_seen() {
        let p = path("api/session/ses_other/prompt");
        let (_, pattern) = classify(&Method::POST, &p);
        assert_eq!(
            foreign_session(pattern, &p, "ses_mine").as_deref(),
            Some("ses_other")
        );
        let mine = path("api/session/ses_mine/prompt");
        let (_, pattern) = classify(&Method::POST, &mine);
        assert!(foreign_session(pattern, &mine, "ses_mine").is_none());
    }

    #[test]
    fn the_directory_is_pinned_over_whatever_the_caller_sent() {
        let query = pinned_query(
            Some("after=7&directory=%2Fetc&location%5Bdirectory%5D=%2Fetc"),
            "/work",
        );
        assert!(query.starts_with("?after=7&"), "{query}");
        assert!(query.contains("directory=/work"), "{query}");
        assert!(query.contains("location%5Bdirectory%5D=/work"), "{query}");
        assert!(!query.contains("/etc"), "{query}");
        // Nothing to keep is still pinned.
        assert_eq!(
            pinned_query(None, "/work"),
            "?directory=/work&location%5Bdirectory%5D=/work"
        );
    }

    #[test]
    fn a_body_naming_another_directory_is_seen() {
        let body = Bytes::from(json!({ "location": { "directory": "/etc" } }).to_string());
        assert!(foreign_scope_in_body(&body, "/work").is_some());
        let ours = Bytes::from(json!({ "location": { "directory": "/work" } }).to_string());
        assert!(foreign_scope_in_body(&ours, "/work").is_none());
        let cwd = Bytes::from(json!({ "command": "bash", "cwd": "/" }).to_string());
        assert!(foreign_scope_in_body(&cwd, "/work").is_some());
    }

    #[test]
    fn always_becomes_once() {
        let mut body = json!({ "reply": "always", "save": [{ "resource": "**" }] });
        assert!(rewrite_always(&mut body));
        assert_eq!(body["reply"], "once");
        assert_eq!(body["save"], json!([]));
        let mut once = json!({ "reply": "once" });
        assert!(!rewrite_always(&mut once));
        assert_eq!(once["reply"], "once");
    }

    #[test]
    fn a_prompt_is_read_in_either_shape() {
        let v2 = Bytes::from(json!({ "prompt": { "text": "fix it" } }).to_string());
        assert_eq!(prompt_text(&v2).as_deref(), Some("fix it"));
        let parts = Bytes::from(
            json!({ "parts": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] })
                .to_string(),
        );
        assert_eq!(prompt_text(&parts).as_deref(), Some("a\nb"));
        assert!(prompt_text(&Bytes::from("{}")).is_none());
    }

    #[test]
    fn a_model_is_read_in_either_shape() {
        let flat = Bytes::from(json!({ "model": "anthropic/claude-x" }).to_string());
        assert_eq!(
            requested_model(&flat).as_deref(),
            Some("anthropic/claude-x")
        );
        let split =
            Bytes::from(json!({ "providerID": "anthropic", "modelID": "claude-x" }).to_string());
        assert_eq!(
            requested_model(&split).as_deref(),
            Some("anthropic/claude-x")
        );
    }

    /// The trace's names are the matrix's verdicts, not a second opinion.
    #[test]
    fn the_trace_names_what_the_matrix_decided() {
        assert_eq!(trace_class("GET", "/global/health"), trace::READABLE);
        assert_eq!(
            trace_class("GET", "/api/session/ses_x/event"),
            trace::STREAM
        );
        assert_eq!(
            trace_class("POST", "/api/session/ses_x/prompt"),
            trace::MEDIATED
        );
        assert_eq!(
            trace_class("POST", "/session/ses_x/fork"),
            trace::UNAVAILABLE
        );
        assert_eq!(
            trace_class("GET", "/global/event"),
            trace::SYNTHESISED,
            "the route the app streams from is the node's own, not a proxied one"
        );
        assert_eq!(trace_class("PATCH", "/config"), trace::FORBIDDEN);
        assert_eq!(
            trace_class("POST", "/session/ses_x/share"),
            trace::FORBIDDEN
        );
        // A route the matrix never heard of, and a method it does not have for
        // a route it does: refused, not unplaced. Deny-by-default means the
        // matrix always has an answer, so `unknown` is never the gateway's.
        assert_eq!(trace_class("GET", "/not/a/route"), trace::FORBIDDEN);
        assert!(trace_class_refuses(trace_class(
            "PUT",
            "/api/session/ses_x/prompt"
        )));
        // Only a request the handler would not get as far as classifying.
        assert_eq!(trace_class("GET", "/../etc/passwd"), trace::UNKNOWN);
        assert_eq!(trace_class("WHAT EVER", "/global/health"), trace::UNKNOWN);
        // Both halves of the deny list are refusals; nothing else is.
        assert!(trace_class_refuses(trace::FORBIDDEN));
        assert!(trace_class_refuses(trace::UNAVAILABLE));
        assert!(!trace_class_refuses(trace::READABLE));
        assert!(!trace_class_refuses(trace::MEDIATED));
        assert!(!trace_class_refuses(trace::STREAM));
        assert!(!trace_class_refuses(trace::SYNTHESISED));
    }

    #[test]
    fn the_permission_id_comes_out_of_the_path() {
        assert_eq!(permission_id("permission/per_1/reply"), "per_1");
        assert_eq!(
            permission_id("api/session/ses_x/permission/per_2/reply"),
            "per_2"
        );
        assert_eq!(permission_id("session/ses_x/permissions/per_3"), "per_3");
    }
}
