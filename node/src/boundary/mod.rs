//! Establishing and verifying the harness boundary. A node that cannot prove
//! the boundary refuses to run harnesses; there is no advisory mode.
//!
//! The boundary has more than one implementation — rootless Podman on a
//! laptop, harness pods behind a NetworkPolicy on a cluster — and every one of
//! them answers the same five checks (`checks::CheckId`) and hands out the
//! same `Runner`. `Backend` is that seam; `backend_for` picks one from
//! `[runtime] kind`.

pub mod checks;
pub mod kubernetes;
pub mod podman;

use std::sync::Arc;

use async_trait::async_trait;

pub use checks::{BoundaryReport, CheckId, CheckResult};

use crate::config::{Config, RuntimeKind};
use crate::runner::{Mount, Runner};

#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("podman: {0}")]
    Podman(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// One client's way out: proxy credentials that open exactly the hosts they
/// were issued for, for as long as this is held. A QA browser run, a
/// repository's dependency preparation and a session each hold their own, so
/// none of them is ever served what another was granted. Dropping it revokes
/// the credentials.
pub struct EgressGrant {
    /// `http://host:port`, without credentials: what a browser is given.
    pub server: String,
    pub username: String,
    pub password: String,
    revoke: Option<Box<dyn FnOnce() + Send>>,
}

impl EgressGrant {
    pub fn new(
        server: String,
        username: String,
        password: String,
        revoke: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            server,
            username,
            password,
            revoke: Some(Box::new(revoke)),
        }
    }

    /// The proxy as a URL carrying its credentials, which is how every
    /// package manager and Git take one.
    pub fn url(&self) -> String {
        match self.server.split_once("://") {
            Some((scheme, rest)) => {
                format!("{scheme}://{}:{}@{rest}", self.username, self.password)
            }
            None => self.server.clone(),
        }
    }

    /// The proxy variables that send a container through this grant instead
    /// of the harness proxy. Both spellings: curl and Git read the lowercase
    /// names, and most package managers read either.
    pub fn env(&self) -> Vec<(String, String)> {
        ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"]
            .into_iter()
            .map(|name| (name.to_string(), self.url()))
            .collect()
    }
}

impl Drop for EgressGrant {
    fn drop(&mut self) {
        if let Some(revoke) = self.revoke.take() {
            revoke();
        }
    }
}

/// One image to build: a Containerfile and the directory it may copy from.
pub struct ImageBuild<'a> {
    /// `name:tag`. The built image is reported by digest, under `name`.
    pub tag: &'a str,
    pub containerfile: &'a std::path::Path,
    pub context: &'a std::path::Path,
    pub labels: &'a [(String, String)],
    pub timeout: std::time::Duration,
}

/// A build that produced an image, and the end of what it printed.
#[derive(Debug, Clone)]
pub struct BuiltImage {
    /// `name@sha256:…`: the reference a run pins, which a later build under
    /// the same tag cannot move.
    pub image: String,
    pub log_tail: String,
}

/// A build that produced none.
#[derive(Debug, Clone)]
pub struct BuildFailure {
    pub error: String,
    pub log_tail: String,
}

/// A runtime that can build an image where the node's runs will find it. A
/// build reaches the network and runs the Containerfile's commands outside the
/// harness boundary, so only what the operator named is ever built.
#[async_trait]
pub trait ImageBuilder: Send + Sync {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure>;
    /// Whether the runtime still holds this image. One it pruned has to be
    /// built again before anything can run in it.
    async fn exists(&self, image: &str) -> bool;
    /// `name@sha256:…` for an image the runtime holds under a tag: what the
    /// tag names right now, which a rebuild under the same tag changes.
    async fn identity(&self, image: &str) -> Option<String>;
    /// Drop an image nothing uses any more. Best effort: one a container
    /// still runs from stays.
    async fn remove(&self, image: &str);
}

/// One harness's image as the runtime holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessImage {
    pub harness_id: String,
    pub image: String,
    pub state: &'static str,
}

/// One way of putting a harness behind a boundary the node can verify.
#[async_trait]
pub trait Backend: Send + Sync {
    /// `podman` or `kubernetes`; shown in logs and `/api/nodes`.
    fn kind(&self) -> &'static str;
    /// `tracon setup`: make what the boundary needs exist. Idempotent.
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError>;
    /// The startup verification, against the same specification a session
    /// runs. `deep` adds the active egress probe from inside the boundary.
    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport;
    /// A runner for the configured harness, carrying these mounts in addition
    /// to the boundary's own.
    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner>;
    /// A runner for a named harness: its image and its state layout. The
    /// configured harness is only the default; a session runs the one it
    /// names.
    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner>;
    /// A runner for a named harness in an image other than its own: the
    /// repository's, with the harness layered on. Everything else is the
    /// boundary's, exactly as `runner_for` has it. A runtime that cannot run
    /// one keeps the harness's image.
    fn runner_in(
        &self,
        harness_id: &str,
        image: &str,
        extra_mounts: Vec<Mount>,
    ) -> Arc<dyn Runner> {
        let _ = image;
        self.runner_for(harness_id, extra_mounts)
    }
    /// Each supported harness's image and its state: `current`, `stale`,
    /// `missing`, or `unknown` where this runtime cannot say. For display;
    /// `check_all` is what refuses a node whose images are not usable.
    async fn harness_images(&self, _cfg: &Config) -> Vec<HarnessImage> {
        Vec::new()
    }
    /// What builds a repository's image for this runtime. `None` for a
    /// runtime that only pulls: its repositories keep the images they are
    /// given.
    fn image_builder(&self) -> Option<&dyn ImageBuilder> {
        None
    }
    /// Copy an explicitly staged directory into runtime-owned storage. The
    /// source is never mounted into a harness.
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError>;
    /// As `import_volume`, for a tree a run will work in. A candidate's
    /// snapshot is read-only on the node — directories and files alike — and
    /// carries those modes into the volume, where a run that is root with no
    /// capabilities cannot override them: it could not create `target/` or
    /// `node_modules/`, or let a formatter fix a file. A backend whose import
    /// does not carry modes has nothing to do.
    async fn import_writable(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.import_volume(volume, source).await
    }
    /// Copy a runtime-owned directory into a node-owned staging path. Callers
    /// validate the snapshot before interpreting it as source or Git data.
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError>;
    /// Copy a workspace out as Git sees it: its repository, and the tracked
    /// and untracked files its ignore rules leave in. What a build or an
    /// install put there — `target/`, `node_modules/` and their symlinks — is
    /// neither the candidate nor something the snapshot's limits could hold.
    /// A backend with no way to filter exports the whole volume.
    async fn export_workspace(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.export_volume(volume, destination).await
    }
    /// The name by which a harness reaches the node (the MCP endpoint and the
    /// deep probe's ping).
    fn harness_host(&self) -> String;
    /// The harness user's home inside its runner; state and gitconfig are
    /// mounted under it.
    fn harness_home(&self) -> String;
    /// The port the node itself serves the CONNECT allowlist proxy on, when
    /// no gateway container carries it.
    fn proxy_port(&self) -> Option<u16> {
        None
    }
    /// The grants this backend's containers present to the node's own proxy,
    /// which the node serves (`gateway::proxy::serve_granted`). `None` for a
    /// backend with no per-client egress.
    fn egress_grants(&self) -> Option<&crate::gateway::proxy::Grants> {
        None
    }
    /// Open a way out for one client, to exactly what `spec` names, until the
    /// returned grant is dropped. Each client is filtered by its own grant:
    /// there is no shared allow list to widen and nothing to wait for.
    /// `Err` when this backend has no per-client egress wired: the caller
    /// refuses to run rather than proceed on the harness's own
    /// (LLM-provider-only, and shared with every other session) egress path.
    fn egress_grant(
        &self,
        _spec: crate::gateway::proxy::GrantSpec,
    ) -> Result<EgressGrant, BoundaryError> {
        Err(BoundaryError::Other(format!(
            "the {} backend has no per-client egress",
            self.kind()
        )))
    }
    /// Remove harnesses left over from a previous run, by name.
    async fn reconcile(&self, names: &[String]);
    /// The runtime volumes this backend holds whose names begin with
    /// `tracon-`. A backend that keeps none has nothing to list.
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>, BoundaryError> {
        Ok(Vec::new())
    }
    /// Whether the runtime holds a volume of this name.
    async fn volume_exists(&self, volume: &str) -> bool {
        self.list_volumes()
            .await
            .is_ok_and(|volumes| volumes.iter().any(|held| held.name == volume))
    }
    /// Make `destination` a copy of `source`, inside the runtime: neither is
    /// read by the node, so what a build or an install left in one (symlinks,
    /// build output) is carried as it is. A source that does not exist leaves
    /// an empty destination.
    async fn clone_volume(&self, source: &str, destination: &str) -> Result<(), BoundaryError> {
        let _ = destination;
        Err(BoundaryError::Other(format!(
            "the {} backend cannot copy runtime volume {source}",
            self.kind()
        )))
    }
    /// Remove a runtime volume. One that does not exist is already removed;
    /// one a running harness still mounts is refused, never forced.
    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        Err(BoundaryError::Other(format!(
            "the {} backend cannot remove runtime volume {volume}",
            self.kind()
        )))
    }
}

/// A runtime volume as its backend reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VolumeInfo {
    pub name: String,
    /// When the backend can say.
    pub created_ms: Option<i64>,
}

/// The `tracon-` volumes kept as directories under `root`, as the Kubernetes
/// and local backends store them. A staging copy mid-import is not a volume.
pub fn directory_volumes(root: &std::path::Path) -> Result<Vec<VolumeInfo>, BoundaryError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut volumes = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("tracon-") || name.contains(".staging-") || !entry.path().is_dir() {
            continue;
        }
        let created_ms = entry
            .metadata()
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        volumes.push(VolumeInfo { name, created_ms });
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(volumes)
}

/// Remove a directory-backed volume; a missing one is already gone.
pub fn remove_directory_volume(root: &std::path::Path, volume: &str) -> Result<(), BoundaryError> {
    if volume.is_empty() || volume.contains('/') || volume.contains("..") {
        return Err(BoundaryError::Other(format!("not a volume name: {volume}")));
    }
    match std::fs::remove_dir_all(root.join(volume)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Copy one directory-backed volume to another, as the runtime's own copy
/// would: everything, symlinks as symlinks. Nothing here is validated, because
/// nothing here is read — the node interprets a volume only through an export.
pub fn clone_directory_volume(
    root: &std::path::Path,
    source: &str,
    destination: &str,
) -> Result<(), BoundaryError> {
    for volume in [source, destination] {
        if volume.is_empty() || volume.contains('/') || volume.contains("..") {
            return Err(BoundaryError::Other(format!("not a volume name: {volume}")));
        }
    }
    fn copy(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let target = to.join(entry.file_name());
            if kind.is_dir() {
                copy(&entry.path(), &target)?;
            } else if kind.is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }
    let (from, to) = (root.join(source), root.join(destination));
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    match from.is_dir() {
        true => Ok(copy(&from, &to)?),
        false => Ok(std::fs::create_dir_all(&to)?),
    }
}

/// The backend `[runtime] kind` selects. Detection that needs the host (the
/// SELinux probe) happens here, once, rather than per session.
pub async fn backend_for(cfg: &Config) -> Arc<dyn Backend> {
    match cfg.runtime.kind {
        RuntimeKind::Podman => Arc::new(podman::PodmanBackend::detect(cfg).await),
        RuntimeKind::Kubernetes => Arc::new(kubernetes::KubeBackend::new(cfg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prepared tree is copied as the runtime would copy it: what an install
    /// left behind — a `.bin` of symlinks — arrives as it was, where the
    /// node's own imports and exports would refuse it.
    #[cfg(unix)]
    #[test]
    fn a_directory_volume_is_cloned_with_its_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("tracon-prep-a/node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(root.path().join("tracon-prep-a/a.txt"), "one").unwrap();
        std::os::unix::fs::symlink("../acorn/bin/acorn", bin.join("acorn")).unwrap();
        std::fs::create_dir_all(root.path().join("tracon-check-b")).unwrap();
        std::fs::write(root.path().join("tracon-check-b/stale"), "x").unwrap();

        clone_directory_volume(root.path(), "tracon-prep-a", "tracon-check-b").unwrap();
        let copy = root.path().join("tracon-check-b");
        assert_eq!(std::fs::read_to_string(copy.join("a.txt")).unwrap(), "one");
        assert_eq!(
            std::fs::read_link(copy.join("node_modules/.bin/acorn")).unwrap(),
            std::path::Path::new("../acorn/bin/acorn")
        );
        assert!(!copy.join("stale").exists());

        // A source that does not exist is an empty volume to copy.
        clone_directory_volume(root.path(), "tracon-cache-base-none", "tracon-prep-cache-c")
            .unwrap();
        assert!(root.path().join("tracon-prep-cache-c").is_dir());
        assert!(clone_directory_volume(root.path(), "../etc", "tracon-x").is_err());
    }
}
