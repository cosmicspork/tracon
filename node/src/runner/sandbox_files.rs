//! Workspaces and harness state on a sandbox's own disk, moved over the
//! sandbox protocol's files channel: on every Podman and Apple sandbox, and
//! a pod over the protocol.
//!
//! The volumes a run works in are directories the node
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
//! - a preparation's tree whole when it ends, since its install is the point:
//!   a preparation whose tree does not come back has failed.
//!
//! **What a lost sandbox costs.** Each sync replaces the node's copy only
//! once the whole archive has been received and checked, so a sandbox that
//! dies (or a node that restarts) mid-run loses what changed since its last
//! completed sync: at most one turn, or [`SYNC_EVERY`] of a long one.
//!
//! **How the syncs went is the session's to see.** Every take-back is
//! reported ([`set_reporter`]): the session manager records the first failure
//! after a success, the success that ends a run of failures and the last
//! take-back as `workspace_sync` events, and a third failure in a row tells
//! the operator. [`sync_status`] is what the session API says of a running
//! sandbox's copy: when it last matched, and how many syncs have failed since.
//!
//! **Nothing from the sandbox is trusted.** Every archive is unpacked by
//! `tracon_sandbox::files::unpack` with the rules `workspace::validate_tree`
//! holds a runtime's tree to (regular files, directories and links only; no
//! absolute or climbing path; every link resolving inside the tree, an
//! absolute one read against where the tree is mounted, except in a
//! preparation's tree, whose links may leave it and are never followed;
//! nothing written through a link; bounded bytes, files and directories)
//! into a staging directory, and only a tree that passed replaces anything. A
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
pub const SYNC_WITHIN: Duration = Duration::from_secs(120);

/// Why the last take-back of a sandbox runs, as its report says.
pub const WHY_EXIT: &str = "exit";

/// Bounds for a tree that carries what a preparation installed
/// (`node_modules`, `vendor/`): a checkout's limits are far below it. Its
/// links may leave it (a virtualenv's interpreter is the image's), kept as
/// links: the node never follows one, and a review or publication exports
/// the checkout, whose links are held to stay inside.
pub const PREPARED_LIMITS: Limits = Limits {
    max_bytes: 8 * 1024 * 1024 * 1024,
    max_files: 1_000_000,
    max_dirs: 500_000,
    max_path_bytes: 4096,
    inert_links: true,
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

/// Whether a volume is a package cache (a session's, a repository's, a
/// preparation's run cache): what a preparation fills and a session only
/// reads from.
fn is_cache(volume: &str) -> bool {
    own(volume).is_some_and(|rest| rest.starts_with("cache-") || rest.starts_with("prep-cache-"))
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
        // Sent in, so a session's installs find what a preparation fetched,
        // and never taken back: a cache is too large to return every turn,
        // and nothing a session leaves in one is anything another needs.
        RunKind::Session if is_cache(volume) => None,
        RunKind::Session if is_workspace(volume) => Some((Mode::Checkout, checkout_limits())),
        RunKind::Session => Some((Mode::Whole, checkout_limits())),
        RunKind::Prepare => Some((Mode::Whole, PREPARED_LIMITS)),
        // A fetch moves a ref in a session's workspace: the checkout comes
        // back, held to a checkout's limits, as the session's own does.
        RunKind::Fetch if is_workspace(volume) => Some((Mode::Checkout, checkout_limits())),
        RunKind::Check | RunKind::Probe | RunKind::Fetch => None,
    }
}

/// The directories the node keeps in place of runtime volumes, one per
/// volume, under one root. A check's copy may instead be an alias (a symbolic
/// link) of the tree it copies, which nothing writes through.
#[derive(Debug, Clone)]
pub struct FilesStore {
    root: PathBuf,
    every: Duration,
    /// How long one take-back may run before it is abandoned.
    within: Duration,
    /// Volumes it keeps that are never sent, whatever a run mounts.
    withheld: Vec<String>,
    /// Keep every volume a run names, making an empty one where none is
    /// kept yet, rather than leaving it to the runtime.
    every_volume: bool,
}

impl FilesStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FilesStore {
            root: root.into(),
            every: SYNC_EVERY,
            within: SYNC_WITHIN,
            withheld: Vec::new(),
            every_volume: false,
        }
    }

    /// Keep every volume a run mounts here, an empty one made for a name not
    /// kept yet, as a runtime makes a volume on first mount. For a provider
    /// that mounts no runtime volume at all (Kubernetes, whose store is the
    /// state claim itself).
    pub fn keeping_every_volume(mut self) -> Self {
        self.every_volume = true;
        self
    }

    /// Never send `volume` into a sandbox, even where a run mounts it: the
    /// provider gives the run empty storage of its own there instead. For a
    /// volume shared between runs (the harness state every session once
    /// shared), whose every other run's files a sandbox must not be handed.
    pub fn withholding(mut self, volume: impl Into<String>) -> Self {
        self.withheld.push(volume.into());
        self
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

    /// Abandon a take-back that runs longer than `within` rather than
    /// [`SYNC_WITHIN`].
    pub fn sync_within(mut self, within: Duration) -> Self {
        self.within = within;
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
        std::fs::create_dir_all(&self.root)?;
        // A caller that gives up stops the copy, and the staging copy goes
        // once nothing writes it.
        let staging = tree::Staging::create(self.staging(dest))?;
        let (path, stopped) = (staging.path().to_path_buf(), staging.stopped());
        let copied = staging
            .blocking(move || clone_tree(&from, &path, &stopped))
            .await;
        if let Err(error) = copied {
            staging.discard().await;
            return Err(error);
        }
        match std::fs::rename(staging.path(), &to) {
            Ok(()) => {
                staging.keep();
                Ok(())
            }
            Err(error) => {
                staging.discard().await;
                Err(error.into())
            }
        }
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
    /// `volume`/`sub_path`, mounted at `target` in the sandbox, and make it
    /// the node's copy. Nothing is changed unless the whole archive passes.
    /// `nested` are paths inside it that other mounts provide, which the node
    /// sends at every start and never takes back.
    ///
    /// Dropping the future stops the unpack and closes `reader`; the staging
    /// directory goes once nothing writes it, and a commit that already began
    /// runs to its end.
    #[allow(clippy::too_many_arguments)]
    pub async fn receive<R>(
        &self,
        volume: &str,
        sub_path: &str,
        target: &str,
        reader: R,
        mode: Mode,
        limits: Limits,
        nested: &[PathBuf],
    ) -> Result<TreeSummary, FilesError>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        self.receive_holding(
            volume,
            sub_path,
            target,
            reader,
            mode,
            limits,
            nested,
            Vec::new(),
        )
        .await
    }

    /// [`Self::receive`], releasing `held` (the locks a sync holds) only
    /// once its blocking work has ended, even when the caller gave up before.
    #[allow(clippy::too_many_arguments)]
    async fn receive_holding<R>(
        &self,
        volume: &str,
        sub_path: &str,
        target: &str,
        reader: R,
        mode: Mode,
        limits: Limits,
        nested: &[PathBuf],
        held: Vec<Box<dyn Send>>,
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
        let staging = tree::Staging::create(self.staging(volume))?;
        for held in held {
            staging.hold(held);
        }
        let mount = Some(PathBuf::from(target));
        let summary = match tree::unpack_staged(reader, &staging, mount, limits).await {
            Ok(summary) => summary,
            Err(error) => {
                staging.discard().await;
                return Err(error.into());
            }
        };
        let root = self.root.clone();
        let nested = nested.to_vec();
        let (path, stopped) = (staging.path().to_path_buf(), staging.stopped());
        let volume = volume.to_string();
        let committed = staging
            .blocking(move || {
                // Its caller gave up: the node keeps its last copy, as it was
                // told.
                if stopped.is_cancelled() {
                    return Err(FilesError::Refused(format!(
                        "taking {volume} back was abandoned before its commit"
                    )));
                }
                let committed = commit(&root, &dest, &path, mode, &nested);
                if committed.is_ok() && stopped.is_cancelled() {
                    tracing::warn!(
                        volume,
                        "a sandbox's tree was committed after its caller gave up; the node's copy is the one just taken back"
                    );
                }
                committed
            })
            .await;
        match committed {
            Ok(()) => {
                staging.keep();
                Ok(summary)
            }
            Err(error) => {
                staging.discard().await;
                Err(error)
            }
        }
    }

    /// Split a run's mounts into what the provider mounts and what goes over
    /// the files channel: every mount of a volume kept here. Each directory
    /// sent in, and the directory each file is sent into, lands on storage
    /// of the sandbox's own ([`Mount::local`]), unless it is inside another
    /// that does.
    pub fn plan(&self, mounts: &[Mount]) -> Result<(Vec<Mount>, Option<Plan>), FilesError> {
        self.plan_from(mounts, "", None)
    }

    /// [`Self::plan`] for a run of `kind` in `image`. From a prepared
    /// snapshot, a run's `/work` is sent over the snapshot's own, which holds
    /// what its preparation installed (`node_modules`, `vendor/`) and none of
    /// the default branch's tracked files; and a session is not sent the
    /// node's copy of a cache, since the snapshot's `/cache` is what its
    /// preparation filled and the node's copy would be sent whole at every
    /// start only to lie over it. A session's workspace is sent as the node
    /// holds it: the checkout alone, unless its own `prepare` ran.
    pub fn plan_from(
        &self,
        mounts: &[Mount],
        image: &str,
        kind: Option<RunKind>,
    ) -> Result<(Vec<Mount>, Option<Plan>), FilesError> {
        let from_snapshot = crate::sandbox_images::is_snapshot(image);
        let image_holds_caches = from_snapshot && kind == Some(RunKind::Session);
        let mut provided = Vec::new();
        let mut placed = Vec::new();
        for mount in mounts {
            if image_holds_caches && is_cache(&mount.volume) && self.tree(&mount.volume).is_some() {
                continue;
            }
            if self.withheld.contains(&mount.volume) {
                provided.push(mount.clone());
                continue;
            }
            if self.every_volume && !mount.is_local() && self.tree(&mount.volume).is_none() {
                std::fs::create_dir_all(self.path(&mount.volume)?)?;
            }
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
        // A file lands in the directory that holds it, which the image may
        // not have, or not have given the sandbox's user: Podman makes a
        // missing parent of a mountpoint as root. That directory is made the
        // sandbox's own too, unless it already is.
        let wanted: Vec<String> = placed
            .iter()
            .filter_map(|p| match p.file {
                false => Some(p.target.clone()),
                true => Path::new(&p.target)
                    .parent()
                    .filter(|parent| *parent != Path::new("/"))
                    .map(|parent| parent.to_string_lossy().into_owned()),
            })
            .collect();
        let mut local: Vec<String> = Vec::new();
        for target in &wanted {
            let inside_another = wanted.iter().any(|o| inside(target, o));
            if !inside_another && !local.contains(target) {
                local.push(target.clone());
            }
        }
        // A snapshot's `/work` is its preparation's tree, made by the
        // sandbox's own user: a tree sent there lands on it in place, where
        // storage of the sandbox's own would first be filled with a copy of
        // it (Podman copies what the image holds into a new volume).
        if from_snapshot {
            local.retain(|target| target != crate::sandbox_images::PREPARED_TREE);
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

/// Copy `from` into the directory `to` with the system `cp` (sharing
/// extents where the filesystem can), or in process where `cp` cannot.
/// Stops, killing `cp`, once `stopped` is set.
fn clone_tree(
    from: &Path,
    to: &Path,
    stopped: &tokio_util::sync::CancellationToken,
) -> Result<(), FilesError> {
    let child = std::process::Command::new("cp")
        .args(["-dRp", "--reflink=auto", "--"])
        .arg(from.join("."))
        .arg(to)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let copied = match child {
        Ok(mut child) => loop {
            if stopped.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(FilesError::Refused("the copy was abandoned".into()));
            }
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        },
        Err(_) => false,
    };
    if !copied {
        let _ = std::fs::remove_dir_all(to);
        crate::workspace::copy_tree(from, to, false)?;
    }
    Ok(())
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
        // Under a link, the path names what the link leads to, not a nested
        // tree in this one.
        if real_directories(staging, path).is_err() {
            continue;
        }
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
        let attached = self.adopt(conn, name, kind);
        // What it was just sent is the node's copy: as current as it gets.
        attached.inner.tracker.lock().unwrap().last_ok_ms = Some(crate::store::now_ms());
        Ok(attached)
    }

    /// The targets [`Self::readopt`] sends again: what the sandbox is never
    /// asked to give back, read-only trees and files, which the node writes
    /// afresh for every launch. The harness's MCP configuration is one, and
    /// it carries the launch's own gateway token.
    pub fn resent(&self) -> Vec<&str> {
        self.placed
            .iter()
            .filter(|p| p.read_only || p.file)
            .map(|p| p.target.as_str())
            .collect()
    }

    /// Take a sandbox an earlier node process sent its trees into and left
    /// running back for a new launch: what the sandbox writes stays as it is
    /// there, the newer copy, and what the node writes for each launch
    /// ([`Self::resent`]) is sent again, so the new harness reads this
    /// launch's configuration rather than the last one's.
    pub async fn readopt(
        self,
        conn: &SandboxConn,
        name: &str,
        kind: RunKind,
    ) -> Result<Attached, FilesError> {
        for p in self.placed.iter().filter(|p| p.read_only || p.file) {
            let lock = crate::workspace::volume_lock(&p.volume);
            let _guard = lock.lock().await;
            send(conn, p).await?;
        }
        Ok(self.adopt(conn, name, kind))
    }

    /// Begin taking back what a run of `kind` takes back from a sandbox
    /// whose trees are there already: one an earlier node process sent them
    /// into and left running. Nothing is sent; the sandbox's copy is the
    /// newer.
    pub fn adopt(self, conn: &SandboxConn, name: &str, kind: RunKind) -> Attached {
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
                    // A session's sandbox holds its workspace's newer copy
                    // while it runs: a fetch's must not replace the node's.
                    refuse_live: kind == RunKind::Fetch,
                })
            })
            .collect();
        let inner = Arc::new(Inner {
            within: self.store.within,
            tracker: Mutex::new(Tracker::default()),
            conn: conn.clone(),
            name: name.to_string(),
            store: self.store.clone(),
            syncs,
            turn: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            syncing: Arc::new(tokio::sync::Mutex::new(())),
            periodic: Mutex::new(None),
        });
        if kind == RunKind::Session && !inner.syncs.is_empty() {
            register(&inner);
            let every = self.store.every;
            let periodic =
                tokio::spawn(periodic(Arc::downgrade(&inner), inner.conn.clone(), every));
            *inner.periodic.lock().unwrap() = Some(periodic);
        }
        Attached { inner }
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
    /// Refuse to take it back while a running sandbox holds the volume.
    refuse_live: bool,
}

struct Inner {
    conn: SandboxConn,
    name: String,
    store: FilesStore,
    syncs: Vec<Sync>,
    /// How long one take-back may run.
    within: Duration,
    /// How its take-backs have gone, for the session API and its reports.
    tracker: Mutex<Tracker>,
    /// A turn is running: the periodic sync runs only then.
    turn: AtomicBool,
    finished: AtomicBool,
    /// One sync at a time per sandbox, held until the blocking work of one
    /// abandoned midway has ended.
    syncing: Arc<tokio::sync::Mutex<()>>,
    periodic: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Inner {
    async fn sync(&self, why: &str) -> Result<TreeSummary, FilesError> {
        // Shared with each receive, which holds it, and the volume's lock,
        // until its blocking work ends: a sync given up midway still holds
        // both while its unpack stops and while a commit it began finishes.
        let one = Arc::new(self.syncing.clone().lock_owned().await);
        if self.conn.is_closed() {
            return Err(FilesError::Refused("the sandbox is gone".into()));
        }
        let started = std::time::Instant::now();
        let mut taken = TreeSummary::default();
        for sync in &self.syncs {
            let guard = crate::workspace::volume_lock(&sync.volume)
                .lock_owned()
                .await;
            if sync.refuse_live && is_live(&sync.volume) {
                return Err(FilesError::Refused(format!(
                    "{} is held by a running sandbox, whose copy is the newer",
                    sync.volume
                )));
            }
            let reader = self
                .conn
                .export_reader(&sync.target, sync.mode.exclusions())
                .await?;
            let got = self
                .store
                .receive_holding(
                    &sync.volume,
                    &sync.sub_path,
                    &sync.target,
                    reader,
                    sync.mode,
                    sync.limits,
                    &sync.nested,
                    vec![Box::new(one.clone()), Box::new(guard)],
                )
                .await?;
            taken.files += got.files;
            taken.dirs += got.dirs;
            taken.links += got.links;
            taken.bytes += got.bytes;
        }
        tracing::debug!(
            sandbox = %self.name,
            why,
            ms = started.elapsed().as_millis() as u64,
            "took the sandbox's trees back"
        );
        Ok(taken)
    }

    /// `sync`, bounded, logged and reported: a failed sync keeps the node's
    /// last copy, and the session hears of it (see [`Tracker`]).
    async fn sync_logged(&self, why: &str) -> Result<(), FilesError> {
        self.tracker.lock().unwrap().in_flight += 1;
        let started = std::time::Instant::now();
        let result = bounded(self.within, self.sync(why)).await;
        match &result {
            Ok(_) => {}
            Err((SyncOutcome::TimedOut, _)) => {
                tracing::warn!(sandbox = %self.name, why, "taking the sandbox's trees back timed out; the node keeps its last copy")
            }
            Err((_, error)) => {
                tracing::warn!(sandbox = %self.name, why, %error, "could not take the sandbox's trees back; the node keeps its last copy")
            }
        }
        let outcome = match &result {
            Ok(summary) => Ok(*summary),
            Err((outcome, error)) => Err((*outcome, error.to_string())),
        };
        let report = {
            let mut tracker = self.tracker.lock().unwrap();
            tracker.in_flight = tracker.in_flight.saturating_sub(1);
            tracker.observe(
                &self.name,
                why,
                outcome,
                started.elapsed().as_millis() as u64,
                crate::store::now_ms(),
            )
        };
        report_sync(report);
        result.map(|_| ()).map_err(|(_, error)| error)
    }

    /// The last sync, once: what it returns is only ever the first call's.
    async fn finish(self: &Arc<Self>) -> Result<(), FilesError> {
        if self.finished.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(periodic) = self.periodic.lock().unwrap().take() {
            periodic.abort();
        }
        let taken = match self.syncs.is_empty() {
            true => Ok(()),
            false => self.sync_logged(WHY_EXIT).await,
        };
        unregister(self);
        taken
    }
}

/// How one take-back ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    Ok,
    Failed,
    TimedOut,
}

impl SyncOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncOutcome::Ok => "ok",
            SyncOutcome::Failed => "failed",
            SyncOutcome::TimedOut => "timed_out",
        }
    }
}

/// One take-back, as it is reported past the node's log: to the reporter
/// [`set_reporter`] installed, which records the ones worth recording on the
/// sandbox's session and tells the operator when they keep failing.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncReport {
    /// The sandbox's (the run's) name, which its session row carries.
    pub sandbox: String,
    pub outcome: SyncOutcome,
    /// `turn end`, `during a turn`, `exit` or `asked`.
    pub why: String,
    /// What came back, when it did.
    pub files: u64,
    pub bytes: u64,
    pub ms: u64,
    pub error: Option<String>,
    /// Failures in a row, this one included: 0 after a success.
    pub failures: u32,
    /// For a success after failures: how many it ended.
    pub recovered: u32,
    /// When the node's copy last matched the sandbox's.
    pub last_ok_ms: Option<i64>,
    /// Worth a `workspace_sync` event: the first failure after a success,
    /// a recovery, and the last take-back whatever it came to. Periodic
    /// successes and repeated failures are not.
    pub record: bool,
}

impl SyncReport {
    /// The sandbox's last take-back, before it is removed.
    pub fn is_exit(&self) -> bool {
        self.why == WHY_EXIT
    }
}

/// How a running sandbox's take-backs stand, for the session API.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SyncStatus {
    /// When the node's copy last matched the sandbox's: its last successful
    /// take-back, or when its trees were sent in.
    pub last_ok_ms: Option<i64>,
    /// Failed take-backs in a row.
    pub failures: u32,
    pub last_error: Option<String>,
    /// A take-back is running now.
    pub in_flight: bool,
    /// The sandbox is running.
    pub live: bool,
}

/// The running state of one sandbox's take-backs, and which of them are
/// worth reporting: pure, so its rules are tested without a sandbox.
#[derive(Debug, Default)]
struct Tracker {
    last_ok_ms: Option<i64>,
    failures: u32,
    last_error: Option<String>,
    in_flight: u32,
    /// The last take-back has been reported.
    finished: bool,
}

impl Tracker {
    /// Count one take-back in and say what to report of it.
    fn observe(
        &mut self,
        sandbox: &str,
        why: &str,
        outcome: Result<TreeSummary, (SyncOutcome, String)>,
        ms: u64,
        now_ms: i64,
    ) -> SyncReport {
        let exit = why == WHY_EXIT;
        let mut report = SyncReport {
            sandbox: sandbox.to_string(),
            outcome: SyncOutcome::Ok,
            why: why.to_string(),
            files: 0,
            bytes: 0,
            ms,
            error: None,
            failures: 0,
            recovered: 0,
            last_ok_ms: None,
            record: false,
        };
        match outcome {
            Ok(summary) => {
                report.files = summary.files;
                report.bytes = summary.bytes;
                report.recovered = std::mem::take(&mut self.failures);
                report.record = report.recovered > 0;
                self.last_ok_ms = Some(now_ms);
                self.last_error = None;
            }
            Err((outcome, error)) => {
                self.failures += 1;
                report.outcome = outcome;
                report.record = self.failures == 1;
                self.last_error = Some(error.clone());
                report.error = Some(error);
            }
        }
        report.failures = self.failures;
        report.last_ok_ms = self.last_ok_ms;
        if exit {
            report.record = true;
            self.finished = true;
        }
        report
    }

    fn status(&self, live: bool) -> SyncStatus {
        SyncStatus {
            last_ok_ms: self.last_ok_ms,
            failures: self.failures,
            last_error: self.last_error.clone(),
            in_flight: self.in_flight > 0,
            live: live && !self.finished,
        }
    }
}

/// `sync` bounded by `within`: how it failed, when it did.
async fn bounded<T>(
    within: Duration,
    sync: impl std::future::Future<Output = Result<T, FilesError>>,
) -> Result<T, (SyncOutcome, FilesError)> {
    match tokio::time::timeout(within, sync).await {
        Ok(Ok(taken)) => Ok(taken),
        Ok(Err(error)) => Err((SyncOutcome::Failed, error)),
        Err(_) => Err((
            SyncOutcome::TimedOut,
            FilesError::Refused(format!(
                "taking the trees back took longer than {}",
                humane(within)
            )),
        )),
    }
}

fn humane(d: Duration) -> String {
    match d.as_secs() {
        0 => format!("{}ms", d.as_millis()),
        secs => format!("{secs}s"),
    }
}

/// Who hears of every take-back: the session manager, once it is up.
pub type SyncReporter = Arc<dyn Fn(SyncReport) + Send + std::marker::Sync>;

fn reporter() -> &'static Mutex<Option<SyncReporter>> {
    static REPORTER: OnceLock<Mutex<Option<SyncReporter>>> = OnceLock::new();
    REPORTER.get_or_init(Default::default)
}

/// Hand every take-back's report to `report` from now on (`None`: to
/// nobody). It runs on the task that synced, so it must not block.
pub fn set_reporter(report: Option<SyncReporter>) {
    *reporter().lock().unwrap() = report;
}

fn report_sync(report: SyncReport) {
    let hook = reporter().lock().unwrap().clone();
    if let Some(hook) = hook {
        hook(report);
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
            let _ = inner.sync_logged("during a turn").await;
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
        let _ = self.inner.finish().await;
    }

    /// [`Self::finish`], saying whether that last take-back failed: a
    /// preparation whose tree never came back has prepared nothing.
    pub async fn conclude(&self) -> Result<(), FilesError> {
        self.inner.finish().await
    }

    /// Take the trees back now, as a turn's end does.
    pub async fn sync(&self) -> Result<(), FilesError> {
        self.inner.sync("asked").await.map(|_| ())
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

/// Whether a session's turn is running in the sandbox of this name, as far
/// as its turns were announced.
pub fn in_turn(name: &str) -> bool {
    named(name).is_some_and(|inner| inner.turn.load(Ordering::SeqCst))
}

/// A session's turn has ended: its trees are taken back now.
pub async fn turn_ended(name: &str) {
    if let Some(inner) = named(name) {
        inner.turn.store(false, Ordering::SeqCst);
        let _ = inner.sync_logged("turn end").await;
    }
}

/// The sandbox of this name is about to be removed: take its trees back
/// first.
pub async fn finish_named(name: &str) {
    if let Some(inner) = named(name) {
        let _ = inner.finish().await;
    }
}

/// How the take-backs of the running sandbox of this name have gone. `None`
/// when no running session's sandbox of that name takes trees back: its
/// files are mounted, or it has ended (its last report then says).
pub fn sync_status(name: &str) -> Option<SyncStatus> {
    let inner = named(name)?;
    let tracker = inner.tracker.lock().unwrap();
    Some(tracker.status(!inner.conn.is_closed()))
}

/// Whether a running sandbox holds this volume.
pub fn is_live(volume: &str) -> bool {
    registry().lock().unwrap().by_volume.contains_key(volume)
}

/// The connection to the running sandbox that holds this volume, and where
/// in it the volume is: for work that has to happen in the live tree
/// (`fetch_branch`), not in the node's copy a later sync would overwrite.
/// `None` when no running sandbox holds it, or the one that did is gone.
pub fn live_target(volume: &str) -> Option<(SandboxConn, String)> {
    let (inner, target) = registry().lock().unwrap().by_volume.get(volume).cloned()?;
    (!inner.conn.is_closed()).then(|| (inner.conn.clone(), target))
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
