//! The preparation preview through its surface.
//!
//! Before a session is launched, the operator can read what preparing the
//! checkout would do and every place the repository asks for something the
//! node will not do, with where that work belongs. Asking reads files and
//! runs nothing.

#[path = "support/mod.rs"]
mod support;
use support::harness::harness;
use support::http::call;

use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn a_devcontainer_hook_is_explained_before_launch_and_never_run() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    std::fs::create_dir_all(dir.path().join(".devcontainer")).unwrap();
    std::fs::write(
        dir.path().join(".devcontainer/devcontainer.json"),
        json!({
            "build": { "dockerfile": "Dockerfile" },
            "initializeCommand": format!("touch {}", marker.display()),
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("Cargo.lock"), "").unwrap();

    let h = harness().await;
    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/preparation?repo={}", dir.path().display()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["ready"], false, "{v}");
    assert_eq!(v["install"], "cargo fetch --locked");
    let items: Vec<&str> = v["incompatible"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["item"].as_str().unwrap())
        .collect();
    assert_eq!(items, vec!["initializeCommand", "build"], "{v}");
    assert!(
        v["incompatible"][0]["instead"]
            .as_str()
            .unwrap()
            .contains("prepare"),
        "{v}"
    );
    assert!(!marker.exists(), "previewing ran a repository hook");

    let (st, _) = call(
        &h.operator,
        "GET",
        "/api/preparation?repo=relative/path",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}
