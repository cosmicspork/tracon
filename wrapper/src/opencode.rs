//! A second window, for OpenCode's own web interface, with nothing in it.
//!
//! tracon's interface is the node's, and the main window is pointed at the
//! node's origin and granted the commands that install and restart the node.
//! OpenCode's native UI is a different thing: third-party code, served from a
//! bundle the node pins, driven by an agent, and reached through a gateway
//! that mediates every call it makes. It is worth having — it is where the
//! harness's own affordances live — and it is not worth giving any of the
//! main window's privileges to.
//!
//! So it gets its own window and its own capability, and the capability
//! grants it nothing (`capabilities/opencode-window.json`). The boundary is
//! three things, none of which depends on the page behaving:
//!
//! - **No IPC.** The capability has no `remote` section, so the UI origin
//!   loaded here can invoke no command: not this app's, not a plugin's.
//! - **One origin.** [`route`] answers every navigation this window attempts.
//!   The UI origin is the whole of its world; an `http(s)` link off it is
//!   handed to the system browser and refused here; anything else is refused
//!   outright. The decision is made in Rust, so opening a browser is not a
//!   capability the window holds.
//! - **A boot token that stays in the fragment.** The node mints it and puts
//!   it in the URL fragment, which is not sent in requests. [`boot_url`]
//!   refuses a boot URL that carries one in the query instead — the very
//!   `?auth_token=` bootstrap OpenCode's own app uses (finding 4) — and
//!   nothing here ever logs a URL with its fragment on it.

use std::sync::{Arc, Mutex};

use tauri::utils::config::WindowConfig;
use tauri::webview::{NewWindowFeatures, NewWindowResponse};
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

/// The window's label, and the one a capability may name.
pub const WINDOW_LABEL: &str = "opencode";

/// What a node that does not serve the OpenCode UI origin is told to the
/// operator as. A node older than that origin answers 404 here, and that is
/// an absence, not a failure.
pub const UNAVAILABLE: &str = "This node does not serve OpenCode's own interface.";

/// Query parameters that would be carrying a credential. OpenCode's app
/// bootstraps its server password from `?auth_token=` and then keeps it in
/// `localStorage`; tracon's boot token goes in the fragment instead, so a
/// boot URL with any of these in its query is the node doing the wrong thing
/// and is refused rather than opened.
const CREDENTIAL_PARAMS: [&str; 6] = [
    "auth_token",
    "access_token",
    "token",
    "password",
    "secret",
    "key",
];

/// The origin this window is allowed to be, shared with the handlers the
/// window was built with. It is set before the window is created and rewritten
/// when a later boot lands on a different port, which is why the handlers read
/// it rather than close over a copy.
#[derive(Default)]
pub struct Allowed(Mutex<String>);

impl Allowed {
    pub fn get(&self) -> String {
        self.0.lock().unwrap().clone()
    }

    pub fn set(&self, origin: String) {
        *self.0.lock().unwrap() = origin;
    }
}

/// What the window does with a navigation it is asked to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The UI origin: the whole of this window's world.
    Allow,
    /// `http(s)` somewhere else — a help page, a provider's sign-in. The
    /// system browser's, not this window's.
    External(String),
    /// A scheme this window has no business following: `file:`, `data:`,
    /// a custom handler, anything that is not the two above.
    Refuse,
}

/// Decide a navigation against the one origin this window may hold.
///
/// `about:blank` is allowed because a blank frame is not a navigation
/// anywhere; a `blob:` of the UI origin serialises to that origin and is
/// allowed with it.
pub fn route(target: &Url, allowed: &str) -> Route {
    if target.as_str() == "about:blank" {
        return Route::Allow;
    }
    if !allowed.is_empty() && target.origin().ascii_serialization() == allowed {
        return Route::Allow;
    }
    if matches!(target.scheme(), "http" | "https") {
        return Route::External(target.as_str().to_owned());
    }
    Route::Refuse
}

/// A URL with nothing secret left on it, for the one line this app ever
/// prints about the OpenCode window. The boot token is in the fragment and
/// the fragment is what this drops.
pub fn redact(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.set_query(None);
    url.to_string()
}

/// Whether the node's answer means "this node has no such endpoint" rather
/// than "that failed". A node from before the UI origin existed answers 404;
/// a route that exists but is switched off answers 501.
pub fn unavailable(status: u16) -> bool {
    matches!(status, 404 | 405 | 501)
}

/// Session ids go into a URL path, so they are checked before they do.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The origin part of a URL, or an error naming what was not one.
pub fn origin_of(url: &str) -> Result<String, String> {
    let parsed: Url = url
        .parse()
        .map_err(|_| format!("`{url}` is not a URL this app can use"))?;
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return Err(format!("`{url}` has no origin"));
    }
    Ok(origin)
}

/// The boot URL out of the node's answer, or why it will not be opened.
///
/// The node is the only thing that mints one, and it is reached over loopback,
/// so this is not distrust of the node so much as the invariants written down
/// where they can fail a build: the UI origin is its own origin and never the
/// node's — same-origin with the operator API is a page that can drive the
/// node with no credential at all — and the boot token is in the fragment.
pub fn boot_url(body: &serde_json::Value, node_origin: &str) -> Result<Url, String> {
    let raw = body
        .get("url")
        .or_else(|| body.get("boot_url"))
        .and_then(serde_json::Value::as_str)
        .ok_or("the node answered without a boot URL for the OpenCode window")?;
    let url: Url = raw
        .parse()
        .map_err(|_| "the node returned a boot URL that is not a URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("the OpenCode window opens http(s) and nothing else".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the boot URL carries credentials in its authority".into());
    }
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        return Err("the boot URL has no origin".into());
    }
    if origin == node_origin {
        return Err(
            "the OpenCode UI would share the node's own origin, where a page needs no credential to drive the node; it needs an origin of its own"
                .into(),
        );
    }
    if let Some(name) = url
        .query_pairs()
        .map(|(k, _)| k.into_owned())
        .find(|k| CREDENTIAL_PARAMS.contains(&k.as_str()))
    {
        return Err(format!(
            "the node put `{name}` in the boot URL's query; a boot token belongs in the fragment, which is not sent in requests"
        ));
    }
    Ok(url)
}

/// Let a link the page meant for a new tab reach the window's own handlers.
///
/// The opener plugin injects a click interceptor into every webview this app
/// builds: it cancels a `target="_blank"` or modified click and invokes
/// `plugin:opener|open_url` instead. In this window that invoke is refused —
/// which is the point — and the click has already been cancelled, so the link
/// would do nothing at all. This runs first, in the capture phase, and stops
/// the event before that listener sees it, on exactly the clicks that listener
/// would have taken. The click then happens natively and `on_new_window`
/// decides, in Rust. The page's own router is untouched: it does not handle
/// `_blank` or modified clicks either.
const LET_LINKS_THROUGH: &str = r#"(function () {
  window.addEventListener('click', function (event) {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.altKey) return
    var anchor = event.composedPath().find(function (node) {
      return node instanceof Node && node.nodeName && node.nodeName.toUpperCase() === 'A'
    })
    if (!anchor || !anchor.href) return
    if (anchor.target !== '_blank' && !event.ctrlKey && !event.shiftKey) return
    try {
      var scheme = new URL(anchor.href).protocol
      if (['http:', 'https:', 'mailto:', 'tel:'].indexOf(scheme) === -1) return
    } catch (_) {
      return
    }
    event.stopImmediatePropagation()
  }, true)
})()"#;

/// Ask the node for a boot URL for this session's OpenCode UI and open the
/// window on it.
///
/// Granted to the main window only (`capabilities/node-interface.json`); the
/// OpenCode window holds no capability that names it, so the page this opens
/// cannot open another.
#[tauri::command]
pub async fn desktop_open_opencode(
    app: tauri::AppHandle,
    session_id: String,
) -> Result<(), String> {
    if !valid_session_id(&session_id) {
        return Err("that is not a session id".into());
    }
    let node = crate::node_url();
    let node_origin = origin_of(&node)?;
    let endpoint = format!("{node}/api/sessions/{session_id}/opencode-boot");
    let response = reqwest::Client::new()
        .post(&endpoint)
        .send()
        .await
        .map_err(|_| "the node did not answer".to_string())?;
    let status = response.status().as_u16();
    if unavailable(status) {
        return Err(UNAVAILABLE.into());
    }
    if !(200..300).contains(&status) {
        let detail = response.text().await.unwrap_or_default();
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("the node refused to open OpenCode's interface ({status})")
        } else {
            format!("the node refused to open OpenCode's interface ({status}): {detail}")
        });
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "the node's answer was not the boot URL".to_string())?;
    let url = boot_url(&body, &node_origin)?;
    open(&app, url)
}

/// Show the window on a boot URL, building it the first time.
fn open(app: &tauri::AppHandle, url: Url) -> Result<(), String> {
    let allowed: Arc<Allowed> = app.state::<Arc<Allowed>>().inner().clone();
    allowed.set(url.origin().ascii_serialization());
    println!("tracon: opening the OpenCode window on {}", redact(&url));

    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        window.navigate(url).map_err(|e| e.to_string())?;
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }

    let mut config = window_config(app)?;
    config.url = WebviewUrl::External(url);

    let nav_app = app.clone();
    let nav_allowed = allowed.clone();
    let new_app = app.clone();
    let new_allowed = allowed.clone();
    let window = WebviewWindowBuilder::from_config(app, &config)
        .map_err(|e| e.to_string())?
        .initialization_script(LET_LINKS_THROUGH)
        .on_navigation(move |target| match route(target, &nav_allowed.get()) {
            Route::Allow => true,
            Route::External(href) => {
                open_externally(&nav_app, &href);
                false
            }
            Route::Refuse => {
                refused(target);
                false
            }
        })
        .on_new_window(move |target, _features: NewWindowFeatures| {
            match route(&target, &new_allowed.get()) {
                // The app's whole OpenCode surface is this one window; a
                // same-origin popup stays in it rather than spawning a second
                // window that no capability describes.
                Route::Allow => {
                    if let Some(window) = new_app.get_webview_window(WINDOW_LABEL) {
                        let _ = window.navigate(target);
                    }
                }
                Route::External(href) => open_externally(&new_app, &href),
                Route::Refuse => refused(&target),
            }
            NewWindowResponse::Deny
        })
        .build()
        .map_err(|e| e.to_string())?;
    let _ = window.show();
    let _ = window.set_focus();
    Ok(())
}

/// The window as `tauri.conf.json` declares it. It is declared there with
/// `"create": false` so that its shape lives beside the main window's and not
/// in this file, and so that nothing opens it at launch.
fn window_config(app: &tauri::AppHandle) -> Result<WindowConfig, String> {
    app.config()
        .app
        .windows
        .iter()
        .find(|w| w.label == WINDOW_LABEL)
        .cloned()
        .ok_or_else(|| format!("no `{WINDOW_LABEL}` window is declared in tauri.conf.json"))
}

/// Hand an off-origin `http(s)` link to the system browser. A Rust call, not
/// a command: the window is granted no opener permission and could not make
/// this happen by itself.
fn open_externally(app: &tauri::AppHandle, href: &str) {
    if let Err(e) = app.opener().open_url(href, None::<&str>) {
        eprintln!("tracon: could not open a link in the browser: {e}");
    }
}

/// Say that something was refused without saying what it was: a refused URL
/// is the one class of URL this app has no reason to trust with a log line.
fn refused(target: &Url) {
    eprintln!(
        "tracon: the OpenCode window refused a `{}` navigation off its origin",
        target.scheme()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UI: &str = "http://127.0.0.1:7421";
    const NODE: &str = "http://127.0.0.1:7420";

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    #[test]
    fn the_ui_origin_is_the_whole_of_the_windows_world() {
        assert_eq!(route(&url("http://127.0.0.1:7421/"), UI), Route::Allow);
        assert_eq!(
            route(&url("http://127.0.0.1:7421/session/abc?x=1#y"), UI),
            Route::Allow
        );
        assert_eq!(route(&url("about:blank"), UI), Route::Allow);
    }

    #[test]
    fn an_off_origin_link_goes_to_the_browser_and_not_to_this_window() {
        assert_eq!(
            route(&url("https://opencode.ai/docs"), UI),
            Route::External("https://opencode.ai/docs".into())
        );
        // A different port on the same host is a different origin, and the
        // node's own origin is the one that matters: it is answered without a
        // credential, so it never loads here.
        assert_eq!(
            route(&url("http://127.0.0.1:7420/api/sessions"), UI),
            Route::External("http://127.0.0.1:7420/api/sessions".into())
        );
        assert_eq!(
            route(&url("http://app.opencode.ai/"), UI),
            Route::External("http://app.opencode.ai/".into())
        );
    }

    #[test]
    fn nothing_but_http_is_followed_or_handed_on() {
        for scheme in [
            "file:///etc/passwd",
            "data:text/html,<h1>hi</h1>",
            "mailto:someone@example.com",
            "tel:+15550100",
            "tauri://localhost/",
            "asset://localhost/etc/passwd",
        ] {
            assert_eq!(route(&url(scheme), UI), Route::Refuse, "{scheme}");
        }
    }

    #[test]
    fn with_no_origin_settled_yet_nothing_is_same_origin() {
        assert_eq!(
            route(&url("http://127.0.0.1:7421/"), ""),
            Route::External("http://127.0.0.1:7421/".into())
        );
    }

    #[test]
    fn a_boot_url_is_taken_whole_and_its_token_stays_in_the_fragment() {
        let body = json!({ "url": "http://127.0.0.1:7421/session/s1#boot=abc123" });
        let url = boot_url(&body, NODE).unwrap();
        assert_eq!(url.fragment(), Some("boot=abc123"));
        assert_eq!(url.origin().ascii_serialization(), UI);
        let body = json!({ "boot_url": "http://127.0.0.1:7421/#boot=abc" });
        assert!(boot_url(&body, NODE).is_ok());
    }

    #[test]
    fn a_boot_token_in_the_query_is_refused() {
        for name in ["auth_token", "token", "access_token", "password", "key"] {
            let body = json!({ "url": format!("http://127.0.0.1:7421/?{name}=abc") });
            let why = boot_url(&body, NODE).unwrap_err();
            assert!(why.contains(name), "{why}");
            assert!(why.contains("fragment"), "{why}");
        }
    }

    #[test]
    fn the_ui_origin_is_never_the_nodes_own_origin() {
        let body = json!({ "url": "http://127.0.0.1:7420/opencode/#boot=abc" });
        let why = boot_url(&body, NODE).unwrap_err();
        assert!(why.contains("origin of its own"), "{why}");
    }

    #[test]
    fn a_boot_url_that_is_not_an_http_url_is_refused() {
        for raw in [
            "file:///tmp/x.html",
            "javascript:alert(1)",
            "http://user:pass@127.0.0.1:7421/",
            "not a url",
        ] {
            assert!(boot_url(&json!({ "url": raw }), NODE).is_err(), "{raw}");
        }
        assert!(boot_url(&json!({}), NODE).is_err());
    }

    #[test]
    fn what_is_logged_carries_neither_fragment_nor_query() {
        let url = url("http://127.0.0.1:7421/session/s1?a=b#boot=secret");
        let line = redact(&url);
        assert_eq!(line, "http://127.0.0.1:7421/session/s1");
        assert!(!line.contains("secret"));
        assert!(!line.contains('#'));
    }

    #[test]
    fn a_node_without_the_endpoint_is_an_absence_not_a_failure() {
        assert!(unavailable(404));
        assert!(unavailable(405));
        assert!(unavailable(501));
        assert!(!unavailable(200));
        assert!(!unavailable(403));
        assert!(!unavailable(500));
    }

    #[test]
    fn a_session_id_that_would_leave_its_path_segment_is_refused() {
        assert!(valid_session_id("01JABCDEF0123456789"));
        assert!(valid_session_id("a-b_c"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../../api/sessions"));
        assert!(!valid_session_id("a/b"));
        assert!(!valid_session_id("a?b=c"));
        assert!(!valid_session_id("a#b"));
    }

    #[test]
    fn the_click_script_only_takes_clicks_the_opener_plugin_would_have_eaten() {
        // The plugin's own listener returns on these; if this script stopped
        // the event for anything more, the page's router would break.
        assert!(LET_LINKS_THROUGH.contains("event.defaultPrevented"));
        assert!(LET_LINKS_THROUGH.contains("anchor.target !== '_blank'"));
        assert!(LET_LINKS_THROUGH.contains("stopImmediatePropagation"));
        assert!(!LET_LINKS_THROUGH.contains("preventDefault()"));
    }
}
