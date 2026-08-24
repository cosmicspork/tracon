//! Establishing and verifying the harness boundary. A node that cannot prove
//! the boundary refuses to run harnesses; there is no advisory mode.

pub mod checks;
pub mod setup;

use serde::Serialize;

pub use checks::{check_all, CheckResult};
pub use setup::setup;

#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("podman: {0}")]
    Podman(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// The outcome of the startup verification.
#[derive(Debug, Clone, Serialize)]
pub struct BoundaryReport {
    pub checks: Vec<CheckResult>,
}

impl BoundaryReport {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    pub fn first_failure(&self) -> Option<&CheckResult> {
        self.checks.iter().find(|c| !c.ok)
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
