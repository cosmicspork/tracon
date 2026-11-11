//! The harness bundle the node mounts into every sandbox: `tracon-sandbox`,
//! `tracon-agent` and Claude Code for one architecture, as
//! `scripts/harness-bundle.sh` assembles it and a release publishes it.
//!
//! On Podman and Apple, and Kubernetes over the sandbox protocol, with no
//! `[sandbox] bundle`, the node fetches the bundle of its own version and architecture from the
//! release, checks it, and keeps it under its state directory
//! (`bundles/<version>/<arch>`). The boundary's own converge does this at
//! start and whenever the definitions move, so a node that was updated mounts
//! the new release's bundle without anyone fetching it. A `[sandbox] bundle`
//! directory, for a node offline or running its own builds, always wins.
//!
//! What is checked, before anything is mounted:
//!
//! - the tarball against the `.sha256` published beside it;
//! - every entry of the tarball: a directory or a regular file under
//!   `<version>/<arch>/`, nothing else;
//! - the manifest: this version and architecture, every component's digest,
//!   every file in `bin/` named by it, and `tracon-sandbox` among them.
//!
//! A cached bundle is checked the same way at every start, so one that was
//! altered on disk is fetched again rather than mounted.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::config::Config;

/// Where in the state directory fetched bundles are kept.
pub const DIR: &str = "bundles";

/// How long one download may take: a bundle is a few hundred megabytes.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("no bundle is published for {0}")]
    UnsupportedArch(String),
    #[error("fetch {url}: {error}")]
    Fetch { url: String, error: String },
    #[error("the bundle {name} does not match its .sha256 ({got}, expected {expected})")]
    Digest {
        name: String,
        got: String,
        expected: String,
    },
    #[error("the bundle is malformed: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The architecture name a bundle is published under for this host.
pub fn host_arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// The bundle this node's sandboxes mount: `[sandbox] bundle` when set,
/// else the one the node fetches for itself (a pod-hosted node's init
/// containers copy it in). `None` on an architecture no release is built
/// for.
pub fn effective(cfg: &Config) -> Option<PathBuf> {
    if let Some(bundle) = &cfg.sandbox.bundle {
        return Some(bundle.clone());
    }
    host_arch().map(|arch| managed_dir(&Config::state_dir(), env!("CARGO_PKG_VERSION"), arch))
}

/// Whether the node fetches its bundle rather than being given one.
pub fn managed(cfg: &Config) -> bool {
    cfg.sandbox.bundle.is_none()
}

pub fn managed_dir(state_dir: &Path, version: &str, arch: &str) -> PathBuf {
    state_dir.join(DIR).join(version).join(arch)
}

/// What `manifest.json` says. Unknown fields are allowed: a later bundle may
/// say more about a component.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    pub arch: String,
    pub components: BTreeMap<String, ManifestComponent>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestComponent {
    pub path: String,
    pub version: String,
    pub sha256: String,
}

pub(crate) fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// A path inside a bundle: relative, and made of plain names only.
pub(crate) fn plain(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Check one unpacked `<version>/<arch>` directory against its manifest.
pub fn verify_dir(dir: &Path, version: &str, arch: &str) -> Result<Manifest, BundleError> {
    let bad = |why: String| BundleError::Malformed(format!("{}: {why}", dir.display()));
    let text = std::fs::read_to_string(dir.join("manifest.json"))
        .map_err(|e| bad(format!("manifest.json: {e}")))?;
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| bad(format!("manifest.json: {e}")))?;
    if manifest.schema != 1 {
        return Err(bad(format!("manifest schema {}", manifest.schema)));
    }
    if manifest.version != version || manifest.arch != arch {
        return Err(bad(format!(
            "the manifest is for {} {}, not {version} {arch}",
            manifest.version, manifest.arch
        )));
    }
    let mut listed = std::collections::BTreeSet::new();
    for (name, component) in &manifest.components {
        let path = Path::new(&component.path);
        if !plain(path) {
            return Err(bad(format!("{name} has the path {:?}", component.path)));
        }
        let file = dir.join(path);
        let meta = std::fs::symlink_metadata(&file)
            .map_err(|e| bad(format!("{}: {e}", component.path)))?;
        if !meta.file_type().is_file() {
            return Err(bad(format!("{} is not a regular file", component.path)));
        }
        let got = sha256_file(&file)?;
        if !got.eq_ignore_ascii_case(&component.sha256) {
            return Err(bad(format!(
                "{} has sha256 {got}, the manifest says {}",
                component.path, component.sha256
            )));
        }
        listed.insert(component.path.clone());
    }
    for entry in std::fs::read_dir(dir.join("bin")).map_err(|e| bad(format!("bin: {e}")))? {
        let entry = entry?;
        let path = format!("bin/{}", entry.file_name().to_string_lossy());
        if !listed.contains(&path) {
            return Err(bad(format!("{path} is not in the manifest")));
        }
    }
    if !manifest.components.contains_key("tracon-sandbox") {
        return Err(bad(
            "it has no tracon-sandbox (a release from before the sandbox protocol)".into(),
        ));
    }
    Ok(manifest)
}

/// The `.sha256` beside a tarball: its digest, held to the tarball's name
/// when it names one.
pub(crate) fn expected_digest(text: &str, name: &str) -> Result<String, BundleError> {
    let mut fields = text.split_whitespace();
    let digest = fields.next().unwrap_or_default();
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BundleError::Malformed(format!(
            "{name}.sha256 holds no digest"
        )));
    }
    if let Some(named) = fields.next() {
        if named.trim_start_matches('*') != name {
            return Err(BundleError::Malformed(format!(
                "{name}.sha256 is for {named}"
            )));
        }
    }
    Ok(digest.to_ascii_lowercase())
}

/// Unpack `tarball` into `into`, refusing anything but directories and
/// regular files under `<version>/<arch>/`.
fn unpack(tarball: &Path, into: &Path, version: &str, arch: &str) -> Result<(), BundleError> {
    use std::os::unix::fs::PermissionsExt as _;
    let root = Path::new(version).join(arch);
    let file = std::fs::File::open(tarball)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in archive
        .entries()
        .map_err(|e| BundleError::Malformed(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| BundleError::Malformed(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| BundleError::Malformed(e.to_string()))?
            .into_owned();
        let path = path.strip_prefix("./").unwrap_or(&path).to_path_buf();
        let kind = entry.header().entry_type();
        if kind.is_pax_local_extensions() || kind.is_pax_global_extensions() {
            continue;
        }
        if !plain(&path) || !(path.starts_with(&root) || root.starts_with(&path)) {
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
            let executable = path.parent() == Some(&root.join("bin"));
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

pub(crate) async fn download(
    client: &reqwest::Client,
    url: &str,
    to: &Path,
) -> Result<(), BundleError> {
    use tokio::io::AsyncWriteExt as _;
    let failed = |error: String| BundleError::Fetch {
        url: url.to_string(),
        error,
    };
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| failed(e.to_string()))?;
    let mut out = tokio::fs::File::create(to).await?;
    while let Some(chunk) = response.chunk().await.map_err(|e| failed(e.to_string()))? {
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    Ok(())
}

/// What [`ensure_at`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// The cached bundle checked out.
    Cached(PathBuf),
    /// It was fetched (again, if the cached one did not check out).
    Fetched(PathBuf),
}

impl Ensured {
    pub fn path(&self) -> &Path {
        match self {
            Ensured::Cached(path) | Ensured::Fetched(path) => path,
        }
    }
}

/// The bundle of `version` for `arch` under `state_dir`, fetched from
/// `release_url` unless the one there checks out.
pub async fn ensure_at(
    client: &reqwest::Client,
    release_url: &str,
    state_dir: &Path,
    version: &str,
    arch: &str,
) -> Result<Ensured, BundleError> {
    let dest = managed_dir(state_dir, version, arch);
    if dest.is_dir() {
        let check = dest.clone();
        let (v, a) = (version.to_string(), arch.to_string());
        match tokio::task::spawn_blocking(move || verify_dir(&check, &v, &a))
            .await
            .map_err(|e| BundleError::Malformed(e.to_string()))?
        {
            Ok(_) => return Ok(Ensured::Cached(dest)),
            Err(error) => {
                tracing::warn!(bundle = %dest.display(), %error, "the cached harness bundle does not check out; fetching it again")
            }
        }
    }
    let root = state_dir.join(DIR);
    std::fs::create_dir_all(&root)?;
    let incoming = tempfile::Builder::new()
        .prefix(".incoming-")
        .tempdir_in(&root)?;
    let name = format!("tracon-bundle-{version}-{arch}.tar.gz");
    let base = format!("{}/v{version}", release_url.trim_end_matches('/'));
    let tarball = incoming.path().join(&name);
    let sums = incoming.path().join(format!("{name}.sha256"));
    download(client, &format!("{base}/{name}.sha256"), &sums).await?;
    download(client, &format!("{base}/{name}"), &tarball).await?;
    let unpacked = incoming.path().join("unpacked");
    let (v, a) = (version.to_string(), arch.to_string());
    let checked = unpacked.clone();
    tokio::task::spawn_blocking(move || -> Result<(), BundleError> {
        let expected = expected_digest(&std::fs::read_to_string(&sums)?, &name)?;
        let got = sha256_file(&tarball)?;
        if got != expected {
            return Err(BundleError::Digest {
                name,
                got,
                expected,
            });
        }
        unpack(&tarball, &checked, &v, &a)?;
        verify_dir(&checked.join(&v).join(&a), &v, &a).map(|_| ())
    })
    .await
    .map_err(|e| BundleError::Malformed(e.to_string()))??;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::rename(unpacked.join(version).join(arch), &dest)?;
    Ok(Ensured::Fetched(dest))
}

/// Every fetched bundle but `keep_version`'s: what a node of this version
/// will never mount again. A sandbox does not outlive the node process that
/// started it, so none is still using one. The service bundles beside them
/// are superseded on their own (`crate::service_bundle`).
pub fn superseded(state_dir: &Path, keep_version: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(state_dir.join(DIR)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy() != keep_version)
        .filter(|entry| entry.file_name() != crate::service_bundle::DIR)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect()
}

/// The node's own bundle, fetched if it has to be, and the bundles of other
/// versions removed. `Ok(None)` when the node is given its bundle.
pub async fn ensure(cfg: &Config) -> Result<Option<Ensured>, BundleError> {
    if !managed(cfg) {
        return Ok(None);
    }
    let ensured = ensure_release(cfg).await?;
    let state = Config::state_dir();
    let version = env!("CARGO_PKG_VERSION");
    for old in superseded(&state, version) {
        match std::fs::remove_dir_all(&old) {
            Ok(()) => {
                tracing::info!(bundle = %old.display(), "removed a harness bundle of another version")
            }
            Err(error) => {
                tracing::warn!(bundle = %old.display(), %error, "could not remove an old harness bundle")
            }
        }
    }
    Ok(Some(ensured))
}

/// This release's bundle for this host, fetched into the node's own place
/// for it if it has to be, whether or not the node's sandboxes run it yet:
/// what `tracon sandbox probe` runs when it is given no bundle.
pub async fn ensure_release(cfg: &Config) -> Result<Ensured, BundleError> {
    let arch =
        host_arch().ok_or_else(|| BundleError::UnsupportedArch(std::env::consts::ARCH.into()))?;
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| BundleError::Fetch {
            url: cfg.sandbox.release_url(),
            error: e.to_string(),
        })?;
    let version = env!("CARGO_PKG_VERSION");
    ensure_at(
        &client,
        &cfg.sandbox.release_url(),
        &Config::state_dir(),
        version,
        arch,
    )
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A bundle as `scripts/harness-bundle.sh` lays it out, as a tarball and
    /// its `.sha256`, built from `files` (name → content) under `bin/`.
    pub(crate) fn tarball(
        dir: &Path,
        version: &str,
        arch: &str,
        files: &[(&str, &[u8])],
    ) -> (PathBuf, PathBuf) {
        let mut components = serde_json::Map::new();
        for (name, content) in files {
            components.insert(
                name.to_string(),
                serde_json::json!({
                    "path": format!("bin/{name}"),
                    "version": version,
                    "sha256": hex::encode(Sha256::digest(content)),
                }),
            );
        }
        let manifest = serde_json::to_vec_pretty(&serde_json::json!({
            "schema": 1, "version": version, "arch": arch, "components": components,
        }))
        .unwrap();
        let name = format!("tracon-bundle-{version}-{arch}.tar.gz");
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
        add(format!("{version}/{arch}/manifest.json"), &manifest, 0o644);
        for (name, content) in files {
            add(format!("{version}/{arch}/bin/{name}"), content, 0o755);
        }
        builder.into_inner().unwrap().finish().unwrap();
        let sums = dir.join(format!("{name}.sha256"));
        std::fs::write(&sums, format!("{}  {name}\n", sha256_file(&path).unwrap())).unwrap();
        (path, sums)
    }

    pub(crate) const FILES: &[(&str, &[u8])] = &[
        ("tracon-sandbox", b"sandbox"),
        ("tracon-agent", b"agent"),
        ("claude", b"claude"),
    ];

    /// Serves `dir` as a release server would: `/v<version>/<file>`.
    pub(crate) async fn serve(
        dir: PathBuf,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::Path as AxPath;
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let app = axum::Router::new().route(
            "/{tag}/{file}",
            axum::routing::get(move |AxPath((tag, file)): AxPath<(String, String)>| {
                let dir = dir.clone();
                let seen = seen.clone();
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

    fn publish(release: &Path, version: &str, files: &[(&str, &[u8])]) {
        let dir = release.join(format!("v{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        tarball(&dir, version, "amd64", files);
    }

    #[tokio::test]
    async fn a_bundle_is_fetched_checked_cached_and_fetched_again_for_a_new_version() {
        let release = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        publish(release.path(), "1.0.0", FILES);
        publish(release.path(), "1.1.0", FILES);
        let (url, asked) = serve(release.path().to_path_buf()).await;
        let client = reqwest::Client::new();

        let first = ensure_at(&client, &url, state.path(), "1.0.0", "amd64")
            .await
            .unwrap();
        assert!(matches!(first, Ensured::Fetched(_)), "{first:?}");
        let dir = first.path().to_path_buf();
        assert_eq!(dir, managed_dir(state.path(), "1.0.0", "amd64"));
        assert_eq!(
            std::fs::read(dir.join("bin/tracon-sandbox")).unwrap(),
            b"sandbox"
        );
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.join("bin/claude"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);

        // A restart of the same version uses what it has.
        let again = ensure_at(&client, &url, state.path(), "1.0.0", "amd64")
            .await
            .unwrap();
        assert_eq!(again, Ensured::Cached(dir.clone()));
        assert_eq!(
            asked.lock().unwrap().len(),
            2,
            "{:?}",
            asked.lock().unwrap()
        );

        // An update: the new version's bundle, with nobody asking for it,
        // and the old one is then superseded.
        let updated = ensure_at(&client, &url, state.path(), "1.1.0", "amd64")
            .await
            .unwrap();
        assert!(matches!(updated, Ensured::Fetched(_)));
        assert!(asked
            .lock()
            .unwrap()
            .contains(&"v1.1.0/tracon-bundle-1.1.0-amd64.tar.gz".to_string()));
        assert_eq!(
            superseded(state.path(), "1.1.0"),
            vec![state.path().join(DIR).join("1.0.0")]
        );

        // A cached bundle altered on disk is fetched again, not mounted.
        std::fs::write(updated.path().join("bin/claude"), b"changed").unwrap();
        let refetched = ensure_at(&client, &url, state.path(), "1.1.0", "amd64")
            .await
            .unwrap();
        assert!(matches!(refetched, Ensured::Fetched(_)));
        assert_eq!(
            std::fs::read(refetched.path().join("bin/claude")).unwrap(),
            b"claude"
        );
    }

    #[tokio::test]
    async fn a_tampered_bundle_is_refused_and_nothing_is_mounted() {
        let release = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        publish(release.path(), "2.0.0", FILES);
        let dir = release.path().join("v2.0.0");
        // A tarball swapped under its .sha256.
        let name = "tracon-bundle-2.0.0-amd64.tar.gz";
        let sums = std::fs::read_to_string(dir.join(format!("{name}.sha256"))).unwrap();
        let other = tempfile::tempdir().unwrap();
        tarball(
            other.path(),
            "2.0.0",
            "amd64",
            &[
                ("tracon-sandbox", b"evil"),
                ("tracon-agent", b"a"),
                ("claude", b"c"),
            ],
        );
        std::fs::copy(other.path().join(name), dir.join(name)).unwrap();
        std::fs::write(dir.join(format!("{name}.sha256")), sums).unwrap();
        let (url, _) = serve(release.path().to_path_buf()).await;
        let refused = ensure_at(
            &reqwest::Client::new(),
            &url,
            state.path(),
            "2.0.0",
            "amd64",
        )
        .await
        .unwrap_err();
        assert!(matches!(refused, BundleError::Digest { .. }), "{refused}");
        assert!(!managed_dir(state.path(), "2.0.0", "amd64").exists());
    }

    #[tokio::test]
    async fn a_bundle_whose_manifest_lies_or_lacks_the_supervisor_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let release = tempfile::tempdir().unwrap();
        // Published consistently, but from before the supervisor shipped.
        publish(
            release.path(),
            "0.44.0",
            &[("tracon-agent", b"agent"), ("claude", b"claude")],
        );
        let (url, _) = serve(release.path().to_path_buf()).await;
        let client = reqwest::Client::new();
        let refused = ensure_at(&client, &url, state.path(), "0.44.0", "amd64")
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("no tracon-sandbox"),
            "{refused}"
        );
        // A version the release has no bundle for.
        let missing = ensure_at(&client, &url, state.path(), "9.9.9", "amd64")
            .await
            .unwrap_err();
        assert!(matches!(missing, BundleError::Fetch { .. }), "{missing}");

        // An unpacked bundle whose file does not match its manifest, or with
        // a file the manifest does not name.
        let dir = tempfile::tempdir().unwrap();
        let (tarball_path, _) = tarball(dir.path(), "3.0.0", "amd64", FILES);
        let out = dir.path().join("out");
        unpack(&tarball_path, &out, "3.0.0", "amd64").unwrap();
        let root = out.join("3.0.0/amd64");
        verify_dir(&root, "3.0.0", "amd64").unwrap();
        assert!(verify_dir(&root, "3.0.1", "amd64").is_err());
        std::fs::write(root.join("bin/extra"), b"x").unwrap();
        assert!(verify_dir(&root, "3.0.0", "amd64")
            .unwrap_err()
            .to_string()
            .contains("bin/extra is not in the manifest"));
        std::fs::remove_file(root.join("bin/extra")).unwrap();
        std::fs::write(root.join("bin/tracon-agent"), b"swapped").unwrap();
        assert!(verify_dir(&root, "3.0.0", "amd64").is_err());
    }

    #[test]
    fn a_tarball_reaching_outside_its_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_ustar();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, "1.0.0/arm64/x", &b"x"[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let refused = unpack(&path, &dir.path().join("out"), "1.0.0", "amd64").unwrap_err();
        assert!(refused.to_string().contains("1.0.0/arm64/x"), "{refused}");

        let link = dir.path().join("l.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&link).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_ustar();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Symlink);
        builder
            .append_link(&mut header, "1.0.0/amd64/bin/claude", "/bin/sh")
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        assert!(unpack(&link, &dir.path().join("out2"), "1.0.0", "amd64").is_err());
    }

    #[test]
    fn the_sha256_file_must_name_the_tarball() {
        let digest = "a".repeat(64);
        assert_eq!(
            expected_digest(&format!("{digest}  t.tar.gz\n"), "t.tar.gz").unwrap(),
            digest
        );
        assert!(expected_digest(&format!("{digest}  other.tar.gz"), "t.tar.gz").is_err());
        assert!(expected_digest("nope", "t.tar.gz").is_err());
    }

    #[test]
    fn a_node_fetches_its_bundle_unless_given_one() {
        assert!(managed(&Config::default()));
        let mut kube = Config::default();
        kube.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        assert!(managed(&kube));
        let mut cfg = Config::default();
        cfg.sandbox.bundle = Some("/opt/bundle".into());
        // An explicit bundle wins over fetching one.
        assert!(!managed(&cfg));
        assert_eq!(effective(&cfg), Some(PathBuf::from("/opt/bundle")));
        cfg.sandbox.bundle = None;
        assert!(managed(&cfg));
        let Some(arch) = host_arch() else {
            return;
        };
        let managed = effective(&cfg).unwrap();
        assert!(managed.ends_with(format!("{DIR}/{}/{arch}", env!("CARGO_PKG_VERSION"))));
    }
}
