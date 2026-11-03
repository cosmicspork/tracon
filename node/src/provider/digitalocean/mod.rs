//! DigitalOcean's Harness Runtime as a sandbox provider: the elastic one,
//! which any node can turn on (`[placement] digitalocean`) and which runs
//! each sandbox as a microVM of DigitalOcean's.
//!
//! - **Create** starts a bare session (`harness: none`) from the node's
//!   template ([`templates`]) with `keep_warm` (the runtime would pause it
//!   15 minutes after its last exec otherwise) and an egress allowlist of
//!   exactly the node's address: the runtime's proxy then lets the sandbox
//!   reach that one IP and nothing by name. Then it delivers the run's
//!   bootstrap by one `exec` ([`bootstrap`]), so nothing of the run is in
//!   the manifest DigitalOcean stores.
//! - **Connect**: `tracon-sandbox`, a child of the runtime's own s6 init,
//!   dials the node's address on 443 with TLS pinned to the node, directly
//!   or through the hub relay (SNI routing, TLS end to end), and HTTP/2
//!   pings every 20 seconds keep the runtime's proxy, which cuts a
//!   connection after an hour of silence, from ever seeing one. The node's
//!   one [`listener::DoListener`] hands each connection to the run whose
//!   token it proved.
//! - **Destroy** removes the session and the agent config its create left,
//!   which removing the session does not.
//! - **Isolation** is read back from DigitalOcean, never from inside: the
//!   session must be a bare one and its stored manifest's egress exactly the
//!   node. DNS is DigitalOcean's resolver, open to any name
//!   (`dns: provider-open`), which is what keeps client work off it.
//!
//! Nothing on this provider is mounted from the node: a session's trees go
//! over the files channel and its gateways through tunnels, as
//! `[runtime.podman] sandbox_files` and `sandbox_tunnels` do locally.

pub mod bootstrap;
pub mod images;
pub mod listener;
pub mod runtime;
pub mod templates;
pub mod upkeep;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracon_sandbox::node::{SandboxConn, SandboxServer};

use super::capabilities::{
    Capabilities, Dns, ImageIdentityKind, Privilege, Route, RuntimeSocket, Tier,
};
use super::{
    Bootstrap, Capacity, CreateSpec, Handle, ImageIdentity, Network, ProviderError, ProviderKind,
    Resources, SandboxProvider, SnapshotId, Status,
};
use crate::do_harness::{
    Client, DoError, Egress, ExecRequest, Manifest, Session, SessionFilter, SessionStatus,
};
use listener::DoListener;
use templates::Templates;

/// `[digitalocean]`: how this node reaches DigitalOcean's Harness Runtime.
/// Nothing runs there unless `[placement] digitalocean` is `overflow` or
/// `prefer` and `credential` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The broker credential holding `DIGITALOCEAN_ACCESS_TOKEN`.
    pub credential: String,
    /// The region's API host (`api-<region>.digitalocean.com`), which is
    /// where sessions are placed; empty for the default host.
    pub region: String,
    /// The API's base URL, for a stand-in; empty for DigitalOcean's.
    pub api_url: String,
    /// An image of the operator's that every session there runs in, by
    /// digest: one with the harness bundle at `/opt/tracon` and the
    /// `tracon-sandbox` service (`containers/do-sandbox`). Or leave it out
    /// and name `registry`.
    pub image: String,
    /// Where the node pushes the template sources it builds itself: each of
    /// its own sandbox images (the pinned base, a repository's prepared
    /// snapshot) with the bundle it verified. DigitalOcean's container
    /// registry takes the same token. Needs `[runtime.podman]
    /// sandbox_images`.
    pub registry: String,
    /// The runtime's base template the image is rebased onto.
    pub base_template: String,
    /// The sandbox size slug; empty for the runtime's default.
    pub size: String,
    /// The most sandboxes this node runs there at once.
    pub limit: u32,
    /// Where the node listens for its sandboxes, `ip:port`. What the public
    /// address forwards to (a load balancer's 443), or the hub relay's
    /// `[mesh.sandbox_relay] target`.
    pub listen: String,
    /// What sandboxes dial, `ip:port`: an address, because the runtime's
    /// egress allowlist admits addresses, never names. The node's public
    /// address, or the hub relay's.
    pub node_addr: String,
    /// The name sandboxes put in SNI. Empty: the hub relay's name for this
    /// node with `relay`, else `tracon-node`.
    pub server_name: String,
    /// Reach the node through the hub relay (`[mesh.sandbox_relay]`).
    pub relay: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            credential: String::new(),
            region: String::new(),
            api_url: String::new(),
            image: String::new(),
            registry: String::new(),
            base_template: "sandbox".into(),
            size: String::new(),
            limit: 4,
            listen: String::new(),
            node_addr: String::new(),
            server_name: String::new(),
            relay: false,
        }
    }
}

/// What a sandbox sends in SNI when nothing else names the node.
pub const DEFAULT_SERVER_NAME: &str = "tracon-node";

impl Settings {
    pub fn is_configured(&self) -> bool {
        !self.credential.trim().is_empty()
    }

    pub fn node_addr(&self) -> Result<SocketAddr, String> {
        self.node_addr.parse().map_err(|_| {
            format!(
                "digitalocean.node_addr {:?} is not ip:port; the runtime's egress admits addresses, not names",
                self.node_addr
            )
        })
    }

    pub fn listen_addr(&self) -> Result<SocketAddr, String> {
        self.listen
            .parse()
            .map_err(|_| format!("digitalocean.listen {:?} is not ip:port", self.listen))
    }

    pub fn client(&self, token: &str) -> Result<Client, DoError> {
        match (self.api_url.trim(), self.region.trim()) {
            ("", region) => Client::regional(region, token),
            (url, _) => Client::with_base_url(url, token),
        }
    }
}

/// Refuse a `[digitalocean]` block the node could not run sessions with.
/// One with no credential is not in use and is not checked.
pub fn validate(
    settings: &Settings,
    relay_enabled: bool,
    sandbox_images: bool,
) -> Result<(), String> {
    if !settings.is_configured() {
        return Ok(());
    }
    settings.node_addr()?;
    settings.listen_addr()?;
    match (settings.image.is_empty(), settings.registry.is_empty()) {
        (false, true) => {
            templates::digest_of(&settings.image)
                .map_err(|e| format!("digitalocean.image: {e}"))?;
        }
        (true, false) if !sandbox_images => {
            return Err(
                "digitalocean.registry builds templates from the node's own sandbox \
                        images, which needs [runtime.podman] sandbox_images"
                    .into(),
            )
        }
        (true, false) => {}
        _ => {
            return Err(
                "name exactly one of digitalocean.image (an image of your own) and \
                 digitalocean.registry (where the node pushes the ones it builds)"
                    .into(),
            )
        }
    }
    if settings.base_template.trim().is_empty() {
        return Err("digitalocean.base_template is empty".into());
    }
    if settings.limit == 0 {
        return Err("digitalocean.limit is 0: leave [digitalocean] out, or set [placement] digitalocean = \"off\"".into());
    }
    if !settings.server_name.is_empty()
        && !settings
            .server_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!(
            "digitalocean.server_name {:?} is not a host name",
            settings.server_name
        ));
    }
    if settings.relay && !relay_enabled {
        return Err("digitalocean.relay needs [mesh.sandbox_relay] enabled".into());
    }
    Ok(())
}

/// What the runtime promises of every sandbox, as the trials found it: a
/// VM of its own, egress limited to the node by the runtime's proxy, the
/// runtime's open resolver, root in the VM, no container runtime socket,
/// and an image the runtime rebuilt under a digest of its own.
pub fn capabilities() -> Capabilities {
    Capabilities {
        tier: Tier::Microvm,
        route: Route::NodeOnly,
        dns: Dns::ProviderOpen,
        privilege: Privilege::RootInVm,
        runtime_socket: RuntimeSocket::Absent,
        image_identity: ImageIdentityKind::Derived,
    }
}

/// A DigitalOcean client, made with the token as the broker holds it now.
pub type ClientSource = Arc<dyn Fn() -> Result<Client, String> + Send + Sync>;

/// The name the sandbox puts in SNI, read when each is made: the hub
/// relay's name for this node changes only when its stream is re-held.
pub type ServerName = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// One sandbox this provider made, while it is the node's.
struct Made {
    run: String,
    /// The local image it runs (its template's source).
    image: String,
    config_id: Option<String>,
    /// Taken by the first `connect`.
    arrival: Option<oneshot::Receiver<SandboxConn>>,
}

#[derive(Clone)]
pub struct Options {
    pub settings: Settings,
    /// Names every session and config this node makes there: `<prefix>-…`
    /// and ending in `-n<node>`, so another node on the same team (whose
    /// prefix may well be the same default) is never counted or cleaned.
    pub owner: Owner,
    /// The gateway's name, which the harness's configuration uses and the
    /// sandbox resolves to loopback, where the tunnels listen.
    pub gateway_host: String,
    /// How long a new session may take to become ready.
    pub ready_within: Duration,
    /// How often its status is read while it does.
    pub poll: Duration,
}

/// Which DigitalOcean names are this node's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub prefix: String,
    /// The first eight characters of the node's id.
    pub node: String,
}

impl Owner {
    pub fn new(prefix: &str, node_id: &str) -> Self {
        Owner {
            prefix: prefix.to_string(),
            node: node_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .take(8)
                .collect::<String>()
                .to_ascii_lowercase(),
        }
    }

    /// The session name for a sandbox the node calls `name`.
    pub fn session_name(&self, name: &str) -> String {
        format!("{name}-n{}", self.node)
    }

    /// The stem of this node's template names.
    pub fn template_stem(&self) -> String {
        format!("{}-t{}", self.prefix, self.node)
    }

    /// Whether a session or config named `name` is this node's.
    pub fn owns(&self, name: &str) -> bool {
        name.starts_with(&format!("{}-", self.prefix))
            && name.ends_with(&format!("-n{}", self.node))
    }
}

pub struct DoProvider {
    client: ClientSource,
    options: Options,
    listener: Arc<DoListener>,
    templates: Arc<Templates>,
    server_name: ServerName,
    made: Mutex<HashMap<String, Made>>,
}

fn failed(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::Failed(error.to_string())
}

/// What the runtime's status reads as: paused is kept, not lost; a status
/// this client does not know counts as running, as cleanup counts it live.
pub fn status_of(session: &Session) -> Status {
    match &session.status {
        SessionStatus::Provisioning => Status::Starting,
        SessionStatus::Ready | SessionStatus::Detached => Status::Running,
        SessionStatus::Paused => Status::Paused,
        SessionStatus::Destroying | SessionStatus::Destroyed | SessionStatus::Failed => {
            Status::Lost
        }
        SessionStatus::Other(_) => Status::Running,
    }
}

/// The egress a stored manifest gives, as addresses and hosts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StoredEgress {
    pub allow_ips: Vec<String>,
    pub allow_hosts: Vec<String>,
    /// Anything else that would widen it: a VPC, a subnet, an unrestricted
    /// or missing egress, a field this reader does not know.
    pub other: Vec<String>,
}

/// Read a stored manifest's egress back: what the sandbox can reach, as
/// the runtime holds it.
pub fn stored_egress(manifest: &serde_json::Value) -> StoredEgress {
    let mut out = StoredEgress::default();
    let strings = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    match manifest.get("egress") {
        None | Some(serde_json::Value::Null) => {
            out.other.push("no egress rule (unrestricted)".into())
        }
        Some(serde_json::Value::String(s)) => out.other.push(format!("egress {s:?}")),
        Some(serde_json::Value::Array(hosts)) => {
            out.allow_hosts = strings(&serde_json::Value::Array(hosts.clone()))
        }
        Some(serde_json::Value::Object(rules)) => {
            for (key, value) in rules {
                match key.as_str() {
                    "allow_ips" => out.allow_ips = strings(value),
                    "allow_hosts" => out.allow_hosts = strings(value),
                    _ if value.is_null() => {}
                    other => out.other.push(format!("egress.{other}")),
                }
            }
        }
        Some(other) => out.other.push(format!("egress {other}")),
    }
    out
}

impl DoProvider {
    pub fn new(
        client: ClientSource,
        options: Options,
        listener: Arc<DoListener>,
        templates: Arc<Templates>,
        server_name: ServerName,
    ) -> Self {
        DoProvider {
            client,
            options,
            listener,
            templates,
            server_name,
            made: Mutex::new(HashMap::new()),
        }
    }

    pub fn client(&self) -> Result<Client, ProviderError> {
        (self.client)().map_err(ProviderError::Failed)
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    pub fn templates(&self) -> &Arc<Templates> {
        &self.templates
    }

    pub fn listener(&self) -> &Arc<DoListener> {
        &self.listener
    }

    /// The node's address as the sandbox's egress names it.
    fn node_ip(&self) -> Result<String, ProviderError> {
        self.options
            .settings
            .node_addr()
            .map(|addr| addr.ip().to_string())
            .map_err(ProviderError::Failed)
    }

    fn server_name(&self) -> Result<String, ProviderError> {
        let settings = &self.options.settings;
        if !settings.server_name.is_empty() {
            return Ok(settings.server_name.clone());
        }
        match (settings.relay, (self.server_name)()) {
            (_, Some(name)) => Ok(name),
            (true, None) => Err(failed(
                "the hub relay is not holding this node's name yet, so a sandbox could not reach it",
            )),
            (false, None) => Ok(DEFAULT_SERVER_NAME.into()),
        }
    }

    /// The manifest of every sandbox: nothing of the run in it.
    pub fn manifest(&self, name: &str, template: &str) -> Result<Manifest, ProviderError> {
        let settings = &self.options.settings;
        Ok(Manifest {
            template: Some(template.to_string()),
            size: (!settings.size.is_empty()).then(|| settings.size.clone()),
            keep_warm: true,
            egress: Some(Egress::Rules {
                allow_hosts: Vec::new(),
                allow_ips: vec![self.node_ip()?],
                vpc_uuid: None,
                subnet_uuid: None,
            }),
            ..Manifest::bare(self.options.owner.session_name(name))
        })
    }

    /// Wait for a session to be ready; one that fails or goes is removed.
    async fn until_ready(&self, client: &Client, id: &str) -> Result<Session, ProviderError> {
        let deadline = tokio::time::Instant::now() + self.options.ready_within;
        loop {
            let session = client.get_session(id).await.map_err(failed)?;
            match status_of(&session) {
                Status::Running => return Ok(session),
                Status::Lost | Status::Exited { .. } => {
                    return Err(failed(format!(
                        "DigitalOcean session {id} is {} instead of ready",
                        session.status
                    )))
                }
                Status::Starting | Status::Paused => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failed(format!(
                    "DigitalOcean session {id} was not ready within {} s",
                    self.options.ready_within.as_secs()
                )));
            }
            tokio::time::sleep(self.options.poll).await;
        }
    }

    /// Tell a ready session's template service how to meet the node.
    async fn deliver(
        &self,
        client: &Client,
        id: &str,
        bootstrap: &Bootstrap,
    ) -> Result<(), ProviderError> {
        let addr = self.options.settings.node_addr.clone();
        let server_name = self.server_name()?;
        let argv = bootstrap::Delivery {
            addr: &addr,
            pin: &bootstrap.cert_pin,
            server_name: &server_name,
            token: &bootstrap.token,
            gateway_host: &self.options.gateway_host,
        }
        .argv()
        .map_err(ProviderError::Failed)?;
        let out = client
            .exec(
                id,
                &ExecRequest {
                    argv,
                    workdir: None,
                    timeout_seconds: Some(60),
                },
            )
            .await
            .map_err(failed)?;
        if out.exit_code != 0 {
            return Err(failed(format!(
                "the bootstrap exited {} in DigitalOcean session {id}: {}",
                out.exit_code,
                out.stderr.trim()
            )));
        }
        Ok(())
    }

    /// Start a made session's run: ready, then told where to meet the node.
    /// On failure the session is removed.
    async fn start_run(
        &self,
        client: &Client,
        session: Session,
        image: &str,
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        let id = session.session_id.clone();
        let arrival = self.listener.expect(&bootstrap.run);
        let started = async {
            let ready = self.until_ready(client, &id).await?;
            self.deliver(client, &id, bootstrap).await?;
            Ok::<_, ProviderError>(ready)
        }
        .await;
        match started {
            Ok(ready) => {
                self.made.lock().unwrap().insert(
                    id.clone(),
                    Made {
                        run: bootstrap.run.clone(),
                        image: image.to_string(),
                        config_id: ready.config_id.or(session.config_id),
                        arrival: Some(arrival),
                    },
                );
                Ok(Handle(id))
            }
            Err(error) => {
                self.listener.forget(&bootstrap.run);
                let _ = client.delete_session(&id).await;
                if let Some(config) = &session.config_id {
                    let _ = client.delete_config(config).await;
                }
                Err(error)
            }
        }
    }

    /// The config a session was made with: remembered, else read back.
    async fn config_of(&self, client: &Client, handle: &Handle) -> Result<String, ProviderError> {
        let known = self
            .made
            .lock()
            .unwrap()
            .get(&handle.0)
            .and_then(|m| m.config_id.clone());
        if let Some(config) = known {
            return Ok(config);
        }
        let session = client.get_session(&handle.0).await.map_err(|e| match e {
            e if e.is_not_found() => ProviderError::NotFound(handle.0.clone()),
            e => failed(e),
        })?;
        session
            .config_id
            .ok_or_else(|| failed(format!("DigitalOcean session {} names no config", handle.0)))
    }

    /// The session `handle` names: an id this provider made, else (a node
    /// process that did not make it, killing by name) the owned session of
    /// that name.
    async fn resolve(
        &self,
        client: &Client,
        handle: &Handle,
    ) -> Result<Option<Session>, ProviderError> {
        match client.get_session(&handle.0).await {
            Ok(session) => return Ok(Some(session)),
            Err(error) if error.is_not_found() => {}
            // A name is not an id: the API may refuse it as malformed.
            Err(DoError::Invalid(_)) | Err(DoError::Api { status: 400, .. }) => {}
            Err(error) => return Err(failed(error)),
        }
        let name = self.options.owner.session_name(&handle.0);
        client.find_session(&name).await.map_err(failed)
    }
}

#[async_trait]
impl SandboxProvider for DoProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::DigitalOcean
    }

    async fn create(&self, spec: &CreateSpec<'_>) -> Result<Handle, ProviderError> {
        // The sandbox is a VM elsewhere: nothing of the node's is mounted.
        if let Some(mount) = spec.mounts.iter().find(|m| !m.is_local()) {
            return Err(failed(format!(
                "DigitalOcean mounts nothing from the node, and {} would be mounted at {}; \
                 its trees go over the files channel ([runtime.podman] sandbox_files)",
                mount.volume, mount.target
            )));
        }
        let template = self.templates.ready(spec.image).map_err(|why| {
            failed(format!(
                "DigitalOcean cannot start a sandbox in {} yet: {why}",
                spec.image
            ))
        })?;
        let name = spec
            .name
            .ok_or_else(|| failed("a DigitalOcean sandbox needs a name"))?;
        let manifest = self.manifest(name, &template)?;
        // Said before anything is made: a relay not held now would leave a
        // session nobody can reach.
        self.server_name()?;
        let client = self.client()?;
        let session = client.create_session(&manifest).await.map_err(failed)?;
        self.start_run(&client, session, spec.image, spec.bootstrap)
            .await
    }

    async fn connect(
        &self,
        handle: &Handle,
        server: &SandboxServer,
        within: Duration,
    ) -> Result<SandboxConn, ProviderError> {
        if server.pin() != self.listener.pin() {
            return Err(failed(
                "the node's sandbox listener holds another identity than this run's",
            ));
        }
        let (run, arrival) = {
            let mut made = self.made.lock().unwrap();
            let entry = made
                .get_mut(&handle.0)
                .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
            (entry.run.clone(), entry.arrival.take())
        };
        let arrival =
            arrival.ok_or_else(|| failed(format!("{} has connected already", handle.0)))?;
        match tokio::time::timeout(within, arrival).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(_)) | Err(_) => {
                self.listener.forget(&run);
                Err(failed(format!(
                    "DigitalOcean session {} did not connect to the node within {} s",
                    handle.0,
                    within.as_secs()
                )))
            }
        }
    }

    /// Its supervisor redials the node's one listener, which takes any live
    /// run's secret: nothing to set up beyond the identity being this one.
    async fn reattach(
        &self,
        _handle: &Handle,
        server: &SandboxServer,
        _conn: &SandboxConn,
    ) -> Result<(), ProviderError> {
        if server.pin() != self.listener.pin() {
            return Err(failed(
                "the node's sandbox listener holds another identity than this run's",
            ));
        }
        Ok(())
    }

    async fn destroy(&self, handle: &Handle) -> Result<(), ProviderError> {
        let client = self.client()?;
        let made = self.made.lock().unwrap().remove(&handle.0);
        if let Some(made) = &made {
            self.listener.forget(&made.run);
        }
        let (id, config) = match made {
            Some(made) => (handle.0.clone(), made.config_id),
            None => match self.resolve(&client, handle).await? {
                Some(session) => (session.session_id, session.config_id),
                None => return Ok(()),
            },
        };
        client.delete_session(&id).await.map_err(failed)?;
        // Removing a session leaves the config its create stored.
        if let Some(config) = config {
            client.delete_config(&config).await.map_err(failed)?;
        }
        Ok(())
    }

    async fn status(&self, handle: &Handle) -> Result<Status, ProviderError> {
        let client = self.client()?;
        match client.get_session(&handle.0).await {
            Ok(session) => Ok(status_of(&session)),
            Err(error) if error.is_not_found() => Ok(Status::Lost),
            Err(error) => Err(failed(error)),
        }
    }

    async fn pause(&self, handle: &Handle) -> Result<(), ProviderError> {
        self.client()?
            .pause_session(&handle.0)
            .await
            .map_err(failed)
    }

    async fn resume(&self, handle: &Handle) -> Result<(), ProviderError> {
        self.client()?
            .resume_session(&handle.0)
            .await
            .map_err(failed)
    }

    async fn snapshot(&self, handle: &Handle) -> Result<SnapshotId, ProviderError> {
        let checkpoint = self
            .client()?
            .create_checkpoint(&handle.0, Some("tracon"))
            .await
            .map_err(failed)?;
        Ok(SnapshotId(format!(
            "{}/{}",
            handle.0, checkpoint.checkpoint_id
        )))
    }

    /// A child forked from a checkpoint, told its own bootstrap: it starts
    /// with its parent's memory, and so with a supervisor whose connection
    /// is gone, which ends; the template's service then waits for this one.
    async fn fork(
        &self,
        snapshot: &SnapshotId,
        _resources: &Resources,
        bootstrap: &Bootstrap,
    ) -> Result<Handle, ProviderError> {
        let (session, checkpoint) = snapshot
            .0
            .split_once('/')
            .ok_or_else(|| failed(format!("{} is not a DigitalOcean checkpoint", snapshot.0)))?;
        let image = self
            .made
            .lock()
            .unwrap()
            .get(session)
            .map(|m| m.image.clone())
            .unwrap_or_default();
        let client = self.client()?;
        let child = client
            .fork_session(session, Some(checkpoint), 1)
            .await
            .map_err(failed)?
            .into_iter()
            .next()
            .ok_or_else(|| failed("DigitalOcean forked no session"))?;
        self.start_run(&client, child, &image, bootstrap).await
    }

    /// Derived: the runtime rebuilt the image as its template, under a
    /// digest of its own that this API does not report, so the template
    /// stands for it.
    async fn image_identity(&self, handle: &Handle) -> Result<ImageIdentity, ProviderError> {
        let image = self
            .made
            .lock()
            .unwrap()
            .get(&handle.0)
            .map(|m| m.image.clone())
            .ok_or_else(|| ProviderError::NotFound(handle.0.clone()))?;
        let template = self
            .templates
            .ready(&image)
            .map_err(ProviderError::Failed)?;
        Ok(ImageIdentity::Derived {
            from: templates::digest_of(&image)
                .map_err(ProviderError::Failed)?
                .to_string(),
            actual: format!("digitalocean-template:{template}"),
        })
    }

    async fn network(&self, handle: &Handle) -> Result<Network, ProviderError> {
        let client = self.client()?;
        let config = self.config_of(&client, handle).await?;
        let stored = client.get_config(&config).await.map_err(failed)?;
        let egress = stored_egress(&stored.manifest);
        Ok(Network {
            destinations: egress
                .allow_ips
                .into_iter()
                .chain(egress.allow_hosts)
                .chain(egress.other)
                .collect(),
            dns: Dns::ProviderOpen,
        })
    }

    /// Read back from DigitalOcean: a bare session (no managed agent in it)
    /// whose stored manifest lets it reach the node's address and nothing
    /// else. Anything else reads as an unverified route.
    async fn verify_isolation(&self, handle: &Handle) -> Result<Capabilities, ProviderError> {
        let client = self.client()?;
        let session = client.get_session(&handle.0).await.map_err(|e| match e {
            e if e.is_not_found() => ProviderError::NotFound(handle.0.clone()),
            e => failed(e),
        })?;
        let config = match session.config_id.clone() {
            Some(config) => config,
            None => self.config_of(&client, handle).await?,
        };
        let stored = client.get_config(&config).await.map_err(failed)?;
        let egress = stored_egress(&stored.manifest);
        let mut wrong = Vec::new();
        if !matches!(session.agent_kind.as_str(), "AGENT_KIND_NONE" | "") {
            wrong.push(format!("runs a managed agent ({})", session.agent_kind));
        }
        if stored.manifest.get("harness").and_then(|h| h.as_str()) != Some("none") {
            wrong.push("its manifest's harness is not none".to_string());
        }
        let node = self.node_ip()?;
        if egress.allow_ips != [node.clone()] {
            wrong.push(format!(
                "its egress admits {:?}, not exactly {node}",
                egress.allow_ips
            ));
        }
        if !egress.allow_hosts.is_empty() {
            wrong.push(format!("its egress admits hosts {:?}", egress.allow_hosts));
        }
        if !egress.other.is_empty() {
            wrong.push(format!("its egress also has {}", egress.other.join(", ")));
        }
        let mut caps = capabilities();
        if !wrong.is_empty() {
            tracing::warn!(session = %handle.0, why = %wrong.join("; "), "a DigitalOcean sandbox's route is not the node's alone");
            caps.route = Route::Unverified;
        }
        Ok(caps)
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    async fn capacity(&self) -> Result<Capacity, ProviderError> {
        let sessions = self
            .client()?
            .list_sessions(&SessionFilter::default())
            .await
            .map_err(failed)?;
        let running = sessions
            .iter()
            .filter(|s| self.options.owner.owns(&s.name) && !s.status.is_terminal())
            .count() as u32;
        Ok(Capacity {
            running,
            limit: Some(self.options.settings.limit),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_this_nodes_alone() {
        let owner = Owner::new("tracon", "ABCD1234-5678");
        assert_eq!(owner.node, "abcd1234");
        let name = owner.session_name("tracon-h-abc");
        assert_eq!(name, "tracon-h-abc-nabcd1234");
        assert!(owner.owns(&name));
        assert!(!owner.owns("tracon-h-abc-nffff0000"));
        assert!(!owner.owns("spike-h-abc-nabcd1234"));
        assert!(!owner.owns("hand-made"));
        assert_eq!(owner.template_stem(), "tracon-tabcd1234");
    }

    #[test]
    fn paused_is_not_lost_and_the_unknown_is_live() {
        let with = |status: &str| Session {
            status: SessionStatus::from(status),
            ..Session::default()
        };
        assert_eq!(status_of(&with("SESSION_STATUS_PAUSED")), Status::Paused);
        assert_eq!(status_of(&with("SESSION_STATUS_READY")), Status::Running);
        assert_eq!(
            status_of(&with("SESSION_STATUS_PROVISIONING")),
            Status::Starting
        );
        assert_eq!(status_of(&with("SESSION_STATUS_DESTROYED")), Status::Lost);
        assert_eq!(status_of(&with("SESSION_STATUS_FAILED")), Status::Lost);
        assert_eq!(
            status_of(&with("SESSION_STATUS_NEW_THING")),
            Status::Running
        );
    }

    #[test]
    fn stored_egress_reads_every_widening() {
        let only = stored_egress(&serde_json::json!({"egress": {"allow_ips": ["203.0.113.7"]}}));
        assert_eq!(only.allow_ips, ["203.0.113.7"]);
        assert!(only.allow_hosts.is_empty() && only.other.is_empty());
        let open = stored_egress(&serde_json::json!({"harness": "none"}));
        assert_eq!(open.other, ["no egress rule (unrestricted)"]);
        let wide = stored_egress(&serde_json::json!({"egress": "unrestricted"}));
        assert!(!wide.other.is_empty());
        let vpc = stored_egress(
            &serde_json::json!({"egress": {"allow_ips": ["1.1.1.1"], "vpc_uuid": "v"}}),
        );
        assert_eq!(vpc.other, ["egress.vpc_uuid"]);
        let hosts = stored_egress(&serde_json::json!({"egress": ["pypi.org"]}));
        assert_eq!(hosts.allow_hosts, ["pypi.org"]);
    }

    fn settings() -> Settings {
        Settings {
            credential: "do".into(),
            image: format!("ghcr.io/x/y@sha256:{}", "a".repeat(64)),
            listen: "0.0.0.0:8443".into(),
            node_addr: "203.0.113.7:443".into(),
            ..Settings::default()
        }
    }

    #[test]
    fn settings_are_checked_only_when_in_use() {
        assert!(validate(&Settings::default(), false, false).is_ok());
        assert!(validate(&settings(), false, false).is_ok());
        for bad in [
            Settings {
                node_addr: "node.example.net:443".into(),
                ..settings()
            },
            Settings {
                listen: "8443".into(),
                ..settings()
            },
            Settings {
                image: "ghcr.io/x/y:latest".into(),
                ..settings()
            },
            Settings {
                limit: 0,
                ..settings()
            },
            Settings {
                server_name: "bad name".into(),
                ..settings()
            },
        ] {
            assert!(validate(&bad, false, true).is_err(), "{bad:?}");
        }
        let relayed = Settings {
            relay: true,
            ..settings()
        };
        assert!(validate(&relayed, false, false).is_err());
        assert!(validate(&relayed, true, false).is_ok());
        // The node builds its own templates only from its own sandbox
        // images, and only one source is named.
        let built = Settings {
            image: String::new(),
            registry: "registry.digitalocean.com/acme".into(),
            ..settings()
        };
        assert!(validate(&built, false, false).is_err());
        assert!(validate(&built, false, true).is_ok());
        let both = Settings {
            registry: "registry.digitalocean.com/acme".into(),
            ..settings()
        };
        assert!(validate(&both, false, true).is_err());
        let neither = Settings {
            image: String::new(),
            ..settings()
        };
        assert!(validate(&neither, false, true).is_err());
    }
}
