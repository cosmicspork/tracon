//! The Mac boundary: every harness, check and preparation in a lightweight
//! VM of its own, made with Apple's `container` (`provider::apple`), over the
//! sandbox protocol. No Podman is involved.
//!
//! - **Network.** A sandbox has `--network none` and no resolver; its one
//!   way out is the protocol's published socket. The MCP and model gateway,
//!   the allowlist proxy and the per-client egress proxy are tunnels over
//!   that connection to the node's own listeners, on the sandbox's loopback.
//!   `container` has no way to add a hosts entry, so the harness reaches the
//!   gateway at `127.0.0.1` rather than by name.
//! - **Trees.** Every volume is a directory in the node's files store,
//!   sent into a sandbox when it starts and taken back over the files
//!   channel, as on a Kubernetes node over the protocol.
//! - **Images.** Harnesses come from the bundle, mounted read-only, on the
//!   stock base image or a repository's image built on it with
//!   `container build` (BuildKit in a builder VM of `container`'s own), and
//!   kept in `container`'s image store.

pub mod checks;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    Backend, BoundaryError, BoundaryReport, BuildFailure, BuiltImage, EgressGrant, ImageBuild,
    ImageBuilder, Presence,
};
use crate::config::Config;
use crate::gateway::proxy::{GrantSpec, Grants};
use crate::provider::apple::{AppleOptions, AppleProvider, CliError, ContainerCli, SystemCli};
use crate::runner::sandbox_files::{self as files, FilesStore};
use crate::runner::{Mount, Runner, RunnerCommand, RunnerError, Spawned};

/// Where a harness reaches the node's gateways: the sandbox's own loopback,
/// where the tunnels listen.
pub const GATEWAY_HOST: &str = "127.0.0.1";

/// Where a build's steps reach their grant, on the build sandbox's loopback.
const BUILD_PROXY_PORT: u16 = 3128;

/// The only platform a Mac's VMs run: an index's other variants are never
/// pulled.
const VM_ARCH: &str = "arm64";
const VM_PLATFORM: &str = "linux/arm64";

pub struct AppleBackend {
    cfg: Config,
    cli: Arc<dyn ContainerCli>,
    /// Every live per-client grant, served by the node on its egress
    /// listener and reached through each sandbox's egress tunnel.
    grants: Grants,
}

impl AppleBackend {
    pub fn new(cfg: &Config) -> Self {
        let cli = SystemCli {
            bin: cfg.runtime.apple.container.clone(),
        };
        Self::with_cli(cfg, Arc::new(cli))
    }

    pub fn with_cli(cfg: &Config, cli: Arc<dyn ContainerCli>) -> Self {
        AppleBackend {
            cfg: cfg.clone(),
            cli,
            grants: Grants::default(),
        }
    }

    async fn container(&self, args: &[&str]) -> Result<String, CliError> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.cli.run(&args).await
    }

    /// Where the trees a sandbox is sent are kept: every volume a run names,
    /// except the harness state every session once shared, which no sandbox
    /// is handed (a session gets its own, `materialize::own_state`).
    pub fn files_store(&self) -> FilesStore {
        FilesStore::for_node()
            .withholding(crate::session::materialize::harness_state_volume())
            .keeping_every_volume()
    }

    /// Every sandbox's environment for `harness_id`: what a harness image
    /// would have set, its proxy at the tunnel, and where it keeps state.
    fn sandbox_env(&self, harness_id: &str) -> Vec<(String, String)> {
        let home = crate::sandbox_images::HOME;
        let proxy = format!("http://{GATEWAY_HOST}:{}", self.cfg.gateway.proxy_port);
        let no_proxy = crate::runner::no_proxy(GATEWAY_HOST);
        let mut env: Vec<(String, String)> = vec![
            ("HOME".into(), home.into()),
            ("HTTPS_PROXY".into(), proxy.clone()),
            ("HTTP_PROXY".into(), proxy),
            ("NO_PROXY".into(), no_proxy.clone()),
            ("no_proxy".into(), no_proxy),
        ];
        let layout = crate::adapter::layout(harness_id);
        if !layout.env.is_empty() {
            env.push((layout.env.into(), format!("{home}/{}", layout.dir)));
        }
        env.extend(crate::sandbox_images::sandbox_env());
        env
    }

    /// The provider a `harness_id` sandbox is made with.
    pub fn provider(&self, harness_id: &str) -> AppleProvider {
        let apple = &self.cfg.runtime.apple;
        let options = AppleOptions {
            bin: apple.container.clone(),
            bundle: crate::bundle::effective(&self.cfg).unwrap_or_default(),
            socket_dir: AppleOptions::default_socket_dir(),
            limit: apple.limit,
            env: self.sandbox_env(harness_id),
            cpus: apple.cpus,
            memory: apple.memory.clone(),
            service_bundles: crate::service_bundle::mounts(&self.cfg),
            user: None,
        };
        AppleProvider::with_cli(options, Box::new(Shared(self.cli.clone())))
    }

    /// The provider a repository's build steps run with: as root, with no
    /// environment of the node's (the steps get the base's own, and the
    /// grant's proxy per command), and no service bundles.
    fn build_provider(&self) -> AppleProvider {
        let apple = &self.cfg.runtime.apple;
        let options = AppleOptions {
            bin: apple.container.clone(),
            bundle: crate::bundle::effective(&self.cfg).unwrap_or_default(),
            socket_dir: AppleOptions::default_socket_dir(),
            limit: None,
            env: Vec::new(),
            cpus: apple.cpus,
            memory: apple.memory.clone(),
            service_bundles: Vec::new(),
            user: Some("0:0".into()),
        };
        AppleProvider::with_cli(options, Box::new(Shared(self.cli.clone())))
    }

    /// Run `steps` in `held`, in turn, each with the grant's `proxy`; what
    /// they printed, and the first that failed.
    async fn run_steps(
        held: &crate::sandbox_images::Held,
        steps: &[String],
        proxy: &str,
        name: &str,
    ) -> (String, Option<String>) {
        let mut log = String::new();
        for (at, step) in steps.iter().enumerate() {
            let number = at + 1;
            log.push_str(&format!("#{number} RUN {step}\n"));
            let cmd = RunnerCommand {
                argv: vec!["/bin/sh".into(), "-ec".into(), step.clone()],
                secret_env: crate::runner::SecretEnv::new(
                    crate::boundary::BUILD_PROXY_VARS
                        .iter()
                        .map(|var| (var.to_string(), proxy.to_string()))
                        .collect(),
                ),
                workdir: Some("/work".into()),
                name: format!("{name}-{number}"),
                ..Default::default()
            };
            match held.run_capture(cmd).await {
                Ok(out) => {
                    log.push_str(&String::from_utf8_lossy(&out.stdout));
                    log.push_str(&String::from_utf8_lossy(&out.stderr));
                    if !out.status.success() {
                        let code = out.status.code().unwrap_or(-1);
                        return (
                            log,
                            Some(format!("build step {number} failed (exit {code})")),
                        );
                    }
                }
                Err(error) => return (log, Some(format!("build step {number}: {error}"))),
            }
        }
        (log, None)
    }

    fn runner_with(&self, harness_id: &str, image: String, mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let provider = Arc::new(self.provider(harness_id));
        Arc::new(
            crate::runner::protocol::ProtocolRunner::new(provider, image)
                .with_mounts(mounts)
                .with_workdir("/work")
                .with_files(self.files_store())
                .with_tunnels(crate::runner::tunnels::gateway_tunnels(&self.cfg))
                .with_bundled_harness(true)
                .with_sandbox_services(true)
                .with_delegate(Arc::new(Images {
                    cli: self.cli.clone(),
                })),
        )
    }

    /// `image` as `name@sha256:…`, if `container` holds it.
    pub async fn image_identity(&self, image: &str) -> Option<String> {
        identity(self.cli.as_ref(), image).await
    }

    /// Every name the image store holds, with the digest it names.
    async fn references(&self) -> Vec<(String, String)> {
        #[derive(Deserialize)]
        struct Listed {
            configuration: Named,
        }
        #[derive(Deserialize)]
        struct Named {
            name: String,
            descriptor: ListedDescriptor,
        }
        #[derive(Deserialize)]
        struct ListedDescriptor {
            digest: String,
        }
        let Ok(text) = self.container(&["image", "list", "--format", "json"]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<Listed>>(&text)
            .unwrap_or_default()
            .into_iter()
            .map(|l| (l.configuration.name, l.configuration.descriptor.digest))
            .collect()
    }

    /// `image` by digest, runnable as such: `container` finds an image only
    /// by a reference its store holds, and asks the registry for any other,
    /// which for an image built here is nowhere. So the digest reference is
    /// added to the store as a tag of its own.
    pub async fn pin_locally(&self, image: &str) -> Option<String> {
        let pinned = self.image_identity(image).await?;
        if pinned != image
            && self
                .container(&["image", "inspect", &pinned])
                .await
                .is_err()
        {
            if let Err(error) = self.container(&["image", "tag", image, &pinned]).await {
                tracing::warn!(%image, %error, "could not name the image by its digest");
                return None;
            }
        }
        Some(pinned)
    }

    /// Why `container` cannot run anything right now, or `None`.
    pub async fn unavailable(&self) -> Option<String> {
        if !cfg!(target_os = "macos") {
            return Some("runtime.kind = \"apple\" needs a Mac".into());
        }
        match self.container(&["system", "status"]).await {
            Ok(_) => None,
            Err(error) => Some(format!(
                "container's services are not running ({error}); start them with \
                 `container system start`"
            )),
        }
    }

    /// The bundle fetched or checked, and the base image pulled and pinned:
    /// what every sandbox here starts from. `Ok(true)` when anything was
    /// fetched or pulled.
    pub async fn converge_images(&self) -> Result<bool, String> {
        if let Some(why) = self.unavailable().await {
            return Err(why);
        }
        let fetched = match crate::bundle::ensure(&self.cfg).await {
            Ok(Some(crate::bundle::Ensured::Fetched(path))) => {
                tracing::info!(bundle = %path.display(), "fetched the harness bundle for this release");
                true
            }
            Ok(_) => false,
            Err(error) => return Err(format!("harness bundle: {error}")),
        };
        let configured = self.cfg.sandbox.base_image_ref();
        let registry = crate::boundary::registry::Registry::new(None);
        let resolved = match crate::sandbox_images::resolve_base(&registry, &configured, VM_ARCH)
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                tracing::warn!(image = %configured, %error, "could not resolve the sandbox base's {VM_PLATFORM} manifest; pinning it as container holds it");
                None
            }
        };
        let (pinned, pulled) = match resolved {
            Some(target) => self.pull_platform(&configured, &target).await?,
            None => {
                let mut pulled = false;
                if self.image_identity(&configured).await.is_none() {
                    tracing::info!(image = %configured, "pulling the sandbox base image");
                    self.container(&["image", "pull", "--platform", VM_PLATFORM, &configured])
                        .await
                        .map_err(|error| format!("pull {configured}: {error}"))?;
                    pulled = true;
                }
                let pinned = self
                    .pin_locally(&configured)
                    .await
                    .ok_or_else(|| format!("{configured} has no digest to pin"))?;
                (pinned, pulled)
            }
        };
        crate::sandbox_images::pin(&configured, &pinned);
        let browser = crate::service_bundle::converge(&self.cfg).await?;
        Ok(fetched || pulled || browser)
    }

    /// `target`, the arm64 manifest `configured` resolved to, held under its
    /// own digest; `true` when it pulled. Pulled by that digest, so nothing
    /// of another platform comes with it. A `container` that cannot pull a
    /// platform's manifest by digest pulls `configured` for the platform and
    /// names it by `target`.
    async fn pull_platform(
        &self,
        configured: &str,
        target: &str,
    ) -> Result<(String, bool), String> {
        if self.container(&["image", "inspect", target]).await.is_ok() {
            return Ok((target.to_string(), false));
        }
        tracing::info!(image = %target, "pulling the sandbox base image for {VM_PLATFORM}");
        let by_digest = self
            .container(&["image", "pull", "--platform", VM_PLATFORM, target])
            .await;
        if let Err(error) = by_digest {
            tracing::warn!(image = %target, %error, "could not pull the base by its platform's digest; pulling it as configured");
            self.container(&["image", "pull", "--platform", VM_PLATFORM, configured])
                .await
                .map_err(|error| format!("pull {configured}: {error}"))?;
            self.container(&["image", "tag", configured, target])
                .await
                .map_err(|error| format!("tag {configured} as {target}: {error}"))?;
        } else if !configured.contains('@') {
            // Named as configured too, so a start that cannot reach the
            // registry still finds it.
            if let Err(error) = self.container(&["image", "tag", target, configured]).await {
                tracing::warn!(image = %target, %error, "could not name the base as configured");
            }
        }
        Ok((target.to_string(), true))
    }

    fn store_error(volume: &str, error: files::FilesError) -> BoundaryError {
        BoundaryError::Other(format!("{volume}: {error}"))
    }

    fn refuse_live(volume: &str) -> Result<(), BoundaryError> {
        match files::is_live(volume) {
            true => Err(BoundaryError::Other(format!(
                "volume {volume} is in use by a running sandbox"
            ))),
            false => Ok(()),
        }
    }

    /// Export a volume: from the sandbox holding it while one runs, else the
    /// node's copy. Checked either way before anything reads it.
    async fn export(
        &self,
        volume: &str,
        destination: &Path,
        checkout: bool,
    ) -> Result<(), BoundaryError> {
        let failed =
            |e: files::FilesError| BoundaryError::Other(format!("copy out of {volume}: {e}"));
        match files::export_live(volume, destination, checkout).await {
            Some(exported) => exported.map(|_| ()).map_err(failed)?,
            None => {
                let store = self.files_store();
                let (volume, destination) = (volume.to_string(), destination.to_path_buf());
                tokio::task::spawn_blocking(move || store.export(&volume, &destination, checkout))
                    .await
                    .map_err(|e| BoundaryError::Other(e.to_string()))?
                    .map_err(failed)?
            }
        }
        crate::workspace::validate_tree(destination)
            .map_err(|e| BoundaryError::Other(e.to_string()))
    }
}

/// One `container` shared by the backend and every provider it makes.
struct Shared(Arc<dyn ContainerCli>);

#[async_trait]
impl ContainerCli for Shared {
    async fn run(&self, args: &[String]) -> Result<String, CliError> {
        self.0.run(args).await
    }
}

/// What `container image inspect` says of an image, as far as it is read.
#[derive(Deserialize)]
struct InspectedImage {
    configuration: ImageConfiguration,
}

#[derive(Deserialize)]
struct ImageConfiguration {
    descriptor: ImageDescriptor,
}

#[derive(Deserialize)]
struct ImageDescriptor {
    digest: String,
}

/// `image` as `name@sha256:…`, the reference a later tag cannot move, if
/// `container` holds it. A name already pinned keeps its name.
async fn identity(cli: &dyn ContainerCli, image: &str) -> Option<String> {
    let text = cli
        .run(&["image".into(), "inspect".into(), image.into()])
        .await
        .ok()?;
    let inspected: Vec<InspectedImage> = serde_json::from_str(&text).ok()?;
    // A digest reference the store holds is its own identity: the base is
    // pinned by its platform's manifest, which the store may keep under an
    // index of its own making.
    if image.contains('@') && crate::config::immutable_image(image).is_ok() {
        return Some(image.to_string());
    }
    let digest = &inspected.first()?.configuration.descriptor.digest;
    let name = image_name(image);
    let identity = format!("{name}@{digest}");
    crate::config::immutable_image(&identity)
        .is_ok()
        .then_some(identity)
}

/// A reference without its tag or digest: a tag is the last `:` after the
/// last `/`, and a registry's port is not one.
pub(crate) fn image_name(image: &str) -> &str {
    match image.split_once('@') {
        Some((name, _)) => name,
        None => match image.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => name,
            _ => image,
        },
    }
}

/// What a protocol runner hands on: the identity of the image a run names.
/// Nothing runs here.
struct Images {
    cli: Arc<dyn ContainerCli>,
}

#[async_trait]
impl Runner for Images {
    async fn spawn(&self, _cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        Err(RunnerError::Other("the image store runs nothing".into()))
    }

    async fn run_capture(&self, _cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        Err(RunnerError::Other("the image store runs nothing".into()))
    }

    async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
        Ok(())
    }

    async fn resolved_image(&self, image: Option<&str>) -> Option<String> {
        identity(self.cli.as_ref(), image?).await
    }
}

pub(crate) fn log_tail(bytes: &[u8]) -> String {
    crate::boundary::podman::log_tail(bytes)
}

#[async_trait]
impl ImageBuilder for AppleBackend {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        let mut args: Vec<String> = vec!["build".into(), "--progress".into(), "plain".into()];
        for (name, value) in build.labels {
            args.push("--label".into());
            args.push(format!("{name}={value}"));
        }
        args.extend([
            "-f".into(),
            build.containerfile.to_string_lossy().into_owned(),
            "-t".into(),
            build.tag.to_string(),
            build.context.to_string_lossy().into_owned(),
        ]);
        let failure = |error: String, log: &str| BuildFailure {
            error,
            log_tail: log_tail(log.as_bytes()),
            ..Default::default()
        };
        // The CLI is killed when its future is dropped at the deadline.
        let log = match tokio::time::timeout(build.timeout, self.cli.run(&args)).await {
            Ok(Ok(out)) => out,
            Ok(Err(CliError::Failed(log))) => {
                return Err(failure("container build failed".into(), &log))
            }
            Ok(Err(CliError::Spawn(error))) => {
                return Err(BuildFailure::transient(format!("container build: {error}")))
            }
            Err(_) => {
                return Err(failure(
                    format!(
                        "the build did not finish in {} minutes",
                        build.timeout.as_secs() / 60
                    ),
                    "",
                ))
            }
        };
        match self.pin_locally(build.tag).await {
            Some(image) => Ok(BuiltImage {
                image,
                log_tail: log_tail(log.as_bytes()),
            }),
            None => Err(failure(
                format!("container reported no digest for {}", build.tag),
                &log,
            )),
        }
    }

    /// `container build` runs in a builder VM with a network of its own,
    /// which nothing here can hold to a list. So the steps run instead in a
    /// sandbox from the base, as root, whose one way out is a tunnel to a
    /// listener of this build's own serving a grant of exactly the steps'
    /// hosts; the sandbox is then committed and configured as the base is.
    async fn build_steps(
        &self,
        build: ImageBuild<'_>,
        steps: &crate::boundary::StepsBuild<'_>,
    ) -> Result<BuiltImage, BuildFailure> {
        if let Some(why) = AppleBackend::unavailable(self).await {
            return Err(BuildFailure::transient(why));
        }
        let failure = |error: String, log: &str| BuildFailure {
            error,
            log_tail: log_tail(log.as_bytes()),
            ..Default::default()
        };
        let serving = crate::boundary::BuildEgress::serve(
            &self.grants,
            Backend::egress_refusal(self),
            steps.hosts,
        )
        .await?;
        let server = crate::runner::protocol::node_server()
            .map_err(|error| BuildFailure::transient(format!("build sandbox: {error}")))?;
        let id = uuid::Uuid::now_v7().simple().to_string();
        let name = crate::config::resource_name(&format!("b-{}", &id[id.len() - 12..]));
        let built = async {
            let held = crate::sandbox_images::Held::start(
                Arc::new(self.build_provider()),
                None,
                server,
                &name,
                steps.base,
                &[],
            )
            .await
            .map_err(|error| failure(error, ""))?;
            let tunnel = crate::runner::tunnels::TunnelSpec::loopback(
                "build-egress",
                BUILD_PROXY_PORT,
                tracon_sandbox::node::Endpoint::Tcp(format!("127.0.0.1:{}", serving.port)),
            );
            if let Err(error) = held.open_tunnels(&[tunnel]).await {
                held.finish().await;
                return Err(failure(format!("build egress tunnel: {error}"), ""));
            }
            let (log, failed) =
                Self::run_steps(&held, steps.steps, &serving.proxy(BUILD_PROXY_PORT), &name).await;
            let image = match failed {
                Some(error) => Err(error),
                None => held.snapshot(build.tag, build.labels).await,
            };
            held.finish().await;
            image
                .map(|image| BuiltImage {
                    image,
                    log_tail: log_tail(log.as_bytes()),
                })
                .map_err(|error| failure(error, &log))
        };
        // The sandbox is removed when its future is dropped at the deadline.
        match tokio::time::timeout(build.timeout, built).await {
            Ok(built) => built,
            Err(_) => Err(failure(
                format!(
                    "the build did not finish in {} minutes",
                    build.timeout.as_secs() / 60
                ),
                "",
            )),
        }
    }

    async fn exists(&self, image: &str) -> bool {
        self.image_identity(image).await.is_some()
    }

    async fn identity(&self, image: &str) -> Option<String> {
        self.image_identity(image).await
    }

    /// `image`, and when it is pinned every other name the store holds it
    /// by: a pinned image here is also tagged, and the tag would keep it.
    async fn remove(&self, image: &str) {
        let mut names = vec![image.to_string()];
        if let Some((name, digest)) = image.split_once('@') {
            names.extend(
                self.references()
                    .await
                    .into_iter()
                    .filter(|(held, at)| at == digest && image_name(held) == name)
                    .map(|(held, _)| held),
            );
        }
        names.sort();
        names.dedup();
        for name in names {
            let _ = self.container(&["image", "delete", &name]).await;
        }
    }

    async fn presence(&self, image: &str) -> Presence {
        if self.unavailable().await.is_some() {
            return Presence::Unknown;
        }
        match self.exists(image).await {
            true => Presence::Present,
            false => Presence::Absent,
        }
    }

    async fn unavailable(&self) -> Option<String> {
        AppleBackend::unavailable(self).await
    }
}

#[async_trait]
impl Backend for AppleBackend {
    fn kind(&self) -> &'static str {
        "apple"
    }

    async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
        self.converge_images()
            .await
            .map(|_| ())
            .map_err(BoundaryError::Other)
    }

    async fn converge(&self, _cfg: &Config) -> Result<bool, BoundaryError> {
        self.converge_images().await.map_err(BoundaryError::Other)
    }

    fn converges(&self) -> bool {
        true
    }

    /// The release and the base it pins: a change to either is a bundle to
    /// fetch or an image to pull.
    fn definitions(&self, cfg: &Config) -> Option<String> {
        Some(format!(
            "{}\u{1f}{}\u{1f}{:?}",
            env!("CARGO_PKG_VERSION"),
            cfg.sandbox.base_image_ref(),
            cfg.sandbox.bundle
        ))
    }

    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        checks::check_all(self, cfg, deep).await
    }

    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.runner_for(&self.cfg.harness.id, extra_mounts)
    }

    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.runner_with(
            harness_id,
            crate::sandbox_images::base_image(&self.cfg),
            extra_mounts,
        )
    }

    fn runner_in(
        &self,
        harness_id: &str,
        image: &str,
        extra_mounts: Vec<Mount>,
    ) -> Arc<dyn Runner> {
        self.runner_with(harness_id, image.to_string(), extra_mounts)
    }

    fn runs_sessions_in_repository_images(&self) -> bool {
        true
    }

    /// One image for every harness: the base, which the bundle completes.
    async fn harness_images(&self, cfg: &Config) -> Vec<crate::boundary::HarnessImage> {
        let image = crate::sandbox_images::base_image(cfg);
        let state = match self.image_identity(&image).await {
            Some(_) => "current",
            None => "missing",
        };
        crate::adapter::KNOWN
            .iter()
            .map(|id| crate::boundary::HarnessImage {
                harness_id: id.to_string(),
                image: image.clone(),
                state,
            })
            .collect()
    }

    fn image_builder(&self) -> Option<&dyn ImageBuilder> {
        Some(self)
    }

    async fn prepared_snapshots(&self) -> Vec<String> {
        let ours = format!("{}-", crate::sandbox_images::snapshot_repo());
        let mut pinned: Vec<String> = self
            .references()
            .await
            .into_iter()
            .filter(|(name, _)| name.starts_with(&ours))
            // A commit not yet configured is a snapshot still being made.
            .filter(|(name, _)| !image_name(name).ends_with("-commit"))
            .map(|(name, digest)| format!("{}@{digest}", image_name(&name)))
            .collect();
        pinned.sort();
        pinned.dedup();
        pinned
    }

    fn preparer(&self) -> Option<crate::sandbox_images::Preparer> {
        Some(crate::sandbox_images::Preparer {
            provider: Arc::new(self.provider(&self.cfg.harness.id)),
            files: Some(self.files_store()),
            tunnels: crate::runner::tunnels::gateway_tunnels(&self.cfg),
        })
    }

    fn harness_image(&self, cfg: &Config, _harness_id: &str) -> String {
        crate::sandbox_images::base_image(cfg)
    }

    async fn import_volume(&self, volume: &str, source: &Path) -> Result<(), BoundaryError> {
        Self::refuse_live(volume)?;
        let store = self.files_store();
        // A check's copy of an immutable snapshot is the snapshot itself,
        // sent straight into its sandbox; nothing reads a check's changes
        // back.
        let done = match files::is_check(volume) {
            true => crate::workspace::validate_tree(source)
                .map_err(files::FilesError::from)
                .and_then(|()| store.alias(volume, source)),
            false => store.import(volume, source),
        };
        done.map_err(|e| BoundaryError::Other(format!("copy into {volume}: {e}")))
    }

    async fn export_volume(&self, volume: &str, destination: &Path) -> Result<(), BoundaryError> {
        self.export(volume, destination, false).await
    }

    async fn export_workspace(
        &self,
        volume: &str,
        destination: &Path,
    ) -> Result<(), BoundaryError> {
        self.export(volume, destination, true).await
    }

    fn harness_host(&self) -> String {
        GATEWAY_HOST.into()
    }

    fn harness_home(&self) -> String {
        crate::sandbox_images::HOME.into()
    }

    fn egress_grants(&self) -> Option<&Grants> {
        Some(&self.grants)
    }

    /// As Podman's over tunnels: the grant's proxy is the sandbox's egress
    /// tunnel, which leads to the node's own egress listener.
    fn egress_grant(&self, spec: GrantSpec) -> Result<EgressGrant, BoundaryError> {
        if self.cfg.gateway.qa_proxy_port == 0 {
            return Err(BoundaryError::Other(
                "per-client egress is not configured (gateway.qa_proxy_port)".into(),
            ));
        }
        let username = spec.client.clone();
        let token = self
            .grants
            .issue(spec)
            .map_err(|error| BoundaryError::Other(format!("egress grant: {error}")))?;
        let grants = self.grants.clone();
        let revoked = token.clone();
        Ok(EgressGrant::new(
            format!("http://{GATEWAY_HOST}:{}", self.cfg.gateway.qa_proxy_port),
            username,
            token,
            move || grants.revoke(&revoked),
        ))
    }

    /// Remove the named sandboxes a previous node process left, with their
    /// sockets.
    async fn reconcile(&self, names: &[String]) {
        use crate::provider::SandboxProvider as _;
        let provider = self.provider(&self.cfg.harness.id);
        for name in names {
            if let Err(error) = provider
                .destroy(&crate::provider::Handle(name.clone()))
                .await
            {
                tracing::warn!(%name, %error, "could not remove a leftover sandbox");
            }
        }
    }

    async fn list_volumes(&self) -> Result<Vec<super::VolumeInfo>, BoundaryError> {
        self.files_store()
            .list()
            .map_err(|e| BoundaryError::Other(e.to_string()))
    }

    async fn volume_exists(&self, volume: &str) -> bool {
        self.files_store().holds(volume)
    }

    /// A snapshot is a commit of the sandbox its preparation ran in, cache
    /// and all.
    async fn seeds_session_cache(&self, base: &str, from_snapshot: bool) -> bool {
        !from_snapshot && self.files_store().holds(base)
    }

    /// Every tree is sent onto the VM's own disk, over the image's.
    fn workspace_over_image(&self) -> bool {
        true
    }

    async fn clone_volume(&self, source: &str, destination: &str) -> Result<(), BoundaryError> {
        self.files_store()
            .clone_volume(source, destination)
            .await
            .map_err(|e| Self::store_error(source, e))
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        Self::refuse_live(volume)?;
        self.files_store()
            .remove(volume)
            .map_err(|e| Self::store_error(volume, e))
    }
}

#[cfg(test)]
mod tests;
