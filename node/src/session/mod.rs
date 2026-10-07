//! Session lifecycle: create a worktree, materialize config, spawn the harness
//! inside the boundary, and hand it to a supervisor.

pub mod chunks;
pub mod external;
pub mod ingest;
pub mod materialize;
pub mod opencode_state;
pub mod state;
pub mod supervisor;

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
    /// `running`. Structured work-item and review sessions leave this absent
    /// and are sent the node's kickoff instead.
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
    /// Lineage, when this session is made from another one. Written onto the
    /// row at creation and never afterwards. Both are validated against this
    /// node's own sessions: a spec naming a session that is not here is
    /// refused rather than recording a dangling ancestor.
    #[serde(default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub continued_from: Option<String>,
    /// The harness to run this session on. Absent: the node's configured
    /// `[harness] id`. A session runs the harness it names for its whole life;
    /// the node holds an image for every supported one.
    #[serde(default)]
    pub harness: Option<String>,
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

/// The phase a reopened session starts in. A plan session reopens as one;
/// anything else reopens as ordinary work on the same workspace. A review
/// session is deliberately not reopened as a review: reviews are spawned by
/// the node against a specific review id, and a second one for a decision
/// that was already made is not what the operator asked for.
fn reopen_phase(stored: &str) -> Phase {
    match stored {
        "plan" => Phase::Plan,
        _ => Phase::Execute,
    }
}

/// What the new session is told, as its first prompt.
///
/// The new harness inherits nothing: not the old one's context window, not
/// its plan, not what it had already tried. Saying so explicitly is the whole
/// point — an agent handed a workspace with no account of how it got that way
/// re-derives it, badly, and calls the result progress.
fn handoff_note(old: &SessionRow, harness: &str) -> String {
    let why = format!(
        "This session continues session {} (`{}` {}), which ran a harness this node no \
         longer has and which is now archived read-only. You are {harness}, and you \
         inherit none of its context",
        old.id, old.harness_id, old.harness_version,
    );
    handoff(old, &why)
}

/// What a session carrying on after a node restart is told. Same harness,
/// same workspace, but none of the interrupted conversation: the harness
/// process that held it is gone with the node.
fn restart_note(old: &SessionRow) -> String {
    let why = format!(
        "This session continues session {}, which was cut off when the node it ran on \
         stopped or restarted in the middle of its work. You inherit none of its context",
        old.id,
    );
    handoff(old, &why)
}

fn handoff(old: &SessionRow, why: &str) -> String {
    let mut note = format!(
        "{why}: not its conversation, not its plan, not what it had already tried.\n\n\
         What you do have is its workspace, exactly as it left it, on branch `{}`.\n\n\
         Start by reading the workspace — the diff against the branch point, and any \
         notes left in it — and say what you find before changing anything. If the \
         earlier session's transcript matters, it is readable in the interface under \
         that id; ask for what you need from it rather than guessing.",
        old.branch,
    );
    if let Some(item) = old.work_item_id.as_deref() {
        note.push_str(&format!(
            "\n\nIt was working on item {item}, which is still open."
        ));
    }
    note
}

/// The first prompt of a session the node starts on structured work: what to
/// do, in a sentence, with the details left to the orientation the harness was
/// already given. `None` for a plain session, which has nothing to start on
/// until the operator says what.
fn kickoff(
    phase: Phase,
    item: Option<&tracon_sync::work::WorkItem>,
    review_id: Option<&str>,
) -> Option<String> {
    let named = |item: &tracon_sync::work::WorkItem| {
        format!(
            "**{}** (`{}…`)",
            item.title,
            &item.id[..8.min(item.id.len())]
        )
    };
    match phase {
        Phase::Plan => {
            let item = item?;
            Some(format!(
                "Plan work item {}. It is described in your orientation. Read what you need, \
                 then write the plan as document `{}` with `doc_write`.",
                named(item),
                crate::corpus::work::plan_slug(&item.id),
            ))
        }
        Phase::Execute => {
            let item = item?;
            let plan = match item.phase_plan_slug.as_deref() {
                Some(slug) => format!(
                    "Follow its plan, document `{slug}`: your orientation carries it, and \
                     `doc_read` returns it whole."
                ),
                None => "It is described in your orientation.".to_string(),
            };
            Some(format!(
                "Carry out work item {}. {plan} Commit the work in the worktree and call \
                 `submit_review` when it is ready.",
                named(item),
            ))
        }
        Phase::Review => {
            let review = review_id?;
            Some(format!(
                "Review the change under review `{review}`. The requirements and the diff are \
                 in your orientation and the change is checked out in the worktree. Give your \
                 verdict with `review_verdict`."
            ))
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
    /// An operator's answer the node cannot act on as sent, field by field.
    /// Nothing was decided; the operator can fix it and answer again.
    #[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Unfit(Vec<crate::mcp::schema::FieldError>),
    /// Asked to launch, resume, or otherwise run a session whose harness this
    /// build has no adapter for. Carries what to do instead.
    #[error(
        "session {id} ran the retired `{harness}` harness and cannot be launched again. \
             Carry its work forward with `tracon session reopen {id} --harness opencode`; \
             its workspace and transcript are kept."
    )]
    Legacy { id: String, harness: String },
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// The operator's answer to a card. A harness's permission request reads only
/// the option and arguments; an approval also keeps the reason and notes,
/// which go back to the caller through `approval_status`.
#[derive(Debug, Clone, Default)]
pub struct OperatorAnswer {
    pub option_id: String,
    pub arguments: Option<serde_json::Value>,
    pub reason: Option<String>,
    pub notes: Option<String>,
}

impl OperatorAnswer {
    fn into_frame(self, id: &str) -> proto::frame::Command {
        proto::frame::Command::Answer {
            permission_id: id.to_string(),
            option_id: self.option_id,
            arguments: self.arguments,
            reason: self.reason,
            notes: self.notes,
        }
    }
}

/// Running sessions, by id. A session that has ended leaves the map; its rows
/// and events stay in the store.
#[derive(Clone)]
pub struct Manager {
    pub(crate) tools: Arc<crate::mcp::Tools>,
    /// Shared with every supervisor and the mesh client, which swaps in a
    /// bundle handed off by a peer.
    policy: Arc<parking_lot::RwLock<crate::policy::Policy>>,
    store: Arc<Store>,
    bus: Bus,
    cfg: Arc<Config>,
    node_id: String,
    live: Arc<Mutex<HashMap<String, mpsc::Sender<Command>>>>,
    /// Session id → (tool token, channel). A token is minted when a session
    /// starts and dropped when it ends, so it authorises exactly one session
    /// for exactly as long as that session runs.
    tokens: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Session id → the running harness's own HTTP API, for the sessions
    /// whose harness has one. Registered when the harness starts and dropped
    /// when its supervisor ends, so the gateway can answer for exactly the
    /// sessions that are live and for no other endpoint.
    native: Arc<Mutex<HashMap<String, crate::adapter::NativeApi>>>,
    /// Which harness processes called under each external lane.
    liveness: Arc<external::Liveness>,
    /// Session id → the live channel the node synthesises for that session's
    /// native UI (`gateway::native_events`). Created on demand — ingestion and
    /// the gateway each ask for it and neither is reliably first — and dropped
    /// when the session's supervisor ends, so its replay ring does not outlive
    /// the session whose events it holds.
    native_events:
        Arc<std::sync::Mutex<HashMap<String, Arc<crate::gateway::native_events::NativeEvents>>>>,
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
    /// Adapters registered by harness id. Startup registers the ones it
    /// built (and probed); a harness with none registered is built from the
    /// config on demand. A test registers a fake under the id it stands for.
    adapters: Arc<std::sync::Mutex<HashMap<String, Arc<dyn HarnessAdapter>>>>,
    /// The adapter a session that names no harness runs on: the configured
    /// harness's, once startup has probed it.
    default_adapter: Arc<std::sync::OnceLock<Arc<dyn HarnessAdapter>>>,
    previews: Arc<crate::http::preview::PreviewTokens>,
    /// The grants whose refusals this node turns into asks, and which an
    /// operator's answer widens. The backend's own, once startup watches it.
    egress: Arc<parking_lot::RwLock<Option<crate::gateway::proxy::Grants>>>,
}

impl Manager {
    pub fn new(
        store: Arc<Store>,
        bus: Bus,
        cfg: Arc<Config>,
        node_id: String,
        tools: Arc<crate::mcp::Tools>,
        policy: Arc<parking_lot::RwLock<crate::policy::Policy>>,
        backend: Arc<dyn crate::boundary::Backend>,
    ) -> Self {
        // A mediated mutation this process did not dispatch was left in flight
        // by one that is gone: nobody is waiting for its answer and nobody
        // knows what it was. That is uncertain, and the session it belongs to
        // refuses a new prompt until reconciliation settles it.
        match store.opencode_reconcile_interrupted(crate::process::instance_id()) {
            Ok(0) => {}
            Ok(n) => tracing::warn!(
                intents = n,
                "the node restarted with harness mutations in flight; their sessions are uncertain"
            ),
            Err(e) => tracing::error!(error = %e, "failed to reconcile interrupted mutations"),
        }
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
            native: Arc::new(Mutex::new(HashMap::new())),
            liveness: Arc::default(),
            native_events: Arc::new(std::sync::Mutex::new(HashMap::new())),
            probe_token: mint_token(),
            mesh: Arc::new(std::sync::OnceLock::new()),
            providers: Arc::new(std::sync::OnceLock::new()),
            adapters: Arc::new(std::sync::Mutex::new(HashMap::new())),
            default_adapter: Arc::new(std::sync::OnceLock::new()),
            previews: Arc::new(crate::http::preview::PreviewTokens::default()),
            egress: Arc::default(),
        }
    }

    pub fn backend(&self) -> &Arc<dyn crate::boundary::Backend> {
        &self.backend
    }

    pub fn cfg(&self) -> &Arc<Config> {
        &self.cfg
    }

    pub fn broker(&self) -> &crate::broker::SharedBroker {
        &self.tools.broker
    }

    /// The identity a channel's commits are authored and committed as: the
    /// account behind its bound forge credential, preferring the forge a
    /// managed clone came from, else the host's own global Git identity.
    /// Bounded in time, so a forge that does not answer delays a launch by
    /// seconds rather than holding it.
    pub async fn commit_identity(
        &self,
        channel: &str,
        repo: &std::path::Path,
    ) -> Option<crate::forge::Identity> {
        let prefer = repo
            .strip_prefix(crate::forge::managed_root(&Config::state_dir()))
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|host| {
                if host.as_os_str().to_string_lossy().contains("gitlab") {
                    crate::forge::Forge::Gitlab
                } else {
                    crate::forge::Forge::Github
                }
            });
        let forge = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::forge::identity_for(&self.tools.broker, channel, &self.node_id, prefer),
        )
        .await
        .ok()
        .flatten();
        match forge {
            Some(identity) => Some(identity),
            None => crate::forge::host_identity(&self.cfg.publish.git).await,
        }
    }

    pub fn previews(&self) -> &Arc<crate::http::preview::PreviewTokens> {
        &self.previews
    }

    /// Create a session on this node with the node's own adapter, for
    /// sessions the node spawns itself (review sessions).
    /// Carry a session's work forward under a harness this build can run.
    ///
    /// The legacy session is never relaunched and never reused: its id names
    /// a transcript produced by a harness that is gone, and a second session
    /// writing under that id would make the transcript unreadable. This makes
    /// a new session instead — same channel, same workspace, same work item —
    /// records where it came from on both rows' terms (`parent_session`,
    /// `continued_from`), and gives it an explicit handoff note as its first
    /// prompt, because the new harness inherits none of the old one's context
    /// and pretending otherwise is how an agent redoes finished work.
    pub async fn reopen(&self, id: &str, harness: &str) -> Result<SessionRow, SessionError> {
        let old = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if !crate::adapter::KNOWN.contains(&harness) {
            return Err(SessionError::Rejected(format!(
                "no harness `{harness}`; this node knows {}",
                crate::adapter::KNOWN.join(", ")
            )));
        }
        let workspace_id = old
            .repo_path
            .strip_prefix("workspace://")
            .map(str::to_string);
        let note = handoff_note(&old, harness);
        let spec = NewSession {
            channel: old.channel.clone(),
            repo_path: old.repo_path.clone(),
            branch: Some(old.branch.clone()),
            work_item_id: old.work_item_id.clone(),
            model: String::new(),
            budget_tokens: Some(old.budget_tokens),
            initial_prompt: Some(note),
            node_id: Some(old.node_id.clone()),
            phase: reopen_phase(&old.phase),
            review_id: None,
            base_sha: None,
            workspace_id,
            parent_session: Some(old.id.clone()),
            continued_from: Some(old.id.clone()),
            harness: Some(harness.to_string()),
        };
        self.create(spec).await
    }

    /// Carry on the work a node restart interrupted.
    ///
    /// The harness process that held the conversation went with the node, so
    /// this is a new session, like `reopen`: same channel, workspace, branch,
    /// work item and harness, the lineage on the row, and a handoff note that
    /// says what was lost. Offered once per interrupted session; a second
    /// continuation of the same one is refused and names the first.
    pub async fn continue_interrupted(&self, id: &str) -> Result<SessionRow, SessionError> {
        let old = self.store.get_session(id)?.ok_or(SessionError::NotFound)?;
        if old.end_reason.as_deref() != Some(state::EndReason::NodeRestart.as_str()) {
            return Err(SessionError::Rejected(
                "only a session a node restart ended can be continued this way".into(),
            ));
        }
        if old.harness_id == external::HARNESS_ID {
            return Err(SessionError::Rejected(
                "an external agent's work runs outside this node; start it again where it runs"
                    .into(),
            ));
        }
        if old.phase == Phase::Review.as_str() {
            return Err(SessionError::Rejected(
                "a review session is started by its review, not continued; resubmit the review"
                    .into(),
            ));
        }
        if let Some(next) = self
            .store
            .list_sessions(None)?
            .into_iter()
            .find(|s| s.continued_from.as_deref() == Some(id))
        {
            return Err(SessionError::Rejected(format!(
                "session {} already continues this one",
                next.id
            )));
        }
        // A session on a checkout keeps its workspace under its own id; one
        // already on a retained workspace names it in its path.
        let workspace_id = old
            .repo_path
            .strip_prefix("workspace://")
            .unwrap_or(&old.id)
            .to_string();
        let harness = crate::adapter::KNOWN
            .contains(&old.harness_id.as_str())
            .then(|| old.harness_id.clone());
        let spec = NewSession {
            channel: old.channel.clone(),
            repo_path: old.repo_path.clone(),
            branch: Some(old.branch.clone()),
            work_item_id: old.work_item_id.clone(),
            model: old.model.clone(),
            budget_tokens: Some(old.budget_tokens),
            initial_prompt: Some(restart_note(&old)),
            node_id: Some(old.node_id.clone()),
            phase: reopen_phase(&old.phase),
            review_id: None,
            base_sha: None,
            workspace_id: Some(workspace_id),
            parent_session: Some(old.id.clone()),
            continued_from: Some(old.id.clone()),
            harness,
        };
        self.create(spec).await
    }

    /// Register an adapter under its own id. Node-spawned sessions on that
    /// harness use it; the node's default is registered at startup, once it
    /// has been probed.
    pub fn set_adapter(&self, adapter: Arc<dyn HarnessAdapter>) {
        self.adapters
            .lock()
            .unwrap()
            .insert(adapter.id().to_string(), adapter);
    }

    /// The adapter for sessions that name no harness. Set once; the first
    /// caller wins, which is startup in the node and the harness in a test.
    pub fn set_default_adapter(&self, adapter: Arc<dyn HarnessAdapter>) {
        let _ = self.default_adapter.set(adapter);
    }

    /// What a model's credential is, as far as choosing a harness goes.
    /// `None` when the node cannot say: a provider it has no configuration
    /// for, or a credential it does not hold.
    fn credential_class(&self, model: &str) -> Option<crate::adapter::compat::CredentialClass> {
        use crate::adapter::compat::CredentialClass;
        let Some((provider_name, _)) = model.split_once('/') else {
            return (!model.trim().is_empty()).then_some(CredentialClass::BareAlias);
        };
        let provider = self.cfg.providers.get(provider_name)?;
        let broker = self.tools.broker.read().ok()?;
        let kind = broker.get(&provider.credential)?.kind.clone();
        Some(CredentialClass::classify(&provider.shape, &kind))
    }

    /// The supported harnesses `model` runs on, for the picker; empty where
    /// the node cannot classify it.
    pub fn model_harnesses(&self, model: &str) -> Vec<&'static str> {
        self.credential_class(model)
            .map(|class| class.harnesses())
            .unwrap_or_default()
    }

    /// The harness a session runs on and why: the one the spec names, if the
    /// model's credential runs there; otherwise the only one it runs on; and
    /// where it runs on either (an Anthropic API key) or the node cannot say,
    /// the configured `[harness] id`.
    fn resolve_harness(
        &self,
        spec: &NewSession,
    ) -> Result<(Arc<dyn HarnessAdapter>, &'static str), SessionError> {
        let class = self.credential_class(&spec.model);
        if let Some(named) = spec.harness.as_deref().filter(|h| !h.trim().is_empty()) {
            if !crate::adapter::KNOWN.contains(&named) {
                return Err(SessionError::Rejected(format!(
                    "unknown harness `{named}`; this node runs {}",
                    crate::adapter::KNOWN.join(", ")
                )));
            }
            self.check_fit(&spec.model, class, named)?;
            return Ok((self.adapter_by_id(named)?, "explicit"));
        }
        let Some(class) = class else {
            return Ok((self.default_adapter()?, "node_default"));
        };
        match class.harnesses().as_slice() {
            [only] => Ok((self.adapter_by_id(only)?, "credential")),
            [] => Err(SessionError::Rejected(format!(
                "no supported harness runs {}",
                spec.model
            ))),
            both if both.contains(&self.cfg.harness.id.as_str()) => {
                Ok((self.default_adapter()?, "node_default"))
            }
            [first, ..] => Ok((self.adapter_by_id(first)?, "credential")),
        }
    }

    fn check_fit(
        &self,
        model: &str,
        class: Option<crate::adapter::compat::CredentialClass>,
        harness: &str,
    ) -> Result<(), SessionError> {
        match class.map(|class| class.fits(harness)) {
            Some(Err(reason)) => Err(SessionError::Rejected(format!(
                "{model} cannot run on the `{harness}` harness: {reason}"
            ))),
            _ => Ok(()),
        }
    }

    /// The configured harness's adapter: the one startup probed, once it has.
    fn default_adapter(&self) -> Result<Arc<dyn HarnessAdapter>, SessionError> {
        if let Some(default) = self.default_adapter.get() {
            return Ok(default.clone());
        }
        self.adapter_by_id(&self.cfg.harness.id)
    }

    /// Every supported harness has an image on this node, so the answer
    /// never depends on which one is the default.
    fn adapter_by_id(&self, harness: &str) -> Result<Arc<dyn HarnessAdapter>, SessionError> {
        if let Some(default) = self.default_adapter.get().filter(|a| a.id() == harness) {
            return Ok(default.clone());
        }
        if let Some(registered) = self.adapters.lock().unwrap().get(harness) {
            return Ok(registered.clone());
        }
        crate::adapter::adapter_for_id(&self.cfg, harness)
            .map_err(|e| SessionError::Rejected(e.to_string()))
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
                    harness_agent: None,
                    harness_found: None,
                    harness_protocol: None,
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
                    legacy_ms: None,
                    parent_session: None,
                    continued_from: None,
                    manifest_digest: None,
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
    /// How many sessions currently hold a capability token, for a refusal to
    /// say whether the node had forgotten this one or never knew any.
    pub async fn live_token_count(&self) -> usize {
        self.tokens.lock().await.len()
    }

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
        // Before the state, because "no longer active" is true of a legacy
        // session and useless to the operator: what they need is that its
        // harness is gone and how to carry the work forward.
        Self::refuse_legacy(&row)?;
        let state = SessionState::from_stored(&row.state);
        if state == SessionState::Paused {
            return Err(SessionError::Rejected("session is paused".into()));
        }
        if state.is_terminal() {
            return Err(SessionError::Rejected("session is no longer active".into()));
        }
        Ok(())
    }

    /// Refuse anything that would put a legacy session back to work.
    ///
    /// A session archived by `archive-legacy` is read-only for good: no
    /// adapter resolves its harness, so there is nothing to launch, and a
    /// second process writing under its id would make its transcript
    /// unreadable. The refusal names the reopen path rather than leaving the
    /// operator with a session that merely will not start.
    pub fn refuse_legacy(row: &SessionRow) -> Result<(), SessionError> {
        if row.legacy_ms.is_some() {
            return Err(SessionError::Legacy {
                id: row.id.clone(),
                harness: row.harness_id.clone(),
            });
        }
        Ok(())
    }

    fn session_allows_work(&self, session_id: &str) -> bool {
        self.ensure_active(session_id).is_ok()
    }
    /// The running harness's own HTTP API for a session, when it has one.
    /// This is the only way the gateway learns an endpoint: a request cannot
    /// name one, so no session's mount can reach another's server.
    pub async fn native_api(&self, session_id: &str) -> Option<crate::adapter::NativeApi> {
        self.native.lock().await.get(session_id).cloned()
    }

    /// The live channel the node synthesises for one session's native UI,
    /// created if this is the first time anyone asked.
    ///
    /// Created on demand rather than with the session because both ends ask
    /// for it independently and neither is reliably first: ingestion starts
    /// writing to it when the harness's streams open, and the gateway starts
    /// reading it when a browser connects. A session with no harness yet
    /// simply has an empty one, which is the honest answer to "what is live"
    /// rather than a 404 the page would retry against forever.
    pub fn native_events(
        &self,
        session_id: &str,
    ) -> Arc<crate::gateway::native_events::NativeEvents> {
        self.native_events
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_insert_with(crate::gateway::native_events::NativeEvents::new)
            .clone()
    }

    /// Register a native API directly. Tests drive the gateway against a fake
    /// harness server without starting one; sessions always go through
    /// `start`.
    #[doc(hidden)]
    pub async fn register_native_api_for_test(
        &self,
        session_id: &str,
        api: crate::adapter::NativeApi,
    ) {
        self.native.lock().await.insert(session_id.to_string(), api);
    }

    /// Whether a session may switch to this `provider/model`: the checks
    /// `create` makes before a session starts, asked again by the gateway when
    /// a native-UI model switch arrives mid-session. The channel must be able
    /// to spend on it, and its credential must run on the session's harness;
    /// the refusal says which.
    pub fn model_authorized(
        &self,
        channel: &str,
        model: &str,
        harness: &str,
    ) -> Result<(), String> {
        if !self.model_usable(channel, model, &self.bindings(channel)) {
            return Err(format!(
                "{model} is not a model channel {channel} is authorised for"
            ));
        }
        self.check_fit(model, self.credential_class(model), harness)
            .map_err(|e| e.to_string())
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

    pub fn policy(&self) -> &Arc<parking_lot::RwLock<crate::policy::Policy>> {
        &self.policy
    }

    /// Republish the waiting bay. Called when a review arrives or is decided,
    /// so the queue updates without the operator refetching.
    pub async fn publish_queue(&self) {
        let waiting = self.store.open_permission_views().unwrap_or_default();
        self.bus.publish(Frame::Queue { waiting });
        if let Ok(reviews) = self.store.open_reviews() {
            self.bus.publish(Frame::Reviews { waiting: reviews });
        }
    }

    /// A review session may read only its owning node's code-review row. This
    /// runs before any project/session/event side effect, so a peer or HTTP
    /// caller cannot use a report ID to inject its narrative into an unrelated
    /// session's orientation.
    fn validate_review_context(&self, spec: &NewSession) -> Result<(), SessionError> {
        let requested = spec.review_id.as_deref().map(str::trim);
        if spec.phase != Phase::Review {
            if requested.is_some() {
                return Err(SessionError::Rejected(
                    "review_id is only valid for a code-review session".into(),
                ));
            }
            return Ok(());
        }
        let Some(id) = requested.filter(|id| !id.is_empty()) else {
            return Err(SessionError::Rejected(
                "a code-review session requires its review_id".into(),
            ));
        };
        let review = self
            .store
            .get_review(id)?
            .ok_or_else(|| SessionError::Rejected(format!("no code review {id}")))?;
        if review.kind == crate::store::reports::KIND {
            return Err(SessionError::Rejected(
                "standalone narrative reports cannot start code-review sessions".into(),
            ));
        }
        if !matches!(review.kind.as_str(), "pr" | "mr") {
            return Err(SessionError::Rejected(format!(
                "review {id} is not a supported code-review kind"
            )));
        }
        if review.channel != spec.channel {
            return Err(SessionError::Rejected(format!(
                "review {id} belongs to channel {}",
                review.channel
            )));
        }
        if review.node_id != self.node_id {
            return Err(SessionError::Rejected(format!(
                "review {id} is owned by node {}",
                review.node_id
            )));
        }
        Ok(())
    }

    /// Validate, insert the row, and start the session in the background. The
    /// row exists before the harness does, so the interface can show a session
    /// that is still starting.
    pub async fn create(&self, spec: NewSession) -> Result<SessionRow, SessionError> {
        self.create_on(spec, None).await
    }

    /// `create`, on an adapter the caller chose. The node's own paths never
    /// choose: the harness follows the model's credential. This is the seam a
    /// test drives a fake harness through.
    pub async fn create_with(
        &self,
        spec: NewSession,
        adapter: Arc<dyn HarnessAdapter>,
    ) -> Result<SessionRow, SessionError> {
        self.create_on(spec, Some(adapter)).await
    }

    async fn create_on(
        &self,
        mut spec: NewSession,
        chosen: Option<Arc<dyn HarnessAdapter>>,
    ) -> Result<SessionRow, SessionError> {
        // Asked to run elsewhere: the owner validates and starts it; its row
        // arrives back both in the ack and, shortly, as a mirrored session.
        if let Some(node) = spec.node_id.as_deref().filter(|n| *n != self.node_id) {
            let mut remote = spec.clone();
            remote.node_id = None;
            let work_item = match remote.work_item_id.as_deref() {
                Some(id) => self
                    .store
                    .work_item_change(&self.node_id, &remote.channel, id)?,
                None => None,
            };
            let v = self
                .forward(
                    node,
                    proto::frame::Command::Create {
                        spec: json!(remote),
                        work_item,
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
        self.validate_review_context(&spec)?;
        // A dangling ancestor is worse than none: it reads as history that
        // can be followed and cannot.
        for ancestor in [
            spec.parent_session.as_deref(),
            spec.continued_from.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if self.store.get_session(ancestor)?.is_none() {
                return Err(SessionError::Rejected(format!(
                    "no session {ancestor} on this node to continue from"
                )));
            }
        }
        let bindings = self.bindings(&spec.channel);
        // An archived channel keeps everything it has and takes nothing new.
        if !bindings["archived"].is_null() {
            return Err(SessionError::ChannelArchived(spec.channel.clone()));
        }
        let phase_bindings = &bindings["phases"][spec.phase.as_str()];
        let (model, model_source) = self.resolve_model(&spec, &bindings)?;
        spec.model = model;
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
        // The harness follows the model's credential, once everything that
        // would refuse the session whatever it ran on has had its say.
        let (adapter, harness_source) = match chosen {
            Some(adapter) => {
                let class = self.credential_class(&spec.model);
                self.check_fit(&spec.model, class, adapter.id())?;
                (adapter, "caller")
            }
            None => self.resolve_harness(&spec)?,
        };
        if let Some(node) = self.store.get_node(&self.node_id)? {
            if node.state != "ready" {
                return Err(SessionError::NodeRefused(
                    node.failed_detail
                        .or(node.failed_check)
                        .unwrap_or_else(|| "boundary check failed".into()),
                ));
            }
            // Each harness's probe is on the node row; a harness it does not
            // list is checked at its own handshake.
            if let Some(harness) = node.harnesses().into_iter().find(|h| h.id == adapter.id()) {
                if harness.mismatch() {
                    return Err(SessionError::VersionMismatch {
                        found: harness.found.unwrap_or_default(),
                        pinned: harness.pinned,
                    });
                }
            }
        }

        // Work items and phase artifacts are an opt-in workflow. Plain
        // workspace sessions use the execute harness mode without claiming an
        // item, plan, or review lifecycle.
        let mut held_item = None;
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
            held_item = Some(view.item);
        } else if spec.phase == Phase::Plan {
            return Err(SessionError::WorkItemRequired);
        }
        // Starting work is one action. A session the node starts on an item
        // or a review is sent its kickoff as the first prompt, rather than
        // sitting `running` until the operator thinks to type something. A
        // caller's own first prompt (a plain session, a reopen's handoff)
        // wins; the kickoff is not kept as a draft, because it is the node's
        // words and not the operator's.
        let kickoff = match spec.initial_prompt.as_deref() {
            Some(prompt) if !prompt.trim().is_empty() => None,
            _ => kickoff(spec.phase, held_item.as_ref(), spec.review_id.as_deref()),
        };
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
            harness_agent: None,
            harness_found: None,
            harness_protocol: None,
            harness_session_id: None,
            container_name: None,
            model: spec.model.clone(),
            project_id: Some(project_id),
            phase: spec.phase.as_str().into(),
            policy_version: Some(self.policy.read().version as i64),
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
            legacy_ms: None,
            parent_session: spec.parent_session.clone(),
            continued_from: spec.continued_from.clone(),
            manifest_digest: None,
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
        if kickoff.is_some() {
            spec.initial_prompt = kickoff;
        }
        let row = self.store.get_session(&id)?.ok_or(SessionError::NotFound)?;
        self.bus.publish(Frame::Session(Box::new(row.clone())));

        let this = self.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            if let Err(e) = this
                .start(
                    &id,
                    spec,
                    branch,
                    slug,
                    adapter,
                    started,
                    (model_source, harness_source),
                )
                .await
            {
                tracing::error!(session = %id, error = %e, "session failed to start");
                // Nothing is writing this session's state now, whatever the
                // launch got as far as: the fence is given back here, because
                // the supervisor that would have released it never ran.
                materialize::release_state(&id);
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
                // A harness that is not the one this node drives is a fact
                // about the image, not about this session: every session here
                // ends the same way until the image or the pin changes, and
                // the row says so rather than reading as one bad start.
                let reason = match e.downcast_ref::<crate::adapter::AdapterError>() {
                    Some(
                        crate::adapter::AdapterError::IncompatibleProtocol { .. }
                        | crate::adapter::AdapterError::VersionMismatch { .. },
                    ) => EndReason::Incompatible,
                    _ => EndReason::Error,
                };
                let _ = this.store.update_session(
                    &id,
                    SessionPatch {
                        state: Some(SessionState::Failed.as_str().into()),
                        end_reason: Some(reason.as_str().into()),
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
        (model_source, harness_source): (&'static str, &'static str),
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
                    let brokered = crate::forge::git_token_for(
                        &self.tools.broker,
                        &Config::state_dir(),
                        &spec.channel,
                        &repo,
                        &self.node_id,
                    );
                    let credential = match &brokered {
                        Some((forge, token)) => {
                            crate::git_remote::brokered(forge.git_user(), Some(token.as_str()))
                        }
                        None => crate::git_remote::Credential::Anonymous,
                    };
                    crate::forge::fetch_managed(&repo, &credential)
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
        if let Some(entry) = adapter
            .refused_workspace_entries()
            .iter()
            .find(|entry| std::fs::symlink_metadata(snapshot.join(entry)).is_ok())
        {
            anyhow::bail!(
                "the workspace carries `{entry}`. The {} harness would load it as its own \
                 configuration after the node's, where it could allow tools without asking or \
                 run commands and plugins, so the session was not started. Remove or rename it \
                 on the branch this work starts from.",
                adapter.id()
            );
        }
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

        // The harness reaches its models through the node: the providers this
        // channel could actually spend on are wired to the gateway with this
        // session's token as the placeholder key, so the only secret in the
        // container names the session. A provider the gateway would refuse is
        // left out rather than offered and then denied mid-turn.
        let wiring = {
            let bindings = self.bindings(&spec.channel);
            let broker = self.tools.broker.read().unwrap();
            crate::gateway::model::harness_wiring(
                &self.cfg,
                &self.backend.harness_host(),
                &token,
                |name, provider| {
                    let bound = bindings["providers"].as_array().is_none_or(|allowed| {
                        allowed.iter().any(|value| value.as_str() == Some(name))
                    });
                    // A credential this harness may not run on (an Anthropic
                    // subscription under OpenCode) is not offered at all.
                    let fits = broker.get(&provider.credential).is_none_or(|credential| {
                        crate::adapter::compat::CredentialClass::classify(
                            &provider.shape,
                            &credential.kind,
                        )
                        .fits(adapter.id())
                        .is_ok()
                    });
                    bound
                        && fits
                        && broker
                            .inject_for(
                                &provider.credential,
                                &spec.channel,
                                &self.node_id,
                                &provider.shape,
                            )
                            .is_ok()
                },
            )
        };
        // The operator's customization for this channel, resolved against the
        // provider set the wiring just settled and the policy the gate is
        // running. Built and recorded here, before anything is staged: a
        // manifest that would be refused — a duplicate skill name, a plugin
        // the image did not bake — fails the launch rather than producing a
        // session whose configuration is not the one anybody described.
        //
        // Recorded, not just built: the revision this returns is the one this
        // session ran, and the next edit mints a different one that leaves
        // this session alone.
        // The session's own way out, where the runtime issues one and the
        // harness is a single process whose fetches are the agent's. OpenCode
        // installs plugins on its own, and what its image baked is the rule
        // for those, so it keeps the harness proxy and reaches no registry.
        let environment = crate::environment::session_environment(&self.cfg, &self.store, id);
        let egress = (adapter.id() == crate::adapter::claude::ClaudeAdapter::ID)
            .then(|| {
                self.backend
                    .egress_grant(crate::environment::session_grant(
                        &self.cfg,
                        &environment,
                        id,
                    ))
                    .ok()
            })
            .flatten();
        let mut wiring = wiring;
        let manifest = {
            let (skills, instructions, agents) = self.store.manifest_contents(&spec.channel)?;
            let built = crate::manifest::build(crate::manifest::Inputs {
                channel: &spec.channel,
                skills,
                instructions,
                agents,
                plugins: &self.cfg.launch.plugins,
                baked: &crate::adapter::baked_plugins(adapter.id()),
                // The toolchain profile belongs to one image. A session on a
                // harness that does not use it records no toolchain rather
                // than a list of servers it never had.
                lsp: crate::manifest::toolchain_lsp(adapter.id()),
                formatters: crate::manifest::toolchain_formatters(adapter.id()),
                providers: wiring.providers.iter().map(|p| p.name.clone()).collect(),
                policy_revision: self.policy.read().version.to_string(),
            })
            .map_err(|e| anyhow::anyhow!("the launch manifest for `{}`: {e}", spec.channel))?;
            self.store.manifest_record(&built)?
        };
        self.store.update_session(
            id,
            SessionPatch {
                manifest_digest: Some(manifest.digest.clone()),
                ..Default::default()
            },
        )?;
        wiring.manifest = manifest.clone();

        // What the session is told first: this node's facts, the work and its
        // constraints, the channel's policy and what is known, and then
        // whatever conventions fit under the cap. Recorded as an event so the
        // transcript shows what the agent was told — and what it was not.
        let ((orientation, missing), context) = {
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
            // What the operator selected for this item, delivered and
            // recorded before the orientation is assembled, so the receipt
            // says exactly what the text below carries. A receipt that
            // cannot be recorded fails the launch: an attempt whose context
            // is not on record is the thing this exists to prevent.
            let context = match item.as_ref() {
                Some(item) => crate::corpus::context::prepare(&self.store, id, item)?,
                None => None,
            };
            let policy = self.policy.read();
            let assembled = crate::corpus::orientation::assemble(
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
                    manifest: &manifest,
                    context: context.as_ref().map(|(d, r)| (d, r)),
                    egress: egress
                        .as_ref()
                        .map(|_| environment.session_egress.as_slice()),
                },
            );
            (assembled, context.map(|(_, receipt)| receipt))
        };
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::ORIENTATION.into(),
            ref_id: None,
            // `missing` names each piece the cap kept out, so the operator
            // reading the log sees what the session was not told rather than
            // a bare flag that something, somewhere, was cut.
            payload: json!({
                "text": orientation,
                "trimmed": !missing.is_empty(),
                "missing": missing,
                "chars": orientation.len(),
                // Which revision of the item's selected context this attempt
                // received, and what differs from the previous attempt; the
                // whole receipt is on the item's context record.
                "context": context.as_ref().map(|r| json!({
                    "revision": r.revision,
                    "digest": r.digest,
                    "previous_revision": r.previous_revision,
                    "changes": r.changes,
                    "omitted": r.omissions().map(|o| &o.slug).collect::<Vec<_>>(),
                })),
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

        if !self.startable(id)? {
            self.tokens.lock().await.remove(id);
            return Ok(());
        }

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
        // Written into the harness's own configuration too: OpenCode reads
        // its MCP servers from the config file, not from anything at launch.
        wiring.mcp_servers = mcp_servers.clone();

        // Whose name the session's commits carry: the account behind the
        // channel's bound forge credential, else the host's own Git identity.
        let identity = self.commit_identity(&spec.channel, &repo).await;
        let scratch = materialize::scratch_for(
            id,
            &snapshot,
            &repo,
            &self.backend.harness_home(),
            adapter.as_ref(),
            &wiring,
            &orientation,
            identity.as_ref(),
        )?;
        self.backend
            .import_volume(&scratch.volume, &scratch.dir)
            .await
            .map_err(anyhow::Error::msg)?;
        let container = format!("tracon-h-{slug}");
        let mut mounts = scratch.mounts;
        mounts.push(workspace.mount("/work", false));
        // The repository's own image, with the harness layered on, so the
        // session has the tools its checks will run with. Resolved from the
        // repository the work came from, which a resumed session's
        // `workspace://` path only leads back to.
        let image = match crate::environment::origin_repo(&self.store, Some(id))
            .ok()
            .flatten()
        {
            Some(origin) => {
                let announce = || {
                    self.record(NewEvent {
                        session_id: id.to_string(),
                        work_item_id: None,
                        kind: ek::REPO_IMAGE.into(),
                        ref_id: None,
                        payload: json!({ "status": "building", "repo": origin }),
                        at_ms: now_ms(),
                        mono_ms: started.elapsed().as_millis() as i64,
                    })
                };
                crate::repo_image::session_image(
                    &self.backend,
                    &self.cfg,
                    &self.store,
                    &origin,
                    (adapter.id(), adapter.pinned_version()),
                    &announce,
                )
                .await
            }
            None => Default::default(),
        };
        // A first build takes minutes, and the operator may have stopped the
        // session while it ran.
        if !self.startable(id)? {
            anyhow::bail!("the session was stopped before its harness started");
        }
        let mut cache_env = Vec::new();
        let runner: Arc<dyn Runner> = match &image.image {
            Some(session_image) => {
                // The session's own dependency cache: the agent installs into
                // it, so no check ever reads it. It starts as a copy of the
                // repository's base cache, so a first `cargo build` does not
                // begin by fetching what the default branch already needs.
                let cache = crate::environment::session_cache_volume(&workspace.id);
                if !self.backend.volume_exists(&cache).await {
                    if let Err(error) = self
                        .backend
                        .clone_volume(&environment.cache_volume, &cache)
                        .await
                    {
                        tracing::warn!(session = %id, %error, "session cache starts empty");
                    }
                }
                mounts.push(crate::runner::Mount::volume(cache, "/cache", false));
                cache_env = crate::environment::cache_env();
                self.backend.runner_in(adapter.id(), session_image, mounts)
            }
            None => self.backend.runner_for(adapter.id(), mounts),
        };

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

        let mut harness_env = wiring.env.clone();
        harness_env.extend([
            (
                "GIT_CONFIG_GLOBAL".into(),
                format!("{}/.gitconfig", self.backend.harness_home()),
            ),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ]);
        harness_env.extend(cache_env);
        if let Some(egress) = &egress {
            harness_env.extend(egress.env());
        }

        // The supervisor's channel exists before the harness does, because the
        // ingestion layer needs it: a permission reconciliation re-raises goes
        // through the same queue as every other one, and reconciliation can
        // begin the moment the stream does.
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        // Only a harness with a durable, sequenced stream has anything to
        // ingest. For the others the adapter keeps its own position, which is
        // all a stdio pipe can offer.
        let ingest = (adapter.id() == crate::adapter::opencode::OpenCodeAdapter::ID).then(|| {
            ingest::Ingest::new(
                self.store.clone(),
                self.bus.clone(),
                self.node_id.clone(),
                id.to_string(),
                started,
                cmd_tx.clone(),
                self.native_events(id),
            )
        });

        let launched = tokio::time::timeout(
            STARTUP_TIMEOUT,
            adapter.launch(
                runner.as_ref(),
                LaunchSpec {
                    cursor: ingest
                        .clone()
                        .map(|i| i as Arc<dyn crate::adapter::DurableCursor>),
                    cwd_in_runner: "/work".into(),
                    model: spec.model.clone(),
                    container_name: container.clone(),
                    harness_home: self.backend.harness_home(),
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
        // What the harness turned out to be, next to what this node expected of
        // it. The pin and the protocol were both checked before the handshake
        // returned, so this records a session that is already known compatible
        // — for reading the transcript later against the build that produced it.
        let compat = handle.compat();
        // Which LSP servers and formatters the config this session was
        // launched with names, and whether the image it launched from has
        // them. OpenCode emits no status of its own for either — a server
        // that fails to spawn is added to a `broken` set with no log line and
        // no event (`config-state.md` §6.5) — so this is recorded at launch or
        // it is not knowable at all.
        let toolchain = match adapter.id() == crate::adapter::opencode::OpenCodeAdapter::ID {
            true => Some(crate::runner::toolchain::probe(runner.as_ref()).await),
            false => None,
        };
        // What the policy-aware API gateway answers for. Registered before the
        // session is announced as started, so the interface never has a
        // running session whose native API it cannot reach.
        if let Some(api) = handle.native_api() {
            self.native.lock().await.insert(id.to_string(), api);
        }
        self.store.update_session(
            id,
            SessionPatch {
                harness_session_id: Some(handle.harness_session_id().to_string()),
                harness_agent: Some(compat.agent.clone()),
                harness_found: Some(compat.version.clone()),
                harness_protocol: Some(compat.protocol.clone()),
                ..Default::default()
            },
        )?;
        // What this session's state *is*, recorded next to the session the
        // moment the harness that made it has said what it is. The database
        // itself carries no version (`config-state.md` §7.2), so a backup taken
        // later would otherwise have nothing to name the schema it copied — and
        // a restore nothing to be gated on.
        if adapter.id() == crate::adapter::opencode::OpenCodeAdapter::ID {
            self.record_state_identity(id, &compat.version, adapter.pinned_version());
        }
        self.record(NewEvent {
            session_id: id.to_string(),
            work_item_id: None,
            kind: ek::SESSION_STARTED.into(),
            ref_id: None,
            payload: json!({
                "model": spec.model, "model_source": model_source,
                "harness": adapter.id(), "harness_source": harness_source,
                "phase": spec.phase.as_str(),
                "work_item_id": spec.work_item_id,
                "policy_version": self.policy.read().version,
                "harness_agent": compat.agent,
                "harness_version": compat.version,
                "harness_expected": adapter.pinned_version(),
                "harness_protocol": compat.protocol,
                "toolchain": toolchain,
                "image": image.image,
                "image_source": image.source(),
                "toolchain_image": image.toolchain_image,
                "image_note": image.note,
            }),
            at_ms: now_ms(),
            mono_ms: started.elapsed().as_millis() as i64,
        });

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
        let sup = match ingest {
            Some(ingest) => sup.with_ingest(ingest),
            None => sup,
        }
        .with_checks(environment.checks);
        let live = self.live.clone();
        let tokens = self.tokens.clone();
        let native = self.native.clone();
        let native_events = self.native_events.clone();
        let sid = id.to_string();
        tokio::spawn(async move {
            sup.run(events, cmd_rx).await;
            live.lock().await.remove(&sid);
            // The token dies with the session; a later call with it is refused.
            tokens.lock().await.remove(&sid);
            // And its way out with it.
            drop(egress);
            // So does the gateway's route to its harness: the endpoint is
            // gone, and a later request must not be forwarded to whatever
            // took the port.
            native.lock().await.remove(&sid);
            // And the live channel its native UI streamed from. Nothing will
            // publish to it again, and its replay ring is a held page of this
            // session's events; a node that ran a thousand sessions would
            // otherwise still be holding all thousand. A browser that
            // reconnects after this gets a fresh, empty channel, which is the
            // truth about a session that has ended.
            native_events.lock().unwrap().remove(&sid);
            materialize::remove(&sid);
        });
        Ok(())
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

    pub async fn answer(&self, id: &str, answer: OperatorAnswer) -> Result<(), SessionError> {
        // An approval this node holds is answered here, not by a session:
        // nothing is waiting on it, and the node runs the call itself.
        if let Some(approval) = self.store.get_approval(id)? {
            if approval.node_id == self.node_id {
                return self.tools.answer_approval(id, answer).await;
            }
            return self
                .forward(&approval.node_id, answer.into_frame(id), false)
                .await
                .map(|_| ());
        }
        let perm = self
            .store
            .get_permission(id)?
            .ok_or(SessionError::NotFound)?;
        let session_id = perm.session_id.ok_or(SessionError::NotFound)?;
        let (ack, wait) = oneshot::channel();
        match self
            .send(
                &session_id,
                Command::Answer {
                    permission_id: id.to_string(),
                    option_id: answer.option_id.clone(),
                    arguments: answer.arguments.clone(),
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
                .forward(&node, answer.into_frame(id), false)
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// An approval's history goes to the log of whoever asked: the session,
    /// or the channel's external log.
    fn record_approval(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        kind: &str,
        payload: serde_json::Value,
    ) {
        match &approval.session_id {
            Some(session_id) => self.record(NewEvent {
                session_id: session_id.clone(),
                work_item_id: None,
                kind: kind.into(),
                ref_id: Some(approval.id.clone()),
                payload,
                at_ms: now_ms(),
                mono_ms: 0,
            }),
            None => self.record_external(
                &approval.channel,
                approval.lane.as_deref(),
                kind,
                Some(&approval.id),
                payload,
            ),
        }
    }

    /// A call was held for the operator: put it in the waiting bay and say so
    /// in the asking session's log. The session itself goes on working.
    pub async fn approval_requested(&self, approval: &crate::store::approvals::ApprovalRow) {
        self.record_approval(
            approval,
            ek::PERMISSION_REQUEST,
            json!({
                "approval_id": approval.id, "permission_id": approval.id,
                "title": approval.title, "kind": "tool", "tool": approval.tool,
                "raw_input": { "tool": approval.tool, "arguments": serde_json::from_str::<serde_json::Value>(&approval.arguments).unwrap_or_default() },
                "expires_ms": approval.expires_ms,
            }),
        );
        self.publish_queue().await;
    }

    /// An approval moved on: answered, run, or expired.
    pub async fn approval_decided(
        &self,
        approval: &crate::store::approvals::ApprovalRow,
        state: &str,
    ) {
        use crate::store::approvals as a;
        let kind = match state {
            a::RUNNING | a::REJECTED | a::CHANGES_REQUESTED => ek::PERMISSION_ANSWER,
            a::EXPIRED => ek::PERMISSION_EXPIRED,
            _ => ek::APPROVAL_SETTLED,
        };
        self.record_approval(
            approval,
            kind,
            json!({
                "approval_id": approval.id, "permission_id": approval.id,
                "tool": approval.tool, "state": state, "reason": approval.reason,
                "option_id": approval.answer_option_id, "notes": approval.operator_note,
            }),
        );
        self.publish_queue().await;
    }

    /// Whether an approved call may still run for this session. A session
    /// that has ended does not stop it: the approval belongs to the channel.
    /// A paused session, or a channel the operator stopped, does.
    pub fn approval_runnable(&self, ctx: &crate::mcp::CallContext) -> Result<(), SessionError> {
        self.channel_usable(&ctx.channel)?;
        match ctx.session_id() {
            None => self.external_open(&ctx.channel),
            Some(id) => {
                if let Some(row) = self.store.get_session(id)? {
                    if row.state == SessionState::Paused.as_str() {
                        return Err(SessionError::Rejected("session is paused".into()));
                    }
                }
                Ok(())
            }
        }
    }

    /// Whether the caller may make a brokered call now: its session is live,
    /// or, for a harness the operator runs themselves, its channel is open to
    /// external harnesses.
    pub fn caller_active(&self, ctx: &crate::mcp::CallContext) -> Result<(), SessionError> {
        match ctx.session_id() {
            Some(id) => self.ensure_active(id),
            None => {
                self.channel_usable(&ctx.channel)?;
                self.external_open(&ctx.channel)
            }
        }
    }

    /// External harnesses on `channel` are not stopped by the operator.
    pub fn external_open(&self, channel: &str) -> Result<(), SessionError> {
        let bindings = self.bindings(channel);
        // A channel-wide pause from an older node fences exactly like a Stop.
        if bindings["external_stopped"] == true || bindings["external_paused"] == true {
            return Err(SessionError::Rejected(
                "external broker access was stopped by the operator; start it again in Settings or with `tracon external start <channel>`".into(),
            ));
        }
        Ok(())
    }

    /// Expire approvals nobody answered in time.
    pub async fn expire_approvals(&self) {
        let Ok(expired) = self.store.expire_due_approvals() else {
            return;
        };
        for approval in &expired {
            self.approval_decided(approval, crate::store::approvals::EXPIRED)
                .await;
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
    /// A check can outlive the session that asked for it, so both ends of
    /// this are fenced in the UPDATE itself rather than by a read first: a
    /// pause or a stop landing mid-check wins, and the check finishing
    /// afterwards must not put the row back into `running`.
    pub fn set_checking(&self, id: &str, checking: bool) {
        let state = if checking {
            SessionState::WaitingOnCheck
        } else {
            SessionState::Running
        };
        let mut fenced: Vec<&str> = SessionState::TERMINAL.to_vec();
        fenced.push(SessionState::Paused.as_str());
        match self
            .store
            .update_session_unless(id, &fenced, SessionPatch::state(state.as_str()))
        {
            Ok(true) => {}
            _ => {
                self.refused_transition(id, "set_checking", json!({ "attempted": state.as_str() }));
                return;
            }
        }
        if let Ok(Some(row)) = self.store.get_session(id) {
            self.bus.publish(Frame::Session(Box::new(row)));
        }
    }

    /// Record a transition refused because the session had already ended or
    /// been fenced. The same shape the supervisor writes, for the writers
    /// that live outside it.
    pub fn refused_transition(&self, id: &str, what: &str, mut detail: serde_json::Value) {
        let state = self
            .store
            .get_session(id)
            .ok()
            .flatten()
            .map(|row| row.state)
            .unwrap_or_else(|| "gone".into());
        detail["what"] = json!(what);
        detail["state"] = json!(state);
        tracing::warn!(session = %id, %what, %state, "refused a late transition");
        self.record_event(id, ek::LATE_REFUSED, detail);
    }

    /// Watch the backend's grants: each host a session's own grant refuses
    /// is put to the operator, and recorded on the session once.
    pub fn record_egress_refusals(&self) {
        if let Some(grants) = self.backend.egress_grants() {
            self.watch_egress(grants);
        }
    }

    /// Turn the refusals under `grants` into asks, and widen these grants
    /// when the operator allows one.
    pub fn watch_egress(&self, grants: &crate::gateway::proxy::Grants) {
        *self.egress.write() = Some(grants.clone());
        grants.on_refusal(self.egress_refusal_observer());
    }

    /// The grants an egress answer widens, once they are watched.
    pub fn egress_grants(&self) -> Option<crate::gateway::proxy::Grants> {
        self.egress.read().clone()
    }

    /// What `watch_egress` installs. A session's refusal raises the
    /// operator's ask, or finds the one already waiting, and the refusal's
    /// reason says so; the event is recorded once per session and host, as a
    /// package manager retries and the operator needs to hear it once. A
    /// grant that is no session's (a preparation's) is refused as it was.
    pub fn egress_refusal_observer(&self) -> crate::gateway::proxy::OnRefusal {
        let manager = self.clone();
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        Arc::new(move |grant, host| {
            let session = grant.session_id.as_ref()?;
            let asked = match crate::mcp::egress::normalize_host(host) {
                Ok(host) => crate::mcp::egress::ask(&manager, session, &host, None, false)
                    .map_err(|error| {
                        tracing::warn!(%session, %host, %error, "could not ask about egress");
                    })
                    .ok(),
                Err(_) => None,
            };
            let first = {
                let mut seen = seen.lock().unwrap();
                // Bounded: a node that runs for months forgets, and at worst
                // says a refusal twice.
                if seen.len() > 10_000 {
                    seen.clear();
                }
                seen.insert((session.clone(), host.to_string()))
            };
            let approval_id = match &asked {
                Some(crate::mcp::egress::Asked::Pending(row, created)) => {
                    if *created {
                        let manager = manager.clone();
                        let row = row.clone();
                        tokio::spawn(async move { manager.approval_requested(&row).await });
                    }
                    Some(row.id.clone())
                }
                Some(crate::mcp::egress::Asked::Declined(row)) => Some(row.id.clone()),
                None => None,
            };
            if first {
                manager.record_event(
                    session,
                    ek::EGRESS_REFUSED,
                    json!({ "host": host, "approval_id": approval_id }),
                );
            }
            asked.map(|asked| crate::mcp::egress::refusal(host, &asked))
        })
    }

    /// Record an event on a session from outside the supervisor.
    /// Log a brokered call's step where its caller keeps its log.
    pub fn record_for(
        &self,
        ctx: &crate::mcp::CallContext,
        kind: &str,
        ref_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        match ctx.session_id() {
            Some(session_id) => self.record(NewEvent {
                session_id: session_id.to_string(),
                work_item_id: None,
                kind: kind.to_string(),
                ref_id: ref_id.map(str::to_string),
                payload,
                at_ms: now_ms(),
                mono_ms: 0,
            }),
            None => self.record_external(&ctx.channel, ctx.lane(), kind, ref_id, payload),
        }
    }

    /// The harness processes that call under external lanes.
    pub fn liveness(&self) -> &external::Liveness {
        &self.liveness
    }

    /// Log what a session-less caller did on its channel.
    pub fn record_external(
        &self,
        channel: &str,
        lane: Option<&str>,
        kind: &str,
        ref_id: Option<&str>,
        payload: serde_json::Value,
    ) {
        match self.store.append_external_event(
            channel,
            &self.node_id,
            lane,
            kind,
            ref_id,
            &payload,
            now_ms(),
        ) {
            Ok(row) => self.bus.publish(Frame::ExternalEvent(Box::new(row))),
            Err(err) => tracing::error!(error = %err, "failed to persist external event"),
        }
    }

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

    /// The model a spec will actually run on, and where it came from.
    ///
    /// An explicit model is checked against the channel's bound provider
    /// exactly as a resolved default is: naming one is a request, not an
    /// exemption, and a name the channel cannot authenticate would otherwise
    /// start a session that can only fail at its first turn. `create` and
    /// `preflight` both go through here, so both refuse it the same way.
    fn resolve_model(
        &self,
        spec: &NewSession,
        bindings: &serde_json::Value,
    ) -> Result<(String, &'static str), SessionError> {
        if spec.model.trim().is_empty() {
            self.resolve_default_model(&spec.channel, spec.phase, bindings)
        } else if self.model_usable(&spec.channel, &spec.model, bindings) {
            Ok((spec.model.clone(), "explicit"))
        } else {
            Err(SessionError::ModelRequired)
        }
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
        let (model, _) = self.resolve_model(spec, &bindings)?;
        self.resolve_harness(&NewSession {
            model,
            ..spec.clone()
        })?;
        Ok(())
    }

    /// The policy revision the gate is running, as a launch manifest records
    /// it: a session's customization and the rules it ran under are one fact
    /// about how that session was configured.
    pub fn policy_version(&self) -> u32 {
        self.policy.read().version
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

    /// Reopen a session that remains supervised on its owning node.
    pub async fn resume(&self, id: &str, reason: String) -> Result<(), SessionError> {
        if let Some(row) = self.store.get_session(id)? {
            Self::refuse_legacy(&row)?;
        }
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
            Err(e) => Err(e),
        }
    }

    /// Refuse every external harness on `channel`, or start allowing them
    /// again. External callers have no session, so this is their only fence.
    pub async fn set_external_stopped(
        &self,
        channel: &str,
        stopped: bool,
    ) -> Result<(), SessionError> {
        match self.channel_usable(channel) {
            Ok(()) | Err(SessionError::ChannelArchived(_)) => {}
            Err(e) => return Err(e),
        }
        write_external_stop(&self.store, &self.node_id, channel, stopped)?;
        Ok(())
    }

    /// Stop a live session. The operator's actual "Stop" action.
    pub async fn stop(&self, id: &str) -> Result<(), SessionError> {
        self.terminate(id).await
    }

    // ---- per-session OpenCode state ----

    /// Write down what this session's OpenCode state is, the moment the
    /// harness has said what it is.
    ///
    /// The build identity and the layout are known here and written straight
    /// away. The *generation* — the applied migration ids — is only in the
    /// database, and the database is at that instant being written by the
    /// harness through a runtime volume; reading it means copying the volume,
    /// which does not belong on a launch path. So it is read behind the
    /// launch, best-effort, and settled for certain by the first quiesced
    /// backup, which is the first moment anything can read it truthfully.
    fn record_state_identity(&self, id: &str, found: &str, pinned: &str) {
        let volume = crate::workspace::scratch_volume_name(id);
        let row = crate::store::OpenCodeStateRow {
            session_id: id.to_string(),
            build_version: found.to_string(),
            build_pinned: pinned.to_string(),
            generation: Vec::new(),
            generation_digest: String::new(),
            // The launch manifest is built and recorded on the session before
            // anything is staged, so by the time the harness has said what it
            // is, the digest is on the row. Read rather than passed down: this
            // is the same fact, and two copies could disagree. Still nullable
            // — a session on a harness with no manifest has none, and the
            // column says "not recorded" rather than a guess.
            manifest_digest: self
                .store
                .get_session(id)
                .ok()
                .flatten()
                .and_then(|row| row.manifest_digest),
            state_volume: volume.clone(),
            state_path: materialize::HARNESS_TREE.to_string(),
            recorded_ms: now_ms(),
            updated_ms: now_ms(),
        };
        if let Err(error) = self.store.opencode_state_put(&row) {
            tracing::warn!(session = %id, %error, "could not record the OpenCode state identity");
            return;
        }
        let store = self.store.clone();
        let backend = self.backend.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let staging = Config::state_dir()
                .join("opencode-state-work")
                .join(&id)
                .join("generation");
            let _ = std::fs::remove_dir_all(&staging);
            {
                let lock = crate::workspace::volume_lock(&volume);
                let _guard = lock.lock().await;
                if backend.export_volume(&volume, &staging).await.is_err() {
                    return;
                }
            }
            let db = staging.join(materialize::OPENCODE_DB);
            match opencode_state::read_generation(&db) {
                Ok(ids) if !ids.is_empty() => {
                    let _ = store.opencode_state_set_generation(&id, &ids);
                }
                Ok(_) => {}
                Err(error) => tracing::debug!(
                    session = %id, %error,
                    "the state generation could not be read behind the launch; \
                     the first backup will settle it"
                ),
            }
            let _ = std::fs::remove_dir_all(&staging);
        });
    }

    /// A quiesced backup of one session's OpenCode state. `quiesce` is the
    /// operator's `--quiesce`: without it a live session is refused rather
    /// than copied out from under its own harness.
    pub async fn backup_state(
        &self,
        id: &str,
        quiesce: bool,
    ) -> Result<opencode_state::BackupReport, opencode_state::StateError> {
        let quiescer: Option<&dyn opencode_state::Quiesce> = quiesce.then_some(self);
        opencode_state::backup(&self.store, self.backend.as_ref(), quiescer, id).await
    }

    /// Migrate one session's state onto another OpenCode release, on a clone.
    pub async fn upgrade_state(
        &self,
        id: &str,
        to: &str,
    ) -> Result<opencode_state::UpgradeReport, opencode_state::StateError> {
        opencode_state::upgrade_state(&self.store, self.backend.as_ref(), id, to).await
    }

    /// Put a verified backup back, if this node's runtime can read it.
    pub async fn restore_state(
        &self,
        id: &str,
        backup: &std::path::Path,
        confirm: bool,
    ) -> Result<opencode_state::RestoreReport, opencode_state::StateError> {
        opencode_state::restore(
            &self.store,
            self.backend.as_ref(),
            &self.cfg,
            id,
            backup,
            confirm,
        )
        .await
    }

    /// End one session; its row closes exactly as `stop` leaves it.
    pub async fn kill(&self, id: &str) -> Result<(), SessionError> {
        self.terminate(id).await
    }

    /// A startup row has no supervisor to command yet, so it is made
    /// terminal directly and startup observes that fence.
    async fn terminate(&self, id: &str) -> Result<(), SessionError> {
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
    /// Containers are removed by each supervisor's teardown. The sessions end
    /// as `node_restart`, not `killed_user`: the operator stopped the node,
    /// not the work, and the session says so and offers to carry it on.
    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = self.live.lock().await.keys().cloned().collect();
        for id in &ids {
            let _ = self
                .send(id, Command::End(state::EndReason::NodeRestart))
                .await;
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

/// Stopping a session so its state can be copied.
///
/// A pause first, so the running turn is quiesced rather than cut in half, and
/// only then a stop. Never `kill`: killing the harness leaves its WAL exactly
/// as unquiesced as taking no quiesce at all would have, which is the one thing
/// `--quiesce` exists to avoid. A pause that is refused because the session has
/// already gone terminal is not a failure — the writer is gone either way, and
/// that is all the backup needs.
#[async_trait::async_trait]
impl opencode_state::Quiesce for Manager {
    async fn quiesce(&self, session_id: &str) -> Result<(), String> {
        let _ = self
            .pause(
                session_id,
                "operator asked for a quiesced backup of this session's state".into(),
            )
            .await;
        self.stop(session_id).await.map_err(|e| e.to_string())
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
            if p.session_id.as_deref() == Some(s.id.as_str()) {
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
        if s.harness_id == external::HARNESS_ID {
            // External harnesses no longer attach as sessions. A row an older
            // node left open is closed, and one the operator had paused
            // becomes the channel's Stop, so the fence it held survives.
            if s.state == state::SessionState::Paused.as_str() {
                let _ = write_external_stop(store, self_node_id, &s.channel, true);
            }
            let _ = store.update_session(
                &s.id,
                SessionPatch {
                    state: Some(state::SessionState::Closed.as_str().into()),
                    end_reason: Some(state::EndReason::Detached.as_str().into()),
                    turn_active: Some(false),
                    ..Default::default()
                },
            );
            cleaned.push(s.id);
            continue;
        }
        if let Some(container) = &s.container_name {
            backend.reconcile(std::slice::from_ref(container)).await;
        }
        let _ = store.update_session(
            &s.id,
            SessionPatch {
                state: Some(state::SessionState::Closed.as_str().into()),
                end_reason: Some(state::EndReason::NodeRestart.as_str().into()),
                turn_active: Some(false),
                ..Default::default()
            },
        );
        let _ = store.append_event(&NewEvent {
            session_id: s.id.clone(),
            work_item_id: None,
            kind: ek::STATE.into(),
            ref_id: None,
            payload: json!({ "state": "closed", "end_reason": "node_restart", "reason": "node restarted" }),
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

/// The durable fence behind an external Stop. Stopping also retires the
/// channel-wide pause an older node may have left; starting clears both.
fn write_external_stop(
    store: &Store,
    node_id: &str,
    channel: &str,
    stopped: bool,
) -> Result<(), SessionError> {
    if store.channel_get(channel)?.is_none() {
        // Materialize both standalone defaults together; creating one
        // otherwise makes the other disappear from the synthesized list.
        for name in crate::http::api::DEFAULT_CHANNELS {
            store.channel_put(name, &[], "{}")?;
            store.node_channel_add(node_id, name)?;
        }
    }
    let (keyring, mut bindings) = match store.channel_get(channel)? {
        Some(channel) => (
            channel.keyring,
            serde_json::from_str(&channel.bindings_json).unwrap_or_else(|_| json!({})),
        ),
        None => (Vec::new(), json!({})),
    };
    if let Some(map) = bindings.as_object_mut() {
        map.remove("external_paused");
        if stopped {
            map.insert("external_stopped".into(), json!(true));
        } else {
            map.remove("external_stopped");
        }
    }
    store.channel_put(
        channel,
        &keyring,
        &serde_json::to_string(&bindings)
            .map_err(|error| SessionError::Rejected(error.to_string()))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(plan: Option<&str>) -> tracon_sync::work::WorkItem {
        tracon_sync::work::WorkItem {
            id: "0123456789abcdef".into(),
            channel: "personal".into(),
            project_id: None,
            title: "Add the ledger".into(),
            body: String::new(),
            state: "open".into(),
            priority: 0,
            deps: vec![],
            discovered_from: None,
            discovered_by_session: None,
            phase_plan_slug: plan.map(str::to_string),
            brief_slug: None,
            closed_by_session: None,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn a_plain_session_has_no_kickoff() {
        assert_eq!(kickoff(Phase::Execute, None, None), None);
    }

    #[test]
    fn an_execute_session_without_a_plan_starts_on_the_item_alone() {
        let text = kickoff(Phase::Execute, Some(&item(None)), None).unwrap();
        assert!(text.contains("**Add the ledger** (`01234567…`)"), "{text}");
        assert!(!text.contains("plan"), "{text}");
    }

    #[test]
    fn a_review_session_is_told_which_review_and_how_to_answer() {
        let text = kickoff(Phase::Review, None, Some("r-1")).unwrap();
        assert!(text.contains("`r-1`"), "{text}");
        assert!(text.contains("`review_verdict`"), "{text}");
    }
}
