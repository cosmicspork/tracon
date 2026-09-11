//! How the node gets a harness process. `PodmanRunner` puts it inside the
//! Podman boundary, the Kubernetes runner inside a harness pod; `LocalRunner`
//! (tests only) runs the argv on the host so the adapter can be exercised
//! without containers.

pub mod kube;
pub mod podman;

use async_trait::async_trait;
use futures_core::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;

/// A runtime-owned directory mounted into a runner. `volume` is a named Podman
/// volume or a PVC-relative directory; it is never a host path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub volume: String,
    /// A relative path inside `volume`; empty means its root.
    pub sub_path: String,
    pub target: String,
    pub read_only: bool,
}

impl Mount {
    pub fn volume(volume: impl Into<String>, target: impl Into<String>, read_only: bool) -> Self {
        Self {
            volume: volume.into(),
            sub_path: String::new(),
            target: target.into(),
            read_only,
        }
    }

    pub fn at(
        volume: impl Into<String>,
        sub_path: impl Into<String>,
        target: impl Into<String>,
        read_only: bool,
    ) -> Self {
        Self {
            volume: volume.into(),
            sub_path: sub_path.into(),
            target: target.into(),
            read_only,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunnerCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub workdir: Option<String>,
    /// An approved preparation image may replace the harness image for a
    /// bounded command. Long-lived harnesses leave this unset.
    pub image: Option<String>,
    pub name: String,
    /// Overrides the runner's default image for this one command. `None`
    /// runs the harness image every other command runs.
    pub image: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("spawn: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// A running harness, reduced to what the ACP transport needs: its stdio and
/// a future that resolves when it is gone. A process and a pod attach look
/// the same from here.
pub struct Spawned {
    pub stdin: Box<dyn AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    /// Resolves with the exit status once the harness has ended. Awaiting it
    /// is also what reaps a child process.
    pub done: BoxFuture<'static, Result<i32, RunnerError>>,
}

impl Spawned {
    /// Wrap a child spawned with piped stdin and stdout.
    pub fn from_child(mut child: Child) -> Result<Self, RunnerError> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RunnerError::Other("stdin not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RunnerError::Other("stdout not piped".into()))?;
        Ok(Self {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            done: Box::pin(async move {
                let status = child.wait().await?;
                Ok(status.code().unwrap_or(-1))
            }),
        })
    }
}

#[async_trait]
pub trait Runner: Send + Sync {
    /// Spawn a long-lived process with stdin/stdout piped (the ACP transport).
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError>;
    /// Run to completion and capture output (e.g. `omp --version`).
    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError>;
    /// Force-remove a named process/container.
    async fn kill(&self, name: &str) -> Result<(), RunnerError>;
    /// The image identity this runner actually used (or will use), confirmed
    /// against the runtime rather than merely read from configuration.
    /// `None` means no confirmed identity exists — evidence keyed on it must
    /// never be treated as reusable. A runner that does not run inside an
    /// image (`LocalRunner`) may report a stable non-digest identity here;
    /// it exists for the audit trail, never for pinning.
    async fn resolved_image(&self) -> Option<String> {
        None
    }
}

/// Runs a command directly on the host, with no boundary. Used by the adapter
/// tests and by nothing on any operator path: sessions always go through a
/// boundary backend's runner.
pub mod local {
    use super::*;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use tokio::process::Command;

    use crate::boundary::{Backend, BoundaryError, BoundaryReport};
    use crate::config::Config;

    /// Where `LocalBackend` keeps a named volume's bytes on disk. Shared by
    /// the backend's import/export and by `LocalRunner`, which has no
    /// container to mount a volume into and so must resolve one straight to
    /// this path.
    pub fn local_runtime_path(volume: &str) -> PathBuf {
        Config::state_dir().join("local-runtime").join(volume)
    }

    /// Runs the argv directly on the host. Mounts and container name are ignored;
    /// env is applied. For adapter tests against the fake agent only.
    pub struct LocalRunner;

    /// Resolve a command's `/work`-style workdir to a real host directory: the
    /// volume backing the mount whose target matches it, else the workdir
    /// itself if it already names a real directory.
    fn resolve_workdir(cmd: &RunnerCommand) -> Option<String> {
        let target = cmd.workdir.as_deref()?;
        cmd.mounts
            .iter()
            .find(|m| m.target == target)
            .map(|m| {
                let base = local_runtime_path(&m.volume);
                if m.sub_path.is_empty() {
                    base
                } else {
                    base.join(&m.sub_path)
                }
                .to_string_lossy()
                .into_owned()
            })
            .or_else(|| {
                std::path::Path::new(target)
                    .is_dir()
                    .then(|| target.to_owned())
            })
    }

    #[async_trait]
    impl Runner for LocalRunner {
        async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
            let (bin, args) = cmd
                .argv
                .split_first()
                .ok_or_else(|| RunnerError::Other("empty argv".into()))?;
            let mut c = Command::new(bin);
            c.args(args)
                .envs(cmd.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            if let Some(w) = cmd.workdir {
                c.current_dir(w);
            }
            Spawned::from_child(c.spawn()?)
        }

        async fn run_capture(
            &self,
            cmd: RunnerCommand,
        ) -> Result<std::process::Output, RunnerError> {
            let (bin, args) = cmd
                .argv
                .split_first()
                .ok_or_else(|| RunnerError::Other("empty argv".into()))?;
            // Local runners exist only for adapter tests and check runs
            // against `LocalBackend`. A workdir naming a mount target
            // resolves to that volume's on-disk bytes; nothing else runs
            // relative to a container path that does not exist on the host.
            let dir = resolve_workdir(&cmd);
            let mut c = Command::new(bin);
            c.args(args).envs(cmd.env);
            if let Some(d) = dir {
                c.current_dir(d);
            }
            Ok(c.output().await?)
        }

        async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
            Ok(())
        }

        // Direct host execution has no image at all. The identity is
        // deliberately not digest-shaped, so `immutable_image_identity`
        // (review::checks) never mistakes it for a pinned one.
        async fn resolved_image(&self) -> Option<String> {
            Some("local:direct-execution".to_string())
        }
    }

    /// A backend with no boundary at all, for tests that never spawn (the
    /// fake adapter) or spawn the fake agent on the host. It is not a
    /// `RuntimeKind`, so no configuration can select it.
    #[doc(hidden)]
    pub struct LocalBackend;

    #[async_trait]
    impl Backend for LocalBackend {
        fn kind(&self) -> &'static str {
            "local"
        }
        async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
            Ok(())
        }
        async fn check_all(&self, _cfg: &Config, _deep: bool) -> BoundaryReport {
            BoundaryReport { checks: Vec::new() }
        }
        fn runner(&self, _extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
            Arc::new(LocalRunner)
        }
        async fn import_volume(
            &self,
            volume: &str,
            source: &std::path::Path,
        ) -> Result<(), BoundaryError> {
            let destination = Config::state_dir().join("local-runtime").join(volume);
            if destination.exists() {
                std::fs::remove_dir_all(&destination)?;
            }
            crate::workspace::copy_tree(source, &destination, false)
                .map_err(|e| BoundaryError::Other(e.to_string()))
        }
        async fn export_volume(
            &self,
            volume: &str,
            destination: &std::path::Path,
        ) -> Result<(), BoundaryError> {
            crate::workspace::copy_tree(
                &Config::state_dir().join("local-runtime").join(volume),
                destination,
                false,
            )
            .map_err(|e| BoundaryError::Other(e.to_string()))
        }
        fn harness_host(&self) -> String {
            "localhost".into()
        }
        fn harness_home(&self) -> String {
            crate::session::materialize::PODMAN_HARNESS_HOME.into()
        }
        async fn reconcile(&self, _names: &[String]) {}
    }
}
