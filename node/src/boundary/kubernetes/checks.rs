//! The Kubernetes answers to the five checks. What can be inspected here is
//! the admitted pod spec (a sandbox probe pod the provider renders, gated,
//! so admission mutates it and nothing ever schedules it), the
//! NetworkPolicies the deployment carries, and the node's own rights;
//! `--deep` runs the shared egress script in a real sandbox, through the
//! tunnels a session gets.

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicyIngressRule};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{DeleteParams, PostParams};
use kube::Api;

use super::KubeBackend;
use crate::boundary::checks::{egress_script, egress_verdict, CheckId, CheckResult, Denial};
use crate::boundary::BoundaryReport;
use crate::config::{Config, HarnessListen};
use crate::provider::kubernetes::pod::ROLE_LABEL;
use crate::runner::RunnerCommand;

fn probe() -> String {
    crate::config::resource_name("boundary-probe")
}

/// The node's own ingress policy, read only to learn whether it admits
/// sandboxes on the sandbox port: `<prefix>-node`.
pub fn node_policy() -> String {
    crate::config::resource_name("node")
}

/// The verbs the node needs, and exactly those: make, read and remove its
/// sandbox pods, read their logs (what a supervisor that
/// never connected said), and read the policies it verifies. No attach, since
/// stdio is the protocol's, and nothing on Secrets, since no run keeps
/// anything in one.
pub const PROTOCOL_RIGHTS: &[(&str, &str, &str, &str)] = &[
    ("", "pods", "", "create"),
    ("", "pods", "", "get"),
    ("", "pods", "", "list"),
    ("", "pods", "", "delete"),
    ("", "pods", "log", "get"),
    ("networking.k8s.io", "networkpolicies", "", "get"),
    // Every policy that might select a sandbox too, and widen its route.
    ("networking.k8s.io", "networkpolicies", "", "list"),
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
        if v.persistent_volume_claim.is_some() {
            return CheckResult::fail(
                CheckId::NoRuntimeSocket,
                format!("claim volume {} is mounted into the sandbox", v.name),
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
        "no host namespaces, no API token, no hostPath, no claim",
    )
}

/// Whether one rule's peers admit a pod of `role`: no `from` at all, or a
/// pod selector in this namespace that is empty or names the role. Anything
/// wider or stranger is not counted, so the port stays shut rather than
/// assumed open.
fn admits_role(rule: &NetworkPolicyIngressRule, role: &str) -> bool {
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
                    && (labels.is_empty() || labels.get(ROLE_LABEL).is_some_and(|r| r == role))
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

/// Whether the node's own ingress policy lets a pod of `role` in on `port`.
/// Only the shape the deployment ships is read; a policy this cannot read as
/// admitting it is taken as not.
fn node_admits_role(rules: &[NetworkPolicyIngressRule], role: &str, port: i32) -> bool {
    rules.iter().any(|rule| {
        admits_role(rule, role)
            && rule_ports(rule.ports.as_deref()).is_none_or(|ports| {
                ports.iter().any(|entry| {
                    matches!(entry, PolicyPort::Tcp { first, last } if (*first..=*last).contains(&port))
                })
            })
    })
}

/// `labels` as a selector reads: `k=v,k=v`.
fn selector_words(labels: &std::collections::BTreeMap<String, String>) -> String {
    labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Whether a policy is the single route a sandbox pod is allowed (the
/// shape shipped in `deploy/kubernetes/base`): it selects exactly
/// this node's sandboxes (their role and this node's `tracon.dev/node`),
/// governs both directions, admits nothing in, and lets out only TCP to this
/// node's own pod on `port`. No rule for DNS, the node pod's other ports,
/// another node's pod, the API server or anything else.
pub fn sandbox_policy_shape(policy: &NetworkPolicy, port: i32) -> Result<(), String> {
    let name = crate::provider::kubernetes::sandbox_policy();
    let sandboxes = crate::provider::kubernetes::sandbox_pod_labels();
    let node = crate::provider::kubernetes::node_pod_labels();
    let spec = policy
        .spec
        .as_ref()
        .ok_or_else(|| format!("{name} has no spec"))?;
    let selector = spec.pod_selector.clone().unwrap_or_default();
    let exactly = selector.match_labels.clone().unwrap_or_default() == sandboxes
        && selector
            .match_expressions
            .as_deref()
            .unwrap_or_default()
            .is_empty();
    if !exactly {
        return Err(format!(
            "{name} does not select exactly {}: this node's sandboxes, and no other node's",
            selector_words(&sandboxes)
        ));
    }
    let types = spec.policy_types.clone().unwrap_or_default();
    if !(types.iter().any(|t| t == "Ingress") && types.iter().any(|t| t == "Egress")) {
        return Err(format!("{name} must govern both Ingress and Egress"));
    }
    if !spec.ingress.as_deref().unwrap_or_default().is_empty() {
        return Err(format!(
            "{name} admits ingress; nothing ever dials a sandbox, so it should admit none"
        ));
    }
    let rules = spec.egress.as_deref().unwrap_or_default();
    if rules.is_empty() {
        return Err(format!(
            "{name} allows no egress at all; a sandbox could not reach the node"
        ));
    }
    for rule in rules {
        let peers = rule.to.as_deref().unwrap_or_default();
        if peers.is_empty() {
            return Err(format!("an egress rule of {name} has no peer restriction"));
        }
        for peer in peers {
            let to_node = peer.ip_block.is_none()
                && peer.namespace_selector.is_none()
                && peer.pod_selector.as_ref().is_some_and(|s| {
                    s.match_expressions
                        .as_deref()
                        .unwrap_or_default()
                        .is_empty()
                        && s.match_labels.as_ref().is_some_and(|labels| {
                            node.iter()
                                .all(|(key, value)| labels.get(key) == Some(value))
                        })
                });
            if !to_node {
                return Err(format!(
                    "{name} lets a sandbox reach beyond this node's pod ({}): another \
                     node's, another pod, a namespace or an address block",
                    selector_words(&node)
                ));
            }
        }
        let entries = rule_ports(rule.ports.as_deref()).unwrap_or_default();
        if entries.is_empty() {
            return Err(format!("an egress rule of {name} allows every port"));
        }
        for entry in entries {
            match entry {
                PolicyPort::Tcp { first, last } if first == port && last == port => {}
                PolicyPort::Tcp { first, last } if first == last => {
                    return Err(format!(
                        "{name} allows port {first}; only the sandbox port {port} is expected \
                         (DNS, the forward, the proxy and the egress port are not)"
                    ))
                }
                PolicyPort::Tcp { first, last } => {
                    return Err(format!(
                        "{name} allows the port range {first}-{last}; only the sandbox port \
                         {port} is expected"
                    ))
                }
                PolicyPort::Other(what) => {
                    return Err(format!(
                        "{name} allows {what}; only TCP to the sandbox port {port} is expected"
                    ))
                }
            }
        }
    }
    Ok(())
}

/// The five checks: the admitted pod (a gated probe the provider renders,
/// never scheduled), the policies, the node's rights; `--deep` runs the
/// egress script in a real sandbox, through the tunnels a session gets.
pub async fn check_all(b: &KubeBackend, cfg: &Config, deep: bool) -> BoundaryReport {
    let mut checks = Vec::new();
    let rest = |checks: &mut Vec<CheckResult>, why: &str| {
        for id in [
            CheckId::HarnessUnprivileged,
            CheckId::NoRuntimeSocket,
            CheckId::NetworkIsolated,
        ] {
            checks.push(CheckResult::fail(id, why.to_string()));
        }
    };
    let provider = match b.sandbox_provider().await {
        Ok(provider) => provider,
        Err(e) => {
            checks.push(CheckResult::fail(CheckId::Runtime, e.clone()));
            rest(&mut checks, "not checked: runtime unavailable");
            return BoundaryReport { checks };
        }
    };
    let runtime = sandbox_runtime(b, &provider).await;
    let runtime_ok = runtime.ok;
    checks.push(runtime);
    if !runtime_ok {
        rest(&mut checks, "not checked: runtime unavailable");
        return BoundaryReport { checks };
    }
    match create_sandbox_probe(&provider).await {
        Ok(pod) => {
            checks.push(check_unprivileged(&pod));
            checks.push(sandbox_storage(&pod, &provider));
            checks.push(sandbox_network(&provider, &pod).await);
            let _ = provider
                .pods()
                .delete(&probe(), &DeleteParams::default().grace_period(0))
                .await;
        }
        Err(e) => rest(&mut checks, &format!("probe pod failed: {e}")),
    }
    if deep {
        checks.push(sandbox_egress(b, cfg, &provider).await);
    }
    BoundaryReport { checks }
}

async fn sandbox_runtime(
    b: &KubeBackend,
    provider: &crate::provider::kubernetes::KubeSandboxProvider,
) -> CheckResult {
    let spec = &provider.options().spec;
    if let Some(error) = b.refusal_error() {
        return CheckResult::fail(CheckId::Runtime, error);
    }
    let client = match b.client().await {
        Ok(c) => c,
        Err(e) => return CheckResult::fail(CheckId::Runtime, e),
    };
    let reviews: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    match first_refused(&reviews, &spec.env.namespace, PROTOCOL_RIGHTS).await {
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
    if let Err(why) = own_pod_labelled(client, &provider.options().node_namespace).await {
        return CheckResult::fail(CheckId::Runtime, why);
    }
    let dispatcher = match provider.dispatcher().await {
        Ok(d) => d,
        Err(e) => return CheckResult::fail(CheckId::Runtime, e.to_string()),
    };
    // The node's own bundle: fetched and checked for its release unless it
    // is given one, then packed for the init containers.
    if crate::bundle::managed(&b.cfg) {
        if let Err(e) = crate::bundle::ensure(&b.cfg).await {
            return CheckResult::fail(CheckId::Runtime, format!("harness bundle: {e}"));
        }
    }
    let bundle = match provider.bundle(&dispatcher).await {
        Ok(bundle) => bundle,
        Err(e) => return CheckResult::fail(CheckId::Runtime, format!("harness bundle: {e}")),
    };
    CheckResult::ok(
        CheckId::Runtime,
        format!(
            "api reachable; pods, pods/log and networkpolicies granted in {}; sandboxes over \
             the protocol dial {} and fetch the bundle {} (sha256 {}) there, and no run keeps \
             anything in a Secret; node {}; the node's pod is labelled {}",
            spec.env.namespace,
            provider.advertise(&dispatcher),
            provider.options().bundle.display(),
            &bundle.sha256[..12],
            spec.env.node_name,
            selector_words(&crate::provider::kubernetes::node_pod_labels()),
        ),
    )
}

/// Whether the node's own pod carries the labels its sandboxes' affinity
/// and policy find it by. Without them every sandbox would wait, unplaced,
/// until its start timed out; with another node's, it would be placed by
/// that one.
async fn own_pod_labelled(client: kube::Client, namespace: &str) -> Result<(), String> {
    let name = crate::provider::kubernetes::pod::PodEnv::own_pod_name().ok_or(
        "the node's own pod is unknown (set TRACON_POD_NAME from metadata.name), so the \
         labels its sandboxes find it by cannot be checked",
    )?;
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    let pod = pods
        .get_opt(&name)
        .await
        .map_err(|e| format!("read the node's own pod {name}: {e}"))?
        .ok_or_else(|| format!("the node's own pod {name} is not in {namespace}"))?;
    match crate::provider::kubernetes::node_pod_unlabelled(&pod.metadata.labels.unwrap_or_default())
    {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// The provider's own rendering of a sandbox, gated so it is admitted and
/// never scheduled, as admitted.
async fn create_sandbox_probe(
    provider: &crate::provider::kubernetes::KubeSandboxProvider,
) -> Result<Pod, String> {
    use crate::provider::{Bootstrap, CreateSpec, Resources, Shape};
    let pods = provider.pods();
    let _ = pods
        .delete(&probe(), &DeleteParams::default().grace_period(0))
        .await;
    let pin = crate::runner::protocol::node_server()
        .map_err(|e| e.to_string())?
        .pin()
        .to_string();
    let labels = crate::runner::RunLabels::of(crate::runner::RunKind::Probe);
    let dispatcher = provider.dispatcher().await.map_err(|e| e.to_string())?;
    let bundle = provider
        .bundle(&dispatcher)
        .await
        .map_err(|e| e.to_string())?;
    let name = probe();
    let pod = provider
        .render(
            &name,
            &CreateSpec {
                name: Some(&name),
                image: "",
                resources: &Resources::default(),
                mounts: &[crate::runner::Mount::local("/work")],
                bootstrap: &Bootstrap {
                    run: name.clone(),
                    cert_pin: pin,
                    token: String::new(),
                },
                labels: &labels,
                shape: Shape::Session,
            },
            &provider.advertise(&dispatcher),
            &bundle.sha256,
            true,
        )
        .map_err(|e| e.to_string())?;
    pods.create(&PostParams::default(), &pod)
        .await
        .map_err(|e| e.to_string())?;
    pods.get(&name).await.map_err(|e| e.to_string())
}

fn sandbox_storage(
    pod: &Pod,
    provider: &crate::provider::kubernetes::KubeSandboxProvider,
) -> CheckResult {
    let base = check_no_runtime_socket(pod);
    if !base.ok {
        return base;
    }
    let found = crate::provider::kubernetes::breaches(pod, &provider.options().spec.gateway_host);
    if !found.is_empty() {
        return CheckResult::fail(CheckId::NoRuntimeSocket, found.join("; "));
    }
    CheckResult::ok(
        CheckId::NoRuntimeSocket,
        "no host namespaces, no API token, no hostPath, no Secret and nothing of the state \
         claim: only emptyDirs of the pod's own, the bundle read-only among them",
    )
}

async fn sandbox_network(
    provider: &crate::provider::kubernetes::KubeSandboxProvider,
    pod: &Pod,
) -> CheckResult {
    let fail = |detail: String| CheckResult::fail(CheckId::NetworkIsolated, detail);
    let spec = &provider.options().spec;
    let Some(ps) = &pod.spec else {
        return fail("pod has no spec".into());
    };
    if ps.dns_policy.as_deref() != Some("None") {
        return fail("sandbox pod has a resolver".into());
    }
    let local = |ip: &str| {
        ip.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    };
    let aliases: Vec<_> = ps.host_aliases.iter().flatten().collect();
    if let Some(alias) = aliases.iter().find(|a| !local(&a.ip)) {
        return fail(format!(
            "a name in the sandbox pod resolves to {}",
            alias.ip
        ));
    }
    if !aliases.iter().any(|a| {
        a.hostnames
            .iter()
            .flatten()
            .any(|h| h == &spec.gateway_host)
    }) {
        return fail(format!(
            "{} is not the sandbox's loopback, where its tunnels listen",
            spec.gateway_host
        ));
    }
    let port = match provider.dispatcher().await {
        Ok(d) => i32::from(d.local_addr().port()),
        Err(e) => return fail(e.to_string()),
    };
    let policies = provider.policies();
    let name = crate::provider::kubernetes::sandbox_policy();
    let policy = match policies.get(&name).await {
        Ok(p) => p,
        Err(e) => {
            return fail(format!(
                "NetworkPolicy {name} missing in {}: {e}; apply \
                 deploy/kubernetes/base",
                spec.env.namespace
            ))
        }
    };
    if let Err(why) = sandbox_policy_shape(&policy, port) {
        return fail(why);
    }
    // NetworkPolicies add up: another that selects this node's sandboxes
    // with a rule of its own widens the one route.
    match policies.list(&kube::api::ListParams::default()).await {
        Ok(listed) => {
            let labels = pod.metadata.labels.clone().unwrap_or_default();
            let wider =
                crate::provider::kubernetes::widening_policies(&listed.items, &name, &labels);
            if !wider.is_empty() {
                return fail(format!(
                    "{} also select this node's sandboxes and allow them more than {name} \
                     does: NetworkPolicies add up, so the route is wider than one; narrow \
                     their selectors (an older {}=sandbox policy wants {}=<its prefix>)",
                    wider.join(", "),
                    ROLE_LABEL,
                    crate::provider::kubernetes::pod::NODE_LABEL
                ));
            }
        }
        Err(e) => {
            return fail(format!(
                "could not list the NetworkPolicies in {}, any of which might widen {name}: {e}",
                spec.env.namespace
            ))
        }
    }
    let node = node_policy();
    match policies.get_opt(&node).await {
        Ok(None) => {}
        Ok(Some(policy)) => {
            let ingress = policy
                .spec
                .as_ref()
                .and_then(|s| s.ingress.as_deref())
                .unwrap_or_default();
            let governs = policy
                .spec
                .as_ref()
                .and_then(|s| s.policy_types.as_ref())
                .is_none_or(|types| types.iter().any(|t| t == "Ingress"));
            if governs
                && !node_admits_role(ingress, crate::provider::kubernetes::SANDBOX_ROLE, port)
            {
                return fail(format!(
                    "{node} does not admit sandboxes on port {port}, so none could connect; \
                     add it to its ingress (deploy/kubernetes/base)"
                ));
            }
        }
        Err(e) => return fail(format!("could not read {node}: {e}")),
    }
    CheckResult::ok(
        CheckId::NetworkIsolated,
        format!(
            "no resolver; {} is the sandbox's loopback; {name} allows egress only to the node \
             pod on the sandbox port {port} and no ingress; the MCP and model gateway and both \
             proxies are tunnels over that one connection",
            spec.gateway_host
        ),
    )
}

/// What only a sandbox must also show from inside: no name resolves, and
/// the node pod's own ports are not there except through the tunnels.
fn sandbox_only_script(pod_ip: &str, forward: u16) -> String {
    let host = match pod_ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{pod_ip}]"),
        _ => pod_ip.to_string(),
    };
    format!(
        "getent hosts example.com >/dev/null 2>&1 && echo DIRECT_DNS_OK; \
         curl -s -o /dev/null -m 5 --noproxy '*' http://{host}:{forward}/harness/ping && echo DIRECT_NODE_OK; "
    )
}

fn sandbox_only_verdict(out: &str, egress_port: u16) -> CheckResult {
    if out.contains("DIRECT_DNS_OK") {
        return CheckResult::fail(CheckId::Egress, "a name resolved inside the sandbox");
    }
    if out.contains("DIRECT_NODE_OK") {
        return CheckResult::fail(
            CheckId::Egress,
            "the sandbox reached the node pod's forward port directly, around its tunnels",
        );
    }
    if let Some(refused) = anonymous_egress_verdict(out, egress_port) {
        return refused;
    }
    let verdict = egress_verdict(out, Denial::Drops);
    match verdict.ok {
        true => CheckResult::ok(
            CheckId::Egress,
            format!(
                "{}; from inside, no name resolves and the node pod answers only through the \
                 tunnels",
                verdict.detail
            ),
        ),
        false => verdict,
    }
}

async fn sandbox_egress(
    b: &KubeBackend,
    cfg: &Config,
    provider: &crate::provider::kubernetes::KubeSandboxProvider,
) -> CheckResult {
    let spec = &provider.options().spec;
    let forward = match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(a) => a.port(),
        HarnessListen::Unix(_) => cfg.gateway.forward_port,
    };
    let script = format!(
        "{}{}{}",
        sandbox_only_script(&spec.env.pod_ip, forward),
        anonymous_egress_probe(&spec.gateway_host, cfg.gateway.egress_port),
        egress_script(&spec.gateway_host, forward)
    );
    let cmd = RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        name: crate::config::resource_name("egress-probe"),
        labels: crate::runner::RunLabels::of(crate::runner::RunKind::Probe),
        ..Default::default()
    };
    let runner = crate::boundary::Backend::runner(b, Vec::new());
    match runner.run_probe(cmd).await {
        Ok(out) => sandbox_only_verdict(
            &String::from_utf8_lossy(&out.stdout),
            cfg.gateway.egress_port,
        ),
        Err(e) => CheckResult::fail(CheckId::Egress, format!("probe failed: {e}")),
    }
}

const ANONYMOUS_GRANT: &str = "GRANT_ANONYMOUS_OK";
const EGRESS_CONNECT: &str = "EGRESS_CONNECT=";

/// The per-client proxy asked with no credentials, from inside a sandbox: it
/// must challenge (407), and must never serve. Asked first, as Podman's probe
/// asks it: the shared script's last command decides its exit status.
fn anonymous_egress_probe(gateway_host: &str, port: u16) -> String {
    format!(
        "curl -s -o /dev/null -m 10 -w '{EGRESS_CONNECT}%{{http_connect}}\\n' \
         -x http://{gateway_host}:{port} https://example.com/ && echo {ANONYMOUS_GRANT}; echo; "
    )
}

/// What the anonymous probe found: a failure when the proxy served the
/// request, or did not challenge it.
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
                 no credentials through the sandbox's tunnel (its CONNECT answered {status}, \
                 not 407): is the node serving it?"
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::Manifests;
    use super::*;
    use crate::provider::kubernetes::pod::{KubeSpec, PodEnv};
    use crate::provider::kubernetes::{KubeSandboxOptions, KubeSandboxProvider};
    use crate::provider::{Bootstrap, CreateSpec, Resources, Shape};
    use k8s_openapi::api::networking::v1::NetworkPolicyEgressRule;

    /// The probe as the provider renders it, before admission. Rendering
    /// reads nothing from the API; the client needs a runtime only to exist.
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
        let client = kube::Client::new(axum::Router::new(), "ns");
        let provider =
            KubeSandboxProvider::new(client, KubeSandboxOptions::from_config(&cfg, spec));
        let labels = crate::runner::RunLabels::of(crate::runner::RunKind::Probe);
        provider
            .render(
                "probe",
                &CreateSpec {
                    name: Some("probe"),
                    image: "",
                    resources: &Resources::default(),
                    mounts: &[crate::runner::Mount::local("/work")],
                    bootstrap: &Bootstrap {
                        run: "probe".into(),
                        cert_pin: format!("sha256:{}", "ab".repeat(32)),
                        token: String::new(),
                    },
                    labels: &labels,
                    shape: Shape::Session,
                },
                "10.0.0.5:7426",
                &"cd".repeat(32),
                true,
            )
            .unwrap()
    }

    #[tokio::test]
    async fn the_rendered_probe_passes_its_own_static_checks() {
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

    /// The deep probe asks the per-client port with no credentials: a
    /// challenge passes, and anything served, or silence, fails.
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
        assert!(
            silent.detail.contains("is the node serving"),
            "{}",
            silent.detail
        );
        assert!(anonymous_egress_verdict("", 7424).is_some());
    }

    fn sandbox_policy(egress: Vec<NetworkPolicyEgressRule>) -> NetworkPolicy {
        NetworkPolicy {
            metadata: Default::default(),
            spec: Some(k8s_openapi::api::networking::v1::NetworkPolicySpec {
                pod_selector: Some(
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                        match_labels: Some(crate::provider::kubernetes::sandbox_pod_labels()),
                        ..Default::default()
                    },
                ),
                policy_types: Some(vec!["Ingress".into(), "Egress".into()]),
                ingress: Some(vec![]),
                egress: Some(egress),
            }),
        }
    }

    /// Egress to this node's own pod, by its role and its node label.
    fn egress_to_own_node(ports: &[i32]) -> NetworkPolicyEgressRule {
        NetworkPolicyEgressRule {
            to: Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyPeer {
                pod_selector: Some(
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                        match_labels: Some(crate::provider::kubernetes::node_pod_labels()),
                        ..Default::default()
                    },
                ),
                ..Default::default()
            }]),
            ports: Some(ports.iter().map(|p| port(*p)).collect()),
        }
    }

    /// A sandbox's one route: egress only to this node's pod, only on the
    /// sandbox port, nothing in. DNS, the node pod's old ports, a range, a
    /// wider peer, any node's pod, an ingress rule or a wider selector are
    /// each refused.
    #[test]
    fn the_sandbox_policy_allows_one_route_and_nothing_else() {
        let egress_to_node = egress_to_own_node;
        assert_eq!(
            sandbox_policy_shape(&sandbox_policy(vec![egress_to_node(&[7426])]), 7426),
            Ok(())
        );
        // Any node's pod, or every sandbox in the namespace, is another
        // node's too.
        let why = sandbox_policy_shape(
            &sandbox_policy(vec![super::tests::egress_to_node(&[7426])]),
            7426,
        )
        .unwrap_err();
        assert!(why.contains("tracon.dev/node=tracon"), "{why}");
        let mut every_sandbox = sandbox_policy(vec![egress_to_node(&[7426])]);
        every_sandbox.spec.as_mut().unwrap().pod_selector = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
                match_labels: Some([(ROLE_LABEL.to_string(), "sandbox".to_string())].into()),
                ..Default::default()
            },
        );
        let why = sandbox_policy_shape(&every_sandbox, 7426).unwrap_err();
        assert!(why.contains("no other node's"), "{why}");
        let why = sandbox_policy_shape(&sandbox_policy(vec![egress_to_node(&[7426, 7421])]), 7426)
            .unwrap_err();
        assert!(why.contains("port 7421"), "{why}");
        let mut dns = egress_to_node(&[53]);
        dns.ports.as_mut().unwrap()[0].protocol = Some("UDP".into());
        let why = sandbox_policy_shape(&sandbox_policy(vec![egress_to_node(&[7426]), dns]), 7426)
            .unwrap_err();
        assert!(why.contains("UDP port 53"), "{why}");
        assert!(sandbox_policy_shape(&sandbox_policy(vec![]), 7426).is_err());
        assert!(sandbox_policy_shape(&sandbox_policy(vec![egress_to_node(&[])]), 7426).is_err());
        let mut anywhere = egress_to_node(&[7426]);
        anywhere.to = Some(vec![k8s_openapi::api::networking::v1::NetworkPolicyPeer {
            ip_block: Some(k8s_openapi::api::networking::v1::IPBlock {
                cidr: "0.0.0.0/0".into(),
                except: None,
            }),
            ..Default::default()
        }]);
        assert!(sandbox_policy_shape(&sandbox_policy(vec![anywhere]), 7426).is_err());
        let mut range = egress_to_node(&[7426]);
        range.ports.as_mut().unwrap()[0].end_port = Some(7430);
        assert!(sandbox_policy_shape(&sandbox_policy(vec![range]), 7426).is_err());

        let mut admits = sandbox_policy(vec![egress_to_node(&[7426])]);
        admits.spec.as_mut().unwrap().ingress = Some(vec![ingress(Some(vec![pods("node")]))]);
        assert!(sandbox_policy_shape(&admits, 7426)
            .unwrap_err()
            .contains("admits ingress"));
        let mut egress_only = sandbox_policy(vec![egress_to_node(&[7426])]);
        egress_only.spec.as_mut().unwrap().policy_types = Some(vec!["Egress".into()]);
        assert!(sandbox_policy_shape(&egress_only, 7426).is_err());
        let mut narrower = sandbox_policy(vec![egress_to_node(&[7426])]);
        narrower
            .spec
            .as_mut()
            .unwrap()
            .pod_selector
            .as_mut()
            .unwrap()
            .match_labels
            .as_mut()
            .unwrap()
            .insert("team".into(), "a".into());
        assert!(sandbox_policy_shape(&narrower, 7426)
            .unwrap_err()
            .contains("does not select exactly"));
    }

    /// The node admits sandboxes on the sandbox port only through a rule
    /// that names them.
    #[test]
    fn the_node_policy_admits_sandboxes_on_their_port_or_not() {
        let from = |role: &str, ports: &[i32]| NetworkPolicyIngressRule {
            from: Some(vec![pods(role)]),
            ports: Some(ports.iter().map(|p| port(*p)).collect()),
        };
        assert!(node_admits_role(
            &[from("harness", &[7421]), from("sandbox", &[7426])],
            "sandbox",
            7426
        ));
        assert!(!node_admits_role(
            &[from("harness", &[7426])],
            "sandbox",
            7426
        ));
        assert!(!node_admits_role(
            &[from("sandbox", &[7421])],
            "sandbox",
            7426
        ));
    }

    /// The shipped manifests: the sandbox policy gives a sandbox one route,
    /// the node's sandbox port, which the node's policy opens to sandboxes
    /// alone; the Role is exactly the rights the checks assert, with nothing
    /// on Secrets and no attach; nothing is left of the harness pods'.
    #[test]
    fn the_shipped_manifests_give_a_sandbox_one_route() {
        let port = crate::config::KUBE_SANDBOX_PORT;
        let file =
            |name: &str| String::from_utf8_lossy(&Manifests::get(name).unwrap().data).into_owned();
        let policy = file("networkpolicy-sandbox.yaml");
        for said in [
            "name: tracon-sandbox",
            "tracon.dev/role: sandbox",
            r#"policyTypes: ["Ingress", "Egress"]"#,
            "ingress: []",
            "tracon.dev/role: node",
            &format!("port: {port}"),
        ] {
            assert!(policy.contains(said), "{said}:\n{policy}");
        }
        // This node's sandboxes and this node's pod, by the default prefix,
        // which the deployment's pod carries.
        assert_eq!(
            policy.matches("tracon.dev/node: tracon").count(),
            2,
            "both the sandboxes it selects and the pod they reach:\n{policy}"
        );
        let deployment = file("deployment.yaml");
        assert!(
            deployment.contains("tracon.dev/node: tracon"),
            "{deployment}"
        );
        assert!(deployment.contains("TRACON_POD_NAME"), "{deployment}");
        let ports: Vec<&str> = policy.lines().filter(|l| l.contains("port:")).collect();
        assert_eq!(ports.len(), 1, "{policy}");
        assert!(file("networkpolicy-node.yaml").contains(&format!("port: {port}")));
        assert!(file("deployment.yaml").contains(&format!("containerPort: {port}")));
        let kustomization = file("kustomization.yaml");
        for listed in [
            "networkpolicy-sandbox.yaml",
            "networkpolicy-node.yaml",
            "networkpolicy-node-build.yaml",
        ] {
            assert!(kustomization.contains(listed), "{listed}:\n{kustomization}");
        }
        assert!(Manifests::get("networkpolicy-harness.yaml").is_none());
        assert!(!kustomization.contains("harness"), "{kustomization}");
        let role: String = file("role.yaml")
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!role.contains("secrets"), "{role}");
        assert!(!role.contains("attach"), "{role}");
        let mut granted: Vec<(String, String)> = Vec::new();
        let mut resource = String::new();
        for line in role.lines().map(str::trim) {
            let list = |line: &str| -> Vec<String> {
                line.split(['[', ']', ',', '"'])
                    .map(str::trim)
                    .filter(|v| !v.is_empty() && !v.ends_with(':'))
                    .map(str::to_string)
                    .collect()
            };
            if let Some(rest) = line.strip_prefix("resources:") {
                resource = list(rest).join(",");
            } else if let Some(rest) = line.strip_prefix("verbs:") {
                granted.extend(list(rest).into_iter().map(|v| (resource.clone(), v)));
            }
        }
        granted.sort();
        let mut asserted: Vec<(String, String)> = PROTOCOL_RIGHTS
            .iter()
            .map(|(_, resource, sub, verb)| {
                let resource = match sub.is_empty() {
                    true => resource.to_string(),
                    false => format!("{resource}/{sub}"),
                };
                (resource, verb.to_string())
            })
            .collect();
        asserted.sort();
        assert_eq!(granted, asserted);
        let node = file("networkpolicy-node.yaml");
        assert!(node.contains("tracon.dev/role: sandbox"), "{node}");
        assert!(node.contains("tracon.dev/node: tracon"), "{node}");
        assert!(!node.contains("tracon.dev/role: harness"), "{node}");
        let build = file("networkpolicy-node-build.yaml");
        let egress = crate::config::Config::default().gateway.egress_port;
        assert!(build.contains(&format!("port: {egress}")), "{build}");
        assert!(
            build.contains("app.kubernetes.io/name: buildkitd"),
            "{build}"
        );
        // The switch is gone: the seeded file loads as it is.
        let configmap = file("configmap.yaml");
        assert!(!configmap.contains("sandbox_protocol"), "{configmap}");
        let seeded: String = configmap
            .lines()
            .skip_while(|l| !l.contains("node.toml: |"))
            .skip(1)
            .map(|l| l.strip_prefix("    ").unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        crate::config::Config::from_toml(&seeded, std::path::Path::new("node.toml")).unwrap();
    }

    /// From inside a sandbox: a resolved name or the node pod's own port
    /// reached directly fails; the shared verdict holds as a dropping
    /// boundary's, with the per-client proxy challenging.
    #[test]
    fn from_inside_a_sandbox_only_the_tunnels_answer() {
        let script = sandbox_only_script("10.244.0.9", 7421);
        assert!(
            script.contains("http://10.244.0.9:7421/harness/ping"),
            "{script}"
        );
        assert!(sandbox_only_script("fd00::9", 7421).contains("http://[fd00::9]:7421/"));
        let good = "EGRESS_CONNECT=407\nDIRECT_NAME_SECONDS=20\nDIRECT_ADDR_SECONDS=20\nPROXY_ALLOWED\npong";
        let ok = sandbox_only_verdict(good, 7424);
        assert!(ok.ok, "{}", ok.detail);
        for marker in ["DIRECT_DNS_OK", "DIRECT_NODE_OK", "DIRECT_OK"] {
            assert!(!sandbox_only_verdict(&format!("{marker}\n{good}"), 7424).ok);
        }
        assert!(!sandbox_only_verdict(&good.replace("407", "000"), 7424).ok);
    }

    #[tokio::test]
    async fn admission_that_adds_privilege_or_a_host_path_fails() {
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
