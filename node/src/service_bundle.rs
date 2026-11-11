//! The release's `browser` service bundle, fetched by the node as it fetches
//! the harness bundle (`crate::bundle`).
//!
//! A `[[service]]` entry says `bundle = "release"` to run the browser bundle
//! its node's release attaches (`tracon-browser-<chrome>-<arch>.tar.gz`, from
//! `scripts/browser-bundle.sh`), rather than a directory the operator
//! unpacked. Only `browser` is published. Where the node runs services in its
//! sandboxes (Podman with `[runtime.podman] sandbox_services`, and Apple),
//! the boundary's converge fetches it into
//! `<state>/bundles/browser/<chrome>/<arch>`, at start and whenever the
//! definitions move, and removes other Chrome versions.
//!
//! `<chrome>` is the headless-shell tag pinned in
//! `containers/browser/Containerfile`, which the release's bundle is built
//! from; `build.rs` reads it, so the node and its release's asset name the
//! same Chrome. The asset is fetched from the node's own release:
//! `<release_url>/v<version>/tracon-browser-<chrome>-<arch>.tar.gz`.
//!
//! What is checked, before anything is mounted, as for the harness bundle:
//!
//! - the tarball against the `.sha256` published beside it;
//! - every entry of the tarball: a directory or a regular file under
//!   `<chrome>/<arch>/`, nothing else;
//! - the manifest: this service, Chrome version and architecture, every file
//!   in the tree named by it with its digest, and its `run` among them.
//!
//! A cached bundle is checked the same way at every converge, so one altered
//! on disk is fetched again rather than mounted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::bundle::{BundleError, Ensured};
use crate::config::{Config, Service};

/// Where under the state directory's `bundles` the browser bundles are kept.
pub const DIR: &str = "browser";

/// What a `[[service]]`'s `bundle` says to run its release's bundle.
pub const RELEASE: &str = "release";

/// The one service a release publishes a bundle of.
pub const BROWSER: &str = "browser";

/// The Chrome version of the release's browser bundle; empty when this build
/// had no pinned headless-shell to read it from.
pub const CHROME_VERSION: &str = env!("TRACON_BROWSER_VERSION");

/// Whether `service` asks for its release's bundle.
pub fn is_release(service: &Service) -> bool {
    service.bundle.as_deref() == Some(Path::new(RELEASE))
}

pub fn managed_dir(state_dir: &Path, chrome: &str, arch: &str) -> PathBuf {
    state_dir
        .join(crate::bundle::DIR)
        .join(DIR)
        .join(chrome)
        .join(arch)
}

/// Where `service`'s bundle is on this host: the directory it names, or for
/// `release` the one the node fetches. `None` when it has none, or names the
/// release's on a host or build the release has none for.
pub fn resolve(service: &Service) -> Option<PathBuf> {
    if !is_release(service) {
        return service.bundle.clone();
    }
    let arch = crate::bundle::host_arch()?;
    (!CHROME_VERSION.is_empty()).then(|| managed_dir(&Config::state_dir(), CHROME_VERSION, arch))
}

/// Each catalogue service's bundle on this host and where a sandbox mounts
/// it (or a DigitalOcean template holds it), for a runtime that runs
/// services in its sandboxes.
pub fn mounts(cfg: &Config) -> Vec<(PathBuf, String)> {
    cfg.service
        .iter()
        .filter_map(|service| Some((resolve(service)?, service.bundle_target())))
        .collect()
}

/// Whether this node runs `[[service]]` bundles in its sandboxes.
fn runs_in_sandboxes(cfg: &Config) -> bool {
    crate::boundary::podman::uses_sandbox_services(cfg)
        || cfg.runtime.kind == crate::config::RuntimeKind::Apple
}

/// Whether the node bakes `[[service]]` bundles into the DigitalOcean
/// templates it builds (`[digitalocean] registry`), where nothing is
/// mounted.
fn bakes_for_digitalocean(cfg: &Config) -> bool {
    crate::provider::digitalocean::runtime::enabled(cfg) && !cfg.digitalocean.registry.is_empty()
}

/// Whether the node fetches the release's browser bundle: it runs services
/// in its sandboxes, or bakes them into DigitalOcean's templates, and one
/// asks for it.
pub fn wanted(cfg: &Config) -> bool {
    (runs_in_sandboxes(cfg) || bakes_for_digitalocean(cfg)) && cfg.service.iter().any(is_release)
}

/// The first in-sandbox service bundle that is not here, in words for the
/// operator; `None` when every one is, or the node runs none. `fetching`:
/// the node is converging, so the release's is on its way.
pub fn missing(cfg: &Config, fetching: bool) -> Option<String> {
    if !runs_in_sandboxes(cfg) {
        return None;
    }
    for service in cfg.service.iter().filter(|s| s.bundle.is_some()) {
        match resolve(service) {
            Some(dir) if dir.is_dir() => {}
            Some(dir) if is_release(service) && fetching => {
                return Some(format!(
                    "the release's browser bundle {} is not here yet; the node is fetching it",
                    dir.display()
                ))
            }
            Some(dir) if is_release(service) => {
                return Some(format!(
                    "the release's browser bundle {} is not here; the node fetches it from {} when it sets up its boundary (`tracon setup`)",
                    dir.display(),
                    cfg.sandbox.release_url()
                ))
            }
            Some(dir) => {
                return Some(format!(
                    "service bundle {:?} is not a directory",
                    dir.display().to_string()
                ))
            }
            None => {
                return Some(format!(
                    "service {}: this build names no release browser bundle for {}",
                    service.name,
                    std::env::consts::ARCH
                ))
            }
        }
    }
    None
}

/// What a browser bundle's `manifest.json` says. Unknown fields (`source`)
/// are allowed.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub service: String,
    pub version: String,
    pub arch: String,
    pub run: String,
    /// Every file but the manifest, by its path in the bundle, with its
    /// sha256.
    pub files: BTreeMap<String, String>,
}

/// Every regular file under `dir`, relative to it; anything else in the tree
/// is refused.
fn files_under(dir: &Path) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let entries = std::fs::read_dir(dir.join(&relative))
            .map_err(|e| format!("{}: {e}", relative.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = relative.join(entry.file_name());
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                found.push(path.to_string_lossy().into_owned());
            } else {
                return Err(format!(
                    "{} is neither a file nor a directory",
                    path.display()
                ));
            }
        }
    }
    Ok(found)
}

/// Check one unpacked `<chrome>/<arch>` directory against its manifest.
pub fn verify_dir(dir: &Path, chrome: &str, arch: &str) -> Result<Manifest, BundleError> {
    let bad = |why: String| BundleError::Malformed(format!("{}: {why}", dir.display()));
    let text = std::fs::read_to_string(dir.join("manifest.json"))
        .map_err(|e| bad(format!("manifest.json: {e}")))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| bad(format!("manifest.json: {e}")))?;
    if manifest.schema != 1 {
        return Err(bad(format!("manifest schema {}", manifest.schema)));
    }
    if manifest.service != BROWSER || manifest.version != chrome || manifest.arch != arch {
        return Err(bad(format!(
            "the manifest is for {} {} {}, not {BROWSER} {chrome} {arch}",
            manifest.service, manifest.version, manifest.arch
        )));
    }
    if !manifest.files.contains_key(&manifest.run) {
        return Err(bad(format!("its run, {:?}, is not in it", manifest.run)));
    }
    for (path, digest) in &manifest.files {
        if !crate::bundle::plain(Path::new(path)) || path == "manifest.json" {
            return Err(bad(format!("the manifest names {path:?}")));
        }
        let file = dir.join(path);
        let meta = std::fs::symlink_metadata(&file).map_err(|e| bad(format!("{path}: {e}")))?;
        if !meta.file_type().is_file() {
            return Err(bad(format!("{path} is not a regular file")));
        }
        let got = crate::bundle::sha256_file(&file)?;
        if !got.eq_ignore_ascii_case(digest) {
            return Err(bad(format!(
                "{path} has sha256 {got}, the manifest says {digest}"
            )));
        }
    }
    for path in files_under(dir).map_err(bad)? {
        if path != "manifest.json" && !manifest.files.contains_key(&path) {
            return Err(bad(format!("{path} is not in the manifest")));
        }
    }
    Ok(manifest)
}

/// Unpack `tarball` into `into`, refusing anything but directories and
/// regular files under `<chrome>/<arch>/`. A file keeps whether it was
/// executable, and nothing else of its mode.
fn unpack(tarball: &Path, into: &Path, chrome: &str, arch: &str) -> Result<(), BundleError> {
    use std::os::unix::fs::PermissionsExt as _;
    let root = Path::new(chrome).join(arch);
    let file = std::fs::File::open(tarball)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let malformed = |e: std::io::Error| BundleError::Malformed(e.to_string());
    for entry in archive.entries().map_err(malformed)? {
        let mut entry = entry.map_err(malformed)?;
        let path = entry.path().map_err(malformed)?.into_owned();
        let path = path.strip_prefix("./").unwrap_or(&path).to_path_buf();
        let kind = entry.header().entry_type();
        if kind.is_pax_local_extensions() || kind.is_pax_global_extensions() {
            continue;
        }
        if !crate::bundle::plain(&path) || !(path.starts_with(&root) || root.starts_with(&path)) {
            return Err(BundleError::Malformed(format!(
                "the tarball holds {}",
                path.display()
            )));
        }
        let dest = into.join(&path);
        if kind.is_dir() {
            std::fs::create_dir_all(&dest)?;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
        } else if kind.is_file() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            let executable = entry.header().mode().map_err(malformed)? & 0o111 != 0;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
        } else {
            return Err(BundleError::Malformed(format!(
                "{} is neither a file nor a directory",
                path.display()
            )));
        }
    }
    Ok(())
}

/// The browser bundle of `chrome` for `arch` under `state_dir`, fetched from
/// the `version` release at `release_url` unless the one there checks out.
pub async fn ensure_at(
    client: &reqwest::Client,
    release_url: &str,
    state_dir: &Path,
    version: &str,
    chrome: &str,
    arch: &str,
) -> Result<Ensured, BundleError> {
    let dest = managed_dir(state_dir, chrome, arch);
    if dest.is_dir() {
        let check = dest.clone();
        let (c, a) = (chrome.to_string(), arch.to_string());
        match tokio::task::spawn_blocking(move || verify_dir(&check, &c, &a))
            .await
            .map_err(|e| BundleError::Malformed(e.to_string()))?
        {
            Ok(_) => return Ok(Ensured::Cached(dest)),
            Err(error) => {
                tracing::warn!(bundle = %dest.display(), %error, "the cached browser bundle does not check out; fetching it again")
            }
        }
    }
    let root = state_dir.join(crate::bundle::DIR).join(DIR);
    std::fs::create_dir_all(&root)?;
    let incoming = tempfile::Builder::new()
        .prefix(".incoming-")
        .tempdir_in(&root)?;
    let name = format!("tracon-browser-{chrome}-{arch}.tar.gz");
    let base = format!("{}/v{version}", release_url.trim_end_matches('/'));
    let tarball = incoming.path().join(&name);
    let sums = incoming.path().join(format!("{name}.sha256"));
    crate::bundle::download(client, &format!("{base}/{name}.sha256"), &sums).await?;
    crate::bundle::download(client, &format!("{base}/{name}"), &tarball).await?;
    let unpacked = incoming.path().join("unpacked");
    let (c, a) = (chrome.to_string(), arch.to_string());
    let checked = unpacked.clone();
    tokio::task::spawn_blocking(move || -> Result<(), BundleError> {
        let expected = crate::bundle::expected_digest(&std::fs::read_to_string(&sums)?, &name)?;
        let got = crate::bundle::sha256_file(&tarball)?;
        if got != expected {
            return Err(BundleError::Digest {
                name,
                got,
                expected,
            });
        }
        unpack(&tarball, &checked, &c, &a)?;
        verify_dir(&checked.join(&c).join(&a), &c, &a).map(|_| ())
    })
    .await
    .map_err(|e| BundleError::Malformed(e.to_string()))??;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(unpacked.join(chrome).join(arch), &dest)?;
    Ok(Ensured::Fetched(dest))
}

/// Every fetched browser bundle but `keep_chrome`'s. Only a converge removes
/// them, and a sandbox that mounts one does not outlive the node process
/// that started it.
pub fn superseded(state_dir: &Path, keep_chrome: &str) -> Vec<PathBuf> {
    let root = state_dir.join(crate::bundle::DIR).join(DIR);
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name != keep_chrome && !name.starts_with(".incoming-")
        })
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect()
}

/// The release's browser bundle, fetched if it has to be, and other Chrome
/// versions removed. `Ok(None)` when the node does not want it.
pub async fn ensure(cfg: &Config) -> Result<Option<Ensured>, BundleError> {
    if !wanted(cfg) {
        return Ok(None);
    }
    let arch = crate::bundle::host_arch()
        .ok_or_else(|| BundleError::UnsupportedArch(std::env::consts::ARCH.into()))?;
    if CHROME_VERSION.is_empty() {
        return Err(BundleError::Malformed(
            "this build names no Chrome version (containers/browser/Containerfile had no pinned headless-shell)".into(),
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(crate::bundle::FETCH_TIMEOUT)
        .build()
        .map_err(|e| BundleError::Fetch {
            url: cfg.sandbox.release_url(),
            error: e.to_string(),
        })?;
    let state = Config::state_dir();
    let ensured = ensure_at(
        &client,
        &cfg.sandbox.release_url(),
        &state,
        env!("CARGO_PKG_VERSION"),
        CHROME_VERSION,
        arch,
    )
    .await?;
    for old in superseded(&state, CHROME_VERSION) {
        match std::fs::remove_dir_all(&old) {
            Ok(()) => {
                tracing::info!(bundle = %old.display(), "removed a browser bundle of another Chrome version")
            }
            Err(error) => {
                tracing::warn!(bundle = %old.display(), %error, "could not remove an old browser bundle")
            }
        }
    }
    Ok(Some(ensured))
}

/// What a boundary's converge adds for the browser: [`ensure`], logged.
/// `Ok(true)` when it fetched the bundle.
pub async fn converge(cfg: &Config) -> Result<bool, String> {
    match ensure(cfg).await {
        Ok(Some(Ensured::Fetched(path))) => {
            tracing::info!(bundle = %path.display(), "fetched the browser bundle for this release");
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) => Err(format!("browser bundle: {error}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};

    /// A browser bundle as `scripts/browser-bundle.sh` lays it out, as a
    /// tarball and its `.sha256`: `files` (path → content, executable) under
    /// `<chrome>/<arch>/`, with `run` among them.
    pub(crate) fn tarball(
        dir: &Path,
        chrome: &str,
        arch: &str,
        files: &[(&str, &[u8], bool)],
    ) -> (PathBuf, PathBuf) {
        let listed: serde_json::Map<String, serde_json::Value> = files
            .iter()
            .map(|(path, content, _)| {
                (
                    path.to_string(),
                    serde_json::json!(hex::encode(Sha256::digest(content))),
                )
            })
            .collect();
        let manifest = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": 1, "service": "browser", "version": chrome, "arch": arch,
            "source": "docker.io/chromedp/headless-shell:x", "run": "run", "files": listed,
        }))
        .unwrap();
        let name = format!("tracon-browser-{chrome}-{arch}.tar.gz");
        let path = dir.join(&name);
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        let mut add = |path: String, content: &[u8], mode: u32| {
            let mut header = tar::Header::new_ustar();
            header.set_size(content.len() as u64);
            header.set_mode(mode);
            header.set_entry_type(tar::EntryType::Regular);
            builder.append_data(&mut header, path, content).unwrap();
        };
        add(format!("{chrome}/{arch}/manifest.json"), &manifest, 0o644);
        for (file, content, executable) in files {
            let mode = if *executable { 0o755 } else { 0o644 };
            add(format!("{chrome}/{arch}/{file}"), content, mode);
        }
        builder.into_inner().unwrap().finish().unwrap();
        let sums = dir.join(format!("{name}.sha256"));
        std::fs::write(
            &sums,
            format!("{}  {name}\n", crate::bundle::sha256_file(&path).unwrap()),
        )
        .unwrap();
        (path, sums)
    }

    pub(crate) const FILES: &[(&str, &[u8], bool)] = &[
        ("run", b"#!/bin/sh\n", true),
        ("headless-shell/headless-shell", b"chrome", true),
        ("lib/libnss3.so", b"nss", false),
        ("fonts/fonts.conf", b"<fontconfig/>", false),
    ];

    fn service(name: &str, bundle: Option<&str>) -> Service {
        Service {
            name: name.into(),
            bundle: bundle.map(PathBuf::from),
            port: 9222,
            ..Service::default()
        }
    }

    #[test]
    fn the_release_bundle_is_the_one_under_the_state_dir() {
        assert!(
            !CHROME_VERSION.is_empty(),
            "build.rs read no Chrome version"
        );
        assert!(CHROME_VERSION.split('.').count() == 4, "{CHROME_VERSION}");
        let release = service("browser", Some("release"));
        assert!(is_release(&release));
        let resolved = resolve(&release).unwrap();
        assert_eq!(
            resolved,
            managed_dir(
                &Config::state_dir(),
                CHROME_VERSION,
                crate::bundle::host_arch().unwrap()
            )
        );
        assert!(resolved.ends_with(format!(
            "bundles/browser/{CHROME_VERSION}/{}",
            crate::bundle::host_arch().unwrap()
        )));
        // An explicit path is its own.
        let explicit = service("browser", Some("/srv/browser/amd64"));
        assert!(!is_release(&explicit));
        assert_eq!(
            resolve(&explicit),
            Some(PathBuf::from("/srv/browser/amd64"))
        );
        assert_eq!(resolve(&service("pg", None)), None);
    }

    #[test]
    fn only_a_node_running_services_in_its_sandboxes_wants_it() {
        let mut cfg = Config {
            service: vec![service("browser", Some("release"))],
            ..Config::default()
        };
        assert_eq!(wanted(&cfg), cfg!(target_os = "linux"));
        cfg.runtime.podman = crate::config::PodmanRuntime::legacy();
        assert!(!wanted(&cfg));
        assert_eq!(missing(&cfg, false), None);
        cfg.runtime.podman.sandbox_protocol = true;
        cfg.runtime.podman.sandbox_services = true;
        assert_eq!(wanted(&cfg), cfg!(target_os = "linux"));
        cfg.runtime.kind = crate::config::RuntimeKind::Apple;
        assert!(wanted(&cfg));
        cfg.service = vec![service("browser", Some("/srv/browser"))];
        assert!(!wanted(&cfg), "an explicit bundle is not fetched");
    }

    /// A node that builds DigitalOcean's templates bakes the browser into
    /// them, so it fetches the bundle though its own sandboxes run none; its
    /// own checks do not ask for it.
    #[test]
    fn a_node_baking_digitalocean_templates_wants_it() {
        let mut cfg = Config {
            service: vec![service("browser", Some("release"))],
            ..Config::default()
        };
        cfg.digitalocean.credential = "do".into();
        cfg.digitalocean.registry = "registry.digitalocean.com/acme".into();
        assert!(!wanted(&cfg), "placement is off");
        cfg.placement.digitalocean = crate::provider::requirements::DoMode::Overflow;
        assert!(wanted(&cfg));
        assert_eq!(missing(&cfg, false), None);
        cfg.digitalocean.registry = String::new();
        cfg.digitalocean.image = format!("x@sha256:{}", "a".repeat(64));
        assert!(!wanted(&cfg), "an image of the operator's holds its own");
    }

    #[test]
    fn the_containerfile_and_the_build_name_the_same_chrome() {
        let containerfile = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../containers/browser/Containerfile"),
        )
        .unwrap();
        assert!(
            containerfile.contains(&format!(
                "FROM docker.io/chromedp/headless-shell:{CHROME_VERSION}@sha256:"
            )),
            "{CHROME_VERSION}"
        );
    }

    #[test]
    fn a_tree_that_strays_from_its_manifest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = tarball(dir.path(), "1.2.3.4", "amd64", FILES);
        let out = dir.path().join("out");
        unpack(&path, &out, "1.2.3.4", "amd64").unwrap();
        let root = out.join("1.2.3.4/amd64");
        let manifest = verify_dir(&root, "1.2.3.4", "amd64").unwrap();
        assert_eq!(manifest.run, "run");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &str| {
            std::fs::metadata(root.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("run"), 0o755);
        assert_eq!(mode("headless-shell/headless-shell"), 0o755);
        assert_eq!(mode("lib/libnss3.so"), 0o644);

        assert!(verify_dir(&root, "1.2.3.5", "amd64").is_err());
        assert!(verify_dir(&root, "1.2.3.4", "arm64").is_err());
        std::fs::write(root.join("lib/extra.so"), b"x").unwrap();
        assert!(verify_dir(&root, "1.2.3.4", "amd64")
            .unwrap_err()
            .to_string()
            .contains("lib/extra.so is not in the manifest"));
        std::fs::remove_file(root.join("lib/extra.so")).unwrap();
        std::os::unix::fs::symlink("/bin/sh", root.join("lib/link")).unwrap();
        assert!(verify_dir(&root, "1.2.3.4", "amd64")
            .unwrap_err()
            .to_string()
            .contains("neither a file nor a directory"));
        std::fs::remove_file(root.join("lib/link")).unwrap();
        std::fs::write(root.join("headless-shell/headless-shell"), b"swapped").unwrap();
        assert!(verify_dir(&root, "1.2.3.4", "amd64").is_err());

        // A tarball reaching outside its version and architecture.
        let other = tempfile::tempdir().unwrap();
        let (arm, _) = tarball(other.path(), "1.2.3.4", "arm64", FILES);
        assert!(unpack(&arm, &dir.path().join("out2"), "1.2.3.4", "amd64")
            .unwrap_err()
            .to_string()
            .contains("1.2.3.4/arm64"));
    }

    #[test]
    fn a_manifest_without_its_run_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = tarball(dir.path(), "1.2.3.4", "amd64", &FILES[1..]);
        let out = dir.path().join("out");
        unpack(&path, &out, "1.2.3.4", "amd64").unwrap();
        assert!(verify_dir(&out.join("1.2.3.4/amd64"), "1.2.3.4", "amd64")
            .unwrap_err()
            .to_string()
            .contains("its run"));
    }

    /// Serves `<release>/v<version>/` as a release server would.
    fn publish(release: &Path, version: &str, chrome: &str, files: &[(&str, &[u8], bool)]) {
        let dir = release.join(format!("v{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        tarball(&dir, chrome, "amd64", files);
    }

    #[tokio::test]
    async fn a_browser_bundle_is_fetched_checked_cached_and_superseded() {
        let release = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        publish(release.path(), "1.0.0", "150.0.0.1", FILES);
        // The next release, with a new Chrome, and one that kept the old.
        publish(release.path(), "1.1.0", "151.0.0.1", FILES);
        publish(release.path(), "1.2.0", "151.0.0.1", FILES);
        let (url, asked) = crate::bundle::tests::serve(release.path().to_path_buf()).await;
        let client = reqwest::Client::new();

        let first = ensure_at(&client, &url, state.path(), "1.0.0", "150.0.0.1", "amd64")
            .await
            .unwrap();
        assert!(matches!(first, Ensured::Fetched(_)), "{first:?}");
        assert_eq!(
            first.path(),
            managed_dir(state.path(), "150.0.0.1", "amd64")
        );
        assert_eq!(
            std::fs::read(first.path().join("run")).unwrap(),
            b"#!/bin/sh\n"
        );

        // The next start uses what it has.
        let again = ensure_at(&client, &url, state.path(), "1.0.0", "150.0.0.1", "amd64")
            .await
            .unwrap();
        assert_eq!(again, Ensured::Cached(first.path().to_path_buf()));
        assert_eq!(
            asked.lock().unwrap().len(),
            2,
            "{:?}",
            asked.lock().unwrap()
        );

        // An update with a new Chrome fetches it; the old one is superseded,
        // and the harness bundle's own supersession leaves them all alone.
        let updated = ensure_at(&client, &url, state.path(), "1.1.0", "151.0.0.1", "amd64")
            .await
            .unwrap();
        assert!(matches!(updated, Ensured::Fetched(_)));
        assert!(asked
            .lock()
            .unwrap()
            .contains(&"v1.1.0/tracon-browser-151.0.0.1-amd64.tar.gz".to_string()));
        assert_eq!(
            superseded(state.path(), "151.0.0.1"),
            vec![state.path().join("bundles/browser/150.0.0.1")]
        );
        assert!(crate::bundle::superseded(state.path(), "1.1.0").is_empty());

        // A node update that keeps its Chrome keeps the bundle it has.
        let kept = ensure_at(&client, &url, state.path(), "1.2.0", "151.0.0.1", "amd64")
            .await
            .unwrap();
        assert_eq!(kept, Ensured::Cached(updated.path().to_path_buf()));

        // One altered on disk is fetched again, not mounted.
        std::fs::write(kept.path().join("lib/libnss3.so"), b"changed").unwrap();
        let refetched = ensure_at(&client, &url, state.path(), "1.2.0", "151.0.0.1", "amd64")
            .await
            .unwrap();
        assert!(matches!(refetched, Ensured::Fetched(_)));
        assert_eq!(
            std::fs::read(refetched.path().join("lib/libnss3.so")).unwrap(),
            b"nss"
        );
    }

    #[tokio::test]
    async fn a_browser_bundle_under_a_wrong_sha256_is_refused() {
        let release = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        publish(release.path(), "2.0.0", "152.0.0.1", FILES);
        let dir = release.path().join("v2.0.0");
        let name = "tracon-browser-152.0.0.1-amd64.tar.gz";
        std::fs::write(
            dir.join(format!("{name}.sha256")),
            format!("{}  {name}\n", "0".repeat(64)),
        )
        .unwrap();
        let (url, _) = crate::bundle::tests::serve(release.path().to_path_buf()).await;
        let refused = ensure_at(
            &reqwest::Client::new(),
            &url,
            state.path(),
            "2.0.0",
            "152.0.0.1",
            "amd64",
        )
        .await
        .unwrap_err();
        assert!(matches!(refused, BundleError::Digest { .. }), "{refused}");
        assert!(!managed_dir(state.path(), "152.0.0.1", "amd64").exists());
        // Nothing of the attempt is left behind.
        let left: Vec<_> = std::fs::read_dir(state.path().join("bundles/browser"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
