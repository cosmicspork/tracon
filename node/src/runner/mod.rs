//! How the node gets a harness process. `PodmanRunner` puts it inside the
//! Podman boundary, the Kubernetes runner inside a harness pod; `LocalRunner`
//! (tests only) runs the argv on the host so the adapter can be exercised
//! without containers.

pub mod kube;
pub mod podman;
pub mod toolchain;

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
    /// The port the process listens on inside the runner, for a harness that
    /// is driven over HTTP rather than over stdio. The runner makes that port
    /// reachable from the node and nowhere else — a loopback publish, or the
    /// pod's own address on the cluster network — and reports where in
    /// `Spawned::endpoint`. Unset for every stdio harness.
    pub expose: Option<u16>,
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
    /// Where the node reaches the port `RunnerCommand::expose` asked for, as
    /// `host:port`. Never a public address: a loopback publish on this host,
    /// or the harness pod's own address on the cluster network. `None` when
    /// nothing was exposed.
    pub endpoint: Option<String>,
}

/// A free TCP port on this host's loopback, for a harness the node drives over
/// HTTP. The listener is closed before the port is handed out, so this is a
/// hint rather than a reservation; the caller binds it immediately.
pub fn free_loopback_port() -> Result<u16, RunnerError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
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
            endpoint: None,
        })
    }

    /// Where the node reaches this process's exposed port.
    pub fn at(mut self, endpoint: Option<String>) -> Self {
        self.endpoint = endpoint;
        self
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
    /// The name `run_capture` gives a command, which is not always the name
    /// it was asked for: the container runners disambiguate concurrent node
    /// processes with a suffix. A caller that has to stop a capture — a
    /// timeout, a cancellation — must kill what the runtime named, so this is
    /// where that translation lives rather than being reconstructed (and
    /// getting it wrong) at the call site.
    fn capture_name(&self, name: &str) -> String {
        name.to_string()
    }
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
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex, OnceLock};
    use tokio::process::Command;

    use crate::boundary::{Backend, BoundaryError, BoundaryReport};
    use crate::config::Config;

    /// The process group each named command is running in. A container runner
    /// asks the runtime to remove a container by name; there is no runtime
    /// here, so the name has to be resolved to something killable, and the
    /// group rather than the process so a `sh -c` that spawned children takes
    /// them with it. Process-wide because `LocalRunner` is a unit struct that
    /// every call constructs afresh.
    fn running() -> &'static Mutex<HashMap<String, u32>> {
        static RUNNING: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
        RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn remember(name: &str, pid: Option<u32>) {
        if let (false, Some(pid)) = (name.is_empty(), pid) {
            if let Ok(mut map) = running().lock() {
                map.insert(name.to_string(), pid);
            }
        }
    }

    fn forget(name: &str) {
        if let Ok(mut map) = running().lock() {
            map.remove(name);
        }
    }

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
            let name = cmd.name.clone();
            let mut c = Command::new(bin);
            c.args(args)
                .envs(cmd.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            // Its own group, so `kill` reaches whatever it started too.
            #[cfg(unix)]
            c.process_group(0);
            if let Some(w) = cmd.workdir {
                c.current_dir(w);
            }
            // The process binds the loopback port itself (the adapter picked
            // it and passed it in argv); there is no boundary here to publish
            // through, so the endpoint is that same loopback pair.
            let endpoint = cmd.expose.map(|port| format!("127.0.0.1:{port}"));
            let child = c.spawn()?;
            remember(&name, child.id());
            Ok(Spawned::from_child(child)?.at(endpoint))
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
            let name = cmd.name.clone();
            let mut c = Command::new(bin);
            c.args(args)
                .envs(cmd.env)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // A capture whose future is dropped — a cancellation, a
                // timeout — must not leave the command running on the host.
                .kill_on_drop(true);
            #[cfg(unix)]
            c.process_group(0);
            if let Some(d) = dir {
                c.current_dir(d);
            }
            let child = c.spawn()?;
            remember(&name, child.id());
            let output = child.wait_with_output().await;
            forget(&name);
            Ok(output?)
        }

        /// Kill the whole process group this name was started in. A container
        /// runner removes a container; the honest local equivalent is the
        /// group, not just the direct child, or a `sh -c` that backgrounded
        /// work would survive its own cancellation.
        async fn kill(&self, name: &str) -> Result<(), RunnerError> {
            let pid = running().lock().ok().and_then(|map| map.get(name).copied());
            let Some(pid) = pid else { return Ok(()) };
            #[cfg(unix)]
            {
                let group = i32::try_from(pid).unwrap_or(0);
                if group > 0 {
                    // SAFETY: a kill(2) with a signal and a pid; it reads and
                    // writes nothing of ours and cannot fail unsafely. A
                    // negative pid addresses the group.
                    unsafe {
                        libc::kill(-group, libc::SIGKILL);
                    }
                }
            }
            forget(name);
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

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use std::time::Duration;

        /// A container runner removes a container and everything in it. The
        /// local equivalent has to be the process *group*, or a command that
        /// backgrounded work would go on running after the node killed it —
        /// which is exactly what a cancelled check does.
        #[tokio::test]
        async fn kill_reaches_the_whole_process_group() {
            let dir = std::env::temp_dir().join(format!(
                "tracon-kill-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let pid_file = dir.join("child.pid");
            let name = format!("tracon-test-kill-{}", uuid::Uuid::now_v7());
            let capture = tokio::spawn({
                let name = name.clone();
                let pid_file = pid_file.clone();
                async move {
                    LocalRunner
                        .run_capture(RunnerCommand {
                            argv: vec![
                                "sh".into(),
                                "-c".into(),
                                format!(
                                    "sleep 30 & echo $! > {}; wait",
                                    pid_file.to_string_lossy()
                                ),
                            ],
                            name,
                            ..Default::default()
                        })
                        .await
                }
            });

            let alive = |pid: &str| {
                std::process::Command::new("kill")
                    .args(["-0", pid])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false)
            };
            let mut backgrounded = None;
            for _ in 0..500 {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .map(|pid| pid.trim().to_string())
                    .filter(|pid| !pid.is_empty())
                {
                    backgrounded = Some(pid);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let backgrounded = backgrounded.expect("the command never backgrounded anything");
            assert!(alive(&backgrounded));

            LocalRunner.kill(&name).await.unwrap();
            let _ = capture.await;
            let mut gone = false;
            for _ in 0..500 {
                if !alive(&backgrounded) {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                gone,
                "work the killed command backgrounded is still running (pid {backgrounded})"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
