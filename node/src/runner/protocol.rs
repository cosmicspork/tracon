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
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tracon_sandbox::node::{SandboxConn, SandboxServer, ServerOptions, Tunnel};
use tracon_sandbox::protocol::{ExitStatus, SpawnSpec};
use tracon_sandbox::tls::NodeIdentity;

use super::tunnels::TunnelSpec;
use super::{Mount, Runner, RunnerCommand, RunnerError, Sidecar, Spawned};
use crate::provider::{
    Bootstrap, CreateSpec, Handle, ProviderError, Resources, SandboxProvider, Shape, Status,
};
use crate::telemetry::{run_kind, telemetry};

/// Why a catalogue service (by name) cannot run in the sandbox `handle`
/// (`None`: not known to this process), if it cannot.
pub type ServiceRefusal = Arc<dyn Fn(Option<&Handle>, &str) -> Option<String> + Send + Sync>;

/// How long a sandbox has to start and connect.
pub const CONNECT_WITHIN: Duration = Duration::from_secs(60);

/// How long an orderly shutdown of a finished sandbox may take before it is
/// removed regardless.
const SHUTDOWN_WITHIN: Duration = Duration::from_secs(5);

/// Where the serving node keeps its sandbox identity, under its state
/// directory.
pub const IDENTITY_DIR: &str = "sandbox-identity";

/// What `tracon serve` settles before any sandbox is made.
struct Settings {
    identity_dir: PathBuf,
    resume_within: Duration,
}

fn settings() -> &'static OnceLock<Settings> {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    &SETTINGS
}

/// For the serving node: keep the sandbox identity under the state
/// directory, so the pin every sandbox holds survives a restart, and wait
/// `[sandbox] resume_within_secs` for a dropped sandbox to redial. Called
/// before anything asks for [`node_server`]; other processes (a CLI check)
/// get an identity of their own that dies with them.
pub fn configure(cfg: &crate::config::Config) {
    let _ = settings().set(Settings {
        identity_dir: crate::config::Config::state_dir().join(IDENTITY_DIR),
        resume_within: cfg.sandbox.resume_within(),
    });
}

/// The node's side of every sandbox connection in this process: one
/// identity, and the registry of each run's tokens.
///
/// For the serving node the identity is kept (see [`configure`]): made once,
/// loaded at every start. Rotating it is removing the directory; sandboxes
/// started before refuse the next node's certificate and end their runs.
pub fn node_server() -> Result<Arc<SandboxServer>, RunnerError> {
    static SERVER: OnceLock<Arc<SandboxServer>> = OnceLock::new();
    if let Some(server) = SERVER.get() {
        return Ok(server.clone());
    }
    let failed =
        |e: tracon_sandbox::tls::TlsError| RunnerError::Other(format!("sandbox identity: {e}"));
    let mut options = ServerOptions::default();
    let identity = match settings().get() {
        Some(settings) => {
            options.resume_within = settings.resume_within;
            let (identity, made) =
                NodeIdentity::load_or_generate(&settings.identity_dir).map_err(failed)?;
            if made {
                tracing::info!(pin = %identity.pin(), dir = %settings.identity_dir.display(), "made the node's sandbox identity");
            }
            identity
        }
        None => NodeIdentity::generate().map_err(failed)?,
    };
    let server = SandboxServer::new(&identity, options).map_err(failed)?;
    Ok(SERVER.get_or_init(|| Arc::new(server)).clone())
}

/// Which sandbox each running command's name is in, for `kill`. Process-wide
/// because a runner is made afresh for each call, and the call that kills is
/// rarely the one that started.
fn running() -> &'static Mutex<HashMap<String, (Handle, String)>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, (Handle, String)>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// While a run's connection is down, ask the provider after its sandbox:
/// one that has stopped or gone will never redial, so its run ends now
/// rather than at the end of the resume window.
fn end_if_gone(
    provider: Arc<dyn SandboxProvider>,
    server: Arc<SandboxServer>,
    handle: Handle,
    conn: &SandboxConn,
) {
    if !conn.is_resumable() {
        return;
    }
    let weak = conn.downgrade();
    let run = conn.run().to_string();
    let ended = conn.ended();
    tokio::spawn(async move {
        tokio::pin!(ended);
        loop {
            tokio::select! {
                _ = weak.disconnected() => {}
                _ = &mut ended => return,
            }
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(AWAY_POLL) => {}
                    _ = &mut ended => return,
                }
                match weak.upgrade() {
                    None => return,
                    Some(conn) if conn.is_connected() => break,
                    Some(_) => {}
                }
                match provider.status(&handle).await {
                    Ok(Status::Exited { .. } | Status::Lost) | Err(ProviderError::NotFound(_)) => {
                        tracing::warn!(sandbox = %handle.0, %run, "a sandbox stopped while its connection was down; its run is over");
                        server.end_run(&run);
                        return;
                    }
                    _ => {}
                }
            }
        }
    });
}

/// How often a sandbox whose connection is down is looked for.
const AWAY_POLL: Duration = Duration::from_secs(2);

/// A session's sandbox, as the node would leave it running when it stops.
struct Kept {
    run: String,
    handle: Handle,
    server: Arc<SandboxServer>,
    conn: tracon_sandbox::node::WeakConn,
    harness: u64,
    /// Left for the next node process: its teardown neither stops nor
    /// removes it.
    detached: bool,
}

/// Every session's sandbox this process runs, by name.
fn sessions() -> &'static Mutex<HashMap<String, Kept>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Kept>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn detached(name: &str) -> bool {
    sessions()
        .lock()
        .unwrap()
        .get(name)
        .is_some_and(|kept| kept.detached)
}

/// The node is stopping: leave every session's sandbox that can resume
/// running for the next node process, with a record of it
/// (`runner::reattach`). Their sessions still end as the node stops them,
/// and their trees come back as at any end; what stays is the sandbox, its
/// services and its tunnels' listeners, which the next process takes back
/// when it carries the session on. One the next process never takes back
/// ends itself once its redials run out. The number left.
pub fn detach_all() -> usize {
    let mut left = 0;
    for (name, kept) in sessions().lock().unwrap().iter_mut() {
        if kept.detached {
            continue;
        }
        let Some(conn) = kept.conn.upgrade() else {
            continue;
        };
        if !conn.is_resumable() || conn.is_closed() {
            continue;
        }
        let Some(digest) = kept.server.tokens().resume_digest(&kept.run) else {
            continue;
        };
        let record = super::reattach::Record {
            name: name.clone(),
            run: kept.run.clone(),
            handle: kept.handle.0.clone(),
            digest: hex::encode(digest),
            harness: Some(kept.harness),
            in_turn: super::sandbox_files::in_turn(name),
            image: super::reattach::noted_image(name),
        };
        match super::reattach::keep(&record) {
            Ok(()) => {
                kept.detached = true;
                left += 1;
            }
            Err(error) => {
                tracing::warn!(sandbox = %name, %error, "could not keep a sandbox for the next node process; it is removed")
            }
        }
    }
    left
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
    /// What each sandbox's isolation, read back from the provider once it
    /// has connected, must satisfy before anything is sent into it.
    requirements: Option<crate::provider::Requirements>,
    /// How long a run's token may wait to be redeemed; `None` for as long
    /// as the run lasts.
    token_window: Option<Duration>,
    /// Every process and service this runner starts gets this environment,
    /// under what each sets itself: for a provider that does not set it on
    /// the sandbox, whose supervisor's own environment is the provider's.
    env: Vec<(String, String)>,
    /// Why a catalogue service cannot run in one of this runner's
    /// sandboxes, said before anything reaches it.
    service_refusal: Option<ServiceRefusal>,
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
            requirements: None,
            token_window: None,
            env: Vec::new(),
            service_refusal: None,
        }
    }

    /// Give every process and service this runner starts `env`, under what
    /// each sets itself.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// Refuse a catalogue service at `service_start` when `refusal` says
    /// why, rather than letting it fail inside the sandbox.
    pub fn with_service_refusal(mut self, refusal: ServiceRefusal) -> Self {
        self.service_refusal = Some(refusal);
        self
    }

    /// Why `sidecar` cannot run with this runner, before anything reaches
    /// its sandbox: the provider refuses it, it would run as a container
    /// beside a sandbox that has none, or it would listen where one of the
    /// sandbox's tunnels does.
    fn service_refusal(&self, sidecar: &Sidecar) -> Option<String> {
        let service = sidecar
            .name
            .strip_prefix(&format!("{}-svc-", sidecar.session_container))
            .unwrap_or(&sidecar.name);
        if let Some(refusal) = &self.service_refusal {
            let handle = running()
                .lock()
                .unwrap()
                .get(&sidecar.session_container)
                .map(|(handle, _)| handle.clone());
            if let Some(why) = refusal(handle.as_ref(), service) {
                return Some(why);
            }
        }
        // A tunnel's port matters only to a service inside the sandbox.
        self.in_sandbox(sidecar)?;
        self.tunnels
            .iter()
            .find(|tunnel| {
                matches!(&tunnel.listen, tracon_sandbox::protocol::ListenAddr::Tcp(addr)
                    if addr.rsplit_once(':').is_some_and(|(_, port)| port == sidecar.port.to_string()))
            })
            .map(|tunnel| {
                format!(
                    "service {service} listens on 127.0.0.1:{}, where the sandbox's {} tunnel \
                     already does; give it another port",
                    sidecar.port, tunnel.name
                )
            })
    }

    /// Hold every sandbox to `requirements`, as its provider reads its
    /// isolation back after it connects: one that falls short is removed
    /// before any file, tunnel or command reaches it.
    pub fn with_requirements(mut self, requirements: crate::provider::Requirements) -> Self {
        self.requirements = Some(requirements);
        self
    }

    /// Refuse a run's token once `window` has passed unredeemed, for a
    /// provider that may keep a copy of how the token was delivered.
    pub fn with_token_window(mut self, window: Duration) -> Self {
        self.token_window = Some(window);
        self
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
        let image = cmd.image.clone().unwrap_or_else(|| self.image.clone());
        let mounts: Vec<Mount> = self
            .extra_mounts
            .iter()
            .chain(cmd.mounts.iter())
            .cloned()
            .collect();
        let (provided, files) = match &self.files {
            Some(store) => store
                .plan_from(&mounts, &image, cmd.labels.kind)
                .map_err(|e| RunnerError::Other(format!("sandbox {name}: {e}")))?,
            None => (mounts.clone(), None),
        };
        // A session's sandbox the node's previous process left running is
        // taken back as it is: its trees, services and tunnels are there.
        let kept = match shape {
            Shape::Session => super::reattach::take(name),
            _ => None,
        };
        let adopted = kept.is_some();
        let (handle, conn, mut teardown) = match kept {
            Some(record) => self.adopt(name, record, &server).await?,
            None => {
                self.make(name, cmd, shape, &server, &provided, &image)
                    .await?
            }
        };
        if let Some(requirements) = &self.requirements {
            let verified = self
                .provider
                .verify_isolation(&handle)
                .await
                .map_err(|e| e.to_string())
                .and_then(|caps| requirements.check(&caps));
            if let Err(why) = verified {
                teardown.finish().await;
                return Err(RunnerError::Other(format!(
                    "sandbox {name} is not isolated as required: {why}"
                )));
            }
        }
        super::services::register(name, &teardown.run, conn.clone());
        if adopted {
            super::services::adopt(name).await;
        }
        if let Some(files) = files {
            // A spawn is a session's harness unless it says otherwise.
            let kind = cmd.labels.kind.unwrap_or(match stdin {
                true => super::RunKind::Session,
                false => super::RunKind::Probe,
            });
            let attached = match adopted {
                true => files.readopt(&conn, name, kind).await,
                false => files.attach(&conn, name, kind).await,
            };
            match attached {
                Ok(attached) => teardown.files = Some(attached),
                Err(error) => {
                    teardown.finish().await;
                    return Err(RunnerError::Other(format!(
                        "send files to sandbox {name}: {error}"
                    )));
                }
            }
        }
        let tunnels = match adopted {
            true => Ok(super::tunnels::adopt(&conn, &self.tunnels)),
            false => super::tunnels::open(&conn, &self.tunnels).await,
        };
        match tunnels {
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
        let env: BTreeMap<String, String> = self
            .env
            .iter()
            .chain(cmd.plain_env())
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

    /// Make the sandbox and take its first connection.
    async fn make(
        &self,
        name: &str,
        cmd: &RunnerCommand,
        shape: Shape,
        server: &Arc<SandboxServer>,
        provided: &[Mount],
        image: &str,
    ) -> Result<(Handle, SandboxConn, Teardown), RunnerError> {
        let run = format!("{name}:{}", uuid::Uuid::now_v7().simple());
        let token = match self.token_window {
            Some(window) => server.tokens().issue_within(run.clone(), window),
            None => server.tokens().issue(run.clone()),
        };
        let bootstrap = Bootstrap {
            run: run.clone(),
            cert_pin: server.pin().to_string(),
            token,
        };
        let created = {
            // While the sandbox is made, not while it runs.
            let _images = crate::boundary::reconcile::images_in_use().await;
            self.provider
                .create(&CreateSpec {
                    name: Some(name),
                    image,
                    resources: &Resources::default(),
                    mounts: provided,
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
            .insert(name.to_string(), (handle.clone(), teardown.run.clone()));
        let conn = match self
            .provider
            .connect(&handle, server, self.connect_within)
            .await
        {
            Ok(conn) => conn,
            Err(error) => {
                teardown.finish().await;
                return Err(failed(error, || format!("connect to sandbox {name}")));
            }
        };
        end_if_gone(self.provider.clone(), server.clone(), handle.clone(), &conn);
        let mut teardown = teardown;
        teardown.conn = Some(conn.clone());
        Ok((handle, conn, teardown))
    }

    /// Take back a sandbox the node's previous process left running: know
    /// its run again, set its carrier up again, and wait for it to redial.
    /// The harness that ran in it is stopped; the caller starts another,
    /// which carries the conversation on. On any failure the sandbox goes,
    /// as a sandbox that failed to start would.
    async fn adopt(
        &self,
        name: &str,
        record: super::reattach::Record,
        server: &Arc<SandboxServer>,
    ) -> Result<(Handle, SandboxConn, Teardown), RunnerError> {
        let handle = Handle(record.handle.clone());
        // What this process knew of it as the one that left it, if it is.
        sessions().lock().unwrap().remove(name);
        let Some(digest) = record.digest() else {
            let _ = self.provider.destroy(&handle).await;
            return Err(RunnerError::Other(format!(
                "sandbox {name}: its record is unreadable"
            )));
        };
        let conn = server.restore(&record.run, digest);
        let teardown = Teardown {
            provider: self.provider.clone(),
            server: server.clone(),
            name: name.to_string(),
            handle: handle.clone(),
            run: record.run.clone(),
            conn: Some(conn.clone()),
            files: None,
            tunnels: Vec::new(),
            armed: true,
        };
        running()
            .lock()
            .unwrap()
            .insert(name.to_string(), (handle.clone(), record.run.clone()));
        if let Err(error) = self.provider.reattach(&handle, server, &conn).await {
            teardown.finish().await;
            return Err(failed(error, || format!("take sandbox {name} back")));
        }
        let within = server.options().resume_within;
        if !matches!(
            tokio::time::timeout(within, conn.connected()).await,
            Ok(true)
        ) {
            teardown.finish().await;
            return Err(RunnerError::Other(format!(
                "sandbox {name} did not come back within {} s",
                within.as_secs()
            )));
        }
        tracing::info!(sandbox = %name, run = %record.run, "took back a sandbox the previous node process left running");
        end_if_gone(self.provider.clone(), server.clone(), handle.clone(), &conn);
        if let Some(id) = record.harness {
            // Usually gone already: the node closed its stdin as it stopped.
            let harness = conn.process(id);
            let _ = harness.signal_group(libc::SIGKILL).await;
            let _ = tokio::time::timeout(Duration::from_secs(10), harness.wait()).await;
        }
        Ok((handle, conn, teardown))
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
        let taken_back = teardown.conclude().await;
        let output = output.map_err(|e| RunnerError::Other(format!("{name}: {e}")))?;
        // What a preparation installed is its point: one that ran and whose
        // tree never reached the node has prepared nothing. A failed command
        // says more by its own exit.
        if let (Err(error), Some(super::RunKind::Prepare)) = (&taken_back, cmd.labels.kind) {
            if exit_code(output.status) == 0 {
                return Err(RunnerError::Other(format!(
                    "{name}: could not take its trees back: {error}"
                )));
            }
        }
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
    async fn finish(self) {
        let _ = self.conclude().await;
    }

    /// [`Self::finish`], saying whether the last take-back of its trees
    /// failed.
    async fn conclude(mut self) -> Result<(), super::sandbox_files::FilesError> {
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
        .await
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
) -> Result<(), super::sandbox_files::FilesError> {
    super::services::unregister(&name, &run);
    // While the sandbox is still there to read from, and may still come
    // back to be read from: its resume secret goes after.
    let taken_back = match files {
        Some(files) => files.conclude().await,
        None => Ok(()),
    };
    if detached(&name) {
        return taken_back;
    }
    {
        let mut kept = sessions().lock().unwrap();
        if kept.get(&name).is_some_and(|k| k.run == run) {
            kept.remove(&name);
        }
    }
    if let Some(conn) = conn {
        // Its processes get their stop signal and grace from the sandbox,
        // before the provider's removal makes sure of it.
        let _ = tokio::time::timeout(SHUTDOWN_WITHIN, conn.shutdown()).await;
    }
    server.end_run(&run);
    {
        let mut map = running().lock().unwrap();
        if map.get(&name).is_some_and(|(held, _)| *held == handle) {
            map.remove(&name);
        }
    }
    if let Err(error) = provider.destroy(&handle).await {
        tracing::warn!(sandbox = %handle.0, %error, "could not remove a sandbox");
    }
    taken_back
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
        if let Some(conn) = &teardown.conn {
            sessions().lock().unwrap().insert(
                name.clone(),
                Kept {
                    run: teardown.run.clone(),
                    handle: teardown.handle.clone(),
                    server: teardown.server.clone(),
                    conn: conn.downgrade(),
                    harness: process.id(),
                    detached: false,
                },
            );
        }
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
        // Left for the next node process, with what runs in it.
        if detached(name) || super::services::sandbox_of(name).is_some_and(|s| detached(&s)) {
            return Ok(());
        }
        // A service in a session's sandbox stops there; its sandbox stays.
        if let Some(stopped) = super::services::stop(name).await {
            return stopped;
        }
        // What it changed comes back before it goes.
        super::sandbox_files::finish_named(name).await;
        let running = running().lock().unwrap().remove(name);
        // Gone on purpose: nothing waits for it to redial.
        if let Some((_, run)) = &running {
            if let Ok(server) = self.server() {
                server.end_run(run);
            }
        }
        let handle = running
            .map(|(handle, _)| handle)
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
        if let Some(why) = self.service_refusal(sidecar) {
            return Err(RunnerError::Other(why));
        }
        if let Some(spec) = self.in_sandbox(sidecar) {
            if self.env.is_empty() {
                return super::services::start(sidecar, spec).await;
            }
            let mut spec = spec.clone();
            spec.env = self
                .env
                .iter()
                .cloned()
                .chain(std::mem::take(&mut spec.env))
                .collect();
            return super::services::start(sidecar, &spec).await;
        }
        match &self.delegate {
            Some(delegate) => delegate.start_sidecar(sidecar).await,
            None => Err(RunnerError::Other(super::SIDECARS_UNSUPPORTED.into())),
        }
    }

    fn supervises_sidecar(&self, sidecar: &Sidecar) -> bool {
        self.in_sandbox(sidecar).is_some()
    }

    fn sidecar_in_sandbox(&self, sidecar: &Sidecar) -> bool {
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
