//! Installing the pinned OpenCode UI bundle, so `tracon setup` leaves a node
//! able to serve the native interface.
//!
//! The bundle is a release artefact, not a git object: 34 MiB of build output
//! that `containers/opencode-ui/build.sh` produces from the pinned upstream tag
//! and that the release workflow attests and uploads as
//! `opencode-ui-v<version>.tar.gz`. What is checked in is the recipe and
//! `containers/opencode-ui/DIGEST`, the tree digest of what the recipe makes.
//!
//! So this is the same shape as the harness image's fetch-and-check: get the
//! bytes, unpack them somewhere that is not yet the serving directory, and let
//! **the node's own loader** decide whether they are the pinned tree —
//! `http::ui::Bundle::load` recomputes the digest over the bytes it would
//! serve, which is the only check worth making. A tree that does not match is
//! never moved into place.
//!
//! Nothing here falls back. A node that could not get the bundle serves no
//! native UI and says so; the one thing the UI origin must never do is reach
//! for `app.opencode.ai` (`docs/reference/opencode-v1.18.30` finding 3), and a
//! silent "install something else" would be the same mistake one layer down.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{config::Config, http::ui};

/// The repository the release assets come from — the same one `install.sh`
/// fetches the binary from.
pub const REPO: &str = "cosmicspork/tracon";

/// How long the whole download may take. The artefact is ~19 MB compressed on
/// a link that may be a home connection; generous, but not unbounded.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// The release asset's name, which carries the upstream version rather than
/// tracon's: two tracon releases that vendor the same OpenCode ship the same
/// bundle.
pub fn asset_name() -> String {
    format!("opencode-ui-v{}.tar.gz", ui::PINNED_VERSION)
}

/// Where the asset is fetched from. `TRACON_VERSION` names a tag, exactly as
/// it does for `install.sh`; without it, the latest release.
pub fn release_url() -> String {
    let asset = asset_name();
    match std::env::var("TRACON_VERSION") {
        Ok(tag) if !tag.trim().is_empty() => {
            format!(
                "https://github.com/{REPO}/releases/download/{}/{asset}",
                tag.trim()
            )
        }
        _ => format!("https://github.com/{REPO}/releases/latest/download/{asset}"),
    }
}

/// What an install did, for the line the operator reads.
#[derive(Debug)]
pub enum Outcome {
    /// The directory already held the pinned tree. Nothing was fetched.
    Current { dir: PathBuf, files: usize },
    /// A tree was unpacked, verified, and moved into place.
    Installed {
        dir: PathBuf,
        files: usize,
        source: String,
    },
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current { dir, files } => write!(
                f,
                "OpenCode UI v{} is installed at {} ({files} files, digest verified)",
                ui::PINNED_VERSION,
                dir.display()
            ),
            Self::Installed { dir, files, source } => write!(
                f,
                "OpenCode UI v{} installed to {} from {source} ({files} files, digest verified)",
                ui::PINNED_VERSION,
                dir.display()
            ),
        }
    }
}

/// What to tell an operator whose node has no bundle. Written once, so the CLI
/// and anything else that reports it say the same thing.
pub fn absent_advice() -> String {
    format!(
        "The native OpenCode interface is served from a pinned bundle this node does not have.\n\
         Its origin refuses rather than falling back to app.opencode.ai, so sessions have no\n\
         native UI until one is installed. Either:\n  \
           tracon setup --ui-bundle <{}>\n  \
           containers/opencode-ui/build.sh --src <opencode checkout at v{}>",
        asset_name(),
        ui::PINNED_VERSION
    )
}

/// Put the pinned bundle where `http::ui` reads it.
///
/// `tarball` is a local `opencode-ui-v<version>.tar.gz` for an offline install
/// (`tracon setup --ui-bundle`); without one the release asset is fetched.
/// `force` re-installs over a directory that already verifies, which is what
/// `--rebuild` means for every other thing `setup` owns.
pub async fn install(cfg: &Config, tarball: Option<&Path>, force: bool) -> Result<Outcome> {
    let dir = cfg.ui.opencode_bundle_path();
    if !force {
        if let Ok(bundle) = ui::Bundle::load(&dir) {
            return Ok(Outcome::Current {
                dir,
                files: bundle.len(),
            });
        }
    }

    let (bytes, source) = match tarball {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading {}", path.display()))?;
            (bytes, path.display().to_string())
        }
        None => {
            let url = release_url();
            tracing::info!(%url, "fetching the OpenCode UI bundle");
            let bytes = fetch(&url).await?;
            (bytes, url)
        }
    };
    tracing::info!(
        bytes = bytes.len(),
        sha256 = %hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)),
        "OpenCode UI bundle downloaded"
    );

    let parent = dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", dir.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let staging = parent.join(format!(
        ".{}.incoming",
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "opencode-ui".into())
    ));
    let _ = std::fs::remove_dir_all(&staging);
    let unpacked = unpack(&bytes, &staging).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&staging);
    })?;

    // The node's own loader, over the bytes it would serve. Anything that gets
    // past this is the pinned tree by definition, and nothing that does not
    // get past it is ever moved into place.
    let bundle = match ui::Bundle::load(&staging) {
        Ok(bundle) => bundle,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            bail!("{source} is not the pinned bundle: {e}");
        }
    };
    let files = bundle.len();
    debug_assert_eq!(files, unpacked, "the loader sees what was unpacked");

    let retired = parent.join(format!(
        ".{}.replaced",
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "opencode-ui".into())
    ));
    let _ = std::fs::remove_dir_all(&retired);
    let had_one = dir.exists();
    if had_one {
        std::fs::rename(&dir, &retired)
            .with_context(|| format!("moving the old bundle out of {}", dir.display()))?;
    }
    if let Err(e) = std::fs::rename(&staging, &dir) {
        // Put back what was there rather than leaving the node with neither.
        if had_one {
            let _ = std::fs::rename(&retired, &dir);
        }
        return Err(e).with_context(|| format!("installing the bundle at {}", dir.display()));
    }
    let _ = std::fs::remove_dir_all(&retired);

    Ok(Outcome::Installed { dir, files, source })
}

async fn fetch(url: &str) -> Result<Vec<u8>> {
    // No proxy: the release is fetched over the operator's own link, and an
    // ambient HTTPS_PROXY in a node's environment is for the harness boundary,
    // not for this.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(FETCH_TIMEOUT)
        .build()?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !response.status().is_success() {
        bail!(
            "{url} answered {}; this release may not carry {}",
            response.status(),
            asset_name()
        );
    }
    Ok(response.bytes().await?.to_vec())
}

/// Unpack the tarball into a directory that is not yet the serving one.
///
/// Deliberately strict about what an entry may be. The digest check afterwards
/// is what decides whether the tree is the pinned one, but it happens *after*
/// these bytes have touched the filesystem — so a symlink, a device node, or a
/// path that climbs out of the staging directory is refused here, where it can
/// still do nothing.
fn unpack(bytes: &[u8], into: &Path) -> Result<usize> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(false);
    archive.set_preserve_mtime(false);
    archive.set_overwrite(false);
    std::fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;

    let mut files = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let named = entry.path()?.into_owned();
        if kind.is_dir() {
            continue;
        }
        if !kind.is_file() {
            bail!(
                "{} is a {kind:?} entry; the bundle is regular files only",
                named.display()
            );
        }
        let relative = within(&named)
            .ok_or_else(|| anyhow::anyhow!("{} is not a path inside the bundle", named.display()))?;
        let dest = into.join(&relative);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("unpacking {}", relative.display()))?;
        files += 1;
    }
    if files == 0 {
        bail!("the archive holds no files");
    }
    Ok(files)
}

/// A tar entry's path as a relative path with no way out of the directory it
/// is unpacked into, or `None`.
fn within(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => {
                if part.to_str().is_none_or(|s| s.contains('\0')) {
                    return None;
                }
                out.push(part);
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_is_named_for_the_pinned_upstream_version() {
        assert_eq!(asset_name(), "opencode-ui-v1.18.30.tar.gz");
        let url = release_url();
        assert!(url.starts_with("https://github.com/cosmicspork/tracon/releases/"));
        assert!(url.ends_with("/opencode-ui-v1.18.30.tar.gz"), "{url}");
    }

    /// The advice names both ways in, and never a fallback.
    #[test]
    fn the_message_for_a_node_without_one_says_what_to_do() {
        let advice = absent_advice();
        assert!(advice.contains("--ui-bundle"), "{advice}");
        assert!(advice.contains("build.sh"), "{advice}");
        assert!(advice.contains("refuses rather than falling back"), "{advice}");
    }

    #[test]
    fn an_entry_that_climbs_out_is_not_a_path_inside_the_bundle() {
        assert_eq!(within(Path::new("./index.html")).unwrap(), Path::new("index.html"));
        assert_eq!(
            within(Path::new("./assets/index-a.js")).unwrap(),
            Path::new("assets/index-a.js")
        );
        assert!(within(Path::new("../escape")).is_none());
        assert!(within(Path::new("assets/../../escape")).is_none());
        assert!(within(Path::new("/etc/passwd")).is_none());
        assert!(within(Path::new(".")).is_none());
    }

    /// A tarball that is not the pinned tree gets as far as staging and no
    /// further: the loader refuses it and nothing is installed.
    #[tokio::test]
    async fn a_tarball_that_is_not_the_pinned_tree_installs_nothing() {
        let home = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let dir = home.path().join("state/opencode-ui");
        cfg.ui.opencode_bundle_dir = Some(dir.clone());

        let mut tar = tar::Builder::new(Vec::new());
        let body = b"<html><head><script type=\"module\" src=\"/assets/a.js\"></script></head></html>";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "index.html", &body[..]).unwrap();
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &raw).unwrap();
        let tarball = home.path().join("bundle.tar.gz");
        std::fs::write(&tarball, gz.finish().unwrap()).unwrap();

        let err = install(&cfg, Some(&tarball), false).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not the pinned bundle"), "{message}");
        assert!(message.contains("tree digest"), "{message}");
        assert!(!dir.exists(), "nothing is installed from a tree that fails the digest");
        // And the staging directory is not left behind either.
        let leftovers: Vec<_> = std::fs::read_dir(home.path().join("state"))
            .map(|entries| entries.filter_map(Result::ok).map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The real artefact, on a machine that has one. `TRACON_UI_BUNDLE_TARBALL`
    /// names an `opencode-ui-v<version>.tar.gz`; without it there is nothing to
    /// install and the case says so rather than asserting against a stand-in.
    #[tokio::test]
    async fn the_release_tarball_installs_and_verifies() {
        let Some(tarball) = std::env::var_os("TRACON_UI_BUNDLE_TARBALL").map(PathBuf::from) else {
            eprintln!("skipped: set TRACON_UI_BUNDLE_TARBALL to a release bundle tarball");
            return;
        };
        let home = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        let dir = home.path().join("state/opencode-ui");
        cfg.ui.opencode_bundle_dir = Some(dir.clone());

        let outcome = install(&cfg, Some(&tarball), false).await.unwrap();
        let Outcome::Installed { files, .. } = outcome else {
            panic!("a fresh directory is an install, not a no-op: {outcome:?}");
        };
        assert_eq!(files, ui::Bundle::load(&dir).unwrap().len());
        // The serving directory holds the tree, not the tarball's `./` prefix.
        assert!(dir.join("index.html").is_file());
        assert!(dir.join("assets").is_dir());

        // A second run finds it current and fetches nothing; `force` reinstalls.
        assert!(matches!(
            install(&cfg, None, false).await.unwrap(),
            Outcome::Current { .. }
        ));
        assert!(matches!(
            install(&cfg, Some(&tarball), true).await.unwrap(),
            Outcome::Installed { .. }
        ));
    }

    /// An archive carrying a symlink is refused before the digest is reached —
    /// the check that runs before the bytes can mean anything.
    #[tokio::test]
    async fn a_symlink_in_the_archive_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.ui.opencode_bundle_dir = Some(home.path().join("state/opencode-ui"));

        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        tar.append_link(&mut header, "index.html", "/etc/passwd")
            .unwrap();
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &raw).unwrap();
        let tarball = home.path().join("bundle.tar.gz");
        std::fs::write(&tarball, gz.finish().unwrap()).unwrap();

        let err = install(&cfg, Some(&tarball), false).await.unwrap_err();
        assert!(
            err.to_string().contains("regular files only"),
            "{err}"
        );
    }
}
