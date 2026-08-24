//! How the node gets a harness process. `PodmanRunner` (PR 3) puts it inside the
//! boundary; `LocalRunner` (tests only) runs the argv on the host so the adapter
//! can be exercised without containers.

use async_trait::async_trait;
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

#[async_trait]
pub trait Runner: Send + Sync {
    /// Spawn a long-lived process with stdin/stdout piped (the ACP transport).
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Child, RunnerError>;
    /// Run to completion and capture output (e.g. `omp --version`).
    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError>;
    /// Force-remove a named process/container.
    async fn kill(&self, name: &str) -> Result<(), RunnerError>;
}

/// Runs a command directly on the host, with no boundary. Used by the adapter
/// tests and by nothing on any operator path: sessions always go through
/// `PodmanRunner`.
pub mod local {
    use super::*;
    use std::process::Stdio;
    use tokio::process::Command;

    /// Runs the argv directly on the host. Mounts and container name are ignored;
    /// env is applied. For adapter tests against the fake agent only.
    pub struct LocalRunner;

    #[async_trait]
    impl Runner for LocalRunner {
        async fn spawn(&self, cmd: RunnerCommand) -> Result<Child, RunnerError> {
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
            Ok(c.spawn()?)
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
}
