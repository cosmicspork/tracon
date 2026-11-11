//! What every sandbox pod is made of besides its own rendering
//! (`KubeSandboxProvider::render`): the labels it carries, the node pod's
//! own facts, its sizing, and how a pod that never started, or was refused,
//! is read back.

use std::time::Duration;

use k8s_openapi::api::core::v1::{
    ContainerStatus, LocalObjectReference, PodStatus, ResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

use crate::config::{Config, KubeResources, PodSizing};
use crate::runner::{Mount, RunLabels, RunnerError};

pub const ROLE_LABEL: &str = "tracon.dev/role";
/// Which node a pod is, or was made by: its `[boundary] resource_prefix`.
/// The node's own pod carries it beside `tracon.dev/role: node`, and so does
/// every pod it makes, so two nodes in one namespace each select their own
/// (a sandbox's affinity and NetworkPolicy, a sweep) and never the other's.
pub const NODE_LABEL: &str = "tracon.dev/node";
pub const SESSION_LABEL: &str = "tracon.dev/session";
pub const PROBE_GATE: &str = "tracon.dev/probe";
/// What the pod is for: `RunKind`, else its sizing.
pub const KIND_LABEL: &str = "tracon.dev/kind";
/// Which `[runtime.kubernetes.resources.*]` table sized the pod.
pub const SIZING_LABEL: &str = "tracon.dev/sizing";
pub const REPO_LABEL: &str = "tracon.dev/repo";
pub const CHANNEL_LABEL: &str = "tracon.dev/channel";
pub const HARNESS_LABEL: &str = "tracon.dev/harness";
pub const PHASE_LABEL: &str = "tracon.dev/phase";

pub const SHM_VOLUME: &str = "shm";
pub const SCRATCH_VOLUME: &str = "scratch";
/// Where a sandbox pod's `TMPDIR` points: an emptyDir of its own, so a
/// repository image whose `/tmp` the harness uid cannot write still has
/// somewhere to put temporary files, and nothing there outlives the pod.
pub const SCRATCH_MOUNT: &str = "/tmp";

/// What the node learns about its own pod from the downward API. The
/// deployment sets these; a node without them is not running as a pod.
#[derive(Debug, Clone)]
pub struct PodEnv {
    pub namespace: String,
    pub pod_ip: String,
    pub node_name: String,
}

impl PodEnv {
    /// The namespace the node's own pod runs in, which sandbox pods may be
    /// configured away from (`[runtime.kubernetes] namespace`).
    pub fn own_namespace() -> Option<String> {
        std::env::var("TRACON_NAMESPACE")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| {
                std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
    }

    /// The node's own pod's name: `TRACON_POD_NAME` (downward API
    /// `metadata.name`), else `HOSTNAME`, which a pod's hostname is unless
    /// its spec sets another.
    pub fn own_pod_name() -> Option<String> {
        ["TRACON_POD_NAME", "HOSTNAME"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

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

/// Which of `KubeResources`' sizings a pod was rendered with. Each is one
/// `[runtime.kubernetes.resources.*]` table, so the `tracon.dev/sizing` label
/// names the knob a change to that pod's size turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    /// `resources.session`: a session's harness, and the boundary's probe pod.
    Session,
    /// `resources.command`: a captured command, a check or a preparation.
    Command,
    /// `resources.probe`.
    Probe,
}

impl Sizing {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Command => "command",
            Self::Probe => "probe",
        }
    }

    pub(crate) fn sized(self, resources: &KubeResources) -> PodSizing {
        match self {
            Self::Session => resources.session(),
            Self::Command => resources.command(),
            Self::Probe => resources.probe(),
        }
    }
}

/// `value` as a Kubernetes label value, or `None` when nothing of it is left:
/// at most 63 characters, alphanumeric at both ends, `[-_.A-Za-z0-9]` between.
/// Lowercased, with any other character (`/`, `@`, a space, anything not
/// ASCII) made a `-` and runs of `-` collapsed. Two long names that differ only
/// past the 63rd character share a value, which a label can live with.
pub fn label_value(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        let c = c.to_ascii_lowercase();
        let c = if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.truncate(63);
    let trimmed = out.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The labels a sandbox pod carries: the three every selector reads (its
/// role, which node made it, its name), then what it is for. A value with
/// nothing left once sanitized is left out rather than rendered empty.
pub(crate) fn pod_labels(
    name: &str,
    labels: &RunLabels,
    sizing: Sizing,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    out.insert(ROLE_LABEL.to_string(), super::SANDBOX_ROLE.to_string());
    out.insert(
        NODE_LABEL.to_string(),
        crate::config::resource_prefix().to_string(),
    );
    out.insert(SESSION_LABEL.to_string(), name.to_string());
    out.insert(
        KIND_LABEL.to_string(),
        labels
            .kind
            .map(crate::runner::RunKind::as_str)
            .unwrap_or(sizing.as_str())
            .to_string(),
    );
    out.insert(SIZING_LABEL.to_string(), sizing.as_str().to_string());
    if let Some(phase) = labels.phase {
        out.insert(PHASE_LABEL.to_string(), phase.to_string());
    }
    for (key, value) in [
        (REPO_LABEL, &labels.repo),
        (CHANNEL_LABEL, &labels.channel),
        (HARNESS_LABEL, &labels.harness),
    ] {
        if let Some(value) = value.as_deref().and_then(label_value) {
            out.insert(key.to_string(), value);
        }
    }
    out
}

/// Why the cluster refused a sandbox pod for lack of capacity: one that does
/// not fit beside the node's own pod is failed by the kubelet's admission
/// (`OutOfcpu`, `OutOfmemory`, ...) with no container ever started, or a
/// namespace's ResourceQuota or LimitRange refuses it at create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    OutOfCpu,
    OutOfMemory,
    OutOfPods,
    OutOfEphemeralStorage,
    /// Another `OutOf*`: an extended resource, a device.
    OutOfOther,
    /// A ResourceQuota or LimitRange refused the create.
    Quota,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutOfCpu => "out_of_cpu",
            Self::OutOfMemory => "out_of_memory",
            Self::OutOfPods => "out_of_pods",
            Self::OutOfEphemeralStorage => "out_of_ephemeral_storage",
            Self::OutOfOther => "out_of_other",
            Self::Quota => "quota",
        }
    }

    /// The kubelet's admission reason, read: `OutOf` and the resource it ran
    /// out of.
    pub(crate) fn from_reason(reason: &str) -> Option<Self> {
        Some(match reason.strip_prefix("OutOf")? {
            "cpu" => Self::OutOfCpu,
            "memory" => Self::OutOfMemory,
            "pods" => Self::OutOfPods,
            "ephemeral-storage" => Self::OutOfEphemeralStorage,
            _ => Self::OutOfOther,
        })
    }
}

/// The capacity refusal a pod's status reports, if that is how it ended: phase
/// Failed, an `OutOf*` reason, and no container that ever ran. A pod whose
/// container ran and ended is a command's own result, whatever its phase.
pub(crate) fn refusal(status: &PodStatus) -> Option<Refusal> {
    if status.phase.as_deref() != Some("Failed") {
        return None;
    }
    let ran = status.container_statuses.iter().flatten().any(|c| {
        [c.state.as_ref(), c.last_state.as_ref()]
            .into_iter()
            .flatten()
            .any(|s| s.running.is_some() || s.terminated.is_some())
    });
    if ran {
        return None;
    }
    Refusal::from_reason(status.reason.as_deref()?)
}

/// What a refused pod is reported as: the resource it was refused for, what
/// it asked for, and the `[runtime.kubernetes.resources.*]` key that sizes
/// it, which is what an operator changes. `said` is the cluster's own words.
pub(crate) fn refusal_error(
    name: &str,
    refusal: Refusal,
    sizing: Sizing,
    resources: &KubeResources,
    said: &str,
) -> RunnerError {
    let sized = sizing.sized(resources);
    let table = format!("[runtime.kubernetes.resources.{}]", sizing.as_str());
    let what = match refusal {
        Refusal::OutOfCpu => format!(
            "pod {name} refused by the node: not enough cpu (requested {}); {table} cpu_request sizes it",
            sized.cpu_request
        ),
        Refusal::OutOfMemory => format!(
            "pod {name} refused by the node: not enough memory (requested {}); {table} memory_request sizes it",
            sized.memory_request
        ),
        Refusal::OutOfPods => {
            format!("pod {name} refused by the node: it already runs as many pods as it may")
        }
        Refusal::OutOfEphemeralStorage => {
            format!("pod {name} refused by the node: not enough ephemeral storage")
        }
        Refusal::OutOfOther => format!("pod {name} refused by the node: a resource ran out"),
        Refusal::Quota => match quota_unspecified(said).as_slice() {
            [] => format!(
                "pod {name} refused by the namespace's ResourceQuota or LimitRange; {table} sizes it"
            ),
            keys => format!(
                "pod {name} refused by the namespace's ResourceQuota: it requires every pod to set {}; set {} in {table}",
                keys.iter()
                    .map(|(resource, _)| *resource)
                    .collect::<Vec<_>>()
                    .join(", "),
                keys.iter()
                    .map(|(_, key)| *key)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        },
    };
    let message = match said.trim() {
        "" => what,
        said => format!("{what} ({said})"),
    };
    RunnerError::Refused {
        reason: refusal.as_str(),
        message,
    }
}

/// Whether the API server refused a create for a ResourceQuota or a
/// LimitRange: a 403 whose message is the quota's (`exceeded quota`, or
/// `failed quota: <name>: must specify ...` when the quota covers a resource
/// the pod leaves unset) or the LimitRange's (`... per Container ...`,
/// `... per Pod ...`).
pub(crate) fn quota_refused(code: u16, message: &str) -> bool {
    code == 403
        && [
            "exceeded quota",
            "failed quota",
            "per Container",
            "per Pod",
            "LimitRange",
        ]
        .iter()
        .any(|said| message.contains(said))
}

/// The resources a quota's `must specify limits.cpu,limits.memory[ for: ...]`
/// says the pod left unset, each with the `[runtime.kubernetes.resources.*]`
/// key that sets it. Empty when the refusal is not that one, or names
/// nothing a key sets.
fn quota_unspecified(said: &str) -> Vec<(&'static str, &'static str)> {
    let Some((_, rest)) = said.split_once("must specify ") else {
        return Vec::new();
    };
    let listed = rest.split(" for:").next().unwrap_or_default();
    let mut keys = Vec::new();
    for resource in listed.split(',').map(str::trim) {
        let key = match resource {
            "limits.cpu" => ("limits.cpu", "cpu_limit"),
            "limits.memory" => ("limits.memory", "memory_limit"),
            "requests.cpu" | "cpu" => ("requests.cpu", "cpu_request"),
            "requests.memory" | "memory" => ("requests.memory", "memory_request"),
            _ => continue,
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The uid every sandbox pod runs as: the stock base's own user
/// (`sandbox_images::USER`), which owns its home, `/work` and `/cache`.
pub const UID: i64 = 65532;

/// Everything that puts a harness inside a pod behind the boundary.
#[derive(Debug, Clone)]
pub struct KubeSpec {
    pub image: String,
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
    /// Seconds between the SIGTERM the kubelet sends and the SIGKILL that
    /// follows. See `Boundary::stop_timeout_secs`.
    pub stop_timeout_secs: u16,
    /// How long a pod may stay unstarted (unscheduled, pulling, mounting)
    /// before the run is given up. `[runtime.kubernetes] start_timeout_secs`.
    pub start_timeout: Duration,
    /// `[runtime.kubernetes.resources]`: a session's pod and a captured
    /// command's are sized apart.
    pub resources: KubeResources,
    /// `[runtime.kubernetes] image_pull_secrets`.
    pub image_pull_secrets: Vec<String>,
    /// `[runtime.kubernetes] shm_size`.
    pub shm_size: String,
}

impl KubeSpec {
    pub fn from_config(cfg: &Config, env: PodEnv) -> Self {
        Self::for_harness(cfg, &cfg.harness.id, env)
    }

    /// The spec for a named harness: the stock base, which the bundle
    /// completes with every harness, and the harness's layout.
    pub fn for_harness(cfg: &Config, harness_id: &str, env: PodEnv) -> Self {
        let k = &cfg.runtime.kubernetes;
        let layout = crate::adapter::layout(harness_id);
        Self {
            image: crate::sandbox_images::base_image(cfg),
            home: crate::sandbox_images::HOME.into(),
            uid: UID,
            gateway_host: k.gateway_host.clone(),
            proxy_port: cfg.gateway.proxy_port,
            env,
            extra_mounts: Vec::new(),
            workdir: "/work".into(),
            state_env: layout.env,
            state_dir: layout.dir,
            stop_timeout_secs: cfg.boundary.stop_timeout_secs,
            start_timeout: Duration::from_secs(k.start_timeout_secs),
            resources: k.resources.clone(),
            image_pull_secrets: k.image_pull_secrets.clone(),
            shm_size: k.shm_size.clone(),
        }
    }

    /// Explicit requests and a memory limit, always. A namespace's
    /// LimitRange fills in whatever a pod leaves out, and a request above
    /// its default limit is then refused at admission.
    pub(crate) fn requirements(sizing: &PodSizing) -> ResourceRequirements {
        let quantity = |value: &str| Quantity(value.to_string());
        let mut limits = std::collections::BTreeMap::from([(
            "memory".to_string(),
            quantity(&sizing.memory_limit),
        )]);
        if let Some(cpu) = &sizing.cpu_limit {
            limits.insert("cpu".into(), quantity(cpu));
        }
        ResourceRequirements {
            requests: Some(std::collections::BTreeMap::from([
                ("cpu".to_string(), quantity(&sizing.cpu_request)),
                ("memory".to_string(), quantity(&sizing.memory_request)),
            ])),
            limits: Some(limits),
            ..Default::default()
        }
    }

    pub(crate) fn pull_secrets(&self) -> Option<Vec<LocalObjectReference>> {
        (!self.image_pull_secrets.is_empty()).then(|| {
            self.image_pull_secrets
                .iter()
                .map(|name| LocalObjectReference { name: name.clone() })
                .collect()
        })
    }
}

pub(crate) const HARNESS_CONTAINER: &str = "harness";

/// The status of the one container a sandbox pod runs, by name.
pub(crate) fn harness_status(status: &PodStatus) -> Option<&ContainerStatus> {
    let statuses = status.container_statuses.as_deref()?;
    statuses
        .iter()
        .find(|c| c.name == HARNESS_CONTAINER)
        .or_else(|| statuses.first())
}

/// Why a pod has not started, in the words an operator can act on. The pod
/// status has no "pulling" state: a pull shows as `ContainerCreating`, and the
/// pod's events (which a runner holds no right to read) say the rest.
pub(crate) fn pending_reason(status: &PodStatus) -> String {
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

/// Whether a reference names its content: `repo[:tag]@sha256:` and 64 hex
/// digits, the shape `review::checks` keys evidence on.
fn digest_pinned(image: &str) -> bool {
    image.split_once("@sha256:").is_some_and(|(repo, digest)| {
        !repo.is_empty() && digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// The image identity of a sandbox pod's command, which is all a
/// `ProtocolRunner` hands on here: it runs nothing itself.
///
/// A reference pinned by digest is its own answer: the kubelet pulls by that
/// digest and runs nothing else under it, so a runner made fresh for an
/// approval answers without having run anything. A tag has no confirmed
/// identity: evidence must never invent a pin.
pub struct DigestIdentity;

#[async_trait::async_trait]
impl crate::runner::Runner for DigestIdentity {
    async fn spawn(
        &self,
        _cmd: crate::runner::RunnerCommand,
    ) -> Result<crate::runner::Spawned, RunnerError> {
        Err(RunnerError::Other(
            "a sandbox pod's command runs over the sandbox protocol".into(),
        ))
    }

    async fn run_capture(
        &self,
        _cmd: crate::runner::RunnerCommand,
    ) -> Result<std::process::Output, RunnerError> {
        Err(RunnerError::Other(
            "a sandbox pod's command runs over the sandbox protocol".into(),
        ))
    }

    async fn kill(&self, _name: &str) -> Result<(), RunnerError> {
        Ok(())
    }

    async fn resolved_image(&self, image: Option<&str>) -> Option<String> {
        image
            .filter(|image| digest_pinned(image))
            .map(str::to_string)
    }
}

/// Strip a container-runtime scheme prefix (`docker-pullable://`,
/// `containerd://`, ...) from an `imageID` and keep only a value that is
/// actually digest-shaped (`repo@sha256:hex`); a bare `sha256:hex` with no
/// repository, or anything else, is not something `immutable_image_identity`
/// can parse and must not be returned as if it were.
pub(crate) fn normalize_image_id(raw: &str) -> Option<String> {
    let stripped = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    stripped.contains("@sha256:").then(|| stripped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Runner;

    fn spec() -> KubeSpec {
        KubeSpec::from_config(
            &Config::default(),
            PodEnv {
                namespace: "tracon-lab".into(),
                pod_ip: "10.244.0.9".into(),
                node_name: "general-1".into(),
            },
        )
    }

    /// Every sandbox runs the stock base as its own user, whatever the
    /// harness, and only a digest-pinned reference is an identity.
    #[tokio::test]
    async fn a_sandbox_runs_the_base_and_only_a_digest_is_its_identity() {
        let spec = spec();
        assert_eq!(
            spec.image,
            crate::sandbox_images::base_image(&Config::default())
        );
        assert_eq!(spec.uid, UID);
        assert_eq!(spec.home, crate::sandbox_images::HOME);
        assert_eq!(spec.start_timeout, Duration::from_secs(15 * 60));
        let hex = "0123456789abcdef".repeat(4);
        let pinned = format!("ghcr.io/o/i@sha256:{hex}");
        assert_eq!(
            DigestIdentity.resolved_image(Some(&pinned)).await,
            Some(pinned)
        );
        assert_eq!(
            DigestIdentity.resolved_image(Some("ghcr.io/o/i:1")).await,
            None
        );
        assert_eq!(DigestIdentity.resolved_image(None).await, None);
    }

    #[test]
    fn every_pod_says_what_it_is_for_and_how_it_was_sized() {
        let labels = pod_labels(
            "tracon-h-1",
            &RunLabels {
                kind: Some(crate::runner::RunKind::Session),
                repo: Some("Cosmic Spork/Repo".into()),
                channel: Some("---".into()),
                harness: Some("claude".into()),
                phase: Some("plan"),
            },
            Sizing::Session,
        );
        assert_eq!(labels[ROLE_LABEL], super::super::SANDBOX_ROLE);
        assert_eq!(labels[NODE_LABEL], crate::config::resource_prefix());
        assert_eq!(labels[SESSION_LABEL], "tracon-h-1");
        assert_eq!(labels[KIND_LABEL], "session");
        assert_eq!(labels[SIZING_LABEL], "session");
        assert_eq!(labels[REPO_LABEL], "cosmic-spork-repo");
        assert_eq!(labels[HARNESS_LABEL], "claude");
        assert_eq!(labels[PHASE_LABEL], "plan");
        // Nothing left once sanitized: left out.
        assert!(!labels.contains_key(CHANNEL_LABEL), "{labels:?}");
        // A caller that named no kind still leaves no pod unlabelled.
        let labels = pod_labels("tracon-x-1", &RunLabels::default(), Sizing::Command);
        assert_eq!(labels[KIND_LABEL], "command");
        assert_eq!(labels[SIZING_LABEL], "command");
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

    /// Every value fits a Kubernetes label: at most 63 characters,
    /// alphanumeric at both ends, `[-_.A-Za-z0-9]` between.
    #[test]
    fn label_values_are_sanitized_to_what_kubernetes_accepts() {
        let valid = |value: &str| {
            value.len() <= 63
                && value.starts_with(|c: char| c.is_ascii_alphanumeric())
                && value.ends_with(|c: char| c.is_ascii_alphanumeric())
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        assert_eq!(label_value("personal").as_deref(), Some("personal"));
        assert_eq!(label_value("@ops").as_deref(), Some("ops"));
        assert_eq!(
            label_value("Cosmic Spork/Repo").as_deref(),
            Some("cosmic-spork-repo")
        );
        assert_eq!(label_value("café au lait").as_deref(), Some("caf-au-lait"));
        let long = label_value(&format!("{}-{}", "a".repeat(62), "b".repeat(37))).unwrap();
        assert!(long.len() <= 63, "{long}");
        assert!(
            long.ends_with(|c: char| c.is_ascii_alphanumeric()),
            "{long}"
        );
        assert_eq!(label_value("---"), None);
        assert_eq!(label_value(""), None);
        for input in [
            "personal",
            "@ops",
            "Cosmic Spork/Repo",
            "café",
            "a.b_c-d",
            "_x_",
            &"z/".repeat(80),
        ] {
            if let Some(value) = label_value(input) {
                assert!(valid(&value), "{input:?} -> {value:?}");
            }
        }
    }

    /// A pod the kubelet refuses at admission never ran: it is a refusal
    /// naming the resource and the key that sizes it, not a command that
    /// ended without an exit code.
    #[test]
    fn a_pod_refused_for_capacity_is_classified() {
        use k8s_openapi::api::core::v1::{ContainerState, ContainerStateTerminated};
        let failed = |reason: &str| PodStatus {
            phase: Some("Failed".into()),
            reason: Some(reason.into()),
            message: Some("Pod was rejected: Node didn't have enough resource".into()),
            ..Default::default()
        };
        assert_eq!(refusal(&failed("OutOfmemory")), Some(Refusal::OutOfMemory));
        assert_eq!(refusal(&failed("OutOfcpu")), Some(Refusal::OutOfCpu));
        assert_eq!(refusal(&failed("OutOfpods")), Some(Refusal::OutOfPods));
        assert_eq!(
            refusal(&failed("OutOfephemeral-storage")),
            Some(Refusal::OutOfEphemeralStorage)
        );
        assert_eq!(
            refusal(&failed("OutOfnvidia.com/gpu")),
            Some(Refusal::OutOfOther)
        );
        assert_eq!(refusal(&failed("Evicted")), None);
        let mut pending = failed("OutOfmemory");
        pending.phase = Some("Pending".into());
        assert_eq!(refusal(&pending), None);

        // A container that ran and ended is the command's own result.
        let mut ended = failed("OutOfmemory");
        ended.container_statuses = Some(vec![ContainerStatus {
            name: HARNESS_CONTAINER.into(),
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 1,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }]);
        assert_eq!(refusal(&ended), None);

        let resources = spec().resources;
        let error = refusal_error(
            "tracon-c-1",
            Refusal::OutOfMemory,
            Sizing::Command,
            &resources,
            "Node didn't have enough resource: memory",
        );
        let RunnerError::Refused { reason, message } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*reason, "out_of_memory");
        assert!(
            message.contains("not enough memory (requested 2Gi)"),
            "{message}"
        );
        assert!(
            message.contains("[runtime.kubernetes.resources.command] memory_request"),
            "{message}"
        );
        let error = refusal_error(
            "tracon-h-1",
            Refusal::OutOfCpu,
            Sizing::Session,
            &resources,
            "",
        );
        assert!(
            error.to_string().contains("not enough cpu (requested 250m); [runtime.kubernetes.resources.session] cpu_request"),
            "{error}"
        );
    }

    #[test]
    fn a_create_a_quota_refuses_is_told_apart_from_other_failures() {
        assert!(quota_refused(
            403,
            "pods \"tracon-c-1\" is forbidden: exceeded quota: compute, requested: requests.memory=2Gi"
        ));
        assert!(quota_refused(
            403,
            "pods \"tracon-c-1\" is forbidden: maximum memory usage per Container is 1Gi, but limit is 4Gi"
        ));
        assert!(!quota_refused(
            403,
            "pods is forbidden: User cannot create resource"
        ));
        // The quota plugin's other refusal: the namespace's quota covers
        // limits.cpu, which a pod renders only when cpu_limit is set.
        let unset = "pods \"tracon-h-1\" is forbidden: failed quota: compute: must specify limits.cpu for: harness";
        assert!(quota_refused(403, unset));
        assert!(!quota_refused(409, "exceeded quota"));
        let error = refusal_error(
            "tracon-h-1",
            Refusal::Quota,
            Sizing::Session,
            &spec().resources,
            unset,
        );
        let RunnerError::Refused { reason, message } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*reason, "quota");
        assert!(
            message.contains("requires every pod to set limits.cpu; set cpu_limit in [runtime.kubernetes.resources.session]"),
            "{message}"
        );
        assert_eq!(
            quota_unspecified(
                "failed quota: q: must specify limits.cpu,limits.memory,requests.memory"
            ),
            vec![
                ("limits.cpu", "cpu_limit"),
                ("limits.memory", "memory_limit"),
                ("requests.memory", "memory_request"),
            ]
        );
        assert!(quota_unspecified("exceeded quota: compute").is_empty());
    }
}
