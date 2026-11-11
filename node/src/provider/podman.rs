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
//! and the node may not connect to one a subordinate uid made.
//!
//! **Redial.** The node keeps accepting on the socket after the first
//! connection, for that run alone: a supervisor whose connection dropped
//! dials it again with its resume secret.
//!
//! **Network.** By default unchanged from a harness container: the
//! boundary's internal network, the gateway its only peer, the same proxy
//! environment. With `sandbox_tunnels` (`RunSpec::node_only`) the sandbox
//! has no network at all (`--network none`): the socket is its only route,
//! and the gateway's name is its loopback, where the runner's tunnels to the
//! node listen.
//!
//! **Bundle.** Mounted read-only at `/opt/tracon` and relabelled shared, never
//! with `label=disable`.
//!
//! **Token.** On `podman run` as `--env TRACON_SANDBOX_TOKEN` with the value
//! in podman's own environment, so it is on no argv. Podman keeps it in the
//! container's configuration until the container is removed; it is spent
//! once the supervisor has connected.
//!
//! **Cgroup.** A sandbox the node leaves running for its next process must
//! not go down with this one, so `podman` runs without systemd's
//! `INVOCATION_ID` ([`outside_the_service`]).
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
    Bootstrap, Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    Resources, SandboxProvider, SnapshotId, Status,
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
        let mut command = tokio::process::Command::new(bin);
        outside_the_service(&mut command);
        let out = command
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

/// Run `podman` so a sandbox's conmon gets a cgroup of its own rather than
/// staying in the node's. Podman moves conmon into a `libpod-conmon-<id>.scope`
/// unless `INVOCATION_ID` is set, which systemd sets for a service and Podman
/// takes to mean the unit is meant to own the container. Under
/// `tracon.service` that left conmon in the unit's cgroup, where stopping or
/// restarting the unit killed it, and with it every sandbox `detach_all` had
/// left for the next node process. Without it, the unit's stop still reaches
/// the node and everything it spawned, and a left sandbox that is never taken
/// back still ends itself once its redials run out (`--rm` removes it).
pub fn outside_the_service(command: &mut tokio::process::Command) {
    command.env_remove("INVOCATION_ID");
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
    /// The user every sandbox runs as, where not its image's own.
    pub user: Option<String>,
    /// The user namespace every sandbox runs in, where not Podman's default.
    pub userns: Option<String>,
    /// Environment every sandbox gets beyond the boundary's.
    pub env: Vec<(String, String)>,
    /// What [`SandboxProvider::snapshot`] names its images (`<repo>:<id>`).
    pub snapshot_repo: String,
    /// Catalogue service bundles mounted read-only into every sandbox, with
    /// where (`[runtime.podman] sandbox_services`).
    pub service_bundles: Vec<(PathBuf, String)>,
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
        let images = crate::sandbox_images::enabled(cfg);
        Self::new(PodmanOptions {
            spec,
            bundle: crate::bundle::effective(cfg).unwrap_or_default(),
            socket_dir: cfg.sandbox_socket_dir(),
            init: cfg.runtime.podman.sandbox_init,
            limit: None,
            connect_timeout: crate::runner::protocol::CONNECT_WITHIN,
            user: images.then(|| crate::sandbox_images::USER.to_string()),
            userns: images.then(|| crate::sandbox_images::USERNS.to_string()),
            // HOME as Apple sets it: the image's own `ENV HOME` (a snapshot
            // keeps whatever its source said) is not what the node placed
            // the harness's state under.
            env: match images {
                true => std::iter::once(("HOME".into(), crate::sandbox_images::HOME.into()))
                    .chain(crate::sandbox_images::sandbox_env())
                    .collect(),
                false => Vec::new(),
            },
            snapshot_repo: crate::sandbox_images::snapshot_repo(),
            service_bundles: service_bundles(cfg),
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
            user: self.options.user.clone(),
            userns: self.options.userns.clone(),
            env: self.options.env.clone(),
            service_bundles: self.options.service_bundles.clone(),
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
            Err(_) if crate::boundary::podman::local_only(image) => {
                return Err(ProviderError::Failed(format!(
                    "{image} is no longer in Podman's store: a rebuild of its tag replaced \
                     it, or it was removed. No registry holds a localhost/ image, so it is \
                     not pulled; start again to use the image that stands now"
                )))
            }
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

    /// The node's socket bound again for a sandbox still running, in the
    /// directory its container already mounts: the directory stays (a new
    /// one would not be the one mounted), and only the socket an earlier
    /// node process left is replaced.
    fn rebind(&self, name: &str) -> Result<Listener, ProviderError> {
        use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
        let failed = |what: &str, path: &Path, e: std::io::Error| {
            ProviderError::Failed(format!("{what} {}: {e}", path.display()))
        };
        let socket = self.socket_dir(name).join(SOCKET_NAME);
        match std::fs::symlink_metadata(&socket) {
            Ok(meta) if meta.file_type().is_socket() => {
                std::fs::remove_file(&socket).map_err(|e| failed("replace", &socket, e))?
            }
            Ok(_) => {
                return Err(ProviderError::Failed(format!(
                    "{} is not the node's socket",
                    socket.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(failed("read", &socket, e)),
        }
        let listener = bind_unix_labelled(&socket, Some(SOCKET_CONTEXT))
            .map_err(|e| failed("bind", &socket, e))?;
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

impl PodmanProvider {
    /// The configuration of the image `reference` (`podman image inspect`).
    async fn image_inspect(&self, reference: &str) -> Result<serde_json::Value, ProviderError> {
        let text = self
            .cli(&["image", "inspect", "--format", "json", reference])
            .await?;
        let all: Vec<serde_json::Value> = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Failed(format!("podman image inspect: {e}")))?;
        all.into_iter()
            .next()
            .ok_or_else(|| ProviderError::NotFound(reference.to_string()))
    }

    /// Commit the sandbox's filesystem as the image `reference`, labelled
    /// with `labels`, and configured as the image it was made from: a commit
    /// otherwise keeps the run's entrypoint (the supervisor), user, working
    /// directory and environment, the run's token among them. Its volumes
    /// and bind mounts (the workspace, the bundle, the socket directory) are
    /// not in it.
    pub async fn snapshot_as(
        &self,
        handle: &Handle,
        reference: &str,
        labels: &[(String, String)],
    ) -> Result<SnapshotId, ProviderError> {
        let c = self.inspect(handle).await?;
        let image = c["Image"].as_str().unwrap_or_default().to_string();
        let parent = self.image_inspect(&image).await?;
        let changes = commit_changes(&parent["Config"], &c["Config"]["Env"], labels);
        let mut args: Vec<String> = vec!["commit".into(), "--quiet".into()];
        for change in changes {
            args.push("--change".into());
            args.push(change);
        }
        args.push(handle.0.clone());
        args.push(reference.to_string());
        self.cli
            .run(&args, &[])
            .await
            .map_err(|e| ProviderError::Failed(format!("commit {}: {e}", handle.0)))?;
        Ok(SnapshotId(reference.to_string()))
    }

    /// The image a sandbox runs: exact, or derived from another by a
    /// snapshot (its label names which).
    async fn identity_of(&self, c: &serde_json::Value) -> Result<ImageIdentity, ProviderError> {
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
        let image = c["Image"].as_str().unwrap_or_default();
        let from = match image.is_empty() {
            true => None,
            false => self.image_inspect(image).await.ok().and_then(|i| {
                i["Labels"][crate::sandbox_images::FROM_LABEL]
                    .as_str()
                    .or_else(|| i["Config"]["Labels"][crate::sandbox_images::FROM_LABEL].as_str())
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
}

/// What a commit changes so the snapshot is configured as `parent` was, plus
/// `labels`. An environment variable the run added is emptied (a commit
/// cannot remove one); a value the parent had is restored.
pub(crate) fn commit_changes(
    parent: &serde_json::Value,
    run_env: &serde_json::Value,
    labels: &[(String, String)],
) -> Vec<String> {
    let json_list = |v: &serde_json::Value| -> String {
        match v.as_array() {
            Some(items) if !items.is_empty() => serde_json::to_string(items).unwrap_or("[]".into()),
            _ => "[]".into(),
        }
    };
    let quoted = |value: &str| {
        format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$")
        )
    };
    let mut changes = vec![
        format!("ENTRYPOINT {}", json_list(&parent["Entrypoint"])),
        format!("CMD {}", json_list(&parent["Cmd"])),
        format!(
            "USER {}",
            parent["User"]
                .as_str()
                .filter(|u| !u.is_empty())
                .unwrap_or("0:0")
        ),
        format!(
            "WORKDIR {}",
            parent["WorkingDir"]
                .as_str()
                .filter(|w| !w.is_empty())
                .unwrap_or("/")
        ),
    ];
    let parent_env: HashMap<&str, &str> = parent["Env"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str()?.split_once('='))
                .collect()
        })
        .unwrap_or_default();
    let mut names = std::collections::BTreeSet::new();
    for entry in run_env.as_array().into_iter().flatten() {
        let Some((name, value)) = entry.as_str().and_then(|e| e.split_once('=')) else {
            continue;
        };
        if parent_env.get(name) == Some(&value) || !names.insert(name.to_string()) {
            continue;
        }
        changes.push(match parent_env.get(name) {
            Some(original) => format!("ENV {name}={}", quoted(original)),
            None => format!("ENV {name}="),
        });
    }
    for (key, value) in labels {
        changes.push(format!("LABEL {key}={}", quoted(value)));
    }
    changes
}

/// Each catalogue service's bundle and where it is mounted, when this node
/// runs services in its sandboxes.
pub fn service_bundles(cfg: &crate::config::Config) -> Vec<(PathBuf, String)> {
    if !crate::boundary::podman::uses_sandbox_services(cfg) {
        return Vec::new();
    }
    crate::service_bundle::mounts(cfg)
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
    for m in &mounts {
        let target = m["Destination"].as_str().unwrap_or("");
        if target.starts_with(crate::config::SERVICE_BUNDLES_TARGET)
            && m["RW"].as_bool().unwrap_or(true)
        {
            found.push(format!("a service bundle is mounted writable at {target}"));
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

/// Host paths bind-mounted into a sandbox other than the bundle, its
/// socket directory and read-only service bundles: with no network, the
/// socket is meant to be the only thing of the host's it can reach, and a
/// bind mount is a way round that. Named volumes are the sandbox's own.
pub fn other_binds(c: &serde_json::Value) -> Vec<String> {
    let service_bundle = |m: &serde_json::Value, d: &str| {
        d.starts_with(&format!("{}/", crate::config::SERVICE_BUNDLES_TARGET))
            && m["RW"].as_bool() == Some(false)
    };
    c["Mounts"]
        .as_array()
        .map(|mounts| {
            mounts
                .iter()
                .filter(|m| m["Type"].as_str() == Some("bind"))
                .filter_map(|m| Some((m, m["Destination"].as_str()?)))
                .filter(|(m, d)| {
                    *d != BUNDLE_TARGET && *d != SOCKET_DIR_TARGET && !service_bundle(m, d)
                })
                .map(|(_, d)| d)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn networks(c: &serde_json::Value) -> Vec<String> {
    c["NetworkSettings"]["Networks"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Whether a node-only sandbox has no network: Podman's `none`, and no
/// interface beside it. Nothing then resolves names either.
fn node_only_route(c: &serde_json::Value) -> Route {
    let mode = c["HostConfig"]["NetworkMode"].as_str().unwrap_or("");
    let attached = networks(c);
    match mode == "none" && attached.iter().all(|n| n == "none") {
        true => Route::NodeOnly,
        false => Route::Unverified,
    }
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
        let accepted = tokio::select! {
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
        };
        // The socket stays the sandbox's way back: a supervisor whose
        // connection dropped dials it again, with its run's resume secret
        // and nothing else's, until the run ends.
        if let Ok(conn) = &accepted {
            server.keep_accepting(listener, conn);
        }
        accepted
    }

    async fn reattach(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        conn: &SandboxConn,
    ) -> Result<(), ProviderError> {
        match self.status(handle).await? {
            Status::Running => {}
            status => {
                return Err(ProviderError::Failed(format!(
                    "{} is not running ({status:?})",
                    handle.0
                )))
            }
        }
        let listener = self.rebind(&handle.0)?;
        server.keep_accepting(listener, conn);
        Ok(())
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

    /// A commit of the sandbox (see [`PodmanProvider::snapshot_as`]), under
    /// `snapshot_repo`.
    async fn snapshot(&self, handle: &Handle) -> Result<SnapshotId, ProviderError> {
        let from = match self.identity_of(&self.inspect(handle).await?).await? {
            ImageIdentity::Exact { digest } => digest,
            ImageIdentity::Derived { from, .. } => from,
        };
        let reference = format!(
            "{}:{}",
            self.options.snapshot_repo,
            uuid::Uuid::now_v7().simple()
        );
        self.snapshot_as(
            handle,
            &reference,
            &[(crate::sandbox_images::FROM_LABEL.to_string(), from)],
        )
        .await
    }

    async fn snapshot_as(
        &self,
        handle: &Handle,
        reference: &str,
        labels: &[(String, String)],
    ) -> Result<SnapshotId, ProviderError> {
        PodmanProvider::snapshot_as(self, handle, reference, labels).await
    }

    /// A sandbox made from the snapshot's image: a commit is an image like
    /// any other here, so a fork is a create from it, with nothing of the
    /// snapshot's processes and the bundle mounted afresh.
    async fn fork(
        &self,
        snapshot: &SnapshotId,
        resources: &Resources,
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        self.create(&CreateSpec {
            name: None,
            image: &snapshot.0,
            resources,
            mounts: &[],
            bootstrap,
            labels: &crate::runner::RunLabels::default(),
            shape: super::Shape::Session,
        })
        .await
    }

    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError> {
        let c = self.inspect(handle).await?;
        self.identity_of(&c).await
    }

    async fn network(&self, handle: &Handle) -> Result<Network, ProviderError> {
        let c = self.inspect(handle).await?;
        let socket = format!(
            "unix:{}",
            self.socket_dir(&handle.0).join(SOCKET_NAME).display()
        );
        if self.options.spec.node_only {
            let mut destinations = vec![socket];
            if node_only_route(&c) == Route::Unverified {
                destinations.extend(networks(&c).into_iter().map(|n| format!("network:{n}")));
            }
            return Ok(Network {
                destinations,
                dns: Dns::None,
            });
        }
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
        let mut found = breaches(
            &c,
            &self.options.bundle,
            tracon_sandbox::carrier::selinux_enabled(),
        );
        if self.options.spec.node_only {
            found.extend(
                other_binds(&c)
                    .into_iter()
                    .map(|d| format!("a host path is bind-mounted at {d}")),
            );
        }
        if !found.is_empty() {
            return Err(ProviderError::Failed(found.join("; ")));
        }
        let (route, dns) = match self.options.spec.node_only {
            true => (node_only_route(&c), Dns::None),
            false => {
                let network = self.network_inspect().await?;
                route_and_dns(&networks(&c), &self.options.spec.network, &network)
            }
        };
        let image_identity = self.identity_of(&c).await?.kind();
        Ok(Capabilities {
            route,
            dns,
            image_identity,
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
        // The protocol over the internal network and mounted volumes: what
        // a node with tunnels and files off runs.
        let mut cfg = crate::config::Config::default();
        cfg.runtime.podman = crate::config::PodmanRuntime::legacy();
        let mut spec = RunSpec::from_config(&cfg, true);
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
                user: None,
                userns: None,
                env: Vec::new(),
                snapshot_repo: "localhost/spike-snapshot".into(),
                service_bundles: vec![(
                    dir.path().join("browser"),
                    "/opt/tracon-services/browser".into(),
                )],
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
        create_from(s, name, mounts, "localhost/harness@sha256:aa").await
    }

    async fn create_from(
        s: &Setup,
        name: &str,
        mounts: &[Mount],
        image: &str,
    ) -> Result<Handle, ProviderError> {
        s.provider
            .create(&CreateSpec {
                name: Some(name),
                image,
                resources: &Resources::default(),
                mounts,
                bootstrap: &bootstrap(),
                labels: &crate::runner::RunLabels::default(),
                shape: crate::provider::Shape::Command,
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

    #[test]
    fn podman_runs_without_the_services_invocation_id_so_conmon_leaves_its_cgroup() {
        let mut command = tokio::process::Command::new("podman");
        command.env("INVOCATION_ID", "0123abcd");
        outside_the_service(&mut command);
        let envs: Vec<_> = command.as_std().get_envs().collect();
        // `None` is a removal: the child does not inherit it either.
        assert_eq!(envs, [(std::ffi::OsStr::new("INVOCATION_ID"), None)]);
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
            &format!(
                "--mount type=bind,src={},dst=/opt/tracon-services/browser,ro,relabel=shared",
                s.dir.path().join("browser").display()
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
        create_from(&s, "tracon-h-6", &[], "ghcr.io/acme/harness@sha256:aa")
            .await
            .unwrap();
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

    /// An image only this host could hold (one the node built or committed)
    /// that is gone is said to be gone, at once: never a pull from
    /// `https://localhost/v2/`, and never a run Podman would pull it for.
    #[tokio::test]
    async fn a_missing_local_image_is_refused_and_never_pulled() {
        let s = setup(Fake {
            pulled: Some(Arc::new(Mutex::new(false))),
            ..Fake::default()
        });
        let image = "localhost/tracon-repo-app-1a2b3c4d5e@sha256:a970";
        let Err(ProviderError::Failed(error)) = create_from(&s, "tracon-h-7", &[], image).await
        else {
            panic!("a missing local image starts nothing");
        };
        assert!(error.contains("no longer in Podman's store"), "{error}");
        assert!(error.contains(image), "{error}");
        let calls = s.fake.calls.lock().unwrap();
        assert!(
            calls.iter().all(|(a, _)| a[0] != "pull" && a[0] != "run"),
            "{calls:?}"
        );
    }

    /// A local image is run with Podman told never to pull it, so one removed
    /// between the check and the run fails as missing; a registry's may still
    /// be pulled.
    #[tokio::test]
    async fn only_a_local_image_is_run_without_pulling() {
        let s = setup(Fake::default());
        create(&s, "tracon-h-8", &[]).await.unwrap();
        assert!(run_call(&s).0.contains(&"--pull=never".to_string()));
        let s = setup(Fake::default());
        create_from(&s, "tracon-h-9", &[], "ghcr.io/acme/harness@sha256:aa")
            .await
            .unwrap();
        assert!(!run_call(&s).0.contains(&"--pull=never".to_string()));
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
                {"Type": "bind", "Source": s.provider.options.bundle, "Destination": "/opt/tracon", "RW": false},
                {"Type": "bind", "Source": s.provider.socket_dir("tracon-h-1"), "Destination": "/run/tracon", "RW": false},
                {"Type": "volume", "Source": "/var/lib/vol", "Destination": "/work", "RW": true},
                {"Type": "bind", "Source": "/srv/browser", "Destination": "/opt/tracon-services/browser", "RW": false},
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
        let cases: [(&str, serde_json::Value, &str); 7] = [
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
                "/Mounts/3/RW",
                serde_json::json!(true),
                "service bundle is mounted writable",
            ),
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
    async fn a_node_only_sandbox_has_no_network_and_the_gateway_is_its_loopback() {
        let mut s = setup(Fake::default());
        s.provider.options.spec.node_only = true;
        create(&s, "tracon-h-7", &[]).await.unwrap();
        let line = run_call(&s).0.join(" ");
        assert!(
            line.contains("--network none --add-host tracon-gw:127.0.0.1 --cap-drop=ALL"),
            "{line}"
        );
        assert!(!line.contains("tracon-int"), "{line}");
        // The harness still finds the proxy where it always has.
        assert!(
            line.contains("-e HTTPS_PROXY=http://tracon-gw:8888"),
            "{line}"
        );

        // Read back: Podman's `none`, so node-only with no resolver, without
        // asking after a network that need not exist.
        let mut doc = inspected(&s);
        doc[0]["HostConfig"]["NetworkMode"] = serde_json::json!("none");
        doc[0]["NetworkSettings"]["Networks"] = serde_json::json!({"none": {"IPAddress": ""}});
        *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
        let handle = Handle("tracon-h-1".into());
        let caps = s.provider.verify_isolation(&handle).await.unwrap();
        assert_eq!((caps.route, caps.dns), (Route::NodeOnly, Dns::None));
        let network = s.provider.network(&handle).await.unwrap();
        assert_eq!(network.destinations.len(), 1, "{:?}", network.destinations);
        assert!(network.destinations[0].starts_with("unix:"));
        assert!(!s
            .fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(a, _)| a[0] == "network"));

        // Attached to a network after all: not node-only.
        let mut doc = inspected(&s);
        doc[0]["HostConfig"]["NetworkMode"] = serde_json::json!("bridge");
        *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
        assert_eq!(
            s.provider.verify_isolation(&handle).await.unwrap().route,
            Route::Unverified
        );

        // A host path bind-mounted beside the socket is a way round it.
        let mut doc = inspected(&s);
        doc[0]["HostConfig"]["NetworkMode"] = serde_json::json!("none");
        doc[0]["Mounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"Type": "bind", "Source": "/home", "Destination": "/home", "RW": true}));
        *s.fake.inspect.lock().unwrap() = Some(doc.to_string());
        let error = s.provider.verify_isolation(&handle).await.unwrap_err();
        assert!(
            error.to_string().contains("bind-mounted at /home"),
            "{error}"
        );
        // A service bundle is read-only, or it is one too.
        let mut doc = inspected(&s);
        doc[0]["HostConfig"]["NetworkMode"] = serde_json::json!("none");
        doc[0]["Mounts"][3]["RW"] = serde_json::json!(true);
        assert_eq!(
            other_binds(&doc[0]),
            ["/opt/tracon-services/browser".to_string()]
        );
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::runner::podman::RunSpec;
    use std::sync::Arc;

    /// A container made from `localhost/base`, and that image with a
    /// snapshot of it once committed.
    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl PodmanCli for Fake {
        async fn run(&self, args: &[String], _: &[(String, String)]) -> Result<String, CliError> {
            self.calls.lock().unwrap().push(args.to_vec());
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            Ok(match a.as_slice() {
                ["container", "inspect", "--format", "json", name] => serde_json::json!([{
                    "Image": if name.starts_with("fork") { "snapid" } else { "baseid" },
                    "ImageDigest": if name.starts_with("fork") { "sha256:snap" } else { "sha256:base" },
                    "Config": {"Env": [
                        "PATH=/usr/bin", "HOME=/root", "HTTPS_PROXY=http://gw:8888",
                        "TRACON_SANDBOX_TOKEN=run-secret", "TRACON_SANDBOX_PIN=sha256:ab"
                    ]},
                }])
                .to_string(),
                ["image", "inspect", "--format", "json", "baseid"] => serde_json::json!([{
                    "Config": {"Env": ["PATH=/usr/bin", "HOME=/home/harness"],
                               "User": "65532:65532", "WorkingDir": "/work", "Cmd": ["bash"]},
                    "Labels": {},
                }])
                .to_string(),
                ["image", "inspect", "--format", "json", _] => serde_json::json!([{
                    "Config": {"Env": ["PATH=/usr/bin"]},
                    "Labels": {crate::sandbox_images::FROM_LABEL: "localhost/base@sha256:base"},
                }])
                .to_string(),
                ["image", "inspect", "--format", _, _] => "null".into(),
                ["commit", ..] | ["volume", "create", ..] | ["run", ..] | ["stop", ..] | ["rm", ..] => String::new(),
                other => panic!("unexpected podman {other:?}"),
            })
        }
    }

    fn provider(fake: &Fake, dir: &Path) -> PodmanProvider {
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(bundle.join("bin")).unwrap();
        std::fs::write(bundle.join("bin/tracon-sandbox"), b"").unwrap();
        PodmanProvider::with_cli(
            PodmanOptions {
                spec: RunSpec::from_config(&crate::config::Config::default(), true),
                bundle,
                socket_dir: dir.join("s"),
                init: false,
                limit: None,
                connect_timeout: Duration::from_secs(60),
                user: Some("65532:65532".into()),
                userns: Some("keep-id:uid=65532,gid=65532".into()),
                env: vec![("DISABLE_TELEMETRY".into(), "1".into())],
                snapshot_repo: "localhost/spike-snapshot".into(),
                service_bundles: Vec::new(),
            },
            Box::new(fake.clone()),
        )
    }

    #[tokio::test]
    async fn a_snapshot_is_configured_as_its_image_and_keeps_no_token() {
        let fake = Fake::default();
        let dir = tempfile::Builder::new()
            .prefix("ps")
            .tempdir_in("/tmp")
            .unwrap();
        let p = provider(&fake, dir.path());
        let id = p.snapshot(&Handle("prep".into())).await.unwrap();
        assert!(id.0.starts_with("localhost/spike-snapshot:"), "{id:?}");
        let calls = fake.calls.lock().unwrap().clone();
        let commit = calls.iter().find(|c| c[0] == "commit").unwrap();
        let changes: Vec<&str> = commit
            .windows(2)
            .filter(|w| w[0] == "--change")
            .map(|w| w[1].as_str())
            .collect();
        assert!(changes.contains(&"ENTRYPOINT []"), "{changes:?}");
        assert!(changes.contains(&"CMD [\"bash\"]"), "{changes:?}");
        assert!(changes.contains(&"USER 65532:65532"), "{changes:?}");
        assert!(changes.contains(&"WORKDIR /work"), "{changes:?}");
        assert!(
            changes.contains(&"ENV TRACON_SANDBOX_TOKEN="),
            "{changes:?}"
        );
        assert!(changes.contains(&"ENV HTTPS_PROXY="), "{changes:?}");
        assert!(
            changes.contains(&"ENV HOME=\"/home/harness\""),
            "{changes:?}"
        );
        assert!(
            !changes.iter().any(|c| c.starts_with("ENV PATH")),
            "{changes:?}"
        );
        assert!(changes.contains(&"LABEL io.tracon.snapshot.from=\"sha256:base\""));
        assert!(
            !commit.iter().any(|a| a.contains("run-secret")),
            "{commit:?}"
        );
        assert_eq!(
            commit[commit.len() - 2..],
            ["prep".to_string(), id.0.clone()]
        );
    }

    #[tokio::test]
    async fn a_fork_is_a_sandbox_of_the_snapshot_and_reads_as_derived() {
        let fake = Fake::default();
        let dir = tempfile::Builder::new()
            .prefix("pf")
            .tempdir_in("/tmp")
            .unwrap();
        let p = provider(&fake, dir.path());
        let bootstrap = crate::provider::Bootstrap {
            run: "fork-run".into(),
            cert_pin: format!("sha256:{}", "ab".repeat(32)),
            token: "fork-secret".into(),
        };
        let handle = p
            .fork(
                &SnapshotId("localhost/spike-snapshot:1".into()),
                &Resources::default(),
                &bootstrap,
            )
            .await
            .unwrap();
        let calls = fake.calls.lock().unwrap().clone();
        let run = calls.iter().find(|c| c[0] == "run").unwrap();
        assert_eq!(run.last().unwrap(), "localhost/spike-snapshot:1");
        // The sandbox runs as the base's user, which is the node's own.
        let user = run.iter().position(|a| a == "--user").unwrap();
        assert_eq!(run[user + 1], "65532:65532");
        assert!(run.contains(&"--userns=keep-id:uid=65532,gid=65532".to_string()));
        assert!(run.contains(&"DISABLE_TELEMETRY=1".to_string()));
        p.destroy(&handle).await.unwrap();

        let derived = p.image_identity(&Handle("fork-1".into())).await.unwrap();
        assert_eq!(
            derived,
            ImageIdentity::Derived {
                from: "localhost/base@sha256:base".into(),
                actual: "sha256:snap".into(),
            }
        );
        let exact = p.image_identity(&Handle("prep".into())).await.unwrap();
        assert_eq!(
            exact,
            ImageIdentity::Exact {
                digest: "sha256:base".into()
            }
        );
    }

    #[test]
    fn a_restored_value_is_quoted_and_an_added_one_emptied() {
        let parent = serde_json::json!({"Env": ["A=x y\"$z"], "Entrypoint": ["/init"]});
        let run = serde_json::json!(["A=changed", "B=new", "B=again"]);
        let changes = commit_changes(&parent, &run, &[("k".into(), "v".into())]);
        assert_eq!(
            changes,
            [
                "ENTRYPOINT [\"/init\"]",
                "CMD []",
                "USER 0:0",
                "WORKDIR /",
                "ENV A=\"x y\\\"\\$z\"",
                "ENV B=",
                "LABEL k=\"v\"",
            ]
        );
    }
}
