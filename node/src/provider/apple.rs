//! Apple Containerization: one lightweight VM per sandbox, made with Apple's
//! `container` CLI on a macOS host.
//!
//! **Route.** A sandbox runs with `--network none` and no resolver, so it has
//! only `lo`. Its one way out is a unix socket published with
//! `--publish-socket`, which `container` relays over vsock from a listener
//! inside the VM to a socket file on the host. The relay runs only that way
//! round, so `tracon-sandbox` listens (`TRACON_SANDBOX_LISTEN`) and
//! [`SandboxProvider::connect`] connects in at [`AppleProvider::node_socket`];
//! TLS, the pin and the token are as on every other provider. A node listening on the host's vmnet
//! address is no alternative: the application firewall or an endpoint
//! agent's network extension may hold an unsigned binary's traffic there,
//! and an `--internal` network exposes every host service listening on all
//! interfaces.
//!
//! **Bundle.** The harness bundle is mounted read-only at `/opt/tracon`, and
//! the sandbox's entrypoint is the `tracon-sandbox` in it, so the image needs
//! nothing of ours.
//!
//! **Token.** The bootstrap reaches the CLI in an env file the node removes
//! as soon as the container starts, keeping it out of every process's argv.
//! `container` keeps the environment in the container's configuration until
//! [`AppleProvider::destroy`]; the token in it is spent once the sandbox has
//! connected.
//!
//! **Snapshots** are `container commit`: an image of the filesystem, not of
//! the processes, which a fork starts afresh with its own bootstrap.
//!
//! Isolation is read back from `container inspect`, never from inside.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tracon_sandbox::node::{SandboxConn, SandboxServer};

use super::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};
use super::{
    Bootstrap, Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    Resources, SandboxProvider, SnapshotId, Status,
};
use crate::runner::Mount;

/// Where the bundle is mounted inside.
pub const BUNDLE_TARGET: &str = "/opt/tracon";
/// The supervisor, inside the bundle.
pub const SANDBOX_BIN: &str = "/opt/tracon/bin/tracon-sandbox";
/// Where `tracon-sandbox` listens for the node, inside.
pub const SOCKET_INSIDE: &str = "/run/tracon/node.sock";
/// Marks what this provider made, for [`AppleProvider::capacity`].
const LABEL: &str = "tracon.provider=apple";
/// The runtime that gives every container a VM of its own.
const RUNTIME_HANDLER: &str = "container-runtime-linux";
/// macOS's `sun_path`, including the terminating NUL.
const SUN_PATH_MAX: usize = 104;

/// How the node runs `container`. A trait so tests can stand in for it.
#[async_trait]
pub trait ContainerCli: Send + Sync {
    /// Run `container` with `args`; stdout, or stderr as the error.
    async fn run(&self, args: &[String]) -> Result<String, CliError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Failed(String),
    #[error("run container: {0}")]
    Spawn(std::io::Error),
}

impl CliError {
    fn not_found(&self) -> bool {
        matches!(self, CliError::Failed(text) if text.contains("not found"))
    }
}

impl From<CliError> for ProviderError {
    fn from(error: CliError) -> Self {
        ProviderError::Failed(error.to_string())
    }
}

/// The real `container`, at a path the path guard accepts.
#[derive(Default)]
pub struct SystemCli {
    pub bin: String,
}

#[async_trait]
impl ContainerCli for SystemCli {
    async fn run(&self, args: &[String]) -> Result<String, CliError> {
        let fallback = [Path::new("/usr/local/bin/container")];
        let bin = match crate::config::path_guard::resolve(
            "provider.apple.container",
            &self.bin,
            &fallback,
        ) {
            Some(resolved) => resolved
                .map_err(|refusal| CliError::Spawn(std::io::Error::other(refusal.to_string())))?,
            None if self.bin.is_empty() => PathBuf::from("container"),
            None => PathBuf::from(&self.bin),
        };
        let out = tokio::process::Command::new(&bin)
            .args(args)
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
pub struct AppleOptions {
    /// The bundle for `linux/arm64`, holding `bin/tracon-sandbox`.
    pub bundle: PathBuf,
    /// Where the published sockets go on the host. Short, because a socket
    /// path longer than 103 bytes cannot be bound, and private to the node.
    pub socket_dir: PathBuf,
    /// Most sandboxes at once; `None` for no limit of the provider's own.
    pub limit: Option<u32>,
}

impl AppleOptions {
    /// A socket directory for this user under `/tmp`, which is short on every
    /// Mac, unlike `$TMPDIR`.
    pub fn default_socket_dir() -> PathBuf {
        PathBuf::from(format!("/tmp/tracon-{}", unsafe { libc::getuid() }))
    }
}

pub struct AppleProvider {
    cli: Box<dyn ContainerCli>,
    options: AppleOptions,
}

impl AppleProvider {
    pub fn new(options: AppleOptions) -> Self {
        Self::with_cli(options, Box::new(SystemCli::default()))
    }

    pub fn with_cli(options: AppleOptions, cli: Box<dyn ContainerCli>) -> Self {
        AppleProvider { cli, options }
    }

    /// Where the node connects to reach the sandbox `handle` names.
    pub fn node_socket(&self, handle: &Handle) -> PathBuf {
        self.options.socket_dir.join(format!("{}.sock", handle.0))
    }

    async fn cli(&self, args: &[&str]) -> Result<String, CliError> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.cli.run(&args).await
    }

    async fn inspect(&self, handle: &Handle) -> Result<Inspected, ProviderError> {
        let text = match self.cli(&["inspect", &handle.0]).await {
            Ok(text) => text,
            Err(error) if error.not_found() => {
                return Err(ProviderError::NotFound(handle.0.clone()))
            }
            Err(error) => return Err(error.into()),
        };
        let mut all: Vec<Inspected> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Failed(format!("container inspect: {e}")))?;
        match all.len() {
            1 => Ok(all.remove(0)),
            0 => Err(ProviderError::NotFound(handle.0.clone())),
            n => Err(ProviderError::Failed(format!(
                "container inspect {} returned {n} containers",
                handle.0
            ))),
        }
    }

    /// A private, short directory for the published sockets.
    fn prepare_socket_dir(&self) -> Result<(), ProviderError> {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        let dir = &self.options.socket_dir;
        let failed = |what: &str, e: std::io::Error| {
            ProviderError::Failed(format!("{what} {}: {e}", dir.display()))
        };
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(failed("create", e)),
        }
        let meta = std::fs::symlink_metadata(dir).map_err(|e| failed("read", e))?;
        // Under /tmp anyone could have made it first.
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            return Err(ProviderError::Failed(format!(
                "{} is not a directory this user owns",
                dir.display()
            )));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| failed("restrict", e))?;
        }
        Ok(())
    }

    /// `container run` for a new sandbox from `image`.
    fn run_args(
        &self,
        name: &str,
        image: &str,
        resources: &Resources,
        mounts: &[Mount],
        env_file: &Path,
    ) -> Result<Vec<String>, ProviderError> {
        let socket = self.node_socket(&Handle(name.into()));
        let mut args: Vec<String> = [
            "run",
            "--detach",
            "--name",
            name,
            "--label",
            LABEL,
            "--network",
            "none",
            "--no-dns",
        ]
        .map(String::from)
        .into();
        args.push("--publish-socket".into());
        args.push(format!("{}:{SOCKET_INSIDE}", socket.display()));
        args.push("--mount".into());
        args.push(bind_mount(&self.options.bundle, BUNDLE_TARGET, true)?);
        for mount in mounts {
            let source = Path::new(&mount.volume);
            if !source.is_absolute() {
                return Err(ProviderError::Failed(format!(
                    "apple mounts host directories only; {:?} is not an absolute path",
                    mount.volume
                )));
            }
            if mount.target == BUNDLE_TARGET || mount.target.starts_with("/opt/tracon/") {
                return Err(ProviderError::Failed(format!(
                    "{} is the bundle's",
                    mount.target
                )));
            }
            let source = match mount.sub_path.is_empty() {
                true => source.to_path_buf(),
                false => source.join(&mount.sub_path),
            };
            args.push("--mount".into());
            args.push(bind_mount(&source, &mount.target, mount.read_only)?);
        }
        if let Some(millis) = resources.cpu_millis {
            args.push("--cpus".into());
            args.push(millis.div_ceil(1000).max(1).to_string());
        }
        if let Some(bytes) = resources.memory_bytes {
            args.push("--memory".into());
            args.push(format!("{}M", bytes.div_ceil(1 << 20).max(1)));
        }
        args.push("--env-file".into());
        args.push(env_file.to_string_lossy().into_owned());
        args.push("--entrypoint".into());
        args.push(SANDBOX_BIN.into());
        args.push(image.into());
        Ok(args)
    }

    async fn start(
        &self,
        image: &str,
        resources: &Resources,
        mounts: &[Mount],
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        self.prepare_socket_dir()?;
        let name = format!(
            "tracon-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let handle = Handle(name.clone());
        let socket = self.node_socket(&handle);
        if socket.as_os_str().len() >= SUN_PATH_MAX {
            return Err(ProviderError::Failed(format!(
                "socket path {} is longer than macOS allows; choose a shorter socket directory",
                socket.display()
            )));
        }
        let env = tempfile::Builder::new()
            .prefix(".env-")
            .tempfile_in(&self.options.socket_dir)
            .map_err(|e| ProviderError::Failed(format!("env file: {e}")))?;
        std::fs::write(env.path(), env_file(bootstrap)?)
            .map_err(|e| ProviderError::Failed(format!("env file: {e}")))?;
        let args = self.run_args(&name, image, resources, mounts, env.path())?;
        let started = self.cli.run(&args).await;
        drop(env);
        match started {
            Ok(_) => Ok(handle),
            Err(error) => {
                // A failed run can still leave the container behind.
                let _ = self.destroy(&handle).await;
                Err(error.into())
            }
        }
    }
}

fn bind_mount(source: &Path, target: &str, read_only: bool) -> Result<String, ProviderError> {
    let source = source.to_string_lossy();
    for part in [source.as_ref(), target] {
        if part.contains(',') || part.contains('=') {
            return Err(ProviderError::Failed(format!(
                "mount path {part:?} contains ',' or '='"
            )));
        }
    }
    let mut spec = format!("type=bind,source={source},target={target}");
    if read_only {
        spec.push_str(",readonly");
    }
    Ok(spec)
}

/// The bootstrap environment, as `--env-file` reads it.
fn env_file(bootstrap: &Bootstrap) -> Result<String, ProviderError> {
    let lines = [
        ("TRACON_SANDBOX_LISTEN", SOCKET_INSIDE),
        ("TRACON_SANDBOX_PIN", bootstrap.cert_pin.as_str()),
        ("TRACON_SANDBOX_TOKEN", bootstrap.token.as_str()),
    ];
    let mut text = String::new();
    for (key, value) in lines {
        if value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err(ProviderError::Failed(format!("{key} must be one line")));
        }
        text.push_str(&format!("{key}={value}\n"));
    }
    Ok(text)
}

/// The parts of `container inspect` this provider reads.
#[derive(Debug, Deserialize)]
struct Inspected {
    configuration: Configuration,
    status: InspectedStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Configuration {
    image: InspectedImage,
    #[serde(default)]
    mounts: Vec<InspectedMount>,
    #[serde(default)]
    networks: Vec<serde_json::Value>,
    #[serde(default)]
    published_ports: Vec<serde_json::Value>,
    #[serde(default)]
    published_sockets: Vec<PublishedSocket>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    ssh: bool,
    #[serde(default)]
    virtualization: bool,
    #[serde(default)]
    runtime_handler: String,
}

#[derive(Debug, Deserialize)]
struct InspectedImage {
    descriptor: Descriptor,
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    digest: String,
}

#[derive(Debug, Deserialize)]
struct InspectedMount {
    destination: String,
    source: String,
    #[serde(default)]
    options: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PublishedSocket {
    container_path: String,
    host_path: String,
}

#[derive(Debug, Deserialize)]
struct InspectedStatus {
    state: String,
}

impl Inspected {
    fn has_network(&self) -> bool {
        !self.configuration.networks.is_empty()
    }

    fn dns(&self) -> Dns {
        // With no interface but `lo`, no resolver is reachable; with one,
        // vmnet's gateway answers any name.
        match self.has_network() {
            false => Dns::None,
            true => Dns::ProviderOpen,
        }
    }

    /// Whether the node's socket is the only way out.
    fn route(&self, node_socket: &Path) -> Route {
        let c = &self.configuration;
        let only_ours = c.published_sockets
            == [PublishedSocket {
                container_path: SOCKET_INSIDE.into(),
                host_path: node_socket.to_string_lossy().into_owned(),
            }];
        match !self.has_network() && c.published_ports.is_empty() && only_ours && !c.ssh {
            true => Route::NodeOnly,
            false => Route::Unverified,
        }
    }

    /// Problems that no capability expresses but break the boundary.
    fn breaches(&self, bundle: &Path) -> Vec<String> {
        let c = &self.configuration;
        let mut found = Vec::new();
        if c.runtime_handler != RUNTIME_HANDLER {
            found.push(format!(
                "runtime handler is {:?}, not {RUNTIME_HANDLER}",
                c.runtime_handler
            ));
        }
        if c.virtualization {
            found.push("nested virtualization is on".into());
        }
        match c.mounts.iter().find(|m| m.destination == BUNDLE_TARGET) {
            None => found.push(format!("the bundle is not mounted at {BUNDLE_TARGET}")),
            Some(m) => {
                if Path::new(&m.source) != bundle {
                    found.push(format!("{BUNDLE_TARGET} is {}, not the bundle", m.source));
                }
                if !m.options.iter().any(|o| o == "ro") {
                    found.push("the bundle is mounted writable".into());
                }
            }
        }
        found
    }
}

#[derive(Debug, Deserialize)]
struct Listed {
    configuration: ListedConfiguration,
    status: InspectedStatus,
}

#[derive(Debug, Deserialize)]
struct ListedConfiguration {
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
}

fn ours(labels: &std::collections::BTreeMap<String, String>) -> bool {
    let (key, value) = LABEL.split_once('=').expect("label is key=value");
    labels.get(key).map(String::as_str) == Some(value)
}

#[async_trait]
impl SandboxProvider for AppleProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Apple
    }

    /// The provider names the VM itself; `spec.name` is not used.
    async fn create(&self, spec: &CreateSpec<'_>) -> Result<Handle, ProviderError> {
        self.start(spec.image, spec.resources, spec.mounts, spec.bootstrap)
            .await
    }

    async fn connect(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        within: Duration,
    ) -> Result<SandboxConn, ProviderError> {
        server
            .connect_to(&self.node_socket(handle), within)
            .await
            .map_err(|e| ProviderError::Failed(format!("connect to {}: {e}", handle.0)))
    }

    async fn destroy(&self, handle: &Handle) -> Result<(), ProviderError> {
        match self.cli(&["delete", "--force", &handle.0]).await {
            Ok(_) => {}
            Err(error) if error.not_found() => {}
            Err(error) => return Err(error.into()),
        }
        match std::fs::remove_file(self.node_socket(handle)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(ProviderError::Failed(
                format!("remove {}: {e}", self.node_socket(handle).display()),
            )),
            _ => Ok(()),
        }
    }

    async fn status(&self, handle: &Handle) -> Result<Status, ProviderError> {
        let inspected = match self.inspect(handle).await {
            Err(ProviderError::NotFound(_)) => return Ok(Status::Lost),
            other => other?,
        };
        Ok(match inspected.status.state.as_str() {
            "running" | "stopping" => Status::Running,
            // `container` keeps no exit code; the node has it from the
            // sandbox's connection when there was one.
            "stopped" => Status::Exited { code: -1 },
            _ => Status::Lost,
        })
    }

    async fn snapshot(&self, handle: &Handle) -> Result<SnapshotId, ProviderError> {
        let reference = format!(
            "tracon-snapshot/{}:{}",
            handle.0,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        );
        self.cli(&["commit", &handle.0, &reference]).await?;
        Ok(SnapshotId(reference))
    }

    /// A new sandbox from the snapshot's filesystem: nothing of the
    /// snapshot's processes survives, and the bundle is mounted afresh.
    async fn fork(
        &self,
        snapshot: &SnapshotId,
        resources: &Resources,
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        self.start(&snapshot.0, resources, &[], bootstrap).await
    }

    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError> {
        Ok(ImageIdentity::Exact {
            digest: self
                .inspect(handle)
                .await?
                .configuration
                .image
                .descriptor
                .digest,
        })
    }

    async fn network(&self, handle: &Handle) -> Result<Network, ProviderError> {
        let inspected = self.inspect(handle).await?;
        let c = &inspected.configuration;
        let mut destinations: Vec<String> = c
            .published_sockets
            .iter()
            .map(|s| format!("unix:{}", s.host_path))
            .collect();
        destinations.extend(c.networks.iter().map(|n| match n.get("network") {
            Some(serde_json::Value::String(name)) => format!("network:{name}"),
            _ => format!("network:{n}"),
        }));
        destinations.extend(c.published_ports.iter().map(|p| format!("port:{p}")));
        if c.ssh {
            destinations.push("ssh-agent".into());
        }
        Ok(Network {
            destinations,
            dns: inspected.dns(),
        })
    }

    async fn verify_isolation(&self, handle: &Handle) -> Result<Capabilities, ProviderError> {
        let inspected = self.inspect(handle).await?;
        if !ours(&inspected.configuration.labels) {
            return Err(ProviderError::Failed(format!(
                "{} was not made by this provider",
                handle.0
            )));
        }
        let breaches = inspected.breaches(&self.options.bundle);
        if !breaches.is_empty() {
            return Err(ProviderError::Failed(breaches.join("; ")));
        }
        Ok(Capabilities {
            route: inspected.route(&self.node_socket(handle)),
            dns: inspected.dns(),
            ..self.capabilities()
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tier: Tier::Microvm,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::RootInVm,
            // The container API server is reached over XPC on the host, and
            // virtiofs relays no sockets into the VM.
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        }
    }

    async fn capacity(&self) -> Result<Capacity, ProviderError> {
        let text = self.cli(&["list", "--format", "json"]).await?;
        let listed: Vec<Listed> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Failed(format!("container list: {e}")))?;
        let running = listed
            .iter()
            .filter(|c| ours(&c.configuration.labels) && c.status.state == "running")
            .count();
        Ok(Capacity {
            running: running as u32,
            limit: self.options.limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Answers `container` from a script and records every call.
    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        inspect: Arc<Mutex<Option<String>>>,
        env_seen: Arc<Mutex<Option<String>>>,
        fail_run: bool,
    }

    #[async_trait]
    impl ContainerCli for Fake {
        async fn run(&self, args: &[String]) -> Result<String, CliError> {
            self.calls.lock().unwrap().push(args.to_vec());
            match args[0].as_str() {
                "run" => {
                    let at = args.iter().position(|a| a == "--env-file").unwrap();
                    *self.env_seen.lock().unwrap() =
                        Some(std::fs::read_to_string(&args[at + 1]).unwrap());
                    match self.fail_run {
                        true => Err(CliError::Failed("no such image".into())),
                        false => Ok(format!("{}\n", args[3])),
                    }
                }
                "inspect" => match self.inspect.lock().unwrap().clone() {
                    Some(text) => Ok(text),
                    None => Err(CliError::Failed(format!(
                        "Error: container not found: {}",
                        args[1]
                    ))),
                },
                "delete" => Err(CliError::Failed("Error: container not found: x".into())),
                "commit" => Ok(String::new()),
                "list" => Ok(serde_json::json!([
                    {"configuration": {"labels": {"tracon.provider": "apple"}}, "status": {"state": "running"}},
                    {"configuration": {"labels": {"tracon.provider": "apple"}}, "status": {"state": "stopped"}},
                    {"configuration": {"labels": {}}, "status": {"state": "running"}},
                ])
                .to_string()),
                other => panic!("unexpected container {other}"),
            }
        }
    }

    struct Setup {
        provider: AppleProvider,
        fake: Fake,
        _dir: tempfile::TempDir,
        bundle: PathBuf,
    }

    fn setup(fake: Fake) -> Setup {
        let dir = tempfile::Builder::new()
            .prefix("ap")
            .tempdir_in("/tmp")
            .unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        let provider = AppleProvider::with_cli(
            AppleOptions {
                bundle: bundle.clone(),
                socket_dir: dir.path().join("s"),
                limit: Some(4),
            },
            Box::new(fake.clone()),
        );
        Setup {
            provider,
            fake,
            _dir: dir,
            bundle,
        }
    }

    impl AppleProvider {
        async fn create_with(
            &self,
            image: &str,
            resources: &Resources,
            mounts: &[Mount],
            bootstrap: &Bootstrap,
        ) -> Result<Handle, ProviderError> {
            self.create(&CreateSpec {
                name: None,
                image,
                resources,
                mounts,
                bootstrap,
            })
            .await
        }
    }

    fn bootstrap() -> Bootstrap {
        Bootstrap {
            run: "run-1".into(),
            cert_pin: "ab".repeat(32),
            token: "run-secret".into(),
        }
    }

    /// `container inspect` of a sandbox as `create` makes it, as 1.6.1
    /// prints it.
    fn inspected(s: &Setup, handle: &Handle) -> serde_json::Value {
        serde_json::json!([{
            "configuration": {
                "id": handle.0,
                "image": {"descriptor": {"digest": "sha256:fe81", "size": 6688}, "reference": "docker.io/library/ubuntu:24.04"},
                "labels": {"tracon.provider": "apple"},
                "mounts": [{"destination": "/opt/tracon", "options": ["ro"], "source": s.bundle, "type": {"virtiofs": {}}}],
                "networks": [],
                "publishedPorts": [],
                "publishedSockets": [{"containerPath": SOCKET_INSIDE, "hostPath": s.provider.node_socket(handle)}],
                "runtimeHandler": "container-runtime-linux",
                "ssh": false,
                "virtualization": false,
            },
            "id": handle.0,
            "status": {"networks": [], "state": "running"}
        }])
    }

    #[tokio::test]
    async fn create_runs_a_vm_with_no_network_and_the_bundle_read_only() {
        let s = setup(Fake::default());
        let work = s._dir.path().join("work");
        let handle = s
            .provider
            .create_with(
                "ubuntu:24.04",
                &Resources {
                    cpu_millis: Some(1500),
                    memory_bytes: Some(3 << 29),
                    disk_bytes: None,
                },
                &[Mount::at(work.to_string_lossy(), "repo", "/work", false)],
                &bootstrap(),
            )
            .await
            .unwrap();
        let calls = s.fake.calls.lock().unwrap().clone();
        let run = &calls[0];
        let joined = run.join(" ");
        assert!(handle.0.starts_with("tracon-"), "{handle:?}");
        for expected in [
            "--network none",
            "--no-dns",
            &format!("--label {LABEL}"),
            &format!(
                "--publish-socket {}:{SOCKET_INSIDE}",
                s.provider.node_socket(&handle).display()
            ),
            &format!(
                "--mount type=bind,source={},target=/opt/tracon,readonly",
                s.bundle.display()
            ),
            &format!(
                "--mount type=bind,source={}/repo,target=/work --cpus",
                work.display()
            ),
            "--cpus 2 --memory 1536M",
            &format!("--entrypoint {SANDBOX_BIN} ubuntu:24.04"),
        ] {
            assert!(joined.contains(expected), "{expected:?} not in {joined}");
        }
        assert!(
            !joined.contains("run-secret"),
            "the token stays out of argv"
        );
        let env = s.fake.env_seen.lock().unwrap().clone().unwrap();
        assert_eq!(
            env,
            format!(
                "TRACON_SANDBOX_LISTEN={SOCKET_INSIDE}\nTRACON_SANDBOX_PIN={}\nTRACON_SANDBOX_TOKEN=run-secret\n",
                "ab".repeat(32)
            )
        );
        let env_path = &run[run.iter().position(|a| a == "--env-file").unwrap() + 1];
        assert!(!Path::new(env_path).exists(), "the env file is removed");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&s.provider.options.socket_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[tokio::test]
    async fn create_refuses_what_it_cannot_mount() {
        let s = setup(Fake::default());
        for mount in [
            Mount::volume("named-volume", "/work", false),
            Mount::volume("/tmp/x", "/opt/tracon/bin", false),
            Mount::volume("/tmp/a,b", "/work", false),
        ] {
            assert!(
                s.provider
                    .create_with(
                        "img",
                        &Resources::default(),
                        std::slice::from_ref(&mount),
                        &bootstrap()
                    )
                    .await
                    .is_err(),
                "{mount:?}"
            );
        }
        assert!(s.fake.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_run_is_cleaned_up() {
        let s = setup(Fake {
            fail_run: true,
            ..Fake::default()
        });
        let error = s
            .provider
            .create_with("nope", &Resources::default(), &[], &bootstrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no such image"), "{error}");
        let calls = s.fake.calls.lock().unwrap().clone();
        assert_eq!(calls[1][..2], ["delete".to_string(), "--force".into()]);
    }

    #[tokio::test]
    async fn a_socket_path_too_long_for_macos_is_refused() {
        let mut s = setup(Fake::default());
        s.provider.options.socket_dir = s._dir.path().join("d".repeat(90));
        let error = s
            .provider
            .create_with("img", &Resources::default(), &[], &bootstrap())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("longer than macOS allows"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn verify_reads_back_a_node_only_microvm() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-abc".into());
        *s.fake.inspect.lock().unwrap() = Some(inspected(&s, &handle).to_string());
        let caps = s.provider.verify_isolation(&handle).await.unwrap();
        assert_eq!(caps, s.provider.capabilities());
        assert_eq!(caps.tier, Tier::Microvm);
        assert_eq!(caps.route, Route::NodeOnly);
        assert_eq!(caps.dns, Dns::None);
        let network = s.provider.network(&handle).await.unwrap();
        assert_eq!(
            network.destinations,
            [format!(
                "unix:{}",
                s.provider.node_socket(&handle).display()
            )]
        );
        assert_eq!(
            s.provider.image_identity(&handle).await.unwrap(),
            ImageIdentity::Exact {
                digest: "sha256:fe81".into()
            }
        );
        assert_eq!(s.provider.status(&handle).await.unwrap(), Status::Running);
    }

    #[tokio::test]
    async fn verify_reports_any_other_way_out() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-abc".into());
        let cases: [(&str, serde_json::Value); 4] = [
            ("networks", serde_json::json!([{"network": "default"}])),
            ("publishedPorts", serde_json::json!([{"hostPort": 8080}])),
            (
                "publishedSockets",
                serde_json::json!([
                    {"containerPath": SOCKET_INSIDE, "hostPath": s.provider.node_socket(&handle)},
                    {"containerPath": "/run/other.sock", "hostPath": "/tmp/other.sock"}
                ]),
            ),
            ("ssh", serde_json::json!(true)),
        ];
        for (field, value) in cases {
            let mut doc = inspected(&s, &handle);
            doc[0]["configuration"][field] = value;
            *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
            let caps = s.provider.verify_isolation(&handle).await.unwrap();
            assert_eq!(caps.route, Route::Unverified, "{field}");
            if field == "networks" {
                assert_eq!(caps.dns, Dns::ProviderOpen);
                let network = s.provider.network(&handle).await.unwrap();
                assert!(network
                    .destinations
                    .contains(&"network:default".to_string()));
            }
        }
    }

    #[tokio::test]
    async fn verify_refuses_a_broken_boundary() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-abc".into());
        let cases: [(&str, serde_json::Value, &str); 5] = [
            (
                "mounts",
                serde_json::json!([{"destination": "/opt/tracon", "options": [], "source": s.bundle}]),
                "writable",
            ),
            ("mounts", serde_json::json!([]), "not mounted"),
            ("virtualization", serde_json::json!(true), "nested"),
            (
                "runtimeHandler",
                serde_json::json!("other"),
                "runtime handler",
            ),
            ("labels", serde_json::json!({}), "not made by this provider"),
        ];
        for (field, value, expect) in cases {
            let mut doc = inspected(&s, &handle);
            doc[0]["configuration"][field] = value;
            *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
            let error = s.provider.verify_isolation(&handle).await.unwrap_err();
            assert!(error.to_string().contains(expect), "{field}: {error}");
        }
    }

    #[tokio::test]
    async fn a_gone_sandbox_is_lost_and_destroying_it_again_is_fine() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-gone".into());
        assert_eq!(s.provider.status(&handle).await.unwrap(), Status::Lost);
        assert!(matches!(
            s.provider.verify_isolation(&handle).await,
            Err(ProviderError::NotFound(_))
        ));
        s.provider.destroy(&handle).await.unwrap();
        s.provider.destroy(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn stopped_is_exited_and_capacity_counts_only_running_sandboxes_of_ours() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-abc".into());
        let mut doc = inspected(&s, &handle);
        doc[0]["status"]["state"] = "stopped".into();
        *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
        assert_eq!(
            s.provider.status(&handle).await.unwrap(),
            Status::Exited { code: -1 }
        );
        assert_eq!(
            s.provider.capacity().await.unwrap(),
            Capacity {
                running: 1,
                limit: Some(4)
            }
        );
    }

    #[tokio::test]
    async fn a_fork_starts_the_snapshot_with_its_own_bootstrap() {
        let s = setup(Fake::default());
        let parent = Handle("tracon-parent".into());
        let snapshot = s.provider.snapshot(&parent).await.unwrap();
        assert!(snapshot.0.starts_with("tracon-snapshot/tracon-parent:"));
        let child = s
            .provider
            .fork(&snapshot, &Resources::default(), &bootstrap())
            .await
            .unwrap();
        assert_ne!(child, parent);
        let calls = s.fake.calls.lock().unwrap().clone();
        assert_eq!(calls[0], ["commit", "tracon-parent", snapshot.0.as_str()]);
        let run = calls[1].join(" ");
        assert!(
            run.ends_with(&format!("--entrypoint {SANDBOX_BIN} {}", snapshot.0)),
            "{run}"
        );
        assert!(run.contains("target=/opt/tracon,readonly"), "{run}");
    }
}
