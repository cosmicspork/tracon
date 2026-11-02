//! Workspaces and harness state on a sandbox's own disk, moved over the
//! sandbox protocol's files channel (`[runtime.podman] sandbox_files`).
//!
//! Without it a sandbox mounts runtime volumes, as a harness container
//! always has. With it, the volumes a run works in are directories the node
//! keeps (`FilesStore`, under the state directory), and a sandbox never
//! mounts them: the runner sends each one in when the sandbox has connected,
//! onto storage that goes with the sandbox, and takes back what the run
//! changed. Which volumes (named, as every runtime resource, with the node's
//! `[boundary] resource_prefix`): a session's workspace (`<prefix>-workspace-…`)
//! and scratch (`<prefix>-scratch-…`: its configuration, orientation,
//! gitconfig and its own harness state), and a check's or a preparation's
//! tree (`<prefix>-check-…`, `<prefix>-prep-…`). Caches and everything else
//! stay volumes.
//!
//! **What comes back, and when.** Only a session's or a preparation's
//! writable trees, never a check's or a probe's, and never a read-only one:
//!
//! - a session's workspace as its checkout (what Git tracks, the untracked
//!   files its ignore rules leave in, and the repository), at each turn end,
//!   every [`SYNC_EVERY`] while a turn runs, and when the sandbox ends. What
//!   the ignore rules leave out stays in the sandbox and goes with it, except
//!   what the node already held (a preparation's install), which is kept;
//! - a session's own harness state (its transcript, its journal and
//!   database) as a whole tree, at the same moments. A session's sandbox
//!   only ever holds its own session's state, so what comes back is its own
//!   subtree by construction, and it is written only under that session's
//!   volume, under the session's state fence;
//! - a preparation's tree whole when it ends, since its install is the point.
//!
//! **What a lost sandbox costs.** Each sync replaces the node's copy only
//! once the whole archive has been received and checked, so a sandbox that
//! dies (or a node that restarts) mid-run loses what changed since its last
//! completed sync: at most one turn, or [`SYNC_EVERY`] of a long one.
//!
//! **Nothing from the sandbox is trusted.** Every archive is unpacked by
//! `tracon_sandbox::files::unpack` with the rules `workspace::validate_tree`
//! holds a runtime's tree to (regular files, directories and links only; no
//! absolute or climbing path; every link resolving inside the tree; nothing
//! written through a link; bounded bytes, files and directories) into a
//! staging directory, and only a tree that passed replaces anything. A
//! sandbox can say nothing about where its trees go: the node decides that
//! from the mounts it gave the run.
//!
//! **Read-only is not enforced inside.** A mount's read-only bit becomes a
//! tree the node sends at every start and never reads back: what the harness
//! does to its sealed configuration lasts as long as its sandbox.
//!
//! Exports while the sandbox runs (a review's snapshot, `show_work`, the
//! UI's diff) read the live tree straight from the sandbox
//! ([`export_live`]); with none running they read the node's copy.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::AsyncRead;
use tracon_sandbox::files::{self as tree, Limits};
use tracon_sandbox::node::SandboxConn;
use tracon_sandbox::protocol::{Exclusions, TreeSummary};

use super::{Mount, RunKind};

/// How often a session's state is taken back while a turn runs.
pub const SYNC_EVERY: Duration = Duration::from_secs(60);

/// How long one sync may take before it is abandoned (and the node keeps the
/// copy it had).
const SYNC_WITHIN: Duration = Duration::from_secs(120);

/// Bounds for a tree that carries what a preparation installed
/// (`node_modules`, `vendor/`): a checkout's limits are far below it.
pub const PREPARED_LIMITS: Limits = Limits {
    max_bytes: 8 * 1024 * 1024 * 1024,
    max_files: 1_000_000,
    max_dirs: 500_000,
    max_path_bytes: 4096,
};

/// Bounds for a checkout and for harness state: the node's own import limits.
pub fn checkout_limits() -> Limits {
    Limits {
        max_bytes: crate::workspace::MAX_IMPORT_BYTES,
        max_files: crate::workspace::MAX_IMPORT_FILES,
        ..Limits::default()
    }
}

/// The part of a volume's name after this node's resource prefix, when it
/// is one of this node's.
fn own(volume: &str) -> Option<&str> {
    volume
        .strip_prefix(crate::config::resource_prefix())?
        .strip_prefix('-')
}

/// Whether, with files over the protocol, a volume of this name is one the
/// node keeps rather than the runtime.
pub fn backs(volume: &str) -> bool {
    own(volume).is_some_and(|rest| {
        ["workspace-", "scratch-", "check-", "prep-"]
            .iter()
            .any(|kind| rest.starts_with(kind))
            && !rest.starts_with("prep-cache-")
    })
}

/// Whether a volume is a check's own copy, which nothing reads back and
/// which may therefore be the very tree it was copied from.
pub fn is_check(volume: &str) -> bool {
    own(volume).is_some_and(|rest| rest.starts_with("check-"))
}

fn is_workspace(volume: &str) -> bool {
    own(volume).is_some_and(|rest| rest.starts_with("workspace-"))
}

#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    #[error("not a volume name: {0}")]
    Name(String),
    #[error("not a relative path inside a volume: {0}")]
    SubPath(String),
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Tree(#[from] tree::FilesError),
    #[error(transparent)]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error(transparent)]
    Sandbox(#[from] tracon_sandbox::node::SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// How a writable tree comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The checkout replaces the node's, keeping what the node held that the
    /// ignore rules leave out.
    Checkout,
    /// The whole tree replaces the node's.
    Whole,
}

impl Mode {
    fn exclusions(self) -> Exclusions {
        match self {
            Mode::Checkout => Exclusions::Checkout,
            Mode::Whole => Exclusions::None,
        }
    }
}

/// What a run of `kind` takes back from a writable `volume`, if anything.
pub fn sync_mode(volume: &str, kind: RunKind) -> Option<(Mode, Limits)> {
    if is_check(volume) {
        return None;
    }
    match kind {
        RunKind::Session if is_workspace(volume) => Some((Mode::Checkout, checkout_limits())),
        RunKind::Session => Some((Mode::Whole, checkout_limits())),
        RunKind::Prepare => Some((Mode::Whole, PREPARED_LIMITS)),
        RunKind::Check | RunKind::Probe => None,
    }
}

/// The directories the node keeps in place of runtime volumes, one per
/// volume, under one root. A check's copy may instead be an alias (a symbolic
/// link) of the tree it copies, which nothing writes through.
#[derive(Debug, Clone)]
pub struct FilesStore {
    root: PathBuf,
    every: Duration,
}

impl FilesStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FilesStore {
            root: root.into(),
            every: SYNC_EVERY,
        }
    }

    /// This node's, under its state directory.
    pub fn for_node() -> Self {
        Self::new(crate::config::Config::state_dir().join("sandbox-files"))
    }

    /// Take state back this often during a turn rather than every
    /// [`SYNC_EVERY`].
    pub fn sync_every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, volume: &str) -> Result<PathBuf, FilesError> {
        if volume.is_empty()
            || volume.contains('/')
            || volume.contains("..")
            || volume.contains(".staging-")
        {
            return Err(FilesError::Name(volume.into()));
        }
        Ok(self.root.join(volume))
    }

    /// Whether this store keeps the volume.
    pub fn holds(&self, volume: &str) -> bool {
        self.path(volume)
            .is_ok_and(|path| std::fs::symlink_metadata(path).is_ok())
    }

    fn is_alias(&self, volume: &str) -> bool {
        self.path(volume)
            .ok()
            .and_then(|path| std::fs::symlink_metadata(path).ok())
            .is_some_and(|meta| meta.file_type().is_symlink())
    }

    /// Where the volume's tree is: its own directory, or the one it is an
    /// alias of.
    pub fn tree(&self, volume: &str) -> Option<PathBuf> {
        let path = self.path(volume).ok()?;
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink() {
            std::fs::read_link(&path).ok()
        } else if meta.is_dir() {
            Some(path)
        } else {
            None
        }
    }

    /// Copy a node staging directory into the volume, over what is there (as
    /// Podman's own copy into a volume does), checked as it is copied.
    pub fn import(&self, volume: &str, source: &Path) -> Result<(), FilesError> {
        let dest = self.path(volume)?;
        std::fs::create_dir_all(&self.root)?;
        if self.is_alias(volume) {
            std::fs::remove_file(&dest)?;
        }
        if std::fs::symlink_metadata(&dest).is_ok() {
            // Checked whole first, so a refused tree changes nothing.
            crate::workspace::validate_tree(source)?;
            crate::workspace::copy_tree(source, &dest, false)?;
            return Ok(());
        }
        let staging = self.staging(volume);
        if let Err(error) = crate::workspace::copy_tree(source, &staging, false) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error.into());
        }
        std::fs::rename(&staging, &dest)?;
        Ok(())
    }

    /// Make the volume an alias of `target`, a tree that stays put for as
    /// long as the volume is used and that is never written through it.
    pub fn alias(&self, volume: &str, target: &Path) -> Result<(), FilesError> {
        let dest = self.path(volume)?;
        std::fs::create_dir_all(&self.root)?;
        self.remove(volume)?;
        std::os::unix::fs::symlink(target, &dest)?;
        Ok(())
    }

    /// Copy the volume out to `dest`: its checkout, or all of it. Checked as
    /// it is copied, as any tree a runtime hands the node.
    pub fn export(&self, volume: &str, dest: &Path, checkout: bool) -> Result<(), FilesError> {
        let source = self
            .tree(volume)
            .ok_or_else(|| FilesError::Refused(format!("no volume {volume}")))?;
        if checkout {
            crate::workspace::copy_checkout(&source, dest)?;
        } else {
            if std::fs::symlink_metadata(dest).is_ok() {
                std::fs::remove_dir_all(dest)?;
            }
            crate::workspace::copy_tree(&source, dest, false)?;
        }
        Ok(())
    }

    /// Make `dest` a copy of `source`, both kept here. A check's copy is an
    /// alias, since the check's own changes are never read back; any other
    /// is a copy, sharing extents where the filesystem can.
    pub async fn clone_volume(&self, source: &str, dest: &str) -> Result<(), FilesError> {
        let from = self
            .tree(source)
            .ok_or_else(|| FilesError::Refused(format!("no volume {source}")))?;
        if is_check(dest) {
            return self.alias(dest, &from);
        }
        let to = self.path(dest)?;
        self.remove(dest)?;
        let staging = self.staging(dest);
        std::fs::create_dir_all(&staging)?;
        let copied = tokio::process::Command::new("cp")
            .args(["-dRp", "--reflink=auto", "--"])
            .arg(from.join("."))
            .arg(&staging)
            .output()
            .await;
        if !copied.is_ok_and(|out| out.status.success()) {
            let _ = std::fs::remove_dir_all(&staging);
            crate::workspace::copy_tree(&from, &staging, false)?;
        }
        std::fs::rename(&staging, &to)?;
        Ok(())
    }

    /// Remove the volume (an alias goes, not what it names). One that does
    /// not exist is already removed.
    pub fn remove(&self, volume: &str) -> Result<(), FilesError> {
        let path = self.path(volume)?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path)?,
            Ok(_) => std::fs::remove_file(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    /// The volumes kept here.
    pub fn list(&self) -> Result<Vec<crate::boundary::VolumeInfo>, FilesError> {
        crate::boundary::directory_volumes(&self.root)
            .map_err(|error| FilesError::Refused(error.to_string()))
    }

    fn staging(&self, volume: &str) -> PathBuf {
        self.root.join(format!(
            "{volume}.staging-{}",
            uuid::Uuid::now_v7().simple()
        ))
    }

    /// Unpack an archive a sandbox sent of the tree it holds at
    /// `volume`/`sub_path`, and make it the node's copy. Nothing is changed
    /// unless the whole archive passes. `nested` are paths inside it that
    /// other mounts provide, which the node sends at every start and never
    /// takes back.
    pub async fn receive<R>(
        &self,
        volume: &str,
        sub_path: &str,
        reader: R,
        mode: Mode,
        limits: Limits,
        nested: &[PathBuf],
    ) -> Result<TreeSummary, FilesError>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let base = self.path(volume)?;
        if self.is_alias(volume) {
            return Err(FilesError::Refused(format!(
                "{volume} is an alias of another tree and is never written"
            )));
        }
        let relative = relative(sub_path)?;
        std::fs::create_dir_all(&base)?;
        real_directories(&base, &relative)?;
        let dest = base.join(&relative);
        let staging = self.staging(volume);
        std::fs::create_dir(&staging)?;
        let summary = match tree::unpack_async(reader, staging.clone(), limits).await {
            Ok(summary) => summary,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error.into());
            }
        };
        let root = self.root.clone();
        let nested = nested.to_vec();
        let committed = tokio::task::spawn_blocking(move || {
            let committed = commit(&root, &dest, &staging, mode, &nested);
            if committed.is_err() {
                let _ = std::fs::remove_dir_all(&staging);
            }
            committed
        })
        .await
        .map_err(|e| FilesError::Io(std::io::Error::other(e)))?;
        committed?;
        Ok(summary)
    }

    /// Split a run's mounts into what the provider mounts and what goes over
    /// the files channel: every mount of a volume kept here. Each directory
    /// sent in lands on storage of the sandbox's own ([`Mount::local`]),
    /// unless it is inside another that does.
    pub fn plan(&self, mounts: &[Mount]) -> Result<(Vec<Mount>, Option<Plan>), FilesError> {
        let mut provided = Vec::new();
        let mut placed = Vec::new();
        for mount in mounts {
            let Some(tree) = self.tree(&mount.volume) else {
                provided.push(mount.clone());
                continue;
            };
            let source = tree.join(relative(&mount.sub_path)?);
            let file = std::fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_file());
            placed.push(Placed {
                volume: mount.volume.clone(),
                sub_path: mount.sub_path.clone(),
                target: mount.target.clone(),
                read_only: mount.read_only,
                source,
                file,
            });
        }
        if placed.is_empty() {
            return Ok((provided, None));
        }
        placed.sort_by_key(|p| Path::new(&p.target).components().count());
        let mut local: Vec<String> = Vec::new();
        for p in placed.iter().filter(|p| !p.file) {
            let inside_another = placed
                .iter()
                .any(|o| !o.file && o.target != p.target && inside(&p.target, &o.target));
            if !inside_another && !local.contains(&p.target) {
                local.push(p.target.clone());
            }
        }
        provided.extend(local.into_iter().map(Mount::local));
        Ok((
            provided,
            Some(Plan {
                placed,
                store: self.clone(),
            }),
        ))
    }
}

/// `sub_path` as a relative path with nothing but plain components.
fn relative(sub_path: &str) -> Result<PathBuf, FilesError> {
    let mut clean = PathBuf::new();
    for component in Path::new(sub_path).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err(FilesError::SubPath(sub_path.into())),
        }
    }
    Ok(clean)
}

/// Refuse a path whose existing ancestors under `base` are anything but
/// real directories: a link there would put a tree somewhere else.
fn real_directories(base: &Path, relative: &Path) -> Result<(), FilesError> {
    let mut at = base.to_path_buf();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        at.push(component);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(FilesError::Refused(format!(
                    "{} is not a directory",
                    at.display()
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Whether `path` is strictly inside `dir`.
fn inside(path: &str, dir: &str) -> bool {
    Path::new(path) != Path::new(dir) && Path::new(path).starts_with(dir)
}

/// Finish a received tree in `staging` and put it at `dest`.
fn commit(
    root: &Path,
    dest: &Path,
    staging: &Path,
    mode: Mode,
    nested: &[PathBuf],
) -> Result<(), FilesError> {
    for path in nested {
        let at = staging.join(path);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&at)?,
            Ok(_) => std::fs::remove_file(&at)?,
            Err(_) => {}
        }
    }
    if mode == Mode::Checkout {
        carry_ignored(dest, staging, nested)?;
    }
    match std::fs::symlink_metadata(dest) {
        Ok(_) => {
            let old = root.join(format!(
                "{}.staging-{}-old",
                dest.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                uuid::Uuid::now_v7().simple()
            ));
            std::fs::rename(dest, &old)?;
            if let Err(error) = std::fs::rename(staging, dest) {
                let _ = std::fs::rename(&old, dest);
                return Err(error.into());
            }
            let _ = std::fs::remove_dir_all(&old);
        }
        Err(_) => {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(staging, dest)?;
        }
    }
    Ok(())
}

/// Move into `new` (a checkout just received) what `old` (the node's copy)
/// holds that its checkout leaves out, where `new` has nothing in its place:
/// a preparation's install survives the checkout that comes back each turn.
/// Moved, never read: nothing here follows a link or opens a file.
fn carry_ignored(old: &Path, new: &Path, nested: &[PathBuf]) -> Result<(), FilesError> {
    if !std::fs::symlink_metadata(old).is_ok_and(|meta| meta.is_dir()) {
        return Ok(());
    }
    let Some(keep) = tree::kept_paths(old)? else {
        // Not a repository: the checkout was the whole tree.
        return Ok(());
    };
    carry_dir(old, new, Path::new(""), &keep, nested)
}

fn carry_dir(
    old: &Path,
    new: &Path,
    relative: &Path,
    keep: &std::collections::HashSet<PathBuf>,
    nested: &[PathBuf],
) -> Result<(), FilesError> {
    let entries = match std::fs::read_dir(old.join(relative)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let path = relative.join(&name);
        if relative.as_os_str().is_empty() && name.to_string_lossy().eq_ignore_ascii_case(".git") {
            continue;
        }
        if nested.iter().any(|n| n == &path) {
            continue;
        }
        let meta = std::fs::symlink_metadata(old.join(&path))?;
        if keep.contains(&path) {
            if meta.is_dir() {
                carry_dir(old, new, &path, keep, nested)?;
            }
            continue;
        }
        let to = new.join(&path);
        if std::fs::symlink_metadata(&to).is_ok() || real_directories(new, &path).is_err() {
            continue;
        }
        if let Some(parent) = path.parent() {
            if !std::fs::symlink_metadata(new.join(parent)).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
        }
        std::fs::rename(old.join(&path), to)?;
    }
    Ok(())
}

/// One mount a sandbox gets over the files channel.
#[derive(Debug, Clone)]
struct Placed {
    volume: String,
    sub_path: String,
    target: String,
    read_only: bool,
    /// Its tree (or file) on the node.
    source: PathBuf,
    file: bool,
}

/// What one sandbox is sent, from [`FilesStore::plan`].
pub struct Plan {
    placed: Vec<Placed>,
    store: FilesStore,
}

impl Plan {
    /// The targets it places, parents first.
    pub fn targets(&self) -> Vec<&str> {
        self.placed.iter().map(|p| p.target.as_str()).collect()
    }

    /// Send every tree in, then begin taking back what a run of `kind` takes
    /// back. `name` is the run's, which a session's turns are announced by.
    pub async fn attach(
        self,
        conn: &SandboxConn,
        name: &str,
        kind: RunKind,
    ) -> Result<Attached, FilesError> {
        for p in &self.placed {
            let lock = crate::workspace::volume_lock(&p.volume);
            let _guard = lock.lock().await;
            send(conn, p).await?;
        }
        let syncs: Vec<Sync> = self
            .placed
            .iter()
            .filter(|p| !p.read_only && !p.file)
            .filter_map(|p| {
                let (mode, limits) = sync_mode(&p.volume, kind)?;
                let nested = self
                    .placed
                    .iter()
                    .filter(|o| inside(&o.target, &p.target))
                    .filter_map(|o| Path::new(&o.target).strip_prefix(&p.target).ok())
                    .map(Path::to_path_buf)
                    .collect();
                Some(Sync {
                    volume: p.volume.clone(),
                    sub_path: p.sub_path.clone(),
                    target: p.target.clone(),
                    mode,
                    limits,
                    nested,
                })
            })
            .collect();
        let inner = Arc::new(Inner {
            conn: conn.clone(),
            name: name.to_string(),
            store: self.store.clone(),
            syncs,
            turn: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            syncing: tokio::sync::Mutex::new(()),
            periodic: Mutex::new(None),
        });
        if kind == RunKind::Session && !inner.syncs.is_empty() {
            register(&inner);
            let every = self.store.every;
            let periodic =
                tokio::spawn(periodic(Arc::downgrade(&inner), inner.conn.clone(), every));
            *inner.periodic.lock().unwrap() = Some(periodic);
        }
        Ok(Attached { inner })
    }
}

/// Send one tree (or file) into the sandbox.
async fn send(conn: &SandboxConn, p: &Placed) -> Result<(), FilesError> {
    if p.file {
        // A file is sent as a tree of one, under its target's name, into the
        // directory that holds it.
        let target = Path::new(&p.target);
        let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
            return Err(FilesError::Refused(format!(
                "cannot place a file at {}",
                p.target
            )));
        };
        let one = tempfile::tempdir()?;
        let meta = std::fs::symlink_metadata(&p.source)?;
        std::fs::copy(&p.source, one.path().join(name))?;
        std::fs::set_permissions(one.path().join(name), meta.permissions())?;
        conn.import_dir(one.path(), parent, Exclusions::None)
            .await?;
        return Ok(());
    }
    if std::fs::symlink_metadata(&p.source).is_ok_and(|meta| meta.is_dir()) {
        conn.import_dir_within(
            &p.source,
            &p.target,
            Exclusions::None,
            Some(PREPARED_LIMITS),
        )
        .await?;
    } else {
        // Nothing kept there yet: an empty directory, as a fresh volume is.
        conn.import_tar(&p.target, &[0u8; 1024][..], None).await?;
    }
    Ok(())
}

/// One writable tree a sandbox gives back.
#[derive(Debug, Clone)]
struct Sync {
    volume: String,
    sub_path: String,
    target: String,
    mode: Mode,
    limits: Limits,
    nested: Vec<PathBuf>,
}

struct Inner {
    conn: SandboxConn,
    name: String,
    store: FilesStore,
    syncs: Vec<Sync>,
    /// A turn is running: the periodic sync runs only then.
    turn: AtomicBool,
    finished: AtomicBool,
    /// One sync at a time per sandbox.
    syncing: tokio::sync::Mutex<()>,
    periodic: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Inner {
    async fn sync(&self, why: &str) -> Result<(), FilesError> {
        let _one = self.syncing.lock().await;
        if self.conn.is_closed() {
            return Err(FilesError::Refused("the sandbox is gone".into()));
        }
        let started = std::time::Instant::now();
        for sync in &self.syncs {
            let lock = crate::workspace::volume_lock(&sync.volume);
            let _guard = lock.lock().await;
            let reader = self
                .conn
                .export_reader(&sync.target, sync.mode.exclusions())
                .await?;
            self.store
                .receive(
                    &sync.volume,
                    &sync.sub_path,
                    reader,
                    sync.mode,
                    sync.limits,
                    &sync.nested,
                )
                .await?;
        }
        tracing::debug!(
            sandbox = %self.name,
            why,
            ms = started.elapsed().as_millis() as u64,
            "took the sandbox's trees back"
        );
        Ok(())
    }

    /// `sync`, bounded and logged rather than returned: a failed sync keeps
    /// the node's last copy, which is what a caller could do about it.
    async fn sync_logged(&self, why: &str) -> bool {
        match tokio::time::timeout(SYNC_WITHIN, self.sync(why)).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::warn!(sandbox = %self.name, why, %error, "could not take the sandbox's trees back; the node keeps its last copy");
                false
            }
            Err(_) => {
                tracing::warn!(sandbox = %self.name, why, "taking the sandbox's trees back timed out; the node keeps its last copy");
                false
            }
        }
    }

    async fn finish(self: &Arc<Self>) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(periodic) = self.periodic.lock().unwrap().take() {
            periodic.abort();
        }
        if !self.syncs.is_empty() {
            self.sync_logged("exit").await;
        }
        unregister(self);
    }
}

async fn periodic(inner: std::sync::Weak<Inner>, conn: SandboxConn, every: Duration) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = conn.closed() => return,
        }
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if inner.turn.load(Ordering::SeqCst) {
            inner.sync_logged("during a turn").await;
        }
    }
}

/// The files of one running sandbox. Its last sync runs at [`Attached::finish`],
/// before the sandbox is removed.
pub struct Attached {
    inner: Arc<Inner>,
}

impl Attached {
    /// Take the trees back one last time and stop. Idempotent.
    pub async fn finish(&self) {
        self.inner.finish().await;
    }

    /// Take the trees back now, as a turn's end does.
    pub async fn sync(&self) -> Result<(), FilesError> {
        self.inner.sync("asked").await
    }
}

#[derive(Default)]
struct Registry {
    /// A session's sandbox by its run's name.
    by_name: HashMap<String, Arc<Inner>>,
    /// A session's sandbox and where in it, by the workspace volume it holds.
    by_volume: HashMap<String, (Arc<Inner>, String)>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn register(inner: &Arc<Inner>) {
    let mut registry = registry().lock().unwrap();
    registry.by_name.insert(inner.name.clone(), inner.clone());
    for sync in &inner.syncs {
        if sync.sub_path.is_empty() {
            registry
                .by_volume
                .insert(sync.volume.clone(), (inner.clone(), sync.target.clone()));
        }
    }
}

fn unregister(inner: &Arc<Inner>) {
    let mut registry = registry().lock().unwrap();
    if registry
        .by_name
        .get(&inner.name)
        .is_some_and(|held| Arc::ptr_eq(held, inner))
    {
        registry.by_name.remove(&inner.name);
    }
    registry
        .by_volume
        .retain(|_, (held, _)| !Arc::ptr_eq(held, inner));
}

fn named(name: &str) -> Option<Arc<Inner>> {
    registry().lock().unwrap().by_name.get(name).cloned()
}

/// A session's turn has begun in the sandbox of this name: its state is
/// taken back every [`SYNC_EVERY`] until the turn ends. Nothing for a run
/// whose files are mounted.
pub fn turn_began(name: &str) {
    if let Some(inner) = named(name) {
        inner.turn.store(true, Ordering::SeqCst);
    }
}

/// A session's turn has ended: its trees are taken back now.
pub async fn turn_ended(name: &str) {
    if let Some(inner) = named(name) {
        inner.turn.store(false, Ordering::SeqCst);
        inner.sync_logged("turn end").await;
    }
}

/// The sandbox of this name is about to be removed: take its trees back
/// first.
pub async fn finish_named(name: &str) {
    if let Some(inner) = named(name) {
        inner.finish().await;
    }
}

/// Whether a running sandbox holds this volume.
pub fn is_live(volume: &str) -> bool {
    registry().lock().unwrap().by_volume.contains_key(volume)
}

/// Export a volume a running sandbox holds from the sandbox itself, checked
/// as it is unpacked, into `dest` (replaced only once the whole archive
/// passed). `None` when no running sandbox holds it, or the one that did is
/// gone: the node's copy is then the one to read.
pub async fn export_live(
    volume: &str,
    dest: &Path,
    checkout: bool,
) -> Option<Result<TreeSummary, FilesError>> {
    let (inner, target) = registry().lock().unwrap().by_volume.get(volume).cloned()?;
    let exclusions = if checkout {
        Exclusions::Checkout
    } else {
        Exclusions::None
    };
    let limits = if checkout {
        checkout_limits()
    } else {
        PREPARED_LIMITS
    };
    match inner
        .conn
        .export_tar(&target, exclusions, dest, limits)
        .await
    {
        Ok(summary) => Some(Ok(summary)),
        Err(_) if inner.conn.is_closed() => None,
        Err(error) => Some(Err(error.into())),
    }
}

#[cfg(test)]
mod tests;
