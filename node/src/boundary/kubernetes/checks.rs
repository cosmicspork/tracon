//! The Kubernetes answers to the five checks. What can be inspected here is
//! the admitted pod spec (a probe pod that is gated, so admission mutates it
//! and nothing ever schedules it), the NetworkPolicy the deployment carries,
//! and the node's own rights; `--deep` runs the shared egress script as a
//! real harness pod.

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::{
    NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule,
};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, PostParams};
use kube::Api;

use super::KubeBackend;
use crate::boundary::checks::{egress_script, egress_verdict, CheckId, CheckResult, Denial};
use crate::boundary::BoundaryReport;
use crate::config::{Config, HarnessListen};
use crate::runner::kube::{ROLE_LABEL, STATE_VOLUME};
use crate::runner::{Runner, RunnerCommand};

const PROBE: &str = "tracon-boundary-probe";
pub const HARNESS_POLICY: &str = "tracon-harness";
/// The node's own ingress policy, read only to learn whether it admits
/// harness pods on the egress port.
pub const NODE_POLICY: &str = "tracon-node";

/// Whether the harness policy's ingress admits the node's pod and nothing
/// else. The OpenCode adapter drives its harness over HTTP on a port the node
/// picks per launch, so the node pod must reach the harness pod — the one
/// direction the runner documents — while any other peer (another pod, a
/// namespace, an address block, or a rule with no `from` at all, which admits
/// everyone) is refused. No rules at all is the tightest shape and still holds.
fn ingress_only_from_node(
    rules: &[k8s_openapi::api::networking::v1::NetworkPolicyIngressRule],
) -> Result<(), String> {
    for rule in rules {
        let peers = rule.from.as_deref().unwrap_or_default();
        if peers.is_empty() {
            return Err("an ingress rule admits every peer".into());
        }
        for peer in peers {
            if peer.ip_block.is_some() || peer.namespace_selector.is_some() {
                return Err(
                    "ingress reaches beyond the node's pod (ipBlock or namespaceSelector)".into(),
                );
            }
            let from_node = peer
                .pod_selector
                .as_ref()
                .and_then(|s| s.match_labels.as_ref())
                .and_then(|l| l.get(ROLE_LABEL))
                .is_some_and(|v| v == "node");
            if !from_node {
                return Err("an ingress peer is not the node pod".into());
            }
        }
    }
    Ok(())
}

/// The verbs the node needs, and exactly those: create/attach/delete its
/// harness pods, read their state and logs, and read the policy it verifies.
const RIGHTS: &[(&str, &str, &str, &str)] = &[
    ("", "pods", "", "create"),
    ("", "pods", "", "get"),
    ("", "pods", "", "list"),
    ("", "pods", "", "delete"),
    ("", "pods", "attach", "create"),
    ("", "pods", "attach", "get"),
    ("", "pods", "log", "get"),
    ("networking.k8s.io", "networkpolicies", "", "get"),
];

/// The verbs a run's own Secret needs (`runner::kube::secret`): create it
/// before its pod, patch its owner to the pod, delete it once the pod has
/// started, list what a node that died left. Optional: a node without them
/// runs everything that carries no secret environment and refuses what does.
/// No `get`: nothing reads a Secret back.
pub const SECRET_RIGHTS: &[(&str, &str, &str, &str)] = &[
    ("", "secrets", "", "create"),
    ("", "secrets", "", "patch"),
    ("", "secrets", "", "delete"),
    ("", "secrets", "", "list"),
];

/// Whether this service account may do each of `rights` in `namespace`:
/// the first right it may not, or an error asking.
async fn first_refused(
    reviews: &Api<SelfSubjectAccessReview>,
    namespace: &str,
    rights: &[(&str, &str, &str, &str)],
) -> Result<Option<String>, kube::Error> {
    for (group, resource, sub, verb) in rights {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    group: Some(group.to_string()),
                    resource: Some(resource.to_string()),
                    subresource: (!sub.is_empty()).then(|| sub.to_string()),
                    verb: Some(verb.to_string()),
                    namespace: Some(namespace.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let allowed = reviews
            .create(&PostParams::default(), &review)
            .await?
            .status
            .is_some_and(|s| s.allowed);
        if !allowed {
            let what = if sub.is_empty() {
                resource.to_string()
            } else {
                format!("{resource}/{sub}")
            };
            return Ok(Some(format!("{verb} {what}")));
        }
    }
    Ok(None)
}

/// Whether the node may keep a run's secret environment in a Secret of its
/// own, in the words a refused run repeats.
pub async fn secret_rights(client: kube::Client, namespace: &str) -> Result<(), String> {
    let reviews: Api<SelfSubjectAccessReview> = Api::all(client);
    match first_refused(&reviews, namespace, SECRET_RIGHTS).await {
        Ok(None) => Ok(()),
        Ok(Some(refused)) => Err(format!(
            "the service account may not {refused} in {namespace}"
        )),
        Err(e) => Err(format!("access review: {e}")),
    }
}

pub async fn check_all(b: &KubeBackend, cfg: &Config, deep: bool) -> BoundaryReport {
    let mut checks = Vec::new();
    let runtime = check_runtime(b, cfg).await;
    let runtime_ok = runtime.ok;
    checks.push(runtime);
    // The egress port's verdict is replaced, never cleared ahead of time: a
    // re-check runs while sessions start and preparations open grants, and
    // an open port must not read as shut for the length of a check. Each
    // way this check can stop before the network check closes it (below);
    // the network check sets it otherwise.
    let stopped = |why: &str| {
        b.egress_port().set(Err(format!(
            "the boundary check stopped before it could confirm the egress port ({why})"
        )));
    };
    if !runtime_ok {
        stopped("runtime unavailable");
        for id in [
            CheckId::HarnessUnprivileged,
            CheckId::NoRuntimeSocket,
            CheckId::NetworkIsolated,
        ] {
            checks.push(CheckResult::fail(id, "not checked: runtime unavailable"));
        }
        return BoundaryReport { checks };
    }
    let runner = match b
        .runner_for(
            crate::session::materialize::state_mounts(
                &b.harness_home_str(),
                crate::adapter::layout(&b.cfg.harness.id),
            )
            .unwrap_or_default(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            stopped("no runner");
            for id in [
                CheckId::HarnessUnprivileged,
                CheckId::NoRuntimeSocket,
                CheckId::NetworkIsolated,
            ] {
                checks.push(CheckResult::fail(id, format!("runner: {e}")));
            }
            return BoundaryReport { checks };
        }
    };
    // What a node that died left: a run Secret is owned by its pod, but one
    // made in the moment before its pod was has no owner yet. Startup and
    // every re-check come through here, with or without sessions to
    // reconcile.
    if b.secret_rights().is_granted() {
        runner.sweep_run_secrets().await;
    }
    match create_probe(&runner).await {
        Ok(pod) => {
            checks.push(check_unprivileged(&pod));
            checks.push(check_no_runtime_socket(&pod));
            checks.push(check_network(b, &runner, &pod).await);
            let _ = runner
                .pods()
                .delete(PROBE, &DeleteParams::default().grace_period(0))
                .await;
        }
        Err(e) => {
            stopped("the probe pod failed");
            for id in [
                CheckId::HarnessUnprivileged,
                CheckId::NoRuntimeSocket,
                CheckId::NetworkIsolated,
            ] {
                checks.push(CheckResult::fail(id, format!("probe pod failed: {e}")));
            }
        }
    }
    if deep {
        checks.push(check_egress(b, cfg, &runner).await);
    }
    BoundaryReport { checks }
}

impl KubeBackend {
    fn harness_home_str(&self) -> String {
        use crate::boundary::Backend;
        self.harness_home()
    }
}

async fn check_runtime(b: &KubeBackend, cfg: &Config) -> CheckResult {
    let spec = match b.spec() {
        Ok(s) => s,
        Err(e) => return CheckResult::fail(CheckId::Runtime, e),
    };
    // What the egress proxy refuses is part of the boundary: an entry that
    // does not parse is a block the operator meant refused and is not.
    if let Some(error) = b.refusal_error() {
        return CheckResult::fail(CheckId::Runtime, error);
    }
    // The harness reaches the node by pod IP, so the harness listener must
    // face the pod network, not loopback or a socket.
    match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(addr) if !addr.ip().is_loopback() => {}
        other => {
            return CheckResult::fail(
                CheckId::Runtime,
                format!("harness_listen is {other}; a pod-hosted node needs 0.0.0.0:<port>"),
            )
        }
    }
    let client = match b.client().await {
        Ok(c) => c,
        Err(e) => return CheckResult::fail(CheckId::Runtime, e),
    };
    let reviews: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    match first_refused(&reviews, &spec.env.namespace, RIGHTS).await {
        Ok(None) => {}
        Ok(Some(refused)) => {
            return CheckResult::fail(
                CheckId::Runtime,
                format!(
                    "service account may not {refused} in {}",
                    spec.env.namespace
                ),
            )
        }
        Err(e) => return CheckResult::fail(CheckId::Runtime, format!("access review: {e}")),
    }
    // Optional, and never a reason to refuse the node: what it decides is
    // whether a run carrying credentials may start, which the runner asks.
    let secrets = secret_rights(client, &spec.env.namespace).await;
    b.secret_rights().set(secrets.clone());
    let secrets = match secrets {
        Ok(()) => "secrets granted, so a run lent credentials keeps them in a Secret of its own"
            .to_string(),
        Err(why) => format!(
            "secrets not granted ({why}), so a run lent credentials or an egress grant is refused"
        ),
    };
    CheckResult::ok(
        CheckId::Runtime,
        format!(
            "api reachable; pods/attach/log and networkpolicies granted in {}; {secrets}; node {}",
            spec.env.namespace, spec.env.node_name
        ),
    )
}

/// Create the probe (gated, never scheduled) and return it as admitted.
async fn create_probe(runner: &crate::runner::kube::KubeRunner) -> Result<Pod, String> {
    let pods = runner.pods();
    let _ = pods
        .delete(PROBE, &DeleteParams::default().grace_period(0))
        .await;
    let cmd = RunnerCommand {
        argv: vec!["true".into()],
        name: PROBE.into(),
        ..Default::default()
    };
    let pod = runner
        .spec()
        .pod(PROBE, &cmd, true)
        .map_err(|e| e.to_string())?;
    pods.create(&PostParams::default(), &pod)
        .await
        .map_err(|e| e.to_string())?;
    pods.get(PROBE).await.map_err(|e| e.to_string())
}

fn check_unprivileged(pod: &Pod) -> CheckResult {
    let Some(spec) = &pod.spec else {
        return CheckResult::fail(CheckId::HarnessUnprivileged, "pod has no spec");
    };
    let psc = spec.security_context.clone().unwrap_or_default();
    if psc.run_as_non_root != Some(true) || psc.run_as_user.unwrap_or(0) == 0 {
        return CheckResult::fail(CheckId::HarnessUnprivileged, "pod may run as root");
    }
    if psc.seccomp_profile.as_ref().map(|s| s.type_.as_str()) != Some("RuntimeDefault") {
        return CheckResult::fail(CheckId::HarnessUnprivileged, "no seccomp profile");
    }
    for c in spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten())
    {
        let sc = c.security_context.clone().unwrap_or_default();
        if sc.privileged.unwrap_or(false) {
            return CheckResult::fail(
                CheckId::HarnessUnprivileged,
                format!("{} is privileged", c.name),
            );
        }
        if sc.allow_privilege_escalation != Some(false) {
            return CheckResult::fail(
                CheckId::HarnessUnprivileged,
                format!("{} may escalate privileges", c.name),
            );
        }
        let caps = sc.capabilities.unwrap_or_default();
        if caps.drop.as_deref().map(|d| d.iter().any(|x| x == "ALL")) != Some(true) {
            return CheckResult::fail(
                CheckId::HarnessUnprivileged,
                format!("{} keeps capabilities", c.name),
            );
        }
        if caps.add.as_deref().map(|a| !a.is_empty()).unwrap_or(false) {
            return CheckResult::fail(
                CheckId::HarnessUnprivileged,
                format!(
                    "{} adds capabilities {:?}",
                    c.name,
                    caps.add.unwrap_or_default()
                ),
            );
        }
    }
    CheckResult::ok(
        CheckId::HarnessUnprivileged,
        format!(
            "non-root uid {}, no capabilities, no escalation, seccomp RuntimeDefault",
            psc.run_as_user.unwrap_or_default()
        ),
    )
}

fn check_no_runtime_socket(pod: &Pod) -> CheckResult {
    let Some(spec) = &pod.spec else {
        return CheckResult::fail(CheckId::NoRuntimeSocket, "pod has no spec");
    };
    if spec.host_network.unwrap_or(false)
        || spec.host_pid.unwrap_or(false)
        || spec.host_ipc.unwrap_or(false)
    {
        return CheckResult::fail(CheckId::NoRuntimeSocket, "pod shares a host namespace");
    }
    if spec.automount_service_account_token != Some(false) {
        return CheckResult::fail(
            CheckId::NoRuntimeSocket,
            "an API token is mounted into the harness",
        );
    }
    for v in spec.volumes.iter().flatten() {
        if v.host_path.is_some() {
            return CheckResult::fail(
                CheckId::NoRuntimeSocket,
                format!("hostPath volume {} reaches the node's host", v.name),
            );
        }
        if v.projected.is_some() || v.secret.is_some() {
            return CheckResult::fail(
                CheckId::NoRuntimeSocket,
                format!(
                    "volume {} carries something the node did not put there",
                    v.name
                ),
            );
        }
        if v.name != STATE_VOLUME && v.persistent_volume_claim.is_some() {
            return CheckResult::fail(
                CheckId::NoRuntimeSocket,
                format!("unexpected claim volume {}", v.name),
            );
        }
    }
    for c in &spec.containers {
        for m in c.volume_mounts.iter().flatten() {
            let p = m.mount_path.to_ascii_lowercase();
            if p.ends_with(".sock") || p.contains("docker.sock") || p.contains("containerd") {
                return CheckResult::fail(
                    CheckId::NoRuntimeSocket,
                    format!("socket path mounted into the harness: {}", m.mount_path),
                );
            }
        }
    }
    CheckResult::ok(
        CheckId::NoRuntimeSocket,
        "no host namespaces, no API token, no hostPath; only the state claim",
    )
}

/// Whether one rule's peers admit a harness pod: no `from` at all, or a pod
/// selector in this namespace that is empty or names the harness role.
/// Anything wider or stranger is not counted, so the egress port stays shut
/// rather than assumed open.
fn admits_harness(rule: &NetworkPolicyIngressRule) -> bool {
    let Some(peers) = rule.from.as_deref().filter(|peers| !peers.is_empty()) else {
        return true;
    };
    peers.iter().any(|peer| {
        peer.ip_block.is_none()
            && peer.namespace_selector.is_none()
            && peer.pod_selector.as_ref().is_some_and(|selector| {
                let labels = selector.match_labels.clone().unwrap_or_default();
                selector
                    .match_expressions
                    .as_deref()
                    .unwrap_or_default()
                    .is_empty()
                    && (labels.is_empty()
                        || labels.get(ROLE_LABEL).is_some_and(|role| role == "harness"))
            })
    })
}

/// One entry of a rule's `ports`, as far as the checks read it.
#[derive(Debug, PartialEq, Eq)]
enum PolicyPort {
    /// TCP (stated, or left to the default) on `first..=last`: one port when
    /// there is no `endPort`.
    Tcp { first: i32, last: i32 },
    /// Anything else: another protocol, a named port (not resolved here), or
    /// an entry naming no port, said as the refusal says it.
    Other(String),
}

/// The entries a rule names, or `None` for a rule that names none (every
/// port, every protocol).
fn rule_ports(
    ports: Option<&[k8s_openapi::api::networking::v1::NetworkPolicyPort]>,
) -> Option<Vec<PolicyPort>> {
    let ports = ports.filter(|ports| !ports.is_empty())?;
    Some(
        ports
            .iter()
            .map(|p| {
                let protocol = p.protocol.as_deref().unwrap_or("TCP");
                match (protocol, p.port.as_ref()) {
                    ("TCP", Some(IntOrString::Int(port))) => PolicyPort::Tcp {
                        first: *port,
                        last: p.end_port.unwrap_or(*port),
                    },
                    (_, Some(IntOrString::String(name))) => {
                        PolicyPort::Other(format!("the named port {name:?}"))
                    }
                    (_, None) => PolicyPort::Other(format!("every {protocol} port")),
                    (_, Some(IntOrString::Int(port))) => {
                        PolicyPort::Other(format!("{protocol} port {port}"))
                    }
                }
            })
            .collect(),
    )
}

/// Whether the node's own ingress policy lets a harness pod in on `port`.
/// Only the shape the deployment ships is read; a policy this cannot read as
/// admitting it is taken as not.
fn node_admits(rules: &[NetworkPolicyIngressRule], port: i32) -> bool {
    rules.iter().any(|rule| {
        admits_harness(rule)
            && rule_ports(rule.ports.as_deref()).is_none_or(|ports| {
                ports.iter().any(|entry| {
                    matches!(entry, PolicyPort::Tcp { first, last } if (*first..=*last).contains(&port))
                })
            })
    })
}

/// The harness policy's egress, rule by rule: every peer is the node pod and
/// every port is the forward, the proxy, or the egress port. `Ok(true)` when
/// some rule opens the egress port, which is optional: a policy from before
/// per-client egress on Kubernetes names two ports and still holds.
fn harness_egress(
    rules: &[NetworkPolicyEgressRule],
    forward: i32,
    proxy: i32,
    egress: i32,
) -> Result<bool, String> {
    if rules.is_empty() {
        return Err("policy allows no egress at all; the harness could not reach the node".into());
    }
    let mut opens_egress = false;
    for rule in rules {
        let peers = rule.to.clone().unwrap_or_default();
        if peers.is_empty() {
            return Err("an egress rule has no peer restriction".into());
        }
        for peer in peers {
            if peer.ip_block.is_some() || peer.namespace_selector.is_some() {
                return Err(
                    "egress reaches beyond the node's pod (ipBlock or namespaceSelector)".into(),
                );
            }
            let to_node = peer
                .pod_selector
                .as_ref()
                .and_then(|s| s.match_labels.as_ref())
                .and_then(|l| l.get(ROLE_LABEL))
                .map(|v| v == "node")
                .unwrap_or(false);
            if !to_node {
                return Err("an egress peer is not the node pod".into());
            }
        }
        let entries = rule_ports(rule.ports.as_deref()).unwrap_or_default();
        if entries.is_empty() {
            return Err("an egress rule allows every port".into());
        }
        let mut ports = Vec::new();
        for entry in entries {
            match entry {
                PolicyPort::Tcp { first, last } if first == last => ports.push(first),
                PolicyPort::Tcp { first, last } => {
                    return Err(format!(
                        "egress allows the port range {first}-{last}; only the forward, the \
                         proxy and the egress port are expected"
                    ))
                }
                PolicyPort::Other(what) => {
                    return Err(format!(
                        "egress allows {what}; only TCP to the forward, the proxy and the \
                         egress port is expected"
                    ))
                }
            }
        }
        if let Some(p) = ports
            .iter()
            .find(|p| **p != forward && **p != proxy && **p != egress)
        {
            return Err(format!(
                "egress allows port {p}, which is neither the forward, the proxy, nor the \
                 egress port"
            ));
        }
        opens_egress |= ports.contains(&egress);
    }
    Ok(opens_egress)
}

/// The network check, and with it whether per-client egress is on: the
/// egress port's verdict is recorded on the backend whatever the outcome, so
/// a failed or narrowed check closes it again.
async fn check_network(
    b: &KubeBackend,
    runner: &crate::runner::kube::KubeRunner,
    pod: &Pod,
) -> CheckResult {
    let (result, egress) = network(b, runner, pod).await;
    if let Err(why) = &egress {
        if result.ok {
            tracing::warn!(%why, "per-client egress is off");
        }
    }
    b.egress_port().set(egress);
    result
}

async fn network(
    b: &KubeBackend,
    runner: &crate::runner::kube::KubeRunner,
    pod: &Pod,
) -> (CheckResult, Result<(), String>) {
    let fail = |detail: String| {
        (
            CheckResult::fail(CheckId::NetworkIsolated, detail),
            Err("the network check failed".to_string()),
        )
    };
    let spec = runner.spec();
    let Some(ps) = &pod.spec else {
        return fail("pod has no spec".into());
    };
    if ps.dns_policy.as_deref() != Some("None") {
        return fail("harness pod has a resolver".into());
    }
    let alias_ok = ps.host_aliases.iter().flatten().any(|a| {
        a.ip == spec.env.pod_ip
            && a.hostnames
                .iter()
                .flatten()
                .any(|h| h == &spec.gateway_host)
    });
    if !alias_ok {
        return fail(format!(
            "{} does not resolve to this node's pod",
            spec.gateway_host
        ));
    }
    let client = match b.client().await {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let policies: Api<NetworkPolicy> = Api::namespaced(client, &spec.env.namespace);
    let policy = match policies.get(HARNESS_POLICY).await {
        Ok(p) => p,
        Err(e) => {
            return fail(format!(
                "NetworkPolicy {HARNESS_POLICY} missing in {}: {e}",
                spec.env.namespace
            ))
        }
    };
    let Some(pspec) = policy.spec else {
        return fail("policy has no spec".into());
    };
    let selects_harness = pspec
        .pod_selector
        .as_ref()
        .and_then(|s| s.match_labels.as_ref())
        .and_then(|l| l.get(ROLE_LABEL))
        .map(|v| v == "harness")
        .unwrap_or(false);
    if !selects_harness {
        return fail(format!(
            "{HARNESS_POLICY} does not select {ROLE_LABEL}=harness"
        ));
    }
    let types = pspec.policy_types.clone().unwrap_or_default();
    if !(types.iter().any(|t| t == "Ingress") && types.iter().any(|t| t == "Egress")) {
        return fail("policy must govern both Ingress and Egress".into());
    }
    if let Err(why) = ingress_only_from_node(pspec.ingress.as_deref().unwrap_or_default()) {
        return fail(why);
    }
    let forward = i32::from(match &b.cfg.gateway.harness_listen {
        HarnessListen::Tcp(a) => a.port(),
        HarnessListen::Unix(_) => 0,
    });
    let proxy = i32::from(spec.proxy_port);
    let egress_port = b.cfg.gateway.egress_port;
    let opens_egress = match harness_egress(
        pspec.egress.as_deref().unwrap_or_default(),
        forward,
        proxy,
        i32::from(egress_port),
    ) {
        Ok(opens) => opens,
        Err(why) => return fail(why),
    };
    let isolated = format!(
        "no resolver; {HARNESS_POLICY} allows egress only to the node pod on the forward and \
         proxy ports"
    );
    // The rest is whether per-client egress is on, which never fails the
    // check: the release and the policy that opens its port can roll out in
    // either order, and a node whose policy predates the port runs as it did.
    let egress = match opens_egress {
        false => Err(format!(
            "{HARNESS_POLICY} does not allow egress to the node on port {egress_port}, so no \
             grant is issued: a preparation's registries and a session's own hosts stay \
             shut. Add the port to its egress and to {NODE_POLICY}'s ingress \
             (deploy/kubernetes/base) and check the boundary again"
        )),
        true => match policies.get_opt(NODE_POLICY).await {
            Ok(None) => Ok(()),
            Ok(Some(node)) => {
                let ingress = node
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.ingress.as_deref())
                    .unwrap_or_default();
                let governs_ingress = node
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.policy_types.as_ref())
                    .is_none_or(|types| types.iter().any(|t| t == "Ingress"));
                match !governs_ingress || node_admits(ingress, i32::from(egress_port)) {
                    true => Ok(()),
                    false => Err(format!(
                        "{NODE_POLICY} does not admit harness pods on port {egress_port}, so \
                         no grant is issued: a preparation's registries and a session's own \
                         hosts stay shut. Add the port to its ingress (deploy/kubernetes/base) \
                         and check the boundary again"
                    )),
                }
            }
            Err(e) => Err(format!(
                "could not read {NODE_POLICY} to learn whether it admits the egress port: {e}"
            )),
        },
    };
    let detail = match &egress {
        Ok(()) => format!("{isolated}, and on the egress port {egress_port} for grants"),
        Err(why) => format!("{isolated}; {why}"),
    };
    (CheckResult::ok(CheckId::NetworkIsolated, detail), egress)
}

const ANONYMOUS_GRANT: &str = "GRANT_ANONYMOUS_OK";
const EGRESS_CONNECT: &str = "EGRESS_CONNECT=";

/// The per-client proxy asked with no credentials, from inside a harness pod:
/// it must challenge (407), and must never serve. Asked first, as Podman's
/// probe asks it: the shared script's last command decides its exit status.
fn anonymous_egress_probe(gateway_host: &str, port: u16) -> String {
    format!(
        "curl -s -o /dev/null -m 10 -w '{EGRESS_CONNECT}%{{http_connect}}\\n' \
         -x http://{gateway_host}:{port} https://example.com/ && echo {ANONYMOUS_GRANT}; echo; "
    )
}

/// What the anonymous probe found: a failure when the proxy served the
/// request, or did not challenge it while the checks say it is reachable.
fn anonymous_egress_verdict(out: &str, port: u16) -> Option<CheckResult> {
    if out.contains(ANONYMOUS_GRANT) {
        return Some(CheckResult::fail(
            CheckId::Egress,
            "the per-client egress proxy served a request that presented no grant",
        ));
    }
    let status = out
        .lines()
        .find_map(|line| line.trim().strip_prefix(EGRESS_CONNECT))
        .unwrap_or("none");
    (status != "407").then(|| {
        CheckResult::fail(
            CheckId::Egress,
            format!(
                "the per-client egress proxy on port {port} did not challenge a request with \
                 no credentials (its CONNECT answered {status}, not 407): is the port open \
                 in {NODE_POLICY}'s ingress, and is the node serving?"
            ),
        )
    })
}

async fn check_egress(
    b: &KubeBackend,
    cfg: &Config,
    runner: &crate::runner::kube::KubeRunner,
) -> CheckResult {
    let forward = match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(a) => a.port(),
        HarnessListen::Unix(_) => cfg.gateway.forward_port,
    };
    let gateway_host = &runner.spec().gateway_host;
    // Only once the policies open the port: before then a harness pod cannot
    // reach it, and that is the network check's to say.
    let anonymous = b.egress_port().is_open();
    let mut script = egress_script(gateway_host, forward);
    if anonymous {
        script = anonymous_egress_probe(gateway_host, cfg.gateway.egress_port) + &script;
    }
    let cmd = RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        name: "tracon-egress-probe".into(),
        ..Default::default()
    };
    match runner.run_capture(cmd).await {
        // A NetworkPolicy drops; it has no reject verb, and no portable
        // CNI option turns one into an RST or an ICMP unreachable. The
        // measurement is still taken and still reported — it is the
        // difference between a harness whose timeout-free download fails
        // and one that hangs — but it is recorded as this runtime's
        // limitation rather than as a failure of a node that never claimed
        // the property.
        Ok(out) => {
            let out = String::from_utf8_lossy(&out.stdout);
            let refused = anonymous
                .then(|| anonymous_egress_verdict(&out, cfg.gateway.egress_port))
                .flatten();
            refused.unwrap_or_else(|| egress_verdict(&out, Denial::Drops))
        }
        Err(e) => CheckResult::fail(CheckId::Egress, format!("probe failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::kube::{KubeSpec, PodEnv};

    fn probe() -> Pod {
        let mut cfg = Config::default();
        cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
        let spec = KubeSpec::from_config(
            &cfg,
            PodEnv {
                namespace: "ns".into(),
                pod_ip: "10.0.0.5".into(),
                node_name: "n".into(),
            },
        );
        spec.pod("probe", &RunnerCommand::default(), true).unwrap()
    }

    #[test]
    fn the_rendered_probe_passes_its_own_static_checks() {
        let p = probe();
        assert!(
            check_unprivileged(&p).ok,
            "{}",
            check_unprivileged(&p).detail
        );
        assert!(
            check_no_runtime_socket(&p).ok,
            "{}",
            check_no_runtime_socket(&p).detail
        );
    }

    fn ingress(
        peers: Option<Vec<k8s_openapi::api::networking::v1::NetworkPolicyPeer>>,
    ) -> k8s_openapi::api::networking::v1::NetworkPolicyIngressRule {
        k8s_openapi::api::networking::v1::NetworkPolicyIngressRule {
            from: peers,
            ports: None,
        }
    }

    fn pods(role: &str) -> k8s_openapi::api::networking::v1::NetworkPolicyPeer {
        k8s_openapi::api::networking::v1::NetworkPolicyPeer {
            pod_selector: Some(
                k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                    match_labels: Some([(ROLE_LABEL.to_string(), role.to_string())].into()),
                    ..Default::default()
                },
            ),
            ..Default::default()
        }
    }

    /// The node must reach an OpenCode harness's server; nothing else may.
    #[test]
    fn harness_ingress_admits_the_node_pod_and_nothing_else() {
        assert!(ingress_only_from_node(&[]).is_ok());
        assert!(ingress_only_from_node(&[ingress(Some(vec![pods("node")]))]).is_ok());

        assert!(
            ingress_only_from_node(&[ingress(None)]).is_err(),
            "no from admits all"
        );
        assert!(ingress_only_from_node(&[ingress(Some(vec![]))]).is_err());
        assert!(ingress_only_from_node(&[ingress(Some(vec![pods("harness")]))]).is_err());
        let mut wide = pods("node");
        wide.namespace_selector = Some(Default::default());
        assert!(ingress_only_from_node(&[ingress(Some(vec![wide]))]).is_err());
        let block = k8s_openapi::api::networking::v1::NetworkPolicyPeer {
            ip_block: Some(k8s_openapi::api::networking::v1::IPBlock {
                cidr: "0.0.0.0/0".into(),
                except: None,
            }),
            ..Default::default()
        };
        assert!(ingress_only_from_node(&[ingress(Some(vec![block]))]).is_err());
    }

    fn port(port: i32) -> k8s_openapi::api::networking::v1::NetworkPolicyPort {
        k8s_openapi::api::networking::v1::NetworkPolicyPort {
            port: Some(IntOrString::Int(port)),
            protocol: Some("TCP".into()),
            ..Default::default()
        }
    }

    fn egress_to_node(ports: &[i32]) -> NetworkPolicyEgressRule {
        NetworkPolicyEgressRule {
            to: Some(vec![pods("node")]),
            ports: Some(ports.iter().map(|p| port(*p)).collect()),
        }
    }

    /// A policy from before per-client egress (two ports) still holds and
    /// leaves grants off; one with the egress port holds and turns them on;
    /// any other port is still a failure.
    #[test]
    fn the_harness_policy_may_name_the_egress_port_or_not() {
        assert_eq!(
            harness_egress(&[egress_to_node(&[7421, 8888])], 7421, 8888, 7424),
            Ok(false)
        );
        assert_eq!(
            harness_egress(&[egress_to_node(&[7421, 8888, 7424])], 7421, 8888, 7424),
            Ok(true)
        );
        assert_eq!(
            harness_egress(
                &[egress_to_node(&[7421, 8888]), egress_to_node(&[7424])],
                7421,
                8888,
                7424
            ),
            Ok(true)
        );
        let wider = harness_egress(&[egress_to_node(&[7421, 8888, 7420])], 7421, 8888, 7424);
        assert!(wider.unwrap_err().contains("port 7420"));
        assert!(harness_egress(&[egress_to_node(&[])], 7421, 8888, 7424).is_err());
        assert!(harness_egress(&[], 7421, 8888, 7424).is_err());
        let mut anywhere = egress_to_node(&[7424]);
        anywhere.to = Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyPeer {
            ip_block: Some(k8s_openapi::api::networking::v1::IPBlock {
                cidr: "0.0.0.0/0".into(),
                except: None,
            }),
            ..Default::default()
        }]);
        assert!(harness_egress(&[anywhere], 7421, 8888, 7424).is_err());

        // TCP, stated or by default, one port per entry: a range, another
        // protocol or an unresolved name is more than the check can vouch for.
        let mut unstated = egress_to_node(&[7421, 8888, 7424]);
        for entry in unstated.ports.iter_mut().flatten() {
            entry.protocol = None;
        }
        assert_eq!(harness_egress(&[unstated], 7421, 8888, 7424), Ok(true));
        let mut udp = egress_to_node(&[7421, 8888]);
        udp.ports.as_mut().unwrap()[1].protocol = Some("UDP".into());
        let why = harness_egress(&[udp], 7421, 8888, 7424).unwrap_err();
        assert!(why.contains("UDP port 8888"), "{why}");
        let mut range = egress_to_node(&[7421, 8888]);
        range.ports.as_mut().unwrap()[0].end_port = Some(7430);
        let why = harness_egress(&[range], 7421, 8888, 7424).unwrap_err();
        assert!(why.contains("range 7421-7430"), "{why}");
        let mut named = egress_to_node(&[7421, 8888]);
        named.ports.as_mut().unwrap()[0].port = Some(IntOrString::String("forward".into()));
        let why = harness_egress(&[named], 7421, 8888, 7424).unwrap_err();
        assert!(why.contains("named port"), "{why}");
    }

    /// The node's ingress admits harness pods on the egress port only when a
    /// rule from them names it (or names no port); the operator API's rule
    /// for the ingress controller does not count.
    #[test]
    fn the_node_policy_admits_the_egress_port_or_not() {
        let from_harness = |ports: Option<&[i32]>| NetworkPolicyIngressRule {
            from: Some(vec![pods("harness")]),
            ports: ports.map(|ports| ports.iter().map(|p| port(*p)).collect()),
        };
        let controller = NetworkPolicyIngressRule {
            from: Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyPeer {
                namespace_selector: Some(Default::default()),
                ..Default::default()
            }]),
            ports: Some(vec![port(7424)]),
        };
        assert!(!node_admits(
            &[from_harness(Some(&[7421, 8888])), controller.clone()],
            7424
        ));
        assert!(node_admits(
            &[from_harness(Some(&[7421, 8888, 7424])), controller],
            7424
        ));
        assert!(node_admits(&[from_harness(None)], 7424));
        assert!(!node_admits(&[ingress(Some(vec![pods("other")]))], 7424));
        assert!(node_admits(&[ingress(None)], 7424));
        // A TCP range that holds the port admits it; UDP on it does not.
        let mut range = from_harness(Some(&[7420]));
        range.ports.as_mut().unwrap()[0].end_port = Some(7430);
        assert!(node_admits(&[range], 7424));
        let mut udp = from_harness(Some(&[7424]));
        udp.ports.as_mut().unwrap()[0].protocol = Some("UDP".into());
        assert!(!node_admits(&[udp], 7424));
    }

    /// The deep probe asks the per-client port with no credentials: a
    /// challenge passes, anything served fails, and silence fails while the
    /// checks say the port is reachable.
    #[test]
    fn the_per_client_port_must_challenge_an_anonymous_request() {
        let script = anonymous_egress_probe("tracon-gw", 7424);
        assert!(script.contains("-x http://tracon-gw:7424"), "{script}");
        assert!(script.contains("%{http_connect}"), "{script}");
        assert!(anonymous_egress_verdict("EGRESS_CONNECT=407\npong", 7424).is_none());
        let served = anonymous_egress_verdict("EGRESS_CONNECT=200\nGRANT_ANONYMOUS_OK\n", 7424);
        assert!(served.unwrap().detail.contains("presented no grant"));
        let silent = anonymous_egress_verdict("EGRESS_CONNECT=000\n", 7424).unwrap();
        assert!(silent.detail.contains("answered 000"), "{}", silent.detail);
        assert!(silent.detail.contains(NODE_POLICY), "{}", silent.detail);
        assert!(anonymous_egress_verdict("", 7424).is_some());
    }

    /// The shipped policies open the egress port on both sides, so a fresh
    /// deployment issues grants.
    #[test]
    fn the_shipped_policies_open_the_egress_port() {
        let port = crate::config::Config::default().gateway.egress_port;
        for file in ["networkpolicy-harness.yaml", "networkpolicy-node.yaml"] {
            let policy = super::super::Manifests::get(file).unwrap();
            let policy = String::from_utf8_lossy(&policy.data);
            assert!(
                policy.contains(&format!("port: {port}")),
                "{file}:\n{policy}"
            );
        }
        let deployment = super::super::Manifests::get("deployment.yaml").unwrap();
        let deployment = String::from_utf8_lossy(&deployment.data);
        assert!(
            deployment.contains(&format!("containerPort: {port}")),
            "{deployment}"
        );
    }

    /// The Role the deployment ships grants the optional secrets set, and
    /// exactly it: no `get`, since nothing reads a Secret back.
    #[test]
    fn the_shipped_role_grants_the_secret_rights() {
        let role = super::super::Manifests::get("role.yaml").unwrap();
        let role = String::from_utf8_lossy(&role.data);
        let mut lines = role
            .lines()
            .skip_while(|l| !l.contains(r#"resources: ["secrets"]"#));
        assert!(lines.next().is_some(), "no secrets rule:\n{role}");
        let verbs = lines.next().unwrap_or_default();
        let mut granted: Vec<&str> = verbs
            .split(['[', ']', ',', '"'])
            .map(str::trim)
            .filter(|v| !v.is_empty() && !v.starts_with("verbs"))
            .collect();
        granted.sort_unstable();
        let mut asserted: Vec<&str> = SECRET_RIGHTS.iter().map(|(_, _, _, verb)| *verb).collect();
        asserted.sort_unstable();
        assert_eq!(granted, asserted);
    }

    #[test]
    fn admission_that_adds_privilege_or_a_host_path_fails() {
        let mut p = probe();
        p.spec.as_mut().unwrap().containers[0]
            .security_context
            .as_mut()
            .unwrap()
            .privileged = Some(true);
        assert!(!check_unprivileged(&p).ok);
        let mut p = probe();
        p.spec.as_mut().unwrap().volumes.as_mut().unwrap().push(
            k8s_openapi::api::core::v1::Volume {
                name: "sock".into(),
                host_path: Some(k8s_openapi::api::core::v1::HostPathVolumeSource {
                    path: "/run/containerd/containerd.sock".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert!(!check_no_runtime_socket(&p).ok);
        let mut p = probe();
        p.spec.as_mut().unwrap().automount_service_account_token = Some(true);
        assert!(!check_no_runtime_socket(&p).ok);
    }
}
