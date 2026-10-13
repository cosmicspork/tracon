//! The pod-hosted boundary: the node is an unprivileged pod that creates one
//! harness pod per session and holds its stdio; two NetworkPolicies make the
//! node the harness's only route; the node serves the allowlist proxy itself.
//! Everything the node needs to exist (the claim, the policies, its own
//! labels and RBAC) is the deployment's, in `deploy/kubernetes/base`, and is
//! verified rather than created here.
//!
//! The node also serves the per-client egress proxy (`gateway::proxy::
//! serve_granted`), on `[gateway] egress_port`, which a harness pod reaches at
//! the node's pod address under `gateway_host`. A grant is issued only when
//! both halves are known to hold: the policies let a harness pod reach that
//! port, and the node may keep the grant's credentials in a Secret of the
//! run's own rather than in its pod spec.

pub mod build;
pub mod checks;

use std::sync::Arc;

use async_trait::async_trait;
use rust_embed::Embed;

use super::{Backend, BoundaryError, BoundaryReport, EgressGrant};
use crate::config::Config;
use crate::gateway::proxy::{GrantSpec, Grants, Refusal};
use crate::runner::kube::{KubeRunner, KubeSpec, PodEnv, SecretRights};
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
    /// Whether the node may keep a run's secret environment in a Secret, as
    /// the runtime check last found. Every runner handed out shares it, so a
    /// re-check that finds the rights granted (or withdrawn) applies to runs
    /// that start after it.
    secret_rights: SecretRights,
    /// `[runtime.kubernetes.builder]`, or why it is not a builder. `None`
    /// where none is configured.
    builder: Option<Result<build::BuildKitBuilder, build::Unusable>>,
    /// Every live per-client grant, served on the egress port.
    grants: Grants,
    /// Whether the policies let a harness pod reach the egress port, as the
    /// network check last found. Unchecked is closed.
    egress_port: EgressPort,
    /// What the egress proxy never dials here; an error for an
    /// `internal_cidrs` entry that is not an address block, which turns
    /// per-client egress off and fails the runtime check.
    refusal: Result<Refusal, String>,
}

/// The builder `[runtime.kubernetes.builder]` configures, if any, staging on
/// the state volume.
fn builder_for(cfg: &Config) -> Option<Result<build::BuildKitBuilder, build::Unusable>> {
    let settings = cfg.runtime.kubernetes.builder.as_ref()?;
    let scratch = cfg.runtime.kubernetes.state_mount.join(build::SCRATCH);
    Some(
        build::BuildKitBuilder::new(settings, scratch).map_err(|why| {
            build::Unusable(format!(
                "[runtime.kubernetes.builder] is not usable ({why})"
            ))
        }),
    )
}

/// Whether a harness pod can reach the node's egress port: the harness
/// policy's egress and the node's own ingress both allow it. Shared, like
/// `SecretRights`, so a re-check that finds it opened (or closed) applies to
/// the grants issued after it.
#[derive(Clone, Debug, Default)]
pub struct EgressPort(std::sync::Arc<parking_lot::RwLock<Option<Result<(), String>>>>);

impl EgressPort {
    /// Open, as if a check had found it so. For tests.
    pub fn open() -> Self {
        let port = Self::default();
        port.set(Ok(()));
        port
    }

    pub fn set(&self, verdict: Result<(), String>) {
        *self.0.write() = Some(verdict);
    }

    pub fn verdict(&self) -> Option<Result<(), String>> {
        self.0.read().clone()
    }

    pub fn is_open(&self) -> bool {
        matches!(self.verdict(), Some(Ok(())))
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
        Self {
            cfg: cfg.clone(),
            env,
            client,
            secret_rights: SecretRights::default(),
            grants: Grants::default(),
            egress_port: EgressPort::default(),
            refusal,
            builder: builder_for(cfg),
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

    pub fn secret_rights(&self) -> &SecretRights {
        &self.secret_rights
    }

    pub fn egress_port(&self) -> &EgressPort {
        &self.egress_port
    }

    /// The `internal_cidrs` entry that is not an address block, if one is.
    pub fn refusal_error(&self) -> Option<&str> {
        self.refusal.as_ref().err().map(String::as_str)
    }

    /// Why no grant can be issued right now, naming what to change; `None`
    /// when one can.
    pub fn egress_unavailable(&self) -> Option<String> {
        if let Err(error) = &self.refusal {
            return Some(format!("per-client egress is off on this node: {error}"));
        }
        let secrets = match self.secret_rights.verdict() {
            Some(Ok(())) => None,
            Some(Err(why)) => Some(why),
            None => Some("the boundary check has not yet confirmed them".to_string()),
        };
        if let Some(why) = secrets {
            return Some(format!(
                "per-client egress is off on this node: a grant's credentials travel in a \
                 Secret of the run's own, and this node may not manage Secrets ({why}). Grant \
                 the node's Role create, list, patch and delete on secrets \
                 (deploy/kubernetes/base/role.yaml) and check the boundary again"
            ));
        }
        match self.egress_port.verdict() {
            Some(Ok(())) => None,
            Some(Err(why)) => Some(format!("per-client egress is off on this node: {why}")),
            None => Some(
                "per-client egress is off on this node: the boundary check has not yet \
                 confirmed a harness pod can reach the egress port"
                    .into(),
            ),
        }
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
                Arc::new(
                    KubeRunner::new(client, spec).with_secret_rights(self.secret_rights.clone()),
                )
            }
            Err(e) => Arc::new(Unavailable(e)),
        }
    }

    pub async fn runner_for(&self, extra_mounts: Vec<Mount>) -> Result<KubeRunner, String> {
        let mut spec = self.spec()?;
        spec.extra_mounts = extra_mounts;
        Ok(KubeRunner::new(self.client().await?, spec)
            .with_secret_rights(self.secret_rights.clone()))
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

    fn image_builder(&self) -> Option<&dyn super::ImageBuilder> {
        match &self.builder {
            Some(Ok(builder)) => Some(builder),
            Some(Err(unusable)) => Some(unusable),
            None => None,
        }
    }

    fn harness_image(&self, cfg: &Config, harness_id: &str) -> String {
        cfg.kubernetes_harness_image(harness_id)
    }

    /// Pods pull on use, so a cluster names each image and says nothing of it.
    async fn harness_images(&self, cfg: &Config) -> Vec<crate::boundary::HarnessImage> {
        crate::adapter::KNOWN
            .iter()
            .map(|id| crate::boundary::HarnessImage {
                harness_id: id.to_string(),
                image: cfg.kubernetes_harness_image(id),
                state: "unknown",
            })
            .collect()
    }

    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        Backend::runner_for(self, &self.cfg.harness.id, extra_mounts)
    }

    fn runner_for(&self, harness_id: &str, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        self.runner_with(harness_id, extra_mounts, |_| {})
    }

    /// As Podman's: the harness's spec with the session image in place of
    /// the harness image. The session image's own init comes in, and the
    /// harness's entrypoint stays: `repo_image` copies both into every image
    /// it layers for a pod, the entrypoint (OpenCode's, which seeds its
    /// caches) at the path the harness image has it. Only the image changes;
    /// a check or a preparation naming another image still gets neither.
    fn runner_in(
        &self,
        harness_id: &str,
        image: &str,
        extra_mounts: Vec<Mount>,
    ) -> Arc<dyn Runner> {
        self.runner_with(harness_id, extra_mounts, |spec| {
            if spec.image != image {
                spec.image = image.to_string();
                spec.entrypoint = crate::runner::toolchain::image_entrypoint(harness_id);
                spec.init = Some(crate::runner::kube::SESSION_INIT.to_string());
            }
        })
    }

    /// A session runs in its repository's image wherever that image can be
    /// built and layered, which takes `[runtime.kubernetes.builder]`. One
    /// that is configured but unusable still says so through the session's
    /// note, rather than as "this runtime cannot".
    fn runs_sessions_in_repository_images(&self) -> bool {
        self.builder.is_some()
    }

    async fn import_volume(
        &self,
        volume: &str,
        source: &std::path::Path,
    ) -> Result<(), BoundaryError> {
        crate::workspace::validate_tree(source).map_err(|e| BoundaryError::Other(e.to_string()))?;
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
        self.cfg.runtime.kubernetes.harness_home.clone()
    }

    fn proxy_port(&self) -> Option<u16> {
        Some(self.cfg.gateway.proxy_port)
    }

    /// Always served, so a release whose policies open the port later needs
    /// no restart to use it: a grant is what opens anything, and none is
    /// issued until the checks say a harness pod can reach the port.
    fn egress_grants(&self) -> Option<&Grants> {
        Some(&self.grants)
    }

    /// As Podman's, with the node's own pod address as the proxy: a harness
    /// pod reaches it directly under `gateway_host`, with no gateway
    /// container between. The grant's URL carries its password, so it goes
    /// in the run's `secret_env` and from there in a Secret of the run's own
    /// (`runner::kube::secret`); a node that may not do that issues none.
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

    fn secret_env_refusal(&self) -> Option<String> {
        let why = match self.secret_rights.verdict() {
            Some(Ok(())) => return None,
            Some(Err(why)) => why,
            None => "the boundary check has not yet confirmed them".into(),
        };
        Some(format!(
            "refusing to start a session on Kubernetes: its gateway token and egress grant \
             travel in a Secret of its own, and this node may not manage Secrets ({why}). \
             Grant the node's Role create, list, patch and delete on secrets \
             (deploy/kubernetes/base/role.yaml) and check the boundary again; they are never \
             put in the pod spec instead"
        ))
    }

    /// Remove the named harnesses, and with them any run Secret a node that
    /// died left behind (`KubeRunner::sweep_run_secrets`).
    async fn reconcile(&self, names: &[String]) {
        let Ok(runner) = self.runner_for(Vec::new()).await else {
            return;
        };
        for name in names {
            let _ = runner.kill(name).await;
        }
        let swept = runner.sweep_run_secrets().await;
        if swept > 0 {
            tracing::info!(swept, "removed run secrets left by an earlier node");
        }
    }

    // A volume here is a directory on the shared state claim. Nothing stops
    // removing one a pod still mounts, so the caller only removes volumes
    // whose owner is over.
    async fn list_volumes(&self) -> Result<Vec<crate::boundary::VolumeInfo>, BoundaryError> {
        crate::boundary::directory_volumes(&self.cfg.runtime.kubernetes.state_mount)
    }

    async fn remove_volume(&self, volume: &str) -> Result<(), BoundaryError> {
        crate::boundary::remove_directory_volume(&self.cfg.runtime.kubernetes.state_mount, volume)
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

    /// Without `[runtime.kubernetes.builder]` nothing builds and the check
    /// says so. A table that cannot be a builder, and a builder whose daemon
    /// does not answer, are warnings in the check, and a build attempted
    /// anyway fails with the check's own reason. Builds stage on the state
    /// volume. Layered images copy the harness from the release's images.
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
        let cfg = Config::default();
        assert_eq!(
            Backend::harness_image(&unconfigured, &cfg, "claude"),
            format!(
                "ghcr.io/cosmicspork/tracon-harness-claude:{}",
                env!("CARGO_PKG_VERSION")
            )
        );

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
        // A session runs in its repository's image once a builder can make
        // one, and in its harness's own without one.
        assert!(Backend::runs_sessions_in_repository_images(&configured));
        assert!(Backend::runs_sessions_in_repository_images(&invalid));
        assert!(!Backend::runs_sessions_in_repository_images(&unconfigured));
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
