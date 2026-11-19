//! How much room the cluster has for one more session, for placement.
//!
//! Without a pool, every sandbox pod is kept beside the node's own pod by
//! required pod affinity on `kubernetes.io/hostname`, so the room that counts
//! is the node pod's machine's: its allocatable CPU and memory, less the
//! requests of every pod bound to it that has not ended, in any namespace.
//! With `[runtime.kubernetes.pool]`, it is the roomiest of the pool's nodes
//! (those its node selector, with the base's architecture, selects), and a
//! pool with no node at all has room to wait for: the cluster's autoscaler
//! adds one when a pod asks (`Fit::awaits_node`). A session fits when its pod
//! and one check's pod (`[runtime.kubernetes.resources]` `session` and
//! `command`) fit together on a schedulable node, so a session placed here
//! can still run its checks. A node that is cordoned or carries a
//! `NoSchedule` or `NoExecute` taint the pods do not tolerate takes nothing,
//! except that a pool's node still starting up counts for the room it will
//! have (`STARTING`).
//!
//! Read with cluster-wide `get` and `list` on `nodes` and `pods`
//! (`deploy/kubernetes/base/clusterrole.yaml`). Without them it is not read,
//! and placement counts sessions alone, as before.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::ListParams;
use kube::Api;
use serde::Serialize;

use crate::config::{kube_quantity, KubePool, KubeResources, KubeToleration};

/// What a session and its checks need, against what the roomiest
/// schedulable node has free. CPU in millicores, memory in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Fit {
    pub free_cpu: u64,
    pub free_memory: u64,
    pub needs_cpu: u64,
    pub needs_memory: u64,
    pub fits: bool,
    /// The pool has no node now: one is added when a pod asks, so it fits
    /// without anything free yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub awaits_node: bool,
}

/// The taints a node carries while it is still joining the cluster, which
/// lift once it is ready: a pool's new node counts for the room it will
/// have, so a second start during a scale-up is not taken as full.
pub const STARTING: [&str; 3] = [
    "node.kubernetes.io/not-ready",
    "node.cloudprovider.kubernetes.io/uninitialized",
    "node.cilium.io/agent-not-ready",
];

/// What one more session asks of a node, as (millicores, bytes): its own
/// pod's requests and one check's.
pub fn needs(resources: &KubeResources) -> (u64, u64) {
    let (session, command) = (resources.session(), resources.command());
    (
        millis(&session.cpu_request) + millis(&command.cpu_request),
        bytes(&session.memory_request) + bytes(&command.memory_request),
    )
}

fn millis(quantity: &str) -> u64 {
    kube_quantity(quantity).map_or(0, |cores| (cores * 1000.0).ceil() as u64)
}

fn bytes(quantity: &str) -> u64 {
    kube_quantity(quantity).map_or(0, |bytes| bytes.ceil() as u64)
}

/// `cpu` and `memory` out of a resource list, as (millicores, bytes).
fn amounts(list: Option<&BTreeMap<String, Quantity>>) -> (u64, u64) {
    let get = |name: &str| list.and_then(|l| l.get(name)).map(|q| q.0.as_str());
    (get("cpu").map_or(0, millis), get("memory").map_or(0, bytes))
}

/// A pod's requests as the scheduler counts them: its containers' together,
/// or its largest init container's where that is more (they run one at a
/// time, before the rest), plus the pod's overhead.
pub fn pod_requests(pod: &Pod) -> (u64, u64) {
    let Some(spec) = &pod.spec else {
        return (0, 0);
    };
    let requests = |c: &k8s_openapi::api::core::v1::Container| {
        amounts(c.resources.as_ref().and_then(|r| r.requests.as_ref()))
    };
    let (mut cpu, mut memory) = spec
        .containers
        .iter()
        .map(requests)
        .fold((0, 0), |(c, m), (dc, dm)| (c + dc, m + dm));
    for (init_cpu, init_memory) in spec.init_containers.iter().flatten().map(requests) {
        cpu = cpu.max(init_cpu);
        memory = memory.max(init_memory);
    }
    let (overhead_cpu, overhead_memory) = amounts(spec.overhead.as_ref());
    (cpu + overhead_cpu, memory + overhead_memory)
}

/// Whether the scheduler would put a sandbox pod tolerating `tolerations`
/// on `node`: not cordoned, and no taint it is kept off by.
pub fn schedulable(node: &Node, tolerations: &[KubeToleration]) -> bool {
    let Some(spec) = &node.spec else {
        return true;
    };
    !spec.unschedulable.unwrap_or(false)
        && !spec.taints.iter().flatten().any(|t| {
            (t.effect == "NoSchedule" || t.effect == "NoExecute")
                && !tolerations.iter().any(|tolerated| {
                    tolerated.tolerates(&t.key, t.value.as_deref().unwrap_or_default(), &t.effect)
                })
        })
}

/// Whether a pod still holds its requests on its node: one that ended
/// (`Succeeded`, `Failed`) holds nothing.
fn holds(pod: &Pod) -> bool {
    !matches!(
        pod.status.as_ref().and_then(|s| s.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

/// The room on the roomiest schedulable node among `nodes` for `needs`
/// (millicores, bytes), counting the requests of the `pods` bound to each.
/// A node where it fits is preferred; among the rest, the one with the most
/// memory free. No schedulable node at all has nothing free.
pub fn fit(nodes: &[Node], pods: &[Pod], needs: (u64, u64)) -> Fit {
    fit_tolerating(nodes, pods, needs, &[])
}

/// [`fit`], for pods that tolerate `tolerations`.
pub fn fit_tolerating(
    nodes: &[Node],
    pods: &[Pod],
    needs: (u64, u64),
    tolerations: &[KubeToleration],
) -> Fit {
    let (needs_cpu, needs_memory) = needs;
    let best = nodes
        .iter()
        .filter(|node| schedulable(node, tolerations))
        .map(|node| {
            let name = node.metadata.name.as_deref().unwrap_or_default();
            let (cpu, memory) = amounts(node.status.as_ref().and_then(|s| s.allocatable.as_ref()));
            let (used_cpu, used_memory) = pods
                .iter()
                .filter(|pod| holds(pod))
                .filter(|pod| pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(name))
                .map(pod_requests)
                .fold((0, 0), |(c, m), (dc, dm)| (c + dc, m + dm));
            let (free_cpu, free_memory) = (
                cpu.saturating_sub(used_cpu),
                memory.saturating_sub(used_memory),
            );
            (
                free_cpu >= needs_cpu && free_memory >= needs_memory,
                free_memory,
                free_cpu,
            )
        })
        .max();
    let (fits, free_memory, free_cpu) = best.unwrap_or((false, 0, 0));
    Fit {
        free_cpu,
        free_memory,
        needs_cpu,
        needs_memory,
        fits,
        awaits_node: false,
    }
}

/// The room in a pool whose nodes, as its selector lists them, are `nodes`:
/// the roomiest that the pool's pods tolerate (and one still starting, for
/// the room it will have), or, with no node at all, room to wait for.
pub fn pool_fit(nodes: &[Node], pods: &[Pod], needs: (u64, u64), pool: &KubePool) -> Fit {
    if nodes.is_empty() {
        return Fit {
            needs_cpu: needs.0,
            needs_memory: needs.1,
            fits: true,
            awaits_node: true,
            ..Fit::default()
        };
    }
    let mut tolerations = pool.tolerations.clone();
    tolerations.extend(STARTING.iter().map(|key| KubeToleration {
        key: key.to_string(),
        operator: "Exists".into(),
        ..Default::default()
    }));
    fit_tolerating(nodes, pods, needs, &tolerations)
}

/// Read the room on `node` (the node pod's own machine, `TRACON_NODE_NAME`)
/// from the API. `Err` says why it could not be.
pub async fn read(client: &kube::Client, node: &str, needs: (u64, u64)) -> Result<Fit, String> {
    let said = |what: &str, error: kube::Error| match &error {
        kube::Error::Api(status) if status.code == 403 => format!(
            "the node may not {what} (deploy/kubernetes/base/clusterrole.yaml grants it): {}",
            status.message
        ),
        _ => format!("{what}: {error}"),
    };
    let nodes: Api<Node> = Api::all(client.clone());
    let found = nodes
        .get(node)
        .await
        .map_err(|e| said(&format!("read node {node}"), e))?;
    let pods: Api<Pod> = Api::all(client.clone());
    let bound = pods
        .list(&ListParams::default().fields(&format!(
            "spec.nodeName={node},status.phase!=Succeeded,status.phase!=Failed"
        )))
        .await
        .map_err(|e| said(&format!("list the pods on node {node}"), e))?;
    Ok(fit(&[found], &bound.items, needs))
}

/// A node selector as a label selector query: `key=value,...`.
pub fn selector_query(selector: &BTreeMap<String, String>) -> String {
    selector
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The nodes a pool's own labels select, whatever their architecture.
pub async fn pool_nodes(client: &kube::Client, pool: &KubePool) -> Result<Vec<Node>, String> {
    let nodes: Api<Node> = Api::all(client.clone());
    nodes
        .list(&ListParams::default().labels(&selector_query(&pool.node_selector)))
        .await
        .map(|listed| listed.items)
        .map_err(|e| denied("list the sandbox pool's nodes", e))
}

fn denied(what: &str, error: kube::Error) -> String {
    match &error {
        kube::Error::Api(status) if status.code == 403 => format!(
            "the node may not {what} (deploy/kubernetes/base/clusterrole.yaml grants it): {}",
            status.message
        ),
        _ => format!("{what}: {error}"),
    }
}

/// Read the room in `pool` from the API: its nodes of `arch` (the pods'
/// own selector, `pod::pool_selector`), and the pods bound anywhere that
/// have not ended. `Err` says why it could not be.
pub async fn read_pool(
    client: &kube::Client,
    pool: &KubePool,
    arch: &str,
    needs: (u64, u64),
) -> Result<Fit, String> {
    let nodes: Api<Node> = Api::all(client.clone());
    let selector = super::pod::pool_selector(pool, arch);
    let found = nodes
        .list(&ListParams::default().labels(&selector_query(&selector)))
        .await
        .map_err(|e| denied("list the sandbox pool's nodes", e))?;
    if found.items.is_empty() {
        return Ok(pool_fit(&[], &[], needs, pool));
    }
    let pods: Api<Pod> = Api::all(client.clone());
    let bound = pods
        .list(&ListParams::default().fields("status.phase!=Succeeded,status.phase!=Failed"))
        .await
        .map_err(|e| denied("list the pods on the sandbox pool's nodes", e))?;
    Ok(pool_fit(&found.items, &bound.items, needs, pool))
}

/// Memory as an operator reads it: `630 Mi`, `1.5 Gi`.
pub fn memory_words(bytes: u64) -> String {
    const MI: f64 = 1024.0 * 1024.0;
    const GI: f64 = MI * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GI {
        let gi = format!("{:.1}", bytes / GI);
        format!("{} Gi", gi.strip_suffix(".0").unwrap_or(&gi))
    } else {
        format!("{} Mi", (bytes / MI).floor())
    }
}

/// CPU in cores: `2`, `0.75`.
pub fn cpu_words(millis: u64) -> String {
    let cores = format!("{:.2}", millis as f64 / 1000.0);
    cores
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

impl std::fmt::Display for Fit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.awaits_node {
            write!(
                f,
                "the sandbox pool has no node now, and the cluster's autoscaler adds one when a \
                 pod asks; a session and its checks need {} and {} CPU",
                memory_words(self.needs_memory),
                cpu_words(self.needs_cpu)
            )
        } else if self.fits {
            write!(
                f,
                "the cluster has {} and {} CPU free; a session and its checks need {} and {} CPU",
                memory_words(self.free_memory),
                cpu_words(self.free_cpu),
                memory_words(self.needs_memory),
                cpu_words(self.needs_cpu)
            )
        } else if self.free_memory < self.needs_memory {
            write!(
                f,
                "the cluster has {} free, a session and its checks need {}",
                memory_words(self.free_memory),
                memory_words(self.needs_memory)
            )
        } else {
            write!(
                f,
                "the cluster has {} CPU free, a session and its checks need {} CPU",
                cpu_words(self.free_cpu),
                cpu_words(self.needs_cpu)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Container, NodeSpec, NodeStatus, PodSpec, PodStatus, ResourceRequirements, Taint,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn list(cpu: &str, memory: &str) -> BTreeMap<String, Quantity> {
        BTreeMap::from([
            ("cpu".to_string(), Quantity(cpu.into())),
            ("memory".to_string(), Quantity(memory.into())),
        ])
    }

    fn node(name: &str, cpu: &str, memory: &str) -> Node {
        Node {
            metadata: ObjectMeta {
                name: Some(name.into()),
                ..Default::default()
            },
            spec: Some(NodeSpec::default()),
            status: Some(NodeStatus {
                allocatable: Some(list(cpu, memory)),
                ..Default::default()
            }),
        }
    }

    fn container(cpu: &str, memory: &str) -> Container {
        Container {
            name: "c".into(),
            resources: Some(ResourceRequirements {
                requests: Some(list(cpu, memory)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pod(on: &str, phase: &str, containers: Vec<Container>) -> Pod {
        Pod {
            metadata: ObjectMeta::default(),
            spec: Some(PodSpec {
                node_name: Some(on.into()),
                containers,
                ..Default::default()
            }),
            status: Some(PodStatus {
                phase: Some(phase.into()),
                ..Default::default()
            }),
        }
    }

    const GI: u64 = 1024 * 1024 * 1024;
    const MI: u64 = 1024 * 1024;

    #[test]
    fn quantities_are_read_in_every_unit() {
        assert_eq!(millis("250m"), 250);
        assert_eq!(millis("2"), 2000);
        assert_eq!(millis("1.5"), 1500);
        assert_eq!(bytes("512Mi"), 512 * MI);
        assert_eq!(bytes("2Gi"), 2 * GI);
        assert_eq!(bytes("1G"), 1_000_000_000);
        assert_eq!(bytes("6458100Ki"), 6_458_100 * 1024);
        assert_eq!(bytes("not a quantity"), 0);
        // The defaults: a session (250m, 1Gi) and a check (500m, 2Gi).
        assert_eq!(needs(&KubeResources::default()), (750, 3 * GI));
        // Read from the configuration, not assumed.
        let mut resources = KubeResources::default();
        resources.command.memory_request = Some("1Gi".into());
        assert_eq!(needs(&resources), (750, 2 * GI));
    }

    #[test]
    fn a_pod_counts_its_containers_or_its_largest_init_container() {
        let mut p = pod(
            "n1",
            "Running",
            vec![container("250m", "512Mi"), container("100m", "128Mi")],
        );
        assert_eq!(pod_requests(&p), (350, 640 * MI));
        p.spec.as_mut().unwrap().init_containers = Some(vec![container("1", "64Mi")]);
        assert_eq!(pod_requests(&p), (1000, 640 * MI));
        p.spec.as_mut().unwrap().overhead = Some(list("10m", "16Mi"));
        assert_eq!(pod_requests(&p), (1010, 656 * MI));
    }

    /// The homelab's node: 4 vCPU, 6.3 Gi allocatable, four sessions'
    /// worth of requests on it. A check no longer fits, so neither does a
    /// session.
    #[test]
    fn allocatable_less_what_is_bound_is_what_is_free() {
        let nodes = [node("general-3ega6i", "3920m", "6458100Ki")];
        let pods = vec![
            pod(
                "general-3ega6i",
                "Running",
                vec![container("3157m", "5936Mi")],
            ),
            // Ended pods hold nothing; another node's are not this one's.
            pod("general-3ega6i", "Succeeded", vec![container("1", "1Gi")]),
            pod("general-3ega6i", "Failed", vec![container("1", "1Gi")]),
            pod("elsewhere", "Running", vec![container("1", "1Gi")]),
        ];
        let fit = fit(&nodes, &pods, needs(&KubeResources::default()));
        assert_eq!(fit.free_cpu, 763);
        assert_eq!(fit.free_memory, 6_458_100 * 1024 - 5936 * MI);
        assert!(!fit.fits);
        assert_eq!(
            fit.to_string(),
            "the cluster has 370 Mi free, a session and its checks need 3 Gi"
        );
        // With the room back, it fits.
        let fit = super::fit(&nodes, &pods[1..], (750, 3 * GI));
        assert!(fit.fits, "{fit:?}");
    }

    #[test]
    fn the_roomiest_schedulable_node_counts() {
        let mut cordoned = node("cordoned", "16", "64Gi");
        cordoned.spec.as_mut().unwrap().unschedulable = Some(true);
        let mut tainted = node("tainted", "16", "64Gi");
        tainted.spec.as_mut().unwrap().taints = Some(vec![Taint {
            key: "node.kubernetes.io/not-ready".into(),
            effect: "NoSchedule".into(),
            ..Default::default()
        }]);
        let mut preferring = node("preferring", "1", "2Gi");
        preferring.spec.as_mut().unwrap().taints = Some(vec![Taint {
            key: "spot".into(),
            effect: "PreferNoSchedule".into(),
            ..Default::default()
        }]);
        let small = node("small", "4", "4Gi");
        let pods = vec![pod("small", "Running", vec![container("1", "2Gi")])];

        // Only the cordoned and tainted nodes have room: nothing fits.
        let nodes = [cordoned.clone(), tainted.clone(), small.clone()];
        let fit = fit(&nodes, &pods, (750, 3 * GI));
        assert!(!fit.fits);
        assert_eq!((fit.free_cpu, fit.free_memory), (3000, 2 * GI));

        // A node it fits on is taken over a roomier one it does not.
        let nodes = [small.clone(), preferring.clone(), node("big", "8", "8Gi")];
        let fit = super::fit(&nodes, &pods, (750, 3 * GI));
        assert!(fit.fits);
        assert_eq!(fit.free_memory, 8 * GI);
        let fit = super::fit(&nodes[..2], &pods, (750, 3 * GI));
        assert!(!fit.fits);
        assert_eq!(fit.free_memory, 2 * GI);

        // Short of CPU rather than memory, it says so.
        let fit = super::fit(&[node("busy", "500m", "8Gi")], &[], (750, 3 * GI));
        assert!(!fit.fits);
        assert_eq!(
            fit.to_string(),
            "the cluster has 0.5 CPU free, a session and its checks need 0.75 CPU"
        );

        // No schedulable node at all: nothing free.
        let fit = super::fit(&[cordoned, tainted], &[], (750, 3 * GI));
        assert_eq!(
            fit,
            Fit {
                free_cpu: 0,
                free_memory: 0,
                needs_cpu: 750,
                needs_memory: 3 * GI,
                fits: false,
                awaits_node: false,
            }
        );
    }

    fn pool() -> KubePool {
        KubePool {
            node_selector: BTreeMap::from([("workload".into(), "tracon-sandbox".into())]),
            tolerations: vec![KubeToleration {
                key: "workload".into(),
                value: "tracon-sandbox".into(),
                effect: "NoSchedule".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn tainted(mut node: Node, key: &str, value: &str) -> Node {
        node.spec.as_mut().unwrap().taints = Some(vec![Taint {
            key: key.into(),
            value: Some(value.into()),
            effect: "NoSchedule".into(),
            ..Default::default()
        }]);
        node
    }

    /// A pool with no node has room to wait for; one with nodes has the room
    /// of the roomiest its pods tolerate, a node still joining counted for
    /// what it will have.
    #[test]
    fn a_pool_has_the_room_of_its_nodes_or_waits_for_one() {
        let pool = pool();
        let empty = pool_fit(&[], &[], (750, 3 * GI), &pool);
        assert!(empty.fits && empty.awaits_node, "{empty:?}");
        assert_eq!((empty.free_cpu, empty.free_memory), (0, 0));
        assert_eq!(
            empty.to_string(),
            "the sandbox pool has no node now, and the cluster's autoscaler adds one when a pod \
             asks; a session and its checks need 3 Gi and 0.75 CPU"
        );
        assert_eq!(
            serde_json::to_value(empty).unwrap()["awaits_node"],
            serde_json::json!(true)
        );
        // Its own taint is tolerated; another pool's is not.
        let own = tainted(node("pool-a", "4", "6Gi"), "workload", "tracon-sandbox");
        let other = tainted(node("pool-b", "16", "64Gi"), "workload", "minecraft");
        let pods = vec![pod("pool-a", "Running", vec![container("1", "2Gi")])];
        let fit = pool_fit(&[own.clone(), other], &pods, (750, 3 * GI), &pool);
        assert!(fit.fits && !fit.awaits_node, "{fit:?}");
        assert_eq!((fit.free_cpu, fit.free_memory), (3000, 4 * GI));
        assert!(serde_json::to_value(fit)
            .unwrap()
            .get("awaits_node")
            .is_none());
        // A full node in a pool that has one is full, not waited for.
        let busy = vec![pod("pool-a", "Running", vec![container("3", "5Gi")])];
        let fit = pool_fit(std::slice::from_ref(&own), &busy, (750, 3 * GI), &pool);
        assert!(!fit.fits && !fit.awaits_node, "{fit:?}");
        // Without the pool's tolerations, its node would take nothing.
        assert!(!schedulable(&own, &[]));
        // A node still joining counts for the room it will have.
        let joining = tainted(
            node("pool-c", "4", "6Gi"),
            "node.kubernetes.io/not-ready",
            "",
        );
        assert!(pool_fit(std::slice::from_ref(&joining), &[], (750, 3 * GI), &pool).fits);
        assert!(!fit_tolerating(&[joining], &[], (750, 3 * GI), &pool.tolerations).fits);
    }

    /// Read from the API: the pool's nodes of the base's architecture, then
    /// every pod that has not ended; with none, nothing more is read.
    #[tokio::test]
    async fn a_pools_room_is_read_from_the_api() {
        let labels = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = labels.clone();
        let nodes = std::sync::Arc::new(std::sync::Mutex::new(serde_json::json!([])));
        let listed = nodes.clone();
        let routes = axum::Router::new()
            .route(
                "/api/v1/nodes",
                axum::routing::get(
                    move |query: axum::extract::Query<BTreeMap<String, String>>| {
                        *seen.lock().unwrap() =
                            query.get("labelSelector").cloned().unwrap_or_default();
                        let items = listed.lock().unwrap().clone();
                        async move {
                            axum::Json(serde_json::json!({
                                "apiVersion": "v1",
                                "kind": "NodeList",
                                "metadata": {},
                                "items": items,
                            }))
                        }
                    },
                ),
            )
            .route(
                "/api/v1/pods",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "apiVersion": "v1",
                        "kind": "PodList",
                        "metadata": {},
                        "items": [{
                            "metadata": {"name": "tracon-h-1"},
                            "spec": {
                                "nodeName": "pool-a",
                                "containers": [{
                                    "name": "harness",
                                    "resources": {"requests": {"cpu": "250m", "memory": "1Gi"}},
                                }],
                            },
                            "status": {"phase": "Running"},
                        }],
                    }))
                }),
            );
        let client = api(routes);
        let fit = read_pool(&client, &pool(), "amd64", (750, 3 * GI))
            .await
            .unwrap();
        assert!(fit.awaits_node, "{fit:?}");
        assert_eq!(
            *labels.lock().unwrap(),
            "kubernetes.io/arch=amd64,workload=tracon-sandbox"
        );
        *nodes.lock().unwrap() = serde_json::json!([{
            "metadata": {"name": "pool-a"},
            "spec": {"taints": [{"key": "workload", "value": "tracon-sandbox", "effect": "NoSchedule"}]},
            "status": {"allocatable": {"cpu": "3920m", "memory": "6Gi"}},
        }]);
        let fit = read_pool(&client, &pool(), "amd64", (750, 3 * GI))
            .await
            .unwrap();
        assert!(fit.fits && !fit.awaits_node, "{fit:?}");
        assert_eq!((fit.free_cpu, fit.free_memory), (3670, 5 * GI));
    }

    #[test]
    fn amounts_read_as_an_operator_would() {
        assert_eq!(memory_words(630 * MI), "630 Mi");
        assert_eq!(memory_words(3 * GI / 2), "1.5 Gi");
        assert_eq!(memory_words(2 * GI), "2 Gi");
        assert_eq!(cpu_words(2000), "2");
        assert_eq!(cpu_words(750), "0.75");
        assert_eq!(cpu_words(0), "0");
    }

    fn api(routes: axum::Router) -> kube::Client {
        kube::Client::new(routes, "tracon-node")
    }

    /// Read from the API: the node pod's own machine, and the pods bound to
    /// it that have not ended.
    #[tokio::test]
    async fn the_room_is_read_from_the_api() {
        let fields = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = fields.clone();
        let routes = axum::Router::new()
            .route(
                "/api/v1/nodes/general-1",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "apiVersion": "v1",
                        "kind": "Node",
                        "metadata": {"name": "general-1"},
                        "spec": {},
                        "status": {"allocatable": {"cpu": "3920m", "memory": "8Gi"}},
                    }))
                }),
            )
            .route(
                "/api/v1/pods",
                axum::routing::get(
                    move |query: axum::extract::Query<BTreeMap<String, String>>| {
                        *seen.lock().unwrap() =
                            query.get("fieldSelector").cloned().unwrap_or_default();
                        async {
                            axum::Json(serde_json::json!({
                                "apiVersion": "v1",
                                "kind": "PodList",
                                "metadata": {},
                                "items": [{
                                    "metadata": {"name": "tracon-node-0"},
                                    "spec": {
                                        "nodeName": "general-1",
                                        "containers": [{
                                            "name": "node",
                                            "resources": {"requests": {"cpu": "1", "memory": "6Gi"}},
                                        }],
                                    },
                                    "status": {"phase": "Running"},
                                }],
                            }))
                        }
                    },
                ),
            );
        let fit = read(&api(routes), "general-1", (750, 3 * GI))
            .await
            .unwrap();
        assert_eq!((fit.free_cpu, fit.free_memory), (2920, 2 * GI));
        assert!(!fit.fits);
        assert_eq!(
            *fields.lock().unwrap(),
            "spec.nodeName=general-1,status.phase!=Succeeded,status.phase!=Failed"
        );
    }

    /// Without the rights to read nodes, it is not read, and says why.
    #[tokio::test]
    async fn a_node_without_the_rights_says_so() {
        let routes = axum::Router::new().route(
            "/api/v1/nodes/general-1",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({
                        "apiVersion": "v1",
                        "kind": "Status",
                        "status": "Failure",
                        "message": "nodes \"general-1\" is forbidden: User \"system:serviceaccount:tracon:tracon-node\" cannot get resource \"nodes\"",
                        "reason": "Forbidden",
                        "code": 403,
                    })),
                )
            }),
        );
        let why = read(&api(routes), "general-1", (750, 3 * GI))
            .await
            .unwrap_err();
        assert!(
            why.contains("the node may not read node general-1"),
            "{why}"
        );
        assert!(why.contains("clusterrole.yaml"), "{why}");
    }
}
