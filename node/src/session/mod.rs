//! Session lifecycle: create a worktree, materialize config, spawn the harness
//! inside the boundary, and hand it to a supervisor.

pub mod chunks;
pub mod materialize;
pub mod state;
pub mod supervisor;
pub mod worktree;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration, time::Instant};

use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::{
    adapter::{HarnessAdapter, LaunchSpec},
    config::Config,
    runner::{podman::PodmanRunner, podman::RunSpec, Runner},
    session::{
        state::{event_kind as ek, EndReason, SessionState},
        supervisor::{Command, Supervisor},
    },
    store::{now_ms, NewEvent, SessionPatch, SessionRow, Store},
    stream::{Frame, Hub},
};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewSession {
    pub channel: String,
    pub repo_path: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub work_item_id: Option<String>,
    /// Required, with no default: a session without an explicit model is a
    /// validation failure rather than a silent choice.
    pub model: String,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("model is required")]
    ModelRequired,
    #[error("budget must be greater than zero")]
    BadBudget,
    #[error("node refuses to run harnesses: {0}")]
    NodeRefused(String),
    #[error("harness version mismatch: node expects {pinned}, host has {found}")]
    VersionMismatch { found: String, pinned: String },
    #[error("session not found")]
    NotFound,
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// Running sessions, by id. A session that has ended leaves the map; its rows
/// and events stay in the store.
#[derive(Clone)]
pub struct Manager {
    pub(crate) tools: Arc<crate::mcp::Tools>,
    store: Arc<Store>,
    hub: Hub,
    cfg: Arc<Config>,
    node_id: String,
    live: Arc<Mutex<HashMap<String, mpsc::Sender<Command>>>>,
    /// Session id → (tool token, channel). A token is minted when a session
    /// starts and dropped when it ends, so it authorises exactly one session
    /// for exactly as long as that session runs.
    tokens: Arc<Mutex<HashMap<String, (String, String)>>>,
}

impl Manager {
    pub fn new(
        store: Arc<Store>,
        hub: Hub,
        cfg: Arc<Config>,
        node_id: String,
        tools: Arc<crate::mcp::Tools>,
    ) -> Self {
        Self {
            tools,
            store,
            hub,
            cfg,
            node_id,
            live: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a tool token directly. Tests drive the MCP route without
    /// starting a harness; sessions always go through `start`.
    #[doc(hidden)]
    pub async fn register_tool_token_for_test(&self, session_id: &str, channel: &str) -> String {
        let token = mint_token();
        self.tokens
            .lock()
            .await
            .insert(session_id.to_string(), (token.clone(), channel.to_string()));
        token
    }

    /// The channel a tool call may act as, or `None` if the token does not
    /// match a live session. Compared in constant time: a token is a secret.
    pub async fn authorize_tool_call(&self, session_id: &str, presented: &str) -> Option<String> {
        let tokens = self.tokens.lock().await;
        let (expected, channel) = tokens.get(session_id)?;
        if constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
            Some(channel.clone())
        } else {
            None
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Validate, insert the row, and start the session in the background. The
    /// row exists before the harness does, so the interface can show a session
    /// that is still starting.
    pub async fn create(
        &self,
        spec: NewSession,
        adapter: Arc<dyn HarnessAdapter>,
    ) -> Result<SessionRow, SessionError> {
        if spec.model.trim().is_empty() {
            return Err(SessionError::ModelRequired);
        }
        let budget = spec.budget_tokens.unwrap_or(self.cfg.session.budget_tokens);
        if budget <= 0 {
            return Err(SessionError::BadBudget);
        }
        if let Some(node) = self.store.get_node(&self.node_id)? {
            if node.state != "ready" {
                return Err(SessionError::NodeRefused(
                    node.failed_detail
                        .or(node.failed_check)
                        .unwrap_or_else(|| "boundary check failed".into()),
                ));
            }
            if let Some(found) = node.harness_found.filter(|f| f != &node.harness_pinned) {
                return Err(SessionError::VersionMismatch {
                    found,
                    pinned: node.harness_pinned,
                });
            }
        }

        let id = uuid::Uuid::now_v7().to_string();
        let slug = id.split('-').next_back().unwrap_or("session").to_string();
        let branch = spec
            .branch
            .clone()
            .unwrap_or_else(|| format!("feat/tracon-{slug}"));
        let row = SessionRow {
            id: id.clone(),
            node_id: self.node_id.clone(),
            channel: spec.channel.clone(),
            work_item_id: spec.work_item_id.clone(),
            repo_path: spec.repo_path.clone(),
            worktree_path: None,
            branch: branch.clone(),
            harness_id: adapter.id().to_string(),
            harness_version: adapter.pinned_version().to_string(),
            harness_session_id: None,
            container_name: None,
            model: spec.model.clone(),
            budget_tokens: budget,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: SessionState::Starting.as_str().into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: now_ms(),
            started_mono_ms: Some(0),
            ended_mono_ms: None,
            updated_ms: now_ms(),
        };
        self.store.insert_session(&row)?;
        self.hub.publish(Frame::Session(Box::new(row.clone())));

        let this = self.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            if let Err(e) = this.start(&id, spec, branch, slug, adapter, started).await {
                tracing::error!(session = %id, error = %e, "session failed to start");
                // A session that never started holds no capability.
                this.tokens.lock().await.remove(&id);
                let _ = this.store.update_session(
                    &id,
                    SessionPatch {
                        state: Some(SessionState::Failed.as_str().into()),
                        end_reason: Some(EndReason::Error.as_str().into()),
                        last_error: Some(e.to_string()),
                        ended_mono_ms: Some(started.elapsed().as_millis() as i64),
                        ..Default::default()
                    },
                );
                let _ = this.store.append_event(&NewEvent {
                    session_id: id.clone(),
                    work_item_id: None,
                    kind: ek::ERROR.into(),
                    ref_id: None,
                    payload: json!({ "error": e.to_string() }),
                    at_ms: now_ms(),
                    mono_ms: started.elapsed().as_millis() as i64,
                });
                if let Ok(Some(row)) = this.store.get_session(&id) {
                    this.hub.publish(Frame::Session(Box::new(row)));
                }
            }
        });
        Ok(row)
    }

    /// Persist a lifecycle event and publish it on the stream, so a client
    /// watching live sees the same log a reload rebuilds.
    fn record(&self, e: NewEvent) {
        match self.store.append_event(&e) {
            Ok(seq) => self.hub.publish(Frame::Event {
                seq,
                session_id: e.session_id,
                kind: e.kind,
                ref_id: e.ref_id,
                payload: e.payload,
                at_ms: e.at_ms,
            }),
            Err(err) => tracing::error!(error = %err, "failed to persist event"),
        }
    }

    async fn start(
        &self,
        id: &str,
        spec: NewSession,
        branch: String,
        slug: String,
        adapter: Arc<dyn HarnessAdapter>,
        started: Instant,
    ) -> anyhow::Result<()> {
        // Registered before the harness starts: it connects to the node's MCP
        // server during `session/new`, so a token published afterwards is
        // published too late and the node refuses its own harness.
        let token = mint_token();
        self.tokens
            .lock()
            .await
            .insert(id.to_string(), (token.clone(), spec.channel.clone()));

        let repo = PathBuf::from(&spec.repo_path);
        let wt = worktree::create(&repo, &self.cfg.session.worktree_root, &branch, &slug).await?;
        self.store.update_session(
            id,
            SessionPatch {
                worktree_path: Some(wt.path.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )?;
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::WORKTREE.into(),
            ref_id: None,
            payload: json!({
                "path": wt.path, "branch": wt.branch, "base": wt.base,
                "main_checkout_dirty": wt.main_checkout_dirty
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        let scratch = materialize::scratch_for(id, &wt.path)?;
        let selinux = crate::boundary::selinux_enabled().await;
        let mut run_spec = RunSpec::from_config(&self.cfg, selinux);
        run_spec.extra_mounts = scratch.mounts;
        let container = format!("tracon-h-{slug}");
        let runner: Arc<dyn Runner> = Arc::new(PodmanRunner::new(run_spec));

        // The harness reaches the node only through the gateway's forward, and
        // only with this session's token. Tools are offered only if the
        // channel has a credential bound to it; otherwise the harness is given
        // no MCP server at all rather than one that refuses everything.
        let mcp_servers = if self.tools.list(&spec.channel).is_empty() {
            Vec::new()
        } else {
            vec![json!({
                "type": "http",
                "name": "tracon",
                "url": format!(
                    "http://{}:{}/mcp/{id}",
                    self.cfg.boundary.gateway_container, self.cfg.gateway.forward_port
                ),
                "headers": [{ "name": "Authorization", "value": format!("Bearer {token}") }],
            })]
        };

        let (handle, events) = adapter
            .launch(
                runner.as_ref(),
                LaunchSpec {
                    cwd_in_runner: "/work".into(),
                    model: spec.model.clone(),
                    container_name: container.clone(),
                    mcp_servers,
                },
            )
            .await?;

        self.store.update_session(
            id,
            SessionPatch {
                container_name: Some(container.clone()),
                harness_session_id: Some(handle.harness_session_id().to_string()),
                ..Default::default()
            },
        )?;
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::SESSION_STARTED.into(),
            ref_id: None,
            payload: json!({ "model": spec.model, "harness": adapter.id() }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let cmd_tx_for_turns = cmd_tx.clone();
        self.live.lock().await.insert(id.to_string(), cmd_tx);

        let sup = Supervisor::new(
            id.to_string(),
            self.store.clone(),
            self.hub.clone(),
            Arc::from(handle),
            started,
            Duration::from_secs(self.cfg.session.permission_timeout_secs),
            cmd_tx_for_turns,
            runner,
            container.clone(),
        );
        let live = self.live.clone();
        let tokens = self.tokens.clone();
        let sid = id.to_string();
        tokio::spawn(async move {
            sup.run(events, cmd_rx).await;
            live.lock().await.remove(&sid);
            // The token dies with the session; a later call with it is refused.
            tokens.lock().await.remove(&sid);
            materialize::remove(&sid);
        });
        Ok(())
    }

    async fn send(&self, id: &str, cmd: Command) -> Result<(), SessionError> {
        let tx = self.live.lock().await.get(id).cloned();
        match tx {
            Some(tx) => tx
                .send(cmd)
                .await
                .map_err(|_| SessionError::Rejected("session is no longer running".into())),
            None => Err(SessionError::Rejected(
                "session is not running on this node".into(),
            )),
        }
    }

    pub async fn prompt(&self, id: &str, text: String) -> Result<(), SessionError> {
        let (ack, wait) = oneshot::channel();
        self.send(id, Command::Prompt { text, ack }).await?;
        wait.await
            .map_err(|_| SessionError::Rejected("session stopped".into()))?
            .map_err(SessionError::Rejected)
    }

    pub async fn answer(&self, id: &str, option_id: String) -> Result<(), SessionError> {
        let perm = self
            .store
            .get_permission(id)?
            .ok_or(SessionError::NotFound)?;
        let (ack, wait) = oneshot::channel();
        self.send(
            &perm.session_id,
            Command::Answer {
                permission_id: id.to_string(),
                option_id,
                ack,
            },
        )
        .await?;
        wait.await
            .map_err(|_| SessionError::Rejected("session stopped".into()))?
            .map_err(SessionError::Rejected)
    }

    pub async fn kill(&self, id: &str) -> Result<(), SessionError> {
        self.send(id, Command::Kill).await
    }

    /// Graceful shutdown: ask every live session to end and wait briefly.
    /// Containers are removed by each supervisor's teardown.
    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = self.live.lock().await.keys().cloned().collect();
        for id in &ids {
            let _ = self.send(id, Command::Kill).await;
        }
        for _ in 0..50 {
            if self.live.lock().await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        tracing::warn!("some sessions did not stop in time");
    }
}

/// A restarted node owns no running harnesses: rows a previous process left
/// non-terminal are closed honestly, their open permission requests expired,
/// and their containers removed. Without this, an orphaned harness keeps the
/// credential store open and the model probe fails underneath it.
pub async fn reconcile_after_restart(store: &Store) -> Vec<String> {
    let mut cleaned = Vec::new();
    let Ok(sessions) = store.list_sessions(None) else {
        return cleaned;
    };
    for s in sessions {
        if state::SessionState::from_stored(&s.state).is_terminal() {
            continue;
        }
        for p in store.open_permissions().unwrap_or_default() {
            if p.session_id == s.id {
                let _ = store.resolve_permission(&p.id, "expired", None, 0);
                let _ = store.append_event(&NewEvent {
                    session_id: s.id.clone(),
                    work_item_id: None,
                    kind: ek::PERMISSION_EXPIRED.into(),
                    ref_id: Some(p.id.clone()),
                    payload: json!({ "permission_id": p.id, "reason": "denied: node restarted" }),
                    at_ms: now_ms(),
                    mono_ms: 0,
                });
            }
        }
        if let Some(container) = &s.container_name {
            let _ = tokio::process::Command::new("podman")
                .args(["rm", "-f", "-i", container])
                .output()
                .await;
        }
        let _ = store.update_session(
            &s.id,
            SessionPatch {
                state: Some(state::SessionState::Closed.as_str().into()),
                end_reason: Some(state::EndReason::HarnessExit.as_str().into()),
                last_error: Some("node restarted while the session was live".into()),
                turn_active: Some(false),
                ..Default::default()
            },
        );
        let _ = store.append_event(&NewEvent {
            session_id: s.id.clone(),
            work_item_id: None,
            kind: ek::STATE.into(),
            ref_id: None,
            payload: json!({ "state": "closed", "end_reason": "harness_exit", "reason": "node restarted" }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        cleaned.push(s.id);
    }
    cleaned
}

/// A URL-safe random token. Not a session id: ids appear in logs and the
/// interface, and a capability must not be guessable from something displayed.
fn mint_token() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Comparison that does not leak how much of a token matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
