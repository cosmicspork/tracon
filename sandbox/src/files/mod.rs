//! Trees in and out of a sandbox, as tar.
//!
//! [`pack`] writes a directory as an archive, applying the checkout
//! exclusions when asked. [`unpack`] is the one way an archive becomes files,
//! on either side, and it holds an archive to the rules the node holds any
//! tree from a runtime to (`workspace::validate_tree`): only regular files,
//! directories and symbolic links; every path relative and inside the
//! destination; every link resolving inside the tree by the same rule as
//! `link_stays_inside`, or naming a place in it by the absolute path the tree
//! is at in the sandbox; nothing written through a link; and a bounded number
//! of entries and bytes. What crosses is what Git records of a file: its
//! bytes, whether it is executable, and a link as a link. Owners, other mode
//! bits, extended attributes and devices never do.
//!
//! A prepared tree (a virtualenv, a package cache) may also keep links that
//! leave it ([`Limits::inert_links`]): stored as links, and never followed
//! here, by the node, or by any copy the node makes of the tree.

mod checkout;

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::io::SyncIoBridge;

pub use checkout::{ignore_rules_inside, kept_paths};

use crate::protocol::{Exclusions, TreeSummary};

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("unsafe entry (a path or link leaving the tree, or a write through a link): {0}")]
    Unsafe(PathBuf),
    #[error("unsupported entry type {kind} at {path}")]
    Unsupported { path: PathBuf, kind: String },
    #[error("archive exceeds the {0}-byte limit")]
    TooLarge(u64),
    #[error("archive holds more than {0} files")]
    TooManyFiles(usize),
    #[error("archive holds more than {0} directories")]
    TooManyDirs(usize),
    #[error("archive entry {0} is shorter than its header says")]
    Truncated(PathBuf),
    #[error("{0} is not a directory")]
    NotADirectory(PathBuf),
    #[error("read index: {0}")]
    Index(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Bounds on what one archive may create.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limits {
    /// Total bytes of regular files.
    pub max_bytes: u64,
    /// Regular files and links.
    pub max_files: usize,
    pub max_dirs: usize,
    /// The longest relative path, in bytes.
    pub max_path_bytes: usize,
    /// Keep a link whose target leaves the tree as a link rather than refuse
    /// the archive: what a preparation leaves (`.venv/bin/python` naming the
    /// image's interpreter) is its point, and nothing that unpacks, copies or
    /// exports a tree follows one. Never for a checkout: review and
    /// publication hold those to links that stay inside.
    #[serde(default)]
    pub inert_links: bool,
}

impl Default for Limits {
    /// The node's own workspace import limits.
    fn default() -> Self {
        Limits {
            max_bytes: 512 * 1024 * 1024,
            max_files: 20_000,
            max_dirs: 20_000,
            max_path_bytes: 4096,
            inert_links: false,
        }
    }
}

pub(crate) fn open_no_follow(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Whether a symbolic link `depth` directories below the root of the tree
/// resolves inside it: relative, climbing only at its start, and never above
/// the root. The node's rule (`workspace::link_stays_inside`), mirrored so an
/// archive it would accept as a tree is one this accepts as an archive.
pub fn link_stays_inside(target: &Path, depth: usize) -> bool {
    if target.as_os_str().is_empty() {
        return false;
    }
    let mut climbed = 0usize;
    let mut descended = false;
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if !descended && climbed < depth => climbed += 1,
            Component::Normal(_) => descended = true,
            _ => return false,
        }
    }
    true
}

/// [`link_stays_inside`], or an absolute target under `mount` (the absolute
/// path the tree is at in the sandbox) that stays inside read from the root:
/// bun's cache, mounted at `/cache`, links `/cache/bun/x/1@@@1` to
/// `/cache/bun/x@1@@@1`.
pub fn link_resolves_inside(target: &Path, depth: usize, mount: Option<&Path>) -> bool {
    if link_stays_inside(target, depth) {
        return true;
    }
    let Some(mount) = mount.filter(|mount| plain_absolute(mount)) else {
        return false;
    };
    match target.strip_prefix(mount) {
        Ok(rest) => rest.as_os_str().is_empty() || link_stays_inside(rest, 0),
        Err(_) => false,
    }
}

/// An absolute path of one or more plain names: at `/`, every absolute
/// target would be one inside.
fn plain_absolute(path: &Path) -> bool {
    let mut components = path.components();
    components.next() == Some(Component::RootDir)
        && components.clone().next().is_some()
        && components.all(|c| matches!(c, Component::Normal(_)))
}

fn file_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 {
        0o644
    } else {
        0o755
    }
}

/// Write `root` as a tar archive to `out`. Links are archived as links and
/// never followed; sockets, FIFOs and devices are left out.
pub fn pack<W: Write>(
    root: &Path,
    exclusions: Exclusions,
    out: W,
) -> Result<TreeSummary, FilesError> {
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() {
        return Err(FilesError::NotADirectory(root.into()));
    }
    let keep = match exclusions {
        Exclusions::None => None,
        Exclusions::Checkout => kept_paths(root)?,
    };
    // Fused, so the end-of-archive blocks a dropped builder writes never
    // follow an error: a failed pack must not end like a complete one.
    let mut fused = Fused {
        inner: out,
        failed: false,
    };
    let mut builder = tar::Builder::new(&mut fused);
    builder.mode(tar::HeaderMode::Deterministic);
    let mut summary = TreeSummary::default();
    match pack_dir(
        &mut builder,
        root,
        Path::new(""),
        keep.as_ref(),
        &mut summary,
    ) {
        Ok(()) => {
            builder.finish()?;
            Ok(summary)
        }
        Err(error) => {
            builder.get_mut().failed = true;
            Err(error)
        }
    }
}

fn pack_dir<W: Write>(
    builder: &mut tar::Builder<W>,
    dir: &Path,
    relative: &Path,
    keep: Option<&HashSet<PathBuf>>,
    summary: &mut TreeSummary,
) -> Result<(), FilesError> {
    let mut names: Vec<_> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect(),
        // Gone under the walk: nothing to archive.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let rel = relative.join(&name);
        // The repository travels whole; the exclusions decide everything else.
        let repository =
            relative.as_os_str().is_empty() && name.to_string_lossy().eq_ignore_ascii_case(".git");
        let keep_below = if repository { None } else { keep };
        if !repository && keep.is_some_and(|keep| !keep.contains(&rel)) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let file_type = metadata.file_type();
        let mut header = tar::Header::new_gnu();
        if file_type.is_symlink() {
            let target = match std::fs::read_link(&path) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            builder.append_link(&mut header, &rel, &target)?;
            summary.links += 1;
        } else if file_type.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            builder.append_data(&mut header, &rel, io::empty())?;
            summary.dirs += 1;
            pack_dir(builder, &path, &rel, keep_below, summary)?;
        } else if file_type.is_file() {
            // Opened without following a link and checked on the descriptor,
            // so a link swapped in after the stat above is never read through.
            let file = match open_no_follow(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(FilesError::Unsafe(path)),
            };
            let opened = file.metadata()?;
            if !opened.is_file() {
                return Err(FilesError::Unsafe(path));
            }
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(file_mode(opened.permissions().mode()));
            header.set_size(opened.len());
            if let Ok(modified) = opened.modified() {
                if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                    header.set_mtime(since.as_secs());
                }
            }
            // A file that grows under the read is cut at the size in the
            // header; one that shrinks fails the append.
            builder.append_data(&mut header, &rel, file.take(opened.len()))?;
            summary.files += 1;
            summary.bytes += opened.len();
        }
        // Sockets, FIFOs and devices have no bytes to carry.
    }
    Ok(())
}

/// The relative path of an archive entry, or `None` if it is absolute,
/// climbs, or is empty.
fn entry_path(raw: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = Path::new(std::ffi::OsStr::from_bytes(raw));
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

struct Unpacker<'a> {
    root: &'a Path,
    mount: Option<&'a Path>,
    limits: &'a Limits,
    summary: TreeSummary,
}

impl Unpacker<'_> {
    /// Make every directory from the root down to `relative` (inclusive),
    /// refusing to pass through anything that is not a real directory.
    fn ensure_dirs(&mut self, relative: &Path) -> Result<(), FilesError> {
        let mut current = self.root.to_path_buf();
        for component in relative.components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => return Err(FilesError::Unsafe(current)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.summary.dirs += 1;
                    if self.summary.dirs > self.limits.max_dirs as u64 {
                        return Err(FilesError::TooManyDirs(self.limits.max_dirs));
                    }
                    std::fs::DirBuilder::new()
                        .mode(0o755)
                        .create(&current)
                        .map_err(FilesError::Io)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Clear the leaf for a file or link, refusing to replace a directory.
    fn clear_leaf(&self, path: &Path) -> Result<(), FilesError> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => Err(FilesError::Unsafe(path.into())),
            Ok(_) => Ok(std::fs::remove_file(path)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn count_file(&mut self) -> Result<(), FilesError> {
        if self.summary.files + self.summary.links >= self.limits.max_files as u64 {
            return Err(FilesError::TooManyFiles(self.limits.max_files));
        }
        Ok(())
    }

    fn entry<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<(), FilesError> {
        let raw = entry.path_bytes().into_owned();
        let shown = PathBuf::from(String::from_utf8_lossy(&raw).into_owned());
        let kind = entry.header().entry_type();
        if kind == tar::EntryType::XGlobalHeader {
            return Ok(());
        }
        if raw.len() > self.limits.max_path_bytes {
            return Err(FilesError::Unsafe(shown));
        }
        let Some(relative) = entry_path(&raw) else {
            // `./` names the root itself, which already exists.
            if kind == tar::EntryType::Directory
                && shown.components().all(|c| c == Component::CurDir)
                && !raw.is_empty()
            {
                return Ok(());
            }
            return Err(FilesError::Unsafe(shown));
        };
        let parent = relative.parent().unwrap_or(Path::new(""));
        let target = self.root.join(&relative);
        match kind {
            tar::EntryType::Directory => {
                self.ensure_dirs(&relative)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                self.count_file()?;
                let size = entry.size();
                let total = self.summary.bytes.saturating_add(size);
                if total > self.limits.max_bytes {
                    return Err(FilesError::TooLarge(self.limits.max_bytes));
                }
                self.ensure_dirs(parent)?;
                self.clear_leaf(&target)?;
                let mode = file_mode(entry.header().mode().unwrap_or(0o644));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&target)?;
                let copied = io::copy(&mut (&mut entry).take(size), &mut file)?;
                if copied != size {
                    return Err(FilesError::Truncated(relative));
                }
                // The umask may have narrowed the mode it was created with.
                file.set_permissions(std::fs::Permissions::from_mode(mode))?;
                if let Ok(mtime) = entry.header().mtime() {
                    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
                    let _ = file.set_modified(when);
                }
                self.summary.files += 1;
                self.summary.bytes = total;
            }
            tar::EntryType::Symlink => {
                self.count_file()?;
                let link = entry
                    .link_name_bytes()
                    .ok_or_else(|| FilesError::Unsafe(shown.clone()))?
                    .into_owned();
                use std::os::unix::ffi::OsStrExt as _;
                let link = PathBuf::from(std::ffi::OsStr::from_bytes(&link));
                let depth = relative.components().count() - 1;
                if !self.limits.inert_links && !link_resolves_inside(&link, depth, self.mount) {
                    return Err(FilesError::Unsafe(relative));
                }
                self.ensure_dirs(parent)?;
                self.clear_leaf(&target)?;
                std::os::unix::fs::symlink(&link, &target)?;
                self.summary.links += 1;
            }
            other => {
                return Err(FilesError::Unsupported {
                    path: relative,
                    kind: format!("{other:?}"),
                })
            }
        }
        Ok(())
    }
}

/// Unpack an archive into `dest`, an existing directory, by the rules above.
/// `mount` is where the tree is in the sandbox (`dest` itself, unpacking
/// there), which an absolute link is read against. Entries may overwrite
/// files and links already there, never a directory, and never through a
/// link. On error what was unpacked so far stays: the caller unpacks into a
/// directory of its own and discards it on failure.
pub fn unpack<R: Read>(
    reader: R,
    dest: &Path,
    mount: Option<&Path>,
    limits: &Limits,
) -> Result<TreeSummary, FilesError> {
    let metadata = std::fs::symlink_metadata(dest)?;
    if !metadata.is_dir() {
        return Err(FilesError::NotADirectory(dest.into()));
    }
    let mut archive = tar::Archive::new(reader);
    let mut unpacker = Unpacker {
        root: dest,
        mount,
        limits,
        summary: TreeSummary::default(),
    };
    for entry in archive.entries()? {
        unpacker.entry(entry?)?;
    }
    Ok(unpacker.summary)
}

/// A writer that refuses everything once marked failed.
struct Fused<W> {
    inner: W,
    failed: bool,
}

impl<W: Write> Write for Fused<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.inner.flush()
    }
}

/// [`pack`] onto an async writer, off the runtime's threads. The writer comes
/// back either way, so a failure can be signalled on it (an `H2Writer` is
/// reset rather than shut down).
pub async fn pack_async<W>(
    root: PathBuf,
    exclusions: Exclusions,
    writer: W,
) -> (Result<TreeSummary, FilesError>, W)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut bridge = SyncIoBridge::new(writer);
    let joined = tokio::task::spawn_blocking(move || {
        let result = pack(&root, exclusions, &mut bridge).and_then(|summary| {
            bridge.flush()?;
            Ok(summary)
        });
        (result, bridge.into_inner())
    })
    .await;
    match joined {
        Ok(done) => done,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

/// [`unpack`] from an async reader, off the runtime's threads.
pub async fn unpack_async<R>(
    reader: R,
    dest: PathBuf,
    mount: Option<PathBuf>,
    limits: Limits,
) -> Result<TreeSummary, FilesError>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let bridge = SyncIoBridge::new(reader);
    tokio::task::spawn_blocking(move || unpack(bridge, &dest, mount.as_deref(), &limits))
        .await
        .map_err(|e| FilesError::Io(io::Error::other(e)))?
}

#[cfg(test)]
mod tests;
