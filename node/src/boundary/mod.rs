//! Establishing and verifying the harness boundary. A node that cannot prove
//! the boundary refuses to run harnesses; there is no advisory mode.
//!
//! The boundary has more than one implementation — rootless Podman on a
//! laptop, sandbox pods behind a NetworkPolicy on a cluster — and every one of
//! them answers the same five checks (`checks::CheckId`) and hands out the
//! same `Runner`. `Backend` is that seam; `backend_for` picks one from
//! `[runtime] kind`.

pub mod apple;
pub mod checks;
pub mod kubernetes;
pub mod podman;
pub mod reconcile;
pub mod registry;

use std::sync::Arc;

use async_trait::async_trait;

pub use checks::{BoundaryReport, CheckId, CheckResult};

use crate::config::{Config, RuntimeKind};
use crate::runner::{Mount, Runner};

#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("podman: {0}")]
    Podman(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// One client's way out: proxy credentials that open exactly the hosts they
/// were issued for, for as long as this is held. A repository's dependency
/// preparation and a session each hold their own, so
/// none of them is ever served what another was granted. Dropping it revokes
/// the credentials.
pub struct EgressGrant {
    /// `http://host:port`, without credentials: what a browser is given.
    pub server: String,
    pub username: String,
    pub password: String,
    revoke: Option<Box<dyn FnOnce() + Send>>,
}

impl EgressGrant {
    pub fn new(
        server: String,
        username: String,
        password: String,
        revoke: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            server,
            username,
            password,
            revoke: Some(Box::new(revoke)),
        }
    }

    /// The proxy as a URL carrying its credentials, which is how every
    /// package manager and Git take one.
    pub fn url(&self) -> String {
        match self.server.split_once("://") {
            Some((scheme, rest)) => {
                format!("{scheme}://{}:{}@{rest}", self.username, self.password)
            }
            None => self.server.clone(),
        }
    }

    /// The proxy variables that send a container through this grant instead
    /// of the harness proxy. Both spellings: curl and Git read the lowercase
    /// names, and most package managers read either. Secret, because the URL
    /// carries the grant's password: whoever reads it is served the grant's
    /// hosts for as long as it is held.
    pub fn env(&self) -> crate::runner::SecretEnv {
        ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"]
            .into_iter()
            .map(|name| (name.to_string(), self.url()))
            .collect::<Vec<_>>()
            .into()
    }
}

impl Drop for EgressGrant {
    fn drop(&mut self) {
        if let Some(revoke) = self.revoke.take() {
            revoke();
        }
    }
}

/// What a build step is told when it asks for a host the repository's
/// entry does not name.
pub(crate) const BUILD_REFUSAL: &str =
    "not reachable from this repository's build steps; add it to its egress";

/// The proxy variables a build's steps are given, in both spellings: curl
/// reads only the lowercase ones, apt and most package managers either.
pub(crate) const BUILD_PROXY_VARS: [&str; 4] =
    ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"];

/// One build's listener on the node's loopback, serving a grant of the
/// node's egress proxy for exactly the build's hosts. Both go when it is
/// dropped, a build abandoned at its deadline included.
pub(crate) struct BuildEgress {
    task: tokio::task::JoinHandle<()>,
    grants: crate::gateway::proxy::Grants,
    token: String,
    /// The listener's port on the node's loopback.
    pub port: u16,
}

impl BuildEgress {
    pub(crate) async fn serve(
        grants: &crate::gateway::proxy::Grants,
        refusal: crate::gateway::proxy::Refusal,
        hosts: &[String],
    ) -> Result<Self, BuildFailure> {
        let unready = |error: std::io::Error| {
            BuildFailure::transient(format!("build egress listener: {error}"))
        };
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(unready)?;
        let port = listener.local_addr().map_err(unready)?.port();
        let token = grants
            .issue(crate::gateway::proxy::GrantSpec {
                client: "build".into(),
                hosts: hosts.to_vec(),
                refusal: BUILD_REFUSAL.into(),
                ..Default::default()
            })
            .map_err(|error| BuildFailure::transient(format!("build egress grant: {error}")))?;
        let granted = crate::gateway::proxy::Granted::refusing(grants.clone(), refusal);
        Ok(BuildEgress {
            task: tokio::spawn(crate::gateway::proxy::serve_granted(listener, granted)),
            grants: grants.clone(),
            token,
            port,
        })
    }

    /// The grant's proxy URL, as a step reaches it at loopback `port`.
    pub(crate) fn proxy(&self, port: u16) -> String {
        format!("http://build:{}@127.0.0.1:{port}", self.token)
    }
}

impl Drop for BuildEgress {
    fn drop(&mut self) {
        self.task.abort();
        self.grants.revoke(&self.token);
    }
}

/// A repository's `build` steps: run as root on `base`, each in turn,
/// reaching only `hosts`.
pub struct StepsBuild<'a> {
    /// The stock base, pinned by digest.
    pub base: &'a str,
    pub steps: &'a [String],
    pub hosts: &'a [String],
}

/// One image to build: a Containerfile and the directory it may copy from.
pub struct ImageBuild<'a> {
    /// `name:tag`. The built image is reported by digest, under `name`.
    pub tag: &'a str,
    pub containerfile: &'a std::path::Path,
    pub context: &'a std::path::Path,
    pub labels: &'a [(String, String)],
    pub timeout: std::time::Duration,
}

/// A build that produced an image, and the end of what it printed.
#[derive(Debug, Clone)]
pub struct BuiltImage {
    /// `name@sha256:…`: the reference a run pins, which a later build under
    /// the same tag cannot move.
    pub image: String,
    pub log_tail: String,
}

/// A build that produced none.
#[derive(Debug, Clone, Default)]
pub struct BuildFailure {
    pub error: String,
    pub log_tail: String,
    /// The builder could not build at all (a daemon that does not answer, a
    /// registry that refuses the credential, a builder configured wrong),
    /// rather than the recipe failing. Such a failure says nothing about the
    /// recipe, so it is not remembered against it: the next start tries
    /// again.
    pub transient: bool,
}

impl BuildFailure {
    /// A failure of the builder rather than of the recipe.
    pub fn transient(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            transient: true,
            ..Default::default()
        }
    }
}

/// Whether a builder holds an image, as far as it can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    /// It could not be asked: a registry that did not answer. What is
    /// standing keeps being used, and nothing is rebuilt on its account.
    Unknown,
}

/// A runtime that can build an image where the node's runs will find it. A
/// build reaches the network and runs the Containerfile's commands outside the
/// harness boundary, so only what the operator named is ever built.
#[async_trait]
pub trait ImageBuilder: Send + Sync {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure>;
    /// `build`, with the network its `RUN` steps get held to `hosts`,
    /// through a grant on the node's egress proxy, where this builder can
    /// hold it there (Podman's). One that cannot builds on its own network,
    /// as it builds a Dockerfile: a cluster's BuildKit.
    async fn build_with_egress(
        &self,
        build: ImageBuild<'_>,
        _hosts: &[String],
    ) -> Result<BuiltImage, BuildFailure> {
        self.build(build).await
    }
    /// A repository's `build` steps, as `build`'s Containerfile (written
    /// from them) has them, with their network held to the steps' hosts
    /// where the builder can. A builder whose own build reaches the network
    /// however it likes runs the steps some other way (Apple's).
    async fn build_steps(
        &self,
        build: ImageBuild<'_>,
        steps: &StepsBuild<'_>,
    ) -> Result<BuiltImage, BuildFailure> {
        self.build_with_egress(build, steps.hosts).await
    }
    /// Whether the runtime still holds this image. One it pruned has to be
    /// built again before anything can run in it.
    async fn exists(&self, image: &str) -> bool;
    /// `name@sha256:…` for an image the runtime holds under a tag: what the
    /// tag names right now, which a rebuild under the same tag changes.
    async fn identity(&self, image: &str) -> Option<String>;
    /// Drop an image nothing uses any more. Best effort: one a container
    /// still runs from stays.
    async fn remove(&self, image: &str);
    /// Where a build's context and Containerfile are staged. `None` is the
    /// system temporary directory; a builder whose host keeps that on small
    /// ephemeral storage (a pod's container filesystem) names one with room.
    fn scratch_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// `exists`, with a third answer for a builder that may be unable to ask.
    async fn presence(&self, image: &str) -> Presence {
        match self.exists(image).await {
            true => Presence::Present,
            false => Presence::Absent,
        }
    }
    /// Why no build could succeed right now, asked before anything is staged
    /// or recorded. `None` for a builder that is always there.
    async fn unavailable(&self) -> Option<String> {
        None
    }
    /// Whether a superseded image is kept for one more generation before it
    /// is removed. A registry's is, because a pod already scheduled on it may
    /// not have pulled it yet; a local store's need not be, because the
    /// runtime refuses to remove an image a container uses.
    fn retains_previous(&self) -> bool {
        false
    }
    /// The registry every build is pushed to, without a trailing slash;
    /// `None` for a builder whose images stay in a local store.
    fn registry(&self) -> Option<String> {
        None
    }
}

/// One harness's image as the runtime holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessImage {
    pub harness_id: String,
    pub image: String,
    pub state: &'static str,
}

/// One way of putting a harness behind a boundary the node can verify.
#[async_trait]
pub trait Backend: Send + Sync {
    /// `podman` or `kubernetes`; shown in logs and `/api/nodes`.
    fn kind(&self) -> &'static str;
    /// `tracon setup`: make what the boundary needs exist. Idempotent.
    async fn setup(&self, cfg: &Config, rebuild: bool) -> Result<(), BoundaryError>;
    /// What the node runs on its own (`reconcile`): `setup`, touching only
    /// what differs from what this binary and `node.toml` describe, so a
    /// gateway sessions are using is left running unless it has to change.
    /// `Ok(true)` when anything changed. Callers go through
    /// `reconcile::converge`, which keeps it from overlapping container
    /// starts and other setups.
    async fn converge(&self, _cfg: &Config) -> Result<bool, BoundaryError> {
        Ok(false)
    }
    /// Whether `converge` does anything here. Where the boundary is someone
    /// else's to apply (a cluster's manifests), it does not.
    fn converges(&self) -> bool {
        false
    }
    /// A digest of what `converge` would build from: when it changes under a
    /// running node (a new pinned base, a bundle named elsewhere), the node converges
    /// again. `None` where nothing is built.
    fn definitions(&self, _cfg: &Config) -> Option<String> {
        None
    }
    /// Bring back what the boundary needs and finds stopped, without
    /// rerunning setup, before the checks look for it: at startup and on a
    /// re-check. Never creates what setup has not, and never touches what is
    /// running. A failure is logged, not returned: the checks that follow say
    /// what is still wrong.
    async fn recover(&self, _cfg: &Config) {}
    /// The startup verification, against the same specification a session
    /// runs. `deep` adds the active egress probe from inside the boundary.
    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport;
    /// A runner for the configured harness, carrying these mounts in addition
    /// to the boundary's own.
    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner>;
    /// A runner for a named harness: its image and its state layout. The
    /// configured harness is only the default; a session runs the one it
    /// names.
    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner>;
    /// A runner for a named harness in an image other than the stock base:
    /// the repository's, or its prepared snapshot, with the harness from the
    /// bundle. Everything else is the boundary's, exactly as `runner_for` has
    /// it. A runtime that cannot run one keeps the stock base.
    fn runner_in(
        &self,
        harness_id: &str,
        image: &str,
        extra_mounts: Vec<Mount>,
    ) -> Arc<dyn Runner> {
        let _ = image;
        self.runner_for(harness_id, extra_mounts)
    }
    /// Each supported harness's image and its state: `current`, `stale`,
    /// `missing`, or `unknown` where this runtime cannot say. For display;
    /// `check_all` is what refuses a node whose images are not usable.
    async fn harness_images(&self, _cfg: &Config) -> Vec<HarnessImage> {
        Vec::new()
    }
    /// What builds a repository's image for this runtime. `None` for a
    /// runtime that only pulls: its repositories keep the images they are
    /// given.
    fn image_builder(&self) -> Option<&dyn ImageBuilder> {
        None
    }
    /// What a repository's `prepare` runs in and is snapshotted from, on a
    /// runtime whose repositories start from prepared snapshots.
    fn preparer(&self) -> Option<crate::sandbox_images::Preparer> {
        None
    }
    /// Every prepared snapshot of this node's that the runtime holds, pinned;
    /// its image builder removes one. A runtime that keeps them in a registry
    /// lists none: the store's ready rows say which are current.
    async fn prepared_snapshots(&self) -> Vec<String> {
        Vec::new()
    }
    /// Where a sandbox's trees are kept while it runs over the sandbox
    /// protocol, for a sandbox elsewhere (DigitalOcean's) that runs beside
    /// this node's own. `None` for a runtime whose sandboxes mount them.
    fn sandbox_files_store(&self) -> Option<crate::runner::sandbox_files::FilesStore> {
        None
    }
    /// What builds a DigitalOcean template's source and pushes it to a
    /// registry the runtime pulls from, where that is not the node's own
    /// Podman: a cluster's BuildKit. `None` where nothing here can.
    fn registry_builder(&self) -> Option<Arc<dyn ImageBuilder>> {
        None
    }
    /// Copy an explicitly staged directory into runtime-owned storage. The
    /// source is never mounted into a harness.
    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError>;
    /// As `import_volume`, for a tree a run will work in. A candidate's
    /// snapshot is read-only on the node — directories and files alike — and
    /// carries those modes into the volume, where a run that is root with no
    /// capabilities cannot override them: it could not create `target/` or
    /// `node_modules/`, or let a formatter fix a file. A backend whose import
    /// carries no write bits (`workspace::copy_tree` keeps only whether a file
    /// is executable) has nothing to do.
    async fn import_writable(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.import_volume(volume, source).await
    }
    /// Copy a runtime-owned directory into a node-owned staging path. Callers
    /// validate the snapshot before interpreting it as source or Git data.
    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError>;
    /// Copy a workspace out as Git sees it: its repository, and the tracked
    /// and untracked files its ignore rules leave in. What a build or an
    /// install put there — `target/`, `node_modules/` and their symlinks — is
    /// neither the candidate nor something the snapshot's limits could hold.
    /// A backend with no way to filter exports the whole volume.
    async fn export_workspace(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        self.export_volume(volume, destination).await
    }
    /// The name by which a harness reaches the node (the MCP endpoint and the
    /// deep probe's ping).
    fn harness_host(&self) -> String;
    /// The harness user's home inside its runner; state and gitconfig are
    /// mounted under it.
    fn harness_home(&self) -> String;
    /// The port the node itself serves the CONNECT allowlist proxy on, where
    /// a harness reaches it directly rather than through a tunnel.
    fn proxy_port(&self) -> Option<u16> {
        None
    }
    /// The grants this backend's containers present to the node's own proxy,
    /// which the node serves (`gateway::proxy::serve_granted`). `None` for a
    /// backend with no per-client egress.
    fn egress_grants(&self) -> Option<&crate::gateway::proxy::Grants> {
        None
    }
    /// Open a way out for one client, to exactly what `spec` names, until the
    /// returned grant is dropped. Each client is filtered by its own grant:
    /// there is no shared allow list to widen and nothing to wait for.
    /// `Err` when this backend has no per-client egress wired: the caller
    /// refuses to run rather than proceed on the harness's own
    /// (LLM-provider-only, and shared with every other session) egress path.
    fn egress_grant(
        &self,
        _spec: crate::gateway::proxy::GrantSpec,
    ) -> Result<EgressGrant, BoundaryError> {
        Err(BoundaryError::Other(format!(
            "the {} backend has no per-client egress",
            self.kind()
        )))
    }
    /// What the node's own proxies will not dial, whatever a grant or the
    /// allowlist names (`gateway::proxy::Refusal`): the per-client egress
    /// proxy, and the allowlist proxy where the node serves it
    /// (`proxy_port`). This machine and its link unless a runtime says more.
    fn egress_refusal(&self) -> crate::gateway::proxy::Refusal {
        crate::gateway::proxy::Refusal::this_machine()
    }
    /// Remove harnesses left over from a previous run, by name.
    async fn reconcile(&self, names: &[String]);
    /// The runtime volumes this backend holds whose names begin with
    /// `tracon-`. A backend that keeps none has nothing to list.
    async fn list_volumes(&self) -> Result<Vec<VolumeInfo>, BoundaryError> {
        Ok(Vec::new())
    }
    /// Whether the runtime holds a volume of this name.
    async fn volume_exists(&self, volume: &str) -> bool {
        self.list_volumes()
            .await
            .is_ok_and(|volumes| volumes.iter().any(|held| held.name == volume))
    }
    /// Make `destination` a copy of `source`, inside the runtime: neither is
    /// read by the node, so what a build or an install left in one (symlinks,
    /// build output) is carried as it is. A source that does not exist leaves
    /// an empty destination.
    async fn clone_volume(&self, source: &str, destination: &str) -> Result<(), BoundaryError> {
        let _ = destination;
        Err(BoundaryError::Other(format!(
            "the {} backend cannot copy runtime volume {source}",
            self.kind()
        )))
    }
    /// Whether a new session cache starts as a copy of the repository's base
    /// cache `base`. Podman's does, from the snapshot's cache when there is
    /// one (`sandbox_images::seed_for`); a runtime whose snapshot carries the
    /// cache inside it, or which holds no base cache, has nothing to copy.
    async fn seeds_session_cache(&self, base: &str, from_snapshot: bool) -> bool {
        let _ = (base, from_snapshot);
        true
    }
    /// Whether a session's workspace is sent over its image's own `/work`
    /// rather than mounted in its place, so that what a prepared snapshot
    /// left there (`node_modules`, `vendor/`) is under the session's tree.
    /// Only then may a session at the snapshot's commit skip `prepare`.
    fn workspace_over_image(&self) -> bool {
        false
    }
    /// Remove a runtime volume. One that does not exist is already removed;
    /// one a running harness still mounts is refused, never forced.
    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        Err(BoundaryError::Other(format!(
            "the {} backend cannot remove runtime volume {volume}",
            self.kind()
        )))
    }
    /// The containers this node's copies into and out of runtime volumes
    /// made (`tracon-transfer-…`) that are still here. A copy removes its
    /// own when it ends, so one listed long after outlived a node that
    /// stopped mid-copy, and holds its image and its volume as in use. A
    /// backend that makes none has none to list.
    async fn list_transfers(&self) -> Result<Vec<TransferInfo>, BoundaryError> {
        Ok(Vec::new())
    }
    /// Remove one of [`Backend::list_transfers`]' containers. One running
    /// is refused, never forced.
    async fn remove_transfer(&self, name: &str) -> Result<(), BoundaryError> {
        Err(BoundaryError::Other(format!(
            "the {} backend cannot remove container {name}",
            self.kind()
        )))
    }
}

/// A transfer's container as its backend reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TransferInfo {
    pub name: String,
    pub running: bool,
    /// When the backend can say.
    pub created_ms: Option<i64>,
    /// The id of the image it was made from.
    pub image: String,
}

/// A runtime volume as its backend reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VolumeInfo {
    pub name: String,
    /// When the backend can say.
    pub created_ms: Option<i64>,
}

/// This node's volumes kept as directories under `root`, as the Kubernetes
/// and local backends store them. A staging copy mid-import is not a volume.
pub fn directory_volumes(root: &std::path::Path) -> Result<Vec<VolumeInfo>, BoundaryError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut volumes = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !crate::gc::is_own(&name) || name.contains(".staging-") || !entry.path().is_dir() {
            continue;
        }
        let created_ms = entry
            .metadata()
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        volumes.push(VolumeInfo { name, created_ms });
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(volumes)
}

/// Remove a directory-backed volume; a missing one is already gone.
pub fn remove_directory_volume(root: &std::path::Path, volume: &str) -> Result<(), BoundaryError> {
    if volume.is_empty() || volume.contains('/') || volume.contains("..") {
        return Err(BoundaryError::Other(format!("not a volume name: {volume}")));
    }
    match std::fs::remove_dir_all(root.join(volume)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn check_volume_names(names: &[&str]) -> Result<(), BoundaryError> {
    for volume in names {
        if volume.is_empty() || volume.contains('/') || volume.contains("..") {
            return Err(BoundaryError::Other(format!("not a volume name: {volume}")));
        }
    }
    Ok(())
}

/// Copy one directory-backed volume to another, as the runtime's own copy
/// would: everything, symlinks as symlinks. Nothing here is validated, because
/// nothing here is read — the node interprets a volume only through an export.
pub fn clone_directory_volume(
    root: &std::path::Path,
    source: &str,
    destination: &str,
) -> Result<(), BoundaryError> {
    check_volume_names(&[source, destination])?;
    fn copy(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let target = to.join(entry.file_name());
            if kind.is_dir() {
                copy(&entry.path(), &target)?;
            } else if kind.is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
    }
    let (from, to) = (root.join(source), root.join(destination));
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    match from.is_dir() {
        true => Ok(copy(&from, &to)?),
        false => Ok(std::fs::create_dir_all(&to)?),
    }
}

/// `clone_directory_volume` through the system `cp`, which keeps modes,
/// links and times in one pass and shares extents where the filesystem can
/// (`--reflink=auto`): a run's dependency cache and each check's tree are
/// clones, of tens of thousands of files. The flags are the ones Podman's
/// clone uses (`CLONE_TREE`), `-a` without extended attributes and security
/// labels, which GNU coreutils and busybox both take. A `cp` that is missing
/// or refuses them leaves the copy to the node itself.
pub async fn clone_directory_volume_with(
    cp: &str,
    root: &std::path::Path,
    source: &str,
    destination: &str,
) -> Result<(), BoundaryError> {
    check_volume_names(&[source, destination])?;
    let (from, to) = (root.join(source), root.join(destination));
    if !from.is_dir() {
        return clone_directory_volume(root, source, destination);
    }
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    std::fs::create_dir_all(&to)?;
    let copied = tokio::process::Command::new(cp)
        .args(["-dRp", "--reflink=auto", "--"])
        .arg(from.join("."))
        .arg(&to)
        .output()
        .await;
    match copied {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => tracing::warn!(
            source,
            destination,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "cp could not clone the volume; copying it in process"
        ),
        Err(error) => tracing::warn!(
            source,
            destination,
            %error,
            "cp is unavailable; copying the volume in process"
        ),
    }
    clone_directory_volume(root, source, destination)
}

/// The backend `[runtime] kind` selects. Detection that needs the host (the
/// SELinux probe) happens here, once, rather than per session.
pub async fn backend_for(cfg: &Config) -> Arc<dyn Backend> {
    match cfg.runtime.kind {
        RuntimeKind::Podman => Arc::new(podman::PodmanBackend::detect(cfg).await),
        RuntimeKind::Kubernetes => Arc::new(kubernetes::KubeBackend::new(cfg)),
        RuntimeKind::Apple => Arc::new(apple::AppleBackend::new(cfg)),
    }
}

/// Whether this node's sandboxes take their trees over the files channel and
/// reach the node only through tunnels: what a sandbox that mounts nothing
/// and has no network of its own (DigitalOcean's) needs of the node. Every
/// runtime's do.
pub fn sandboxes_over_protocol(cfg: &Config) -> bool {
    match cfg.runtime.kind {
        RuntimeKind::Apple | RuntimeKind::Podman | RuntimeKind::Kubernetes => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prepared tree is copied as the runtime would copy it: what an install
    /// left behind — a `.bin` of symlinks — arrives as it was, where the
    /// node's own imports and exports would refuse it.
    #[cfg(unix)]
    #[test]
    fn a_directory_volume_is_cloned_with_its_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("tracon-prep-a/node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(root.path().join("tracon-prep-a/a.txt"), "one").unwrap();
        std::os::unix::fs::symlink("../acorn/bin/acorn", bin.join("acorn")).unwrap();
        std::fs::create_dir_all(root.path().join("tracon-check-b")).unwrap();
        std::fs::write(root.path().join("tracon-check-b/stale"), "x").unwrap();

        clone_directory_volume(root.path(), "tracon-prep-a", "tracon-check-b").unwrap();
        let copy = root.path().join("tracon-check-b");
        assert_eq!(std::fs::read_to_string(copy.join("a.txt")).unwrap(), "one");
        assert_eq!(
            std::fs::read_link(copy.join("node_modules/.bin/acorn")).unwrap(),
            std::path::Path::new("../acorn/bin/acorn")
        );
        assert!(!copy.join("stale").exists());

        // A source that does not exist is an empty volume to copy.
        clone_directory_volume(root.path(), "tracon-cache-base-none", "tracon-prep-cache-c")
            .unwrap();
        assert!(root.path().join("tracon-prep-cache-c").is_dir());
        assert!(clone_directory_volume(root.path(), "../etc", "tracon-x").is_err());
    }

    /// The system `cp` and the in-process copy it falls back to make the same
    /// clone: executable bits and links kept, a stale destination replaced.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_clone_keeps_modes_and_links_with_or_without_cp() {
        use std::os::unix::fs::PermissionsExt;

        for cp in ["cp", "/nonexistent/tracon-cp"] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("tracon-prep-a");
            std::fs::create_dir_all(source.join("node_modules/.bin")).unwrap();
            std::fs::write(source.join("run.sh"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(
                source.join("run.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                "../acorn/bin/acorn",
                source.join("node_modules/.bin/acorn"),
            )
            .unwrap();
            std::fs::create_dir_all(root.path().join("tracon-check-b")).unwrap();
            std::fs::write(root.path().join("tracon-check-b/stale"), "x").unwrap();

            clone_directory_volume_with(cp, root.path(), "tracon-prep-a", "tracon-check-b")
                .await
                .unwrap();
            let copy = root.path().join("tracon-check-b");
            let mode = std::fs::metadata(copy.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "{cp}");
            assert_eq!(
                std::fs::read_link(copy.join("node_modules/.bin/acorn")).unwrap(),
                std::path::Path::new("../acorn/bin/acorn"),
                "{cp}"
            );
            assert!(!copy.join("stale").exists(), "{cp}");
            assert!(
                !copy.join("tracon-prep-a").exists(),
                "{cp} nested the source"
            );

            clone_directory_volume_with(cp, root.path(), "tracon-none", "tracon-empty")
                .await
                .unwrap();
            assert!(root.path().join("tracon-empty").is_dir());
            assert!(
                clone_directory_volume_with(cp, root.path(), "../etc", "tracon-x")
                    .await
                    .is_err()
            );
        }
    }
}
