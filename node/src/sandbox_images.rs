//! Sandboxes without harness images, as every node runs them. Nothing is
//! built or layered for a harness:
//!
//! - **Images.** A session, check or preparation runs the stock base image
//!   (`[sandbox] base_image`, pulled for the host's architecture alone and
//!   pinned by that architecture's manifest digest when the boundary
//!   converges) or its repository's image. A repository's Dockerfile may
//!   stand on the base: `FROM ${TRACON_SANDBOX_BASE}` names the pinned base
//!   (`repo_image::build_base`), and a new base rebuilds it; one that does
//!   not, or an `image` of its own, is refused (`repo_image::refusal`).
//!   Older nodes' `tracon-harness-*` images and layered `-claude`/`-native`
//!   repository images are neither built nor used; `tracon gc --legacy`
//!   removes them.
//! - **Harnesses.** Claude Code and tracon-agent run from the harness bundle
//!   mounted at `/opt/tracon` (`crate::bundle`), which the node fetches for
//!   its own release. The protocol runner starts the bundle's copy of a
//!   harness wherever an adapter names the copy a harness image had.
//! - **Prepare once, then fork.** A repository's `prepare` runs on its
//!   default branch in one sandbox, with the dependency cache in the
//!   sandbox's own filesystem; the provider then snapshots the sandbox
//!   (`podman commit`, `container commit` on an Apple node, or on a
//!   pod-hosted node a build of the `/cache` and `/work` it read back onto
//!   the image the pod ran) and the repository's sessions and checks start
//!   from the snapshot. The
//!   preparation's tree is sent into the sandbox's own filesystem, never a
//!   volume, so the snapshot holds what `prepare` left in `/work` as well as
//!   `/cache`; the default branch's tracked files are removed before the
//!   commit, so a run's own tree, sent over it, is never mixed with another
//!   commit's files. On Podman the cache volumes sessions and candidates'
//!   preparations mount are seeded from the snapshot's `/cache`; on a pod or
//!   an Apple VM they use the snapshot's own, a preparation's run cache sent
//!   over it and only what that preparation added taken back
//!   (`runner::sandbox_files::Mode::Changed`).
//! - **At the snapshot's commit, nothing to prepare.** The snapshot records
//!   the default-branch commit it was prepared from ([`COMMIT_LABEL`] and the
//!   store row's `source_commit`). A session that starts from the current
//!   snapshot at that commit skips `prepare` ([`skips_prepare`]): what it
//!   would install is already under its `/work`, wherever the runtime sends
//!   the workspace over the image's own (`Backend::workspace_over_image`).
//!   At any other commit, or for a resumed workspace, `prepare` runs again
//!   in one sandbox from the snapshot, on its warm `/work` and `/cache`
//!   ([`Held::for_session`]). A candidate's preparation always runs: it is a
//!   tree under review, not a commit the snapshot was made from.
//!
//! A snapshot is keyed on what it was made from: the base image's digest,
//! the repository image's digest, the default branch's lockfiles (a
//! `Cargo.lock` by its third-party packages alone, [`lockfile_identity`]) and
//! the preparation recipe (its commands and egress). A change to any of them
//! leaves the snapshot unused and the next session prepares again; the
//! snapshot it supersedes is removed (`repo_image::supersede`, and
//! `tracon gc` for any left over). The commit is recorded, not keyed: a
//! commit that leaves the lockfiles alone keeps the snapshot current, and
//! only sessions at the commit it was made from skip `prepare`.
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

/// Bumped when what goes into a snapshot changes: 2 added the default
/// branch's build output under `/cache` ([`build_output`]); 3 keys a
/// `Cargo.lock` on its third-party packages ([`lockfile_identity`]).
const SCHEME: &str = "3";

/// On a snapshot image: the digest of the image it was made from.
pub const FROM_LABEL: &str = "io.tracon.snapshot.from";
/// On a snapshot image: the key it was made for.
pub const KEY_LABEL: &str = "io.tracon.snapshot.key";
/// On a snapshot image: the default-branch commit it was prepared from.
/// Recorded, not keyed: a session at this commit skips `prepare`.
pub const COMMIT_LABEL: &str = "io.tracon.snapshot.commit";

/// Where a snapshot holds what its preparation left beside the tree.
pub const PREPARED_TREE: &str = "/work";

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

/// Whether this node's own Podman converges and runs the stock base and the
/// bundle: a Podman node's, whose every sandbox runs on them.
pub fn enabled(cfg: &Config) -> bool {
    cfg.runtime.kind == crate::config::RuntimeKind::Podman
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

/// The stock base by the digest this process pinned it to, as a snapshot is
/// keyed on it: never the configured reference, which names the same image
/// by another string. Before the boundary has converged (a session carried
/// on right after a restart), or after the configured base changed, there is
/// no pin yet, and a key made then would supersede the snapshot made for the
/// pin and be superseded by the next one made after it.
fn snapshot_base(configured: &str, pin: Option<&(String, String)>) -> Result<String, String> {
    match pin {
        Some((named, pinned)) if named == configured => Ok(pinned.clone()),
        _ => Err(format!(
            "the sandbox base {configured} is not pinned yet; the node is still setting up its boundary"
        )),
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

/// Record the base as pinned: what the boundary's converge does, and a
/// test standing in for it.
#[doc(hidden)]
pub fn pin(configured: &str, pinned: &str) {
    *PINNED.write().unwrap() = Some((configured.to_string(), pinned.to_string()));
}

/// What a Podman boundary's converge brings in: the bundle for this
/// release, and the base image pulled and pinned. `Ok(true)` when it
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
    let (pinned, pulled) = pin_base_podman(&configured).await?;
    pin(&configured, &pinned);
    Ok(fetched || pulled)
}

/// `configured` as `name@sha256:…` of the manifest its registry has for
/// Linux on `arch`: an index's entry for it, or the manifest itself when the
/// reference names a single platform's image. Pinned by that and not by the
/// index, a base a new release carries over unchanged keeps its digest, and
/// with it every snapshot and repository image keyed on it. `None` for an
/// image only this host's store can hold.
pub async fn resolve_base(
    registry: &crate::boundary::registry::Registry,
    configured: &str,
    arch: &str,
) -> Result<Option<String>, String> {
    if crate::boundary::podman::local_only(configured) {
        return Ok(None);
    }
    registry
        .platform_identity(configured, arch)
        .await
        .map_err(|error| error.to_string())?
        .map(Some)
        .ok_or_else(|| format!("{configured} is not in its registry"))
}

/// The base pulled into Podman for this host's architecture alone, pinned
/// by that architecture's manifest; `true` when it pulled. A registry that
/// cannot be asked (offline, or a credential only Podman holds) leaves it to
/// Podman to pull as configured, and what it holds is pinned.
pub async fn pin_base_podman(configured: &str) -> Result<(String, bool), String> {
    use crate::boundary::podman::{image_identity, podman};
    async fn held(image: &str) -> bool {
        podman(&["image", "exists", image]).await.is_ok()
    }
    let arch = crate::boundary::registry::host_architecture();
    let platform = format!("linux/{arch}");
    let registry = crate::boundary::registry::Registry::new(None);
    let resolved = match resolve_base(&registry, configured, arch).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(image = %configured, %error, "could not resolve the sandbox base's {platform} manifest; pinning it as Podman holds it");
            None
        }
    };
    let Some(target) = resolved else {
        let mut pulled = false;
        if !held(configured).await {
            tracing::info!(image = %configured, "pulling the sandbox base image");
            podman(&["pull", "--quiet", "--platform", &platform, configured])
                .await
                .map_err(|error| format!("pull {configured}: {error}"))?;
            pulled = true;
        }
        let pinned = image_identity(configured)
            .await
            .ok_or_else(|| format!("{configured} has no digest to pin"))?;
        return Ok((pinned, pulled));
    };
    let mut pulled = false;
    if !held(&target).await {
        tracing::info!(image = %target, "pulling the sandbox base image for {platform}");
        podman(&["pull", "--quiet", "--platform", &platform, &target])
            .await
            .map_err(|error| format!("pull {target}: {error}"))?;
        pulled = true;
    }
    // Named as configured too, so a start that cannot reach the registry
    // still finds it.
    if !configured.contains('@') {
        podman(&["tag", &target, configured])
            .await
            .map_err(|error| format!("tag {target} as {configured}: {error}"))?;
    }
    Ok((target, pulled))
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
        .strip_prefix(&crate::repo_image::image_stem())
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

/// The bundle's copy of a harness an adapter names by its path
/// (`ClaudeAdapter::BINARY`, `NativeAdapter::BINARY`).
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

/// The environment every harness runs with that the stock base does not
/// set: Claude Code told not to update or report itself.
pub fn sandbox_env() -> Vec<(String, String)> {
    [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_TELEMETRY", "1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Every process's environment in a sandbox of a provider that does not
/// set it on the sandbox itself, for a `harness_id` harness: its home, the
/// allowlist proxy at the tunnel on `gateway_host`, which the harness reaches
/// directly, where it keeps its state, and [`sandbox_env`].
pub fn harness_env(gateway_host: &str, proxy_port: u16, harness_id: &str) -> Vec<(String, String)> {
    let proxy = format!("http://{gateway_host}:{proxy_port}");
    let no_proxy = crate::runner::no_proxy(gateway_host);
    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), HOME.into()),
        ("HTTPS_PROXY".into(), proxy.clone()),
        ("HTTP_PROXY".into(), proxy),
        ("NO_PROXY".into(), no_proxy.clone()),
        ("no_proxy".into(), no_proxy),
    ];
    let layout = crate::adapter::layout(harness_id);
    if !layout.env.is_empty() {
        env.push((layout.env.into(), format!("{HOME}/{}", layout.dir)));
    }
    env.extend(sandbox_env());
    env
}

/// A lockfile of the default branch, by what in it preparation depends on
/// ([`lockfile_identity`]).
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
    /// What the warm-up builds the default branch with after `prepare`, so
    /// the snapshot carries the build output checks start from: the
    /// repository's checks ([`build_output`]).
    pub build: Vec<String>,
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
        part("build");
        for command in &self.build {
            part(command);
        }
        part("build env");
        for (name, value) in crate::environment::prepared_check_env() {
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
        let object = format!("{commit}:{path}");
        if let Ok(blob) = crate::review::resolve(&dir, &object).await {
            let blob = match path {
                "Cargo.lock" => crate::review::blob(&dir, &object)
                    .await
                    .ok()
                    .and_then(|bytes| lockfile_identity(&bytes))
                    .unwrap_or(blob),
                _ => blob,
            };
            found.push(Lockfile {
                path: path.to_string(),
                blob,
            });
        }
    }
    found
}

/// What a `Cargo.lock` asks preparation to fetch: its packages from a
/// registry or a Git source, each by name, version, source and checksum.
/// The workspace's own packages (no `source`) are left out, so a release
/// that bumps their versions, or a commit that adds a path dependency
/// between them, keeps the snapshot: `cargo fetch` fetches the same, and a
/// check rebuilds the workspace's crates anyway. `None` when it does not
/// read as a lockfile, so the caller keys on its bytes instead.
pub fn lockfile_identity(bytes: &[u8]) -> Option<String> {
    let lock: toml::Table = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    let packages = lock.get("package")?.as_array()?;
    let field = |package: &toml::Table, name: &str| {
        package
            .get(name)
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let mut fetched: Vec<String> = packages
        .iter()
        .filter_map(toml::Value::as_table)
        .filter(|package| package.contains_key("source"))
        .map(|package| {
            ["name", "version", "source", "checksum"]
                .map(|name| field(package, name))
                .join("\0")
        })
        .collect();
    fetched.sort();
    let mut hash = Sha256::new();
    hash.update(b"cargo-dependencies\0");
    hash.update(field(&lock, "version").as_bytes());
    for package in &fetched {
        hash.update([0]);
        hash.update(package.as_bytes());
    }
    Some(format!("cargo:{}", hex::encode(hash.finalize())))
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
    let base = snapshot_base(
        &cfg.sandbox.base_image_ref(),
        PINNED.read().unwrap().as_ref(),
    )?;
    let (_, commit) = crate::repo_image::default_commit(repo).await?;
    Ok(SnapshotInputs {
        base,
        image,
        lockfiles: lockfiles(repo, &commit).await,
        commit,
        prepare: environment.prepare.clone(),
        egress: environment.egress.clone(),
        build: environment.checks.clone(),
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

/// Each repository's current prepared snapshot, as the store has it ready:
/// what a session there starts from, wherever the runtime keeps it.
pub fn ready_snapshots(store: &Store) -> Vec<String> {
    let repos: std::collections::BTreeSet<String> = store
        .repo_images()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.kind == SNAPSHOT)
        .map(|row| row.repo_path)
        .collect();
    repos
        .iter()
        .filter_map(|repo| store.ready_repo_image(repo, SNAPSHOT).ok().flatten())
        .map(|row| row.image)
        .filter(|image| !image.is_empty())
        .collect()
}

/// The current image of each repository the node built one for, whether
/// or not it also has a ready prepared snapshot: a session elsewhere starts
/// from it when the entry prepares nothing, and also when the snapshot the
/// store has ready is not the current one (its key moved with a new image
/// or lockfile, or its image is gone), or while the current one's template
/// builds.
pub fn current_repo_images(store: &Store) -> Vec<String> {
    let repos: std::collections::BTreeSet<String> = store
        .repo_images()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.kind == crate::repo_image::BASE)
        .map(|row| row.repo_path)
        .collect();
    repos
        .iter()
        .filter_map(|repo| {
            store
                .ready_repo_image(repo, crate::repo_image::BASE)
                .ok()
                .flatten()
        })
        .map(|row| row.image)
        .filter(|image| !image.is_empty())
        .collect()
}

/// Whether `image` is one of this node's prepared snapshots, by its name:
/// `localhost/<prefix>-snapshot-…` in a local store, or the same name under
/// a registry, where a pod-hosted node's builder pushes it.
pub fn is_snapshot(image: &str) -> bool {
    let name = image.split_once('@').map_or(image, |(name, _)| name);
    let last = name.rsplit('/').next().unwrap_or(name);
    last.starts_with(&format!("{}-snapshot-", crate::config::resource_prefix()))
}

/// The default-branch commit `snapshot` was prepared from, while it is still
/// `repo`'s newest ready snapshot; `None` for one superseded since, or made
/// before commits were recorded.
pub fn prepared_from(store: &Store, repo: &Path, snapshot: &str) -> Option<String> {
    store
        .ready_repo_image(&repo.to_string_lossy(), SNAPSHOT)
        .ok()
        .flatten()
        .filter(|row| row.image == snapshot && !row.source_commit.is_empty())
        .map(|row| row.source_commit)
}

/// Whether a session whose workspace starts at `base` may skip `prepare`
/// because the snapshot it starts from was prepared at that very commit. A
/// resumed workspace (`base` unknown) or another commit prepares again.
pub fn skips_prepare(prepared_at: Option<&str>, base: Option<&str>) -> bool {
    matches!((prepared_at, base), (Some(at), Some(base)) if !at.is_empty() && at == base)
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
    if environment.prepare.is_empty() {
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
    let seen = made_count(repo);
    warm_after(backend, cfg, store, repo, environment, secrets, seen).await
}

/// The last warm-up each repository finished: how many have finished, the
/// key the last was for, and what it failed with, if it did.
type Made = HashMap<std::path::PathBuf, (u64, String, Option<String>)>;

fn made() -> &'static Mutex<Made> {
    static MADE: std::sync::OnceLock<Mutex<Made>> = std::sync::OnceLock::new();
    MADE.get_or_init(Default::default)
}

/// How many of `repo`'s warm-ups have finished, read before waiting for its
/// warm-up lock, so [`warm_after`] can tell one that finished meanwhile.
pub(crate) fn made_count(repo: &Path) -> u64 {
    made()
        .lock()
        .unwrap()
        .get(repo)
        .map_or(0, |(count, _, _)| *count)
}

/// What the preparation of `key` that finished after `seen` failed with, if
/// that is how the last one ended.
fn failed_since(repo: &Path, key: &str, seen: u64) -> Option<String> {
    made()
        .lock()
        .unwrap()
        .get(repo)
        .filter(|(count, made_for, _)| *count != seen && made_for == key)
        .and_then(|(_, _, failed)| failed.clone())
}

fn record_made(repo: &Path, key: &str, failed: Option<&str>) {
    let mut made = made().lock().unwrap();
    let count = made.get(repo).map_or(0, |(count, _, _)| *count) + 1;
    made.insert(
        repo.to_path_buf(),
        (count, key.to_string(), failed.map(str::to_string)),
    );
}

/// [`warm`] for a caller that counted `seen` before waiting its turn: a
/// preparation of the same key that failed while it waited is its answer
/// too, rather than a second preparation of the same thing.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn warm_after(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    environment: &RepoEnvironment,
    secrets: &crate::environment::PrepareSecrets,
    seen: u64,
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
    if let Some(error) = failed_since(repo, &key, seen) {
        return Err(error);
    }
    let made = make_snapshot(
        backend,
        cfg,
        store,
        repo,
        environment,
        secrets,
        &inputs,
        &key,
        &preparer,
    )
    .await;
    record_made(repo, &key, made.as_ref().err().map(String::as_str));
    made
}

#[allow(clippy::too_many_arguments)]
async fn make_snapshot(
    backend: &dyn Backend,
    cfg: &Config,
    store: &Store,
    repo: &Path,
    environment: &RepoEnvironment,
    secrets: &crate::environment::PrepareSecrets,
    inputs: &SnapshotInputs,
    key: &str,
    preparer: &Preparer,
) -> Result<Option<String>, String> {
    let key = key.to_string();
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
        // Kept by the preparer's store, so it is sent into the sandbox's own
        // filesystem and the commit carries what `prepare` left in it.
        match &preparer.files {
            Some(files) => files.import(&work, &tree.root).map_err(|e| e.to_string())?,
            None => backend
                .import_writable(&work, &tree.root)
                .await
                .map_err(|error| error.to_string())?,
        }
        let tracked = tracked_files(&tree.root);
        let reference = crate::repo_image::build_tag(&snapshot_repository(repo), &key, &row);
        let name = crate::config::resource_name(&format!("w-{}", &id[..12]));
        let mut held = Held::for_preparation(preparer, &name, &inputs.image, &work).await;
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
                // The default branch built, on what `prepare` installed, so
                // the snapshot's `/cache` holds the output its checks start
                // from. A build that fails still leaves what it compiled.
                let built = match &prepared {
                    Ok(()) => build_output(&held, environment, &name, repo).await,
                    Err(_) => false,
                };
                let prepared = match (prepared, &tracked) {
                    (Ok(()), Ok(tracked)) => held.remove_tracked(tracked).await,
                    (Ok(()), Err(error)) => Err(error.clone()),
                    (failed, _) => failed,
                };
                let labels = [
                    (FROM_LABEL.to_string(), inputs.image.clone()),
                    (KEY_LABEL.to_string(), key.clone()),
                    (COMMIT_LABEL.to_string(), inputs.commit.clone()),
                ];
                let snapshot = match prepared {
                    Ok(()) => match held.snapshot(&reference, &labels).await {
                        // A runtime that reads the trees back to build the
                        // snapshot (a pod's) may have no room for the build
                        // output: the dependencies alone are still worth it.
                        Err(error) if built => {
                            tracing::warn!(
                                repo = %repo.display(),
                                %error,
                                "the snapshot with the default branch's build output was not made; \
                                 making it without"
                            );
                            match drop_build_output(&held, &name).await {
                                Ok(()) => held.snapshot(&reference, &labels).await,
                                Err(dropped) => Err(format!("{error}; {dropped}")),
                            }
                        }
                        made => made,
                    },
                    Err(error) => Err(error),
                };
                held.finish().await;
                snapshot
            }
            Err(error) => Err(error),
        };
        match &preparer.files {
            Some(files) => {
                let _ = files.remove(&work);
            }
            None => {
                let _ = backend.remove_volume(&work).await;
            }
        }
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
            // Its DigitalOcean template too, where sessions may run there.
            crate::provider::digitalocean::runtime::DoRuntime::prepare_snapshot_if_running(
                repo, &pinned,
            );
            Ok(Some(pinned))
        }
        Err(error) => {
            let _ = store.finish_repo_image(&row, Err(&error), "", &[]);
            Err(error)
        }
    }
}

/// Build the default branch in a warm-up's sandbox, after its `prepare`,
/// so the snapshot carries the build output under `/cache`
/// (`environment::build_env`: Cargo's target directory, Go's build cache)
/// that a review's checks start from, each over a writable layer of its own
/// (`review::checks::check_cache`). What builds it is the repository's own
/// checks, run as a check runs (`environment::prepared_check_env`: nothing
/// fetched, no egress grant, nothing lent), so the output is the very one
/// they build; their outcome is not evidence of anything and is only
/// logged. It is made with the snapshot, so a new image, base, lockfile or
/// recipe rebuilds it, and it goes with the snapshot it is in. Whether any
/// build ran.
async fn build_output(held: &Held, environment: &RepoEnvironment, name: &str, repo: &Path) -> bool {
    use crate::review::checks::{until_stopped, RunToCompletion};
    let command = |step: &str, script: &str| RunnerCommand {
        argv: crate::review::output::merged_shell(script),
        env: crate::environment::prepared_check_env(),
        workdir: Some(PREPARED_TREE.into()),
        name: format!("{name}-{step}"),
        image: Some(environment.image.clone()),
        labels: crate::runner::RunLabels {
            repo: environment.repo.clone(),
            ..crate::runner::RunLabels::of(crate::runner::RunKind::Prepare)
        },
        ..Default::default()
    };
    // Made by the sandbox's user whether or not anything builds into them,
    // so a check's writable layer over them is its own to write in.
    let made = held
        .run_capture(command("b", &make_build_dirs()))
        .await
        .is_ok_and(|output| output.status.success());
    if !made {
        tracing::warn!(repo = %repo.display(), "no build output: its directories could not be made");
        return false;
    }
    let mut built = false;
    for (step, check) in environment.checks.iter().enumerate() {
        let cmd = command(&format!("b{step}"), check);
        let running = cmd.name.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(environment.timeout_secs);
        let began = std::time::Instant::now();
        let ran = until_stopped(
            held.run_capture(cmd),
            deadline,
            &RunToCompletion,
            Some((held as &dyn Runner, running.as_str())),
        )
        .await;
        built = true;
        let took = began.elapsed();
        match ran {
            Ok(Ok(output)) => tracing::info!(
                repo = %repo.display(),
                %check,
                exit = output.status.code().unwrap_or(-1),
                ?took,
                "built the default branch for its checks"
            ),
            Ok(Err(error)) => {
                tracing::warn!(repo = %repo.display(), %check, %error, "the default branch's build did not run")
            }
            Err(_) => tracing::warn!(
                repo = %repo.display(),
                %check,
                ?took,
                "the default branch's build timed out; the snapshot keeps what it built"
            ),
        }
    }
    built
}

/// The shell step that makes [`crate::environment::build_env`]'s directories.
fn make_build_dirs() -> String {
    let dirs: Vec<String> = crate::environment::build_env()
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    format!("mkdir -p {}", dirs.join(" "))
}

/// Empty the build output directories again, for a snapshot that could not
/// carry them.
async fn drop_build_output(held: &Held, name: &str) -> Result<(), String> {
    let dirs: Vec<String> = crate::environment::build_env()
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    let script = format!(
        "rm -rf {dirs} && {make}",
        dirs = dirs.join(" "),
        make = make_build_dirs()
    );
    let output = held
        .run_capture(RunnerCommand {
            argv: crate::review::output::merged_shell(&script),
            workdir: Some(PREPARED_TREE.into()),
            name: format!("{name}-drop"),
            ..Default::default()
        })
        .await
        .map_err(|error| format!("remove the build output: {error}"))?;
    match output.status.success() {
        true => Ok(()),
        false => Err(format!(
            "remove the build output: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

/// Every file and link under `root` (a commit's tree as the node staged it),
/// relative to it.
fn tracked_files(root: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(root.join(dir))? {
            let entry = entry?;
            let path = dir.join(entry.file_name());
            match entry.file_type()?.is_dir() {
                true => walk(root, &path, out)?,
                false => out.push(path),
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out)
        .map_err(|error| format!("list the default branch's files: {error}"))?;
    Ok(out)
}

/// How a [`Held`] sandbox is made, beyond its image and mounts.
#[derive(Debug, Clone)]
pub struct Making {
    /// How its trees come back at the end ([`Held::start_as`]).
    pub kind: crate::runner::RunKind,
    /// What its provider is told it is.
    pub labels: crate::runner::RunLabels,
    /// What a provider elsewhere sizes it by.
    pub resources: Resources,
    /// How long its token may wait to be redeemed, for a provider that may
    /// keep a copy of how it was delivered; `None` for as long as it runs.
    pub token_window: Option<Duration>,
}

/// A run of its own for a sandbox named `name`, and the bootstrap that
/// carries a token issued for that run alone.
fn issue(server: &SandboxServer, name: &str, window: Option<Duration>) -> (String, Bootstrap) {
    let run = format!("{name}:{}", uuid::Uuid::now_v7().simple());
    let token = match window {
        Some(window) => server.tokens().issue_within(run.clone(), window),
        None => server.tokens().issue(run.clone()),
    };
    let bootstrap = Bootstrap {
        run: run.clone(),
        cert_pin: server.pin().to_string(),
        token,
    };
    (run, bootstrap)
}

/// One sandbox held open for several commands: a preparation's steps all
/// run in it, so what each leaves in the sandbox's own filesystem is there
/// for the next and for the snapshot. Its mounts are fixed when it is made;
/// a command's own are not mounted again.
pub struct Held {
    provider: Arc<dyn SandboxProvider>,
    server: Arc<SandboxServer>,
    handle: Handle,
    /// What it was made from, which a snapshot built rather than committed
    /// stands on.
    image: String,
    run: String,
    conn: SandboxConn,
    running: Mutex<HashMap<String, Arc<Process>>>,
    /// Open for as long as the sandbox is: on a node whose sandboxes have
    /// no network, the way to the gateways.
    tunnels: Mutex<Vec<tracon_sandbox::node::Tunnel>>,
    /// The trees sent in where the runtime mounts no volume.
    files: Option<crate::runner::sandbox_files::Attached>,
    /// Where every command runs, whatever directory it names: where the
    /// sandbox holds its tree, when that is not `/work`.
    workdir: Option<String>,
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
        Held::start_as(
            provider,
            files,
            server,
            name,
            image,
            mounts,
            crate::runner::RunKind::Check,
        )
        .await
    }

    /// [`Held::start`], its trees taken back at the end as a run of `kind`
    /// takes them: a check's never, a preparation's whole.
    pub async fn start_as(
        provider: Arc<dyn SandboxProvider>,
        files: Option<&crate::runner::sandbox_files::FilesStore>,
        server: Arc<SandboxServer>,
        name: &str,
        image: &str,
        mounts: &[Mount],
        kind: crate::runner::RunKind,
    ) -> Result<Held, String> {
        Held::start_making(
            provider,
            files,
            server,
            name,
            image,
            mounts,
            &Making {
                kind,
                labels: crate::runner::RunLabels::of(crate::runner::RunKind::Prepare),
                resources: Resources::default(),
                token_window: None,
            },
        )
        .await
    }

    /// [`Held::start_as`], made as `making` says.
    pub async fn start_making(
        provider: Arc<dyn SandboxProvider>,
        files: Option<&crate::runner::sandbox_files::FilesStore>,
        server: Arc<SandboxServer>,
        name: &str,
        image: &str,
        mounts: &[Mount],
        making: &Making,
    ) -> Result<Held, String> {
        let kind = making.kind;
        let (mounts, plan) = match files {
            // A tree sent in lands in the image's own filesystem rather than
            // on storage of the sandbox's own: what the image holds there
            // (a snapshot's prepared `/work`) is under it, and a commit of
            // the sandbox carries what was done to it.
            Some(store) => {
                let (provided, plan) = store
                    .plan(mounts)
                    .map_err(|e| format!("sandbox {name}: {e}"))?;
                let provided = provided.into_iter().filter(|m| !m.is_local()).collect();
                (provided, plan)
            }
            None => (mounts.to_vec(), None),
        };
        let (run, bootstrap) = issue(&server, name, making.token_window);
        let created = {
            let _images = crate::boundary::reconcile::images_in_use().await;
            provider
                .create(&CreateSpec {
                    name: Some(name),
                    image,
                    resources: &making.resources,
                    mounts: &mounts,
                    bootstrap: &bootstrap,
                    labels: &making.labels,
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
        // Taken back as a run of `kind` would be: a warm-up's (a check's)
        // never, since what it leaves is in the sandbox's own filesystem,
        // for the snapshot.
        let sent = match (&connected, plan) {
            (Ok(conn), Some(plan)) => plan
                .attach(conn, name, kind)
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
                image: image.to_string(),
                run,
                conn,
                running: Mutex::new(HashMap::new()),
                tunnels: Mutex::new(Vec::new()),
                files,
                workdir: None,
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

    /// A sandbox from `snapshot` in which a session's own `prepare` runs,
    /// every step in it, on the session's `workspace` at `/work` and the
    /// snapshot's warm `/cache`. Nothing of the node's cache is sent in or
    /// taken back; the workspace is taken back whole at [`Held::take_back`].
    pub async fn for_session(
        preparer: &Preparer,
        name: &str,
        snapshot: &str,
        workspace: &str,
    ) -> Result<Held, String> {
        let server = crate::runner::protocol::node_server().map_err(|e| e.to_string())?;
        let held = Held::start_as(
            preparer.provider.clone(),
            preparer.files.as_ref(),
            server,
            name,
            snapshot,
            &[Mount::volume(workspace.to_string(), "/work", false)],
            crate::runner::RunKind::Prepare,
        )
        .await?;
        if !preparer.tunnels.is_empty() {
            if let Err(error) = held.open_tunnels(&preparer.tunnels).await {
                held.finish().await;
                return Err(error);
            }
        }
        Ok(held)
    }

    /// Take the sandbox's trees back, once, and remove it: a preparation
    /// whose tree did not come back has prepared nothing.
    pub async fn take_back(self) -> Result<(), String> {
        let taken = match &self.files {
            Some(files) => files
                .conclude()
                .await
                .map_err(|error| format!("take the prepared tree back: {error}")),
            None => Ok(()),
        };
        self.finish().await;
        taken
    }

    /// Remove `tracked` (paths relative to `/work`) from the sandbox's
    /// `/work`, leaving what a preparation added beside them: a run's own
    /// tree is sent over a snapshot's `/work`, and a file the default branch
    /// tracks that the run's commit deleted must not come back with it.
    pub async fn remove_tracked(&self, tracked: &[std::path::PathBuf]) -> Result<(), String> {
        use std::os::unix::ffi::OsStrExt as _;
        if tracked.is_empty() {
            return Ok(());
        }
        const LISTED: &str = "/tmp/tracon-tracked";
        let list = tempfile::tempdir().map_err(|e| e.to_string())?;
        let mut names = Vec::new();
        for path in tracked {
            names.extend_from_slice(path.as_os_str().as_bytes());
            names.push(0);
        }
        std::fs::write(list.path().join("list"), names).map_err(|e| e.to_string())?;
        self.conn
            .import_dir(
                list.path(),
                LISTED,
                tracon_sandbox::protocol::Exclusions::None,
            )
            .await
            .map_err(|error| format!("send the tracked files' list: {error}"))?;
        let script = format!("cd /work && xargs -0 rm -f -- < {LISTED}/list && rm -rf {LISTED}");
        let output = self
            .conn
            .spawn(SpawnSpec {
                argv: vec!["sh".into(), "-c".into(), script],
                stdout: true,
                stderr: true,
                ..SpawnSpec::default()
            })
            .await
            .map_err(|e| format!("remove the tracked files: {e}"))?
            .output()
            .await
            .map_err(|e| format!("remove the tracked files: {e}"))?;
        match exit_code(output.status) {
            0 => Ok(()),
            code => Err(format!(
                "remove the tracked files (exit {code}): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
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

    /// Run every command in `workdir`, where this sandbox holds its tree.
    pub fn at(mut self, workdir: &str) -> Self {
        self.workdir = Some(workdir.to_string());
        self
    }

    /// The run its token was issued for.
    pub fn run(&self) -> &str {
        &self.run
    }

    /// Its isolation as its provider reads it back, held to `requirements`.
    pub async fn verify(&self, requirements: &crate::provider::Requirements) -> Result<(), String> {
        self.provider
            .verify_isolation(&self.handle)
            .await
            .map_err(|error| error.to_string())
            .and_then(|caps| requirements.check(&caps))
    }

    /// What it runs, as its provider confirms it.
    pub async fn identity(&self) -> Result<crate::provider::ImageIdentity, String> {
        self.provider
            .image_identity(&self.handle)
            .await
            .map_err(|error| format!("{}'s image: {error}", self.handle.0))
    }

    /// Close its tunnels: nothing in it reaches the node's gateways after.
    pub fn close_tunnels(&self) {
        drop(std::mem::take(&mut *self.tunnels.lock().unwrap()));
    }

    /// A checkpoint of it, as its provider takes one, to fork from.
    pub async fn checkpoint(&self) -> Result<crate::provider::SnapshotId, String> {
        self.provider
            .snapshot(&self.handle)
            .await
            .map_err(|error| format!("checkpoint {}: {error}", self.handle.0))
    }

    /// End its run on the node's side: its connection, and any redial of
    /// it, are refused from now on. A fork of it holds a copy of its
    /// supervisor, which must never come back as this run.
    pub fn end_run(&self) {
        self.server.end_run(&self.run);
    }

    /// Sandboxes forked from `snapshot`, one per name, each with a run and
    /// token of its own, connected: what each came to, in `names`' order.
    /// Nothing is sent into them and no tunnel is opened: they start from
    /// what the snapshot holds, with no way to the node's gateways. At most
    /// the provider's [`SandboxProvider::fork_batch`] are forked in one call,
    /// so more take several.
    pub async fn forked(
        provider: Arc<dyn SandboxProvider>,
        server: Arc<SandboxServer>,
        snapshot: &crate::provider::SnapshotId,
        image: &str,
        names: &[String],
        making: &Making,
    ) -> Vec<Result<Held, String>> {
        let mut children = Vec::with_capacity(names.len());
        for batch in names.chunks(provider.fork_batch().max(1)) {
            let issued: Vec<(String, Bootstrap)> = batch
                .iter()
                .map(|name| issue(&server, name, making.token_window))
                .collect();
            let bootstraps: Vec<Bootstrap> = issued.iter().map(|(_, b)| b.clone()).collect();
            let forked = provider
                .fork_many(snapshot, &making.resources, &bootstraps)
                .await;
            let handles = match forked {
                Ok(handles) => handles,
                Err(error) => {
                    for (run, _) in &issued {
                        server.tokens().revoke(run);
                    }
                    let why = format!("fork {}: {error}", snapshot.0);
                    children.extend(batch.iter().map(|_| Err(why.clone())));
                    continue;
                }
            };
            let connected =
                handles
                    .into_iter()
                    .zip(issued)
                    .zip(batch)
                    .map(|((handle, (run, _)), name)| {
                        let (provider, server) = (provider.clone(), server.clone());
                        async move {
                            let handle = match handle {
                                Ok(handle) => handle,
                                Err(error) => {
                                    server.tokens().revoke(&run);
                                    return Err(format!("fork {name}: {error}"));
                                }
                            };
                            match provider
                                .connect(&handle, &server, crate::runner::protocol::CONNECT_WITHIN)
                                .await
                            {
                                Ok(conn) => Ok(Held {
                                    provider,
                                    server,
                                    handle,
                                    image: image.to_string(),
                                    run,
                                    conn,
                                    running: Mutex::new(HashMap::new()),
                                    tunnels: Mutex::new(Vec::new()),
                                    files: None,
                                    workdir: None,
                                    finished: Default::default(),
                                }),
                                Err(error) => {
                                    server.tokens().revoke(&run);
                                    let _ = provider.destroy(&handle).await;
                                    Err(format!("connect to sandbox {name}: {error}"))
                                }
                            }
                        }
                    });
            children.extend(futures_util::future::join_all(connected).await);
        }
        children
    }

    pub async fn snapshot(
        &self,
        reference: &str,
        labels: &[(String, String)],
    ) -> Result<String, String> {
        self.provider
            .snapshot_from(
                &self.handle,
                &self.conn,
                &self.image,
                reference,
                &crate::image_gc::with_owner(labels),
            )
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
            cwd: Some(
                self.workdir
                    .clone()
                    .or_else(|| cmd.workdir.clone())
                    .unwrap_or_else(|| "/work".into())
                    .into(),
            ),
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
            build: vec!["npm test".into()],
        }
    }

    /// Two releases whose indexes differ (their attestations do) over the
    /// same image pin the same base, so a snapshot keyed on it is kept.
    #[tokio::test]
    async fn an_unchanged_platform_manifest_keeps_the_key_across_indexes() {
        use crate::boundary::registry::tests::{docker_config, oci_index, serve, AMD64, ARM64};
        let (fake, host) = serve().await;
        let older = fake.put_manifest("ns/base", "0.45.0", oci_index('a').as_bytes());
        let newer = fake.put_manifest("ns/base", "0.46.0", oci_index('b').as_bytes());
        assert_ne!(older, newer);
        let config = docker_config(&host);
        let registry = crate::boundary::registry::Registry::new(Some(config.path().into()))
            .for_test("unused", "unused");
        let resolve = |tag: &'static str, arch: &'static str| {
            let registry = registry.clone();
            let configured = format!("{host}/ns/base:{tag}");
            async move {
                resolve_base(&registry, &configured, arch)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let (base_45, base_46) = (
            resolve("0.45.0", "amd64").await,
            resolve("0.46.0", "amd64").await,
        );
        assert_eq!(base_45, format!("{host}/ns/base@{AMD64}"));
        assert_eq!(base_45, base_46);
        assert_eq!(
            resolve("0.46.0", "arm64").await,
            format!("{host}/ns/base@{ARM64}")
        );
        let keyed = |base: &str| {
            SnapshotInputs {
                base: base.to_string(),
                image: base.to_string(),
                ..inputs()
            }
            .key()
        };
        assert_eq!(keyed(&base_45), keyed(&base_46));
        // Pinned by the index, as before, each release had a key of its own.
        assert_ne!(
            keyed(&format!("{host}/ns/base@{older}")),
            keyed(&format!("{host}/ns/base@{newer}"))
        );
        // An image only this host can hold is not asked of any registry.
        assert_eq!(
            resolve_base(&registry, "localhost/spike-base:1", "amd64")
                .await
                .unwrap(),
            None
        );
    }

    /// A lockfile as a workspace of two crates and one registry dependency
    /// writes it, with the workspace at `ours` and the dependency at `theirs`.
    fn cargo_lock(ours: &str, theirs: &str, internal: bool) -> Vec<u8> {
        let deps = if internal {
            "\n dependencies = [\"a\"]"
        } else {
            ""
        };
        format!(
            "version = 4\n\n\
             [[package]]\nname = \"a\"\nversion = \"{ours}\"\ndependencies = [\"serde\"]\n\n\
             [[package]]\nname = \"b\"\nversion = \"{ours}\"{deps}\n\n\
             [[package]]\nname = \"serde\"\nversion = \"{theirs}\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
             checksum = \"{theirs}-sum\"\n"
        )
        .into_bytes()
    }

    /// A release or a new path dependency between the workspace's own crates
    /// keeps the snapshot; a registry package that moves does not.
    #[test]
    fn a_cargo_lock_is_keyed_on_what_it_fetches() {
        let before = lockfile_identity(&cargo_lock("0.1.0", "1.0.0", false)).unwrap();
        assert!(before.starts_with("cargo:"));
        assert_eq!(
            lockfile_identity(&cargo_lock("0.2.0", "1.0.0", false)).unwrap(),
            before,
            "a workspace version bump"
        );
        assert_eq!(
            lockfile_identity(&cargo_lock("0.1.0", "1.0.0", true)).unwrap(),
            before,
            "a path dependency between workspace crates"
        );
        assert_ne!(
            lockfile_identity(&cargo_lock("0.1.0", "1.0.1", false)).unwrap(),
            before,
            "a registry package update"
        );
        assert_eq!(lockfile_identity(b"not a lockfile ["), None);
        assert_eq!(lockfile_identity(b"version = 4\n"), None);
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
            // The build its output is made by: the checks.
            SnapshotInputs {
                build: vec!["npm test".into(), "npm run lint".into()],
                ..inputs()
            },
            SnapshotInputs {
                build: vec![],
                ..inputs()
            },
        ];
        for changed in changes {
            assert_ne!(changed.key(), key, "{changed:?}");
        }
    }

    /// A session carried on right after a restart, before the boundary has
    /// pinned the base, makes no snapshot under the configured name: its key
    /// would differ from the one `repo build` made, and from the one the next
    /// session makes once the base is pinned.
    #[test]
    fn a_snapshot_is_keyed_on_the_pinned_base_only() {
        let configured = "ghcr.io/cosmicspork/tracon-sandbox-base:0.48.0";
        let pinned = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:aa";
        let pin = (configured.to_string(), pinned.to_string());
        assert!(snapshot_base(configured, None).is_err());
        let other = (
            "ghcr.io/cosmicspork/tracon-sandbox-base:0.47.0".to_string(),
            pinned.to_string(),
        );
        assert!(snapshot_base(configured, Some(&other)).is_err());
        let base = snapshot_base(configured, Some(&pin)).unwrap();
        assert_eq!(base, pinned);
        let keyed = |base: &str| {
            SnapshotInputs {
                base: base.to_string(),
                ..inputs()
            }
            .key()
        };
        assert_eq!(keyed(&base), inputs().key());
        assert_ne!(keyed(configured), keyed(&base));
    }

    /// A warm-up that waited while another prepared the same key takes that
    /// one's failure rather than preparing again; one that started after it,
    /// or for another key, prepares.
    #[test]
    fn a_failed_preparation_is_the_answer_to_whoever_waited_for_it() {
        let repo = Path::new("/src/a3-waited");
        let before = made_count(repo);
        record_made(repo, "k1", Some("npm ci exited 1"));
        assert_eq!(
            failed_since(repo, "k1", before).as_deref(),
            Some("npm ci exited 1")
        );
        assert_eq!(failed_since(repo, "k2", before), None);
        assert_eq!(failed_since(repo, "k1", made_count(repo)), None);
        let before = made_count(repo);
        record_made(repo, "k1", None);
        assert_eq!(failed_since(repo, "k1", before), None);
    }

    /// Only a session at the very commit its snapshot was prepared from
    /// skips `prepare`; another commit, a resumed workspace whose start is
    /// unknown, or a snapshot with no recorded commit prepares again.
    #[test]
    fn prepare_is_skipped_only_at_the_snapshots_commit() {
        assert!(skips_prepare(Some("c1"), Some("c1")));
        assert!(!skips_prepare(Some("c1"), Some("c2")));
        assert!(!skips_prepare(Some("c1"), None));
        assert!(!skips_prepare(None, Some("c1")));
        assert!(!skips_prepare(Some(""), Some("")));
    }

    /// The commit is read from the repository's newest ready snapshot, and
    /// only while that is the one a session starts from: a snapshot a newer
    /// one superseded (a stale one) says nothing.
    #[test]
    fn the_commit_is_the_current_snapshots_own() {
        let store = Store::open_in_memory().unwrap();
        let repo = Path::new("/src/b3-app");
        let made = |key: &str, commit: &str, image: &str| {
            let row = store
                .start_repo_image(&RepoImageStart {
                    repo_path: &repo.to_string_lossy(),
                    kind: SNAPSHOT,
                    recipe_hash: key,
                    source_ref: "",
                    source_commit: commit,
                    parent_image: "base@sha256:aa",
                })
                .unwrap();
            store.finish_repo_image(&row, Ok(image), "", &[]).unwrap();
        };
        assert_eq!(prepared_from(&store, repo, "snap@sha256:01"), None);
        made("k1", "c1", "snap@sha256:01");
        assert_eq!(
            prepared_from(&store, repo, "snap@sha256:01").as_deref(),
            Some("c1")
        );
        assert_eq!(
            prepared_from(&store, Path::new("/src/other"), "snap@sha256:01"),
            None
        );
        std::thread::sleep(Duration::from_millis(5));
        made("k2", "c2", "snap@sha256:02");
        assert_eq!(prepared_from(&store, repo, "snap@sha256:01"), None, "stale");
        assert_eq!(
            prepared_from(&store, repo, "snap@sha256:02").as_deref(),
            Some("c2")
        );
        std::thread::sleep(Duration::from_millis(5));
        made("k3", "", "snap@sha256:03");
        assert_eq!(prepared_from(&store, repo, "snap@sha256:03"), None);
    }

    #[test]
    fn a_snapshot_is_known_by_its_name() {
        let snapshot = format!("{}@sha256:aa", snapshot_repository(Path::new("/src/app")));
        assert!(is_snapshot(&snapshot));
        assert!(!is_snapshot("localhost/tracon-repo-app-0123:1"));
        assert!(!is_snapshot(
            "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:aa"
        ));
        assert!(!is_snapshot(""));
        // Pushed by a pod-hosted node's builder, under its registry.
        assert!(is_snapshot(
            "registry.digitalocean.com/cs/tracon-snapshot-app-0123:abc"
        ));
        assert!(is_snapshot(
            "registry.digitalocean.com/cs/tracon-snapshot-app-0123@sha256:aa"
        ));
        assert!(!is_snapshot(
            "registry.digitalocean.com/cs/tracon-repo-app-0123@sha256:aa"
        ));
        // Another node's, whose prefix is not this one's.
        assert!(!is_snapshot("localhost/spike-snapshot-app-0123:1"));
    }

    /// What is removed from a snapshot's `/work`: every file and link of the
    /// commit's tree, never a directory, which may also hold what `prepare`
    /// installed.
    #[test]
    fn the_tracked_files_are_every_file_and_link_of_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("src/deep/a.js"), "").unwrap();
        std::os::unix::fs::symlink("deep/a.js", root.join("src/link")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let mut files = tracked_files(root).unwrap();
        files.sort();
        assert_eq!(
            files,
            ["package.json", "src/deep/a.js", "src/link"]
                .map(std::path::PathBuf::from)
                .to_vec()
        );
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
    fn every_podman_node_runs_the_sandbox_images() {
        let mut cfg = Config::default();
        assert!(enabled(&cfg));
        assert!(definitions(&cfg).starts_with("sandbox-images"));
        crate::config::validate_sandbox(&cfg).unwrap();
        cfg.sandbox.release_url = Some("ftp://example".into());
        assert!(crate::config::validate_sandbox(&cfg).is_err());
        let mut kube = Config::default();
        kube.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        assert!(!enabled(&kube));
        assert_eq!(definitions(&kube), "");
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
