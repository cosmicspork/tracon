//! What the main window hands to the system browser.
//!
//! Its own file so `tests/links.rs` can check the rule against the addresses
//! the node actually sends, such as a device approval's `approve_url`, without
//! building the app.

/// What the main window may send to the browser: `https`, with no credentials
/// in the URL. The scope the opener plugin was granted, kept.
pub fn browser_link(target: &tauri::Url) -> bool {
    target.scheme() == "https" && target.username().is_empty() && target.password().is_none()
}
