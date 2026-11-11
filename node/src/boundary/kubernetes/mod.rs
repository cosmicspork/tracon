//! The pod-hosted boundary: the node is an unprivileged pod that runs every
//! harness, check and preparation in a sandbox pod of its own over the
//! sandbox protocol (`provider::kubernetes`). One NetworkPolicy makes the
//! node pod's sandbox port a sandbox's only route, and everything else it
//! reaches (the MCP and model gateway, the allowlist proxy, the per-client
//! egress proxy) is a tunnel over that one connection. Everything the node
//! needs to exist (the claim, the policies, its own labels and RBAC) is the
//! deployment's, in `deploy/kubernetes/base`, and is verified rather than
//! created here.
//!
//! There are no harness images: every sandbox runs the stock base, or a
//! repository's image built on it from its `build` steps, with the harness
//! from the bundle. The node pins the base by the manifest its registry
//! holds for this machine's architecture when it converges (nothing is
//! pulled here: a pod's kubelet pulls the digest), and with
//! `[runtime.kubernetes.builder]` it builds the steps and each repository's
//! prepared snapshot on the cluster's BuildKit.

pub mod build;
pub mod checks;

use std::sync::Arc;

use async_trait::async_trait;
use rust_embed::Embed;

use super::{Backend, BoundaryError, BoundaryReport, EgressGrant};
use crate::config::Config;
use crate::gateway::proxy::{GrantSpec, Grants, Refusal};
use crate::provider::kubernetes::pod::{DigestIdentity, KubeSpec, PodEnv};
use crate::runner::{Mount, Runner};

/// The manifests a node of this kind expects around it, printed by
/// `tracon setup` for the operator (or the Coder template) to apply.
#[derive(Embed)]
#[folder = "../deploy/kubernetes/base"]
pub struct Manifests;

pub struct KubeBackend {
    cfg: Config,
    /// Resolved once; a node that cannot learn its own pod facts is not a pod.
    env: Result<PodEnv, String>,
    client: tokio::sync::OnceCell<Result<kube::Client, String>>,
    /// `[runtime.kubernetes.builder]`, or why it is not a builder. `None`
    /// where none is configured.
    builder: Option<Result<Arc<build::BuildKitBuilder>, build::Unusable>>,
    /// Every live per-client grant, served on the egress port.
    grants: Grants,
    /// What the egress proxy never dials here; an error for an
    /// `internal_cidrs` entry that is not an address block, which turns
    /// per-client egress off and fails the runtime check.
    refusal: Result<Refusal, String>,
}

/// The builder `[runtime.kubernetes.builder]` configures, if any, staging on
/// the state volume. Its build steps reach the node's egress proxy, served
/// from `grants`, at the node pod's address when it is known.
fn builder_for(
    cfg: &Config,
    env: Option<&PodEnv>,
    grants: &Grants,
    refused: Option<&str>,
) -> Option<Result<Arc<build::BuildKitBuilder>, build::Unusable>> {
    let settings = cfg.runtime.kubernetes.builder.as_ref()?;
    let scratch = cfg.runtime.kubernetes.state_mount.join(build::SCRATCH);
    Some(
        build::BuildKitBuilder::new(settings, scratch)
            .map(|builder| {
                let builder = match env.map(|env| egress_address(&env.pod_ip, cfg)) {
                    Some(proxy) => builder.with_steps_egress(build::StepsEgress {
                        grants: grants.clone(),
                        proxy,
                        refused: refused.map(str::to_string),
                    }),
                    None => builder,
                };
                Arc::new(builder)
            })
            .map_err(|why| {
                build::Unusable(format!(
                    "[runtime.kubernetes.builder] is not usable ({why})"
                ))
            }),
    )
}

/// The node's per-client egress listener as a pod reaches it: the node pod's
/// address, the egress port.
fn egress_address(pod_ip: &str, cfg: &Config) -> String {
    let port = cfg.gateway.egress_port;
    match pod_ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{pod_ip}]:{port}"),
        _ => format!("{pod_ip}:{port}"),
    }
}

impl KubeBackend {
    pub fn new(cfg: &Config) -> Self {
        Self::built(cfg, PodEnv::detect(cfg), tokio::sync::OnceCell::new())
    }

    /// A backend for a pod whose facts and client are given rather than
    /// detected: a test's fake API.
    #[doc(hidden)]
    pub fn with_client(cfg: &Config, env: PodEnv, client: kube::Client) -> Self {
        Self::built(
            cfg,
            Ok(env),
            tokio::sync::OnceCell::new_with(Some(Ok(client))),
        )
    }

    fn built(
        cfg: &Config,
        env: Result<PodEnv, String>,
        client: tokio::sync::OnceCell<Result<kube::Client, String>>,
    ) -> Self {
        let refusal = Refusal::cluster(&cfg.runtime.kubernetes.internal_cidrs)
            .map_err(|error| format!("[runtime.kubernetes] internal_cidrs: {error}"));
        let grants = Grants::default();
        let builder = builder_for(
            cfg,
            env.as_ref().ok(),
            &grants,
            refusal.as_ref().err().map(String::as_str),
        );
        Self {
            cfg: cfg.clone(),
            env,
            client,
            grants,
            refusal,
            builder,
        }
    }

    /// What the builder check found, in the words the `runtime` check
    /// carries. Never a failure: a node without a working builder still runs
    /// everything but a repository's `dockerfile`, which says why it is not
    /// built.
    pub async fn builder_status(&self) -> String {
        match &self.builder {
            None => "builder: not configured ([runtime.kubernetes.builder]), so a repository's \
                     dockerfile is not built here"
                .to_string(),
            Some(Err(build::Unusable(why))) => format!("builder: {}", build::unusable(why)),
            Some(Ok(builder)) => match builder.check().await {
                Ok(said) => format!("builder: {said}"),
                Err(why) => format!("builder: {}", build::unusable(&why)),
            },
        }
    }

    /// The bundle fetched or checked, and the stock base pinned by the manifest its registry holds for this
    /// machine's architecture, which is every sandbox's, since affinity keeps
    /// them on the node pod's machine. Nothing is pulled here; a pod's
    /// kubelet pulls the pinned digest. With `[digitalocean] registry`, the
    /// release's browser bundle too, which the templates it builds hold.
    /// `Ok(true)` when a bundle was fetched or the pin moved.
    pub async fn converge_images(&self) -> Result<bool, String> {
        let fetched = match crate::bundle::ensure(&self.cfg).await {
            Ok(Some(crate::bundle::Ensured::Fetched(path))) => {
                tracing::info!(bundle = %path.display(), "fetched the harness bundle for this release");
                true
            }
            Ok(_) => false,
            Err(error) => return Err(format!("harness bundle: {error}")),
        };
        // The DigitalOcean templates this node builds hold the browser, and a
        // peer's request waits on it: fetched whether or not the pin succeeds.
        let browser = crate::service_bundle::converge(&self.cfg).await;
        let configured = self.cfg.sandbox.base_image_ref();
        // The builder's credential for a base kept in its registry; a base
        // anywhere else (the release's, on GHCR) is public and read without.
        let docker_config = self
            .cfg
            .runtime
            .kubernetes
            .builder
            .as_ref()
            .and_then(|builder| {
                let host = builder
                    .registry
                    .trim()
                    .split('/')
                    .next()
                    .unwrap_or_default();
                (!host.is_empty() && configured.starts_with(&format!("{host}/")))
                    .then(|| builder.docker_config.clone())
            });
        let registry = crate::boundary::registry::Registry::new(docker_config);
        let arch = crate::boundary::registry::host_architecture();
        let pinned = crate::sandbox_images::resolve_base(&registry, &configured, arch)
            .await
            .map_err(|error| format!("pin the sandbox base {configured}: {error}"))?
            .ok_or_else(|| {
                format!(
                    "the sandbox base {configured} is only in a local image store, which no pod \
                     pulls from: set [sandbox] base_image to an image in a registry"
                )
            })?;
        let moved = crate::sandbox_images::active_base().as_deref() != Some(pinned.as_str());
        if moved {
            tracing::info!(image = %pinned, "pinned the sandbox base for linux/{arch}");
        }
        crate::sandbox_images::pin(&configured, &pinned);
        Ok(fetched || moved || browser?)
    }

    /// The `internal_cidrs` entry that is not an address block, if one is.
    pub fn refusal_error(&self) -> Option<&str> {
        self.refusal.as_ref().err().map(String::as_str)
    }

    /// Why no grant can be issued, naming what to change; `None` when one
    /// can. A sandbox reaches the egress proxy through its tunnel, and the
    /// grant's credentials travel over the protocol: neither a policy port
    /// nor a Secret is involved.
    pub fn egress_unavailable(&self) -> Option<String> {
        self.refusal
            .as_ref()
            .err()
            .map(|error| format!("per-client egress is off on this node: {error}"))
    }

    pub async fn client(&self) -> Result<kube::Client, String> {
        self.client
            .get_or_init(|| async {
                let config =
                    kube::Config::incluster().map_err(|e| format!("in-cluster config: {e}"))?;
                kube::Client::try_from(config).map_err(|e| format!("client: {e}"))
            })
            .await
            .clone()
    }

    pub fn spec(&self) -> Result<KubeSpec, String> {
        self.spec_for(&self.cfg.harness.id)
    }

    pub fn spec_for(&self, harness_id: &str) -> Result<KubeSpec, String> {
        Ok(KubeSpec::for_harness(
            &self.cfg,
            harness_id,
            self.env.clone()?,
        ))
    }

    /// A runner for `harness_id` whose spec `adjust` has had its say over.
    fn runner_with(
        &self,
        harness_id: &str,
        extra_mounts: Vec<Mount>,
        adjust: impl FnOnce(&mut KubeSpec),
    ) -> Arc<dyn Runner> {
        let client = match self.client.get() {
            Some(Ok(c)) => c.clone(),
            Some(Err(e)) => return Arc::new(Unavailable(e.clone())),
            None => {
                return Arc::new(Unavailable(
                    "kubernetes client not initialised (checks did not run)".into(),
                ))
            }
        };
        match self.spec_for(harness_id) {
            Ok(mut spec) => {
                spec.extra_mounts = extra_mounts;
                adjust(&mut spec);
                self.protocol_runner(client, spec)
            }
            Err(e) => Arc::new(Unavailable(e)),
        }
    }

    /// Where a node running sandboxes keeps the trees it sends them: the
    /// state claim's own directories, every volume a run names among them,
    /// except the harness state every session once shared, which no sandbox
    /// is handed (a session gets its own, `materialize::own_state`).
    pub fn files_store(&self) -> crate::runner::sandbox_files::FilesStore {
        crate::runner::sandbox_files::FilesStore::new(&self.cfg.runtime.kubernetes.state_mount)
            .withholding(crate::session::materialize::harness_state_volume())
            .keeping_every_volume()
    }

    /// The sandbox provider around `spec`, which names the pod a harness would
    /// have run in.
    fn provider_for(
        &self,
        client: kube::Client,
        spec: KubeSpec,
    ) -> crate::provider::kubernetes::KubeSandboxProvider {
        crate::provider::kubernetes::KubeSandboxProvider::new(
            client,
            crate::provider::kubernetes::KubeSandboxOptions::from_config(&self.cfg, spec),
        )
    }

    /// The provider for this node's own harness, for the checks.
    pub async fn sandbox_provider(
        &self,
    ) -> Result<crate::provider::kubernetes::KubeSandboxProvider, String> {
        let spec = self.spec()?;
        Ok(self.provider_for(self.client().await?, spec))
    }

    /// A sandbox per command over the protocol, with its trees sent over the
    /// files channel and its routes to the node as tunnels. A digest-pinned
    /// image is its own identity, answered without running anything.
    fn protocol_runner(&self, client: kube::Client, mut spec: KubeSpec) -> Arc<dyn Runner> {
        let mounts = std::mem::take(&mut spec.extra_mounts);
        let image = spec.image.clone();
        let workdir = spec.workdir.clone();
        let provider = self.provider_for(client, spec);
        Arc::new(
            crate::runner::protocol::ProtocolRunner::new(Arc::new(provider), image)
                .with_mounts(mounts)
                .with_workdir(workdir)
                .with_delegate(Arc::new(DigestIdentity))
                .with_files(self.files_store())
                .with_tunnels(crate::runner::tunnels::kube_tunnels(&self.cfg))
                .with_bundled_harness(true),
        )
    }

    /// Copy a volume on the claim to a node-owned path through a staging
    /// directory beside it, so a refused or failed copy never leaves a partial
    /// tree where a snapshot is expected.
    fn export_with(
        &self,
        volume: &str,
        destination: &std::path::Path,
        copy: impl FnOnce(
            &std::path::Path,
            &std::path::Path,
        ) -> Result<(), crate::workspace::WorkspaceError>,
    ) -> Result<(), BoundaryError> {
        let source = self.cfg.runtime.kubernetes.state_mount.join(volume);
        if !source.is_dir() {
            return Err(BoundaryError::Other(format!(
                "runtime volume {volume} does not exist"
            )));
        }
        let staged = destination.with_extension(format!("staging-{}", uuid::Uuid::now_v7()));
        let _ = std::fs::remove_dir_all(&staged);
        if let Err(error) = copy(&source, &staged) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(BoundaryError::Other(error.to_string()));
        }
        if destination.exists() {
            std::fs::remove_dir_all(destination)?;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(staged, destination)?;
        crate::workspace::validate_tree(destination)
            .map_err(|e| BoundaryError::Other(e.to_string()))
    }

    pub fn manifests() -> String {
        let mut out = String::new();
        let mut names: Vec<_> = Manifests::iter().collect();
        names.sort();
        for name in names {
            if let Some(f) = Manifests::get(&name) {
                out.push_str(&format!("# --- {name}\n"));
                out.push_str(&String::from_utf8_lossy(&f.data));
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// A runner that cannot be built reports the reason on first use rather than
/// at construction: `Backend::runner` is infallible because a refused node
/// never reaches it (the checks failed first), and the sync signature keeps
/// the session path free of a second async hop.
struct Unavailable(String);

#[async_trait]
impl Runner for Unavailable {
    async fn spawn(
        &self,
        _cmd: crate::runner::RunnerCommand,
    ) -> Result<crate::runner::Spawned, crate::runner::RunnerError> {
        Err(crate::runner::RunnerError::Other(self.0.clone()))
    }
    async fn run_capture(
        &self,
        _cmd: crate::runner::RunnerCommand,
    ) -> Result<std::process::Output, crate::runner::RunnerError> {
        Err(crate::runner::RunnerError::Other(self.0.clone()))
    }
    async fn kill(&self, _name: &str) -> Result<(), crate::runner::RunnerError> {
        Ok(())
    }
}

#[async_trait]
impl Backend for KubeBackend {
    fn kind(&self) -> &'static str {
        "kubernetes"
    }

    async fn setup(&self, _cfg: &Config, _rebuild: bool) -> Result<(), BoundaryError> {
        // Nothing to build: images come from the release, and what the pod
        // needs around it is applied by the operator. Say what that is.
        self.converge_images().await.map_err(BoundaryError::Other)?;
        println!("{}", Self::manifests());
        eprintln!(
            "apply the manifests above (kubectl apply -k deploy/kubernetes/base, or an overlay of it); \
             then `tracon check-boundary --deep` inside the node pod"
        );
        Ok(())
    }

    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        let mut report = checks::check_all(self, cfg, deep).await;
        let builder = self.builder_status().await;
        if let Some(runtime) = report
            .checks
            .iter_mut()
            .find(|check| check.id == super::CheckId::Runtime)
        {
            runtime.detail = format!("{}; {builder}", runtime.detail);
        }
        report
    }

    async fn converge(&self, _cfg: &Config) -> Result<bool, BoundaryError> {
        self.converge_images().await.map_err(BoundaryError::Other)
    }

    /// A bundle to fetch and a base to pin; the cluster's manifests stay the
    /// operator's.
    fn converges(&self) -> bool {
        true
    }

    /// The release, the base and the bundle; and, while the base is not
    /// pinned (its registry did not answer), something new at every look,
    /// so the node's upkeep tries again each time it looks rather than
    /// staying unpinned until a restart.
    fn definitions(&self, cfg: &Config) -> Option<String> {
        let unpinned = match crate::sandbox_images::active_base() {
            Some(_) => String::new(),
            None => format!(
                "\u{1f}unpinned {}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default()
            ),
        };
        Some(format!(
            "{}\u{1f}{}\u{1f}{:?}{unpinned}",
            env!("CARGO_PKG_VERSION"),
            cfg.sandbox.base_image_ref(),
            cfg.sandbox.bundle
        ))
    }

    fn image_builder(&self) -> Option<&dyn super::ImageBuilder> {
        match &self.builder {
            Some(Ok(builder)) => Some(builder.as_ref()),
            Some(Err(unusable)) => Some(unusable),
            None => None,
        }
    }

    /// With a builder to push each snapshot: the node's own sandboxes, whose
    /// snapshots are built from what their preparation left.
    fn preparer(&self) -> Option<crate::sandbox_images::Preparer> {
        let Some(Ok(builder)) = &self.builder else {
            return None;
        };
        let Some(Ok(client)) = self.client.get() else {
            return None;
        };
        let spec = self.spec().ok()?;
        let provider = self.provider_for(client.clone(), spec).with_snapshots(
            crate::provider::kubernetes::Snapshots::new(
                builder.clone(),
                crate::repo_image::BUILD_TIMEOUT,
            ),
        );
        Some(crate::sandbox_images::Preparer {
            provider: Arc::new(provider),
            files: Some(self.files_store()),
            tunnels: crate::runner::tunnels::kube_tunnels(&self.cfg),
        })
    }

    fn sandbox_files_store(&self) -> Option<crate::runner::sandbox_files::FilesStore> {
        Some(self.files_store())
    }

    fn registry_builder(&self) -> Option<Arc<dyn super::ImageBuilder>> {
        match &self.builder {
            Some(Ok(builder)) => Some(builder.clone()),
            _ => None,
        }
    }
    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        Backend::runner_for(self, &self.cfg.harness.id, extra_mounts)
    }

    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.runner_with(harness_id, extra_mounts, |_| {})
    }

    /// The harness's spec with the session's image (its repository's, or
    /// its prepared snapshot) in place of the base.
    fn runner_in(
        &self,
        harness_id: &str,
        image: &str,
        extra_mounts: Vec<Mount>,
    ) -> Arc<dyn Runner> {
        self.runner_with(harness_id, extra_mounts, |spec| {
            spec.image = image.to_string();
        })
    }

    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        crate::workspace::validate_tree(source).map_err(|e| BoundaryError::Other(e.to_string()))?;
        refuse_live(volume)?;
        let root = self.cfg.runtime.kubernetes.state_mount.join(volume);
        let staged = root.with_extension(format!("staging-{}", uuid::Uuid::now_v7()));
        let _ = std::fs::remove_dir_all(&staged);
        crate::workspace::copy_tree(source, &staged, false)
            .map_err(|e| BoundaryError::Other(e.to_string()))?;
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        std::fs::rename(staged, root)?;
        Ok(())
    }

    async fn export_volume(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        if export_live(volume, destination, false).await? {
            return Ok(());
        }
        self.export_with(volume, destination, |source, staged| {
            crate::workspace::copy_tree(source, staged, false)
        })
    }

    /// The node reads the claim directly, so it stages what Podman's export
    /// stages in a container (`STAGE_WORKSPACE`), with the index and the
    /// ignore rules read as files: no Git runs on configuration the agent
    /// wrote.
    async fn export_workspace(
        &self,
        volume: &str,
        destination: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        if export_live(volume, destination, true).await? {
            return Ok(());
        }
        self.export_with(volume, destination, crate::workspace::copy_checkout)
    }

    async fn clone_volume(&self, source: &str, destination: &str) -> Result<(), BoundaryError> {
        super::clone_directory_volume_with(
            "cp",
            &self.cfg.runtime.kubernetes.state_mount,
            source,
            destination,
        )
        .await
    }

    fn harness_host(&self) -> String {
        self.cfg.runtime.kubernetes.gateway_host.clone()
    }

    fn harness_home(&self) -> String {
        crate::sandbox_images::HOME.into()
    }

    fn proxy_port(&self) -> Option<u16> {
        Some(self.cfg.gateway.proxy_port)
    }

    /// A sandbox reaches it through its tunnel; a grant is what opens
    /// anything.
    fn egress_grants(&self) -> Option<&Grants> {
        Some(&self.grants)
    }

    /// As Podman's: a sandbox reaches the proxy under `gateway_host`, its
    /// own loopback, through its tunnel. The grant's URL carries its
    /// password, so it goes in the run's `secret_env`, which travels with
    /// the spawn over the protocol.
    fn egress_grant(&self, spec: GrantSpec) -> Result<EgressGrant, BoundaryError> {
        if let Some(why) = self.egress_unavailable() {
            return Err(BoundaryError::Other(why));
        }
        let username = spec.client.clone();
        let token = self
            .grants
            .issue(spec)
            .map_err(|error| BoundaryError::Other(format!("egress grant: {error}")))?;
        let grants = self.grants.clone();
        let revoked = token.clone();
        Ok(EgressGrant::new(
            format!(
                "http://{}:{}",
                self.cfg.runtime.kubernetes.gateway_host, self.cfg.gateway.egress_port
            ),
            username,
            token,
            move || grants.revoke(&revoked),
        ))
    }

    fn egress_refusal(&self) -> Refusal {
        // An unusable `internal_cidrs` issues no grants at all; the listener
        // still refuses at least what it always would.
        self.refusal
            .clone()
            .unwrap_or_else(|_| Refusal::cluster(&[]).expect("the private blocks parse"))
    }

    /// Remove the named sandboxes a previous node process left.
    async fn reconcile(&self, names: &[String]) {
        use crate::provider::SandboxProvider as _;
        let provider = match self.sandbox_provider().await {
            Ok(provider) => provider,
            Err(error) => {
                tracing::warn!(%error, "could not remove leftover sandboxes");
                return;
            }
        };
        for name in names {
            if let Err(error) = provider
                .destroy(&crate::provider::Handle(name.clone()))
                .await
            {
                tracing::warn!(%name, %error, "could not remove a leftover sandbox");
            }
        }
    }

    // A volume here is a directory on the shared state claim. Nothing stops
    // removing one a pod still mounts, so the caller only removes volumes
    // whose owner is over.
    async fn list_volumes(&self) -> Result<Vec<crate::boundary::VolumeInfo>, BoundaryError> {
        crate::boundary::directory_volumes(&self.cfg.runtime.kubernetes.state_mount)
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        refuse_live(volume)?;
        crate::boundary::remove_directory_volume(&self.cfg.runtime.kubernetes.state_mount, volume)
    }

    /// A snapshot carries the cache its preparation filled in its image.
    async fn seeds_session_cache(&self, base: &str, from_snapshot: bool) -> bool {
        !from_snapshot && self.files_store().holds(base)
    }

    /// Every tree is sent onto the pod's own storage, and from a snapshot
    /// onto its image's own `/work`.
    fn workspace_over_image(&self) -> bool {
        true
    }
}

/// A volume a running sandbox holds is its, not the claim's copy: as a
/// runtime refuses to replace or remove a volume a container mounts.
fn refuse_live(volume: &str) -> Result<(), BoundaryError> {
    match crate::runner::sandbox_files::is_live(volume) {
        true => Err(BoundaryError::Other(format!(
            "volume {volume} is in use by a running sandbox"
        ))),
        false => Ok(()),
    }
}

/// Export a volume a running sandbox holds from the sandbox itself, checked
/// as it arrives. `false` when no sandbox holds it: the claim's copy is
/// then the one to read.
async fn export_live(
    volume: &str,
    destination: &std::path::Path,
    checkout: bool,
) -> Result<bool, BoundaryError> {
    match crate::runner::sandbox_files::export_live(volume, destination, checkout).await {
        None => Ok(false),
        Some(Err(e)) => Err(BoundaryError::Other(format!("copy out of {volume}: {e}"))),
        Some(Ok(_)) => {
            crate::workspace::validate_tree(destination)
                .map_err(|e| BoundaryError::Other(e.to_string()))?;
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &std::path::Path, script: &str) {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn backend(state: &std::path::Path) -> KubeBackend {
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg.runtime.kubernetes.state_mount = state.to_path_buf();
        KubeBackend::new(&cfg)
    }

    /// The node has a bundle to fetch and a base to pin, so it converges;
    /// every harness runs on the base; a workspace goes over a snapshot's own
    /// `/work`. Without a builder nothing is snapshotted.
    #[tokio::test]
    async fn every_harness_runs_on_the_base() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg.runtime.kubernetes.state_mount = tmp.path().to_path_buf();
        let protocol = KubeBackend::new(&cfg);
        assert!(protocol.converges());
        assert!(protocol
            .definitions(&cfg)
            .is_some_and(|d| d.contains(&cfg.sandbox.base_image_ref())));
        assert!(protocol.workspace_over_image());
        assert_eq!(
            crate::environment::harness_image(&cfg),
            crate::sandbox_images::base_image(&cfg)
        );
        assert!(protocol.preparer().is_none());
        assert!(
            !protocol
                .seeds_session_cache("tracon-cache-base-x", true)
                .await
        );
    }

    /// A node that builds DigitalOcean's templates fetches the release's
    /// browser bundle for them as it converges, whatever becomes of the
    /// base's pin; it keeps its trees where its own sandboxes do and hands
    /// out its builder for the templates.
    #[tokio::test]
    async fn a_digitalocean_template_builder_fetches_the_browser() {
        use crate::service_bundle::{self, CHROME_VERSION};
        let Some(arch) = crate::bundle::host_arch() else {
            return;
        };
        let release = tempfile::tempdir().unwrap();
        let published = release
            .path()
            .join(format!("v{}", env!("CARGO_PKG_VERSION")));
        std::fs::create_dir_all(&published).unwrap();
        service_bundle::tests::tarball(
            &published,
            CHROME_VERSION,
            arch,
            service_bundle::tests::FILES,
        );
        let (url, _) = crate::bundle::tests::serve(release.path().to_path_buf()).await;

        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg.runtime.kubernetes.state_mount = tmp.path().to_path_buf();
        cfg.runtime.kubernetes.builder = Some(crate::config::KubeBuilder {
            registry: "registry.digitalocean.com/acme".into(),
            ..Default::default()
        });
        // A bundle of the operator's, so nothing else is fetched, and a base
        // no pod could pull, so the pin fails.
        cfg.sandbox.bundle = Some(tmp.path().join("bundle"));
        cfg.sandbox.base_image = Some("localhost/spike-base:1".into());
        cfg.sandbox.release_url = Some(url);
        cfg.service = vec![crate::config::Service {
            name: "browser".into(),
            bundle: Some(service_bundle::RELEASE.into()),
            port: 9222,
            ..Default::default()
        }];
        cfg.digitalocean.credential = "do".into();
        cfg.digitalocean.image = String::new();
        cfg.digitalocean.registry = "registry.digitalocean.com/acme".into();
        cfg.placement.digitalocean = crate::provider::requirements::DoMode::Overflow;
        let browser = service_bundle::managed_dir(&Config::state_dir(), CHROME_VERSION, arch);
        let _ = std::fs::remove_dir_all(&browser);

        let backend = KubeBackend::new(&cfg);
        let pinned = backend.converge_images().await.unwrap_err();
        assert!(pinned.contains("only in a local image store"), "{pinned}");
        assert!(browser.join("run").is_file(), "{}", browser.display());
        let _ = std::fs::remove_dir_all(&browser);

        assert!(backend.sandbox_files_store().is_some());
        let builder = backend.registry_builder().expect("a builder");
        assert_eq!(
            builder.registry().as_deref(),
            Some("registry.digitalocean.com/acme")
        );
        assert_eq!(builder.scratch_dir(), Some(tmp.path().join(build::SCRATCH)));
    }

    /// Without `[runtime.kubernetes.builder]` nothing builds and the check
    /// says so. A table that cannot be a builder, and a builder whose daemon
    /// does not answer, are warnings in the check, and a build attempted
    /// anyway fails with the check's own reason. Builds stage on the state
    /// volume.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_builder_is_handed_out_only_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let unconfigured = backend(tmp.path());
        assert!(unconfigured.image_builder().is_none());
        assert!(unconfigured
            .builder_status()
            .await
            .contains("builder: not configured"));

        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg.runtime.kubernetes.builder = Some(crate::config::KubeBuilder {
            registry: "cosmicspork".into(),
            ..Default::default()
        });
        let invalid = KubeBackend::new(&cfg);
        let said = invalid.builder_status().await;
        assert!(said.contains("is not usable (registry must be"), "{said}");
        std::fs::write(tmp.path().join("Containerfile"), "FROM scratch\n").unwrap();
        async fn attempt(backend: &KubeBackend, dir: &std::path::Path) -> String {
            let containerfile = dir.join("Containerfile");
            backend
                .image_builder()
                .expect("a builder is handed out")
                .build(crate::boundary::ImageBuild {
                    tag: "localhost/tracon-repo-a:t",
                    containerfile: &containerfile,
                    context: dir,
                    labels: &[],
                    timeout: std::time::Duration::from_secs(30),
                })
                .await
                .unwrap_err()
                .error
        }
        assert_eq!(
            Some(attempt(&invalid, tmp.path()).await.as_str()),
            said.strip_prefix("builder: ")
        );

        let buildctl = tmp.path().join("buildctl");
        std::fs::write(
            &buildctl,
            format!(
                "#!/bin/sh\necho \"$TMPDIR\" > {}/tmpdir\necho 'error: connection refused' >&2\nexit 1\n",
                tmp.path().display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&buildctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        cfg.runtime.kubernetes.builder = Some(crate::config::KubeBuilder {
            registry: "registry.digitalocean.com/cosmicspork".into(),
            buildctl: buildctl.to_string_lossy().into_owned(),
            ..Default::default()
        });
        cfg.runtime.kubernetes.state_mount = tmp.path().join("state");
        let configured = KubeBackend::new(&cfg);
        assert_eq!(
            configured.image_builder().unwrap().scratch_dir(),
            Some(tmp.path().join("state/tmp"))
        );
        let said = configured.builder_status().await;
        assert!(
            said.contains("tcp://buildkitd.tracon-build.svc:1234 refused")
                && said.contains("connection refused")
                && said.ends_with("so a repository's image cannot be built"),
            "{said}"
        );
        assert_eq!(
            Some(attempt(&configured, tmp.path()).await.as_str()),
            said.strip_prefix("builder: ")
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("tmpdir"))
                .unwrap()
                .trim(),
            tmp.path().join("state/tmp").to_string_lossy()
        );
    }

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A workspace moves on and off the claim as Git would see it: scripts
    /// stay executable through an import and a clone, and an export after an
    /// install leaves `node_modules` (and its links) and `target` behind
    /// rather than refusing them.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_workspace_keeps_modes_and_leaves_build_output_on_the_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let backend = backend(&state);

        let snapshot = tmp.path().join("snapshot");
        std::fs::create_dir_all(&snapshot).unwrap();
        sh(
            &snapshot,
            "git init -q -b main . && git config user.email t@e && git config user.name t \
             && printf 'node_modules/\\ntarget/\\n' > .gitignore \
             && printf '#!/bin/sh\\n' > run.sh && chmod 555 run.sh && echo a > a.txt \
             && git add -A && git commit -qm one && chmod 444 a.txt",
        );
        backend
            .import_volume("tracon-workspace-a", &snapshot)
            .await
            .unwrap();
        let volume = state.join("tracon-workspace-a");
        assert_eq!(mode(&volume.join("run.sh")), 0o755);
        // Writable on the claim, though read-only in the snapshot.
        assert_eq!(mode(&volume.join("a.txt")), 0o644);

        backend
            .clone_volume("tracon-workspace-a", "tracon-check-b")
            .await
            .unwrap();
        assert_eq!(mode(&state.join("tracon-check-b/run.sh")), 0o755);

        // What a session's install and build leave behind, including a link
        // that leaves the tree.
        sh(
            &volume,
            "mkdir -p node_modules/.bin node_modules/acorn/bin target \
             && echo x > node_modules/acorn/bin/acorn \
             && ln -s ../acorn/bin/acorn node_modules/.bin/acorn \
             && ln -s /usr/bin/python3 target/python && echo new > b.txt",
        );
        assert!(backend
            .export_volume("tracon-workspace-a", &tmp.path().join("whole"))
            .await
            .is_err());
        assert!(
            !tmp.path().join("whole").exists(),
            "a refused export leaves nothing behind"
        );

        let exported = tmp.path().join("exported");
        backend
            .export_workspace("tracon-workspace-a", &exported)
            .await
            .unwrap();
        assert_eq!(mode(&exported.join("run.sh")), 0o755);
        assert_eq!(
            std::fs::read_to_string(exported.join("b.txt")).unwrap(),
            "new\n"
        );
        assert!(exported.join(".git/HEAD").is_file());
        assert!(!exported.join("node_modules").exists());
        assert!(!exported.join("target").exists());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("staging-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
