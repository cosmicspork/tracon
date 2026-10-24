//! Connecting a model provider: the node runs the provider's own OAuth sign-in
//! (`crate::oauth`) and keeps the tokens in the broker as an `oauth`
//! credential.

pub mod callback;
pub mod mesh;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use parking_lot::Mutex;
use proto::envelope::DataKey;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use self::callback::{CallbackCapture, CallbackError, CallbackOutcome, CaptureEvent, CaptureReply};
use self::mesh::{claim_key, ClaimResult, CredentialMesh};
use crate::{
    broker::{self, Broker, Credential, SharedBroker, KIND_API_KEY, KIND_OAUTH},
    config::{Config, Provider},
    oauth::{self, anthropic, codex, Endpoints, Flow, OAuthError, Pkce, Tokens},
    store::now_ms,
    stream::{Bus, Frame},
};

/// How far ahead of expiry a credential is renewed by a node that renews
/// first: one set to (`[mesh] renew_credentials`), or the only holder.
const REFRESH_AHEAD_MS: i64 = 30 * 60 * 1000;
/// How far ahead every other holder of a shared credential steps in, leaving
/// the renewing node the first half hour.
const FALLBACK_REFRESH_AHEAD_MS: i64 = 15 * 60 * 1000;
pub const REFRESH_TICK_SECS: u64 = 5 * 60;
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const MAX_MANUAL_INPUT: usize = 8 * 1024;

/// The provider state for a credential that can only be replaced by signing in
/// again. Not "failed": nothing went wrong and nothing is going to be retried,
/// the operator simply has to reconnect.
const NEEDS_RECONNECT: &str = "needs_reconnect";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOwner {
    Local,
    Peer(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginCompletion {
    LocalCallback,
    Paste,
    DeviceCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectResult {
    pub url: String,
    pub completion: LoginCompletion,
    pub completion_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_code: Option<String>,
}

struct Inflight {
    generation: u64,
    owner: LoginOwner,
    channels: Vec<String>,
    share: bool,
    started_ms: i64,
    cancel: CancellationToken,
    state: InflightState,
    /// For a sign-in a peer started: the admin statement its operator
    /// confirmed the connect with (`mesh::admin`), which the pasted code
    /// that finishes this sign-in rides on ([`Providers::confirmed_login`]).
    confirmed: Option<String>,
}

/// How long a peer's confirmed connect also covers the code that finishes
/// it. Past it, the code needs a confirmation of its own.
pub const CONFIRMED_LOGIN_MS: i64 = 15 * 60 * 1000;

enum InflightState {
    Starting,
    Pending(PendingLogin),
}

struct PendingLogin {
    result: ConnectResult,
    grant: Grant,
    capture: Option<CallbackCapture>,
    completion: Arc<Mutex<CompletionState>>,
}

/// What finishing a sign-in needs that the operator never sees.
#[derive(Clone)]
enum Grant {
    /// An authorization code comes back, by callback or paste, and is
    /// exchanged against this verifier at this redirect.
    Browser {
        flow: Flow,
        redirect: String,
        state: String,
        verifier: String,
    },
    /// The node polls until the operator approves; nothing comes back.
    Device,
}

/// A sign-in the provider has accepted the start of, before it is installed.
struct Started {
    result: ConnectResult,
    grant: Grant,
    capture: Option<(
        CallbackCapture,
        tokio::sync::mpsc::UnboundedReceiver<CaptureEvent>,
    )>,
    device: Option<codex::DeviceStart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionState {
    Open,
    Claimed(LoginCompletion),
    Finished,
}

enum StoreKind {
    /// A new sign-in, bound to these channels and held by these nodes.
    Fresh {
        channels: Vec<String>,
        nodes: Vec<String>,
    },
    /// New tokens for the sign-in held under this credential name.
    Refresh { credential: String },
}

/// What storing a sign-in changed in the broker.
struct Stored {
    name: String,
    credential: Credential,
    /// Other sign-ins here that gave up the channels this one took, to hand
    /// off as a change of channels is.
    narrowed: Vec<(String, Credential)>,
    /// Sign-ins it replaced outright, every channel they served now its.
    removed: Vec<String>,
}

#[derive(Debug, Clone)]
struct Note {
    state: &'static str,
    error: Option<String>,
    updated_ms: i64,
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreshed {
    Renewed,
    /// Another holder of the shared credential renews this version; its copy
    /// arrives by handoff.
    LeftToAnotherHolder,
}

/// What removing one of a provider's sign-ins takes from the node
/// ([`Providers::displaced`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Displaced {
    /// The credential the sign-in is held under.
    pub sign_in: String,
    /// The channels that lose the provider on this node: those the sign-in
    /// serves that no other sign-in here would.
    pub channels: Vec<String>,
    /// The other nodes holding a shared copy of the sign-in. A disconnect
    /// here leaves their copies.
    pub peers: Vec<String>,
}

pub struct Providers {
    cfg: Arc<Config>,
    broker: SharedBroker,
    store_key: DataKey,
    endpoints: Endpoints,
    http: reqwest::Client,
    node_id: String,
    bus: Bus,
    inflight: Mutex<HashMap<String, Inflight>>,
    /// Per provider: how its last sign-in attempt went.
    notes: Mutex<HashMap<String, Note>>,
    /// Per credential: why a sign-in could not be renewed. Kept apart from
    /// `notes`, whose keys are provider names a credential's often equals.
    renewals: Mutex<HashMap<String, Note>>,
    next_generation: AtomicU64,
    on_connected: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
    on_publish: std::sync::OnceLock<Box<dyn Fn(Vec<serde_json::Value>) + Send + Sync>>,
    mesh: std::sync::OnceLock<Arc<dyn CredentialMesh>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no provider named {0}")]
    Unknown(String),
    #[error("provider {0} has no login flow; import an API key with `tracon credential import`")]
    NoLogin(String),
    #[error("a login for {0} is already in progress")]
    Busy(String),
    #[error("no login in progress for {0}")]
    NotPending(String),
    #[error("that login belongs to another node")]
    WrongOwner,
    #[error("connected providers must be managed on their owning node")]
    RemoteDisconnect,
    #[error("{0}")]
    Failed(String),
    /// The credential the provider names holds something an API key must
    /// not replace.
    #[error("{0}")]
    Conflict(String),
    /// The credential has run out and nothing can renew it: connect again.
    #[error("{0}")]
    ReconnectRequired(String),
}

impl Providers {
    pub fn new(
        cfg: Arc<Config>,
        broker: SharedBroker,
        store_key: DataKey,
        node_id: String,
        bus: Bus,
    ) -> Arc<Self> {
        Self::with_endpoints(cfg, broker, store_key, Endpoints::default(), node_id, bus)
    }

    pub fn with_endpoints(
        cfg: Arc<Config>,
        broker: SharedBroker,
        store_key: DataKey,
        endpoints: Endpoints,
        node_id: String,
        bus: Bus,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            broker,
            store_key,
            endpoints,
            http: oauth::client(),
            node_id,
            bus,
            inflight: Mutex::new(HashMap::new()),
            notes: Mutex::new(HashMap::new()),
            renewals: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            on_connected: std::sync::OnceLock::new(),
            on_publish: std::sync::OnceLock::new(),
            mesh: std::sync::OnceLock::new(),
        })
    }

    pub fn set_on_connected(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_connected.set(f);
    }

    pub fn set_on_publish(&self, f: Box<dyn Fn(Vec<serde_json::Value>) + Send + Sync>) {
        let _ = self.on_publish.set(f);
    }

    /// The mesh a credential is shared, claimed and handed off through.
    /// Without one, this node is the only holder of anything it signs in to.
    pub fn set_mesh(&self, mesh: Arc<dyn CredentialMesh>) {
        let _ = self.mesh.set(mesh);
    }

    /// A handoff stored these credentials: what the node says about its
    /// providers follows, and a provider waiting on a new sign-in no longer is.
    pub fn handoff_received(&self, credential_names: &[String]) {
        let arrived = self.providers_signed_in_by(credential_names);
        if arrived.is_empty() {
            return;
        }
        {
            let mut notes = self.notes.lock();
            for name in &arrived {
                notes.remove(name);
            }
        }
        // A copy that arrived is a sign-in renewed or made elsewhere: what
        // was known about renewing the one it replaces no longer holds.
        self.forget_renewals(credential_names);
        if let Some(callback) = self.on_connected.get() {
            callback();
        }
        self.publish();
    }

    pub fn list_private(&self) -> Vec<Value> {
        self.list(true)
    }

    pub fn list_public(&self) -> Vec<Value> {
        self.list(false)
    }

    fn list(&self, private: bool) -> Vec<Value> {
        let broker = self.broker.read().unwrap();
        let inflight = self.inflight.lock();
        let notes = self.notes.lock();
        let renewals = self.renewals.lock();
        self.cfg
            .providers
            .iter()
            .map(|(name, provider)| {
                let base = provider.credential.as_str();
                let cred = broker
                    .primary_sign_in(name, base, &self.node_id)
                    .filter(|(_, c)| c.is_model());
                let held = self.held_sign_ins(&broker, name, provider);
                let stalled = |credential: &str| {
                    renewals
                        .get(credential)
                        .filter(|note| note.state == NEEDS_RECONNECT)
                };
                let (state, result, error, updated_ms) = if let Some(slot) = inflight.get(name) {
                    let result = match &slot.state {
                        InflightState::Starting => None,
                        InflightState::Pending(pending) => Some(pending.result.clone()),
                    };
                    ("pending", result, None, Some(slot.started_ms))
                // A credential that can no longer be renewed is still in the
                // broker, so this has to come before "connected" or the
                // interface would keep calling it healthy until a request
                // failed.
                } else if let Some(note) = cred.and_then(|(credential, _)| stalled(credential)) {
                    (
                        note.state,
                        None,
                        private.then(|| note.error.clone()).flatten(),
                        Some(note.updated_ms),
                    )
                } else if let Some((_, credential)) = cred {
                    ("connected", None, None, credential.expires_ms)
                } else if let Some(note) = notes.get(name) {
                    (
                        note.state,
                        None,
                        private.then(|| note.error.clone()).flatten(),
                        Some(note.updated_ms),
                    )
                } else {
                    ("disconnected", None, None, None)
                };
                // Each sign-in by the channels it actually serves here: one
                // a later sign-in took a channel from no longer serves it,
                // though a copy shared from a peer may still be bound to it.
                let sign_ins: Vec<Value> = held
                    .iter()
                    .map(|(credential_name, c)| {
                        let serves: Vec<&String> = c
                            .channels
                            .iter()
                            .filter(|channel| {
                                broker
                                    .model_credential(name, base, channel, &self.node_id)
                                    .is_some_and(|(chosen, _)| chosen == *credential_name)
                            })
                            .collect();
                        let stalled = stalled(credential_name);
                        let mut row = json!({
                            "credential": credential_name,
                            "state": stalled.map_or("connected", |note| note.state),
                            "kind": c.kind,
                            "identity": c.identity,
                            "expires_ms": c.expires_ms,
                            "channels": serves,
                            "bound": c.channels,
                        });
                        if private {
                            let object = row.as_object_mut().expect("sign-in summary object");
                            object.insert(
                                "error".into(),
                                json!(stalled.and_then(|note| note.error.clone())),
                            );
                            object.insert("shared_with".into(), json!(self.peers_of(c)));
                        }
                        row
                    })
                    .collect();
                let mut channels: Vec<String> = Vec::new();
                for row in &sign_ins {
                    for channel in row["channels"].as_array().into_iter().flatten() {
                        if let Some(channel) = channel.as_str() {
                            if !channels.iter().any(|held| held == channel) {
                                channels.push(channel.to_string());
                            }
                        }
                    }
                }
                let mut summary = json!({
                    "name": name,
                    "state": state,
                    "kind": cred.map(|(_, c)| c.kind.clone()),
                    "can_login": provider.login.as_deref().and_then(Flow::for_login).is_some(),
                    "declared": self.cfg.declared_providers.contains(name),
                    "identity": cred.and_then(|(_, c)| c.identity.clone()),
                    "expires_ms": cred.and_then(|(_, c)| c.expires_ms),
                    "channels": channels,
                    "updated_ms": updated_ms,
                    "sign_ins": sign_ins,
                });
                if private {
                    let object = summary.as_object_mut().expect("provider summary object");
                    object.insert(
                        "url".into(),
                        result
                            .as_ref()
                            .map(|value| json!(value.url))
                            .unwrap_or(Value::Null),
                    );
                    object.insert(
                        "completion".into(),
                        result
                            .as_ref()
                            .map(|value| json!(value.completion))
                            .unwrap_or(Value::Null),
                    );
                    object.insert(
                        "completion_note".into(),
                        result
                            .as_ref()
                            .and_then(|value| value.completion_note.clone())
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    );
                    object.insert(
                        "device_code".into(),
                        result
                            .and_then(|value| value.device_code)
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    );
                    object.insert(
                        "error".into(),
                        error.map(Value::String).unwrap_or(Value::Null),
                    );
                    // The other nodes holding a copy of the primary sign-in,
                    // for a client that predates `sign_ins`.
                    object.insert(
                        "shared_with".into(),
                        json!(cred.map(|(_, c)| self.peers_of(c)).unwrap_or_default()),
                    );
                }
                summary
            })
            .collect()
    }

    /// The model sign-ins `name` holds on this node, the one it is described
    /// by ([`Broker::primary_sign_in`]) first and the rest by name.
    fn held_sign_ins<'a>(
        &self,
        broker: &'a Broker,
        name: &str,
        provider: &Provider,
    ) -> Vec<(&'a str, &'a Credential)> {
        let primary = broker
            .primary_sign_in(name, &provider.credential, &self.node_id)
            .map(|(credential, _)| credential);
        let mut held: Vec<_> = broker
            .family(name, &provider.credential)
            .filter(|(_, c)| c.is_model() && c.allows_node(&self.node_id))
            .collect();
        held.sort_by_key(|(credential, _)| (Some(*credential) != primary, *credential));
        held
    }

    /// The nodes other than this one that hold a copy of `credential`.
    fn peers_of(&self, credential: &Credential) -> Vec<String> {
        credential
            .nodes
            .iter()
            .filter(|node| **node != self.node_id)
            .cloned()
            .collect()
    }

    /// The providers one of `credential_names` is a sign-in of, by name.
    fn providers_signed_in_by(&self, credential_names: &[String]) -> Vec<String> {
        self.cfg
            .providers
            .iter()
            .filter(|(_, provider)| {
                credential_names.iter().any(|credential| {
                    *credential == provider.credential
                        || broker::is_sign_in_name(credential, &provider.credential)
                })
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn forget_renewals(&self, credential_names: &[String]) {
        let mut renewals = self.renewals.lock();
        for credential in credential_names {
            renewals.remove(credential);
        }
    }

    fn publish(&self) {
        if let Some(callback) = self.on_publish.get() {
            callback(self.list_public());
        }
        self.bus.publish(Frame::Providers {
            providers: self.list_private(),
        });
    }

    fn note(&self, name: &str, state: &'static str, error: Option<String>) {
        self.notes.lock().insert(
            name.to_string(),
            Note {
                state,
                error,
                updated_ms: now_ms(),
            },
        );
    }

    fn flow(&self, name: &str) -> Result<Flow, ProviderError> {
        let provider = self
            .cfg
            .providers
            .get(name)
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))?;
        provider
            .login
            .as_deref()
            .and_then(Flow::for_login)
            .ok_or_else(|| ProviderError::NoLogin(name.to_string()))
    }

    /// Start a sign-in. `local_callback` means the browser is on this node's
    /// host, so a loopback redirect reaches it; otherwise the provider's own
    /// page shows a code (Anthropic) or the device flow runs (Codex).
    ///
    /// `share` hands the credential, once signed in, to every other member
    /// bound to one of `channels`: one sign-in for the mesh.
    pub async fn connect(
        self: &Arc<Self>,
        name: &str,
        channels: Vec<String>,
        share: bool,
        owner: LoginOwner,
        local_callback: bool,
    ) -> Result<ConnectResult, ProviderError> {
        let flow = self.flow(name)?;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        {
            let mut inflight = self.inflight.lock();
            if let Some(existing) = inflight.get(name) {
                if existing.owner == owner {
                    if let InflightState::Pending(pending) = &existing.state {
                        return Ok(pending.result.clone());
                    }
                }
                return Err(ProviderError::Busy(name.to_string()));
            }
            inflight.insert(
                name.to_string(),
                Inflight {
                    generation,
                    owner,
                    channels,
                    share,
                    started_ms: now_ms(),
                    cancel: cancel.clone(),
                    state: InflightState::Starting,
                    confirmed: None,
                },
            );
        }

        let started = match self.start(flow, local_callback).await {
            Ok(started) => started,
            Err(error) => {
                self.remove_generation(name, generation);
                return Err(error);
            }
        };
        let Started {
            result,
            grant,
            capture,
            device,
        } = started;
        let (capture, capture_events) = match capture {
            Some((capture, events)) => (Some(capture), Some(events)),
            None => (None, None),
        };
        let installed = {
            let mut inflight = self.inflight.lock();
            match inflight.get_mut(name) {
                Some(slot)
                    if slot.generation == generation
                        && matches!(slot.state, InflightState::Starting) =>
                {
                    slot.state = InflightState::Pending(PendingLogin {
                        result: result.clone(),
                        grant,
                        capture: capture.clone(),
                        completion: Arc::new(Mutex::new(CompletionState::Open)),
                    });
                    true
                }
                _ => false,
            }
        };
        if !installed {
            if let Some(capture) = &capture {
                capture.stop();
            }
            return Err(ProviderError::NotPending(name.to_string()));
        }
        self.notes.lock().remove(name);
        self.publish();

        if let Some(mut events) = capture_events {
            let providers = self.clone();
            let provider = name.to_string();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    match event {
                        CaptureEvent::Request(request) => match request.outcome {
                            CallbackOutcome::Code(url) => {
                                let reply = match providers
                                    .submit(
                                        &provider,
                                        generation,
                                        &url,
                                        LoginCompletion::LocalCallback,
                                    )
                                    .await
                                {
                                    Ok(()) => CaptureReply::success(),
                                    Err(_) => CaptureReply::failed(),
                                };
                                let succeeded = reply.status == hyper::StatusCode::OK;
                                let _ = request.reply.send(reply);
                                if succeeded {
                                    break;
                                }
                            }
                            CallbackOutcome::Denied => {
                                providers
                                    .terminal(&provider, generation, "Sign-in was not authorized.")
                                    .await;
                                let _ = request.reply.send(CaptureReply::denied());
                                break;
                            }
                        },
                        CaptureEvent::ListenerFailed => {
                            providers
                                .terminal(
                                    &provider,
                                    generation,
                                    "Local callback listener failed; connect again.",
                                )
                                .await;
                            break;
                        }
                    }
                }
            });
        }

        if let Some(device) = device {
            let providers = self.clone();
            let provider = name.to_string();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                providers
                    .poll_device(&provider, generation, device, cancel)
                    .await;
            });
        }

        let providers = self.clone();
        let provider = name.to_string();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = tokio::time::sleep(LOGIN_TIMEOUT) => {
                    providers
                        .terminal(&provider, generation, "Sign-in timed out; connect again.")
                        .await;
                }
            }
        });

        Ok(result)
    }

    async fn start(&self, flow: Flow, local_callback: bool) -> Result<Started, ProviderError> {
        match flow {
            Flow::Anthropic => {
                let pkce = Pkce::new();
                let state = oauth::random_token(32);
                let mut completion_note = None;
                let listened = if local_callback {
                    match CallbackCapture::listen(0, "/callback", &state) {
                        Ok(listened) => Some(listened),
                        Err(_) => {
                            completion_note = Some(
                                "The local callback could not start; paste the code the sign-in page shows."
                                    .into(),
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let (redirect, completion, capture) = match listened {
                    Some((capture, events, port)) => (
                        anthropic::loopback_redirect(port),
                        LoginCompletion::LocalCallback,
                        Some((capture, events)),
                    ),
                    None => (
                        anthropic::HOSTED_REDIRECT.to_string(),
                        LoginCompletion::Paste,
                        None,
                    ),
                };
                Ok(Started {
                    result: ConnectResult {
                        url: anthropic::authorize_url(
                            &self.endpoints,
                            &redirect,
                            &pkce.challenge,
                            &state,
                        ),
                        completion,
                        completion_note,
                        device_code: None,
                    },
                    grant: Grant::Browser {
                        flow,
                        redirect,
                        state,
                        verifier: pkce.verifier,
                    },
                    capture,
                    device: None,
                })
            }
            Flow::Codex => {
                let mut completion_note = None;
                if local_callback {
                    let pkce = Pkce::new();
                    let state = oauth::random_token(32);
                    match CallbackCapture::listen(
                        self.endpoints.codex_callback_port,
                        "/auth/callback",
                        &state,
                    ) {
                        Ok((capture, events, port)) => {
                            let redirect = codex::loopback_redirect(port);
                            return Ok(Started {
                                result: ConnectResult {
                                    url: codex::authorize_url(
                                        &self.endpoints,
                                        &redirect,
                                        &pkce.challenge,
                                        &state,
                                    ),
                                    completion: LoginCompletion::LocalCallback,
                                    completion_note: None,
                                    device_code: None,
                                },
                                grant: Grant::Browser {
                                    flow,
                                    redirect,
                                    state,
                                    verifier: pkce.verifier,
                                },
                                capture: Some((capture, events)),
                                device: None,
                            });
                        }
                        Err(CallbackError::AddrInUse(port)) => {
                            completion_note = Some(format!(
                                "Local callback port {port} is in use; enter the code at the provider page instead."
                            ));
                        }
                        Err(_) => {
                            completion_note = Some(
                                "The local callback could not start; enter the code at the provider page instead."
                                    .into(),
                            );
                        }
                    }
                }
                let device = codex::start_device(&self.http, &self.endpoints)
                    .await
                    .map_err(|error| ProviderError::Failed(error.to_string()))?;
                Ok(Started {
                    result: ConnectResult {
                        url: codex::device_page(&self.endpoints),
                        completion: LoginCompletion::DeviceCode,
                        completion_note,
                        device_code: Some(device.user_code.clone()),
                    },
                    grant: Grant::Device,
                    capture: None,
                    device: Some(device),
                })
            }
        }
    }

    async fn poll_device(
        &self,
        name: &str,
        generation: u64,
        device: codex::DeviceStart,
        cancel: CancellationToken,
    ) {
        let wait = device.interval
            + std::time::Duration::from_secs(self.endpoints.device_poll_margin_secs);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            match codex::poll_device(&self.http, &self.endpoints, &device).await {
                Ok(None) => {}
                Ok(Some(tokens)) => {
                    let _ = self.finish(name, generation, tokens).await;
                    return;
                }
                // A poll that did not arrive is asked again; the sign-in's
                // own timeout is what bounds it.
                Err(OAuthError::Unavailable(error)) => {
                    tracing::debug!(provider = %name, %error, "device sign-in poll failed");
                }
                Err(OAuthError::Rejected(_)) => {
                    self.terminal(name, generation, "Sign-in was not authorized.")
                        .await;
                    return;
                }
            }
        }
    }

    pub async fn code(
        &self,
        name: &str,
        text: &str,
        owner: &LoginOwner,
    ) -> Result<(), ProviderError> {
        let generation = {
            let inflight = self.inflight.lock();
            let slot = inflight
                .get(name)
                .ok_or_else(|| ProviderError::NotPending(name.to_string()))?;
            if &slot.owner != owner {
                return Err(ProviderError::WrongOwner);
            }
            if !matches!(slot.state, InflightState::Pending(_)) {
                return Err(ProviderError::NotPending(name.to_string()));
            }
            slot.generation
        };
        self.submit(name, generation, text, LoginCompletion::Paste)
            .await
    }

    /// Exchange a returned authorization code, whether the callback carried it
    /// or the operator pasted it.
    async fn submit(
        &self,
        name: &str,
        generation: u64,
        text: &str,
        source: LoginCompletion,
    ) -> Result<(), ProviderError> {
        let trimmed = text.trim();
        if trimmed.is_empty()
            || trimmed.len() > MAX_MANUAL_INPUT
            || trimmed.contains('\r')
            || trimmed.contains('\n')
        {
            return Err(ProviderError::Failed(
                "the redirect URL or code must be one non-empty line no longer than 8 KiB".into(),
            ));
        }
        let (grant, completion) = {
            let inflight = self.inflight.lock();
            let slot = inflight
                .get(name)
                .filter(|slot| slot.generation == generation)
                .ok_or_else(|| ProviderError::NotPending(name.to_string()))?;
            let InflightState::Pending(pending) = &slot.state else {
                return Err(ProviderError::NotPending(name.to_string()));
            };
            if matches!(pending.grant, Grant::Device) {
                return Err(ProviderError::Failed(
                    "this sign-in finishes on the provider page; there is nothing to paste".into(),
                ));
            }
            let mut state = pending.completion.lock();
            if *state != CompletionState::Open {
                return Err(ProviderError::Failed(
                    "this sign-in completion was already submitted".into(),
                ));
            }
            *state = CompletionState::Claimed(source);
            (pending.grant.clone(), pending.completion.clone())
        };
        let reopen = || {
            let mut state = completion.lock();
            if *state == CompletionState::Claimed(source) {
                *state = CompletionState::Open;
            }
        };
        let Grant::Browser {
            flow,
            redirect,
            state,
            verifier,
        } = grant
        else {
            unreachable!("a device grant is refused above");
        };
        let Some(pasted) = oauth::parse_pasted(trimmed) else {
            reopen();
            return Err(ProviderError::Failed(
                "that is not a redirect URL or sign-in code".into(),
            ));
        };
        if pasted
            .state
            .as_deref()
            .is_some_and(|pasted| pasted != state)
        {
            reopen();
            return Err(ProviderError::Failed(
                "that code belongs to a different sign-in; use the one this sign-in's link shows"
                    .into(),
            ));
        }
        let exchanged = match flow {
            Flow::Anthropic => {
                anthropic::exchange(
                    &self.http,
                    &self.endpoints,
                    &pasted.code,
                    &state,
                    &redirect,
                    &verifier,
                )
                .await
            }
            Flow::Codex => {
                codex::exchange(
                    &self.http,
                    &self.endpoints,
                    &pasted.code,
                    &redirect,
                    &verifier,
                )
                .await
            }
        };
        match exchanged {
            Ok(tokens) => {
                *completion.lock() = CompletionState::Finished;
                self.finish(name, generation, tokens).await
            }
            Err(error) => {
                reopen();
                Err(ProviderError::Failed(error.to_string()))
            }
        }
    }

    /// Keep what a sign-in produced, if that sign-in is still the one waiting.
    async fn finish(
        &self,
        name: &str,
        generation: u64,
        tokens: Tokens,
    ) -> Result<(), ProviderError> {
        let Some(slot) = self.take_generation(name, generation) else {
            return Err(ProviderError::NotPending(name.to_string()));
        };
        stop_capture(&slot);
        slot.cancel.cancel();
        let mesh = self.mesh.get();
        let mut nodes = vec![self.node_id.clone()];
        if slot.share {
            for member in mesh
                .map(|mesh| mesh.members_in(&slot.channels))
                .unwrap_or_default()
            {
                if !nodes.contains(&member) {
                    nodes.push(member);
                }
            }
        }
        let stored = self.store(
            name,
            tokens,
            StoreKind::Fresh {
                channels: slot.channels,
                nodes,
            },
        );
        match &stored {
            Ok(stored) => {
                self.notes.lock().remove(name);
                self.forget_renewals(std::slice::from_ref(&stored.name));
                self.forget_renewals(&stored.removed);
                if let Some(mesh) = mesh {
                    self.hand_off(mesh.as_ref(), &stored.name, &stored.credential);
                    for (credential_name, credential) in &stored.narrowed {
                        self.hand_off(mesh.as_ref(), credential_name, credential);
                    }
                }
                if let Some(callback) = self.on_connected.get() {
                    callback();
                }
            }
            Err(error) => self.note(name, "failed", Some(error.to_string())),
        }
        self.publish();
        stored.map(|_| ())
    }

    /// Hand a credential to every node it lists but this one.
    fn hand_off(&self, mesh: &dyn CredentialMesh, credential_name: &str, credential: &Credential) {
        let others: Vec<String> = credential
            .nodes
            .iter()
            .filter(|node| **node != self.node_id)
            .cloned()
            .collect();
        if !others.is_empty() {
            mesh.hand_off(credential_name, credential, &others);
        }
    }

    /// Remove one of `name`'s sign-ins here, the one held under `sign_in`.
    /// Without one, a sign-in still in flight is cancelled, or else the only
    /// sign-in held is removed; with several held, which one must be said.
    pub async fn disconnect(
        &self,
        name: &str,
        sign_in: Option<&str>,
        owner: &LoginOwner,
    ) -> Result<(), ProviderError> {
        if sign_in.is_none() {
            let pending = {
                let mut inflight = self.inflight.lock();
                match inflight.get(name) {
                    Some(slot) if &slot.owner != owner => return Err(ProviderError::WrongOwner),
                    Some(_) => inflight.remove(name),
                    None => None,
                }
            };
            if let Some(slot) = pending {
                stop_capture(&slot);
                slot.cancel.cancel();
                self.notes.lock().remove(name);
                self.publish();
                return Ok(());
            }
        }
        if matches!(owner, LoginOwner::Peer(_)) {
            return Err(ProviderError::RemoteDisconnect);
        }

        let removed = {
            let mut broker = self.broker.write().unwrap();
            let Some(credential_name) = self.sign_in_named(&broker, name, sign_in)? else {
                drop(broker);
                self.notes.lock().remove(name);
                self.publish();
                return Ok(());
            };
            let mut staged = broker.clone();
            staged.remove(&credential_name);
            staged
                .save(&self.store_key)
                .map_err(|error| ProviderError::Failed(error.to_string()))?;
            *broker = staged;
            credential_name
        };
        self.forget_renewals(std::slice::from_ref(&removed));
        self.notes.lock().remove(name);
        self.publish();
        Ok(())
    }

    /// The sign-in of `name` held here under `sign_in`, or with `None` the
    /// only one held. `None` back when none is; refused when `sign_in` is
    /// not one of `name`'s, or none is named and there are several.
    fn sign_in_named(
        &self,
        broker: &Broker,
        name: &str,
        sign_in: Option<&str>,
    ) -> Result<Option<String>, ProviderError> {
        let provider = self
            .cfg
            .providers
            .get(name)
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))?;
        let held: Vec<&str> = self
            .held_sign_ins(broker, name, provider)
            .into_iter()
            .map(|(credential, _)| credential)
            .collect();
        match sign_in {
            Some(wanted) if held.contains(&wanted) => Ok(Some(wanted.to_string())),
            Some(wanted) => Err(ProviderError::Conflict(format!(
                "{name} holds no sign-in named {wanted} on this node"
            ))),
            None => match held.as_slice() {
                [] => Ok(None),
                [only] => Ok(Some(only.to_string())),
                several => Err(ProviderError::Conflict(format!(
                    "{name} holds {} sign-ins on this node ({}); name the one to change",
                    several.len(),
                    several.join(", ")
                ))),
            },
        }
    }

    /// The credential a provider uses was removed outside [`Self::disconnect`],
    /// as `tracon credential rm` does: what the disconnect would have cleared
    /// goes too. A sign-in still in flight is stopped, or it would store the
    /// credential again when it finished; the note is forgotten; and the new
    /// state is published, so the interface and the mesh stop showing a
    /// connection that is gone.
    pub fn credential_removed(&self, name: &str, credential: &str) {
        let pending = self.inflight.lock().remove(name);
        if let Some(slot) = pending {
            stop_capture(&slot);
            slot.cancel.cancel();
        }
        self.notes.lock().remove(name);
        self.forget_renewals(&[credential.to_string()]);
        self.publish();
    }

    /// Credentials a peer's revoke dropped here: each provider that used one
    /// forgets its note and the new state is published, so what this node
    /// publishes stops showing the connection. Unlike
    /// [`Self::credential_removed`], a sign-in in flight here is left to
    /// finish: it is this node's operator connecting again, and what it
    /// stores is a sign-in of their own.
    pub fn credentials_revoked(&self, credential_names: &[String]) {
        let revoked = self.providers_signed_in_by(credential_names);
        {
            let mut notes = self.notes.lock();
            for name in &revoked {
                notes.remove(name);
            }
        }
        self.forget_renewals(credential_names);
        self.publish();
    }

    /// Change the channels one of a connected provider's sign-ins serves
    /// (the one under `sign_in`, or the only one), keeping its tokens. A
    /// shared credential also goes to the members of any added channel; a
    /// node that already holds it keeps its copy for a channel taken away.
    /// A channel another sign-in made here serves moves to this one, which
    /// is refused when that would leave the other serving none.
    pub fn set_channels(
        &self,
        name: &str,
        sign_in: Option<&str>,
        channels: Vec<String>,
    ) -> Result<(), ProviderError> {
        let base = self.credential_name(name)?;
        let (credential_name, credential, was_unbound, narrowed) = {
            let mut broker = self.broker.write().unwrap();
            let credential_name = self
                .sign_in_named(&broker, name, sign_in)?
                .ok_or_else(|| ProviderError::Failed(format!("{name} is not connected")))?;
            let mut staged = broker.clone();
            let mut credential = staged
                .get(&credential_name)
                .cloned()
                .ok_or_else(|| ProviderError::Failed(format!("{name} is not connected")))?;
            let was_unbound = credential.channels.is_empty();
            if credential.nodes.iter().any(|node| *node != self.node_id) {
                for member in self
                    .mesh
                    .get()
                    .map(|mesh| mesh.members_in(&channels))
                    .unwrap_or_default()
                {
                    if !credential.nodes.contains(&member) {
                        credential.nodes.push(member);
                    }
                }
            }
            let narrowed = self
                .take_channels(&mut staged, name, &base, &credential_name, &channels)
                .map_err(ProviderError::Conflict)?;
            credential.channels = channels;
            // Peers keep the copy they hold unless this one is newer.
            if credential.grant_id.is_some() {
                credential.grant_version += 1;
            }
            staged.put(&credential_name, credential.clone());
            staged
                .save(&self.store_key)
                .map_err(|error| ProviderError::Failed(error.to_string()))?;
            *broker = staged;
            (credential_name, credential, was_unbound, narrowed)
        };
        if let Some(mesh) = self.mesh.get() {
            self.hand_off(mesh.as_ref(), &credential_name, &credential);
            for (credential_name, credential) in &narrowed {
                self.hand_off(mesh.as_ref(), credential_name, credential);
            }
        }
        if was_unbound {
            if let Some(callback) = self.on_connected.get() {
                callback();
            }
        }
        self.publish();
        Ok(())
    }

    /// Seal an API key as the credential a provider names, bound to these
    /// channels on this node. It is not handed to peers: a key stays on the
    /// node it was given to. A subscription sign-in under the same name is
    /// left alone; disconnecting it is a separate, deliberate step.
    pub fn set_key(
        &self,
        name: &str,
        key: &str,
        channels: Vec<String>,
    ) -> Result<(), ProviderError> {
        let credential_name = self.credential_name(name)?;
        {
            let mut broker = self.broker.write().unwrap();
            let mut staged = broker.clone();
            let mut credential = staged.get(&credential_name).cloned().unwrap_or_default();
            match credential.kind.as_str() {
                KIND_OAUTH => {
                    return Err(ProviderError::Conflict(format!(
                        "{credential_name} holds a subscription sign-in; disconnect it before adding a key"
                    )))
                }
                KIND_API_KEY => {}
                // A fresh `Credential` is an empty `env` one.
                _ if credential.env.is_empty() => {}
                _ => {
                    return Err(ProviderError::Conflict(format!(
                        "credential {credential_name} is not a model key and cannot be replaced here"
                    )))
                }
            }
            credential.kind = KIND_API_KEY.into();
            credential.provider = Some(name.to_string());
            credential.channels = channels;
            credential.expires_ms = None;
            credential.identity = None;
            credential.env.insert("API_KEY".into(), key.to_string());
            staged.put(&credential_name, credential);
            staged
                .save(&self.store_key)
                .map_err(|error| ProviderError::Failed(error.to_string()))?;
            *broker = staged;
        }
        self.notes.lock().remove(name);
        if let Some(callback) = self.on_connected.get() {
            callback();
        }
        self.publish();
        Ok(())
    }

    async fn terminal(&self, name: &str, generation: u64, message: &'static str) {
        let Some(slot) = self.take_generation(name, generation) else {
            return;
        };
        stop_capture(&slot);
        slot.cancel.cancel();
        self.note(name, "failed", Some(message.into()));
        self.publish();
    }

    fn remove_generation(&self, name: &str, generation: u64) {
        if let Some(slot) = self.take_generation(name, generation) {
            slot.cancel.cancel();
        }
    }

    fn take_generation(&self, name: &str, generation: u64) -> Option<Inflight> {
        let mut inflight = self.inflight.lock();
        if inflight
            .get(name)
            .is_some_and(|slot| slot.generation == generation)
        {
            inflight.remove(name)
        } else {
            None
        }
    }

    /// What disconnecting the sign-in of `name` under `sign_in` (or the
    /// only one) would take away here: the channels it serves that no other
    /// sign-in here would, and the peers whose copies it leaves. `None` when
    /// nothing is lost, nothing is held, or the disconnect only cancels a
    /// sign-in still in progress.
    pub fn displaced(
        &self,
        name: &str,
        sign_in: Option<&str>,
    ) -> Result<Option<Displaced>, ProviderError> {
        let base = self.credential_name(name)?;
        if sign_in.is_none() && self.inflight.lock().contains_key(name) {
            return Ok(None);
        }
        let broker = self.broker.read().unwrap();
        let Some(credential_name) = self.sign_in_named(&broker, name, sign_in)? else {
            return Ok(None);
        };
        let Some(credential) = broker.get(&credential_name) else {
            return Ok(None);
        };
        let serves = |channel: &str| {
            broker
                .model_credential(name, &base, channel, &self.node_id)
                .is_some_and(|(chosen, _)| chosen == credential_name)
        };
        let covered = |channel: &str| {
            broker.family(name, &base).any(|(other, c)| {
                other != credential_name && c.allows(channel) && c.allows_node(&self.node_id)
            })
        };
        let displaced = Displaced {
            channels: credential
                .channels
                .iter()
                .filter(|channel| serves(channel) && !covered(channel))
                .cloned()
                .collect(),
            peers: self.peers_of(credential),
            sign_in: credential_name.clone(),
        };
        let loses = !displaced.channels.is_empty() || !displaced.peers.is_empty();
        Ok(loses.then_some(displaced))
    }

    /// Whether this node holds the credential `name`'s entry names bound to
    /// no channel: this node's alone, which a sign-in would take over, so
    /// not a peer's to sign in over.
    pub fn holds_unbound(&self, name: &str) -> Result<bool, ProviderError> {
        let base = self.credential_name(name)?;
        Ok(self
            .broker
            .read()
            .unwrap()
            .get(&base)
            .is_some_and(|credential| credential.channels.is_empty()))
    }

    /// Record that the sign-in for `name` that `owner` has in progress was
    /// started on the strength of the admin statement `confirmation`.
    pub fn confirm_login(&self, name: &str, owner: &LoginOwner, confirmation: &str) {
        if let Some(slot) = self.inflight.lock().get_mut(name) {
            if &slot.owner == owner && matches!(slot.state, InflightState::Pending(_)) {
                slot.confirmed = Some(confirmation.to_string());
            }
        }
    }

    /// The statement the sign-in for `name` in progress was confirmed with,
    /// if `owner` started it, it waits for its code, and it is younger than
    /// [`CONFIRMED_LOGIN_MS`]: what lets that sign-in's own code through
    /// without a second confirmation. Any other sign-in has none.
    pub fn confirmed_login(&self, name: &str, owner: &LoginOwner, now: i64) -> Option<String> {
        let inflight = self.inflight.lock();
        let slot = inflight.get(name)?;
        (&slot.owner == owner
            && matches!(slot.state, InflightState::Pending(_))
            && now.saturating_sub(slot.started_ms) <= CONFIRMED_LOGIN_MS)
            .then(|| slot.confirmed.clone())
            .flatten()
    }

    /// The channels a sign-in in progress for `name` will bind its
    /// credential to, or `None` when there is none.
    pub fn login_channels(&self, name: &str) -> Option<Vec<String>> {
        self.inflight
            .lock()
            .get(name)
            .map(|slot| slot.channels.clone())
    }

    fn credential_name(&self, name: &str) -> Result<String, ProviderError> {
        self.cfg
            .providers
            .get(name)
            .map(|provider| provider.credential.clone())
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))
    }

    fn store(&self, name: &str, tokens: Tokens, kind: StoreKind) -> Result<Stored, ProviderError> {
        let base = self.credential_name(name)?;
        let fresh = matches!(kind, StoreKind::Fresh { .. });
        let mut broker = self.broker.write().unwrap();
        let mut staged = broker.clone();
        let (credential_name, mut credential, narrowed, removed) = match kind {
            // Decided here rather than when the sign-in started, since the
            // sign-ins held may have changed while the operator signed in.
            StoreKind::Fresh { channels, nodes } => {
                let (credential_name, removed) = claim(&staged, name, &base, &channels);
                for replaced in &removed {
                    staged.remove(replaced);
                }
                let narrowed = self
                    .take_channels(&mut staged, name, &base, &credential_name, &channels)
                    .map_err(ProviderError::Failed)?;
                let credential = Credential {
                    channels,
                    nodes,
                    grant_id: Some(uuid::Uuid::now_v7().to_string()),
                    grant_origin: Some(self.node_id.clone()),
                    ..Default::default()
                };
                (credential_name, credential, narrowed, removed)
            }
            // A refresh keeps where the credential may be used and who it was
            // shared with; only the tokens change.
            StoreKind::Refresh { credential } => {
                let credential_name = credential;
                let credential = staged.get(&credential_name).cloned().ok_or_else(|| {
                    ProviderError::Failed(format!(
                        "no existing OAuth credential for {name} to refresh"
                    ))
                })?;
                if credential.kind != KIND_OAUTH
                    || credential.provider.as_deref() != Some(name)
                    || !broker::in_family(&credential_name, &credential, name, &base)
                {
                    return Err(ProviderError::Failed(format!(
                        "the existing credential for {name} is not the same OAuth provider"
                    )));
                }
                let mut credential = credential;
                // A credential from before sign-ins were tracked gets its
                // identity at its first renewal, from the one node that may
                // renew it then.
                credential.ensure_grant(&self.node_id);
                credential.grant_version += 1;
                (credential_name, credential, Vec::new(), Vec::new())
            }
        };
        credential.kind = KIND_OAUTH.into();
        credential.provider = Some(name.to_string());
        credential.expires_ms = tokens.expires_ms;
        if tokens.identity.is_some() || fresh {
            credential.identity = tokens.identity;
        }
        credential.env.insert("ACCESS_TOKEN".into(), tokens.access);
        if let Some(refresh) = tokens.refresh {
            credential.env.insert("REFRESH_TOKEN".into(), refresh);
        } else if fresh {
            credential.env.remove("REFRESH_TOKEN");
        }
        if let Some(account_id) = tokens.account_id {
            credential
                .env
                .insert("CHATGPT_ACCOUNT_ID".into(), account_id);
        } else if fresh {
            credential.env.remove("CHATGPT_ACCOUNT_ID");
        }
        staged.put(&credential_name, credential.clone());
        staged
            .save(&self.store_key)
            .map_err(|error| ProviderError::Failed(error.to_string()))?;
        *broker = staged;
        Ok(Stored {
            name: credential_name,
            credential,
            narrowed,
            removed,
        })
    }

    /// Take `channels` from every other sign-in of `name` made on this node
    /// that serves one of them, other than the one under `keep`, so each
    /// channel is served by one sign-in made here. Returns the sign-ins
    /// changed. A copy of another node's sign-in is its origin's to change
    /// and is left; [`Broker::model_credential`] prefers the one made here.
    /// Refused when a sign-in would be left serving no channel.
    fn take_channels(
        &self,
        staged: &mut Broker,
        name: &str,
        base: &str,
        keep: &str,
        channels: &[String],
    ) -> Result<Vec<(String, Credential)>, String> {
        let overlapping: Vec<String> = staged
            .family(name, base)
            .filter(|(other, c)| {
                *other != keep
                    && c.is_model()
                    && c.originates_at(&self.node_id)
                    && c.channels.iter().any(|channel| channels.contains(channel))
            })
            .map(|(other, _)| other.to_string())
            .collect();
        let mut narrowed = Vec::new();
        for other in overlapping {
            let mut credential = staged.get(&other).cloned().expect("listed above");
            credential
                .channels
                .retain(|channel| !channels.contains(channel));
            if credential.channels.is_empty() {
                return Err(format!(
                    "the {name} sign-in {other} serves only channels this one would take; \
                     disconnect it first"
                ));
            }
            if credential.grant_id.is_some() {
                credential.grant_version += 1;
            }
            staged.put(&other, credential.clone());
            narrowed.push((other, credential));
        }
        Ok(narrowed)
    }

    /// Renew a credential's tokens.
    ///
    /// A refresh rotates the refresh token and revokes the access token it
    /// replaced, so a credential held by several nodes is renewed by one of
    /// them per version: whichever wins the hub's claim, which then hands the
    /// renewed copy to the rest. Without a claim — no hub, a hub that predates
    /// claims, or one that cannot be reached — only the node that signed in,
    /// first in `nodes`, renews.
    pub async fn refresh(
        &self,
        name: &str,
        credential_name: &str,
    ) -> Result<Refreshed, ProviderError> {
        let flow = self.flow(name)?;
        let held = self
            .broker
            .read()
            .unwrap()
            .get(credential_name)
            .cloned()
            .ok_or_else(|| ProviderError::Failed(format!("no {name} credential to refresh")))?;
        // A token minted without one (`claude setup-token`'s) runs its
        // course and is replaced by signing in.
        let Some(refresh_token) = held.env.get("REFRESH_TOKEN").cloned() else {
            return Err(ProviderError::ReconnectRequired(format!(
                "the {name} token cannot be renewed in place; connect the provider again"
            )));
        };
        if held.nodes.iter().any(|node| *node != self.node_id) {
            let first = held.nodes.first() == Some(&self.node_id);
            let claimed = match (self.mesh.get(), held.grant_id.as_deref()) {
                (Some(mesh), Some(grant)) => {
                    mesh.claim(&claim_key(grant), held.grant_version).await
                }
                _ => ClaimResult::Unsupported,
            };
            match claimed {
                ClaimResult::Won => {}
                ClaimResult::Lost => return Ok(Refreshed::LeftToAnotherHolder),
                ClaimResult::Unsupported | ClaimResult::Unreachable(_) if !first => {
                    return Ok(Refreshed::LeftToAnotherHolder)
                }
                ClaimResult::Unsupported | ClaimResult::Unreachable(_) => {}
            }
        }
        let refreshed = match flow {
            Flow::Anthropic => {
                anthropic::refresh(&self.http, &self.endpoints, &refresh_token).await
            }
            Flow::Codex => codex::refresh(&self.http, &self.endpoints, &refresh_token).await,
        };
        let tokens = refreshed.map_err(|error| match error {
            OAuthError::Rejected(reason) => {
                ProviderError::ReconnectRequired(format!("{reason}; connect the provider again"))
            }
            OAuthError::Unavailable(reason) => ProviderError::Failed(reason),
        })?;
        let stored = self.store(
            name,
            tokens,
            StoreKind::Refresh {
                credential: credential_name.to_string(),
            },
        )?;
        if let Some(mesh) = self.mesh.get() {
            self.hand_off(mesh.as_ref(), &stored.name, &stored.credential);
        }
        self.publish();
        Ok(Refreshed::Renewed)
    }

    /// The sign-ins, as (provider, credential), whose token expires soon
    /// enough that this node should try to renew it. Whether it actually
    /// does is [`Providers::refresh`]'s claim.
    pub fn due_for_refresh(&self, now: i64) -> Vec<(String, String)> {
        let broker = self.broker.read().unwrap();
        let renewals = self.renewals.lock();
        let mut due = Vec::new();
        for (name, provider) in self.cfg.providers.iter().filter(|(_, provider)| {
            provider
                .login
                .as_deref()
                .and_then(Flow::for_login)
                .is_some()
        }) {
            for (credential_name, credential) in self.held_sign_ins(&broker, name, provider) {
                // A credential already known not to renew is not due for
                // anything: asking again every tick would be a loop that
                // only stops when the operator reconnects.
                if renewals
                    .get(credential_name)
                    .is_some_and(|note| note.state == NEEDS_RECONNECT)
                {
                    continue;
                }
                let shared = credential.nodes.iter().any(|node| *node != self.node_id);
                let ahead = if self.cfg.mesh.renew_credentials || !shared {
                    REFRESH_AHEAD_MS
                } else {
                    FALLBACK_REFRESH_AHEAD_MS
                };
                if credential.kind == KIND_OAUTH
                    && credential
                        .expires_ms
                        .is_some_and(|expires| expires - now < ahead)
                {
                    due.push((name.clone(), credential_name.to_string()));
                }
            }
        }
        due
    }

    pub async fn refresh_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(REFRESH_TICK_SECS));
        loop {
            tick.tick().await;
            for (name, credential) in self.due_for_refresh(now_ms()) {
                match self.refresh(&name, &credential).await {
                    Ok(Refreshed::Renewed) => {
                        tracing::info!(provider = %name, %credential, "token refreshed")
                    }
                    Ok(Refreshed::LeftToAnotherHolder) => {
                        tracing::debug!(provider = %name, %credential, "another holder renews this token")
                    }
                    // Nothing will change on its own: the note is what stops
                    // this being asked again, and it is cleared by the next
                    // successful connect.
                    Err(ProviderError::ReconnectRequired(reason)) => {
                        tracing::warn!(provider = %name, %credential, reason = %reason, "token needs a new sign-in");
                        self.renewal_note(&credential, NEEDS_RECONNECT, Some(reason));
                        self.publish();
                    }
                    Err(error) => {
                        tracing::warn!(provider = %name, %credential, error = %error, "token refresh failed");
                        self.renewal_note(
                            &credential,
                            "failed",
                            Some(format!("refresh failed: {error}")),
                        );
                        self.publish();
                    }
                }
            }
        }
    }

    fn renewal_note(&self, credential: &str, state: &'static str, error: Option<String>) {
        self.renewals.lock().insert(
            credential.to_string(),
            Note {
                state,
                error,
                updated_ms: now_ms(),
            },
        );
    }
}

/// The name a new sign-in of `name` for `channels` is stored under, and the
/// sign-ins it replaces: every one held whose channels all fall within
/// `channels`, which this sign-in now serves instead. It takes over `base`
/// when that is one of them or nothing holds it, otherwise one of the
/// replaced, otherwise a new `base@<channel>`. `base` bound to no channel
/// serves nothing, and a sign-in takes it over (a peer may not start one
/// then: `forwarded_scope`); any other credential bound to none is left.
fn claim(staged: &Broker, name: &str, base: &str, channels: &[String]) -> (String, Vec<String>) {
    let covered: Vec<String> = staged
        .family(name, base)
        .filter(|(_, c)| {
            !c.channels.is_empty() && c.channels.iter().all(|channel| channels.contains(channel))
        })
        .map(|(credential, _)| credential.to_string())
        .collect();
    let base_free = staged.get(base).is_none_or(|held| held.channels.is_empty());
    let reuse = if base_free || covered.iter().any(|credential| credential == base) {
        base.to_string()
    } else if let Some(first) = covered.first() {
        first.clone()
    } else {
        let stem = format!(
            "{base}@{}",
            channels.iter().min().map_or("sign-in", String::as_str)
        );
        let mut candidate = stem.clone();
        let mut n = 2;
        while staged.get(&candidate).is_some() {
            candidate = format!("{stem}-{n}");
            n += 1;
        }
        candidate
    };
    let removed = covered
        .into_iter()
        .filter(|credential| *credential != reuse)
        .collect();
    (reuse, removed)
}

fn stop_capture(slot: &Inflight) {
    if let InflightState::Pending(pending) = &slot.state {
        if let Some(capture) = &pending.capture {
            capture.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> Arc<Providers> {
        Providers::new(
            Arc::new(Config::default()),
            crate::broker::Broker::default().shared(),
            DataKey::from_bytes([7u8; 32]),
            "n".into(),
            Bus::new(),
        )
    }

    fn slot(owner: LoginOwner, started_ms: i64, pending: bool) -> Inflight {
        Inflight {
            generation: 1,
            owner,
            channels: vec![],
            share: true,
            started_ms,
            cancel: CancellationToken::new(),
            state: match pending {
                true => InflightState::Pending(PendingLogin {
                    result: ConnectResult {
                        url: "https://example.invalid".into(),
                        completion: LoginCompletion::Paste,
                        completion_note: None,
                        device_code: None,
                    },
                    grant: Grant::Device,
                    capture: None,
                    completion: Arc::new(Mutex::new(CompletionState::Open)),
                }),
                false => InflightState::Starting,
            },
            confirmed: None,
        }
    }

    /// A peer's confirmed connect covers the code for that sign-in only:
    /// the same owner, still waiting for its code, and young enough.
    #[test]
    fn a_confirmed_login_covers_only_its_own_waiting_sign_in() {
        let p = providers();
        let a = LoginOwner::Peer("a".into());
        let b = LoginOwner::Peer("b".into());
        let now = 10 * CONFIRMED_LOGIN_MS;
        p.inflight
            .lock()
            .insert("anthropic".into(), slot(a.clone(), now - 1_000, true));
        assert_eq!(
            p.confirmed_login("anthropic", &a, now),
            None,
            "not confirmed yet"
        );
        // Another owner cannot mark it, nor is it covered for one.
        p.confirm_login("anthropic", &b, "peer b");
        assert_eq!(p.confirmed_login("anthropic", &a, now), None);
        p.confirm_login("anthropic", &a, "peer a");
        assert_eq!(
            p.confirmed_login("anthropic", &a, now).as_deref(),
            Some("peer a")
        );
        assert_eq!(p.confirmed_login("anthropic", &b, now), None);
        assert_eq!(
            p.confirmed_login("anthropic", &LoginOwner::Local, now),
            None
        );
        assert_eq!(p.confirmed_login("openai-codex", &a, now), None);
        // Past its lifetime.
        assert_eq!(
            p.confirmed_login("anthropic", &a, now + CONFIRMED_LOGIN_MS),
            None
        );
        // A sign-in not yet waiting for its code is neither marked nor covered.
        p.inflight
            .lock()
            .insert("openai-codex".into(), slot(a.clone(), now, false));
        p.confirm_login("openai-codex", &a, "peer a");
        assert_eq!(p.confirmed_login("openai-codex", &a, now), None);
        p.inflight.lock().get_mut("openai-codex").unwrap().confirmed = Some("peer a".into());
        assert_eq!(p.confirmed_login("openai-codex", &a, now), None);
    }

    /// A peer's revoke landing while this node's operator is signing in
    /// again leaves that sign-in to finish; a removal here stops it.
    #[test]
    fn a_revoke_from_a_peer_leaves_a_sign_in_in_flight_alone() {
        let p = providers();
        let credential = p.credential_name("anthropic").unwrap();
        p.inflight
            .lock()
            .insert("anthropic".into(), slot(LoginOwner::Local, now_ms(), true));
        p.credentials_revoked(std::slice::from_ref(&credential));
        let cancel = p.inflight.lock()["anthropic"].cancel.clone();
        assert!(!cancel.is_cancelled());

        p.credential_removed("anthropic", &credential);
        assert!(cancel.is_cancelled());
        assert!(p.inflight.lock().get("anthropic").is_none());
    }
}
