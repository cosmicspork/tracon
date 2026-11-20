//! Plugin UIs (plan Decision 5): each plugin whose manifest has `[ui]` is
//! served on a loopback listener of its own, so its pages have an origin of
//! their own and may talk to it (`connect-src 'self'`) without reaching the
//! operator's origin or another plugin's.
//!
//! The node serves the files under the build's `ui/` itself, from the
//! installed archive, and forwards whatever the pages ask of `api/...` to
//! the plugin's instance for the channel, as the operator's request with a
//! signed caller context ([`crate::mcp::Tools::ui_for_operator`]).
//!
//! Like the document preview listener (`super::preview`), nothing is served
//! without a capability token in the path, `/t/<token>/...`, minted by the
//! operator API (`POST /api/plugins/{name}/ui`) for one plugin and channel,
//! held in memory only, and forgotten after an hour unused. A listener
//! answers only its own plugin's tokens, only to a loopback `Host` naming
//! its port (no DNS rebinding), and takes a request that changes something
//! only from a page of its own origin.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use base64::Engine;
use parking_lot::Mutex;
use rand::RngCore;

use super::api::{ApiError, AppState};
use crate::{
    config::Config,
    corpus::html::normalize_path,
    mcp::{CallContext, OperatorCallError, UiRequest},
    plugin::install,
    store::now_ms,
};

const IDLE_TTL: Duration = Duration::from_secs(60 * 60);

/// What a plugin's pages may do: load what their own origin serves, and
/// talk to it. No inline script, no plugins, no form posts elsewhere, and
/// no frames but a `blob:` the page made itself (hoja previews a PDF it
/// rendered that way).
pub const PLUGIN_UI_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; media-src 'self' data: blob:; worker-src 'self' blob:; frame-src blob:; object-src 'none'; base-uri 'self'; form-action 'self'";

/// The largest request a plugin's page may send its API.
pub const BODY_MAX: usize = 1024 * 1024;

/// The request headers passed on to the plugin. Never a cookie, an
/// `Origin` or a `Referer`: the caller context says who is asking.
const PASSED_HEADERS: &[&str] = &["content-type", "accept", "if-match", "if-none-match"];

/// What a token opens: one plugin's UI, for one channel's instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiBinding {
    pub plugin: String,
    pub channel: String,
}

#[derive(Debug)]
struct TokenEntry {
    binding: UiBinding,
    last_used: Instant,
}

struct Listening {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

/// The node's plugin UI tokens and listeners.
#[derive(Default)]
pub struct PluginUis {
    tokens: Mutex<HashMap<String, TokenEntry>>,
    listeners: Mutex<HashMap<String, Listening>>,
}

impl PluginUis {
    /// A fresh 256-bit token for `binding`, and when it lapses if unused.
    pub fn mint(&self, binding: UiBinding) -> (String, i64) {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut entries = self.tokens.lock();
        entries.retain(|_, entry| now.duration_since(entry.last_used) < IDLE_TTL);
        entries.insert(
            token.clone(),
            TokenEntry {
                binding,
                last_used: now,
            },
        );
        (token, now_ms() + IDLE_TTL.as_millis() as i64)
    }

    /// What `token` opens, while it has not lapsed; using it keeps it.
    pub fn lookup(&self, token: &str) -> Option<UiBinding> {
        let now = Instant::now();
        let mut entries = self.tokens.lock();
        let entry = entries.get_mut(token)?;
        if now.duration_since(entry.last_used) >= IDLE_TTL {
            entries.remove(token);
            return None;
        }
        entry.last_used = now;
        Some(entry.binding.clone())
    }

    /// The port `plugin`'s UI is being served on, if it is.
    pub fn port(&self, plugin: &str) -> Option<u16> {
        self.listeners
            .lock()
            .get(plugin)
            .filter(|l| !l.task.is_finished())
            .map(|l| l.port)
    }

    /// Serve `plugin`'s UI on `127.0.0.1:port` with the router `app` makes,
    /// unless it is served there already. A listener on another port, which
    /// the configured range no longer holds, is closed first.
    pub fn listen(
        &self,
        plugin: &str,
        port: u16,
        app: impl FnOnce() -> Router,
    ) -> std::io::Result<()> {
        let mut listeners = self.listeners.lock();
        if let Some(open) = listeners.get(plugin) {
            if open.port == port && !open.task.is_finished() {
                return Ok(());
            }
            open.task.abort();
        }
        listeners.remove(plugin);
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        let app = app();
        let name = plugin.to_string();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(plugin = %name, %error, "plugin UI listener stopped");
            }
        });
        tracing::info!(plugin, port, "plugin UI listener");
        listeners.insert(plugin.to_string(), Listening { port, task });
        Ok(())
    }

    /// Close `plugin`'s listener and forget its tokens: it was removed.
    pub fn stop(&self, plugin: &str) {
        if let Some(open) = self.listeners.lock().remove(plugin) {
            open.task.abort();
        }
        self.tokens.lock().retain(|_, e| e.binding.plugin != plugin);
    }
}

impl Drop for PluginUis {
    fn drop(&mut self) {
        for (_, open) in self.listeners.lock().drain() {
            open.task.abort();
        }
    }
}

#[derive(Clone)]
struct UiState {
    app: AppState,
    plugin: Arc<str>,
    port: u16,
}

/// The router for `plugin`'s listener on `port`.
pub fn router(app: AppState, plugin: &str, port: u16) -> Router {
    Router::new()
        .route("/t/{token}/", any(serve_entry))
        .route("/t/{token}/{*path}", any(serve))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(DefaultBodyLimit::max(BODY_MAX))
        .layer(middleware::from_fn(ui_headers))
        .with_state(UiState {
            app,
            plugin: plugin.into(),
            port,
        })
}

/// The `Host` a request names, when it is this listener on loopback.
fn own_host(headers: &HeaderMap, port: u16) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .contains(&host.to_ascii_lowercase())
    .then(|| host.to_string())
}

async fn serve_entry(
    state: State<UiState>,
    Path(token): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    serve(
        state,
        Path((token, String::new())),
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn serve(
    State(state): State<UiState>,
    Path((token, path)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(host) = own_host(&headers, state.port) else {
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    };
    let Some(binding) = state
        .app
        .manager
        .plugin_uis()
        .lookup(&token)
        .filter(|b| *b.plugin == *state.plugin)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if path == "api" || path.starts_with("api/") {
        return proxy(
            &state, &binding, &token, method, &uri, &headers, &host, body,
        )
        .await;
    }
    if !matches!(method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    static_file(&state, &path, method == Method::HEAD).await
}

/// A file of the active build's `ui/`, or its entry for the bare prefix.
async fn static_file(state: &UiState, path: &str, head: bool) -> Response {
    let build = state
        .app
        .store()
        .plugin_get(&state.plugin)
        .ok()
        .flatten()
        .filter(|r| r.state != crate::store::plugins::REMOVED)
        .and_then(|r| r.active);
    let Some((build, ui)) = build.and_then(|b| {
        let ui = b.manifest.ui.clone()?;
        Some((b, ui))
    }) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let relative = match path {
        "" => ui.entry.trim_start_matches("ui/").to_string(),
        other => match normalize_path(other) {
            Ok(p) => p,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let root = install::build_dir(
        &Config::state_dir(),
        &state.plugin,
        &build.version,
        &build.digest,
    )
    .join("ui");
    let file = root.join(&relative);
    // A plain file of the tree, and nothing a link leads out of it to.
    let inside = match (
        tokio::fs::canonicalize(&root).await,
        tokio::fs::canonicalize(&file).await,
    ) {
        (Ok(root), Ok(file)) => file.starts_with(root),
        _ => false,
    };
    let plain = tokio::fs::symlink_metadata(&file)
        .await
        .is_ok_and(|m| m.is_file());
    if !inside || !plain {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(bytes) = tokio::fs::read(&file).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let media = mime_guess::from_path(&relative).first_or_octet_stream();
    let media = match (media.type_(), media.subtype().as_str()) {
        (mime_guess::mime::TEXT, _) | (_, "javascript") => format!("{media}; charset=utf-8"),
        _ => media.to_string(),
    };
    let body = if head { Vec::new() } else { bytes };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, media)
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response())
}

/// Forward a request for `api/...` to the plugin as the operator's.
#[allow(clippy::too_many_arguments)]
async fn proxy(
    state: &UiState,
    binding: &UiBinding,
    token: &str,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    host: &str,
    body: Bytes,
) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if origin != Some(format!("http://{host}").as_str()) {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "a change to a plugin is taken only from its own pages",
            )
            .into_response();
        }
    }
    // The path as sent, still percent-encoded, after `/t/<token>`.
    let raw = uri.path();
    let Some(path) = raw.strip_prefix(&format!("/t/{token}")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path.to_string(),
    };
    let passed = PASSED_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            Some((name.to_string(), value.to_string()))
        })
        .collect();
    let ctx = CallContext::operator(
        format!("/plugins/{}/ui", binding.plugin),
        &binding.channel,
        &state.app.node_id,
    );
    let request = UiRequest {
        method,
        path,
        headers: passed,
        body,
    };
    match state
        .app
        .tools
        .ui_for_operator(&ctx, &binding.plugin, request)
        .await
    {
        Ok(answer) => {
            let mut response = Response::builder()
                .status(StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY));
            for (name, value) in &answer.headers {
                response = response.header(name.as_str(), value.as_str());
            }
            response
                .body(axum::body::Body::from(answer.body))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(error) => {
            use OperatorCallError as E;
            let status = match &error {
                E::NotInstalled { .. } => StatusCode::NOT_FOUND,
                E::Request(_) => StatusCode::UNPROCESSABLE_ENTITY,
                E::NotOffered(_) | E::Denied(_) | E::Asked(_) => StatusCode::FORBIDDEN,
                E::Unavailable | E::Unreachable(_) => StatusCode::SERVICE_UNAVAILABLE,
                E::Tool(_) => StatusCode::BAD_GATEWAY,
            };
            ApiError::new(status, error.to_string()).into_response()
        }
    }
}

async fn ui_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.remove(header::SET_COOKIE);
    headers.insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert(
        "cross-origin-resource-policy",
        "same-origin".parse().unwrap(),
    );
    headers.insert("content-security-policy", PLUGIN_UI_CSP.parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(plugin: &str) -> UiBinding {
        UiBinding {
            plugin: plugin.into(),
            channel: "personal".into(),
        }
    }

    #[test]
    fn tokens_are_256_bit_url_safe_capabilities_bound_to_a_plugin_and_channel() {
        let uis = PluginUis::default();
        let (token, expires_ms) = uis.mint(binding("hoja"));
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&token)
                .unwrap()
                .len(),
            32
        );
        assert!(expires_ms > now_ms());
        assert_eq!(uis.lookup(&token), Some(binding("hoja")));
        assert_eq!(uis.lookup("not-a-token"), None);
        let (other, _) = uis.mint(binding("workboard"));
        assert_ne!(token, other);
        // Stopping a plugin's UI forgets its tokens, and only its own.
        uis.stop("hoja");
        assert_eq!(uis.lookup(&token), None);
        assert_eq!(uis.lookup(&other), Some(binding("workboard")));
    }

    #[test]
    fn only_a_loopback_host_naming_the_port_is_this_listener() {
        let host = |h: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, h.parse().unwrap());
            own_host(&headers, 7440)
        };
        assert_eq!(host("127.0.0.1:7440").as_deref(), Some("127.0.0.1:7440"));
        assert!(host("localhost:7440").is_some());
        assert!(host("[::1]:7440").is_some());
        for other in [
            "127.0.0.1:7441",
            "evil.example:7440",
            "127.0.0.1",
            "localhost",
        ] {
            assert_eq!(host(other), None, "{other}");
        }
    }

    #[test]
    fn the_csp_lets_a_page_talk_to_its_own_origin_and_nothing_else() {
        let directives: HashMap<&str, &str> = PLUGIN_UI_CSP
            .split(';')
            .filter_map(|d| d.trim().split_once(' '))
            .collect();
        assert_eq!(directives["connect-src"], "'self'");
        assert_eq!(directives["script-src"], "'self'");
        assert_eq!(directives["default-src"], "'self'");
        assert_eq!(directives["frame-src"], "blob:");
        assert_eq!(directives["object-src"], "'none'");
        assert_eq!(directives["form-action"], "'self'");
        assert!(!PLUGIN_UI_CSP.contains("unsafe-eval"));
        assert!(!directives["script-src"].contains("unsafe-inline"));
    }
}
