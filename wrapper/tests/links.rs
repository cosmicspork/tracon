//! The links the node asks the desktop app to open in the system browser.
//!
//! Against a remote node the app's webview cannot make a passkey, so signing
//! in and confirming an admin action go through a device approval: the
//! interface opens the node's `approve_url` (`<public_origin>/#approve=<id>`)
//! as a `_blank` link, and the main window hands it to the system browser
//! only if [`browser_link`] passes it. No new capability or IPC is involved.

#[path = "../src/links.rs"]
mod links;

use links::browser_link;

fn link(s: &str) -> bool {
    browser_link(&tauri::Url::parse(s).unwrap())
}

#[test]
fn a_device_approval_address_goes_to_the_system_browser() {
    // As `http::device` builds it from an https `[auth] public_origin`.
    assert!(link(
        "https://tracon-node.example.org/#approve=q0VbYF9rZ2x2b2JkUXlqTg"
    ));
    assert!(link(
        "https://tracon-node.example.org:8443/#approve=q0VbYF9rZ2x2b2JkUXlqTg"
    ));
}

#[test]
fn nothing_but_plain_https_leaves_the_window() {
    // A loopback node is the app's own; it never asks for an approval, and
    // http is never handed out.
    assert!(!link(
        "http://localhost:7420/#approve=q0VbYF9rZ2x2b2JkUXlqTg"
    ));
    assert!(!link(
        "https://user:pass@tracon-node.example.org/#approve=q0VbYF9rZ2x2b2JkUXlqTg"
    ));
    assert!(!link("javascript:alert(1)"));
    assert!(!link("file:///etc/passwd"));
}
