use super::*;
use crate::config::Config;
use crate::provider::kubernetes::pod::{
    PodEnv, CHANNEL_LABEL, KIND_LABEL, REPO_LABEL, SIZING_LABEL,
};
use crate::provider::{Bootstrap, Resources};
use crate::runner::{Mount, RunKind, RunLabels};

fn options() -> KubeSandboxOptions {
    let mut cfg = Config::default();
    cfg.runtime.kind = crate::config::RuntimeKind::Kubernetes;
    let spec = KubeSpec::from_config(
        &cfg,
        PodEnv {
            namespace: "tracon-node".into(),
            pod_ip: "10.244.0.9".into(),
            node_name: "general-1".into(),
        },
    );
    KubeSandboxOptions {
        node_namespace: "tracon-node".into(),
        ..KubeSandboxOptions::from_config(&cfg, spec)
    }
}

/// Rendering reads nothing from the API; the client needs a runtime only
/// to exist.
fn provider() -> KubeSandboxProvider {
    let client = kube::Client::new(axum::Router::new(), "tracon-node");
    KubeSandboxProvider::new(client, options())
}

fn bootstrap() -> Bootstrap {
    Bootstrap {
        run: "tracon-h-1:run".into(),
        cert_pin: format!("sha256:{}", "ab".repeat(32)),
        token: "one-time-token".into(),
    }
}

fn session_mounts() -> Vec<Mount> {
    vec![
        Mount::local("/work"),
        Mount::local("/home/harness/.claude"),
        // Withheld from the files channel: still never the claim.
        Mount::volume("tracon-harness-state", "/home/harness/.state", false),
    ]
}

fn render(shape: Shape, labels: &RunLabels, gated: bool) -> Pod {
    let mounts = session_mounts();
    provider()
        .render(
            "tracon-h-1",
            &CreateSpec {
                name: Some("tracon-h-1"),
                image: "registry.example/app@sha256:aa",
                resources: &Resources::default(),
                mounts: &mounts,
                bootstrap: &bootstrap(),
                labels,
                shape,
            },
            "10.244.0.9:7426",
            &"cd".repeat(32),
            gated,
        )
        .unwrap()
}

/// Nothing of the claim, no Secret, no subPath, no `nodeName`: the pod's
/// storage is its own, and affinity keeps it beside the node pod.
#[tokio::test]
async fn a_sandbox_pod_holds_nothing_of_the_claim_and_sits_beside_the_node() {
    let pod = render(Shape::Session, &RunLabels::of(RunKind::Session), false);
    let rendered = serde_json::to_string(&pod).unwrap();
    assert!(!rendered.contains("persistentVolumeClaim"), "{rendered}");
    assert!(!rendered.contains("subPath"), "{rendered}");
    assert!(!rendered.contains("secretKeyRef"), "{rendered}");
    assert!(!rendered.contains("tracon-state"), "{rendered}");
    let spec = pod.spec.clone().unwrap();
    assert!(spec.node_name.is_none());
    assert!(spec.node_selector.is_none());
    assert!(spec.scheduling_gates.is_none());
    let term = &spec
        .affinity
        .unwrap()
        .pod_affinity
        .unwrap()
        .required_during_scheduling_ignored_during_execution
        .unwrap()[0];
    assert_eq!(term.topology_key, "kubernetes.io/hostname");
    assert_eq!(term.namespaces, Some(vec!["tracon-node".to_string()]));
    // This node's pod alone: another node in the namespace has the role too.
    assert_eq!(
        term.label_selector.as_ref().unwrap().match_labels,
        Some(BTreeMap::from([
            (ROLE_LABEL.into(), "node".into()),
            (NODE_LABEL.into(), "tracon".into()),
        ]))
    );
    let labels = pod.metadata.labels.clone().unwrap();
    assert_eq!(labels[ROLE_LABEL], SANDBOX_ROLE);
    assert_eq!(labels[NODE_LABEL], "tracon");
    assert!(spec.volumes.iter().flatten().all(|v| v.empty_dir.is_some()));
    // Every directory the run names is an emptyDir at its target.
    let mounted: Vec<&str> = spec.containers[0]
        .volume_mounts
        .iter()
        .flatten()
        .map(|m| m.mount_path.as_str())
        .collect();
    for target in [
        "/opt/tracon",
        "/home/harness",
        "/dev/shm",
        "/tmp",
        "/work",
        "/home/harness/.claude",
        "/home/harness/.state",
    ] {
        assert!(mounted.contains(&target), "{target} in {mounted:?}");
    }
    assert!(breaches(&pod, "tracon-gw").is_empty());
}

/// The supervisor is the container's command, from the node's own bundle,
/// which an init container in the sandbox's own image fetches from the node
/// on the sandbox port and checks against the digest the node wrote; read-only
/// where it runs; both containers carry the gate.
#[tokio::test]
async fn the_bundle_is_fetched_from_the_node_and_the_supervisor_runs_from_it() {
    let pod = render(Shape::Session, &RunLabels::default(), false);
    let spec = pod.spec.unwrap();
    let init = &spec.init_containers.as_ref().unwrap()[0];
    let c = &spec.containers[0];
    assert_eq!(init.name, BUNDLE_CONTAINER);
    assert_eq!(init.image, c.image);
    let script = &init.command.as_ref().unwrap()[2];
    assert!(script.contains("sha256sum -c"), "{script}");
    let env: BTreeMap<String, String> = init
        .env
        .iter()
        .flatten()
        .map(|e| (e.name.clone(), e.value.clone().unwrap()))
        .collect();
    assert_eq!(
        env["TRACON_BUNDLE_URL"],
        "http://10.244.0.9:7426/bundle.tar"
    );
    assert_eq!(env["TRACON_BUNDLE_SHA256"], "cd".repeat(32));
    assert_eq!(c.command, Some(vec![SUPERVISOR.to_string()]));
    assert!(c.args.is_none());
    assert_eq!(c.stdin, None);
    let bundle = c
        .volume_mounts
        .iter()
        .flatten()
        .find(|m| m.mount_path == BUNDLE_TARGET)
        .unwrap();
    assert_eq!(bundle.read_only, Some(true));
    for c in [init, c] {
        let sc = c.security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.privileged, Some(false));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".into()])
        );
    }
    // The user P6's sandboxes run as, with the home it gives them.
    let psc = spec.security_context.unwrap();
    assert_eq!(psc.run_as_user, Some(65532));
    assert_eq!(psc.run_as_group, Some(65532));
    assert_eq!(psc.fs_group, Some(65532));
}

/// The node packs its bundle as the same bytes every time, and serves it on
/// the sandbox port to a plain `GET /bundle.tar` while a TLS hello on the
/// same port still goes to the run expected at its address.
#[tokio::test]
async fn the_node_serves_its_bundle_on_the_sandbox_port() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle");
    std::fs::create_dir_all(bundle.join("bin")).unwrap();
    std::fs::write(bundle.join("bin/tracon-sandbox"), "#!/bin/sh\n").unwrap();
    std::fs::write(bundle.join("manifest.json"), "{}").unwrap();
    let served = served_bundle(&bundle, &dir.path().join("served"))
        .await
        .unwrap();
    let again = write_tar(&bundle, &dir.path().join("again.tar")).unwrap();
    assert_eq!(served.sha256, again.sha256, "packed the same way twice");

    let dispatcher = Dispatcher::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    dispatcher.serve(served.clone());
    let get = |path: &'static str| {
        let port = dispatcher.local_addr().port();
        async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: n\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut body = Vec::new();
            stream.read_to_end(&mut body).await.unwrap();
            body
        }
    };
    let answer = get(BUNDLE_PATH).await;
    let split = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    assert!(answer.starts_with(b"HTTP/1.1 200 OK"));
    use sha2::Digest as _;
    assert_eq!(
        hex::encode(sha2::Sha256::digest(&answer[split + 4..])),
        served.sha256
    );
    let mut entries = tar::Archive::new(&answer[split + 4..]);
    let names: Vec<String> = entries
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect();
    assert_eq!(names, ["bin", "bin/tracon-sandbox", "manifest.json"]);
    assert!(get("/etc/passwd").await.starts_with(b"HTTP/1.1 404"));

    // A refused bundle: no supervisor in it.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(served_bundle(&empty, &dir.path().join("served"))
        .await
        .is_err());
}

/// The gateway's name is the pod's loopback, where the tunnels listen; no
/// resolver; the supervisor is told the node pod's address and nothing that
/// names a Secret, and the token is spent by its first use (see the
/// integration test) rather than hidden.
#[tokio::test]
async fn the_sandbox_reaches_the_node_only_by_address_and_its_tunnels() {
    let pod = render(Shape::Session, &RunLabels::default(), false);
    let spec = pod.spec.unwrap();
    assert_eq!(spec.dns_policy.as_deref(), Some("None"));
    let alias = &spec.host_aliases.unwrap()[0];
    assert_eq!(alias.ip, "127.0.0.1");
    assert_eq!(alias.hostnames, Some(vec!["tracon-gw".into()]));
    let env: BTreeMap<String, String> = spec.containers[0]
        .env
        .iter()
        .flatten()
        .map(|e| (e.name.clone(), e.value.clone().unwrap()))
        .collect();
    assert_eq!(env["TRACON_SANDBOX_ADDR"], "10.244.0.9:7426");
    assert_eq!(env["TRACON_SANDBOX_TOKEN"], "one-time-token");
    assert_eq!(env["HTTPS_PROXY"], "http://tracon-gw:8888");
    assert!(env["TRACON_SANDBOX_PIN"].starts_with("sha256:"));
}

/// The labels a metrics stack reads (`pod_labels`) are kept, with the role
/// the sandbox policy selects; each shape is sized as the harness pod it
/// stands in for, and a probe has no shared memory.
#[tokio::test]
async fn sandbox_pods_keep_their_labels_and_sizing() {
    let labels = RunLabels {
        kind: Some(RunKind::Check),
        repo: Some("Cosmic Spork/Repo".into()),
        channel: Some("@ops".into()),
        ..Default::default()
    };
    let pod = render(Shape::Command, &labels, false);
    let l = pod.metadata.labels.clone().unwrap();
    assert_eq!(l[ROLE_LABEL], SANDBOX_ROLE);
    assert_eq!(l[SESSION_LABEL], "tracon-h-1");
    assert_eq!(l[KIND_LABEL], "check");
    assert_eq!(l[SIZING_LABEL], "command");
    assert_eq!(l[REPO_LABEL], "cosmic-spork-repo");
    assert_eq!(l[CHANNEL_LABEL], "ops");
    let requests = pod.spec.unwrap().containers[0]
        .resources
        .clone()
        .unwrap()
        .requests
        .unwrap();
    assert_eq!(requests["memory"].0, "2Gi");

    let pod = render(Shape::Probe, &RunLabels::default(), false);
    let l = pod.metadata.labels.clone().unwrap();
    assert_eq!(l[KIND_LABEL], "probe");
    let spec = pod.spec.unwrap();
    assert!(spec.volumes.unwrap().iter().all(|v| v.name != SHM_VOLUME));
    assert_eq!(
        spec.containers[0]
            .resources
            .clone()
            .unwrap()
            .requests
            .unwrap()["memory"]
            .0,
        "128Mi"
    );

    let gated = render(Shape::Probe, &RunLabels::of(RunKind::Probe), true);
    assert_eq!(
        gated.spec.unwrap().scheduling_gates.unwrap()[0].name,
        PROBE_GATE
    );
}

/// What admission could add that the checks refuse.
#[tokio::test]
async fn admission_that_widens_the_pod_is_a_breach() {
    let mut pod = render(Shape::Session, &RunLabels::default(), false);
    let spec = pod.spec.as_mut().unwrap();
    spec.volumes.as_mut().unwrap().push(Volume {
        name: "state".into(),
        persistent_volume_claim: Some(
            k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                claim_name: "tracon-state".into(),
                read_only: None,
            },
        ),
        ..Default::default()
    });
    spec.containers[0]
        .volume_mounts
        .as_mut()
        .unwrap()
        .push(VolumeMount {
            name: "state".into(),
            mount_path: "/work".into(),
            sub_path: Some("tracon-workspace-a".into()),
            ..Default::default()
        });
    spec.containers[0].env.as_mut().unwrap().push(EnvVar {
        name: "TOKEN".into(),
        value_from: Some(Default::default()),
        ..Default::default()
    });
    spec.dns_policy = Some("ClusterFirst".into());
    spec.automount_service_account_token = None;
    let found = breaches(&pod, "tracon-gw").join("\n");
    for said in [
        "volume state is not an emptyDir",
        "subPath",
        "takes TOKEN from elsewhere",
        "resolver",
        "API token",
    ] {
        assert!(found.contains(said), "{said} in {found}");
    }

    let mut pod = render(Shape::Session, &RunLabels::default(), false);
    pod.spec.as_mut().unwrap().init_containers.as_mut().unwrap()[0].image =
        Some("evil/fetcher".into());
    pod.spec.as_mut().unwrap().host_aliases = Some(vec![HostAlias {
        ip: "10.0.0.1".into(),
        hostnames: Some(vec!["tracon-gw".into()]),
    }]);
    let found = breaches(&pod, "tracon-gw").join("\n");
    assert!(found.contains("another image"), "{found}");
    assert!(found.contains("resolves to 10.0.0.1"), "{found}");
    assert!(found.contains("not the pod's loopback"), "{found}");
}

/// Affinity puts the pod through the scheduler, which leaves one that does
/// not fit pending; a lack of room is a refusal, as the kubelet's was.
#[test]
fn a_pod_the_scheduler_has_no_room_for_is_refused() {
    use k8s_openapi::api::core::v1::PodCondition;
    let unschedulable_for = |message: &str| PodStatus {
        phase: Some("Pending".into()),
        conditions: Some(vec![PodCondition {
            type_: "PodScheduled".into(),
            status: "False".into(),
            reason: Some("Unschedulable".into()),
            message: Some(message.into()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let k = options().spec;
    let status =
        unschedulable_for("0/3 nodes are available: 1 Insufficient memory, 2 node(s) didn't match pod affinity rules.");
    match progress("tracon-c-1", &status, Sizing::Command, &k) {
        Progress::Failed(reason, ProviderError::Refused { message, .. }) => {
            assert_eq!(reason, "out_of_memory");
            assert!(
                message.contains("[runtime.kubernetes.resources.command] memory_request"),
                "{message}"
            );
        }
        _ => panic!("not refused"),
    }
    let other = unschedulable_for("0/3 nodes are available: 3 node(s) had untolerated taint.");
    assert!(matches!(
        progress("tracon-c-1", &other, Sizing::Command, &k),
        Progress::Waiting
    ));
}

/// A connection is handed only to the run expected at its address, whether
/// it arrived before the node learned that address or after; one from any
/// other address is never handed over.
// The stranger dials from 127.0.0.2, which only Linux routes as loopback
// without configuration; a pod-hosted node runs on Linux.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn connections_go_to_the_run_expected_at_their_address() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let dispatcher = Dispatcher::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let port = dispatcher.local_addr().port();
    let dial = |from: &str, byte: u8| {
        let from: SocketAddr = format!("{from}:0").parse().unwrap();
        async move {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.bind(from).unwrap();
            let mut stream = socket.connect(([127, 0, 0, 1], port).into()).await.unwrap();
            // A hello's first byte, then which connection this is.
            stream.write_all(&[TLS_HANDSHAKE, byte]).await.unwrap();
            stream
        }
    };
    // Before the address is known: held.
    let _early = dial("127.0.0.1", 1).await;
    let _stranger = dial("127.0.0.2", 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut expected = dispatcher.expect(&["127.0.0.1".parse().unwrap()]);
    let mut first = tokio::time::timeout(Duration::from_secs(5), expected.next())
        .await
        .unwrap()
        .unwrap();
    let mut byte = [0u8; 2];
    first.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, [TLS_HANDSHAKE, 1]);
    // After: handed straight over.
    let _late = dial("127.0.0.1", 3).await;
    let mut second = tokio::time::timeout(Duration::from_secs(5), expected.next())
        .await
        .unwrap()
        .unwrap();
    second.read_exact(&mut byte).await.unwrap();
    assert_eq!(byte, [TLS_HANDSHAKE, 3]);
    // The stranger's is never handed to this run.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), expected.next())
            .await
            .is_err()
    );
    drop(expected);
    assert!(dispatcher.state.lock().unwrap().waiting.is_empty());
}

/// The node's own pod is found by its role and its prefix: one with only
/// the role, or another node's prefix, is not this node's.
#[test]
fn the_node_pod_is_found_by_its_role_and_its_prefix() {
    assert_eq!(node_pod_unlabelled(&node_pod_labels()), None);
    let mut more = node_pod_labels();
    more.insert("app".into(), "tracon".into());
    assert_eq!(node_pod_unlabelled(&more), None);
    let why = node_pod_unlabelled(&BTreeMap::from([(ROLE_LABEL.into(), "node".into())])).unwrap();
    assert!(why.contains("tracon.dev/node: tracon"), "{why}");
    assert!(!why.contains("tracon.dev/role"), "{why}");
    let another = BTreeMap::from([
        (ROLE_LABEL.into(), "node".into()),
        (NODE_LABEL.into(), "spike".into()),
    ]);
    assert!(node_pod_unlabelled(&another).is_some());
}

/// Only a policy besides the sandbox's own, selecting the sandbox, with a
/// rule of its own, widens its route; one that selects other pods, or only
/// denies, does not.
#[test]
fn another_policy_selecting_a_sandbox_widens_its_route() {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicySpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;
    let policy = |name: &str, selector: LabelSelector, egress: bool| NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(name.into()),
            ..Default::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(selector),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(match egress {
                true => vec![NetworkPolicyEgressRule::default()],
                false => vec![],
            }),
            ..Default::default()
        }),
    };
    let labels = |map: &[(&str, &str)]| LabelSelector {
        match_labels: Some(
            map.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..Default::default()
    };
    let pod = sandbox_pod_labels();
    let own = sandbox_policy();
    let policies = vec![
        policy(&own, labels(&[(ROLE_LABEL, SANDBOX_ROLE)]), true),
        policy("spike-sandbox", labels(&[(ROLE_LABEL, SANDBOX_ROLE)]), true),
        policy(
            "spike-only",
            labels(&[(ROLE_LABEL, SANDBOX_ROLE), (NODE_LABEL, "spike")]),
            true,
        ),
        policy("deny-all", LabelSelector::default(), false),
        policy("harness", labels(&[(ROLE_LABEL, "harness")]), true),
        policy("everything", LabelSelector::default(), true),
        policy(
            "by-expression",
            LabelSelector {
                match_expressions: Some(vec![LabelSelectorRequirement {
                    key: NODE_LABEL.into(),
                    operator: "In".into(),
                    values: Some(vec!["tracon".into(), "spike".into()]),
                }]),
                ..Default::default()
            },
            true,
        ),
    ];
    assert_eq!(
        widening_policies(&policies, &own, &pod),
        ["spike-sandbox", "everything", "by-expression"]
    );
}

/// A snapshot is the image the preparation ran, with its cache and its
/// prepared `/work` on top, owned by the sandbox's user: nothing else of the
/// pod, and the image's own user, home and directory kept.
#[test]
fn a_snapshot_layers_the_prepared_trees_onto_the_image_it_ran() {
    let snapshots = Snapshots::new(
        Arc::new(crate::boundary::kubernetes::build::Unusable(
            "unused".into(),
        )),
        Duration::from_secs(60),
    );
    assert_eq!(
        snapshot_containerfile("registry.example/app@sha256:aa", &snapshots.trees),
        "FROM registry.example/app@sha256:aa\n\
         COPY --chown=65532:65532 cache/ /cache/\n\
         COPY --chown=65532:65532 work/ /work/\n"
    );
}

/// A snapshot's trees together get at most the prepared limit, less where
/// the state volume cannot spare that and keep its margin; too little room
/// refuses before anything is staged.
#[test]
fn a_snapshot_is_staged_only_within_what_the_volume_can_spare() {
    const GIB: u64 = 1024 * 1024 * 1024;
    assert_eq!(snapshot_budget(None), Ok(SNAPSHOT_MAX_BYTES));
    assert_eq!(snapshot_budget(Some(100 * GIB)), Ok(SNAPSHOT_MAX_BYTES));
    assert_eq!(snapshot_budget(Some(SNAPSHOT_FREE_MARGIN + GIB)), Ok(GIB));
    let why = snapshot_budget(Some(SNAPSHOT_FREE_MARGIN + 1024)).unwrap_err();
    assert!(why.contains("make room"), "{why}");
    assert!(snapshot_budget(Some(0)).is_err());
}
