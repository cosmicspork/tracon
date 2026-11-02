//! A runner that makes a sandbox with a [`SandboxProvider`] and runs each
//! command in it over the sandbox protocol: the provider starts
//! `tracon-sandbox`, the sandbox proves a token issued for this one run, and
//! the command's argv, environment, working directory, stdio and exit status
//! all travel over that connection. Nothing here knows which provider it is.
//!
//! One sandbox per command, as one container per command before it, under
//! the name the command was given, so `kill` and the session's services find
//! it the way they always have.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tracon_sandbox::node::{SandboxConn, SandboxServer, ServerOptions, Tunnel};
use tracon_sandbox::protocol::{ExitStatus, SpawnSpec};
use tracon_sandbox::tls::NodeIdentity;

use super::tunnels::TunnelSpec;
use super::{Mount, Runner, RunnerCommand, RunnerError, Sidecar, Spawned};
use crate::provider::{
    Bootstrap, CreateSpec, Handle, ProviderError, Resources, SandboxProvider, Shape,
};
use crate::telemetry::{run_kind, telemetry};

/// How long a sandbox has to start and connect.
pub const CONNECT_WITHIN: Duration = Duration::from_secs(60);

/// How long an orderly shutdown of a finished sandbox may take before it is
/// removed regardless.
const SHUTDOWN_WITHIN: Duration = Duration::from_secs(5);

/// The node's side of every sandbox connection in this process: one
/// identity, and the registry of each run's token.
///
/// The certificate is made when the process first needs it, so its pin
/// changes with every restart; a sandbox outliving the node that started
/// it cannot reconnect to the next one. Keeping it across restarts is for
/// when sandboxes can resume.
pub fn node_server() -> Result<Arc<SandboxServer>, RunnerError> {
    static SERVER: OnceLock<Arc<SandboxServer>> = OnceLock::new();
    if let Some(server) = SERVER.get() {
        return Ok(server.clone());
    }
    let identity = NodeIdentity::generate()
        .map_err(|e| RunnerError::Other(format!("sandbox identity: {e}")))?;
    let server = SandboxServer::new(&identity, ServerOptions::default())
        .map_err(|e| RunnerError::Other(format!("sandbox identity: {e}")))?;
    Ok(SERVER.get_or_init(|| Arc::new(server)).clone())
}

/// Which sandbox each running command's name is in, for `kill`. Process-wide
/// because a runner is made afresh for each call, and the call that kills is
/// rarely the one that started.
fn running() -> &'static Mutex<HashMap<String, Handle>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, Handle>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct ProtocolRunner {
    provider: Arc<dyn SandboxProvider>,
    /// `None` for this process's own, `node_server`.
    server: Option<Arc<SandboxServer>>,
    /// The image a command that names none runs in.
    image: String,
    /// Mounted into every sandbox this runner makes, before the command's.
    extra_mounts: Vec<Mount>,
    /// The working directory a command that names none starts in, when a
    /// mount provides it.
    workdir: String,
    connect_within: Duration,
    /// What this runner hands on rather than does: the image identity, and
    /// the services beside a session, which stay containers of their own
    /// joined to the sandbox's network.
    delegate: Option<Arc<dyn Runner>>,
    /// Where the volumes it sends over the files channel are kept, rather
    /// than mounted (`sandbox_files`).
    files: Option<super::sandbox_files::FilesStore>,
    /// Opened on every sandbox's connection before its command starts.
    tunnels: Vec<TunnelSpec>,
    /// Start a harness from the mounted bundle, where its adapter names the
    /// path a harness image had it at (`sandbox_images`).
    bundled_harness: bool,
    /// Run a catalogue service that has a bundle inside the session's
    /// sandbox, under its supervisor, rather than handing it to `delegate`.
    sandbox_services: bool,
}

impl ProtocolRunner {
    pub fn new(provider: Arc<dyn SandboxProvider>, image: impl Into<String>) -> Self {
        ProtocolRunner {
            provider,
            server: None,
            image: image.into(),
            extra_mounts: Vec::new(),
            workdir: "/work".into(),
            connect_within: CONNECT_WITHIN,
            delegate: None,
            files: None,
            tunnels: Vec::new(),
            bundled_harness: false,
            sandbox_services: false,
        }
    }

    /// Send the volumes `store` keeps over the files channel instead of
    /// mounting them.
    pub fn with_files(mut self, store: super::sandbox_files::FilesStore) -> Self {
        self.files = Some(store);
        self
    }

    /// Open `tunnels` on each sandbox's connection before its command
    /// starts, and close them with it.
    pub fn with_tunnels(mut self, tunnels: Vec<TunnelSpec>) -> Self {
        self.tunnels = tunnels;
        self
    }

    pub fn with_bundled_harness(mut self, bundled: bool) -> Self {
        self.bundled_harness = bundled;
        self
    }

    pub fn with_sandbox_services(mut self, on: bool) -> Self {
        self.sandbox_services = on;
        self
    }

    /// How `sidecar` runs in its session's sandbox, when this runner runs it
    /// there.
    fn in_sandbox<'a>(&self, sidecar: &'a Sidecar) -> Option<&'a super::InSandbox> {
        sidecar
            .in_sandbox
            .as_ref()
            .filter(|_| self.sandbox_services)
    }

    /// Accept sandboxes with `server` rather than this process's own.
    pub fn with_server(mut self, server: Arc<SandboxServer>) -> Self {
        self.server = Some(server);
        self
    }

    fn server(&self) -> Result<Arc<SandboxServer>, RunnerError> {
        match &self.server {
            Some(server) => Ok(server.clone()),
            None => node_server(),
        }
    }

    pub fn with_mounts(mut self, mounts: Vec<Mount>) -> Self {
        self.extra_mounts = mounts;
        self
    }

    pub fn with_workdir(mut self, workdir: impl Into<String>) -> Self {
        self.workdir = workdir.into();
        self
    }

    pub fn with_delegate(mut self, delegate: Arc<dyn Runner>) -> Self {
        self.delegate = Some(delegate);
        self
    }

    pub fn with_connect_within(mut self, within: Duration) -> Self {
        self.connect_within = within;
        self
    }

    /// Make the sandbox, connect, and start the command in it, its stdin and
    /// stderr piped as asked (stdout always is). On any failure the sandbox
    /// is gone again.
    async fn start(
        &self,
        name: &str,
        cmd: &RunnerCommand,
        shape: Shape,
    ) -> Result<Started, RunnerError> {
        let (stdin, stderr) = (shape == Shape::Session, shape != Shape::Session);
        cmd.secret_env.validate()?;
        if cmd.argv.is_empty() {
            return Err(RunnerError::Other("empty argv".into()));
        }
        let server = self.server()?;
        let run = format!("{name}:{}", uuid::Uuid::now_v7().simple());
        let token = server.tokens().issue(run.clone());
        let bootstrap = Bootstrap {
            run: run.clone(),
            cert_pin: server.pin().to_string(),
            token,
        };
        let mounts: Vec<Mount> = self
            .extra_mounts
            .iter()
            .chain(cmd.mounts.iter())
            .cloned()
            .collect();
        let image = cmd.image.clone().unwrap_or_else(|| self.image.clone());
        let (provided, files) = match &self.files {
            Some(store) => store
                .plan(&mounts)
                .map_err(|e| RunnerError::Other(format!("sandbox {name}: {e}")))?,
            None => (mounts.clone(), None),
        };
        let created = {
            // While the sandbox is made, not while it runs.
            let _images = crate::boundary::reconcile::images_in_use().await;
            self.provider
                .create(&CreateSpec {
                    name: Some(name),
                    image: &image,
                    resources: &Resources::default(),
                    mounts: &provided,
                    bootstrap: &bootstrap,
                    labels: &cmd.labels,
                    shape,
                })
                .await
        };
        let handle = match created {
            Ok(handle) => handle,
            Err(error) => {
                server.tokens().revoke(&run);
                return Err(failed(error, || format!("start sandbox {name}")));
            }
        };
        let teardown = Teardown {
            provider: self.provider.clone(),
            server: server.clone(),
            name: name.to_string(),
            handle: handle.clone(),
            run,
            conn: None,
            files: None,
            tunnels: Vec::new(),
            armed: true,
        };
        running()
            .lock()
            .unwrap()
            .insert(name.to_string(), handle.clone());
        let conn = match self
            .provider
            .connect(&handle, &server, self.connect_within)
            .await
        {
            Ok(conn) => conn,
            Err(error) => {
                teardown.finish().await;
                return Err(failed(error, || format!("connect to sandbox {name}")));
            }
        };
        let mut teardown = teardown;
        teardown.conn = Some(conn.clone());
        super::services::register(name, &teardown.run, conn.clone());
        if let Some(files) = files {
            // A spawn is a session's harness unless it says otherwise.
            let kind = cmd.labels.kind.unwrap_or(match stdin {
                true => super::RunKind::Session,
                false => super::RunKind::Probe,
            });
            match files.attach(&conn, name, kind).await {
                Ok(attached) => teardown.files = Some(attached),
                Err(error) => {
                    teardown.finish().await;
                    return Err(RunnerError::Other(format!(
                        "send files to sandbox {name}: {error}"
                    )));
                }
            }
        }
        match super::tunnels::open(&conn, &self.tunnels).await {
            Ok(tunnels) => teardown.tunnels = tunnels,
            Err(error) => {
                teardown.finish().await;
                return Err(RunnerError::Other(format!("sandbox {name}: {error}")));
            }
        }
        let workdir = cmd.workdir.clone().unwrap_or_else(|| self.workdir.clone());
        // As on a container: a working directory only a mount provides is
        // passed; otherwise the image's own applies, which is the same path.
        let cwd = mounts
            .iter()
            .any(|m| m.target == workdir)
            .then(|| workdir.into());
        let env: BTreeMap<String, String> = cmd
            .plain_env()
            .chain(cmd.secret_env.iter())
            .cloned()
            .collect();
        let mut argv = cmd.argv.clone();
        if self.bundled_harness {
            if let Some(bundled) = crate::sandbox_images::bundled_argv0(&argv[0]) {
                argv[0] = bundled;
            }
        }
        let spec = SpawnSpec {
            argv,
            env,
            cwd,
            stdin,
            stdout: true,
            stderr,
            ..SpawnSpec::default()
        };
        let process = match conn.spawn(spec).await {
            Ok(process) => process,
            Err(error) => {
                teardown.finish().await;
                return Err(RunnerError::Other(format!(
                    "start {} in sandbox {name}: {error}",
                    cmd.argv[0]
                )));
            }
        };
        Ok(Started { process, teardown })
    }
}

/// A provider's error as the runner's: a refusal for capacity keeps its
/// classification, anything else is said as `what` failed.
fn failed(error: ProviderError, what: impl FnOnce() -> String) -> RunnerError {
    match error {
        ProviderError::Refused { reason, message } => RunnerError::Refused { reason, message },
        error => RunnerError::Other(format!("{}: {error}", what())),
    }
}

impl ProtocolRunner {
    async fn capture(
        &self,
        cmd: RunnerCommand,
        shape: Shape,
    ) -> Result<std::process::Output, RunnerError> {
        let _in_flight = telemetry().run_started(run_kind(cmd.labels.kind, shape.as_str()));
        let name = self.capture_name(&cmd.name);
        let Started { process, teardown } = self.start(&name, &cmd, shape).await?;
        let output = process.output().await;
        teardown.finish().await;
        let output = output.map_err(|e| RunnerError::Other(format!("{name}: {e}")))?;
        Ok(std::process::Output {
            status: std_status(exit_code(output.status)),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

struct Started {
    process: tracon_sandbox::node::Process,
    teardown: Teardown,
}

/// Removes a sandbox however its command ends: awaited at the end of a run,
/// or from a task of its own when the run is dropped half way (a timeout, a
/// cancelled caller).
struct Teardown {
    provider: Arc<dyn SandboxProvider>,
    server: Arc<SandboxServer>,
    name: String,
    handle: Handle,
    run: String,
    conn: Option<SandboxConn>,
    files: Option<super::sandbox_files::Attached>,
    /// Kept open for as long as the sandbox is; dropping one stops it
    /// taking connections.
    tunnels: Vec<Tunnel>,
    armed: bool,
}

impl Teardown {
    async fn finish(mut self) {
        self.armed = false;
        let _tunnels = std::mem::take(&mut self.tunnels);
        teardown(
            self.provider.clone(),
            self.server.clone(),
            self.name.clone(),
            self.handle.clone(),
            self.run.clone(),
            self.conn.take(),
            self.files.take(),
        )
        .await;
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        drop(std::mem::take(&mut self.tunnels));
        runtime.spawn(teardown(
            self.provider.clone(),
            self.server.clone(),
            self.name.clone(),
            self.handle.clone(),
            self.run.clone(),
            self.conn.take(),
            self.files.take(),
        ));
    }
}

async fn teardown(
    provider: Arc<dyn SandboxProvider>,
    server: Arc<SandboxServer>,
    name: String,
    handle: Handle,
    run: String,
    conn: Option<SandboxConn>,
    files: Option<super::sandbox_files::Attached>,
) {
    server.tokens().revoke(&run);
    super::services::unregister(&name, &run);
    // While the sandbox is still there to read from.
    if let Some(files) = files {
        files.finish().await;
    }
    if let Some(conn) = conn {
        // Its processes get their stop signal and grace from the sandbox,
        // before the provider's removal makes sure of it.
        let _ = tokio::time::timeout(SHUTDOWN_WITHIN, conn.shutdown()).await;
    }
    {
        let mut map = running().lock().unwrap();
        if map.get(&name) == Some(&handle) {
            map.remove(&name);
        }
    }
    if let Err(error) = provider.destroy(&handle).await {
        tracing::warn!(sandbox = %handle.0, %error, "could not remove a sandbox");
    }
}

/// An exit as a container's would read: the code, or 128 and the signal.
fn exit_code(status: ExitStatus) -> i32 {
    match (status.code, status.signal) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => -1,
    }
}

fn std_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    std::process::ExitStatus::from_raw((code & 0xff) << 8)
}

#[async_trait]
impl Runner for ProtocolRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        let name = match cmd.name.is_empty() {
            true => crate::config::resource_name("h"),
            false => cmd.name.clone(),
        };
        let Started { process, teardown } = self.start(&name, &cmd, Shape::Session).await?;
        let stdio = match process.stdio().await {
            Ok(stdio) => stdio,
            Err(error) => {
                teardown.finish().await;
                return Err(RunnerError::Other(format!("attach to {name}: {error}")));
            }
        };
        let (Some(stdin), Some(stdout)) = (stdio.stdin, stdio.stdout) else {
            teardown.finish().await;
            return Err(RunnerError::Other(format!("{name} has no stdio")));
        };
        // Counted until the harness is gone.
        let in_flight = telemetry().run_started(run_kind(cmd.labels.kind, "session"));
        Ok(Spawned {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            done: Box::pin(async move {
                let _in_flight = in_flight;
                let status = process.wait().await;
                teardown.finish().await;
                status
                    .map(exit_code)
                    .map_err(|e| RunnerError::Other(format!("{name}: {e}")))
            }),
        })
    }

    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        self.capture(cmd, Shape::Command).await
    }

    async fn run_probe(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        self.capture(cmd, Shape::Probe).await
    }

    /// The sandbox the name is running in, else a sandbox of that name (one
    /// left by an earlier node process, which the provider named as asked).
    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        // A service in a session's sandbox stops there; its sandbox stays.
        if let Some(stopped) = super::services::stop(name).await {
            return stopped;
        }
        // What it changed comes back before it goes.
        super::sandbox_files::finish_named(name).await;
        let handle = running()
            .lock()
            .unwrap()
            .remove(name)
            .unwrap_or_else(|| Handle(name.to_string()));
        self.provider
            .destroy(&handle)
            .await
            .map_err(|e| RunnerError::Other(format!("remove sandbox {name}: {e}")))
    }

    /// As the container runners: two node processes can share a runtime.
    fn capture_name(&self, name: &str) -> String {
        format!(
            "{}-{}",
            if name.is_empty() {
                crate::config::resource_name("x")
            } else {
                name.to_string()
            },
            std::process::id()
        )
    }

    async fn resolved_image(&self, image: Option<&str>) -> Option<String> {
        let delegate = self.delegate.as_ref()?;
        delegate
            .resolved_image(Some(image.unwrap_or(&self.image)))
            .await
    }

    async fn start_sidecar(&self, sidecar: &Sidecar) -> Result<(), RunnerError> {
        if let Some(spec) = self.in_sandbox(sidecar) {
            return super::services::start(sidecar, spec).await;
        }
        match &self.delegate {
            Some(delegate) => delegate.start_sidecar(sidecar).await,
            None => Err(RunnerError::Other(super::SIDECARS_UNSUPPORTED.into())),
        }
    }

    fn supervises_sidecar(&self, sidecar: &Sidecar) -> bool {
        self.in_sandbox(sidecar).is_some()
    }

    async fn probe_sidecar(&self, sidecar: &Sidecar) -> Result<bool, RunnerError> {
        if let Some(spec) = self.in_sandbox(sidecar) {
            return super::services::probe(sidecar, spec).await;
        }
        match &self.delegate {
            Some(delegate) => delegate.probe_sidecar(sidecar).await,
            None => Err(RunnerError::Other(super::SIDECARS_UNSUPPORTED.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    // The runner against a real `tracon-sandbox` loop is in
    // `tests/protocol_runner.rs`: the loop reaps every child of its process,
    // so it never runs among other tests.
    use super::*;

    #[test]
    fn exits_read_as_a_containers_would() {
        assert_eq!(
            exit_code(ExitStatus {
                code: Some(2),
                signal: None
            }),
            2
        );
        assert_eq!(
            exit_code(ExitStatus {
                code: None,
                signal: Some(9)
            }),
            137
        );
        assert_eq!(std_status(137).code(), Some(137));
        assert!(std_status(0).success());
    }
}
