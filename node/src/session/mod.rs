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
    runner::Runner,
    session::{
        state::{event_kind as ek, EndReason, SessionState},
        supervisor::{Command, Supervisor},
    },
    store::{now_ms, NewEvent, SessionPatch, SessionRow, Store},
    stream::{Bus, Frame},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewSession {
    pub channel: String,
    pub repo_path: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub work_item_id: Option<String>,
    /// Empty means "whatever this channel binds to the phase"
    /// (`phases.<phase>.model`). A session with neither is a validation
    /// failure rather than a silent choice.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub budget_tokens: Option<i64>,
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
    #[error("no model: name one, or bind phases.<phase>.model on the channel")]
    ModelRequired,
    #[error("budget must be greater than zero")]
    BadBudget,
    #[error("node refuses to run harnesses: {0}")]
    NodeRefused(String),
    #[error("harness version mismatch: node expects {pinned}, host has {found}")]
    VersionMismatch { found: String, pinned: String },
    #[error("session not found")]
    NotFound,
    #[error("channel {0} is not one this node holds keys for; create or enroll it first")]
    UnknownChannel(String),
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
            probe_token: mint_token(),
            mesh: Arc::new(std::sync::OnceLock::new()),
            providers: Arc::new(std::sync::OnceLock::new()),
            adapter: Arc::new(std::sync::OnceLock::new()),
        }
    }

    pub fn backend(&self) -> &Arc<dyn crate::boundary::Backend> {
        &self.backend
    }

    pub fn cfg(&self) -> &Arc<Config> {
        &self.cfg
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

    /// The session a gateway request belongs to, from the placeholder key it
    /// carries: `(session_id, channel)`.
    pub async fn session_for_token(&self, presented: &str) -> Option<(String, String)> {
        let tokens = self.tokens.lock().await;
        tokens
            .iter()
            .find(|(_, (expected, _))| constant_time_eq(expected.as_bytes(), presented.as_bytes()))
            .map(|(id, (_, channel))| (id.clone(), channel.clone()))
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
        let phase_bindings = &bindings["phases"][spec.phase.as_str()];
        // A channel binds a model to each phase — plan reads and thinks,
        // execute builds — so the operator names one once rather than at every
        // start. Resolved here rather than before the forward above, so a
        // session bound for another node is answered by that node's own
        // bindings, not by this one's possibly stale copy.
        if spec.model.trim().is_empty() {
            match phase_bindings["model"].as_str() {
                Some(m) if !m.trim().is_empty() => spec.model = m.to_string(),
                _ => return Err(SessionError::ModelRequired),
            }
        }
        let budget = spec
            .budget_tokens
            .or_else(|| phase_bindings["budget_tokens"].as_i64())
            .unwrap_or(self.cfg.session.budget_tokens);
        if budget <= 0 {
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

        // The ledger decides: a plan or execute session needs a ready item
        // nobody holds, and execute needs the plan a plan session wrote.
        if spec.phase != Phase::Review {
            let item_id = spec
                .work_item_id
                .as_deref()
                .filter(|i| !i.trim().is_empty())
                .ok_or(SessionError::WorkItemRequired)?;
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
        }
        // Bank identity: the channel and the repository's remote, resolved on
        // this side of the boundary and recorded on the row for memory to key
        // on; never a checkout path.
        let (project_id, project_name, remote) = crate::corpus::project::identify(
            &spec.channel,
            std::path::Path::new(&spec.repo_path),
            &self.cfg.publish.git,
        )
        .await;
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
            repo_path: spec.repo_path.clone(),
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
        };
        self.store.insert_session(&row)?;
        self.bus.publish(Frame::Session(Box::new(row.clone())));

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
        let wt = match spec.base_sha.as_deref() {
            Some(sha) => {
                worktree::create_at(&repo, &self.cfg.session.worktree_root, &branch, &slug, sha)
                    .await?
            }
            None => {
                // A managed clone (under the node's repos root) fetches with
                // the forge credential its channel is bound to; anywhere else
                // uses the operator's own git auth, as before.
                let env = crate::forge::git_env_for(
                    &self.tools.broker,
                    &crate::config::Config::state_dir(),
                    &spec.channel,
                    &repo,
                    &self.node_id,
                );
                worktree::create(&repo, &self.cfg.session.worktree_root, &branch, &slug, &env)
                    .await?
            }
        };
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

        let scratch = materialize::scratch_for(
            id,
            &wt.path,
            &repo,
            &self.backend.harness_home(),
            adapter.as_ref(),
            &wiring,
            &orientation,
        )?;
        let container = format!("tracon-h-{slug}");
        let runner: Arc<dyn Runner> = self.backend.runner(scratch.mounts);

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

        let launched = adapter
            .launch(
                runner.as_ref(),
                LaunchSpec {
                    cwd_in_runner: "/work".into(),
                    model: spec.model.clone(),
                    container_name: container.clone(),
                    mcp_servers,
                    tools: self.cfg.harness.tools.clone(),
                    env: wiring.env.clone(),
                    system_prompt_file: Some(scratch.orientation_path.clone()),
                },
            )
            .await;
        let (handle, events) = match launched {
            Ok(v) => v,
            Err(e) => {
                // The container may have been created before launch failed (a
                // version mismatch or unknown model is reported after start).
                // Remove it so no credential-mounted harness is left running.
                let _ = runner.kill(&container).await;
                return Err(e.into());
            }
        };

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
                "model": spec.model, "harness": adapter.id(), "phase": spec.phase.as_str(),
                "work_item_id": spec.work_item_id,
                "policy_version": self.policy.read().unwrap().version,
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let cmd_tx_for_turns = cmd_tx.clone();
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

    async fn send(&self, id: &str, cmd: Command) -> Result<(), SessionError> {
        let tx = self.live.lock().await.get(id).cloned();
        match tx {
            Some(tx) => tx
                .send(cmd)
                .await
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

    pub async fn answer(&self, id: &str, option_id: String) -> Result<(), SessionError> {
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

    /// A channel's bindings as JSON (`{}` when unbound or standalone).
    pub fn bindings(&self, channel: &str) -> serde_json::Value {
        self.store
            .channel_get(channel)
            .ok()
            .flatten()
            .and_then(|c| serde_json::from_str(&c.bindings_json).ok())
            .unwrap_or_else(|| json!({}))
    }

    pub async fn kill(&self, id: &str) -> Result<(), SessionError> {
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
        if let Some(container) = &s.container_name {
            backend.reconcile(std::slice::from_ref(container)).await;
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
