//! Kubernetes sandboxes over the sandbox protocol
//! (`[runtime.kubernetes] sandbox_protocol`): one pod per sandbox, on the
//! node's own machine, whose only process the node starts is
//! `tracon-sandbox`, which then runs the harness over the protocol.
//!
//! **Bundle.** The node's own (`[sandbox] bundle`, else the one it fetched
//! and checked for its release, `crate::bundle`). The node serves it as one
//! tar on the sandbox port, to the only pods that can reach that port; an init
//! container in the sandbox's own image fetches it, checks it against the
//! digest the node wrote into the pod spec, and unpacks it into an emptyDir the
//! sandbox mounts read-only at `/opt/tracon`. Image volumes would save the
//! copy, but a cluster on containerd 1.7 cannot mount them, and a bundle image
//! would be a second copy of what the node already holds and has checked.
//!
//! **Files.** Nothing of the state claim is mounted. Every directory a run
//! names is an emptyDir of the pod's own, filled and read back over the
//! protocol's files channel (`runner::sandbox_files`), so a harness pod never
//! holds a subPath of the node's ReadWriteOnce volume and needs no `nodeName`
//! to share it. It is kept beside the node pod by pod affinity instead.
//!
//! **Carrier.** The sandbox dials the node pod's address on `sandbox_port`
//! over TLS, pinned to the node's certificate. Its NetworkPolicy
//! (`tracon-sandbox`, prefixed per node) allows that one destination and
//! nothing else: no DNS, no ingress. The MCP and model gateway and both
//! proxies are tunnels over the connection (`runner::tunnels::kube_tunnels`),
//! the gateway's name mapped to the pod's loopback.
//!
//! **Token.** The run's token is in the pod's environment, and no Secret is
//! made for it. What keeps it worth nothing to whoever reads the pod spec:
//! it is consumed by the first hello that presents it, accepted or not; the
//! node only hands a connection to the TLS handshake when it comes from the
//! address of the pod the token was issued to (any other is closed before a
//! byte of it is read, so it can neither use nor burn the token); and it is
//! revoked when the run ends or its sandbox fails to connect in time. Only a
//! `tracon-sandbox` pod may reach the port at all. After a dropped
//! connection the supervisor redials with the resume secret the node gave
//! it inside TLS, which binds the same way: taken only from that pod's
//! address, and only for its run.
//!
//! Isolation is read back from the API (the admitted pod, the policies),
//! never from inside.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{
    Affinity, Capabilities as PodCapabilities, Container, EmptyDirVolumeSource, EnvVar, HostAlias,
    Pod, PodAffinity, PodAffinityTerm, PodDNSConfig, PodSchedulingGate, PodSecurityContext,
    PodSpec, PodStatus, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{DeleteParams, ListParams, LogParams, PostParams};
use kube::Api;
use tokio::net::TcpStream;
use tracon_sandbox::node::{SandboxConn, SandboxError, SandboxServer};

use super::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};
use super::{
    Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    SandboxProvider, Shape, Status,
};
use crate::runner::kube::{
    harness_status, normalize_image_id, pending_reason, pod_labels, quota_refused, refusal,
    refusal_error, KubeSpec, Refusal, Sizing, HARNESS_CONTAINER, PROBE_GATE, ROLE_LABEL,
    SCRATCH_MOUNT, SESSION_LABEL, SHM_VOLUME,
};
use crate::runner::podman::BUNDLE_TARGET;
use crate::runner::RunnerError;
use crate::telemetry::{run_kind, telemetry};

/// The role a sandbox pod carries (`tracon.dev/role`), which its own
/// NetworkPolicy selects. Not `harness`: a harness pod's policy opens the
/// node pod's forward and proxy ports, which a sandbox reaches only through
/// its tunnels.
pub const SANDBOX_ROLE: &str = "sandbox";
/// Where on the sandbox port the node serves its bundle.
pub const BUNDLE_PATH: &str = "/bundle.tar";
/// What the init container runs: the bundle fetched from the node, checked
/// against the digest in its environment, unpacked where the sandbox mounts
/// it. Any image with a shell, `curl` or `wget`, `sha256sum` and `tar`.
const FETCH_BUNDLE: &str = "set -eu; f=/opt/tracon/.bundle.tar; \
if command -v curl >/dev/null 2>&1; then curl -fsS --noproxy '*' -o \"$f\" \"$TRACON_BUNDLE_URL\"; \
else wget -q -O \"$f\" \"$TRACON_BUNDLE_URL\"; fi; \
echo \"$TRACON_BUNDLE_SHA256  $f\" | sha256sum -c - >/dev/null; \
tar -xf \"$f\" -C /opt/tracon; rm -f \"$f\"";
pub const BUNDLE_VOLUME: &str = "bundle";
pub const HOME_VOLUME: &str = "home";
/// The init container that copies the bundle in.
pub const BUNDLE_CONTAINER: &str = "bundle";
const SUPERVISOR: &str = "/opt/tracon/bin/tracon-sandbox";

/// How often a pod on its way up is read.
const POLL: Duration = Duration::from_millis(500);
/// Reads in a row that may fail before a start is given up.
const READ_FAILURES: u32 = 10;
/// How long a connection from an address no sandbox is expected at yet is
/// kept, for the node to learn the pod address it was made for: a
/// supervisor can dial before the pod's status has been read.
const HOLD: Duration = Duration::from_secs(15);
/// Most connections held at once.
const HOLD_MAX: usize = 64;

/// The NetworkPolicy a sandbox pod is selected by: `<prefix>-sandbox`.
pub fn sandbox_policy() -> String {
    crate::config::resource_name("sandbox")
}

/// What the provider is configured with.
#[derive(Debug, Clone)]
pub struct KubeSandboxOptions {
    /// The pod a harness would have run in: namespace, uid, home, gateway
    /// name and proxy port, the node pod's facts, sizing, pull secrets,
    /// stop and start timeouts. Its image is the default for a create that
    /// names none; its `extra_mounts` and `init` are not used.
    pub spec: KubeSpec,
    /// The bundle directory every sandbox gets, holding `bin/tracon-sandbox`.
    pub bundle: PathBuf,
    /// Where the node keeps the tar of it that it serves.
    pub served_dir: PathBuf,
    /// Where the node takes its sandboxes' connections.
    pub listen: SocketAddr,
    /// The namespace of the node's own pod, which affinity keeps every
    /// sandbox beside.
    pub node_namespace: String,
    /// How long the supervisor keeps dialling once started.
    pub connect_timeout: Duration,
}

impl KubeSandboxOptions {
    pub fn from_config(cfg: &crate::config::Config, spec: KubeSpec) -> Self {
        let k = &cfg.runtime.kubernetes;
        KubeSandboxOptions {
            node_namespace: crate::runner::kube::PodEnv::own_namespace()
                .unwrap_or_else(|| spec.env.namespace.clone()),
            spec,
            bundle: crate::bundle::effective(cfg).unwrap_or_default(),
            served_dir: crate::config::Config::state_dir().join(crate::bundle::DIR),
            listen: SocketAddr::from(([0, 0, 0, 0], k.sandbox_port)),
            connect_timeout: crate::runner::protocol::CONNECT_WITHIN,
        }
    }
}

/// One listener per address for every sandbox this process makes, handing
/// each connection to the run expected at its peer's address.
pub struct Dispatcher {
    local: SocketAddr,
    state: Mutex<DispatchState>,
    accepting: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// What an init container is handed for `GET /bundle.tar`.
    bundle: Mutex<Option<Arc<ServedBundle>>>,
}

/// The node's bundle as the one tar its sandboxes' init containers fetch.
#[derive(Debug)]
pub struct ServedBundle {
    pub path: PathBuf,
    pub sha256: String,
    pub len: u64,
}

/// The tar of the bundle at `dir`, made once per process under `served`:
/// entries in name order, no times or owners, so the same bundle is the same
/// bytes.
pub async fn served_bundle(dir: &Path, served: &Path) -> Result<Arc<ServedBundle>, ProviderError> {
    static MADE: OnceLock<tokio::sync::Mutex<HashMap<PathBuf, Arc<ServedBundle>>>> =
        OnceLock::new();
    let mut made = MADE.get_or_init(Default::default).lock().await;
    if let Some(bundle) = made.get(dir) {
        if bundle.path.is_file() {
            return Ok(bundle.clone());
        }
    }
    if !dir.join("bin/tracon-sandbox").is_file() {
        return Err(failed(format!(
            "{} has no bin/tracon-sandbox; set [sandbox] bundle to a harness bundle, or let \
             the node fetch its release's",
            dir.display()
        )));
    }
    use sha2::Digest as _;
    let key = hex::encode(sha2::Sha256::digest(dir.as_os_str().as_encoded_bytes()));
    let path = served.join(format!("served-{}.tar", &key[..16]));
    let (from, to) = (dir.to_path_buf(), path.clone());
    let bundle = tokio::task::spawn_blocking(move || write_tar(&from, &to))
        .await
        .map_err(|e| failed(e.to_string()))?
        .map_err(|e| failed(format!("pack the bundle {}: {e}", dir.display())))?;
    let bundle = Arc::new(bundle);
    made.insert(dir.to_path_buf(), bundle.clone());
    Ok(bundle)
}

fn write_tar(dir: &Path, to: &Path) -> std::io::Result<ServedBundle> {
    use sha2::Digest as _;
    use std::io::Read as _;
    fn add(
        builder: &mut tar::Builder<std::fs::File>,
        root: &Path,
        at: &Path,
    ) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(root.join(at))?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let rel = at.join(entry.file_name());
            let meta = std::fs::symlink_metadata(entry.path())?;
            let mut header = tar::Header::new_ustar();
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            if meta.is_dir() {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                builder.append_data(&mut header, &rel, std::io::empty())?;
                add(builder, root, &rel)?;
            } else if meta.is_file() {
                use std::os::unix::fs::PermissionsExt as _;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(meta.permissions().mode() & 0o755);
                header.set_size(meta.len());
                builder.append_data(&mut header, &rel, std::fs::File::open(entry.path())?)?;
            }
            // Anything else is no part of a bundle and is left out.
        }
        Ok(())
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = to.with_extension("tar.partial");
    let mut builder = tar::Builder::new(std::fs::File::create(&staging)?);
    add(&mut builder, dir, Path::new(""))?;
    builder.into_inner()?.sync_all()?;
    std::fs::rename(&staging, to)?;
    let mut file = std::fs::File::open(to)?;
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        len += n as u64;
        hash.update(&buf[..n]);
    }
    Ok(ServedBundle {
        path: to.to_path_buf(),
        sha256: hex::encode(hash.finalize()),
        len,
    })
}

/// The first byte of a TLS handshake record: a supervisor's hello. Anything
/// else on the sandbox port is an init container asking for the bundle.
const TLS_HANDSHAKE: u8 = 0x16;

/// Answer one plain HTTP request on the sandbox port: the bundle for
/// `GET /bundle.tar`, nothing else.
async fn serve_bundle(mut stream: TcpStream, bundle: Option<Arc<ServedBundle>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut request = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 8192 {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    if !matches!(read, Ok(Ok(()))) {
        return;
    }
    let line = String::from_utf8_lossy(&request);
    let line = line.lines().next().unwrap_or_default();
    let mut parts = line.split(' ');
    let wanted = parts.next() == Some("GET") && parts.next() == Some(BUNDLE_PATH);
    let (Some(bundle), true) = (bundle, wanted) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let Ok(mut file) = tokio::fs::File::open(&bundle.path).await else {
        let _ = stream
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bundle.len
    );
    if stream.write_all(head.as_bytes()).await.is_ok() {
        let _ = tokio::io::copy(&mut file, &mut stream).await;
        let _ = stream.shutdown().await;
    }
}

#[derive(Default)]
struct DispatchState {
    waiting: HashMap<IpAddr, tokio::sync::mpsc::UnboundedSender<TcpStream>>,
    held: Vec<(IpAddr, Instant, TcpStream)>,
}

impl Dispatcher {
    /// The process's dispatcher on `listen`, bound and accepting on first
    /// use.
    pub async fn bind(listen: SocketAddr) -> std::io::Result<Arc<Dispatcher>> {
        static BOUND: OnceLock<tokio::sync::Mutex<HashMap<SocketAddr, Arc<Dispatcher>>>> =
            OnceLock::new();
        let mut bound = BOUND.get_or_init(Default::default).lock().await;
        // One whose accepting task has ended (its runtime is gone) is bound
        // again.
        if let Some(dispatcher) = bound.get(&listen) {
            let live = dispatcher
                .accepting
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|task| !task.is_finished());
            if live {
                return Ok(dispatcher.clone());
            }
        }
        bound.remove(&listen);
        let listener = tokio::net::TcpListener::bind(listen).await?;
        let dispatcher = Arc::new(Dispatcher {
            local: listener.local_addr()?,
            state: Mutex::default(),
            accepting: Mutex::new(None),
            bundle: Mutex::new(None),
        });
        tracing::info!(listen = %dispatcher.local, "sandbox listener");
        let accepting = Arc::downgrade(&dispatcher);
        let task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let Some(dispatcher) = accepting.upgrade() else {
                            return;
                        };
                        tokio::spawn(async move {
                            let mut first = [0u8; 1];
                            let peeked = tokio::time::timeout(
                                Duration::from_secs(10),
                                stream.peek(&mut first),
                            )
                            .await;
                            match peeked {
                                Ok(Ok(1)) if first[0] == TLS_HANDSHAKE => {
                                    dispatcher.arrive(peer.ip(), stream)
                                }
                                Ok(Ok(1)) => {
                                    let bundle = dispatcher.bundle.lock().unwrap().clone();
                                    serve_bundle(stream, bundle).await
                                }
                                _ => {}
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "sandbox listener");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        *dispatcher.accepting.lock().unwrap() = Some(task);
        bound.insert(listen, dispatcher.clone());
        Ok(dispatcher)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Hand `bundle` to the init containers that ask.
    pub fn serve(&self, bundle: Arc<ServedBundle>) {
        *self.bundle.lock().unwrap() = Some(bundle);
    }

    /// A connection from `peer`: to the run expected there, else held a
    /// while for one that is about to be.
    fn arrive(&self, peer: IpAddr, stream: TcpStream) {
        let peer = peer.to_canonical();
        let mut state = self.state.lock().unwrap();
        let stream = match state.waiting.get(&peer) {
            Some(waiting) => match waiting.send(stream) {
                Ok(()) => return,
                Err(returned) => {
                    state.waiting.remove(&peer);
                    returned.0
                }
            },
            None => stream,
        };
        state.held.retain(|(_, at, _)| at.elapsed() < HOLD);
        if state.held.len() >= HOLD_MAX {
            tracing::warn!(%peer, "closed a connection to the sandbox listener: too many held");
            return;
        }
        tracing::debug!(%peer, "holding a connection no sandbox is expected at yet");
        state.held.push((peer, Instant::now(), stream));
    }

    /// Expect a sandbox at `addresses`: every connection from one of them,
    /// held or arriving, until the expectation is dropped.
    pub fn expect(self: &Arc<Self>, addresses: &[IpAddr]) -> Expected {
        let (send, receive) = tokio::sync::mpsc::unbounded_channel();
        let addresses: Vec<IpAddr> = addresses.iter().map(IpAddr::to_canonical).collect();
        let mut state = self.state.lock().unwrap();
        state.held.retain(|(_, at, _)| at.elapsed() < HOLD);
        let held = std::mem::take(&mut state.held);
        for (peer, at, stream) in held {
            match addresses.contains(&peer) {
                true => {
                    let _ = send.send(stream);
                }
                false => state.held.push((peer, at, stream)),
            }
        }
        for address in &addresses {
            state.waiting.insert(*address, send.clone());
        }
        Expected {
            dispatcher: self.clone(),
            addresses,
            receive,
        }
    }
}

/// Connections from one sandbox's addresses.
pub struct Expected {
    dispatcher: Arc<Dispatcher>,
    addresses: Vec<IpAddr>,
    receive: tokio::sync::mpsc::UnboundedReceiver<TcpStream>,
}

impl Expected {
    pub async fn next(&mut self) -> Option<TcpStream> {
        self.receive.recv().await
    }
}

/// Keep expecting the pod at its addresses once it has connected: its
/// supervisor redials after a dropped connection, from the same pod, with
/// its run's resume secret, which nothing from any other address can
/// present. Until the run ends.
fn take_back(server: SandboxServer, mut expected: Expected, conn: &SandboxConn) {
    if !conn.is_resumable() {
        return;
    }
    let run = conn.run().to_string();
    let ended = conn.ended();
    tokio::spawn(async move {
        tokio::pin!(ended);
        loop {
            let stream = tokio::select! {
                stream = expected.next() => match stream {
                    Some(stream) => stream,
                    None => return,
                },
                _ = &mut ended => return,
            };
            let accepted = tokio::select! {
                accepted = server.accept_for(stream, &run) => accepted,
                _ = &mut ended => return,
            };
            if let Err(error) = accepted {
                tracing::warn!(%run, %error, "refused a connection from a sandbox's address");
            }
        }
    });
}

impl Drop for Expected {
    fn drop(&mut self) {
        let mut state = self.dispatcher.state.lock().unwrap();
        for address in &self.addresses {
            state.waiting.remove(address);
        }
    }
}

/// A pod made and not yet connected: what its start is counted as.
struct Pending {
    kind: &'static str,
    sizing: Sizing,
    begun: Instant,
}

pub struct KubeSandboxProvider {
    client: kube::Client,
    options: KubeSandboxOptions,
    pending: Mutex<HashMap<String, Pending>>,
}

/// How each of the runner's shapes is sized: as the harness pod it stands
/// in for.
fn sizing(shape: Shape) -> Sizing {
    match shape {
        Shape::Session => Sizing::Session,
        Shape::Command => Sizing::Command,
        Shape::Probe => Sizing::Probe,
    }
}

fn failed(what: impl std::fmt::Display) -> ProviderError {
    ProviderError::Failed(what.to_string())
}

fn refused(error: RunnerError) -> ProviderError {
    match error {
        RunnerError::Refused { reason, message } => ProviderError::Refused { reason, message },
        other => failed(other),
    }
}

/// Why the scheduler cannot place a pod for lack of room on the node's
/// machine: affinity binds it there, so it would wait for room rather than
/// be refused, as a pod bound by `nodeName` was.
fn unschedulable(status: &PodStatus) -> Option<(Refusal, String)> {
    let condition = status.conditions.as_deref()?.iter().find(|c| {
        c.type_ == "PodScheduled"
            && c.status == "False"
            && c.reason.as_deref() == Some("Unschedulable")
    })?;
    let message = condition.message.clone().unwrap_or_default();
    let refusal = if message.contains("Insufficient cpu") {
        Refusal::OutOfCpu
    } else if message.contains("Insufficient memory") {
        Refusal::OutOfMemory
    } else if message.contains("Too many pods") {
        Refusal::OutOfPods
    } else if message.contains("Insufficient ephemeral-storage") {
        Refusal::OutOfEphemeralStorage
    } else {
        return None;
    };
    Some((refusal, message))
}

impl KubeSandboxProvider {
    pub fn new(client: kube::Client, options: KubeSandboxOptions) -> Self {
        KubeSandboxProvider {
            client,
            options,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn options(&self) -> &KubeSandboxOptions {
        &self.options
    }

    pub fn client(&self) -> kube::Client {
        self.client.clone()
    }

    pub fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.options.spec.env.namespace)
    }

    pub fn policies(&self) -> Api<NetworkPolicy> {
        Api::namespaced(self.client.clone(), &self.options.spec.env.namespace)
    }

    /// The listener every sandbox dials, bound on first use.
    pub async fn dispatcher(&self) -> Result<Arc<Dispatcher>, ProviderError> {
        Dispatcher::bind(self.options.listen).await.map_err(|e| {
            failed(format!(
                "listen for sandboxes on {}: {e}",
                self.options.listen
            ))
        })
    }

    /// The node's bundle, packed and served on the listener.
    pub async fn bundle(
        &self,
        dispatcher: &Dispatcher,
    ) -> Result<Arc<ServedBundle>, ProviderError> {
        let bundle = served_bundle(&self.options.bundle, &self.options.served_dir).await?;
        dispatcher.serve(bundle.clone());
        Ok(bundle)
    }

    /// Where a sandbox dials: the node pod's address, the listener's port.
    pub fn advertise(&self, dispatcher: &Dispatcher) -> String {
        let ip = &self.options.spec.env.pod_ip;
        let port = dispatcher.local_addr().port();
        match ip.parse::<IpAddr>() {
            Ok(IpAddr::V6(_)) => format!("[{ip}]:{port}"),
            _ => format!("{ip}:{port}"),
        }
    }

    /// The pod for one sandbox. `gated` adds a scheduling gate, so the pod is
    /// admitted (and mutated by whatever admission the cluster runs) but never
    /// scheduled: the boundary checks read it back and delete it. `dial` is
    /// where the supervisor meets the node, and where the init container
    /// fetches the bundle whose tar has digest `bundle_sha256`.
    pub fn render(
        &self,
        name: &str,
        spec: &CreateSpec<'_>,
        dial: &str,
        bundle_sha256: &str,
        gated: bool,
    ) -> Result<Pod, ProviderError> {
        let k = &self.options.spec;
        let sizing = sizing(spec.shape);
        let mut labels = pod_labels(name, spec.labels, sizing);
        labels.insert(ROLE_LABEL.into(), SANDBOX_ROLE.into());
        let image = match spec.image.is_empty() {
            true => k.image.clone(),
            false => spec.image.to_string(),
        };
        let proxy = format!("http://{}:{}", k.gateway_host, k.proxy_port);
        let no_proxy = crate::runner::no_proxy(&k.gateway_host);
        let mut env: Vec<(&str, String)> = vec![
            ("HOME", k.home.clone()),
            ("HTTPS_PROXY", proxy.clone()),
            ("HTTP_PROXY", proxy),
            ("NO_PROXY", no_proxy.clone()),
            ("no_proxy", no_proxy),
            ("TMPDIR", SCRATCH_MOUNT.to_string()),
        ];
        if !k.state_env.is_empty() {
            env.push((k.state_env, format!("{}/{}", k.home, k.state_dir)));
        }
        env.extend([
            ("TRACON_SANDBOX_ADDR", dial.to_string()),
            ("TRACON_SANDBOX_PIN", spec.bootstrap.cert_pin.clone()),
            // Not a Secret: see the module's notes on what keeps it spent.
            ("TRACON_SANDBOX_TOKEN", spec.bootstrap.token.clone()),
            (
                "TRACON_SANDBOX_CONNECT_TIMEOUT_MS",
                self.options.connect_timeout.as_millis().to_string(),
            ),
        ]);
        let env: Vec<EnvVar> = env
            .into_iter()
            .map(|(name, value)| EnvVar {
                name: name.into(),
                value: Some(value),
                ..Default::default()
            })
            .collect();

        let empty = |name: &str| Volume {
            name: name.into(),
            empty_dir: Some(EmptyDirVolumeSource::default()),
            ..Default::default()
        };
        let mount = |name: &str, path: &str, read_only: Option<bool>| VolumeMount {
            name: name.into(),
            mount_path: path.into(),
            read_only,
            ..Default::default()
        };
        let mut volumes = vec![empty(BUNDLE_VOLUME), empty(HOME_VOLUME)];
        let mut mounts = vec![
            mount(BUNDLE_VOLUME, BUNDLE_TARGET, Some(true)),
            mount(HOME_VOLUME, &k.home, None),
        ];
        if spec.shape != Shape::Probe {
            volumes.push(Volume {
                name: SHM_VOLUME.into(),
                empty_dir: Some(EmptyDirVolumeSource {
                    medium: Some("Memory".into()),
                    size_limit: Some(Quantity(k.shm_size.clone())),
                }),
                ..Default::default()
            });
            mounts.push(mount(SHM_VOLUME, "/dev/shm", None));
        }
        volumes.push(empty(crate::runner::kube::SCRATCH_VOLUME));
        mounts.push(mount(
            crate::runner::kube::SCRATCH_VOLUME,
            SCRATCH_MOUNT,
            None,
        ));
        // Every directory the run names is storage of the pod's own: filled
        // over the files channel, or empty where nothing is sent.
        let mut targets: Vec<&str> = Vec::new();
        for m in spec.mounts {
            let target = m.target.as_str();
            if !target.starts_with('/')
                || target.split('/').any(|part| part == "..")
                || target == k.home
                || target == SCRATCH_MOUNT
                || target == BUNDLE_TARGET
                || targets.contains(&target)
            {
                continue;
            }
            targets.push(target);
        }
        for (i, target) in targets.iter().enumerate() {
            let name = format!("files-{i}");
            volumes.push(empty(&name));
            mounts.push(mount(&name, target, None));
        }

        let requirements = KubeSpec::requirements(&sizing.sized(&k.resources));
        let gate = SecurityContext {
            privileged: Some(false),
            allow_privilege_escalation: Some(false),
            capabilities: Some(PodCapabilities {
                drop: Some(vec!["ALL".into()]),
                add: None,
            }),
            run_as_non_root: Some(true),
            ..Default::default()
        };
        let node_selector = LabelSelector {
            match_labels: Some(BTreeMap::from([(ROLE_LABEL.into(), "node".into())])),
            ..Default::default()
        };
        Ok(Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some(k.env.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".into()),
                termination_grace_period_seconds: Some(i64::from(k.stop_timeout_secs)),
                automount_service_account_token: Some(false),
                enable_service_links: Some(false),
                // No resolver: nothing to look up. The gateway's name is the
                // pod's own loopback, where the tunnels listen.
                dns_policy: Some("None".into()),
                dns_config: Some(PodDNSConfig {
                    nameservers: Some(vec!["127.0.0.1".into()]),
                    ..Default::default()
                }),
                host_aliases: Some(vec![HostAlias {
                    ip: "127.0.0.1".into(),
                    hostnames: Some(vec![k.gateway_host.clone()]),
                }]),
                // Beside the node pod, on its machine, through the scheduler
                // rather than around it.
                affinity: Some(Affinity {
                    pod_affinity: Some(PodAffinity {
                        required_during_scheduling_ignored_during_execution: Some(vec![
                            PodAffinityTerm {
                                label_selector: Some(node_selector),
                                namespaces: Some(vec![self.options.node_namespace.clone()]),
                                topology_key: "kubernetes.io/hostname".into(),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                scheduling_gates: gated.then(|| {
                    vec![PodSchedulingGate {
                        name: PROBE_GATE.into(),
                    }]
                }),
                security_context: Some(PodSecurityContext {
                    run_as_non_root: Some(true),
                    run_as_user: Some(k.uid),
                    run_as_group: Some(k.uid),
                    fs_group: Some(k.uid),
                    seccomp_profile: Some(SeccompProfile {
                        type_: "RuntimeDefault".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                volumes: Some(volumes),
                image_pull_secrets: k.pull_secrets(),
                init_containers: Some(vec![Container {
                    name: BUNDLE_CONTAINER.into(),
                    // The sandbox's own image: nothing else is pulled for it.
                    image: Some(image.clone()),
                    image_pull_policy: Some("IfNotPresent".into()),
                    command: Some(vec!["sh".into(), "-c".into(), FETCH_BUNDLE.into()]),
                    env: Some(vec![
                        EnvVar {
                            name: "TRACON_BUNDLE_URL".into(),
                            value: Some(format!("http://{dial}{BUNDLE_PATH}")),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "TRACON_BUNDLE_SHA256".into(),
                            value: Some(bundle_sha256.into()),
                            ..Default::default()
                        },
                    ]),
                    volume_mounts: Some(vec![mount(BUNDLE_VOLUME, BUNDLE_TARGET, None)]),
                    // As much as the sandbox asks: a pod's request is the
                    // larger of the two, so this adds nothing to it.
                    resources: Some(requirements.clone()),
                    security_context: Some(gate.clone()),
                    ..Default::default()
                }]),
                containers: vec![Container {
                    name: HARNESS_CONTAINER.into(),
                    image: Some(image),
                    image_pull_policy: Some("IfNotPresent".into()),
                    command: Some(vec![SUPERVISOR.into()]),
                    env: Some(env),
                    volume_mounts: Some(mounts),
                    resources: Some(requirements),
                    security_context: Some(gate),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    async fn logs(&self, name: &str) -> String {
        match self
            .pods()
            .logs(
                name,
                &LogParams {
                    container: Some(HARNESS_CONTAINER.into()),
                    tail_lines: Some(20),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
            Ok(_) => "it wrote nothing".into(),
            Err(error) => format!("its log could not be read: {error}"),
        }
    }

    fn take_pending(&self, name: &str) -> Pending {
        self.pending
            .lock()
            .unwrap()
            .remove(name)
            .unwrap_or(Pending {
                kind: "session",
                sizing: Sizing::Session,
                begun: Instant::now(),
            })
    }
}

/// How a pod on its way up stands, read from its status.
enum Progress {
    Waiting,
    Started,
    /// Ended before it connected.
    Ended(String),
    Failed(&'static str, ProviderError),
}

fn progress(name: &str, status: &PodStatus, sizing: Sizing, k: &KubeSpec) -> Progress {
    if let Some(refused) = refusal(status) {
        return Progress::Failed(
            refused.as_str(),
            self::refused(refusal_error(
                name,
                refused,
                sizing,
                &k.resources,
                status.message.as_deref().unwrap_or_default(),
            )),
        );
    }
    if let Some((refused, said)) = unschedulable(status) {
        return Progress::Failed(
            refused.as_str(),
            self::refused(refusal_error(name, refused, sizing, &k.resources, &said)),
        );
    }
    let waiting = |statuses: Option<&Vec<k8s_openapi::api::core::v1::ContainerStatus>>| {
        statuses
            .into_iter()
            .flatten()
            .filter_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.as_deref())
            .find_map(|reason| match reason {
                "ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" => {
                    Some(("image_pull", reason.to_string()))
                }
                "CreateContainerConfigError" | "CreateContainerError" => {
                    Some(("config_error", reason.to_string()))
                }
                _ => None,
            })
    };
    if let Some((counted, reason)) = waiting(status.init_container_statuses.as_ref())
        .or_else(|| waiting(status.container_statuses.as_ref()))
    {
        return Progress::Failed(counted, failed(format!("pod {name}: {reason}")));
    }
    // The bundle's copy failing leaves the pod pending with its init
    // container terminated non-zero.
    if let Some(code) = status
        .init_container_statuses
        .iter()
        .flatten()
        .filter_map(|c| c.state.as_ref()?.terminated.as_ref())
        .map(|t| t.exit_code)
        .find(|code| *code != 0)
    {
        return Progress::Failed(
            "bundle",
            failed(format!(
                "pod {name}: copying the harness bundle in failed ({code}); is \
                 its image able to fetch it (sh, curl or wget, sha256sum and tar)?"
            )),
        );
    }
    match status.phase.as_deref().unwrap_or_default() {
        "Running" => Progress::Started,
        "Succeeded" | "Failed" => Progress::Ended(
            [status.reason.as_deref(), status.message.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": "),
        ),
        _ => Progress::Waiting,
    }
}

/// The addresses a pod's status gives it.
fn pod_ips(status: &PodStatus) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = status
        .pod_ips
        .iter()
        .flatten()
        .filter_map(|p| p.ip.parse().ok())
        .collect();
    if let Some(ip) = status.pod_ip.as_deref().and_then(|ip| ip.parse().ok()) {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips
}

async fn next(expected: &mut Option<Expected>) -> Option<TcpStream> {
    match expected {
        Some(expected) => expected.next().await,
        None => std::future::pending().await,
    }
}

/// What the admitted pod shows of the boundary, as the checks read it: the
/// gate on every container, no host namespace or API token, nothing mounted
/// but the pod's own emptyDirs, no secret named, no resolver, the gateway's
/// name on loopback, and the bundle from the bundle image.
pub fn breaches(pod: &Pod, gateway_host: &str) -> Vec<String> {
    let mut found = Vec::new();
    let Some(spec) = &pod.spec else {
        return vec!["the pod has no spec".into()];
    };
    let psc = spec.security_context.clone().unwrap_or_default();
    if psc.run_as_non_root != Some(true) || psc.run_as_user.unwrap_or(0) == 0 {
        found.push("the pod may run as root".into());
    }
    if psc.seccomp_profile.as_ref().map(|s| s.type_.as_str()) != Some("RuntimeDefault") {
        found.push("the pod has no seccomp profile".into());
    }
    if spec.host_network.unwrap_or(false)
        || spec.host_pid.unwrap_or(false)
        || spec.host_ipc.unwrap_or(false)
    {
        found.push("the pod shares a host namespace".into());
    }
    if spec.automount_service_account_token != Some(false) {
        found.push("an API token is mounted".into());
    }
    if spec.share_process_namespace.unwrap_or(false) {
        found.push("the pod shares its process namespace".into());
    }
    for v in spec.volumes.iter().flatten() {
        if v.empty_dir.is_none()
            || v.host_path.is_some()
            || v.persistent_volume_claim.is_some()
            || v.secret.is_some()
            || v.projected.is_some()
            || v.config_map.is_some()
        {
            found.push(format!("volume {} is not an emptyDir of its own", v.name));
        }
    }
    let containers = spec
        .containers
        .iter()
        .chain(spec.init_containers.iter().flatten());
    for c in containers {
        let sc = c.security_context.clone().unwrap_or_default();
        if sc.privileged.unwrap_or(false) {
            found.push(format!("{} is privileged", c.name));
        }
        if sc.allow_privilege_escalation != Some(false) {
            found.push(format!("{} may escalate privileges", c.name));
        }
        let caps = sc.capabilities.unwrap_or_default();
        if caps.drop.as_deref().map(|d| d.iter().any(|x| x == "ALL")) != Some(true) {
            found.push(format!("{} keeps capabilities", c.name));
        }
        if caps.add.as_deref().is_some_and(|a| !a.is_empty()) {
            found.push(format!("{} adds capabilities", c.name));
        }
        for m in c.volume_mounts.iter().flatten() {
            if m.sub_path.is_some() || m.sub_path_expr.is_some() {
                found.push(format!("{} mounts a subPath at {}", c.name, m.mount_path));
            }
        }
        for e in c.env.iter().flatten() {
            if e.value_from.is_some() {
                found.push(format!("{} takes {} from elsewhere", c.name, e.name));
            }
        }
        if c.env_from.as_ref().is_some_and(|e| !e.is_empty()) {
            found.push(format!("{} takes its environment from elsewhere", c.name));
        }
    }
    match spec
        .init_containers
        .iter()
        .flatten()
        .find(|c| c.name == BUNDLE_CONTAINER)
    {
        Some(c)
            if c.image.is_some()
                && spec
                    .containers
                    .iter()
                    .any(|h| h.name == HARNESS_CONTAINER && h.image == c.image) => {}
        Some(_) => found.push("the bundle is fetched in another image than the sandbox's".into()),
        None => found.push("no init container copies the bundle in".into()),
    }
    match spec.containers.iter().find(|c| c.name == HARNESS_CONTAINER) {
        Some(c) => {
            let bundle = c
                .volume_mounts
                .iter()
                .flatten()
                .find(|m| m.mount_path == BUNDLE_TARGET);
            if bundle.is_none_or(|m| m.read_only != Some(true) || m.name != BUNDLE_VOLUME) {
                found.push(format!(
                    "the bundle is not mounted read-only at {BUNDLE_TARGET}"
                ));
            }
        }
        None => found.push("the pod runs no sandbox".into()),
    }
    if spec.dns_policy.as_deref() != Some("None")
        || spec
            .dns_config
            .as_ref()
            .and_then(|d| d.nameservers.as_ref())
            .is_some_and(|n| n.iter().any(|ns| !loopback(ns)))
    {
        found.push("the pod has a resolver".into());
    }
    for alias in spec.host_aliases.iter().flatten() {
        if !loopback(&alias.ip) {
            found.push(format!("a name resolves to {}", alias.ip));
        }
    }
    let gateway_local = spec
        .host_aliases
        .iter()
        .flatten()
        .any(|a| loopback(&a.ip) && a.hostnames.iter().flatten().any(|h| h == gateway_host));
    if !gateway_local {
        found.push(format!("{gateway_host} is not the pod's loopback"));
    }
    found
}

fn loopback(ip: &str) -> bool {
    ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[async_trait]
impl SandboxProvider for KubeSandboxProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Kubernetes
    }

    async fn create(&self, spec: &CreateSpec<'_>) -> Result<Handle, ProviderError> {
        let name = match spec.name {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => crate::config::resource_name(&format!(
                "s-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            )),
        };
        let sizing = sizing(spec.shape);
        let kind = run_kind(spec.labels.kind, sizing.as_str());
        let dispatcher = self.dispatcher().await?;
        let bundle = self.bundle(&dispatcher).await?;
        let pod = self.render(
            &name,
            spec,
            &self.advertise(&dispatcher),
            &bundle.sha256,
            false,
        )?;
        let pods = self.pods();
        // Left by an earlier run of the same name.
        let _ = pods
            .delete(&name, &DeleteParams::default().grace_period(0))
            .await;
        let begun = Instant::now();
        if let Err(error) = pods.create(&PostParams::default(), &pod).await {
            let error = match &error {
                kube::Error::Api(status) if quota_refused(status.code, &status.message) => {
                    refused(refusal_error(
                        &name,
                        Refusal::Quota,
                        sizing,
                        &self.options.spec.resources,
                        &status.message,
                    ))
                }
                _ => failed(format!("create pod {name}: {error}")),
            };
            match &error {
                ProviderError::Refused { reason, .. } => {
                    telemetry().pod_refused(kind, sizing.as_str(), reason)
                }
                _ => telemetry().pod_start_failed(kind, sizing.as_str(), "create_error"),
            }
            return Err(error);
        }
        self.pending.lock().unwrap().insert(
            name.clone(),
            Pending {
                kind,
                sizing,
                begun,
            },
        );
        Ok(Handle(name))
    }

    /// Wait for the pod to start (bounded by `start_timeout_secs`, since a
    /// pull can take minutes), expecting its supervisor at the pod's own
    /// address from the moment the pod has one; then for its connection
    /// (bounded by `within`). A connection that fails its handshake is not
    /// the supervisor's, and the next is waited for.
    async fn connect(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        within: Duration,
    ) -> Result<SandboxConn, ProviderError> {
        let name = handle.0.as_str();
        let pending = self.take_pending(name);
        let (kind, sizing_label) = (pending.kind, pending.sizing.as_str());
        let fail = |reason: &'static str, error: ProviderError| {
            match &error {
                ProviderError::Refused { reason, .. } => {
                    telemetry().pod_refused(kind, sizing_label, reason)
                }
                _ => telemetry().pod_start_failed(kind, sizing_label, reason),
            }
            error
        };
        let dispatcher = self.dispatcher().await?;
        let pods = self.pods();
        let start_deadline = tokio::time::Instant::now() + self.options.spec.start_timeout;
        let mut connect_deadline: Option<tokio::time::Instant> = None;
        let mut expected: Option<Expected> = None;
        let mut failures = 0;
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                Some(stream) = next(&mut expected) => {
                    match server.accept(stream).await {
                        Ok(conn) => {
                            if connect_deadline.is_none() {
                                telemetry().pod_started(kind, sizing_label, pending.begun.elapsed());
                            }
                            if let Some(expected) = expected.take() {
                                take_back(server.clone(), expected, &conn);
                            }
                            return Ok(conn);
                        }
                        Err(
                            error @ (SandboxError::BadToken
                            | SandboxError::Handshake(_)
                            | SandboxError::Tls(_)
                            | SandboxError::Timeout
                            | SandboxError::Io(_)),
                        ) => {
                            tracing::warn!(sandbox = %name, %error, "refused a connection from a sandbox's address");
                        }
                        Err(error) => return Err(failed(format!("{name}: {error}"))),
                    }
                }
                _ = tick.tick() => {
                    let pod = match pods.get(name).await {
                        Ok(pod) => {
                            failures = 0;
                            pod
                        }
                        Err(kube::Error::Api(status)) if status.is_not_found() => {
                            return Err(fail("other", failed(format!("pod {name} was removed before it connected"))));
                        }
                        Err(error) => {
                            failures += 1;
                            if failures >= READ_FAILURES {
                                return Err(fail("other", failed(format!("get pod {name}: {error}"))));
                            }
                            continue;
                        }
                    };
                    let status = pod.status.unwrap_or_default();
                    if expected.is_none() {
                        let ips = pod_ips(&status);
                        if !ips.is_empty() {
                            expected = Some(dispatcher.expect(&ips));
                        }
                    }
                    match progress(name, &status, pending.sizing, &self.options.spec) {
                        Progress::Failed(reason, error) => return Err(fail(reason, error)),
                        Progress::Ended(why) => {
                            return Err(failed(format!(
                                "{name} stopped before connecting ({why}): {}",
                                self.logs(name).await
                            )));
                        }
                        Progress::Started if connect_deadline.is_none() => {
                            telemetry().pod_started(kind, sizing_label, pending.begun.elapsed());
                            connect_deadline = Some(tokio::time::Instant::now() + within);
                        }
                        Progress::Started | Progress::Waiting => {}
                    }
                    let now = tokio::time::Instant::now();
                    if connect_deadline.is_none() && now >= start_deadline {
                        return Err(fail("start_timeout", failed(format!(
                            "pod {name} did not start in {}s: {}; [runtime.kubernetes] \
                             start_timeout_secs sets how long a start may take",
                            self.options.spec.start_timeout.as_secs(),
                            pending_reason(&status)
                        ))));
                    }
                    if connect_deadline.is_some_and(|deadline| now >= deadline) {
                        return Err(failed(format!(
                            "{name} did not connect within {} s: {}",
                            within.as_secs(),
                            self.logs(name).await
                        )));
                    }
                }
            }
        }
    }

    /// Expect the pod at its addresses again, as `connect` did once it had
    /// connected: its resume secret is taken from there alone.
    async fn reattach(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        conn: &SandboxConn,
    ) -> Result<(), ProviderError> {
        let pod = self
            .pods()
            .get_opt(&handle.0)
            .await
            .map_err(|e| failed(format!("get pod {}: {e}", handle.0)))?
            .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
        let ips = pod_ips(&pod.status.unwrap_or_default());
        if ips.is_empty() {
            return Err(failed(format!("pod {} has no address", handle.0)));
        }
        let expected = self.dispatcher().await?.expect(&ips);
        take_back(server.clone(), expected, conn);
        Ok(())
    }

    async fn destroy(&self, handle: &Handle) -> Result<(), ProviderError> {
        self.pending.lock().unwrap().remove(&handle.0);
        match self
            .pods()
            .delete(&handle.0, &DeleteParams::default().grace_period(0))
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.is_not_found() => Ok(()),
            Err(error) => Err(failed(format!("delete pod {}: {error}", handle.0))),
        }
    }

    async fn status(&self, handle: &Handle) -> Result<Status, ProviderError> {
        let pod = match self.pods().get_opt(&handle.0).await {
            Ok(Some(pod)) => pod,
            Ok(None) => return Ok(Status::Lost),
            Err(error) => return Err(failed(format!("get pod {}: {error}", handle.0))),
        };
        let status = pod.status.unwrap_or_default();
        Ok(match status.phase.as_deref().unwrap_or_default() {
            "Pending" | "" => Status::Starting,
            "Running" => Status::Running,
            "Succeeded" | "Failed" => Status::Exited {
                code: harness_status(&status)
                    .and_then(|c| c.state.as_ref())
                    .and_then(|s| s.terminated.as_ref())
                    .map(|t| t.exit_code)
                    .unwrap_or(-1),
            },
            _ => Status::Lost,
        })
    }

    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError> {
        let pod = self
            .pods()
            .get_opt(&handle.0)
            .await
            .map_err(|e| failed(format!("get pod {}: {e}", handle.0)))?
            .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
        let id = pod
            .status
            .as_ref()
            .and_then(harness_status)
            .and_then(|c| normalize_image_id(&c.image_id))
            .ok_or_else(|| failed(format!("{} reports no image digest yet", handle.0)))?;
        let digest = id
            .split_once('@')
            .map(|(_, digest)| digest.to_string())
            .unwrap_or(id);
        Ok(ImageIdentity::Exact { digest })
    }

    async fn network(&self, handle: &Handle) -> Result<Network, ProviderError> {
        let pod = self
            .pods()
            .get_opt(&handle.0)
            .await
            .map_err(|e| failed(format!("get pod {}: {e}", handle.0)))?
            .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
        let dns = match pod.spec.as_ref().and_then(|s| s.dns_policy.as_deref()) {
            Some("None") => Dns::None,
            _ => Dns::ProviderOpen,
        };
        let dispatcher = self.dispatcher().await?;
        Ok(Network {
            destinations: vec![format!("tcp:{}", self.advertise(&dispatcher))],
            dns,
        })
    }

    async fn verify_isolation(&self, handle: &Handle) -> Result<Capabilities, ProviderError> {
        let pod = self
            .pods()
            .get_opt(&handle.0)
            .await
            .map_err(|e| failed(format!("get pod {}: {e}", handle.0)))?
            .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
        let labels = pod.metadata.labels.clone().unwrap_or_default();
        if labels.get(ROLE_LABEL).map(String::as_str) != Some(SANDBOX_ROLE)
            || labels.get(SESSION_LABEL) != Some(&handle.0)
        {
            return Err(failed(format!(
                "{} was not made by this provider",
                handle.0
            )));
        }
        let found = breaches(&pod, &self.options.spec.gateway_host);
        if !found.is_empty() {
            return Err(failed(found.join("; ")));
        }
        let port = i32::from(self.dispatcher().await?.local_addr().port());
        let route = match self.policies().get_opt(&sandbox_policy()).await {
            Ok(Some(policy)) => {
                match crate::boundary::kubernetes::checks::sandbox_policy_shape(&policy, port) {
                    Ok(()) => Route::NodeOnly,
                    Err(why) => {
                        tracing::warn!(%why, "the sandbox policy does not hold");
                        Route::Unverified
                    }
                }
            }
            _ => Route::Unverified,
        };
        Ok(Capabilities {
            route,
            ..self.capabilities()
        })
    }

    /// What the checks establish: a pod with no privileges, no resolver and
    /// one route, to the node pod's sandbox port.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tier: Tier::Container,
            route: Route::NodeOnly,
            dns: Dns::None,
            privilege: Privilege::Unprivileged,
            runtime_socket: RuntimeSocket::Absent,
            image_identity: ImageIdentityKind::Exact,
        }
    }

    async fn capacity(&self) -> Result<Capacity, ProviderError> {
        let listed = self
            .pods()
            .list(&ListParams::default().labels(&format!("{ROLE_LABEL}={SANDBOX_ROLE}")))
            .await
            .map_err(|e| failed(format!("list pods: {e}")))?;
        let own = format!("{}-", crate::config::resource_prefix());
        let running = listed
            .items
            .iter()
            .filter(|p| {
                p.metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| n.starts_with(&own))
            })
            .filter(|p| {
                !matches!(
                    p.status.as_ref().and_then(|s| s.phase.as_deref()),
                    Some("Succeeded" | "Failed")
                )
            })
            .count() as u32;
        Ok(Capacity {
            running,
            limit: None,
        })
    }
}

#[cfg(test)]
mod tests;
