//! The pod-hosted boundary: the node is an unprivileged pod that creates one
//! harness pod per session and holds its stdio; two NetworkPolicies make the
//! node the harness's only route; the node serves the allowlist proxy itself.
//! Everything the node needs to exist (the claim, the policies, its own
//! labels and RBAC) is the deployment's, in `deploy/kubernetes/base`, and is
//! verified rather than created here.

pub mod checks;

use std::sync::Arc;

use async_trait::async_trait;
use rust_embed::Embed;

use super::{Backend, BoundaryError, BoundaryReport};
use crate::config::Config;
use crate::runner::kube::{KubeRunner, KubeSpec, PodEnv};
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
}

impl KubeBackend {
    pub fn new(cfg: &Config) -> Self {
        Self {
            cfg: cfg.clone(),
            env: PodEnv::detect(cfg),
            client: tokio::sync::OnceCell::new(),
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
        Ok(KubeSpec::from_config(&self.cfg, self.env.clone()?))
    }

    pub async fn runner_for(&self, extra_mounts: Vec<Mount>) -> Result<KubeRunner, String> {
        let mut spec = self.spec()?;
        spec.extra_mounts = extra_mounts;
        Ok(KubeRunner::new(self.client().await?, spec))
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
            "apply the manifests above (kubectl apply -k deploy/kubernetes/lab, or your template); \
             then `tracon check-boundary --deep` inside the node pod"
        );
        Ok(())
    }

    async fn check_all(&self, cfg: &Config, deep: bool) -> BoundaryReport {
        checks::check_all(self, cfg, deep).await
    }

    fn runner(&self, extra_mounts: Vec<Mount>) -> Arc<dyn Runner> {
        let client = match self.client.get() {
            Some(Ok(c)) => c.clone(),
            Some(Err(e)) => return Arc::new(Unavailable(e.clone())),
            None => {
                return Arc::new(Unavailable(
                    "kubernetes client not initialised (checks did not run)".into(),
                ))
            }
        };
        match self.spec() {
            Ok(mut spec) => {
                spec.extra_mounts = extra_mounts;
                Arc::new(KubeRunner::new(client, spec))
            }
            Err(e) => Arc::new(Unavailable(e)),
        }
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

    async fn reconcile(&self, names: &[String]) {
        let Ok(runner) = self.runner_for(Vec::new()).await else {
            return;
        };
        for name in names {
            let _ = runner.kill(name).await;
        }
    }
}
