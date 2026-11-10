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
}

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
        }))
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
            .filter_map(|image| templates.name_for(image).ok())
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

    /// The runner a session placed here runs in: its trees over the files
    /// channel, its gateways through tunnels, its isolation read back and
    /// held to `requirements` before anything reaches it.
    pub fn runner(
        &self,
        cfg: &Config,
        requirements: Requirements,
        mounts: Vec<Mount>,
        image: String,
    ) -> Arc<dyn Runner> {
        self.runner_with(
            cfg,
            requirements,
            mounts,
            image,
            crate::runner::sandbox_files::FilesStore::for_node(),
        )
    }

    pub fn runner_with(
        &self,
        cfg: &Config,
        requirements: Requirements,
        mounts: Vec<Mount>,
        image: String,
        files: crate::runner::sandbox_files::FilesStore,
    ) -> Arc<dyn Runner> {
        let provider: Arc<dyn crate::provider::SandboxProvider> = self.provider.clone();
        Arc::new(
            crate::runner::protocol::ProtocolRunner::new(provider, image)
                // The template's image holds the bundle at /opt/tracon.
                .with_bundled_harness(true)
                .with_mounts(mounts)
                .with_workdir("/work")
                .with_files(files)
                .with_tunnels(crate::runner::tunnels::gateway_tunnels(cfg))
                .with_requirements(requirements)
                .with_token_window(TOKEN_WINDOW),
        )
    }
}
