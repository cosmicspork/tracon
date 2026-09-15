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
    broker::{Credential, SharedBroker, KIND_OAUTH},
    config::Config,
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
}

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
    Refresh,
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

pub struct Providers {
    cfg: Arc<Config>,
    broker: SharedBroker,
    store_key: DataKey,
    endpoints: Endpoints,
    http: reqwest::Client,
    node_id: String,
    bus: Bus,
    inflight: Mutex<HashMap<String, Inflight>>,
    notes: Mutex<HashMap<String, Note>>,
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
        let arrived: Vec<String> = self
            .cfg
            .providers
            .iter()
            .filter(|(_, provider)| credential_names.contains(&provider.credential))
            .map(|(name, _)| name.clone())
            .collect();
        if arrived.is_empty() {
            return;
        }
        {
            let mut notes = self.notes.lock();
            for name in &arrived {
                notes.remove(name);
            }
        }
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
        self.cfg
            .providers
            .iter()
            .map(|(name, provider)| {
                let cred = broker.model_credential_for(name, &self.node_id);
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
                } else if let Some(note) =
                    notes.get(name).filter(|note| note.state == NEEDS_RECONNECT)
                {
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
                let mut summary = json!({
                    "name": name,
                    "state": state,
                    "kind": cred.map(|(_, c)| c.kind.clone()),
                    "can_login": provider.login.as_deref().and_then(Flow::for_login).is_some(),
                    "identity": cred.and_then(|(_, c)| c.identity.clone()),
                    "expires_ms": cred.and_then(|(_, c)| c.expires_ms),
                    "channels": cred.map(|(_, c)| c.channels.clone()).unwrap_or_default(),
                    "updated_ms": updated_ms,
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
                }
                summary
            })
            .collect()
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
            Ok((credential_name, credential)) => {
                self.notes.lock().remove(name);
                if let Some(mesh) = mesh {
                    self.hand_off(mesh.as_ref(), credential_name, credential);
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

    pub async fn disconnect(&self, name: &str, owner: &LoginOwner) -> Result<(), ProviderError> {
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
        if matches!(owner, LoginOwner::Peer(_)) {
            return Err(ProviderError::RemoteDisconnect);
        }

        let credential_name = self.credential_name(name)?;
        {
            let mut broker = self.broker.write().unwrap();
            let mut staged = broker.clone();
            if staged.remove(&credential_name) {
                staged
                    .save(&self.store_key)
                    .map_err(|error| ProviderError::Failed(error.to_string()))?;
                *broker = staged;
            }
        }
        self.notes.lock().remove(name);
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

    fn credential_name(&self, name: &str) -> Result<String, ProviderError> {
        self.cfg
            .providers
            .get(name)
            .map(|provider| provider.credential.clone())
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))
    }

    fn store(
        &self,
        name: &str,
        tokens: Tokens,
        kind: StoreKind,
    ) -> Result<(String, Credential), ProviderError> {
        let credential_name = self.credential_name(name)?;
        let fresh = matches!(kind, StoreKind::Fresh { .. });
        let mut broker = self.broker.write().unwrap();
        let mut staged = broker.clone();
        let mut credential = match kind {
            StoreKind::Fresh { channels, nodes } => Credential {
                channels,
                nodes,
                grant_id: Some(uuid::Uuid::now_v7().to_string()),
                ..Default::default()
            },
            // A refresh keeps where the credential may be used and who it was
            // shared with; only the tokens change.
            StoreKind::Refresh => {
                let credential = staged.get(&credential_name).cloned().ok_or_else(|| {
                    ProviderError::Failed(format!(
                        "no existing OAuth credential for {name} to refresh"
                    ))
                })?;
                if credential.kind != KIND_OAUTH || credential.provider.as_deref() != Some(name) {
                    return Err(ProviderError::Failed(format!(
                        "the existing credential for {name} is not the same OAuth provider"
                    )));
                }
                let mut credential = credential;
                // A credential from before sign-ins were tracked gets its
                // identity at its first renewal, from the one node that may
                // renew it then.
                if credential.grant_id.is_none() {
                    credential.grant_id = Some(uuid::Uuid::now_v7().to_string());
                }
                credential.grant_version += 1;
                credential
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
        Ok((credential_name, credential))
    }

    /// Renew a credential's tokens.
    ///
    /// A refresh rotates the refresh token and revokes the access token it
    /// replaced, so a credential held by several nodes is renewed by one of
    /// them per version: whichever wins the hub's claim, which then hands the
    /// renewed copy to the rest. Without a claim — no hub, a hub that predates
    /// claims, or one that cannot be reached — only the node that signed in,
    /// first in `nodes`, renews.
    pub async fn refresh(&self, name: &str) -> Result<Refreshed, ProviderError> {
        let flow = self.flow(name)?;
        let credential_name = self.credential_name(name)?;
        let held = self
            .broker
            .read()
            .unwrap()
            .get(&credential_name)
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
        let (credential_name, credential) = self.store(name, tokens, StoreKind::Refresh)?;
        if let Some(mesh) = self.mesh.get() {
            self.hand_off(mesh.as_ref(), &credential_name, &credential);
        }
        self.publish();
        Ok(Refreshed::Renewed)
    }

    /// Providers whose token expires soon enough that this node should try to
    /// renew it. Whether it actually does is [`Providers::refresh`]'s claim.
    pub fn due_for_refresh(&self, now: i64) -> Vec<String> {
        let broker = self.broker.read().unwrap();
        let notes = self.notes.lock();
        self.cfg
            .providers
            .iter()
            .filter(|(_, provider)| {
                provider
                    .login
                    .as_deref()
                    .and_then(Flow::for_login)
                    .is_some()
            })
            // A credential already known not to renew is not due for
            // anything: asking again every tick would be a loop that only
            // stops when the operator reconnects.
            .filter(|(name, _)| {
                notes
                    .get(name.as_str())
                    .is_none_or(|note| note.state != NEEDS_RECONNECT)
            })
            .filter(|(name, _)| {
                broker
                    .model_credential_for(name, &self.node_id)
                    .map(|(_, credential): (&str, &Credential)| {
                        let shared = credential.nodes.iter().any(|node| *node != self.node_id);
                        let ahead = if self.cfg.mesh.renew_credentials || !shared {
                            REFRESH_AHEAD_MS
                        } else {
                            FALLBACK_REFRESH_AHEAD_MS
                        };
                        credential.kind == KIND_OAUTH
                            && credential
                                .expires_ms
                                .is_some_and(|expires| expires - now < ahead)
                    })
                    .unwrap_or(false)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub async fn refresh_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(REFRESH_TICK_SECS));
        loop {
            tick.tick().await;
            for name in self.due_for_refresh(now_ms()) {
                match self.refresh(&name).await {
                    Ok(Refreshed::Renewed) => tracing::info!(provider = %name, "token refreshed"),
                    Ok(Refreshed::LeftToAnotherHolder) => {
                        tracing::debug!(provider = %name, "another holder renews this token")
                    }
                    // Nothing will change on its own: the note is what stops
                    // this being asked again, and it is cleared by the next
                    // successful connect.
                    Err(ProviderError::ReconnectRequired(reason)) => {
                        tracing::warn!(provider = %name, reason = %reason, "token needs a new sign-in");
                        self.note(&name, NEEDS_RECONNECT, Some(reason));
                        self.publish();
                    }
                    Err(error) => {
                        tracing::warn!(provider = %name, error = %error, "token refresh failed");
                        self.note(&name, "failed", Some(format!("refresh failed: {error}")));
                        self.publish();
                    }
                }
            }
        }
    }
}

fn stop_capture(slot: &Inflight) {
    if let InflightState::Pending(pending) = &slot.state {
        if let Some(capture) = &pending.capture {
            capture.stop();
        }
    }
}
