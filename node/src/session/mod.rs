//! Session lifecycle: create a worktree, materialize config, spawn the harness
//! inside the boundary, and hand it to a supervisor.

pub mod chunks;
pub mod external;
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
    runner::Runner,
    session::{
        state::{event_kind as ek, EndReason, SessionState},
        supervisor::{Command, Supervisor},
    },
    store::{now_ms, NewEvent, SessionPatch, SessionRow, Store},
    stream::{Bus, Frame},
};

/// Startup is bounded separately from a turn: no model work has happened yet,
/// so a stuck container or ACP handshake is a failed start, not a half-live
/// session.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
const COMMAND_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewSession {
    pub channel: String,
    pub repo_path: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub work_item_id: Option<String>,
    /// Empty resolves deterministically from the channel phase binding, then
    /// the owner node's offered catalogue. The resolved value is persisted
    /// before the harness starts.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
    /// The first unstructured prompt, queued only after the harness reaches
    /// `running`. Structured work-item sessions leave this absent.
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// The node to run on. Absent or this node: here. Another node: forwarded
    /// to it, which validates and starts the session.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Which phase of the item this session is. Plan and execute need an
    /// item; execute needs the item's plan. Review sessions are spawned by
    /// the node against a review.
    #[serde(default)]
    pub phase: Phase,
    /// Review sessions only: the review to read, and the commit to check out
    /// (the worktree is created at it rather than at origin's default).
    #[serde(default)]
    pub review_id: Option<String>,
    #[serde(default)]
    pub base_sha: Option<String>,
    /// Resume a durable runtime-owned workspace rather than importing a new
    /// selected checkout. The value is a workspace id, never a host path.
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Plan,
    #[default]
    Execute,
    Review,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Execute => "execute",
            Self::Review => "review",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no usable model: name one, bind the channel phase, or connect a provider")]
    ModelRequired,
    #[error("budget must not be negative")]
    BadBudget,
    #[error("node refuses to run harnesses: {0}")]
    NodeRefused(String),
    #[error("harness version mismatch: node expects {pinned}, host has {found}")]
    VersionMismatch { found: String, pinned: String },
    #[error("session not found")]
    NotFound,
    #[error("channel {0} is not one this node holds keys for; create or enroll it first")]
    UnknownChannel(String),
    #[error("channel {0} is archived: bring it back to start work on it again")]
    ChannelArchived(String),
    #[error("a work item is required: pick one from the ready list")]
    WorkItemRequired,
    #[error("work item is not ready: {0}")]
    NotReady(String),
    #[error("work item is already held by session {0}")]
    InSession(String),
    #[error("execute needs a plan: run a plan session for this item first ({0})")]
    PlanRequired(String),
    #[error("channel is at its daily ceiling: {0}")]
    Ceiling(String),
    /// The session belongs to another node: `(node_id, channel)`.
    #[error("session is owned by node {0}")]
    Remote(String, String),
    #[error("node {0} did not answer; it may be unreachable")]
    PeerUnreachable(String),
    #[error("this node is not on a mesh; the session's owner cannot be reached")]
    NoMesh,
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
    /// Shared with every supervisor and the mesh client, which swaps in a
    /// bundle handed off by a peer.
    policy: Arc<std::sync::RwLock<crate::policy::Policy>>,
    store: Arc<Store>,
    bus: Bus,
    cfg: Arc<Config>,
    node_id: String,
    live: Arc<Mutex<HashMap<String, mpsc::Sender<Command>>>>,
    /// Session id → (tool token, channel). A token is minted when a session
    /// starts and dropped when it ends, so it authorises exactly one session
    /// for exactly as long as that session runs.
    tokens: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Channel → the session an externally attached harness acts as. One per
    /// channel: a second terminal on the same channel joins the attachment
    /// rather than opening a second one, so the home shows what is connected
    /// and not how many windows are open.
    external: Arc<Mutex<HashMap<String, external::Attachment>>>,
    /// The node's own model probe presents this to the gateway; it may only
    /// read, and it names no channel.
    probe_token: String,
    /// The hub client, once one exists: commands for sessions other nodes own
    /// are forwarded through it.
    mesh: Arc<std::sync::OnceLock<Arc<crate::mesh::client::MeshClient>>>,
    /// The boundary every harness runs behind.
    backend: Arc<dyn crate::boundary::Backend>,
    /// Provider logins, once the node exists to run them.
    providers: Arc<std::sync::OnceLock<Arc<crate::providers::Providers>>>,
    /// The node's harness adapter, for sessions the node spawns itself.
    adapter: Arc<std::sync::OnceLock<Arc<dyn HarnessAdapter>>>,
    previews: Arc<crate::http::preview::PreviewTokens>,
}

impl Manager {
    pub fn new(
        store: Arc<Store>,
        bus: Bus,
        cfg: Arc<Config>,
        node_id: String,
        tools: Arc<crate::mcp::Tools>,
        policy: Arc<std::sync::RwLock<crate::policy::Policy>>,
        backend: Arc<dyn crate::boundary::Backend>,
    ) -> Self {
        Self {
            tools,
            policy,
            backend,
            store,
            bus,
            cfg,
            node_id,
            live: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            external: Arc::new(Mutex::new(HashMap::new())),
            probe_token: mint_token(),
            mesh: Arc::new(std::sync::OnceLock::new()),
            providers: Arc::new(std::sync::OnceLock::new()),
            adapter: Arc::new(std::sync::OnceLock::new()),
            previews: Arc::new(crate::http::preview::PreviewTokens::default()),
        }
    }

    pub fn backend(&self) -> &Arc<dyn crate::boundary::Backend> {
        &self.backend
    }

    pub fn cfg(&self) -> &Arc<Config> {
        &self.cfg
    }

    pub fn previews(&self) -> &Arc<crate::http::preview::PreviewTokens> {
        &self.previews
    }

    /// Create a session on this node with the node's own adapter, for
    /// sessions the node spawns itself (review sessions).
    pub async fn create_local(&self, spec: NewSession) -> Result<SessionRow, SessionError> {
        let adapter = self
            .adapter
            .get()
            .cloned()
            .ok_or_else(|| SessionError::Rejected("no harness adapter yet".into()))?;
        self.create(spec, adapter).await
    }

    /// The node's adapter, set once at startup so node-spawned sessions can
    /// use it.
    pub fn set_adapter(&self, adapter: Arc<dyn HarnessAdapter>) {
        let _ = self.adapter.set(adapter);
    }

    pub fn set_mesh(&self, mesh: Arc<crate::mesh::client::MeshClient>) {
        let _ = self.mesh.set(mesh);
    }

    pub fn mesh(&self) -> Option<&Arc<crate::mesh::client::MeshClient>> {
        self.mesh.get()
    }

    pub fn set_providers(&self, p: Arc<crate::providers::Providers>) {
        let _ = self.providers.set(p);
    }

    pub fn providers(&self) -> Option<&Arc<crate::providers::Providers>> {
        self.providers.get()
    }

    /// Run a command on the node that owns a session. A prompt to an owner
    /// that is unreachable is queued and delivered when it returns (the
    /// operator asked for it; the outbox keeps it); anything else must be
    /// answered now or fail, because the operator is waiting on the result.
    async fn forward(
        &self,
        node_id: &str,
        command: proto::frame::Command,
        queue_if_unreachable: bool,
    ) -> Result<serde_json::Value, SessionError> {
        let mesh = self.mesh.get().ok_or(SessionError::NoMesh)?;
        let timeout = Duration::from_secs(self.cfg.mesh.command_timeout_secs.max(1));
        if queue_if_unreachable && !mesh.peer_reachable(node_id) {
            mesh.send_command(node_id, command)
                .map_err(|e| SessionError::Rejected(e.to_string()))?;
            return Ok(json!({ "queued": true }));
        }
        match mesh.command(node_id, command, timeout).await {
            Ok(v) => Ok(v),
            Err(crate::mesh::forward::CommandError::Timeout) => {
                Err(SessionError::PeerUnreachable(node_id.to_string()))
            }
            Err(crate::mesh::forward::CommandError::Refused(m)) => Err(SessionError::Rejected(m)),
            Err(crate::mesh::forward::CommandError::Local(m)) => Err(SessionError::Rejected(m)),
        }
    }

    /// Register a tool token directly. Tests drive the MCP route without
    /// starting a harness; sessions always go through `start`. A token only
    /// authorizes a live session, so a session the test never created is
    /// recorded as running here.
    #[doc(hidden)]
    pub async fn register_tool_token_for_test(&self, session_id: &str, channel: &str) -> String {
        if self.store.get_session(session_id).ok().flatten().is_none() {
            let now = crate::store::now_ms();
            self.store
                .ensure_peer_node(&self.node_id)
                .expect("test session node row");
            self.store
                .insert_session(&SessionRow {
                    id: session_id.to_string(),
                    node_id: self.node_id.clone(),
                    channel: channel.to_string(),
                    work_item_id: None,
                    repo_path: String::new(),
                    worktree_path: None,
                    branch: String::new(),
                    harness_id: "fake".into(),
                    harness_version: String::new(),
                    harness_session_id: None,
                    container_name: None,
                    model: String::new(),
                    project_id: None,
                    phase: Phase::Execute.as_str().into(),
                    policy_version: None,
                    review_id: None,
                    budget_tokens: 0,
                    tokens_used: 0,
                    cost_usd: None,
                    context_used: None,
                    context_size: None,
                    state: SessionState::Running.as_str().into(),
                    end_reason: None,
                    last_error: None,
                    turn_active: 0,
                    draft: None,
                    draft_updated_ms: None,
                    created_ms: now,
                    started_mono_ms: Some(0),
                    ended_mono_ms: None,
                    updated_ms: now,
                    archived_ms: None,
                })
                .expect("test session row");
        }
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
        let channel = {
            let tokens = self.tokens.lock().await;
            let (expected, channel) = tokens.get(session_id)?;
            constant_time_eq(expected.as_bytes(), presented.as_bytes()).then(|| channel.clone())?
        };
        self.session_allows_work(session_id).then_some(channel)
    }

    /// The session a gateway request belongs to, from the placeholder key it
    /// carries: `(session_id, channel)`.
    pub async fn session_for_token(&self, presented: &str) -> Option<(String, String)> {
        let found = {
            let tokens = self.tokens.lock().await;
            tokens
                .iter()
                .find(|(_, (expected, _))| {
                    constant_time_eq(expected.as_bytes(), presented.as_bytes())
                })
                .map(|(id, (_, channel))| (id.clone(), channel.clone()))
        };
        let (id, channel) = found?;
        self.session_allows_work(&id).then_some((id, channel))
    }

    /// Reassert a session fence immediately before an irreversible brokered
    /// action. Token validation authorizes a request at ingress; callers that
    /// await policy, checks, or an external service must call this again at
    /// dispatch so a concurrent pause or stop wins.
    pub fn ensure_active(&self, session_id: &str) -> Result<(), SessionError> {
        let row = self
            .store
            .get_session(session_id)?
            .ok_or(SessionError::NotFound)?;
        let state = SessionState::from_stored(&row.state);
        if state == SessionState::Paused {
            return Err(SessionError::Rejected("session is paused".into()));
        }
        if state.is_terminal() {
            return Err(SessionError::Rejected("session is no longer active".into()));
        }
        Ok(())
    }

    fn session_allows_work(&self, session_id: &str) -> bool {
        self.ensure_active(session_id).is_ok()
    }
    pub fn probe_token(&self) -> &str {
        &self.probe_token
    }

    pub fn is_probe_token(&self, presented: &str) -> bool {
        constant_time_eq(self.probe_token.as_bytes(), presented.as_bytes())
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Take a checked, node-owned snapshot of a durable workspace. Review,
    /// export, and publication consume this path; no caller receives the
    /// mutable runtime volume or an operator-selected source path.
    pub async fn snapshot_workspace(&self, session_id: &str) -> Result<PathBuf, SessionError> {
        let session = self
            .store
            .get_session(session_id)?
            .ok_or(SessionError::NotFound)?;
        if session.node_id != self.node_id {
            return Err(SessionError::Remote(session.node_id, session.channel));
        }
        if session.harness_id == external::HARNESS_ID {
            return session
                .worktree_path
                .map(PathBuf::from)
                .ok_or_else(|| SessionError::Rejected("external session has no worktree".into()));
        }
        let workspace_id = session
            .repo_path
            .strip_prefix("workspace://")
            .unwrap_or(&session.id);
        let workspace = crate::workspace::Workspace {
            id: workspace_id.to_string(),
            volume: crate::workspace::volume_name(workspace_id),
            snapshot: crate::workspace::snapshot_path(workspace_id),
        };
        let snapshot = crate::workspace::export(self.backend.as_ref(), &workspace)
            .await
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        crate::workspace::sanitize_git(&snapshot)
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        self.store.update_session(
            session_id,
            SessionPatch {
                worktree_path: Some(snapshot.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )?;
        Ok(snapshot)
    }

    /// The same snapshot, addressed by either a session id or the id of an
    /// imported workspace that has not run a session yet: an operator can
    /// export and download what they imported without starting anything.
    pub async fn snapshot_workspace_or_session(&self, id: &str) -> Result<PathBuf, SessionError> {
        if self.store.get_session(id)?.is_some() {
            return self.snapshot_workspace(id).await;
        }
        let workspace = crate::workspace::Workspace {
            id: id.to_string(),
            volume: crate::workspace::volume_name(id),
            snapshot: crate::workspace::snapshot_path(id),
        };
        let snapshot = crate::workspace::export(self.backend.as_ref(), &workspace)
            .await
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        crate::workspace::sanitize_git(&snapshot)
            .map_err(|e| SessionError::Rejected(e.to_string()))?;
        Ok(snapshot)
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn policy(&self) -> &Arc<std::sync::RwLock<crate::policy::Policy>> {
        &self.policy
    }

    /// Republish the waiting bay. Called when a review arrives or is decided,
    /// so the queue updates without the operator refetching.
    pub async fn publish_queue(&self) {
        let waiting = self.store.open_permissions().unwrap_or_default();
        self.bus.publish(Frame::Queue { waiting });
        if let Ok(reviews) = self.store.open_reviews() {
            self.bus.publish(Frame::Reviews { waiting: reviews });
        }
    }

    /// Validate, insert the row, and start the session in the background. The
    /// row exists before the harness does, so the interface can show a session
    /// that is still starting.
    pub async fn create(
        &self,
        mut spec: NewSession,
        adapter: Arc<dyn HarnessAdapter>,
    ) -> Result<SessionRow, SessionError> {
        // Asked to run elsewhere: the owner validates and starts it; its row
        // arrives back both in the ack and, shortly, as a mirrored session.
        if let Some(node) = spec.node_id.as_deref().filter(|n| *n != self.node_id) {
            let mut remote = spec.clone();
            remote.node_id = None;
            let v = self
                .forward(
                    node,
                    proto::frame::Command::Create {
                        spec: json!(remote),
                    },
                    false,
                )
                .await?;
            let row: SessionRow =
                serde_json::from_value(v).map_err(|e| SessionError::Rejected(e.to_string()))?;
            let _ = self.store.ensure_peer_node(node);
            self.store.upsert_session_mirror(&row)?;
            self.bus
                .publish_untapped(Frame::Session(Box::new(row.clone())));
            return Ok(row);
        }
        let bindings = self.bindings(&spec.channel);
        // An archived channel keeps everything it has and takes nothing new.
        if !bindings["archived"].is_null() {
            return Err(SessionError::ChannelArchived(spec.channel.clone()));
        }
        let phase_bindings = &bindings["phases"][spec.phase.as_str()];
        let model_source = if spec.model.trim().is_empty() {
            let (model, source) =
                self.resolve_default_model(&spec.channel, spec.phase, &bindings)?;
            spec.model = model;
            source
        } else {
            "explicit"
        };
        let budget = spec
            .budget_tokens
            .or_else(|| phase_bindings["budget_tokens"].as_i64())
            .unwrap_or(self.cfg.session.budget_tokens);
        if budget < 0 {
            return Err(SessionError::BadBudget);
        }
        // Runaway spend is the failure mode that scales with how well the
        // system works: a channel at its daily ceiling starts nothing.
        let ceiling = crate::metrics::ceiling(&self.store, &bindings, &spec.channel);
        if ceiling.at() {
            return Err(SessionError::Ceiling(ceiling.reason()));
        }
        // A meshed node seals every frame under the channel's key, so a channel
        // it has no keyring for cannot carry a session. A standalone node keeps
        // channels as plain labels.
        if self.cfg.mesh.hub_url.is_some() && self.store.channel_get(&spec.channel)?.is_none() {
            return Err(SessionError::UnknownChannel(spec.channel.clone()));
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

        // Work items and phase artifacts are an opt-in workflow. Plain
        // workspace sessions use the execute harness mode without claiming an
        // item, plan, or review lifecycle.
        if let Some(item_id) = spec
            .work_item_id
            .as_deref()
            .filter(|i| !i.trim().is_empty())
        {
            let view = self
                .store
                .work_status(&spec.channel, None)?
                .into_iter()
                .find(|v| v.item.id == item_id)
                .ok_or_else(|| {
                    SessionError::NotReady(format!("no work item {item_id} on {}", spec.channel))
                })?;
            match &view.readiness {
                tracon_sync::work::Readiness::Ready => {}
                tracon_sync::work::Readiness::Closed => {
                    return Err(SessionError::NotReady("it is closed".into()))
                }
                tracon_sync::work::Readiness::Blocked { by } => {
                    let by: Vec<String> = by
                        .iter()
                        .map(|b| match b {
                            tracon_sync::work::Blocker::Open { id } => {
                                format!("open item {}", &id[..8.min(id.len())])
                            }
                            tracon_sync::work::Blocker::Unknown { id } => {
                                format!("item {} not seen here", &id[..8.min(id.len())])
                            }
                            tracon_sync::work::Blocker::Cycle => "a dependency cycle".into(),
                        })
                        .collect();
                    return Err(SessionError::NotReady(format!(
                        "blocked by {}",
                        by.join(", ")
                    )));
                }
            }
            if let Some(holder) = view.session_id {
                return Err(SessionError::InSession(holder));
            }
            let requires_plan = bindings["phases"]["execute"]["requires_plan"]
                .as_bool()
                .unwrap_or(true);
            if spec.phase == Phase::Execute && requires_plan {
                let planned = view
                    .item
                    .phase_plan_slug
                    .as_deref()
                    .and_then(|slug| self.store.doc_get(&spec.channel, slug).ok().flatten())
                    .is_some();
                if !planned {
                    return Err(SessionError::PlanRequired(item_id.to_string()));
                }
            }
        } else if spec.phase == Phase::Plan {
            return Err(SessionError::WorkItemRequired);
        }
        // Bank identity: the channel and the repository's remote, resolved on
        // this side of the boundary and recorded on the row for memory to key
        // on; never a checkout path.
        let (project_id, project_name, remote) = match spec.workspace_id.as_deref() {
            Some(workspace_id) => {
                let canonical = format!("workspace/{workspace_id}");
                (
                    crate::corpus::project::project_id(&spec.channel, &canonical),
                    "imported workspace".to_string(),
                    None,
                )
            }
            None => {
                crate::corpus::project::identify(
                    &spec.channel,
                    std::path::Path::new(&spec.repo_path),
                    &self.cfg.publish.git,
                )
                .await
            }
        };
        let _ = self.store.project_put(&crate::store::ProjectRow {
            id: project_id.clone(),
            channel: spec.channel.clone(),
            name: project_name,
            remote_url: remote,
            created_ms: now_ms(),
        });
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
            repo_path: spec
                .workspace_id
                .as_deref()
                .map(|workspace_id| format!("workspace://{workspace_id}"))
                .unwrap_or_else(|| spec.repo_path.clone()),
            worktree_path: None,
            branch: branch.clone(),
            harness_id: adapter.id().to_string(),
            harness_version: adapter.pinned_version().to_string(),
            harness_session_id: None,
            container_name: None,
            model: spec.model.clone(),
            project_id: Some(project_id),
            phase: spec.phase.as_str().into(),
            policy_version: Some(self.policy.read().unwrap().version as i64),
            review_id: spec.review_id.clone(),
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
            archived_ms: None,
        };
        self.store.insert_session(&row)?;
        // A plain session has no work item to retain its first instruction.
        // Keep it as a draft before asynchronous setup, and let on_prompt
        // clear it only after the supervisor accepts it.
        if let Some(prompt) = spec
            .initial_prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            self.store.set_draft(&id, Some(prompt))?;
        }
        let row = self.store.get_session(&id)?.ok_or(SessionError::NotFound)?;
        self.bus.publish(Frame::Session(Box::new(row.clone())));

        let this = self.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            if let Err(e) = this
                .start(&id, spec, branch, slug, adapter, started, model_source)
                .await
            {
                tracing::error!(session = %id, error = %e, "session failed to start");
                // A stop can race startup before a supervisor exists. It is
                // terminal by the time startup reports its cancellation and
                // must not be overwritten as a failed launch.
                let terminal = this
                    .store
                    .get_session(&id)
                    .ok()
                    .flatten()
                    .is_some_and(|row| SessionState::from_stored(&row.state).is_terminal());
                if terminal {
                    this.tokens.lock().await.remove(&id);
                    return;
                }
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
                // Recorded through the bus, not just the store: a client
                // watching (here or on a peer) should see why it never started.
                this.record(NewEvent {
                    session_id: id.clone(),
                    work_item_id: None,
                    kind: ek::ERROR.into(),
                    ref_id: None,
                    payload: json!({ "error": e.to_string() }),
                    at_ms: now_ms(),
                    mono_ms: started.elapsed().as_millis() as i64,
                });
                if let Ok(Some(row)) = this.store.get_session(&id) {
                    this.bus.publish(Frame::Session(Box::new(row)));
                }
            }
        });
        Ok(row)
    }

    /// Persist a lifecycle event and publish it on the stream, so a client
    /// watching live sees the same log a reload rebuilds.
    fn record(&self, e: NewEvent) {
        match self.store.append_event(&e) {
            Ok(seq) => self.bus.publish(Frame::Event {
                seq,
                node_id: self.node_id.clone(),
                session_id: e.session_id,
                kind: e.kind,
                ref_id: e.ref_id,
                payload: e.payload,
                at_ms: e.at_ms,
            }),
            Err(err) => tracing::error!(error = %err, "failed to persist event"),
        }
    }

    fn startable(&self, id: &str) -> Result<bool, crate::store::StoreError> {
        Ok(self
            .store
            .get_session(id)?
            .is_some_and(|row| row.state == SessionState::Starting.as_str()))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start(
        &self,
        id: &str,
        spec: NewSession,
        branch: String,
        slug: String,
        adapter: Arc<dyn HarnessAdapter>,
        started: Instant,
        model_source: &'static str,
    ) -> anyhow::Result<()> {
        if !self.startable(id)? {
            return Ok(());
        }
        // Registered before the harness starts: it connects to the node's MCP
        // server during `session/new`, so a token published afterwards is
        // published too late and the node refuses its own harness.
        let token = mint_token();
        self.tokens
            .lock()
            .await
            .insert(id.to_string(), (token.clone(), spec.channel.clone()));

        let repo = PathBuf::from(&spec.repo_path);
        let workspace = match spec.workspace_id.as_deref() {
            Some(existing) => crate::workspace::Workspace {
                id: existing.to_string(),
                volume: crate::workspace::volume_name(existing),
                snapshot: crate::workspace::snapshot_path(existing),
            },
            None => {
                if repo.starts_with(crate::forge::managed_root(&Config::state_dir())) {
                    let env = crate::forge::git_env_for(
                        &self.tools.broker,
                        &Config::state_dir(),
                        &spec.channel,
                        &repo,
                        &self.node_id,
                    );
                    crate::forge::fetch_managed(&repo, &env)
                        .await
                        .map_err(anyhow::Error::msg)?;
                }
                let workspace = crate::workspace::seed_from_checkout(
                    &self.cfg.publish.git,
                    &repo,
                    &branch,
                    spec.base_sha.as_deref(),
                    id,
                )
                .await?;
                crate::workspace::import(
                    self.backend.as_ref(),
                    &workspace,
                    &crate::workspace::staging_path(id),
                )
                .await?;
                workspace
            }
        };
        let snapshot = crate::workspace::export(self.backend.as_ref(), &workspace).await?;
        crate::workspace::sanitize_git(&snapshot)?;
        self.store.update_session(
            id,
            SessionPatch {
                worktree_path: Some(snapshot.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )?;
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::WORKTREE.into(),
            ref_id: None,
            payload: json!({
                "workspace_id": workspace.id,
                "volume": workspace.volume,
                "branch": branch,
                "source": if spec.workspace_id.is_some() { "resumed" } else { "imported" }
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        if !self.startable(id)? {
            self.tokens.lock().await.remove(id);
            return Ok(());
        }

        // The harness reaches its models through the node: every provider is
        // wired to the gateway with this session's token as the placeholder
        // key, so the only secret in the container names the session.
        let wiring =
            crate::gateway::model::harness_wiring(&self.cfg, &self.backend.harness_host(), &token);
        // What the session is told first: conventions from the corpus, this
        // node's facts, the channel's policy, and what is known. Recorded as
        // an event so the transcript shows what the agent was told.
        let (orientation, trimmed) = {
            let session_row = self.store.get_session(id)?;
            let project = session_row
                .as_ref()
                .and_then(|s| s.project_id.clone())
                .and_then(|pid| self.store.project_get(&pid).ok().flatten());
            let node = self.store.get_node(&self.node_id)?;
            let item = spec
                .work_item_id
                .as_deref()
                .and_then(|i| self.store.work_get(i).ok().flatten());
            let plan_body = item
                .as_ref()
                .and_then(|i| i.phase_plan_slug.as_deref())
                .and_then(|slug| self.store.doc_get(&spec.channel, slug).ok().flatten())
                .map(|d| d.body);
            let ready = self
                .store
                .work_ready(&spec.channel, project.as_ref().map(|p| p.id.as_str()))
                .unwrap_or_default();
            let review = spec
                .review_id
                .as_deref()
                .and_then(|r| self.store.get_review(r).ok().flatten());
            let tool_names: Vec<String> = self
                .tools
                .list_for(&spec.channel, &self.node_id, spec.phase)
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect();
            let policy = self.policy.read().unwrap();
            crate::corpus::orientation::assemble(
                &self.store,
                &policy,
                &crate::corpus::orientation::Facts {
                    node_name: node.as_ref().map(|n| n.name.as_str()).unwrap_or(""),
                    node_id: &self.node_id,
                    backend: self.backend.kind(),
                    harness: adapter.id(),
                    harness_version: adapter.pinned_version(),
                    channel: &spec.channel,
                    project_id: project.as_ref().map(|p| p.id.as_str()),
                    project_name: project.as_ref().map(|p| p.name.as_str()),
                    tools: &tool_names,
                    worktree: "/work",
                    phase: spec.phase.as_str(),
                    item: item.as_ref(),
                    plan_body: plan_body.as_deref(),
                    ready: &ready,
                    review: review.as_ref(),
                },
            )
        };
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::ORIENTATION.into(),
            ref_id: None,
            payload: json!({ "text": orientation, "trimmed": trimmed, "chars": orientation.len() }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        if !self.startable(id)? {
            self.tokens.lock().await.remove(id);
            return Ok(());
        }

        let scratch = materialize::scratch_for(
            id,
            &snapshot,
            &repo,
            &self.backend.harness_home(),
            adapter.as_ref(),
            &wiring,
            &orientation,
        )?;
        self.backend
            .import_volume(&scratch.volume, &scratch.dir)
            .await
            .map_err(anyhow::Error::msg)?;
        let container = format!("tracon-h-{slug}");
        let mut mounts = scratch.mounts;
        mounts.push(workspace.mount("/work", false));
        let runner: Arc<dyn Runner> = self.backend.runner(mounts);

        // Record the container name before it exists: it is deterministic, and a
        // launch that fails after the container is created would otherwise leave
        // a credential-mounted harness with no name in the store for
        // `reconcile_after_restart` to remove.
        self.store.update_session(
            id,
            SessionPatch {
                container_name: Some(container.clone()),
                ..Default::default()
            },
        )?;

        // The harness reaches the node only through the gateway's forward, and
        // only with this session's token. Tools are offered only if the
        // channel has a credential bound to it; otherwise the harness is given
        // no MCP server at all rather than one that refuses everything.
        let mcp_servers = if self
            .tools
            .list_for(&spec.channel, &self.node_id, spec.phase)
            .is_empty()
        {
            Vec::new()
        } else {
            vec![json!({
                "type": "http",
                "name": "tracon",
                "url": format!(
                    "http://{}:{}/mcp/{id}",
                    self.backend.harness_host(),
                    self.cfg.gateway.forward_port
                ),
                "headers": [{ "name": "Authorization", "value": format!("Bearer {token}") }],
            })]
        };

        let mut harness_env = wiring.env.clone();
        harness_env.extend([
            (
                "GIT_CONFIG_GLOBAL".into(),
                format!("{}/.gitconfig", self.backend.harness_home()),
            ),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ]);

        let launched = tokio::time::timeout(
            STARTUP_TIMEOUT,
            adapter.launch(
                runner.as_ref(),
                LaunchSpec {
                    cwd_in_runner: "/work".into(),
                    model: spec.model.clone(),
                    container_name: container.clone(),
                    mcp_servers,
                    tools: self.cfg.harness.tools.clone(),
                    env: harness_env,
                    system_prompt_file: Some(scratch.orientation_path.clone()),
                },
            ),
        )
        .await;
        let (handle, events) = match launched {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                return Err(e.into());
            }
            Err(_) => {
                let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
                return Err(anyhow::anyhow!(
                    "harness startup timed out after {} seconds",
                    STARTUP_TIMEOUT.as_secs()
                ));
            }
        };

        if !self.startable(id)? {
            let _ = tokio::time::timeout(CLEANUP_TIMEOUT, runner.kill(&container)).await;
            self.tokens.lock().await.remove(id);
            return Ok(());
        }
        self.store.update_session(
            id,
            SessionPatch {
                harness_session_id: Some(handle.harness_session_id().to_string()),
                ..Default::default()
            },
        )?;
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::SESSION_STARTED.into(),
            ref_id: None,
            payload: json!({
                "model": spec.model, "model_source": model_source,
                "harness": adapter.id(), "phase": spec.phase.as_str(),
                "work_item_id": spec.work_item_id,
                "policy_version": self.policy.read().unwrap().version,
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let cmd_tx_for_turns = cmd_tx.clone();
        if let Some(text) = spec.initial_prompt.filter(|text| !text.trim().is_empty()) {
            let (ack, _wait) = oneshot::channel();
            cmd_tx_for_turns
                .send(Command::Prompt { text, ack })
                .await
                .map_err(|_| anyhow::anyhow!("session supervisor did not start"))?;
        }
        self.live.lock().await.insert(id.to_string(), cmd_tx);

        let sup = Supervisor::new(
            id.to_string(),
            self.node_id.clone(),
            self.store.clone(),
            self.bus.clone(),
            Arc::from(handle),
            started,
            Duration::from_secs(self.cfg.session.permission_timeout_secs),
            cmd_tx_for_turns,
            runner,
            container.clone(),
            self.policy.clone(),
            spec.channel.clone(),
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

    /// The session an external harness on `channel` acts as: the live one, or
    /// a new row and the loop that answers for it.
    ///
    /// No tool token is minted. The door for these is the operator API, which
    /// the operator's own guard already answers; a token here would be a
    /// second capability for the same trust, and `session_for_token` must
    /// never resolve one of these for the model gateway.
    pub async fn attach_external(&self, channel: &str) -> Result<String, SessionError> {
        self.channel_usable(channel)?;
        let bindings = self.bindings(channel);
        if bindings["external_stopped"] == true {
            return Err(SessionError::Rejected(
                "external broker access was stopped by the operator; explicitly clear channel binding external_stopped before reattaching".into(),
            ));
        }
        let mut attached = self.external.lock().await;
        if let Some(a) = attached.get(channel) {
            // The in-memory `live` map's cleanup lags a Kill/Stop's
            // synchronous store fence by a scheduler tick or two; trusting
            // it here would hand a caller an attachment already published
            // as closed. The store row is what that fence actually commits.
            let reusable = self
                .store
                .get_session(&a.session_id)?
                .is_some_and(|row| !SessionState::from_stored(&row.state).is_terminal());
            if reusable {
                if let Ok(mut t) = a.last_seen.lock() {
                    *t = Instant::now();
                }
                return Ok(a.session_id.clone());
            }
        }
        // The live attachment is gone (a node restart drops it; the pause
        // fence does not), but the operator paused this channel and never
        // resumed it. Creating a new running attachment here would resume
        // broker work without the operator's say-so, which is the one thing
        // a pause promises will not happen.
        if bindings["external_paused"] == true {
            return Err(SessionError::Rejected(
                "external broker access is paused; resume the session before it accepts more work"
                    .into(),
            ));
        }

        let id = uuid::Uuid::now_v7().to_string();
        let row = SessionRow {
            id: id.clone(),
            node_id: self.node_id.clone(),
            channel: channel.to_string(),
            work_item_id: None,
            repo_path: String::new(),
            worktree_path: None,
            branch: String::new(),
            harness_id: external::HARNESS_ID.into(),
            harness_version: String::new(),
            harness_session_id: None,
            container_name: None,
            model: String::new(),
            project_id: None,
            phase: Phase::Execute.as_str().into(),
            policy_version: Some(self.policy.read().unwrap().version as i64),
            review_id: None,
            // Recorded, never enforced: this node brokers no model call for an
            // external harness, so there is nothing here to count.
            budget_tokens: self.cfg.session.budget_tokens,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: SessionState::Running.as_str().into(),
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: now_ms(),
            started_mono_ms: Some(0),
            ended_mono_ms: None,
            updated_ms: now_ms(),
            archived_ms: None,
        };
        self.store.insert_session(&row)?;
        self.bus.publish(Frame::Session(Box::new(row.clone())));
        self.record(NewEvent {
            session_id: id.clone(),
            work_item_id: None,
            kind: ek::SESSION_STARTED.into(),
            ref_id: None,
            payload: json!({
                "harness": external::HARNESS_ID,
                "channel": channel,
                "policy_version": self.policy.read().unwrap().version,
            }),
            at_ms: now_ms(),
            mono_ms: 0,
        });

        let last_seen = Arc::new(std::sync::Mutex::new(Instant::now()));
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        self.live.lock().await.insert(id.clone(), cmd_tx);
        attached.insert(
            channel.to_string(),
            external::Attachment {
                session_id: id.clone(),
                last_seen: last_seen.clone(),
            },
        );
        drop(attached);

        let looper = external::Loop {
            session_id: id.clone(),
            node_id: self.node_id.clone(),
            store: self.store.clone(),
            bus: self.bus.clone(),
            permission_timeout: Duration::from_secs(self.cfg.session.permission_timeout_secs),
            idle_timeout: Duration::from_secs(self.cfg.external.idle_timeout_secs.max(60)),
            last_seen,
        };
        let live = self.live.clone();
        let external = self.external.clone();
        let sid = id.clone();
        let ch = channel.to_string();
        tokio::spawn(async move {
            looper.run(cmd_rx).await;
            live.lock().await.remove(&sid);
            let mut attached = external.lock().await;
            // Only if it is still this attachment: a reattach may have
            // replaced it while this one was closing.
            if attached.get(&ch).is_some_and(|a| a.session_id == sid) {
                attached.remove(&ch);
            }
        });
        Ok(id)
    }

    /// Live external attachments as `(channel, session_id)`, for the CLI and
    /// the interface.
    pub async fn external_attachments(&self) -> Vec<(String, String)> {
        let attached = self.external.lock().await;
        let live = self.live.lock().await;
        attached
            .iter()
            .filter(|(_, a)| live.contains_key(&a.session_id))
            .map(|(c, a)| (c.clone(), a.session_id.clone()))
            .collect()
    }

    /// End the attachment on a channel, if there is one.
    pub async fn detach_external(&self, channel: &str) -> Result<(), SessionError> {
        let id = self
            .external
            .lock()
            .await
            .get(channel)
            .map(|a| a.session_id.clone())
            .ok_or(SessionError::NotFound)?;
        self.send(&id, Command::Kill).await
    }

    /// A channel a session may run on: one this node holds, or one of the two
    /// labels a standalone node offers before any channel is created.
    fn channel_usable(&self, channel: &str) -> Result<(), SessionError> {
        let known = match self.store.channel_list() {
            Ok(rows) if rows.iter().any(|c| !c.name.starts_with('@')) => rows
                .iter()
                .any(|c| c.name == channel && !c.name.starts_with('@')),
            // No channels yet: the interface offers these two, so they work.
            Ok(_) => crate::http::api::DEFAULT_CHANNELS.contains(&channel),
            Err(e) => return Err(e.into()),
        };
        if !known {
            return Err(SessionError::UnknownChannel(channel.to_string()));
        }
        if !self.bindings(channel)["archived"].is_null() {
            return Err(SessionError::ChannelArchived(channel.to_string()));
        }
        Ok(())
    }

    async fn send(&self, id: &str, cmd: Command) -> Result<(), SessionError> {
        let tx = self.live.lock().await.get(id).cloned();
        match tx {
            Some(tx) => tokio::time::timeout(COMMAND_SEND_TIMEOUT, tx.send(cmd))
                .await
                .map_err(|_| {
                    SessionError::Rejected(
                        "session supervisor did not accept the command in time".into(),
                    )
                })?
                .map_err(|_| SessionError::Rejected("session is no longer running".into())),
            // Not live here. Someone else's, or one of ours that has ended.
            None => match self.store.get_session(id)? {
                Some(row) if row.node_id != self.node_id => {
                    Err(SessionError::Remote(row.node_id, row.channel))
                }
                _ => Err(SessionError::Rejected(
                    "session is not running on this node".into(),
                )),
            },
        }
    }

    /// Put a request to the operator on a live session's queue and wait for
    /// the answer. Used by brokered tool calls the policy does not decide.
    pub async fn ask_permission(
        &self,
        id: &str,
        request: crate::adapter::PermissionRequest,
    ) -> Result<crate::adapter::PermissionReply, SessionError> {
        let (reply, wait) = oneshot::channel();
        self.send(id, Command::Permission { request, reply })
            .await?;
        wait.await
            .map_err(|_| SessionError::Rejected("session ended before answering".into()))
    }

    pub async fn prompt(&self, id: &str, text: String) -> Result<(), SessionError> {
        self.ensure_active(id)?;
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                id,
                Command::Prompt {
                    text: text.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Prompt {
                        session_id: id.to_string(),
                        text,
                    },
                    true,
                )
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    pub async fn answer(
        &self,
        id: &str,
        option_id: String,
        arguments: Option<serde_json::Value>,
    ) -> Result<(), SessionError> {
        let perm = self
            .store
            .get_permission(id)?
            .ok_or(SessionError::NotFound)?;
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                &perm.session_id,
                Command::Answer {
                    permission_id: id.to_string(),
                    option_id: option_id.clone(),
                    arguments: arguments.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Answer {
                        permission_id: id.to_string(),
                        option_id,
                        arguments,
                    },
                    false,
                )
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// The item a session holds was closed: record it and end the session
    /// once its turn is over. A session not live here (ended, or another
    /// node's) is left alone; the close itself has already replicated.
    pub async fn item_closed(&self, id: &str, summary: &str) {
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::WORK_CLOSED.into(),
            ref_id: None,
            payload: json!({ "summary": summary }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        let _ = self
            .send(id, Command::EndAfterTurn(EndReason::ItemClose))
            .await;
    }

    /// Mark a session as waiting on deterministic checks (or back to running)
    /// and tell the interface. Called from the submit tool, inside a turn.
    pub fn set_checking(&self, id: &str, checking: bool) {
        let Ok(Some(current)) = self.store.get_session(id) else {
            return;
        };
        let current_state = SessionState::from_stored(&current.state);
        if current_state == SessionState::Paused || current_state.is_terminal() {
            return;
        }
        let state = if checking {
            SessionState::WaitingOnCheck
        } else {
            SessionState::Running
        };
        let _ = self
            .store
            .update_session(id, SessionPatch::state(state.as_str()));
        if let Ok(Some(row)) = self.store.get_session(id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
    }

    /// Record an event on a session from outside the supervisor.
    pub fn record_event(&self, session_id: &str, kind: &str, payload: serde_json::Value) {
        self.record(NewEvent {
            session_id: session_id.to_string(),
            work_item_id: None,
            kind: kind.to_string(),
            ref_id: None,
            payload,
            at_ms: now_ms(),
            mono_ms: 0,
        });
    }

    /// The phase's artifact landed: end the session once its turn is over.
    pub async fn phase_done(&self, id: &str) {
        let _ = self
            .send(id, Command::EndAfterTurn(EndReason::PhaseDone))
            .await;
    }

    /// Resolve only configured, channel-authorized defaults. The node's model
    /// order is the adapter's order, so this stays deterministic without
    /// guessing at credentials that are not bound to this channel.
    fn resolve_default_model(
        &self,
        channel: &str,
        phase: Phase,
        bindings: &serde_json::Value,
    ) -> Result<(String, &'static str), SessionError> {
        for (value, source) in [
            (
                bindings["phases"][phase.as_str()]["model"].as_str(),
                "channel_phase",
            ),
            (bindings["model"].as_str(), "channel"),
        ] {
            if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
                if self.model_usable(channel, value, bindings) {
                    return Ok((value.to_string(), source));
                }
                return Err(SessionError::ModelRequired);
            }
        }
        let models = self
            .store
            .get_node(&self.node_id)?
            .and_then(|node| node.models_json)
            .and_then(|json| serde_json::from_str::<Vec<crate::adapter::ModelOption>>(&json).ok())
            .unwrap_or_default();
        models
            .into_iter()
            .map(|model| model.value)
            .find(|model| self.model_usable(channel, model, bindings))
            .map(|model| (model, "node_catalogue"))
            .ok_or(SessionError::ModelRequired)
    }

    /// Harnesses use `provider/model` names where they can. A provider-less
    /// alias is adapter-owned and cannot be safely reverse-engineered here;
    /// it was already present in the node's successful catalogue.
    fn model_usable(&self, channel: &str, model: &str, bindings: &serde_json::Value) -> bool {
        let Some((provider_name, _)) = model.split_once('/') else {
            return true;
        };
        let Some(provider) = self.cfg.providers.get(provider_name) else {
            return true;
        };
        if let Some(allowed) = bindings["providers"].as_array() {
            if !allowed
                .iter()
                .any(|name| name.as_str() == Some(provider_name))
            {
                return false;
            }
        }
        let Ok(broker) = self.tools.broker.read() else {
            return false;
        };
        broker
            .inject_for(
                &provider.credential,
                channel,
                &self.node_id,
                &provider.shape,
            )
            .is_ok()
    }

    /// The channel-archived and model-availability checks `create` runs,
    /// without any of its side effects. A caller that must materialize
    /// something expensive before `create` (a continuity transfer's
    /// workspace) checks this first, so a request `create` will reject
    /// outright never leaks that work.
    pub fn preflight(&self, spec: &NewSession) -> Result<(), SessionError> {
        let bindings = self.bindings(&spec.channel);
        if !bindings["archived"].is_null() {
            return Err(SessionError::ChannelArchived(spec.channel.clone()));
        }
        if spec.model.trim().is_empty() {
            self.resolve_default_model(&spec.channel, spec.phase, &bindings)?;
        } else if !self.model_usable(&spec.channel, &spec.model, &bindings) {
            return Err(SessionError::ModelRequired);
        }
        Ok(())
    }

    /// A channel's bindings as JSON (`{}` when unbound or standalone).
    pub fn bindings(&self, channel: &str) -> serde_json::Value {
        self.store
            .channel_get(channel)
            .ok()
            .flatten()
            .and_then(|c| serde_json::from_str(&c.bindings_json).ok())
            .unwrap_or_else(|| json!({}))
    }

    /// Fence a live session without discarding its workspace or evidence.
    pub async fn pause(&self, id: &str, reason: String) -> Result<(), SessionError> {
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                id,
                Command::Pause {
                    source: supervisor::PauseSource::Operator,
                    reason: reason.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Pause {
                        session_id: id.to_string(),
                        reason,
                    },
                    false,
                )
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// Reopen a session that remains supervised on its owning node, or an
    /// external attachment's paused fence, which a restart cannot restore a
    /// process for but can still honestly release.
    pub async fn resume(&self, id: &str, reason: String) -> Result<(), SessionError> {
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                id,
                Command::Resume {
                    source: supervisor::PauseSource::Operator,
                    reason: reason.clone(),
                    ack,
                },
            )
            .await
        {
            Ok(()) => wait
                .await
                .map_err(|_| SessionError::Rejected("session stopped".into()))?
                .map_err(SessionError::Rejected),
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Resume {
                        session_id: id.to_string(),
                        reason,
                    },
                    false,
                )
                .await
                .map(|_| ()),
            Err(SessionError::Rejected(_)) => self.resume_stale_external(id, &reason).await,
            Err(e) => Err(e),
        }
    }

    /// A restart drops the in-memory attachment an external harness had, but
    /// the pause fence it left on the channel is durable and outlives it.
    /// There is no live loop to hand the row back to, so this clears the
    /// fence and closes the stale row honestly rather than pretending the
    /// old attachment is running again; the next call re-attaches fresh.
    /// A managed session has no such shortcut — its process is really gone —
    /// so this refuses it exactly as `send` already would have.
    async fn resume_stale_external(&self, id: &str, reason: &str) -> Result<(), SessionError> {
        let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if row.node_id != self.node_id
            || row.state != SessionState::Paused.as_str()
            || row.harness_id != external::HARNESS_ID
        {
            return Err(SessionError::Rejected(
                "session is not running on this node".into(),
            ));
        }
        if let Some(channel) = self.store.channel_get(&row.channel)? {
            let mut bindings: serde_json::Value =
                serde_json::from_str(&channel.bindings_json).unwrap_or_else(|_| json!({}));
            bindings["external_paused"] = json!(false);
            self.store.channel_put(
                &row.channel,
                &channel.keyring,
                &serde_json::to_string(&bindings)
                    .map_err(|error| SessionError::Rejected(error.to_string()))?,
            )?;
        }
        self.store.update_session(
            id,
            SessionPatch {
                state: Some(SessionState::Closed.as_str().into()),
                end_reason: Some(EndReason::Detached.as_str().into()),
                last_error: Some(
                    "operator resumed broker access after a restart; the prior attachment did not survive it, reattach to continue".into(),
                ),
                ended_mono_ms: Some(0),
                turn_active: Some(false),
                ..Default::default()
            },
        )?;
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::STATE.into(),
            ref_id: None,
            payload: json!({ "state": "closed", "end_reason": "detached", "reason": reason }),
            at_ms: now_ms(),
            mono_ms: 0,
        });
        if let Ok(Some(row)) = self.store.get_session(id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
        Ok(())
    }

    /// Stop a live session and, for an external harness, durably fence the
    /// channel: broker access stays refused until the operator explicitly
    /// clears it. The operator's actual "Stop" action.
    pub async fn stop(&self, id: &str) -> Result<(), SessionError> {
        self.terminate(id, true).await
    }

    /// End one session without touching a channel-wide fence. An external
    /// harness's next call reattaches fresh; a managed session's row closes
    /// exactly as `stop` leaves it. Used where ending this attachment, not
    /// refusing the channel, is the whole request.
    pub async fn kill(&self, id: &str) -> Result<(), SessionError> {
        self.terminate(id, false).await
    }

    /// A startup row has no supervisor to command yet, so it is made
    /// terminal directly and startup observes that fence.
    async fn terminate(&self, id: &str, fence_external: bool) -> Result<(), SessionError> {
        if fence_external {
            if let Some(row) = self.store.get_session(id)? {
                if row.harness_id == external::HARNESS_ID {
                    let channel = self.store.channel_get(&row.channel)?;
                    if channel.is_none() {
                        // Materialize both standalone defaults together; creating
                        // one otherwise makes the other disappear from the
                        // synthesized channel list.
                        for name in crate::http::api::DEFAULT_CHANNELS {
                            self.store.channel_put(name, &[], "{}")?;
                            self.store.node_channel_add(&self.node_id, name)?;
                        }
                    }
                    let channel = self.store.channel_get(&row.channel)?;
                    let (keyring, mut bindings) = match channel {
                        Some(channel) => (
                            channel.keyring,
                            serde_json::from_str(&channel.bindings_json)
                                .unwrap_or_else(|_| json!({})),
                        ),
                        None => (Vec::new(), json!({})),
                    };
                    // Stop supersedes any earlier pause: one fence gates
                    // reattachment from here on, and it is this one.
                    bindings["external_stopped"] = json!(true);
                    bindings["external_paused"] = json!(false);
                    self.store.channel_put(
                        &row.channel,
                        &keyring,
                        &serde_json::to_string(&bindings)
                            .map_err(|error| SessionError::Rejected(error.to_string()))?,
                    )?;
                }
            }
        }
        // Fence dispatch synchronously before the asynchronous supervisor
        // teardown. A 200 must never leave broker access open.
        // Tracked so the stale-row fallback below can report the teardown
        // this call already performed instead of re-deriving eligibility
        // from a row this same call just made terminal.
        let mut already_closed = false;
        if let Some(row) = self.store.get_session(id)? {
            if row.node_id == self.node_id
                && row.state != SessionState::Starting.as_str()
                && !SessionState::from_stored(&row.state).is_terminal()
            {
                self.store.update_session(
                    id,
                    SessionPatch {
                        state: Some(SessionState::Closed.as_str().into()),
                        end_reason: Some(EndReason::KilledUser.as_str().into()),
                        ended_mono_ms: Some(0),
                        turn_active: Some(false),
                        ..Default::default()
                    },
                )?;
                already_closed = true;
                if let Ok(Some(row)) = self.store.get_session(id) {
                    self.bus.publish(Frame::Session(Box::new(row)));
                }
            }
        }
        match self.send(id, Command::Kill).await {
            Err(SessionError::Remote(node, _)) => self
                .forward(
                    &node,
                    proto::frame::Command::Kill {
                        session_id: id.to_string(),
                    },
                    false,
                )
                .await
                .map(|_| ()),
            // No live supervisor, but this call already made the row
            // terminal and published it above: the teardown happened, so
            // report success rather than rejecting a row this call itself
            // just closed.
            Err(SessionError::Rejected(_)) if already_closed => Ok(()),
            Err(SessionError::Rejected(_)) => {
                let row = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
                if row.node_id != self.node_id
                    || (row.state != SessionState::Starting.as_str()
                        && row.state != SessionState::Paused.as_str())
                {
                    return Err(SessionError::Rejected(
                        "session is not running on this node".into(),
                    ));
                }
                self.store.update_session(
                    id,
                    SessionPatch {
                        state: Some(SessionState::Closed.as_str().into()),
                        end_reason: Some(EndReason::KilledUser.as_str().into()),
                        ended_mono_ms: Some(0),
                        turn_active: Some(false),
                        ..Default::default()
                    },
                )?;
                if let Some(container) = row.container_name {
                    let _ = tokio::time::timeout(
                        CLEANUP_TIMEOUT,
                        self.backend.runner(Vec::new()).kill(&container),
                    )
                    .await;
                }
                self.record(NewEvent {
                    session_id: id.to_string(),
                    work_item_id: None,
                    kind: ek::STATE.into(),
                    ref_id: None,
                    payload: json!({ "state": "closed", "end_reason": "killed_user" }),
                    at_ms: now_ms(),
                    mono_ms: 0,
                });
                if let Ok(Some(row)) = self.store.get_session(id) {
                    self.bus.publish(Frame::Session(Box::new(row)));
                }
                Ok(())
            }
            other => other,
        }
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
///
/// Only this node's sessions: a peer's mirrored rows are its to close.
pub async fn reconcile_after_restart(
    store: &Store,
    self_node_id: &str,
    backend: &dyn crate::boundary::Backend,
) -> Vec<String> {
    let mut cleaned = Vec::new();
    let Ok(sessions) = store.list_sessions(None) else {
        return cleaned;
    };
    for s in sessions {
        if s.node_id != self_node_id || state::SessionState::from_stored(&s.state).is_terminal() {
            continue;
        }
        for p in store.open_permissions().unwrap_or_default() {
            if p.session_id == s.id {
                // Monotonic clocks do not survive a restart, so no meaningful
                // duration exists here. Resolve at the created reading (duration
                // 0 = not measured) rather than 0, which would go negative
                // against a nonzero created_mono_ms.
                let _ = store.resolve_permission(&p.id, "expired", None, p.created_mono_ms);
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
        if s.state == state::SessionState::Paused.as_str() && s.harness_id == external::HARNESS_ID {
            // External broker access is fenced on the channel, not owned by
            // a process this node supervises: the pause already made that
            // fence durable, a restart does not touch it, and only an
            // explicit operator Resume (`Manager::resume_stale_external`)
            // clears it. Resurrecting the row here would not help; closing
            // it would silently drop the fence for the channel's next call.
            continue;
        }
        if let Some(container) = &s.container_name {
            backend.reconcile(std::slice::from_ref(container)).await;
        }
        let _ = store.update_session(
            &s.id,
            SessionPatch {
                state: Some(state::SessionState::Closed.as_str().into()),
                end_reason: Some(state::EndReason::HarnessExit.as_str().into()),
                last_error: Some("node restarted while the session was active".into()),
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
