//! Readiness through its surface.
//!
//! An investigation asks only for a place to run; verification asks for
//! checks and a pinned image; publication asks for a forge and a credential
//! this channel may use. Each gap is reported before a session is spent on the
//! path, and none of the later paths' needs is held against an earlier one.

#[path = "support/mod.rs"]
mod support;
use support::harness::{harness_with, Harness};
use support::http::call;
use support::state;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use serde_json::Value;

use tracon::broker::Credential;
use tracon::config::{Config, Repo, Supervision};

const PINNED: &str = "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

async fn ready_node(h: &Harness) {
    h.store
        .put_node(&support::rows::node_row("n1", "n"))
        .unwrap();
}

async fn readiness(h: &Harness, repo: &str) -> Value {
    let (st, v) = call(
        &h.operator,
        "GET",
        &format!("/api/readiness?channel=personal&repo={repo}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn missing(v: &Value, path: &str) -> Vec<String> {
    v[path]["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap().to_string())
        .collect()
}

fn bind_gh(h: &Harness, channel: &str) {
    h.tools.broker.write().unwrap().put(
        "gh",
        Credential {
            env: BTreeMap::from([("GH_TOKEN".to_string(), "t".to_string())]),
            channels: vec![channel.to_string()],
            ..Credential::default()
        },
    );
}

#[tokio::test]
async fn each_path_asks_only_for_what_it_needs() {
    state::isolate();
    let clone = state::managed_clone("readiness-paths");
    git(
        &clone,
        &["remote", "set-url", "origin", "git@github.com:o/r.git"],
    );
    let repo = "github.com/readiness-paths/repo".to_string();

    // No checks, and no credential at all.
    let h = harness_with(Config {
        supervision: Supervision {
            checks: Vec::new(),
            ..Supervision::default()
        },
        ..Config::default()
    })
    .await;
    ready_node(&h).await;
    let v = readiness(&h, &repo).await;
    assert_eq!(v["investigate"]["ready"], true, "{v}");
    assert_eq!(missing(&v, "verify"), vec!["checks"], "{v}");
    assert_eq!(missing(&v, "publish"), vec!["checks", "credential"], "{v}");

    // The repository's own checks and image: verifiable, but the gh
    // credential serves another channel.
    let h = harness_with(Config {
        repo: vec![Repo {
            path: clone.clone(),
            image: Some(PINNED.into()),
            checks: Some(vec!["just check".into()]),
            ..Repo::default()
        }],
        ..Config::default()
    })
    .await;
    ready_node(&h).await;
    bind_gh(&h, "work");
    let v = readiness(&h, &repo).await;
    assert_eq!(v["verify"]["ready"], true, "{v}");
    assert_eq!(missing(&v, "publish"), vec!["credential"], "{v}");
    assert!(
        v["publish"]["missing"][0]["message"]
            .as_str()
            .unwrap()
            .contains("not bound"),
        "{v}"
    );

    bind_gh(&h, "personal");
    let v = readiness(&h, &repo).await;
    assert_eq!(v["publish"]["ready"], true, "{v}");
}

#[tokio::test]
async fn a_node_that_cannot_launch_says_so_on_every_path() {
    state::isolate();
    let dir = tempfile::tempdir().unwrap();
    let h = harness_with(Config::default()).await;
    // The node row has never been checked, and the directory is not a git
    // repository with a remote.
    let v = readiness(&h, &dir.path().to_string_lossy()).await;
    for path in ["investigate", "verify", "publish"] {
        assert_eq!(v[path]["ready"], false, "{v}");
        assert!(missing(&v, path).contains(&"launch".to_string()), "{v}");
    }
    assert!(
        missing(&v, "publish").contains(&"remote".to_string()),
        "{v}"
    );

    let (st, _) = call(
        &h.operator,
        "GET",
        "/api/readiness?channel=personal&repo=",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A forge repository this node has not cloned is no gap: the session clones
/// it first, and readiness says so. An operator's own checkout is refused on
/// every path, in the words a create is refused with.
#[tokio::test]
async fn an_uncloned_repository_will_clone_and_a_host_checkout_is_refused() {
    state::isolate();
    let h = harness_with(Config::default()).await;
    ready_node(&h).await;
    bind_gh(&h, "personal");
    let v = readiness(&h, "github.com/readiness/not-cloned").await;
    assert_eq!(v["investigate"]["ready"], true, "{v}");
    assert_eq!(
        v["investigate"]["notes"][0]["message"],
        "github.com/readiness/not-cloned is not cloned yet (will clone)",
        "{v}"
    );
    assert!(
        !missing(&v, "publish").contains(&"remote".to_string()),
        "{v}"
    );

    let host = tempfile::tempdir().unwrap();
    git(host.path(), &["init", "-q"]);
    let v = readiness(&h, &host.path().to_string_lossy()).await;
    for path in ["investigate", "verify", "publish"] {
        assert_eq!(missing(&v, path)[0], "repo", "{v}");
        assert_eq!(
            v[path]["missing"][0]["message"],
            tracon::forge::HOST_CHECKOUT_REFUSED,
            "{v}"
        );
    }
}
