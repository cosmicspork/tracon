//! The rootless Podman boundary: an internal network, a gateway container
//! carrying the allowlist proxy and the node forward, and a harness container
//! on the internal network only. Phase 0 proved it by hand; this is that
//! proof as code.

pub mod checks;
pub mod setup;

use std::sync::Arc;

use async_trait::async_trait;

use super::{Backend, BoundaryError, BoundaryReport};
use crate::config::Config;
use crate::runner::podman::{PodmanRunner, RunSpec};
use crate::runner::{Mount, Runner};

pub struct PodmanBackend {
    cfg: Config,
    selinux: bool,
}

impl PodmanBackend {
    pub async fn detect(cfg: &Config) -> Self {
        Self {
            cfg: cfg.clone(),
            selinux: selinux_enabled().await,
        }
    }

    pub fn spec(&self) -> RunSpec {
        RunSpec::from_config(&self.cfg, self.selinux)
    }
}

#[async_trait]
impl Backend for PodmanBackend {
    fn kind(&self) -> &'static str {
        "podman"
    }

    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError> {
        setup::setup(cfg, rebuild).await
    }

    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        checks::check_all(cfg, self.selinux, deep).await
    }

    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let mut spec = self.spec();
        spec.extra_mounts = extra_mounts;
        Arc::new(PodmanRunner::new(spec))
    }

    fn harness_host(&self) -> String {
        self.cfg.boundary.gateway_container.clone()
    }

    fn harness_home(&self) -> String {
        crate::session::materialize::PODMAN_HARNESS_HOME.into()
    }

    async fn reconcile(&self, names: &[String]) {
        for name in names {
            let _ = podman(&["rm", "-f", "-i", name]).await;
        }
    }
}

/// Run `podman` with args and return stdout, or the stderr as an error.
pub(crate) async fn podman(args: &[&str]) -> Result<String, BoundaryError> {
    let out = tokio::process::Command::new("podman")
        .args(args)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(BoundaryError::Podman(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

pub(crate) async fn podman_json(args: &[&str]) -> Result<serde_json::Value, BoundaryError> {
    let text = podman(args).await?;
    Ok(serde_json::from_str(&text)?)
}

/// Whether the container host enforces SELinux (Podman needs `label=disable`
/// for bind mounts when it does).
pub async fn selinux_enabled() -> bool {
    podman(&["info", "--format", "{{.Host.Security.SELinuxEnabled}}"])
        .await
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}
