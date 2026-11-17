//! Fetching, checking and staging a plugin's archive: from the latest
//! GitHub release of an allowed owner's repository, or from an archive the
//! operator hands the node.
//!
//! What is checked before anything is staged:
//!
//! - the owner, against `[plugins] owners`, before any request is made;
//! - the archive, against its line in the release's `checksums.txt`;
//! - every entry of the archive: a directory or a regular file with a plain
//!   path, `tracon-plugin.toml`, `bin/` and `ui/` at the top and nothing
//!   else, within bounds on entries and size;
//! - the manifest (`manifest::Manifest::validate`), the tree it names, and
//!   that the archive is named `<name>-<version>-<target>.tar.gz` as it says.
//!
//! A checked build is kept at `<state>/plugins/bin/<name>/<version>-<digest>/`,
//! where only the node writes, with the archive's digest and the tree's
//! pinned in `node.db` (`store::plugins`). Nothing here activates it.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::manifest::{self, Manifest};
use crate::bundle::{self, BundleError, Limits};
use crate::store::plugins::Build;

/// Where in the state directory plugins are kept.
pub const DIR: &str = "plugins";

/// The GitHub API the node asks for a repository's latest release.
pub const GITHUB_API: &str = "https://api.github.com";

/// The asset beside a release's archives that gives their digests.
pub const CHECKSUMS: &str = "checksums.txt";

/// The most an archive the operator uploads may be.
pub const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

/// The most one archive may unpack to.
const LIMITS: Limits = Limits {
    entries: 10_000,
    bytes: 1 << 30,
};

/// What may sit at the top of a plugin archive.
const TOP_LEVEL: &[&str] = &[manifest::FILE, "bin", "ui"];

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The node will not install it, and says why.
    #[error("{0}")]
    Refused(String),
    /// A release could not be read.
    #[error("{0}")]
    Fetch(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
}

impl From<BundleError> for InstallError {
    fn from(error: BundleError) -> Self {
        match error {
            BundleError::Fetch { .. } => InstallError::Fetch(error.to_string()),
            BundleError::Io(e) => InstallError::Io(e),
            BundleError::UnsupportedArch(_)
            | BundleError::Digest { .. }
            | BundleError::Malformed(_) => InstallError::Refused(error.to_string()),
        }
    }
}

fn refused(why: impl Into<String>) -> InstallError {
    InstallError::Refused(why.into())
}

/// The target a release publishes this host's archive for.
pub fn target() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-gnu"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

fn host_target() -> Result<&'static str, InstallError> {
    target().ok_or_else(|| {
        refused(format!(
            "no plugin target for this host ({} {})",
            std::env::consts::ARCH,
            std::env::consts::OS
        ))
    })
}

pub fn root(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR)
}

/// Where builds are kept, each plugin's under its name.
pub fn bin_dir(state_dir: &Path) -> PathBuf {
    root(state_dir).join("bin")
}

/// Where a plugin keeps its data, by channel. Kept when it is removed
/// unless purged.
pub fn data_dir(state_dir: &Path, name: &str) -> PathBuf {
    root(state_dir).join("data").join(name)
}

/// Where one build of `name` is kept.
pub fn build_dir(state_dir: &Path, name: &str, version: &str, digest: &str) -> PathBuf {
    bin_dir(state_dir)
        .join(name)
        .join(format!("{version}-{}", &digest[..16.min(digest.len())]))
}

/// `owner/repo`, as GitHub spells them.
pub fn parse_repo(spec: &str) -> Result<(&str, &str), InstallError> {
    let bad = || refused(format!("{spec:?} is not owner/repo"));
    let (owner, repo) = spec.split_once('/').ok_or_else(bad)?;
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let repo_ok = !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !owner_ok || !repo_ok {
        return Err(bad());
    }
    Ok((owner, repo))
}

/// One release, as the node reads it.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub body: String,
    /// Each asset's name and where to download it.
    pub assets: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// Where releases are read from: GitHub's API, unauthenticated.
#[derive(Clone)]
pub struct Releases {
    client: reqwest::Client,
    api: String,
}

impl Releases {
    pub fn github() -> Result<Self, InstallError> {
        Self::at(GITHUB_API)
    }

    /// Releases read from `api` (a stand-in, in tests).
    pub fn at(api: &str) -> Result<Self, InstallError> {
        let client = reqwest::Client::builder()
            .timeout(bundle::FETCH_TIMEOUT)
            .user_agent("tracon")
            .build()
            .map_err(|e| InstallError::Fetch(e.to_string()))?;
        Ok(Self {
            client,
            api: api.trim_end_matches('/').to_string(),
        })
    }

    pub async fn latest(&self, owner: &str, repo: &str) -> Result<Release, InstallError> {
        let url = format!("{}/repos/{owner}/{repo}/releases/latest", self.api);
        let failed = |e: String| InstallError::Fetch(format!("{url}: {e}"));
        let response = self
            .client
            .get(&url)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| failed(e.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(refused(format!("{owner}/{repo} has no published release")));
        }
        let release: GithubRelease = response
            .error_for_status()
            .map_err(|e| failed(e.to_string()))?
            .json()
            .await
            .map_err(|e| failed(e.to_string()))?;
        Ok(Release {
            tag: release.tag_name,
            body: release.body.unwrap_or_default(),
            assets: release
                .assets
                .into_iter()
                .map(|a| (a.name, a.browser_download_url))
                .collect(),
        })
    }
}

/// A checked build, kept where it will run from, and what the card shows of
/// where it came from.
#[derive(Debug, Clone)]
pub struct Staged {
    pub build: Build,
    pub dir: PathBuf,
    /// `owner/repo`, or `local:<file name>`.
    pub source: String,
    pub tag: Option<String>,
    pub changelog: Option<String>,
}

/// The incoming area under the plugins directory, made private first.
fn incoming(state_dir: &Path) -> Result<tempfile::TempDir, InstallError> {
    let root = root(state_dir);
    private_dir(&root)?;
    private_dir(&bin_dir(state_dir))?;
    Ok(tempfile::Builder::new()
        .prefix(".incoming-")
        .tempdir_in(&root)?)
}

/// `dir`, made if it has to be, readable by the node's user alone.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Fetch, check and stage the latest release of `spec` (`owner/repo`).
/// The owner is checked against `owners` before anything is asked.
pub async fn fetch(
    releases: &Releases,
    state_dir: &Path,
    owners: &[String],
    spec: &str,
) -> Result<Staged, InstallError> {
    let (owner, repo) = parse_repo(spec)?;
    if !owners.iter().any(|o| o.eq_ignore_ascii_case(owner)) {
        return Err(refused(format!(
            "{owner} is not an allowed plugin owner ([plugins] owners)"
        )));
    }
    let target = host_target()?;
    let release = releases.latest(owner, repo).await?;
    let suffix = format!("-{target}.tar.gz");
    let archives: Vec<&(String, String)> = release
        .assets
        .iter()
        .filter(|(name, _)| name.ends_with(&suffix))
        .collect();
    let (asset, url) = match archives.as_slice() {
        [one] => (one.0.clone(), one.1.clone()),
        [] => {
            return Err(refused(format!(
                "{spec} {} publishes no archive for {target}",
                release.tag
            )))
        }
        _ => {
            return Err(refused(format!(
                "{spec} {} publishes more than one archive for {target}",
                release.tag
            )))
        }
    };
    let sums_url = release
        .assets
        .iter()
        .find(|(name, _)| name == CHECKSUMS)
        .map(|(_, url)| url.clone())
        .ok_or_else(|| refused(format!("{spec} {} publishes no {CHECKSUMS}", release.tag)))?;
    let incoming = incoming(state_dir)?;
    let tarball = incoming.path().join("archive.tar.gz");
    let sums = incoming.path().join(CHECKSUMS);
    bundle::download(&releases.client, &sums_url, &sums).await?;
    bundle::download(&releases.client, &url, &tarball).await?;
    let state = state_dir.to_path_buf();
    let (build, dir) = tokio::task::spawn_blocking(move || {
        let expected = bundle::digest_in_checksums(&std::fs::read_to_string(&sums)?, &asset)?;
        let got = bundle::sha256_file(&tarball)?;
        if got != expected {
            return Err(InstallError::from(BundleError::Digest {
                name: asset,
                got,
                expected,
            }));
        }
        let staged = stage_tarball(&state, incoming.path(), &tarball, &got, &asset, target);
        drop(incoming);
        staged
    })
    .await
    .map_err(|e| InstallError::Io(std::io::Error::other(e.to_string())))??;
    Ok(Staged {
        build,
        dir,
        source: format!("{owner}/{repo}"),
        tag: Some(release.tag),
        changelog: Some(release.body).filter(|b| !b.trim().is_empty()),
    })
}

/// Check and stage an archive the operator handed the node, named
/// `file_name`. Its own digest is what is pinned; no owner is asked about,
/// since the operator is its source. Blocking.
pub fn stage_local(
    state_dir: &Path,
    bytes: &[u8],
    file_name: &str,
) -> Result<Staged, InstallError> {
    let name = Path::new(file_name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| bundle::plain(Path::new(n)))
        .ok_or_else(|| refused(format!("{file_name:?} is not an archive's file name")))?
        .to_string();
    let target = host_target()?;
    let incoming = incoming(state_dir)?;
    let tarball = incoming.path().join("archive.tar.gz");
    std::fs::write(&tarball, bytes)?;
    let digest = bundle::sha256_file(&tarball)?;
    let (build, dir) = stage_tarball(state_dir, incoming.path(), &tarball, &digest, &name, target)?;
    Ok(Staged {
        build,
        dir,
        source: format!("local:{name}"),
        tag: None,
        changelog: None,
    })
}

/// Unpack and check `tarball` (whose digest is `digest`, published as
/// `asset`) in `incoming`, then move it to where its build is kept.
fn stage_tarball(
    state_dir: &Path,
    incoming: &Path,
    tarball: &Path,
    digest: &str,
    asset: &str,
    target: &str,
) -> Result<(Build, PathBuf), InstallError> {
    let unpacked = incoming.join("unpacked");
    std::fs::create_dir(&unpacked)?;
    bundle::unpack_tree(
        tarball,
        &unpacked,
        Path::new(""),
        |path| path.starts_with("bin"),
        LIMITS,
    )?;
    for entry in std::fs::read_dir(&unpacked)? {
        let name = entry?.file_name();
        if !TOP_LEVEL.iter().any(|allowed| name == *allowed) {
            return Err(refused(format!(
                "the archive holds {} at its top; only {}, bin/ and ui/ belong there",
                name.to_string_lossy(),
                manifest::FILE
            )));
        }
    }
    let text = match std::fs::symlink_metadata(unpacked.join(manifest::FILE)) {
        Ok(meta) if meta.is_file() => std::fs::read_to_string(unpacked.join(manifest::FILE))
            .map_err(|e| refused(format!("{}: {e}", manifest::FILE)))?,
        _ => return Err(refused(format!("the archive holds no {}", manifest::FILE))),
    };
    let manifest = Manifest::parse(&text).map_err(InstallError::Refused)?;
    manifest
        .validate_tree(&unpacked)
        .map_err(InstallError::Refused)?;
    let expected = manifest.asset_name(target);
    if asset != expected {
        return Err(refused(format!(
            "the archive is named {asset}, and its manifest says {expected}"
        )));
    }
    let tree_digest = tree_digest(&unpacked).map_err(InstallError::Refused)?;
    let dest = build_dir(state_dir, &manifest.name, &manifest.version, digest);
    if dest.exists() {
        // The same archive, staged before: kept as it is if it still is
        // what was checked, and replaced if not.
        if tree_digest_of(&dest).as_deref() == Some(tree_digest.as_str()) {
            return Ok((
                Build {
                    version: manifest.version.clone(),
                    digest: digest.to_string(),
                    tree_digest,
                    manifest,
                },
                dest,
            ));
        }
        std::fs::remove_dir_all(&dest)?;
    }
    if let Some(parent) = dest.parent() {
        private_dir(parent)?;
    }
    std::fs::rename(&unpacked, &dest)?;
    Ok((
        Build {
            version: manifest.version.clone(),
            digest: digest.to_string(),
            tree_digest,
            manifest,
        },
        dest,
    ))
}

/// The digest of a staged tree: sha256 over each file's path, content
/// digest and whether it is executable, in path order. Anything but files
/// and directories is refused.
pub fn tree_digest(dir: &Path) -> Result<String, String> {
    use sha2::{Digest as _, Sha256};
    use std::os::unix::fs::PermissionsExt as _;
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let entries = std::fs::read_dir(dir.join(&relative))
            .map_err(|e| format!("{}: {e}", dir.join(&relative).display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = relative.join(entry.file_name());
            let meta = std::fs::symlink_metadata(dir.join(&path))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                files.push((path, meta.permissions().mode() & 0o111 != 0));
            } else {
                return Err(format!(
                    "{} is neither a file nor a directory",
                    path.display()
                ));
            }
        }
    }
    files.sort();
    let mut hash = Sha256::new();
    for (path, executable) in files {
        let digest = bundle::sha256_file(&dir.join(&path))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        hash.update(path.as_os_str().as_encoded_bytes());
        hash.update(format!("\0{digest}\0{}\n", u8::from(executable)));
    }
    Ok(hex::encode(hash.finalize()))
}

fn tree_digest_of(dir: &Path) -> Option<String> {
    tree_digest(dir).ok()
}

/// Whether the build kept for `build` is still what was checked.
pub fn verify(state_dir: &Path, name: &str, build: &Build) -> Result<(), String> {
    let dir = build_dir(state_dir, name, &build.version, &build.digest);
    match tree_digest(&dir) {
        Ok(digest) if digest == build.tree_digest => Ok(()),
        Ok(_) => Err(format!(
            "{} changed on disk since it was checked",
            dir.display()
        )),
        Err(error) => Err(error),
    }
}

/// Remove every build of `name` but those in `keep`, and the plugin's
/// directory when none is kept.
pub fn prune(state_dir: &Path, name: &str, keep: &[&Build]) -> std::io::Result<()> {
    let dir = bin_dir(state_dir).join(name);
    let kept: Vec<PathBuf> = keep
        .iter()
        .map(|b| build_dir(state_dir, name, &b.version, &b.digest))
        .collect();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !kept.contains(&path) {
            match entry.file_type()?.is_dir() {
                true => std::fs::remove_dir_all(&path)?,
                false => std::fs::remove_file(&path)?,
            }
        }
    }
    if kept.is_empty() {
        std::fs::remove_dir(&dir).or_else(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        })?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A plugin archive as a release ships it: the manifest, `bin/<name>`
    /// executable, `ui/index.html` when it has a UI, and `extra` entries
    /// (path, content, mode) besides.
    pub(crate) fn archive(manifest: &str, extra: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let parsed: toml::Table = manifest.parse().unwrap();
        let program = parsed["exec"][0].as_str().unwrap().to_string();
        let mut entries: Vec<(String, Vec<u8>, u32)> = vec![
            (manifest::FILE.into(), manifest.as_bytes().to_vec(), 0o644),
            (program, b"#!/bin/sh\nexit 0\n".to_vec(), 0o755),
        ];
        if let Some(ui) = parsed.get("ui") {
            entries.push((
                ui["entry"].as_str().unwrap().into(),
                b"<p>ui</p>".to_vec(),
                0o644,
            ));
        }
        for (path, content, mode) in extra {
            entries.push((path.to_string(), content.to_vec(), *mode));
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut gz);
            for (path, content, mode) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(mode);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_path(&path).unwrap_or_else(|_| {
                    // A hostile path the builder will not set: written raw.
                    let name = &mut header.as_old_mut().name;
                    name[..path.len()].copy_from_slice(path.as_bytes());
                });
                header.set_cksum();
                builder.append(&header, content.as_slice()).unwrap();
            }
            builder.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    /// The contract example as `name` at `version`, with `formats`.
    pub(crate) fn manifest(name: &str, version: &str, formats: &[&str]) -> String {
        let formats: Vec<String> = formats.iter().map(|f| format!("{f:?}")).collect();
        manifest::tests::EXAMPLE
            .replace("name = \"sql-review\"", &format!("name = {name:?}"))
            .replace("version = \"0.1.0\"", &format!("version = {version:?}"))
            .replace(
                "contract = 1\n",
                &format!("contract = 1\nformats = [{}]\n", formats.join(", ")),
            )
            .replace("bin/sql-review", &format!("bin/{name}"))
    }

    pub(crate) fn asset(name: &str, version: &str) -> String {
        format!("{name}-{version}-{}.tar.gz", target().unwrap())
    }

    /// A release as GitHub's API would describe it, and its assets, served
    /// from memory: `releases[(owner/repo)] = (tag, body, assets)`. Every
    /// request is recorded.
    pub(crate) type Published =
        std::collections::BTreeMap<String, (String, String, Vec<(String, Vec<u8>)>)>;

    pub(crate) async fn serve(
        published: std::sync::Arc<std::sync::Mutex<Published>>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::extract::{Path as AxPath, State};
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        type Shared = (
            std::sync::Arc<std::sync::Mutex<Published>>,
            std::sync::Arc<std::sync::Mutex<Vec<String>>>,
            String,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state: Shared = (published, asked.clone(), base.clone());
        let app = axum::Router::new()
            .route(
                "/repos/{owner}/{repo}/releases/latest",
                axum::routing::get(
                    |State((published, asked, base)): State<Shared>,
                     AxPath((owner, repo)): AxPath<(String, String)>| async move {
                        let spec = format!("{owner}/{repo}");
                        asked.lock().unwrap().push(format!("latest {spec}"));
                        let published = published.lock().unwrap();
                        let Some((tag, body, assets)) = published.get(&spec) else {
                            return (axum::http::StatusCode::NOT_FOUND, String::new());
                        };
                        let assets: Vec<serde_json::Value> = assets
                            .iter()
                            .map(|(name, _)| {
                                serde_json::json!({
                                    "name": name,
                                    "browser_download_url": format!("{base}/download/{owner}/{repo}/{name}"),
                                })
                            })
                            .collect();
                        (
                            axum::http::StatusCode::OK,
                            serde_json::json!({ "tag_name": tag, "body": body, "assets": assets })
                                .to_string(),
                        )
                    },
                ),
            )
            .route(
                "/download/{owner}/{repo}/{name}",
                axum::routing::get(
                    |State((published, asked, _)): State<Shared>,
                     AxPath((owner, repo, name)): AxPath<(String, String, String)>| async move {
                        asked.lock().unwrap().push(format!("download {name}"));
                        let published = published.lock().unwrap();
                        match published
                            .get(&format!("{owner}/{repo}"))
                            .and_then(|(_, _, assets)| assets.iter().find(|(n, _)| *n == name))
                        {
                            Some((_, bytes)) => (axum::http::StatusCode::OK, bytes.clone()),
                            None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                        }
                    },
                ),
            )
            .with_state(state);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, asked)
    }

    /// `archive` published as `name`'s release `version`, with a
    /// `checksums.txt` that lists it.
    pub(crate) fn release(name: &str, version: &str, archive: Vec<u8>) -> Vec<(String, Vec<u8>)> {
        use sha2::Digest as _;
        let asset = asset(name, version);
        let sums = format!("{}  {asset}\n", hex::encode(sha2::Sha256::digest(&archive)));
        vec![(asset, archive), (CHECKSUMS.into(), sums.into_bytes())]
    }

    pub(crate) fn owners() -> Vec<String> {
        vec!["cosmicspork".into()]
    }

    #[tokio::test]
    async fn a_release_is_fetched_checked_and_staged_by_its_digest() {
        if target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let archive = archive(&manifest("sql-review", "0.1.0", &[]), &[]);
        let published = std::sync::Arc::new(std::sync::Mutex::new(Published::from([(
            "cosmicspork/sql-review".to_string(),
            (
                "v0.1.0".to_string(),
                "First.".to_string(),
                release("sql-review", "0.1.0", archive.clone()),
            ),
        )])));
        let (api, _) = serve(published).await;
        let releases = Releases::at(&api).unwrap();
        let staged = fetch(&releases, state.path(), &owners(), "cosmicspork/sql-review")
            .await
            .unwrap();
        use sha2::Digest as _;
        let digest = hex::encode(sha2::Sha256::digest(&archive));
        assert_eq!(staged.build.digest, digest);
        assert_eq!(staged.source, "cosmicspork/sql-review");
        assert_eq!(staged.tag.as_deref(), Some("v0.1.0"));
        assert_eq!(staged.changelog.as_deref(), Some("First."));
        assert_eq!(
            staged.dir,
            state
                .path()
                .join(format!("plugins/bin/sql-review/0.1.0-{}", &digest[..16]))
        );
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&staged.dir.join("bin/sql-review")), 0o755);
        assert_eq!(mode(&staged.dir.join("ui/index.html")), 0o644);
        assert_eq!(mode(&state.path().join("plugins")), 0o700);
        assert_eq!(mode(&state.path().join("plugins/bin")), 0o700);
        verify(state.path(), "sql-review", &staged.build).unwrap();
        // Nothing of the attempt is left beside it.
        let left: Vec<_> = std::fs::read_dir(state.path().join("plugins"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["bin"]);

        // A tampered tree no longer verifies.
        std::fs::write(staged.dir.join("ui/index.html"), b"<p>changed</p>").unwrap();
        assert!(verify(state.path(), "sql-review", &staged.build)
            .unwrap_err()
            .contains("changed on disk"));
    }

    #[tokio::test]
    async fn an_owner_outside_the_allowlist_is_refused_before_any_request() {
        let state = tempfile::tempdir().unwrap();
        let published = std::sync::Arc::new(std::sync::Mutex::new(Published::new()));
        let (api, asked) = serve(published).await;
        let releases = Releases::at(&api).unwrap();
        let refused = fetch(&releases, state.path(), &owners(), "evil/plugin")
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("evil is not an allowed plugin owner ([plugins] owners)"),
            "{refused}"
        );
        assert!(asked.lock().unwrap().is_empty());
        assert!(!state.path().join("plugins").exists());
        for bad in ["nope", "a/b/c", "a/..", "/x"] {
            assert!(fetch(&releases, state.path(), &owners(), bad)
                .await
                .is_err());
        }
        assert!(asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_release_that_does_not_add_up_is_refused_and_nothing_is_staged() {
        if target().is_none() {
            return;
        }
        let good = archive(&manifest("sql-review", "0.1.0", &[]), &[]);
        let other = archive(&manifest("sql-review", "0.1.0", &["x"]), &[]);
        let mut wrong_digest = release("sql-review", "0.1.0", good.clone());
        wrong_digest[0].1 = other;
        let mut no_sums = release("sql-review", "0.1.0", good.clone());
        no_sums.pop();
        let mut two = release("sql-review", "0.1.0", good.clone());
        two.push((asset("sql-review-extra", "0.1.0"), good.clone()));
        let renamed: Vec<(String, Vec<u8>)> = release("other", "0.1.0", good.clone());
        type Assets = Vec<(String, Vec<u8>)>;
        let cases: Vec<(Assets, &str)> = vec![
            (wrong_digest, "does not match"),
            (no_sums, "publishes no checksums.txt"),
            (two, "more than one archive"),
            (vec![(CHECKSUMS.into(), Vec::new())], "publishes no archive"),
            (renamed, "its manifest says sql-review-0.1.0-"),
        ];
        for (assets, says) in cases {
            let state = tempfile::tempdir().unwrap();
            let published = std::sync::Arc::new(std::sync::Mutex::new(Published::from([(
                "cosmicspork/sql-review".to_string(),
                ("v0.1.0".to_string(), String::new(), assets),
            )])));
            let (api, _) = serve(published).await;
            let releases = Releases::at(&api).unwrap();
            let refused = fetch(&releases, state.path(), &owners(), "cosmicspork/sql-review")
                .await
                .unwrap_err();
            assert!(matches!(refused, InstallError::Refused(_)), "{refused}");
            assert!(refused.to_string().contains(says), "{says}: {refused}");
            let staged: Vec<_> = std::fs::read_dir(state.path().join("plugins/bin"))
                .map(|d| d.flatten().map(|e| e.file_name()).collect())
                .unwrap_or_default();
            assert!(staged.is_empty(), "{says}: {staged:?}");
            let left: Vec<_> = std::fs::read_dir(state.path().join("plugins"))
                .map(|d| d.flatten().map(|e| e.file_name()).collect())
                .unwrap_or_default();
            assert!(left.iter().all(|n| n == "bin"), "{says}: {left:?}");
        }
        // A repository with no release.
        let state = tempfile::tempdir().unwrap();
        let published = std::sync::Arc::new(std::sync::Mutex::new(Published::new()));
        let (api, _) = serve(published).await;
        let releases = Releases::at(&api).unwrap();
        let refused = fetch(&releases, state.path(), &owners(), "cosmicspork/none")
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("no published release"),
            "{refused}"
        );
    }

    #[test]
    fn a_hostile_archive_is_refused() {
        let Some(target) = target() else {
            return;
        };
        let text = manifest("sql-review", "0.1.0", &[]);
        let name = format!("sql-review-0.1.0-{target}.tar.gz");
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (
                archive(&text, &[("../escape", b"x", 0o644)]),
                "the tarball holds ../escape",
            ),
            (
                archive(&text, &[("/etc/x", b"x", 0o644)]),
                "the tarball holds /etc/x",
            ),
            (
                archive(&text, &[("lib/x.so", b"x", 0o644)]),
                "lib at its top",
            ),
            (
                archive(&text, &[("bin/sql-review", b"again", 0o755)]),
                "exists",
            ),
            (archive(&text, &[("tracon-plugin.toml/x", b"", 0o644)]), ""),
        ];
        for (bytes, says) in cases {
            let state = tempfile::tempdir().unwrap();
            let refused = stage_local(state.path(), &bytes, &name).unwrap_err();
            assert!(refused.to_string().contains(says), "{says}: {refused}");
            assert!(!state.path().join("plugins/bin/sql-review").exists());
        }

        // A symbolic link.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        {
            let mut builder = tar::Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            builder
                .append_link(&mut header, "bin/sql-review", "/bin/sh")
                .unwrap();
            builder.finish().unwrap();
        }
        let state = tempfile::tempdir().unwrap();
        let refused = stage_local(state.path(), &gz.finish().unwrap(), &name).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("neither a file nor a directory"),
            "{refused}"
        );

        // Over the bounds.
        let dir = tempfile::tempdir().unwrap();
        let tarball = dir.path().join("t.tar.gz");
        std::fs::write(&tarball, archive(&text, &[("ui/big", &[0u8; 4096], 0o644)])).unwrap();
        let small = Limits {
            entries: 100,
            bytes: 1024,
        };
        let refused = bundle::unpack_tree(
            &tarball,
            &dir.path().join("a"),
            Path::new(""),
            |_| false,
            small,
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("more than 1024 bytes"),
            "{refused}"
        );
        let few = Limits {
            entries: 2,
            bytes: 1 << 20,
        };
        let refused = bundle::unpack_tree(
            &tarball,
            &dir.path().join("b"),
            Path::new(""),
            |_| false,
            few,
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("more than 2 entries"),
            "{refused}"
        );
    }

    #[test]
    fn a_local_archive_is_pinned_by_its_own_digest_under_its_shipped_name() {
        let Some(target) = target() else {
            return;
        };
        let state = tempfile::tempdir().unwrap();
        let bytes = archive(&manifest("sql-review", "0.1.0", &[]), &[]);
        let refused = stage_local(state.path(), &bytes, "plugin.tar.gz").unwrap_err();
        assert!(
            refused.to_string().contains("its manifest says"),
            "{refused}"
        );
        let name = format!("./dist/sql-review-0.1.0-{target}.tar.gz");
        let staged = stage_local(state.path(), &bytes, &name).unwrap();
        use sha2::Digest as _;
        assert_eq!(
            staged.build.digest,
            hex::encode(sha2::Sha256::digest(&bytes))
        );
        assert_eq!(
            staged.source,
            format!("local:sql-review-0.1.0-{target}.tar.gz")
        );
        assert!(staged.tag.is_none());
        // Staged again, the same build is kept as it was.
        let again = stage_local(state.path(), &bytes, &name).unwrap();
        assert_eq!(again.dir, staged.dir);
        assert_eq!(again.build, staged.build);
    }

    #[test]
    fn prune_keeps_only_the_builds_named() {
        let Some(target) = target() else {
            return;
        };
        let state = tempfile::tempdir().unwrap();
        let stage = |version: &str| {
            let bytes = archive(&manifest("a", version, &[]), &[]);
            stage_local(
                state.path(),
                &bytes,
                &format!("a-{version}-{target}.tar.gz"),
            )
            .unwrap()
        };
        let (one, two, three) = (stage("1.0.0"), stage("1.1.0"), stage("1.2.0"));
        prune(state.path(), "a", &[&one.build, &three.build]).unwrap();
        assert!(one.dir.exists() && three.dir.exists());
        assert!(!two.dir.exists());
        prune(state.path(), "a", &[]).unwrap();
        assert!(!state.path().join("plugins/bin/a").exists());
        prune(state.path(), "never", &[]).unwrap();
    }
}
