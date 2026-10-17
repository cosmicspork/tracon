//! Managed workspaces live in runtime-owned storage.  The node stages bytes
//! through bounded copies; a harness never receives a host path or a forge
//! credential.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::runner::Mount;

pub const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_IMPORT_FILES: usize = 20_000;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace source {0} does not exist")]
    Missing(PathBuf),
    #[error(
        "workspace source contains a symbolic link leaving the tree or an unsupported file: {0}"
    )]
    Unsafe(PathBuf),
    #[error("workspace import exceeds the {MAX_IMPORT_BYTES}-byte limit")]
    TooLarge,
    #[error("workspace import contains more than {MAX_IMPORT_FILES} files")]
    TooManyFiles,
    #[error("git {op} refused: {message}")]
    Git { op: &'static str, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct Workspace {
    pub id: String,
    pub volume: String,
    pub snapshot: PathBuf,
}

impl Workspace {
    pub fn mount(&self, target: impl Into<String>, read_only: bool) -> Mount {
        Mount::volume(self.volume.clone(), target, read_only)
    }
}

pub fn volume_name(id: &str) -> String {
    format!("tracon-workspace-{}", safe_id(id))
}

pub fn scratch_volume_name(id: &str) -> String {
    format!("tracon-scratch-{}", safe_id(id))
}

pub fn snapshot_path(id: &str) -> PathBuf {
    crate::config::Config::state_dir()
        .join("workspaces")
        .join(safe_id(id))
}

pub fn staging_path(id: &str) -> PathBuf {
    crate::config::Config::state_dir()
        .join("workspace-staging")
        .join(safe_id(id))
}

fn safe_id(id: &str) -> String {
    id.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '-'
            }
        })
        .collect()
}

/// One lock per runtime volume, serializing import/export against it. Two
/// operations racing the same volume (a session's own start-up export racing
/// a client's `/download`, two concurrent approvals snapshotting the same
/// workspace, or a provider refresh exporting the login state a reconnect is
/// clearing) would otherwise interleave a remove-and-rename with a reader,
/// handing back a torn directory, content that was never the tree
/// `validate_tree` just approved, or no directory at all.
pub(crate) fn volume_lock(volume: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: std::sync::LazyLock<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>> =
        std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));
    LOCKS
        .lock()
        .unwrap()
        .entry(volume.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// Open a file without following a symlink at its leaf. Combined with
/// checking the *opened* descriptor's metadata (`std::fs::File::metadata`
/// stats the fd, not the path), this collapses the classic stat-then-open
/// TOCTOU into one resolution: a symlink swapped in after `read_dir` but
/// before this call cannot make an open elsewhere succeed silently.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Copy selected bytes without following symlinks.  `skip_git` is used only
/// when overlaying an operator checkout onto the separately cloned seed: host
/// Git configuration and executable metadata never cross that boundary.
///
/// What crosses is what Git itself records of a file: its bytes, whether it
/// is executable, and a symbolic link as a link (see `link_stays_inside`).
pub fn copy_tree(source: &Path, destination: &Path, skip_git: bool) -> Result<(), WorkspaceError> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|_| WorkspaceError::Missing(source.into()))?;

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceError::Unsafe(source.into()));
    }
    let mut budget = CopyBudget::default();
    copy_dir(source, destination, skip_git, None, 0, &mut budget)?;
    // A directory that vanished under the walk is copied as empty, which is
    // the right answer for something inside the tree and the wrong one for the
    // tree itself: no destination at all means the source went away, not that
    // it held nothing.
    if !destination.is_dir() {
        return Err(WorkspaceError::Missing(source.into()));
    }
    Ok(())
}

/// Overlay a checkout the way `git ls-files -co --exclude-standard` sees it,
/// which is what Podman's export stages (`STAGE_WORKSPACE`): every file the
/// index tracks, even one an ignore rule matches, and the untracked files the
/// rules leave in. Dependency trees and build output are ignored, so they
/// neither break the copy (their symlinks) nor exhaust its budget, and an
/// ignored `.env` never reaches the harness. The rules and the index are read
/// as files; no Git runs in the checkout, where its configuration could
/// execute commands.
fn overlay_checkout(source: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    let metadata =
        std::fs::symlink_metadata(source).map_err(|_| WorkspaceError::Missing(source.into()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceError::Unsafe(source.into()));
    }
    let mut keep = std::collections::HashSet::new();
    let walk = ignore::WalkBuilder::new(source)
        .standard_filters(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| !entry.file_name().eq_ignore_ascii_case(".git"))
        .build();
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => match error.io_error() {
                Some(io) if vanished(io) => continue,
                _ => return Err(WorkspaceError::Unsafe(source.into())),
            },
        };
        if let Ok(relative) = entry.path().strip_prefix(source) {
            keep.insert(relative.to_path_buf());
        }
    }
    // A linked worktree's `.git` is a file naming an index elsewhere; its
    // walk alone decides, as before.
    let repository = source.join(".git");
    if std::fs::symlink_metadata(&repository).is_ok_and(|metadata| metadata.is_dir()) {
        for tracked in tracked_paths(&repository)? {
            let mut path = tracked.as_path();
            while !path.as_os_str().is_empty() && keep.insert(path.to_path_buf()) {
                path = path.parent().unwrap_or(Path::new(""));
            }
        }
    }
    let keep = Keep {
        root: source.to_path_buf(),
        paths: keep,
    };
    let mut budget = CopyBudget::default();
    copy_dir(source, destination, true, Some(&keep), 0, &mut budget)
}

/// Large enough for the index of a tree far past `MAX_IMPORT_FILES`.
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;

/// Read one file of a Git directory without following a link at its leaf.
/// `None` when it does not exist.
fn read_git_file(path: &Path) -> Result<Option<Vec<u8>>, WorkspaceError> {
    use std::io::Read;
    let file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) if vanished(&error) => return Ok(None),
        Err(_) => return Err(WorkspaceError::Unsafe(path.into())),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES {
        return Err(WorkspaceError::Unsafe(path.into()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_INDEX_BYTES).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// The paths a repository's index tracks, parsed from `index` (and the
/// shared index a split index names) as data. A missing index tracks
/// nothing; one that cannot be parsed refuses the copy, as `git ls-files`
/// would fail on it.
fn tracked_paths(git_dir: &Path) -> Result<Vec<PathBuf>, WorkspaceError> {
    let refused = |name: &str| WorkspaceError::Git {
        op: "read index",
        message: format!("{name} is not an index this node can read"),
    };
    let Some(index) = read_git_file(&git_dir.join("index"))? else {
        return Ok(Vec::new());
    };
    // Only the object id's width is read from the configuration; one that
    // cannot be read is the default, SHA-1, and a wrong guess fails the parse.
    let config = read_git_file(&git_dir.join("config")).ok().flatten();
    let sha256 = config.is_some_and(|config| {
        let config: Vec<u8> = config
            .iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .map(u8::to_ascii_lowercase)
            .collect();
        config
            .windows(b"objectformat=sha256".len())
            .any(|window| window == b"objectformat=sha256")
    });
    let hash_len = if sha256 { 32 } else { 20 };
    let mut paths = Vec::new();
    let shared = parse_index(&index, hash_len, &mut paths).ok_or_else(|| refused("index"))?;
    if let Some(shared) = shared {
        let name = format!("sharedindex.{shared}");
        let bytes = read_git_file(&git_dir.join(&name))?.ok_or_else(|| refused(&name))?;
        parse_index(&bytes, hash_len, &mut paths).ok_or_else(|| refused(&name))?;
    }
    Ok(paths)
}

/// Git's index format, versions 2 to 4, for the entry names alone. Returns
/// the hex name of the shared index a split index points at, if any.
fn parse_index(bytes: &[u8], hash_len: usize, out: &mut Vec<PathBuf>) -> Option<Option<String>> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(..4)? != b"DIRC" {
        return None;
    }
    let version = word(4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = word(8)? as usize;
    let mut at = 12;
    let mut previous = Vec::new();
    for _ in 0..count {
        let start = at;
        // ctime, mtime, dev, ino, mode, uid, gid and size, then the object id.
        let flags_at = start + 40 + hash_len;
        let flags = u16::from_be_bytes(bytes.get(flags_at..flags_at + 2)?.try_into().ok()?);
        let mut name_at = flags_at + 2;
        if flags & 0x4000 != 0 {
            if version < 3 {
                return None;
            }
            name_at += 2;
        }
        let name = if version == 4 {
            // The name drops a varint count of bytes from the end of the
            // previous one and appends a NUL-terminated suffix; no padding.
            let mut strip = 0usize;
            loop {
                let byte = *bytes.get(name_at)?;
                name_at += 1;
                strip = strip
                    .checked_mul(128)?
                    .checked_add(usize::from(byte & 0x7f))?;
                if byte & 0x80 == 0 {
                    break;
                }
                strip = strip.checked_add(1)?;
            }
            let end = name_at + bytes.get(name_at..)?.iter().position(|b| *b == 0)?;
            let mut name: Vec<u8> = previous.get(..previous.len().checked_sub(strip)?)?.to_vec();
            name.extend_from_slice(&bytes[name_at..end]);
            at = end + 1;
            name
        } else {
            let end = name_at + bytes.get(name_at..)?.iter().position(|b| *b == 0)?;
            // NUL padding to a multiple of eight, at least one byte.
            at = start + ((end - start + 8) & !7);
            bytes[name_at..end].to_vec()
        };
        // A split index stores an entry that replaces a shared one without
        // its name; the shared index names it.
        if !name.is_empty() {
            out.push(index_path(&name));
        }
        previous = name;
    }
    let mut shared = None;
    let body = bytes.len().checked_sub(hash_len)?;
    while at + 8 <= body {
        let size = word(at + 4)? as usize;
        let data = bytes.get(at + 8..(at + 8).checked_add(size)?)?;
        if &bytes[at..at + 4] == b"link" {
            let oid = data.get(..hash_len)?;
            if oid.iter().any(|byte| *byte != 0) {
                shared = Some(oid.iter().map(|byte| format!("{byte:02x}")).collect());
            }
        }
        at += 8 + size;
    }
    Some(shared)
}

fn index_path(name: &[u8]) -> PathBuf {
    // A sparse index names a directory it does not expand with a trailing `/`.
    let name = name.strip_suffix(b"/").unwrap_or(name);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(name))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(name).into_owned())
    }
}

/// Copy a workspace the way its own repository sees it: the repository, and
/// the tracked and untracked files its ignore rules leave in. The host-side
/// counterpart of what a container backend stages before an export, for a
/// backend whose volumes are plain directories.
pub fn copy_checkout(source: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    if destination.exists() {
        std::fs::remove_dir_all(destination)?;
    }
    let repository = source.join(".git");
    if !repository.is_dir() {
        return copy_tree(source, destination, false);
    }
    overlay_checkout(source, destination)?;
    copy_tree(&repository, &destination.join(".git"), false)
}

struct Keep {
    root: PathBuf,
    paths: std::collections::HashSet<PathBuf>,
}

impl Keep {
    fn allows(&self, path: &Path) -> bool {
        path.strip_prefix(&self.root)
            .is_ok_and(|relative| self.paths.contains(relative))
    }
}
/// Create or replace the runtime-owned bytes for a workspace from a validated
/// node staging directory. This is the only bridge from a candidate snapshot
/// or browser artifact into a harness-visible filesystem.
pub async fn import(
    backend: &dyn crate::boundary::Backend,
    workspace: &Workspace,
    source: &Path,
) -> Result<(), WorkspaceError> {
    let lock = volume_lock(&workspace.volume);
    let _guard = lock.lock().await;
    validate_tree(source)?;
    backend
        .import_volume(&workspace.volume, source)
        .await
        .map_err(|e| WorkspaceError::Git {
            op: "runtime import",
            message: e.to_string(),
        })
}

/// Export a validated immutable snapshot of a workspace. The returned path is
/// under node state, never the selected host source.
pub async fn export(
    backend: &dyn crate::boundary::Backend,
    workspace: &Workspace,
) -> Result<PathBuf, WorkspaceError> {
    let lock = volume_lock(&workspace.volume);
    let _guard = lock.lock().await;
    backend
        .export_workspace(&workspace.volume, &workspace.snapshot)
        .await
        .map_err(|e| WorkspaceError::Git {
            op: "runtime export",
            message: e.to_string(),
        })?;
    if let Err(error) = validate_tree(&workspace.snapshot) {
        // A rejected snapshot never sits around at a predictable node-local
        // path for something else to stumble into later.
        let _ = std::fs::remove_dir_all(&workspace.snapshot);
        return Err(error);
    }
    Ok(workspace.snapshot.clone())
}

/// Build a named runtime workspace from an already identified candidate or
/// artifact directory. It does not initialize or otherwise reinterpret Git.
pub async fn from_snapshot(
    backend: &dyn crate::boundary::Backend,
    id: &str,
    source: &Path,
) -> Result<Workspace, WorkspaceError> {
    let workspace = Workspace {
        id: id.to_string(),
        volume: volume_name(id),
        snapshot: snapshot_path(id),
    };
    let lock = volume_lock(&workspace.volume);
    let _guard = lock.lock().await;
    validate_tree(source)?;
    // Writable, because a candidate's snapshot is read-only and what is built
    // from it here — a check's build output, a preparation's install — is not.
    backend
        .import_writable(&workspace.volume, source)
        .await
        .map_err(|e| WorkspaceError::Git {
            op: "runtime import",
            message: e.to_string(),
        })?;
    Ok(workspace)
}

#[derive(Default)]
struct CopyBudget {
    bytes: u64,
    files: usize,
}

/// An entry that was listed and is already gone by the time it is read.
///
/// The trees copied here are live: any `git commit` — the node's own, or the
/// agent's inside its workspace — leaves `git maintenance run --auto --detach`
/// running behind it, and that creates and removes
/// `.git/objects/maintenance.lock` under this walk. Nothing that no longer
/// exists has bytes to copy or can be a symlink pointing out of the tree, so
/// it is skipped rather than failing the copy. This never relaxes a check:
/// every entry that is still there is stated, opened, and budgeted exactly as
/// before, and the copy that produces the imported bytes is the one whose
/// checks bind.
fn vanished(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

/// Whether a symbolic link `depth` directories below the root of the tree
/// being copied resolves inside it. The target must be relative, may climb
/// only at its start, and never above the root. Climbing only at the start is
/// what makes this hold for the filesystem and not just the text: the
/// directories it climbs out of are real ones, since the walk never descends
/// through a link, and every link it passes through after that is held to the
/// same rule. A `..` after a link (`node_modules/x/..`) would climb out of
/// wherever that link leads, which no reading of the text can bound. An npm
/// `.bin` entry (`../acorn/bin/acorn`) or a pnpm store link passes. Review
/// holds a candidate's links to the same rule, so a candidate it accepts is
/// one every copy accepts.
pub(crate) fn link_stays_inside(target: &Path, depth: usize) -> bool {
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

/// Clear the way for a copied file or link. An overlay lands on a checkout
/// that may already hold a link at the same path, and creating a file there
/// would write wherever that link points.
fn replace_leaf(path: &Path) -> Result<(), WorkspaceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() => Ok(std::fs::remove_file(path)?),
        _ => Ok(()),
    }
}

fn copy_dir(
    source: &Path,
    destination: &Path,
    skip_git: bool,
    keep: Option<&Keep>,
    depth: usize,
    budget: &mut CopyBudget,
) -> Result<(), WorkspaceError> {
    // Listed before the destination is created: a directory that vanished
    // under the walk must not leave an empty one behind in the copy.
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if vanished(&error) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    std::fs::create_dir_all(destination)?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if vanished(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        let name = entry.file_name();
        if skip_git && name.to_string_lossy().eq_ignore_ascii_case(".git") {
            continue;
        }
        let source_path = entry.path();
        if keep.is_some_and(|keep| !keep.allows(&source_path)) {
            continue;
        }
        let destination_path = destination.join(&name);
        let metadata = match std::fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if vanished(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            // The link is copied as the text that was checked; nothing here
            // follows it, so a target swapped in later is never read.
            let target = match std::fs::read_link(&source_path) {
                Ok(target) => target,
                Err(error) if vanished(&error) => continue,
                Err(_) => return Err(WorkspaceError::Unsafe(source_path)),
            };
            if !link_stays_inside(&target, depth) {
                return Err(WorkspaceError::Unsafe(source_path));
            }
            budget.files += 1;
            if budget.files > MAX_IMPORT_FILES {
                return Err(WorkspaceError::TooManyFiles);
            }
            replace_leaf(&destination_path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &destination_path)?;
            #[cfg(not(unix))]
            return Err(WorkspaceError::Unsafe(source_path));
            continue;
        }
        if metadata.is_dir() {
            copy_dir(
                &source_path,
                &destination_path,
                skip_git,
                keep,
                depth + 1,
                budget,
            )?;
            continue;
        }
        if !metadata.is_file() {
            return Err(WorkspaceError::Unsafe(source_path));
        }
        // The lstat above and a copy-by-path would resolve `source_path`
        // twice; a symlink swapped in between would make the second
        // resolution copy an arbitrary file's bytes into the snapshot.
        // Opening without following a symlink and re-checking the *opened*
        // descriptor collapses that into one resolution.
        let mut src = match open_no_follow(&source_path) {
            Ok(file) => file,
            // Only an entry that is already gone is skipped. A refused open —
            // a symlink swapped in under `O_NOFOLLOW`, which is `ELOOP`, or a
            // permission change — is still unsafe.
            Err(error) if vanished(&error) => continue,
            Err(_) => return Err(WorkspaceError::Unsafe(source_path)),
        };
        let opened = src.metadata()?;
        if !opened.is_file() {
            return Err(WorkspaceError::Unsafe(source_path));
        }
        budget.files += 1;
        budget.bytes = budget.bytes.saturating_add(opened.len());
        if budget.files > MAX_IMPORT_FILES {
            return Err(WorkspaceError::TooManyFiles);
        }
        if budget.bytes > MAX_IMPORT_BYTES {
            return Err(WorkspaceError::TooLarge);
        }
        if let Some(parent) = destination_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        replace_leaf(&destination_path)?;
        let mut dst = std::fs::File::create(&destination_path)?;
        std::io::copy(&mut src, &mut dst)?;
        // Git's two file modes, and never the source's write bits: a
        // read-only snapshot imported into a volume stays writable there.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if opened.permissions().mode() & 0o111 == 0 {
                0o644
            } else {
                0o755
            };
            dst.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

/// Check a tree received from a runtime before node-side Git, export, or
/// publication reads it.  It is deliberately the same strict shape as import.
pub fn validate_tree(root: &Path) -> Result<(), WorkspaceError> {
    let parent = root
        .parent()
        .ok_or_else(|| WorkspaceError::Unsafe(root.into()))?;
    let inspection = parent.join(format!(".tracon-inspect-{}", uuid::Uuid::now_v7()));
    let result = copy_tree(root, &inspection, false);

    let _ = std::fs::remove_dir_all(inspection);
    result
}
/// Attach signed transfer context as inert JSON. Tracon never executes or uses
/// this file as configuration; it is solely visible provenance for the next
/// session. The staging root must already be a checked directory.
pub fn write_context(selected: &Path, context: &serde_json::Value) -> Result<(), WorkspaceError> {
    let metadata = std::fs::symlink_metadata(selected)
        .map_err(|_| WorkspaceError::Missing(selected.into()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorkspaceError::Unsafe(selected.into()));
    }
    let context_dir = selected.join(".tracon");
    if context_dir.exists()
        && std::fs::symlink_metadata(&context_dir)?
            .file_type()
            .is_symlink()
    {
        return Err(WorkspaceError::Unsafe(context_dir));
    }
    std::fs::create_dir_all(&context_dir)?;
    let path = context_dir.join("transfer-context.json");
    let encoded = serde_json::to_vec_pretty(context).map_err(|e| WorkspaceError::Git {
        op: "transfer context",
        message: e.to_string(),
    })?;
    std::fs::write(path, encoded)?;
    Ok(())
}

/// Make an independent checkout that carries committed and selected
/// uncommitted files but none of the operator's Git configuration.  `source`
/// is read only; `git clone --no-local` keeps Git from hard-linking or writing
/// back into it.
pub async fn seed_from_checkout(
    git: &str,
    source: &Path,
    branch: &str,
    base_sha: Option<&str>,
    id: &str,
) -> Result<Workspace, WorkspaceError> {
    if !source.is_dir() {
        return Err(WorkspaceError::Missing(source.into()));
    }
    // `branch`/`base_sha` are client-supplied (`NewSession::branch`/`base_sha`)
    // and reach `git` as bare positional arguments below. A value starting
    // with `-` would be parsed as an option instead of a revision — with
    // `clone --no-local` forcing a remote-style transport, an option like
    // `--upload-pack=...` on a revision is a known command-injection vector.
    if branch.starts_with('-') || base_sha.is_some_and(|s| s.starts_with('-')) {
        return Err(WorkspaceError::Git {
            op: "checkout",
            message: "branch or base commit looks like a command-line option".into(),
        });
    }
    let stage = staging_path(id);
    let _ = std::fs::remove_dir_all(&stage);
    if let Some(parent) = stage.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source_arg = source.to_string_lossy().into_owned();
    let stage_arg = stage.to_string_lossy().into_owned();
    run_git(
        git,
        None,
        "clone",
        [
            "clone",
            "--no-local",
            "--no-hardlinks",
            "--no-checkout",
            "--",
            source_arg.as_str(),
            stage_arg.as_str(),
        ],
    )
    .await?;
    let revision = base_sha.unwrap_or("HEAD");
    run_git(
        git,
        Some(&stage),
        "checkout",
        ["checkout", "--detach", revision],
    )
    .await?;
    run_git(
        git,
        Some(&stage),
        "branch",
        ["checkout", "-B", branch, revision],
    )
    .await?;
    // Overlay files after checkout so selected, uncommitted work arrives as
    // bytes only. .git stays the fresh, node-created checkout.
    overlay_checkout(source, &stage)?;
    sanitize_git(&stage)?;
    Ok(Workspace {
        id: id.to_string(),
        volume: volume_name(id),
        snapshot: snapshot_path(id),
    })
}

/// Seed an empty, commit-capable managed workspace from explicit uploaded
/// files.  The caller has already written the upload into `selected` using
/// checked relative paths.
pub async fn seed_from_files(
    git: &str,
    selected: &Path,
    id: &str,
) -> Result<Workspace, WorkspaceError> {
    let stage = staging_path(id);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    copy_tree(selected, &stage, true)?;
    run_git(git, Some(&stage), "init", ["init", "-b", "main"]).await?;
    sanitize_git(&stage)?;
    Ok(Workspace {
        id: id.to_string(),
        volume: volume_name(id),
        snapshot: snapshot_path(id),
    })
}

/// Every name under a Git directory that can name a program, redirect where
/// configuration is read from, or reinterpret object identity.  `commondir`
/// is on the list because it relocates `$GIT_COMMON_DIR`: with one planted,
/// Git reads `<that dir>/config` and ignores the `.git/config` written below,
/// so rewriting the config alone leaves the whole file selectable by the
/// agent.
const EXECUTABLE_GIT_METADATA: &[&str] = &[
    "config",
    "config.worktree",
    "commondir",
    "hooks",
    "info/grafts",
    "objects/info/alternates",
    "objects/info/http-alternates",
    "refs/replace",
];

/// The nested Git directories a workspace can carry: one per linked worktree
/// the agent added (`git worktree add`, which is where `config.worktree`
/// legitimately lives) and one per submodule.  Each is a complete Git
/// directory and gets the same treatment as the top level.
const NESTED_GIT_DIRS: &[&str] = &["worktrees", "modules"];

/// Ensure an exported workspace cannot supply a credential helper, hook,
/// alternates file, replacement ref, graft, relocated common directory, or
/// `config.worktree` to a node-side Git command — at the top level or in any
/// linked worktree or submodule directory it carries.  Objects and ordinary
/// refs remain so the commit and blob identity can be verified without
/// trusting metadata behavior.
pub fn sanitize_git(root: &Path) -> Result<(), WorkspaceError> {
    let git = root.join(".git");
    let metadata =
        std::fs::symlink_metadata(&git).map_err(|_| WorkspaceError::Unsafe(git.clone()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkspaceError::Unsafe(git));
    }
    scrub_git_dir(&git, 0)?;
    std::fs::create_dir_all(git.join("info"))?;
    std::fs::write(
        git.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\thooksPath = /dev/null\n\tfsmonitor =\n",
    )?;
    Ok(())
}

/// Submodules nest, so the walk is bounded rather than trusting the tree to
/// terminate. Nothing legitimate is anywhere near this deep.
const MAX_NESTED_GIT_DEPTH: usize = 8;

fn scrub_git_dir(git: &Path, depth: usize) -> Result<(), WorkspaceError> {
    for name in EXECUTABLE_GIT_METADATA {
        remove_metadata(&git.join(name))?;
    }
    if depth >= MAX_NESTED_GIT_DEPTH {
        // A tree nested past the bound is refused outright rather than left
        // half-scrubbed.
        for name in NESTED_GIT_DIRS {
            remove_metadata(&git.join(name))?;
        }
        return Ok(());
    }
    for name in NESTED_GIT_DIRS {
        let container = git.join(name);
        let Ok(entries) = std::fs::read_dir(&container) else {
            continue;
        };
        for entry in entries {
            // Same live-tree race as the copy walk: a nested Git directory
            // that is gone needs no scrubbing.
            let nested = match entry {
                Ok(entry) => entry.path(),
                Err(error) if vanished(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            let metadata = match std::fs::symlink_metadata(&nested) {
                Ok(metadata) => metadata,
                Err(error) if vanished(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink() {
                return Err(WorkspaceError::Unsafe(nested));
            }
            if metadata.is_dir() {
                scrub_git_dir(&nested, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Remove one metadata entry without following a symlink at its leaf: a
/// `.git/hooks` symlinked at a host directory must be unlinked, never
/// recursed into and emptied.
fn remove_metadata(path: &Path) -> Result<(), WorkspaceError> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    let removed = if metadata.file_type().is_symlink() || !metadata.is_dir() {
        std::fs::remove_file(path)
    } else {
        std::fs::remove_dir_all(path)
    };
    match removed {
        Ok(()) => Ok(()),
        // A lock file Git wrote and then removed itself: gone is the outcome
        // this asks for.
        Err(error) if vanished(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Checked relative paths from browser folder uploads.  No path may escape,
/// become absolute, or name an empty component.
pub fn selected_path(root: &Path, relative: &str) -> Result<PathBuf, WorkspaceError> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
        || path.components().any(
            |component| matches!(component, Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git")),
        )
    {
        return Err(WorkspaceError::Unsafe(path.into()));
    }
    Ok(root.join(path))
}

/// Give a tree rebuilt from a candidate's files an empty repository of its
/// own, which a session's workspace needs. A candidate carries no history,
/// and nothing is committed here, so the repository claims no commit.
pub async fn init_empty_git(git: &str, root: &Path) -> Result<(), WorkspaceError> {
    run_git(git, Some(root), "init", ["init", "-q"]).await
}

async fn run_git<'a>(
    git: &str,
    dir: Option<&Path>,
    op: &'static str,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<(), WorkspaceError> {
    let mut command = Command::new(crate::config::path_guard::program("publish.git", git)?);
    command
        .env_clear()
        .env("PATH", crate::config::path_guard::child_path())
        .env("HOME", crate::config::Config::state_dir().join("git-home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        // A graft file truncates or rewrites parentage, so a seed clone would
        // carry a history that is not the one the source repository holds.
        .env("GIT_GRAFT_FILE", "/dev/null");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    command
        .arg("--no-replace-objects")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.useReplaceRefs=false",
            "-c",
            "credential.helper=",
        ])
        .args(args);
    let out = command.output().await?;
    if out.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::Git {
            op,
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Git directory holding every piece of metadata an agent can write
    /// into its own workspace and expect Git to act on.
    fn planted(root: &Path) {
        let git = root.join(".git");
        for dir in ["hooks", "info", "objects/info", "refs/replace"] {
            std::fs::create_dir_all(git.join(dir)).unwrap();
        }
        std::fs::write(
            git.join("config"),
            "[credential]\n\thelper = !sh -c 'touch /pwned'\n",
        )
        .unwrap();
        std::fs::write(
            git.join("config.worktree"),
            "[core]\n\tsshCommand = touch /a\n",
        )
        .unwrap();
        std::fs::write(git.join("commondir"), "../elsewhere\n").unwrap();
        std::fs::write(git.join("hooks/pre-commit"), "#!/bin/sh\ntouch /pwned\n").unwrap();
        std::fs::write(git.join("info/grafts"), "deadbeef\n").unwrap();
        std::fs::write(git.join("objects/info/alternates"), "/var/lib/objects\n").unwrap();
        std::fs::write(git.join("objects/info/http-alternates"), "http://x/\n").unwrap();
        std::fs::write(git.join("refs/replace/deadbeef"), "cafebabe\n").unwrap();
    }

    #[test]
    fn sanitize_git_removes_every_executable_metadata_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        planted(root);
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        std::fs::write(root.join(".git/refs/heads/main"), "deadbeef\n").unwrap();

        sanitize_git(root).unwrap();

        let git = root.join(".git");
        for gone in [
            "config.worktree",
            "commondir",
            "hooks",
            "info/grafts",
            "objects/info/alternates",
            "objects/info/http-alternates",
            "refs/replace",
        ] {
            assert!(!git.join(gone).exists(), "{gone} survived sanitize_git");
        }
        // Ordinary refs stay: commit identity must remain verifiable.
        assert!(git.join("refs/heads/main").exists());
        let config = std::fs::read_to_string(git.join("config")).unwrap();
        assert!(!config.contains("helper"), "{config}");
        assert!(config.contains("hooksPath = /dev/null"), "{config}");
    }

    /// `commondir` relocates `$GIT_COMMON_DIR`, and Git then reads *that*
    /// directory's `config` rather than the one `sanitize_git` writes. Left in
    /// place it hands the whole configuration file back to the agent, so the
    /// rewritten config alone proves nothing.
    #[test]
    fn a_planted_common_directory_cannot_select_the_configuration_git_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        let elsewhere = tmp.path().join("elsewhere.git");
        for dir in [root.join(".git/refs"), root.join(".git/objects")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(elsewhere.join("refs")).unwrap();
        std::fs::create_dir_all(elsewhere.join("objects")).unwrap();
        std::fs::write(elsewhere.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            elsewhere.join("config"),
            "[core]\n\tsshCommand = touch /pwned-by-commondir\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".git/commondir"),
            format!("{}\n", elsewhere.display()),
        )
        .unwrap();

        let ssh_command = || {
            let out = std::process::Command::new("git")
                .args(["-C", root.to_str().unwrap(), "config", "core.sshCommand"])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(
            ssh_command(),
            "touch /pwned-by-commondir",
            "the planted common directory should be live before sanitizing"
        );

        sanitize_git(&root).unwrap();

        assert_eq!(ssh_command(), "");
    }

    #[test]
    fn sanitize_git_scrubs_linked_worktree_and_submodule_git_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        planted(root);
        let linked = root.join(".git/worktrees/w1");
        let submodule = root.join(".git/modules/lib");
        std::fs::create_dir_all(linked.join("hooks")).unwrap();
        std::fs::create_dir_all(submodule.join("objects/info")).unwrap();
        std::fs::create_dir_all(submodule.join("hooks")).unwrap();
        std::fs::write(
            linked.join("config.worktree"),
            "[core]\n\tsshCommand = touch /a\n",
        )
        .unwrap();
        std::fs::write(linked.join("commondir"), "../..\n").unwrap();
        std::fs::write(linked.join("hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            submodule.join("config"),
            "[core]\n\tsshCommand = touch /a\n",
        )
        .unwrap();
        std::fs::write(submodule.join("hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        std::fs::write(submodule.join("objects/info/alternates"), "/var/lib\n").unwrap();
        std::fs::write(submodule.join("objects/keep"), "object\n").unwrap();

        sanitize_git(root).unwrap();

        assert!(!linked.join("config.worktree").exists());
        assert!(!linked.join("commondir").exists());
        assert!(!linked.join("hooks").exists());
        assert!(!submodule.join("config").exists());
        assert!(!submodule.join("hooks").exists());
        assert!(!submodule.join("objects/info/alternates").exists());
        // A submodule's objects are identity, not behavior; they stay.
        assert!(submodule.join("objects/keep").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_hooks_directory_is_unlinked_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        let operator = tmp.path().join("operator-hooks");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&operator).unwrap();
        std::fs::write(operator.join("pre-commit"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&operator, root.join(".git/hooks")).unwrap();

        sanitize_git(&root).unwrap();

        assert!(std::fs::symlink_metadata(root.join(".git/hooks")).is_err());
        assert!(
            operator.join("pre-commit").exists(),
            "the symlink target must not have been emptied"
        );
    }

    /// Every tree copied here is live. Any `git commit` leaves a detached
    /// `git maintenance run --auto` behind it, and that creates and removes
    /// `.git/objects/maintenance.lock` while the node is walking the same
    /// directory: treating a listed-then-gone entry as a failure lost whole
    /// imports to a lock file that was never part of the workspace.
    #[test]
    fn an_entry_that_vanishes_under_the_walk_does_not_fail_the_copy() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("workspace");
        std::fs::create_dir_all(source.join(".git/objects")).unwrap();
        for i in 0..200 {
            std::fs::write(source.join(format!("f{i}.txt")), "contents").unwrap();
        }
        let lock = source.join(".git/objects/maintenance.lock");
        let stop = Arc::new(AtomicBool::new(false));
        let churn = std::thread::spawn({
            let stop = stop.clone();
            move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::write(&lock, "1");
                    let _ = std::fs::remove_file(&lock);
                }
            }
        });

        let copies = (0..40)
            .try_for_each(|i| copy_tree(&source, &tmp.path().join(format!("copy-{i}")), false));

        stop.store(true, Ordering::Relaxed);
        churn.join().unwrap();
        copies.unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("copy-39/f0.txt")).unwrap(),
            "contents",
            "the entries that were there must still have been copied"
        );
    }

    /// The skip above is for entries that are gone, and for nothing else: a
    /// symbolic link is still a tree the node refuses to copy.
    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_source_is_still_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("workspace");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("a.txt"), "contents").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", source.join("escape")).unwrap();

        assert!(matches!(
            copy_tree(&source, &tmp.path().join("copy"), false),
            Err(WorkspaceError::Unsafe(_))
        ));
    }

    fn sh(dir: &Path, script: &str) {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn sh_out(dir: &Path, script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A graft file rewrites parentage for whatever reads the repository —
    /// including the `upload-pack` the seed clone runs inside the operator's
    /// own checkout. Honoured, it would seed the session from a history the
    /// source repository does not have.
    #[tokio::test]
    async fn a_graft_in_the_source_checkout_cannot_truncate_the_seeded_history() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        sh(
            &source,
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && echo one > a.txt && git add -A && git commit -qm one \
             && echo two >> a.txt && git add -A && git commit -qm two \
             && mkdir -p .git/info && git rev-parse HEAD > .git/info/grafts",
        );
        assert_eq!(
            sh_out(&source, "git rev-list --count HEAD"),
            "1",
            "the planted graft should be live"
        );

        let id = format!("seed-graft-{}", uuid::Uuid::now_v7());
        match seed_from_checkout("git", &source, "feat/x", None, &id).await {
            // Either outcome upholds the invariant: the clone asks for the
            // history the commits actually describe, so a source that will
            // only serve the grafted one is refused rather than quietly
            // seeding a session from it.
            Err(WorkspaceError::Git { op: "clone", .. }) => {}
            Ok(workspace) => {
                let stage = staging_path(&workspace.id);
                assert_eq!(sh_out(&stage, "git rev-list --count HEAD"), "2");
                let _ = std::fs::remove_dir_all(&stage);
            }
            Err(other) => panic!("{other}"),
        }
    }

    /// A checkout that has installed its dependencies holds symlinks and
    /// build output its ignore rules exclude; the overlay carries what Git
    /// would show and nothing it ignores.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_overlay_carries_untracked_work_but_nothing_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        sh(
            &source,
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && printf 'node_modules/\\n.env\\n' > .gitignore && echo one > a.txt \
             && git add -A && git commit -qm one \
             && echo edited > a.txt && echo new > b.txt && echo SECRET=1 > .env \
             && mkdir -p app/node_modules/.bin && ln -s ../acorn app/node_modules/.bin/acorn",
        );

        let id = format!("seed-overlay-{}", uuid::Uuid::now_v7());
        let workspace = seed_from_checkout("git", &source, "feat/x", None, &id)
            .await
            .unwrap();
        let stage = staging_path(&workspace.id);
        assert_eq!(
            std::fs::read_to_string(stage.join("a.txt")).unwrap(),
            "edited\n"
        );
        assert_eq!(
            std::fs::read_to_string(stage.join("b.txt")).unwrap(),
            "new\n"
        );
        assert!(!stage.join(".env").exists());
        assert!(!stage.join("app/node_modules").exists());
        let _ = std::fs::remove_dir_all(&stage);
    }

    /// A session that installed or built leaves dependency trees and build
    /// output in its workspace. An export takes what Git would show, so their
    /// symlinks never refuse the snapshot and their size never exhausts it.
    #[cfg(unix)]
    #[test]
    fn an_export_leaves_what_the_repository_ignores_behind() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("volume");
        std::fs::create_dir_all(&source).unwrap();
        sh(
            &source,
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && printf 'node_modules/\\ntarget/\\n*.log\\n' > .gitignore && echo one > a.txt \
             && printf '#!/bin/sh\\n' > run.sh && chmod +x run.sh && ln -s a.txt link.txt \
             && mkdir fixtures && echo kept > fixtures/run.log && git add -f fixtures/run.log \
             && echo gone > gone.txt && git add -A && git commit -qm one \
             && rm gone.txt && echo new > b.txt && echo noise > fixtures/new.log \
             && mkdir -p node_modules/.bin target/debug \
             && ln -s ../acorn node_modules/.bin/acorn && echo built > target/debug/app \
             && ln -s /usr/bin/python3 target/python",
        );
        // The whole volume is refused: a link in its build output leaves it.
        assert!(matches!(
            validate_tree(&source),
            Err(WorkspaceError::Unsafe(_))
        ));

        let exported = tmp.path().join("snapshot");
        std::fs::create_dir_all(&exported).unwrap();
        std::fs::write(exported.join("stale.txt"), "from an earlier export").unwrap();
        copy_checkout(&source, &exported).unwrap();
        validate_tree(&exported).unwrap();
        assert_eq!(
            std::fs::read_to_string(exported.join("b.txt")).unwrap(),
            "new\n"
        );
        assert!(exported.join(".git/HEAD").is_file());
        // Tracked, though an ignore rule matches it, as `git ls-files` says.
        assert_eq!(
            std::fs::read_to_string(exported.join("fixtures/run.log")).unwrap(),
            "kept\n"
        );
        assert_eq!(
            std::fs::read_link(exported.join("link.txt")).unwrap(),
            Path::new("a.txt")
        );
        let mode = std::fs::metadata(exported.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "{mode:o}");
        for absent in [
            "node_modules",
            "target",
            "gone.txt",
            "stale.txt",
            "fixtures/new.log",
        ] {
            assert!(!exported.join(absent).exists(), "{absent} was exported");
        }

        // A volume that is not a repository has no rules to apply.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join("target")).unwrap();
        std::fs::write(plain.join("target/out"), "kept").unwrap();
        let copied = tmp.path().join("plain-copy");
        copy_checkout(&plain, &copied).unwrap();
        assert!(copied.join("target/out").is_file());
    }

    /// The index is read as a file, in every shape Git writes it: the
    /// extended flags an intent-to-add entry sets (version 3), prefix-compressed
    /// names (version 4), and a split index whose entries live in a shared
    /// file beside it.
    #[test]
    fn every_index_version_names_its_tracked_files() {
        for setup in [
            "",
            "git add -N -f intent.txt",
            "git update-index --index-version 4",
            "git update-index --split-index && echo more > dir/more.txt && git add -f dir/more.txt",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let source = tmp.path();
            sh(
                source,
                "git init -q -b main . && git config user.email t@e && git config user.name t \
                 && printf 'dir/\\n*.txt\\n' > .gitignore && mkdir -p dir/deeper \
                 && echo a > dir/a.txt && echo b > dir/deeper/b.txt && echo i > intent.txt \
                 && git add -f dir && git add .gitignore && git commit -qm one",
            );
            if !setup.is_empty() {
                sh(source, setup);
            }
            if setup.contains("split") {
                assert!(std::fs::read_dir(source.join(".git"))
                    .unwrap()
                    .flatten()
                    .any(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("sharedindex.")));
            }
            let mut tracked = tracked_paths(&source.join(".git")).unwrap();
            tracked.sort();
            let mut expected = vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("dir/a.txt"),
                PathBuf::from("dir/deeper/b.txt"),
            ];
            if setup.contains("intent") {
                expected.push(PathBuf::from("intent.txt"));
            }
            if setup.contains("more") {
                expected.push(PathBuf::from("dir/more.txt"));
            }
            expected.sort();
            tracked.dedup();
            assert_eq!(tracked, expected, "after `{setup}`");
        }
        assert!(parse_index(b"DIRC\0\0\0\x02\0\0\0\x05", 20, &mut Vec::new()).is_none());
        assert!(parse_index(b"not an index", 20, &mut Vec::new()).is_none());
    }

    #[test]
    fn a_link_may_climb_only_at_its_start_and_never_out_of_the_tree() {
        for (target, depth) in [
            ("../acorn/bin/acorn", 2),
            ("../../.pnpm/x@1/node_modules/x", 2),
            ("a.txt", 0),
            ("./a.txt", 0),
            ("..", 1),
        ] {
            assert!(
                link_stays_inside(Path::new(target), depth),
                "{target} at depth {depth}"
            );
        }
        for (target, depth) in [
            ("/etc/passwd", 3),
            ("..", 0),
            ("../../x", 1),
            ("x/../../y", 2),
            ("link/..", 3),
            ("sub/../", 0),
            ("", 0),
        ] {
            assert!(
                !link_stays_inside(Path::new(target), depth),
                "{target} at depth {depth}"
            );
        }
    }

    /// What Git records of a file survives a copy: whether it is executable,
    /// and a link that stays inside the tree, as the link it is. Write bits do
    /// not: a read-only snapshot is copied into something a run can write.
    #[cfg(unix)]
    #[test]
    fn a_copy_keeps_executable_bits_and_links_inside_the_tree() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("workspace");
        let bin = source.join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(source.join("node_modules/acorn/bin")).unwrap();
        std::fs::write(source.join("node_modules/acorn/bin/acorn"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            source.join("node_modules/acorn/bin/acorn"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        std::fs::write(source.join("README"), "read only").unwrap();
        std::fs::set_permissions(
            source.join("README"),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        std::os::unix::fs::symlink("../acorn/bin/acorn", bin.join("acorn")).unwrap();

        let copy = tmp.path().join("copy");
        copy_tree(&source, &copy, false).unwrap();
        let mode = |path: &str| {
            std::fs::metadata(copy.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("node_modules/acorn/bin/acorn"), 0o755);
        assert_eq!(mode("README"), 0o644);
        assert_eq!(
            std::fs::read_link(copy.join("node_modules/.bin/acorn")).unwrap(),
            Path::new("../acorn/bin/acorn")
        );
        validate_tree(&source).unwrap();

        // A link that climbs out of the tree is refused wherever it sits.
        std::os::unix::fs::symlink("../../../../outside", bin.join("escape")).unwrap();
        assert!(matches!(
            copy_tree(&source, &tmp.path().join("copy-2"), false),
            Err(WorkspaceError::Unsafe(_))
        ));
    }

    /// Uploads and continuity transfers name their own relative paths, and a
    /// workspace seeded from them gets a Git directory the node creates. No
    /// path from either may write into one.
    #[test]
    fn selected_paths_cannot_name_git_metadata() {
        let root = Path::new("/tmp/does-not-need-to-exist");
        for refused in [
            ".git",
            ".git/config",
            ".git/hooks/pre-commit",
            "a/.git/config",
            ".GIT/config",
            "../escape",
            "/absolute",
            "",
        ] {
            assert!(
                matches!(selected_path(root, refused), Err(WorkspaceError::Unsafe(_))),
                "{refused} should be refused"
            );
        }
        assert_eq!(
            selected_path(root, "src/main.rs").unwrap(),
            root.join("src/main.rs")
        );
    }

    /// A `.git` that is a file is a `gitdir:` pointer at a directory outside
    /// the workspace, where there is nothing to sanitize. The workspace is
    /// refused rather than exported with the pointer intact.
    #[test]
    fn a_git_file_pointing_elsewhere_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join(".git"), "gitdir: /etc\n").unwrap();
        assert!(matches!(sanitize_git(root), Err(WorkspaceError::Unsafe(_))));
    }
}
