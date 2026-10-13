//! The Kubernetes runner: one harness Pod per session, created through the
//! API by a node that is itself a pod, its stdio held by the node over
//! `pods/attach`. The single place a harness pod is rendered; the boundary
//! checks introspect the same rendering, so what is verified is what runs.
//!
//! Isolation is not in this file. It lives in the NetworkPolicies the
//! deployment carries (`deploy/kubernetes/base`), which the checks verify,
//! and in the pod's security context, which is rendered here and verified
//! after admission.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, ContainerStatus, EnvVar, HostAlias,
    PersistentVolumeClaimVolumeSource, Pod, PodDNSConfig, PodSchedulingGate, PodSecurityContext,
    PodSpec, PodStatus, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{AttachParams, DeleteParams, LogParams, PostParams};
use kube::{Api, Client};

use super::toolchain::image_entrypoint;
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
    /// See `RunSpec::state_env`: the harness names its own state directory.
    pub state_env: &'static str,
    pub state_dir: &'static str,
    /// The harness image's entrypoint, when it has one. Podman prefixes a
    /// container's command with the image's `ENTRYPOINT`; a pod's `command`
    /// *replaces* it. So an image whose entrypoint does real work (the
    /// OpenCode harness seeds its package caches there) has to have it named
    /// here, or a pod silently skips it. It belongs to `image` alone: a
    /// command run in another image (a repository's, for its checks or its
    /// preparation) does not have this file.
    pub entrypoint: Option<String>,
    /// Seconds between the SIGTERM the kubelet sends and the SIGKILL that
    /// follows. See `Boundary::stop_timeout_secs`.
    pub stop_timeout_secs: u16,
    /// How long a pod may stay unstarted (unscheduled, pulling, mounting)
    /// before the run is given up. `[runtime.kubernetes] start_timeout_secs`.
    pub start_timeout: Duration,
}

impl KubeSpec {
    pub fn from_config(cfg: &Config, env: PodEnv) -> Self {
        Self::for_harness(cfg, &cfg.harness.id, env)
    }

    /// The spec for a named harness: its image, its layout, its entrypoint.
    pub fn for_harness(cfg: &Config, harness_id: &str, env: PodEnv) -> Self {
        let k = &cfg.runtime.kubernetes;
        let layout = crate::adapter::layout(harness_id);
        Self {
            image: cfg.kubernetes_harness_image(harness_id),
            state_claim: k.state_claim.clone(),
            state_mount: k.state_mount.clone(),
            home: k.harness_home.clone(),
            uid: k.uid,
            gateway_host: k.gateway_host.clone(),
            proxy_port: cfg.gateway.proxy_port,
            env,
            extra_mounts: Vec::new(),
            workdir: "/work".into(),
            state_env: layout.env,
            state_dir: layout.dir,
            entrypoint: image_entrypoint(harness_id),
            stop_timeout_secs: cfg.boundary.stop_timeout_secs,
            start_timeout: Duration::from_secs(k.start_timeout_secs),
        }
    }

    /// Make sure each mount's source exists on the PVC before the pod names it.
    /// A mount whose source is already there — a staged file such as the
    /// harness's config or orientation, or a directory from an earlier run —
    /// is left alone: `create_dir_all` over a staged *file* fails "File
    /// exists", and that failed every session on a Kubernetes node before its
    /// harness started.
    async fn ensure_mount_roots(&self, cmd: &RunnerCommand) -> Result<(), RunnerError> {
        for mount in self.extra_mounts.iter().chain(cmd.mounts.iter()) {
            let path = self.state_mount.join(self.sub_path(mount)?);
            if tokio::fs::symlink_metadata(&path).await.is_ok() {
                continue;
            }
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|e| RunnerError::Other(format!("prepare PVC runtime volume: {e}")))?;
        }
        Ok(())
    }

    /// The pod's PVC is the runtime-owned storage root. Each conceptual
    /// volume gets its own checked subdirectory under it; no host path is ever
    /// interpreted as a mount source.
    fn sub_path(&self, mount: &Mount) -> Result<String, RunnerError> {
        if mount.volume.is_empty()
            || !mount
                .volume
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || mount.sub_path.starts_with('/')
            || mount.sub_path.split('/').any(|part| part == "..")
        {
            return Err(RunnerError::Other(format!(
                "invalid runtime volume {}:{}",
                mount.volume, mount.sub_path
            )));
        }
        Ok(if mount.sub_path.is_empty() {
            mount.volume.clone()
        } else {
            format!("{}/{}", mount.volume, mount.sub_path)
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
                    sub_path: Some(self.sub_path(m)?),
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
        let state = format!("{}/{}", self.home, self.state_dir);
        let mut env = vec![
            ("HOME", self.home.clone()),
            ("HTTPS_PROXY", proxy.clone()),
            ("HTTP_PROXY", proxy),
            ("NO_PROXY", super::no_proxy(&self.gateway_host)),
            ("no_proxy", super::no_proxy(&self.gateway_host)),
            (self.state_env, state),
        ]
        .into_iter()
        // A harness that names no state-directory variable (OpenCode decides
        // by HOME and XDG, which its adapter sets) gets none invented for it,
        // and a value the command sets itself is not also set here: two
        // entries for one name is a rule about ordering rather than a choice.
        .filter(|(k, _)| !k.is_empty() && !cmd.env.iter().any(|(name, _)| name == k))
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
        let image = cmd.image.clone().unwrap_or_else(|| self.image.clone());
        // Only the harness image has the harness's entrypoint. Forcing it onto
        // a repository image would run a path that image does not have, and
        // every check and preparation in it would fail before it started.
        let entrypoint = self.entrypoint.as_ref().filter(|_| image == self.image);
        Ok(Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(self.env.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".into()),
                // How long the kubelet waits between SIGTERM and SIGKILL. The
                // harness image runs a real init as PID 1 of the container's
                // own namespace, so `shareProcessNamespace` is not needed and
                // is deliberately not set: sharing one would put the harness
                // and any sidecar in the same namespace, which is a wider
                // grant than reaping needs. The namespace teardown at the end
                // of this window is what actually removes the LSP and
                // formatter children OpenCode never reaps
                // (`config-state.md` §6.7).
                termination_grace_period_seconds: Some(i64::from(self.stop_timeout_secs)),
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
                    name: HARNESS_CONTAINER.into(),
                    image: Some(image),
                    image_pull_policy: Some("IfNotPresent".into()),
                    // An image entrypoint is named rather than replaced: a pod
                    // `command` overrides `ENTRYPOINT`, so an image that does
                    // work before `exec`ing the harness would have that work
                    // skipped here and nowhere else.
                    command: Some(match entrypoint {
                        Some(entrypoint) => vec![entrypoint.clone()],
                        None => cmd.argv.clone(),
                    }),
                    args: entrypoint.map(|_| cmd.argv.clone()),
                    working_dir: Some(workdir),
                    stdin: Some(true),
                    stdin_once: Some(true),
                    // A harness driven over HTTP is reached at the pod's own
                    // address on the cluster network: no Service, no NodePort,
                    // nothing named that anything else could resolve. The
                    // declared port is documentation for whoever reads the
                    // pod; the listener is what actually opens it.
                    ports: cmd.expose.map(|port| {
                        vec![ContainerPort {
                            container_port: i32::from(port),
                            name: Some("harness".into()),
                            protocol: Some("TCP".into()),
                            ..Default::default()
                        }]
                    }),
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
    /// The image a `run_capture` asked for and the identity the kubelet
    /// reported for it, read before the pod is removed. `resolved_image`
    /// answers from this only for the same reference: an `imageID` is
    /// evidence about the image that ran, never about another.
    last_image: tokio::sync::Mutex<Option<(String, String)>>,
}

/// How often a started pod is asked whether it has finished. A check runs for
/// minutes, so a second's lag on its end costs nothing and halves the reads.
const FINISH_POLL: Duration = Duration::from_secs(1);
const START_POLL: Duration = Duration::from_millis(500);
/// Reads in a row that may fail before a run is given up. A long run outlives
/// the odd API blip; a pod that is gone (404) ends it at once.
const READ_FAILURES: u32 = 10;

/// Removes a capture's pod when its `run_capture` is dropped before it got to
/// remove the pod itself: a caller that stops waiting (its own deadline, a
/// cancellation) must not leave the command running in the cluster.
struct PodRemoval {
    pods: Api<Pod>,
    name: String,
    armed: bool,
}

impl Drop for PodRemoval {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let pods = self.pods.clone();
            let name = std::mem::take(&mut self.name);
            runtime.spawn(async move {
                let _ = pods
                    .delete(&name, &DeleteParams::default().grace_period(0))
                    .await;
            });
        }
    }
}

impl KubeRunner {
    pub fn new(client: Client, spec: KubeSpec) -> Self {
        Self {
            client,
            spec,
            last_image: tokio::sync::Mutex::new(None),
        }
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

    /// Wait until the pod has started: scheduled, its image pulled, its
    /// container created. That is the only part bounded here, by
    /// `KubeSpec::start_timeout`, because it is the part a cluster can stall
    /// on for reasons the command never sees. A pod that ran and ended before
    /// it was looked at has started too.
    async fn wait_started(&self, name: &str) -> Result<(), RunnerError> {
        let pods = self.pods();
        let timeout = self.spec.start_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let pod = pods
                .get(name)
                .await
                .map_err(|e| RunnerError::Other(format!("get pod {name}: {e}")))?;
            let status = pod.status.unwrap_or_default();
            let phase = status.phase.as_deref().unwrap_or_default();
            if matches!(phase, "Running" | "Succeeded" | "Failed") {
                return Ok(());
            }
            // A pod that cannot start says why in its container state; surface
            // that rather than a bare timeout.
            if let Some(reason) = harness_status(&status)
                .and_then(|c| c.state.as_ref())
                .and_then(|s| s.waiting.as_ref())
                .and_then(|w| w.reason.as_deref())
            {
                if matches!(
                    reason,
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
                    "pod {name} did not start in {}s: {}; \
                     [runtime.kubernetes] start_timeout_secs sets how long a start may take",
                    timeout.as_secs(),
                    pending_reason(&status)
                )));
            }
            tokio::time::sleep(START_POLL).await;
        }
    }

    /// Wait, without a limit, for a started pod's container to end, and
    /// return the pod as it was then. The caller owns the deadline: it stops
    /// a run by `kill`, which removes the pod, and that ends this wait too.
    async fn wait_finished(&self, name: &str) -> Result<Pod, RunnerError> {
        let pods = self.pods();
        let mut failures = 0;
        loop {
            match pods.get(name).await {
                Ok(pod) => {
                    failures = 0;
                    let status = pod.status.as_ref();
                    let terminated = status
                        .and_then(harness_status)
                        .and_then(|c| c.state.as_ref())
                        .is_some_and(|s| s.terminated.is_some());
                    let phase = status.and_then(|s| s.phase.as_deref());
                    if terminated || matches!(phase, Some("Succeeded" | "Failed")) {
                        return Ok(pod);
                    }
                }
                Err(kube::Error::Api(status)) if status.is_not_found() => {
                    return Err(RunnerError::Other(format!(
                        "pod {name} was removed before it finished"
                    )));
                }
                Err(e) => {
                    failures += 1;
                    if failures >= READ_FAILURES {
                        return Err(RunnerError::Other(format!("get pod {name}: {e}")));
                    }
                }
            }
            tokio::time::sleep(FINISH_POLL).await;
        }
    }

    /// The pod's own address on the cluster network. Nothing is published and
    /// no Service is created: the node reaches a harness pod directly, which
    /// is also why the deployment's NetworkPolicies have to allow that one
    /// direction (node pod → harness pod) and nothing else.
    async fn pod_ip(&self, name: &str, timeout: Duration) -> Result<String, RunnerError> {
        let pods = self.pods();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let ip = pods
                .get(name)
                .await
                .ok()
                .and_then(|pod| pod.status)
                .and_then(|status| status.pod_ip)
                .filter(|ip| !ip.is_empty());
            if let Some(ip) = ip {
                return Ok(ip);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(RunnerError::Other(format!(
                    "pod {name} reported no address in {}s",
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn remove(&self, name: &str) {
        let _ = self
            .pods()
            .delete(name, &DeleteParams::default().grace_period(0))
            .await;
    }

    async fn ensure_mount_roots(&self, cmd: &RunnerCommand) -> Result<(), RunnerError> {
        self.spec.ensure_mount_roots(cmd).await
    }

    /// What a finished capture pod reports: the container's own exit code,
    /// its combined output, and the image the kubelet actually ran.
    async fn captured(&self, name: &str, pod: Pod) -> Result<CaptureEnd, RunnerError> {
        let status = pod.status.unwrap_or_default();
        let container = harness_status(&status);
        let code = match container
            .and_then(|c| c.state.as_ref())
            .and_then(|s| s.terminated.as_ref())
        {
            Some(terminated) => terminated.exit_code,
            // A pod can end without its container ever reporting an exit (an
            // eviction, a node lost): there is no code to give, so there is
            // no result, rather than a 0 or 1 made up from the phase.
            None => {
                return Err(RunnerError::Other(format!(
                    "pod {name} ended ({}) without an exit code: {}",
                    status.phase.as_deref().unwrap_or("unknown phase"),
                    [status.reason.as_deref(), status.message.as_deref()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(": ")
                )))
            }
        };
        // Read what the kubelet actually pulled: the container's `imageID`,
        // not the reference this runner asked for.
        let image_id = container.and_then(|c| normalize_image_id(&c.image_id));
        // The log endpoint interleaves stdout and stderr into one stream and
        // has no stable way to ask for either alone, so both arrive as
        // stdout. An empty `stderr` here says nothing about what the command
        // wrote there; a log that could not be read says so in it instead.
        let (stdout, stderr) = match self
            .pods()
            .logs(
                name,
                &LogParams {
                    container: Some(HARNESS_CONTAINER.into()),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(logs) => (logs.into_bytes(), Vec::new()),
            Err(e) => (
                Vec::new(),
                format!("tracon: the output of pod {name} could not be read: {e}\n").into_bytes(),
            ),
        };
        Ok(CaptureEnd {
            code,
            image_id,
            stdout,
            stderr,
        })
    }
}

struct CaptureEnd {
    code: i32,
    image_id: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

const HARNESS_CONTAINER: &str = "harness";

/// The status of the one container a harness pod runs, by name.
fn harness_status(status: &PodStatus) -> Option<&ContainerStatus> {
    let statuses = status.container_statuses.as_deref()?;
    statuses
        .iter()
        .find(|c| c.name == HARNESS_CONTAINER)
        .or_else(|| statuses.first())
}

/// Why a pod has not started, in the words an operator can act on. The pod
/// status has no "pulling" state: a pull shows as `ContainerCreating`, and the
/// pod's events (which a runner holds no right to read) say the rest.
fn pending_reason(status: &PodStatus) -> String {
    if let Some(waiting) = harness_status(status)
        .and_then(|c| c.state.as_ref())
        .and_then(|s| s.waiting.as_ref())
    {
        let reason = waiting.reason.as_deref().unwrap_or("waiting");
        let detail = waiting.message.as_deref().unwrap_or_default();
        return match (reason, detail) {
            ("ContainerCreating", "") => {
                "ContainerCreating (the image is still pulling, or a volume is still mounting)"
                    .into()
            }
            (reason, "") => reason.to_string(),
            (reason, detail) => format!("{reason}: {detail}"),
        };
    }
    if let Some(condition) = status.conditions.as_deref().and_then(|cs| {
        cs.iter()
            .find(|c| c.type_ == "PodScheduled" && c.status == "False")
    }) {
        return format!(
            "not scheduled: {}",
            [condition.reason.as_deref(), condition.message.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": ")
        );
    }
    format!(
        "phase {}",
        status
            .phase
            .as_deref()
            .filter(|p| !p.is_empty())
            .unwrap_or("Pending")
    )
}

/// An `ExitStatus` that reports `code` as the exit code. Kubernetes reports a
/// signalled container as 128 + the signal, which is already a code.
fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    let code = if (0..=255).contains(&code) { code } else { 255 };
    std::process::ExitStatus::from_raw(code << 8)
}

#[async_trait]
impl Runner for KubeRunner {
    async fn spawn(&self, cmd: RunnerCommand) -> Result<Spawned, RunnerError> {
        self.ensure_mount_roots(&cmd).await?;
        let name = Self::name_for(&cmd, "tracon-h");
        let pod = self.spec.pod(&name, &cmd, false)?;
        let pods = self.pods();
        self.remove(&name).await;
        pods.create(&PostParams::default(), &pod)
            .await
            .map_err(|e| RunnerError::Other(format!("create pod {name}: {e}")))?;
        if let Err(e) = self.wait_started(&name).await {
            self.remove(&name).await;
            return Err(e);
        }
        // The address the node dials for an HTTP-driven harness. A pod that is
        // Running has an IP, but the status can be read a moment before the
        // kubelet records it, so this waits rather than failing the launch.
        let endpoint = match cmd.expose {
            Some(port) => match self.pod_ip(&name, Duration::from_secs(30)).await {
                Ok(ip) => Some(format!("{ip}:{port}")),
                Err(e) => {
                    self.remove(&name).await;
                    return Err(e);
                }
            },
            None => None,
        };
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
            endpoint,
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

    /// Run a command in a pod of its own and wait for it to end. Only the
    /// start is bounded (`KubeSpec::start_timeout`); the run itself is not,
    /// as `podman run` is not, so a check gets the budget its caller gave it
    /// and not a runner's guess. A caller stops a run with `kill` on
    /// `capture_name`, and dropping this future removes the pod as well.
    ///
    /// The result carries the container's own exit code. Its output is the
    /// pod log, in which the cluster has already merged stdout and stderr, so
    /// it is all in `stdout`; see `captured`.
    async fn run_capture(&self, cmd: RunnerCommand) -> Result<std::process::Output, RunnerError> {
        self.ensure_mount_roots(&cmd).await?;
        let name = self.capture_name(&Self::name_for(&cmd, "tracon-x"));
        let asked = cmd.image.clone().unwrap_or_else(|| self.spec.image.clone());
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
        let mut removal = PodRemoval {
            pods: pods.clone(),
            name: name.clone(),
            armed: true,
        };
        let result = match self.wait_started(&name).await {
            Ok(()) => match self.wait_finished(&name).await {
                Ok(finished) => self.captured(&name, finished).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        self.remove(&name).await;
        removal.armed = false;
        let end = result?;
        *self.last_image.lock().await = end.image_id.map(|id| (asked, id));
        Ok(std::process::Output {
            status: exit_status(end.code),
            stdout: end.stdout,
            stderr: end.stderr,
        })
    }

    async fn kill(&self, name: &str) -> Result<(), RunnerError> {
        self.remove(name).await;
        Ok(())
    }

    /// Two node processes can share a namespace, so a capture's pod carries
    /// this process's id. `kill` has to be given the same name back.
    fn capture_name(&self, name: &str) -> String {
        format!(
            "{}-{}",
            if name.is_empty() { "tracon-x" } else { name },
            std::process::id()
        )
    }

    /// The identity of the image a command runs in (`image`, else this
    /// runner's own).
    ///
    /// A reference pinned by digest is its own answer: the kubelet pulls by
    /// that digest and runs nothing else under it, so there is nothing a run
    /// could confirm that the reference does not already say. That is what
    /// lets a runner made fresh for an approval (every caller makes one)
    /// answer without having run anything.
    ///
    /// A tag is only answered with the `imageID` the kubelet reported for that
    /// same reference in this runner's last `run_capture`. `None` before
    /// that, on failure, or when the runtime reported no digest: evidence
    /// must never invent a pin.
    async fn resolved_image(&self, image: Option<&str>) -> Option<String> {
        let image = image.unwrap_or(&self.spec.image);
        if digest_pinned(image) {
            return Some(image.to_string());
        }
        self.last_image
            .lock()
            .await
            .as_ref()
            .filter(|(asked, _)| asked == image)
            .map(|(_, id)| id.clone())
    }
}

/// Whether a reference names its content: `repo[:tag]@sha256:` and 64 hex
/// digits, the shape `review::checks` keys evidence on.
fn digest_pinned(image: &str) -> bool {
    image.split_once("@sha256:").is_some_and(|(repo, digest)| {
        !repo.is_empty() && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// Strip a container-runtime scheme prefix (`docker-pullable://`,
/// `containerd://`, ...) from an `imageID` and keep only a value that is
/// actually digest-shaped (`repo@sha256:hex`); a bare `sha256:hex` with no
/// repository, or anything else, is not something `immutable_image_identity`
/// can parse and must not be returned as if it were.
fn normalize_image_id(raw: &str) -> Option<String> {
    let stripped = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    stripped.contains("@sha256:").then(|| stripped.to_string())
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
            argv: vec!["opencode".into(), "serve".into()],
            name: "tracon-h-1".into(),
            mounts: vec![
                Mount::volume("tracon-workspace-repo-x", "/work", false),
                Mount::at(
                    "tracon-scratch-repo-x",
                    "gitconfig",
                    "/home/harness/.gitconfig",
                    true,
                ),
            ],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_staged_file_mount_is_left_as_it_is() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = spec();
        spec.state_mount = root.path().to_path_buf();
        // What `import_volume` leaves behind: the scratch volume already holds
        // the file this mount names.
        let staged = root.path().join("tracon-scratch-repo-x");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("gitconfig"), "[user]\n").unwrap();

        spec.ensure_mount_roots(&cmd()).await.unwrap();

        assert!(
            staged.join("gitconfig").is_file(),
            "the staged file survives"
        );
        assert!(
            root.path().join("tracon-workspace-repo-x").is_dir(),
            "a missing root is made"
        );
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
        // OpenCode names no state-directory variable (`Layout::env` is
        // empty), so the pod carries none rather than one it invented.
        assert!(!env.iter().any(|(name, _)| name.ends_with("STATE_DIR")));
        // The image's entrypoint is named rather than replaced; the argv
        // becomes the args. See the entrypoint test below.
        assert_eq!(
            c.command,
            Some(vec![crate::runner::toolchain::IMAGE_ENTRYPOINT.to_string()])
        );
        assert_eq!(c.args, Some(vec!["opencode".into(), "serve".into()]));
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
        assert_eq!(work.sub_path.as_deref(), Some("tracon-workspace-repo-x"));
        assert_eq!(work.read_only, Some(false));
        let cfg = mounts
            .iter()
            .find(|m| m.mount_path == "/home/harness/.gitconfig")
            .unwrap();
        assert_eq!(
            cfg.sub_path.as_deref(),
            Some("tracon-scratch-repo-x/gitconfig")
        );
        assert_eq!(cfg.read_only, Some(true));
        assert!(mounts.iter().all(|m| m.name == STATE_VOLUME));
    }

    #[test]
    fn invalid_runtime_volume_is_refused() {
        let mut c = cmd();
        c.mounts.push(Mount::volume("../host", "/x", true));
        assert!(spec().pod("p", &c, false).is_err());
    }

    /// A pod `command` replaces the image's `ENTRYPOINT`. The OpenCode image's
    /// entrypoint seeds the package caches that keep the harness off a
    /// registry, so a pod that skipped it would reach for one on a network
    /// that refuses — a slower, noisier version of the same failure the seed
    /// exists to prevent.
    #[test]
    fn the_opencode_image_entrypoint_is_named_rather_than_replaced() {
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg.harness.id = crate::adapter::opencode::OpenCodeAdapter::ID.into();
        let spec = KubeSpec::from_config(
            &cfg,
            PodEnv {
                namespace: "tracon-lab".into(),
                pod_ip: "10.244.0.9".into(),
                node_name: "general-1".into(),
            },
        );
        let pod = spec.pod("tracon-h-1", &cmd(), false).unwrap();
        let s = pod.spec.unwrap();
        let c = &s.containers[0];
        assert_eq!(
            c.command,
            Some(vec![crate::runner::toolchain::IMAGE_ENTRYPOINT.to_string()])
        );
        assert_eq!(c.args, Some(vec!["opencode".into(), "serve".into()]));
        // The grace period is the same number the Podman runner stops with,
        // and the pod carries no shared process namespace: the container's own
        // init is what reaps.
        assert_eq!(s.termination_grace_period_seconds, Some(10));
        assert!(s.share_process_namespace.is_none());
    }

    /// A harness image with no entrypoint — the Claude Code one — keeps the
    /// plain shape: the command is the argv and there are no args.
    #[test]
    fn an_image_without_an_entrypoint_still_carries_its_argv_as_the_command() {
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        cfg.harness.id = crate::adapter::claude::ClaudeAdapter::ID.into();
        let spec = KubeSpec::from_config(
            &cfg,
            PodEnv {
                namespace: "tracon-lab".into(),
                pod_ip: "10.244.0.9".into(),
                node_name: "general-1".into(),
            },
        );
        let pod = spec.pod("tracon-h-1", &cmd(), false).unwrap();
        let c = &pod.spec.unwrap().containers[0];
        assert_eq!(c.command, Some(vec!["opencode".into(), "serve".into()]));
        assert!(c.args.is_none());
    }

    /// The entrypoint is the harness image's file. A check or a preparation
    /// run in a repository's image gets its argv as the command, and naming
    /// the harness image explicitly is the same as naming none.
    #[test]
    fn only_the_harness_image_gets_the_harness_entrypoint() {
        let spec = spec();
        assert!(spec.entrypoint.is_some(), "the default harness is OpenCode");
        let mut repo = cmd();
        repo.image = Some("registry.example/app@sha256:".to_string() + &"a".repeat(64));
        let pod = spec.pod("tracon-c-1", &repo, false).unwrap();
        let c = &pod.spec.unwrap().containers[0];
        assert_eq!(c.image.as_deref(), repo.image.as_deref());
        assert_eq!(c.command, Some(vec!["opencode".into(), "serve".into()]));
        assert!(c.args.is_none());

        let mut harness = cmd();
        harness.image = Some(spec.image.clone());
        let pod = spec.pod("tracon-c-2", &harness, false).unwrap();
        let c = &pod.spec.unwrap().containers[0];
        assert_eq!(
            c.command,
            Some(vec![crate::runner::toolchain::IMAGE_ENTRYPOINT.to_string()])
        );
        assert_eq!(c.args, Some(vec!["opencode".into(), "serve".into()]));
    }

    #[test]
    fn the_start_deadline_comes_from_configuration() {
        assert_eq!(spec().start_timeout, Duration::from_secs(15 * 60));
        let mut cfg = Config::default();
        cfg.runtime.kubernetes.start_timeout_secs = 42;
        let spec = KubeSpec::from_config(&cfg, spec().env);
        assert_eq!(spec.start_timeout, Duration::from_secs(42));
    }

    #[test]
    fn only_a_full_digest_counts_as_pinned() {
        let hex = "0123456789abcdef".repeat(4);
        assert!(digest_pinned(&format!("ghcr.io/o/i@sha256:{hex}")));
        assert!(digest_pinned(&format!("ghcr.io/o/i:1.2@sha256:{hex}")));
        assert!(!digest_pinned("ghcr.io/o/i:1.2"));
        assert!(!digest_pinned(&format!("@sha256:{hex}")));
        assert!(!digest_pinned("ghcr.io/o/i@sha256:abc"));
        assert!(!digest_pinned(&format!(
            "ghcr.io/o/i@sha256:{}",
            "z".repeat(64)
        )));
    }

    #[test]
    fn exit_codes_are_reported_as_they_are() {
        assert_eq!(exit_status(0).code(), Some(0));
        assert!(exit_status(0).success());
        assert_eq!(exit_status(2).code(), Some(2));
        assert_eq!(exit_status(127).code(), Some(127));
        assert_eq!(exit_status(137).code(), Some(137));
        // Out of range is a failure, never a wrapped-around success.
        assert_eq!(exit_status(256).code(), Some(255));
        assert_eq!(exit_status(-1).code(), Some(255));
    }

    #[test]
    fn a_pod_that_has_not_started_says_why() {
        use k8s_openapi::api::core::v1::{ContainerState, ContainerStateWaiting, PodCondition};
        let waiting = |reason: &str, message: Option<&str>| PodStatus {
            phase: Some("Pending".into()),
            container_statuses: Some(vec![ContainerStatus {
                name: HARNESS_CONTAINER.into(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some(reason.into()),
                        message: message.map(str::to_string),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(pending_reason(&waiting("ContainerCreating", None)).contains("pulling"));
        assert_eq!(
            pending_reason(&waiting("CreateContainerError", Some("no such file"))),
            "CreateContainerError: no such file"
        );
        let unscheduled = PodStatus {
            phase: Some("Pending".into()),
            conditions: Some(vec![PodCondition {
                type_: "PodScheduled".into(),
                status: "False".into(),
                reason: Some("Unschedulable".into()),
                message: Some("0/1 nodes are available".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(
            pending_reason(&unscheduled),
            "not scheduled: Unschedulable: 0/1 nodes are available"
        );
        assert_eq!(pending_reason(&PodStatus::default()), "phase Pending");
    }

    #[test]
    fn a_probe_is_gated_so_it_is_admitted_but_never_scheduled() {
        let pod = spec().pod("probe", &cmd(), true).unwrap();
        let s = pod.spec.unwrap();
        assert_eq!(s.scheduling_gates.unwrap()[0].name, PROBE_GATE);
        // The API refuses nodeName on a gated pod; the probe pins by label.
        assert!(s.node_name.is_none());
        assert_eq!(
            s.node_selector
                .unwrap()
                .get("kubernetes.io/hostname")
                .map(String::as_str),
            Some("general-1")
        );
    }
}
