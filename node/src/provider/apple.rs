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
//! the processes, which a fork starts afresh with its own bootstrap. A
//! commit keeps the run's environment and takes no changes, so a prepared
//! snapshot ([`SandboxProvider::snapshot_as`]) is the commit built over once
//! more with a configuration of its own; the layers are shared.
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
/// Where `tracon-sandbox` listens for the node, inside: somewhere the
/// image's own user may bind, which `/run` is not for the base's 65532.
pub const SOCKET_INSIDE: &str = "/tmp/tracon-node.sock";
/// Marks what this provider made, for [`AppleProvider::capacity`].
const LABEL: &str = "tracon.provider=apple";
/// The runtime that gives every container a VM of its own.
const RUNTIME_HANDLER: &str = "container-runtime-linux";
/// How long configuring a commit as a snapshot may take: a build over one
/// image, which starts the builder if it is not running.
const SNAPSHOT_BUILD_WITHIN: Duration = Duration::from_secs(600);
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
            "runtime.apple.container",
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
#[derive(Debug, Clone, Default)]
pub struct AppleOptions {
    /// The `container` CLI; empty for where Apple's installer puts it.
    pub bin: String,
    /// The bundle for `linux/arm64`, holding `bin/tracon-sandbox`.
    pub bundle: PathBuf,
    /// Where the published sockets go on the host. Short, because a socket
    /// path longer than 103 bytes cannot be bound, and private to the node.
    pub socket_dir: PathBuf,
    /// Most sandboxes at once; `None` for no limit of the provider's own.
    pub limit: Option<u32>,
    /// Every sandbox's environment besides its bootstrap: its home, its
    /// proxies, where its harness keeps state. Never a secret: `container`
    /// keeps it in the sandbox's configuration.
    pub env: Vec<(String, String)>,
    /// What a sandbox gets when its spec says nothing: CPUs, and memory as
    /// `container` takes it (`4G`).
    pub cpus: Option<u32>,
    pub memory: String,
    /// Each catalogue service's bundle and where it is mounted, read-only,
    /// for services run under the sandbox's supervisor.
    pub service_bundles: Vec<(PathBuf, String)>,
    /// Who the supervisor, and so every command, runs as, when not the
    /// image's own user: root for a repository's build steps.
    pub user: Option<String>,
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
        let cli = SystemCli {
            bin: options.bin.clone(),
        };
        Self::with_cli(options, Box::new(cli))
    }

    pub fn options(&self) -> &AppleOptions {
        &self.options
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

    /// An image's digest and its `linux/arm64` configuration (`Env`, `User`,
    /// `Entrypoint` and the rest, as OCI has them).
    async fn image_config(
        &self,
        image: &str,
    ) -> Result<(String, serde_json::Value), ProviderError> {
        let text = match self.cli(&["image", "inspect", image]).await {
            Ok(text) => text,
            Err(error) if error.not_found() => {
                return Err(ProviderError::NotFound(image.to_string()))
            }
            Err(error) => return Err(error.into()),
        };
        let all: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Failed(format!("container image inspect: {e}")))?;
        let first = all
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::NotFound(image.to_string()))?;
        let digest = first["configuration"]["descriptor"]["digest"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !digest.starts_with("sha256:") {
            return Err(ProviderError::Failed(format!("{image} has no digest")));
        }
        let config = first["variants"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| v["platform"]["os"] == "linux" && v["platform"]["architecture"] == "arm64")
            .map(|v| v["config"]["config"].clone())
            .unwrap_or_default();
        Ok((digest, config))
    }

    /// `image` named by its digest as well, which is the only way
    /// `container` runs an image it built by digest: any reference its store
    /// does not hold is asked of the registry. The pinned reference.
    async fn pin(&self, image: &str, digest: &str) -> Result<String, ProviderError> {
        let pinned = format!("{}@{digest}", crate::boundary::apple::image_name(image));
        self.cli(&["image", "tag", image, &pinned]).await?;
        Ok(pinned)
    }

    /// The commit `committed` built over as `reference`: configured as
    /// `parent` was, with `labels`, and every variable the run added
    /// emptied. `created` collects what it named on the way.
    async fn configure(
        &self,
        committed: &str,
        parent: &serde_json::Value,
        reference: &str,
        labels: &[(String, String)],
        created: &mut Vec<String>,
    ) -> Result<String, ProviderError> {
        let (digest, config) = self.image_config(committed).await?;
        let from = self.pin(committed, &digest).await?;
        created.push(from.clone());
        let mut containerfile = format!("FROM {from}\n");
        for change in super::podman::commit_changes(parent, &config["Env"], labels) {
            containerfile.push_str(&change);
            containerfile.push('\n');
        }
        let context = tempfile::tempdir()
            .map_err(|e| ProviderError::Failed(format!("snapshot context: {e}")))?;
        let file = context.path().join("Containerfile");
        std::fs::write(&file, containerfile)
            .map_err(|e| ProviderError::Failed(format!("snapshot context: {e}")))?;
        let (file, dir) = (file.to_string_lossy(), context.path().to_string_lossy());
        // `-q` never returns in container 1.6.
        let args = [
            "build",
            "--progress",
            "plain",
            "-f",
            &file,
            "-t",
            reference,
            &dir,
        ];
        let build = self.cli(&args);
        match tokio::time::timeout(SNAPSHOT_BUILD_WITHIN, build).await {
            Ok(built) => {
                built.map_err(|e| ProviderError::Failed(format!("build {reference}: {e}")))?
            }
            Err(_) => {
                return Err(ProviderError::Failed(format!(
                    "build {reference}: no result within {}s",
                    SNAPSHOT_BUILD_WITHIN.as_secs()
                )))
            }
        };
        let (digest, _) = self.image_config(reference).await?;
        self.pin(reference, &digest).await
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
        for (source, target) in &self.options.service_bundles {
            args.push("--mount".into());
            args.push(bind_mount(source, target, true)?);
        }
        for mount in mounts {
            if mount.target == BUNDLE_TARGET || mount.target.starts_with("/opt/tracon/") {
                return Err(ProviderError::Failed(format!(
                    "{} is the bundle's",
                    mount.target
                )));
            }
            // A tree the files channel fills, or a volume no sandbox is
            // handed (the harness state every session once shared): the
            // VM's own disk, empty, as a runtime's fresh volume would be.
            let source = Path::new(&mount.volume);
            if mount.is_local() || !source.is_absolute() {
                continue;
            }
            let source = match mount.sub_path.is_empty() {
                true => source.to_path_buf(),
                false => source.join(&mount.sub_path),
            };
            args.push("--mount".into());
            args.push(bind_mount(&source, &mount.target, mount.read_only)?);
        }
        match (resources.cpu_millis, self.options.cpus) {
            (Some(millis), _) => {
                args.push("--cpus".into());
                args.push(millis.div_ceil(1000).max(1).to_string());
            }
            (None, Some(cpus)) => {
                args.push("--cpus".into());
                args.push(cpus.to_string());
            }
            (None, None) => {}
        }
        match (resources.memory_bytes, self.options.memory.as_str()) {
            (Some(bytes), _) => {
                args.push("--memory".into());
                args.push(format!("{}M", bytes.div_ceil(1 << 20).max(1)));
            }
            (None, "") => {}
            (None, memory) => {
                args.push("--memory".into());
                args.push(memory.into());
            }
        }
        args.push("--env-file".into());
        args.push(env_file.to_string_lossy().into_owned());
        if let Some(user) = &self.options.user {
            args.push("--user".into());
            args.push(user.clone());
        }
        args.push("--entrypoint".into());
        args.push(SANDBOX_BIN.into());
        args.push(image.into());
        Ok(args)
    }

    /// The name a sandbox is made under: the node's, where `container` and
    /// the socket directory can take it, so a node that restarts finds what
    /// it left by name; else one of the provider's own.
    fn name_for(&self, asked: Option<&str>) -> String {
        let fits = |name: &str| {
            let socket = self.options.socket_dir.join(format!("{name}.sock"));
            !name.is_empty()
                && name.len() <= 63
                && name.as_bytes()[0].is_ascii_alphanumeric()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && socket.as_os_str().len() < SUN_PATH_MAX
        };
        match asked.filter(|name| fits(name)) {
            Some(name) => name.to_string(),
            None => format!(
                "tracon-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ),
        }
    }

    async fn start(
        &self,
        name: Option<&str>,
        image: &str,
        resources: &Resources,
        mounts: &[Mount],
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        self.prepare_socket_dir()?;
        // `container` asks a registry for any image its store lacks, and
        // for a localhost/ one that is nowhere: tens of seconds, then a
        // refused connection.
        if crate::boundary::podman::local_only(image) {
            match self.cli(&["image", "inspect", image]).await {
                Ok(_) => {}
                Err(error) if error.not_found() => {
                    return Err(ProviderError::Failed(format!(
                        "{image} is no longer in container's image store: a rebuild of its \
                         tag replaced it, or it was removed. No registry holds a localhost/ \
                         image, so it is not pulled; start again to use the image that \
                         stands now"
                    )))
                }
                Err(error) => return Err(error.into()),
            }
        }
        let name = self.name_for(name);
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
        std::fs::write(env.path(), env_file(&self.options.env, bootstrap)?)
            .map_err(|e| ProviderError::Failed(format!("env file: {e}")))?;
        let args = self.run_args(&name, image, resources, mounts, env.path())?;
        // One the node left under this name (a run it lost on a restart) is
        // over: the name is this run's now.
        self.destroy(&handle).await?;
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

/// The sandbox's environment and then its bootstrap, as `--env-file` reads
/// it. A bootstrap name in `env` is refused rather than overridden.
fn env_file(env: &[(String, String)], bootstrap: &Bootstrap) -> Result<String, ProviderError> {
    let lines = [
        ("TRACON_SANDBOX_LISTEN", SOCKET_INSIDE),
        ("TRACON_SANDBOX_PIN", bootstrap.cert_pin.as_str()),
        ("TRACON_SANDBOX_TOKEN", bootstrap.token.as_str()),
    ];
    let mut text = String::new();
    for (key, value) in env {
        let named = !key.is_empty()
            && !key.starts_with("TRACON_SANDBOX_")
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !named || value.contains(['\n', '\r', '\0']) {
            return Err(ProviderError::Failed(format!(
                "{key:?} cannot go in a sandbox's environment"
            )));
        }
        text.push_str(&format!("{key}={value}\n"));
    }
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
    /// What the sandbox was made from, as it was named.
    #[serde(default)]
    reference: String,
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

    async fn create(&self, spec: &CreateSpec<'_>) -> Result<Handle, ProviderError> {
        self.start(
            spec.name,
            spec.image,
            spec.resources,
            spec.mounts,
            spec.bootstrap,
        )
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

    /// The commit, built over with the configuration of the image the
    /// sandbox was made from and `labels`; the snapshot is named by its
    /// digest as well, so it runs by the reference the node records.
    async fn snapshot_as(
        &self,
        handle: &Handle,
        reference: &str,
        labels: &[(String, String)],
    ) -> Result<SnapshotId, ProviderError> {
        let parent = self.inspect(handle).await?.configuration.image.reference;
        let (_, parent) = self.image_config(&parent).await?;
        // In a repository of its own, so neither its tag nor its digest
        // name is ever taken for a snapshot.
        let name = crate::boundary::apple::image_name(reference);
        let tag = reference
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(':'))
            .unwrap_or("latest");
        let committed = format!("{name}-commit:{tag}");
        self.cli(&["commit", &handle.0, &committed])
            .await
            .map_err(|e| ProviderError::Failed(format!("commit {}: {e}", handle.0)))?;
        let mut created = vec![committed.clone()];
        let configured = self
            .configure(&committed, &parent, reference, labels, &mut created)
            .await;
        // The commit's own names; its layers are the snapshot's.
        for image in created.iter().rev() {
            if let Err(error) = self.cli(&["image", "delete", image]).await {
                tracing::warn!(%image, %error, "could not remove a snapshot's commit");
            }
        }
        configured.map(SnapshotId)
    }

    /// A new sandbox from the snapshot's filesystem: nothing of the
    /// snapshot's processes survives, and the bundle is mounted afresh.
    async fn fork(
        &self,
        snapshot: &SnapshotId,
        resources: &Resources,
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        self.start(None, &snapshot.0, resources, &[], bootstrap)
            .await
    }

    /// Exact, or derived from another by a snapshot (its label names which).
    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError> {
        let image = self.inspect(handle).await?.configuration.image;
        let digest = image.descriptor.digest;
        let from = match image.reference.is_empty() {
            true => None,
            false => self
                .image_config(&image.reference)
                .await
                .ok()
                .and_then(|(_, c)| {
                    c["Labels"][crate::sandbox_images::FROM_LABEL]
                        .as_str()
                        .map(str::to_string)
                }),
        };
        Ok(match from {
            Some(from) => ImageIdentity::Derived {
                from,
                actual: digest,
            },
            None => ImageIdentity::Exact { digest },
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
        /// The Containerfile each build was given.
        built: Arc<Mutex<Vec<String>>>,
        /// Images the store does not hold.
        missing: Arc<Mutex<Vec<String>>>,
    }

    /// `container image inspect` of `image`: the parent as the base has it,
    /// a commit with the run's environment in it, and a snapshot labelled
    /// with what it came from.
    fn image_json(image: &str) -> String {
        let (digest, env, labels) = match image {
            i if i.contains("-commit:") || i.contains("-commit@") => (
                "sha256:c0",
                serde_json::json!([
                    "PATH=/usr/bin",
                    "HOME=/home/harness",
                    "TRACON_SANDBOX_TOKEN=t"
                ]),
                serde_json::json!({}),
            ),
            i if i.starts_with("localhost/snap") => (
                "sha256:5a",
                serde_json::json!([
                    "PATH=/usr/bin",
                    "HOME=/home/harness",
                    "TRACON_SANDBOX_TOKEN="
                ]),
                serde_json::json!({(crate::sandbox_images::FROM_LABEL): "sha256:fe81"}),
            ),
            _ => (
                "sha256:fe81",
                serde_json::json!(["PATH=/usr/bin", "HOME=/home/harness"]),
                serde_json::json!({}),
            ),
        };
        serde_json::json!([{
            "configuration": {"descriptor": {"digest": digest}, "name": image},
            "variants": [
                {"platform": {"os": "linux", "architecture": "amd64"}, "config": {"config": {}}},
                {"platform": {"os": "linux", "architecture": "arm64"}, "config": {"config": {
                    "Env": env, "Labels": labels, "User": "65532:65532", "WorkingDir": "/work"
                }}}
            ]
        }])
        .to_string()
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
                "image" => match args[1].as_str() {
                    "inspect" if self.missing.lock().unwrap().contains(&args[2]) => Err(
                        CliError::Failed(format!("Error: image not found: {}", args[2])),
                    ),
                    "inspect" => Ok(image_json(&args[2])),
                    "tag" | "delete" => Ok(String::new()),
                    other => panic!("unexpected container image {other}"),
                },
                "build" => {
                    let at = args.iter().position(|a| a == "-f").unwrap();
                    self.built
                        .lock()
                        .unwrap()
                        .push(std::fs::read_to_string(&args[at + 1]).unwrap());
                    Ok("#4 DONE 0.1s\n".into())
                }
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
                ..AppleOptions::default()
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
                labels: &crate::runner::RunLabels::default(),
                shape: crate::provider::Shape::Session,
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
    async fn a_missing_local_image_is_refused_without_asking_a_registry() {
        let gone = "localhost/tracon-repo-gone@sha256:0a";
        let fake = Fake::default();
        fake.missing.lock().unwrap().push(gone.into());
        let s = setup(fake);
        let error = s
            .provider
            .create_with(gone, &Resources::default(), &[], &bootstrap())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no longer in container's image store"),
            "{error}"
        );
        assert!(!s.fake.calls.lock().unwrap().iter().any(|c| c[0] == "run"));
    }

    #[tokio::test]
    async fn only_a_provider_given_a_user_runs_its_sandboxes_as_that_user() {
        let s = setup(Fake::default());
        s.provider
            .create_with("ubuntu:24.04", &Resources::default(), &[], &bootstrap())
            .await
            .unwrap();
        let mut options = s.provider.options().clone();
        options.user = Some("0:0".into());
        let root = AppleProvider::with_cli(options, Box::new(s.fake.clone()));
        root.create_with("ubuntu:24.04", &Resources::default(), &[], &bootstrap())
            .await
            .unwrap();
        let runs: Vec<Vec<String>> = s
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == "run")
            .cloned()
            .collect();
        assert!(!runs[0].contains(&"--user".to_string()), "{:?}", runs[0]);
        let at = runs[1].iter().position(|a| a == "--user").unwrap();
        assert_eq!(runs[1][at + 1], "0:0");
        assert_eq!(runs[1].last().unwrap(), "ubuntu:24.04");
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
        // Anything left under the name goes first.
        assert_eq!(calls[0], ["delete", "--force", handle.0.as_str()]);
        let run = &calls[1];
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

    /// The node's name where it fits, so a restarted node finds it; the
    /// sandbox's environment, then its bootstrap; the configured size where
    /// the spec names none.
    #[tokio::test]
    async fn create_takes_the_nodes_name_environment_and_size() {
        let mut s = setup(Fake::default());
        s.provider.options.env = vec![
            ("HOME".into(), "/home/harness".into()),
            ("HTTPS_PROXY".into(), "http://127.0.0.1:8888".into()),
        ];
        s.provider.options.cpus = Some(3);
        s.provider.options.memory = "6G".into();
        s.provider.options.service_bundles = vec![(
            PathBuf::from("/srv/browser"),
            "/opt/services/browser".into(),
        )];
        let (resources, boot, labels) = (
            Resources::default(),
            bootstrap(),
            crate::runner::RunLabels::default(),
        );
        let spec = |name| CreateSpec {
            name,
            image: "img",
            resources: &resources,
            mounts: &[],
            bootstrap: &boot,
            labels: &labels,
            shape: crate::provider::Shape::Session,
        };
        let handle = s
            .provider
            .create(&spec(Some("tracon-h-abc")))
            .await
            .unwrap();
        assert_eq!(handle.0, "tracon-h-abc");
        let run = s.fake.calls.lock().unwrap()[1].join(" ");
        assert!(run.contains("--name tracon-h-abc"), "{run}");
        assert!(run.contains("--cpus 3 --memory 6G"), "{run}");
        assert!(
            run.contains("type=bind,source=/srv/browser,target=/opt/services/browser,readonly"),
            "{run}"
        );
        let env = s.fake.env_seen.lock().unwrap().clone().unwrap();
        assert!(
            env.starts_with(
                "HOME=/home/harness\nHTTPS_PROXY=http://127.0.0.1:8888\nTRACON_SANDBOX_LISTEN="
            ),
            "{env}"
        );
        let long = "x".repeat(80);
        for refused in ["bad name", "", long.as_str()] {
            let handle = s.provider.create(&spec(Some(refused))).await.unwrap();
            assert!(handle.0.starts_with("tracon-") && handle.0 != refused);
        }
        s.provider.options.env = vec![("TRACON_SANDBOX_TOKEN".into(), "mine".into())];
        assert!(s.provider.create(&spec(None)).await.is_err());
    }

    #[tokio::test]
    async fn create_refuses_what_it_cannot_mount() {
        let s = setup(Fake::default());
        for mount in [
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

    /// What the files channel fills and what no sandbox is handed are the
    /// VM's own disk: nothing of the node's is mounted there.
    #[tokio::test]
    async fn local_and_withheld_volumes_are_the_vms_own_disk() {
        let s = setup(Fake::default());
        s.provider
            .create_with(
                "img",
                &Resources::default(),
                &[
                    Mount::local("/work"),
                    Mount::volume("tracon-harness-state", "/home/harness/.claude", false),
                ],
                &bootstrap(),
            )
            .await
            .unwrap();
        let run = s.fake.calls.lock().unwrap()[1].join(" ");
        assert!(!run.contains("target=/work"), "{run}");
        assert!(!run.contains("harness-state"), "{run}");
        assert_eq!(run.matches("--mount").count(), 1, "only the bundle: {run}");
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
        assert_eq!(calls[1][0], "run");
        assert_eq!(calls[2][..2], ["delete".to_string(), "--force".into()]);
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

    /// A prepared snapshot is the commit built over: the run's token emptied,
    /// the base's entrypoint and user back, labelled, and named by digest;
    /// the commit's own names are gone.
    #[tokio::test]
    async fn a_prepared_snapshot_is_configured_as_its_parent_and_pinned() {
        let s = setup(Fake::default());
        let handle = Handle("tracon-w-1".into());
        *s.fake.inspect.lock().unwrap() = Some(inspected(&s, &handle).to_string());
        let labels = [(
            crate::sandbox_images::KEY_LABEL.to_string(),
            "k1".to_string(),
        )];
        let snapshot = s
            .provider
            .snapshot_as(&handle, "localhost/snap-app:k1", &labels)
            .await
            .unwrap();
        assert_eq!(snapshot.0, "localhost/snap-app@sha256:5a");
        let built = s.fake.built.lock().unwrap().clone();
        assert_eq!(
            built,
            ["FROM localhost/snap-app-commit@sha256:c0\n\
              ENTRYPOINT []\nCMD []\nUSER 65532:65532\nWORKDIR /work\n\
              ENV TRACON_SANDBOX_TOKEN=\n\
              LABEL io.tracon.snapshot.key=\"k1\"\n"]
        );
        let calls = s.fake.calls.lock().unwrap().clone();
        let has = |call: &[&str]| calls.iter().any(|c| c == call);
        assert!(has(&[
            "commit",
            "tracon-w-1",
            "localhost/snap-app-commit:k1"
        ]));
        assert!(has(&[
            "image",
            "tag",
            "localhost/snap-app:k1",
            "localhost/snap-app@sha256:5a"
        ]));
        let deleted: Vec<&str> = calls
            .iter()
            .filter(|c| c[..2] == ["image", "delete"])
            .map(|c| c[2].as_str())
            .collect();
        assert_eq!(
            deleted,
            [
                "localhost/snap-app-commit@sha256:c0",
                "localhost/snap-app-commit:k1"
            ]
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
        let run = calls[2].join(" ");
        assert!(
            run.ends_with(&format!("--entrypoint {SANDBOX_BIN} {}", snapshot.0)),
            "{run}"
        );
        assert!(run.contains("target=/opt/tracon,readonly"), "{run}");
    }
}
