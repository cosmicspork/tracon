//! OpenCode's native interface, served by tracon on an origin of its own.
//!
//! The harness ships a web UI inside its binary and serves it from a catch-all
//! that falls back to `https://app.opencode.ai` whenever the embedded bundle
//! fails to resolve — a fallback no flag disables, which proxies the method and
//! body, and which fires silently on a swallowed import error
//! (`docs/reference/opencode-v1.18.30/api-ui.md` §6, finding 3). Under it sits
//! a `Content-Security-Policy` with `connect-src *`. So tracon does not proxy
//! that route. It serves the pinned bundle itself, from here, and the harness's
//! own `/*` is never reached.
//!
//! ## Why an origin and not a path
//!
//! Three facts about the bundle, none of them negotiable without forking it:
//! its asset, font and manifest references are root-absolute, so it cannot be
//! served under a subpath (§8 #16); its `site.webmanifest` claims `scope: "/"`
//! and would collide with tracon's own PWA manifest on a shared origin (#17);
//! and its server base URL is `location.origin` (§6). The third is the one that
//! turns a constraint into a design: **whatever origin serves the page is the
//! origin its API calls go to.** So this listener serves the page *and* answers
//! the API calls, by routing them into the mediated gateway
//! (`gateway::opencode`) for the one session the caller's cookie names.
//!
//! ## What authorises a request here
//!
//! Not the operator cookie. It is never read on this origin — `authorise`
//! looks only at [`UI_COOKIE`] — so a browser that sends both (loopback ports
//! share a cookie jar; see `config::Ui::opencode_url`) is authorised by
//! neither more nor less than the UI grant it holds. The reverse holds too:
//! `http::auth::guard` reads only `tracon_session`, so this cookie is not a
//! credential on the operator origin.
//!
//! The grant arrives as a **single-use, 60-second bootstrap token** in a URL
//! *fragment*, which never reaches a server log, a `Referer`, or a proxy. An
//! inline script tracon splices into the served `index.html` strips the
//! fragment, spends the token at `POST /boot`, and only then loads the app's
//! own module — so by the time upstream code runs there is nothing in the URL
//! and a `HttpOnly` cookie in the jar. The app sees no `?auth_token=`, stores
//! no password, and sends no `Authorization` header (finding 4): the harness's
//! Basic credential is injected by the gateway on the node, exactly as it is
//! for the operator's own interface.
//!
//! ## What is refused
//!
//! * Any path that is neither a file in the pinned bundle, nor a route the
//!   app's own router declares, nor a root the gateway's matrix knows: **404**.
//!   There is no catch-all into the harness, which is the whole point.
//! * A mutation or a WebSocket upgrade whose `Origin` is not this origin, or
//!   is opaque, or is absent — the cookie alone is never enough, the same rule
//!   `auth::guard` applies on the operator origin.
//! * A `Host` that is not this listener's: the DNS-rebinding defence.
//! * Anything at all once the session has ended or the operator has logged
//!   out. Both are re-read from the database on every request.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    session::state::SessionState,
    store::{now_ms, Store, UiGrantRow, UI_GRANT_BOOT, UI_GRANT_COOKIE},
};

use super::{
    api::{ApiError, AppState},
    auth, hostname,
};

/// The cookie that authorises this origin, and nothing else. Deliberately not
/// the operator's name: two names is what makes "the operator cookie is not
/// valid here" a fact about the code rather than about the browser's scoping.
pub const UI_COOKIE: &str = "tracon_opencode_ui";

/// How long a bootstrap token is worth anything. It crosses one process
/// boundary — the operator's page opening a window — and is spent immediately.
pub const BOOT_TTL_MS: i64 = 60 * 1000;

/// How long the cookie the exchange returns lives. Shorter than the operator's
/// 90 days because it is bound to one session, and a session that outlives a
/// working day is one whose window will be reopened anyway.
pub const COOKIE_TTL_MS: i64 = 12 * 60 * 60 * 1000;

/// The tree digest of the vendored bundle, produced and checked by
/// `containers/opencode-ui/build.sh`. What is served is verified against this
/// at load; a tree that does not match is not served at all.
pub const PINNED_DIGEST: &str = include_str!("../../../containers/opencode-ui/DIGEST");

/// The version the digest is of, for the message the operator reads.
pub const PINNED_VERSION: &str = "1.18.30";

// ---------------------------------------------------------------------------
// The bundle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Asset {
    bytes: Bytes,
    content_type: &'static str,
}

/// The pinned bundle, in memory, with the one edit tracon makes to it.
#[derive(Debug)]
pub struct Bundle {
    files: BTreeMap<String, Asset>,
    /// `index.html` with the app's module script deferred behind the bootstrap
    /// (see [`splice_bootstrap`]). Served at `/` and at every app route.
    index: Bytes,
    digest: String,
    /// `sha256-…` for every inline `<script>` in what is actually served: the
    /// app's own theme preload, and tracon's bootstrap. Computed rather than
    /// configured, so an upstream bundle that gains or loses an inline script
    /// does not silently need `'unsafe-inline'`.
    script_hashes: Vec<String>,
}

#[derive(Debug)]
pub enum BundleError {
    Missing(PathBuf),
    Io(PathBuf, std::io::Error),
    /// The tree is not the pinned one. Naming both digests is the point: an
    /// operator who rebuilt it can see whether the build drifted or the
    /// directory is simply something else.
    Digest {
        want: String,
        got: String,
    },
    /// No `index.html`, or one whose module script tracon cannot find — which
    /// would mean serving a page that never boots.
    Shape(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(p) => write!(
                f,
                "no OpenCode UI bundle at {}; build one with \
                 `containers/opencode-ui/build.sh --src <opencode checkout at v{PINNED_VERSION}>`",
                p.display()
            ),
            Self::Io(p, e) => write!(f, "reading {}: {e}", p.display()),
            Self::Digest { want, got } => write!(
                f,
                "the bundle's tree digest is {got}, not the pinned {want}; \
                 rebuild it with containers/opencode-ui/build.sh"
            ),
            Self::Shape(why) => write!(f, "the bundle is not the shape tracon serves: {why}"),
        }
    }
}

impl std::error::Error for BundleError {}

impl Bundle {
    /// Read a vendored tree, verify it is the pinned one, and prepare what is
    /// served.
    pub fn load(dir: &FsPath) -> Result<Self, BundleError> {
        if !dir.join("index.html").is_file() {
            return Err(BundleError::Missing(dir.to_path_buf()));
        }
        let mut files = BTreeMap::new();
        collect(dir, dir, &mut files)?;

        // The same digest `build.sh` prints: for each file in path order, the
        // path, a NUL, the bytes, a NUL. Computed over what is in memory, so
        // it covers exactly what will be served rather than what was on disk
        // at some earlier moment.
        let mut hash = Sha256::new();
        for (path, asset) in &files {
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(&asset.bytes);
            hash.update([0]);
        }
        let digest = hex::encode(hash.finalize());
        let want = PINNED_DIGEST.trim();
        if digest != want {
            return Err(BundleError::Digest {
                want: want.to_string(),
                got: digest,
            });
        }

        let raw = files
            .get("index.html")
            .map(|a| a.bytes.clone())
            .ok_or_else(|| BundleError::Shape("no index.html".into()))?;
        let raw = std::str::from_utf8(&raw)
            .map_err(|_| BundleError::Shape("index.html is not UTF-8".into()))?
            .to_string();
        let index = splice_bootstrap(&raw)?;
        let script_hashes = inline_script_hashes(&index);
        // index.html is served from `index`, never from the map; dropping it
        // stops `/index.html` from serving the un-spliced page and booting the
        // app with no cookie.
        files.remove("index.html");

        Ok(Self {
            files,
            index: Bytes::from(index),
            digest,
            script_hashes,
        })
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn len(&self) -> usize {
        self.files.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    fn get(&self, path: &str) -> Option<&Asset> {
        self.files.get(path)
    }
}

fn collect(
    root: &FsPath,
    dir: &FsPath,
    out: &mut BTreeMap<String, Asset>,
) -> Result<(), BundleError> {
    let entries = std::fs::read_dir(dir).map_err(|e| BundleError::Io(dir.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| BundleError::Io(dir.to_path_buf(), e))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| BundleError::Io(path.clone(), e))?;
        if kind.is_dir() {
            collect(root, &path, out)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_err(|_| BundleError::Shape(format!("{} is outside the bundle", path.display())))?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path).map_err(|e| BundleError::Io(path.clone(), e))?;
        out.insert(
            rel.clone(),
            Asset {
                bytes: Bytes::from(bytes),
                content_type: content_type(&rel),
            },
        );
    }
    Ok(())
}

/// The media types the bundle actually contains. An explicit table rather than
/// a guess: `X-Content-Type-Options: nosniff` is set on every response, so a
/// type this got wrong is a file the browser refuses rather than one it
/// misreads — and an extension not on this list is served as an opaque
/// download, never as script or markup.
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// The bootstrap
// ---------------------------------------------------------------------------

/// The script tracon splices in, with `__APP__` replaced by the app's own
/// module entrypoint.
///
/// Everything it does is in service of one ordering guarantee: **the app's
/// code must not run until the fragment is gone and the cookie is set.** If it
/// ran first it would see a URL carrying a capability (which upstream would
/// then have to be trusted to strip) and make its first API call
/// unauthenticated (which would be refused, and the app treats a refusal as a
/// server it should ask the operator to configure).
///
/// It asks the node where to land rather than reading the route out of the
/// fragment: the app's session route is keyed by base64url of something only
/// the node knows (`packages/app/src/utils/session-route.ts`), and the two
/// layouts it can be in declare different routes. The one route both declare
/// is `/server/{key}/session/{id}`, so that is what `/boot` hands back.
const BOOTSTRAP: &str = r#";(function () {
  var app = "__APP__"
  function start() {
    var s = document.createElement("script")
    s.type = "module"
    s.crossOrigin = "anonymous"
    s.src = app
    document.head.appendChild(s)
  }
  var hash = location.hash || ""
  var q = hash.indexOf("?")
  var boot = null
  if (q >= 0) {
    var params = new URLSearchParams(hash.slice(q + 1))
    boot = params.get("boot")
  }
  if (!boot) {
    start()
    return
  }
  // Gone before anything can read it, and before any request this page makes
  // can carry it in a Referer.
  history.replaceState(null, "", location.pathname)
  // Whether this page is framed decides how the node must scope the cookie
  // it is about to set: inside the installed app's shell this origin is
  // third-party, and a `SameSite=Strict` cookie would never arrive. Comparing
  // the two window handles is allowed across origins; reading anything off
  // `top` is not, and nothing here does.
  var framed = false
  try {
    framed = window.top !== window.self
  } catch (e) {
    framed = true
  }
  fetch("/boot", {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ boot: boot, framed: framed }),
  })
    .then(function (r) {
      return r.ok ? r.json() : null
    })
    .then(function (body) {
      if (body && body.path) history.replaceState(null, "", body.path)
    })
    .catch(function () {})
    .then(start, start)
})()
"#;

/// Replace the app's module `<script>` with the bootstrap, which loads it.
///
/// Public because it is the unit of "the page this origin serves": the PWA
/// shell's browser test (`node/tests/opencode_pwa_shell.rs`) stands the origin
/// up without the 34 MiB pinned bundle, and what it is testing — the boot
/// exchange and the cookie that comes out of it — is this script rather than
/// upstream's JavaScript. Taking the real one rather than a copy is what stops
/// the test from passing against a bootstrap the node does not serve.
pub fn splice_bootstrap(html: &str) -> Result<String, BundleError> {
    let (open, close) = find_module_script(html)
        .ok_or_else(|| BundleError::Shape("no <script type=\"module\"> in index.html".into()))?;
    let tag = &html[open..close];
    let src = attribute(tag, "src")
        .ok_or_else(|| BundleError::Shape("the module script has no src".into()))?;
    if !src.starts_with('/') || src.contains('"') {
        return Err(BundleError::Shape(format!(
            "the module script's src is not a root-absolute path: {src}"
        )));
    }
    let script = BOOTSTRAP.replace("__APP__", &src);
    Ok(format!(
        "{}<script>{}</script>{}",
        &html[..open],
        script,
        &html[close..]
    ))
}

/// The byte range of `<script type="module" …></script>`, open tag through
/// closing tag.
fn find_module_script(html: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(rel) = html[from..].find("<script") {
        let open = from + rel;
        let gt = open + html[open..].find('>')?;
        let tag = &html[open..=gt];
        let end = html[gt..].find("</script>").map(|i| gt + i + 9)?;
        if attribute(tag, "type").as_deref() == Some("module") {
            return Some((open, end));
        }
        from = end;
    }
    None
}

/// A double-quoted attribute's value. The bundle is machine-generated by Vite
/// and quotes everything; anything else is refused by the caller rather than
/// parsed.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let at = tag.find(&needle)? + needle.len();
    let rest = &tag[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// `sha256-…` for every `<script>` with no `src`, which is what `script-src`
/// has to name for the page to run without `'unsafe-inline'`.
fn inline_script_hashes(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = html[from..].find("<script") {
        let open = from + rel;
        let Some(gt) = html[open..].find('>').map(|i| open + i) else {
            break;
        };
        let Some(end) = html[gt..].find("</script>").map(|i| gt + i) else {
            break;
        };
        let tag = &html[open..=gt];
        if attribute(tag, "src").is_none() {
            let body = &html[gt + 1..end];
            let digest = Sha256::digest(body.as_bytes());
            out.push(format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            ));
        }
        from = end + 9;
    }
    out
}

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct UiState {
    app: AppState,
    bundle: Option<Arc<Bundle>>,
    /// The origin this listener is reached at: the cookie's audience, the
    /// `Origin` every mutation must carry, and what the CSP is written for.
    origin: Arc<String>,
    /// The hostname of that origin, for the `Host` check.
    host: Arc<String>,
    /// `Secure` on the cookie unless the origin is loopback, where tracon
    /// serves plain HTTP on purpose and a `Secure` cookie would be dropped.
    secure: bool,
    headers: Arc<SecurityHeaders>,
}

#[derive(Debug)]
struct SecurityHeaders {
    csp: HeaderValue,
}

impl UiState {
    pub fn new(app: AppState, bundle: Option<Arc<Bundle>>, operator_origin: &str) -> Self {
        let cfg = app.cfg.clone();
        let origin = cfg.ui.opencode_origin().expect("validated UI origin");
        let host = cfg.ui.opencode_host().expect("validated UI origin");
        let secure = !cfg.ui.opencode_is_loopback();
        let hashes = bundle
            .as_ref()
            .map(|b| b.script_hashes.join(" "))
            .unwrap_or_default();
        Self {
            app,
            bundle,
            origin: Arc::new(origin),
            host: Arc::new(host),
            secure,
            headers: Arc::new(SecurityHeaders {
                csp: csp(&hashes, operator_origin),
            }),
        }
    }

    fn store(&self) -> &Arc<Store> {
        self.app.store()
    }
}

/// tracon's policy for this origin, replacing the `connect-src *` the harness
/// emits (finding 3, §8 #7).
///
/// Every directive that is wider than `'self'` is here because the pinned
/// bundle needs it, and each one is evidence from §6 rather than caution:
///
/// * `'wasm-unsafe-eval'` — the terminal dynamically imports `ghostty-web` and
///   instantiates its WebAssembly.
/// * the `sha256-` script hashes — Vite inlines the theme-preload script at
///   build time, and tracon inlines the bootstrap. Hashes, so the policy stays
///   free of `'unsafe-inline'` and a bundle whose inline scripts changed
///   fails visibly rather than silently gaining permission.
/// * `style-src 'unsafe-inline'` — themes are injected as a `<style>` element
///   built from `localStorage`, and Solid sets element styles inline. Measured
///   against the real bundle, not assumed.
/// * `img-src`/`media-src blob:` — attachment previews are `createObjectURL`.
/// * `worker-src blob:` — two same-origin module workers (markdown, diffs).
/// * `font-src data:` — §6 says the fonts are self-hosted under `/assets/`,
///   and they are, but a browser run against the built bundle also blocked a
///   base64 `data:font/woff2` the stylesheet inlines. Widened from what the
///   reading predicted because the measurement said so.
///
/// And every directive that is *narrower* than what the harness sends is the
/// point of serving this ourselves: `connect-src 'self'` is what stops the
/// app's release-notes fetch of `https://opencode.ai/changelog.json` and any
/// other upstream call, and `img-src` without `https:` stops the notification
/// icon it would otherwise load from the same host.
///
/// `frame-ancestors` is the operator origin alone: Gate D's mobile shell
/// embeds this view, and nothing else may.
fn csp(script_hashes: &str, operator_origin: &str) -> HeaderValue {
    let script = if script_hashes.is_empty() {
        "'self' 'wasm-unsafe-eval'".to_string()
    } else {
        format!("'self' 'wasm-unsafe-eval' {script_hashes}")
    };
    let value = format!(
        "default-src 'self'; script-src {script}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; media-src 'self' data: blob:; font-src 'self' data:; \
         worker-src 'self' blob:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
         form-action 'none'; frame-ancestors {operator_origin}"
    );
    HeaderValue::from_str(&value).unwrap_or_else(|_| {
        // An operator origin that cannot go in a header would have been
        // refused by `Ui::opencode_origin`; fail closed rather than serve an
        // unprotected page.
        HeaderValue::from_static(
            "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        )
    })
}

pub fn router(state: UiState) -> Router {
    Router::new()
        .route("/boot", post(boot))
        .fallback(dispatch)
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .with_state(state)
}

/// What every response on this origin carries, and what every request must
/// satisfy before a handler sees it.
///
/// The `Origin` rule is the one worth stating: a cookie is attached by the
/// browser to a request a *different* site made, so the cookie alone can never
/// distinguish the app from a page attacking it. Requiring `Origin` to equal
/// this origin on everything that changes state — and on every WebSocket
/// upgrade, which carries no preflight — is what does. `Origin: null` is an
/// opaque origin (a sandboxed frame, a `file://` page, a cross-origin
/// redirect); it is refused outright rather than treated as absent, the same
/// reading `auth::guard` makes.
async fn origin_guard(
    State(state): State<UiState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());

    // DNS rebinding: a page on `evil.example` resolving to this address still
    // sends `Host: evil.example`.
    if let Some(h) = host {
        let h = hostname(h);
        let ok = h == state.host.as_str() || matches!(h, "localhost" | "127.0.0.1" | "::1");
        if !ok {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "this listener does not answer to that Host",
            ));
        }
    }

    if origin.is_some_and(|o| o.trim().eq_ignore_ascii_case("null")) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "opaque-origin request refused",
        ));
    }

    let upgrading = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let mutating = !matches!(req.method(), &Method::GET | &Method::HEAD);
    if mutating || upgrading {
        let matches = origin.is_some_and(|o| same_origin(o, &state.origin));
        if !matches {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "this origin requires a matching Origin header on writes and upgrades",
            ));
        }
    }

    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, state.headers.csp.clone());
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // The app opens external links with `noopener`, but a cross-origin opener
    // of *this* page would otherwise keep a handle to it.
    h.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    Ok(response)
}

/// Compare an `Origin` value with the configured one. Scheme and host must
/// agree exactly; the port is compared as written, so `http://h` and
/// `http://h:80` are not conflated in either direction — the operator's
/// interface is what mints the URL, and it mints this exact string.
fn same_origin(sent: &str, want: &str) -> bool {
    sent.trim().eq_ignore_ascii_case(want)
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Everything but `/boot`.
///
/// The order is the argument: a file in the pinned bundle, then a route the
/// app's own router declares, then a call the gateway's matrix might mediate,
/// then nothing. There is no final `else` that forwards — a path nobody here
/// claims is a 404, because the one thing this origin must never become is a
/// tunnel to the harness's own catch-all (finding 3).
async fn dispatch(
    State(state): State<UiState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
    let bundle = state.bundle.clone();
    let read_only = matches!(method, Method::GET | Method::HEAD);

    // A node with no vendored bundle still answers the API: the two are
    // independent, and a session whose window is open when the tree is
    // replaced should not have its calls turn into a page.
    let page = || match &bundle {
        Some(b) => html(b.index.clone()),
        None => unavailable(),
    };

    if path == "/" {
        return if read_only {
            page()
        } else {
            StatusCode::METHOD_NOT_ALLOWED.into_response()
        };
    }

    let Some(segments) = segments(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if read_only {
        if let Some(asset) = bundle.as_ref().and_then(|b| b.get(&segments.join("/"))) {
            return file(asset, &segments);
        }
    }

    // A path the gateway's matrix could speak for. It decides; unknown routes
    // are refused there by name, not forwarded.
    if segments
        .first()
        .is_some_and(|s| crate::gateway::opencode::is_api_root(s))
    {
        let grant = match authorise(&state, &headers).await {
            Ok(grant) => grant,
            Err(refusal) => return refusal.into_response(),
        };
        return to_gateway(&state, &grant, method, &uri, headers, body).await;
    }

    // A route the app owns. Serving the shell here is what makes a reload or a
    // bookmarked session work; it is tracon's own page, not a proxy.
    if read_only && app_route(&segments) {
        return page();
    }

    StatusCode::NOT_FOUND.into_response()
}

/// Split a request path into non-empty, unambiguous segments. `None` for a
/// path with an empty, `.` or `..` segment — those have more than one reading,
/// and a path with more than one reading is not served.
fn segments(path: &str) -> Option<Vec<String>> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let parts: Vec<String> = trimmed.split('/').map(|s| s.to_string()).collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || p == "." || p == ".." || p.contains('\0'))
    {
        return None;
    }
    Some(parts)
}

/// Whether a path is one of the routes `packages/app/src/app.tsx` declares.
///
/// A transcription of that file's `<Route>` table, not a wildcard: `/`,
/// `/new-session`, `/server/{key}/session/{id}`, and the directory-scoped
/// `/{dir}`, `/{dir}/session`, `/{dir}/session/{id}`. Both `{dir}` and `{key}`
/// are base64url of something the app encoded
/// (`packages/app/src/utils/session-route.ts`), and a directory slug decodes
/// to an absolute path — which is what keeps `/anything` from matching.
fn app_route(segments: &[String]) -> bool {
    let s: Vec<&str> = segments.iter().map(String::as_str).collect();
    match s.as_slice() {
        [] => true,
        ["new-session"] => true,
        ["server", key, "session", _] => decode_slug(key).is_some(),
        [dir] | [dir, "session"] | [dir, "session", _] => {
            decode_slug(dir).is_some_and(|d| d.starts_with('/'))
        }
        _ => false,
    }
}

/// base64url without padding, as `base64Encode` in
/// `packages/core/src/util/encode.ts` produces it.
fn decode_slug(value: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// The app's route for one session, which `/boot` hands back so the page lands
/// on it without the fragment ever having said so.
///
/// `/server/{key}/session/{id}` rather than the directory-scoped
/// `/{dir}/session/{id}`: the app declares two route tables, one per layout
/// (`packages/app/src/app.tsx`, `Routes()`), chosen by a setting in the
/// viewer's own `localStorage`. The directory-scoped route is a *redirect* in
/// the newer one -- which is how the first browser run of this landed back on
/// the home page. `/server/{key}/session/{id}` is declared in both.
///
/// `key` is `ServerConnection.key` of the server the app is talking to, which
/// for an HTTP connection is its URL (`src/context/server.tsx:224-227`), and
/// that URL is `location.origin` (`entry.tsx:103`) -- this origin.
pub fn session_route(origin: &str, harness_session: &str) -> String {
    format!(
        "/server/{}/session/{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(origin),
        harness_session
    )
}

fn html(body: Bytes) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn file(asset: &Asset, segments: &[String]) -> Response {
    // Vite's asset names carry a content hash, so they are immutable; nothing
    // else is.
    let cache = if segments.first().is_some_and(|s| s == "assets") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, cache),
        ],
        asset.bytes.clone(),
    )
        .into_response()
}

/// What this origin says when no bundle is vendored. Not a 404 and not a
/// proxy: the listener exists, the operator has something to do, and the page
/// says what.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!(
            "<!doctype html><meta charset=utf-8><title>OpenCode UI not vendored</title>\
             <p>This node serves OpenCode's interface from a pinned bundle it does not have. \
             Build one with <code>containers/opencode-ui/build.sh --src &lt;opencode checkout \
             at v{PINNED_VERSION}&gt;</code>.\n"
        ),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The bootstrap exchange
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BootBody {
    boot: String,
    /// Whether the page spending this token is embedded in a frame — the
    /// installed PWA's shell (`spa/src/routes/OpencodeShell.svelte`) rather
    /// than the desktop window or a tab of its own. The bootstrap reports it
    /// because only the page can know, and the answer decides one thing: how
    /// this origin's cookie must be scoped to survive being third-party.
    /// Absent on an older bootstrap, which is the un-framed case.
    #[serde(default)]
    framed: bool,
}

/// Spend a bootstrap token for a cookie.
///
/// Reached only through `origin_guard`, so the `Origin` has already been
/// required to be this one — a cross-site page cannot spend a token it somehow
/// learned. Consumption is a conditional `UPDATE` in the store, so the second
/// attempt on a token is refused however it races the first.
async fn boot(
    State(state): State<UiState>,
    headers: HeaderMap,
    Json(body): Json<BootBody>,
) -> Result<Response, ApiError> {
    let now = now_ms();
    let store = state.store().clone();
    let _ = store.ui_grants_purge(now);

    // Bounded work before hashing; the token this mints is 43 characters.
    if body.boot.len() > 512 {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "not a boot token"));
    }
    let grant = store
        .ui_grant_consume_boot(&auth::hash(&body.boot), &state.origin, now)
        .map_err(internal)?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "this bootstrap token is spent, expired, or was not minted for this origin",
            )
        })?;

    // Between minting and spending, the session may have ended and the
    // operator may have logged out. A token is a capability, not a promise.
    if let Err(why) = still_live(&store, &grant, now) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, why));
    }
    let Some(api) = state.app.manager.native_api(&grant.session_id).await else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this session is not running a harness with a native API",
        ));
    };

    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    store
        .ui_grant_insert(&UiGrantRow {
            token_hash: auth::hash(&secret),
            kind: UI_GRANT_COOKIE.into(),
            session_id: grant.session_id.clone(),
            audience: grant.audience.clone(),
            operator: grant.operator.clone(),
            created_ms: now,
            expires_ms: now + COOKIE_TTL_MS,
            used_ms: None,
        })
        .map_err(internal)?;

    let mut response = Json(json!({
        "ok": true,
        "session": api.session_id,
        "path": session_route(&state.origin, &api.session_id),
    }))
    .into_response();
    set_cookie(
        &mut response,
        &secret,
        state.secure,
        COOKIE_TTL_MS / 1000,
        body.framed,
    );
    let _ = headers;
    Ok(response)
}

/// Host-only (no `Domain`) and `HttpOnly` always. The rest depends on where
/// the page spending the token is.
///
/// **In a window or a tab of its own** — the desktop app, a desktop browser —
/// the cookie is `SameSite=Strict`: it is never wanted on a request some other
/// site caused, and there is no cross-site entry path, because the operator's
/// own page opens the window and the bootstrap that follows is same-origin.
///
/// **In the installed app's shell** the same cookie would never be set at all.
/// The shell is a page on tracon's origin; the frame inside it is this origin;
/// so every request the frame makes — including its own `POST /boot` — is
/// *third-party* by the browser's reckoning, whatever it looks like from
/// inside the frame. `SameSite=Strict` is refused on the way in and would not
/// be sent on the way out.
///
/// So a framed boot gets `SameSite=None` with `Partitioned` (CHIPS). The pair
/// is the point:
///
/// * `SameSite=None` is what lets the cookie exist in a third-party context at
///   all. On its own it would be a loosening — the cookie would ride along on
///   any cross-site request to this origin.
/// * `Partitioned` takes that back, and more. The jar is keyed by the
///   *top-level* site, so the cookie this exchange sets exists only while
///   tracon's own origin is the page around it. A frame of this origin on
///   `evil.example` is a different partition and an empty jar — which is a
///   stronger statement than `SameSite=Strict` made, because it holds even
///   against a same-site attacker. It is also what makes the view work with
///   third-party cookies blocked, which is the default on the phones this is
///   for.
/// * And the loosening `SameSite=None` would otherwise be is already answered
///   by `origin_guard`: every mutation and every WebSocket upgrade on this
///   origin must carry a matching `Origin`, so a cookie riding along on a
///   cross-site request buys the caller nothing it can read or write.
///
/// `Partitioned` requires `Secure`, so a framed cookie carries it even on
/// loopback, where this listener is plain HTTP. That is not a contradiction:
/// `http://localhost` and `http://127.0.0.0/8` are *potentially trustworthy*
/// origins, and browsers accept `Secure` cookies from them. Asserted against
/// a real Chromium in `node/tests/opencode_pwa_shell.rs` rather than assumed.
fn set_cookie(
    response: &mut Response,
    secret: &str,
    secure: bool,
    max_age_secs: i64,
    framed: bool,
) {
    let scope = if framed {
        // Forced `Secure`: `Partitioned` is ignored without it, and an ignored
        // partition attribute is a cookie that is either blocked or shared.
        "; Secure; SameSite=None; Partitioned"
    } else if secure {
        "; Secure; SameSite=Strict"
    } else {
        "; SameSite=Strict"
    };
    let v = format!("{UI_COOKIE}={secret}; HttpOnly{scope}; Path=/; Max-Age={max_age_secs}");
    if let Ok(value) = HeaderValue::from_str(&v) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// The grant a request carries, if it still carries one.
///
/// Deliberately re-reads the session and the operator login rather than
/// trusting the row: revocation that depends on a delete is revocation that a
/// missed delete silently loses.
async fn authorise(state: &UiState, headers: &HeaderMap) -> Result<UiGrantRow, ApiError> {
    let refused = |message: &str| -> ApiError { ApiError::new(StatusCode::UNAUTHORIZED, message) };
    let Some(secret) = auth::cookie_value(headers, UI_COOKIE) else {
        return Err(refused(
            "this origin needs a session capability; open it from tracon",
        ));
    };
    let now = now_ms();
    let store = state.store();
    let grant = match store.ui_grant_cookie(&auth::hash(secret), &state.origin, now) {
        Ok(Some(grant)) => grant,
        Ok(None) => return Err(refused("this capability has expired or was revoked")),
        Err(e) => {
            tracing::warn!(error = %e, "UI grant lookup failed");
            return Err(refused("this capability could not be checked"));
        }
    };
    if let Err(why) = still_live(store, &grant, now) {
        // Dead for every browser holding one, not only for this request, so
        // the rows go rather than being refused one at a time forever. The
        // refusal above is what enforces it; this is housekeeping.
        let _ = store.ui_grants_forget_session(&grant.session_id);
        return Err(refused(why));
    }
    Ok(grant)
}

/// Whether the two things a grant hangs from are still there. Returning the
/// reason rather than a bool so the operator reads why their window stopped
/// working.
fn still_live(store: &Store, grant: &UiGrantRow, now: i64) -> Result<(), &'static str> {
    if !grant.operator.is_empty()
        && !matches!(store.auth_session_live(&grant.operator, now), Ok(Some(_)))
    {
        return Err("the operator login this was granted under has ended");
    }
    match store.get_session(&grant.session_id) {
        Ok(Some(row)) if !SessionState::from_stored(&row.state).is_terminal() => Ok(()),
        Ok(Some(_)) => Err("this session has ended"),
        _ => Err("this session is not on this node"),
    }
}

// ---------------------------------------------------------------------------
// Into the gateway
// ---------------------------------------------------------------------------

/// Hand a UI-origin API call to the mediated gateway as the operator's own
/// interface would make it.
///
/// A thin adapter on purpose: `gateway::opencode` owns what a call means — the
/// route matrix, the pinned directory, the injected credential, the intent row
/// — and this only supplies the one thing the UI origin knows that the mount
/// point would otherwise carry, which session the caller is for. The session
/// comes from the *cookie*, never from the path, so no request on this origin
/// can name another session's mount.
///
/// The `Cookie` header is dropped before the call. It is this origin's
/// authority and has no meaning upstream; the gateway injects Basic auth of
/// its own and the harness must never see a tracon capability.
async fn to_gateway(
    state: &UiState,
    grant: &UiGrantRow,
    method: Method,
    uri: &Uri,
    mut headers: HeaderMap,
    body: Bytes,
) -> Response {
    let tail = uri.path().trim_start_matches('/').to_string();
    let mut mounted = format!("/api/opencode/{}/{}", grant.session_id, tail);
    if let Some(q) = uri.query() {
        mounted.push('?');
        mounted.push_str(q);
    }
    let Ok(mounted) = mounted.parse::<Uri>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    headers.remove(header::COOKIE);
    crate::gateway::opencode::handle(
        State(state.app.clone()),
        Path((grant.session_id.clone(), tail)),
        method,
        mounted,
        headers,
        body,
    )
    .await
}

// ---------------------------------------------------------------------------
// Minting, from the operator's own interface
// ---------------------------------------------------------------------------

/// `POST /api/sessions/{id}/opencode-boot` on the **operator** router: "Open in
/// OpenCode".
///
/// Behind the operator guard like every other route there, so the caller is
/// already the operator. What it returns is a URL whose capability is in the
/// fragment — never the query — so the token appears in no access log, no
/// `Referer`, and nothing a reverse proxy records.
pub async fn open(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let origin = state
        .cfg
        .ui
        .opencode_origin()
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let store = state.store();
    let now = now_ms();
    let _ = store.ui_grants_purge(now);

    let Ok(Some(row)) = store.get_session(&session_id) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no session {session_id} on this node"),
        ));
    };
    if SessionState::from_stored(&row.state).is_terminal() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this session has ended",
        ));
    }
    let Some(api) = state.manager.native_api(&session_id).await else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this session is not running a harness with a native API",
        ));
    };

    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    store
        .ui_grant_insert(&UiGrantRow {
            token_hash: auth::hash(&token),
            kind: UI_GRANT_BOOT.into(),
            session_id: session_id.clone(),
            audience: origin.clone(),
            // Empty for a loopback operator, who holds no cookie. The grant is
            // then revoked by the session ending and by nothing else, which is
            // the same authority that operator already has at the machine.
            operator: auth::session_hash(&headers).unwrap_or_default(),
            created_ms: now,
            expires_ms: now + BOOT_TTL_MS,
            used_ms: None,
        })
        .map_err(internal)?;

    Ok(Json(json!({
        "url": format!("{origin}/#/session/{}?boot={token}", api.session_id),
        "origin": origin,
        "expires_ms": now + BOOT_TTL_MS,
        // What the capability becomes once spent. The shell cannot read an
        // `HttpOnly` cookie, so without this it could only guess when a frame
        // it backgrounded has certainly stopped working.
        "cookie_ttl_ms": COOKIE_TTL_MS,
    })))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// Bind the UI origin's listener. A node with no vendored bundle still binds
/// it: the origin's refusals are what the rest of the system is written
/// against, and a listener that appears only sometimes is worse than one that
/// says what is missing.
pub async fn serve(app: AppState, operator_listen: SocketAddr) -> anyhow::Result<()> {
    let cfg = app.cfg.clone();
    let listen = cfg.ui.opencode_listen;
    let dir = cfg.ui.opencode_bundle_path();
    let bundle = match Bundle::load(&dir) {
        Ok(b) => {
            tracing::info!(
                files = b.len(),
                digest = %b.digest(),
                version = PINNED_VERSION,
                "OpenCode UI bundle verified"
            );
            Some(Arc::new(b))
        }
        Err(e) => {
            tracing::warn!(error = %e, "serving no OpenCode UI");
            None
        }
    };
    let operator_origin = format!("http://{operator_listen}");
    let state = UiState::new(app, bundle, &operator_origin);
    let origin = state.origin.to_string();
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| anyhow::anyhow!("bind OpenCode UI listener {listen}: {e}"))?;
    tracing::info!(listen = %listen, %origin, "OpenCode UI listener");
    if let Err(error) = axum::serve(listener, router(state)).await {
        tracing::error!(%error, "OpenCode UI listener stopped");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_routes_are_the_ones_the_bundle_declares() {
        let seg = |p: &str| segments(p).unwrap();
        let dir = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("/work");
        assert!(app_route(&seg("/")));
        assert!(app_route(&seg("/new-session")));
        assert!(app_route(&seg(&format!("/{dir}"))));
        assert!(app_route(&seg(&format!("/{dir}/session"))));
        assert!(app_route(&seg(&format!("/{dir}/session/ses_1"))));
        assert!(app_route(&seg(&format!("/server/{dir}/session/ses_1"))));
        // A single segment that is not a directory slug is nobody's route.
        assert!(!app_route(&seg("/nope")));
        assert!(!app_route(&seg("/nope/nope/nope")));
        assert!(!app_route(&seg(&format!("/{dir}/nope"))));
        assert!(!app_route(&seg(&format!("/{dir}/session/ses_1/extra"))));
        // A slug that decodes to something that is not a path.
        let word = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("work");
        assert!(!app_route(&seg(&format!("/{word}"))));
    }

    #[test]
    fn ambiguous_paths_are_not_paths() {
        assert!(segments("/a//b").is_none());
        assert!(segments("/../etc").is_none());
        assert!(segments("/a/./b").is_none());
        assert_eq!(segments("/").unwrap(), Vec::<String>::new());
        assert_eq!(segments("/a/b").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn the_bootstrap_replaces_the_module_script_and_keeps_the_stylesheet() {
        let html = concat!(
            "<html><head><script id=\"t\">var a=1</script>",
            "<script type=\"module\" crossorigin src=\"/assets/index-abc.js\"></script>",
            "<link rel=\"stylesheet\" href=\"/assets/index-def.css\">",
            "</head><body></body></html>"
        );
        let out = splice_bootstrap(html).unwrap();
        // The app is no longer loaded by the markup...
        assert!(!out.contains("<script type=\"module\""));
        // ...but the bootstrap knows where it is, and the stylesheet is intact.
        assert!(out.contains("/assets/index-abc.js"));
        assert!(out.contains("/assets/index-def.css"));
        assert!(out.contains("POST"));
        assert!(out.contains("/boot"));
        assert!(out.contains("history.replaceState"));
        // Both inline scripts are named by hash, so no 'unsafe-inline'.
        let hashes = inline_script_hashes(&out);
        assert_eq!(hashes.len(), 2, "{hashes:?}");
        assert!(hashes.iter().all(|h| h.starts_with("'sha256-")));
    }

    #[test]
    fn a_bundle_without_a_module_script_is_refused() {
        assert!(matches!(
            splice_bootstrap("<html><head></head></html>"),
            Err(BundleError::Shape(_))
        ));
        assert!(matches!(
            splice_bootstrap("<script type=\"module\"></script>"),
            Err(BundleError::Shape(_))
        ));
    }

    #[test]
    fn the_policy_names_the_hashes_and_never_unsafe_inline_scripts() {
        let value = csp("'sha256-aaa' 'sha256-bbb'", "http://127.0.0.1:7420");
        let v = value.to_str().unwrap();
        assert!(v.contains("script-src 'self' 'wasm-unsafe-eval' 'sha256-aaa' 'sha256-bbb'"));
        assert!(!v.contains("script-src 'self' 'unsafe-inline'"));
        // The harness's own `connect-src *` is replaced, not widened.
        assert!(v.contains("connect-src 'self'"));
        assert!(!v.contains("connect-src *"));
        // No upstream host may be reached, and only tracon may frame this.
        assert!(!v.contains("opencode.ai"));
        assert!(v.contains("frame-ancestors http://127.0.0.1:7420"));
        assert!(v.contains("base-uri 'none'"));
        assert!(v.contains("form-action 'none'"));
    }

    /// What the shell's frame depends on, stated as an assertion rather than
    /// left to a browser run: a cookie set for a framed page must survive
    /// being third-party, and one set for a window must not become available
    /// to anyone else's page.
    #[test]
    fn a_framed_boot_is_partitioned_and_a_windowed_one_stays_strict() {
        let cookie = |secure: bool, framed: bool| {
            let mut response = StatusCode::OK.into_response();
            set_cookie(&mut response, "s3cret", secure, 42, framed);
            response
                .headers()
                .get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        // Framed: `SameSite=None` so it is set and sent at all inside the
        // shell's iframe, `Partitioned` so it exists only while tracon's own
        // origin is the page around it, and `Secure` because CHIPS requires it
        // -- on loopback too, where this listener is plain HTTP and the origin
        // is nonetheless potentially trustworthy.
        for secure in [true, false] {
            let framed = cookie(secure, true);
            assert!(framed.starts_with("tracon_opencode_ui=s3cret; "), "{framed}");
            assert!(framed.contains("; HttpOnly"), "{framed}");
            assert!(framed.contains("; Secure"), "{framed}");
            assert!(framed.contains("; SameSite=None"), "{framed}");
            assert!(framed.contains("; Partitioned"), "{framed}");
            assert!(!framed.contains("SameSite=Strict"), "{framed}");
            // Host-only: no `Domain`, so a sibling host never receives it.
            assert!(!framed.contains("Domain="), "{framed}");
            assert!(framed.contains("; Path=/; Max-Age=42"), "{framed}");
        }

        // A window of its own has no third-party leg, so it keeps the
        // narrowest rule there is and is never partitioned into a jar the
        // desktop app would have to re-mint against.
        let windowed = cookie(true, false);
        assert!(windowed.contains("; SameSite=Strict"), "{windowed}");
        assert!(windowed.contains("; Secure"), "{windowed}");
        assert!(!windowed.contains("Partitioned"), "{windowed}");
        // And on loopback, where a `Secure` cookie would be dropped by a
        // browser that has not made the trustworthy-origin carve-out, the
        // un-framed cookie still does not carry it.
        let loopback = cookie(false, false);
        assert!(!loopback.contains("Secure"), "{loopback}");
        assert!(loopback.contains("; SameSite=Strict"), "{loopback}");
    }

    /// The bootstrap is what tells `/boot` which of those two it is. It must
    /// ask without reading anything off a cross-origin `top`, and must treat a
    /// throw as framed rather than as a window.
    #[test]
    fn the_bootstrap_reports_whether_it_is_framed() {
        let html = concat!(
            "<html><head>",
            "<script type=\"module\" crossorigin src=\"/assets/index-abc.js\"></script>",
            "</head><body></body></html>"
        );
        let out = splice_bootstrap(html).unwrap();
        assert!(out.contains("window.top !== window.self"), "{out}");
        assert!(out.contains("framed: framed"), "{out}");
        // Nothing reads a property off `top`, which would throw cross-origin.
        assert!(!out.contains("window.top."), "{out}");
    }

    #[test]
    fn the_session_route_is_the_apps_own_shape() {
        // `packages/app/src/utils/session-route.ts`: base64url of the server
        // key, no padding -- and the server key is this origin.
        let route = session_route("http://127.0.0.1:7423", "ses_1");
        assert_eq!(route, "/server/aHR0cDovLzEyNy4wLjAuMTo3NDIz/session/ses_1");
        // Declared by both of the app's layouts, and served here.
        assert!(app_route(&segments(&route).unwrap()));
    }

    #[test]
    fn origins_are_compared_whole() {
        assert!(same_origin(
            "http://127.0.0.1:7423",
            "http://127.0.0.1:7423"
        ));
        assert!(!same_origin(
            "http://127.0.0.1:7420",
            "http://127.0.0.1:7423"
        ));
        assert!(!same_origin(
            "https://127.0.0.1:7423",
            "http://127.0.0.1:7423"
        ));
        assert!(!same_origin("null", "http://127.0.0.1:7423"));
        assert!(!same_origin(
            "http://127.0.0.1:7423.evil.example",
            "http://127.0.0.1:7423"
        ));
    }

    #[test]
    fn every_route_the_app_calls_is_a_root_the_gateway_knows() {
        // The app's server-protocol probe and its streams, from
        // `api-ui.md` §6: if one of these were not on the matrix it would be a
        // 404 on this origin rather than a decision the gateway made.
        for root in ["global", "api", "session", "config", "doc", "pty", "event"] {
            assert!(
                crate::gateway::opencode::is_api_root(root),
                "{root} is not on the route matrix"
            );
        }
        // And a path the app never calls is not a tunnel.
        assert!(!crate::gateway::opencode::is_api_root("assets"));
        assert!(!crate::gateway::opencode::is_api_root("boot"));
    }
}
