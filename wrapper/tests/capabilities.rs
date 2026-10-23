//! The app's privilege boundary, asserted against the manifest rather than
//! against the code that reads it.
//!
//! `tauri.conf.json` and `capabilities/*.json` are what Tauri actually
//! enforces at runtime, and they are data: a permission added to the wrong
//! file, or a second window declared beside the main one, compiles and passes
//! clippy. So the files themselves are the thing under test here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn wrapper_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn config() -> Value {
    read(&wrapper_dir().join("tauri.conf.json"))
}

/// Every capability file, by identifier.
fn capabilities() -> Vec<(String, Value)> {
    let dir = wrapper_dir().join("capabilities");
    let mut found: Vec<(String, Value)> = std::fs::read_dir(&dir)
        .expect("capabilities/")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let value = read(&p);
            let id = value["identifier"]
                .as_str()
                .expect("identifier")
                .to_string();
            (id, value)
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!found.is_empty(), "no capability files were found");
    found
}

/// The permission identifiers a capability grants, including the objects that
/// carry a scope (`{"identifier": "…", "allow": […]}`).
fn granted(capability: &Value) -> BTreeSet<String> {
    capability["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .map(|p| {
            p.as_str()
                .map(str::to_string)
                .or_else(|| p["identifier"].as_str().map(str::to_string))
                .expect("a permission is a string or carries an identifier")
        })
        .collect()
}

fn windows_of(capability: &Value) -> Vec<String> {
    capability["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .map(|w| w.as_str().expect("a window label").to_string())
        .collect()
}

fn window(config: &Value, label: &str) -> Value {
    config["app"]["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .find(|w| w["label"].as_str() == Some(label))
        .unwrap_or_else(|| panic!("no `{label}` window is declared"))
        .clone()
}

#[test]
fn the_main_window_is_the_only_one_and_the_app_builds_it() {
    let config = config();
    let labels: Vec<String> = config["app"]["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .map(|w| {
            w["label"]
                .as_str()
                .expect("every window is labelled")
                .into()
        })
        .collect();
    assert_eq!(labels, vec!["main".to_string()]);

    // The main window is built by the app at startup (`build_main_window`),
    // with the handler that answers its links, so it is not created from here.
    let main = window(&config, "main");
    assert_eq!(main["create"], Value::Bool(false));
    assert_eq!(main["url"], "setup.html", "the main window opens on setup");
}

#[test]
fn every_capability_is_the_main_windows() {
    for (id, capability) in capabilities() {
        assert_eq!(
            windows_of(&capability),
            vec!["main".to_string()],
            "`{id}` names a window other than the main one"
        );
    }
}

/// No window reaches the system browser through a plugin command. A link
/// meant for a new window is answered by the app's own handlers, in Rust, and
/// the host's launcher is started from there (`src/launcher.rs`); a grant here
/// would be a second way out that skips them.
#[test]
fn no_window_holds_an_opener_grant() {
    for (id, capability) in capabilities() {
        for permission in granted(&capability) {
            assert!(
                !permission.starts_with("opener:"),
                "`{id}` grants `{permission}`"
            );
        }
    }
}

#[test]
fn tauri_is_not_asked_to_loosen_anything() {
    let config = config().to_string();
    let mut text = config;
    for (_, capability) in capabilities() {
        text.push_str(&capability.to_string());
    }
    for dangerous in [
        "dangerousRemoteDomainIpcAccess",
        "dangerousDisableAssetCspModification",
        "dangerousUseHttpScheme",
    ] {
        assert!(!text.contains(dangerous), "`{dangerous}` is set");
    }
}

#[test]
fn tauri_writes_no_csp_of_its_own_over_the_one_the_origin_serves() {
    let config = config();
    // Null, not absent and not a policy: the main window holds the node's
    // interface, and that content's CSP is the one the node sends. Tauri only
    // ever writes a policy into content it serves itself, and a policy here
    // would be a second, looser opinion about a page this app does not own.
    assert_eq!(config["app"]["security"]["csp"], Value::Null);
    assert!(config["app"]["security"]
        .get("dangerousDisableAssetCspModification")
        .is_none());
}

#[test]
fn every_desktop_permission_granted_names_a_command_the_build_declares() {
    let build = std::fs::read_to_string(wrapper_dir().join("build.rs")).expect("build.rs");
    for (id, capability) in capabilities() {
        for permission in granted(&capability) {
            let Some(rest) = permission.strip_prefix("allow-") else {
                continue;
            };
            let command = rest.replace('-', "_");
            assert!(
                build.contains(&format!("\"{command}\"")),
                "`{id}` grants `{permission}`, but build.rs declares no `{command}`"
            );
        }
    }
}
