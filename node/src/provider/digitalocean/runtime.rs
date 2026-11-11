//! The node's DigitalOcean provider while it runs: built once at start when
//! `[digitalocean]` has a credential and `[placement] digitalocean` is not
//! `off`, holding the sandbox listener, the template and the upkeep, and
//! making the runner a session placed there runs in.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::listener::DoListener;
use super::templates::{Source, TemplateState, Templates};
use super::{upkeep, ClientSource, DoProvider, HubRelay, Options, Owner, Settings};
use crate::config::Config;
use crate::provider::requirements::DoMode;
use crate::provider::{Candidate, ProviderKind, Requirements, SandboxProvider as _};
use crate::runner::{Mount, Runner};

/// How long a run's token may wait to be redeemed. The sandbox redeems it
/// seconds after the bootstrap's exec; this bounds an unredeemed copy, such
/// as one in DigitalOcean's record of that exec.
pub const TOKEN_WINDOW: Duration = Duration::from_secs(5 * 60);

/// How long a new session may take to be ready.
const READY_WITHIN: Duration = Duration::from_secs(180);

/// How often the template is looked at while it builds, and once ready.
const TEMPLATE_BUILDING_POLL: Duration = Duration::from_secs(30);
const TEMPLATE_READY_POLL: Duration = Duration::from_secs(15 * 60);

/// How often configs, checkpoints and stale templates are swept.
const SWEEP_EVERY: Duration = Duration::from_secs(30 * 60);

/// How long placement waits on the runtime's answer about capacity.
const CAPACITY_WITHIN: Duration = Duration::from_secs(15);

pub struct DoRuntime {
    provider: Arc<DoProvider>,
    /// Why the hub cannot carry sandboxes over its own URL, once its
    /// `/v0/info` has said so.
    hub_refused: std::sync::Mutex<Option<String>>,
}

/// How often an unanswered look at the hub's `/v0/info` is tried again.
const HUB_INFO_RETRY: Duration = Duration::from_secs(60);

static RUNTIME: OnceLock<Arc<DoRuntime>> = OnceLock::new();

/// The node's DigitalOcean provider, when it has one.
pub fn global() -> Option<Arc<DoRuntime>> {
    RUNTIME.get().cloned()
}

/// Make `runtime` the node's, as `DoRuntime::start` does, for a runtime
/// built around a test's stand-ins. The first one stays.
#[doc(hidden)]
pub fn install(runtime: Arc<DoRuntime>) -> bool {
    RUNTIME.set(runtime).is_ok()
}

/// Whether sessions may run on DigitalOcean here at all.
pub fn enabled(cfg: &Config) -> bool {
    cfg.live().placement.digitalocean != DoMode::Off && cfg.digitalocean.is_configured()
}

/// The token from the broker credential `[digitalocean] credential` names.
fn client_source(
    settings: Settings,
    broker: crate::broker::SharedBroker,
    node_id: String,
) -> ClientSource {
    Arc::new(move || {
        let env = broker
            .read()
            .map_err(|_| "the credential store is unavailable".to_string())?
            .env_for_node(&settings.credential, &node_id)
            .map_err(|e| format!("DigitalOcean credential {}: {e}", settings.credential))?;
        let token = env.get(crate::do_harness::TOKEN_ENV).ok_or_else(|| {
            format!(
                "DigitalOcean credential {} has no {}",
                settings.credential,
                crate::do_harness::TOKEN_ENV
            )
        })?;
        settings.client(token).map_err(|e| e.to_string())
    })
}

impl DoRuntime {
    /// Start the provider for `cfg`, or nothing when it is not configured
    /// on. Binding its listener is the one failure that stops it.
    pub async fn start(
        cfg: &Config,
        broker: crate::broker::SharedBroker,
        node_id: &str,
    ) -> Result<Option<Arc<Self>>, String> {
        if !enabled(cfg) {
            return Ok(None);
        }
        let settings = cfg.digitalocean.clone();
        let server = crate::runner::protocol::node_server().map_err(|e| e.to_string())?;
        let listener = DoListener::bind(settings.listen_addr()?, server)
            .await
            .map_err(|e| format!("digitalocean.listen {}: {e}", settings.listen))?;
        let client = client_source(settings.clone(), broker, node_id.to_string());
        let relay = HubRelay {
            host: Arc::new(crate::mesh::sandbox_relay::current_host),
            hub_url: cfg.mesh.hub_url.clone(),
        };
        let source = match settings.image.is_empty() {
            false => Source::Given(settings.image.clone()),
            true => Source::Built {
                registry: settings.registry.clone(),
                prefix: crate::config::resource_prefix().to_string(),
                bundle: crate::bundle::effective(cfg)
                    .ok_or("digitalocean.registry needs the harness bundle this node mounts")?,
                services: crate::service_bundle::mounts(cfg),
            },
        };
        let runtime = Self::with(
            client,
            settings,
            source,
            Owner::new(crate::config::resource_prefix(), node_id),
            cfg.boundary.gateway_container.clone(),
            listener,
            relay,
        )?;
        let _ = RUNTIME.set(runtime.clone());
        tokio::spawn(runtime.clone().upkeep());
        if let (super::Reach::HubUrl { .. }, Some(hub_url)) =
            (&runtime.provider.options().reach, cfg.mesh.hub_url.clone())
        {
            tokio::spawn(runtime.clone().check_hub(hub_url));
        }
        tracing::info!(
            listen = %runtime.provider.listener().local_addr(),
            "DigitalOcean sandbox provider started"
        );
        Ok(Some(runtime))
    }

    /// A runtime around the parts given, for a test's stand-ins.
    pub fn with(
        client: ClientSource,
        settings: Settings,
        source: Source,
        owner: Owner,
        gateway_host: String,
        listener: Arc<DoListener>,
        relay: HubRelay,
    ) -> Result<Arc<Self>, String> {
        let reach = settings.reach(relay.hub_url.as_deref())?;
        let templates = Arc::new(Templates::new(
            source,
            &settings.base_template,
            &owner.template_stem(),
        )?);
        let provider = DoProvider::new(
            client,
            Options {
                settings,
                owner,
                gateway_host,
                reach,
                ready_within: READY_WITHIN,
                poll: Duration::from_secs(1),
            },
            listener,
            templates,
            relay.host,
        );
        Ok(Arc::new(DoRuntime {
            provider: Arc::new(provider),
            hub_refused: std::sync::Mutex::new(None),
        }))
    }

    /// Ask the hub, until it answers, whether it serves the relay's
    /// WebSocket; one that does not keeps this provider unhealthy, saying
    /// why, rather than leaving each sandbox to fail its dial unexplained.
    pub async fn check_hub(self: Arc<Self>, hub_url: String) {
        let http = match reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(http) => http,
            Err(error) => return tracing::warn!(%error, "no client to ask the hub with"),
        };
        let info = format!("{}/v0/info", hub_url.trim_end_matches('/'));
        loop {
            let answer = match http.get(&info).send().await {
                Ok(res) if res.status().is_success() => res.json::<serde_json::Value>().await.ok(),
                _ => None,
            };
            if let Some(info) = answer {
                if let Err(why) = super::hub_serves_connect(&hub_url, &info) {
                    tracing::error!(%why, "DigitalOcean sandboxes cannot reach this node through the hub");
                    *self.hub_refused.lock().unwrap() = Some(why);
                }
                return;
            }
            tokio::time::sleep(HUB_INFO_RETRY).await;
        }
    }

    pub fn provider(&self) -> &Arc<DoProvider> {
        &self.provider
    }

    /// The image a session there runs in when it has none of its own: the
    /// operator's, else this node's pinned base.
    pub fn default_image(&self) -> Option<String> {
        self.provider
            .templates()
            .default_image(crate::sandbox_images::active_base())
    }

    /// The image a session there runs in: `own` (its repository's prepared
    /// snapshot) when its template is ready, else the default. One whose
    /// template is not ready yet has it prepared in the background, for the
    /// sessions after this one.
    pub fn image_for(self: &Arc<Self>, own: Option<String>) -> Option<String> {
        if let Some(own) = own.filter(|own| self.provider.templates().serves(own)) {
            if self.provider.templates().ready(&own).is_ok() {
                return Some(own);
            }
            self.prepare(own);
        }
        self.default_image()
    }

    /// Register `image`'s template in the background, looking until it is
    /// ready or failed: part of preparing that image, never of a session.
    pub fn prepare(self: &Arc<Self>, image: String) {
        if !self.provider.templates().serves(&image) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                match runtime.ensure_template(&image).await {
                    TemplateState::Ready { .. } | TemplateState::Failed { .. } => return,
                    TemplateState::Preparing
                    | TemplateState::Building { .. }
                    | TemplateState::Unknown => {}
                }
                tokio::time::sleep(TEMPLATE_BUILDING_POLL).await;
            }
        });
    }

    /// Prepare `image`'s template, when this node has a DigitalOcean
    /// provider: what a repository's preparation calls once it has made a
    /// snapshot.
    pub fn prepare_if_running(image: &str) {
        if let Some(runtime) = global() {
            runtime.prepare(image.to_string());
        }
    }

    /// The templates this node keeps: the default image's and each current
    /// snapshot's.
    async fn wanted(&self) -> std::collections::HashSet<String> {
        let templates = self.provider.templates();
        let mut images: Vec<String> = self.default_image().into_iter().collect();
        if matches!(templates.source(), Source::Built { .. }) {
            images.extend(
                crate::sandbox_images::snapshot_images()
                    .await
                    .into_iter()
                    .map(|(pinned, _)| pinned),
            );
        }
        images
            .iter()
            .flat_map(|image| templates.kept(image))
            .collect()
    }

    /// Orphans once, then the template until it is ready and the sweeps.
    async fn upkeep(self: Arc<Self>) {
        let owner = self.provider.options().owner.clone();
        match self.provider.client() {
            Ok(client) => match upkeep::remove_orphans(&client, &owner).await {
                Ok(swept) if swept.sessions.is_empty() && swept.failed.is_empty() => {}
                Ok(swept) => tracing::warn!(
                    sessions = ?swept.sessions, failed = ?swept.failed,
                    "removed DigitalOcean sessions a previous node process left"
                ),
                Err(error) => {
                    tracing::warn!(%error, "could not look for orphaned DigitalOcean sessions")
                }
            },
            Err(error) => tracing::warn!(%error, "DigitalOcean is configured but unreachable"),
        }
        let mut swept_at: Option<tokio::time::Instant> = None;
        loop {
            let ready = match self.default_image() {
                Some(image) => matches!(
                    self.ensure_template(&image).await,
                    TemplateState::Ready { .. }
                ),
                None => false,
            };
            if swept_at.is_none_or(|at| at.elapsed() >= SWEEP_EVERY) {
                self.sweep().await;
                swept_at = Some(tokio::time::Instant::now());
            }
            tokio::time::sleep(match ready {
                true => TEMPLATE_READY_POLL.min(SWEEP_EVERY),
                false => TEMPLATE_BUILDING_POLL,
            })
            .await;
        }
    }

    /// Register or look at `image`'s template, and say where it stands.
    pub async fn ensure_template(&self, image: &str) -> TemplateState {
        let client = match self.provider.client() {
            Ok(client) => client,
            Err(error) => {
                tracing::debug!(%error, "DigitalOcean template not looked at");
                return TemplateState::Unknown;
            }
        };
        match self
            .provider
            .templates()
            .ensure(&client, client.token(), image)
            .await
        {
            Ok(state) => {
                if !matches!(state, TemplateState::Ready { .. }) {
                    tracing::info!(%image, state = %state.describe(), "DigitalOcean template not ready");
                }
                state
            }
            Err(error) => {
                tracing::warn!(%image, %error, "could not look at a DigitalOcean template");
                TemplateState::Unknown
            }
        }
    }

    pub async fn sweep(&self) -> Option<upkeep::Swept> {
        let client = self.provider.client().ok()?;
        let wanted = self.wanted().await;
        let swept = upkeep::sweep(
            &client,
            &self.provider.options().owner,
            self.provider.templates(),
            &wanted,
            crate::store::now_ms(),
        )
        .await;
        match &swept {
            Ok(s)
                if s.configs.is_empty()
                    && s.checkpoints.is_empty()
                    && s.templates.is_empty()
                    && s.failed.is_empty() => {}
            Ok(s) => tracing::info!(
                configs = s.configs.len(), checkpoints = s.checkpoints.len(),
                freed_bytes = s.freed_bytes, templates = ?s.templates, failed = ?s.failed,
                "swept DigitalOcean"
            ),
            Err(error) => tracing::warn!(%error, "could not sweep DigitalOcean"),
        }
        swept.ok()
    }

    /// Why sessions cannot run there now, or `Ok`: what the node needs on
    /// its side (trees over the files channel, gateways over tunnels), the
    /// template, and the relay's name when it is the way in.
    pub fn health(&self, cfg: &Config) -> Result<(), String> {
        if !crate::boundary::sandboxes_over_protocol(cfg) {
            return Err(
                "a DigitalOcean sandbox needs [runtime.podman] sandbox_protocol, sandbox_files and \
                 sandbox_tunnels on this node, or runtime.kind = \"apple\": it mounts nothing \
                 and has no gateway of its own"
                    .into(),
            );
        }
        let image = self
            .default_image()
            .ok_or("this node has not pinned its base image yet")?;
        self.provider.templates().ready(&image)?;
        if let Some(why) = self.hub_refused.lock().unwrap().clone() {
            return Err(why);
        }
        let settings = &self.provider.options().settings;
        if settings.relay
            && settings.server_name.is_empty()
            && crate::mesh::sandbox_relay::current_host().is_none()
        {
            return Err("the hub relay is not holding this node's name".into());
        }
        Ok(())
    }

    /// The provider as placement sees it now.
    pub async fn candidate(&self, cfg: &Config) -> Candidate {
        let mut health = self.health(cfg);
        let capacity = match tokio::time::timeout(CAPACITY_WITHIN, self.provider.capacity()).await {
            Ok(Ok(capacity)) => capacity,
            Ok(Err(error)) => {
                health = health.and(Err(format!("its sessions could not be listed: {error}")));
                crate::provider::Capacity {
                    running: 0,
                    limit: Some(self.provider.options().settings.limit),
                }
            }
            Err(_) => {
                health = health.and(Err("it did not answer in time".to_string()));
                crate::provider::Capacity {
                    running: 0,
                    limit: Some(self.provider.options().settings.limit),
                }
            }
        };
        Candidate {
            kind: ProviderKind::DigitalOcean,
            capabilities: self.provider.capabilities(),
            capacity,
            health,
        }
    }

    /// The provider as the Settings capability view shows it: what it
    /// promises of every sandbox, by what makes it so, how its sandboxes
    /// reach the node, and whether it can take a session now and why not.
    pub async fn report(&self, cfg: &Config) -> crate::provider::CapabilityReport {
        use crate::provider::capabilities::{CapabilityLine, Elsewhere, Route, ServiceSupport};
        let candidate = self.candidate(cfg).await;
        let caps = candidate.capabilities;
        let options = self.provider.options();
        let (allow_ips, allow_hosts) = options.reach.egress();
        let egress: Vec<String> = allow_ips.into_iter().chain(allow_hosts).collect();
        let (reach, dial) = match &options.reach {
            super::Reach::Addr(addr) if options.settings.relay => ("hub relay", addr.to_string()),
            super::Reach::Addr(addr) => ("address", addr.to_string()),
            super::Reach::HubUrl { url, .. } => ("hub url", url.clone()),
        };
        let line = |capability, value: &'static str, detail: String| CapabilityLine {
            capability,
            value: Some(value),
            check: None,
            detail,
        };
        let lines = vec![
            line(
                "tier",
                caps.tier.as_str(),
                "each sandbox is a microVM of DigitalOcean's".into(),
            ),
            line(
                "route",
                caps.route.as_str(),
                match caps.route {
                    Route::NodeOnly => format!(
                        "each sandbox's egress admits {} alone, read back from DigitalOcean \
                         before anything is sent in",
                        egress.join(", ")
                    ),
                    Route::Unverified => format!(
                        "each sandbox reaches {} by name through the runtime's proxy, which \
                         admits it by the name it is asked for alone: the hub's other routes, \
                         and any site sharing its front, are reachable too",
                        egress.join(", ")
                    ),
                },
            ),
            line(
                "dns",
                caps.dns.as_str(),
                "the runtime's resolver answers any name, so client work is never placed here"
                    .into(),
            ),
            line(
                "privilege",
                caps.privilege.as_str(),
                "root is the VM's own; which user the harness runs as is the user line, as \
                 its sandboxes report it"
                    .into(),
            ),
            line(
                "runtime_socket",
                caps.runtime_socket.as_str(),
                "a sandbox there has no container runtime".into(),
            ),
            line(
                "image_identity",
                caps.image_identity.as_str(),
                "DigitalOcean rebuilds each image as a template, under a digest of its own".into(),
            ),
        ];
        let services = self
            .service_support(cfg)
            .into_iter()
            .map(|(name, support)| ServiceSupport {
                name,
                ok: support.is_ok(),
                why: support.err(),
            })
            .collect();
        crate::provider::CapabilityReport {
            provider: crate::provider::ProviderKind::DigitalOcean,
            verified: candidate.health.is_ok(),
            lines,
            capabilities: Some(caps),
            enforced: true,
            elsewhere: Some(Elsewhere {
                reach,
                dial,
                egress,
                placement: cfg.live().placement.digitalocean,
                healthy: candidate.health.is_ok(),
                why: candidate.health.err(),
                capacity: candidate.capacity,
                services,
                expected_uid: self.provider.expected_uid(),
                reported: self.provider.reported().map(|info| {
                    crate::provider::capabilities::Reported {
                        version: info.version,
                        uid: info.uid,
                    }
                }),
            }),
        }
    }

    /// Whether each catalogue service can run in a sandbox here now, and
    /// why not: in the template a new sandbox would start from (or, before
    /// there is one, the template the node would build).
    pub fn service_support(&self, cfg: &Config) -> Vec<(String, Result<(), String>)> {
        let templates = self.provider.templates();
        let template = self
            .default_image()
            .and_then(|image| templates.ready(&image).ok());
        let held = match &template {
            Some(name) => templates.services_of(name),
            None => templates.current_services(),
        };
        cfg.service
            .iter()
            .map(|service| {
                let support = service_support(service, held.as_deref(), template.as_deref());
                (service.name.clone(), support)
            })
            .collect()
    }

    /// The runner a session placed here runs in: its trees over the files
    /// channel, its gateways through tunnels, its isolation read back and
    /// held to `requirements` before anything reaches it.
    pub fn runner(
        &self,
        cfg: &Config,
        requirements: Requirements,
        mounts: Vec<Mount>,
        image: String,
        harness_id: &str,
    ) -> Arc<dyn Runner> {
        self.runner_with(
            cfg,
            requirements,
            mounts,
            image,
            harness_id,
            crate::runner::sandbox_files::FilesStore::for_node(),
        )
    }

    /// Every process's environment in a sandbox here. The runtime's init
    /// gives the supervisor the runtime's own: its proxy, which admits the
    /// node's way in and nothing else, and a `NO_PROXY` without the
    /// gateway's name, so a harness left with them sends its MCP, model and
    /// egress traffic to that proxy and is refused. So, as an image or a
    /// container gives a local sandbox: the stock base's home, where a
    /// `harness_id` harness keeps its state, and the node's allowlist proxy
    /// through the tunnel, in both spellings, with the gateway's name
    /// reached directly. The supervisor's own dial is not affected: it uses
    /// the bootstrap's copy of the runtime's proxy (`TRACON_SANDBOX_PROXY`).
    pub fn sandbox_env(&self, cfg: &Config, harness_id: &str) -> Vec<(String, String)> {
        let gateway = &self.provider.options().gateway_host;
        let proxy = format!("http://{gateway}:{}", cfg.gateway.proxy_port);
        let mut env =
            crate::sandbox_images::harness_env(gateway, cfg.gateway.proxy_port, harness_id);
        env.extend([
            ("https_proxy".to_string(), proxy.clone()),
            ("http_proxy".to_string(), proxy),
        ]);
        env
    }

    pub fn runner_with(
        &self,
        cfg: &Config,
        requirements: Requirements,
        mounts: Vec<Mount>,
        image: String,
        harness_id: &str,
        files: crate::runner::sandbox_files::FilesStore,
    ) -> Arc<dyn Runner> {
        let provider: Arc<dyn crate::provider::SandboxProvider> = self.provider.clone();
        // Judged against the template each sandbox was made from.
        let refusal: crate::runner::protocol::ServiceRefusal = {
            let made = self.provider.clone();
            let catalogue = cfg.service.clone();
            Arc::new(move |handle, name| {
                let service = catalogue.iter().find(|s| s.name == name)?;
                let template = handle.and_then(|h| made.template_of(h));
                let held = template
                    .as_deref()
                    .and_then(|t| made.templates().services_of(t));
                service_support(service, held.as_deref(), template.as_deref()).err()
            })
        };
        Arc::new(
            crate::runner::protocol::ProtocolRunner::new(provider, image)
                // The template's image holds the bundle at /opt/tracon.
                .with_bundled_harness(true)
                .with_mounts(mounts)
                .with_workdir("/work")
                .with_files(files)
                .with_tunnels(crate::runner::tunnels::gateway_tunnels(cfg))
                .with_env(self.sandbox_env(cfg, harness_id))
                .with_sandbox_services(true)
                .with_service_refusal(refusal)
                .with_requirements(requirements)
                .with_token_window(TOKEN_WINDOW),
        )
    }
}

/// Whether `service` runs in a sandbox made from `template`, which holds
/// the service bundles `held` (`None`: an image of the operator's, taken on
/// trust). One with a bundle runs inside, under the sandbox's supervisor,
/// from the template's copy: nothing is mounted there and nothing runs
/// beside a sandbox, so one with only an image cannot, and nor can one whose
/// bundle that template does not hold. Its traffic is the sandbox's own: the
/// node's allowlist proxy through the tunnel, never a wider egress.
fn service_support(
    service: &crate::config::Service,
    held: Option<&[String]>,
    template: Option<&str>,
) -> Result<(), String> {
    match (&service.bundle, held) {
        (None, _) => Err(format!(
            "service {} runs as a container of its own beside a session, and DigitalOcean \
             runs nothing beside its sandboxes: give it a bundle ([[service]] bundle) to run \
             it inside one",
            service.name
        )),
        (Some(_), Some(held)) if !held.contains(&service.name) => Err(format!(
            "service {}'s bundle is not in DigitalOcean's template{}: {}",
            service.name,
            template.map(|t| format!(" {t}")).unwrap_or_default(),
            match crate::service_bundle::resolve(service) {
                Some(dir) if dir.is_dir() => format!(
                    "{} was not on this node when the template was built; the next one holds it",
                    dir.display()
                ),
                _ => "this node has no copy of it".into(),
            }
        )),
        (Some(_), _) => Ok(()),
    }
}
