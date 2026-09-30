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

/// Held for exactly one QA browser run's container. Dropping it restores
/// the QA egress gateway to deny-all; the mutex permit inside it also keeps
/// two QA browser runs from ever observing each other's scope.
pub struct QaEgressGuard {
    _permit: tokio::sync::MutexGuard<'static, ()>,
    reset: Option<Box<dyn FnOnce() + Send>>,
}

impl QaEgressGuard {
    pub fn new(
        permit: tokio::sync::MutexGuard<'static, ()>,
        reset: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            _permit: permit,
            reset: Some(Box::new(reset)),
        }
    }
}

impl Drop for QaEgressGuard {
    fn drop(&mut self) {
        if let Some(reset) = self.reset.take() {
            reset();
        }
    }
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
    /// Each supported harness's image and its state: `current`, `stale`,
    /// `missing`, or `unknown` where this runtime cannot say. For display;
    /// `check_all` is what refuses a node whose images are not usable.
    async fn harness_images(&self, _cfg: &Config) -> Vec<HarnessImage> {
        Vec::new()
    }
    /// Copy an explicitly staged directory into runtime-owned storage. The
    /// source is never mounted into a harness.
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError>;
    /// Copy a runtime-owned directory into a node-owned staging path. Callers
    /// validate the snapshot before interpreting it as source or Git data.
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError>;
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
    /// Restrict this backend's dedicated QA browser egress gateway to
    /// exactly these hosts for the life of the returned guard. Every QA
    /// browser run acquires this before its container starts and holds it
    /// until the container exits. `Err` when this backend has no scoped QA
    /// gateway wired: the caller refuses to run rather than proceed on the
    /// harness's own (LLM-provider-only, and shared with every other
    /// session) egress path.
    async fn scope_qa_egress(
        &self,
        _allowed_hosts: &[String],
    ) -> Result<QaEgressGuard, BoundaryError> {
        Err(BoundaryError::Other(format!(
            "the {} backend has no QA browser egress gateway",
            self.kind()
        )))
    }
    /// `http://host:port` a QA browser container gives Playwright as its
    /// proxy once `scope_qa_egress` succeeds. `None` when it always errors.
    fn qa_proxy_url(&self) -> Option<String> {
        None
    }
    /// Remove harnesses left over from a previous run, by name.
    async fn reconcile(&self, names: &[String]);
    /// The runtime volumes this backend holds whose names begin with
    /// `tracon-`. A backend that keeps none has nothing to list.
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>, BoundaryError> {
        Ok(Vec::new())
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

/// The backend `[runtime] kind` selects. Detection that needs the host (the
/// SELinux probe) happens here, once, rather than per session.
pub async fn backend_for(cfg: &Config) -> Arc<dyn Backend> {
    match cfg.runtime.kind {
        RuntimeKind::Podman => Arc::new(podman::PodmanBackend::detect(cfg).await),
        RuntimeKind::Kubernetes => Arc::new(kubernetes::KubeBackend::new(cfg)),
    }
}
