//! Kubernetes sandboxes over the sandbox protocol (`provider::kubernetes`),
//! against a fake API and `tracon-sandbox`'s own loop: the fake's "kubelet"
//! runs each pod the node creates as the pod spec says. Its init container's
//! part: fetch the bundle from the node's sandbox port and check it against
//! the digest the spec names. Then the loop, from the environment the spec
//! gives it, dialling the same port over TCP as a sandbox pod would.
//!
//! The loop reaps every child of its process with `waitpid(-1)`, so this is
//! one test function in a file of its own (see `protocol_runner.rs`), and a
//! sandbox's paths are a temporary directory's.

#![cfg(target_os = "linux")]

#[path = "support/mod.rs"]
mod support;
use support::kube_api::{KubeApi, Script};

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tracon::boundary::{BuildFailure, BuiltImage, ImageBuild, ImageBuilder};
use tracon::config::{Config, RuntimeKind};
use tracon::provider::kubernetes::{
    KubeSandboxOptions, KubeSandboxProvider, Snapshots, SANDBOX_ROLE,
};
use tracon::provider::SandboxProvider as _;
use tracon::runner::kube::{KubeSpec, PodEnv, ROLE_LABEL};
use tracon::runner::protocol::ProtocolRunner;
use tracon::runner::sandbox_files::FilesStore;
use tracon::runner::{Mount, RunKind, RunLabels, Runner, RunnerCommand, SecretEnv};
use tracon_sandbox::carrier::Dial;
use tracon_sandbox::inside;
use tracon_sandbox::node::{SandboxError, SandboxServer, ServerOptions};
use tracon_sandbox::tls::{CertPin, NodeIdentity};

const NAMESPACE: &str = "tracon-node";

fn sandbox(config: inside::Config) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _ = runtime.block_on(inside::run(config));
        runtime.shutdown_timeout(Duration::from_secs(1));
    })
}

fn env_of(pod: &Pod) -> BTreeMap<String, String> {
    pod.spec.as_ref().unwrap().containers[0]
        .env
        .iter()
        .flatten()
        .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
        .collect()
}

/// Stands in for the kubelet: every pod the node creates gets the loop,
/// started with that pod's own bootstrap environment.
struct Kubelet {
    threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    task: tokio::task::JoinHandle<()>,
    /// What each init container unpacked, by entry name.
    fetched: Arc<Mutex<Vec<Vec<String>>>>,
}

/// An init container's fetch: `GET` the bundle, check its digest, list it.
async fn fetch_bundle(url: &str, sha256: &str) -> Vec<String> {
    use sha2::Digest as _;
    use tokio::io::AsyncReadExt as _;
    let rest = url.strip_prefix("http://").unwrap();
    let (addr, path) = rest.split_once('/').unwrap();
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET /{path} HTTP/1.1\r\nHost: node\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    assert!(answer.starts_with(b"HTTP/1.1 200 OK"));
    let at = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let body = &answer[at..];
    assert_eq!(hex::encode(sha2::Sha256::digest(body)), sha256);
    tar::Archive::new(body)
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect()
}

impl Kubelet {
    fn start(api: KubeApi) -> Self {
        let threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>> = Arc::default();
        let started = threads.clone();
        let fetched: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let fetches = fetched.clone();
        let task = tokio::spawn(async move {
            let mut seen = HashSet::new();
            loop {
                for pod in api.created() {
                    let name = pod.metadata.name.clone().unwrap();
                    if !seen.insert(name) {
                        continue;
                    }
                    let init = &pod.spec.as_ref().unwrap().init_containers.as_ref().unwrap()[0];
                    let init_env: BTreeMap<String, String> = init
                        .env
                        .iter()
                        .flatten()
                        .map(|e| (e.name.clone(), e.value.clone().unwrap_or_default()))
                        .collect();
                    let unpacked = fetch_bundle(
                        &init_env["TRACON_BUNDLE_URL"],
                        &init_env["TRACON_BUNDLE_SHA256"],
                    )
                    .await;
                    fetched.lock().unwrap().push(unpacked);
                    let env = env_of(&pod);
                    let config = inside::Config::new(
                        Dial::Tcp(env["TRACON_SANDBOX_ADDR"].clone()),
                        CertPin::parse(&env["TRACON_SANDBOX_PIN"]).unwrap(),
                        env["TRACON_SANDBOX_TOKEN"].clone(),
                    );
                    started.lock().unwrap().push(sandbox(config));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        Kubelet {
            threads,
            task,
            fetched: fetches,
        }
    }

    /// Wait for every sandbox to have ended, so no reaper outlives the test.
    async fn settle(&self) {
        let threads: Vec<_> = std::mem::take(&mut *self.threads.lock().unwrap());
        for thread in threads {
            tokio::time::timeout(
                Duration::from_secs(30),
                tokio::task::spawn_blocking(move || thread.join()),
            )
            .await
            .expect("a sandbox did not end")
            .unwrap()
            .unwrap();
        }
    }
}

fn provider(api: &KubeApi, bundle: &std::path::Path) -> KubeSandboxProvider {
    let mut cfg = Config::default();
    cfg.runtime.kind = RuntimeKind::Kubernetes;
    cfg.runtime.kubernetes.sandbox_protocol = true;
    let spec = KubeSpec::from_config(
        &cfg,
        PodEnv {
            namespace: NAMESPACE.into(),
            pod_ip: "127.0.0.1".into(),
            node_name: "general-1".into(),
        },
    );
    KubeSandboxProvider::new(
        api.client(),
        KubeSandboxOptions {
            listen: "127.0.0.1:0".parse().unwrap(),
            node_namespace: NAMESPACE.into(),
            bundle: bundle.join("bundle"),
            served_dir: bundle.join("served"),
            ..KubeSandboxOptions::from_config(&cfg, spec)
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kubernetes_sandboxes_over_the_protocol() {
    let api = KubeApi::new(NAMESPACE).default_script(
        Script::default()
            .pod_ip("127.0.0.1")
            .running(Duration::from_secs(3600)),
    );
    let kubelet = Kubelet::start(api.clone());
    let identity = NodeIdentity::generate().unwrap();
    let server = Arc::new(SandboxServer::new(&identity, ServerOptions::default()).unwrap());
    let disk = tempfile::tempdir().unwrap();
    // The node's bundle, as it would hold one: here, only a supervisor's
    // name in it, since the loop runs in this process.
    std::fs::create_dir_all(disk.path().join("bundle/bin")).unwrap();
    std::fs::write(disk.path().join("bundle/bin/tracon-sandbox"), "stand-in").unwrap();
    let store = FilesStore::new(disk.path().join("claim"))
        .withholding("tracon-harness-state")
        .keeping_every_volume();
    let work = disk.path().join("sandbox/work");
    let seed = disk.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("a.txt"), "from the claim\n").unwrap();
    store.import("tracon-workspace-x", &seed).unwrap();
    let runner = ProtocolRunner::new(
        Arc::new(provider(&api, disk.path())),
        "registry.example/harness:1",
    )
    .with_server(server.clone())
    .with_files(store.clone());

    // A captured command: its workspace sent in over the files channel, its
    // secret environment over the protocol, its output and exit code back.
    let out = runner
        .run_capture(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "cat {}/a.txt; echo \"$LENT\"; echo to-stderr >&2; exit 3",
                    work.display()
                ),
            ],
            secret_env: SecretEnv::new(vec![("LENT".into(), "lent-value-123".into())]),
            mounts: vec![Mount::volume(
                "tracon-workspace-x",
                work.to_string_lossy(),
                false,
            )],
            name: "tracon-c-1".into(),
            labels: RunLabels {
                repo: Some("app".into()),
                ..RunLabels::of(RunKind::Check)
            },
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from the claim\nlent-value-123\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "to-stderr\n");

    // What the node made of it: a sandbox pod, sized and labelled as the
    // check it is, with nothing of the claim, and no Secret anywhere.
    let created = api.created();
    let pod = created.last().unwrap();
    let labels = pod.metadata.labels.clone().unwrap();
    assert_eq!(labels[ROLE_LABEL], SANDBOX_ROLE);
    assert_eq!(labels["tracon.dev/node"], "tracon");
    assert_eq!(labels["tracon.dev/kind"], "check");
    assert_eq!(labels["tracon.dev/sizing"], "command");
    assert_eq!(labels["tracon.dev/repo"], "app");
    let rendered = serde_json::to_string(pod).unwrap();
    assert!(!rendered.contains("persistentVolumeClaim"), "{rendered}");
    assert!(!rendered.contains("subPath"), "{rendered}");
    assert!(!rendered.contains("secretKeyRef"), "{rendered}");
    assert!(
        !rendered.contains("lent-value-123"),
        "secret environment reached the pod spec: {rendered}"
    );
    assert!(api.secrets_created().is_empty());
    assert!(api.secret_patches().is_empty());
    assert!(api.live().is_empty(), "the pod outlived its run");
    // Its init container fetched the node's own bundle, as it checked out.
    assert_eq!(
        kubelet.fetched.lock().unwrap()[0],
        ["bin", "bin/tracon-sandbox"]
    );

    // Its token was spent by the sandbox's first hello: presented again, to
    // the same node, it is refused.
    let spent = env_of(pod)["TRACON_SANDBOX_TOKEN"].clone();
    let socket = disk.path().join("again.sock");
    let listener =
        tracon_sandbox::carrier::Listener::Unix(tokio::net::UnixListener::bind(&socket).unwrap());
    let again = sandbox(inside::Config::new(
        Dial::Unix(socket.clone()),
        server.pin(),
        spent,
    ));
    match server.accept_from(&listener).await {
        Err(SandboxError::BadToken) => {}
        Err(other) => panic!("a spent token was refused for the wrong reason: {other}"),
        Ok(_) => panic!("a spent token was accepted"),
    }
    tokio::task::spawn_blocking(move || again.join())
        .await
        .unwrap()
        .unwrap();

    // A session: its harness's stdio over the protocol, sized as a session.
    let spawned = runner
        .spawn(RunnerCommand {
            argv: vec!["cat".into()],
            name: "tracon-h-1".into(),
            labels: RunLabels::of(RunKind::Session),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut stdin = spawned.stdin;
    let mut lines = BufReader::new(spawned.stdout).lines();
    stdin.write_all(b"over the protocol\n").await.unwrap();
    assert_eq!(
        lines.next_line().await.unwrap().as_deref(),
        Some("over the protocol")
    );
    drop(stdin);
    assert_eq!(spawned.done.await.unwrap(), 0);
    let session = api.created().last().cloned().unwrap();
    let labels = session.metadata.labels.unwrap();
    assert_eq!(labels["tracon.dev/kind"], "session");
    assert_eq!(labels["tracon.dev/sizing"], "session");
    assert!(api.secrets_created().is_empty());

    // The provider reads isolation back from the pod and the policy: with
    // no sandbox policy in place, the route is not claimed.
    let provider = provider(&api, disk.path());
    let handle = tracon::provider::Handle("tracon-h-2".into());
    api.insert_pod(session_pod(&provider, "tracon-h-2"), Script::default());
    let caps = provider.verify_isolation(&handle).await.unwrap();
    assert_eq!(
        caps.route,
        tracon::provider::capabilities::Route::Unverified
    );
    assert_eq!(caps.dns, tracon::provider::capabilities::Dns::None);

    // With this node's policy in place, the route is claimed: it selects
    // this node's sandboxes and reaches this node's pod alone.
    let port = provider.dispatcher().await.unwrap().local_addr().port();
    api.insert_policy(own_sandbox_policy(i32::from(port)));
    let caps = provider.verify_isolation(&handle).await.unwrap();
    assert_eq!(caps.route, tracon::provider::capabilities::Route::NodeOnly);

    // Policies add up: another that selects every sandbox in the namespace
    // (an older node's `role: sandbox` one) widens the route, which is then
    // not claimed.
    let mut older = own_sandbox_policy(i32::from(port));
    older.metadata.name = Some("spike-sandbox".into());
    older
        .spec
        .as_mut()
        .unwrap()
        .pod_selector
        .as_mut()
        .unwrap()
        .match_labels = Some(BTreeMap::from([(ROLE_LABEL.into(), SANDBOX_ROLE.into())]));
    api.insert_policy(older);
    let caps = provider.verify_isolation(&handle).await.unwrap();
    assert_eq!(
        caps.route,
        tracon::provider::capabilities::Route::Unverified
    );
    let _ = provider.destroy(&handle).await;

    // A sandbox pod of another node in the namespace is not this one's.
    let mut theirs = session_pod(&provider, "spike-h-1");
    theirs
        .metadata
        .labels
        .as_mut()
        .unwrap()
        .insert("tracon.dev/node".into(), "spike".into());
    api.insert_pod(theirs, Script::default());
    let refused = provider
        .verify_isolation(&tracon::provider::Handle("spike-h-1".into()))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("not made by this provider"));

    // A preparation, snapshotted: a pod cannot be committed, so what the
    // preparation left in its trees is read back over the protocol and built
    // onto the image the pod ran, by the node's builder.
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let trees = disk.path().join("sandbox/prepared");
    std::fs::create_dir_all(&trees).unwrap();
    let snapshots = Snapshots {
        trees: vec![
            ("cache".into(), trees.join("cache").to_string_lossy().into()),
            ("work".into(), trees.join("work").to_string_lossy().into()),
        ],
        ..Snapshots::new(
            Arc::new(Recording(recorded.clone())),
            Duration::from_secs(60),
        )
    };
    let held = tracon::sandbox_images::Held::start(
        Arc::new(provider.with_snapshots(snapshots)),
        None,
        server.clone(),
        "tracon-w-1",
        "registry.example/app@sha256:aa",
        &[],
    )
    .await
    .unwrap();
    let prepared = held
        .run_capture(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "mkdir -p {t}/cache/npm {t}/work/node_modules/x && echo cached > \
                     {t}/cache/npm/blob && echo installed > {t}/work/node_modules/x/index.js \
                     && ln -s /usr/bin/env {t}/work/node_modules/x/env",
                    t = trees.display()
                ),
            ],
            name: "tracon-w-1-p0".into(),
            workdir: Some(trees.to_string_lossy().into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(prepared.status.success(), "{prepared:?}");
    let snapshot = held
        .conclude(
            Ok(()),
            "localhost/tracon-snapshot-app-0123:k1",
            &[("io.tracon.snapshot.key".into(), "k1".into())],
        )
        .await
        .unwrap();
    assert_eq!(
        snapshot,
        "registry.example/cs/tracon-snapshot-app-0123@sha256:5555"
    );
    let recorded = recorded.lock().unwrap().clone();
    let [built] = recorded.as_slice() else {
        panic!("one build: {recorded:?}");
    };
    assert_eq!(built.tag, "localhost/tracon-snapshot-app-0123:k1");
    assert!(
        built
            .containerfile
            .starts_with("FROM registry.example/app@sha256:aa\n"),
        "{}",
        built.containerfile
    );
    assert!(
        built
            .containerfile
            .contains("COPY --chown=65532:65532 cache/ "),
        "{}",
        built.containerfile
    );
    for (path, content) in [
        ("cache/npm/blob", "cached\n"),
        ("work/node_modules/x/index.js", "installed\n"),
    ] {
        assert_eq!(
            built.files.get(path).map(String::as_str),
            Some(content),
            "{path} in {:?}",
            built.files
        );
    }
    // A link that leaves its tree comes back as the link it is, never
    // followed.
    assert_eq!(
        built
            .files
            .get("work/node_modules/x/env")
            .map(String::as_str),
        Some("-> /usr/bin/env")
    );
    assert!(built
        .labels
        .contains(&("io.tracon.snapshot.key".into(), "k1".into())));
    assert!(!api.live().contains(&"tracon-w-1".to_string()));

    kubelet.task.abort();
    kubelet.settle().await;
}

/// `<prefix>-sandbox` as the deployment ships it, on `port`.
fn own_sandbox_policy(port: i32) -> k8s_openapi::api::networking::v1::NetworkPolicy {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyPeer, NetworkPolicyPort,
        NetworkPolicySpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
    let select = |labels| LabelSelector {
        match_labels: Some(labels),
        ..Default::default()
    };
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(tracon::provider::kubernetes::sandbox_policy()),
            ..Default::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(select(tracon::provider::kubernetes::sandbox_pod_labels())),
            policy_types: Some(vec!["Ingress".into(), "Egress".into()]),
            ingress: Some(vec![]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    pod_selector: Some(select(tracon::provider::kubernetes::node_pod_labels())),
                    ..Default::default()
                }]),
                ports: Some(vec![NetworkPolicyPort {
                    port: Some(IntOrString::Int(port)),
                    protocol: Some("TCP".into()),
                    ..Default::default()
                }]),
            }]),
        }),
    }
}

/// What the builder was asked to build.
#[derive(Debug, Clone)]
struct Recorded {
    tag: String,
    containerfile: String,
    labels: Vec<(String, String)>,
    /// Every file in the context by its path: its content, or `-> target`
    /// for a link.
    files: BTreeMap<String, String>,
}

/// A builder that records each build and reports it pushed.
struct Recording(Arc<Mutex<Vec<Recorded>>>);

#[async_trait::async_trait]
impl ImageBuilder for Recording {
    async fn build(&self, build: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
        fn walk(root: &std::path::Path, at: &std::path::Path, out: &mut BTreeMap<String, String>) {
            for entry in std::fs::read_dir(root.join(at)).unwrap() {
                let entry = entry.unwrap();
                let rel = at.join(entry.file_name());
                let meta = std::fs::symlink_metadata(entry.path()).unwrap();
                let key = rel.to_string_lossy().into_owned();
                if meta.is_dir() {
                    walk(root, &rel, out);
                } else if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(entry.path()).unwrap();
                    out.insert(key, format!("-> {}", target.display()));
                } else {
                    out.insert(key, std::fs::read_to_string(entry.path()).unwrap());
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(build.context, std::path::Path::new(""), &mut files);
        self.0.lock().unwrap().push(Recorded {
            tag: build.tag.into(),
            containerfile: std::fs::read_to_string(build.containerfile).unwrap(),
            labels: build.labels.to_vec(),
            files,
        });
        Ok(BuiltImage {
            image: "registry.example/cs/tracon-snapshot-app-0123@sha256:5555".into(),
            log_tail: String::new(),
        })
    }
    async fn exists(&self, _image: &str) -> bool {
        true
    }
    async fn identity(&self, image: &str) -> Option<String> {
        Some(image.into())
    }
    async fn remove(&self, _image: &str) {}
}

/// A sandbox pod as the provider renders it, for a read-back.
fn session_pod(provider: &KubeSandboxProvider, name: &str) -> Pod {
    use tracon::provider::{Bootstrap, CreateSpec, Resources, Shape};
    provider
        .render(
            name,
            &CreateSpec {
                name: Some(name),
                image: "",
                resources: &Resources::default(),
                mounts: &[],
                bootstrap: &Bootstrap {
                    run: name.into(),
                    cert_pin: format!("sha256:{}", "ab".repeat(32)),
                    token: String::new(),
                },
                labels: &RunLabels::of(RunKind::Session),
                shape: Shape::Session,
            },
            "127.0.0.1:1",
            &"ab".repeat(32),
            false,
        )
        .unwrap()
}
