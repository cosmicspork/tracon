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

#[derive(Debug, Clone)]
pub struct Mount {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RunnerCommand {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub workdir: Option<String>,
    pub name: String,
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
}

/// Runs a command directly on the host, with no boundary. Used by the adapter
/// tests and by nothing on any operator path: sessions always go through a
/// boundary backend's runner.
pub mod local {
    use super::*;
    use std::process::Stdio;
    use std::sync::Arc;
    use tokio::process::Command;

    use crate::boundary::{Backend, BoundaryError, BoundaryReport};
    use crate::config::Config;

    /// Runs the argv directly on the host. Mounts and container name are ignored;
    /// env is applied. For adapter tests against the fake agent only.
    pub struct LocalRunner;

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
            Ok(Command::new(bin).args(args).envs(cmd.env).output().await?)
        }

        async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
            Ok(())
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
        fn harness_host(&self) -> String {
            "localhost".into()
        }
        fn harness_home(&self) -> String {
            crate::session::materialize::PODMAN_HARNESS_HOME.into()
        }
        async fn reconcile(&self, _names: &[String]) {}
    }
}
