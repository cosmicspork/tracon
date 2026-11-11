//! `[[service]] bundle = "release"`: the node fetches its release's browser
//! bundle at converge, from a local release server, into its state directory,
//! and keeps only that Chrome's. No Podman: what is checked is the fetch.

#[path = "support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};
use tracon::config::{Config, RuntimeKind, Service};
use tracon::service_bundle::{self, CHROME_VERSION};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The release's browser bundle for `arch` as `scripts/browser-bundle.sh`
/// makes it, published under `/v<VERSION>/` in `release`.
fn publish(release: &Path, arch: &str) {
    let files: &[(&str, &[u8], u32)] = &[
        ("run", b"#!/bin/sh\nexec true\n", 0o755),
        ("headless-shell/headless-shell", b"chrome", 0o755),
        ("lib/libnss3.so", b"nss", 0o644),
    ];
    let listed: serde_json::Map<String, serde_json::Value> = files
        .iter()
        .map(|(path, content, _)| (path.to_string(), sha256(content).into()))
        .collect();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schema": 1, "service": "browser", "version": CHROME_VERSION, "arch": arch,
        "source": "docker.io/chromedp/headless-shell", "run": "run", "files": listed,
    }))
    .unwrap();
    let dir = release.join(format!("v{VERSION}"));
    std::fs::create_dir_all(&dir).unwrap();
    let name = format!("tracon-browser-{CHROME_VERSION}-{arch}.tar.gz");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        std::fs::File::create(dir.join(&name)).unwrap(),
        flate2::Compression::fast(),
    ));
    let mut add = |path: &str, content: &[u8], mode: u32| {
        let mut header = tar::Header::new_ustar();
        header.set_size(content.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(
                &mut header,
                format!("{CHROME_VERSION}/{arch}/{path}"),
                content,
            )
            .unwrap();
    };
    add("manifest.json", &manifest, 0o644);
    for (path, content, mode) in files {
        add(path, content, *mode);
    }
    builder.into_inner().unwrap().finish().unwrap();
    let tarball = std::fs::read(dir.join(&name)).unwrap();
    std::fs::write(
        dir.join(format!("{name}.sha256")),
        format!("{}  {name}\n", sha256(&tarball)),
    )
    .unwrap();
}

async fn serve(dir: PathBuf) -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::extract::Path as AxPath;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let app = axum::Router::new().route(
        "/{tag}/{file}",
        axum::routing::get(move |AxPath((tag, file)): AxPath<(String, String)>| {
            let (dir, seen) = (dir.clone(), seen.clone());
            async move {
                seen.lock().unwrap().push(format!("{tag}/{file}"));
                match std::fs::read(dir.join(&tag).join(&file)) {
                    Ok(bytes) => (axum::http::StatusCode::OK, bytes),
                    Err(_) => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), asked)
}

/// A node that runs services in its sandboxes, with the release's browser in
/// its catalogue. Apple's runtime always does, on any host; Podman's needs
/// Linux.
fn node(release_url: &str) -> Config {
    let mut cfg = Config {
        service: vec![Service {
            name: "browser".into(),
            bundle: Some("release".into()),
            port: 9222,
            ready: Some("/json/version".into()),
            ..Service::default()
        }],
        ..Config::default()
    };
    cfg.runtime.kind = RuntimeKind::Apple;
    cfg.sandbox.release_url = Some(release_url.into());
    tracon::config::validate_services(&cfg.service).unwrap();
    cfg
}

#[tokio::test]
async fn the_node_fetches_its_releases_browser_and_keeps_only_that_chrome() {
    support::state::isolate();
    let arch = tracon::bundle::host_arch().unwrap();
    let release = tempfile::tempdir().unwrap();
    publish(release.path(), arch);
    let (url, asked) = serve(release.path().to_path_buf()).await;
    let cfg = node(&url);
    assert!(service_bundle::wanted(&cfg));

    let state = Config::state_dir();
    let bundles = state.join("bundles");
    // An older Chrome fetched by a previous release, and this release's
    // harness bundle beside it.
    let stale = bundles.join("browser/1.0.0.0");
    std::fs::create_dir_all(stale.join(arch)).unwrap();
    std::fs::create_dir_all(bundles.join(VERSION).join(arch)).unwrap();

    let missing = service_bundle::missing(&cfg, false).unwrap();
    assert!(missing.contains(&url), "{missing}");
    assert!(service_bundle::converge(&cfg).await.unwrap(), "fetched");
    let fetched = service_bundle::resolve(&cfg.service[0]).unwrap();
    assert_eq!(
        fetched,
        bundles.join("browser").join(CHROME_VERSION).join(arch)
    );
    assert!(fetched.join("run").is_file());
    assert_eq!(service_bundle::missing(&cfg, false), None);
    assert_eq!(
        service_bundle::mounts(&cfg),
        [(fetched.clone(), "/opt/tracon-services/browser".to_string())]
    );
    assert!(!stale.exists(), "another Chrome's bundle is superseded");
    assert!(
        bundles.join(VERSION).join(arch).is_dir(),
        "the harness bundle is not the browser's to remove"
    );
    assert_eq!(
        *asked.lock().unwrap(),
        [
            format!("v{VERSION}/tracon-browser-{CHROME_VERSION}-{arch}.tar.gz.sha256"),
            format!("v{VERSION}/tracon-browser-{CHROME_VERSION}-{arch}.tar.gz"),
        ]
    );

    // The next start finds it and fetches nothing.
    assert!(!service_bundle::converge(&cfg).await.unwrap(), "cached");
    assert_eq!(asked.lock().unwrap().len(), 2);

    // The harness bundle's own supersession leaves the browser's alone.
    assert!(!tracon::bundle::superseded(&state, VERSION).contains(&bundles.join("browser")));

    // A release whose .sha256 does not match is refused, and what the node
    // had is left for the next converge to check again rather than replaced.
    std::fs::remove_dir_all(&fetched).unwrap();
    let name = format!("tracon-browser-{CHROME_VERSION}-{arch}.tar.gz");
    std::fs::write(
        release.path().join(format!("v{VERSION}/{name}.sha256")),
        format!("{}  {name}\n", "0".repeat(64)),
    )
    .unwrap();
    let refused = service_bundle::converge(&cfg).await.unwrap_err();
    assert!(refused.contains("does not match its .sha256"), "{refused}");
    assert!(!fetched.exists());
}

#[tokio::test]
async fn a_node_that_runs_no_services_in_its_sandboxes_fetches_nothing() {
    support::state::isolate();
    let (url, asked) = serve(tempfile::tempdir().unwrap().path().to_path_buf()).await;
    let mut cfg = node(&url);
    cfg.runtime.kind = RuntimeKind::default();
    cfg.runtime.podman = tracon::config::PodmanRuntime::legacy();
    assert!(!service_bundle::wanted(&cfg));
    assert!(!service_bundle::converge(&cfg).await.unwrap());
    assert_eq!(service_bundle::missing(&cfg, false), None);
    assert!(asked.lock().unwrap().is_empty());
}
