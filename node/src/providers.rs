//! Connecting a model provider through the harness's own login flow.

pub mod callback;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use parking_lot::Mutex;
use proto::envelope::DataKey;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use self::callback::{
    CallbackCapture, CallbackError, CallbackOutcome, CallbackTarget, CaptureEvent, CaptureReply,
};
use crate::{
    adapter::{claude::ClaudeAdapter, opencode::OpenCodeAdapter, HarnessAdapter, LiftedToken},
    boundary::Backend,
    broker::{Credential, SharedBroker, KIND_OAUTH},
    config::Config,
    runner::Mount,
    session::materialize::state_target,
    store::now_ms,
    stream::{Bus, Frame},
};

const REFRESH_AHEAD_MS: i64 = 30 * 60 * 1000;
pub const REFRESH_TICK_SECS: u64 = 5 * 60;
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const MAX_MANUAL_INPUT: usize = 8 * 1024;

/// The provider state for a credential that can only be replaced by signing in
/// again. Not "failed": nothing went wrong and nothing is going to be retried,
/// the operator simply has to reconnect.
const NEEDS_RECONNECT: &str = "needs_reconnect";

/// Which adapter runs which provider's login.
///
/// The login client and the session harness are independent, and each
/// subscription has exactly one client that can mint it. `claude setup-token`
/// is the whole of the Anthropic path, so `anthropic` resolves to the Claude
/// adapter whatever `[harness] id` names; `opencode auth login -p openai` is the
/// whole of the Codex path, so `openai`/`openai-codex` resolve to the
/// OpenCode adapter the same way. The retired harness used to broker Codex
/// for a node running anything else, which is why this table names it now.
/// Every other login stays with the harness the node runs sessions with.
///
/// The table also decides who lifts: `setup-token` prints its token once and
/// leaves nothing in a store directory, so the instance that ran the login is
/// the only place the token exists until the broker has it. Login and lift
/// must therefore reach the same `Arc`, which is why this is held rather than
/// rebuilt per call.
pub struct LoginAdapters {
    session: Arc<dyn HarnessAdapter>,
    per_provider: HashMap<String, Arc<dyn HarnessAdapter>>,
}

impl LoginAdapters {
    /// The table a node runs with.
    pub fn for_node(cfg: &Config, session: Arc<dyn HarnessAdapter>, backend: &dyn Backend) -> Self {
        let mut per_provider = HashMap::new();
        if session.id() != ClaudeAdapter::ID {
            let claude = ClaudeAdapter::new(crate::adapter::image_version(ClaudeAdapter::ID))
                .with_login_image(backend.login_image());
            per_provider.insert(
                ClaudeAdapter::LOGIN_PROVIDER.to_string(),
                Arc::new(claude) as Arc<dyn HarnessAdapter>,
            );
        }
        if session.id() != OpenCodeAdapter::ID {
            let opencode = Arc::new(
                OpenCodeAdapter::new(crate::adapter::image_version(OpenCodeAdapter::ID))
                    .with_login_image(backend.codex_login_image()),
            ) as Arc<dyn HarnessAdapter>;
            for provider in OpenCodeAdapter::LOGIN_PROVIDERS {
                per_provider.insert(provider.to_string(), opencode.clone());
            }
        }
        // Named so an unconfigured provider cannot silently acquire a login
        // client it was never meant to have.
        per_provider.retain(|name, _| cfg.providers.contains_key(name));
        Self {
            session,
            per_provider,
        }
    }

    /// Every login through one adapter: what a node whose harness brokers all
    /// of its own logins uses, and what the tests drive.
    pub fn all(session: Arc<dyn HarnessAdapter>) -> Self {
        Self {
            session,
            per_provider: HashMap::new(),
        }
    }

    pub fn get(&self, provider: &str) -> &Arc<dyn HarnessAdapter> {
        self.per_provider.get(provider).unwrap_or(&self.session)
    }
}

type Stdin = Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>;

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
    started_ms: i64,
    state: InflightState,
}

enum InflightState {
    Starting,
    Pending(PendingLogin),
}

struct PendingLogin {
    result: ConnectResult,
    stdin: Stdin,
    capture: Option<CallbackCapture>,
    completion: Arc<Mutex<CompletionState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionState {
    Open,
    Claimed(LoginCompletion),
    Finished,
}

#[derive(Clone, Copy)]
enum LiftKind {
    Fresh,
    Refresh,
}

#[derive(Debug, Clone)]
struct Note {
    state: &'static str,
    error: Option<String>,
    updated_ms: i64,
}

pub struct Providers {
    cfg: Arc<Config>,
    broker: SharedBroker,
    store_key: DataKey,
    logins: LoginAdapters,
    backend: Arc<dyn Backend>,
    node_id: String,
    bus: Bus,
    store_root: PathBuf,
    inflight: Mutex<HashMap<String, Inflight>>,
    notes: Mutex<HashMap<String, Note>>,
    next_generation: AtomicU64,
    on_connected: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
    on_publish: std::sync::OnceLock<Box<dyn Fn(Vec<serde_json::Value>) + Send + Sync>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no provider named {0}")]
    Unknown(String),
    #[error("provider {0} has no login flow; import an API key with `tracon credential import`")]
    NoLogin(String),
    #[error("provider {0} needs a local callback; connect it on a local node, then share the credential")]
    RequiresLocalCallback(String),
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<Config>,
        broker: SharedBroker,
        store_key: DataKey,
        adapter: Arc<dyn HarnessAdapter>,
        backend: Arc<dyn Backend>,
        node_id: String,
        bus: Bus,
    ) -> Arc<Self> {
        let logins = LoginAdapters::for_node(&cfg, adapter, backend.as_ref());
        Self::new_in(
            Self::store_root(),
            cfg,
            broker,
            store_key,
            logins,
            backend,
            node_id,
            bus,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_in(
        store_root: PathBuf,
        cfg: Arc<Config>,
        broker: SharedBroker,
        store_key: DataKey,
        logins: LoginAdapters,
        backend: Arc<dyn Backend>,
        node_id: String,
        bus: Bus,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            broker,
            store_key,
            logins,
            backend,
            node_id,
            bus,
            store_root,
            inflight: Mutex::new(HashMap::new()),
            notes: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            on_connected: std::sync::OnceLock::new(),
            on_publish: std::sync::OnceLock::new(),
        })
    }

    pub fn set_on_connected(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_connected.set(f);
    }

    pub fn set_on_publish(&self, f: Box<dyn Fn(Vec<serde_json::Value>) + Send + Sync>) {
        let _ = self.on_publish.set(f);
    }

    pub fn store_root() -> PathBuf {
        Config::state_dir().join("providers")
    }

    pub fn store_dir(&self, provider: &str) -> PathBuf {
        self.store_root.join(provider)
    }

    fn state_volume(&self, provider: &str) -> String {
        format!(
            "tracon-provider-{}",
            provider
                .bytes()
                .map(|b| if b.is_ascii_alphanumeric() {
                    b as char
                } else {
                    '-'
                })
                .collect::<String>()
        )
    }

    async fn clear_state_volume(&self, provider: &str) -> Result<(), ProviderError> {
        let empty = self
            .store_root
            .join(format!(".{}-empty-{}", provider, uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&empty)
            .map_err(|error| ProviderError::Failed(error.to_string()))?;
        let volume = self.state_volume(provider);
        let result = {
            // A clear replaces the volume wholesale. Held against the same
            // lock the lift's export takes, so a refresh or a finishing login
            // can never read the volume during the window where it does not
            // exist; without it the reader fails with "source does not exist"
            // and the credential is silently never lifted.
            let lock = crate::workspace::volume_lock(&volume);
            let _guard = lock.lock().await;
            self.backend
                .import_volume(&volume, &empty)
                .await
                .map_err(|error| ProviderError::Failed(error.to_string()))
        };
        let _ = std::fs::remove_dir_all(empty);
        result
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
                    "can_login": provider.login.is_some(),
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

    fn login_id(&self, name: &str, local_callback: bool) -> Result<(String, bool), ProviderError> {
        let provider = self
            .cfg
            .providers
            .get(name)
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))?;
        if !local_callback {
            if let Some(login) = &provider.device_login {
                return Ok((login.clone(), true));
            }
            if provider.requires_local_callback {
                return Err(ProviderError::RequiresLocalCallback(name.to_string()));
            }
        }
        provider
            .login
            .clone()
            .map(|login| (login, false))
            .ok_or_else(|| ProviderError::NoLogin(name.to_string()))
    }

    fn runner_for(&self, provider: &str) -> std::io::Result<Arc<dyn crate::runner::Runner>> {
        // The layout is the login client's, not the session harness's: a login
        // run by another adapter keeps its state where that adapter looks.
        Ok(self.backend.runner(vec![Mount::volume(
            self.state_volume(provider),
            state_target(
                &self.backend.harness_home(),
                self.logins.get(provider).layout(),
            ),
            false,
        )]))
    }

    pub async fn connect(
        self: &Arc<Self>,
        name: &str,
        channels: Vec<String>,
        owner: LoginOwner,
        local_callback: bool,
    ) -> Result<ConnectResult, ProviderError> {
        let (login, device_login) = self.login_id(name, local_callback)?;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
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
                    started_ms: now_ms(),
                    state: InflightState::Starting,
                },
            );
        }
        if let Err(error) = self.clear_state_volume(name).await {
            self.remove_generation(name, generation);
            return Err(error);
        }
        let login_process = format!("tracon-login-{name}-{generation}");

        let runner = match self.runner_for(name) {
            Ok(runner) => runner,
            Err(error) => {
                self.remove_generation(name, generation);
                return Err(ProviderError::Failed(error.to_string()));
            }
        };
        let state_dir = state_target(&self.backend.harness_home(), self.logins.get(name).layout());
        let flow = match self
            .logins
            .get(name)
            .login(runner.as_ref(), &login, &login_process, &state_dir)
            .await
        {
            Ok(flow) => flow,
            Err(error) => {
                self.remove_generation(name, generation);
                return Err(ProviderError::Failed(error.to_string()));
            }
        };

        // A login client whose only flow is a device code (OpenCode's Codex
        // login) says so by returning one, whatever the provider's table
        // names.
        let device_login = device_login || flow.device_code.is_some();
        let mut completion = if device_login {
            LoginCompletion::DeviceCode
        } else {
            LoginCompletion::Paste
        };
        let mut completion_note = None;
        let mut capture = None;
        let mut capture_events = None;
        if local_callback && !device_login {
            match CallbackTarget::parse(&flow.url) {
                Ok(target) => {
                    let port = target.port();
                    match CallbackCapture::start(target).await {
                        Ok((listener, events)) => {
                            completion = LoginCompletion::LocalCallback;
                            capture = Some(listener);
                            capture_events = Some(events);
                        }
                        Err(CallbackError::AddrInUse(_)) => {
                            completion_note = Some(format!(
                                "Local callback port {port} is unavailable; paste the redirect URL or code."
                            ));
                        }
                        Err(CallbackError::InvalidTarget(_)) => unreachable!(),
                        Err(CallbackError::Listener) => {
                            self.remove_generation(name, generation);
                            self.kill_login(name, generation).await;
                            return Err(ProviderError::Failed(
                                "Local callback listener could not start; connect again.".into(),
                            ));
                        }
                    }
                }
                // A redirect to the provider's own page is a login designed
                // around the paste-back, not a callback that went missing.
                Err(_) if callback::redirects_to_hosted_page(&flow.url) => {}
                Err(_) => {
                    completion_note = Some(
                        "This provider did not offer a usable localhost callback; paste the redirect URL or code."
                            .into(),
                    );
                }
            }
        }
        let device_code = if device_login {
            match flow.device_code.clone() {
                Some(code) => Some(code),
                None => {
                    self.remove_generation(name, generation);
                    self.kill_login(name, generation).await;
                    return Err(ProviderError::Failed(
                        "device sign-in did not provide a code; connect again.".into(),
                    ));
                }
            }
        } else {
            None
        };
        let result = ConnectResult {
            url: flow.url.clone(),
            completion,
            completion_note,
            device_code,
        };
        let stdin = Arc::new(tokio::sync::Mutex::new(flow.stdin));
        let installed = {
            let mut inflight = self.inflight.lock();
            match inflight.get_mut(name) {
                Some(slot)
                    if slot.generation == generation
                        && matches!(slot.state, InflightState::Starting) =>
                {
                    slot.state = InflightState::Pending(PendingLogin {
                        result: result.clone(),
                        stdin: stdin.clone(),
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
            self.kill_login(name, generation).await;
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

        let providers = self.clone();
        let provider = name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(LOGIN_TIMEOUT).await;
            providers
                .terminal(&provider, generation, "Sign-in timed out; connect again.")
                .await;
        });

        let providers = self.clone();
        let provider = name.to_string();
        let done = flow.done;
        tokio::spawn(async move {
            let exit = done.await;
            let removed = providers.take_generation(&provider, generation);
            let Some(slot) = removed else {
                return;
            };
            stop_capture(&slot);
            match exit {
                Ok(0) => match providers
                    .lift(&provider, &login, slot.channels, LiftKind::Fresh)
                    .await
                {
                    Ok(()) => {
                        providers.notes.lock().remove(&provider);
                        if let Some(callback) = providers.on_connected.get() {
                            callback();
                        }
                    }
                    Err(error) => providers.note(&provider, "failed", Some(error.to_string())),
                },
                _ => providers.note(
                    &provider,
                    "failed",
                    Some("Sign-in failed; connect again.".into()),
                ),
            }
            providers.publish();
        });

        Ok(result)
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
        let (stdin, capture, completion) = {
            let inflight = self.inflight.lock();
            let slot = inflight
                .get(name)
                .filter(|slot| slot.generation == generation)
                .ok_or_else(|| ProviderError::NotPending(name.to_string()))?;
            let InflightState::Pending(pending) = &slot.state else {
                return Err(ProviderError::NotPending(name.to_string()));
            };
            let mut state = pending.completion.lock();
            if *state != CompletionState::Open {
                return Err(ProviderError::Failed(
                    "this sign-in completion was already submitted".into(),
                ));
            }
            *state = CompletionState::Claimed(source);
            (
                pending.stdin.clone(),
                pending.capture.clone(),
                pending.completion.clone(),
            )
        };

        let mut line = trimmed.to_string();
        line.push('\n');
        let write = async {
            let mut writer = stdin.lock().await;
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        }
        .await;
        if let Err(error) = write {
            let mut state = completion.lock();
            if *state == CompletionState::Claimed(source) {
                *state = CompletionState::Open;
            }
            return Err(ProviderError::Failed(error.to_string()));
        }
        *completion.lock() = CompletionState::Finished;
        if let Some(capture) = capture {
            capture.stop();
        }
        Ok(())
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
            self.kill_login(name, slot.generation).await;
            shutdown_stdin(&slot).await;
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
        self.kill_login(name, generation).await;
        shutdown_stdin(&slot).await;
        self.note(name, "failed", Some(message.into()));
        self.publish();
    }

    async fn kill_login(&self, name: &str, generation: u64) {
        let _ = self
            .backend
            .runner(Vec::new())
            .kill(&format!("tracon-login-{name}-{generation}"))
            .await;
    }

    fn remove_generation(&self, name: &str, generation: u64) {
        let _ = self.take_generation(name, generation);
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

    async fn lift(
        &self,
        name: &str,
        login: &str,
        channels: Vec<String>,
        kind: LiftKind,
    ) -> Result<(), ProviderError> {
        let state = self.store_dir(name);
        let volume = self.state_volume(name);
        {
            let lock = crate::workspace::volume_lock(&volume);
            let _guard = lock.lock().await;
            self.backend
                .export_volume(&volume, &state)
                .await
                .map_err(|error| ProviderError::Failed(error.to_string()))?;
        }
        let lifted = self.logins.get(name).lift(&state, login).await;
        let _ = std::fs::remove_dir_all(&state);
        let token: LiftedToken =
            lifted.map_err(|error| ProviderError::Failed(error.to_string()))?;
        let credential_name = self.credential_name(name)?;
        let mut broker = self.broker.write().unwrap();
        let mut staged = broker.clone();
        let mut credential = match kind {
            LiftKind::Fresh => Credential {
                channels,
                ..Default::default()
            },
            LiftKind::Refresh => {
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
                credential
            }
        };
        credential.kind = KIND_OAUTH.into();
        credential.provider = Some(name.to_string());
        credential.nodes = vec![self.node_id.clone()];
        credential.expires_ms = token.expires_ms;
        credential.identity = token.identity;
        credential.env.insert("ACCESS_TOKEN".into(), token.access);
        if let Some(refresh) = token.refresh {
            credential.env.insert("REFRESH_TOKEN".into(), refresh);
        } else if matches!(kind, LiftKind::Fresh) {
            credential.env.remove("REFRESH_TOKEN");
        }
        if let Some(account_id) = token.account_id {
            credential
                .env
                .insert("CHATGPT_ACCOUNT_ID".into(), account_id);
        } else if matches!(kind, LiftKind::Fresh) {
            credential.env.remove("CHATGPT_ACCOUNT_ID");
        }
        staged.put(&credential_name, credential);
        staged
            .save(&self.store_key)
            .map_err(|error| ProviderError::Failed(error.to_string()))?;
        *broker = staged;
        Ok(())
    }

    pub async fn refresh(&self, name: &str) -> Result<(), ProviderError> {
        let login = self.login_id(name, true)?.0;
        let runner = self
            .runner_for(name)
            .map_err(|error| ProviderError::Failed(error.to_string()))?;
        self.logins
            .get(name)
            .refresh(runner.as_ref(), &login, &format!("tracon-refresh-{name}"))
            .await
            .map_err(|error| match error {
                crate::adapter::AdapterError::ReconnectRequired(reason) => {
                    ProviderError::ReconnectRequired(reason)
                }
                other => ProviderError::Failed(other.to_string()),
            })?;
        self.lift(name, &login, Vec::new(), LiftKind::Refresh)
            .await?;
        self.publish();
        Ok(())
    }

    pub fn due_for_refresh(&self, now: i64) -> Vec<String> {
        let broker = self.broker.read().unwrap();
        let notes = self.notes.lock();
        self.cfg
            .providers
            .iter()
            .filter(|(_, provider)| provider.login.is_some())
            // A credential whose adapter has already said it cannot be renewed
            // is not due for anything: asking again every tick would be a loop
            // that only stops when the operator reconnects.
            .filter(|(name, _)| {
                notes
                    .get(name.as_str())
                    .is_none_or(|note| note.state != NEEDS_RECONNECT)
            })
            .filter(|(name, _)| {
                broker
                    .model_credential_for(name, &self.node_id)
                    .map(|(_, credential): (&str, &Credential)| {
                        credential.kind == KIND_OAUTH
                            && credential
                                .expires_ms
                                .map(|expires| expires - now < REFRESH_AHEAD_MS)
                                .unwrap_or(false)
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
                    Ok(()) => tracing::info!(provider = %name, "token refreshed"),
                    // Nothing went wrong and nothing will change on its own:
                    // the note is what stops this being asked again, and it is
                    // cleared by the next successful connect.
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

async fn shutdown_stdin(slot: &Inflight) {
    if let InflightState::Pending(pending) = &slot.state {
        let _ = pending.stdin.lock().await.shutdown().await;
    }
}
