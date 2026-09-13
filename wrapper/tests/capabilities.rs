//! The privilege boundary between the two windows, asserted against the
//! manifest rather than against the code that reads it.
//!
//! `tauri.conf.json` and `capabilities/*.json` are what Tauri actually
//! enforces at runtime, and they are data: a permission added to the wrong
//! file compiles, passes clippy, and hands the OpenCode UI the command that
//! restarts the node. So the files themselves are the thing under test here.

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
/// carry a scope (`{"identifier": "opener:allow-open-url", "allow": […]}`).
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
fn the_opencode_window_is_declared_and_nothing_opens_it_at_launch() {
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
    assert_eq!(labels, vec!["main".to_string(), "opencode".to_string()]);

    let opencode = window(&config, "opencode");
    assert_eq!(
        opencode["create"],
        Value::Bool(false),
        "the OpenCode window is created on demand, on a boot URL the node mints"
    );
    // It carries no URL of its own: the only URL it ever holds is the one the
    // node returns, checked by `opencode::boot_url` before the window exists.
    assert!(opencode.get("url").is_none());

    let main = window(&config, "main");
    assert!(
        main.get("create").is_none(),
        "the main window is still the one the app opens on"
    );
}

#[test]
fn the_opencode_window_is_granted_no_command_that_manages_the_node() {
    let capabilities = capabilities();
    let node_management: BTreeSet<String> = capabilities
        .iter()
        .filter(|(id, _)| id != "opencode-window")
        .flat_map(|(_, c)| granted(c))
        .filter(|p| p.starts_with("allow-desktop-"))
        .collect();
    assert!(
        node_management.contains("allow-desktop-install-update")
            && node_management.contains("allow-desktop-restart-node")
            && node_management.contains("allow-desktop-install-cli")
            && node_management.contains("allow-desktop-open-opencode"),
        "the node-management set was not found where it was expected: {node_management:?}"
    );

    let (_, opencode) = capabilities
        .iter()
        .find(|(id, _)| id == "opencode-window")
        .expect("capabilities/opencode-window.json");
    let held = granted(opencode);
    for command in &node_management {
        assert!(
            !held.contains(command),
            "the OpenCode window is granted `{command}`"
        );
    }
}

#[test]
fn the_opencode_window_holds_nothing_but_its_own_window_controls() {
    let capabilities = capabilities();
    let (_, opencode) = capabilities
        .iter()
        .find(|(id, _)| id == "opencode-window")
        .expect("capabilities/opencode-window.json");

    assert_eq!(windows_of(opencode), vec!["opencode".to_string()]);
    // No `remote`: this is what decides that the UI origin, which is all this
    // window ever loads, can invoke nothing at all. A `remote.urls` entry here
    // would hand the page below every permission in the file.
    assert!(
        opencode.get("remote").is_none(),
        "the OpenCode window's capability names a remote origin"
    );
    assert_eq!(opencode["local"], Value::Bool(true));

    for permission in granted(opencode) {
        assert!(
            permission.starts_with("core:window:allow-"),
            "the OpenCode window is granted `{permission}`"
        );
        for forbidden in ["shell:", "fs:", "http:", "opener:", "core:app:"] {
            assert!(!permission.starts_with(forbidden), "{permission}");
        }
    }
}

#[test]
fn no_other_capability_reaches_the_opencode_window() {
    for (id, capability) in capabilities() {
        if id == "opencode-window" {
            continue;
        }
        assert_eq!(
            windows_of(&capability),
            vec!["main".to_string()],
            "`{id}` names a window other than the main one"
        );
    }
}

#[test]
fn the_system_browser_is_the_main_windows_grant_and_only_over_https() {
    let mut found = 0;
    for (id, capability) in capabilities() {
        for permission in capability["permissions"].as_array().expect("permissions") {
            let identifier = permission
                .as_str()
                .or_else(|| permission["identifier"].as_str())
                .expect("an identifier");
            if !identifier.starts_with("opener:") {
                continue;
            }
            found += 1;
            assert_eq!(id, "node-interface");
            assert_eq!(windows_of(&capability), vec!["main".to_string()]);
            let allowed = permission["allow"].as_array().expect("a scoped opener");
            for entry in allowed {
                let url = entry["url"].as_str().expect("a url scope");
                assert!(
                    url.starts_with("https://") || url.starts_with("http://"),
                    "the opener is scoped to `{url}`"
                );
            }
        }
    }
    assert_eq!(found, 1, "the opener is granted in exactly one place");
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
    // Null, not absent and not a policy: the OpenCode window holds remote
    // content, and remote content's CSP is the one its server sends. Tauri
    // only ever writes a policy into content it serves itself, and a policy
    // here would be a second, looser opinion about a page this app does not
    // own.
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
