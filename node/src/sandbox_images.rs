//! `[runtime.podman] sandbox_images`: sandboxes without harness images.
//!
//! With the setting on (it needs `sandbox_protocol`), nothing is built or
//! layered for a harness:
//!
//! - **Images.** A session, check or preparation runs the stock base image
//!   (`[sandbox] base_image`, pulled and pinned by digest when the boundary
//!   converges) or its repository's image. A repository's Dockerfile may
//!   stand on the base: `FROM ${TRACON_SANDBOX_BASE}` names the pinned base
//!   (`repo_image::build_base`), and a new base rebuilds it; one that does
//!   not, or an `image` of its own, is refused (`repo_image::refusal`). The
//!   `tracon-harness-*` images and the layered `-claude`/`-native` repository
//!   images are neither built nor used.
//! - **Harnesses.** Claude Code and tracon-agent run from the harness bundle
//!   mounted at `/opt/tracon` (`crate::bundle`), which the node fetches for
//!   its own release. The protocol runner starts the bundle's copy of a
//!   harness wherever an adapter names the copy a harness image had.
//! - **Prepare once, then fork.** A repository's `prepare` runs on its
//!   default branch in one sandbox, with the dependency cache in the
//!   sandbox's own filesystem; the provider then snapshots the sandbox
//!   (`podman commit`, or `container commit` on an Apple node) and the
//!   repository's sessions and checks start from the snapshot. On Podman the
//!   cache volumes they mount are seeded from it; on Apple a cache is sent in
//!   over the snapshot's own, which keeps what it held. Either way the warm
//!   cache rides along. The workspace is a volume, so nothing of it is in a
//!   snapshot.
//!
//! A snapshot is keyed on what it was made from: the base image's digest,
//! the repository image's digest, the default branch's lockfiles and the
//! preparation recipe (its commands and egress). A change to any of them
//! leaves the snapshot unused and the next session prepares again; the
//! snapshot it supersedes is removed (`repo_image::supersede`, and
//! `tracon gc` for any left over).
//!
//! Sandboxes run as the base image's own user (65532), in a user namespace
//! that maps it to the node's own user (`--userns=keep-id`): what the image
//! gave that user (`/work`, `/cache`, its home) and the volumes the node fills
//! are both its own, and the harness's state is under its home,
//! `/home/harness`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use tracon_sandbox::node::{Process, SandboxConn, SandboxServer};
use tracon_sandbox::protocol::{ExitStatus, SpawnSpec};

use crate::boundary::Backend;
use crate::config::Config;
use crate::environment::RepoEnvironment;
use crate::provider::{Bootstrap, CreateSpec, Handle, Resources, SandboxProvider};
use crate::runner::{Mount, Runner, RunnerCommand, RunnerError, Spawned};
use crate::store::{RepoImageStart, Store};

/// `repo_image` rows of this kind are a repository's prepared snapshots.
pub const SNAPSHOT: &str = "snapshot";

/// Bumped when what goes into a snapshot changes.
const SCHEME: &str = "1";

/// On a snapshot image: the digest of the image it was made from.
pub const FROM_LABEL: &str = "io.tracon.snapshot.from";
/// On a snapshot image: the key it was made for.
pub const KEY_LABEL: &str = "io.tracon.snapshot.key";

/// What the environment's image is called when it is a snapshot.
pub const SNAPSHOT_SOURCE: &str = "prepared snapshot";

/// The user every sandbox runs as: the stock base's own.
pub const USER: &str = "65532:65532";

/// The user namespace every sandbox runs in: [`USER`] is the node's own user
/// on the host, so it owns what the node's volumes hold, and the node owns
/// what it writes.
pub const USERNS: &str = "keep-id:uid=65532,gid=65532";

/// The harness's home in a sandbox: the stock base's user's.
pub const HOME: &str = "/home/harness";

/// Who the node's own short-lived containers (volume transfers and copies)
/// run as: root of a plain rootless namespace, the node's own user on the
/// host, as they always have.
pub const UTILITY_USER: &str = "0:0";

/// Where the bundle's binaries are inside a sandbox.
pub const BUNDLE_BIN: &str = "/opt/tracon/bin";

/// The build argument a repository's Dockerfile names the base by.
pub const BASE_ARG: &str = "TRACON_SANDBOX_BASE";

/// Whether this node runs its sandboxes without harness images.
pub fn enabled(cfg: &Config) -> bool {
    cfg.runtime.kind == crate::config::RuntimeKind::Podman
        && cfg.runtime.podman.sandbox_images
        && crate::boundary::podman::uses_sandbox_protocol(cfg)
}

/// Whether this node's sandboxes run harnesses from the bundle on the stock
/// base or a repository's image, with no harness images: Podman's with the
/// setting, and an Apple node's always. A repository with a `prepare` starts
/// from its prepared snapshot on either.
pub fn bundled(cfg: &Config) -> bool {
    enabled(cfg) || cfg.runtime.kind == crate::config::RuntimeKind::Apple
}

/// The base image as configured, and as this process pinned it.
static PINNED: RwLock<Option<(String, String)>> = RwLock::new(None);

/// The stock base: pinned by digest once the boundary has converged, the
/// configured reference until then.
pub fn base_image(cfg: &Config) -> String {
    pinned_or(
        cfg.sandbox.base_image_ref(),
        PINNED.read().unwrap().as_ref(),
    )
}

/// `configured`, or what it was pinned to when the pin is of it.
fn pinned_or(configured: String, pin: Option<&(String, String)>) -> String {
    match pin {
        Some((named, pinned)) if *named == configured => pinned.clone(),
        _ => configured,
    }
}

/// The pinned base, when the setting is on and the base has been pinned: what
/// a repository's recipe is built on.
pub fn active_base() -> Option<String> {
    PINNED
        .read()
        .unwrap()
        .as_ref()
        .map(|(_, pinned)| pinned.clone())
}

pub(crate) fn pin(configured: &str, pinned: &str) {
    *PINNED.write().unwrap() = Some((configured.to_string(), pinned.to_string()));
}

/// What the boundary's converge adds with the setting on: the bundle for
/// this release, and the base image pulled and pinned. `Ok(true)` when it
/// fetched or pulled anything.
pub async fn converge(cfg: &Config) -> Result<bool, String> {
    if !enabled(cfg) {
        return Ok(false);
    }
    let fetched = match crate::bundle::ensure(cfg).await {
        Ok(Some(crate::bundle::Ensured::Fetched(path))) => {
            tracing::info!(bundle = %path.display(), "fetched the harness bundle for this release");
            true
        }
        Ok(_) => false,
        Err(error) => return Err(format!("harness bundle: {error}")),
    };
    let configured = cfg.sandbox.base_image_ref();
    let mut pulled = false;
    if crate::boundary::podman::podman(&["image", "exists", &configured])
        .await
        .is_err()
    {
        tracing::info!(image = %configured, "pulling the sandbox base image");
        crate::boundary::podman::podman(&["pull", "--quiet", &configured])
            .await
            .map_err(|error| format!("pull {configured}: {error}"))?;
        pulled = true;
    }
    let pinned = crate::boundary::podman::image_identity(&configured)
        .await
        .ok_or_else(|| format!("{configured} has no digest to pin"))?;
    pin(&configured, &pinned);
    Ok(fetched || pulled)
}

/// What [`converge`] depends on, for `Backend::definitions`: a change is a
/// reason to converge again.
pub fn definitions(cfg: &Config) -> String {
    if !enabled(cfg) {
        return String::new();
    }
    format!(
        "sandbox-images\0{}\0{}\0{}",
        env!("CARGO_PKG_VERSION"),
        crate::bundle::effective(cfg)
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cfg.sandbox.base_image_ref()
    )
}

/// Where this node keeps its snapshots: `localhost/<prefix>-snapshot`.
/// Named per node, unlike a repository's image, because a snapshot is this
/// node's preparation and its sweep removes it.
pub fn snapshot_repo() -> String {
    format!("localhost/{}", crate::config::resource_name("snapshot"))
}

/// The image `repo`'s snapshots are tagged under: this node's snapshot
/// repository, with the repository's name and path hash as its image has.
pub fn snapshot_repository(repo: &Path) -> String {
    let image = crate::repo_image::image_name(repo);
    let rest = image
        .strip_prefix("localhost/tracon-repo-")
        .unwrap_or(&image);
    format!("{}-{rest}", snapshot_repo())
}

/// Every prepared snapshot image the runtime holds: pinned
/// (`name@sha256:…`, as the store records it) and as `podman image rm` takes
/// it.
pub async fn snapshot_images() -> Vec<(String, String)> {
    let filter = format!("label={KEY_LABEL}");
    let Ok(out) = crate::boundary::podman::podman(&[
        "images",
        "--filter",
        &filter,
        "--format",
        "{{.Repository}} {{.Digest}} {{.ID}}",
    ])
    .await
    else {
        return Vec::new();
    };
    // Every node on the host labels its snapshots alike; only the name says
    // which are this node's.
    let ours = format!("{}-", snapshot_repo());
    out.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (name, digest, id) = (fields.next()?, fields.next()?, fields.next()?);
            (name.starts_with(&ours) || name == snapshot_repo())
                .then(|| (format!("{name}@{digest}"), id.to_string()))
        })
        .collect()
}

/// The bundle's copy of a harness an adapter names by the path its image
/// had it at.
pub fn bundled_argv0(argv0: &str) -> Option<String> {
    [
        crate::adapter::claude::ClaudeAdapter::BINARY,
        crate::adapter::native::NativeAdapter::BINARY,
    ]
    .contains(&argv0)
    .then(|| {
        let name = argv0.rsplit('/').next().unwrap_or(argv0);
        format!("{BUNDLE_BIN}/{name}")
    })
}

/// The environment a harness image set and a sandbox from the stock base
/// does not: Claude Code told not to update or report itself.
pub fn sandbox_env() -> Vec<(String, String)> {
    [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_TELEMETRY", "1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A lockfile of the default branch, by its Git object id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockfile {
    pub path: String,
    pub blob: String,
}

/// Everything a snapshot is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInputs {
    /// The stock base, by digest.
    pub base: String,
    /// The image the preparation runs in, by digest: the base itself, or the
    /// repository's.
    pub image: String,
    /// The default branch's commit, recorded with the snapshot; not part of
    /// its key, so a commit that leaves the lockfiles alone keeps it.
    pub commit: String,
    pub lockfiles: Vec<Lockfile>,
    pub prepare: Vec<String>,
    pub egress: Vec<String>,
}

impl SnapshotInputs {
    pub fn key(&self) -> String {
        let mut hash = Sha256::new();
        let mut part = |text: &str| {
            hash.update(text.as_bytes());
            hash.update([0]);
        };
        part(SCHEME);
        part(&self.base);
        part(&self.image);
        for lockfile in &self.lockfiles {
            part(&lockfile.path);
            part(&lockfile.blob);
        }
        part("prepare");
        for command in &self.prepare {
            part(command);
        }
        part("egress");
        for host in &self.egress {
            part(host);
        }
        part("env");
        for (name, value) in crate::environment::cache_env() {
            part(&name);
            part(&value);
        }
        hex::encode(hash.finalize())
    }
}

/// The lockfiles preparation recognises, as `commit` has them.
pub async fn lockfiles(repo: &Path, commit: &str) -> Vec<Lockfile> {
    let dir = repo.to_string_lossy();
    let mut found = Vec::new();
    for &(path, _) in crate::environment::LOCKFILES {
        if let Ok(blob) = crate::review::resolve(&dir, &format!("{commit}:{path}")).await {
            found.push(Lockfile {
                path: path.to_string(),
                blob,
            });
        }
    }
    found
}

/// What a snapshot of `repo` in `environment` would be made from now.
pub async fn inputs(
    backend: &dyn Backend,
    cfg: &Config,
    repo: &Path,
    environment: &RepoEnvironment,
) -> Result<SnapshotInputs, String> {
    let builder = backend
        .image_builder()
        .ok_or_else(|| format!("the {} runtime keeps no images", backend.kind()))?;
    let image = builder
        .identity(&environment.image)
        .await
        .ok_or_else(|| format!("the image {} is not here", environment.image))?;
    let (_, commit) = crate::repo_image::default_commit(repo).await?;
    Ok(SnapshotInputs {
        base: base_image(cfg),
        image,
        lockfiles: lockfiles(repo, &commit).await,
        commit,
        prepare: environment.prepare.clone(),
        egress: environment.egress.clone(),
    })
}

/// Which snapshot each base cache volume's copies are seeded from: a copy
/// made of the base cache while a repository has a snapshot is filled from
/// the snapshot's cache instead, which is where its preparation left it.
fn seeds() -> &'static Mutex<HashMap<String, String>> {
    static SEEDS: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    SEEDS.get_or_init(Default::default)
}

pub fn seed_for(base_cache: &str) -> Option<String> {
    seeds().lock().unwrap().get(base_cache).cloned()
}

pub fn record_seed(base_cache: &str, snapshot: &str) {
    seeds()
        .lock()
        .unwrap()
        .insert(base_cache.to_string(), snapshot.to_string());
}

/// What a runtime prepares a repository in and snapshots
/// ([`Backend::preparer`]): its provider, the store that sends the
/// preparation's tree in where the runtime mounts none, and the tunnels to
/// the gateways where its sandboxes have no network.
pub struct Preparer {
    pub provider: Arc<dyn SandboxProvider>,
    pub files: Option<crate::runner::sandbox_files::FilesStore>,
    pub tunnels: Vec<crate::runner::tunnels::TunnelSpec>,
}

/// The ready snapshot of `repo` made for `key`, if the runtime still has it.
pub async fn current(
    backend: &dyn Backend,
    store: &Store,
    repo: &Path,
    key: &str,
) -> Option<String> {
    let row = store
        .ready_repo_image(&repo.to_string_lossy(), SNAPSHOT)
        .ok()
        .flatten()
        .filter(|row| row.recipe_hash == key)?;
    backend
        .image_builder()?
        .exists(&row.image)
        .await
        .then_some(row.image)
}

/// The snapshot `repo`'s runs in `environment` start from, when there is a
/// current one; with `prepare`, one is made first if there is not.
pub async fn snapshot_for(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    environment: &RepoEnvironment,
    prepare: bool,
) -> Option<String> {
    if !bundled(cfg) || environment.prepare.is_empty() {
        return None;
    }
    let inputs = match inputs(backend, cfg, repo, environment).await {
        Ok(inputs) => inputs,
        Err(error) => {
            tracing::info!(repo = %repo.display(), %error, "no snapshot for this repository");
            return None;
        }
    };
    let key = inputs.key();
    let mut found = current(backend, store, repo, &key).await;
    if found.is_none() && prepare {
        crate::repo_image::warm_base_cache(backend, cfg, store, repo).await;
        found = current(backend, store, repo, &key).await;
    }
    if let Some(snapshot) = &found {
        record_seed(&environment.cache_volume, snapshot);
    }
    found
}

/// The default-branch preparation with the setting on: `repo`'s `prepare`
/// in one sandbox from its image, then a snapshot of that sandbox for its
/// runs to start from. Nothing when the current one is already made.
pub async fn warm(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    environment: &RepoEnvironment,
    secrets: &crate::environment::PrepareSecrets,
) -> Result<Option<String>, String> {
    let inputs = inputs(backend, cfg, repo, environment).await?;
    let key = inputs.key();
    let Some(preparer) = backend.preparer() else {
        return Err(format!("the {} runtime makes no snapshots", backend.kind()));
    };
    if let Some(snapshot) = current(backend, store, repo, &key).await {
        record_seed(&environment.cache_volume, &snapshot);
        return Ok(Some(snapshot));
    }
    let repo_path = repo.to_string_lossy().into_owned();
    let row = store
        .start_repo_image(&RepoImageStart {
            repo_path: &repo_path,
            kind: SNAPSHOT,
            recipe_hash: &key,
            source_ref: "",
            source_commit: &inputs.commit,
            parent_image: &inputs.image,
        })
        .map_err(|error| error.to_string())?;
    let made = async {
        let tree = crate::review::snapshot_candidate(
            &repo_path,
            &inputs.commit,
            cfg.live().supervision.max_snapshot_bytes,
        )
        .await
        .map_err(|error| error.to_string())?;
        let id = uuid::Uuid::now_v7().simple().to_string();
        let work = crate::config::resource_name(&format!("warm-{id}"));
        backend
            .import_writable(&work, &tree.root)
            .await
            .map_err(|error| error.to_string())?;
        let reference = format!("{}:{}", snapshot_repository(repo), &key[..12]);
        let name = crate::config::resource_name(&format!("w-{}", &id[..12]));
        let mut held = Held::for_preparation(&preparer, &name, &inputs.image, &work).await;
        if let Ok(opened) = &held {
            if !preparer.tunnels.is_empty() {
                if let Err(error) = opened.open_tunnels(&preparer.tunnels).await {
                    opened.finish().await;
                    held = Err(error);
                }
            }
        }
        let made = match held {
            Ok(held) => {
                let prepared = crate::review::checks::run_preparation_with(
                    backend,
                    &held,
                    environment,
                    (&work, ""),
                    &name,
                    tokio::time::Instant::now() + Duration::from_secs(environment.timeout_secs),
                    &crate::review::checks::RunToCompletion,
                    "no snapshot is made",
                    secrets,
                    Some(repo),
                    crate::telemetry::Purpose::Warm,
                )
                .await
                .map_err(|stopped| stopped.to_string());
                held.conclude(
                    prepared,
                    &reference,
                    &[
                        (FROM_LABEL.to_string(), inputs.image.clone()),
                        (KEY_LABEL.to_string(), key.clone()),
                    ],
                )
                .await
            }
            Err(error) => Err(error),
        };
        let _ = backend.remove_volume(&work).await;
        made
    };
    match made.await {
        Ok(reference) => {
            // Apple's provider names the snapshot by digest itself.
            let pinned = match reference.contains("@sha256:") {
                true => reference,
                false => match backend.image_builder() {
                    Some(builder) => builder.identity(&reference).await.unwrap_or(reference),
                    None => reference,
                },
            };
            let _ = store.finish_repo_image(&row, Ok(&pinned), "", &[]);
            if let Some(builder) = backend.image_builder() {
                crate::repo_image::supersede(builder, store, repo, SNAPSHOT).await;
            }
            record_seed(&environment.cache_volume, &pinned);
            tracing::info!(repo = %repo.display(), snapshot = %pinned, "prepared the repository and snapshotted it");
            // Its DigitalOcean template too, where sessions may run there:
            // pushed from Podman's store.
            if backend.kind() == "podman" {
                crate::provider::digitalocean::runtime::DoRuntime::prepare_if_running(&pinned);
            }
            Ok(Some(pinned))
        }
        Err(error) => {
            let _ = store.finish_repo_image(&row, Err(&error), "", &[]);
            Err(error)
        }
    }
}

/// One sandbox held open for several commands: a preparation's steps all
/// run in it, so what each leaves in the sandbox's own filesystem is there
/// for the next and for the snapshot. Its mounts are fixed when it is made;
/// a command's own are not mounted again.
pub struct Held {
    provider: Arc<dyn SandboxProvider>,
    server: Arc<SandboxServer>,
    handle: Handle,
    run: String,
    conn: SandboxConn,
    running: Mutex<HashMap<String, Arc<Process>>>,
    /// Open for as long as the sandbox is: on a node whose sandboxes have
    /// no network, the way to the gateways.
    tunnels: Mutex<Vec<tracon_sandbox::node::Tunnel>>,
    /// The trees sent in where the runtime mounts no volume.
    files: Option<crate::runner::sandbox_files::Attached>,
    finished: std::sync::atomic::AtomicBool,
}

impl Held {
    pub async fn start(
        provider: Arc<dyn SandboxProvider>,
        files: Option<&crate::runner::sandbox_files::FilesStore>,
        server: Arc<SandboxServer>,
        name: &str,
        image: &str,
        mounts: &[Mount],
    ) -> Result<Held, String> {
        let (mounts, plan) = match files {
            Some(store) => store
                .plan(mounts)
                .map_err(|e| format!("sandbox {name}: {e}"))?,
            None => (mounts.to_vec(), None),
        };
        let run = format!("{name}:{}", uuid::Uuid::now_v7().simple());
        let token = server.tokens().issue(run.clone());
        let bootstrap = Bootstrap {
            run: run.clone(),
            cert_pin: server.pin().to_string(),
            token,
        };
        let created = {
            let _images = crate::boundary::reconcile::images_in_use().await;
            provider
                .create(&CreateSpec {
                    name: Some(name),
                    image,
                    resources: &Resources::default(),
                    mounts: &mounts,
                    bootstrap: &bootstrap,
                    labels: &crate::runner::RunLabels::of(crate::runner::RunKind::Prepare),
                    shape: crate::provider::Shape::Command,
                })
                .await
        };
        let handle = match created {
            Ok(handle) => handle,
            Err(error) => {
                server.tokens().revoke(&run);
                return Err(format!("start sandbox {name}: {error}"));
            }
        };
        let connected = provider
            .connect(&handle, &server, crate::runner::protocol::CONNECT_WITHIN)
            .await
            .map_err(|error| format!("connect to sandbox {name}: {error}"));
        // Sent in, never taken back: what a preparation leaves is in the
        // sandbox's own filesystem, for the snapshot.
        let sent = match (&connected, plan) {
            (Ok(conn), Some(plan)) => plan
                .attach(conn, name, crate::runner::RunKind::Check)
                .await
                .map(Some)
                .map_err(|error| format!("send files to sandbox {name}: {error}")),
            _ => Ok(None),
        };
        match (connected, sent) {
            (Ok(conn), Ok(files)) => Ok(Held {
                provider,
                server,
                handle,
                run,
                conn,
                running: Mutex::new(HashMap::new()),
                tunnels: Mutex::new(Vec::new()),
                files,
                finished: Default::default(),
            }),
            (Err(error), _) | (_, Err(error)) => {
                server.tokens().revoke(&run);
                let _ = provider.destroy(&handle).await;
                Err(error)
            }
        }
    }

    /// A sandbox for a preparation: `image`, with `work` at `/work`, on this
    /// process's own node identity.
    pub async fn for_preparation(
        preparer: &Preparer,
        name: &str,
        image: &str,
        work: &str,
    ) -> Result<Held, String> {
        let server = crate::runner::protocol::node_server().map_err(|e| e.to_string())?;
        Held::start(
            preparer.provider.clone(),
            preparer.files.as_ref(),
            server,
            name,
            image,
            &[Mount::volume(work.to_string(), "/work", false)],
        )
        .await
    }

    /// After a preparation: on success, the sandbox snapshotted as
    /// `reference` with `labels`; the sandbox is gone afterwards either way.
    pub async fn conclude(
        self,
        prepared: Result<(), String>,
        reference: &str,
        labels: &[(String, String)],
    ) -> Result<String, String> {
        let snapshot = match prepared {
            Ok(()) => self.snapshot(reference, labels).await,
            Err(error) => Err(error),
        };
        self.finish().await;
        snapshot
    }

    /// Open `specs` on the sandbox's connection, for every command after.
    pub async fn open_tunnels(
        &self,
        specs: &[crate::runner::tunnels::TunnelSpec],
    ) -> Result<(), String> {
        let opened = crate::runner::tunnels::open(&self.conn, specs)
            .await
            .map_err(|error| error.to_string())?;
        self.tunnels.lock().unwrap().extend(opened);
        Ok(())
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub async fn snapshot(
        &self,
        reference: &str,
        labels: &[(String, String)],
    ) -> Result<String, String> {
        self.provider
            .snapshot_as(&self.handle, reference, labels)
            .await
            .map(|id| id.0)
            .map_err(|error| format!("snapshot {}: {error}", self.handle.0))
    }

    pub async fn finish(&self) {
        if self
            .finished
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.server.tokens().revoke(&self.run);
        drop(std::mem::take(&mut *self.tunnels.lock().unwrap()));
        if let Some(files) = &self.files {
            files.finish().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.conn.shutdown()).await;
        if let Err(error) = self.provider.destroy(&self.handle).await {
            tracing::warn!(sandbox = %self.handle.0, %error, "could not remove a preparation sandbox");
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.server.tokens().revoke(&self.run);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (provider, handle) = (self.provider.clone(), self.handle.clone());
        runtime.spawn(async move {
            let _ = provider.destroy(&handle).await;
        });
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    match (status.code, status.signal) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => -1,
    }
}

#[async_trait]
impl Runner for Held {
    async fn spawn(&self, _cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        Err(RunnerError::Other(
            "a preparation sandbox runs commands to completion only".into(),
        ))
    }

    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        use std::os::unix::process::ExitStatusExt as _;
        cmd.secret_env.validate()?;
        if cmd.argv.is_empty() {
            return Err(RunnerError::Other("empty argv".into()));
        }
        let env: BTreeMap<String, String> = cmd
            .plain_env()
            .chain(cmd.secret_env.iter())
            .cloned()
            .collect();
        let spec = SpawnSpec {
            argv: cmd.argv.clone(),
            env,
            cwd: Some(cmd.workdir.clone().unwrap_or_else(|| "/work".into()).into()),
            stdin: false,
            stdout: true,
            stderr: true,
            ..SpawnSpec::default()
        };
        let process = Arc::new(
            self.conn
                .spawn(spec)
                .await
                .map_err(|e| RunnerError::Other(format!("{}: {e}", cmd.name)))?,
        );
        self.running
            .lock()
            .unwrap()
            .insert(cmd.name.clone(), process.clone());
        let output = process.output().await;
        self.running.lock().unwrap().remove(&cmd.name);
        let output = output.map_err(|e| RunnerError::Other(format!("{}: {e}", cmd.name)))?;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw((exit_code(output.status) & 0xff) << 8),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        let process = self.running.lock().unwrap().remove(name);
        if let Some(process) = process {
            process
                .signal_group(libc::SIGKILL)
                .await
                .map_err(|e| RunnerError::Other(format!("{name}: {e}")))?;
        }
        Ok(())
    }

    fn capture_name(&self, name: &str) -> String {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> SnapshotInputs {
        SnapshotInputs {
            base: "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:aa".into(),
            image: "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:aa".into(),
            commit: "c1".into(),
            lockfiles: vec![Lockfile {
                path: "package-lock.json".into(),
                blob: "b1".into(),
            }],
            prepare: vec!["npm ci".into()],
            egress: vec!["registry.npmjs.org".into()],
        }
    }

    #[test]
    fn a_snapshot_key_moves_with_every_input_but_the_commit() {
        let key = inputs().key();
        let same_lockfiles = SnapshotInputs {
            commit: "c2".into(),
            ..inputs()
        };
        assert_eq!(
            same_lockfiles.key(),
            key,
            "a commit that leaves the lockfiles keeps it"
        );
        let changes: Vec<SnapshotInputs> = vec![
            SnapshotInputs {
                base: "base@sha256:bb".into(),
                ..inputs()
            },
            SnapshotInputs {
                image: "repo@sha256:cc".into(),
                ..inputs()
            },
            SnapshotInputs {
                lockfiles: vec![Lockfile {
                    path: "package-lock.json".into(),
                    blob: "b2".into(),
                }],
                ..inputs()
            },
            SnapshotInputs {
                lockfiles: vec![],
                ..inputs()
            },
            SnapshotInputs {
                prepare: vec!["npm ci".into(), "npm run build".into()],
                ..inputs()
            },
            SnapshotInputs {
                egress: vec![],
                ..inputs()
            },
        ];
        for changed in changes {
            assert_ne!(changed.key(), key, "{changed:?}");
        }
    }

    #[test]
    fn snapshots_are_named_for_this_node() {
        let repo = snapshot_repository(Path::new("/src/app"));
        assert!(
            repo.starts_with(&format!(
                "localhost/{}-snapshot-app-",
                crate::config::resource_prefix()
            )),
            "{repo}"
        );
        assert!(repo.starts_with(&snapshot_repo()));
    }

    #[test]
    fn harnesses_come_from_the_bundle() {
        assert_eq!(
            bundled_argv0("/usr/local/bin/claude").as_deref(),
            Some("/opt/tracon/bin/claude")
        );
        assert_eq!(
            bundled_argv0("/usr/local/bin/tracon-agent").as_deref(),
            Some("/opt/tracon/bin/tracon-agent")
        );
        assert_eq!(bundled_argv0("sh"), None);
        assert_eq!(bundled_argv0("/usr/local/bin/node"), None);
    }

    #[test]
    fn off_by_default_and_only_over_the_protocol() {
        let mut cfg = Config::default();
        assert!(!enabled(&cfg));
        assert_eq!(definitions(&cfg), "");
        cfg.runtime.podman.sandbox_images = true;
        assert!(!enabled(&cfg), "the setting stands on sandbox_protocol");
        assert!(crate::config::validate_sandbox(&cfg)
            .unwrap_err()
            .contains("sandbox_protocol"));
        cfg.runtime.podman.sandbox_protocol = true;
        crate::config::validate_sandbox(&cfg).unwrap();
        assert_eq!(enabled(&cfg), cfg!(target_os = "linux"));
        cfg.sandbox.release_url = Some("ftp://example".into());
        assert!(crate::config::validate_sandbox(&cfg).is_err());
    }

    #[test]
    fn the_base_is_the_releases_own_until_pinned() {
        let mut cfg = Config::default();
        assert_eq!(
            cfg.sandbox.base_image_ref(),
            format!(
                "ghcr.io/cosmicspork/tracon-sandbox-base:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        cfg.sandbox.base_image = Some("localhost/spike-base:1".into());
        assert_eq!(base_image(&cfg), "localhost/spike-base:1");
        let pin = (
            "localhost/spike-base:1".to_string(),
            "localhost/spike-base@sha256:ab".to_string(),
        );
        assert_eq!(
            pinned_or(cfg.sandbox.base_image_ref(), Some(&pin)),
            "localhost/spike-base@sha256:ab"
        );
        // A pin for another configured image is not this one's.
        assert_eq!(
            pinned_or("localhost/other:1".into(), Some(&pin)),
            "localhost/other:1"
        );
    }
}
