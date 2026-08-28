//! The Kubernetes answers to the five checks. What can be inspected here is
//! the admitted pod spec (a probe pod that is gated, so admission mutates it
//! and nothing ever schedules it), the NetworkPolicy the deployment carries,
//! and the node's own rights; `--deep` runs the shared egress script as a
//! real harness pod.

use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, PostParams};
use kube::Api;

use super::KubeBackend;
use crate::boundary::checks::{egress_script, egress_verdict, CheckId, CheckResult};
use crate::boundary::BoundaryReport;
use crate::config::{Config, HarnessListen};
use crate::runner::kube::{ROLE_LABEL, STATE_VOLUME};
use crate::runner::{Runner, RunnerCommand};

const PROBE: &str = "tracon-boundary-probe";
pub const HARNESS_POLICY: &str = "tracon-harness";

/// The verbs the node needs, and exactly those: create/attach/delete its
/// harness pods, read their state and logs, and read the policy it verifies.
const RIGHTS: &[(&str, &str, &str, &str)] = &[
    ("", "pods", "", "create"),
    ("", "pods", "", "get"),
    ("", "pods", "", "list"),
    ("", "pods", "", "delete"),
    ("", "pods", "attach", "create"),
    ("", "pods", "log", "get"),
    ("networking.k8s.io", "networkpolicies", "", "get"),
];

pub async fn check_all(b: &KubeBackend, cfg: &Config, deep: bool) -> BoundaryReport {
    let mut checks = Vec::new();
    let runtime = check_runtime(b, cfg).await;
    let runtime_ok = runtime.ok;
    checks.push(runtime);
    if !runtime_ok {
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
            crate::session::materialize::state_mounts(&b.harness_home_str()).unwrap_or_default(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
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
        checks.push(check_egress(cfg, &runner).await);
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
    let reviews: Api<SelfSubjectAccessReview> = Api::all(client);
    for (group, resource, sub, verb) in RIGHTS {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    group: Some(group.to_string()),
                    resource: Some(resource.to_string()),
                    subresource: (!sub.is_empty()).then(|| sub.to_string()),
                    verb: Some(verb.to_string()),
                    namespace: Some(spec.env.namespace.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        match reviews.create(&PostParams::default(), &review).await {
            Ok(r) if r.status.as_ref().map(|s| s.allowed).unwrap_or(false) => {}
            Ok(_) => {
                let what = if sub.is_empty() {
                    resource.to_string()
                } else {
                    format!("{resource}/{sub}")
                };
                return CheckResult::fail(
                    CheckId::Runtime,
                    format!(
                        "service account may not {verb} {what} in {}",
                        spec.env.namespace
                    ),
                );
            }
            Err(e) => return CheckResult::fail(CheckId::Runtime, format!("access review: {e}")),
        }
    }
    CheckResult::ok(
        CheckId::Runtime,
        format!(
            "api reachable; pods/attach/log and networkpolicies granted in {}; node {}",
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

async fn check_network(
    b: &KubeBackend,
    runner: &crate::runner::kube::KubeRunner,
    pod: &Pod,
) -> CheckResult {
    let spec = runner.spec();
    let Some(ps) = &pod.spec else {
        return CheckResult::fail(CheckId::NetworkIsolated, "pod has no spec");
    };
    if ps.dns_policy.as_deref() != Some("None") {
        return CheckResult::fail(CheckId::NetworkIsolated, "harness pod has a resolver");
    }
    let alias_ok = ps.host_aliases.iter().flatten().any(|a| {
        a.ip == spec.env.pod_ip
            && a.hostnames
                .iter()
                .flatten()
                .any(|h| h == &spec.gateway_host)
    });
    if !alias_ok {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!("{} does not resolve to this node's pod", spec.gateway_host),
        );
    }
    let client = match b.client().await {
        Ok(c) => c,
        Err(e) => return CheckResult::fail(CheckId::NetworkIsolated, e),
    };
    let policies: Api<NetworkPolicy> = Api::namespaced(client, &spec.env.namespace);
    let policy = match policies.get(HARNESS_POLICY).await {
        Ok(p) => p,
        Err(e) => {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                format!(
                    "NetworkPolicy {HARNESS_POLICY} missing in {}: {e}",
                    spec.env.namespace
                ),
            )
        }
    };
    let Some(pspec) = policy.spec else {
        return CheckResult::fail(CheckId::NetworkIsolated, "policy has no spec");
    };
    let selects_harness = pspec
        .pod_selector
        .as_ref()
        .and_then(|s| s.match_labels.as_ref())
        .and_then(|l| l.get(ROLE_LABEL))
        .map(|v| v == "harness")
        .unwrap_or(false);
    if !selects_harness {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            format!("{HARNESS_POLICY} does not select {ROLE_LABEL}=harness"),
        );
    }
    let types = pspec.policy_types.clone().unwrap_or_default();
    if !(types.iter().any(|t| t == "Ingress") && types.iter().any(|t| t == "Egress")) {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            "policy must govern both Ingress and Egress",
        );
    }
    if pspec
        .ingress
        .as_ref()
        .map(|i| !i.is_empty())
        .unwrap_or(false)
    {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            "policy admits ingress to the harness",
        );
    }
    let egress = pspec.egress.unwrap_or_default();
    if egress.is_empty() {
        return CheckResult::fail(
            CheckId::NetworkIsolated,
            "policy allows no egress at all; the harness could not reach the node",
        );
    }
    for rule in &egress {
        let peers = rule.to.clone().unwrap_or_default();
        if peers.is_empty() {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                "an egress rule has no peer restriction",
            );
        }
        for peer in peers {
            if peer.ip_block.is_some() || peer.namespace_selector.is_some() {
                return CheckResult::fail(
                    CheckId::NetworkIsolated,
                    "egress reaches beyond the node's pod (ipBlock or namespaceSelector)",
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
                return CheckResult::fail(
                    CheckId::NetworkIsolated,
                    "an egress peer is not the node pod",
                );
            }
        }
        let ports: Vec<i32> = rule
            .ports
            .iter()
            .flatten()
            .filter_map(|p| p.port.as_ref())
            .filter_map(|p| match p {
                k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i) => Some(*i),
                _ => None,
            })
            .collect();
        if ports.is_empty() {
            return CheckResult::fail(CheckId::NetworkIsolated, "an egress rule allows every port");
        }
        let forward = i32::from(match &b.cfg.gateway.harness_listen {
            HarnessListen::Tcp(a) => a.port(),
            HarnessListen::Unix(_) => 0,
        });
        let proxy = i32::from(spec.proxy_port);
        if let Some(p) = ports.iter().find(|p| **p != forward && **p != proxy) {
            return CheckResult::fail(
                CheckId::NetworkIsolated,
                format!("egress allows port {p}, which is neither the forward nor the proxy"),
            );
        }
    }
    CheckResult::ok(
        CheckId::NetworkIsolated,
        format!(
            "no resolver; {HARNESS_POLICY} allows egress only to the node pod on the forward and proxy ports"
        ),
    )
}

async fn check_egress(cfg: &Config, runner: &crate::runner::kube::KubeRunner) -> CheckResult {
    let forward = match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(a) => a.port(),
        HarnessListen::Unix(_) => cfg.gateway.forward_port,
    };
    let script = egress_script(&runner.spec().gateway_host, forward);
    let cmd = RunnerCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        name: "tracon-egress-probe".into(),
        ..Default::default()
    };
    match runner.run_capture(cmd).await {
        Ok(out) => egress_verdict(&String::from_utf8_lossy(&out.stdout)),
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
