//! The node's DigitalOcean provider while it runs: built once at start when
//! `[digitalocean]` has a credential and `[placement] digitalocean` is not
//! `off`, holding the sandbox listener, the template and the upkeep, and
//! making the runner a session placed there runs in.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::listener::DoListener;
use super::templates::{Source, TemplateState, Templates};
use super::{upkeep, ClientSource, DoProvider, HubRelay, Options, Owner, Settings};
use crate::boundary::{Backend, ImageBuilder};
use crate::config::{Config, RuntimeKind};
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

/// How long a review's checks placed here wait for the template of the
/// image they run in, which the runtime takes several minutes to build once
/// per image: past it, they run on the node's own runtime after all.
pub const CHECK_TEMPLATE_WITHIN: Duration = Duration::from_secs(10 * 60);

/// How often a check waiting for its template looks at it.
pub const CHECK_TEMPLATE_POLL: Duration = Duration::from_secs(15);

/// How long placement waits on the runtime's answer about capacity.
const CAPACITY_WITHIN: Duration = Duration::from_secs(15);

/// How long a peer's answer to a template request is waited for; the
/// request waits in the outbox longer, and the peer's hello says the rest.
const PEER_ANSWER_WITHIN: Duration = Duration::from_secs(60);

pub struct DoRuntime {
    provider: Arc<DoProvider>,
    /// Why the hub cannot carry sandboxes over its own URL, once its
    /// `/v0/info` has said so.
    hub_refused: std::sync::Mutex<Option<String>>,
    /// What asks peers for templates (`templates = "peer"`).
    mesh: OnceLock<Arc<crate::mesh::client::MeshClient>>,
    /// With `templates = "peer"`: what the stock base's key is made of here.
    peer_base: OnceLock<PeerBase>,
    /// The stock base's portable image, or why it is not known yet.
    peer_default: Mutex<Result<String, String>>,
    /// The images whose templates are being looked at until ready.
    preparing: Mutex<HashSet<String>>,
    /// The keys being prepared here for peers.
    peer_jobs: Mutex<HashSet<String>>,
    /// The repositories being prepared for a peer's key: the snapshot each
    /// makes is taken for that key as it is made.
    expected: Mutex<HashMap<PathBuf, String>>,
    /// The node's runtime and store, which say which prepared snapshots are
    /// current and so whose templates are kept.
    node: OnceLock<(Arc<dyn Backend>, Arc<crate::store::Store>)>,
}

/// What the stock base's portable key is made of on a node that takes its
/// templates from a peer, read from its configuration at start.
#[derive(Debug, Clone)]
pub struct PeerBase {
    /// `[sandbox] base_image`, as configured.
    pub base_ref: String,
    pub harness: String,
    pub services: Vec<(String, String)>,
}

impl PeerBase {
    pub fn of(cfg: &Config) -> Self {
        PeerBase {
            base_ref: cfg.sandbox.base_image_ref(),
            harness: super::portable::harness_release(cfg),
            services: super::portable::services(cfg),
        }
    }
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

/// The image a session runs in on DigitalOcean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoImage {
    pub image: String,
    /// Why it is not what the session's repository gets on this node's own
    /// runtime, when it is not: for its `session_started` event.
    pub note: Option<String>,
}

/// What a repository's sessions on this node's own runtime run in, best
/// first, as the images DigitalOcean templates are made from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Own {
    /// The repository's short name.
    pub repository: Option<String>,
    /// Its prepared snapshot, then the image its entry builds or names: the
    /// one a session runs in when the snapshot's template is not ready, and
    /// the template itself of an entry that prepares nothing.
    pub images: Vec<String>,
    /// Why the entry asks for an image of its own that is not here.
    pub missing: Option<String>,
}

impl Own {
    /// From `snapshot` (the repository's prepared one, if any) and its
    /// `environment`.
    pub fn of(snapshot: Option<String>, environment: &crate::environment::RepoEnvironment) -> Self {
        let mut images: Vec<String> = snapshot.into_iter().collect();
        let mut missing = None;
        if environment.names_image() {
            // The environment falls back to the stock base, and says why.
            if environment.image_source.starts_with("harness image") {
                missing = Some(match environment.image_source.contains("held") {
                    true => "its image is held: start its build in Settings, Repositories",
                    false => "its image has not been built on this node yet",
                });
            } else if !images.contains(&environment.image) {
                images.push(environment.image.clone());
            }
        }
        if images.is_empty() && missing.is_none() && !environment.prepare.is_empty() {
            missing = Some("its prepared snapshot has not been made on this node yet");
        }
        Own {
            repository: environment.repo.clone(),
            images,
            missing: missing.map(str::to_string),
        }
    }
}

/// The note of a session that runs without its repository's own image.
fn missing(repository: Option<&str>, why: &str) -> String {
    format!(
        "no DigitalOcean template for {}: {why}",
        repository.unwrap_or("its repository")
    )
}

/// The image of `own` a session runs in, else `default`, and the one whose
/// template is to be prepared: the first of `own`, while it is not ready.
fn choose(
    templates: &Templates,
    own: &Own,
    default: Option<String>,
) -> (Option<DoImage>, Option<String>) {
    let Some(first) = own.images.first() else {
        let note = own
            .missing
            .as_deref()
            .map(|why| missing(own.repository.as_deref(), why));
        return (default.map(|image| DoImage { image, note }), None);
    };
    let why = match templates.ready(first) {
        Ok(_) => {
            let image = DoImage {
                image: first.clone(),
                note: None,
            };
            return (Some(image), None);
        }
        Err(_) if matches!(templates.source(), Source::Given(_)) => {
            "[digitalocean] image is the one image every session there runs in".to_string()
        }
        Err(why) => why,
    };
    let prepare = templates.serves(first).then(|| first.clone());
    let chosen = match own.images[1..]
        .iter()
        .find(|image| templates.ready(image).is_ok())
    {
        Some(image) => Some(DoImage {
            image: image.clone(),
            note: Some(format!(
                "no DigitalOcean template for {}'s prepared snapshot: {why}; the session runs \
                 in its image, unprepared",
                own.repository.as_deref().unwrap_or("its repository")
            )),
        }),
        None => default.map(|image| DoImage {
            image,
            note: Some(missing(own.repository.as_deref(), &why)),
        }),
    };
    (chosen, prepare)
}

impl DoRuntime {
    /// Start the provider for `cfg`, or nothing when it is not configured
    /// on. Binding its listener, and a node that cannot build the templates
    /// it is to build, are the failures that stop it.
    pub async fn start(
        cfg: &Config,
        broker: crate::broker::SharedBroker,
        node_id: &str,
        backend: Arc<dyn Backend>,
        store: Arc<crate::store::Store>,
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
        let source = match (settings.templates, settings.image.is_empty()) {
            (super::TemplateSource::Peer, _) => Source::Peer {
                builder: (!settings.builder.trim().is_empty())
                    .then(|| settings.builder.trim().to_string()),
                services: super::portable::services(cfg)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect(),
            },
            (_, false) => Source::Given(settings.image.clone()),
            (_, true) => Source::Built {
                registry: settings.registry.clone(),
                prefix: crate::config::resource_prefix().to_string(),
                bundle: crate::bundle::effective(cfg)
                    .ok_or("digitalocean.registry needs the harness bundle this node mounts")?,
                services: crate::service_bundle::mounts(cfg),
            },
        };
        let peer = matches!(source, Source::Peer { .. });
        let builder = match source {
            Source::Built { .. } => source_builder(cfg, backend.as_ref())?,
            _ => None,
        };
        let runtime = Self::assemble(
            client,
            settings,
            source,
            Owner::new(crate::config::resource_prefix(), node_id),
            // The name the harness's own configuration gives the gateway,
            // which the sandbox's hosts file makes its loopback.
            backend.harness_host(),
            listener,
            relay,
            builder,
        )?;
        if peer {
            let _ = runtime.peer_base.set(PeerBase::of(cfg));
        }
        runtime.set_node(backend, store);
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
        Self::assemble(
            client,
            settings,
            source,
            owner,
            gateway_host,
            listener,
            relay,
            None,
        )
    }

    /// `with`, building its sources on `builder` where Podman does not.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        client: ClientSource,
        settings: Settings,
        source: Source,
        owner: Owner,
        gateway_host: String,
        listener: Arc<DoListener>,
        relay: HubRelay,
        builder: Option<Arc<dyn ImageBuilder>>,
    ) -> Result<Arc<Self>, String> {
        let reach = settings.reach(relay.hub_url.as_deref())?;
        // A builder keeps what peers asked for across restarts.
        let shared = match &source {
            Source::Built { .. } => super::shared::Shared::load(
                crate::config::Config::state_dir().join(super::shared::FILE),
            ),
            _ => super::shared::Shared::in_memory(),
        };
        let mut templates =
            Templates::new(source, &settings.base_template, &owner.template_stem())?
                .with_team(&super::portable::team_stem(&owner.prefix))
                .with_shared(shared);
        if let Some(builder) = builder {
            templates = templates.with_builder(builder);
        }
        let templates = Arc::new(templates);
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
            mesh: OnceLock::new(),
            peer_base: OnceLock::new(),
            peer_default: Mutex::new(Err(
                "the stock base's release digest is not known yet".into()
            )),
            preparing: Mutex::new(HashSet::new()),
            peer_jobs: Mutex::new(HashSet::new()),
            expected: Mutex::new(HashMap::new()),
            node: OnceLock::new(),
        }))
    }

    /// Keep the templates of the prepared snapshots `backend` holds and
    /// `store` has ready: what `start` does, for a test's stand-ins.
    pub fn set_node(&self, backend: Arc<dyn Backend>, store: Arc<crate::store::Store>) {
        self.provider.meter().set_store(store.clone());
        let _ = self.node.set((backend, store));
    }

    /// Ask peers for templates over `mesh`.
    pub fn set_mesh(&self, mesh: Arc<crate::mesh::client::MeshClient>) {
        let _ = self.mesh.set(mesh);
    }

    fn takes_from_peer(&self) -> bool {
        matches!(self.provider.templates().source(), Source::Peer { .. })
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
    /// operator's, else this node's pinned base, or with templates from a
    /// peer the stock base's portable key.
    pub fn default_image(&self) -> Option<String> {
        let fallback = match self.takes_from_peer() {
            true => self.peer_default.lock().unwrap().clone().ok(),
            false => crate::sandbox_images::active_base(),
        };
        self.provider.templates().default_image(fallback)
    }

    /// The image a session on `origin` runs in there: its repository's
    /// prepared snapshot when that has a template ready, else the image its
    /// entry builds or names (the template itself of an entry that prepares
    /// nothing), else the default, with why it is not the repository's own.
    /// One with none yet has it made in the background (here, or asked of
    /// a peer), for the sessions after this one.
    pub async fn session_image(
        self: &Arc<Self>,
        backend: &dyn crate::boundary::Backend,
        cfg: &Config,
        store: &crate::store::Store,
        origin: Option<&Path>,
        environment: &crate::environment::RepoEnvironment,
    ) -> Option<DoImage> {
        if !self.takes_from_peer() {
            // Never prepared for this session.
            let own = match origin {
                Some(origin) => Own::of(
                    crate::sandbox_images::snapshot_for(
                        backend,
                        cfg,
                        store,
                        origin,
                        environment,
                        false,
                    )
                    .await,
                    environment,
                ),
                None => Own::default(),
            };
            return self.image_for(&own);
        }
        let mut why = None;
        // An entry that builds its own image has a template of its own,
        // prepared or not; one that does neither runs on the stock base.
        if let Some(origin) =
            origin.filter(|_| !environment.prepare.is_empty() || environment.names_image())
        {
            match self.peer_repo_image(cfg, origin).await {
                Ok((image, identity, commit)) => {
                    let ready = self.provider.templates().ready(&image);
                    self.ask_peer(&image, &identity, &commit);
                    match ready {
                        Ok(_) => return Some(DoImage { image, note: None }),
                        Err(not) => why = Some(not),
                    }
                }
                Err(not) => {
                    tracing::info!(repo = %origin.display(), why = %not, "no DigitalOcean template from a peer for this repository");
                    why = Some(not);
                }
            }
        }
        let image = self.default_image()?;
        self.ask_peer(&image, "", "");
        Some(DoImage {
            image,
            note: why.map(|why| missing(environment.repo.as_deref(), &why)),
        })
    }

    /// `origin`'s portable image, its forge identity and the commit its
    /// key was read at.
    async fn peer_repo_image(
        &self,
        cfg: &Config,
        origin: &Path,
    ) -> Result<(String, String, String), String> {
        let identity = super::portable::forge_identity(cfg, origin)
            .await
            .ok_or("its forge identity is not known")?;
        let (inputs, commit) = super::portable::repo_inputs(cfg, origin, &identity).await?;
        Ok((inputs.image(), identity, commit))
    }

    /// Ask a peer to make `image`'s template, or tell it the template is
    /// still in use, when it is time to ([`super::peers::should_ask`]).
    /// `repository` is empty for the stock base. The answer is waited for in
    /// the background.
    pub fn ask_peer(self: &Arc<Self>, image: &str, repository: &str, commit: &str) {
        let Source::Peer { builder, .. } = self.provider.templates().source() else {
            return;
        };
        let Some(key) = super::portable::key_of(image) else {
            return;
        };
        let now = crate::store::now_ms();
        let state = super::peers::state(key, builder.as_deref(), now);
        if !super::peers::should_ask(key, &state, now) {
            return;
        }
        let Some((peer, name)) = super::peers::builder_for(key, builder.as_deref(), now) else {
            return;
        };
        let Some(mesh) = self.mesh.get().cloned() else {
            return;
        };
        super::peers::asked(key, &name, now);
        tracing::info!(peer = %name, %repository, %key, "asking a peer for a DigitalOcean template");
        let command = proto::frame::Command::DoTemplatePrepare {
            repository: repository.to_string(),
            commit: commit.to_string(),
            key: key.to_string(),
        };
        let key = key.to_string();
        tokio::spawn(async move {
            match mesh.command(&peer, command, PEER_ANSWER_WITHIN).await {
                Ok(_) => super::peers::answered(&key, &name, None),
                Err(crate::mesh::forward::CommandError::Refused(why)) => {
                    tracing::warn!(peer = %name, %key, %why, "a peer refused to make a DigitalOcean template");
                    super::peers::answered(&key, &name, Some(why));
                }
                // Its hello says the rest.
                Err(error) => {
                    tracing::info!(peer = %name, %key, %error, "a peer has not answered a DigitalOcean template request")
                }
            }
        });
    }

    /// Work out the stock base's portable image, once its release is known.
    async fn refresh_peer_default(&self) {
        let Some(base) = self.peer_base.get() else {
            return;
        };
        let known = super::portable::base_release(&base.base_ref)
            .await
            .map(|release| {
                super::portable::PortableInputs {
                    base: release,
                    harness: base.harness.clone(),
                    services: base.services.clone(),
                    repo: None,
                }
                .image()
            });
        if let Err(why) = &known {
            tracing::warn!(%why, "the stock base's DigitalOcean template key is not known");
        }
        *self.peer_default.lock().unwrap() = known;
    }

    /// Whether `key` is being prepared here for a peer.
    pub fn is_preparing_key(&self, key: &str) -> bool {
        self.peer_jobs.lock().unwrap().contains(key)
    }

    pub(super) fn done_preparing_key(&self, key: &str) {
        self.peer_jobs.lock().unwrap().remove(key);
    }

    /// Prepare `key`'s template for a peer in the background, unless it is
    /// being prepared already.
    pub(super) fn start_peer_job(
        self: &Arc<Self>,
        builder: super::shared::Builder,
        repo: Option<PathBuf>,
        repository: String,
        commit: String,
        key: String,
    ) {
        if !self.peer_jobs.lock().unwrap().insert(key.clone()) {
            return;
        }
        tokio::spawn(super::shared::peer_job(
            self.clone(),
            builder,
            repo,
            repository,
            commit,
            key,
        ));
    }

    /// Take `image`'s template for a peer's `key`.
    pub fn adopt(&self, image: &str, key: &str) {
        let templates = self.provider.templates();
        match templates.own_name_for(image) {
            Ok(own) => templates.shared().adopt(key, image, &own),
            Err(why) => templates.shared().fail(key, &why),
        }
    }

    pub(super) fn expect_snapshot(&self, repo: &Path, key: &str) {
        self.expected
            .lock()
            .unwrap()
            .insert(repo.to_path_buf(), key.to_string());
    }

    pub(super) fn unexpect_snapshot(&self, repo: &Path) {
        self.expected.lock().unwrap().remove(repo);
    }

    /// Prepare the template of `repo`'s new snapshot `image`, when this node
    /// has a DigitalOcean provider: taken for a peer's key first when it was
    /// prepared for one.
    pub fn prepare_snapshot_if_running(repo: &Path, image: &str) {
        if let Some(runtime) = global() {
            let key = runtime.expected.lock().unwrap().get(repo).cloned();
            if let Some(key) = key {
                runtime.adopt(image, &key);
            }
            runtime.prepare(image.to_string());
        }
    }

    /// What this node says in its hello of the templates it made for
    /// peers, when it builds them.
    pub fn advertised(&self) -> Option<serde_json::Value> {
        let templates = self.provider.templates();
        if !matches!(templates.source(), Source::Built { .. }) {
            return None;
        }
        let adverts = super::shared::Adverts {
            templates: templates
                .shared()
                .adverts(|name| templates.state_named(name)),
        };
        serde_json::to_value(adverts).ok()
    }

    /// The image a session there runs in: the first of `own` whose template
    /// is ready, else the default. The first of them has its template
    /// prepared in the background when it is not ready, for the sessions
    /// after this one.
    pub fn image_for(self: &Arc<Self>, own: &Own) -> Option<DoImage> {
        let (chosen, prepare) = choose(self.provider.templates(), own, self.default_image());
        if let Some(image) = prepare {
            self.prepare(image);
        }
        chosen
    }

    /// Register `image`'s template in the background, looking until it is
    /// ready or failed: part of preparing that image, never of a session.
    pub fn prepare(self: &Arc<Self>, image: String) {
        if self.takes_from_peer() || !self.provider.templates().serves(&image) {
            return;
        }
        if !self.preparing.lock().unwrap().insert(image.clone()) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                match runtime.ensure_template(&image).await {
                    TemplateState::Ready { .. }
                    | TemplateState::Failed { .. }
                    | TemplateState::OnPeer { .. }
                    | TemplateState::NoBuilder { .. } => break,
                    TemplateState::Preparing
                    | TemplateState::Building { .. }
                    | TemplateState::Unknown => {}
                }
                tokio::time::sleep(TEMPLATE_BUILDING_POLL).await;
            }
            runtime.preparing.lock().unwrap().remove(&image);
        });
    }

    /// The templates this node keeps: the default image's, each current
    /// snapshot's, each repository's image's, and each a peer asked for
    /// within `shared::PEER_KEEP`. `None` while that cannot be known: before
    /// the default image is (the boundary pins the base moments after the
    /// provider starts), or before the runtime and store that say which
    /// snapshots and images are current are set; nothing is swept then.
    async fn wanted(&self) -> Option<std::collections::HashSet<String>> {
        let templates = self.provider.templates();
        let mut images: Vec<String> = vec![self.default_image()?];
        if let Source::Built { .. } = templates.source() {
            let (backend, store) = self.node.get()?;
            // A runtime that keeps its snapshots in a registry lists none;
            // the store's ready rows are the current ones either way.
            images.extend(backend.prepared_snapshots().await);
            images.extend(crate::sandbox_images::ready_snapshots(store));
            // Each repository's image, prepared or not: a session there
            // starts from it whenever its snapshot is not current (a store
            // row can be ready for a key since moved on) or that snapshot's
            // template is not ready yet (`choose`).
            images.extend(crate::sandbox_images::current_repo_images(store));
        }
        Some(
            images
                .iter()
                .flat_map(|image| templates.kept(image))
                .chain(templates.shared().wanted(crate::store::now_ms()))
                .collect(),
        )
    }

    /// Look again at each template a peer still wants: registered again if
    /// it has gone, and its state known for the hello after a restart.
    fn keep_shared(self: &Arc<Self>) {
        let templates = self.provider.templates();
        let wanted: HashSet<String> = templates
            .shared()
            .wanted(crate::store::now_ms())
            .into_iter()
            .collect();
        for record in templates.shared().records() {
            if record.image.is_empty() || record.failed.is_some() || !wanted.contains(&record.name)
            {
                continue;
            }
            if !matches!(
                templates.state_named(&record.name),
                Some(TemplateState::Ready { .. })
            ) {
                self.prepare(record.image);
            }
        }
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
            if self.takes_from_peer() {
                self.refresh_peer_default().await;
            }
            let ready = match self.default_image() {
                Some(image) if self.takes_from_peer() => {
                    self.ask_peer(&image, "", "");
                    self.provider.templates().ready(&image).is_ok()
                }
                Some(image) => matches!(
                    self.ensure_template(&image).await,
                    TemplateState::Ready { .. }
                ),
                None => false,
            };
            self.keep_shared();
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
        self.end_gone_sandboxes(&client).await;
        let wanted = self.wanted().await;
        if wanted.is_none() {
            tracing::debug!("DigitalOcean templates not swept: the default image is not known yet");
        }
        let swept = upkeep::sweep(
            &client,
            &self.provider.options().owner,
            self.provider.templates(),
            wanted.as_ref(),
            crate::store::now_ms(),
        )
        .await;
        if let Ok(swept) = &swept {
            // Gone from DigitalOcean, so gone here too: a start in one of
            // their images registers it again rather than naming it.
            self.provider.templates().forget(&swept.templates);
            self.provider.templates().shared().forget(&swept.templates);
        }
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

    /// End the lifetime of every sandbox noted as running that the runtime
    /// no longer lists: lost, or removed by a sweep or an earlier process.
    async fn end_gone_sandboxes(&self, client: &crate::do_harness::Client) {
        if self.provider.meter().store().is_none() {
            return;
        }
        let owner = &self.provider.options().owner;
        let listed_ms = crate::store::now_ms();
        match client
            .list_sessions(&crate::do_harness::SessionFilter::default())
            .await
        {
            Ok(sessions) => {
                let live = sessions
                    .into_iter()
                    .filter(|s| owner.owns(&s.name) && !s.status.is_terminal())
                    .map(|s| s.session_id)
                    .collect();
                let ended = self.provider.meter().reconcile(&live, listed_ms);
                if ended > 0 {
                    tracing::info!(
                        ended,
                        "ended the lifetimes of DigitalOcean sandboxes found gone"
                    );
                }
            }
            Err(error) => tracing::debug!(%error, "could not list DigitalOcean sessions to meter"),
        }
    }

    /// Why sessions cannot run there now, or `Ok`: what the node needs on
    /// its side (trees over the files channel, gateways over tunnels), the
    /// template, and the relay's name when it is the way in.
    pub fn health(&self, cfg: &Config) -> Result<(), String> {
        if !crate::boundary::sandboxes_over_protocol(cfg) {
            return Err(
                "a DigitalOcean sandbox needs this node to run its own sandboxes over the \
                 sandbox protocol: it mounts nothing and has no gateway of its own"
                    .into(),
            );
        }
        let image = match self.takes_from_peer() {
            true => self.peer_default.lock().unwrap().clone()?,
            false => self
                .default_image()
                .ok_or("this node has not pinned its base image yet")?,
        };
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

    /// Today's spend on sandboxes against `[spend] infra_usd_per_day`, once
    /// the node's store is known.
    pub fn spend(&self, cfg: &Config) -> Option<crate::metrics::InfraSpend> {
        let (_, store) = self.node.get()?;
        Some(crate::metrics::infra_spend(store, cfg))
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
            closed: self.spend(cfg).filter(|s| s.at()).map(|s| s.reason()),
            room: None,
        }
    }

    /// The provider as the Settings capability view shows it: what it
    /// promises of every sandbox, by what makes it so, how its sandboxes
    /// reach the node, and whether it can take a session now and why not.
    pub async fn report(&self, cfg: &Config) -> crate::provider::CapabilityReport {
        use crate::provider::capabilities::{CapabilityLine, Elsewhere, Route, ServiceSupport};
        let candidate = self.candidate(cfg).await;
        let sizing = self.sizing(cfg).await;
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
            room: None,
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
                size: sizing.default.as_ref().ok().map(|c| c.slug.clone()),
                memory_mb: sizing.default.as_ref().ok().and_then(|c| c.memory_mb),
                price_per_hour_usd: sizing
                    .default
                    .as_ref()
                    .ok()
                    .and_then(|c| c.price_per_hour_usd),
                size_error: sizing.default.err(),
                configured_size: Some(self.provider.options().settings.size.clone())
                    .filter(|s| !s.is_empty()),
                catalog_error: sizing.catalog_error,
                floors: sizing.floors,
                spend: self.spend(cfg),
            }),
        }
    }

    /// The size a new sandbox is made at with no floor, and each live
    /// `[placement]` `require.memory` floor with the size it resolves to:
    /// what `create` would choose now (`meter::choose`).
    async fn sizing(&self, cfg: &Config) -> Sizing {
        use super::meter::{choose, floor_mb};
        let configured = &self.provider.options().settings.size;
        let catalog = match self.provider.client() {
            Ok(client) => self.provider.meter().catalog(&client).await,
            Err(error) => Err(error.to_string()),
        };
        let (sizes, catalog_error) = match catalog {
            Ok(sizes) => (sizes, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let chosen = |floor: Option<u64>| choose(&sizes, configured, floor.map(floor_mb));
        let placement = &cfg.live().placement;
        let entries = placement
            .channel
            .iter()
            .map(|(name, c)| (format!("channel {name}"), &c.require))
            .chain(
                placement
                    .repo
                    .iter()
                    .map(|r| (r.path.display().to_string(), &r.require)),
            );
        let floors = entries
            .filter_map(|(scope, require)| {
                let memory = require.memory.clone()?;
                let size = chosen(require.memory_bytes());
                Some(crate::provider::capabilities::SizeFloor {
                    scope,
                    memory,
                    size: size.as_ref().ok().map(|c| c.slug.clone()),
                    memory_mb: size.as_ref().ok().and_then(|c| c.memory_mb),
                    price_per_hour_usd: size.as_ref().ok().and_then(|c| c.price_per_hour_usd),
                    error: size.err(),
                })
            })
            .collect();
        Sizing {
            default: chosen(None),
            catalog_error,
            floors,
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

    /// The runner a session placed here runs in: its trees over the files
    /// channel from `files` (where this node's own sandboxes keep theirs),
    /// its gateways through tunnels, its isolation read back and held to
    /// `requirements` before anything reaches it.
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
                .with_tunnels(do_tunnels(cfg))
                .with_env(self.sandbox_env(cfg, harness_id))
                .with_sandbox_services(true)
                .with_service_refusal(refusal)
                // The memory floor sizes each sandbox (`meter::choose`).
                .with_resources(crate::provider::Resources {
                    memory_bytes: requirements.memory_bytes(),
                    ..Default::default()
                })
                .with_requirements(requirements)
                .with_token_window(TOKEN_WINDOW),
        )
    }
}

impl DoRuntime {
    /// The template a review's checks in `image` start from here, waiting
    /// up to `within` while it is registered and built, looking every
    /// `every`; or why there is none: this node makes none for `image` (an
    /// image of the operator's names only itself, and a peer's templates
    /// are made for sessions' images alone), it failed, it was not ready in
    /// time, or the checks were withdrawn meanwhile.
    pub async fn check_template(
        &self,
        image: &str,
        within: Duration,
        every: Duration,
        cancel: &dyn crate::review::checks::Cancel,
    ) -> Result<String, String> {
        let templates = self.provider.templates();
        if self.takes_from_peer() || !templates.serves(image) {
            return Err(format!(
                "DigitalOcean makes no template for {image} on this node"
            ));
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Ok(template) = templates.ready(image) {
                return Ok(template);
            }
            match self.ensure_template(image).await {
                TemplateState::Ready { .. } => {
                    if let Ok(template) = templates.ready(image) {
                        return Ok(template);
                    }
                }
                state @ (TemplateState::Failed { .. }
                | TemplateState::OnPeer { .. }
                | TemplateState::NoBuilder { .. }) => return Err(state.describe()),
                TemplateState::Preparing
                | TemplateState::Building { .. }
                | TemplateState::Unknown => {}
            }
            if let Some(reason) = cancel.cancelled() {
                return Err(format!("the checks were withdrawn: {reason}"));
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(format!(
                    "its DigitalOcean template was not ready within {} minutes: {}",
                    within.as_secs() / 60,
                    templates.state(image).describe()
                ));
            }
            tokio::time::sleep(every.min(deadline - now)).await;
        }
    }

    /// What runs a review's checks here, as forks of one prepared sandbox
    /// (`review::forked`): each sandbox held to `requirements` and sized by
    /// their memory floor, its tree sent from `files` (where this node's own
    /// sandboxes keep theirs) and, while it prepares, the node's gateways
    /// through tunnels.
    pub fn check_forks(
        &self,
        cfg: &Config,
        requirements: Requirements,
        files: crate::runner::sandbox_files::FilesStore,
        repo: Option<String>,
    ) -> Result<crate::review::forked::Forks, String> {
        let server = crate::runner::protocol::node_server().map_err(|e| e.to_string())?;
        Ok(crate::review::forked::Forks {
            kind: ProviderKind::DigitalOcean,
            provider: self.provider.clone(),
            server,
            files,
            tunnels: do_tunnels(cfg),
            making: crate::sandbox_images::Making {
                kind: crate::runner::RunKind::Check,
                labels: crate::runner::RunLabels {
                    repo,
                    ..crate::runner::RunLabels::of(crate::runner::RunKind::Check)
                },
                resources: crate::provider::Resources {
                    memory_bytes: requirements.memory_bytes(),
                    ..Default::default()
                },
                token_window: Some(TOKEN_WINDOW),
            },
            requirements,
            workdir: crate::sandbox_images::PREPARED_TREE.to_string(),
        })
    }
}

/// What the Settings view shows of sandbox sizes.
struct Sizing {
    default: Result<super::meter::Chosen, String>,
    catalog_error: Option<String>,
    floors: Vec<crate::provider::capabilities::SizeFloor>,
}

/// The tunnels a sandbox there gets to the node's gateways: those this
/// node's own sandboxes get, which lead to the listeners its runtime serves.
/// A pod-hosted node serves its allowlist proxy itself on a port, where a
/// Podman or Apple node serves it on a socket.
pub fn do_tunnels(cfg: &Config) -> Vec<crate::runner::tunnels::TunnelSpec> {
    match cfg.runtime.kind {
        RuntimeKind::Kubernetes => crate::runner::tunnels::kube_tunnels(cfg),
        RuntimeKind::Podman | RuntimeKind::Apple => crate::runner::tunnels::gateway_tunnels(cfg),
    }
}

/// What builds `[digitalocean] registry`'s template sources here: Podman
/// itself (`None`), or a Kubernetes node's BuildKit. DigitalOcean runs
/// amd64 alone, and either builds for this machine.
fn source_builder(
    cfg: &Config,
    backend: &dyn Backend,
) -> Result<Option<Arc<dyn ImageBuilder>>, String> {
    let arch = crate::boundary::registry::host_architecture();
    if arch != "amd64" {
        return Err(format!(
            "digitalocean.registry builds templates for amd64, which is all DigitalOcean runs, \
             and this node is {arch}: take them from a peer (templates = \"peer\")"
        ));
    }
    match cfg.runtime.kind {
        RuntimeKind::Kubernetes => backend.registry_builder().map(Some).ok_or_else(|| {
            "digitalocean.registry builds templates on [runtime.kubernetes.builder] here, \
             which is not usable: see the runtime check"
                .to_string()
        }),
        RuntimeKind::Podman | RuntimeKind::Apple => Ok(None),
    }
}

/// Whether `service` runs in a sandbox made from `template`, which holds
/// the service bundles `held` (`None`: an image of the operator's, taken on
/// trust). It runs inside, under the sandbox's supervisor, from the
/// template's copy: nothing is mounted there, so one whose bundle that
/// template does not hold cannot. Its traffic is the sandbox's own: the
/// node's allowlist proxy through the tunnel, never a wider egress.
fn service_support(
    service: &crate::config::Service,
    held: Option<&[String]>,
    template: Option<&str>,
) -> Result<(), String> {
    match held {
        Some(held) if !held.contains(&service.name) => Err(format!(
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
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Repo;
    use crate::store::{RepoImageStart, Store};

    const BASE: &str = "ghcr.io/cosmicspork/tracon-sandbox-base@sha256:c9d629a811111111222222222222222233333333333333334444444444444444";
    const REPO: &str = "localhost/tracon-repo-sql-review-0123456789@sha256:5555555555555555666666666666666677777777777777778888888888888888";
    const SNAPSHOT: &str = "localhost/tracon-snapshot-sql-review-0123@sha256:9999999999999999aaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbcccccccccccccccc";

    fn built() -> Templates {
        Templates::new(
            Source::Built {
                registry: "registry.digitalocean.com/acme".into(),
                prefix: "tracon".into(),
                bundle: "/state/bundles/0.51.2/amd64".into(),
                services: Vec::new(),
            },
            "coding-base",
            "tracon-tabcd1234",
        )
        .unwrap()
    }

    fn ready(store: &Store, repo: &str, kind: &str, image: &str) {
        let row = store
            .start_repo_image(&RepoImageStart {
                repo_path: repo,
                kind,
                recipe_hash: "k1",
                source_ref: "main",
                source_commit: "abc",
                parent_image: BASE,
            })
            .unwrap();
        store.finish_repo_image(&row, Ok(image), "", &[]).unwrap();
    }

    /// `entry`'s environment for /src/sql-review, with its image built when
    /// `built` says so.
    fn environment(entry: &Repo, built: bool) -> crate::environment::RepoEnvironment {
        let store = Store::open_in_memory().unwrap();
        if built {
            ready(&store, "/src/sql-review", crate::repo_image::BASE, REPO);
        }
        crate::environment::environment_with(
            &Config::default(),
            &store,
            Some(Path::new("/src/sql-review")),
            Some(entry),
        )
    }

    fn build_only() -> Repo {
        Repo {
            path: "/src/sql-review".into(),
            build: vec!["curl https://sh.rustup.rs | sh -s -- -y".into()],
            ..Default::default()
        }
    }

    /// A repository whose entry builds its image and prepares nothing runs
    /// there in a template made from that image, once it is ready; until
    /// then, on the stock base, with why, and its template is made.
    #[test]
    fn a_build_only_entry_runs_in_its_images_template() {
        let templates = built();
        let own = Own::of(None, &environment(&build_only(), true));
        assert_eq!(own.images, vec![REPO.to_string()]);
        assert_eq!(own.missing, None);

        let (chosen, prepare) = choose(&templates, &own, Some(BASE.into()));
        let chosen = chosen.unwrap();
        assert_eq!(chosen.image, BASE);
        let note = chosen.note.unwrap();
        assert!(
            note.starts_with("no DigitalOcean template for sql-review: its template"),
            "{note}"
        );
        assert_eq!(prepare.as_deref(), Some(REPO), "its template is made");

        templates.seen_ready(REPO);
        let (chosen, prepare) = choose(&templates, &own, Some(BASE.into()));
        assert_eq!(
            chosen,
            Some(DoImage {
                image: REPO.into(),
                note: None
            })
        );
        assert_eq!(prepare, None);
    }

    /// An entry that neither builds nor prepares runs on the stock base,
    /// which is all it gets on the node's own runtime too: nothing to say.
    #[test]
    fn an_entry_that_builds_and_prepares_nothing_runs_on_the_stock_base() {
        let plain = Repo {
            path: "/src/sql-review".into(),
            ..Default::default()
        };
        let own = Own::of(None, &environment(&plain, false));
        assert_eq!(
            own,
            Own {
                repository: Some("sql-review".into()),
                ..Default::default()
            }
        );
        let (chosen, prepare) = choose(&built(), &own, Some(BASE.into()));
        assert_eq!(
            chosen,
            Some(DoImage {
                image: BASE.into(),
                note: None
            })
        );
        assert_eq!(prepare, None);
    }

    /// One whose image is not built here, or that prepares on the stock
    /// base with no snapshot made yet, says so rather than running on the
    /// stock base unremarked.
    #[test]
    fn a_repository_without_its_image_here_says_so() {
        let own = Own::of(None, &environment(&build_only(), false));
        assert!(own.images.is_empty());
        let (chosen, prepare) = choose(&built(), &own, Some(BASE.into()));
        let chosen = chosen.unwrap();
        assert_eq!(chosen.image, BASE);
        assert_eq!(
            chosen.note.as_deref(),
            Some("no DigitalOcean template for sql-review: its image has not been built on this node yet")
        );
        assert_eq!(prepare, None);

        let prepares = Repo {
            path: "/src/sql-review".into(),
            prepare: vec!["cargo fetch --locked".into()],
            ..Default::default()
        };
        let own = Own::of(None, &environment(&prepares, false));
        let note = choose(&built(), &own, Some(BASE.into()))
            .0
            .unwrap()
            .note
            .unwrap();
        assert!(
            note.contains("prepared snapshot has not been made"),
            "{note}"
        );
    }

    /// A prepared snapshot comes first; while its template builds, the
    /// image it was prepared in serves if its template is ready, and only
    /// the snapshot's template is made.
    #[test]
    fn a_snapshot_whose_template_builds_falls_back_to_its_image() {
        let templates = built();
        let entry = Repo {
            prepare: vec!["cargo fetch --locked".into()],
            ..build_only()
        };
        let own = Own::of(Some(SNAPSHOT.into()), &environment(&entry, true));
        assert_eq!(own.images, vec![SNAPSHOT.to_string(), REPO.to_string()]);
        templates.seen_ready(REPO);
        let (chosen, prepare) = choose(&templates, &own, Some(BASE.into()));
        let chosen = chosen.unwrap();
        assert_eq!(chosen.image, REPO);
        assert!(
            chosen
                .note
                .as_deref()
                .unwrap()
                .contains("prepared snapshot"),
            "{chosen:?}"
        );
        assert_eq!(prepare.as_deref(), Some(SNAPSHOT));
        templates.seen_ready(SNAPSHOT);
        let (chosen, _) = choose(&templates, &own, Some(BASE.into()));
        assert_eq!(chosen.unwrap().image, SNAPSHOT);
    }

    /// The operator's one image serves every session, which is said.
    #[test]
    fn the_operators_image_is_said_to_stand_in() {
        let given = "registry.example/team/sandbox@sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let templates = Templates::new(Source::Given(given.into()), "coding-base", "t").unwrap();
        let own = Own::of(None, &environment(&build_only(), true));
        let (chosen, prepare) = choose(&templates, &own, Some(given.into()));
        let chosen = chosen.unwrap();
        assert_eq!(chosen.image, given);
        assert!(
            chosen
                .note
                .as_deref()
                .unwrap()
                .contains("[digitalocean] image"),
            "{chosen:?}"
        );
        assert_eq!(prepare, None);
    }

    /// Each repository's current image has its template kept from the
    /// sweep, whether or not a prepared snapshot is ready: a session there
    /// starts from the image when that snapshot is not the current one
    /// (`snapshot_for` holds a ready row to its key) or its template is not
    /// ready yet. A session carrying on another's workspace chooses the
    /// same way, from the repository that workspace came from.
    #[test]
    fn each_repositorys_image_is_kept_prepared_or_not() {
        let store = Store::open_in_memory().unwrap();
        ready(&store, "/src/sql-review", crate::repo_image::BASE, REPO);
        ready(&store, "/src/app", crate::repo_image::BASE, BASE);
        ready(
            &store,
            "/src/app",
            crate::sandbox_images::SNAPSHOT,
            SNAPSHOT,
        );
        assert_eq!(
            crate::sandbox_images::current_repo_images(&store),
            vec![BASE.to_string(), REPO.to_string()]
        );
        assert_eq!(
            crate::sandbox_images::ready_snapshots(&store),
            vec![SNAPSHOT.to_string()]
        );

        // A snapshot that is not current leaves the repository's sessions on
        // its image, whose template is chosen and made.
        let entry = Repo {
            prepare: vec!["cargo fetch --locked".into()],
            ..build_only()
        };
        let own = Own::of(None, &environment(&entry, true));
        assert_eq!(own.images, vec![REPO.to_string()]);
        let templates = built();
        let (_, prepare) = choose(&templates, &own, Some(BASE.into()));
        assert_eq!(prepare.as_deref(), Some(REPO));
    }
}
