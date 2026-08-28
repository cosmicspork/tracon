//! Establishing and verifying the harness boundary. A node that cannot prove
//! the boundary refuses to run harnesses; there is no advisory mode.
//!
//! The boundary has more than one implementation — rootless Podman on a
//! laptop, harness pods behind a NetworkPolicy on a cluster — and every one of
//! them answers the same five checks (`checks::CheckId`) and hands out the
//! same `Runner`. `Backend` is that seam; `backend_for` picks one from
//! `[runtime] kind`.

pub mod checks;
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
    /// A runner carrying these mounts in addition to the boundary's own.
    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner>;
    /// The name by which a harness reaches the node (the MCP endpoint and the
    /// deep probe's ping).
    fn harness_host(&self) -> String;
    /// Remove harnesses left over from a previous run, by name.
    async fn reconcile(&self, names: &[String]);
}

/// The backend `[runtime] kind` selects. Detection that needs the host (the
/// SELinux probe) happens here, once, rather than per session.
pub async fn backend_for(cfg: &Config) -> Arc<dyn Backend> {
    match cfg.runtime.kind {
        RuntimeKind::Podman => Arc::new(podman::PodmanBackend::detect(cfg).await),
    }
}
