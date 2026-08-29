//! Connecting a model provider: the harness's own login flow, run as a
//! node-owned subprocess inside the boundary against a per-provider store the
//! node keeps and never mounts into a session. The URL and paste-back go
//! through the Nodes screen; the resulting token is lifted into the broker as
//! an `oauth` credential, and refreshed the same way on a timer. The vendor
//! logic stays in the vendor's binary; the node owns everything around it.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use proto::envelope::DataKey;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::{
    adapter::{HarnessAdapter, LiftedToken},
    boundary::Backend,
    broker::{Credential, SharedBroker, KIND_OAUTH},
    config::Config,
    runner::Mount,
    session::materialize::state_target,
    store::now_ms,
    stream::{Bus, Frame},
};

/// Refresh a token this far ahead of its expiry.
const REFRESH_AHEAD_MS: i64 = 30 * 60 * 1000;
/// How often the refresh loop looks.
pub const REFRESH_TICK_SECS: u64 = 5 * 60;

type Stdin = Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>>;

struct Inflight {
    url: String,
    channels: Vec<String>,
    stdin: Stdin,
    started_ms: i64,
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
    adapter: Arc<dyn HarnessAdapter>,
    backend: Arc<dyn Backend>,
    node_id: String,
    bus: Bus,
    inflight: Mutex<HashMap<String, Inflight>>,
    /// The last thing that happened to a provider that is not in the broker:
    /// a login that failed, a refresh that failed.
    notes: Mutex<HashMap<String, Note>>,
    /// What to do once a provider is connected: the model probe, once the
    /// node exists to run it.
    on_connected: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
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
    #[error("{0}")]
    Failed(String),
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
        Arc::new(Self {
            cfg,
            broker,
            store_key,
            adapter,
            backend,
            node_id,
            bus,
            inflight: Mutex::new(HashMap::new()),
            notes: Mutex::new(HashMap::new()),
            on_connected: std::sync::OnceLock::new(),
        })
    }

    pub fn set_on_connected(&self, f: Box<dyn Fn() + Send + Sync>) {
        let _ = self.on_connected.set(f);
    }

    /// The node-owned login store for one provider. Under the state
    /// directory so a pod-hosted node reaches it as a subPath of its claim.
    pub fn store_dir(provider: &str) -> PathBuf {
        Config::state_dir().join("providers").join(provider)
    }

    /// Every configured provider with its state: `connected` (a credential is
    /// usable here), `pending` (a login is waiting on the operator),
    /// `failed`, or `disconnected`.
    pub fn list(&self) -> Vec<Value> {
        let broker = self.broker.read().unwrap();
        let inflight = self.inflight.lock().unwrap();
        let notes = self.notes.lock().unwrap();
        self.cfg
            .providers
            .iter()
            .map(|(name, p)| {
                let cred = broker.model_credential_for(name, &self.node_id);
                let (state, url, error, updated_ms) = if let Some(i) = inflight.get(name) {
                    ("pending", Some(i.url.clone()), None, Some(i.started_ms))
                } else if let Some((_, c)) = cred {
                    ("connected", None, None, None.or(c.expires_ms))
                } else if let Some(n) = notes.get(name) {
                    (n.state, None, n.error.clone(), Some(n.updated_ms))
                } else {
                    ("disconnected", None, None, None)
                };
                json!({
                    "name": name,
                    "state": state,
                    "kind": cred.map(|(_, c)| c.kind.clone()),
                    "can_login": p.login.is_some(),
                    "url": url,
                    "error": error,
                    "identity": cred.and_then(|(_, c)| c.identity.clone()),
                    "expires_ms": cred.and_then(|(_, c)| c.expires_ms),
                    "channels": cred.map(|(_, c)| c.channels.clone()).unwrap_or_default(),
                    "updated_ms": updated_ms,
                })
            })
            .collect()
    }

    fn publish(&self) {
        self.bus.publish(Frame::Providers {
            providers: self.list(),
        });
    }

    fn note(&self, name: &str, state: &'static str, error: Option<String>) {
        self.notes.lock().unwrap().insert(
            name.to_string(),
            Note {
                state,
                error,
                updated_ms: now_ms(),
            },
        );
    }

    fn login_id(&self, name: &str) -> Result<String, ProviderError> {
        let p = self
            .cfg
            .providers
            .get(name)
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))?;
        p.login
            .clone()
            .ok_or_else(|| ProviderError::NoLogin(name.to_string()))
    }

    fn runner_for(&self, provider: &str) -> std::io::Result<Arc<dyn crate::runner::Runner>> {
        let dir = Self::store_dir(provider);
        std::fs::create_dir_all(dir.join("agent"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(self.backend.runner(vec![Mount {
            source: dir.to_string_lossy().into_owned(),
            target: state_target(&self.backend.harness_home(), self.adapter.layout()),
            read_only: false,
        }]))
    }

    /// Start the harness's login for a provider and return the URL the
    /// operator opens. The subprocess waits for the paste-back on its stdin.
    pub async fn connect(
        self: &Arc<Self>,
        name: &str,
        channels: Vec<String>,
    ) -> Result<String, ProviderError> {
        let login = self.login_id(name)?;
        if self.inflight.lock().unwrap().contains_key(name) {
            return Err(ProviderError::Busy(name.to_string()));
        }
        let runner = self
            .runner_for(name)
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        let flow = self
            .adapter
            .login(runner.as_ref(), &login, &format!("tracon-login-{name}"))
            .await
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        let url = flow.url.clone();
        self.inflight.lock().unwrap().insert(
            name.to_string(),
            Inflight {
                url: url.clone(),
                channels,
                stdin: Arc::new(tokio::sync::Mutex::new(flow.stdin)),
                started_ms: now_ms(),
            },
        );
        self.notes.lock().unwrap().remove(name);
        self.publish();

        // Watch the subprocess: when it ends, lift what it stored or record
        // why it failed.
        let me = self.clone();
        let name = name.to_string();
        let done = flow.done;
        let output = flow.output;
        tokio::spawn(async move {
            let code = done.await;
            let removed = me.inflight.lock().unwrap().remove(&name);
            let channels = removed.map(|i| i.channels).unwrap_or_default();
            match code {
                Ok(0) => match me.lift(&name, &login, channels).await {
                    Ok(()) => {
                        me.notes.lock().unwrap().remove(&name);
                        tracing::info!(provider = %name, "provider connected");
                        if let Some(f) = me.on_connected.get() {
                            f();
                        }
                    }
                    Err(e) => {
                        tracing::warn!(provider = %name, error = %e, "login finished but nothing was lifted");
                        me.note(&name, "failed", Some(e.to_string()));
                    }
                },
                Ok(code) => {
                    let tail = output.lock().unwrap().clone();
                    let tail = tail
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .to_string();
                    tracing::warn!(provider = %name, code, "login exited");
                    me.note(
                        &name,
                        "failed",
                        Some(format!("login exited with {code}: {tail}")),
                    );
                }
                Err(e) => me.note(&name, "failed", Some(e.to_string())),
            }
            me.publish();
        });
        Ok(url)
    }

    /// The operator pasted the redirect URL or code: hand it to the waiting
    /// login.
    pub async fn code(&self, name: &str, text: &str) -> Result<(), ProviderError> {
        let stdin = self
            .inflight
            .lock()
            .unwrap()
            .get(name)
            .map(|i| i.stdin.clone())
            .ok_or_else(|| ProviderError::NotPending(name.to_string()))?;
        let mut line = text.trim().to_string();
        line.push('\n');
        let mut w = stdin.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        w.flush()
            .await
            .map_err(|e| ProviderError::Failed(e.to_string()))
    }

    /// Cancel a pending login, or forget a connected provider's credential.
    pub async fn disconnect(&self, name: &str) -> Result<(), ProviderError> {
        let pending = self.inflight.lock().unwrap().remove(name);
        if let Some(i) = pending {
            // Closing stdin ends the readline the login waits on; the kill
            // covers a login that does not notice.
            let _ = i.stdin.lock().await.shutdown().await;
            let _ = self
                .backend
                .runner(Vec::new())
                .kill(&format!("tracon-login-{name}"))
                .await;
        }
        let cred_name = self.credential_name(name)?;
        {
            let mut b = self.broker.write().unwrap();
            if b.remove(&cred_name) {
                if let Err(e) = b.save(&self.store_key) {
                    return Err(ProviderError::Failed(e.to_string()));
                }
            }
        }
        self.notes.lock().unwrap().remove(name);
        self.publish();
        Ok(())
    }

    fn credential_name(&self, name: &str) -> Result<String, ProviderError> {
        self.cfg
            .providers
            .get(name)
            .map(|p| p.credential.clone())
            .ok_or_else(|| ProviderError::Unknown(name.to_string()))
    }

    /// Read what the login stored and put it in the broker, pinned to this
    /// node and bound to `channels`. A refresh keeps the existing bindings.
    async fn lift(
        &self,
        name: &str,
        login: &str,
        channels: Vec<String>,
    ) -> Result<(), ProviderError> {
        let dir = Self::store_dir(name);
        let tok: LiftedToken = self
            .adapter
            .lift(&dir, login)
            .await
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        let cred_name = self.credential_name(name)?;
        let mut b = self.broker.write().unwrap();
        let mut cred = b.get(&cred_name).cloned().unwrap_or_default();
        if !channels.is_empty() {
            cred.channels = channels;
        }
        cred.kind = KIND_OAUTH.into();
        cred.provider = Some(name.to_string());
        cred.nodes = vec![self.node_id.clone()];
        cred.expires_ms = tok.expires_ms;
        cred.identity = tok.identity;
        cred.env.insert("ACCESS_TOKEN".into(), tok.access);
        if let Some(r) = tok.refresh {
            cred.env.insert("REFRESH_TOKEN".into(), r);
        }
        b.put(&cred_name, cred);
        b.save(&self.store_key)
            .map_err(|e| ProviderError::Failed(e.to_string()))
    }

    /// Run the harness's refresh for a provider and lift the result.
    pub async fn refresh(&self, name: &str) -> Result<(), ProviderError> {
        let login = self.login_id(name)?;
        let runner = self
            .runner_for(name)
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        self.adapter
            .refresh(runner.as_ref(), &login, &format!("tracon-refresh-{name}"))
            .await
            .map_err(|e| ProviderError::Failed(e.to_string()))?;
        self.lift(name, &login, Vec::new()).await?;
        self.publish();
        Ok(())
    }

    /// Providers whose `oauth` credential expires within the refresh window.
    pub fn due_for_refresh(&self, now: i64) -> Vec<String> {
        let broker = self.broker.read().unwrap();
        self.cfg
            .providers
            .iter()
            .filter(|(_, p)| p.login.is_some())
            .filter(|(name, _)| {
                broker
                    .model_credential_for(name, &self.node_id)
                    .map(|(_, c): (&str, &Credential)| {
                        c.kind == KIND_OAUTH
                            && c.expires_ms
                                .map(|e| e - now < REFRESH_AHEAD_MS)
                                .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The refresh loop: ahead of expiry, never on demand from a request.
    pub async fn refresh_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(REFRESH_TICK_SECS));
        loop {
            tick.tick().await;
            for name in self.due_for_refresh(now_ms()) {
                match self.refresh(&name).await {
                    Ok(()) => tracing::info!(provider = %name, "token refreshed"),
                    Err(e) => {
                        tracing::warn!(provider = %name, error = %e, "token refresh failed");
                        self.note(&name, "failed", Some(format!("refresh failed: {e}")));
                        self.publish();
                    }
                }
            }
        }
    }
}
