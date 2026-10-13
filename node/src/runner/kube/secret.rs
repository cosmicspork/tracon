//! The per-run Secret: how environment that must not be readable from a pod
//! spec reaches a harness or a captured command on Kubernetes.
//!
//! A pod's `env[].value` is plaintext to anyone who may `get pods` in the
//! namespace, and to every audit log and `kubectl describe` that sees the
//! pod. So a command's `secret_env` goes into a Secret of the run's own,
//! `<pod>-env`, and the pod names each key with `secretKeyRef`:
//!
//! 1. The Secret is created first, immutable, labelled with the pod it is
//!    for. A pod that names a missing Secret does not start
//!    (`CreateContainerConfigError`), so the order is not a choice.
//! 2. The pod is created and the Secret's `ownerReferences` patched to the
//!    pod's uid, so the cluster's garbage collector removes it with the pod
//!    if this node dies before it can.
//! 3. Once the pod has started, the kubelet has read the values into the
//!    container's environment and nothing reads the Secret again (pods run
//!    with `restartPolicy: Never`), so it is deleted then rather than when
//!    the run ends. A run that ends before it starts (a failure, a kill, a
//!    dropped future) deletes it on the way out.
//! 4. `sweep` removes what a dead node left: a Secret whose pod is gone or
//!    has already started.
//!
//! None of this happens without the rights to do it. A node whose Role does
//! not let it create, patch, list and delete Secrets refuses a run that
//! carries secret environment; it never falls back to a plaintext value.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use k8s_openapi::api::core::v1::{EnvVar, EnvVarSource, Pod, Secret, SecretKeySelector};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::ByteString;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::Api;

use super::KubeRunner;
use crate::runner::{RunnerCommand, RunnerError};

/// On a run's Secret: the name of the pod it is for.
pub const RUN_LABEL: &str = "tracon.dev/run";
/// On a run's Secret: the node process that made it, for whoever reads the
/// namespace. Two node processes can share one.
pub const PID_LABEL: &str = "tracon.dev/pid";

/// How old a Secret whose pod does not exist must be before a sweep takes it.
/// A run creates its Secret a moment before its pod, and another node
/// process sharing the namespace may be in that moment.
pub const SWEEP_GRACE: Duration = Duration::from_secs(120);

/// The Secret a pod's secret environment lives in.
pub fn secret_name(pod: &str) -> String {
    format!("{pod}-env")
}

/// What the boundary check found about this node's rights over Secrets.
/// Shared between the backend, which learns it, and every runner it hands
/// out, which refuses a run carrying secret environment until it is known to
/// be granted. Unchecked is a refusal too.
#[derive(Clone, Debug, Default)]
pub struct SecretRights(Arc<RwLock<Option<Result<(), String>>>>);

impl SecretRights {
    /// Rights already known to be held. For tests and for a runner built
    /// against a cluster that was checked some other way.
    pub fn granted() -> Self {
        let rights = Self::default();
        rights.set(Ok(()));
        rights
    }

    pub fn set(&self, verdict: Result<(), String>) {
        if let Ok(mut held) = self.0.write() {
            *held = Some(verdict);
        }
    }

    pub fn verdict(&self) -> Option<Result<(), String>> {
        self.0.read().ok().and_then(|held| held.clone())
    }

    pub fn is_granted(&self) -> bool {
        matches!(self.verdict(), Some(Ok(())))
    }

    fn require(&self) -> Result<(), RunnerError> {
        match self.verdict() {
            Some(Ok(())) => Ok(()),
            Some(Err(why)) => Err(refusal(&why)),
            None => Err(refusal("the boundary check has not yet confirmed them")),
        }
    }
}

fn refusal(why: &str) -> RunnerError {
    RunnerError::Other(format!(
        "refusing a run that carries credentials: it needs a Secret of its own and this \
         node may not manage Secrets ({why}). Grant the node's Role create, list, patch \
         and delete on secrets (deploy/kubernetes/base/role.yaml) and check the boundary \
         again; the credentials are never put in the pod spec instead"
    ))
}

/// Each secret variable once, in the order first given.
fn unique_names(cmd: &RunnerCommand) -> Vec<&str> {
    let mut seen = std::collections::BTreeSet::new();
    cmd.secret_env
        .names()
        .filter(|name| seen.insert(*name))
        .collect()
}

/// The pod's references to its Secret: one `secretKeyRef` per variable, the
/// key named as the variable. No value is ever rendered here.
pub(super) fn env_refs(pod: &str, cmd: &RunnerCommand) -> Vec<EnvVar> {
    let secret = secret_name(pod);
    unique_names(cmd)
        .into_iter()
        .map(|name| EnvVar {
            name: name.to_string(),
            value: None,
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: secret.clone(),
                    key: name.to_string(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
        })
        .collect()
}

/// The Secret for `pod`'s run. Immutable: nothing should change what a
/// started pod was given, and an immutable Secret is not watched by the
/// kubelet. A name given twice keeps its last value, as an environment does.
pub fn render(namespace: &str, pod: &str, cmd: &RunnerCommand) -> Secret {
    let data = cmd
        .secret_env
        .iter()
        .map(|(name, value)| (name.clone(), ByteString(value.clone().into_bytes())))
        .collect::<BTreeMap<_, _>>();
    Secret {
        metadata: ObjectMeta {
            name: Some(secret_name(pod)),
            namespace: Some(namespace.to_string()),
            labels: Some(BTreeMap::from([
                (RUN_LABEL.to_string(), pod.to_string()),
                (PID_LABEL.to_string(), std::process::id().to_string()),
            ])),
            ..Default::default()
        },
        immutable: Some(true),
        type_: Some("Opaque".into()),
        data: Some(data),
        ..Default::default()
    }
}

/// `text` with every secret value of `cmd` taken out. An API error about a
/// Secret is not expected to quote its data, and is not trusted not to.
fn redacted(cmd: &RunnerCommand, text: String) -> String {
    cmd.secret_env
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .fold(text, |text, (_, value)| text.replace(value, "[redacted]"))
}

fn forbidden(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(status) if status.code == 403)
}

/// One run's Secret, deleted when this is released or dropped.
pub(super) struct RunEnv {
    secrets: Api<Secret>,
    name: String,
    armed: bool,
}

impl RunEnv {
    /// Create the Secret for `pod`, or nothing when the command carries no
    /// secret environment. Refused outright when the node may not manage
    /// Secrets.
    async fn create(
        secrets: Api<Secret>,
        rights: &SecretRights,
        namespace: &str,
        pod: &str,
        cmd: &RunnerCommand,
    ) -> Result<Option<Self>, RunnerError> {
        if cmd.secret_env.is_empty() {
            return Ok(None);
        }
        rights.require()?;
        cmd.secret_env.validate()?;
        let name = secret_name(pod);
        // One left by an earlier run of the same pod name would refuse this
        // creation; it belongs to nothing running, since the pod of that name
        // has just been removed.
        let _ = secrets.delete(&name, &DeleteParams::default()).await;
        match secrets
            .create(&PostParams::default(), &render(namespace, pod, cmd))
            .await
        {
            Ok(_) => Ok(Some(Self {
                secrets,
                name,
                armed: true,
            })),
            Err(error) if forbidden(&error) => {
                let why = format!("may not create secrets in {namespace}");
                rights.set(Err(why.clone()));
                Err(refusal(&why))
            }
            Err(error) => Err(RunnerError::Other(redacted(
                cmd,
                format!("create secret {name}: {error}"),
            ))),
        }
    }

    /// Make the pod the Secret's owner, so the cluster removes the Secret
    /// with the pod when nothing on this side is left to.
    async fn own(&self, pod: &Pod) -> Result<(), RunnerError> {
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let uid = pod.metadata.uid.clone().ok_or_else(|| {
            RunnerError::Other(format!("pod {pod_name} was created without a uid"))
        })?;
        let patch = serde_json::json!({
            "metadata": {
                "ownerReferences": [OwnerReference {
                    api_version: "v1".into(),
                    kind: "Pod".into(),
                    name: pod_name,
                    uid,
                    ..Default::default()
                }]
            }
        });
        self.secrets
            .patch(&self.name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
            .map(|_| ())
            .map_err(|e| RunnerError::Other(format!("own secret {}: {e}", self.name)))
    }

    async fn remove(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if let Err(error) = self
            .secrets
            .delete(&self.name, &DeleteParams::default())
            .await
        {
            if !matches!(&error, kube::Error::Api(status) if status.is_not_found()) {
                tracing::warn!(secret = %self.name, %error, "run secret left for the sweep");
            }
        }
    }
}

impl Drop for RunEnv {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let secrets = self.secrets.clone();
            let name = std::mem::take(&mut self.name);
            runtime.spawn(async move {
                let _ = secrets.delete(&name, &DeleteParams::default()).await;
            });
        }
    }
}

/// Delete a run's Secret, if it holds one.
pub(super) async fn release(env: &mut Option<RunEnv>) {
    if let Some(env) = env.as_mut() {
        env.remove().await;
    }
}

impl KubeRunner {
    /// What this runner learns, from its backend, about its rights over
    /// Secrets. A runner given none refuses every run carrying secret
    /// environment.
    pub fn with_secret_rights(mut self, rights: SecretRights) -> Self {
        self.secret_rights = rights;
        self
    }

    pub fn secrets(&self) -> Api<Secret> {
        Api::namespaced(self.client.clone(), &self.spec.env.namespace)
    }

    /// Create `pod`, and before it the run's Secret when the command carries
    /// secret environment, owned by the pod once it exists. Whatever fails
    /// takes the other with it. The guard returned deletes the Secret.
    pub(super) async fn create_pod(
        &self,
        name: &str,
        pod: &Pod,
        cmd: &RunnerCommand,
    ) -> Result<Option<RunEnv>, RunnerError> {
        let mut env = RunEnv::create(
            self.secrets(),
            &self.secret_rights,
            &self.spec.env.namespace,
            name,
            cmd,
        )
        .await?;
        let created = match self.pods().create(&PostParams::default(), pod).await {
            Ok(created) => created,
            Err(e) => {
                release(&mut env).await;
                return Err(RunnerError::Other(format!("create pod {name}: {e}")));
            }
        };
        if let Some(guard) = env.as_mut() {
            if let Err(e) = guard.own(&created).await {
                self.remove(name).await;
                guard.remove().await;
                return Err(e);
            }
        }
        Ok(env)
    }

    /// Delete the Secret a run of `pod` may hold. For a kill, which is given
    /// only a name.
    pub(super) async fn remove_env(&self, pod: &str) {
        if !self.secret_rights.is_granted() {
            return;
        }
        let _ = self
            .secrets()
            .delete(&secret_name(pod), &DeleteParams::default())
            .await;
    }

    /// Remove the run Secrets nothing needs any more: those whose pod has
    /// started (the kubelet read them then) and those whose pod is gone and
    /// that are older than `SWEEP_GRACE`. Returns how many it removed. A
    /// node that may not list Secrets has made none to sweep.
    pub async fn sweep_run_secrets(&self) -> usize {
        let secrets = self.secrets();
        let listed = match secrets
            .list_metadata(&ListParams::default().labels(RUN_LABEL))
            .await
        {
            Ok(listed) => listed,
            Err(error) => {
                tracing::debug!(%error, "run secrets not swept");
                return 0;
            }
        };
        let pods = self.pods();
        let now = k8s_openapi::jiff::Timestamp::now().as_second();
        let mut removed = 0;
        for item in listed.items {
            let meta = item.metadata;
            let (Some(name), Some(pod)) = (
                meta.name.clone(),
                meta.labels.as_ref().and_then(|l| l.get(RUN_LABEL)).cloned(),
            ) else {
                continue;
            };
            let old = meta
                .creation_timestamp
                .as_ref()
                .is_none_or(|t| now - t.0.as_second() >= SWEEP_GRACE.as_secs() as i64);
            let unneeded = match pods.get_opt(&pod).await {
                Ok(None) => old,
                Ok(Some(pod)) => pod
                    .status
                    .and_then(|s| s.phase)
                    .is_some_and(|phase| phase != "Pending"),
                Err(_) => false,
            };
            if unneeded
                && secrets
                    .delete(&name, &DeleteParams::default())
                    .await
                    .is_ok()
            {
                removed += 1;
            }
        }
        removed
    }
}
