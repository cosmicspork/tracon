//! The Kubernetes runner: one harness Pod per session, created through the
//! API by a node that is itself a pod, its stdio held by the node over
//! `pods/attach`. The single place a harness pod is rendered; the boundary
//! checks introspect the same rendering, so what is verified is what runs.
//!
//! Isolation is not in this file. It lives in the NetworkPolicies the
//! deployment carries (`deploy/kubernetes/base`), which the checks verify,
//! and in the pod's security context, which is rendered here and verified
//! after admission.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EnvVar, HostAlias, PersistentVolumeClaimVolumeSource, Pod,
    PodDNSConfig, PodSchedulingGate, PodSecurityContext, PodSpec, SeccompProfile, SecurityContext,
    Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{AttachParams, DeleteParams, LogParams, PostParams};
use kube::{Api, Client};

use super::{Mount, Runner, RunnerCommand, RunnerError, Spawned};
use crate::config::Config;

pub const ROLE_LABEL: &str = "tracon.dev/role";
pub const SESSION_LABEL: &str = "tracon.dev/session";
pub const PROBE_GATE: &str = "tracon.dev/probe";
pub const STATE_VOLUME: &str = "state";

/// What the node learns about its own pod from the downward API. The
/// deployment sets these; a node without them is not running as a pod.
#[derive(Debug, Clone)]
pub struct PodEnv {
    pub namespace: String,
    pub pod_ip: String,
    pub node_name: String,
}

impl PodEnv {
    pub fn detect(cfg: &Config) -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let namespace = if cfg.runtime.kubernetes.namespace.is_empty() {
            var("TRACON_NAMESPACE")
                .or_else(|| {
                    std::fs::read_to_string(
                        "/var/run/secrets/kubernetes.io/serviceaccount/namespace",
                    )
                    .ok()
                    .map(|s| s.trim().to_string())
                })
                .ok_or(
                    "namespace unknown: set TRACON_NAMESPACE or [runtime.kubernetes] namespace",
                )?
        } else {
            cfg.runtime.kubernetes.namespace.clone()
        };
        Ok(Self {
            namespace,
            pod_ip: var("TRACON_POD_IP")
                .ok_or("TRACON_POD_IP unset (downward API status.podIP)")?,
            node_name: var("TRACON_NODE_NAME")
                .ok_or("TRACON_NODE_NAME unset (downward API spec.nodeName)")?,
        })
    }
}

/// Everything that puts a harness inside a pod behind the boundary.
#[derive(Debug, Clone)]
pub struct KubeSpec {
    pub image: String,
    pub state_claim: String,
    pub state_mount: PathBuf,
    pub home: String,
    pub uid: i64,
    pub gateway_host: String,
    pub proxy_port: u16,
    pub env: PodEnv,
    pub extra_mounts: Vec<Mount>,
    pub workdir: String,
}

impl KubeSpec {
    pub fn from_config(cfg: &Config, env: PodEnv) -> Self {
        let k = &cfg.runtime.kubernetes;
        Self {
            image: k.harness_image.clone(),
            state_claim: k.state_claim.clone(),
            state_mount: k.state_mount.clone(),
            home: k.harness_home.clone(),
            uid: k.uid,
            gateway_host: k.gateway_host.clone(),
            proxy_port: cfg.gateway.proxy_port,
            env,
            extra_mounts: Vec::new(),
            workdir: "/work".into(),
        }
    }

    /// A host path on the shared volume, as a `subPath` under the claim. A
    /// source outside the volume cannot be given to a pod at all, which is
    /// the point: nothing of the node's is reachable except what it put on
    /// the volume for this session.
    fn sub_path(&self, source: &str) -> Result<String, RunnerError> {
        Path::new(source)
            .strip_prefix(&self.state_mount)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|_| {
                RunnerError::Other(format!(
                    "mount source {source} is outside the state volume {}",
                    self.state_mount.display()
                ))
            })
    }

    fn volume_mounts(&self, cmd: &RunnerCommand) -> Result<Vec<VolumeMount>, RunnerError> {
        self.extra_mounts
            .iter()
            .chain(cmd.mounts.iter())
            .map(|m| {
                Ok(VolumeMount {
                    name: STATE_VOLUME.into(),
                    mount_path: m.target.clone(),
                    sub_path: Some(self.sub_path(&m.source)?),
                    read_only: Some(m.read_only),
                    ..Default::default()
                })
            })
            .collect()
    }

    /// Render the pod. `gated` adds a scheduling gate so the pod is admitted
    /// (and therefore mutated by whatever admission the cluster runs) but never
    /// scheduled: the boundary checks read the result and delete it.
    pub fn pod(&self, name: &str, cmd: &RunnerCommand, gated: bool) -> Result<Pod, RunnerError> {
        let proxy = format!("http://{}:{}", self.gateway_host, self.proxy_port);
        let state = crate::session::materialize::state_target(&self.home);
        let mut env = vec![
            ("HOME", self.home.clone()),
            ("HTTPS_PROXY", proxy.clone()),
            ("HTTP_PROXY", proxy),
            ("NO_PROXY", self.gateway_host.clone()),
            ("OMP_STATE_DIR", state),
        ]
        .into_iter()
        .map(|(k, v)| EnvVar {
            name: k.into(),
            value: Some(v),
            ..Default::default()
        })
        .collect::<Vec<_>>();
        env.extend(cmd.env.iter().map(|(k, v)| EnvVar {
            name: k.clone(),
            value: Some(v.clone()),
            ..Default::default()
        }));
        let mut labels = std::collections::BTreeMap::new();
        labels.insert(ROLE_LABEL.to_string(), "harness".to_string());
        labels.insert(SESSION_LABEL.to_string(), name.to_string());
        let workdir = cmd.workdir.clone().unwrap_or_else(|| self.workdir.clone());
        Ok(Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(self.env.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".into()),
                // No API token, no service env, no resolver: the harness has
                // nothing to discover and no name to look up. The one name it
                // needs resolves to the node's own pod.
                automount_service_account_token: Some(false),
                enable_service_links: Some(false),
                dns_policy: Some("None".into()),
                dns_config: Some(PodDNSConfig {
                    nameservers: Some(vec!["127.0.0.1".into()]),
                    ..Default::default()
                }),
                host_aliases: Some(vec![HostAlias {
                    ip: self.env.pod_ip.clone(),
                    hostnames: Some(vec![self.gateway_host.clone()]),
                }]),
                // The state claim is ReadWriteOnce, so the harness must land
                // on the node's own Kubernetes node. A gated pod may not carry
                // `nodeName` (the API refuses it until the gates clear), so the
                // probe pins by hostname label instead; what the checks read
                // from it — privilege, mounts, network — is unaffected.
                node_name: (!gated).then(|| self.env.node_name.clone()),
                node_selector: gated.then(|| {
                    std::collections::BTreeMap::from([(
                        "kubernetes.io/hostname".to_string(),
                        self.env.node_name.clone(),
                    )])
                }),
                scheduling_gates: gated.then(|| {
                    vec![PodSchedulingGate {
                        name: PROBE_GATE.into(),
                    }]
                }),
                security_context: Some(PodSecurityContext {
                    run_as_non_root: Some(true),
                    run_as_user: Some(self.uid),
                    run_as_group: Some(self.uid),
                    fs_group: Some(self.uid),
                    seccomp_profile: Some(SeccompProfile {
                        type_: "RuntimeDefault".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                volumes: Some(vec![Volume {
                    name: STATE_VOLUME.into(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: self.state_claim.clone(),
                        read_only: Some(false),
                    }),
                    ..Default::default()
                }]),
                containers: vec![Container {
                    name: "harness".into(),
                    image: Some(self.image.clone()),
                    image_pull_policy: Some("IfNotPresent".into()),
                    command: Some(cmd.argv.clone()),
                    working_dir: Some(workdir),
                    stdin: Some(true),
                    stdin_once: Some(true),
                    env: Some(env),
                    volume_mounts: Some(self.volume_mounts(cmd)?),
                    // The gate: no capabilities, no way to gain any.
                    security_context: Some(SecurityContext {
                        privileged: Some(false),
                        allow_privilege_escalation: Some(false),
                        capabilities: Some(Capabilities {
                            drop: Some(vec!["ALL".into()]),
                            add: None,
                        }),
                        run_as_non_root: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

pub struct KubeRunner {
    client: Client,
    spec: KubeSpec,
}

impl KubeRunner {
    pub fn new(client: Client, spec: KubeSpec) -> Self {
        Self { client, spec }
    }

    pub fn spec(&self) -> &KubeSpec {
        &self.spec
    }

    pub fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.spec.env.namespace)
    }

    fn name_for(cmd: &RunnerCommand, fallback: &str) -> String {
        if cmd.name.is_empty() {
            fallback.to_string()
        } else {
            cmd.name.clone()
        }
    }

    /// Wait for a phase the caller can act on. Pulling the image is the slow
    /// part on a fresh node, so this is generous.
    async fn wait_for(
        &self,
        name: &str,
        want: &[&str],
        timeout: Duration,
    ) -> Result<String, RunnerError> {
        let pods = self.pods();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let pod = pods
                .get(name)
                .await
                .map_err(|e| RunnerError::Other(format!("get pod {name}: {e}")))?;
            let status = pod.status.unwrap_or_default();
            let phase = status.phase.clone().unwrap_or_default();
            if want.contains(&phase.as_str()) {
                return Ok(phase);
            }
            if phase == "Failed" || phase == "Succeeded" {
                return Ok(phase);
            }
            // A pod that cannot start says why in its container state; surface
            // that rather than a bare timeout.
            if let Some(reason) = status
                .container_statuses
                .as_deref()
                .and_then(|cs| cs.first())
                .and_then(|c| c.state.as_ref())
                .and_then(|s| s.waiting.as_ref())
                .and_then(|w| w.reason.clone())
            {
                if matches!(
                    reason.as_str(),
                    "ErrImagePull"
                        | "ImagePullBackOff"
                        | "CreateContainerConfigError"
                        | "InvalidImageName"
                ) {
                    return Err(RunnerError::Other(format!("pod {name}: {reason}")));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(RunnerError::Other(format!(
                    "pod {name} did not reach {} in {}s (phase {phase})",
                    want.join("/"),
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn remove(&self, name: &str) {
        let _ = self
            .pods()
            .delete(name, &DeleteParams::default().grace_period(0))
            .await;
    }
}

#[async_trait]
impl Runner for KubeRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        let name = Self::name_for(&cmd, "tracon-h");
        let pod = self.spec.pod(&name, &cmd, false)?;
        let pods = self.pods();
        self.remove(&name).await;
        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| RunnerError::Other(format!("create pod {name}: {e}")))?;
        if let Err(e) = self
            .wait_for(&name, &["Running"], Duration::from_secs(180))
            .await
        {
            self.remove(&name).await;
            return Err(e);
        }
        let mut attached = match pods
            .attach(
                &name,
                &AttachParams::default()
                    .stdin(true)
                    .stdout(true)
                    .stderr(false)
                    .tty(false),
            )
            .await
        {
            Ok(a) => a,
            Err(e) => {
                self.remove(&name).await;
                return Err(RunnerError::Other(format!("attach {name}: {e}")));
            }
        };
        let stdin = attached
            .stdin()
            .ok_or_else(|| RunnerError::Other("attach gave no stdin".into()))?;
        let stdout = attached
            .stdout()
            .ok_or_else(|| RunnerError::Other("attach gave no stdout".into()))?;
        let pods_for_exit = self.pods();
        let exit_name = name.clone();
        Ok(Spawned {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            done: Box::pin(async move {
                let _ = attached.join().await;
                let code = pods_for_exit
                    .get(&exit_name)
                    .await
                    .ok()
                    .and_then(|p| p.status)
                    .and_then(|s| s.container_statuses)
                    .and_then(|cs| cs.into_iter().next())
                    .and_then(|c| c.state)
                    .and_then(|s| s.terminated)
                    .map(|t| t.exit_code)
                    .unwrap_or(-1);
                let _ = pods_for_exit
                    .delete(&exit_name, &DeleteParams::default().grace_period(0))
                    .await;
                Ok(code)
            }),
        })
    }

    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        use std::os::unix::process::ExitStatusExt;
        let name = format!(
            "{}-{}",
            Self::name_for(&cmd, "tracon-x"),
            std::process::id()
        );
        let mut pod = self.spec.pod(&name, &cmd, false)?;
        if let Some(c) = pod.spec.as_mut().and_then(|s| s.containers.first_mut()) {
            c.stdin = None;
            c.stdin_once = None;
        }
        let pods = self.pods();
        self.remove(&name).await;
        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| RunnerError::Other(format!("create pod {name}: {e}")))?;
        let phase = self
            .wait_for(&name, &["Succeeded", "Failed"], Duration::from_secs(180))
            .await;
        let result = match phase {
            Ok(phase) => {
                let logs = pods
                    .logs(&name, &LogParams::default())
                    .await
                    .unwrap_or_default();
                let code = if phase == "Succeeded" { 0 } else { 1 };
                Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(code << 8),
                    stdout: logs.into_bytes(),
                    stderr: Vec::new(),
                })
            }
            Err(e) => Err(e),
        };
        self.remove(&name).await;
        result
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        self.remove(name).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> KubeSpec {
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        KubeSpec::from_config(
            &cfg,
            PodEnv {
                namespace: "tracon-lab".into(),
                pod_ip: "10.244.0.9".into(),
                node_name: "general-1".into(),
            },
        )
    }

    fn cmd() -> RunnerCommand {
        RunnerCommand {
            argv: vec!["omp".into(), "acp".into()],
            name: "tracon-h-1".into(),
            mounts: vec![
                Mount {
                    source: "/state/work/repo-x".into(),
                    target: "/work".into(),
                    read_only: false,
                },
                Mount {
                    source: "/state/work/repos/x/.git/config".into(),
                    target: "/state/work/repos/x/.git/config".into(),
                    read_only: true,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn pod_carries_the_gate() {
        let pod = spec().pod("tracon-h-1", &cmd(), false).unwrap();
        let s = pod.spec.unwrap();
        let c = &s.containers[0];
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".into()])
        );
        let psc = s.security_context.unwrap();
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.run_as_user, Some(65532));
        assert_eq!(s.automount_service_account_token, Some(false));
        assert_eq!(s.dns_policy.as_deref(), Some("None"));
        assert_eq!(s.node_name.as_deref(), Some("general-1"));
        assert!(s.scheduling_gates.is_none());
        let alias = &s.host_aliases.unwrap()[0];
        assert_eq!(alias.ip, "10.244.0.9");
        assert_eq!(alias.hostnames, Some(vec!["tracon-gw".into()]));
        assert!(s.volumes.unwrap().iter().all(|v| v.host_path.is_none()));
        let env: Vec<(String, String)> = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
            .collect();
        assert!(env.contains(&("HTTPS_PROXY".into(), "http://tracon-gw:8888".into())));
        assert!(env.contains(&("OMP_STATE_DIR".into(), "/home/harness/.omp".into())));
        assert_eq!(c.command, Some(vec!["omp".into(), "acp".into()]));
        assert_eq!(
            pod.metadata
                .labels
                .unwrap()
                .get(ROLE_LABEL)
                .map(String::as_str),
            Some("harness")
        );
    }

    #[test]
    fn mounts_become_sub_paths_of_the_shared_volume() {
        let pod = spec().pod("p", &cmd(), false).unwrap();
        let mounts = pod.spec.unwrap().containers[0]
            .volume_mounts
            .clone()
            .unwrap();
        let work = mounts.iter().find(|m| m.mount_path == "/work").unwrap();
        assert_eq!(work.sub_path.as_deref(), Some("work/repo-x"));
        assert_eq!(work.read_only, Some(false));
        let cfg = mounts
            .iter()
            .find(|m| m.mount_path == "/state/work/repos/x/.git/config")
            .unwrap();
        assert_eq!(cfg.read_only, Some(true));
        assert!(mounts.iter().all(|m| m.name == STATE_VOLUME));
    }

    #[test]
    fn a_source_outside_the_volume_cannot_be_mounted() {
        let mut c = cmd();
        c.mounts.push(Mount {
            source: "/home/jd/.omp".into(),
            target: "/x".into(),
            read_only: true,
        });
        assert!(spec().pod("p", &c, false).is_err());
    }

    #[test]
    fn a_probe_is_gated_so_it_is_admitted_but_never_scheduled() {
        let pod = spec().pod("probe", &cmd(), true).unwrap();
        let s = pod.spec.unwrap();
        assert_eq!(s.scheduling_gates.unwrap()[0].name, PROBE_GATE);
        // The API refuses nodeName on a gated pod; the probe pins by label.
        assert!(s.node_name.is_none());
        assert_eq!(
            s.node_selector.unwrap().get("kubernetes.io/hostname").map(String::as_str),
            Some("general-1")
        );
    }
}
