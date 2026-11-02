//! Rootless Podman: one container per sandbox, with `tracon-sandbox` from the
//! mounted harness bundle in place of the harness, which it then starts over
//! the sandbox protocol.
//!
//! **Carrier.** The node binds a unix socket in a directory of the sandbox's
//! own and mounts that directory read-only into the container; the
//! supervisor dials it (`TRACON_SANDBOX_SOCKET`). Under SELinux a confined
//! container (`container_t`) may not connect to a socket carrying the node's
//! label, so the socket is created with the container's
//! ([`tracon_sandbox::carrier::bind_unix_labelled`]) in a directory labelled
//! `container_file_t`. The other way round (the supervisor listening in a
//! mounted directory, the node connecting in, as on Apple) needs no socket
//! label, but the socket then belongs to whatever user the image runs as,
//! and the node may not connect to one a subordinate uid made; and the
//! supervisor stops listening after one connection, where a node listener
//! can take the supervisor's connection again.
//!
//! **Network.** Unchanged from a harness container: the boundary's internal
//! network, the gateway its only peer, the same proxy environment. Only
//! control and stdio move to the protocol.
//!
//! **Bundle.** Mounted read-only at `/opt/tracon` and relabelled shared, never
//! with `label=disable`.
//!
//! **Token.** On `podman run` as `--env TRACON_SANDBOX_TOKEN` with the value
//! in podman's own environment, so it is on no argv. Podman keeps it in the
//! container's configuration until the container is removed; it is spent
//! once the supervisor has connected.
//!
//! Isolation is read back from `podman inspect`, never from inside.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use tracon_sandbox::carrier::{bind_unix_labelled, label_path, Listener};
use tracon_sandbox::node::{SandboxConn, SandboxError, SandboxServer};

use super::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};
use super::{
    Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    SandboxProvider, Status,
};
use crate::runner::podman::{
    RunSpec, SandboxEntry, SandboxLaunch, BUNDLE_TARGET, SANDBOX_LABEL, SOCKET_DIR_TARGET,
};

/// The label the node's socket is created with, so a container may connect.
const SOCKET_CONTEXT: &str = "system_u:system_r:container_t:s0";
/// The label of the directory holding it, so a container may reach it.
const DIR_CONTEXT: &str = "system_u:object_r:container_file_t:s0";
/// The socket's name in its directory.
const SOCKET_NAME: &str = "node.sock";
/// Linux's `sun_path`, including the terminating NUL.
const SUN_PATH_MAX: usize = 108;

/// How the node runs `podman`. A trait so tests can stand in for it.
#[async_trait]
pub trait PodmanCli: Send + Sync {
    /// Run `podman` with `args`, and `env` added to its own environment;
    /// stdout, or stderr as the error.
    async fn run(&self, args: &[String], env: &[(String, String)]) -> Result<String, CliError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Failed(String),
    #[error("run podman: {0}")]
    Spawn(std::io::Error),
}

impl CliError {
    fn not_found(&self) -> bool {
        matches!(self, CliError::Failed(text)
            if text.contains("no such container") || text.contains("no such object"))
    }
}

impl From<CliError> for ProviderError {
    fn from(error: CliError) -> Self {
        ProviderError::Failed(error.to_string())
    }
}

/// The real `podman`, through the path guard.
pub struct SystemCli {
    pub bin: String,
}

#[async_trait]
impl PodmanCli for SystemCli {
    async fn run(&self, args: &[String], env: &[(String, String)]) -> Result<String, CliError> {
        let bin = crate::config::path_guard::program("boundary.podman", &self.bin)
            .map_err(CliError::Spawn)?;
        let out = tokio::process::Command::new(bin)
            .args(args)
            .envs(env.iter().cloned())
            .env("PATH", crate::config::path_guard::child_path())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(CliError::Spawn)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(CliError::Failed(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }
}

/// What the provider is configured with.
#[derive(Debug, Clone)]
pub struct PodmanOptions {
    /// The boundary each sandbox gets: network, gateway, the gate, the
    /// stop timeout, the proxy and state environment. Its image is the
    /// default when a create names none, its `extra_mounts` are mounted
    /// into every sandbox, and its `selinux_label_disable` is ignored.
    pub spec: RunSpec,
    /// The bundle for this host's architecture, holding
    /// `bin/tracon-sandbox`.
    pub bundle: PathBuf,
    /// Where each sandbox's socket directory is made. Short, because a
    /// socket path longer than 107 bytes cannot be bound, and private to the
    /// node.
    pub socket_dir: PathBuf,
    /// Start the supervisor under Podman's `--init` rather than as PID 1.
    pub init: bool,
    /// Most sandboxes at once; `None` for no limit of the provider's own.
    pub limit: Option<u32>,
    /// How long the supervisor keeps dialling.
    pub connect_timeout: Duration,
}

pub struct PodmanProvider {
    cli: Box<dyn PodmanCli>,
    options: PodmanOptions,
    /// Bound at create, taken by the first connect.
    listeners: Mutex<HashMap<String, Listener>>,
}

impl PodmanProvider {
    pub fn new(options: PodmanOptions) -> Self {
        let bin = options.spec.podman_bin.clone();
        Self::with_cli(options, Box::new(SystemCli { bin }))
    }

    pub fn with_cli(options: PodmanOptions, cli: Box<dyn PodmanCli>) -> Self {
        PodmanProvider {
            cli,
            options,
            listeners: Mutex::new(HashMap::new()),
        }
    }

    /// The provider a node configured for it runs with, around `spec`.
    pub fn from_config(cfg: &crate::config::Config, mut spec: RunSpec) -> Self {
        spec.selinux_label_disable = false;
        Self::new(PodmanOptions {
            spec,
            bundle: cfg.sandbox.bundle.clone().unwrap_or_default(),
            socket_dir: cfg.sandbox_socket_dir(),
            init: cfg.runtime.podman.sandbox_init,
            limit: None,
            connect_timeout: crate::runner::protocol::CONNECT_WITHIN,
        })
    }

    pub fn options(&self) -> &PodmanOptions {
        &self.options
    }

    /// The directory holding the node's socket for the sandbox `name`: named
    /// by a digest of the name so its length does not depend on it.
    pub fn socket_dir(&self, name: &str) -> PathBuf {
        let digest = hex::encode(Sha256::digest(name.as_bytes()));
        self.options.socket_dir.join(&digest[..16])
    }

    /// Where the sandbox `name`'s supervisor log is kept: beside its socket
    /// directory, not in it, so nothing inside can read it.
    pub fn log_file(&self, name: &str) -> PathBuf {
        let dir = self.socket_dir(name);
        dir.with_extension("log")
    }

    async fn cli(&self, args: &[&str]) -> Result<String, CliError> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.cli.run(&args, &[]).await
    }

    /// The argv `create` runs for `name`, and the boundary checks' probe
    /// creates: the provider's spec with the bundle, the socket directory
    /// and the bootstrap pin.
    pub fn run_args(
        &self,
        name: &str,
        spec: &CreateSpec<'_>,
        entry: SandboxEntry,
        create_only: bool,
    ) -> Vec<String> {
        let launch = SandboxLaunch {
            bundle: self.options.bundle.clone(),
            socket_dir: self.socket_dir(name),
            pin: spec.bootstrap.cert_pin.clone(),
            entry,
            cpu_millis: spec.resources.cpu_millis,
            memory_bytes: spec.resources.memory_bytes,
            connect_timeout_ms: self.options.connect_timeout.as_millis() as u64,
            log_file: (!create_only).then(|| self.log_file(name)),
        };
        let image = match spec.image.is_empty() {
            true => self.options.spec.image.as_str(),
            false => spec.image,
        };
        self.options
            .spec
            .sandbox_args(name, image, spec.mounts, &launch, create_only)
    }

    /// How the supervisor starts in `image`: an image with an entrypoint of
    /// its own keeps it and runs the supervisor as its command, as it would
    /// have run the harness.
    pub async fn entry_for(&self, image: &str) -> Result<SandboxEntry, ProviderError> {
        if self.options.init {
            return Ok(SandboxEntry::UnderInit);
        }
        let inspect = [
            "image",
            "inspect",
            "--format",
            "{{json .Config.Entrypoint}}",
            image,
        ];
        let text = match self.cli(&inspect).await {
            Ok(text) => text,
            // Not here yet: `podman run` would have pulled it, so pull it.
            Err(_) => {
                self.cli(&["pull", "--quiet", image])
                    .await
                    .map_err(|e| ProviderError::Failed(format!("pull {image}: {e}")))?;
                self.cli(&inspect).await?
            }
        };
        let entrypoint: Option<Vec<String>> = serde_json::from_str(text.trim())
            .map_err(|e| ProviderError::Failed(format!("image inspect {image}: {e}")))?;
        Ok(match entrypoint.is_some_and(|e| !e.is_empty()) {
            true => SandboxEntry::UnderImageEntrypoint,
            false => SandboxEntry::Pid1,
        })
    }

    /// A private parent, and a directory for this sandbox's socket in it
    /// that a container may reach, empty.
    pub fn prepare_dir(&self, name: &str) -> Result<PathBuf, ProviderError> {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        let root = &self.options.socket_dir;
        let failed = |what: &str, path: &Path, e: std::io::Error| {
            ProviderError::Failed(format!("{what} {}: {e}", path.display()))
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)
            .map_err(|e| failed("create", root, e))?;
        let meta = std::fs::symlink_metadata(root).map_err(|e| failed("read", root, e))?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            return Err(ProviderError::Failed(format!(
                "{} is not a directory this user owns",
                root.display()
            )));
        }
        // Host users stay out at the parent; the bind mount is how the
        // container reaches the directory below it.
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| failed("restrict", root, e))?;
        }
        let dir = self.socket_dir(name);
        let socket = dir.join(SOCKET_NAME);
        if socket.as_os_str().len() >= SUN_PATH_MAX {
            return Err(ProviderError::Failed(format!(
                "socket path {} is too long; choose a shorter socket directory",
                socket.display()
            )));
        }
        // Left by a sandbox of the same name that was never destroyed.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&dir)
            .map_err(|e| failed("create", &dir, e))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| failed("open", &dir, e))?;
        label_path(&dir, DIR_CONTEXT).map_err(|e| failed("label", &dir, e))?;
        Ok(dir)
    }

    /// [`Self::prepare_dir`], with the node's socket bound in it, labelled
    /// so a container may connect.
    fn bind(&self, name: &str) -> Result<Listener, ProviderError> {
        use std::os::unix::fs::PermissionsExt as _;
        let failed = |what: &str, path: &Path, e: std::io::Error| {
            ProviderError::Failed(format!("{what} {}: {e}", path.display()))
        };
        let socket = self.prepare_dir(name)?.join(SOCKET_NAME);
        let listener = bind_unix_labelled(&socket, Some(SOCKET_CONTEXT))
            .map_err(|e| failed("bind", &socket, e))?;
        // Whatever uid the image runs as: the directory's parent keeps the
        // host's other users out, and the token keeps out anything else.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| failed("open", &socket, e))?;
        Ok(Listener::Unix(listener))
    }

    async fn inspect(&self, handle: &Handle) -> Result<serde_json::Value, ProviderError> {
        let text = match self
            .cli(&["container", "inspect", "--format", "json", &handle.0])
            .await
        {
            Ok(text) => text,
            Err(error) if error.not_found() => {
                return Err(ProviderError::NotFound(handle.0.clone()))
            }
            Err(error) => return Err(error.into()),
        };
        let mut all: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Failed(format!("podman inspect: {e}")))?;
        match all.len() {
            1 => Ok(all.remove(0)),
            0 => Err(ProviderError::NotFound(handle.0.clone())),
            n => Err(ProviderError::Failed(format!(
                "podman inspect {} returned {n} containers",
                handle.0
            ))),
        }
    }

    async fn network_inspect(&self) -> Result<serde_json::Value, ProviderError> {
        let text = self
            .cli(&[
                "network",
                "inspect",
                &self.options.spec.network,
                "--format",
                "json",
            ])
            .await?;
        let all: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Failed(format!("podman network inspect: {e}")))?;
        all.into_iter().next().ok_or_else(|| {
            ProviderError::Failed(format!("no network {}", self.options.spec.network))
        })
    }

    /// The last lines a sandbox's supervisor wrote, for an error about it.
    fn logs(&self, handle: &Handle) -> String {
        let Ok(text) = std::fs::read_to_string(self.log_file(&handle.0)) else {
            return "it wrote nothing".into();
        };
        let lines: Vec<&str> = text
            .lines()
            // `<time> <stream> <F|P> <text>`, as Podman's k8s-file writes it.
            .map(|line| line.splitn(4, ' ').nth(3).unwrap_or(line))
            .collect();
        lines[lines.len().saturating_sub(20)..].join("\n")
    }

    async fn ensure_volumes(&self, spec: &CreateSpec<'_>) -> Result<(), ProviderError> {
        let mut seen = std::collections::BTreeSet::new();
        for mount in self.options.spec.extra_mounts.iter().chain(spec.mounts) {
            if mount.is_local() || !seen.insert(&mount.volume) {
                continue;
            }
            self.cli(&["volume", "create", "--ignore", &mount.volume])
                .await
                .map_err(|e| {
                    ProviderError::Failed(format!("create runtime volume {}: {e}", mount.volume))
                })?;
        }
        Ok(())
    }
}

/// What `podman inspect` shows of a container's boundary, as the checks read
/// it.
fn breaches(c: &serde_json::Value, bundle: &Path, selinux: bool) -> Vec<String> {
    let mut found = Vec::new();
    let hc = &c["HostConfig"];
    if hc["Privileged"].as_bool().unwrap_or(true) {
        found.push("the container is privileged".into());
    }
    if hc["CapAdd"].as_array().is_some_and(|a| !a.is_empty()) {
        found.push("the container adds capabilities".into());
    }
    if c["EffectiveCaps"].as_array().is_some_and(|a| !a.is_empty()) {
        found.push("the container keeps effective capabilities".into());
    }
    let security: Vec<&str> = hc["SecurityOpt"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
        .unwrap_or_default();
    let nnp = security.iter().any(|s| s.contains("no-new-privileges"))
        || c["Config"]["Annotations"]["run.oci.no_new_privileges"] == "true"
        || hc["NoNewPrivileges"].as_bool().unwrap_or(false);
    if !nnp {
        found.push("no-new-privileges is not set".into());
    }
    if security.iter().any(|s| s.contains("label=disable")) {
        found.push("SELinux separation is disabled".into());
    }
    if selinux && c["ProcessLabel"].as_str().unwrap_or("").is_empty() {
        found.push("the container runs with no SELinux label".into());
    }
    let mounts = c["Mounts"].as_array().cloned().unwrap_or_default();
    match mounts
        .iter()
        .find(|m| m["Destination"].as_str() == Some(BUNDLE_TARGET))
    {
        None => found.push(format!("the bundle is not mounted at {BUNDLE_TARGET}")),
        Some(m) => {
            if m["Source"].as_str().map(Path::new) != Some(bundle) {
                found.push(format!("{BUNDLE_TARGET} is not the bundle"));
            }
            if m["RW"].as_bool().unwrap_or(true) {
                found.push("the bundle is mounted writable".into());
            }
        }
    }
    if let Some(m) = mounts
        .iter()
        .find(|m| m["Destination"].as_str() == Some(SOCKET_DIR_TARGET))
    {
        if m["RW"].as_bool().unwrap_or(true) {
            found.push("the socket directory is mounted writable".into());
        }
    }
    for m in &mounts {
        for side in ["Source", "Destination"] {
            let path = m[side].as_str().unwrap_or("").to_ascii_lowercase();
            if path.ends_with(".sock")
                || path.contains("docker.sock")
                || path.contains("podman.sock")
            {
                found.push(format!("a socket is mounted: {path}"));
            }
        }
    }
    found
}

fn networks(c: &serde_json::Value) -> Vec<String> {
    c["NetworkSettings"]["Networks"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Whether the sandbox's one network is the boundary's internal one, and
/// what answers names on it.
fn route_and_dns(attached: &[String], expected: &str, network: &serde_json::Value) -> (Route, Dns) {
    let internal = network["internal"].as_bool().unwrap_or(false);
    let dns = match network["dns_enabled"].as_bool().unwrap_or(true) {
        false => Dns::None,
        true => Dns::ProviderOpen,
    };
    match attached == [expected.to_string()] && internal {
        true => (Route::NodeOnly, dns),
        false => (Route::Unverified, dns),
    }
}

#[async_trait]
impl SandboxProvider for PodmanProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Podman
    }

    async fn create(&self, spec: &CreateSpec<'_>) -> Result<Handle, ProviderError> {
        let name = match spec.name {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => crate::config::resource_name(&format!(
                "s-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            )),
        };
        let handle = Handle(name.clone());
        if !self.options.bundle.join("bin/tracon-sandbox").is_file() {
            return Err(ProviderError::Failed(format!(
                "{} has no bin/tracon-sandbox; set [sandbox] bundle to a harness bundle for this host",
                self.options.bundle.display()
            )));
        }
        self.ensure_volumes(spec).await?;
        let image = match spec.image.is_empty() {
            true => self.options.spec.image.clone(),
            false => spec.image.to_string(),
        };
        let entry = self.entry_for(&image).await?;
        let listener = self.bind(&name)?;
        self.listeners
            .lock()
            .unwrap()
            .insert(name.clone(), listener);
        let args = self.run_args(&name, spec, entry, false);
        let token = [(
            "TRACON_SANDBOX_TOKEN".to_string(),
            spec.bootstrap.token.clone(),
        )];
        match self.cli.run(&args, &token).await {
            Ok(_) => Ok(handle),
            Err(error) => {
                // A failed run can still leave the container behind.
                let _ = self.destroy(&handle).await;
                Err(error.into())
            }
        }
    }

    /// Accept the supervisor's connection on the sandbox's socket. A
    /// connection that fails its handshake is not the supervisor's, and
    /// the next is waited for; a sandbox that stops first fails at once,
    /// with what it wrote.
    async fn connect(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        within: Duration,
    ) -> Result<SandboxConn, ProviderError> {
        let listener = self
            .listeners
            .lock()
            .unwrap()
            .remove(&handle.0)
            .ok_or_else(|| {
                ProviderError::Failed(format!("{} has no listener to connect on", handle.0))
            })?;
        let deadline = tokio::time::Instant::now() + within;
        let accept = async {
            loop {
                match server.accept_from(&listener).await {
                    Ok(conn) => return Ok(conn),
                    Err(
                        error @ (SandboxError::BadToken
                        | SandboxError::Handshake(_)
                        | SandboxError::Tls(_)
                        | SandboxError::Timeout),
                    ) => {
                        tracing::warn!(sandbox = %handle.0, %error, "refused a connection on a sandbox's socket");
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let watch = async {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                match self.status(handle).await {
                    Ok(Status::Starting | Status::Running) => {}
                    Ok(status) => return status,
                    Err(_) => return Status::Lost,
                }
            }
        };
        tokio::select! {
            accepted = tokio::time::timeout_at(deadline, accept) => match accepted {
                Ok(Ok(conn)) => Ok(conn),
                Ok(Err(error)) => Err(ProviderError::Failed(format!("{}: {error}", handle.0))),
                Err(_) => Err(ProviderError::Failed(format!(
                    "{} did not connect within {} s: {}",
                    handle.0,
                    within.as_secs(),
                    self.logs(handle)
                ))),
            },
            ended = watch => Err(ProviderError::Failed(format!(
                "{} stopped before connecting ({ended:?}): {}",
                handle.0,
                self.logs(handle)
            ))),
        }
    }

    /// Stop (the supervisor tears its processes down on the stop signal,
    /// and `--stop-timeout` bounds how long it may take), then remove.
    async fn destroy(&self, handle: &Handle) -> Result<(), ProviderError> {
        self.listeners.lock().unwrap().remove(&handle.0);
        let timeout = self.options.spec.stop_timeout_secs.to_string();
        let _ = self
            .cli(&["stop", "--time", &timeout, "--ignore", &handle.0])
            .await;
        // `--volumes`: the sandbox's own storage goes with it.
        let removed = self
            .cli(&["rm", "--force", "--volumes", "--ignore", &handle.0])
            .await;
        let _ = std::fs::remove_file(self.log_file(&handle.0));
        let dir = self.socket_dir(&handle.0);
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(ProviderError::Failed(format!(
                    "remove {}: {e}",
                    dir.display()
                )))
            }
            _ => {}
        }
        removed.map(|_| ()).map_err(Into::into)
    }

    async fn status(&self, handle: &Handle) -> Result<Status, ProviderError> {
        let c = match self.inspect(handle).await {
            Err(ProviderError::NotFound(_)) => return Ok(Status::Lost),
            other => other?,
        };
        let state = &c["State"];
        Ok(match state["Status"].as_str().unwrap_or("") {
            "created" | "configured" | "initialized" => Status::Starting,
            "running" | "stopping" => Status::Running,
            "paused" => Status::Paused,
            "exited" | "stopped" => Status::Exited {
                code: state["ExitCode"].as_i64().unwrap_or(-1) as i32,
            },
            _ => Status::Lost,
        })
    }

    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError> {
        let c = self.inspect(handle).await?;
        let digest = c["ImageDigest"]
            .as_str()
            .filter(|d| !d.is_empty())
            .or_else(|| c["Image"].as_str())
            .unwrap_or("")
            .to_string();
        let digest = match digest.starts_with("sha256:") || digest.is_empty() {
            true => digest,
            false => format!("sha256:{digest}"),
        };
        Ok(ImageIdentity::Exact { digest })
    }

    async fn network(&self, handle: &Handle) -> Result<Network, ProviderError> {
        let c = self.inspect(handle).await?;
        let attached = networks(&c);
        let network = self.network_inspect().await?;
        let (_, dns) = route_and_dns(&attached, &self.options.spec.network, &network);
        let mut destinations: Vec<String> =
            attached.iter().map(|n| format!("network:{n}")).collect();
        destinations.push(format!(
            "unix:{}",
            self.socket_dir(&handle.0).join(SOCKET_NAME).display()
        ));
        Ok(Network { destinations, dns })
    }

    async fn verify_isolation(&self, handle: &Handle) -> Result<Capabilities, ProviderError> {
        let c = self.inspect(handle).await?;
        let labels = &c["Config"]["Labels"];
        let (key, value) = SANDBOX_LABEL.split_once('=').expect("label is key=value");
        if labels[key].as_str() != Some(value) {
            return Err(ProviderError::Failed(format!(
                "{} was not made by this provider",
                handle.0
            )));
        }
        let found = breaches(
            &c,
            &self.options.bundle,
            tracon_sandbox::carrier::selinux_enabled(),
        );
        if !found.is_empty() {
            return Err(ProviderError::Failed(found.join("; ")));
        }
        let network = self.network_inspect().await?;
        let (route, dns) = route_and_dns(&networks(&c), &self.options.spec.network, &network);
        Ok(Capabilities {
            route,
            dns,
            ..self.capabilities()
        })
    }

    /// What today's boundary checks establish: an internal network whose
    /// only peer is the gateway, with no DNS.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tier: Tier::Container,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::Unprivileged,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        }
    }

    async fn capacity(&self) -> Result<Capacity, ProviderError> {
        // Every node on the container host labels its sandboxes alike; only
        // the name says which are this node's.
        let text = self
            .cli(&[
                "ps",
                "--filter",
                &format!("label={SANDBOX_LABEL}"),
                "--filter",
                &format!("name=^{}-", crate::config::resource_prefix()),
                "--filter",
                "status=running",
                "--format",
                "{{.ID}}",
            ])
            .await?;
        Ok(Capacity {
            running: text.lines().filter(|l| !l.trim().is_empty()).count() as u32,
            limit: self.options.limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Bootstrap, Resources};
    use crate::runner::Mount;
    use std::sync::Arc;

    /// One `podman` call: its argv, and what was added to its environment.
    type Call = (Vec<String>, Vec<(String, String)>);

    /// Answers `podman` from a script and records every call.
    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<Call>>>,
        entrypoint: Arc<Mutex<Option<String>>>,
        inspect: Arc<Mutex<Option<String>>>,
        internal: bool,
        fail_run: bool,
        /// The image is not in the store until pulled.
        pulled: Option<Arc<Mutex<bool>>>,
    }

    #[async_trait]
    impl PodmanCli for Fake {
        async fn run(&self, args: &[String], env: &[(String, String)]) -> Result<String, CliError> {
            self.calls
                .lock()
                .unwrap()
                .push((args.to_vec(), env.to_vec()));
            let first: Vec<&str> = args.iter().take(2).map(String::as_str).collect();
            match first.as_slice() {
                ["image", "inspect"]
                    if self.pulled.as_ref().is_some_and(|p| !*p.lock().unwrap()) =>
                {
                    Err(CliError::Failed("Error: no such image".into()))
                }
                ["pull", ..] => {
                    if let Some(pulled) = &self.pulled {
                        *pulled.lock().unwrap() = true;
                    }
                    Ok(String::new())
                }
                ["image", "inspect"] => Ok(self
                    .entrypoint
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "null".into())),
                ["volume", "create"] => Ok(String::new()),
                ["run", ..] => match self.fail_run {
                    true => Err(CliError::Failed("Error: no such image".into())),
                    false => Ok("0123abcd\n".into()),
                },
                ["container", "inspect"] => match self.inspect.lock().unwrap().clone() {
                    Some(text) => Ok(text),
                    None => Err(CliError::Failed(format!(
                        "Error: no such container {}",
                        args[4]
                    ))),
                },
                ["network", "inspect"] => Ok(serde_json::json!([{
                    "name": "tracon-int", "internal": self.internal, "dns_enabled": false
                }])
                .to_string()),
                ["stop", ..] | ["rm", ..] => Ok(String::new()),
                ["ps", ..] => Ok("aa\nbb\n".into()),
                other => panic!("unexpected podman {other:?}"),
            }
        }
    }

    struct Setup {
        provider: PodmanProvider,
        fake: Fake,
        dir: tempfile::TempDir,
    }

    fn setup(fake: Fake) -> Setup {
        let dir = tempfile::Builder::new()
            .prefix("pp")
            .tempdir_in("/tmp")
            .unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(bundle.join("bin")).unwrap();
        std::fs::write(bundle.join("bin/tracon-sandbox"), b"").unwrap();
        let mut spec = RunSpec::from_config(&crate::config::Config::default(), true);
        spec.extra_mounts = vec![Mount::volume(
            "tracon-harness-state",
            "/root/.claude",
            false,
        )];
        let provider = PodmanProvider::with_cli(
            PodmanOptions {
                spec,
                bundle,
                socket_dir: dir.path().join("s"),
                init: false,
                limit: Some(4),
                connect_timeout: Duration::from_secs(60),
            },
            Box::new(fake.clone()),
        );
        Setup {
            provider,
            fake,
            dir,
        }
    }

    fn bootstrap() -> Bootstrap {
        Bootstrap {
            run: "run-1".into(),
            cert_pin: format!("sha256:{}", "ab".repeat(32)),
            token: "run-secret".into(),
        }
    }

    async fn create(s: &Setup, name: &str, mounts: &[Mount]) -> Result<Handle, ProviderError> {
        s.provider
            .create(&CreateSpec {
                name: Some(name),
                image: "localhost/harness@sha256:aa",
                resources: &Resources::default(),
                mounts,
                bootstrap: &bootstrap(),
            })
            .await
    }

    fn run_call(s: &Setup) -> Call {
        s.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(args, _)| args[0] == "run")
            .cloned()
            .unwrap()
    }

    #[tokio::test]
    async fn create_runs_the_supervisor_as_pid_1_with_the_boundary_and_the_bundle_read_only() {
        let s = setup(Fake::default());
        let handle = create(
            &s,
            "tracon-h-1",
            &[Mount::volume("tracon-ws-1", "/work", false)],
        )
        .await
        .unwrap();
        assert_eq!(handle.0, "tracon-h-1");
        let (args, env) = run_call(&s);
        let line = args.join(" ");
        for expected in [
            "run --detach --rm --name tracon-h-1 --label tracon.provider=podman",
            "--network tracon-int --add-host tracon-gw:",
            "--cap-drop=ALL --security-opt=no-new-privileges --stop-signal SIGTERM",
            "-e HTTPS_PROXY=http://tracon-gw:8888",
            "-e TRACON_SANDBOX_SOCKET=/run/tracon/node.sock",
            &format!("-e TRACON_SANDBOX_PIN=sha256:{}", "ab".repeat(32)),
            &format!(
                "--env TRACON_SANDBOX_TOKEN --log-driver k8s-file --log-opt path={} --mount",
                s.provider.log_file("tracon-h-1").display()
            ),
            &format!(
                "--mount type=bind,src={},dst=/opt/tracon,ro,relabel=shared",
                s.provider.options.bundle.display()
            ),
            &format!(
                "--mount type=bind,src={},dst=/run/tracon,ro",
                s.provider.socket_dir("tracon-h-1").display()
            ),
            "--mount type=volume,src=tracon-harness-state,dst=/root/.claude",
            "--mount type=volume,src=tracon-ws-1,dst=/work",
            "--entrypoint /opt/tracon/bin/tracon-sandbox localhost/harness@sha256:aa",
        ] {
            assert!(line.contains(expected), "{expected:?} not in {line}");
        }
        assert!(line.ends_with("localhost/harness@sha256:aa"), "{line}");
        // The selinux spec asked for it; a sandbox never gets it.
        assert!(!line.contains("label=disable"), "{line}");
        assert!(!line.contains("--init"), "{line}");
        assert!(!line.contains("run-secret"), "the token stays off the argv");
        assert_eq!(
            env,
            [("TRACON_SANDBOX_TOKEN".to_string(), "run-secret".to_string())]
        );
        let calls = s.fake.calls.lock().unwrap().clone();
        assert!(calls
            .iter()
            .any(|(a, _)| a.join(" ") == "volume create --ignore tracon-ws-1"));

        // The node's socket is waiting in a directory of the sandbox's own.
        use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
        let socket = s.provider.socket_dir("tracon-h-1").join(SOCKET_NAME);
        let meta = std::fs::symlink_metadata(&socket).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o666);
        let root = std::fs::metadata(&s.provider.options.socket_dir).unwrap();
        assert_eq!(root.permissions().mode() & 0o777, 0o700);
    }

    #[tokio::test]
    async fn an_image_with_an_entrypoint_keeps_it_and_init_mode_uses_podmans() {
        let fake = Fake::default();
        *fake.entrypoint.lock().unwrap() = Some(r#"["/usr/bin/entry.sh"]"#.into());
        let s = setup(fake);
        create(&s, "tracon-h-2", &[]).await.unwrap();
        let line = run_call(&s).0.join(" ");
        assert!(
            line.ends_with("localhost/harness@sha256:aa /opt/tracon/bin/tracon-sandbox"),
            "{line}"
        );
        assert!(!line.contains("--entrypoint"), "{line}");

        let mut s = setup(Fake::default());
        s.provider.options.init = true;
        create(&s, "tracon-h-3", &[]).await.unwrap();
        let line = run_call(&s).0.join(" ");
        assert!(line.contains(" --init "), "{line}");
        assert!(
            line.contains("--entrypoint /opt/tracon/bin/tracon-sandbox"),
            "{line}"
        );
        // Init mode decides without asking the image.
        assert!(!s
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(a, _)| a[0] == "image"));
    }

    #[tokio::test]
    async fn an_image_not_in_the_store_is_pulled_as_podman_run_would() {
        let s = setup(Fake {
            pulled: Some(Arc::new(Mutex::new(false))),
            ..Fake::default()
        });
        create(&s, "tracon-h-6", &[]).await.unwrap();
        let calls: Vec<String> = s
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| a[0].clone())
            .collect();
        let at = |what: &str| calls.iter().position(|c| c == what).unwrap();
        assert!(at("pull") < at("run"), "{calls:?}");
    }

    #[tokio::test]
    async fn a_failed_run_is_cleaned_up_and_a_missing_bundle_refused() {
        let s = setup(Fake {
            fail_run: true,
            ..Fake::default()
        });
        let error = create(&s, "tracon-h-4", &[]).await.unwrap_err();
        assert!(error.to_string().contains("no such image"), "{error}");
        let calls: Vec<String> = s
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| a.join(" "))
            .collect();
        assert!(
            calls.contains(&"rm --force --volumes --ignore tracon-h-4".to_string()),
            "{calls:?}"
        );
        assert!(!s.provider.socket_dir("tracon-h-4").exists());

        let s = setup(Fake::default());
        std::fs::remove_file(s.provider.options.bundle.join("bin/tracon-sandbox")).unwrap();
        let error = create(&s, "tracon-h-5", &[]).await.unwrap_err();
        assert!(error.to_string().contains("[sandbox] bundle"), "{error}");
        assert!(s.fake.calls.lock().unwrap().is_empty());
    }

    /// `podman inspect` of a sandbox as `create` makes it.
    fn inspected(s: &Setup) -> serde_json::Value {
        serde_json::json!([{
            "Image": "29d3eabf95a3",
            "ImageDigest": "sha256:29d3eabf",
            "ProcessLabel": "system_u:system_r:container_t:s0:c1,c2",
            "EffectiveCaps": [],
            "Config": {"Labels": {"tracon.provider": "podman"}, "Annotations": {}},
            "HostConfig": {"Privileged": false, "CapAdd": [], "SecurityOpt": ["no-new-privileges"]},
            "Mounts": [
                {"Source": s.provider.options.bundle, "Destination": "/opt/tracon", "RW": false},
                {"Source": s.provider.socket_dir("tracon-h-1"), "Destination": "/run/tracon", "RW": false},
                {"Source": "/var/lib/vol", "Destination": "/work", "RW": true},
            ],
            "NetworkSettings": {"Networks": {"tracon-int": {"IPAddress": "10.89.0.5"}}},
            "State": {"Status": "running", "ExitCode": 0},
        }])
    }

    #[tokio::test]
    async fn verify_reads_back_the_boundary() {
        let s = setup(Fake {
            internal: true,
            ..Fake::default()
        });
        *s.fake.inspect.lock().unwrap() = Some(inspected(&s).to_string());
        let handle = Handle("tracon-h-1".into());
        assert_eq!(
            s.provider.verify_isolation(&handle).await.unwrap(),
            s.provider.capabilities()
        );
        assert_eq!(s.provider.status(&handle).await.unwrap(), Status::Running);
        assert_eq!(
            s.provider.image_identity(&handle).await.unwrap(),
            ImageIdentity::Exact {
                digest: "sha256:29d3eabf".into()
            }
        );
        let network = s.provider.network(&handle).await.unwrap();
        assert_eq!(network.dns, Dns::None);
        assert_eq!(network.destinations[0], "network:tracon-int");

        // A network with a way out is no longer node-only.
        let s2 = setup(Fake::default());
        *s2.fake.inspect.lock().unwrap() = Some(inspected(&s2).to_string());
        assert_eq!(
            s2.provider.verify_isolation(&handle).await.unwrap().route,
            Route::Unverified
        );
    }

    #[tokio::test]
    async fn verify_refuses_a_broken_boundary() {
        let s = setup(Fake {
            internal: true,
            ..Fake::default()
        });
        let handle = Handle("tracon-h-1".into());
        let cases: [(&str, serde_json::Value, &str); 6] = [
            (
                "/HostConfig/Privileged",
                serde_json::json!(true),
                "privileged",
            ),
            (
                "/HostConfig/SecurityOpt",
                serde_json::json!(["no-new-privileges", "label=disable"]),
                "SELinux",
            ),
            (
                "/HostConfig/SecurityOpt",
                serde_json::json!([]),
                "no-new-privileges",
            ),
            (
                "/Mounts/0/RW",
                serde_json::json!(true),
                "bundle is mounted writable",
            ),
            ("/Mounts/1/RW", serde_json::json!(true), "socket directory"),
            (
                "/Config/Labels",
                serde_json::json!({}),
                "not made by this provider",
            ),
        ];
        for (pointer, value, expect) in cases {
            let mut doc = inspected(&s);
            *doc[0].pointer_mut(pointer).unwrap() = value;
            *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
            let error = s.provider.verify_isolation(&handle).await.unwrap_err();
            assert!(error.to_string().contains(expect), "{pointer}: {error}");
        }
    }

    #[tokio::test]
    async fn a_gone_sandbox_is_lost_destroy_is_idempotent_and_capacity_counts_ours() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-h-9".into());
        assert_eq!(s.provider.status(&handle).await.unwrap(), Status::Lost);
        s.provider.destroy(&handle).await.unwrap();
        s.provider.destroy(&handle).await.unwrap();
        let mut doc = inspected(&s);
        doc[0]["State"] = serde_json::json!({"Status": "exited", "ExitCode": 3});
        *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
        assert_eq!(
            s.provider.status(&handle).await.unwrap(),
            Status::Exited { code: 3 }
        );
        assert_eq!(
            s.provider.capacity().await.unwrap(),
            Capacity {
                running: 2,
                limit: Some(4)
            }
        );
        let _ = &s.dir;
    }

    #[tokio::test]
    async fn a_sandbox_that_stops_before_connecting_fails_at_once_with_its_logs() {
        let s = setup(Fake::default());
        let identity = tracon_sandbox::tls::NodeIdentity::generate().unwrap();
        let server =
            SandboxServer::new(&identity, tracon_sandbox::node::ServerOptions::default()).unwrap();
        let handle = create(&s, "tracon-h-1", &[]).await.unwrap();
        let mut doc = inspected(&s);
        doc[0]["State"] = serde_json::json!({"Status": "exited", "ExitCode": 64});
        *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
        std::fs::write(
            s.provider.log_file(&handle.0),
            "2026-11-02T14:46:17.5-06:00 stderr F tracon-sandbox: dial: refused\n",
        )
        .unwrap();
        let started = std::time::Instant::now();
        let error = s
            .provider
            .connect(&handle, &server, Duration::from_secs(30))
            .await
            .err()
            .expect("a stopped sandbox does not connect")
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(error.contains("stopped before connecting"), "{error}");
        assert!(error.contains("dial: refused"), "{error}");
    }
}
