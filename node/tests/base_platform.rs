//! The stock base pinned by the host architecture's own manifest, against
//! real Podman and the published bases: two releases whose base image is the
//! same pin the same digest, and only this host's architecture is pulled.
//!
//! Runs only when asked for, and pulls from ghcr.io:
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 cargo test -p tracon --test base_platform
//! ```
//!
//! Podman is pointed at a store of the test's own (`CONTAINERS_STORAGE_CONF`),
//! so the node's images are neither seen nor touched, and the store is
//! removed when the test ends.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;

use tracon::boundary::registry::{host_architecture, Registry};

use support::podman::{podman, requested};

const BASE: &str = "ghcr.io/cosmicspork/tracon-sandbox-base";

/// A Podman store of the test's own, removed when dropped.
struct Store {
    dir: tempfile::TempDir,
}

impl Store {
    fn new() -> Store {
        let dir = tempfile::Builder::new()
            .prefix("spike-b6-store-")
            .tempdir()
            .unwrap();
        let (root, run) = (dir.path().join("root"), dir.path().join("run"));
        let conf = dir.path().join("storage.conf");
        std::fs::write(
            &conf,
            format!(
                "[storage]\ndriver = \"overlay\"\ngraphroot = \"{}\"\nrunroot = \"{}\"\n",
                root.display(),
                run.display()
            ),
        )
        .unwrap();
        // Before any Podman runs: the node's code under test spawns it with
        // this process's environment.
        std::env::set_var("CONTAINERS_STORAGE_CONF", &conf);
        let graph_root = podman(&["info", "--format", "{{.Store.GraphRoot}}"]).unwrap();
        assert_eq!(
            PathBuf::from(&graph_root),
            root,
            "Podman is not using the test's own store"
        );
        Store { dir }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Layers are owned by the namespace's users, so only Podman's own
        // namespace can remove them, and the overlay driver may leave its
        // directory mounted there.
        let dir = self.dir.path().to_string_lossy().into_owned();
        let script = r#"umount "$1/root/overlay" 2>/dev/null; rm -rf "$1""#;
        let _ = podman(&["unshare", "sh", "-c", script, "sh", &dir]);
    }
}

#[tokio::test]
async fn releases_with_the_same_base_pin_the_same_manifest_and_pull_only_this_arch() {
    if !requested() {
        return;
    }
    let _store = Store::new();
    let arch = host_architecture();
    let other = match arch {
        "amd64" => "arm64",
        _ => "amd64",
    };
    let (older, newer) = (format!("{BASE}:0.45.0"), format!("{BASE}:0.46.0"));

    let (pinned_older, pulled_older) = tracon::sandbox_images::pin_base_podman(&older)
        .await
        .unwrap();
    let (pinned_newer, pulled_newer) = tracon::sandbox_images::pin_base_podman(&newer)
        .await
        .unwrap();
    let registry = Registry::new(None);
    let index = |image: String| {
        let registry = registry.clone();
        async move { registry.identity(&image).await.unwrap() }
    };
    let (index_older, index_newer) = (index(older.clone()).await, index(newer.clone()).await);
    let other_arch = registry
        .platform_identity(&older, other)
        .await
        .unwrap()
        .unwrap();
    eprintln!("0.45.0: index {index_older}, linux/{arch} {pinned_older}");
    eprintln!("0.46.0: index {index_newer}, linux/{arch} {pinned_newer}");
    eprintln!("0.45.0: linux/{other} {other_arch}");

    assert_ne!(index_older, index_newer, "the releases' indexes differ");
    assert_eq!(
        pinned_older, pinned_newer,
        "the same linux/{arch} image under both"
    );
    assert_ne!(
        pinned_older, index_older,
        "pinned by the platform, not the index"
    );
    assert!(pulled_older);
    assert!(!pulled_newer, "the second release's base was already held");

    // One image, this host's, and nothing of the other architecture.
    let images = podman(&["images", "--quiet", "--no-trunc"]).unwrap();
    let ids: std::collections::BTreeSet<&str> = images.lines().collect();
    assert_eq!(ids.len(), 1, "{images}");
    let held_arch = podman(&[
        "image",
        "inspect",
        "--format",
        "{{.Architecture}}",
        &pinned_older,
    ])
    .unwrap();
    assert_eq!(held_arch, arch);
    assert!(podman(&["image", "exists", &other_arch]).is_none());

    // Both tags name it, so a start without the registry finds it, and
    // Podman's own digest for it is the one pinned.
    for tag in [&older, &newer] {
        let digest = podman(&["image", "inspect", "--format", "{{.Digest}}", tag]).unwrap();
        assert_eq!(format!("{BASE}@{digest}"), pinned_older, "{tag}");
    }
}
