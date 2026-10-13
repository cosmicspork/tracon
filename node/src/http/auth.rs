//! Operator authentication.
//!
//! Local callers are the operator by definition — the node's state is their
//! state, and a shell on the machine already has it. "Local" means a direct
//! request from this machine addressed to this machine ([`is_local`]), not
//! merely a loopback peer: a reverse proxy on the same host connects from
//! loopback too. That is the whole story until a token is issued. Issuing one
//! (`tracon auth issue`) opens a second door: a client that presents the token
//! gets a cookie, and the cookie is what reaches the node from anywhere else.
//! Nothing else changes; a local request keeps working credential-less so the
//! CLI, `just dev`, and `kubectl port-forward` are untouched.
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
    http::{header, HeaderMap, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::hostname;
pub use super::passkey::AdminAction;
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
/// Passkey ceremonies opened or finished per window, per address and in
/// total. Each admin action off this machine costs two (the challenge and
/// its answer), so this is a few dozen confirmations in five minutes: far
/// more than an operator makes, and a bound on what a stolen cookie can make
/// the node verify and store.
const CEREMONY_PER_IP: u32 = 60;
const CEREMONY_GLOBAL: u32 = 120;

pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// A fixed window of attempts. Cleared wholesale when the window turns, so
/// it cannot grow without bound.
struct Limiter {
    window_start_ms: i64,
    global: u32,
    per_ip: HashMap<IpAddr, u32>,
    per_ip_cap: u32,
    global_cap: u32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(LOGIN_PER_IP, LOGIN_GLOBAL)
    }
}

impl Limiter {
    fn new(per_ip_cap: u32, global_cap: u32) -> Self {
        Self {
            window_start_ms: 0,
            global: 0,
            per_ip: HashMap::new(),
            per_ip_cap,
            global_cap,
        }
    }

    /// Whether another attempt is allowed, counting it if so.
    fn allow(&mut self, ip: Option<IpAddr>, now: i64) -> bool {
        if now - self.window_start_ms >= LOGIN_WINDOW_MS {
            self.window_start_ms = now;
            self.global = 0;
            self.per_ip.clear();
        }
        if self.global >= self.global_cap {
            return false;
        }
        if let Some(ip) = ip {
            let n = self.per_ip.entry(ip).or_insert(0);
            if *n >= self.per_ip_cap {
                return false;
            }
            *n += 1;
        }
        self.global += 1;
        true
    }
}

/// What the guard needs on every request: whether a token is configured
/// (cached so the common path does not touch the database), and the login
/// limiter.
pub struct AuthState {
    token_hash: Mutex<Option<String>>,
    limiter: Mutex<Limiter>,
    ceremonies: Mutex<Limiter>,
}

impl AuthState {
    /// `_bind` is the address the operator listener is bound to. It is no
    /// longer consulted: whether a caller is local is a property of the
    /// request ([`is_local`]), which accepts only loopback names in `Host`.
    /// A `Host` naming the bind address is refused even where it is honest,
    /// as with `curl http://0.0.0.0:7420` on a node bound to `0.0.0.0`
    /// (which connects from `127.0.0.1`): failing closed costs that caller
    /// nothing but typing `localhost`, while a specific non-loopback bind
    /// address arrives with a loopback peer only through a relay. The
    /// parameter stays so the many callers that build an `AppState` do not
    /// all change at once.
    pub fn new(_bind: String, token_hash: Option<String>) -> Self {
        Self {
            token_hash: Mutex::new(token_hash),
            limiter: Mutex::new(Limiter::default()),
            ceremonies: Mutex::new(Limiter::new(CEREMONY_PER_IP, CEREMONY_GLOBAL)),
        }
    }

    pub fn load(store: &Store, bind: String) -> Self {
        Self::new(bind, store.kv_get(OPERATOR_TOKEN_KEY).ok().flatten())
    }

    fn configured(&self) -> Option<String> {
        self.token_hash.lock().unwrap().clone()
    }

    /// Whether a token exists that can explicitly step a caller up to
    /// administrator access. This does not disclose the token or its hash.
    pub fn token_configured(&self) -> bool {
        self.configured().is_some()
    }

    fn set_configured(&self, hash: Option<String>) {
        *self.token_hash.lock().unwrap() = hash;
    }

    /// Whether another passkey ceremony may be opened or finished now,
    /// counting it if so.
    pub(crate) fn allow_ceremony(&self, ip: Option<IpAddr>, now: i64) -> bool {
        self.ceremonies.lock().unwrap().allow(ip, now)
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
/// login, the non-sensitive administrator access-status probe, and the SPA
/// shell/assets. The shell is open-source code and the status probe contains
/// no secret; serving them is what lets a remote login screen render. Every
/// other `/api` path stays gated.
fn is_public(req: &Request) -> bool {
    let path = req.uri().path();
    if path == "/api/login" {
        return true;
    }
    if req.method() == axum::http::Method::GET && path == "/api/admin/access" {
        return true;
    }
    req.method() == axum::http::Method::GET && !path.starts_with("/api/")
}

/// Headers a proxy adds to say whom it is forwarding for. A client on this
/// machine talking to the node directly sends none of them, so any one of
/// them means the loopback peer is a relay for someone else: `tailscale
/// serve`, `cloudflared`, Caddy, nginx, Envoy or Istio, a CDN, a pod's
/// sidecar, a forward proxy the CLI was pointed at. Every one makes the
/// request remote; there is no list of trusted proxies, because a proxy the
/// node trusted would make every caller behind it the operator.
const FORWARDING_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-forwarded-server",
    "x-real-ip",
    "x-client-ip",
    "true-client-ip",
    "x-forwarded-scheme",
    "x-forwarded-ssl",
    "x-forwarded-client-cert",
    "x-original-forwarded-for",
    "x-envoy-external-address",
    "x-cluster-client-ip",
    "cf-connecting-ip",
    "fastly-client-ip",
    "tailscale-user-login",
    "via",
];

/// Whether a host is a loopback name: `localhost`, an address in
/// `127.0.0.0/8`, or `::1`. Exact, not a suffix: `localhost.example` is a DNS
/// name anyone can point at `127.0.0.1`, and a page served from it still
/// sends its own name.
fn loopback_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// Whether a `Host` header (or an HTTP/2 `:authority`) names this machine's
/// loopback. Anything that is not a bare `host[:port]` is not.
fn host_is_loopback(value: &str) -> bool {
    let Ok(url) = url::Url::parse(&format!("http://{value}")) else {
        return false;
    };
    url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.host().is_some_and(|host| loopback_host(&host))
}

/// An `Origin` that is an HTTP(S) origin on this machine's loopback, parsed.
/// `null`, and any other scheme, is not one.
fn loopback_origin(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.host().is_some_and(|host| loopback_host(&host)))
    .then_some(url)
}

/// Whether `origin` is the origin the request was addressed to: the same host
/// and the same port as `authority` (a `Host` value), reading a port-less
/// `Host` with the origin's own scheme's default.
fn origin_matches(origin: &url::Url, authority: &str) -> bool {
    let Ok(addressed) = url::Url::parse(&format!("{}://{authority}", origin.scheme())) else {
        return false;
    };
    addressed.host() == origin.host()
        && addressed.port_or_known_default() == origin.port_or_known_default()
}

/// The single answer to "is this the operator at the node's own machine":
/// the one test behind the guard's credential-less branch, [`Loopback`], and
/// every `local`/`loopback` flag the interface is told.
///
/// All of these must hold:
/// - the TCP peer is a loopback address (a request with no peer address is
///   remote: this fails closed);
/// - no proxy forwarding header is present ([`FORWARDING_HEADERS`]);
/// - the `Host`, and the request target's authority when it has one, name
///   loopback (`localhost`, `127.0.0.0/8`, `[::1]`), which is also the
///   DNS-rebinding defence: a page on `evil.example` that resolves to
///   `127.0.0.1` still sends `Host: evil.example`;
/// - an `Origin`, when the caller sends one, is an HTTP(S) loopback origin
///   with the same host and port the request was addressed to (its `Host`).
///   A page on another loopback origin, such as the OpenCode UI listener or
///   any dev server on another port, is another site and is not the operator.
///
/// The `Host` port is not compared with the port the node is bound to.
/// `kubectl port-forward 8080:7420` and the Vite dev server (`just dev`,
/// which proxies `/api` with the browser's own `Host` and `Origin`) both
/// reach the node with a loopback `Host` on another port, and both are the
/// operator at the machine. A proxy that rewrites `Host` to its upstream
/// would present the bound port anyway, so checking it would stop nothing.
/// What is compared is the `Origin` against that `Host`: the page must be the
/// one the request was addressed to.
///
/// A missing `Host` is allowed: browsers always send one, so it is not a
/// rebinding vector, and a bare local client may omit it. A duplicated
/// `Host` or `Origin` is not something a direct client sends, and is remote.
///
/// What this cannot see is a same-host relay that strips forwarding headers
/// and rewrites `Host` to its upstream: by construction that request is
/// indistinguishable from one made on the machine. Proxies put in front of a
/// node must set `X-Forwarded-For` (tailscale serve, Caddy, cloudflared and
/// Traefik do by default; nginx needs `proxy_set_header X-Forwarded-For`).
pub(crate) fn is_local(peer: Option<IpAddr>, uri: &Uri, headers: &HeaderMap) -> bool {
    if !peer.is_some_and(|ip| ip.to_canonical().is_loopback()) {
        return false;
    }
    if FORWARDING_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return false;
    }
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = match (hosts.next(), hosts.next()) {
        (None, _) => None,
        (Some(host), None) => match host.to_str() {
            Ok(host) if host_is_loopback(host) => Some(host),
            _ => return false,
        },
        _ => return false,
    };
    let authority = uri.authority().map(|authority| authority.as_str());
    if authority.is_some_and(|authority| !host_is_loopback(authority)) {
        return false;
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    match (origins.next(), origins.next()) {
        (None, _) => true,
        (Some(origin), None) => {
            let Some(origin) = origin.to_str().ok().and_then(loopback_origin) else {
                return false;
            };
            // Another loopback origin is another site: the OpenCode UI
            // listener, a dev server on some other port. Its page can send a
            // simple cross-origin POST with no preflight, so a loopback
            // Origin alone would let it drive the node credential-free.
            host.or(authority)
                .is_some_and(|addressed| origin_matches(&origin, addressed))
        }
        _ => false,
    }
}

/// [`is_local`] of a whole request, as the guard sees it.
fn request_is_local(req: &Request) -> bool {
    is_local(peer_ip(req), req.uri(), req.headers())
}

/// [`is_local`] of the parts an extractor or a handler sees.
pub(crate) fn parts_are_local(parts: &axum::http::request::Parts) -> bool {
    is_local(parts_peer(parts), &parts.uri, &parts.headers)
}

/// The TCP peer of a request, as an extractor sees it.
pub(crate) fn parts_peer(parts: &axum::http::request::Parts) -> Option<IpAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
}

/// Whether the request is [local](is_local), for a handler that also takes a
/// body. Never rejects.
pub struct Local(pub bool);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Local {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Local(parts_are_local(parts)))
    }
}

/// A handler that runs only for a caller on the node's own machine.
///
/// The operator token is one credential for one operator, so it is not the
/// thing to hang this on: a phone holding it is the operator too. What
/// separates these routes is that they rewrite what the node *is* — the
/// binaries it executes, the file it reads its configuration from, the hub it
/// trusts — where a stolen cookie would otherwise be remote code execution.
/// Standing a node up is done at the node, and "at the node" is [`is_local`],
/// so a cookie relayed through a proxy on the node's own host does not count.
pub struct Loopback;

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Loopback {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        if parts_are_local(parts) {
            return Ok(Loopback);
        }
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "this is done at the node itself: open the interface on the node's own machine",
        ))
    }
}

/// An explicitly authenticated administrator. Unlike the ordinary operator
/// middleware, this extractor never treats a loopback connection as a
/// credential: callers present either a live operator-session cookie or the
/// configured bearer token.
pub struct Administrator;

/// Check the credential portions shared by the extractor and the deliberately
/// non-sensitive access-status endpoint. A live cookie stops being valid when
/// the configured operator token is removed, so deleting the token closes both
/// doors immediately.
pub(crate) fn administrator_authenticated(
    auth: &AuthState,
    store: &Store,
    headers: &HeaderMap,
) -> bool {
    let Some(configured) = auth.configured() else {
        return false;
    };
    if let Some(secret) = cookie_value(headers, COOKIE) {
        return matches!(
            store.auth_session_live(&hash(secret), now_ms()),
            Ok(Some(_))
        );
    }
    bearer(headers).is_some_and(|token| hash(token) == configured)
}

impl axum::extract::FromRequestParts<super::api::AppState> for Administrator {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &super::api::AppState,
    ) -> Result<Self, Self::Rejection> {
        if administrator_authenticated(&state.auth, state.store(), &parts.headers) {
            return Ok(Administrator);
        }
        Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            if state.auth.token_configured() {
                "administrator authentication required; sign in with the operator token"
            } else {
                "administrator access needs an operator token configured on this node"
            },
        ))
    }
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
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let local = request_is_local(&req);

    // `Origin: null` is what a browser sends from an opaque origin: a
    // sandboxed iframe, a `file://` page, a cross-origin redirect. It is not
    // the absence of an origin — it is one that can never equal this node's,
    // so letting it fall through as `None` would exempt exactly the contexts
    // that most need the check from the same-origin test below. No operator
    // client is opaque: the interface is served from this node's own origin,
    // and the one sandboxed surface the node does host, the HTML preview
    // viewer, is a separate listener this guard never sees, is reached with a
    // capability token rather than the cookie, and is served under a CSP with
    // `connect-src 'none'`/`form-action 'none'` so it can originate no
    // request at all. There is therefore nothing to scope an exception to.
    if origin.is_some_and(|o| o.trim().eq_ignore_ascii_case("null")) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "opaque-origin request refused",
        ));
    }

    // 1. On this machine, directly, with a local Host and Origin: the
    //    operator.
    if local {
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
        set_cookie(&mut response, &secret, !local, SESSION_TTL_MS / 1000);
    }
    Ok(response)
}

/// `Secure` unless the caller and requested origin are local, matching
/// `http::ui`'s cookie
/// (`UiState::secure`, `!cfg.ui.opencode_is_loopback()`): tracon serves plain
/// HTTP on loopback on purpose, and a WKWebView-backed client (the macOS
/// desktop wrapper) drops a `Secure` cookie set over that connection rather
/// than accepting it as Chromium/Safari's own "potentially trustworthy
/// origin" exception would. A local caller never needed the cookie for
/// authentication anyway (`guard`'s step 1) — this only fixes the client's
/// own `authenticated` readback.
fn set_cookie(response: &mut Response, secret: &str, secure: bool, max_age_secs: i64) {
    // SameSite=Lax rather than Strict: opening a review from a push
    // notification is a top-level cross-site navigation, and Strict would drop
    // the cookie on exactly that. Lax still withholds it from cross-site
    // writes, which every mutating call here is.
    let scope = if secure {
        "; Secure; SameSite=Lax"
    } else {
        "; SameSite=Lax"
    };
    let v = format!("{COOKIE}={secret}; HttpOnly{scope}; Path=/; Max-Age={max_age_secs}");
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
    Local(local): Local,
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
            // Spending the token is the pairing: this client may enroll the
            // node's first passkey for a short while (`http::passkey`).
            paired_ms: Some(now),
        })
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut response = Json(json!({ "ok": true })).into_response();
    set_cookie(&mut response, &secret, !local, SESSION_TTL_MS / 1000);
    Ok(response)
}

/// Drop this client's cookie. Other devices keep theirs.
pub async fn logout(
    State(state): State<AppState>,
    Local(local): Local,
    headers: HeaderMap,
) -> Response {
    if let Some(secret) = cookie_value(&headers, COOKIE) {
        let _ = state.store().auth_session_delete(&hash(secret));
        // Its devices go with it; the JOIN would have silenced them anyway,
        // but a listed device that can never be reached is a lie.
        let _ = state.store().auth_sessions_purge(now_ms());
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    set_cookie(&mut response, "", !local, 0);
    response
}

#[derive(Deserialize)]
pub struct TokenBody {
    /// SHA-256 of the operator token, hex. The CLI hashes it so the token
    /// itself never crosses the wire, not even over loopback.
    pub token_hash: String,
}

/// Set (or replace) the operator token. Every existing cookie dies with it;
/// enrolled passkeys stay. An admin action: off this machine it needs a
/// passkey to confirm it ([`AdminAction`]).
pub async fn put_token(
    State(state): State<AppState>,
    AdminAction(Json(body)): AdminAction<Json<TokenBody>>,
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

/// Remove the operator token: the node answers loopback only again. An admin
/// action, like setting one.
pub async fn delete_token(
    State(state): State<AppState>,
    AdminAction(()): AdminAction,
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

    fn request(method: axum::http::Method, path: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[test]
    fn only_the_non_sensitive_admin_status_probe_is_public() {
        assert!(is_public(&request(
            axum::http::Method::GET,
            "/api/admin/access",
        )));
        assert!(!is_public(&request(
            axum::http::Method::POST,
            "/api/admin/access",
        )));
        assert!(!is_public(&request(
            axum::http::Method::GET,
            "/api/admin/mesh",
        )));
    }

    #[test]
    fn administrator_requires_an_explicit_bearer_credential() {
        let store = Store::open_in_memory().unwrap();
        let auth = AuthState::new("127.0.0.1".into(), Some(hash("operator-token")));
        // There is deliberately no loopback input to this check: a local
        // address cannot become an administrator credential by accident.
        assert!(!administrator_authenticated(
            &auth,
            &store,
            &HeaderMap::new()
        ));
        let headers = headers_with(header::AUTHORIZATION, "Bearer operator-token");
        assert!(administrator_authenticated(&auth, &store, &headers));
        let wrong = headers_with(header::AUTHORIZATION, "Bearer another-token");
        assert!(!administrator_authenticated(&auth, &store, &wrong));
    }

    fn local(peer: &str, uri: &str, headers: &[(&str, &str)]) -> bool {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        is_local(
            Some(peer.parse().unwrap()),
            &uri.parse::<Uri>().unwrap(),
            &map,
        )
    }

    #[test]
    fn a_direct_loopback_request_is_local() {
        // The CLI: reqwest sends a Host and no Origin.
        assert!(local(
            "127.0.0.1",
            "/api/node",
            &[("host", "127.0.0.1:7420")]
        ));
        // The desktop wrapper's webview, at the node's own origin.
        assert!(local(
            "127.0.0.1",
            "/api/config",
            &[
                ("host", "127.0.0.1:7420"),
                ("origin", "http://127.0.0.1:7420")
            ]
        ));
        assert!(local(
            "::1",
            "/api/node",
            &[("host", "[::1]:7420"), ("origin", "http://[::1]:7420")]
        ));
        assert!(local(
            "127.0.0.1",
            "/api/node",
            &[
                ("host", "LOCALHOST:7420"),
                ("origin", "http://localhost:7420")
            ]
        ));
        // All of 127.0.0.0/8 is this machine.
        assert!(local(
            "127.0.0.2",
            "/api/node",
            &[("host", "127.0.0.2:7420")]
        ));
        // A dual-stack listener sees IPv4 loopback as mapped IPv6.
        assert!(local(
            "::ffff:127.0.0.1",
            "/api/node",
            &[("host", "127.0.0.1:7420")]
        ));
        // `kubectl port-forward 8080:7420`, and `just dev`'s Vite proxy, which
        // passes the browser's own Host and Origin through.
        assert!(local(
            "127.0.0.1",
            "/api/node",
            &[("host", "localhost:8080")]
        ));
        assert!(local(
            "127.0.0.1",
            "/api/config",
            &[
                ("host", "localhost:5173"),
                ("origin", "http://localhost:5173")
            ]
        ));
        // No Host at all is not a browser.
        assert!(local("127.0.0.1", "/api/node", &[]));
        // HTTP/2 carries the authority in the target rather than in Host.
        assert!(local("127.0.0.1", "http://127.0.0.1:7420/api/node", &[]));
    }

    /// Another loopback origin is another site. The OpenCode UI listener
    /// (`127.0.0.1:7423`) must never drive the node, and a dev server on some
    /// other port is somebody else's page: either can send a simple
    /// cross-origin POST with no preflight, so neither may be credential-free.
    #[test]
    fn a_page_on_another_loopback_origin_is_not_local() {
        for origin in [
            "http://127.0.0.1:7423",
            "http://127.0.0.1:7422",
            "http://localhost:3000",
            "http://localhost:7420",
            "http://[::1]:7420",
        ] {
            assert!(
                !local(
                    "127.0.0.1",
                    "/api/sessions/s1/kill",
                    &[("host", "127.0.0.1:7420"), ("origin", origin)]
                ),
                "{origin}"
            );
        }
        // An Origin with nothing to compare it with is not trusted either.
        assert!(!local(
            "127.0.0.1",
            "/api/node",
            &[("origin", "http://127.0.0.1:7420")]
        ));
        // The same origin, spelled with or without its default port.
        assert!(local(
            "127.0.0.1",
            "/api/node",
            &[("host", "localhost"), ("origin", "http://localhost:80")]
        ));
        assert!(local(
            "127.0.0.1",
            "/api/node",
            &[("host", "localhost:80"), ("origin", "http://localhost")]
        ));
        // HTTP/2: the authority is what the page was addressed to.
        assert!(local(
            "127.0.0.1",
            "http://127.0.0.1:7420/api/node",
            &[("origin", "http://127.0.0.1:7420")]
        ));
        assert!(!local(
            "127.0.0.1",
            "http://127.0.0.1:7420/api/node",
            &[("origin", "http://127.0.0.1:7423")]
        ));
    }

    #[test]
    fn a_remote_peer_is_never_local() {
        assert!(!local(
            "203.0.113.7",
            "/api/node",
            &[("host", "127.0.0.1:7420")]
        ));
        assert!(!local(
            "10.0.0.5",
            "/api/node",
            &[("host", "localhost:7420")]
        ));
        let no_peer = is_local(None, &"/api/node".parse().unwrap(), &HeaderMap::new());
        assert!(!no_peer, "a request with no peer address fails closed");
    }

    /// `tailscale serve`, `cloudflared` or a pod's sidecar connect from
    /// loopback. Each of these is what one of them sends for a caller
    /// somewhere else, and none of them is the operator at the machine.
    #[test]
    fn a_same_host_proxy_is_not_local() {
        for name in FORWARDING_HEADERS {
            assert!(
                !local(
                    "127.0.0.1",
                    "/api/config",
                    &[("host", "127.0.0.1:7420"), (name, "100.101.102.103")]
                ),
                "{name}"
            );
        }
        // tailscale serve: the tailnet name in Host, the caller in
        // X-Forwarded-For.
        assert!(!local(
            "127.0.0.1",
            "/api/config",
            &[
                ("host", "laptop.tail1234.ts.net"),
                ("x-forwarded-for", "100.101.102.103"),
                ("x-forwarded-host", "laptop.tail1234.ts.net"),
                ("x-forwarded-proto", "https"),
            ]
        ));
        // A proxy that keeps the client's Host and adds nothing.
        assert!(!local(
            "127.0.0.1",
            "/api/config",
            &[("host", "tracon-node.example.org")]
        ));
        // One that rewrites Host but carries the browser's Origin through.
        assert!(!local(
            "127.0.0.1",
            "/api/config",
            &[
                ("host", "127.0.0.1:7420"),
                ("origin", "https://tracon-node.example.org")
            ]
        ));
        // An absolute-form target naming somewhere else.
        assert!(!local(
            "127.0.0.1",
            "http://tracon-node.example.org/api/config",
            &[("host", "127.0.0.1:7420")]
        ));
    }

    #[test]
    fn a_host_or_origin_that_only_looks_local_is_not() {
        for host in [
            "evil.example",
            "evil.example:7420",
            "localhost.evil.example:7420",
            "127.0.0.1.evil.example",
            "127.0.0.1@evil.example",
            "127.0.0.1:7420/path",
            // The bind address of a node listening widely is not loopback.
            "10.0.0.5:7420",
            "0.0.0.0:7420",
        ] {
            assert!(
                !local("127.0.0.1", "/api/node", &[("host", host)]),
                "{host}"
            );
        }
        for origin in [
            "null",
            "http://evil.example",
            "http://localhost.evil.example:7420",
            "tauri://localhost",
            "file:///index.html",
            "http://10.0.0.5:7420",
        ] {
            assert!(
                !local(
                    "127.0.0.1",
                    "/api/node",
                    &[("host", "127.0.0.1:7420"), ("origin", origin)]
                ),
                "{origin}"
            );
        }
        // A duplicated Host or Origin is not what a direct client sends.
        assert!(!local(
            "127.0.0.1",
            "/api/node",
            &[("host", "127.0.0.1:7420"), ("host", "evil.example")]
        ));
        assert!(!local(
            "127.0.0.1",
            "/api/node",
            &[
                ("host", "127.0.0.1:7420"),
                ("origin", "http://127.0.0.1:7420"),
                ("origin", "https://evil.example")
            ]
        ));
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
